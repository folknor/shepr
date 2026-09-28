use std::collections::HashMap;

use shepr_api::schema::{
    Method, WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceRenameParams,
    WorkspaceReportMetadataParams,
};

use super::matches::{flag, required, string, value, values, words};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    List,
    Create(CreateArgs),
    Get { workspace_id: String },
    Focus { workspace_id: String },
    Rename { workspace_id: String, label: String },
    ReportMetadata(ReportMetadataArgs),
    Close { workspace_id: String },
    Invalid,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreateArgs {
    cwd: Option<String>,
    focus: bool,
    label: Option<String>,
    env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReportMetadataArgs {
    workspace_id: String,
    source: String,
    tokens: HashMap<String, Option<String>>,
    seq: Option<u64>,
    ttl_ms: Option<u64>,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Create(_) => "create",
            Self::Get { .. } => "get",
            Self::Focus { .. } => "focus",
            Self::Rename { .. } => "rename",
            Self::ReportMetadata(_) => "report-metadata",
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
        Some(("list", _)) => Command::List,
        Some(("create", command)) => Command::Create(CreateArgs {
            cwd: string(command, "cwd"),
            focus: flag(command, "focus"),
            label: string(command, "label"),
            env: values::<(String, String)>(command, "env")
                .into_iter()
                .collect(),
        }),
        Some(("get", command)) => Command::Get {
            workspace_id: required(command, "workspace_id"),
        },
        Some(("focus", command)) => Command::Focus {
            workspace_id: required(command, "workspace_id"),
        },
        Some(("rename", command)) => Command::Rename {
            workspace_id: required(command, "workspace_id"),
            label: words(command, "label"),
        },
        Some(("report-metadata", command)) => Command::ReportMetadata(ReportMetadataArgs {
            workspace_id: required(command, "workspace_id"),
            source: required(command, "source"),
            tokens: super::matches::metadata_tokens(command),
            seq: value(command, "seq"),
            ttl_ms: value(command, "ttl-ms"),
        }),
        Some(("close", command)) => Command::Close {
            workspace_id: required(command, "workspace_id"),
        },
        _ => Command::Invalid,
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
        Command::ReportMetadata(args) => match report_metadata_params(args) {
            Ok(params) => super::send_ok_request(paths, Method::WorkspaceReportMetadata(params)),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Command::Close { workspace_id } => {
            super::runtime::workspace_close(paths, WorkspaceCloseParams { workspace_id })
        }
        Command::Invalid => Ok(super::missing_subcommand()),
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

fn report_metadata_params(
    args: ReportMetadataArgs,
) -> Result<WorkspaceReportMetadataParams, String> {
    let source = args.source;
    if source.trim().is_empty() {
        return Err("missing required --source".into());
    }
    Ok(WorkspaceReportMetadataParams {
        workspace_id: args.workspace_id,
        source,
        tokens: args.tokens,
        seq: args.seq,
        ttl_ms: args.ttl_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::group_matches;

    fn command(args: &[&str]) -> super::Command {
        super::parse(&group_matches(args))
    }

    fn create_args(args: &[&str]) -> super::CreateArgs {
        let super::Command::Create(args) = command(args) else {
            panic!("expected workspace create");
        };
        args
    }

    fn report_args(args: &[&str]) -> super::ReportMetadataArgs {
        let super::Command::ReportMetadata(args) = command(args) else {
            panic!("expected workspace report-metadata");
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
    fn report_metadata_requires_a_token_and_a_nonblank_source() {
        let params = super::report_metadata_params(report_args(&[
            "workspace",
            "report-metadata",
            "w1",
            "--source=git",
            "--token",
            "branch=main",
            "--seq",
            "4",
            "--ttl-ms=100",
        ]))
        .expect("test precondition");
        assert_eq!(params.workspace_id, "w1");
        assert_eq!(params.seq, Some(4));
        assert_eq!(params.ttl_ms, Some(100));
        assert_eq!(params.tokens.get("branch"), Some(&Some("main".to_string())));

        assert!(
            super::report_metadata_params(report_args(&[
                "workspace",
                "report-metadata",
                "w1",
                "--source",
                " ",
                "--clear-token",
                "branch",
            ]))
            .is_err()
        );
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
