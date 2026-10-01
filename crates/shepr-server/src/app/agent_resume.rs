use std::time::Instant;

use bytes::Bytes;
use ratatui::layout::Rect;

use super::App;
use super::resume_schedule::AttemptOutcome;
use shepr_mux::workspace::Workspace;

struct PendingAgentResumeCandidate {
    pane_id: shepr_core::layout::PaneId,
    terminal_id: shepr_protocol::TerminalId,
    cwd: std::path::PathBuf,
    plan: shepr_agent::agent::resume::AgentResumePlan,
    /// The PTY geometry the resumed shell starts at: its content grid and the
    /// pixel size of one cell of the geometry the workspace was last applied.
    geometry: shepr_core::geometry::PaneGeometry,
}

impl App {
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_agent_resume_plan.is_some())
    }

    fn live_host_theme_reported(&self) -> bool {
        self.live_host_theme_reported
    }

    /// When the headless loop should wake to attempt a resume, `None` while
    /// nothing is eligible or nothing holds an eligible candidate back. Derived
    /// from the schedule on every call; see `ResumeSchedule::wakeup`.
    pub(crate) fn pending_agent_resume_wakeup(&self) -> Option<Instant> {
        if !self.has_pending_agent_resumes() {
            return None;
        }
        self.resume_schedule.wakeup(
            self.clock.now,
            self.has_pending_agent_resume_candidates(),
            self.live_host_theme_reported(),
        )
    }

    /// Attempts every resume the schedule allows now. The one entry point for
    /// every path that starts resumes (the loop and the geometry callbacks),
    /// so all of them share the schedule's theme wait, spacing and backoff.
    /// Returns whether any plan was consumed (an agent launched, or the resume
    /// abandoned).
    pub(crate) fn start_pending_agent_resumes(&mut self, now: Instant) -> bool {
        // The headless loop calls this on every iteration; skip the per-workspace
        // layout walk entirely once nothing is waiting to resume.
        let has_pending_plans = self.has_pending_agent_resumes();
        let eligible = has_pending_plans && self.has_pending_agent_resume_candidates();
        self.resume_schedule
            .observe(now, has_pending_plans, eligible);
        if !self
            .resume_schedule
            .is_due(now, eligible, self.live_host_theme_reported())
        {
            return false;
        }

        let pending = self.pending_agent_resume_candidates();
        let mut pass = self.resume_schedule.begin_pass(now);
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            plan,
            geometry,
        } in &pending
        {
            if self.terminal_runtimes.get(terminal_id).is_some() {
                continue;
            }
            // A worker checks each saved cwd independently. Preserve layout
            // order among ready candidates, but let one slow filesystem lookup
            // hold only its own resume instead of the whole pass.
            let Some(directory_available) =
                self.resume_schedule.take_directory_check(terminal_id, cwd)
            else {
                continue;
            };
            let outcome = self.start_pending_agent_resume(
                *pane_id,
                terminal_id,
                cwd,
                plan,
                *geometry,
                directory_available,
                now,
            );
            if !pass.record(outcome) {
                break;
            }
        }
        self.resume_schedule.finish(&pass);

        let changed = pass.changed();
        if changed {
            self.state.mark_session_dirty();
        }
        if !self.has_pending_agent_resumes() {
            self.resume_schedule.observe(now, false, false);
        }
        changed
    }

    /// Cwds that need checking before the next due resume pass. The headless
    /// server runs these checks on workers and records their results here.
    pub(crate) fn pending_agent_resume_cwd_checks(
        &mut self,
        now: Instant,
    ) -> Vec<(shepr_protocol::TerminalId, std::path::PathBuf)> {
        let has_pending_plans = self.has_pending_agent_resumes();
        let eligible = has_pending_plans && self.has_pending_agent_resume_candidates();
        self.resume_schedule
            .observe(now, has_pending_plans, eligible);
        if !self
            .resume_schedule
            .is_due(now, eligible, self.live_host_theme_reported())
        {
            return Vec::new();
        }
        self.pending_agent_resume_candidates()
            .into_iter()
            .filter(|candidate| {
                !self
                    .resume_schedule
                    .has_directory_check(&candidate.terminal_id, &candidate.cwd)
            })
            .map(|candidate| (candidate.terminal_id, candidate.cwd))
            .collect()
    }

    /// Records a cwd check completed by a worker. The result stays paired with
    /// both the terminal and the path so a changed cwd is checked separately.
    pub(crate) fn record_pending_agent_resume_cwd_check(
        &mut self,
        terminal_id: shepr_protocol::TerminalId,
        cwd: std::path::PathBuf,
        available: bool,
    ) {
        self.resume_schedule
            .record_directory_check(terminal_id, cwd, available);
    }

    /// Whether any pane would be a resume candidate right now, without cloning
    /// plans or collecting them. Same rules as
    /// `pending_agent_resume_candidates`: a candidate needs its workspace laid
    /// out (`resume_layout_area`), a pane in the layout, no runtime yet and an
    /// unconsumed plan.
    fn has_pending_agent_resume_candidates(&self) -> bool {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .any(|(ws_idx, ws)| {
                // A restored pane without usable geometry cannot be launched
                // yet. Check this first so repeated loop passes do not walk its
                // pane tree before the first layout or a nonzero resize.
                self.resume_layout_area(ws_idx).is_some_and(|area| {
                    self.workspace_has_pending_agent_resume(ws)
                        && self
                            .pending_agent_resume_pane_infos(ws, area)
                            .iter()
                            .any(|info| self.pane_awaits_agent_resume(ws, info.id))
                })
            })
    }

    /// The area a workspace's pending resumes are sized in: the area its PTY
    /// geometry was actually applied in (or its first pane spawned at). A
    /// workspace with no recorded geometry has no size to launch with, so its
    /// resumes wait for the first geometry pass; the headless area is never a
    /// stand-in.
    fn resume_layout_area(&self, ws_idx: usize) -> Option<Rect> {
        self.state
            .workspace_area(ws_idx)
            .filter(|area| area.width > 0 && area.height > 0)
    }

    /// Cheap pre-filter: whether any pane of `workspace` awaits a resume. Lets
    /// the candidate walks skip the layout computation for every workspace
    /// with nothing pending, which is almost all of them.
    fn workspace_has_pending_agent_resume(&self, workspace: &Workspace) -> bool {
        workspace
            .panes()
            .keys()
            .any(|pane_id| self.pane_awaits_agent_resume(workspace, *pane_id))
    }

    fn pane_awaits_agent_resume(
        &self,
        workspace: &Workspace,
        pane_id: shepr_core::layout::PaneId,
    ) -> bool {
        workspace.panes().get(&pane_id).is_some_and(|pane| {
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
        let mut pending = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            if !self.workspace_has_pending_agent_resume(ws) {
                continue;
            }
            let Some(area) = self.resume_layout_area(ws_idx) else {
                continue;
            };
            let cell = self
                .state
                .workspace_spawn_geometry(ws_idx)
                .and_then(|geometry| geometry.cell_px());
            for info in self.pending_agent_resume_pane_infos(ws, area) {
                let Some(pane) = ws.panes().get(&info.id) else {
                    continue;
                };
                if self
                    .terminal_runtimes
                    .get(&pane.attached_terminal_id)
                    .is_some()
                {
                    continue;
                }
                let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id) else {
                    continue;
                };
                let Some(plan) = terminal.pending_agent_resume_plan.clone() else {
                    continue;
                };
                pending.push(PendingAgentResumeCandidate {
                    pane_id: info.id,
                    terminal_id: pane.attached_terminal_id.clone(),
                    cwd: terminal.cwd().to_path_buf(),
                    plan,
                    geometry: shepr_mux::workspace::spawn_geometry(
                        info.inner_rect.height,
                        info.inner_rect.width,
                        cell,
                    ),
                });
            }
        }
        pending
    }

    /// Every pane of `workspace` laid out in `area`, at the content rect a
    /// fresh shell gets there. For a visible pane that is the rect the
    /// workspace's next geometry pass gives it (a runtimeless pane is laid out
    /// as a primary screen with its gutter reserved), so a resumed pane is not
    /// resized again as soon as it starts.
    fn pending_agent_resume_pane_infos(
        &self,
        workspace: &Workspace,
        area: Rect,
    ) -> Vec<shepr_mux::workspace::PaneChromeInfo> {
        derived_pending_agent_resume_pane_infos(
            workspace,
            self.state.settings.pane_geometry_in(area),
        )
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: shepr_core::layout::PaneId,
        terminal_id: &shepr_protocol::TerminalId,
        cwd: &std::path::Path,
        plan: &shepr_agent::agent::resume::AgentResumePlan,
        geometry: shepr_core::geometry::PaneGeometry,
        directory_available: bool,
        now: Instant,
    ) -> AttemptOutcome {
        let host_terminal_theme = self.state.host_terminal_theme;

        // A restored resume runs through the shell by design. Quote each argv
        // element into shell text before sending it to the PTY; the planner's
        // metacharacter regression asserts on this resulting text.
        let Some(resume_command) = shepr_remote::interactive_shell_command(&plan.argv) else {
            // The planner refuses to produce an empty argv, so this is a
            // plan that should not exist; retrying cannot change it.
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "abandoning deferred agent resume with empty argv"
            );
            self.abandon_resume(terminal_id, "the saved resume command is empty", now);
            return AttemptOutcome::Abandoned;
        };
        // No launch env only when the pane or its workspace is gone, which no
        // retry fixes.
        let Some(launch_env) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id))
        else {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "abandoning deferred agent resume: pane or workspace is gone"
            );
            self.abandon_resume(terminal_id, "the pane no longer exists", now);
            return AttemptOutcome::Abandoned;
        };
        let launch_env = launch_env.for_agent_resume();

        if !directory_available {
            if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
                terminal.abandon_agent_resume(
                    shepr_mux::terminal::RestoreFailure::DirectoryUnavailable {
                        path: cwd.to_path_buf(),
                    },
                    now,
                );
            }
            return AttemptOutcome::Abandoned;
        }

        // Launch requires a successful worker metadata check for this exact
        // saved path, so a persistently hung mount lookup leaves the loop free
        // while the resume waits. A later child chdir can still block here;
        // moving spawn off-loop would need a reserved runtime generation so
        // events arriving before construction finishes are admitted. Keep that
        // extra registration state out until a mount passes the check and
        // then hangs during chdir.
        let runtime = match shepr_mux::pane::PaneRuntime::spawn(
            pane_id,
            geometry,
            cwd,
            self.state.settings.pane_scrollback_limit_bytes,
            host_terminal_theme,
            self.state.host_terminal_appearance,
            shepr_mux::pane::PaneShellConfig::new(
                &self.state.settings.default_shell,
                self.state.settings.login_shell,
            )
            .require_cwd(),
            &launch_env,
            &self.event_tx,
            &self.render_notify,
            &self.render_dirty,
            &self.pane_teardowns,
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    error = %err,
                    "failed to start shell for deferred agent resume"
                );
                if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
                    terminal.abandon_agent_resume(
                        shepr_mux::terminal::RestoreFailure::shell_start_failed(&err),
                        now,
                    );
                }
                return AttemptOutcome::Abandoned;
            }
        };

        let mut input = resume_command;
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                error = %err,
                "failed to send deferred agent resume command to shell"
            );
            drop(runtime);
            return AttemptOutcome::Retryable;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.runtimes_replaced_panes.push(pane_id);
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal.pending_agent_resume_plan = None;
        }
        AttemptOutcome::Launched
    }

    fn abandon_resume(
        &mut self,
        terminal_id: &shepr_protocol::TerminalId,
        reason: &str,
        now: Instant,
    ) {
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal.abandon_agent_resume(
                shepr_mux::terminal::RestoreFailure::resume_unavailable(reason),
                now,
            );
        }
    }
}

fn derived_pending_agent_resume_pane_infos(
    workspace: &Workspace,
    geometry: shepr_mux::workspace::PaneGeometry,
) -> Vec<shepr_mux::workspace::PaneChromeInfo> {
    // Hidden panes still need their restored agent resumed. Give them their
    // tiled size, while the visible zoomed pane starts at its full screen size.
    let mut panes = geometry.visible_panes(workspace.layout(), false);
    if workspace.zoomed() {
        for zoomed in geometry.visible_panes(workspace.layout(), true) {
            if let Some(info) = panes.iter_mut().find(|info| info.id == zoomed.id) {
                *info = zoomed;
            }
        }
    }
    panes
        .into_iter()
        .map(|mut info| {
            let pane_inner = shepr_mux::workspace::pane_inner_rect(info.rect, info.borders);
            // The resume starts in a fresh shell, on the primary screen, so
            // the content rect is the one a workspace surface gives a primary
            // screen, and the one it gives a pane that has no runtime yet.
            info.inner_rect = shepr_mux::workspace::terminal_content_rect(
                pane_inner,
                geometry.pane_scrollbars,
                false,
            );
            info
        })
        .collect()
}

#[cfg(test)]
fn directory_available(cwd: &std::path::Path) -> bool {
    std::fs::metadata(cwd).is_ok_and(|metadata| metadata.is_dir())
}

#[cfg(test)]
impl App {
    /// A resume pass with the worker's cwd checks run inline first, as the
    /// headless loop would run them on its workers.
    fn start_pending_agent_resumes_inline_for_test(&mut self, now: Instant) -> bool {
        for (terminal_id, cwd) in self.pending_agent_resume_cwd_checks(now) {
            let available = directory_available(&cwd);
            self.record_pending_agent_resume_cwd_check(terminal_id, cwd, available);
        }
        self.start_pending_agent_resumes(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::PENDING_AGENT_RESUME_THEME_WAIT;
    use crate::test_support::*;

    fn test_app() -> App {
        App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        )
    }

    fn report_test_host_theme(app: &mut App) {
        app.set_host_terminal_theme(shepr_termio::host_term::theme::TerminalTheme {
            foreground: Some(shepr_termio::host_term::theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(shepr_termio::host_term::theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        });
    }

    #[tokio::test]
    async fn abandoned_resumes_are_all_settled_in_one_pass_without_spacing() {
        for delay_ms in [100, 250, 0] {
            let config: shepr_config::ServerConfig = toml::from_str(&format!(
                "[session]\nstartup_per_agent_delay_ms = {delay_ms}"
            ))
            .expect("test precondition");
            let mut app = App::new(&config, crate::app::AppPolicy::Test);
            app.state.workspaces = (0..4)
                .map(|_| shepr_mux::workspace::Workspace::test_new("restore"))
                .collect();
            app.state.set_bookmark_index(Some(0));
            app.state
                .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
            app.state.ensure_test_terminals();
            let missing =
                crate::test_support::ScratchDir::new("resume-cwd").join("__missing_resume_cwd__");
            assert!(!missing.try_exists().expect("stat missing resume cwd"));
            for terminal in app.state.terminals.values_mut() {
                // Restore builds a terminal from its saved cwd, which may have
                // disappeared; a live pane never reports a missing one.
                *terminal =
                    shepr_mux::terminal::TerminalState::new(terminal.id.clone(), missing.clone());
                terminal.pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
                    &terminal.id.to_string(),
                    vec!["codex".into()],
                ));
            }
            let now = Instant::now();
            // No live host theme report yet: the first pass only starts the wait.
            assert!(!app.start_pending_agent_resumes_inline_for_test(now));
            let theme_wait = now + PENDING_AGENT_RESUME_THEME_WAIT;
            assert_eq!(app.pending_agent_resume_wakeup(), Some(theme_wait));
            // An abandonment starts no agent, so it spaces nothing out: every
            // overdue candidate is settled in the one pass whatever the delay.
            assert!(app.start_pending_agent_resumes_inline_for_test(theme_wait));
            assert!(!app.has_pending_agent_resumes());
            assert_eq!(app.pending_agent_resume_wakeup(), None);
            assert!(!app.resume_schedule.is_pending());
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

    /// A plan with nothing to run is abandoned with a diagnostic on the pane,
    /// not retried: retrying an empty argv can never succeed and would spin the
    /// loop.
    #[tokio::test]
    async fn an_empty_argv_resume_is_abandoned_not_retried() {
        let mut app = test_app();
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.root_pane();
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![workspace];
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        // An empty argv cannot be turned into a shell command.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0failing-session",
            Vec::new(),
        ));

        let now = Instant::now();
        assert!(!app.start_pending_agent_resumes_inline_for_test(now));
        let due = now + PENDING_AGENT_RESUME_THEME_WAIT;
        assert!(app.start_pending_agent_resumes_inline_for_test(due));
        // The plan is consumed, the pane says why, and nothing is left to
        // wake for or to hold back.
        assert!(!app.has_pending_agent_resumes());
        let terminal = &app.state.terminals[&terminal_id];
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(terminal.restore_error.is_some());
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        assert_eq!(app.pending_agent_resume_wakeup(), None);
        assert!(!app.start_pending_agent_resumes_inline_for_test(due));
    }

    #[tokio::test]
    async fn candidate_probe_agrees_with_the_collected_candidates() {
        let mut app = test_app();
        let pending_workspace = shepr_mux::workspace::Workspace::test_new("pending");
        let pending_pane = pending_workspace.root_pane();
        let pending_terminal = pending_workspace
            .terminal_id(pending_pane)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![
            shepr_mux::workspace::Workspace::test_new("idle"),
            pending_workspace,
        ];
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();

        // Nothing pending anywhere.
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
        assert!(!app.has_pending_agent_resume_candidates());
        assert!(app.pending_agent_resume_candidates().is_empty());

        app.state
            .terminals
            .get_mut(&pending_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0probe-session",
            long_running_test_argv(),
        ));
        assert!(app.has_pending_agent_resume_candidates());
        let candidates = app.pending_agent_resume_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].terminal_id, pending_terminal);
        assert_eq!(candidates[0].pane_id, pending_pane);
        assert_eq!(
            candidates[0].geometry.cell, None,
            "no cell size was recorded"
        );

        // The resumed shell starts with the pixel size of the geometry that was
        // actually applied to its workspace.
        app.state
            .test_record_all_workspace_geometry(crate::app::SpawnGeometry {
                area: Rect::new(0, 0, 100, 30),
                cell_size: shepr_termio::host_term::cell_size::HostCellSize {
                    width_px: 8,
                    height_px: 16,
                },
            });
        let candidates = app.pending_agent_resume_candidates();
        assert_eq!(
            candidates[0].geometry.cell,
            shepr_core::geometry::CellPx::new(8, 16)
        );

        // A workspace the server has not laid out has no geometry to launch with,
        // and neither has one laid out in an empty area.
        app.state.workspace_geometry.clear();
        assert!(!app.has_pending_agent_resume_candidates());
        assert!(app.pending_agent_resume_candidates().is_empty());
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 0, 0));
        assert!(!app.has_pending_agent_resume_candidates());
        assert!(app.pending_agent_resume_candidates().is_empty());
    }

    #[tokio::test]
    async fn an_unchecked_cwd_does_not_block_a_later_checked_resume() {
        let _env = IsolatedEnv::new();
        let mut app = test_app();
        app.state.settings.default_shell = shepr_test_support::fixture::idle_shell().into();
        let waiting_workspace = shepr_mux::workspace::Workspace::test_new("waiting");
        let waiting_pane = waiting_workspace.root_pane();
        let waiting_terminal = waiting_workspace
            .terminal_id(waiting_pane)
            .cloned()
            .expect("test precondition");
        let ready_workspace = shepr_mux::workspace::Workspace::test_new("ready");
        let ready_pane = ready_workspace.root_pane();
        let ready_terminal = ready_workspace
            .terminal_id(ready_pane)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![waiting_workspace, ready_workspace];
        app.state.set_bookmark_index(Some(0));
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
        app.state.ensure_test_terminals();
        for terminal_id in [&waiting_terminal, &ready_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
                &format!("shepr:codex\0codex\0Id\0{terminal_id}"),
                long_running_test_argv(),
            ));
        }
        let waiting_cwd = app.state.terminals[&waiting_terminal].cwd().to_path_buf();
        let ready_cwd = app.state.terminals[&ready_terminal].cwd().to_path_buf();
        report_test_host_theme(&mut app);

        let now = Instant::now();
        app.record_pending_agent_resume_cwd_check(ready_terminal.clone(), ready_cwd.clone(), true);
        assert!(app.start_pending_agent_resumes(now));

        assert!(app.terminal_runtimes.get(&waiting_terminal).is_none());
        assert!(
            app.state.terminals[&waiting_terminal]
                .pending_agent_resume_plan
                .is_some()
        );
        assert!(
            !app.resume_schedule
                .has_directory_check(&waiting_terminal, &waiting_cwd)
        );
        assert!(app.terminal_runtimes.get(&ready_terminal).is_some());
        assert!(
            app.state.terminals[&ready_terminal]
                .pending_agent_resume_plan
                .is_none()
        );
        assert!(
            !app.resume_schedule
                .has_directory_check(&ready_terminal, &ready_cwd)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            drop(runtime);
        }
    }

    fn long_running_test_argv() -> Vec<String> {
        use shepr_test_support::fixture::{self, Step};
        fixture::argv(&[Step::Sleep(std::time::Duration::from_secs(5))])
    }

    /// An argv whose marker needs shell quoting to survive injection into
    /// the restored shell as one word.
    fn marker_resume_test_argv() -> Vec<String> {
        use shepr_test_support::fixture::{self, Step};
        fixture::argv(&[
            Step::Print("restored agent: shell quoted | marker".into()),
            Step::Sleep(std::time::Duration::from_secs(5)),
        ])
    }

    #[tokio::test]
    async fn failed_deferred_restore_keeps_session_reference_without_retrying_elsewhere() {
        for missing_shell in [false, true] {
            let mut app = test_app();
            let workspace = shepr_mux::workspace::Workspace::test_new("unavailable");
            let pane_id = workspace.root_pane();
            let terminal_id = workspace
                .terminal_id(pane_id)
                .expect("test precondition")
                .clone();
            app.state.workspaces = vec![workspace];
            app.state.set_bookmark_index(Some(0));
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
                let missing = crate::test_support::ScratchDir::new("resume-cwd")
                    .join("__shepr_missing_resume_cwd__");
                assert!(!missing.try_exists().expect("stat missing resume cwd"));
                // Restore builds a terminal from its saved cwd, which may have
                // disappeared; a live pane never reports a missing one.
                *terminal = shepr_mux::terminal::TerminalState::new(terminal.id.clone(), missing);
            }
            let session = shepr_agent::agent::resume::PersistedAgentSession {
                source: "shepr:codex".into(),
                agent: shepr_agent::agent::Agent::Codex,
                session_ref: shepr_agent::agent::resume::AgentSessionRef::id("resume-test")
                    .expect("test precondition"),
            };
            terminal.persisted_agent_session = Some(session.clone());
            terminal.pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
                "resume-test",
                long_running_test_argv(),
            ));
            // Restore seeds the resumed agent as detected.
            let _ = terminal.set_detected_state_with_screen_signals_at(
                Some(shepr_agent::detect::Agent::Codex),
                shepr_agent::detect::AgentState::Idle,
                false,
                false,
                Instant::now(),
            );
            app.state
                .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
            report_test_host_theme(&mut app);
            let now = Instant::now();
            assert!(app.start_pending_agent_resumes_inline_for_test(now));
            assert!(app.terminal_runtimes.get(&terminal_id).is_none());
            let terminal = &app.state.terminals[&terminal_id];
            assert!(terminal.pending_agent_resume_plan.is_none());
            assert_eq!(terminal.persisted_agent_session.as_ref(), Some(&session));
            assert!(terminal.restore_error.is_some());
            // No process will ever run here: the seeded detection goes.
            assert_eq!(terminal.detected_agent, None);
            assert_eq!(terminal.effective_known_agent(), None);
            assert!(!app.has_pending_agent_resumes());
            assert!(!app.start_pending_agent_resumes_inline_for_test(now));
        }
    }

    #[tokio::test]
    async fn resume_cwd_removed_after_worker_check_does_not_launch_in_home() {
        use shepr_test_support::fixture::{self, Step};

        let _env = IsolatedEnv::new();
        let mut app = test_app();
        let scratch = ScratchDir::new("resume-cwd-race");
        let cwd = scratch.join("agent-session");
        std::fs::create_dir(&cwd).expect("create resume cwd");
        let available = directory_available(&cwd);
        assert!(available, "the worker check sees the directory");

        let shell = fixture::stand_in(
            scratch.path(),
            "resume-shell",
            &[Step::Sleep(std::time::Duration::from_secs(30))],
        );
        app.state.settings.default_shell = shell
            .to_str()
            .expect("fixture shell path is UTF-8")
            .to_owned();

        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.root_pane();
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let plan =
            crate::test_support::test_codex_plan("resume-cwd-race", long_running_test_argv());
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        *terminal = shepr_mux::terminal::TerminalState::new(terminal_id.clone(), cwd.clone());
        terminal.pending_agent_resume_plan = Some(plan.clone());

        // The worker has already reported success, but the path disappears
        // before PTY construction on the server loop.
        std::fs::remove_dir(&cwd).expect("remove checked resume cwd");
        let outcome = app.start_pending_agent_resume(
            pane_id,
            &terminal_id,
            &cwd,
            &plan,
            shepr_mux::workspace::spawn_geometry(24, 80, None),
            available,
            Instant::now(),
        );

        let runtime = app.terminal_runtimes.remove(&terminal_id);
        let launched = runtime.is_some();
        drop(runtime);
        assert_eq!(outcome, AttemptOutcome::Abandoned);
        assert!(!launched, "a stale resume cwd must not fall back to HOME");
        assert!(app.state.terminals[&terminal_id].restore_error.is_some());
    }

    #[tokio::test]
    async fn pending_agent_resume_waits_for_live_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.root_pane();
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![workspace];
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            marker_resume_test_argv(),
        ));

        assert!(!app.start_pending_agent_resumes_inline_for_test(Instant::now()));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());

        report_test_host_theme(&mut app);

        assert!(app.start_pending_agent_resumes_inline_for_test(Instant::now()));
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
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.root_pane();
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![workspace];
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            long_running_test_argv(),
        ));

        let now = Instant::now();
        assert!(!app.start_pending_agent_resumes_inline_for_test(now));
        assert!(
            app.start_pending_agent_resumes_inline_for_test(now + PENDING_AGENT_RESUME_THEME_WAIT)
        );
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = shepr_mux::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.root_pane();
        let active_terminal = active_workspace
            .terminal_id(active_pane)
            .cloned()
            .expect("test precondition");
        let hidden_workspace = shepr_mux::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.root_pane();
        let hidden_terminal = hidden_workspace
            .terminal_id(hidden_pane)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![active_workspace, hidden_workspace];
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        report_test_host_theme(&mut app);
        for terminal_id in [&active_terminal, &hidden_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
                &format!("shepr:codex\0codex\0Id\0{terminal_id}"),
                long_running_test_argv(),
            ));
        }

        let now = Instant::now();
        assert!(app.start_pending_agent_resumes_inline_for_test(now));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_none());
        // The launch spaces the next one out; the wakeup is the barrier.
        let barrier = now + std::time::Duration::from_millis(100);
        assert!(!app.start_pending_agent_resumes_inline_for_test(now));
        assert_eq!(app.pending_agent_resume_wakeup(), Some(barrier));
        assert!(app.start_pending_agent_resumes_inline_for_test(barrier));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert_eq!(
            app.pending_agent_resume_wakeup(),
            None,
            "launched pending resumes should leave no wakeup"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_zoom_hidden_panes() {
        let mut app = test_app();
        let mut workspace = shepr_mux::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.root_pane();
        // The split pane is focused, so it is the one the zoom shows.
        workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.set_zoomed(true);
        let hidden_terminal = workspace
            .terminal_id(hidden_pane)
            .cloned()
            .expect("test precondition");
        app.state.workspaces = vec![workspace];
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        report_test_host_theme(&mut app);
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .expect("hidden zoom pane terminal should exist")
            .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0zoom-hidden-session",
            long_running_test_argv(),
        ));

        assert!(app.start_pending_agent_resumes_inline_for_test(Instant::now()));
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
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_uses_current_terminal_area_for_background_panes() {
        let mut app = test_app();
        let previous_workspace = shepr_mux::workspace::Workspace::test_new("previous");
        let previous_pane = previous_workspace.root_pane();
        let previous_terminal = previous_workspace
            .terminal_id(previous_pane)
            .cloned()
            .expect("test precondition");
        let current_workspace = shepr_mux::workspace::Workspace::test_new("current");
        app.state.workspaces = vec![previous_workspace, current_workspace];
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.state.set_bookmark_index(Some(1));
        app.state.ensure_test_terminals();
        report_test_host_theme(&mut app);
        app.state
            .terminals
            .get_mut(&previous_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            long_running_test_argv(),
        ));

        assert!(app.start_pending_agent_resumes_inline_for_test(Instant::now()));
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
            drop(runtime);
        }
    }

    /// A visible pane resumes at the size its workspace's next geometry pass gives
    /// it: a runtimeless pane is laid out with the scrollbar gutter a fresh
    /// shell has, so the resumed shell is not one column wider than its first
    /// resize.
    #[tokio::test]
    async fn pending_agent_resume_launches_at_the_size_its_first_resize_keeps() {
        let mut app = test_app();
        app.state.settings.pane_scrollbars = true;
        app.state.settings.pane_borders = shepr_config::PaneBordersConfig::Always;
        let mut workspace = shepr_mux::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        let terminal_id = workspace
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        let area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.test_record_all_workspace_areas(area);
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        let target = app.state.workspaces[0].id.clone();
        let content_rect = |app: &App| {
            let layout = crate::ui::compute_surface_for(
                &app.state,
                &app.terminal_runtimes,
                Some(target.clone()),
                area,
            );
            let info = layout
                .pane_infos
                .iter()
                .find(|info| info.id == pane_id)
                .expect("the resumed pane is visible");
            (info.inner_rect.height, info.inner_rect.width)
        };
        let before_launch = content_rect(&app);
        report_test_host_theme(&mut app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            long_running_test_argv(),
        ));

        assert!(app.start_pending_agent_resumes_inline_for_test(Instant::now()));
        let launched = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should launch")
            .current_size();
        assert_eq!(launched, before_launch);
        assert_eq!(launched, content_rect(&app));

        for (_, runtime) in app.terminal_runtimes.drain() {
            drop(runtime);
        }
    }
}
