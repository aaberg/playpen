//! Interactive project configuration. No runtime or image operations happen here.
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use crate::config::{Files, HarnessChoice, Image};
use crate::host::Host;

pub fn run(host: &Host, reconfigure: bool) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("project setup requires an interactive terminal; run playpen setup in a terminal (or supply an explicit IMAGE to launch without setup)".to_owned());
    }
    run_with(
        host,
        reconfigure,
        &mut io::stdin().lock(),
        &mut io::stderr().lock(),
    )
}

fn run_with(
    host: &Host,
    reconfigure: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<(), String> {
    let mut files = Files::load(host)?;
    writeln!(
        output,
        "Project setup (q or Ctrl-C cancels without saving).\nPaths are relative to {}.",
        files.project.root().display()
    )
    .map_err(prompt_error)?;
    if reconfigure || files.project.string("image").is_none() {
        let default = files.effective_string("image").unwrap_or("rust").to_owned();
        let image = prompt(
            input,
            output,
            "Image (rust/jvm/custom)",
            Some(&default),
            |value| value.parse::<Image>().map(|_| ()),
        )?;
        files.project.set("image", &image)?;
    }
    if files.project.string("image") == Some("custom") {
        let old_recipe = files.project.string("containerfile").map(str::to_owned);
        let mut changed_recipe = false;
        if reconfigure || old_recipe.is_none() {
            let default = old_recipe.as_deref().unwrap_or("Containerfile");
            let recipe = prompt(
                input,
                output,
                "Custom containerfile",
                Some(default),
                |value| validate_path(files.project.root(), value, false),
            )?;
            changed_recipe = old_recipe.as_deref() != Some(recipe.as_str());
            files.project.set("containerfile", &recipe)?;
        }
        if reconfigure || changed_recipe || files.project.string("context").is_none() {
            let recipe = files
                .project
                .string("containerfile")
                .expect("recipe collected");
            let recipe_parent = Path::new(recipe)
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let default = if changed_recipe {
                recipe_parent.to_string_lossy().into_owned()
            } else {
                files
                    .project
                    .string("context")
                    .map(str::to_owned)
                    .unwrap_or_else(|| recipe_parent.to_string_lossy().into_owned())
            };
            let label = if changed_recipe {
                "Custom context (recipe changed; default is its parent)"
            } else {
                "Custom context directory"
            };
            let context = prompt(input, output, label, Some(&default), |value| {
                validate_path(files.project.root(), value, true)
            })?;
            files.project.set("context", &context)?;
        }
    }
    if reconfigure || files.project.string("harness").is_none() {
        let default = files
            .effective_string("harness")
            .unwrap_or("omp")
            .to_owned();
        let harness = prompt(
            input,
            output,
            "Harness (omp/pi/opencode/vibe)",
            Some(&default),
            |value| value.parse::<HarnessChoice>().map(|_| ()),
        )?;
        files.project.set("harness", &harness)?;
    }
    if reconfigure || files.project.host_engine().is_none() {
        writeln!(output, "A container engine is always required to launch the sandbox. host_engine grants the agent access to the host engine with your host-user authority, beyond the sandbox boundary.").map_err(prompt_error)?;
        let default = if files.resolve(None).host_engine {
            "true"
        } else {
            "false"
        };
        let answer = prompt(
            input,
            output,
            "Grant host engine access (true/false)",
            Some(default),
            |value| match value {
                "true" | "false" => Ok(()),
                _ => Err("expected true or false".to_owned()),
            },
        )?;
        files.project.set("host_engine", &answer)?;
    }
    files.project.validate_paths(true)?;
    files.project.save()?;
    writeln!(output, "Saved {}", files.project.path.display()).map_err(prompt_error)?;
    Ok(())
}

fn validate_path(root: &Path, value: &str, directory: bool) -> Result<(), String> {
    if value.is_empty() {
        return Err("path must not be empty".to_owned());
    }
    let path = root.join(value);
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    } {
        Ok(())
    } else {
        Err(format!(
            "{} must be a {}",
            path.display(),
            if directory { "directory" } else { "file" }
        ))
    }
}

fn prompt(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    default: Option<&str>,
    validate: impl Fn(&str) -> Result<(), String>,
) -> Result<String, String> {
    loop {
        write!(
            output,
            "{label}{}> ",
            default
                .map(|value| format!(" [{value}] "))
                .unwrap_or_default()
        )
        .map_err(prompt_error)?;
        output.flush().map_err(prompt_error)?;
        let mut line = String::new();
        let count = input.read_line(&mut line).map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                cancelled()
            } else {
                prompt_error(error)
            }
        })?;
        let answer = line.trim();
        if count == 0 || answer == "q" || answer.contains('\u{3}') {
            return Err(cancelled());
        }
        let answer = if answer.is_empty() {
            default.unwrap_or("")
        } else {
            answer
        };
        match validate(answer) {
            Ok(()) => return Ok(answer.to_owned()),
            Err(error) => writeln!(output, "Invalid input: {error}").map_err(prompt_error)?,
        }
    }
}

fn cancelled() -> String {
    "setup cancelled; no configuration was saved; run playpen setup to try again".to_owned()
}
fn prompt_error(error: io::Error) -> String {
    format!("could not complete setup: {error}; run playpen setup to try again")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Host) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = home.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let host = Host {
            uid: 1000,
            gid: 1000,
            pid: 123,
            home,
            workspace,
            config_home: None,
            data_home: None,
            runtime_dir: root.path().to_owned(),
            engine_pref: None,
            registry_pref: None,
            path: OsString::new(),
        };
        (root, host)
    }

    #[test]
    fn cancellation_never_writes_partial_choices() {
        for answers in ["jvm\nq\n", "jvm\n", "jvm\nomp\n\u{3}\n"] {
            let (_root, host) = fixture();
            let path = host.workspace.join(".playpen/playpen.toml");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = "# keep exactly\nhost_engine = true # grant\n";
            fs::write(&path, original).unwrap();
            assert!(run_with(&host, true, &mut answers.as_bytes(), &mut Vec::new()).is_err());
            assert_eq!(fs::read_to_string(path).unwrap(), original);
        }
        let (_root, host) = fixture();
        assert!(run_with(&host, false, &mut "rust\nq\n".as_bytes(), &mut Vec::new()).is_err());
        assert!(!host.workspace.join(".playpen").exists());
    }

    #[test]
    fn inherited_defaults_are_explicitly_saved_locally() {
        let (_root, host) = fixture();
        let global = host.home.join(".config/playpen/playpen.toml");
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(
            global,
            "image = 'jvm'\nharness = 'pi'\nhost_engine = true\n",
        )
        .unwrap();
        assert!(!crate::config::project_ready(&host).unwrap());
        run_with(&host, false, &mut "\n\n\n".as_bytes(), &mut Vec::new()).unwrap();
        assert!(crate::config::project_ready(&host).unwrap());
        let selected = crate::config::selection(&host).unwrap();
        assert_eq!(selected.image, Some(Image::Jvm));
        assert_eq!(selected.harness, Some(HarnessChoice::Pi));
    }

    #[test]
    fn normal_completion_only_prompts_missing_choices_and_reprompts_invalid() {
        let (_root, host) = fixture();
        let path = host.workspace.join(".playpen/playpen.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# retained\nimage = 'rust'\nhost_engine = false\n").unwrap();
        run_with(
            &host,
            false,
            &mut "invalid\nvibe\n".as_bytes(),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(crate::config::project_ready(&host).unwrap());
        assert_eq!(
            crate::config::selection(&host).unwrap().harness,
            Some(HarnessChoice::Vibe)
        );
        assert!(fs::read_to_string(path).unwrap().contains("# retained"));
    }

    #[test]
    fn custom_context_defaults_to_new_recipe_parent() {
        let (_root, host) = fixture();
        fs::create_dir(host.workspace.join("recipe")).unwrap();
        fs::write(
            host.workspace.join("recipe/Containerfile"),
            "FROM scratch\n",
        )
        .unwrap();
        run_with(
            &host,
            false,
            &mut "custom\nrecipe/Containerfile\n\nomp\nfalse\n".as_bytes(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            crate::config::selection(&host).unwrap().context,
            Some(host.workspace.join("recipe"))
        );
    }
}
