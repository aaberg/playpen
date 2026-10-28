# omp-container

Personal agent sandbox: Oh My Pi in a rootless Podman container.
Two layers: `omp-base` (language-neutral base, rebuild rarely) + thin
per-language flavor images (`omp-rust`, `omp-jvm`, ...).

## Layout

```
base/                omp-base: Fedora, git, zsh, node, bun, omp, agent user
flavors/rust/        omp-rust: rustup toolchain, sccache, native build deps
flavors/jvm/         omp-jvm: JDK LTS, Maven, standalone Kotlin compiler
bin/omp-sbx          launcher
keys.conf.example    template for ~/.config/omp-sbx/keys.conf (API keys)
Makefile             build helpers
```

Adding a flavor: one directory under `flavors/` with a Containerfile
(`FROM localhost/omp-base:latest`), one rule in the Makefile, tag it
`omp-<flavor>`. The launcher needs no changes: `omp-sbx omp-<flavor>`.

## Setup

```bash
# 1. Build everything (base first, then flavors)
make all              # or individually: make base, make rust, make jvm

# 2. Install the launcher
mkdir -p ~/bin && cp bin/omp-sbx ~/bin/ && chmod +x ~/bin/omp-sbx

# 3. One-time: create the throwaway agent home
mkdir -p ~/.local/share/omp-home

# 4. API keys: one NAME=value per line (format: keys.conf.example)
mkdir -p ~/.config/omp-sbx && chmod 700 ~/.config/omp-sbx
echo 'MISTRAL_API_KEY=op://Private/Mistral API Key/credential' \
  > ~/.config/omp-sbx/keys.conf
chmod 600 ~/.config/omp-sbx/keys.conf
```

## Daily use

```bash
cd ~/Development/myproject
omp-sbx omp-rust    # Rust project
omp-sbx omp-jvm     # JVM/Kotlin project
omp-sbx             # no flavor: plain base
omp-sbx --host-engine omp-rust   # trusted project: + host Podman socket
```

Inside the container, omp runs in yolo mode: its $HOME is
`~/.local/share/omp-home` on your disk - expendable by design. The sandbox
boundary is the container's mounts - by default just the workspace and
the agent home.

## Notes

- Fedora is pinned in `base/Containerfile` (`ARG FEDORA_VERSION`); bump it and
  `make base` to move to a newer release.
- Rust: `CARGO_HOME=/opt/agent-cargo`, mounted as the named podman volume
  `omp-cargo`, so the registry cache and `cargo install`-ed tools persist
  across sessions. sccache is wired in via `RUSTC_WRAPPER`; its cache lives
  under the persistent agent home, so clean rebuilds stay fast.
- JVM: JDK 25 (LTS) via `java-25-openjdk-devel`; change `JDK_VERSION` in
  `flavors/jvm/Containerfile` for another line (e.g. 21). Gradle projects
  rely on their wrapper; Maven covers wrapper-less builds. Standalone
  `kotlinc` is for scripts and quick experiments.
- `--userns=keep-id`: files the agent creates are owned by you on the host.
- `:Z` labels bind mounts for SELinux (Fedora); the named volume is labeled
  automatically.
- API keys: `~/.config/omp-sbx/keys.conf` (see `keys.conf.example`) - one
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
  `omp-sbx --host-engine <flavor>` or `OMP_SBX_HOST_ENGINE=1` (per-project
  via direnv). It mounts the host podman socket and points `CONTAINER_HOST`
  / `DOCKER_HOST` at it. Without it the container gets no engine. With it
  the isolation boundary is your host user: code inside can create sibling
  containers that mount anything you can read and inspect every container
  on the engine - treat it as a trusted-project switch.
- The agent's git identity: set it once inside the persistent home
  (`git config --global user.name/user.email`) or mount a minimal gitconfig.
