//! A snapshot of everything the launcher reads from its environment, taken
//! once at startup. Downstream code is pure and testable against an explicit
//! [`Host`] value instead of ambient process state.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Everything the bash launcher pulled out of the environment: ids, HOME,
/// XDG dirs, the workspace, the engine preference and PATH.
#[derive(Debug, Clone)]
pub struct Host {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    /// $HOME (never empty: unset HOME expands so paths anchor at /, like bash).
    pub home: PathBuf,
    /// XDG_CONFIG_HOME when set and non-empty.
    pub config_home: Option<PathBuf>,
    /// XDG_DATA_HOME when set and non-empty.
    pub data_home: Option<PathBuf>,
    /// XDG_RUNTIME_DIR when set and non-empty, else /tmp.
    pub runtime_dir: PathBuf,
    /// The workspace to mount: $PWD if it matches the cwd, else the cwd.
    pub workspace: PathBuf,
    /// PLAYPEN_ENGINE (set and non-empty), podman or docker.
    pub engine_pref: Option<String>,
    /// PLAYPEN_HOST_ENGINE matched ^(1|true|yes)$.
    pub host_engine_env: bool,
    /// $PATH.
    pub path: OsString,
}

impl Host {
    pub fn from_env() -> Self {
        Host {
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            pid: std::process::id(),
            home: home_dir(),
            config_home: nonempty_dir("XDG_CONFIG_HOME"),
            data_home: nonempty_dir("XDG_DATA_HOME"),
            runtime_dir: nonempty_dir("XDG_RUNTIME_DIR").unwrap_or_else(|| PathBuf::from("/tmp")),
            workspace: pick_workspace(std::env::var_os("PWD")),
            engine_pref: std::env::var("PLAYPEN_ENGINE").ok().filter(|v| !v.is_empty()),
            host_engine_env: host_engine_env_value(
                std::env::var("PLAYPEN_HOST_ENGINE").ok().as_deref(),
            ),
            path: std::env::var_os("PATH").unwrap_or_default(),
        }
    }

    /// `~/.config/playpen/keys.conf` (honors XDG_CONFIG_HOME).
    pub fn default_keys_file(&self) -> PathBuf {
        match &self.config_home {
            Some(dir) => dir.join("playpen/keys.conf"),
            None => self.home.join(".config/playpen/keys.conf"),
        }
    }

    /// `~/.config/playpen/harness.conf` (honors XDG_CONFIG_HOME).
    pub fn harness_conf_file(&self) -> PathBuf {
        match &self.config_home {
            Some(dir) => dir.join("playpen/harness.conf"),
            None => self.home.join(".config/playpen/harness.conf"),
        }
    }

    /// `~/.local/share/playpen-home` (honors XDG_DATA_HOME).
    pub fn agent_home_dir(&self) -> PathBuf {
        match &self.data_home {
            Some(dir) => dir.join("playpen-home"),
            None => self.home.join(".local/share/playpen-home"),
        }
    }

    /// `command -v NAME`: an executable file in some PATH directory.
    pub fn find_in_path(&self, name: &str) -> bool {
        path_dirs(&self.path).iter().any(|dir| is_executable_file(&dir.join(name)))
    }
}

/// PATH split into directories. Empty PATH (or empty elements) mean the
/// current directory, like bash.
pub fn path_dirs(path: &OsStr) -> Vec<PathBuf> {
    let bytes = path.as_encoded_bytes();
    bytes
        .split(|b| *b == b':')
        .map(|dir| {
            if dir.is_empty() {
                PathBuf::from(".")
            } else {
                PathBuf::from(OsStr::from_bytes(dir))
            }
        })
        .collect()
}

/// $HOME with bash's empty-expansion semantics.
fn home_dir() -> PathBuf {
    match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from("/"),
    }
}

fn nonempty_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The workspace: $PWD when it still names the current directory (bash
/// verifies this at startup), else the physical cwd.
fn pick_workspace(pwd: Option<OsString>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    match pwd {
        Some(pwd) if same_dir(Path::new(&pwd), &cwd) => PathBuf::from(pwd),
        _ => cwd,
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// PLAYPEN_HOST_ENGINE enables host-engine mode only for 1, true or yes
/// (bash: `=~ ^(1|true|yes)$`).
pub fn host_engine_env_value(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true") | Some("yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            uid: 1000,
            gid: 1000,
            pid: 4711,
            home: PathBuf::from("/home/tester"),
            config_home: None,
            data_home: None,
            runtime_dir: PathBuf::from("/run/user/1000"),
            workspace: PathBuf::from("/work"),
            engine_pref: None,
            host_engine_env: false,
            path: OsString::from("/usr/bin:/bin"),
        }
    }

    #[test]
    fn xdg_fallbacks_match_bash() {
        let h = host();
        assert_eq!(
            h.default_keys_file(),
            PathBuf::from("/home/tester/.config/playpen/keys.conf")
        );
        assert_eq!(
            h.agent_home_dir(),
            PathBuf::from("/home/tester/.local/share/playpen-home")
        );

        let mut h = host();
        h.config_home = Some(PathBuf::from("/xdg/config"));
        h.data_home = Some(PathBuf::from("/xdg/data"));
        assert_eq!(
            h.default_keys_file(),
            PathBuf::from("/xdg/config/playpen/keys.conf")
        );
        assert_eq!(h.agent_home_dir(), PathBuf::from("/xdg/data/playpen-home"));
    }

    #[test]
    fn host_engine_env_truthiness() {
        let cases = [
            (Some("1"), true),
            (Some("true"), true),
            (Some("yes"), true),
            (Some("0"), false),
            (Some("TRUE"), false),
            (Some("Yes"), false),
            (Some("yes "), false),
            (Some(""), false),
            (Some("on"), false),
            (None, false),
        ];
        for (value, expected) in cases {
            assert_eq!(host_engine_env_value(value), expected, "value {value:?}");
        }
    }

    #[test]
    fn path_dirs_matches_bash_splitting() {
        assert_eq!(
            path_dirs(OsStr::new("/usr/bin:/bin")),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
        // empty elements and an empty PATH mean the current directory
        assert_eq!(
            path_dirs(OsStr::new("/a::/b")),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("."),
                PathBuf::from("/b")
            ]
        );
        assert_eq!(path_dirs(OsStr::new("")), vec![PathBuf::from(".")]);
    }

    #[test]
    fn find_in_path_finds_executables() {
        let dir = std::env::temp_dir().join(format!("playpen-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let exe = dir.join("playpen-stub-finder");
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&exe, 0o755);

        let plain = dir.join("playpen-plain-file");
        std::fs::write(&plain, "data").unwrap();
        make_executable(&plain, 0o644);

        let mut h = host();
        h.path = OsString::from(dir.as_os_str());
        assert!(h.find_in_path("playpen-stub-finder"));
        assert!(!h.find_in_path("playpen-plain-file"));
        assert!(!h.find_in_path("playpen-not-there"));

        // a later directory does not shadow an earlier hit (command -v picks
        // the first), and a missing earlier directory is skipped
        let dir2 = std::env::temp_dir().join(format!("playpen-path-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir2).unwrap();
        h.path = OsString::from(format!("{}:{}", dir2.display(), dir.display()));
        assert!(h.find_in_path("playpen-stub-finder"));

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&dir2).unwrap();
    }

    fn make_executable(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(path, perms).unwrap();
    }
}
