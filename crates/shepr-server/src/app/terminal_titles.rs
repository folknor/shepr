use std::collections::HashSet;

use super::App;
use shepr_core::layout::PaneId;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TerminalTitleChanges {
    pub(crate) raw_changed: bool,
    pub(crate) stripped_changed: bool,
}

impl App {
    pub(crate) fn sync_pending_terminal_titles(&mut self) -> TerminalTitleChanges {
        let sources = self.render_dirty.pending_terminal_title_sources();
        let changes = self.sync_terminal_titles(&sources);
        if changes.raw_changed || changes.stripped_changed {
            self.state.mark_shell_projection_dirty();
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changes
    }

    pub(crate) fn sync_terminal_titles(
        &mut self,
        sources: &HashSet<PaneId>,
    ) -> TerminalTitleChanges {
        if sources.is_empty() {
            return TerminalTitleChanges::default();
        }

        let mut observations = Vec::with_capacity(sources.len());
        for pane_id in sources {
            let Some(terminal_id) = self
                .find_pane(*pane_id)
                .map(|(_ws_idx, pane)| pane.attached_terminal_id.clone())
            else {
                continue;
            };
            let Some(runtime) = self.terminal_runtimes.get(&terminal_id) else {
                continue;
            };
            observations.push((terminal_id, runtime.terminal_title()));
        }

        let mut changes = TerminalTitleChanges::default();
        for (terminal_id, title) in observations {
            let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
                continue;
            };
            let change = terminal.set_terminal_title(title);
            changes.raw_changed |= change.raw_changed;
            changes.stripped_changed |= change.stripped_changed;
        }

        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_agent::detect::{Agent, AgentState};
    use shepr_config::Config;
    use shepr_mux::workspace::Workspace;

    #[tokio::test]
    async fn sync_keeps_latest_raw_title_and_reports_stripped_changes() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![Workspace::test_new("one")];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.detected_agent = Some(Agent::Claude);
        terminal.state = AgentState::Working;
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes("\x1b]0;⠋ 修复\u{1F642}标题\x07".as_bytes());
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);
        let sources = HashSet::from([pane_id]);

        assert_eq!(
            app.sync_terminal_titles(&sources),
            TerminalTitleChanges {
                raw_changed: true,
                stripped_changed: true,
            }
        );
        let pane = app.pane_info(0, pane_id).expect("test precondition");
        assert_eq!(pane.terminal_title.as_deref(), Some("⠋ 修复\u{1F642}标题"));
        assert_eq!(
            pane.terminal_title_stripped.as_deref(),
            Some("修复\u{1F642}标题")
        );
        assert_eq!(pane.agent_status, shepr_api::schema::AgentStatus::Working);
        let agent = app.collect_agent_infos().pop().expect("test precondition");
        assert_eq!(agent.terminal_title.as_deref(), Some("⠋ 修复\u{1F642}标题"));
        assert_eq!(
            agent.terminal_title_stripped.as_deref(),
            Some("修复\u{1F642}标题")
        );

        app.terminal_runtimes
            .get(&terminal_id)
            .expect("test precondition")
            .test_process_pty_bytes("\x1b]2;⠙ 修复\u{1F642}标题\x1b\\".as_bytes());
        assert_eq!(
            app.sync_terminal_titles(&sources),
            TerminalTitleChanges {
                raw_changed: true,
                stripped_changed: false,
            }
        );
        let pane = app.pane_info(0, pane_id).expect("test precondition");
        assert_eq!(pane.terminal_title.as_deref(), Some("⠙ 修复\u{1F642}标题"));
        assert_eq!(
            pane.terminal_title_stripped.as_deref(),
            Some("修复\u{1F642}标题")
        );

        app.terminal_runtimes
            .get(&terminal_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"\x1b]0;Done reviewing\x07");
        assert!(app.sync_terminal_titles(&sources).stripped_changed);

        app.terminal_runtimes
            .get(&terminal_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"\x1b]0;\x07");
        assert!(app.sync_terminal_titles(&sources).stripped_changed);
        let pane = app.pane_info(0, pane_id).expect("test precondition");
        assert_eq!(pane.terminal_title, None);
        assert_eq!(pane.terminal_title_stripped, None);
    }

    #[tokio::test]
    async fn syncing_pending_titles_preserves_sidebar_render_impact() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![Workspace::test_new("one")];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("test precondition")
            .clone();
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]0;building\x07");
        app.terminal_runtimes.insert(terminal_id, runtime);
        app.render_dirty.request_terminal_title(pane_id);

        let changes = app.sync_pending_terminal_titles();

        assert!(changes.stripped_changed);
        let render_request = app.render_dirty.take();
        assert!(render_request.generic);
    }
}
