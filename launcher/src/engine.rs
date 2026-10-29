//! Container engine concerns, isolated in one place: selection, user
//! mapping and the host-engine socket plumbing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::host::Host;
use crate::signals;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Podman,
    Docker,
}

impl Engine {
    pub fn binary(self) -> &'static str {
        match self {
            Engine::Podman => "podman",
            Engine::Docker => "docker",
        }
    }
}

/// PLAYPEN_ENGINE when set, else autodetect podman -> docker.
/// Fatal errors print the bash launcher's exact messages.
pub fn select(host: &Host) -> Engine {
    match host.engine_pref.as_deref() {
        Some("podman") => Engine::Podman,
        Some("docker") => Engine::Docker,
        Some(other) => {
            signals::fail(format!(
                "playpen: unknown engine: {other} (expected podman or docker)"
            ))
        }
        None => {
            if host.find_in_path("podman") {
                Engine::Podman
            } else if host.find_in_path("docker") {
                Engine::Docker
            } else {
                signals::fail(String::from(
                    "playpen: neither podman nor docker found in PATH",
                ))
            }
        }
    }
}

/// Podman maps the agent to the invoking user via keep-id; docker has no
/// equivalent, so run as the invoking UID:GID directly.
pub fn userns_args(engine: Engine, uid: u32, gid: u32) -> Vec<OsString> {
    match engine {
        Engine::Podman => vec![OsString::from("--userns=keep-id")],
        Engine::Docker => vec![
            OsString::from("--user"),
            OsString::from(format!("{uid}:{gid}")),
        ],
    }
}

/// First candidate that exists as a socket - docker's fallback chain
/// (primary, then /var/run/docker.sock) and podman's single path both go
/// through here.
pub fn pick_socket(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|c| is_socket(c)).cloned()
}

/// Locate the host engine socket for `--host-engine`, with the bash
/// launcher's exact diagnostics on failure.
pub fn host_socket(engine: Engine, host: &Host) -> PathBuf {
    match engine {
        Engine::Podman => {
            match pick_socket(&[PathBuf::from(format!(
                "/run/user/{}/podman/podman.sock",
                host.uid
            ))]) {
                Some(socket) => socket,
                None => signals::fail(
                    "playpen: host podman socket not found - start it with: \
                     systemctl --user start podman.socket"
                        .to_string(),
                ),
            }
        }
        Engine::Docker => {
            match pick_socket(&[
                host.home.join(".docker/run/docker.sock"),
                PathBuf::from("/var/run/docker.sock"),
            ]) {
                Some(socket) => socket,
                None => signals::fail(String::from(
                    "playpen: host docker socket not found - is the docker daemon running?",
                )),
            }
        }
    }
}

/// The engine args that grant the container host-engine access.
/// -v socket:/run/host-engine.sock plus DOCKER_HOST (both engines) and
/// CONTAINER_HOST (podman only).
pub fn host_engine_args(engine: Engine, socket: &Path) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-v"),
        {
            let mut v = socket.as_os_str().to_os_string();
            v.push(":/run/host-engine.sock");
            v
        },
        OsString::from("-e"),
        OsString::from("DOCKER_HOST=unix:///run/host-engine.sock"),
    ];
    if engine == Engine::Podman {
        args.push(OsString::from("-e"));
        args.push(OsString::from("CONTAINER_HOST=unix:///run/host-engine.sock"));
    }
    args
}

/// `[[ -S $file ]]`: stat succeeds and the entry is a socket.
pub fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn userns_args_per_engine() {
        assert_eq!(
            userns_args(Engine::Podman, 1000, 1001),
            vec![OsString::from("--userns=keep-id")]
        );
        assert_eq!(
            userns_args(Engine::Docker, 1000, 1001),
            vec![OsString::from("--user"), OsString::from("1000:1001")]
        );
    }

    #[test]
    fn host_engine_args_mount_and_env() {
        let podman = host_engine_args(Engine::Podman, Path::new("/run/user/1000/podman/podman.sock"));
        assert_eq!(
            podman,
            vec![
                OsString::from("-v"),
                OsString::from("/run/user/1000/podman/podman.sock:/run/host-engine.sock"),
                OsString::from("-e"),
                OsString::from("DOCKER_HOST=unix:///run/host-engine.sock"),
                OsString::from("-e"),
                OsString::from("CONTAINER_HOST=unix:///run/host-engine.sock"),
            ]
        );
        let docker = host_engine_args(Engine::Docker, Path::new("/var/run/docker.sock"));
        assert_eq!(
            docker,
            vec![
                OsString::from("-v"),
                OsString::from("/var/run/docker.sock:/run/host-engine.sock"),
                OsString::from("-e"),
                OsString::from("DOCKER_HOST=unix:///run/host-engine.sock"),
            ]
        );
        assert!(!docker
            .iter()
            .any(|a| a.to_string_lossy().starts_with("CONTAINER_HOST")));
    }

    #[test]
    fn socket_detection() {
        let dir = std::env::temp_dir().join(format!("playpen-sock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(is_socket(&sock));
        drop(listener);

        let plain = dir.join("plain");
        std::fs::write(&plain, "x").unwrap();
        assert!(!is_socket(&plain));
        assert!(!is_socket(&dir.join("missing.sock")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pick_socket_uses_first_existing_socket() {
        let dir = std::env::temp_dir().join(format!("playpen-pick-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.sock");
        let b = dir.join("b.sock");
        let plain = dir.join("plain");

        // nothing exists yet
        assert_eq!(pick_socket(std::slice::from_ref(&a)), None);

        // fallback wins when the primary is missing
        let listener_b = std::os::unix::net::UnixListener::bind(&b).unwrap();
        assert_eq!(pick_socket(&[a.clone(), b.clone()]), Some(b.clone()));

        // primary wins once it exists
        let listener_a = std::os::unix::net::UnixListener::bind(&a).unwrap();
        assert_eq!(pick_socket(&[a.clone(), b]), Some(a));

        // non-socket files never match
        std::fs::write(&plain, "x").unwrap();
        assert_eq!(pick_socket(&[plain]), None);

        drop(listener_a);
        drop(listener_b);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
