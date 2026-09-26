use std::collections::HashMap;

use clap::ArgMatches;

use crate::api::schema::{
    Method, WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceRenameParams,
    WorkspaceReportMetadataParams,
};

use super::matches::{flag, required, string, value, values, words};

pub(super) fn run_workspace_command(
    matches: &ArgMatches,
    paths: &crate::config::AppPaths,
) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("list", _)) => super::runtime::workspace_list(paths),
        Some(("create", matches)) => match create_params(matches, paths) {
            Ok(params) => super::runtime::workspace_create(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Some(("get", matches)) => {
            super::runtime::workspace_get(paths, required(matches, "workspace_id"))
        }
        Some(("focus", matches)) => {
            super::runtime::workspace_focus(paths, required(matches, "workspace_id"))
        }
        Some(("rename", matches)) => super::runtime::workspace_rename(
            paths,
            WorkspaceRenameParams {
                workspace_id: required(matches, "workspace_id"),
                label: words(matches, "label"),
            },
        ),
        Some(("report-metadata", matches)) => match report_metadata_params(matches) {
            Ok(params) => super::send_ok_request(paths, Method::WorkspaceReportMetadata(params)),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Some(("close", matches)) => super::runtime::workspace_close(
            paths,
            WorkspaceCloseParams {
                workspace_id: required(matches, "workspace_id"),
            },
        ),
        _ => Ok(super::missing_subcommand()),
    }
}

fn create_params(
    matches: &ArgMatches,
    paths: &crate::config::AppPaths,
) -> Result<WorkspaceCreateParams, String> {
    Ok(WorkspaceCreateParams {
        source_workspace_id: None,
        cwd: super::matches::cwd(matches, paths)?,
        focus: flag(matches, "focus"),
        label: string(matches, "label"),
        env: values::<(String, String)>(matches, "env")
            .into_iter()
            .collect::<HashMap<_, _>>(),
    })
}

fn report_metadata_params(matches: &ArgMatches) -> Result<WorkspaceReportMetadataParams, String> {
    let Some(source) = string(matches, "source").filter(|source| !source.trim().is_empty()) else {
        return Err("missing required --source".into());
    };
    Ok(WorkspaceReportMetadataParams {
        workspace_id: required(matches, "workspace_id"),
        source,
        tokens: super::matches::metadata_tokens(matches),
        seq: value::<u64>(matches, "seq"),
        ttl_ms: value::<u64>(matches, "ttl-ms"),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::command_matches;

    fn test_paths() -> crate::config::AppPaths {
        crate::config::AppPaths::test_with_context(
            std::path::Path::new("/tmp/shepr-cli-paths"),
            Some(std::path::Path::new("/home/me")),
            Some(std::path::Path::new("/home/me/proj")),
        )
    }

    #[test]
    fn create_reads_every_option() {
        let params = super::create_params(
            &command_matches(&[
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
            &command_matches(&["workspace", "create", "--focus", "--no-focus"]),
            &test_paths(),
        )
        .expect("test precondition");
        assert!(!params.focus);
        assert_eq!(params.cwd, None);

        // A relative directory is the caller's, not the server's.
        let params = super::create_params(
            &command_matches(&["workspace", "create", "--cwd", "."]),
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.cwd.as_deref(), Some("/home/me/proj"));
    }

    #[test]
    fn report_metadata_requires_a_token_and_a_nonblank_source() {
        let params = super::report_metadata_params(&command_matches(&[
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
            super::report_metadata_params(&command_matches(&[
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
        let rename = command_matches(&["workspace", "rename", "w1", "my", "-dev", "box"]);
        assert_eq!(super::words(&rename, "label"), "my -dev box");
    }
}
