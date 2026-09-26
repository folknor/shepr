use std::collections::HashMap;

use clap::ArgMatches;

use crate::api::schema::{TabCreateParams, TabListParams, TabRenameParams};

use super::matches::{flag, required, string, values, words};

pub(super) fn run_tab_command(
    matches: &ArgMatches,
    paths: &super::target::CliContext,
) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("list", matches)) => super::runtime::tab_list(
            paths,
            TabListParams {
                workspace_id: string(matches, "workspace"),
            },
        ),
        Some(("create", matches)) => match create_params(matches, paths) {
            Ok(params) => super::runtime::tab_create(paths, params),
            Err(message) => Ok(super::usage_error(&message)),
        },
        Some(("get", matches)) => super::runtime::tab_get(paths, required(matches, "tab_id")),
        Some(("focus", matches)) => super::runtime::tab_focus(paths, required(matches, "tab_id")),
        Some(("rename", matches)) => super::runtime::tab_rename(
            paths,
            TabRenameParams {
                tab_id: required(matches, "tab_id"),
                label: words(matches, "label"),
            },
        ),
        Some(("close", matches)) => super::runtime::tab_close(paths, required(matches, "tab_id")),
        _ => Ok(super::missing_subcommand()),
    }
}

fn create_params(
    matches: &ArgMatches,
    paths: &super::target::CliContext,
) -> Result<TabCreateParams, String> {
    Ok(TabCreateParams {
        workspace_id: string(matches, "workspace"),
        cwd: super::matches::cwd(matches, paths)?,
        focus: flag(matches, "focus"),
        label: string(matches, "label"),
        env: values::<(String, String)>(matches, "env")
            .into_iter()
            .collect::<HashMap<_, _>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::command_matches;

    fn test_paths() -> super::super::target::CliContext {
        super::super::target::CliContext::test_local(crate::config::AppPaths::test_with_context(
            std::path::Path::new("/tmp/shepr-cli-paths"),
            Some(std::path::Path::new("/home/me")),
            Some(std::path::Path::new("/home/me/proj")),
        ))
    }

    #[test]
    fn create_reads_every_option() {
        let params = super::create_params(
            &command_matches(&[
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
        let params = super::create_params(
            &command_matches(&["tab", "create", "--cwd=."]),
            &test_paths(),
        )
        .expect("test precondition");
        assert_eq!(params.cwd.as_deref(), Some("/home/me/proj"));
    }
}
