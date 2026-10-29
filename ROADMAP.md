# Roadmap

From a personal bash launcher to a distributable tool: pre-built images on
ghcr.io, the harness as a thin topmost layer, and one static Rust binary
installed with `curl -fsSL ... | sh`. Phases 1-3 build that; phase 4 adds
macOS. No native Windows - WSL2 users run the Linux binary inside WSL.

## Current state

- `bin/playpen` (bash): launches omp by default; `--shell` for a bash;
  `-- CMD` passthrough. podman default, docker supported. Keys from
  `keys.conf`, literals or `op://` resolved via the 1Password CLI.
- Images built locally via `make`: playpen-base (contains omp), playpen-rust,
  playpen-jvm. No registry, no installer.
- The launcher is the spec: the Rust port reproduces its behavior exactly.

## Target architecture

```
fedora-minimal
└── playpen-base:latest          (no harness)
    ├── playpen-rust:latest  ->  playpen-rust:<harness>    (thin layers)
    └── playpen-jvm:latest   ->  playpen-jvm:<harness>
```

- Two image layers: flavor (toolchain; pre-built, pulled from ghcr.io) and
  harness (omp / vibe / opencode; one RUN layer, built locally on demand,
  never published).
- Naming: harness as a tag variant (`playpen-jvm:omp`), so variants group
  under one repo name in `podman image ls`.
- The launcher orchestrates engines by shelling out (`Command`); no client
  libraries. Engine differences stay in one place.

### CLI

| Command | Behavior |
|---|---|
| `playpen [flags] [image]` | resolve harness -> image `:<harness>`; build if missing/stale; run harness |
| `playpen --shell ...` | bash in the same harness image |
| `playpen ... -- CMD` | passthrough into the container |
| `playpen ... --harness NAME` | per-run override, same build rules |
| `playpen harness` | menu -> `HARNESS=` in `~/.config/playpen/harness.conf` |
| `playpen harness update [NAME]` | rebuild existing `playpen*:<name>` images, cache-busted |
| `playpen update` (phase 3) | re-pull flavor images, rebuild harness layers |

### Build rules

- Harness image missing -> build it (FROM layers are cached; cost is the
  harness install, ~1 min).
- Flavor image created later than its harness image -> rebuild the layer.
  Detect via image `Created` timestamps; keeps harness layers in sync after
  `make base` / `make jvm` rebuilds.
- `harness update` -> `--no-cache` rebuild: fresh harness version, toolchain
  untouched (base layers still cached).

## Why Rust

The state matrix is flavor x harness x engine x local/remote. Bash carried
the current script fine but cannot express that as testable data. Rust gives
typed state, unit tests for parsing/naming/staleness, and a static musl
binary with no runtime dependencies - what `curl | sh` wants.

Rough budget: launcher ~700 lines, install.sh ~100, CI ~150 lines YAML.

## Platform support

| Platform | Tier |
|---|---|
| Linux amd64 + arm64 | Tier 1 (v1) |
| macOS, engine-in-VM (Docker Desktop / colima) | Tier 2 (phase 4) |
| Windows native | Out of scope; WSL2 runs the Linux binary |

## Phase 1 - Rust port at parity

Reproduce `bin/playpen` exactly, as a crate in `launcher/`:

- CLI: `--host-engine` (+`PLAYPEN_HOST_ENGINE`), `--keys FILE`, `--shell`,
  image positional (default `playpen-base`), `-- CMD` passthrough, `-h`.
- Engine selection: `PLAYPEN_ENGINE` or autodetect podman -> docker.
- keys.conf: `NAME=value`, `#`-comments, CRLF tolerated, 0600 tightening,
  `op read` for `op://` values, temp 0600 env-file removed on exit.
- Engine specifics: podman `--userns=keep-id` + socket
  `/run/user/$UID/podman/podman.sock` (hint: `systemctl --user start
  podman.socket`); docker `--user $UID:$GID` + socket
  `~/.docker/run/docker.sock` -> `/var/run/docker.sock`;
  `DOCKER_HOST`/`CONTAINER_HOST` when `--host-engine`.
- Mounts: `$PWD:/workspace:Z`, agent home `~/.local/share/playpen-home`,
  `playpen-cargo` volume for `playpen-rust*`, name `playpen-agent-<pid>`.
- Default command omp; `--shell` -> `/bin/bash`; explicit `-- CMD` wins;
  conflicting arguments error out.

Exit criteria: stub-engine test suite green (a fake podman on PATH, asserting
the assembled argv - the same trick used to test the bash version); parity
checklist above complete; `bin/playpen` removed; `make install` = `cargo
build --release` + copy to `~/.local/bin`.

## Phase 2 - Harness selection + layering

- Remove the omp install from `base/Containerfile` (flavors rebuild once).
- Harness registry in the launcher: name -> npm package + exec command.
  omp: `@oh-my-pi/pi-coding-agent` / `omp`. vibe, opencode: TODO exact
  packages. Recipes are heredoc Containerfiles built in an empty temp
  context; a harness that ever needs build-context files moves to
  `harness/<name>/` in the repo.
- `playpen harness` menu -> `~/.config/playpen/harness.conf`
  (`HARNESS=omp`; per-flavor lines like `playpen-jvm=vibe` later).
- `--harness NAME` flag; build rules as above; `harness update`.
- Decision: `--shell` opens the harness image - the environment the agent
  sees.

Exit criteria: pick harness -> build -> run works end to end; `harness
update` bumps the omp version (compare `omp --version` before/after)
without rebuilding the toolchain.

## Phase 3 - CI publishing + installer

- GitHub Actions: build base + flavors multi-arch on native amd64/arm64
  runners, push to ghcr.io as multi-arch manifests; build musl binaries
  (x86_64, aarch64) with sha256 checksums; publish install.sh.
- Launcher: pull `ghcr.io/<owner>/<image>` when missing locally; local
  images keep working offline (a pull only happens when the image is
  absent).
- `playpen update`: re-pull flavors + rebuild harness layers.
- install.sh: detect arch, fetch the release tarball, verify checksum,
  install to `~/.local/bin`, print a PATH hint.

Exit criteria: from a clean machine with no local images and no repo
checkout: `curl -fsSL ... | sh`, then `playpen playpen-jvm` pulls, builds
the harness layer, and runs; `playpen update` refreshes everything.

## Phase 4 - macOS

- Targets: `aarch64-apple-darwin` (+ `x86_64-apple-darwin` for Intel Macs).
- Engine: docker-compatible VMs first (Docker Desktop, colima, Rancher
  Desktop); podman machine later, only if it proves solid.
- Platform branches: drop `--userns=keep-id`; omit `--user` (the VM's file
  sharing maps ownership); `:Z` stays (no-op); keys.conf mode handling
  ports as-is (POSIX modes exist on APFS); `--host-engine` gated off -
  there is no host socket file to mount.
- Docs: engine setup, bind-mount performance caveats, VM memory limits.
- Signing: optional. Unsigned curl-installed binaries run without
  Gatekeeper complaints; Developer ID + notarization only if users ask.

Exit criteria: the phase-3 demo works on a real Mac with Docker Desktop;
caveats documented. Prerequisite: access to a Mac - CI cannot exercise
the VM engines there.

## Non-goals

- Native Windows. Use WSL2: playpen runs inside it as a Linux tool.
- podman-on-Windows.
- `--host-engine` outside Linux.
- Publishing harness images; they build on demand.
- A GUI.

## Open questions

- Exact npm packages / exec names for vibe and opencode in the registry.
- ghcr tag scheme: `latest`, a `stable` pointer, or semver pinned per
  launcher version.
- macOS agent-home convention: keep `~/.local/share/playpen-home` for
  cross-platform consistency, or `~/Library/Application Support`.
- `--shell` target: harness image (current decision) vs raw flavor image.
