//! Integration tests: run the playpen binary against stub engines on PATH and
//! assert the assembled argv - the exit-criteria technique from the roadmap,
//! the same way the bash launcher was tested.
//!
//! Every test runs in an isolated fixture (HOME, XDG dirs, workspace) with a
//! scrubbed environment, so results do not depend on the host's podman,
//! docker, op or ~/.config. The stub engine also fakes the image commands
//! phase 2 added: `image inspect --format {{.Created}}` and `images` read a
//! state file of `ref<TAB>created` lines, and `build` records its argv plus
//! the Containerfile it was handed.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PLAYPEN_BIN: &str = env!("CARGO_BIN_EXE_playpen");

/// A stub engine. `run` records its argv (NUL-separated) and copies the
/// contents of any --env-file, then exits with $STUB_EXIT (or dies by
/// $STUB_SIGNAL so the launcher sees a signaled child). `build` records its
/// argv plus the recipe from the context dir. `image inspect` and `images`
/// read $STUB_STATE_DIR/images.txt. Builtins only: PATH in tests contains
/// just this stub.
const ENGINE_STUB: &str = r#"#!/bin/bash
case "$1" in
  run)
    printf '%s\0' "$@" > "$STUB_ARGS_FILE"
    : > "$STUB_ENV_FILE"
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == "--env-file" ]]; then
        while IFS= read -r line; do
          printf '%s\n' "$line"
        done < "$2" >> "$STUB_ENV_FILE"
      fi
      shift
    done
    if [[ -n "${STUB_SIGNAL:-}" ]]; then kill -"${STUB_SIGNAL}" "$$"; fi
    exit "${STUB_EXIT:-0}"
    ;;
  build)
    ctx="${@: -1}"
    {
      printf '%s\0' "$@"
      printf '%s\0' "RECIPE"
      printf '%s\0' "$(< "$ctx/Containerfile")"$'\n'
      printf '%s\0' "END"
    } >> "$STUB_BUILDS_FILE"
    exit 0
    ;;
  image)
    ref="${@: -1}"
    while IFS=$'\t' read -r r c; do
      if [[ "$r" == "$ref" ]]; then
        printf '%s\n' "$c"
        exit 0
      fi
    done < "$STUB_STATE_DIR/images.txt"
    printf 'Error: no such image: %s\n' "$ref" >&2
    exit 1
    ;;
  images)
    while IFS=$'\t' read -r r _; do
      printf '%s\n' "$r"
    done < "$STUB_STATE_DIR/images.txt"
    exit 0
    ;;
esac
exit 0
"#;

const OP_STUB: &str = "#!/bin/bash\necho resolved-secret-value\nexit ${STUB_OP_STATUS:-0}\n";

/// The omp harness recipe the launcher generates for a flavor.
fn omp_recipe(flavor: &str) -> String {
    format!(
        "# playpen harness layer: omp on {flavor}\n\
         FROM {flavor}:latest\n\
         USER root\n\
         RUN npm install -g @oh-my-pi/pi-coding-agent \\\n\
         \x20   && chown -R agent:agent /home/agent\n\
         USER agent\n\
         WORKDIR /workspace\n"
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    runtime: PathBuf,
    work: PathBuf,
    state: PathBuf,
    args_file: PathBuf,
    builds_file: PathBuf,
    env_file: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = Fixture {
            bin: dir.path().join("bin"),
            home: dir.path().join("home"),
            config: dir.path().join("config"),
            data: dir.path().join("data"),
            runtime: dir.path().join("runtime"),
            work: dir.path().join("work"),
            state: dir.path().join("state"),
            args_file: dir.path().join("stub-args"),
            builds_file: dir.path().join("stub-builds"),
            env_file: dir.path().join("stub-env"),
            _dir: dir,
        };
        for dir in [
            &fixture.bin,
            &fixture.home,
            &fixture.config,
            &fixture.data,
            &fixture.runtime,
            &fixture.work,
            &fixture.state,
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(fixture.state.join("images.txt"), "").unwrap();
        fixture
    }

    fn stub_engine(&self, name: &str) {
        write_exec(&self.bin.join(name), ENGINE_STUB);
    }

    fn stub_op(&self) {
        write_exec(&self.bin.join("op"), OP_STUB);
    }

    fn keys_conf(&self, content: &str) -> PathBuf {
        let dir = self.config.join("playpen");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keys.conf");
        fs::write(&file, content).unwrap();
        file
    }

    /// Seed an image with a Created timestamp (any engine layout).
    fn image(&self, image_ref: &str, created: &str) {
        let line = format!("{image_ref}\t{created}\n");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.state.join("images.txt"))
            .unwrap();
        file.write_all(line.as_bytes()).unwrap();
    }

    /// Seed `playpen-<flavor>:latest` only: its harness image is missing,
    /// so a run must build the layer.
    fn seed_flavor(&self, flavor: &str) {
        self.image(&format!("{flavor}:latest"), "2024-01-01T00:00:00Z");
    }

    /// Seed the flavor and a fresh (newer) harness layer: no build needed.
    fn seed_fresh(&self, flavor: &str) {
        self.seed_flavor(flavor);
        self.image(&format!("{flavor}:omp"), "2025-06-01T00:00:00Z");
    }

    fn harness_conf(&self, content: &str) {
        let dir = self.config.join("playpen");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("harness.conf"), content).unwrap();
    }

    fn harness_conf_contents(&self) -> String {
        fs::read_to_string(self.config.join("playpen/harness.conf")).unwrap_or_default()
    }

    fn playpen(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(PLAYPEN_BIN);
        cmd.env_clear();
        cmd.env("PATH", &self.bin);
        cmd.env("HOME", &self.home);
        cmd.env("PWD", &self.work);
        cmd.env("XDG_CONFIG_HOME", &self.config);
        cmd.env("XDG_DATA_HOME", &self.data);
        cmd.env("XDG_RUNTIME_DIR", &self.runtime);
        cmd.env("STUB_ARGS_FILE", &self.args_file);
        cmd.env("STUB_ENV_FILE", &self.env_file);
        cmd.env("STUB_BUILDS_FILE", &self.builds_file);
        cmd.env("STUB_STATE_DIR", &self.state);
        cmd.current_dir(&self.work);
        cmd.args(args);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.playpen(args).output().expect("run playpen")
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = self.playpen(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().expect("run playpen")
    }

    /// Run with the given stdin, for the interactive menu.
    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut cmd = self.playpen(args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn playpen");
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().expect("wait playpen")
    }

    /// The argv the stub engine received for `run`.
    fn argv(&self) -> Vec<String> {
        let bytes = fs::read(&self.args_file).unwrap_or_default();
        bytes
            .split(|b| *b == 0)
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect()
    }

    /// The recorded `build` invocations: (argv, recipe) per build.
    fn builds(&self) -> Vec<(Vec<String>, String)> {
        let bytes = fs::read(&self.builds_file).unwrap_or_default();
        let chunks: Vec<String> = bytes
            .split(|b| *b == 0)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect();
        let mut builds = Vec::new();
        let mut i = 0;
        while i < chunks.len() {
            if chunks[i].is_empty() || chunks[i] == "END" {
                i += 1;
                continue;
            }
            let mut argv = Vec::new();
            while i < chunks.len() && chunks[i] != "RECIPE" {
                argv.push(chunks[i].clone());
                i += 1;
            }
            if i + 1 >= chunks.len() {
                break; // truncated: no recipe chunk
            }
            builds.push((argv, chunks[i + 1].clone()));
            i += 2; // the trailing END is skipped at the loop top
        }
        builds
    }

    /// Contents of the env-file the engine received, or "" when none.
    fn env_file_contents(&self) -> String {
        fs::read_to_string(&self.env_file).unwrap_or_default()
    }

    fn stderr(&self, out: &Output) -> String {
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn stdout(&self, out: &Output) -> String {
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn leftover_env_files(&self) -> Vec<String> {
        fs::read_dir(&self.runtime)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("playpen-env."))
            .collect()
    }

    fn assert_ok(&self, out: &Output) {
        assert!(
            out.status.success(),
            "expected success, exit {:?}\nstderr: {}",
            out.status.code(),
            self.stderr(out)
        );
    }

    /// A recorded build: right tag, recipe, and -f pointing into the context.
    fn assert_build(&self, build: &(Vec<String>, String), target: &str, recipe: &str) {
        let (argv, recorded_recipe) = build;
        assert_eq!(argv[0], "build");
        let target_pos = argv
            .iter()
            .position(|a| a == "-t")
            .expect("build passes -t")
            + 1;
        assert_eq!(argv[target_pos], target);
        let file = argv
            .iter()
            .zip(argv.iter().skip(1))
            .find(|(a, _)| a.as_str() == "-f")
            .map(|(_, b)| b.clone())
            .expect("build passes -f");
        let context = argv.last().unwrap();
        assert!(
            file.starts_with(&format!("{context}/")),
            "-f file lives in the context: {file}"
        );
        assert!(file.ends_with("/Containerfile"), "-f names the recipe: {file}");
        assert_eq!(recorded_recipe, recipe);
    }
}

fn write_exec(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn container_name(argv: &[String]) -> &str {
    argv.iter()
        .zip(argv.iter().skip(1))
        .find(|(a, _)| a.as_str() == "--name")
        .map(|(_, b)| b.as_str())
        .expect("argv contains --name")
}

fn assert_container_name_pattern(argv: &[String]) {
    let name = container_name(argv);
    let pid = name
        .strip_prefix("playpen-agent-")
        .expect("container name is playpen-agent-<pid>")
        .parse::<u32>()
        .expect("pid suffix parses");
    assert!(pid > 0);
}

#[test]
fn default_run_builds_harness_then_runs_omp() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_flavor("playpen-base");
    let out = f.run(&[]);
    f.assert_ok(&out);

    // the harness layer is missing: one build with the omp recipe
    let builds = f.builds();
    assert_eq!(builds.len(), 1, "one build: {builds:?}");
    f.assert_build(&builds[0], "playpen-base:omp", &omp_recipe("playpen-base"));
    assert!(
        f.stderr(&out).contains("playpen: building harness image playpen-base:omp"),
        "the build is announced: {}",
        f.stderr(&out)
    );

    let argv = f.argv();
    assert_eq!(
        argv,
        vec![
            "run".to_string(),
            "--rm".to_string(),
            "-it".to_string(),
            "--userns=keep-id".to_string(),
            "--name".to_string(),
            container_name(&argv).to_string(),
            "-v".to_string(),
            format!("{}:/workspace:Z", f.work.display()),
            "-v".to_string(),
            format!("{}:/home/agent:Z", f.data.join("playpen-home").display()),
            "playpen-base:omp".to_string(),
            "omp".to_string(),
        ]
    );
    assert_container_name_pattern(&argv);
    // no keys file: warn, but still launch without --env-file
    assert!(f.stderr(&out).contains("launching without API keys"));
    assert_eq!(f.env_file_contents(), "");
    // the agent home is created on first use
    assert!(f.data.join("playpen-home").is_dir());
    assert!(f.stderr(&out).contains("playpen: created"));
}

#[test]
fn fresh_harness_image_is_not_rebuilt() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    let out = f.run(&[]);
    f.assert_ok(&out);
    assert!(f.builds().is_empty(), "nothing to build: {:?}", f.builds());
    assert!(f.argv().contains(&"playpen-base:omp".to_string()));
}

#[test]
fn stale_harness_image_is_rebuilt() {
    // podman-format Created values prove the other timestamp layout parses
    let f = Fixture::new();
    f.stub_engine("podman");
    f.image("playpen-jvm:latest", "2025-11-02 10:00:00.000000000 +0100 CET");
    f.image("playpen-jvm:omp", "2025-11-01 09:00:00.000000000 +0100 CET");
    let out = f.run(&["playpen-jvm"]);
    f.assert_ok(&out);

    let builds = f.builds();
    assert_eq!(builds.len(), 1, "one rebuild: {builds:?}");
    let (argv, _recipe) = &builds[0];
    assert_eq!(argv[0], "build");
    assert!(
        !argv.contains(&"--no-cache".to_string()),
        "staleness rebuilds use the cache: {argv:?}"
    );
    f.assert_build(&builds[0], "playpen-jvm:omp", &omp_recipe("playpen-jvm"));
    assert!(f
        .stderr(&out)
        .contains("playpen: rebuilding harness image playpen-jvm:omp (flavor image is newer)"));
    assert!(f.argv().contains(&"playpen-jvm:omp".to_string()));
}

#[test]
fn missing_flavor_image_is_fatal_with_a_hint() {
    let f = Fixture::new();
    f.stub_engine("podman");
    let out = f.run(&["playpen-jvm"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("playpen: playpen-jvm:latest not found - build the flavor first (make jvm)"));
    assert!(f.builds().is_empty(), "no build without a flavor");
    assert!(f.argv().is_empty(), "the engine must not run");
}

#[test]
fn shell_mode_runs_bash() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    let out = f.run(&["--shell"]);
    f.assert_ok(&out);
    let argv = f.argv();
    let pos = argv
        .iter()
        .position(|a| a == "playpen-base:omp")
        .expect("harness image");
    assert_eq!(&argv[pos + 1..], &["/bin/bash".to_string()]);
    assert_eq!(argv.len(), pos + 2, "bash is the only command word: {argv:?}");
}

#[test]
fn passthrough_command_wins_and_keeps_flags() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-jvm");
    f.seed_fresh("playpen-base");

    let out = f.run(&["playpen-jvm", "--", "omp", "--version"]);
    f.assert_ok(&out);
    let argv = f.argv();
    let pos = argv.iter().position(|a| a == "playpen-jvm:omp").expect("image");
    assert_eq!(&argv[pos + 1..], &["omp".to_string(), "--version".to_string()]);

    // flag-like words and further `--` stay verbatim
    let out = f.run(&["--", "bash", "-c", "true", "--tail"]);
    f.assert_ok(&out);
    let argv = f.argv();
    let pos = argv
        .iter()
        .position(|a| a == "playpen-base:omp")
        .expect("default image");
    assert_eq!(
        &argv[pos + 1..],
        &[
            "bash".to_string(),
            "-c".to_string(),
            "true".to_string(),
            "--tail".to_string()
        ]
    );
}

#[test]
fn shell_and_command_conflict_is_an_error() {
    let f = Fixture::new();
    f.stub_engine("podman");
    let out = f.run(&["--shell", "--", "omp"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("--shell cannot be combined with a command after --"));
    assert!(f.argv().is_empty(), "the engine must not run");
}

#[test]
fn cargo_volume_only_for_rust_images() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    f.seed_fresh("playpen-rust");
    f.seed_fresh("playpen-rust-nightly");

    let out = f.run(&["playpen-rust"]);
    f.assert_ok(&out);
    let argv = f.argv();
    let cargo = argv
        .iter()
        .position(|a| a == "playpen-cargo:/opt/agent-cargo")
        .expect("cargo volume for playpen-rust");
    let home = argv
        .iter()
        .position(|a| a.ends_with(":/home/agent:Z"))
        .expect("home mount");
    let image = argv
        .iter()
        .position(|a| a == "playpen-rust:omp")
        .expect("image");
    assert!(home < cargo && cargo < image, "volume sits between home and image");

    let out = f.run(&["playpen-base"]);
    f.assert_ok(&out);
    assert!(!f.argv().contains(&"playpen-cargo:/opt/agent-cargo".to_string()));

    // bash glob `playpen-rust*`: a longer rust-ish image also gets it
    let out = f.run(&["playpen-rust-nightly"]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"playpen-cargo:/opt/agent-cargo".to_string()));
}

#[test]
fn docker_runs_as_invoking_user() {
    let f = Fixture::new();
    f.stub_engine("docker");
    f.seed_fresh("playpen-base");
    let out = f.run_with(&[], &[("PLAYPEN_ENGINE", "docker")]);
    f.assert_ok(&out);
    let argv = f.argv();
    let user = argv
        .iter()
        .zip(argv.iter().skip(1))
        .find(|(a, _)| a.as_str() == "--user")
        .map(|(_, b)| b.clone())
        .expect("docker gets --user");
    assert_eq!(
        user,
        format!("{}:{}", unsafe { libc::getuid() }, unsafe { libc::getgid() })
    );
    assert!(!argv.iter().any(|a| a == "--userns=keep-id"));
}

#[test]
fn autodetect_prefers_podman_then_docker() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.stub_engine("docker");
    f.seed_fresh("playpen-base");
    let out = f.run(&[]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"--userns=keep-id".to_string()));

    let f = Fixture::new();
    f.stub_engine("docker");
    f.seed_fresh("playpen-base");
    let out = f.run(&[]);
    f.assert_ok(&out);
    assert!(!f.argv().contains(&"--userns=keep-id".to_string()));
    assert!(f.argv().iter().any(|a| a == "--user"));
}

#[test]
fn no_engine_found_is_fatal() {
    let f = Fixture::new();
    let out = f.run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains("neither podman nor docker found in PATH"));
}

#[test]
fn unknown_engine_pref_is_fatal() {
    let f = Fixture::new();
    f.stub_engine("podman");
    let out = f.run_with(&[], &[("PLAYPEN_ENGINE", "containerd")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("unknown engine: containerd (expected podman or docker)"));
}

#[test]
fn keys_lifecycle_literal_and_op_reference() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.stub_op();
    f.seed_fresh("playpen-base");
    f.keys_conf(
        "MISTRAL_API_KEY=literal-key\n\
         # a comment\n\
         OPENAI_API_KEY=op://Private/OpenAI/skey\n",
    );
    let out = f.run(&[]);
    f.assert_ok(&out);

    let argv = f.argv();
    let envfile = argv
        .iter()
        .position(|a| a == "--env-file")
        .expect("--env-file present");
    let image = argv
        .iter()
        .position(|a| a == "playpen-base:omp")
        .expect("image");
    assert!(envfile < image);
    let path = &argv[envfile + 1];
    assert!(
        path.starts_with(format!("{}/playpen-env.", f.runtime.display()).as_str()),
        "env file lives in XDG_RUNTIME_DIR: {path}"
    );

    assert_eq!(
        f.env_file_contents(),
        "MISTRAL_API_KEY=literal-key\nOPENAI_API_KEY=resolved-secret-value\n"
    );
    // the temp env-file is removed when the launcher exits
    assert!(f.leftover_env_files().is_empty(), "no playpen-env.* left behind");
}

#[test]
fn keys_parsing_edges() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.stub_op();
    f.seed_fresh("playpen-base");
    f.keys_conf(
        "# full line comment\n\
         \n\
         NO_EQUALS_LINE\n\
         EMPTY=\n\
         =LEADING_EQ\n\
         SPACED NAME = value kept \r\n\
         CRLF_KEY=crlf-value\r\n\
         KEY_WITH_EQ=a=b=c\n\
         OPKEY=op://Private/x/y\n\
         LAST_NO_NEWLINE=tail",
    );
    let out = f.run(&[]);
    f.assert_ok(&out);
    // verified against the bash launcher: IFS='=' read keeps the leading
    // space in values ("SPACED NAME = value kept" -> " value kept ")
    assert_eq!(
        f.env_file_contents(),
        "SPACEDNAME= value kept \nCRLF_KEY=crlf-value\nKEY_WITH_EQ=a=b=c\n\
         OPKEY=resolved-secret-value\nLAST_NO_NEWLINE=tail\n"
    );
}

#[test]
fn keys_op_failure_aborts_launch() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    write_exec(
        &f.bin.join("op"),
        "#!/bin/bash\necho nope >&2\nexit 1\n",
    );
    f.keys_conf("OPKEY=op://Private/x/y\n");
    let out = f.run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("playpen: could not resolve 1Password reference for OPKEY"));
    assert!(f.argv().is_empty(), "the engine must not run");
    assert!(f.leftover_env_files().is_empty());
}

#[test]
fn keys_missing_explicit_file_is_fatal() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    let out = f.run(&["--keys", "/nonexistent/keys.conf"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("keys file not found: /nonexistent/keys.conf"));
}

#[test]
fn keys_file_with_only_comments_launches_without_env_file() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    f.keys_conf("# nothing here\n\n");
    let out = f.run(&[]);
    f.assert_ok(&out);
    assert!(!f.argv().contains(&"--env-file".to_string()));
    assert_eq!(f.env_file_contents(), "");
}

#[test]
fn keys_permissions_are_tightened() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    let keys = f.keys_conf("KEY=value\n");
    fs::set_permissions(&keys, fs::Permissions::from_mode(0o644)).unwrap();

    let out = f.run(&[]);
    f.assert_ok(&out);
    assert!(f
        .stderr(&out)
        .contains("was group/other-readable - tightened to 600"));
    let mode = fs::metadata(&keys).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // a second run over an already-tight file stays silent
    let out = f.run(&[]);
    f.assert_ok(&out);
    assert!(!f.stderr(&out).contains("tightened"));
}

#[test]
fn host_engine_docker_mounts_socket() {
    let f = Fixture::new();
    f.stub_engine("docker");
    f.seed_fresh("playpen-base");
    let sock_dir = f.home.join(".docker/run");
    fs::create_dir_all(&sock_dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(sock_dir.join("docker.sock")).unwrap();

    let out = f.run_with(&["--host-engine"], &[("PLAYPEN_ENGINE", "docker")]);
    f.assert_ok(&out);
    assert!(f
        .stderr(&out)
        .contains("host engine access enabled - the boundary is your host user"));

    let argv = f.argv();
    assert!(argv.contains(&"-v".to_string()));
    assert!(argv.contains(&format!(
        "{}:/run/host-engine.sock",
        sock_dir.join("docker.sock").display()
    )));
    assert!(argv.contains(&"DOCKER_HOST=unix:///run/host-engine.sock".to_string()));
    assert!(!argv.iter().any(|a| a.starts_with("CONTAINER_HOST")));
    drop(listener);
}

#[test]
fn host_engine_podman_missing_socket_reports_hint() {
    // The podman socket path (/run/user/$UID/...) is host-global and cannot
    // be faked in a test, so the argv shape for podman is asserted by the
    // engine.rs unit test; here we check the missing-socket diagnostics,
    // guarded to stay meaningful only when the host socket is truly absent.
    let sock = PathBuf::from(format!("/run/user/{}/podman/podman.sock", unsafe {
        libc::getuid()
    }));
    if sock.exists() {
        return;
    }
    let f = Fixture::new();
    f.stub_engine("podman");
    let out = f.run_with(&["--host-engine"], &[("PLAYPEN_ENGINE", "podman")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains(
        "host podman socket not found - start it with: systemctl --user start podman.socket"
    ));
    assert!(f.argv().is_empty());
}

#[test]
fn host_engine_docker_missing_socket_is_fatal() {
    // /var/run/docker.sock is host-global: a rootful docker daemon makes
    // the launcher legitimately proceed through its fallback, so guard
    // against host state like the podman test does.
    if Path::new("/var/run/docker.sock").exists() {
        return;
    }
    let f = Fixture::new();
    f.stub_engine("docker");
    let out = f.run_with(&["--host-engine"], &[("PLAYPEN_ENGINE", "docker")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("host docker socket not found - is the docker daemon running?"));
}

#[test]
fn host_engine_via_environment_variable() {
    let f = Fixture::new();
    f.stub_engine("docker");
    f.seed_fresh("playpen-base");
    let sock_dir = f.home.join(".docker/run");
    fs::create_dir_all(&sock_dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(sock_dir.join("docker.sock")).unwrap();

    // 1/true/yes enable, like the bash regex ^(1|true|yes)$
    let out = f.run_with(&[], &[("PLAYPEN_HOST_ENGINE", "1"), ("PLAYPEN_ENGINE", "docker")]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"DOCKER_HOST=unix:///run/host-engine.sock".to_string()));

    let out = f.run_with(&[], &[("PLAYPEN_HOST_ENGINE", "yes"), ("PLAYPEN_ENGINE", "docker")]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"DOCKER_HOST=unix:///run/host-engine.sock".to_string()));

    // anything else leaves it off
    let out = f.run_with(&[], &[("PLAYPEN_HOST_ENGINE", "0"), ("PLAYPEN_ENGINE", "docker")]);
    f.assert_ok(&out);
    assert!(!f.argv().iter().any(|a| a.starts_with("DOCKER_HOST")));

    let out = f.run_with(&[], &[("PLAYPEN_HOST_ENGINE", "sure"), ("PLAYPEN_ENGINE", "docker")]);
    f.assert_ok(&out);
    assert!(!f.argv().iter().any(|a| a.starts_with("DOCKER_HOST")));
    drop(listener);
}

#[test]
fn exit_code_of_the_container_propagates() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    let out = f.run_with(&[], &[("STUB_EXIT", "42")]);
    assert_eq!(out.status.code(), Some(42));
}

#[test]
fn signaled_container_exits_128_plus_signal() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    let out = f.run_with(&[], &[("STUB_SIGNAL", "TERM")]);
    assert_eq!(out.status.code(), Some(143));
}

#[test]
fn sigquit_removes_the_env_file_and_exits_131() {
    // SIGQUIT must not leak the resolved secrets: the signal layer removes
    // the env-file and exits 128+3 while the engine child is still running.
    // The stub plays along with the build flow (harness image missing ->
    // build succeeds) and quits its parent at `run` time, then closes the
    // inherited pipe fds (so output() gets EOF as soon as the launcher dies)
    // and sleeps to keep the launcher blocked in status() - no race with
    // normal completion.
    let f = Fixture::new();
    f.stub_op();
    f.seed_flavor("playpen-base");
    f.keys_conf("OPKEY=op://Private/x/y\n");
    write_exec(
        &f.bin.join("podman"),
        r#"#!/bin/bash
case "$1" in
  run)
    kill -QUIT "$PPID"
    exec >/dev/null 2>&1
    sleep 5
    exit 0
    ;;
  image)
    if [[ "${@: -1}" == "playpen-base:omp" ]]; then exit 1; fi
    printf '2024-01-01 00:00:00.000000000 +0000 UTC\n'
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
"#,
    );
    let out = f.run(&[]);
    assert_eq!(out.status.code(), Some(131));
    assert!(
        f.leftover_env_files().is_empty(),
        "SIGQUIT must not leave the env-file behind"
    );
}

#[test]
fn keys_flag_after_separator_is_a_passthrough() {
    // the review finding: a trailing --keys after -- belongs to the
    // container command, it must not trip launcher validation
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");
    let out = f.run(&["--", "echo", "--keys"]);
    f.assert_ok(&out);
    let argv = f.argv();
    let pos = argv
        .iter()
        .position(|a| a == "playpen-base:omp")
        .expect("default image");
    assert_eq!(&argv[pos + 1..], &["echo".to_string(), "--keys".to_string()]);
}

#[test]
fn keys_flag_may_take_the_separator_as_its_value() {
    // bash grabs the token after --keys sight-unseen, even `--`; the result
    // is then a missing keys file named "--" (not a usage error)
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_flavor("playpen-jvm");
    let out = f.run(&["--keys", "--", "playpen-jvm"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains("keys file not found: --"));
    assert!(f.argv().is_empty(), "the engine must not run");
}

#[test]
fn bare_double_dash_needs_a_command() {
    let f = Fixture::new();
    f.stub_engine("podman");
    let out = f.run(&["--"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains("playpen: -- needs a command"));
    assert!(f.argv().is_empty());
}

#[test]
fn keys_flag_without_value_is_an_error() {
    let f = Fixture::new();
    f.stub_engine("podman");
    let out = f.run(&["--keys"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains("--keys needs a file argument"));
}

#[test]
fn unknown_option_and_second_image_are_errors() {
    let f = Fixture::new();
    f.stub_engine("podman");
    let out = f.run(&["--bogus"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.argv().is_empty());

    let out = f.run(&["playpen-jvm", "playpen-rust"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.argv().is_empty());
}

#[test]
fn help_and_version_print_and_exit_zero() {
    let f = Fixture::new();
    let out = f.run(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = f.stdout(&out);
    assert!(stdout.contains("playpen"));
    assert!(stdout.contains("PLAYPEN_ENGINE"));
    assert!(stdout.contains("harness"), "help mentions the harness layering");

    let out = f.run(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(f.stdout(&out).contains("playpen"));
}

#[test]
fn harness_flag_selects_and_validates() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");

    // rejected before any engine work: no run, no build
    let out = f.run(&["--harness", "bogus"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("playpen: unknown harness 'bogus' (available: omp)"));
    assert!(f.argv().is_empty());
    assert!(f.builds().is_empty());

    let out = f.run(&["--harness", "omp"]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"playpen-base:omp".to_string()));
}

#[test]
fn harness_conf_selects_and_flag_overrides_it() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.seed_fresh("playpen-base");

    // a broken selection is fatal, with the file it came from
    f.harness_conf("HARNESS=bogus\n");
    let out = f.run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains(
        "playpen: unknown harness 'bogus' in"
    ));
    assert!(f.stderr(&out).contains("harness.conf"));

    // ... and the flag wins over the conf
    let out = f.run(&["--harness", "omp"]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"playpen-base:omp".to_string()));
}

#[test]
fn tagged_and_foreign_images_run_verbatim() {
    let f = Fixture::new();
    f.stub_engine("podman");

    // an explicit tag pins the image: no layering, no build, no inspect need
    let out = f.run(&["playpen-jvm:latest"]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"playpen-jvm:latest".to_string()));
    assert!(f.builds().is_empty());
    assert!(!f.argv().contains(&"playpen-jvm:omp".to_string()));

    // foreign images are not playpen flavors
    let out = f.run(&["alpine", "--", "sh"]);
    f.assert_ok(&out);
    assert!(f.argv().contains(&"alpine".to_string()));
    assert!(f.builds().is_empty());
}

#[test]
fn harness_menu_writes_the_selection() {
    let f = Fixture::new();
    f.stub_engine("podman"); // not needed, but proves the menu is engine-free
    let out = f.run_with_stdin(&["harness"], "1\n");
    f.assert_ok(&out);
    assert!(
        f.stdout(&out).contains("Available harnesses:"),
        "menu on stdout, got:\nstdout: {}\nstderr: {}",
        f.stdout(&out),
        f.stderr(&out)
    );
    assert!(f.stdout(&out).contains("omp"));
    assert_eq!(f.harness_conf_contents(), "HARNESS=omp\n");
    assert!(f.stdout(&out).contains("saved to"));
}

#[test]
fn harness_menu_preserves_other_lines_and_takes_names() {
    let f = Fixture::new();
    f.harness_conf("# pick once\nplaypen-jvm=vibe\n");
    let out = f.run_with_stdin(&["harness"], "omp\n");
    f.assert_ok(&out);
    let conf = f.harness_conf_contents();
    assert_eq!(conf, "# pick once\nplaypen-jvm=vibe\nHARNESS=omp\n");
}

#[test]
fn harness_menu_rejects_bad_input() {
    let f = Fixture::new();
    let out = f.run_with_stdin(&["harness"], "7\n");
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains("no harness number 7"));
    assert_eq!(f.harness_conf_contents(), "", "nothing written");

    let out = f.run_with_stdin(&["harness"], "bogus\n");
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains("unknown harness 'bogus'"));

    // EOF (Ctrl-D) aborts without writing
    let out = f.run_with_stdin(&["harness"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(f.stderr(&out).contains("no selection - nothing written"));
}

#[test]
fn harness_update_rebuilds_matching_images_cache_busted() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.image("localhost/playpen-base:omp", "2025-06-01T00:00:00Z");
    f.image("playpen-jvm:omp", "2025-06-01T00:00:00Z");
    f.image("playpen-jvm:latest", "2025-01-01T00:00:00Z");
    f.image("docker.io/library/node:22", "2025-05-01T00:00:00Z");

    let out = f.run(&["harness", "update"]);
    f.assert_ok(&out);

    // every playpen*:<selected harness> image, nothing else
    let builds = f.builds();
    assert_eq!(builds.len(), 2, "two updates: {builds:?}");
    // update rebuilds in listing order, cache-busted
    let targets: Vec<&str> = builds.iter().map(|(argv, _)| argv[3].as_str()).collect();
    assert_eq!(
        targets,
        vec!["localhost/playpen-base:omp", "playpen-jvm:omp"],
        "{builds:?}"
    );
    for (argv, recipe) in &builds {
        assert_eq!(argv[0], "build");
        assert_eq!(argv[1], "--no-cache", "update is cache-busted: {argv:?}");
        let target = argv[3].as_str();
        let (repo, tag) = target.rsplit_once(':').expect("listing prints repo:tag");
        assert_eq!(tag, "omp");
        assert!(
            recipe.contains(&format!("FROM {repo}:latest")),
            "{target} builds from {repo}:latest: {recipe}"
        );
    }
    assert!(f.stderr(&out).contains("playpen: updating playpen-jvm:omp"));
    assert!(f.stderr(&out).contains("playpen: updating localhost/playpen-base:omp"));
}

#[test]
fn harness_update_without_matching_images_is_a_no_op() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.image("playpen-jvm:latest", "2025-01-01T00:00:00Z");
    f.image("docker.io/library/node:22", "2025-05-01T00:00:00Z");

    let out = f.run(&["harness", "update"]);
    f.assert_ok(&out);
    assert!(f
        .stderr(&out)
        .contains("playpen: no playpen*:omp images to update"));
    assert!(f.builds().is_empty());
}

#[test]
fn harness_update_name_argument_and_unknowns() {
    let f = Fixture::new();
    f.stub_engine("podman");
    f.image("playpen-jvm:omp", "2025-06-01T00:00:00Z");

    let out = f.run(&["harness", "update", "omp"]);
    f.assert_ok(&out);
    assert_eq!(f.builds().len(), 1);
    f.assert_build(&f.builds()[0], "playpen-jvm:omp", &omp_recipe("playpen-jvm"));

    let out = f.run(&["harness", "update", "bogus"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(f
        .stderr(&out)
        .contains("playpen: unknown harness 'bogus' (available: omp)"));
}
