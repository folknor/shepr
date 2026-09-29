use std::collections::HashMap;

use shepr_api::schema::{WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceRenameParams};

use super::matches::{flag, required, string, values, words};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    List,
    Create(CreateArgs),
    Get { workspace_id: String },
    Focus { workspace_id: String },
    Rename { workspace_id: String, label: String },
    Close { workspace_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateArgs {
    cwd: Option<String>,
    focus: bool,
    label: Option<String>,
    env: HashMap<String, String>,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Create(_) => "create",
            Self::Get { .. } => "get",
            Self::Focus { .. } => "focus",
            Self::Rename { .. } => "rename",
            Self::Close { .. } => "close",
        }
    }

    pub(super) fn can_run_on_machine(&self) -> bool {
        match self {
            Self::List
            | Self::Create(_)
            | Self::Get { .. }
            | Self::Focus { .. }
            | Self::Rename { .. }
            | Self::Close { .. } => true,
        }
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("list", _)) => Some(Command::List),
        Some(("create", command)) => Some(Command::Create(CreateArgs {
            cwd: string(command, "cwd"),
            focus: flag(command, "focus"),
            label: string(command, "label"),
            env: values::<(String, String)>(command, "env")
                .into_iter()
                .collect(),
        })),
        Some(("get", command)) => Some(Command::Get {
            workspace_id: required(command, "workspace_id")?,
        }),
        Some(("focus", command)) => Some(Command::Focus {
            workspace_id: required(command, "workspace_id")?,
        }),
        Some(("rename", command)) => Some(Command::Rename {
            workspace_id: required(command, "workspace_id")?,
            label: words(command, "label"),
        }),
        Some(("close", command)) => Some(Command::Close {
            workspace_id: required(command, "workspace_id")?,
        }),
        _ => None,
    }
}

pub(super) fn run_workspace_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::List => super::runtime::workspace_list(paths),
        Command::Create(args) => match create_params(args, paths) {
            Ok(params) => super::runtime::workspace_create(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Get { workspace_id } => super::runtime::workspace_get(paths, workspace_id),
        Command::Focus { workspace_id } => super::runtime::workspace_focus(paths, workspace_id),
        Command::Rename {
            workspace_id,
            label,
        } => super::runtime::workspace_rename(
            paths,
            WorkspaceRenameParams {
                workspace_id,
                label,
            },
        ),
        Command::Close { workspace_id } => {
            super::runtime::workspace_close(paths, WorkspaceCloseParams { workspace_id })
        }
    }
}

fn create_params(
    args: CreateArgs,
    paths: &super::target::CliContext,
) -> Result<WorkspaceCreateParams, String> {
    Ok(WorkspaceCreateParams {
        source_workspace_id: None,
        cwd: resolve_cwd(args.cwd, paths)?,
        focus: args.focus,
        label: args.label,
        env: args.env,
    })
}

fn resolve_cwd(
    cwd: Option<String>,
    paths: &super::target::CliContext,
) -> Result<Option<String>, String> {
    let Some(raw) = cwd else {
        return Ok(None);
    };
    super::matches::resolve_cwd(
        &raw,
        paths.is_remote(),
        paths.home_dir(),
        paths.current_dir(),
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::super::tests::group_matches;

    fn command(args: &[&str]) -> super::Command {
        super::parse(&group_matches(args)).expect("test precondition")
    }

    fn create_args(args: &[&str]) -> super::CreateArgs {
        let super::Command::Create(args) = command(args) else {
            panic!("expected workspace create");
        };
        args
    }

    fn test_paths() -> super::super::target::CliContext {
        super::super::target::CliContext::test_local(shepr_config::AppPaths::rooted_at(
            std::path::Path::new("/tmp/shepr-cli-paths"),
            Some(std::path::Path::new("/home/me")),
            Some(std::path::Path::new("/home/me/proj")),
        ))
    }

    #[test]
    fn create_reads_every_option() {
        let params = super::create_params(
            create_args(&[
                "workspace",
                "create",
                "--cwd",
                "/srv",
                "--label=api",
                "--env",
                "A=1",
                "--env=B=",
                "--focus",
            ]),
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.cwd.as_deref(), Some("/srv"));
        assert_eq!(params.label.as_deref(), Some("api"));
        assert!(params.focus);
        assert_eq!(params.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(params.env.get("B").map(String::as_str), Some(""));

        let params = super::create_params(
            create_args(&["workspace", "create", "--focus", "--no-focus"]),
            &test_paths(),
        )
        .expect("test precondition");
        assert!(!params.focus);
        assert_eq!(params.cwd, None);

        // A relative directory is the caller's, not the server's.
        let params = super::create_params(
            create_args(&["workspace", "create", "--cwd", "."]),
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.cwd.as_deref(), Some("/home/me/proj"));
    }

    #[test]
    fn rename_joins_label_words() {
        let super::Command::Rename { label, .. } =
            command(&["workspace", "rename", "w1", "my", "-dev", "box"])
        else {
            panic!("expected workspace rename");
        };
        assert_eq!(label, "my -dev box");
    }
}
