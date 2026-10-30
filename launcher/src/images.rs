//! Harness-layer orchestration: how the image positional maps to the image
//! that runs, the build rules from the roadmap (build when missing, rebuild
//! when the flavor image is newer), pulling flavor images from the registry
//! when they are missing locally (phase 3), and `harness update` /
//! `playpen update`. All engine interaction goes through `Command` - no
//! client libraries - and engine differences stay limited to flags both
//! understand.

use std::ffi::OsString;
use std::process::Command;

use crate::engine::Engine;
use crate::harness::{self, Harness};
use crate::host::Host;
use crate::signals;

/// How the image positional maps to the image that actually runs.
pub enum Plan {
    /// A bare `playpen-*` name: flavor repo plus a harness layer,
    /// `playpen-jvm` -> `playpen-jvm:omp`.
    Layered { flavor: String, target: String },
    /// A tagged or foreign image runs verbatim - the escape hatch to run a
    /// flavor (or anything else) without its harness layer.
    Verbatim,
}

/// Bare `playpen-*` names get harness layering; anything with a tag, a
/// registry path, or another name runs as given.
pub fn plan(image: &str, harness: &str) -> Plan {
    if !image.contains(':') && !image.contains('/') && image.starts_with("playpen-") {
        Plan::Layered {
            flavor: image.to_string(),
            target: format!("{image}:{harness}"),
        }
    } else {
        Plan::Verbatim
    }
}

/// Why an image's Created timestamp could not be determined.
enum CreatedError {
    /// The engine reports the image as absent.
    Missing,
    /// The engine reported a timestamp we cannot parse.
    Unparseable(String),
    /// The inspection failed for a reason other than a missing image; the
    /// engine's stderr tells why.
    Failed(String),
}

/// Where flavor images are pulled from when missing locally (roadmap
/// phase 3). The local image keeps its plain name (`playpen-jvm`), so
/// layer names, staleness and updates are unaffected by the registry.
pub const DEFAULT_REGISTRY: &str = "ghcr.io/aaberg";

/// The registry to pull flavors from: `PLAYPEN_REGISTRY` when set, else
/// the default. A trailing slash is tolerated.
pub fn registry(host: &Host) -> &str {
    // unwrap_or on a trimmed slice keeps the default branch allocation-free
    match host.registry_pref.as_deref() {
        Some(registry) => registry.trim_end_matches('/'),
        None => DEFAULT_REGISTRY,
    }
}

/// The registry reference a flavor is pulled from. podman lists local
/// images under `localhost/`; the registry ref must not carry it.
fn registry_ref(flavor: &str, registry: &str) -> String {
    let remote = flavor.strip_prefix("localhost/").unwrap_or(flavor);
    format!("{registry}/{remote}:latest")
}

/// The build rules: build a missing harness image; rebuild one whose flavor
/// image was created later, so `make base` / `make jvm` rebuilds keep the
/// layers in sync. A flavor missing locally is pulled from the registry
/// first; offline, an existing layer still runs (without the staleness
/// check). Anything surprising fails loudly - a broken state must neither
/// rebuild silently on every launch nor skip a needed rebuild.
pub fn ensure(engine: Engine, flavor: &str, target: &str, harness: &Harness, registry: &str) {
    let flavor_ref = format!("{flavor}:latest");
    match created(engine, target) {
        Ok(harness_created) => match provision(engine, flavor, registry) {
            // The flavor is present (or freshly pulled): staleness rules.
            Ok(()) => match created(engine, &flavor_ref) {
                Ok(flavor_created) if flavor_created > harness_created => {
                    eprintln!("playpen: rebuilding harness image {target} (flavor image is newer)");
                    build(engine, flavor, target, harness, false);
                }
                Ok(_) => {}
                Err(CreatedError::Missing) => eprintln!(
                    "playpen: {flavor_ref} not found - skipping staleness check"
                ),
                Err(CreatedError::Unparseable(raw)) => unparseable(&flavor_ref, &raw),
                Err(CreatedError::Failed(stderr)) => inspect_failed(&flavor_ref, &stderr),
            },
            // The layer exists, so the run proceeds without the check.
            Err(reason) => eprintln!("playpen: skipping staleness check ({reason})"),
        },
        Err(CreatedError::Missing) => {
            if let Err(reason) = provision(engine, flavor, registry) {
                eprintln!("playpen: {reason}");
                missing_flavor(&flavor_ref);
            }
            eprintln!("playpen: building harness image {target}");
            build(engine, flavor, target, harness, false);
        }
        Err(CreatedError::Unparseable(raw)) => unparseable(target, &raw),
        Err(CreatedError::Failed(stderr)) => inspect_failed(target, &stderr),
    }
}

/// Make sure the flavor image exists locally: pull it from the registry
/// when missing, then tag it with its local name so every downstream rule
/// (layer names, staleness, updates) sees the plain local name. A failed
/// pull is `Err(reason)`; the engine's own error output has already
/// streamed to the terminal.
fn provision(engine: Engine, flavor: &str, registry: &str) -> Result<(), String> {
    let flavor_ref = format!("{flavor}:latest");
    match created(engine, &flavor_ref) {
        Ok(_) => return Ok(()),
        Err(CreatedError::Missing) => {}
        Err(CreatedError::Unparseable(raw)) => unparseable(&flavor_ref, &raw),
        Err(CreatedError::Failed(stderr)) => inspect_failed(&flavor_ref, &stderr),
    }
    let remote_ref = registry_ref(flavor, registry);
    eprintln!("playpen: {flavor_ref} not found - pulling {remote_ref}");
    fetch(engine, flavor, registry)
}

/// Pull the registry's flavor image and tag it with its local name. The
/// pull output streams like a build's; `Err` names the failed step.
fn fetch(engine: Engine, flavor: &str, registry: &str) -> Result<(), String> {
    let remote_ref = registry_ref(flavor, registry);
    let local_ref = format!("{flavor}:latest");
    let pull = Command::new(engine.binary())
        .args(["pull", &remote_ref])
        .status();
    match pull {
        Ok(status) if status.success() => {}
        Ok(_) => return Err(format!("pull of {remote_ref} failed")),
        Err(err) => run_error(engine, err),
    }
    let tag = Command::new(engine.binary())
        .args(["tag", &remote_ref, &local_ref])
        .status();
    match tag {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(format!("tagging {remote_ref} as {local_ref} failed")),
        Err(err) => run_error(engine, err),
    }
}

/// `playpen update`: re-pull the flavor image behind every existing
/// `playpen*:<harness>` layer, then rebuild the layers cache-busted -
/// `harness update` with fresh toolchains underneath. A failed pull
/// aborts the update (the engine's error has already streamed).
pub fn refresh(host: &Host, engine: Engine, name: Option<&str>) -> ! {
    let harness = harness::resolve(host, name);
    let registry = registry(host);
    let matching: Vec<String> = list(engine)
        .into_iter()
        .filter(|image_ref| matches_playpen_harness(image_ref, harness.name))
        .collect();
    if matching.is_empty() {
        eprintln!(
            "playpen: no playpen*:{} images to update",
            harness.name
        );
        std::process::exit(0);
    }
    for image_ref in matching {
        let (repo, _tag) = image_ref.rsplit_once(':').expect("listing prints repo:tag");
        eprintln!("playpen: updating {image_ref}");
        if let Err(reason) = fetch(engine, repo, registry) {
            signals::fail(format!("playpen: {reason}"));
        }
        build(engine, repo, &image_ref, harness, true);
    }
    std::process::exit(0);
}

/// `harness update [NAME]`: rebuild every existing `playpen*:<name>` image
/// cache-busted - a fresh harness version on an untouched toolchain.
pub fn update(host: &Host, engine: Engine, name: Option<&str>) -> ! {
    let harness = harness::resolve(host, name);
    let matching: Vec<String> = list(engine)
        .into_iter()
        .filter(|image_ref| matches_playpen_harness(image_ref, harness.name))
        .collect();
    if matching.is_empty() {
        eprintln!(
            "playpen: no playpen*:{} images to update",
            harness.name
        );
        std::process::exit(0);
    }
    for image_ref in matching {
        let (repo, _tag) = image_ref.rsplit_once(':').expect("listing prints repo:tag");
        eprintln!("playpen: updating {image_ref}");
        build(engine, repo, &image_ref, harness, true);
    }
    std::process::exit(0)
}

/// `playpen*:<name>` per the roadmap. podman lists local images under
/// `localhost/`; docker prints local repositories without a prefix.
fn matches_playpen_harness(image_ref: &str, name: &str) -> bool {
    image_ref
        .rsplit_once(':')
        .is_some_and(|(repo, tag)| {
            tag == name && repo.strip_prefix("localhost/").unwrap_or(repo).starts_with("playpen")
        })
}

/// `engine images --format '{{.Repository}}:{{.Tag}}'`.
fn list(engine: Engine) -> Vec<String> {
    let output = Command::new(engine.binary())
        .args(["images", "--format", "{{.Repository}}:{{.Tag}}"])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect(),
        Ok(output) => signals::fail(format!(
            "playpen: could not list images: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(err) => run_error(engine, err),
    }
}

/// `engine image inspect --format '{{.Created}}'` parsed to a comparable
/// instant. The engine's local-time layout means a DST transition between
/// two builds changes the offset, so comparisons must be on instants, not
/// on the printed fields.
fn created(engine: Engine, image_ref: &str) -> Result<Created, CreatedError> {
    let output = Command::new(engine.binary())
        .args(["image", "inspect", "--format", "{{.Created}}", image_ref])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            parse_created(&raw).ok_or(CreatedError::Unparseable(raw))
        }
        // Exit codes do not tell missing from broken (podman uses 125 for
        // both); the stderr wording does.
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if reports_missing_image(&stderr) {
                Err(CreatedError::Missing)
            } else {
                Err(CreatedError::Failed(stderr))
            }
        }
        Err(err) => run_error(engine, err),
    }
}

/// An image's Created instant at the full precision engines report, with
/// the timezone offset applied: the same instant compares equal in either
/// engine's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Created {
    /// Seconds since the Unix epoch, offset applied.
    epoch: i64,
    /// Sub-second nanoseconds.
    nanos: u32,
}

/// Parse an engine's `{{.Created}}` value: podman prints Go's time.String()
/// (`2025-10-29 20:07:01.23456789 +0100 CET`), docker prints RFC 3339
/// (`2025-10-29T19:07:01.234567890Z`, trailing zeros trimmed). Fractional
/// seconds and the offset are kept, not rounded away - staleness must not
/// skip a rebuild over a tie or reverse over a DST flip.
fn parse_created(raw: &str) -> Option<Created> {
    let raw = raw.trim();
    let b = raw.as_bytes();
    let digits = |range: std::ops::Range<usize>| b.get(range).is_some_and(|s| s.iter().all(u8::is_ascii_digit));
    let number = |from: usize, to: usize| raw[from..to].parse::<u32>().ok();
    if b.len() < 19
        || !digits(0..4) || b[4] != b'-'
        || !digits(5..7) || b[7] != b'-'
        || !digits(8..10)
        || (b[10] != b'T' && b[10] != b' ')
        || !digits(11..13) || b[13] != b':'
        || !digits(14..16) || b[16] != b':'
        || !digits(17..19)
    {
        return None;
    }
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    // Fraction: optional, up to 9 digits (podman omits a zero fraction,
    // RFC 3339 trims trailing zeros - pad to nanoseconds).
    let mut i = 19;
    let mut nanos = 0u32;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        let end = raw[start..]
            .bytes()
            .position(|byte| !byte.is_ascii_digit())
            .map(|offset| start + offset)
            .unwrap_or(raw.len());
        if end == start || end - start > 9 {
            return None;
        }
        let mut fraction = raw[start..end].to_string();
        while fraction.len() < 9 {
            fraction.push('0');
        }
        nanos = fraction.parse().ok()?;
        i = end;
    }

    // Offset: Z, or +HHMM (podman) or +HH:MM (RFC 3339); podman puts a space
    // before the offset and a zone name after it - both ignored.
    while b.get(i) == Some(&b' ') {
        i += 1;
    }
    let offset_seconds = match b.get(i) {
        Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let negative = *sign == b'-';
            i += 1;
            if !digits(i..i + 2) {
                return None;
            }
            let (offset_hour, offset_minute) = if digits(i + 2..i + 4) {
                (number(i, i + 2)?, number(i + 2, i + 4)?)
            } else if b.get(i + 2) == Some(&b':') && digits(i + 3..i + 5) {
                (number(i, i + 2)?, number(i + 3, i + 5)?)
            } else {
                return None;
            };
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let seconds = (offset_hour * 3600 + offset_minute * 60) as i64;
            if negative {
                -seconds
            } else {
                seconds
            }
        }
        // Both engines always print an offset; a timestamp without one is
        // a format change to fail closed on.
        _ => return None,
    };

    let epoch = days_from_civil(year, month, day) * 86_400
        + (hour * 3600 + minute * 60 + second) as i64
        - offset_seconds;
    Some(Created { epoch, nanos })
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
/// Months count from March, so the leap day lands at the end of the
/// counting year instead of inside it.
fn days_from_civil(y: u32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let year_of_era = y - era * 400; // [0, 399]
    let month_from_march = if m > 2 { i64::from(m) - 3 } else { i64::from(m) + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Whether inspect stderr says the image is absent rather than reporting
/// some other failure (podman: "image not known"; docker: "No such image"
/// / "No such object").
fn reports_missing_image(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    ["image not known", "no such image", "no such object", "not found", "no such reference"]
        .iter()
        .any(|needle| stderr.contains(needle))
}

/// Build a harness layer: the recipe is written into an empty temp dir that
/// serves as the build context, streamed like a normal engine build. A
/// failed build aborts the launch with the engine's own exit code.
fn build(engine: Engine, flavor: &str, target: &str, harness: &Harness, no_cache: bool) {
    let recipe = harness::containerfile(flavor, harness);
    let context = tempfile::tempdir()
        .unwrap_or_else(|err| signals::fail(format!("playpen: could not create build context: {err}")));
    let containerfile = context.path().join("Containerfile");
    if let Err(err) = std::fs::write(&containerfile, &recipe) {
        signals::fail(format!(
            "playpen: could not write {}: {err}",
            containerfile.display()
        ));
    }

    let mut args: Vec<OsString> = vec![OsString::from("build")];
    if no_cache {
        args.push(OsString::from("--no-cache"));
    }
    args.push(OsString::from("-t"));
    args.push(OsString::from(target));
    args.push(OsString::from("-f"));
    args.push(containerfile.into_os_string());
    args.push(context.path().as_os_str().to_os_string());
    let status = Command::new(engine.binary())
        .args(&args)
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("playpen: harness image build failed");
            signals::exit_with_status(&status);
        }
        Err(err) => run_error(engine, err),
    }
}

/// A missing flavor image is the one build failure the engine cannot phrase
/// for the user: the layer's FROM would be dangling. Point at the make rule.
fn missing_flavor(flavor_ref: &str) -> ! {
    let flavor = flavor_ref.strip_suffix(":latest").unwrap_or(flavor_ref);
    let make = flavor.strip_prefix("playpen-").unwrap_or(flavor);
    signals::fail(format!(
        "playpen: {flavor_ref} not found - build the flavor first (make {make}) or point \
         PLAYPEN_ENGINE at the engine that has it"
    ))
}

/// Fail closed on an unparseable timestamp: neither silently skipping
/// rebuilds nor rebuilding on every launch is acceptable, so this is a bug
/// to report, not a state to guess around.
fn unparseable(image: &str, raw: &str) -> ! {
    signals::fail(format!(
        "playpen: could not parse Created timestamp of {image}: {raw:?} - please report this"
    ))
}

/// Report an inspection failure that is not a missing image, with the
/// engine's own explanation.
fn inspect_failed(image: &str, stderr: &str) -> ! {
    let reason = if stderr.is_empty() {
        "(no error output)"
    } else {
        stderr
    };
    signals::fail(format!("playpen: could not inspect {image}: {reason}"))
}

/// Command spawn failed (127 for a missing binary, like the bash launcher).
fn run_error(engine: Engine, err: std::io::Error) -> ! {
    if err.kind() == std::io::ErrorKind::NotFound {
        signals::fail(format!("playpen: {}: command not found", engine.binary()));
    }
    signals::fail(format!(
        "playpen: could not run {}: {err}",
        engine.binary()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            uid: 1000,
            gid: 1000,
            pid: 4711,
            home: std::path::PathBuf::from("/home/tester"),
            config_home: None,
            data_home: None,
            runtime_dir: std::path::PathBuf::from("/run/user/1000"),
            workspace: std::path::PathBuf::from("/work"),
            engine_pref: None,
            registry_pref: None,
            host_engine_env: false,
            path: Default::default(),
        }
    }

    #[test]
    fn registry_defaults_to_ghcr_and_honors_the_override() {
        assert_eq!(registry(&host()), DEFAULT_REGISTRY);
        let mut h = host();
        h.registry_pref = Some("registry.local:5000/acme".to_string());
        assert_eq!(registry(&h), "registry.local:5000/acme");
        // a trailing slash would produce a double slash in refs
        h.registry_pref = Some("registry.local:5000/acme/".to_string());
        assert_eq!(registry(&h), "registry.local:5000/acme");
    }

    #[test]
    fn registry_refs_strip_podmans_localhost_prefix() {
        assert_eq!(
            registry_ref("playpen-jvm", "ghcr.io/aaberg"),
            "ghcr.io/aaberg/playpen-jvm:latest"
        );
        assert_eq!(
            registry_ref("localhost/playpen-base", "ghcr.io/aaberg"),
            "ghcr.io/aaberg/playpen-base:latest"
        );
    }

    #[test]
    fn plan_layers_bare_playpen_images_only() {
        let Plan::Layered { flavor, target } = plan("playpen-base", "omp") else {
            panic!("default image is layered");
        };
        assert_eq!(flavor, "playpen-base");
        assert_eq!(target, "playpen-base:omp");

        match plan("playpen-jvm", "omp") {
            Plan::Layered { flavor, target } => {
                assert_eq!(flavor, "playpen-jvm");
                assert_eq!(target, "playpen-jvm:omp");
            }
            Plan::Verbatim => panic!(),
        }

        // tags, registry paths and foreign names run as given
        assert!(matches!(plan("playpen-jvm:latest", "omp"), Plan::Verbatim));
        assert!(matches!(plan("playpen-jvm:omp", "omp"), Plan::Verbatim));
        assert!(matches!(plan("docker.io/playpen-x:1", "omp"), Plan::Verbatim));
        assert!(matches!(plan("alpine", "omp"), Plan::Verbatim));
    }

    #[test]
    fn created_parses_both_engine_layouts_as_instants() {
        let created = |raw: &str| parse_created(raw).expect("parses");
        // docker RFC 3339 (UTC) and podman Go format (local offset) of the
        // same instant compare equal - including the +HH:MM offset spelling
        assert_eq!(
            created("2025-10-29T19:07:01Z"),
            created("2025-10-29 20:07:01 +0100 CET")
        );
        assert_eq!(
            created("2025-10-29T19:07:01Z"),
            created("2025-10-29T20:07:01+01:00")
        );
        // offset applied, not just copied: 00:30 -05:00 is 05:30Z
        assert_eq!(
            created("2025-10-29T05:30:00Z"),
            created("2025-10-29T00:30:00-05:00")
        );
        // ... and +05:00 shifts it into the previous UTC day
        assert_eq!(
            created("2025-10-28T19:30:00Z"),
            created("2025-10-29T00:30:00+05:00")
        );

        // sub-second precision survives; RFC 3339 trims trailing zeros,
        // podman omits a zero fraction entirely
        assert!(created("2025-10-29T12:00:00.900Z") > created("2025-10-29T12:00:00.100Z"));
        assert_eq!(
            created("2025-10-29T12:00:00.9Z"),
            created("2025-10-29T12:00:00.900000000Z")
        );
        assert_eq!(
            created("2025-10-29T12:00:00Z"),
            created("2025-10-29 12:00:00 +0000 UTC")
        );
        // the review finding: same printed second, different fraction -
        // across layouts: 12:00:00.900 +0100 is 11:00:00.9Z
        assert!(
            parse_created("2025-10-29 12:00:00.900000000 +0100 CEST")
                > parse_created("2025-10-29T11:00:00.100Z")
        );

        // the review finding: offsets can flip the printed order - local
        // 01:30 -0500 (06:30Z) is later than local 01:45 -0400 (05:45Z)
        assert!(
            created("2025-11-02 01:30:00.000000000 -0500 EST")
                > created("2025-11-02 01:45:00.000000000 -0400 EDT")
        );

        // real podman output from this machine
        assert!(parse_created("2026-10-29 19:32:53.017372362 +0000 UTC").is_some());
        // the zero-value time some engines report for untagged layers
        assert_eq!(created("0001-01-01T00:00:00Z").epoch, -62_135_596_800);
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        // pinned with `date -u`: epoch seconds / 86400
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 2, 29), 11_016); // leap day
        assert_eq!(days_from_civil(2026, 10, 29), 20_755);
    }

    #[test]
    fn created_rejects_malformed_values() {
        for raw in [
            "",
            "yesterday",
            "2025-10-29",
            "2025-10-29T19:07:01",          // no offset: format change
            "2025-13-29T19:07:01Z",          // month out of range
            "2025-00-29T19:07:01Z",
            "2025-10-32T19:07:01Z",          // day out of range
            "2025-10-29T24:07:01Z",          // hour out of range
            "2025-10-29T19:60:01Z",
            "2025-10-29T19:07:60Z",
            "2025-10-29T19:07:01.Z",         // bare fraction
            "2025-10-29T19:07:01.1234567890Z", // more than 9 fraction digits
            "2025-10-29T19:07:01+010",       // truncated offset
            "2025-10-29T19:07:01+2400",      // offset hour out of range
            "2025-10-29T19:07:01+00:60",     // offset minute out of range
            "2025/10/29T19:07:01Z",          // wrong separators
        ] {
            assert_eq!(parse_created(raw), None, "{raw:?} must not parse");
        }
    }

    #[test]
    fn missing_image_errors_are_recognized_across_engines() {
        // podman 5.8 (verified on this machine)
        assert!(reports_missing_image(
            "Error: playpen-base:omp: image not known"
        ));
        // docker
        assert!(reports_missing_image(
            "Error response from daemon: No such image: playpen-base:omp"
        ));
        assert!(reports_missing_image("Error: No such object: playpen-base:omp"));
        // anything else is a real failure, not a missing image
        assert!(!reports_missing_image("Error: database is locked"));
        assert!(!reports_missing_image("Cannot connect to the Docker daemon"));
        assert!(!reports_missing_image(""));
    }

    #[test]
    fn update_filter_matches_playpen_harness_variants() {
        assert!(matches_playpen_harness("playpen-jvm:omp", "omp"));
        assert!(matches_playpen_harness("localhost/playpen-base:omp", "omp"));
        assert!(matches_playpen_harness("playpen:omp", "omp"), "playpen* glob");
        // wrong tag, foreign repos, malformed lines
        assert!(!matches_playpen_harness("playpen-jvm:latest", "omp"));
        assert!(!matches_playpen_harness("docker.io/library/node:omp", "omp"));
        assert!(!matches_playpen_harness("myplaypen-jvm:omp", "omp"));
        assert!(!matches_playpen_harness("playpen-jvm", "omp"));
        assert!(!matches_playpen_harness("<none>:<none>", "omp"));
    }
}
