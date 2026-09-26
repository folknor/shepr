use std::collections::HashMap;

use clap::ArgMatches;

use crate::api::schema::{TabCreateParams, TabListParams, TabRenameParams};

use super::matches::{flag, required, string, values, words};

pub(super) fn run_tab_command(matches: &ArgMatches) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("list", matches)) => super::runtime::tab_list(TabListParams {
            workspace_id: string(matches, "workspace"),
        }),
        Some(("create", matches)) => super::runtime::tab_create(create_params(matches)),
        Some(("get", matches)) => super::runtime::tab_get(required(matches, "tab_id")),
        Some(("focus", matches)) => super::runtime::tab_focus(required(matches, "tab_id")),
        Some(("rename", matches)) => super::runtime::tab_rename(TabRenameParams {
            tab_id: required(matches, "tab_id"),
            label: words(matches, "label"),
        }),
        Some(("close", matches)) => super::runtime::tab_close(required(matches, "tab_id")),
        _ => Ok(super::missing_subcommand()),
    }
}

fn create_params(matches: &ArgMatches) -> TabCreateParams {
    TabCreateParams {
        workspace_id: string(matches, "workspace"),
        cwd: string(matches, "cwd"),
        focus: flag(matches, "focus"),
        label: string(matches, "label"),
        env: values::<(String, String)>(matches, "env")
            .into_iter()
            .collect::<HashMap<_, _>>(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::command_matches;

    #[test]
    fn create_reads_every_option() {
        let params = super::create_params(&command_matches(&[
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
        ]));
        assert_eq!(params.workspace_id.as_deref(), Some("w2"));
        assert_eq!(params.cwd.as_deref(), Some("/srv"));
        assert_eq!(params.label.as_deref(), Some("logs"));
        assert!(params.focus);
        assert_eq!(params.env.get("A").map(String::as_str), Some("1"));
    }
}
