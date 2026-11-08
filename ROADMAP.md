# Roadmap

Playpen is a Rust application that configures, builds, and launches a project's
agent environment. The target is one installed binary, project-local settings,
and locally built images rather than Playpen images published to a registry.

This document separates the current implementation from planned behavior.
[README.md](README.md) documents commands that work today.

## Current implementation

- One Cargo package at the repository root: `Cargo.toml`, `Cargo.lock`, `src/`,
  and `tests/`. Container recipes and JVM helper files live under `images/`;
  installer and real-engine smoke automation live under `scripts/`.
- A Rust launcher shells out to Podman or Docker. Podman is preferred unless
  `PLAYPEN_ENGINE` selects Docker, or only Docker is available.
- The invocation directory is mounted at `/workspace`. The agent runs with the
  host user's numeric UID/GID and a persistent, expendable agent home.
- `--shell`, explicit `-- CMD`, API keys from `keys.conf`, `op://` resolution,
  signal cleanup, and opt-in host-engine socket access are implemented.
- Project/global TOML supports `host_engine` only. Harness selection is still
  stored separately in the global `harness.conf`; only omp is implemented.
- `make` builds base and flavor images locally. The launcher pulls missing
  flavor images from GHCR and builds thin harness layers locally. Tagged or
  foreign images run verbatim rather than receiving a harness layer.
- CI checks Rust and workflows, then smokes packaged musl binaries against real
  Docker and Podman on Linux amd64/arm64. Releases still publish images to GHCR
  alongside binary archives, checksums, and the installer.

Interactive project setup, automatic local flavor builds, the additional
harnesses, and binary-only releases are not implemented yet.

## Target workflow

### First use in a project

Running `playpen` with missing or incomplete project settings starts setup:

1. Choose Rust, JVM, or a custom Containerfile/Dockerfile.
2. For a custom image, ask for the file's location on disk, check that it
   exists, and store its location in project TOML.
3. Choose omp, pi, opencode, or vibe.
4. Choose whether the container needs access to the host engine (`host_engine`).
   Explain the host-access boundary and default to disabled.
5. Atomically save complete settings in `.playpen/playpen.toml`.
6. Build the selected environment locally, then launch the harness in the
   mounted project directory.

Docker or Podman is required to build and run Playpen containers regardless of
`host_engine`. That setting only grants access to the host engine from inside
the container. Local builds can still download upstream base images, packages,
and tools; local does not mean offline or fully reproducible.

### Subsequent use

- Load saved project settings without setup prompts.
- Reuse an existing image and launch directly.
- If the required image is missing, build it automatically from saved settings
  and then launch. Do not ask the setup questions again.
- Refresh existing environments explicitly rather than updating on every run.
- Keep one-off launch overrides separate from persistent project settings.

Save validated settings before building so a failed build can be retried with
`playpen`. Cancelling setup must not persist partial answers. Existing files
containing only `host_engine` must retain that value while setup collects the
missing choices. Incomplete configuration without an interactive terminal must
produce an actionable error rather than hang.

## Implementation boundaries

Keep a single Cargo package. Reuse the existing engine, mount, key, and signal
handling rather than rewriting container execution or adding an engine SDK.
Separate configuration persistence, interactive setup, image provisioning, and
launch orchestration within the application as those responsibilities grow.

- Embed built-in recipes and required helper files in the binary so local
  builds work without a source checkout. Build only the selected flavor and
  harness, sharing cached base layers where appropriate.
- Replace registry provisioning and pull-based updates with local build/update
  behavior. Retire `PLAYPEN_REGISTRY` and the separate `harness.conf` selection
  path when their replacements are complete; do not keep parallel conventions.
- Use project-scoped identities for custom images so unrelated projects cannot
  overwrite each other's environments.
- Resolve custom recipe paths against the project root. Prefer project-relative
  paths when possible. Default the build context to the recipe's directory and
  allow an explicit context for recipes that copy files from another root.
- Define custom-image prerequisites before promising arbitrary Dockerfile
  compatibility. The current harness installer assumes npm and an `agent` user;
  installation dependencies, entrypoints, home, ownership, and working directory
  need a supported contract.
- Preserve secrets outside project TOML and keep secret resolution after image
  provisioning. Preserve cleanup, ownership, exit-status, and isolation behavior.

## Delivery milestones

### 1. Direction and layout — implemented

- Replace the historical Bash-port/GHCR roadmap with the project-first direction.
- Move the Rust package to the repository root and collect image resources in
  `images/`; move the installer source to `scripts/install.sh`.
- Update Makefile, CI/release paths, source comments, ignores, and documentation.
- Preserve current CLI, image names, harness selection, and registry behavior.
  The published installer asset remains named `install.sh`, so its URL is stable.

Acceptance: standard Cargo commands work from the repository root, existing
checks and launcher smoke scenarios still work, and image/build/package commands
refer to the relocated resources. No setup or registry cutover belongs here.

### 2. Configuration and setup — planned

- Add typed project image/harness settings and custom recipe/context paths.
- Extend configuration inspection/editing and introduce the first-run wizard.
- Reuse discovery and atomic writes; detect incomplete files, preserve existing
  values, and define how users deliberately reconfigure a project.
- Consolidate persistent harness selection into TOML.

Acceptance: choices survive a second invocation; partial files enter setup;
invalid paths, invalid choices, cancellation, and noninteractive first use have
clear behavior without partial writes. Build failures retain completed settings.

### 3. Local provisioning and harnesses — planned

- Bundle recipes and helper files; build base, selected flavor/custom image,
  and selected harness locally.
- Implement installation and execution for omp, pi, opencode, and vibe using
  their actual dependencies rather than assuming every harness is an npm package.
- Reuse existing images, rebuild missing images, and provide explicit refreshes.
- Replace registry-dependent provisioning and obsolete harness-selection paths.

Acceptance: an installed binary outside the checkout builds and launches Rust,
JVM, and supported custom environments. All four harnesses start successfully;
repeat runs do not rebuild, and deleting a required image triggers recovery.

### 4. CI and release cutover — planned

- Keep Rust formatting, Clippy, unit/integration coverage, workflow validation,
  Linux amd64/arm64, and real Docker/Podman coverage.
- Change smoke setup to let Playpen provision images rather than running
  `make all` first. Drive the wizard through a pseudo-terminal with isolated
  HOME/configuration and no AI-provider requests.
- Exercise first use, prompt/build-free reuse, missing-image rebuilds, and a
  custom recipe with `COPY`. Retain mount/ownership, key cleanup, exit status,
  and enabled/disabled host-engine checks.
- Test the exact packaged binaries outside the checkout before publication.
- Remove registry login/pushes, manifests, digest promotion, anonymous-pull gates,
  and package-write permissions. Publish binaries, checksums, and installer only.

Acceptance: fresh-project consumer scenarios pass on supported engines and
architectures using the release artifacts; workflows no longer publish or depend
on Playpen images in GHCR. Develop this alongside milestone 3 and do not release
the local-only cutover without its packaged-binary smoke coverage.

### 5. User-facing cutover — planned

- Keep README examples, installer instructions, help, and configuration guidance
  aligned with each shipped change; remove the obsolete registry instructions
  once local provisioning replaces them.
- Document custom-image requirements, update behavior, build network needs, and
  migration from the old harness selection/launch workflow.
- Retire existing GitHub packages separately from stopping new publication,
  accounting for older releases that still pull them.

Acceptance: documented installation and daily use follow the new workflow, no
obsolete registry/configuration path remains in the shipped application, and
package retirement has an explicit old-release policy.

## Decisions to finalize during implementation

- Exact installation/version policy and executable invocation for each new
  harness, and the supported custom-image prerequisite contract.
- CLI names for explicit setup/build/update operations and the scope of refreshes.
- Whether launches from a subdirectory mount the resolved project root
  (recommended) instead of the invocation directory (current behavior).
- Image identity and dependency tracking: prefer engine image IDs/labels over
  comparing parsed creation timestamps; avoid normal-run refresh work.
- Whether project TOML is shared in version control or kept local, and how
  host-engine access from existing or modified project settings is trusted.

## Platform scope and non-goals

Linux amd64 and arm64 remain supported, including Linux use inside WSL2.
macOS support is deferred beyond this refactor and needs real engine-in-VM
validation. Native Windows, a GUI, container-engine client libraries, and
publishing Playpen images are not part of the target direction.
