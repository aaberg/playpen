//! Harness installation recipes and launch commands. Persistent selection is
//! stored in project/global TOML; additional installers belong to milestone 3.

use crate::config::HarnessChoice;
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

/// The currently supported harnesses. Additional harness installation recipes
/// are planned in ROADMAP.md.
pub const REGISTRY: &[Harness] = &[Harness {
    name: "omp",
    npm_package: "@oh-my-pi/pi-coding-agent",
    exec: "omp",
}];

pub fn lookup(name: &str) -> Option<&'static Harness> {
    REGISTRY.iter().find(|h| h.name == name)
}

/// The currently installable harness, used for explicit image launches without
/// a saved selection.
pub fn default_harness() -> &'static Harness {
    &REGISTRY[0]
}

/// Validate a one-off or saved selection before doing any engine work. Future
/// harness choices are accepted by configuration, not silently replaced by omp.
pub fn resolve(name: Option<&str>) -> &'static Harness {
    let Some(name) = name else {
        return default_harness();
    };
    let choice = name.parse::<HarnessChoice>().unwrap_or_else(|err| {
        signals::fail(format!("playpen: {err}"));
    });
    lookup(choice.as_str()).unwrap_or_else(|| {
        signals::fail(format!(
            "playpen: harness {name:?} is configured, but its provisioning is not implemented \
             until milestone 3; use --harness omp for an available harness"
        ))
    })
}

/// The harness-layer Containerfile: install the npm package on the flavor
/// image, then return to the agent user. Built in an empty temporary context
/// because the current recipe needs no additional files.
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
