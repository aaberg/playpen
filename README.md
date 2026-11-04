# playpen

Personal agent sandbox: an AI coding agent (omp by default) in a rootless
container (Podman by default, Docker supported).
Two image layers: flavors (`playpen-base`, rebuilt rarely, plus per-language
`playpen-rust`, `playpen-jvm`, ...) and thin harness layers on top
(`playpen-jvm:omp`) that the launcher builds on demand.

Engine selection: the launcher uses podman unless `PLAYPEN_ENGINE=docker`
(or podman is absent and docker is found). Builds go through the same
engine: `make all ENGINE=docker`.

## Layout

```
base/                playpen-base: Fedora, git, bash, node, bun, agent user
flavors/rust/        playpen-rust: rustup toolchain, sccache, native build deps
flavors/jvm/         playpen-jvm: JDK LTS, Maven, Kotlin compiler, Kotlin/Java LSPs
launcher/            Rust launcher crate (binary: playpen)
install.sh           curl-installer for the released musl binary
.github/workflows    PR/main validation and release-only multi-arch publication
keys.conf.example    template for ~/.config/playpen/keys.conf (API keys)
Makefile             build helpers
```

Adding a flavor: one directory under `flavors/` with a Containerfile
(`FROM playpen-base:latest`), one rule in the Makefile, tag it
`playpen-<flavor>`. The launcher needs no changes: `playpen playpen-<flavor>`
gives it a harness layer automatically.

## Setup

Install the launcher (Linux x86_64/aarch64, static musl binary):

```bash
curl -fsSL https://github.com/aaberg/playpen/releases/latest/download/install.sh | sh
```

Flavor images are pulled from `ghcr.io/aaberg` on first use - no local
builds needed. Building from source instead (or working on the launcher):

```bash
# 1. Build everything (base first, then flavors)
make all              # or individually: make base, make rust, make jvm

# 2. Install the launcher (cargo build --release, copy to ~/.local/bin)
make install
```

Then, once:

```bash
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
playpen playpen-rust    # Rust project (runs playpen-rust:omp)
playpen playpen-jvm     # JVM/Kotlin project
playpen                 # no flavor: plain base
playpen --host-engine playpen-rust   # trusted project: + host engine socket
playpen --no-host-engine playpen-rust # disable configured host engine access
playpen --shell playpen-rust         # bash prompt in the harness image
playpen playpen-jvm -- omp --version # any other command inside
playpen update          # re-pull flavors from ghcr.io + rebuild layers
```

The container command is the harness (omp, started in /workspace), so
quitting it ends the session and drops you back on the host - the container
is removed. `--shell` (or `-- bash`) gives the old interactive-shell
behavior.

Inside the container, the agent runs in yolo mode: its $HOME is
`~/.local/share/playpen-home` on your disk - expendable by design. The sandbox
boundary is the container's mounts - by default just the workspace and
the agent home.

## Configuration

Project settings live in `<project-root>/.playpen/playpen.toml`. Global settings
live in `$XDG_CONFIG_HOME/playpen/playpen.toml`, falling back to
`$HOME/.config/playpen/playpen.toml` when `XDG_CONFIG_HOME` is unset, empty,
or relative. The same fallback applies on Linux, WSL, and macOS; it does not
depend on the variable being set by the operating system.

The first supported setting is a TOML boolean:

```toml
host_engine = true
```

Edit configuration without opening the file:

```bash
playpen config set host_engine true          # enable for this project
playpen config set host_engine false         # explicitly disable for this project
playpen config rm host_engine                # remove the project override
playpen config --global set host_engine true # enable globally
playpen config --global rm host_engine       # remove the global override
```

Each command reports the selected file and effective value/source. Commands
work without a container engine. `set` creates missing directories/files;
`rm` is idempotent and does not create missing files. Edits are atomic and
preserve comments and existing file permissions.

Inspect effective configuration:

```bash
playpen config list          # project > global > built-in defaults
playpen config --global list # global > built-in defaults; ignore the project
```

The output is merged TOML: each setting appears once, with its winning source
in a comment. For example:

```toml
# source: "/home/alice/myproject/.playpen/playpen.toml"
host_engine = false
```

Defaults are included even when no configuration files exist. Listing is
read-only and creates no files or directories. Global-only listing does not
discover or validate project configuration, so an invalid project file does
not block it. Both commands validate the configuration layers they do load.
Output describes persistent settings and defaults, not launch overrides:
`--host-engine` and `--no-host-engine` cannot be combined with `config list`.

Precedence, highest first:

1. Explicit `--host-engine` or `--no-host-engine` launch flag (mutually exclusive).
2. Project `host_engine`, including an explicit `false`.
3. Global `host_engine`.
4. Built-in default: `false`.

Removing a key means **inherit**, not disable: removing a project `false` can
re-enable access if the global value is `true`. Use `set host_engine false`
or `--no-host-engine` to disable it explicitly.

Discovery starts at the working directory and walks upward to the nearest
Git root (`.git` directory or worktree file) or home-directory boundary,
including that boundary. Outside the home directory and Git, discovery can
reach the filesystem root. Only the nearest existing `.playpen/playpen.toml`
is loaded; ancestor project files are not merged. If none exists, project
edits target the Git root, or the working directory outside Git. Discovery
does not change the working directory mounted at `/workspace`.

Missing files mean no overrides. Invalid TOML, unknown keys, and non-boolean
`host_engine` values fail with a file diagnostic, even if a higher-priority
setting would override them. CLI values must be exactly `true` or `false`.
Configuration commands validate both selected layers before writing.

Project configuration is trusted automatically: a cloned or modified file can
enable host-engine access without approval. The launch warning is informational,
not an approval prompt. Review project configuration before launching untrusted
code; a gitignored file can still be modified by code inside the mounted project.

`PLAYPEN_HOST_ENGINE` is no longer read. Replace shell/direnv exports with the
project or global commands above. Other environment settings and the existing
keys/harness configuration files are unchanged.

## Harnesses

The agent itself is a thin image layer the launcher builds and maintains:

- `playpen [image]` resolves the harness (omp for now) and runs
  `<image>:<harness>` - `playpen playpen-jvm` runs `playpen-jvm:omp`.
- A missing harness image is built on the fly (~1 min: the flavor layers
  are cached, only the harness install runs).
- A flavor image missing locally is pulled from `ghcr.io/aaberg` and tagged
  with its local name - then the same rules apply. Local images keep
  working offline: a pull only happens when the flavor is absent.
- A flavor image newer than its harness layer (after `make base` /
  `make rust` / `make jvm`) triggers a rebuild of the layer - so the agent
  environment always matches the toolchain.
- `playpen harness` picks the default (stored as `HARNESS=` in
  `~/.config/playpen/harness.conf`); `--harness NAME` overrides per run.
- `playpen harness update` rebuilds every installed `playpen*:<harness>`
  image cache-busted: a fresh harness version on an untouched toolchain.
- `playpen update` does both halves for every installed layer: re-pull the
  flavor from the registry, then the cache-busted rebuild.
- Escape hatch: a tagged or foreign image runs verbatim without layering -
  `playpen playpen-jvm:latest -- bash` is the raw flavor.

## Notes

- Flavors are published to `ghcr.io/aaberg` when a GitHub Release is published,
  not on tag pushes or pushes to `main`. Verified images advance `:latest`
  only when GitHub currently designates that release as Latest; prereleases
  never advance it. Anonymous pulls require public packages. `PLAYPEN_REGISTRY`
  overrides the registry (a mirror, or images you pushed under another owner).
- The base image is `fedora-minimal` (`ARG FEDORA_VERSION` in
  `base/Containerfile`); bump it and `make base` to move to a newer release.
  It carries no harness anymore - the launcher layers omp (and later
  alternatives) on top of the flavors on demand.
- Rust: `CARGO_HOME=/opt/agent-cargo`, mounted as the named volume
  `playpen-cargo` (only for `playpen-rust`), so the registry cache and
  `cargo install`-ed tools persist across sessions. sccache is wired in via `RUSTC_WRAPPER`; its cache lives
  under the persistent agent home, so clean rebuilds stay fast.
- JVM: JDK 25 (LTS) via `java-25-openjdk-devel`. Gradle projects rely on
  their wrapper; Maven covers wrapper-less builds. Standalone `kotlinc` is
  for scripts and quick experiments. The image also includes JetBrains
  [`kotlin-lsp`](https://github.com/Kotlin/kotlin-lsp) 263.7084.0 and Eclipse
  [`jdtls`](https://github.com/eclipse-jdtls/eclipse.jdt.ls) 1.63.0 on `PATH`.
  Both downloads are pinned and SHA-256 checked, with Linux amd64/arm64
  selection. Kotlin LSP is alpha and includes its own JetBrains Runtime;
  JDT LS uses the image's JDK, which must remain compatible if changing
  `JDK_VERSION` in `flavors/jvm/Containerfile`. Updating Kotlin LSP requires
  updating both architecture checksums; updating JDT LS requires its version,
  build timestamp, and checksum.
  The launchers set JVM `user.home` from `$HOME` for numeric host UIDs.
  JDT LS writes configuration and workspace data under
  `${XDG_CACHE_HOME:-$HOME/.cache}/jdtls/<workspace-path-sha256>/`, not `/opt`.
  omp auto-detects both commands in projects with recognized Maven/Gradle
  root markers; no extra LSP configuration is needed for those projects.
- User mapping: podman `--userns=keep-id` plus explicit `--user $UID:$GID`;
  docker `--user $UID:$GID`. Files the agent creates are owned by you on the
  host, even when your UID differs from the image's agent UID. Images set
  `HOME=/home/agent` so numeric users without a passwd entry use the mounted home.
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
- Host engine access (Podman or Docker, for Testcontainers etc.) is disabled by
  default. Enable it with `playpen --host-engine <flavor>` or `host_engine = true`
  in [configuration](#configuration). It mounts the host engine socket and sets `DOCKER_HOST`
  (plus `CONTAINER_HOST` for Podman). Both engines also receive
  `--security-opt label=disable` so SELinux permits access to the host socket;
  normal launches retain SELinux label confinement and get no engine socket.
  Images include both the real Docker CLI and Podman: use `docker ps` /
  `docker run` with either host engine, or `podman ps` / `podman run` with a
  Podman host. Podman's CLI cannot control Docker. Docker launches add the
  socket's numeric GID as a supplementary group for the non-root container user;
  other host supplementary groups are not forwarded.
  The launcher prefers Podman when both engines are installed; set
  `PLAYPEN_ENGINE=docker` to select Docker explicitly. Socket discovery uses
  `/run/user/$UID/podman/podman.sock` for Podman, or
  `~/.docker/run/docker.sock` then `/var/run/docker.sock` for Docker.
  Docker contexts, custom `DOCKER_HOST` endpoints, and other rootless Docker
  socket locations are not discovered.
  Sibling bind-mount source paths refer to the engine host, not the agent's
  `/workspace`; published ports belong to that host as well.
  With host-engine access, code inside can create sibling containers that
  mount files accessible to the engine service user and inspect every container
  on that engine. Rootless Podman grants your host user's access; rootful Docker
  effectively grants host root access. Treat this as a trusted-project switch.
- The agent's git identity: set it once inside the persistent home
  (`git config --global user.name/user.email`) or mount a minimal gitconfig.

## CI and releases

`.github/workflows/ci.yml` runs on pull requests and pushes to `main`. It
checks workflow syntax/shell steps, Rust formatting and Clippy, and the
launcher's unit and stub-engine integration tests on native amd64/arm64
runners. A Docker/Podman matrix builds all images and exercises the actual
launcher extracted from each musl release tarball.

On Ubuntu 26.04, CI and release Podman setup also allow Podman's `SIGTERM` to
the `pasta` networking helper in AppArmor and reload its profile before starting
the API socket. This narrow workaround addresses rootless network cleanup
failures tracked in [runner-images#15182](https://github.com/actions/runner-images/issues/15182).

The credential-free smoke runner compiles and runs Rust, Java, and Kotlin
examples, checks workspace/home mounts and file ownership, verifies dummy-key
handling and exit status propagation, and builds/reuses a real omp harness
layer. It also verifies `docker ps` and a sibling-container workload through
the forwarded socket on both engines, plus Podman's native remote CLI on Podman.
It uses a pseudo-terminal because the launcher is interactive. Its temporary
configuration, image aliases, sibling containers, and cargo volume are removed
afterward; it does not use your API keys or call an AI provider.

Run the same checks locally with a real engine:

```bash
make test
make all ENGINE=podman
cargo build --release --locked --manifest-path launcher/Cargo.toml
systemctl --user start podman.socket
python3 scripts/smoke.py --engine podman --launcher launcher/target/release/playpen
# For Docker, build images with ENGINE=docker and use --engine docker.
```

To release, update `launcher/Cargo.toml` and `launcher/Cargo.lock` and commit
the version change. In **GitHub → Releases → Draft a new release**, choose or
create a `v<version>` tag matching the launcher package version, for example
`v0.1.0` or `v0.2.0-rc.1`. Write the title and notes, choose the prerelease and
Latest settings, then click **Publish release**. Saving a draft or pushing a
tag alone does not start `.github/workflows/release.yml`.

The workflow:

1. Runs launcher CI and checks that the release tag matches the launcher version.
2. Builds native musl packages and images, then smokes the extracted binaries.
3. Pushes run-specific architecture references, assembles versioned OCI indexes
   from their digests (including provenance), and checks both Linux architectures.
4. Pulls each versioned image without registry credentials on both architectures
   with Docker and Podman, then runs the packaged-launcher smoke again.
5. Attaches the binary archives, checksums, and `install.sh` to your existing
   release. It then advances container `:latest` only if the release is not a
   prerelease and GitHub still designates it as Latest, verifying the public aliases.

The workflow never creates a release or changes its title, notes, publication
status, or prerelease/Latest settings. Releases are serialized and active
publication is not cancelled by a newly published release. GitHub's Latest
selection, rather than semantic-version ordering, controls the stable image channel.

The release is public before its assets and images are ready. While the workflow
runs, or if it fails, the release can be incomplete; the installer's
`releases/latest/download` links can temporarily point to missing assets. Retry
failed runs from Actions. Reruns replace generated assets and versioned image tags,
but preserve your release text and settings.

Image build inputs still include moving Fedora packages, Bun, and Rust stable:
retain published digests for exact rollback. The launcher still consumes the
stable `:latest` channel, not a version pinned to its binary. Explicit versioned
image arguments run raw images without automatic harness layering.

### GHCR package setup and troubleshooting

For `playpen-base`, `playpen-rust`, and `playpen-jvm`, ensure:

- **Package settings → Manage Actions access** grants `aaberg/playpen` Write
  access, or the package inherits the repository's permissions.
- **Package visibility** is Public, so first-use launcher pulls work without
  authentication. Newly created packages default to private; a first release
  may stop at the anonymous-consumer gate until visibility is changed.

Images carry the repository source label, and release builds add version and
revision labels. The workflow uses `GITHUB_TOKEN` with package-write permission
only in publication jobs; CI never publishes containers.

The previous GHCR `denied` error occurred after layer uploads, while publishing
image metadata. Successful login does not prove package write access, and a
public repository link does not prove permission inheritance. Named per-image
push steps now provide detailed diagnostics. Check the package settings before
changing credentials; a failed workflow leaves the manually published release incomplete.

## Roadmap

The plan - Rust launcher, harness layering, published images, installer,
macOS support - lives in [ROADMAP.md](ROADMAP.md).
