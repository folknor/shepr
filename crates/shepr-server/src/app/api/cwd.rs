use std::path::PathBuf;

use shepr_protocol::command::EndpointError;

use super::endpoint::endpoint_rejected;

/// The launch cwd named by a client-shell command (`workspace.create`), or the
/// directory `workspace.checkout_root` asks about.
///
/// A relative path is refused, not resolved: the server's own working
/// directory means nothing to the caller, and the client sends an absolute
/// directory. Refusing here is also what keeps every pane's launch cwd
/// absolute, which the session snapshot requires: a relative saved cwd fails
/// the snapshot's deserialization, and with it the whole saved layout on the
/// next start.
pub(super) fn launch_cwd(raw: &str) -> Result<PathBuf, EndpointError> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return endpoint_rejected(format!("cwd {raw:?} must be an absolute path"));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::test_support::*;
    use shepr_mux::workspace::Workspace;
    use shepr_protocol::command::{WorkspaceCreateParams, WorkspaceCreateSource};

    #[test]
    fn launch_cwd_accepts_absolute_and_names_a_refused_relative_path() {
        assert_eq!(
            launch_cwd("/srv/project").expect("absolute cwd"),
            PathBuf::from("/srv/project")
        );
        for raw in ["relative/dir", ".", ""] {
            let error = launch_cwd(raw).expect_err("relative cwd is refused");
            assert_eq!(
                error,
                EndpointError::Rejected(format!("cwd {raw:?} must be an absolute path"))
            );
        }
    }

    /// `.` is relative but exists, so without the boundary check the request
    /// would launch a pane whose saved cwd is relative.
    #[tokio::test]
    async fn workspace_create_refuses_a_relative_cwd_before_spawning() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
        let mut app = App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        );
        app.set_test_shell(exiting_test_command());
        app.state.workspaces = vec![Workspace::test_new("relative-cwd")];
        app.state.ensure_test_terminals();
        let terminal_count = app.state.terminals.len();

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

        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].pane_count(), 1);
        assert_eq!(app.state.terminals.len(), terminal_count);
        shutdown_test_runtimes(&mut app);
    }
}
