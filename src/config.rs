//! Project/global configuration, including lossless, atomic command-line edits.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Value};

use crate::host::Host;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Image {
    Rust,
    Jvm,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessChoice {
    Omp,
    Pi,
    Opencode,
    Vibe,
}

macro_rules! choices {
    ($ty:ident, $label:literal, $($variant:ident => $name:literal),+ $(,)?) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }
        }
        impl std::str::FromStr for $ty {
            type Err = String;
            fn from_str(value: &str) -> Result<Self, String> {
                match value {
                    $($name => Ok(Self::$variant),)+
                    _ => Err(format!("invalid {} {value:?}: expected {}", $label,
                        [$($name),+].join(", "))),
                }
            }
        }
    };
}
choices!(Image, "image", Rust => "rust", Jvm => "jvm", Custom => "custom");
choices!(HarnessChoice, "harness", Omp => "omp", Pi => "pi", Opencode => "opencode", Vibe => "vibe");

#[derive(Debug, PartialEq, Eq)]
pub struct Resolved {
    pub host_engine: bool,
    /// The file supplying the value; flags and the built-in default have no file.
    pub source: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub image: Option<Image>,
    pub harness: Option<HarnessChoice>,
    pub containerfile: Option<PathBuf>,
    pub context: Option<PathBuf>,
}

pub fn resolve(host: &Host, host_engine_override: Option<bool>) -> Result<Resolved, String> {
    Ok(Files::load(host)?.resolve(host_engine_override))
}

pub fn selection(host: &Host) -> Result<Selection, String> {
    let files = Files::load(host)?;
    Ok(Selection {
        image: files
            .effective_string("image")
            .map(str::parse)
            .transpose()?,
        harness: files
            .effective_string("harness")
            .map(str::parse)
            .transpose()?,
        containerfile: files.project.resolved_path("containerfile"),
        context: files.project.resolved_path("context"),
    })
}

pub fn project_ready(host: &Host) -> Result<bool, String> {
    let files = Files::load(host)?;
    if !files.project.complete() {
        return Ok(false);
    }
    if files.project.string("image") == Some("custom")
        && (files.project.string("containerfile").is_none()
            || files.project.string("context").is_none())
    {
        return Ok(false);
    }
    files.project.validate_paths(true)?;
    Ok(true)
}

/// Global inspection never discovers or reads project configuration.
pub fn list(host: &Host, global: bool) -> Result<String, String> {
    if global {
        let file = File::load(global_path(host), true)?;
        Ok(describe(&[&file]))
    } else {
        let files = Files::load(host)?;
        Ok(describe(&[&files.project, &files.global]))
    }
}

fn describe(files: &[&File]) -> String {
    let mut output = String::new();
    for key in [
        "image",
        "harness",
        "host_engine",
        "containerfile",
        "context",
    ] {
        if let Some(file) = files.iter().find(|file| file.document.contains_key(key)) {
            let value = file.document[key]
                .as_value()
                .expect("validated scalar")
                .clone();
            let value = match value {
                Value::Boolean(value) => value.value().to_string(),
                Value::String(value) => toml_edit::value(value.value().as_str()).to_string(),
                _ => unreachable!("validated scalar"),
            };
            output.push_str(&format!("# source: {:?}\n{key} = {value}\n", file.path));
        } else if key == "host_engine" {
            output.push_str("# source: built-in default\nhost_engine = false\n");
        }
    }
    output
}

pub fn edit(host: &Host, global: bool, key: &str, value: Option<&str>) -> Result<String, String> {
    validate_key(key, global)?;
    let replacement = value.map(|value| parse_value(key, value)).transpose()?;
    let mut files = Files::load(host)?;
    let file = if global {
        &mut files.global
    } else {
        &mut files.project
    };
    let before = file.document.to_string();
    file.change(key, replacement);
    file.validate(global)?;
    if !global && value.is_some() && matches!(key, "containerfile" | "context") {
        file.validate_path(key, key == "context")?;
    }
    let changed = before != file.document.to_string();
    if changed {
        file.save()?;
    }
    let path = file.path.clone();
    Ok(format!(
        "{} {}\n{}",
        if changed { "Updated" } else { "Unchanged" },
        path.display(),
        describe(&[&files.project, &files.global])
    ))
}

fn validate_key(key: &str, global: bool) -> Result<(), String> {
    match key {
        "image" | "harness" | "host_engine" => Ok(()),
        "containerfile" | "context" if !global => Ok(()),
        "containerfile" | "context" => Err(format!("{key} is project-only; omit --global")),
        _ => Err(format!("unknown configuration key {key:?}")),
    }
}

fn parse_value(key: &str, value: &str) -> Result<Value, String> {
    match key {
        "host_engine" => match value {
            "true" => Ok(Value::from(true)),
            "false" => Ok(Value::from(false)),
            _ => Err(format!(
                "invalid value {value:?} for host_engine: expected true or false"
            )),
        },
        "image" => Ok(Value::from(value.parse::<Image>()?.as_str())),
        "harness" => Ok(Value::from(value.parse::<HarnessChoice>()?.as_str())),
        "containerfile" | "context" if !value.is_empty() => Ok(Value::from(value)),
        _ => Err(format!("{key} must be a nonempty path")),
    }
}

pub(crate) struct Files {
    pub(crate) global: File,
    pub(crate) project: File,
}

impl Files {
    pub(crate) fn load(host: &Host) -> Result<Self, String> {
        let project_path = project_path(host)?;
        Ok(Self {
            global: File::load(global_path(host), true)?,
            project: File::load(project_path, false)?,
        })
    }

    pub(crate) fn effective_string(&self, key: &str) -> Option<&str> {
        self.project.string(key).or_else(|| self.global.string(key))
    }

    pub(crate) fn resolve(&self, host_engine_override: Option<bool>) -> Resolved {
        if let Some(host_engine) = host_engine_override {
            return Resolved {
                host_engine,
                source: None,
            };
        }
        for file in [&self.project, &self.global] {
            if let Some(host_engine) = file.host_engine() {
                return Resolved {
                    host_engine,
                    source: Some(file.path.clone()),
                };
            }
        }
        Resolved {
            host_engine: false,
            source: None,
        }
    }
}

pub(crate) struct File {
    pub(crate) path: PathBuf,
    document: DocumentMut,
}

impl File {
    fn load(path: PathBuf, global: bool) -> Result<Self, String> {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err)
                if err.kind() == io::ErrorKind::NotFound && !configuration_entry_exists(&path)? =>
            {
                String::new()
            }
            Err(err) => return Err(path_error("read configuration", &path, err)),
        };
        let document = text
            .parse::<DocumentMut>()
            .map_err(|err| format!("invalid configuration {}: {err}", path.display()))?;
        let file = Self { path, document };
        file.validate(global)?;
        Ok(file)
    }

    fn validate(&self, global: bool) -> Result<(), String> {
        for (key, item) in self.document.iter() {
            let result = validate_key(key, global).and_then(|()| {
                if key == "host_engine" {
                    item.as_bool()
                        .map(|_| ())
                        .ok_or_else(|| "host_engine must be a boolean".to_owned())
                } else {
                    let value = item
                        .as_str()
                        .ok_or_else(|| format!("{key} must be a string"))?;
                    parse_value(key, value).map(|_| ())
                }
            });
            result
                .map_err(|err| format!("invalid configuration {}: {err}", self.path.display()))?;
        }
        Ok(())
    }

    pub(crate) fn string(&self, key: &str) -> Option<&str> {
        self.document.get(key).and_then(|item| item.as_str())
    }

    pub(crate) fn host_engine(&self) -> Option<bool> {
        self.document
            .get("host_engine")
            .and_then(|item| item.as_bool())
    }

    pub(crate) fn complete(&self) -> bool {
        self.string("image").is_some()
            && self.string("harness").is_some()
            && self.host_engine().is_some()
    }

    pub(crate) fn root(&self) -> &Path {
        self.path
            .parent()
            .expect("config directory")
            .parent()
            .expect("project root")
    }

    pub(crate) fn resolved_path(&self, key: &str) -> Option<PathBuf> {
        self.string(key).map(|value| self.root().join(value))
    }

    pub(crate) fn validate_paths(&self, require_custom: bool) -> Result<(), String> {
        for (key, directory) in [("containerfile", false), ("context", true)] {
            if self.string(key).is_some() {
                self.validate_path(key, directory)?;
            } else if require_custom && self.string("image") == Some("custom") {
                return Err(format!("custom image requires {key}; run playpen setup"));
            }
        }
        Ok(())
    }

    fn validate_path(&self, key: &str, directory: bool) -> Result<(), String> {
        let Some(path) = self.resolved_path(key) else {
            return Ok(());
        };
        let metadata = fs::metadata(&path).map_err(|err| {
            format!(
                "{}; correct {key} with playpen config or run playpen setup",
                path_error("inspect custom image path", &path, err)
            )
        })?;
        if if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        } {
            return Err(format!(
                "{key} {} must be a {}; run playpen setup",
                path.display(),
                if directory { "directory" } else { "file" }
            ));
        }
        Ok(())
    }

    pub(crate) fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.change(key, Some(parse_value(key, value)?));
        Ok(())
    }

    fn change(&mut self, key: &str, replacement: Option<Value>) {
        if let Some(mut replacement) = replacement {
            let unchanged = self
                .document
                .get(key)
                .and_then(|item| item.as_value())
                .map(|old| match (&replacement, old) {
                    (Value::Boolean(new), Value::Boolean(old)) => new.value() == old.value(),
                    (Value::String(new), Value::String(old)) => new.value() == old.value(),
                    _ => false,
                })
                .unwrap_or(false);
            if unchanged
                && !(key == "image"
                    && self.string("image") != Some("custom")
                    && (self.document.contains_key("containerfile")
                        || self.document.contains_key("context")))
            {
                return;
            }
            if let Some(old) = self
                .document
                .get_mut(key)
                .and_then(|item| item.as_value_mut())
            {
                *replacement.decor_mut() = old.decor().clone();
                *old = replacement;
            } else {
                self.document[key] = toml_edit::Item::Value(replacement);
            }
            if key == "image" && self.string("image") != Some("custom") {
                self.remove("containerfile");
                self.remove("context");
            }
        } else {
            self.remove(key);
        }
    }

    fn remove(&mut self, name: &str) {
        let next = self
            .document
            .iter()
            .skip_while(|(key, _)| *key != name)
            .nth(1)
            .map(|(key, _)| key.to_owned());
        let Some((key, item)) = self.document.remove_entry(name) else {
            return;
        };
        let mut comments = key
            .leaf_decor()
            .prefix()
            .and_then(|raw| raw.as_str())
            .unwrap_or("")
            .to_owned();
        let suffix = item
            .as_value()
            .and_then(|value| value.decor().suffix())
            .and_then(|raw| raw.as_str())
            .unwrap_or("");
        if suffix.contains('#') {
            comments.push_str(suffix);
            comments.push('\n');
        }
        if let Some(next) = next {
            let mut key = self.document.key_mut(&next).expect("following key");
            comments.push_str(
                key.leaf_decor()
                    .prefix()
                    .and_then(|raw| raw.as_str())
                    .unwrap_or(""),
            );
            key.leaf_decor_mut().set_prefix(comments);
        } else {
            comments.push_str(self.document.trailing().as_str().unwrap_or(""));
            self.document.set_trailing(comments);
        }
    }

    pub(crate) fn save(&self) -> Result<(), String> {
        self.validate(false)?;
        atomic_write(&self.path, &self.document.to_string())
    }
}

fn global_path(host: &Host) -> PathBuf {
    match host
        .config_home
        .as_deref()
        .filter(|path| path.is_absolute())
    {
        Some(dir) => dir.join("playpen/playpen.toml"),
        None => host.home.join(".config/playpen/playpen.toml"),
    }
}

/// Search physical ancestors without changing the logical workspace mounted
/// into the container. Canonicalizing HOME too prevents symlink spellings from
/// bypassing the home boundary. A .git worktree file is a boundary like a dir.
fn project_path(host: &Host) -> Result<PathBuf, String> {
    let workspace = fs::canonicalize(&host.workspace)
        .map_err(|err| path_error("locate workspace", &host.workspace, err))?;
    let home = match fs::canonicalize(&host.home) {
        Ok(home) => Some(home),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(path_error("locate home directory", &host.home, err)),
    };
    let home_boundary = home.as_deref().filter(|home| workspace.starts_with(home));
    for directory in workspace.ancestors() {
        let candidate = directory.join(".playpen/playpen.toml");
        if configuration_entry_exists(&candidate)? {
            return Ok(candidate);
        }
        let git = directory.join(".git");
        let is_git_root = match fs::metadata(&git) {
            Ok(metadata) => metadata.is_file() || metadata.is_dir(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(err) => return Err(path_error("inspect Git boundary", &git, err)),
        };
        if is_git_root {
            return Ok(candidate);
        }
        if Some(directory) == home_boundary {
            break;
        }
    }
    // Without an existing project file or enclosing Git root, edits belong to
    // the workspace itself, not the home/filesystem boundary reached above.
    Ok(workspace.join(".playpen/playpen.toml"))
}

// Count dangling symlinks as entries so discovery cannot skip them and writes
// cannot mistake them for missing files that are safe to create.
fn configuration_entry_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(path_error("inspect configuration", path, err)),
    }
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let resolved = match fs::canonicalize(path) {
        Ok(resolved) => Some(resolved),
        Err(err) if err.kind() == io::ErrorKind::NotFound && !configuration_entry_exists(path)? => {
            None
        }
        Err(err) => return Err(path_error("resolve configuration", path, err)),
    };
    let destination = resolved.as_deref().unwrap_or(path);
    let parent = destination
        .parent()
        .expect("configuration paths have a parent");
    fs::create_dir_all(parent)
        .map_err(|err| path_error("create configuration directory", parent, err))?;
    let mut temporary = NamedTempFile::with_prefix_in(".playpen-config.", parent)
        .map_err(|err| path_error("create temporary configuration", path, err))?;
    match fs::metadata(destination) {
        Ok(metadata) => temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|err| path_error("preserve configuration permissions", path, err))?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(path_error("inspect configuration", path, err)),
    }
    temporary
        .write_all(contents.as_bytes())
        .map_err(|err| path_error("write configuration", path, err))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|err| path_error("sync configuration", path, err))?;
    temporary
        .persist(destination)
        .map_err(|err| path_error("replace configuration", path, err.error))?;
    Ok(())
}

fn path_error(action: &str, path: &Path, error: io::Error) -> String {
    format!("could not {action} {}: {error}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use tempfile::TempDir;

    struct Fixture {
        root: TempDir,
        host: Host,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let workspace = home.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            let host = Host {
                uid: 1000,
                gid: 1000,
                pid: 123,
                home,
                config_home: None,
                data_home: None,
                runtime_dir: root.path().to_path_buf(),
                workspace,
                engine_pref: None,
                registry_pref: None,
                path: OsString::new(),
            };
            Self { root, host }
        }

        fn put(&self, path: &Path, text: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn project(&self) -> PathBuf {
            self.host.workspace.join(".playpen/playpen.toml")
        }

        fn global(&self) -> PathBuf {
            global_path(&self.host)
        }
    }

    #[test]
    fn precedence_preserves_false_and_missing_is_distinct() {
        let f = Fixture::new();
        assert_eq!(
            resolve(&f.host, None).unwrap(),
            Resolved {
                host_engine: false,
                source: None,
            }
        );
        f.put(&f.global(), "host_engine = true\n");
        f.put(&f.project(), "# No project preference.\n");
        assert_eq!(
            resolve(&f.host, None).unwrap(),
            Resolved {
                host_engine: true,
                source: Some(f.global()),
            }
        );
        f.put(&f.project(), "host_engine = false\n");
        assert_eq!(
            resolve(&f.host, None).unwrap(),
            Resolved {
                host_engine: false,
                source: Some(f.project()),
            }
        );
        assert_eq!(
            resolve(&f.host, Some(true)).unwrap(),
            Resolved {
                host_engine: true,
                source: None,
            }
        );
        f.put(&f.project(), "host_engine = true\n");
        assert_eq!(
            resolve(&f.host, Some(false)).unwrap(),
            Resolved {
                host_engine: false,
                source: None,
            }
        );
    }

    #[test]
    fn nearest_file_does_not_merge_ancestor_project_settings() {
        let f = Fixture::new();
        f.put(
            &f.host.home.join(".playpen/playpen.toml"),
            "host_engine = true\n",
        );
        f.put(
            &f.project(),
            "# Deliberately inherit global, not ancestor project.\n",
        );
        assert!(!resolve(&f.host, None).unwrap().host_engine);
        f.put(&f.global(), "host_engine = true\n");
        assert_eq!(resolve(&f.host, None).unwrap().source, Some(f.global()));
        // An invalid farther project file is not part of the selected layers.
        f.put(
            &f.host.home.join(".playpen/playpen.toml"),
            "unknown = true\n",
        );
        assert!(resolve(&f.host, None).unwrap().host_engine);
    }

    #[test]
    fn home_boundary_is_inclusive_and_never_crossed() {
        let f = Fixture::new();
        f.put(
            &f.root.path().join(".playpen/playpen.toml"),
            "host_engine = true\n",
        );
        assert!(!resolve(&f.host, None).unwrap().host_engine);
        let home_config = f.host.home.join(".playpen/playpen.toml");
        f.put(&home_config, "host_engine = true\n");
        assert_eq!(resolve(&f.host, None).unwrap().source, Some(home_config));
    }

    #[test]
    fn git_file_and_directory_boundaries_are_inclusive_edit_targets() {
        for worktree in [false, true] {
            let mut f = Fixture::new();
            let git_root = f.host.workspace.clone();
            if worktree {
                f.put(&git_root.join(".git"), "gitdir: /some/worktree\n");
            } else {
                fs::create_dir(git_root.join(".git")).unwrap();
            }
            f.put(
                &f.host.home.join(".playpen/playpen.toml"),
                "host_engine = true\n",
            );
            f.host.workspace = git_root.join("nested/deeper");
            fs::create_dir_all(&f.host.workspace).unwrap();
            assert!(!resolve(&f.host, None).unwrap().host_engine);
            assert_eq!(
                project_path(&f.host).unwrap(),
                git_root.join(".playpen/playpen.toml")
            );
            edit(&f.host, false, "host_engine", Some("true")).unwrap();
            assert_eq!(
                resolve(&f.host, None).unwrap().source,
                Some(git_root.join(".playpen/playpen.toml"))
            );
        }
    }

    #[test]
    fn outside_home_searches_to_filesystem_root_but_edits_workspace() {
        let mut f = Fixture::new();
        f.host.workspace = f.root.path().join("outside/nested");
        fs::create_dir_all(&f.host.workspace).unwrap();
        assert_eq!(project_path(&f.host).unwrap(), f.project());
        let outer = f.root.path().join(".playpen/playpen.toml");
        f.put(&outer, "host_engine = true\n");
        assert_eq!(resolve(&f.host, None).unwrap().source, Some(outer));
    }

    #[test]
    fn symlink_spellings_cannot_bypass_home_boundary_or_change_workspace() {
        let mut f = Fixture::new();
        let physical_workspace = f.host.workspace.clone();
        let logical_home = f.root.path().join("home-link");
        symlink(&f.host.home, &logical_home).unwrap();
        f.host.home = logical_home.clone();
        f.host.workspace = logical_home.join("workspace");
        f.put(
            &f.root.path().join(".playpen/playpen.toml"),
            "host_engine = true\n",
        );
        assert!(!resolve(&f.host, None).unwrap().host_engine);
        assert_eq!(f.host.workspace, logical_home.join("workspace"));
        assert_eq!(
            project_path(&f.host).unwrap(),
            physical_workspace.join(".playpen/playpen.toml")
        );
    }

    #[test]
    fn invalid_layers_are_fatal_even_when_overridden() {
        let f = Fixture::new();
        for invalid in [
            "host_engine = [",
            "host_engine = 'true'\n",
            "host_engine = 1\n",
            "other = true\n",
            "[host_engine]\nenabled = true\n",
        ] {
            f.put(&f.global(), invalid);
            f.put(&f.project(), "host_engine = false\n");
            let error = resolve(&f.host, Some(false)).unwrap_err();
            assert!(error.contains(&f.global().display().to_string()), "{error}");
            f.put(&f.global(), "host_engine = true\n");
            f.put(&f.project(), invalid);
            let error = resolve(&f.host, Some(true)).unwrap_err();
            assert!(
                error.contains(&f.project().display().to_string()),
                "{error}"
            );
        }
    }

    #[test]
    fn edit_replaces_atomically_preserving_comments_and_permissions() {
        let f = Fixture::new();
        let path = f.project();
        let original =
            "# Project preference\nhost_engine  =  false # Keep this explanation.\n\n# Footer\n";
        f.put(&path, original);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let old_file = fs::File::open(&path).unwrap();
        let old_inode = old_file.metadata().unwrap().ino();
        let report = edit(&f.host, false, "host_engine", Some("true")).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original.replace("false", "true")
        );
        assert_ne!(fs::metadata(&path).unwrap().ino(), old_inode);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(report.contains(&path.display().to_string()));
        assert!(report.contains("host_engine = true"));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn edits_follow_configuration_symlinks_preserving_links_and_target_metadata() {
        for global in [false, true] {
            for chained in [false, true] {
                let f = Fixture::new();
                let path = if global { f.global() } else { f.project() };
                let target = f.host.home.join("dotfiles/playpen.toml");
                let original = "# Preference\nhost_engine = false\n# Footer\n";
                f.put(&target, original);
                fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let link = if chained {
                    symlink("playpen.toml", target.with_file_name("alias.toml")).unwrap();
                    target.with_file_name("alias.toml")
                } else {
                    target.clone()
                };
                // Both scopes are two directories below HOME.
                let relative = Path::new("../..").join(link.strip_prefix(&f.host.home).unwrap());
                symlink(&relative, &path).unwrap();
                let old_file = fs::File::open(&target).unwrap();
                let report = edit(&f.host, global, "host_engine", Some("true")).unwrap();
                assert_eq!(fs::read_link(&path).unwrap(), relative);
                assert_eq!(
                    fs::read_to_string(&target).unwrap(),
                    original.replace("false", "true")
                );
                assert_ne!(
                    fs::metadata(&target).unwrap().ino(),
                    old_file.metadata().unwrap().ino()
                );
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    fs::read_to_string(&target).unwrap()
                );
                assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                    0o640
                );
                assert!(report.contains(&path.display().to_string()), "{report}");
                assert_eq!(resolve(&f.host, None).unwrap().source, Some(path.clone()));

                edit(&f.host, global, "host_engine", None).unwrap();
                assert_eq!(fs::read_link(&path).unwrap(), relative);
                assert_eq!(
                    fs::read_to_string(&target).unwrap(),
                    "# Preference\n# Footer\n"
                );
                assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                    0o640
                );
                if chained {
                    assert_eq!(fs::read_link(&link).unwrap(), Path::new("playpen.toml"));
                }
                assert_eq!(
                    fs::read_dir(target.parent().unwrap()).unwrap().count(),
                    if chained { 2 } else { 1 }
                );
            }
        }
    }

    #[test]
    fn broken_configuration_links_fail_without_replacement_or_ancestor_edits() {
        for global in [false, true] {
            for cyclic in [false, true] {
                let f = Fixture::new();
                let ancestor = f.host.home.join(".playpen/playpen.toml");
                f.put(&ancestor, "host_engine = false\n");
                let path = if global { f.global() } else { f.project() };
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let target = if cyclic {
                    path.clone()
                } else {
                    f.host.home.join("missing.toml")
                };
                symlink(&target, &path).unwrap();
                for value in [Some("true"), None] {
                    let error = edit(&f.host, global, "host_engine", value).unwrap_err();
                    assert!(error.contains(&path.display().to_string()), "{error}");
                }
                let error = list(&f.host, global).unwrap_err();
                assert!(error.contains(&path.display().to_string()), "{error}");
                // The writer must also reject a link that became broken after loading.
                let error = atomic_write(&path, "host_engine = true\n").unwrap_err();
                assert!(error.contains(&path.display().to_string()), "{error}");
                assert_eq!(fs::read_link(&path).unwrap(), target);
                assert_eq!(
                    fs::read_to_string(&ancestor).unwrap(),
                    "host_engine = false\n"
                );
                if !cyclic {
                    assert!(!target.exists());
                }
                assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
            }
        }
    }

    #[test]
    fn removal_is_idempotent_and_never_creates_missing_files() {
        let f = Fixture::new();
        edit(&f.host, false, "host_engine", None).unwrap();
        assert!(!f.project().parent().unwrap().exists());
        edit(&f.host, true, "host_engine", None).unwrap();
        assert!(!f.global().parent().unwrap().exists());
        f.put(&f.project(), "# Keep only this comment.\n");
        edit(&f.host, false, "host_engine", None).unwrap();
        assert_eq!(
            fs::read_to_string(f.project()).unwrap(),
            "# Keep only this comment.\n"
        );
        f.put(&f.global(), "host_engine = true\n");
        f.put(&f.project(), "# Heading\nhost_engine = false\n# Footer\n");
        let report = edit(&f.host, false, "host_engine", None).unwrap();
        assert_eq!(
            fs::read_to_string(f.project()).unwrap(),
            "# Heading\n# Footer\n"
        );
        assert!(report.contains("host_engine = true"));
        assert!(report.contains(&f.global().display().to_string()));
        edit(&f.host, false, "host_engine", None).unwrap();
        f.put(
            &f.project(),
            "# Heading\nhost_engine = false # Local override.\n# Footer\n",
        );
        edit(&f.host, false, "host_engine", None).unwrap();
        let comments = fs::read_to_string(f.project()).unwrap();
        assert!(comments.contains("# Heading"));
        assert!(comments.contains("# Local override."));
        assert!(comments.contains("# Footer"));
        assert!(!comments.contains("host_engine"));
        assert!(resolve(&f.host, None).unwrap().host_engine);
    }

    #[test]
    fn edit_rejects_invalid_cli_and_other_layer_before_any_mutation() {
        let f = Fixture::new();
        for invalid in ["True", "FALSE", "1", "yes", "", " true"] {
            assert!(edit(&f.host, false, "host_engine", Some(invalid)).is_err());
        }
        assert!(edit(&f.host, false, "unknown", Some("true")).is_err());
        assert!(edit(&f.host, false, "unknown", None).is_err());
        assert!(!f.project().exists());
        f.put(&f.global(), "unknown = true\n");
        assert!(edit(&f.host, false, "host_engine", Some("true")).is_err());
        assert!(!f.project().exists());
        f.put(&f.global(), "host_engine = false\n");
        f.put(&f.project(), "host_engine = 'invalid'\n");
        assert!(edit(&f.host, true, "host_engine", Some("true")).is_err());
        assert_eq!(
            fs::read_to_string(f.global()).unwrap(),
            "host_engine = false\n"
        );
    }

    #[test]
    fn xdg_config_requires_absolute_nonempty_path() {
        let mut f = Fixture::new();
        let fallback = f.global();
        f.put(&fallback, "host_engine = true\n");
        for invalid in [PathBuf::new(), PathBuf::from("relative/config")] {
            f.host.config_home = Some(invalid);
            assert_eq!(global_path(&f.host), fallback);
            assert_eq!(
                resolve(&f.host, None).unwrap().source,
                Some(fallback.clone())
            );
        }
        let xdg = f.root.path().join("xdg");
        f.host.config_home = Some(xdg.clone());
        assert_eq!(global_path(&f.host), xdg.join("playpen/playpen.toml"));
        assert!(!resolve(&f.host, None).unwrap().host_engine);
        edit(&f.host, true, "host_engine", Some("true")).unwrap();
        assert_eq!(
            resolve(&f.host, None).unwrap().source,
            Some(xdg.join("playpen/playpen.toml"))
        );
    }

    #[test]
    fn typed_values_and_project_only_paths_are_validated_without_writes() {
        let f = Fixture::new();
        for (key, value) in [
            ("image", "Rust"),
            ("harness", "unknown"),
            ("context", ""),
            ("containerfile", "missing"),
        ] {
            assert!(edit(&f.host, false, key, Some(value)).is_err());
        }
        for key in ["containerfile", "context"] {
            assert!(edit(&f.host, true, key, Some(".")).is_err());
        }
        assert!(!f.project().exists());
        for harness in ["omp", "pi", "opencode", "vibe"] {
            edit(&f.host, false, "harness", Some(harness)).unwrap();
            assert_eq!(
                selection(&f.host).unwrap().harness.unwrap().as_str(),
                harness
            );
        }
        f.put(&f.global(), "containerfile = 'Containerfile'\n");
        assert!(list(&f.host, true).is_err());
    }

    #[test]
    fn partial_custom_configuration_is_not_ready_and_paths_use_discovered_root() {
        let mut f = Fixture::new();
        let root = f.host.workspace.clone();
        f.put(&root.join("recipes/Containerfile"), "FROM scratch\n");
        f.put(
            &f.project(),
            "image = 'custom'\nharness = 'omp'\nhost_engine = false\n",
        );
        f.host.workspace = root.join("nested/deep");
        fs::create_dir_all(&f.host.workspace).unwrap();
        assert!(!project_ready(&f.host).unwrap());
        edit(
            &f.host,
            false,
            "containerfile",
            Some("recipes/Containerfile"),
        )
        .unwrap();
        edit(&f.host, false, "context", Some("recipes")).unwrap();
        assert!(project_ready(&f.host).unwrap());
        let selected = selection(&f.host).unwrap();
        assert_eq!(
            selected.containerfile,
            Some(root.join("recipes/Containerfile"))
        );
        assert_eq!(selected.context, Some(root.join("recipes")));
        fs::remove_file(root.join("recipes/Containerfile")).unwrap();
        assert!(project_ready(&f.host).is_err());
        // Stale paths do not prevent replacing the other path or switching away.
        edit(&f.host, false, "context", Some(".")).unwrap();
        edit(&f.host, false, "image", Some("rust")).unwrap();
        assert!(project_ready(&f.host).unwrap());
    }

    #[test]
    fn builtin_switch_removes_multiple_keys_but_preserves_each_comment() {
        let f = Fixture::new();
        f.put(&f.project(), "# image note\nimage = 'custom'\n# recipe note\ncontainerfile = 'stale' # recipe inline\n# context note\ncontext = 'stale' # context inline\n# harness note\nharness = 'omp'\nhost_engine = false\n# footer\n");
        edit(&f.host, false, "image", Some("jvm")).unwrap();
        let saved = fs::read_to_string(f.project()).unwrap();
        for comment in [
            "# image note",
            "# recipe note",
            "# recipe inline",
            "# context note",
            "# context inline",
            "# harness note",
            "# footer",
        ] {
            assert_eq!(saved.matches(comment).count(), 1, "{saved}");
        }
        assert!(!saved.contains("containerfile ="));
        assert!(!saved.contains("context ="));
        assert!(project_ready(&f.host).unwrap());
    }

    #[test]
    fn global_inspection_does_not_read_invalid_project() {
        let f = Fixture::new();
        f.put(&f.project(), "invalid = [");
        f.put(
            &f.global(),
            "image = 'jvm'\nharness = 'pi'\nhost_engine = true\n",
        );
        let listing = list(&f.host, true).unwrap();
        assert!(listing.contains("image = \"jvm\""));
        assert!(listing.contains("harness = \"pi\""));
        assert!(listing.contains("host_engine = true"));
    }
}
