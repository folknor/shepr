use std::time::Instant;

use super::App;
use crate::limits::MIN_RENDER_INTERVAL;

impl App {
    pub(crate) fn shutdown_terminal_runtime(&mut self, terminal_id: &shepr_protocol::TerminalId) {
        if let Some(runtime) = self.terminal_runtimes.remove(terminal_id) {
            drop(runtime);
        }
    }

    /// Shuts down the runtimes of terminals a state removal detached.
    pub(crate) fn shutdown_detached_terminal_runtimes(
        &mut self,
        terminal_ids: &[shepr_protocol::TerminalId],
    ) {
        for terminal_id in terminal_ids {
            self.shutdown_terminal_runtime(terminal_id);
        }
    }

    pub(crate) fn can_render_now(&self, now: Instant) -> bool {
        match self.last_render_at {
            Some(last_render_at) => now.duration_since(last_render_at) >= MIN_RENDER_INTERVAL,
            None => true,
        }
    }

    pub(crate) fn can_present_now(&self, now: Instant) -> bool {
        match self.last_presentation_at {
            Some(last_presentation_at) => {
                now.duration_since(last_presentation_at) >= MIN_RENDER_INTERVAL
            }
            None => true,
        }
    }

    pub(crate) fn record_render_attempt(&mut self, now: Instant, presentation: bool) {
        self.last_render_at = Some(now);
        if presentation {
            self.last_presentation_at = Some(now);
        }
    }

    pub(crate) fn next_headless_loop_deadline_with_git_refresh(
        &self,
        now: Instant,
        needs_render: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        let render_deadline = if needs_render {
            self.last_render_at
                .map(|last_render_at| last_render_at + MIN_RENDER_INTERVAL)
                .filter(|deadline| *deadline > now)
        } else {
            None
        };

        [
            include_git_refresh
                .then(|| self.git_refresh_deadline())
                .flatten(),
            self.pending_agent_resume_wakeup(),
            self.session_saver.deadline(),
            // A failed automatic workspace creation retries when its backoff
            // ends; a past retry time waits for the next wake instead.
            self.default_workspace_retry_at
                .filter(|retry_at| *retry_at > now),
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_mux::workspace::Workspace;

    #[test]
    fn hidden_render_attempt_keeps_presentation_cadence_available() {
        let (mut app, _) = test_app_with_pane();
        let initial_presentation = Instant::now();
        app.record_render_attempt(initial_presentation, true);

        let hidden_attempt = initial_presentation + MIN_RENDER_INTERVAL;
        app.record_render_attempt(hidden_attempt, false);
        let foreground_echo = hidden_attempt + Duration::from_millis(1);

        assert!(!app.can_render_now(foreground_echo));
        assert!(app.can_present_now(foreground_echo));
    }

    fn test_app_with_pane() -> (super::super::App, shepr_core::layout::PaneId) {
        let mut app = super::super::App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Suspended,
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.root_pane();
        app.state.test_push_workspace(ws);
        app.state.set_bookmark_index(Some(0));
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 80, 24));
        (app, pane_id)
    }
}
