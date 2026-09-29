use std::path::PathBuf;

use shepr_api::error::{ApiError, ApiErrorCode};

/// The launch cwd named by an API request (`workspace.create`, `tab.create`
/// and `pane.split`).
///
/// A relative path is refused, not resolved: the server's own working
/// directory means nothing to the caller, and the CLI already absolutises
/// `--cwd` against the caller's directory before sending. Refusing here is
/// also what keeps every pane's launch cwd absolute, which the session
/// snapshot requires: a relative saved cwd fails the snapshot's
/// deserialization, and with it the whole saved layout on the next start.
pub(super) fn launch_cwd(raw: &str) -> Result<PathBuf, ApiError> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(ApiError::new(
            ApiErrorCode::InvalidCwd,
            format!("cwd {raw:?} must be an absolute path"),
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::test_support::*;
    use shepr_api::schema::{
        ErrorResponse, PaneRightClickTarget, PaneSplitParams, SplitDirection, TabCreateParams,
        WorkspaceCreateParams,
    };
    use shepr_mux::workspace::Workspace;

    #[test]
    fn launch_cwd_accepts_absolute_and_names_a_refused_relative_path() {
        assert_eq!(
            launch_cwd("/srv/project").expect("absolute cwd"),
            PathBuf::from("/srv/project")
        );
        for raw in ["relative/dir", ".", ""] {
            let error = launch_cwd(raw).expect_err("relative cwd is refused");
            assert_eq!(error.code, ApiErrorCode::InvalidCwd);
            assert_eq!(
                error.into_message(),
                format!("cwd {raw:?} must be an absolute path")
            );
        }
    }

    fn assert_refused(response: &shepr_api::error::ApiResult, code: &str) {
        let error: ErrorResponse = crate::test_support::test_error(response);
        assert_eq!(error.error.code, code);
        assert!(
            error.error.message.contains("\".\""),
            "message names the offending cwd: {}",
            error.error.message
        );
    }

    /// `.` is relative but exists, so without the boundary check every one of
    /// these requests would launch a pane whose saved cwd is relative.
    #[tokio::test]
    async fn every_api_launch_refuses_a_relative_cwd_before_spawning() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        app.state.settings.default_shell = exiting_test_command().into();
        app.state.settings.login_shell = false;
        app.state.workspaces = vec![Workspace::test_new("relative-cwd")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.ensure_test_terminals();
        let terminal_count = app.state.terminals.len();

        let response = app.handle_workspace_create(WorkspaceCreateParams {
            source_workspace_id: None,
            cwd: Some(".".into()),
            focus: false,
            label: None,
            env: Default::default(),
        });
        assert_refused(&response, "invalid_cwd");

        let response = app.handle_tab_create(TabCreateParams {
            workspace_id: None,
            cwd: Some(".".into()),
            focus: false,
            label: None,
            env: Default::default(),
        });
        assert_refused(&response, "invalid_cwd");

        let response = app.handle_pane_split(PaneSplitParams {
            workspace_id: None,
            target_pane_id: None,
            direction: SplitDirection::Right,
            ratio: None,
            cwd: Some(".".into()),
            focus: false,
            right_click: PaneRightClickTarget::default(),
            env: Default::default(),
        });
        assert_refused(&response, "invalid_cwd");

        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].tabs().len(), 1);
        assert_eq!(app.state.workspaces[0].pane_count(), 1);
        assert_eq!(app.state.terminals.len(), terminal_count);
        shutdown_test_runtimes(&mut app);
    }
}
