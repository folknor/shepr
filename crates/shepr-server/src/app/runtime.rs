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
            self.pending_agent_resume_deadline,
            self.session_saver.deadline(),
            self.next_tab_bar_status_deadline(),
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    #[cfg(test)]
    pub(crate) fn drain_internal_events(&mut self) -> bool {
        self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT)
            .1
    }

    #[cfg(test)]
    pub(crate) fn drain_all_internal_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event {
                break;
            }
        }
        changed
    }

    #[cfg(test)]
    fn drain_internal_events_up_to(&mut self, limit: usize) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_render_impact(ev);
        }
        (had_event, changed)
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
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs()[0].root_pane();
        app.state.workspaces.push(ws);
        app.state.set_active_index(Some(0));
        app.state
            .view
            .pane_infos
            .push(shepr_mux::workspace::PaneChromeInfo {
                id: pane_id,
                rect: ratatui::layout::Rect::new(0, 0, 80, 24),
                inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
                scrollbar_rect: None,
                borders: ratatui::widgets::Borders::NONE,
                is_focused: true,
            });
        (app, pane_id)
    }
}
