use shepr_core::absolute_path::AbsolutePath;
use shepr_protocol::command::EndpointError;

use super::endpoint::invalid_argument;

/// The launch cwd named by a client-shell command (`workspace.create`).
///
/// A relative path is refused without a filesystem access: the server's cwd
/// means nothing to the caller. Saved value defects are admitted by the strict
/// session schema and dropped per pane during restore, rather than rejecting
/// the whole saved layout at deserialization.
/// This is lexical launch input, not a UsableCwd observation: filesystem
/// admission belongs to the child, so a hung mount cannot stall
/// this event-loop boundary.
pub(super) fn launch_cwd(raw: &shepr_protocol::RemotePath) -> Result<AbsolutePath, EndpointError> {
    match AbsolutePath::new(raw.as_path()) {
        Ok(path) => Ok(path),
        Err(_) => invalid_argument(format!(
            "cwd {:?} must be an absolute path",
            raw.display_text()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::test_support::*;
    use shepr_mux::workspace::Workspace;
    use shepr_protocol::command::{WorkspaceCreateParams, WorkspaceCreateSource};
    use std::path::PathBuf;

    #[test]
    fn launch_cwd_accepts_absolute_and_names_a_refused_relative_path() {
        assert_eq!(
            launch_cwd(&"/srv/project".into())
                .expect("absolute cwd")
                .into_path_buf(),
            PathBuf::from("/srv/project")
        );
        for raw in ["relative/dir", ".", ""] {
            let error = launch_cwd(&raw.into()).expect_err("relative cwd is refused");
            assert_eq!(
                error,
                EndpointError::InvalidArgument(format!("cwd {raw:?} must be an absolute path"))
            );
        }
    }

    /// `.` is relative but exists, so without the boundary check the request
    /// would launch a pane whose saved cwd is relative.
    #[tokio::test]
    async fn workspace_create_refuses_a_relative_cwd_before_spawning() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
        let mut app = App::new(&shepr_config::ServerConfig::default());
        app.set_test_shell(exiting_test_command());
        app.state
            .test_set_workspaces(vec![Workspace::test_new("relative-cwd")]);
        let pane_count = app.state.workspaces().records().count();

        let response = app.handle_workspace_create(
            WorkspaceCreateParams {
                source: WorkspaceCreateSource::Cwd(".".into()),
                label: None,
            },
            &crate::app::EndpointContext::without_geometry(),
        );
        let error = response.expect_err("the relative cwd is refused");
        assert!(
            error.to_string().contains("\".\""),
            "message names the offending cwd: {error}"
        );

        assert_eq!(app.state.workspaces().len(), 1);
        assert_eq!(app.state.ws(0).tree().len(), 1);
        assert_eq!(app.state.workspaces().records().count(), pane_count);
        shutdown_test_runtimes(&mut app);
    }
}
