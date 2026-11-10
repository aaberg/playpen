//! playpen: launch an AI coding agent inside a sandboxed container.
//! The launcher resolves configuration, provisions the selected image and
//! harness, and runs the container through Podman or Docker.

mod argv;
mod cli;
mod config;
mod engine;
mod harness;
mod host;
mod images;
mod keys;
mod setup;
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
        Action::Setup => match setup::run(&host, true) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("playpen: {err}");
                ExitCode::FAILURE
            }
        },
        Action::HarnessUpdate { name } => {
            images::update(&host, engine::select(&host), name.as_deref())
        }
        Action::Update { harness } => {
            images::refresh(&host, engine::select(&host), harness.as_deref())
        }
        Action::Config { global, cmd } => {
            let result = match cmd {
                cli::ConfigSub::Set { key, value } => {
                    config::edit(&host, global, &key, Some(&value))
                }
                cli::ConfigSub::Rm { key } => config::edit(&host, global, &key, None),
                cli::ConfigSub::List => config::list(&host, global),
            };
            match result {
                Ok(report) => {
                    print!("{report}");
                    if !report.ends_with('\n') {
                        println!();
                    }
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("playpen: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Action::Run(config) => run(&host, config),
    }
}

fn run(host: &Host, config: Config) -> ExitCode {
    if config.image.is_none() {
        let ready = match config::project_ready(host) {
            Ok(ready) => ready,
            Err(err) => {
                eprintln!("playpen: {err}");
                return ExitCode::FAILURE;
            }
        };
        if !ready {
            if let Err(err) = setup::run(host, false) {
                eprintln!("playpen: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    let selection = match config::selection(host) {
        Ok(selection) => selection,
        Err(err) => {
            eprintln!("playpen: {err}");
            return ExitCode::FAILURE;
        }
    };
    let image = match config.image.as_deref() {
        Some(image) => image,
        None => match selection.image {
            Some(config::Image::Rust) => "playpen-rust",
            Some(config::Image::Jvm) => "playpen-jvm",
            Some(config::Image::Custom) => {
                eprintln!(
                    "playpen: custom image provisioning is not implemented until milestone 3; \
                     settings are saved; use an explicit IMAGE to run an existing image"
                );
                return ExitCode::FAILURE;
            }
            None => unreachable!("setup saves a complete project selection"),
        },
    };
    let settings = match config::resolve(host, config.host_engine) {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!("playpen: {err}");
            return ExitCode::FAILURE;
        }
    };
    let harness = harness::resolve(
        config
            .harness
            .as_deref()
            .or(selection.harness.as_ref().map(|choice| choice.as_str())),
    );
    let engine = engine::select(host);
    let plan = images::plan(image, harness.name);

    // Fail fast on a missing host-engine socket, before any build work;
    // the boundary warning also precedes the (streamed) build output.
    let engine_args = host_engine_args(host, settings.host_engine, engine);

    let image = match plan {
        images::Plan::Layered { flavor, target } => {
            images::ensure(engine, &flavor, &target, harness, images::registry(host));
            target
        }
        images::Plan::Verbatim => image.to_owned(),
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
        cargo_volume: image.starts_with("playpen-rust"),
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

fn host_engine_args(host: &Host, enabled: bool, engine: engine::Engine) -> Vec<OsString> {
    if !enabled {
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
            host_engine: None,
            keys_file: PathBuf::new(),
            keys_explicit: false,
            harness: None,
            shell,
            image: Some(String::from("playpen-base")),
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
