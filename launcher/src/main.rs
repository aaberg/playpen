//! playpen: launch an AI coding agent inside a sandboxed container.
//! Phase 1 ported the bash launcher at exact parity; phase 2 makes the
//! launcher the orchestrator of the harness layers; phase 3 adds pulling
//! flavor images from the registry when missing locally (see ROADMAP.md).

mod argv;
mod cli;
mod engine;
mod harness;
mod host;
mod images;
mod keys;
mod signals;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use crate::cli::{Action, Config};
use crate::harness::Harness;
use crate::host::Host;

fn main() -> ExitCode {
    let host = Host::from_env();
    match cli::parse(&host) {
        Action::HarnessMenu => harness::menu(&host),
        Action::HarnessUpdate { name } => {
            images::update(&host, engine::select(&host), name.as_deref())
        }
        Action::Update { harness } => {
            images::refresh(&host, engine::select(&host), harness.as_deref())
        }
        Action::Run(config) => run(&host, config),
    }
}

fn run(host: &Host, config: Config) -> ExitCode {
    let engine = engine::select(host);
    let harness = harness::resolve(host, config.harness.as_deref());
    let plan = images::plan(&config.image, harness.name);

    // Fail fast on a missing host-engine socket, before any build work;
    // the boundary warning also precedes the (streamed) build output.
    let engine_args = host_engine_args(host, &config, engine);

    let image = match plan {
        images::Plan::Layered { flavor, target } => {
            images::ensure(engine, &flavor, &target, harness, images::registry(host));
            target
        }
        images::Plan::Verbatim => config.image.clone(),
    };

    // A failed build must not waste an `op read` prompt: keys resolve last.
    // keys::load registers the env-file with the signal layer for cleanup
    // on any exit path.
    signals::install();
    let env_file = keys::load(&config, host);
    let agent_home = ensure_agent_home(host);

    let command = command(&config, harness);
    let launch = argv::Launch {
        userns: engine::userns_args(engine, host.uid, host.gid),
        container_name: format!("playpen-agent-{}", host.pid),
        workspace: host.workspace.as_path(),
        agent_home: agent_home.as_path(),
        cargo_volume: config.image.starts_with("playpen-rust"),
        engine_args,
        env_file: env_file.as_ref().map(|f| f.path()),
        image: &image,
        command: &command,
    };
    let args = argv::build(&launch);

    match Command::new(engine.binary()).args(&args).status() {
        Ok(status) => ExitCode::from(signals::status_code(&status) as u8),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("playpen: {}: command not found", engine.binary());
            ExitCode::from(127)
        }
        Err(err) => {
            eprintln!("playpen: could not run {}: {err}", engine.binary());
            ExitCode::from(126)
        }
    }
}

/// The container command: an explicit `-- CMD` wins, `--shell` opens the
/// harness image with bash, otherwise the harness's exec command runs.
fn command(config: &Config, harness: &Harness) -> Vec<OsString> {
    if !config.command.is_empty() {
        return config.command.clone();
    }
    if config.shell {
        return vec![OsString::from("/bin/bash")];
    }
    vec![OsString::from(harness.exec)]
}

fn host_engine_args(host: &Host, config: &Config, engine: engine::Engine) -> Vec<OsString> {
    if !config.host_engine {
        return Vec::new();
    }
    // Socket diagnostics (and exit) first, then the boundary warning - the
    // bash launcher's order.
    let socket = engine::host_socket(engine, host);
    eprintln!("playpen: host engine access enabled - the boundary is your host user");
    engine::host_engine_args(engine, &socket)
}

fn ensure_agent_home(host: &Host) -> PathBuf {
    let dir = host.agent_home_dir();
    if !dir.is_dir() {
        if let Err(err) = std::fs::create_dir_all(&dir) {
            signals::fail(format!(
                "playpen: could not create {}: {err}",
                dir.display()
            ));
        }
        eprintln!("playpen: created {}", dir.display());
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(shell: bool, command: &[&str]) -> Config {
        Config {
            host_engine: false,
            keys_file: PathBuf::new(),
            keys_explicit: false,
            harness: None,
            shell,
            image: String::from("playpen-base"),
            command: command.iter().map(OsString::from).collect(),
        }
    }

    #[test]
    fn command_resolution_shell_explicit_harness() {
        let omp = harness::lookup("omp").unwrap();
        assert_eq!(
            command(&config(false, &[]), omp),
            vec![OsString::from("omp")],
            "no command: the harness's exec runs"
        );
        assert_eq!(
            command(&config(true, &[]), omp),
            vec![OsString::from("/bin/bash")],
            "--shell opens the harness image with bash"
        );
        assert_eq!(
            command(&config(false, &["mvn", "test"]), omp),
            vec![OsString::from("mvn"), OsString::from("test")],
            "an explicit command wins"
        );
    }
}
