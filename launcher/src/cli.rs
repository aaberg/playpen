//! CLI parsing. The run flags are the phase-1 spec, ported from
//! `bin/playpen`; messages marked "spec" keep the bash launcher's exact
//! wording. Phase 2 adds the `--harness` flag and the `harness` subcommand.
//! Everything downstream receives a fully resolved [`Action`].

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::exit;

use clap::{Parser, Subcommand};

use crate::host::Host;

#[derive(Parser, Debug)]
#[command(
    name = "playpen",
    version,
    about = "Run an AI coding agent inside a sandboxed container",
    long_about = "Run an AI coding agent (harness omp by default) inside a rootless container.\n\
                  The workspace is mounted at /workspace; the agent gets a throwaway\n\
                  home that persists on the host.",
    after_help = "Environment:\n  \
        PLAYPEN_ENGINE          force the container engine: podman or docker\n  \
        PLAYPEN_HOST_ENGINE     same as --host-engine when set to 1, true or yes\n\n\
        Keys file (override with --keys FILE):\n  \
        ~/.config/playpen/keys.conf - one NAME=value per line; a value is a literal\n  \
        or an op:// reference resolved with the 1Password CLI at launch.\n\n\
        Harness selection (override with --harness NAME):\n  \
        ~/.config/playpen/harness.conf - set by `playpen harness`\n\n\
        Bare playpen-* images get a thin harness layer (playpen-jvm ->\n  \
        playpen-jvm:omp), built on demand; tagged or foreign images run as-is.\n\n\
        Examples:\n  \
        playpen playpen-rust                     run omp in the Rust flavor\n  \
        playpen --shell playpen-jvm              bash in the JVM flavor\n  \
        playpen playpen-jvm -- omp --version     one-off command\n  \
        playpen --host-engine playpen-rust       grant host engine access\n  \
        playpen harness                          pick the default harness\n  \
        playpen harness update                   refresh installed harnesses"
)]
pub struct Cli {
    /// Mount the host container engine socket (trusted projects only)
    #[arg(long)]
    pub host_engine: bool,

    /// API keys file [default: ~/.config/playpen/keys.conf]
    #[arg(long, value_name = "FILE")]
    pub keys: Option<PathBuf>,

    /// Start /bin/bash instead of the agent harness
    #[arg(long)]
    pub shell: bool,

    /// Run this harness instead of the selected one
    #[arg(long, value_name = "NAME")]
    pub harness: Option<String>,

    /// Container image to run
    #[arg(value_name = "IMAGE", default_value = "playpen-base")]
    pub image: String,

    /// Command passed verbatim to the container (everything after --)
    #[arg(value_name = "CMD", last = true, num_args = 0..)]
    pub command: Vec<OsString>,

    /// Subcommands; none means a plain run
    #[command(subcommand)]
    pub sub: Option<Sub>,
}

/// The launcher's subcommands.
#[derive(Subcommand, Debug)]
pub enum Sub {
    /// Pick the default harness (stored in ~/.config/playpen/harness.conf)
    Harness {
        /// Rebuild installed harness images with fresh harness versions
        #[command(subcommand)]
        cmd: Option<HarnessSub>,
    },
}

#[derive(Subcommand, Debug)]
pub enum HarnessSub {
    /// Rebuild existing playpen*:<harness> images, cache-busted
    Update {
        /// Harness to update [default: the selected one]
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
}

/// What the user asked for, fully resolved.
#[derive(Debug)]
pub enum Action {
    /// A container run.
    Run(Config),
    /// `playpen harness`: the selection menu.
    HarnessMenu,
    /// `playpen harness update [NAME]`.
    HarnessUpdate { name: Option<String> },
}

/// The resolved launch configuration. The container command is resolved in
/// main, where the harness (and its exec name) is known.
#[derive(Debug)]
pub struct Config {
    pub host_engine: bool,
    pub keys_file: PathBuf,
    pub keys_explicit: bool,
    /// The `--harness` flag, when given.
    pub harness: Option<String>,
    pub shell: bool,
    pub image: String,
    /// Non-empty only with an explicit `-- CMD`.
    pub command: Vec<OsString>,
}

/// Parse argv, exiting on error or `-h`/`--help`/`--version`.
pub fn parse(host: &Host) -> Action {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let argv = match normalize(&argv) {
        Ok(argv) => argv,
        Err(message) => {
            eprintln!("playpen: {message}");
            exit(1);
        }
    };
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            exit(if err.use_stderr() { 1 } else { 0 });
        }
    };
    resolve(cli, host)
}

/// Rewrite argv the way the bash launcher consumed it, before clap sees it:
///
/// - a `--keys` token grabs the next token sight-unseen as its value (even
///   when that token is `--`), rewritten to `--keys=VALUE` so clap agrees;
///   nothing left to grab is the spec error
/// - the first `--` not swallowed as a keys value is the command separator:
///   it must be followed by a command, and everything after it is passed
///   through verbatim - a trailing `--keys` there belongs to the container
///
/// Err carries the spec error message (without the "playpen: " prefix).
fn normalize(argv: &[OsString]) -> Result<Vec<OsString>, &'static str> {
    let mut out = Vec::with_capacity(argv.len() + 4);
    out.push(argv.first().cloned().unwrap_or_default());
    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == OsStr::new("--keys") {
            let value = match argv.get(i + 1) {
                Some(value) => value,
                None => return Err("--keys needs a file argument"),
            };
            let mut joined = OsString::from("--keys=");
            joined.push(value);
            out.push(joined);
            i += 2;
        } else if arg == OsStr::new("--") {
            if i == argv.len() - 1 {
                return Err("-- needs a command");
            }
            out.push(arg.clone());
            out.extend(argv[i + 1..].iter().cloned());
            break;
        } else {
            out.push(arg.clone());
            i += 1;
        }
    }
    Ok(out)
}

fn resolve(cli: Cli, host: &Host) -> Action {
    if let Some(Sub::Harness { cmd }) = cli.sub {
        return match cmd {
            None => Action::HarnessMenu,
            Some(HarnessSub::Update { name }) => Action::HarnessUpdate { name },
        };
    }
    if cli.shell && !cli.command.is_empty() {
        // spec
        eprintln!("playpen: --shell cannot be combined with a command after --");
        exit(1);
    }
    let (keys_file, keys_explicit) = match cli.keys {
        Some(file) => (file, true),
        None => (host.default_keys_file(), false),
    };
    Action::Run(Config {
        host_engine: cli.host_engine || host.host_engine_env,
        keys_file,
        keys_explicit,
        harness: cli.harness,
        shell: cli.shell,
        image: cli.image,
        command: cli.command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn normalize_strs(args: &[&str]) -> Result<Vec<String>, &'static str> {
        let argv: Vec<OsString> = std::iter::once("playpen")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect();
        normalize(&argv).map(|v| {
            v.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        })
    }

    #[test]
    fn normalize_passes_plain_args_through() {
        assert_eq!(
            normalize_strs(&["--shell", "playpen-jvm"]),
            Ok(vec![
                "playpen".to_string(),
                "--shell".to_string(),
                "playpen-jvm".to_string()
            ])
        );
    }

    #[test]
    fn normalize_joins_keys_with_its_value() {
        assert_eq!(
            normalize_strs(&["--keys", "/tmp/k", "--", "omp", "--version"]),
            Ok(vec![
                "playpen".to_string(),
                "--keys=/tmp/k".to_string(),
                "--".to_string(),
                "omp".to_string(),
                "--version".to_string()
            ])
        );
        // repeated flags: bash overwrites KEYS_FILE, clap keeps the last
        assert_eq!(
            normalize_strs(&["--keys", "a", "--keys", "b"]),
            Ok(vec![
                "playpen".to_string(),
                "--keys=a".to_string(),
                "--keys=b".to_string()
            ])
        );
    }

    #[test]
    fn normalize_keys_grabs_the_separator_as_its_value() {
        // bash: --keys consumes the next token sight-unseen, even `--`
        assert_eq!(
            normalize_strs(&["--keys", "--", "x"]),
            Ok(vec!["playpen".to_string(), "--keys=--".to_string(), "x".to_string()])
        );
        // ... so a trailing bare `--` after a keys value is still an error
        assert_eq!(
            normalize_strs(&["--keys", "x", "--"]),
            Err("-- needs a command")
        );
    }

    #[test]
    fn normalize_keeps_post_separator_tokens_verbatim() {
        // the reported bug: trailing --keys after -- belongs to the command
        assert_eq!(
            normalize_strs(&["--", "echo", "--keys"]),
            Ok(vec![
                "playpen".to_string(),
                "--".to_string(),
                "echo".to_string(),
                "--keys".to_string()
            ])
        );
        // a second -- inside the command stays verbatim
        assert_eq!(
            normalize_strs(&["--", "a", "--", "b"]),
            Ok(vec![
                "playpen".to_string(),
                "--".to_string(),
                "a".to_string(),
                "--".to_string(),
                "b".to_string()
            ])
        );
    }

    #[test]
    fn normalize_spec_errors() {
        assert_eq!(normalize_strs(&["--keys"]), Err("--keys needs a file argument"));
        assert_eq!(normalize_strs(&["--"]), Err("-- needs a command"));
        assert_eq!(
            normalize_strs(&["--keys", "a", "--keys"]),
            Err("--keys needs a file argument")
        );
    }

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
            path: Default::default(),
        }
    }

    fn cli(shell: bool, image: &str, command: Vec<OsString>) -> Cli {
        Cli {
            host_engine: false,
            keys: None,
            shell,
            harness: None,
            image: image.to_string(),
            command,
            sub: None,
        }
    }

    #[test]
    fn subcommands_beat_the_image_positional() {
        let c = Cli {
            sub: Some(Sub::Harness { cmd: None }),
            ..cli(false, "playpen-base", vec![])
        };
        assert!(matches!(resolve(c, &host()), Action::HarnessMenu));

        let c = Cli {
            sub: Some(Sub::Harness {
                cmd: Some(HarnessSub::Update { name: None }),
            }),
            ..cli(false, "playpen-base", vec![])
        };
        assert!(matches!(resolve(c, &host()), Action::HarnessUpdate { .. }));

        let c = Cli {
            sub: Some(Sub::Harness {
                cmd: Some(HarnessSub::Update {
                    name: Some("omp".to_string()),
                }),
            }),
            ..cli(false, "playpen-base", vec![])
        };
        match resolve(c, &host()) {
            Action::HarnessUpdate { name } => assert_eq!(name.as_deref(), Some("omp")),
            _ => panic!("expected HarnessUpdate"),
        }

        assert!(matches!(
            resolve(cli(false, "playpen-base", vec![]), &host()),
            Action::Run(_)
        ));
    }

    #[test]
    fn run_config_carries_flags_and_command_verbatim() {
        let c = cli(true, "playpen-jvm", vec![]);
        let Action::Run(cfg) = resolve(c, &host()) else {
            panic!("expected run");
        };
        assert!(cfg.shell);
        assert_eq!(cfg.image, "playpen-jvm");
        assert!(cfg.command.is_empty(), "no explicit command");

        let c = cli(false, "playpen-jvm", vec!["omp".into(), "--version".into()]);
        let Action::Run(cfg) = resolve(c, &host()) else {
            panic!("expected run");
        };
        assert!(!cfg.shell);
        assert_eq!(
            cfg.command,
            vec![OsString::from("omp"), OsString::from("--version")]
        );
    }

    #[test]
    fn harness_flag_is_carried_unresolved() {
        let c = Cli {
            harness: Some("vibe".to_string()),
            ..cli(false, "playpen-base", vec![])
        };
        let Action::Run(cfg) = resolve(c, &host()) else {
            panic!("expected run");
        };
        assert_eq!(cfg.harness.as_deref(), Some("vibe"));
    }

    #[test]
    fn keys_explicit_flag() {
        let c = Cli {
            keys: Some(PathBuf::from("/tmp/mykeys")),
            ..cli(false, "playpen-base", vec![])
        };
        let Action::Run(cfg) = resolve(c, &host()) else {
            panic!("expected run");
        };
        assert_eq!(cfg.keys_file, PathBuf::from("/tmp/mykeys"));
        assert!(cfg.keys_explicit);

        let Action::Run(cfg) = resolve(cli(false, "playpen-base", vec![]), &host()) else {
            panic!("expected run");
        };
        assert_eq!(
            cfg.keys_file,
            PathBuf::from("/home/tester/.config/playpen/keys.conf")
        );
        assert!(!cfg.keys_explicit);
    }

    #[test]
    fn host_engine_env_is_honored() {
        let mut h = host();
        h.host_engine_env = true;
        let Action::Run(cfg) = resolve(cli(false, "playpen-base", vec![]), &h) else {
            panic!("expected run");
        };
        assert!(cfg.host_engine);

        let mut h = host();
        h.host_engine_env = false;
        let c = Cli {
            host_engine: true,
            ..cli(false, "playpen-base", vec![])
        };
        let Action::Run(cfg) = resolve(c, &h) else {
            panic!("expected run");
        };
        assert!(cfg.host_engine);
        let Action::Run(cfg) = resolve(cli(false, "playpen-base", vec![]), &h) else {
            panic!("expected run");
        };
        assert!(!cfg.host_engine);
    }
}
