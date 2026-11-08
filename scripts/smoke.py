#!/usr/bin/env python3
"""Smoke already-built images and an exact launcher binary using a real engine.

Run as a non-root user with access to the selected engine, after building/loading
playpen-{base,rust,jvm}:latest:

    python3 scripts/smoke.py --engine docker --launcher /path/to/playpen

Podman uses the same invocation with --engine podman; start its rootless API socket
first with systemctl --user start podman.socket. Both isolated and opt-in host
engine launches are exercised. No AI requests are made; only a dummy secret is
supplied. The harness install needs npm network access.
All launcher state, image aliases, and the cargo volume belong to this run. The
forwarding executable restores the engine's original environment (in particular
HOME and XDG storage/config paths). It substitutes the cargo volume name and
requests removal of intermediate build containers, including failed builds.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import termios
import time
import uuid


DUMMY = "playpen-smoke-not-a-credential=with=equals"
KEYS = f"SMOKE_DUMMY_SECRET={DUMMY}\n"
OUTPUT_LIMIT = 4 * 1024 * 1024


class SmokeError(Exception):
    pass


class Interrupted(SmokeError):
    def __init__(self, signum):
        super().__init__(f"interrupted by signal {signum}")
        self.signum = signum


def require(condition, message):
    if not condition:
        raise SmokeError(message)


def terminate_tree(process):
    # Each command starts a session: kill the group even if its leader exited.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        process.poll()  # Reap the leader without neglecting its descendants.
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired as error:
        raise SmokeError(f"could not reap command process {process.pid}") from error


def run(command, *, env, cwd=None, timeout=60, tty=False, expected=0, quiet=False):
    command = [str(arg) for arg in command]
    if not quiet:
        print("+ " + shlex.join(command), flush=True)
    master = slave = None
    process = None
    output = bytearray()
    try:
        if tty:
            master, slave = pty.openpty()
        process = subprocess.Popen(
            command,
            env=env,
            cwd=cwd,
            stdin=slave if tty else subprocess.DEVNULL,
            stdout=slave if tty else subprocess.PIPE,
            stderr=slave if tty else subprocess.STDOUT,
            start_new_session=True,
            # The script is single-threaded. Claim the PTY as the session's
            # controlling terminal as well as providing isatty() descriptors.
            preexec_fn=(lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0)) if tty else None,
        )
        if slave is not None:
            os.close(slave)
            slave = None
        fd = master if tty else process.stdout.fileno()
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeError(f"command timed out after {timeout}s: {shlex.join(command)}")
            readable, _, _ = select.select([fd], [], [], min(remaining, 0.25))
            if not readable:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError as error:
                if tty and error.errno == errno.EIO:
                    chunk = b""  # Linux PTYs report EIO when the slave closes.
                else:
                    raise
            if not chunk:
                break
            if not quiet:
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
            output.extend(chunk)
            if len(output) > OUTPUT_LIMIT:
                del output[:-OUTPUT_LIMIT]
        code = process.wait(timeout=max(0.01, deadline - time.monotonic()))
    except BaseException:
        if process is not None:
            terminate_tree(process)
        raise
    finally:
        if slave is not None:
            os.close(slave)
        if master is not None:
            os.close(master)
        if process is not None and process.stdout is not None:
            process.stdout.close()
    text = output.decode("utf-8", errors="replace").replace("\r\n", "\n")
    if expected is not None and code != expected:
        detail = f"\n{text}" if quiet else ""
        raise SmokeError(
            f"expected exit {expected}, got {code}: {shlex.join(command)}{detail}"
        )
    return code, text


# This is an exec-forwarder, not a fake engine. Record the actual arguments and
# temporary env-file metadata before exec so assertions survive --rm cleanup.
FORWARDER = r'''
import json
import os
from pathlib import Path
import stat
import sys

settings = json.loads(Path(SETTINGS).read_text())
args = sys.argv[1:]
for index in range(len(args) - 1):
    if args[index] == "-v" and args[index + 1] == "playpen-cargo:/opt/agent-cargo":
        args[index + 1] = settings["volume"] + ":/opt/agent-cargo"
if args[:1] == ["build"]:
    args.insert(1, "--force-rm")
event = {"args": args}
if "--env-file" in args:
    path = Path(args[args.index("--env-file") + 1])
    event["env_file"] = str(path)
    event["env_mode"] = stat.S_IMODE(path.stat().st_mode)
with open(settings["log"], "a", encoding="utf-8") as log:
    log.write(json.dumps(event) + "\n")
os.execve(settings["engine"], [settings["engine"], *args], settings["environment"])
'''


class Smoke:
    def __init__(self, engine_name, launcher, root):
        self.root = root
        self.launcher = launcher
        self.engine_name = engine_name
        self.engine = shutil.which(engine_name)
        require(self.engine is not None, f"{engine_name} is not on PATH")
        self.engine = str(Path(self.engine).absolute())
        self.engine_env = os.environ.copy()
        self.token = "smoke-" + uuid.uuid4().hex[:16]
        self.volume = "playpen-cargo-" + self.token
        self.aliases = []
        self.siblings = set()
        self.log = root / "engine.jsonl"
        self.workspace = root / "workspace"
        self.home = root / "home"
        self.data = root / "data"
        self.runtime = root / "runtime"
        config = root / "config"
        bin_dir = root / "bin"
        for directory in (self.workspace, self.home, self.data, self.runtime, config, bin_dir):
            directory.mkdir(mode=0o700)
        self.agent_home = self.data / "playpen-home"
        self.agent_home.mkdir(mode=0o700)
        (self.workspace / "host-workspace").write_text("workspace-input\n")
        (self.agent_home / "host-home").write_text("home-input\n")
        playpen_config = config / "playpen"
        playpen_config.mkdir()
        self.keys = playpen_config / "keys.conf"
        self.keys.write_text(KEYS)
        self.keys.chmod(0o644)  # The launcher must tighten this to 0600.
        (playpen_config / "playpen.toml").write_text('harness = "omp"\n')
        settings = root / "engine-settings.json"
        settings.write_text(json.dumps({
            "engine": self.engine,
            "environment": self.engine_env,
            "volume": self.volume,
            "log": str(self.log),
        }))
        settings.chmod(0o600)
        wrapper = bin_dir / engine_name
        wrapper.write_text(
            f"#!{sys.executable}\nSETTINGS = {str(settings)!r}\n" + FORWARDER
        )
        wrapper.chmod(0o700)
        # Do not expose the invoking user's API credentials, agent settings, or
        # PLAYPEN_* overrides to the launcher. The engine alone retains its
        # original environment, including remote/rootless connection settings.
        self.launcher_env = {
            key: value for key, value in os.environ.items()
            if key in ("LANG", "LC_ALL", "LC_CTYPE", "TZ")
        }
        self.launcher_env.update({
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(config),
            "XDG_DATA_HOME": str(self.data),
            "XDG_CACHE_HOME": str(root / "cache"),
            "XDG_RUNTIME_DIR": str(self.runtime),
            "TMPDIR": str(root),
            "PWD": str(self.workspace),
            "PATH": str(bin_dir) + os.pathsep + os.environ.get("PATH", os.defpath),
            "TERM": "dumb",
            "PLAYPEN_ENGINE": engine_name,
        })

    def engine_run(self, *args, **kwargs):
        return run([self.engine, *args], env=self.engine_env, **kwargs)

    def events(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def tag(self, source, target):
        self.aliases.append(target)  # Register before a cancellable command.
        self.engine_run("tag", source, target)
        return target

    def image_id(self, image):
        return self.engine_run(
            "image", "inspect", "--format", "{{.Id}}", image, quiet=True
        )[1].strip()

    def assert_removed(self, name):
        _, output = self.engine_run(
            "ps", "-a", "--filter", "name=" + name,
            "--format", "{{.Names}}", quiet=True,
        )
        require(name not in output.splitlines(), f"--rm left container {name}")

    def launch(self, image, command, *, timeout=120, expected=0, explicit_keys=False, host_engine=False):
        before = len(self.events())
        args = [self.launcher]
        if explicit_keys:
            args.extend(["--keys", self.keys])
        if host_engine:
            args.append("--host-engine")
        args.extend([image, "--", *command])
        _, output = run(
            args, env=self.launcher_env, cwd=self.workspace, tty=True,
            timeout=timeout, expected=expected,
        )
        events = self.events()[before:]
        launches = [event for event in events if event["args"][:1] == ["run"]]
        require(len(launches) == 1, "expected exactly one real engine run per launcher call")
        event = launches[0]
        argv = event["args"]
        require(event.get("env_mode") == 0o600, "temporary keys env-file is not 0600")
        env_file = Path(event["env_file"])
        require(env_file.parent == self.runtime, "keys env-file escaped isolated runtime")
        require(not env_file.exists(), "launcher left its temporary keys env-file")
        require(not list(self.runtime.iterdir()), "launcher left runtime files")
        require(stat.S_IMODE(self.keys.stat().st_mode) == 0o600, "keys.conf was not tightened")
        self.assert_removed(argv[argv.index("--name") + 1])
        return output, events

    def shell(self, image, script, **kwargs):
        return self.launch(image, ["bash", "-euo", "pipefail", "-c", script], **kwargs)

    def ownership(self, *paths):
        for path in paths:
            info = path.stat()
            require(
                (info.st_uid, info.st_gid) == (os.getuid(), os.getgid()),
                f"wrong host ownership for {path}: {info.st_uid}:{info.st_gid}",
            )

    def host_engine_access(self, image):
        if self.engine_name == "docker":
            # The launcher intentionally sees an isolated HOME. Preserve its
            # supported discovery order without exposing the user's config.
            sockets = (
                Path(self.engine_env.get("HOME", str(Path.home()))) / ".docker/run/docker.sock",
                Path("/var/run/docker.sock"),
            )
            socket = next((path for path in sockets if path.is_socket()), None)
            require(socket is not None, "host-engine smoke needs ~/.docker/run/docker.sock or /var/run/docker.sock")
            isolated_socket = self.home / ".docker/run/docker.sock"
            isolated_socket.parent.mkdir(parents=True)
            isolated_socket.symlink_to(socket.resolve(strict=True))
        else:
            socket = Path(f"/run/user/{os.getuid()}/podman/podman.sock")
            require(
                socket.is_socket(),
                "host-engine smoke needs systemctl --user start podman.socket",
            )

        sibling = "playpen-sibling-" + self.token
        # Register before launching: an interrupted docker client can leave a
        # successfully created sibling even when the launcher itself is gone.
        self.siblings.add(sibling)
        script = f'''
[[ $DOCKER_HOST = unix:///run/host-engine.sock ]]
[[ -S /run/host-engine.sock ]]
'''
        if self.engine_name == "podman":
            script += '[[ $CONTAINER_HOST = unix:///run/host-engine.sock ]]\n'
        else:
            script += '[[ ! -v CONTAINER_HOST ]]\n'
        script += f'''
sibling={shlex.quote(sibling)}
trap 'docker rm -f "$sibling" >/dev/null 2>&1 || true' EXIT
docker ps
docker run --detach --pull=never --name "$sibling" {shlex.quote(image)} sleep 120
names=$(docker ps --filter "name=$sibling" --format '{{{{.Names}}}}')
printf '%s\\n' "$names"
[[ $names = "$sibling" ]]
workload=$(docker exec "$sibling" printf 'SMOKE_SIBLING_OK\\n')
printf '%s\\n' "$workload"
[[ $workload = SMOKE_SIBLING_OK ]]
'''
        if self.engine_name == "podman":
            script += '''
names=$(podman --remote ps --filter "name=$sibling" --format '{{.Names}}')
printf '%s\\n' "$names"
[[ $names = "$sibling" ]]
printf 'SMOKE_PODMAN_REMOTE_OK\\n'
'''
        script += '''
docker rm -f "$sibling"
names=$(docker ps -a --filter "name=$sibling" --format '{{.Names}}')
[[ -z $names ]]
trap - EXIT
printf 'SMOKE_HOST_ENGINE_OK\\n'
'''
        output, _ = self.shell(image, script, host_engine=True)
        for marker in ("SMOKE_SIBLING_OK", "SMOKE_HOST_ENGINE_OK"):
            require(marker in output, f"host-engine output missing: {marker}")
        if self.engine_name == "podman":
            require("SMOKE_PODMAN_REMOTE_OK" in output, "Podman remote command did not execute")
        self.assert_removed(sibling)

    def exercise(self):
        for flavor in ("base", "rust", "jvm"):
            self.image_id(f"playpen-{flavor}:latest")
        raw = {
            flavor: self.tag(
                f"playpen-{flavor}:latest", f"playpen-{flavor}-{self.token}:raw"
            ) for flavor in ("base", "rust", "jvm")
        }
        boundary = f'''
id
printf 'HOME=%s\\n' "$HOME"
[[ $(id -u) = {os.getuid()} && $(id -g) = {os.getgid()} && $(id -u) != 0 ]]
[[ $PWD = /workspace ]]
[[ $HOME = /home/agent ]]
[[ $(cat /workspace/host-workspace) = workspace-input ]]
[[ $(cat /home/agent/host-home) = home-input ]]
[[ $SMOKE_DUMMY_SECRET = {shlex.quote(DUMMY)} ]]
[[ ! -v DOCKER_HOST && ! -v CONTAINER_HOST ]]
[[ ! -S /run/host-engine.sock && ! -S /var/run/docker.sock ]]
[[ ! -S /run/podman/podman.sock && ! -S /run/user/{os.getuid()}/podman/podman.sock ]]
printf 'workspace-output\\n' > /workspace/container-workspace
printf 'home-output\\n' > /home/agent/container-home
'''
        tools = '''
git --version
node --version
npm --version
bun --version
python3 --version
pipx --version
gcc --version
g++ --version
make --version
curl --version
podman --version
docker --version
sqlite3 --version
printf 'SMOKE_BASE_OK\\n'
'''
        output, _ = self.shell(raw["base"], boundary + tools, explicit_keys=True)
        for pattern in (r"git version \d", r"Python 3\.", r"podman version \d", r"Docker version \d", r"SMOKE_BASE_OK"):
            require(re.search(pattern, output), f"base tool output missing: {pattern}")
        require((self.workspace / "container-workspace").read_text() == "workspace-output\n", "workspace mount did not persist")
        require((self.agent_home / "container-home").read_text() == "home-output\n", "agent-home mount did not persist")
        self.ownership(self.workspace / "container-workspace", self.agent_home / "container-home")
        output, _ = self.shell(
            raw["base"], boundary + "printf 'SMOKE_EXIT_37\\n'; exit 37", expected=37
        )
        require("SMOKE_EXIT_37" in output, "exit propagation command did not execute")
        self.host_engine_access(raw["base"])

        project = self.workspace / "rust-example"
        (project / "src").mkdir(parents=True)
        (project / "Cargo.toml").write_text(
            '[package]\nname = "playpen-smoke"\nversion = "0.0.0"\nedition = "2021"\n'
        )
        (project / "src/main.rs").write_text('fn main() { println!("SMOKE_RUST_OK"); }\n')
        output, _ = self.shell(raw["rust"], boundary + '''
rustc --version
cargo --version
rustfmt --version
cargo clippy --version
rust-analyzer --version
sccache --version
clang --version
cmake --version
rg --version
fd --version
git lfs version
[[ -d /opt/agent-cargo ]]
cd /workspace/rust-example
cargo build --offline
./target/debug/playpen-smoke
''', timeout=240)
        for pattern in (r"rustc \d+\.", r"cargo \d+\.", r"sccache \d+\.", r"SMOKE_RUST_OK"):
            require(re.search(pattern, output), f"Rust output missing: {pattern}")
        self.ownership(project / "Cargo.lock", project / "target/debug/playpen-smoke")

        (self.workspace / "Hello.java").write_text(
            'public class Hello { public static void main(String[] args) { '
            'System.out.println("SMOKE_JAVA_OK"); } }\n'
        )
        (self.workspace / "Hello.kt").write_text('fun main() { println("SMOKE_KOTLIN_OK") }\n')
        output, _ = self.shell(raw["jvm"], boundary + '''
java -version
javac -version
mvn --version
kotlinc -version
javac Hello.java
java Hello
kotlinc Hello.kt -include-runtime -d hello.jar
java -jar hello.jar
''', timeout=240)
        for pattern in (r"javac \d+", r"Apache Maven \d", r"kotlinc-jvm \d", r"SMOKE_JAVA_OK", r"SMOKE_KOTLIN_OK"):
            require(re.search(pattern, output), f"JVM output missing: {pattern}")
        self.ownership(self.workspace / "Hello.class", self.workspace / "hello.jar")

        # A unique bare flavor forces the real launcher to build its npm layer,
        # without overwriting an existing user's :omp image.
        flavor = "playpen-" + self.token
        self.tag("playpen-base:latest", flavor + ":latest")
        target = flavor + ":omp"
        self.aliases.append(target)
        first, _ = self.launch(flavor, ["omp", "--version"], timeout=1200)
        require(re.search(r"\b\d+\.\d+\.\d+\b", first), "omp --version did not report a version")
        image_id = self.image_id(target)
        second, _ = self.launch(flavor, ["omp", "--version"], timeout=120)
        require(self.image_id(target) == image_id, "second launch changed the harness image")
        require(re.search(r"\b\d+\.\d+\.\d+\b", second), "reused omp --version did not report a version")

    def cleanup(self):
        errors = []
        names = set(self.siblings)
        for event in self.events():
            args = event["args"]
            if args[:1] == ["run"] and "--name" in args:
                names.add(args[args.index("--name") + 1])
        for name in names:
            try:
                # Missing is normal after --rm; ps below distinguishes that
                # from a failed removal of a surviving container.
                self.engine_run("rm", "-f", name, timeout=30, expected=None, quiet=True)
                self.assert_removed(name)
            except Exception as error:
                errors.append(str(error))
        for image in reversed(self.aliases):
            try:
                code, _ = self.engine_run(
                    "image", "inspect", image, expected=None, quiet=True, timeout=30
                )
                if code == 0:
                    self.engine_run("image", "rm", image, quiet=True, timeout=30)
            except Exception as error:
                errors.append(str(error))
        try:
            code, _ = self.engine_run(
                "volume", "inspect", self.volume, expected=None, quiet=True, timeout=30
            )
            if code == 0:
                self.engine_run("volume", "rm", self.volume, quiet=True, timeout=30)
        except Exception as error:
            errors.append(str(error))
        if errors:
            raise SmokeError("cleanup failed:\n" + "\n".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", choices=("docker", "podman"), required=True)
    parser.add_argument("--launcher", type=Path, required=True)
    args = parser.parse_args()
    require(os.getuid() != 0, "run smoke as a non-root user with engine access")
    launcher = args.launcher.expanduser().resolve(strict=True)
    require(launcher.is_file() and os.access(launcher, os.X_OK), f"launcher is not executable: {launcher}")

    def interrupt(signum, _frame):
        raise Interrupted(signum)

    previous = {sig: signal.signal(sig, interrupt) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        with tempfile.TemporaryDirectory(prefix="playpen-smoke-") as directory:
            smoke = Smoke(args.engine, launcher, Path(directory))
            try:
                smoke.exercise()
            finally:
                original_error = sys.exc_info()[1]
                # A second signal must not interrupt bounded cleanup. Callable
                # handlers (unlike SIG_IGN) reset to defaults in exec children.
                for sig in previous:
                    signal.signal(sig, lambda _signum, _frame: None)
                try:
                    smoke.cleanup()
                except Exception as cleanup_error:
                    if original_error is None:
                        raise
                    print(f"smoke: cleanup also failed: {cleanup_error}", file=sys.stderr)
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)
    print(f"Smoke passed: {args.engine}; tools, compilers, launcher isolation, host engine access, sibling cleanup, exit status, and harness reuse")


if __name__ == "__main__":
    try:
        main()
    except Interrupted as error:
        print(f"smoke: {error}", file=sys.stderr)
        sys.exit(128 + error.signum)
    except (SmokeError, OSError, subprocess.SubprocessError) as error:
        print(f"smoke: {error}", file=sys.stderr)
        sys.exit(1)
