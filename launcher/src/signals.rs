//! Signal handling and process exit, the counterpart of the bash EXIT trap:
//!
//! - SIGINT: recorded, not acted on. The engine child receives the same
//!   SIGINT (foreground process group); the launcher keeps waiting and then
//!   propagates the child's status - bash's "wait and cooperative exit".
//!   Without a handler the launcher would die before removing the env-file.
//! - SIGQUIT/SIGTERM/SIGHUP: remove the env-file (it holds API keys), then
//!   exit with 128+signal like a shell would report for itself. The bash
//!   launcher leaked the file on SIGQUIT and SIGTERM; the port cleans up on
//!   every terminating signal it can catch.
//! - [`fail`]: the single exit path for fatal errors. The env-file is
//!   deleted before exiting, because `process::exit` skips destructors -
//!   the bash launcher's EXIT trap did this with a trap.

use std::path::PathBuf;
use std::sync::OnceLock;

use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

static ENV_FILE: OnceLock<PathBuf> = OnceLock::new();

pub fn install() {
    let mut signals = match Signals::new([SIGINT, SIGQUIT, SIGTERM, SIGHUP]) {
        Ok(signals) => signals,
        Err(_) => return, // degrade: bash without handlers behaves the same
    };
    std::thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGINT => {}
                _ => {
                    if let Some(path) = ENV_FILE.get() {
                        let _ = std::fs::remove_file(path);
                    }
                    std::process::exit(128 + sig);
                }
            }
        }
    });
}

/// Remember the env-file so signals and [`fail`] can delete it.
pub fn register_env_file(path: PathBuf) {
    let _ = ENV_FILE.set(path);
}

/// Print a fatal error and exit 1, deleting the env-file first.
pub fn fail(message: String) -> ! {
    if let Some(path) = ENV_FILE.get() {
        let _ = std::fs::remove_file(path);
    }
    eprintln!("{message}");
    std::process::exit(1)
}
