use std::collections::HashSet;

use super::App;
use shepr_core::layout::PaneId;

impl App {
    /// Pulls the titles of `sources` from their runtimes. Any changed title
    /// for a pane with an effective agent changes shell agent metadata and
    /// marks the shared projection dirty. Titles are still stored for every
    /// pane so a later agent detection sees the current title immediately.
    pub(crate) fn sync_terminal_titles(&mut self, sources: &HashSet<PaneId>) -> bool {
        if sources.is_empty() {
            return false;
        }

        let mut observations = Vec::with_capacity(sources.len());
        for pane_id in sources {
            let Some(runtime) = self.terminal_runtimes.get(pane_id) else {
                continue;
            };
            observations.push((*pane_id, runtime.read().terminal_title()));
        }

        self.state.sync_terminal_titles(observations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_agent::{Agent, AgentState};
    use shepr_config::ServerConfig;
    use shepr_mux::workspace::Workspace;

    #[tokio::test]
    async fn sync_keeps_latest_raw_and_stripped_agent_titles() {
        let mut app = App::new(&ServerConfig::default());
        app.state
            .test_set_workspaces(vec![Workspace::test_new("one")]);
        let pane_id = app.state.ws(0).tree().root();
        let terminal = app.state.terminal_mut(pane_id);
        terminal
            .ownership_mut()
            .set_detected_state_with_screen_signals_at(
                Some(Agent::Claude),
                AgentState::Working,
                false,
                false,
                std::time::Instant::now(),
            );
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes("\x1b]0;⠋ 修复\u{1F642}标题\x07".as_bytes());
        app.terminal_runtimes.insert(pane_id, runtime);
        let sources = HashSet::from([pane_id]);

        assert!(app.sync_terminal_titles(&sources));
        let agent = app.collect_agent_infos().pop().expect("test precondition");
        assert_eq!(agent.agent_status, shepr_api::schema::AgentStatus::Working);
        assert_eq!(agent.terminal_title.as_deref(), Some("⠋ 修复\u{1F642}标题"));
        assert_eq!(
            agent.terminal_title_stripped.as_deref(),
            Some("修复\u{1F642}标题")
        );

        app.terminal_runtimes
            .get(&pane_id)
            .expect("test precondition")
            .test_process_pty_bytes("\x1b]2;⠙ 修复\u{1F642}标题\x1b\\".as_bytes());
        assert!(app.sync_terminal_titles(&sources));
        let agent = app.collect_agent_infos().pop().expect("test precondition");
        assert_eq!(agent.terminal_title.as_deref(), Some("⠙ 修复\u{1F642}标题"));
        assert_eq!(
            agent.terminal_title_stripped.as_deref(),
            Some("修复\u{1F642}标题")
        );

        app.terminal_runtimes
            .get(&pane_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"\x1b]0;Done reviewing\x07");
        assert!(app.sync_terminal_titles(&sources));

        app.terminal_runtimes
            .get(&pane_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"\x1b]0;\x07");
        assert!(app.sync_terminal_titles(&sources));
        let agent = app.collect_agent_infos().pop().expect("test precondition");
        assert_eq!(agent.terminal_title, None);
        assert_eq!(agent.terminal_title_stripped, None);
    }

    #[tokio::test]
    async fn a_plain_shell_title_is_stored_without_moving_the_shell_projection() {
        let mut app = App::new(&ServerConfig::default());
        app.state
            .test_set_workspaces(vec![Workspace::test_new("one")]);
        let pane_id = app.state.ws(0).tree().root();
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]0;building\x07");
        app.terminal_runtimes.insert(pane_id, runtime);
        let revision = app.state.shell_projection_revision();

        let changed = app.sync_terminal_titles(&HashSet::from([pane_id]));

        assert!(!changed);
        assert_eq!(app.state.shell_projection_revision(), revision);
        assert_eq!(
            app.state
                .terminal(pane_id)
                .and_then(|terminal| terminal.terminal_title()),
            Some("building")
        );

        app.state
            .terminal_mut(pane_id)
            .ownership_mut()
            .set_detected_state_with_screen_signals_at(
                Some(Agent::Claude),
                AgentState::Working,
                false,
                false,
                std::time::Instant::now(),
            );
        assert_eq!(
            app.collect_agent_infos()
                .pop()
                .expect("detected agent")
                .terminal_title
                .as_deref(),
            Some("building")
        );
    }
}
