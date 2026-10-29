//! Path resolution: the XDG fallbacks `${XDG_*:-default}` the bash launcher
//! used, plus the workspace ($PWD) validation bash performs at startup.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `${VAR:-fallback}` - an unset *or empty* variable falls back, like bash.
fn xdg_or(var: &str, fallback: &[&str]) -> PathBuf {
    let nonempty = std::env::var_os(var).filter(|v| !v.is_empty());
    let mut parts: Vec<OsString> = match nonempty {
        Some(v) => vec![v],
        None => fallback.iter().map(OsString::from).collect(),
    };
    parts.pop().expect("fallback path is non-empty");
    let mut path = PathBuf::from(parts.pop().expect("first component"));
    for part in parts {
        path.push(part);
    }
    path
}

/// `~/.config/playpen/keys.conf` (honors XDG_CONFIG_HOME).
pub fn default_keys_file() -> PathBuf {
    xdg_or("XDG_CONFIG_HOME", &["$HOME", ".config"]).join("playpen/keys.conf")
}

/// `~/.local/share/playpen-home` (honors XDG_DATA_HOME).
pub fn agent_home_dir() -> PathBuf {
    xdg_or("XDG_DATA_HOME", &["$HOME", ".local", "share"]).join("playpen-home")
}

/// Temp-file directory: XDG_RUNTIME_DIR, else /tmp.
pub fn runtime_dir() -> PathBuf {
    xdg_or("XDG_RUNTIME_DIR", &["/tmp"])
}

/// The workspace to mount: $PWD if it still points at the current directory
/// (bash verifies this at startup), else the physical cwd.
pub fn workspace() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    match std::env::var_os("PWD") {
        Some(pwd) if same_dir(&Path::new(&pwd), &cwd) => PathBuf::from(pwd),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_env() {
        std::env::set_var("HOME", "/home/tester");
        std::env::set_var("XDG_CONFIG_HOME", "");
        std::env::set_var("XDG_DATA_HOME", "");
        std::env::set_var("XDG_RUNTIME_DIR", "");
    }

    #[test]
    fn xdg_fallbacks_match_bash() {
        test_env();
        assert_eq!(
            default_keys_file(),
            PathBuf::from("/home/tester/.config/playpen/keys.conf")
        );
        assert_eq!(
            agent_home_dir(),
            PathBuf::from("/home/tester/.local/share/playpen-home")
        );
        assert_eq!(runtime_dir(), PathBuf::from("/tmp"));

        std::env::set_var("XDG_CONFIG_HOME", "/xdg/config");
        std::env::set_var("XDG_DATA_HOME", "/xdg/data");
        std::env::set_var("XDG_RUNTIME_DIR", "/xdg/run");
        assert_eq!(
            default_keys_file(),
            PathBuf::from("/xdg/config/playpen/keys.conf")
        );
        assert_eq!(
            agent_home_dir(),
            PathBuf::from("/xdg/data/playpen-home")
        );
        assert_eq!(runtime_dir(), PathBuf::from("/xdg/run"));
    }

    #[test]
    fn workspace_prefers_valid_pwd() {
        let cwd = std::env::current_dir().unwrap();
        std::env::set_var("PWD", &cwd);
        assert_eq!(workspace(), cwd);
        std::env::set_var("PWD", "/definitely/not/here");
        assert_eq!(workspace(), cwd);
    }
}
