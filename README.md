# playpen

Personal agent sandbox: Oh My Pi in a rootless container (Podman by
default, Docker supported).
Two layers: `playpen-base` (language-neutral base, rebuild rarely) + thin
per-language flavor images (`playpen-rust`, `playpen-jvm`, ...).

Engine selection: the launcher uses podman unless `PLAYPEN_ENGINE=docker`
(or podman is absent and docker is found). Builds go through the same
engine: `make all ENGINE=docker`.

## Layout

```
base/                playpen-base: Fedora, git, bash, node, bun, omp, agent user
flavors/rust/        playpen-rust: rustup toolchain, sccache, native build deps
flavors/jvm/         playpen-jvm: JDK LTS, Maven, standalone Kotlin compiler
launcher/            Rust launcher crate (binary: playpen)
keys.conf.example    template for ~/.config/playpen/keys.conf (API keys)
Makefile             build helpers
```

Adding a flavor: one directory under `flavors/` with a Containerfile
(`FROM playpen-base:latest`), one rule in the Makefile, tag it
`playpen-<flavor>`. The launcher needs no changes: `playpen playpen-<flavor>`.

## Setup

```bash
# 1. Build everything (base first, then flavors)
make all              # or individually: make base, make rust, make jvm

# 2. Install the launcher (Rust binary to ~/.local/bin; needs cargo)
make install

# 3. One-time: create the throwaway agent home
mkdir -p ~/.local/share/playpen-home

# 4. API keys: one NAME=value per line (format: keys.conf.example)
mkdir -p ~/.config/playpen && chmod 700 ~/.config/playpen
echo 'MISTRAL_API_KEY=op://Private/Mistral API Key/credential' \
  > ~/.config/playpen/keys.conf
chmod 600 ~/.config/playpen/keys.conf
```

## Daily use

```bash
cd ~/Development/myproject
playpen playpen-rust    # Rust project
playpen playpen-jvm     # JVM/Kotlin project
playpen                 # no flavor: plain base
playpen --host-engine playpen-rust   # trusted project: + host engine socket
playpen --shell playpen-rust         # bash prompt instead of omp
playpen playpen-jvm -- omp --version # any other command inside
```

The container command is omp (started in /workspace), so quitting omp ends
the session and drops you back on the host - the container is removed.
`--shell` (or `-- bash`) gives the old interactive-shell behavior.

Inside the container, omp runs in yolo mode: its $HOME is
`~/.local/share/playpen-home` on your disk - expendable by design. The sandbox
boundary is the container's mounts - by default just the workspace and
the agent home.

## Notes

- The base image is `fedora-minimal` (`ARG FEDORA_VERSION` in
  `base/Containerfile`); bump it and `make base` to move to a newer release.
- Rust: `CARGO_HOME=/opt/agent-cargo`, mounted as the named volume
  `playpen-cargo` (only for `playpen-rust`), so the registry cache and
  `cargo install`-ed tools persist across sessions. sccache is wired in via `RUSTC_WRAPPER`; its cache lives
  under the persistent agent home, so clean rebuilds stay fast.
- JVM: JDK 25 (LTS) via `java-25-openjdk-devel`; change `JDK_VERSION` in
  `flavors/jvm/Containerfile` for another line (e.g. 21). Gradle projects
  rely on their wrapper; Maven covers wrapper-less builds. Standalone
  `kotlinc` is for scripts and quick experiments.
- User mapping: podman `--userns=keep-id`; docker `--user $UID:$GID`. Either
  way, files the agent creates are owned by you on the host.
- `:Z` labels bind mounts for SELinux (Fedora); the named volume is labeled
  automatically.
- API keys: `~/.config/playpen/keys.conf` (see `keys.conf.example`) - one
  `NAME=value` per line; a value is a literal or an `op://` reference. The
  launcher resolves references with the 1Password CLI (vault must be
  unlocked - expect one biometric prompt per launch) and passes every key
  to the container via a 0600 temp `--env-file`, deleted on exit. For the
  container's lifetime each key is readable by anything inside the
  container and visible to anyone with engine access (`podman inspect`
  shows env) - 1Password protects the credential at rest, not in use, so
  inject only what a project needs: `--keys <file>` swaps in a
  per-project key set. Literal values are plaintext in the config file -
  keep it mode 0600 (the launcher tightens it if looser) and prefer
  `op://`.
- Host Podman access (Testcontainers etc.) is opt-in per launch:
  `playpen --host-engine <flavor>` or `PLAYPEN_HOST_ENGINE=1` (per-project
  via direnv). It mounts the host podman socket and points `CONTAINER_HOST`
  / `DOCKER_HOST` at it. Without it the container gets no engine. With it
  the isolation boundary is your host user: code inside can create sibling
  containers that mount anything you can read and inspect every container
  on the engine - treat it as a trusted-project switch.
- The agent's git identity: set it once inside the persistent home
  (`git config --global user.name/user.email`) or mount a minimal gitconfig.

## Roadmap

The plan - Rust launcher, harness layering, published images, installer,
macOS support - lives in [ROADMAP.md](ROADMAP.md).
