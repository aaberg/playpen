//! playpen: launch an AI coding agent inside a sandboxed container.
//! Phase 1: exact port of the original bash launcher.

mod argv;
mod cli;
mod engine;
mod host;
mod keys;
mod signals;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use crate::cli::Config;
use crate::host::Host;

fn main() -> ExitCode {
    let host = Host::from_env();
    let config = cli::parse(&host);
    let engine = engine::select(&host);

    // Message order matches the bash launcher: keys (op prompts) ->
    // host engine -> agent home -> exec. keys::load registers the env-file
    // with the signal layer for cleanup on any exit path.
    signals::install();
    let env_file = keys::load(&config, &host);

    let engine_args = host_engine_args(&host, &config, engine);
    let agent_home = ensure_agent_home(&host);

    let launch = argv::Launch {
        userns: engine::userns_args(engine, host.uid, host.gid),
        container_name: format!("playpen-agent-{}", host.pid),
        workspace: host.workspace.as_path(),
        agent_home: agent_home.as_path(),
        cargo_volume: config.image.starts_with("playpen-rust"),
        engine_args,
        env_file: env_file.as_ref().map(|f| f.path()),
        image: &config.image,
        command: &config.command,
    };
    let args = argv::build(&launch);

    match Command::new(engine.binary()).args(&args).status() {
        Ok(status) => ExitCode::from(exit_code(&status)),
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

/// The container's exit status becomes ours; a signaled engine becomes 128+sig.
fn exit_code(status: &std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code as u8,
        None => (128 + status.signal().unwrap_or(0)) as u8,
    }
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
            signals::fail(format!("playpen: could not create {}: {err}", dir.display()));
        }
        eprintln!("playpen: created {}", dir.display());
    }
    dir
}
