//! Pure assembly of the `engine run ...` argv, in the exact order the bash
//! launcher used. The stub-engine tests assert against this output.

use std::ffi::OsString;
use std::path::Path;

/// One launch, fully resolved. Field order mirrors the bash invocation:
/// run --rm -it | userns | --name | workspace | agent home | cargo volume |
/// host-engine | env-file | image | command.
pub struct Launch<'a> {
    pub userns: Vec<OsString>,
    pub container_name: String,
    pub workspace: &'a Path,
    pub agent_home: &'a Path,
    pub cargo_volume: bool,
    pub engine_args: Vec<OsString>,
    pub env_file: Option<&'a Path>,
    pub image: &'a str,
    pub command: &'a [OsString],
}

/// Assemble the arguments that follow the engine binary name.
pub fn build(launch: &Launch) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("-it"),
    ];
    argv.extend(launch.userns.iter().cloned());
    argv.push(OsString::from("--name"));
    argv.push(OsString::from(&launch.container_name));
    push_bind(&mut argv, launch.workspace, ":/workspace:Z");
    push_bind(&mut argv, launch.agent_home, ":/home/agent:Z");
    if launch.cargo_volume {
        argv.push(OsString::from("-v"));
        argv.push(OsString::from("playpen-cargo:/opt/agent-cargo"));
    }
    argv.extend(launch.engine_args.iter().cloned());
    if let Some(env_file) = launch.env_file {
        argv.push(OsString::from("--env-file"));
        argv.push(env_file.as_os_str().to_os_string());
    }
    argv.push(OsString::from(launch.image));
    argv.extend(launch.command.iter().cloned());
    argv
}

fn push_bind(argv: &mut Vec<OsString>, source: &Path, suffix: &str) {
    argv.push(OsString::from("-v"));
    let mut bind = source.as_os_str().to_os_string();
    bind.push(suffix);
    argv.push(bind);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn assembles_bash_order_exactly() {
        let command = os(&["omp"]);
        let userns = os(&["--userns=keep-id"]);
        let l = Launch {
            userns,
            container_name: "playpen-agent-4711".to_string(),
            workspace: Path::new("/work"),
            agent_home: Path::new("/home/agent-home"),
            cargo_volume: false,
            engine_args: vec![],
            env_file: None,
            image: "playpen-base",
            command: &command,
        };
        assert_eq!(
            build(&l),
            os(&[
                "run",
                "--rm",
                "-it",
                "--userns=keep-id",
                "--name",
                "playpen-agent-4711",
                "-v",
                "/work:/workspace:Z",
                "-v",
                "/home/agent-home:/home/agent:Z",
                "playpen-base",
                "omp",
            ])
        );
    }

    #[test]
    fn volumes_env_and_command_slots() {
        let command = os(&["/bin/bash"]);
        let userns = os(&["--userns=keep-id"]);
        let engine_args = os(&["-e", "DOCKER_HOST=unix:///run/host-engine.sock"]);
        let l = Launch {
            userns,
            container_name: "playpen-agent-4711".to_string(),
            workspace: Path::new("/work"),
            agent_home: Path::new("/home/agent-home"),
            cargo_volume: true,
            engine_args,
            env_file: Some(Path::new("/tmp/playpen-env.abc123")),
            image: "playpen-rust",
            command: &command,
        };
        assert_eq!(
            build(&l),
            os(&[
                "run",
                "--rm",
                "-it",
                "--userns=keep-id",
                "--name",
                "playpen-agent-4711",
                "-v",
                "/work:/workspace:Z",
                "-v",
                "/home/agent-home:/home/agent:Z",
                "-v",
                "playpen-cargo:/opt/agent-cargo",
                "-e",
                "DOCKER_HOST=unix:///run/host-engine.sock",
                "--env-file",
                "/tmp/playpen-env.abc123",
                "playpen-rust",
                "/bin/bash",
            ])
        );
    }
}
