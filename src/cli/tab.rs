use std::collections::HashMap;

use crate::api::schema::{TabCreateParams, TabListParams, TabRenameParams};

use super::matches::{flag, required, string, values, words};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    List { workspace: Option<String> },
    Create(CreateArgs),
    Get { tab_id: String },
    Focus { tab_id: String },
    Rename { tab_id: String, label: String },
    Close { tab_id: String },
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateArgs {
    workspace: Option<String>,
    cwd: Option<String>,
    focus: bool,
    label: Option<String>,
    env: HashMap<String, String>,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Create(_) => "create",
            Self::Get { .. } => "get",
            Self::Focus { .. } => "focus",
            Self::Rename { .. } => "rename",
            Self::Close { .. } => "close",
            Self::Invalid => "",
        }
    }

    pub(super) fn is_api_command(&self) -> bool {
        !matches!(self, Self::Invalid)
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Command {
    match matches.subcommand() {
        Some(("list", command)) => Command::List {
            workspace: string(command, "workspace"),
        },
        Some(("create", command)) => Command::Create(CreateArgs {
            workspace: string(command, "workspace"),
            cwd: string(command, "cwd"),
            focus: flag(command, "focus"),
            label: string(command, "label"),
            env: values::<(String, String)>(command, "env")
                .into_iter()
                .collect(),
        }),
        Some(("get", command)) => Command::Get {
            tab_id: required(command, "tab_id"),
        },
        Some(("focus", command)) => Command::Focus {
            tab_id: required(command, "tab_id"),
        },
        Some(("rename", command)) => Command::Rename {
            tab_id: required(command, "tab_id"),
            label: words(command, "label"),
        },
        Some(("close", command)) => Command::Close {
            tab_id: required(command, "tab_id"),
        },
        _ => Command::Invalid,
    }
}

pub(super) fn run_tab_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::List { workspace } => super::runtime::tab_list(
            paths,
            TabListParams {
                workspace_id: workspace,
            },
        ),
        Command::Create(args) => match create_params(args, paths) {
            Ok(params) => super::runtime::tab_create(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Get { tab_id } => super::runtime::tab_get(paths, tab_id),
        Command::Focus { tab_id } => super::runtime::tab_focus(paths, tab_id),
        Command::Rename { tab_id, label } => {
            super::runtime::tab_rename(paths, TabRenameParams { tab_id, label })
        }
        Command::Close { tab_id } => super::runtime::tab_close(paths, tab_id),
        Command::Invalid => Ok(super::missing_subcommand()),
    }
}

fn create_params(
    args: CreateArgs,
    paths: &super::target::CliContext,
) -> Result<TabCreateParams, String> {
    let cwd = args
        .cwd
        .map(|raw| {
            super::matches::resolve_cwd(
                &raw,
                paths.is_remote(),
                paths.home_dir(),
                paths.current_dir(),
            )
        })
        .transpose()?;
    Ok(TabCreateParams {
        workspace_id: args.workspace,
        cwd,
        focus: args.focus,
        label: args.label,
        env: args.env,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::group_matches;

    fn create_args(args: &[&str]) -> super::CreateArgs {
        let super::Command::Create(args) = super::parse(&group_matches(args)) else {
            panic!("expected tab create");
        };
        args
    }

    fn test_paths() -> super::super::target::CliContext {
        super::super::target::CliContext::test_local(shepr_config::AppPaths::test_with_context(
            std::path::Path::new("/tmp/shepr-cli-paths"),
            Some(std::path::Path::new("/home/me")),
            Some(std::path::Path::new("/home/me/proj")),
        ))
    }

    #[test]
    fn create_reads_every_option() {
        let params = super::create_params(
            create_args(&[
                "tab",
                "create",
                "--workspace=w2",
                "--cwd",
                "/srv",
                "--label",
                "logs",
                "--env",
                "A=1",
                "--no-focus",
                "--focus",
            ]),
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.workspace_id.as_deref(), Some("w2"));
        assert_eq!(params.cwd.as_deref(), Some("/srv"));
        assert_eq!(params.label.as_deref(), Some("logs"));
        assert!(params.focus);
        assert_eq!(params.env.get("A").map(String::as_str), Some("1"));

        // A relative directory is the caller's, not the server's.
        let params =
            super::create_params(create_args(&["tab", "create", "--cwd=."]), &test_paths())
                .expect("test precondition");
        assert_eq!(params.cwd.as_deref(), Some("/home/me/proj"));
    }
}
