//! Harness-layer orchestration: how the image positional maps to the image
//! that runs, the build rules from the roadmap (build when missing, rebuild
//! when the flavor image is newer), and `harness update`. All engine
//! interaction goes through `Command` - no client libraries - and engine
//! differences stay limited to flags both understand.

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
    /// The engine has no such image.
    Missing,
    /// The engine reported a timestamp we cannot parse.
    Unparseable(String),
}

/// The build rules: build a missing harness image; rebuild one whose flavor
/// image was created later, so `make base` / `make jvm` rebuilds keep the
/// layers in sync. Anything surprising fails loudly - a broken state must
/// neither rebuild silently on every launch nor skip a needed rebuild.
pub fn ensure(engine: Engine, flavor: &str, target: &str, harness: &Harness) {
    let flavor_ref = format!("{flavor}:latest");
    match created(engine, target) {
        Ok(harness_created) => {
            match created(engine, &flavor_ref) {
                Ok(flavor_created) if flavor_created > harness_created => {
                    eprintln!("playpen: rebuilding harness image {target} (flavor image is newer)");
                    build(engine, flavor, target, harness, false);
                }
                Ok(_) => {}
                // The layer exists, so it runs; with the flavor gone there is
                // simply nothing to compare for staleness.
                Err(CreatedError::Missing) => eprintln!(
                    "playpen: {flavor_ref} not found - skipping staleness check"
                ),
                Err(CreatedError::Unparseable(raw)) => unparseable(&flavor_ref, &raw),
            }
        }
        Err(CreatedError::Missing) => {
            if matches!(created(engine, &flavor_ref), Err(CreatedError::Missing)) {
                missing_flavor(&flavor_ref);
            }
            eprintln!("playpen: building harness image {target}");
            build(engine, flavor, target, harness, false);
        }
        Err(CreatedError::Unparseable(raw)) => unparseable(target, &raw),
    }
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

/// `engine image inspect --format '{{.Created}}'` normalized to second
/// granularity. Only ever compared against timestamps from the same engine,
/// so the differing podman/docker layouts cannot skew the comparison.
fn created(engine: Engine, image_ref: &str) -> Result<String, CreatedError> {
    let output = Command::new(engine.binary())
        .args(["image", "inspect", "--format", "{{.Created}}", image_ref])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            normalize_created(&raw).ok_or(CreatedError::Unparseable(raw))
        }
        // The engine reports a missing image via a nonzero status; any other
        // inspect failure surfaces when the build it triggers runs.
        Ok(_) => Err(CreatedError::Missing),
        Err(err) => run_error(engine, err),
    }
}

/// Normalize an engine's `{{.Created}}` value to `YYYY-MM-DDTHH:MM:SS`:
/// podman prints Go's time.String()
/// (`2025-10-29 20:07:01.23456789 +0100 CET`), docker prints RFC 3339
/// (`2025-10-29T19:07:01.234567890Z`). Timezone and sub-second precision
/// are dropped: staleness compares two timestamps from the same engine, and
/// a layer build takes far longer than a second.
pub fn normalize_created(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let bytes = raw.as_bytes();
    let separator = |index: usize| matches!(index, 4 | 7 | 10 | 13 | 16);
    if bytes.len() < 19
        || !bytes[..19]
            .iter()
            .enumerate()
            .all(|(i, b)| separator(i) || b.is_ascii_digit())
        || (bytes[10] != b'T' && bytes[10] != b' ')
    {
        return None;
    }
    let mut normalized = String::with_capacity(19);
    normalized.push_str(&raw[..10]);
    normalized.push('T');
    normalized.push_str(&raw[11..19]);
    Some(normalized)
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
    fn created_timestamps_parse_in_both_engine_layouts() {
        // docker: RFC 3339
        assert_eq!(
            normalize_created("2025-10-29T19:07:01.234567890Z"),
            Some("2025-10-29T19:07:01".to_string())
        );
        // podman: Go time.String()
        assert_eq!(
            normalize_created("2025-10-29 20:07:01.23456789 +0100 CET"),
            Some("2025-10-29T20:07:01".to_string())
        );
        // second precision compares correctly across the date boundary
        assert!(normalize_created("2025-11-02 00:00:00 +0100 CET").unwrap()
            > normalize_created("2025-10-29T23:59:59Z").unwrap());

        assert_eq!(normalize_created(""), None);
        assert_eq!(normalize_created("2025-10-29"), None);
        assert_eq!(normalize_created("yesterday"), None);
        // shape, not calendar validity: digits and separators are all we
        // need to compare two timestamps of the same format
        assert!(normalize_created("2025-13-45T99:99:99Z").is_some());
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
