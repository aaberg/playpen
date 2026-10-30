//! The harness layer: a registry of launchable agents (name, npm package
//! and exec command), the generated Containerfile recipe that installs one
//! on top of a flavor image, and the `harness.conf` selection file.
//! Roadmap phase 2: harnesses are thin per-flavor image layers built on
//! demand by the launcher, never published.

use std::io::Write;

use crate::host::Host;
use crate::signals;

/// One harness in the registry.
pub struct Harness {
    /// Selection name and image tag variant (`playpen-jvm:omp`).
    pub name: &'static str,
    /// npm package installed into the layer.
    pub npm_package: &'static str,
    /// The command the launcher runs by default.
    pub exec: &'static str,
}

/// The known harnesses. vibe and opencode join once their exact npm packages
/// are pinned (roadmap open question).
pub const REGISTRY: &[Harness] = &[Harness {
    name: "omp",
    npm_package: "@oh-my-pi/pi-coding-agent",
    exec: "omp",
}];

pub fn lookup(name: &str) -> Option<&'static Harness> {
    REGISTRY.iter().find(|h| h.name == name)
}

/// The registry's first entry: what a fresh machine runs.
pub fn default_harness() -> &'static Harness {
    &REGISTRY[0]
}

fn available() -> String {
    REGISTRY
        .iter()
        .map(|h| h.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which harness a run uses: the `--harness` flag, else `HARNESS=` in
/// harness.conf, else the registry default. Unknown names are fatal - a
/// typo'd selection would otherwise run a nonexistent command.
pub fn resolve(host: &Host, flag: Option<&str>) -> &'static Harness {
    if let Some(name) = flag {
        return lookup(name).unwrap_or_else(|| {
            signals::fail(format!(
                "playpen: unknown harness '{name}' (available: {})",
                available()
            ))
        });
    }
    match conf_harness(host) {
        None => default_harness(),
        Some(name) => lookup(&name).unwrap_or_else(|| {
            signals::fail(format!(
                "playpen: unknown harness '{name}' in {} (available: {})",
                host.harness_conf_file().display(),
                available()
            ))
        }),
    }
}

/// `HARNESS=` from harness.conf; absent or empty means unset. Comments and
/// other lines (future per-flavor entries like `playpen-jvm=vibe`) are
/// tolerated and ignored.
pub fn conf_harness(host: &Host) -> Option<String> {
    let file = host.harness_conf_file();
    let content = match std::fs::read_to_string(&file) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            signals::fail(format!("playpen: could not read {}: {err}", file.display()))
        }
    };
    content.lines().find_map(|line| {
        if line.trim_start().starts_with('#') {
            return None;
        }
        let (name, value) = line.split_once('=')?;
        let value = value.trim();
        (name.trim() == "HARNESS" && !value.is_empty()).then(|| value.to_string())
    })
}

/// Write `HARNESS=<name>` to harness.conf, keeping every other line
/// (comments, per-flavor entries) as-is. The config directory is created
/// on demand.
pub fn write_conf(host: &Host, name: &str) -> std::io::Result<()> {
    let file = host.harness_conf_file();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let selection = format!("HARNESS={name}\n");
    let updated = match std::fs::read_to_string(&file) {
        Ok(content) => {
            // Replace the first HARNESS= line in place; every other line
            // (comments, future per-flavor entries) passes through, and a
            // file without a selection gets one appended.
            let mut out = String::new();
            let mut replaced = false;
            for line in content.lines() {
                let is_selection = !line.trim_start().starts_with('#')
                    && line.split_once('=').is_some_and(|(key, _)| key.trim() == "HARNESS");
                if is_selection && !replaced {
                    out.push_str(&selection);
                    replaced = true;
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            if !replaced {
                out.push_str(&selection);
            }
            out
        }
        Err(_) => selection,
    };
    std::fs::write(&file, updated)
}

/// The harness-layer Containerfile: FROM the flavor image, install the npm
/// package exactly the way base/Containerfile used to, back to the agent
/// user. Built in an empty temp context, so the recipe needs no build
/// context files; a harness that ever needs some moves to `harness/<name>/`
/// in the repo (roadmap).
pub fn containerfile(flavor: &str, harness: &Harness) -> String {
    format!(
        concat!(
            "# playpen harness layer: {name} on {flavor}\n",
            "FROM {flavor}:latest\n",
            "USER root\n",
            "RUN npm install -g {pkg} \\\n",
            "    && chown -R agent:agent /home/agent\n",
            "USER agent\n",
            "WORKDIR /workspace\n",
        ),
        name = harness.name,
        flavor = flavor,
        pkg = harness.npm_package,
    )
}

/// `playpen harness`: list the registry, read a selection from stdin,
/// persist it to harness.conf.
pub fn menu(host: &Host) -> ! {
    println!("Available harnesses:");
    for (i, harness) in REGISTRY.iter().enumerate() {
        println!("  {}) {}", i + 1, harness.name);
    }
    let default_index = current_index(host);
    print!("Select harness [{}]: ", default_index + 1);
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => signals::fail(String::from("playpen: no selection - nothing written")),
        Ok(_) => {}
        Err(err) => signals::fail(format!("playpen: could not read selection: {err}")),
    }
    let harness = match parse_choice(line.trim(), default_index) {
        Ok(harness) => harness,
        Err(message) => signals::fail(format!("playpen: {message}")),
    };
    if let Err(err) = write_conf(host, harness.name) {
        signals::fail(format!(
            "playpen: could not write {}: {err}",
            host.harness_conf_file().display()
        ));
    }
    println!(
        "playpen: HARNESS={} saved to {}",
        harness.name,
        host.harness_conf_file().display()
    );
    std::process::exit(0)
}

/// The menu's default entry: the current selection when it is still in the
/// registry, else the first.
fn current_index(host: &Host) -> usize {
    conf_harness(host)
        .as_deref()
        .and_then(lookup)
        .and_then(|h| REGISTRY.iter().position(|r| r.name == h.name))
        .unwrap_or(0)
}

/// A menu answer: a 1-based number or a harness name; empty input means
/// the default. Err carries the message without the "playpen: " prefix.
fn parse_choice(input: &str, default_index: usize) -> Result<&'static Harness, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(&REGISTRY[default_index]);
    }
    if let Ok(number) = input.parse::<usize>() {
        let harness = number
            .checked_sub(1)
            .and_then(|index| REGISTRY.get(index));
        return harness.ok_or_else(|| {
            format!(
                "no harness number {number} (available: {available})",
                available = available()
            )
        });
    }
    lookup(input)
        .ok_or_else(|| format!("unknown harness '{input}' (available: {})", available()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn host_with_config(config: &std::path::Path) -> Host {
        Host {
            uid: 1000,
            gid: 1000,
            pid: 4711,
            home: PathBuf::from("/home/tester"),
            config_home: Some(config.to_path_buf()),
            data_home: None,
            runtime_dir: PathBuf::from("/run/user/1000"),
            workspace: PathBuf::from("/work"),
            engine_pref: None,
            registry_pref: None,
            host_engine_env: false,
            path: Default::default(),
        }
    }

    fn host() -> Host {
        host_with_config(std::path::Path::new("/nonexistent-config"))
    }

    #[test]
    fn recipe_installs_the_package_on_the_flavor() {
        let omp = lookup("omp").unwrap();
        assert_eq!(
            containerfile("playpen-jvm", omp),
            "# playpen harness layer: omp on playpen-jvm\n\
             FROM playpen-jvm:latest\n\
             USER root\n\
             RUN npm install -g @oh-my-pi/pi-coding-agent \\\n\
             \x20   && chown -R agent:agent /home/agent\n\
             USER agent\n\
             WORKDIR /workspace\n"
        );
    }

    #[test]
    fn resolve_flag_conf_and_default() {
        let host = host();
        assert_eq!(resolve(&host, Some("omp")).name, "omp");
        assert_eq!(resolve(&host, None).name, "omp", "no conf: registry default");
    }

    #[test]
    fn conf_harness_parses_and_tolerates() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_with_config(dir.path());
        std::fs::create_dir_all(host.harness_conf_file().parent().unwrap()).unwrap();
        std::fs::write(
            host.harness_conf_file(),
            "# comment\n\nplaypen-jvm=vibe\nHARNESS=omp\n",
        )
        .unwrap();
        assert_eq!(conf_harness(&host).as_deref(), Some("omp"));

        // CRLF, spaces and an empty value
        std::fs::write(host.harness_conf_file(), " HARNESS = omp \r\n").unwrap();
        assert_eq!(conf_harness(&host).as_deref(), Some("omp"));
        std::fs::write(host.harness_conf_file(), "HARNESS=\n").unwrap();
        assert_eq!(conf_harness(&host), None, "empty value means unset");

        // absent file
        std::fs::remove_file(host.harness_conf_file()).unwrap();
        assert_eq!(conf_harness(&host), None);
    }

    #[test]
    fn write_conf_replaces_only_the_selection_line() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_with_config(dir.path());
        write_conf(&host, "omp").unwrap();
        assert_eq!(
            std::fs::read_to_string(host.harness_conf_file()).unwrap(),
            "HARNESS=omp\n"
        );

        // a re-write over existing content keeps unrelated lines
        std::fs::write(
            host.harness_conf_file(),
            "# pick once\nplaypen-jvm=vibe\nHARNESS=omp\n",
        )
        .unwrap();
        write_conf(&host, "omp").unwrap();
        assert_eq!(
            std::fs::read_to_string(host.harness_conf_file()).unwrap(),
            "# pick once\nplaypen-jvm=vibe\nHARNESS=omp\n"
        );
    }

    #[test]
    fn menu_choice_parsing() {
        assert_eq!(parse_choice("", 0).unwrap().name, "omp", "empty = default");
        assert_eq!(parse_choice("1", 0).unwrap().name, "omp");
        assert_eq!(parse_choice("omp", 0).unwrap().name, "omp");
        assert!(parse_choice(" 2 ", 0).is_err(), "out of range");
        assert!(parse_choice("0", 0).is_err(), "0 is not a valid entry");
        assert!(parse_choice("bogus", 0).is_err());
    }
}
