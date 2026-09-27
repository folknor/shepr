use std::time::Instant;

use bytes::Bytes;
use ratatui::layout::Rect;

use super::App;

/// Delay before retrying pending agent resumes whose due launch attempt failed
/// without consuming the plan.
const PENDING_AGENT_RESUME_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a restored managed agent's name waits, once its resume command is
/// typed, for the agent's process (or a hook report from it) to appear. Only
/// the process has to show up, not reach a prompt, so this is generous.
const MANAGED_AGENT_RESUME_TIMEOUT: std::time::Duration = crate::pane::MANAGED_AGENT_RESUME_TIMEOUT;

struct PendingAgentResumeCandidate {
    pane_id: crate::layout::PaneId,
    terminal_id: crate::protocol::TerminalId,
    cwd: std::path::PathBuf,
    plan: crate::agent::resume::AgentResumePlan,
    rows: u16,
    cols: u16,
}

impl App {
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_agent_resume_plan.is_some())
    }

    pub(crate) fn sync_pending_agent_resume_deadline(&mut self, now: Instant) {
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
            self.next_agent_resume_at = None;
            return;
        }
        if !self.has_pending_agent_resume_candidates() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        if let Some(next) = self.next_agent_resume_at {
            self.pending_agent_resume_deadline = Some(next);
        } else {
            self.pending_agent_resume_deadline
                .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
        }
    }

    pub(crate) fn pending_agent_resume_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    /// Reconciles every managed agent whose deadline has passed: a launch
    /// past its settle delay or timeout, a restored agent whose resume never
    /// showed up. The loop wakes at `AppState::next_managed_agent_deadline`,
    /// but nothing else reconciles a pane that produces no events, and a
    /// deadline left in the past makes every loop wakeup immediate. Every
    /// reconcile of a due agent moves it to a later deadline or none, so this
    /// cannot keep firing.
    pub(crate) fn expire_due_managed_agents(&mut self, now: Instant) -> bool {
        if self
            .state
            .next_managed_agent_deadline()
            .is_none_or(|deadline| now < deadline)
        {
            return false;
        }
        let due: Vec<crate::protocol::TerminalId> = self
            .state
            .terminals
            .iter()
            .filter(|(_, terminal)| {
                terminal
                    .next_managed_agent_deadline()
                    .is_some_and(|deadline| now >= deadline)
            })
            .map(|(terminal_id, _)| terminal_id.clone())
            .collect();
        let mut changed = false;
        for terminal_id in due {
            let reconciled = self
                .state
                .terminals
                .get_mut(&terminal_id)
                .is_some_and(|terminal| terminal.reconcile_managed_agent_at(now, false));
            if !reconciled {
                continue;
            }
            changed = true;
            let pane = self
                .state
                .workspaces
                .iter()
                .enumerate()
                .find_map(|(ws_idx, ws)| {
                    ws.tabs.iter().find_map(|tab| {
                        tab.panes
                            .iter()
                            .find(|(_, pane)| pane.attached_terminal_id == terminal_id)
                            .map(|(pane_id, _)| (ws_idx, *pane_id))
                    })
                });
            if let Some((ws_idx, pane_id)) = pane {
                self.emit_pane_updated(ws_idx, pane_id);
            }
        }
        if changed {
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
        changed
    }

    pub(crate) fn start_pending_agent_resumes(
        &mut self,
        now: Instant,
        allow_empty_theme: bool,
    ) -> bool {
        // The headless loop calls this on every iteration; skip the per-tab
        // layout walk entirely once nothing is waiting to resume.
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
            self.next_agent_resume_at = None;
            return false;
        }
        // Geometry/theme events can also enter here; they must not bypass spacing.
        if self.next_agent_resume_at.is_some_and(|next| now < next) {
            return false;
        }
        let pending = self.pending_agent_resume_candidates();
        let mut changed = false;
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            plan,
            rows,
            cols,
        } in &pending
        {
            if self.terminal_runtimes.get(terminal_id).is_some() {
                continue;
            }
            changed |= self.start_pending_agent_resume(
                *pane_id,
                terminal_id,
                cwd,
                plan,
                *rows,
                *cols,
                allow_empty_theme,
            );
            if changed && !self.startup_per_agent_delay.is_zero() {
                self.next_agent_resume_at = Some(now + self.startup_per_agent_delay);
                self.pending_agent_resume_deadline = self.next_agent_resume_at;
                break;
            }
        }

        if changed {
            self.schedule_session_save();
        }
        // Launching a resume changes neither the layout nor the terminal area,
        // so the remaining candidates are exactly the ones collected above
        // whose plan is still unconsumed and that still have no runtime; no
        // second layout walk is needed to find out.
        let candidates_remain = pending.iter().any(|candidate| {
            self.terminal_runtimes.get(&candidate.terminal_id).is_none()
                && self
                    .state
                    .terminals
                    .get(&candidate.terminal_id)
                    .is_some_and(|terminal| terminal.pending_agent_resume_plan.is_some())
        });
        if !candidates_remain {
            self.pending_agent_resume_deadline = None;
        } else if self.pending_agent_resume_due(now) {
            // Candidates remain although the wakeup that released them has
            // passed: a launch failed without consuming its plan (no launch
            // env, or the resume command could not be queued to the shell).
            // Leaving the deadline in the past would make every loop deadline
            // immediate and spin the server, respawning shells each time, so
            // back off before retrying. Routing the backoff through
            // `next_agent_resume_at` keeps `sync_pending_agent_resume_deadline`
            // from restoring the stale deadline.
            let retry_at = now + PENDING_AGENT_RESUME_RETRY_INTERVAL;
            self.next_agent_resume_at = Some(retry_at);
            self.pending_agent_resume_deadline = Some(retry_at);
        }
        if !self.has_pending_agent_resumes() {
            self.next_agent_resume_at = None;
        }
        changed
    }

    /// Whether any pane would be a resume candidate right now, without cloning
    /// plans or collecting them. Same rules as
    /// `pending_agent_resume_candidates`: a candidate needs a known terminal
    /// area, a pane in the layout, no runtime yet and an unconsumed plan.
    fn has_pending_agent_resume_candidates(&self) -> bool {
        let terminal_area = self.state.view.terminal_area;
        if terminal_area.width == 0 || terminal_area.height == 0 {
            return false;
        }
        self.state
            .workspaces
            .iter()
            .enumerate()
            .any(|(ws_idx, ws)| {
                ws.tabs.iter().enumerate().any(|(tab_idx, tab)| {
                    self.tab_has_pending_agent_resume(tab)
                        && self
                            .pending_agent_resume_pane_infos(ws_idx, tab_idx, tab, terminal_area)
                            .iter()
                            .any(|info| self.pane_awaits_agent_resume(tab, info.id))
                })
            })
    }

    /// Cheap pre-filter: whether any pane of `tab` awaits a resume. Lets the
    /// candidate walks skip the layout computation for every tab with nothing
    /// pending, which is almost all of them.
    fn tab_has_pending_agent_resume(&self, tab: &crate::workspace::Tab) -> bool {
        tab.panes
            .keys()
            .any(|pane_id| self.pane_awaits_agent_resume(tab, *pane_id))
    }

    fn pane_awaits_agent_resume(
        &self,
        tab: &crate::workspace::Tab,
        pane_id: crate::layout::PaneId,
    ) -> bool {
        tab.panes.get(&pane_id).is_some_and(|pane| {
            self.terminal_runtimes
                .get(&pane.attached_terminal_id)
                .is_none()
                && self
                    .state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .is_some_and(|terminal| terminal.pending_agent_resume_plan.is_some())
        })
    }

    fn pending_agent_resume_candidates(&self) -> Vec<PendingAgentResumeCandidate> {
        let terminal_area = self.state.view.terminal_area;
        if terminal_area.width == 0 || terminal_area.height == 0 {
            return Vec::new();
        };

        let mut pending = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter().enumerate() {
                if !self.tab_has_pending_agent_resume(tab) {
                    continue;
                }
                for info in
                    self.pending_agent_resume_pane_infos(ws_idx, tab_idx, tab, terminal_area)
                {
                    let Some(pane) = tab.panes.get(&info.id) else {
                        continue;
                    };
                    if self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let Some(plan) = terminal.pending_agent_resume_plan.clone() else {
                        continue;
                    };
                    pending.push(PendingAgentResumeCandidate {
                        pane_id: info.id,
                        terminal_id: pane.attached_terminal_id.clone(),
                        cwd: terminal.cwd.clone(),
                        plan,
                        rows: info.inner_rect.height,
                        cols: info.inner_rect.width,
                    });
                }
            }
        }
        pending
    }

    fn pending_agent_resume_pane_infos(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        tab: &crate::workspace::Tab,
        terminal_area: Rect,
    ) -> Vec<crate::workspace::PaneChromeInfo> {
        let mut pane_infos = derived_pending_agent_resume_pane_infos(
            tab,
            terminal_area,
            self.state.settings.pane_borders,
            self.state.settings.pane_gaps,
            self.state.settings.pane_outer_borders,
            self.state.settings.pane_scrollbars,
        );

        if self.state.active_index() == Some(ws_idx)
            && self
                .state
                .workspaces
                .get(ws_idx)
                .is_some_and(|ws| tab_idx == ws.active_tab_index())
        {
            for visible_info in &self.state.view.pane_infos {
                if let Some(info) = pane_infos
                    .iter_mut()
                    .find(|info| info.id == visible_info.id)
                {
                    *info = visible_info.clone();
                } else {
                    pane_infos.push(visible_info.clone());
                }
            }
        }

        pane_infos
    }

    pub(crate) fn start_pending_agent_resume_for_terminal(
        &mut self,
        terminal_id: &crate::protocol::TerminalId,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        if self.terminal_runtimes.get(terminal_id).is_some() {
            return false;
        }
        let Some((pane_id, cwd, plan)) = self.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                tab.layout.pane_ids().into_iter().find_map(|pane_id| {
                    let pane = tab.panes.get(&pane_id)?;
                    if &pane.attached_terminal_id != terminal_id {
                        return None;
                    }
                    let terminal = self.state.terminals.get(terminal_id)?;
                    Some((
                        pane_id,
                        terminal.cwd.clone(),
                        terminal.pending_agent_resume_plan.clone()?,
                    ))
                })
            })
        }) else {
            return false;
        };

        let changed = self.start_pending_agent_resume(
            pane_id,
            terminal_id,
            &cwd,
            &plan,
            rows,
            cols,
            allow_empty_theme,
        );
        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: &crate::protocol::TerminalId,
        cwd: &std::path::Path,
        plan: &crate::agent::resume::AgentResumePlan,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        let host_terminal_theme = self.state.host_terminal_theme;
        if host_terminal_theme.is_empty() && !allow_empty_theme {
            return false;
        }

        // A restored resume runs through the shell by design. Quote each argv
        // element into shell text before sending it to the PTY; the planner's
        // metacharacter regression asserts on this resulting text.
        let Some(resume_command) = crate::remote::interactive_shell_command(&plan.argv) else {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "failed to start deferred agent resume with empty argv"
            );
            return false;
        };
        let Some(launch_env) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id, Vec::new()))
        else {
            return false;
        };
        let launch_env = launch_env.for_agent_resume();

        if !cwd.is_dir() {
            if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
                terminal.abandon_agent_resume(
                    "Saved directory is unavailable. Restore the directory and restart this session.".into(),
                    Instant::now(),
                );
            }
            return true;
        }

        let runtime = match crate::pane::PaneRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.settings.pane_scrollback_limit_bytes,
            host_terminal_theme,
            self.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(
                &self.state.settings.default_shell,
                self.state.settings.login_shell,
            ),
            &launch_env,
            &self.event_tx,
            &self.render_notify,
            &self.render_dirty,
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    err = %err,
                    "failed to start shell for deferred agent resume"
                );
                if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
                    terminal.abandon_agent_resume(
                        format!(
                            "Could not start the saved shell: {err}. Fix the shell configuration and restart this session."
                        ),
                        Instant::now(),
                    );
                }
                return true;
            }
        };

        let mut input = resume_command;
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                err = %err,
                "failed to send deferred agent resume command to shell"
            );
            runtime.shutdown();
            return false;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal.pending_agent_resume_plan = None;
            // A restored managed name now waits for the agent to appear and
            // is released at the deadline if it never does. The paths above
            // that return early leave it awaiting the resume: a retried
            // launch starts the clock then, and a resume that can never run
            // (missing directory, unstartable shell) leaves a pane with no
            // runtime at all, where the name is kept like any unavailable
            // restored pane's so a later save writes it back.
            terminal.begin_managed_agent_resume(Instant::now(), MANAGED_AGENT_RESUME_TIMEOUT);
        }
        true
    }
}

fn derived_pending_agent_resume_pane_infos(
    tab: &crate::workspace::Tab,
    terminal_area: Rect,
    pane_borders: crate::config::PaneBordersConfig,
    pane_gaps: bool,
    pane_outer_borders: bool,
    pane_scrollbars: bool,
) -> Vec<crate::workspace::PaneChromeInfo> {
    let geometry = crate::workspace::PaneGeometry {
        area: terminal_area,
        pane_borders,
        pane_gaps,
        pane_outer_borders,
        pane_scrollbars,
    };
    // Hidden panes still need their restored agent started. Give them their
    // tiled size, while the visible zoomed pane starts at its full screen size.
    let mut panes = geometry.tab_panes(&tab.layout, false);
    if tab.zoomed {
        for zoomed in geometry.tab_panes(&tab.layout, true) {
            if let Some(info) = panes.iter_mut().find(|info| info.id == zoomed.id) {
                *info = zoomed;
            }
        }
    }
    panes
        .into_iter()
        .map(|mut info| {
            let pane_inner = crate::workspace::pane_inner_rect(info.rect, info.borders);
            info.inner_rect = stable_terminal_inner_rect(pane_inner);
            info
        })
        .collect()
}

fn stable_terminal_inner_rect(pane_inner: Rect) -> Rect {
    if pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[tokio::test]
    async fn pending_agent_resume_spacing_survives_events_and_failed_restores() {
        for delay_ms in [100, 250, 0] {
            let config: crate::config::Config = toml::from_str(&format!(
                "[session]\nstartup_per_agent_delay_ms = {delay_ms}"
            ))
            .expect("test precondition");
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &config,
                crate::app::AppPolicy::TEST,
                api_rx,
                crate::api::EventHub::default(),
            );
            app.state.workspaces = (0..4)
                .map(|_| crate::workspace::Workspace::test_new("restore"))
                .collect();
            app.state.set_active_index(Some(0));
            app.state.view.terminal_area = Rect::new(0, 0, 100, 30);
            app.state.ensure_test_terminals();
            let missing = std::env::current_dir()
                .expect("test precondition")
                .join("__missing_resume_cwd__");
            assert!(!missing.exists());
            for terminal in app.state.terminals.values_mut() {
                terminal.cwd = missing.clone();
                terminal.pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
                    &terminal.id.to_string(),
                    vec!["codex".into()],
                ));
            }
            let now = Instant::now();
            app.sync_pending_agent_resume_deadline(now);
            assert!(!app.start_pending_agent_resumes(now, false));
            assert!(app.start_pending_agent_resumes(now, true));
            if delay_ms != 0 {
                let next = now + std::time::Duration::from_millis(delay_ms);
                assert_eq!(
                    app.state
                        .terminals
                        .values()
                        .filter(|t| t.restore_error.is_some())
                        .count(),
                    1
                );
                // Geometry changes clear the wakeup, but must preserve the launch gap.
                app.pending_agent_resume_deadline = None;
                app.sync_pending_agent_resume_deadline(now);
                assert_eq!(app.pending_agent_resume_deadline, Some(next));
                assert!(
                    !app.start_pending_agent_resumes(
                        next - std::time::Duration::from_millis(1),
                        true
                    )
                );
                // A late wakeup must not release every overdue agent in a burst.
                for processed in 2..=4 {
                    let late = now + std::time::Duration::from_secs(processed * 10);
                    assert!(app.start_pending_agent_resumes(late, true));
                    assert_eq!(
                        app.state
                            .terminals
                            .values()
                            .filter(|t| t.restore_error.is_some())
                            .count(),
                        usize::try_from(processed).unwrap_or(usize::MAX)
                    );
                }
            }
            assert!(!app.has_pending_agent_resumes());
            assert!(app.pending_agent_resume_deadline.is_none());
            assert!(app.next_agent_resume_at.is_none());
            assert_eq!(
                app.state
                    .terminals
                    .values()
                    .filter(|t| t.restore_error.is_some())
                    .count(),
                4
            );
        }
    }

    /// A due launch that fails without consuming its plan must push the wakeup
    /// forward; a deadline left in the past makes the server loop spin.
    #[tokio::test]
    async fn failed_due_resume_backs_off_instead_of_leaving_deadline_in_the_past() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.view.terminal_area = Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        // An empty argv cannot be turned into a shell command, so the launch
        // fails and leaves the plan in place.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0failing-session",
            Vec::new(),
        ));

        let now = Instant::now();
        app.pending_agent_resume_deadline = Some(now - std::time::Duration::from_millis(1));
        assert!(!app.start_pending_agent_resumes(now, app.pending_agent_resume_due(now)));
        assert!(app.has_pending_agent_resumes());
        let retry_at = now + PENDING_AGENT_RESUME_RETRY_INTERVAL;
        assert_eq!(app.pending_agent_resume_deadline, Some(retry_at));
        assert!(!app.pending_agent_resume_due(now));

        // The scheduler's per-iteration sync must keep the backoff.
        app.sync_pending_agent_resume_deadline(now);
        assert_eq!(app.pending_agent_resume_deadline, Some(retry_at));
        assert!(!app.start_pending_agent_resumes(now, false));
        assert_eq!(app.pending_agent_resume_deadline, Some(retry_at));

        // Once the backoff passes, the retry fails again and backs off again.
        assert!(!app.start_pending_agent_resumes(retry_at, true));
        assert_eq!(
            app.pending_agent_resume_deadline,
            Some(retry_at + PENDING_AGENT_RESUME_RETRY_INTERVAL)
        );
    }

    #[tokio::test]
    async fn candidate_probe_agrees_with_the_collected_candidates() {
        let mut app = test_app();
        let pending_workspace = crate::workspace::Workspace::test_new("pending");
        let pending_pane = pending_workspace.tabs[0].root_pane;
        let pending_terminal = pending_workspace
            .terminal_id(pending_pane)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("idle"),
            pending_workspace,
        ];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();

        // Nothing pending anywhere.
        app.state.view.terminal_area = Rect::new(0, 0, 100, 30);
        assert!(!app.has_pending_agent_resume_candidates());
        assert!(app.pending_agent_resume_candidates().is_empty());

        app.state
            .terminals
            .get_mut(&pending_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0probe-session",
            long_running_test_argv(),
        ));
        assert!(app.has_pending_agent_resume_candidates());
        let candidates = app.pending_agent_resume_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].terminal_id, pending_terminal);
        assert_eq!(candidates[0].pane_id, pending_pane);

        // Without a terminal area there is no geometry to launch with.
        app.state.view.terminal_area = Rect::new(0, 0, 0, 0);
        assert!(!app.has_pending_agent_resume_candidates());
        assert!(app.pending_agent_resume_candidates().is_empty());
    }

    fn long_running_test_argv() -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]
    }

    fn marker_resume_test_argv() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' 'restored agent: shell quoted | marker'; sleep 5".into(),
        ]
    }

    #[tokio::test]
    async fn failed_deferred_restore_keeps_session_reference_without_retrying_elsewhere() {
        for missing_shell in [false, true] {
            let mut app = test_app();
            let workspace = crate::workspace::Workspace::test_new("unavailable");
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace
                .terminal_id(pane_id)
                .expect("test precondition")
                .clone();
            app.state.workspaces = vec![workspace];
            app.state.set_active_index(Some(0));
            app.state.ensure_test_terminals();
            if missing_shell {
                app.state.settings.default_shell = "__shepr_missing_resume_shell__".into();
            }
            let terminal = app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("test precondition");
            if !missing_shell {
                terminal.cwd = std::env::current_dir()
                    .expect("test precondition")
                    .join("__shepr_missing_resume_cwd__");
                assert!(!terminal.cwd.exists());
            }
            let session = crate::agent::resume::PersistedAgentSession {
                source: "shepr:codex".into(),
                agent: crate::agent::Agent::Codex,
                session_ref: crate::agent::resume::AgentSessionRef::id("resume-test")
                    .expect("test precondition"),
            };
            terminal.persisted_agent_session = Some(session.clone());
            terminal.pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
                "resume-test",
                long_running_test_argv(),
            ));
            // Restore seeds the resumed agent as detected and names it.
            terminal.restore_managed_agent_for_resume("worker".into(), crate::detect::Agent::Codex);
            let _ = terminal.set_detected_state_with_screen_signals_at(
                Some(crate::detect::Agent::Codex),
                crate::detect::AgentState::Idle,
                false,
                false,
                Instant::now(),
            );
            app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);
            assert!(app.terminal_runtimes.get(&terminal_id).is_none());
            let terminal = &app.state.terminals[&terminal_id];
            assert!(terminal.pending_agent_resume_plan.is_none());
            assert_eq!(terminal.persisted_agent_session.as_ref(), Some(&session));
            assert!(terminal.restore_error.is_some());
            // No process will ever run here: the seeded detection goes, the
            // name stays for the next save.
            assert_eq!(terminal.detected_agent, None);
            assert_eq!(terminal.effective_known_agent(), None);
            assert_eq!(terminal.agent_name.as_deref(), Some("worker"));
            assert!(!app.has_pending_agent_resumes());
            assert!(!app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_waits_for_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30))
            .into_iter()
            .map(Into::into)
            .collect();
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            marker_resume_test_argv(),
        ));

        assert!(!app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());

        app.state.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive launch");
        assert!(terminal.pending_agent_resume_plan.is_none());

        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should leave a shell runtime");
        let marker = "restored agent: shell quoted | marker";
        for _ in 0..20 {
            if runtime
                .snapshot_history()
                .is_some_and(|text| text.contains(marker))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime
                .snapshot_history()
                .expect("runtime should expose terminal history")
                .contains(marker),
            "deferred restore should inject the resume argv into the restored shell"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30))
            .into_iter()
            .map(Into::into)
            .collect();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            long_running_test_argv(),
        ));

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(!app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.start_pending_agent_resumes(Instant::now(), true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn a_launched_resume_releases_its_managed_name_when_the_agent_never_appears() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30))
            .into_iter()
            .map(Into::into)
            .collect();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.restore_managed_agent_for_resume("worker".into(), crate::detect::Agent::Codex);
        // A resume command that runs, but never becomes the agent.
        terminal.pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0never-appears",
            long_running_test_argv(),
        ));
        assert_eq!(app.state.next_managed_agent_deadline(), None);

        let launched_at = Instant::now();
        assert!(app.start_pending_agent_resumes(launched_at, true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let deadline = app
            .state
            .next_managed_agent_deadline()
            .expect("a launched resume waits for its agent until a deadline");
        assert!(deadline >= launched_at + MANAGED_AGENT_RESUME_TIMEOUT);
        assert_eq!(
            app.state.terminals[&terminal_id].agent_name.as_deref(),
            Some("worker")
        );

        assert!(!app.expire_due_managed_agents(deadline - std::time::Duration::from_millis(1)));
        assert!(app.expire_due_managed_agents(deadline));
        assert_eq!(app.state.terminals[&terminal_id].agent_name, None);
        assert_eq!(app.state.terminals[&terminal_id].managed_agent_kind(), None);
        assert_eq!(app.state.next_managed_agent_deadline(), None);
        assert!(!app.expire_due_managed_agents(deadline));

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = crate::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.tabs[0].root_pane;
        let active_terminal = active_workspace
            .terminal_id(active_pane)
            .cloned()
            .expect("test precondition");
        let hidden_workspace = crate::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.tabs[0].root_pane;
        let hidden_terminal = hidden_workspace
            .terminal_id(hidden_pane)
            .cloned()
            .expect("test precondition");
        app.state.view.pane_infos = active_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30))
            .into_iter()
            .map(Into::into)
            .collect();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![active_workspace, hidden_workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        for terminal_id in [&active_terminal, &hidden_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
                &format!("shepr:codex\0codex\0Id\0{terminal_id}"),
                long_running_test_argv(),
            ));
        }
        app.pending_agent_resume_deadline =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

        let now = Instant::now();
        assert!(app.start_pending_agent_resumes(now, false));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_none());
        assert!(!app.start_pending_agent_resumes(now, true));
        assert!(
            app.start_pending_agent_resumes(now + std::time::Duration::from_millis(100), false,)
        );
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.pending_agent_resume_deadline.is_none(),
            "launched pending resumes should clear the wakeup deadline"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_inactive_tab_panes_with_current_terminal_area() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("tabs");
        let active_pane = workspace.tabs[0].root_pane;
        let inactive_tab = workspace.test_add_tab(Some("agents"));
        let inactive_pane = workspace.tabs[inactive_tab].root_pane;
        let inactive_terminal = workspace.tabs[inactive_tab]
            .terminal_id(inactive_pane)
            .cloned()
            .expect("test precondition");
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30))
            .into_iter()
            .map(Into::into)
            .collect();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        assert!(
            app.state
                .workspaces
                .first()
                .and_then(|ws| ws.tabs[0].terminal_id(active_pane))
                .is_some()
        );
        app.state.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&inactive_terminal)
            .expect("inactive tab terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0inactive-tab-session",
            long_running_test_argv(),
        ));

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&inactive_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&inactive_terminal)
                .expect("inactive tab terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "inactive tab restored panes should not wait for tab focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_zoom_hidden_active_tab_panes() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.tabs[0].root_pane;
        let visible_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].zoomed = true;
        let hidden_terminal = workspace
            .terminal_id(hidden_pane)
            .cloned()
            .expect("test precondition");
        app.state.view.pane_infos = vec![crate::workspace::PaneChromeInfo {
            id: visible_pane,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .expect("hidden zoom pane terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0zoom-hidden-session",
            long_running_test_argv(),
        ));

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&hidden_terminal)
                .expect("hidden zoom pane terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "zoom-hidden restored panes should not wait for pane focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_uses_current_terminal_area_for_background_panes() {
        let mut app = test_app();
        let previous_workspace = crate::workspace::Workspace::test_new("previous");
        let previous_pane = previous_workspace.tabs[0].root_pane;
        let previous_terminal = previous_workspace
            .terminal_id(previous_pane)
            .cloned()
            .expect("test precondition");
        let current_workspace = crate::workspace::Workspace::test_new("current");
        app.state.view.pane_infos = previous_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30))
            .into_iter()
            .map(Into::into)
            .collect();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.state.workspaces = vec![previous_workspace, current_workspace];
        app.state.set_active_index(Some(1));
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&previous_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            long_running_test_argv(),
        ));

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(app.pending_agent_resume_deadline.is_some());
        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&previous_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&previous_terminal)
                .expect("previous terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "background restored panes should not wait for focus once terminal area is known"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_with_inner_rect_size() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.view.pane_infos = vec![crate::workspace::PaneChromeInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent::resume::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            long_running_test_argv(),
        ));

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .expect("pending resume should launch")
                .current_size(),
            (28, 98)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }
}
