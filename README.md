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
mkdir -p ~/.config/omp-sbx
echo 'MISTRAL_API_KEY=op://Private/Mistral API Key/credential' \
  > ~/.config/omp-sbx/keys.conf
```

## Daily use

```bash
cd ~/Development/myproject
omp-sbx omp-rust    # Rust project
omp-sbx omp-jvm     # JVM/Kotlin project
omp-sbx             # no flavor: plain base
```

Inside the container, omp runs in yolo mode safely: its $HOME is
`~/.local/share/omp-home` on your disk - expendable by design.

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
  to the container via `--env-file`. Resolved values exist only in a 0600
  tmpfs file for the duration of the launch; literal values are plaintext
  in the config file, so prefer `op://`.
- Testcontainers: the host podman socket is mounted and `CONTAINER_HOST` /
  `DOCKER_HOST` point at it - a conscious tradeoff: gives the container real
  host Podman.
- The agent's git identity: set it once inside the persistent home
  (`git config --global user.name/user.email`) or mount a minimal gitconfig.
