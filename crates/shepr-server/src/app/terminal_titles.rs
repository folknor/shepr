use std::collections::HashSet;

use super::App;
use shepr_core::layout::PaneId;
use shepr_mux::terminal::state::TerminalTitleChange;

impl App {
    /// Pulls the titles of `sources` from their runtimes. Any changed title
    /// also changes the shell agent metadata, so it marks the shell projection
    /// dirty here; callers fold the returned changes into their own view
    /// change.
    pub(crate) fn sync_terminal_titles(
        &mut self,
        sources: &HashSet<PaneId>,
    ) -> TerminalTitleChange {
        if sources.is_empty() {
            return TerminalTitleChange::default();
        }

        let mut observations = Vec::with_capacity(sources.len());
        for pane_id in sources {
            let Some(runtime) = self.terminal_runtimes.get(pane_id) else {
                continue;
            };
            observations.push((*pane_id, runtime.read().terminal_title()));
        }

        let mut changes = TerminalTitleChange::default();
        for (pane_id, title) in observations {
            let Some(record) = self.state.workspaces.pane_mut(pane_id) else {
                continue;
            };
            let change = record.terminal_mut().set_terminal_title(title);
            changes.raw_changed |= change.raw_changed;
            changes.stripped_changed |= change.stripped_changed;
        }
        if changes.raw_changed || changes.stripped_changed {
            self.state.mark_shell_projection_dirty();
        }

        changes
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
    async fn sync_keeps_latest_raw_title_and_reports_stripped_changes() {
        let mut app = App::new(&ServerConfig::default());
        app.state
            .test_set_workspaces(vec![Workspace::test_new("one")]);
        app.state.seed_bookmark_index(Some(0));
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

        assert_eq!(
            app.sync_terminal_titles(&sources),
            TerminalTitleChange {
                raw_changed: true,
                stripped_changed: true,
            }
        );
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
        assert_eq!(
            app.sync_terminal_titles(&sources),
            TerminalTitleChange {
                raw_changed: true,
                stripped_changed: false,
            }
        );
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
        assert!(app.sync_terminal_titles(&sources).stripped_changed);

        app.terminal_runtimes
            .get(&pane_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"\x1b]0;\x07");
        assert!(app.sync_terminal_titles(&sources).stripped_changed);
        let agent = app.collect_agent_infos().pop().expect("test precondition");
        assert_eq!(agent.terminal_title, None);
        assert_eq!(agent.terminal_title_stripped, None);
    }

    #[tokio::test]
    async fn title_sync_moves_the_shell_projection() {
        let mut app = App::new(&ServerConfig::default());
        app.state
            .test_set_workspaces(vec![Workspace::test_new("one")]);
        app.state.seed_bookmark_index(Some(0));
        let pane_id = app.state.ws(0).tree().root();
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]0;building\x07");
        app.terminal_runtimes.insert(pane_id, runtime);
        let revision = app.state.shell_projection_revision;

        let changes = app.sync_terminal_titles(&HashSet::from([pane_id]));

        assert!(changes.stripped_changed);
        assert_ne!(app.state.shell_projection_revision, revision);
    }
}
