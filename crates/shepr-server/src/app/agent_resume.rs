use std::time::Instant;

use bytes::Bytes;

use super::App;
use super::resume_schedule::{AttemptOutcome, ResumePlans};
use shepr_mux::workspace::Workspace;

struct PendingAgentResumeCandidate {
    pane_id: shepr_core::layout::PaneId,
    cwd: shepr_core::absolute_path::AbsolutePath,
    plan: shepr_agent::resume::AgentResumePlan,
    /// The PTY geometry the resumed shell starts at: its content grid and the
    /// pixel size of one cell of the geometry the workspace was last applied.
    geometry: shepr_core::geometry::PaneGeometry,
}

/// What one resume pass did.
#[derive(Debug, Default)]
pub(crate) struct ResumeOutcome {
    /// Some plan was consumed (launched or abandoned).
    pub(crate) consumed: bool,
    /// Panes whose runtime a launch replaced; their focus is re-reported.
    pub(crate) replaced_runtimes: Vec<shepr_core::layout::PaneId>,
}

impl App {
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        if self.resume_schedule.is_retired() {
            return false;
        }
        self.state
            .workspaces
            .records()
            .any(|(_, record)| record.terminal().agent_resume().is_pending())
    }

    /// When the headless loop should wake to attempt a resume, `None` while
    /// nothing is eligible or nothing holds an eligible candidate back. Derived
    /// from the schedule on every call; see `ResumeSchedule::wakeup`.
    pub(crate) fn pending_agent_resume_wakeup(&self) -> Option<Instant> {
        if !self.has_pending_agent_resumes() {
            return None;
        }
        self.resume_schedule
            .wakeup(self.clock.now, self.has_pending_agent_resume_candidates())
    }

    /// Attempts every resume the schedule allows now. The one entry point for
    /// every path that starts resumes (the loop and the geometry callbacks),
    /// so all of them share the schedule's theme wait, spacing and backoff.
    /// Returns whether any plan was consumed (an agent launched, or the resume
    /// abandoned) and the panes whose runtime a launch replaced. A consumed
    /// plan marks the shell projection dirty.
    #[must_use]
    pub(crate) fn start_pending_agent_resumes(&mut self, now: Instant) -> ResumeOutcome {
        // The headless loop calls this on every iteration; skip the per-workspace
        // layout walk entirely once nothing is waiting to resume.
        let has_pending_plans = self.has_pending_agent_resumes();
        let eligible = has_pending_plans && self.has_pending_agent_resume_candidates();
        let plans = if eligible {
            ResumePlans::Eligible
        } else if has_pending_plans {
            ResumePlans::Waiting
        } else {
            ResumePlans::None
        };
        self.resume_schedule.observe(now, plans);
        let mut outcome = ResumeOutcome::default();
        if !self.resume_schedule.is_due(now, eligible) {
            return outcome;
        }

        let pending = self.pending_agent_resume_candidates();
        let mut pass = self.resume_schedule.begin_pass(now);
        for PendingAgentResumeCandidate {
            pane_id,
            cwd,
            plan,
            geometry,
        } in &pending
        {
            let attempt = self.start_pending_agent_resume(*pane_id, cwd, plan, *geometry, now);
            if attempt == AttemptOutcome::Launched {
                outcome.replaced_runtimes.push(*pane_id);
            }
            if !pass.record(attempt) {
                break;
            }
        }
        self.resume_schedule.finish(&pass);

        outcome.consumed = pass.changed();
        if outcome.consumed {
            self.state.mark_session_dirty();
            self.state.mark_shell_projection_dirty();
        }
        if !self.has_pending_agent_resumes() {
            self.resume_schedule.observe(now, ResumePlans::None);
        }
        outcome
    }

    /// Whether any pane would be a resume candidate right now, without cloning
    /// plans or collecting them. Same rules as
    /// `pending_agent_resume_candidates`: a candidate needs its workspace laid
    /// out (`resume_layout_area`), a pane in the layout, no runtime yet and an
    /// unconsumed plan.
    fn has_pending_agent_resume_candidates(&self) -> bool {
        self.state.workspaces.iter().any(|ws| {
            // A restored pane without usable geometry cannot be launched
            // yet. Check this first so repeated loop passes do not walk its
            // pane tree before the first layout or a nonzero resize.
            Self::resume_layout_area(ws).is_some_and(|area| {
                self.workspace_has_pending_agent_resume(ws)
                    && self
                        .pending_agent_resume_pane_infos(ws, area)
                        .iter()
                        .any(|info| self.pane_awaits_agent_resume(ws, info.chrome.id))
            })
        })
    }

    /// The area a workspace's pending resumes are sized in: the area its PTY
    /// geometry was actually applied in (or its first pane spawned at). A
    /// workspace with no recorded geometry has no size to launch with, so its
    /// resumes wait for the first geometry pass; the headless area is never a
    /// stand-in.
    fn resume_layout_area(workspace: &Workspace) -> Option<shepr_core::geometry::Rect> {
        workspace
            .spawn_geometry()
            .map(|geometry| geometry.area)
            .filter(|area| area.width > 0 && area.height > 0)
    }

    /// Cheap pre-filter: whether any pane of `workspace` awaits a resume. Lets
    /// the candidate walks skip the layout computation for every workspace
    /// with nothing pending, which is almost all of them.
    fn workspace_has_pending_agent_resume(&self, workspace: &Workspace) -> bool {
        workspace
            .tree()
            .panes()
            .any(|(pane_id, _)| self.pane_awaits_agent_resume(workspace, pane_id))
    }

    fn pane_awaits_agent_resume(
        &self,
        workspace: &Workspace,
        pane_id: shepr_core::layout::PaneId,
    ) -> bool {
        self.resume_candidate(workspace, pane_id).is_some()
    }

    fn resume_candidate<'w>(
        &self,
        workspace: &'w Workspace,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<(
        &'w shepr_mux::terminal::TerminalState,
        &'w shepr_agent::resume::AgentResumePlan,
        &'w shepr_core::absolute_path::AbsolutePath,
    )> {
        let terminal = workspace.tree().pane(pane_id)?.terminal();
        let plan = terminal
            .agent_resume()
            .candidate(self.terminal_runtimes.get(&pane_id).is_some())?;
        // Resume deliberately uses the saved path without probing or filtering
        // it. The child's required chdir owns admission of that directory.
        Some((terminal, plan, terminal.cwd()))
    }

    fn pending_agent_resume_candidates(&self) -> Vec<PendingAgentResumeCandidate> {
        let mut pending = Vec::new();
        for ws in self.state.workspaces.iter() {
            if !self.workspace_has_pending_agent_resume(ws) {
                continue;
            }
            let Some(area) = Self::resume_layout_area(ws) else {
                continue;
            };
            let cell = ws.spawn_geometry().and_then(|geometry| geometry.cell_px());
            for info in self.pending_agent_resume_pane_infos(ws, area) {
                let Some((_, plan, cwd)) = self.resume_candidate(ws, info.chrome.id) else {
                    continue;
                };
                pending.push(PendingAgentResumeCandidate {
                    pane_id: info.chrome.id,
                    cwd: cwd.clone(),
                    plan: plan.clone(),
                    geometry: shepr_core::geometry::PaneGeometry::with_cell(
                        info.content.width,
                        info.content.height,
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
        area: shepr_core::geometry::Rect,
    ) -> Vec<shepr_core::chrome::PaneContent> {
        derived_pending_agent_resume_pane_infos(workspace, self.state.chrome_in(area))
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: shepr_core::layout::PaneId,
        cwd: &shepr_core::absolute_path::AbsolutePath,
        plan: &shepr_agent::resume::AgentResumePlan,
        geometry: shepr_core::geometry::PaneGeometry,
        now: Instant,
    ) -> AttemptOutcome {
        // Quote the planner's validated command before typing it into the shell.
        let resume_command = plan.to_shell_command();
        // No public identity only when the pane or its workspace is gone,
        // which no retry fixes.
        let Some(public_id) = self.state.pane(pane_id).map(|pane| pane.public_id()) else {
            tracing::warn!(
                pane = %pane_id,
                agent = %plan.agent(),
                "abandoning deferred agent resume: pane or workspace is gone"
            );
            self.abandon_resume(
                pane_id,
                shepr_mux::terminal::ResumeUnavailableReason::PaneGone,
                now,
            );
            return AttemptOutcome::Abandoned;
        };

        // The launch returns once forked; the child enters the saved directory
        // itself (never falling back), so a directory that is gone or on a
        // hung mount holds only this pane. How it went arrives as the launch's
        // settlement (`pane_launch`), which types the command or abandons the
        // plan with the reason.
        let runtime = match self.launch_pane(
            pane_id,
            public_id,
            geometry,
            cwd,
            shepr_mux::pane::LaunchKind::AgentResume,
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = %pane_id,
                    agent = %plan.agent(),
                    error = %err,
                    "failed to start shell for deferred agent resume"
                );
                self.abandon_agent_resume(
                    pane_id,
                    shepr_mux::terminal::PaneStartFailure::shell_start_failed(&err),
                    now,
                );
                return AttemptOutcome::Abandoned;
            }
        };

        let mut input = resume_command;
        input.push('\r');
        self.install_runtime(pane_id, runtime);
        self.hold_resume_command(pane_id, Bytes::from(input));
        AttemptOutcome::Launched
    }

    fn abandon_resume(
        &mut self,
        pane_id: shepr_core::layout::PaneId,
        reason: shepr_mux::terminal::ResumeUnavailableReason,
        now: Instant,
    ) {
        self.abandon_agent_resume(
            pane_id,
            shepr_mux::terminal::PaneStartFailure::resume_unavailable(reason),
            now,
        );
    }
}

fn derived_pending_agent_resume_pane_infos(
    workspace: &Workspace,
    chrome: shepr_mux::workspace::WorkspaceChrome,
) -> Vec<shepr_core::chrome::PaneContent> {
    chrome.resume_panes(workspace.tree().layout(), workspace.tree().zoomed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::PENDING_AGENT_RESUME_THEME_WAIT;
    use crate::test_support::*;
    use ratatui::layout::Rect;

    fn test_app() -> crate::app::TestApp {
        App::new(&shepr_config::ServerConfig::default())
    }

    /// Feeds the app its queued runtime events, as the headless loop does,
    /// until every resume launch it dispatched has settled.
    async fn settle_resume_launches(app: &mut crate::app::TestApp) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while app
            .state
            .workspaces
            .records()
            .any(|(_, record)| record.terminal().agent_resume().is_launching())
        {
            let event = tokio::time::timeout_at(deadline, app.next_event())
                .await
                .expect("the resume launch settles");
            app.handle_internal_event(event);
        }
    }

    #[tokio::test]
    async fn resume_scans_stop_once_no_plan_is_pending() {
        let mut app = test_app();
        assert!(!app.resume_schedule.is_retired());

        let outcome = app.start_pending_agent_resumes(Instant::now());

        assert!(!outcome.consumed);
        assert!(outcome.replaced_runtimes.is_empty());
        assert!(app.resume_schedule.is_retired());
        assert!(!app.has_pending_agent_resumes());
        assert_eq!(app.pending_agent_resume_wakeup(), None);
    }

    fn report_test_host_theme(app: &mut App) {
        assert!(
            app.set_host_terminal_theme(shepr_term::host::TerminalTheme {
                foreground: Some(shepr_term::host::RgbColor {
                    r: 220,
                    g: 220,
                    b: 220,
                }),
                background: Some(shepr_term::host::RgbColor {
                    r: 20,
                    g: 20,
                    b: 20,
                }),
                ..Default::default()
            })
        );
    }

    /// A saved directory that is gone settles each launch as a placeholder
    /// that says so: the child's chdir is what found it missing, with no
    /// check on the event loop and no fallback.
    #[tokio::test]
    async fn resumes_whose_directory_is_gone_settle_as_placeholders() {
        let config: shepr_config::ServerConfig =
            toml::from_str("[session]\nstartup_per_agent_delay_ms = 0").expect("test precondition");
        let mut app = App::new(&config);
        app.state.test_set_workspaces(
            (0..4)
                .map(|_| shepr_mux::workspace::Workspace::test_new("restore"))
                .collect(),
        );
        app.state.seed_bookmark_index(Some(0));
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
        let missing =
            crate::test_support::ScratchDir::new("resume-cwd").join("__missing_resume_cwd__");
        assert!(!missing.try_exists().expect("stat missing resume cwd"));
        for (pane, record) in app.state.workspaces.records_mut() {
            let terminal = record.terminal_mut();
            // Restore builds a terminal from its saved cwd, which may have
            // disappeared; a live pane never reports a missing one.
            *terminal = shepr_mux::terminal::TerminalState::new(
                shepr_core::absolute_path::AbsolutePath::new(missing.clone())
                    .expect("scratch cwd is absolute"),
            );
            terminal.plan_agent_resume(crate::test_support::test_codex_plan(
                &pane.to_string(),
                vec!["codex".into()],
            ));
        }
        let now = Instant::now();
        // No live host theme report yet: the first pass only starts the wait.
        assert!(!app.start_pending_agent_resumes(now).consumed);
        let theme_wait = now + PENDING_AGENT_RESUME_THEME_WAIT;
        assert_eq!(app.pending_agent_resume_wakeup(), Some(theme_wait));
        assert!(app.start_pending_agent_resumes(theme_wait).consumed);
        settle_resume_launches(&mut app).await;
        assert!(!app.has_pending_agent_resumes());
        assert!(app.terminal_runtimes.values().next().is_none());
        for (_, record) in app.state.workspaces.records() {
            assert!(matches!(
                record.terminal().restore_error(),
                Some(shepr_mux::terminal::PaneStartFailure::DirectoryUnavailable { path, .. })
                    if *path == missing
            ));
        }
    }

    #[tokio::test]
    async fn candidate_probe_agrees_with_the_collected_candidates() {
        let mut app = test_app();
        let pending_workspace = shepr_mux::workspace::Workspace::test_new("pending");
        let pending_pane = pending_workspace.tree().root();
        app.state.test_set_workspaces(vec![
            shepr_mux::workspace::Workspace::test_new("idle"),
            pending_workspace,
        ]);
        app.state.seed_bookmark_index(Some(0));

        // Nothing pending anywhere.
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
        assert!(!app.has_pending_agent_resume_candidates());
        assert!(app.pending_agent_resume_candidates().is_empty());

        app.state.terminal_mut(pending_pane).plan_agent_resume(
            crate::test_support::test_codex_plan(
                "shepr:codex\0codex\0Id\0probe-session",
                long_running_test_argv(),
            ),
        );
        assert!(app.has_pending_agent_resume_candidates());
        let candidates = app.pending_agent_resume_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].pane_id, pending_pane);
        assert_eq!(
            candidates[0].geometry.cell(),
            None,
            "no cell size was recorded"
        );

        // The resumed shell starts with the pixel size of the geometry that was
        // actually applied to its workspace.
        app.state
            .test_record_all_workspace_geometry(crate::app::SpawnGeometry {
                area: shepr_core::geometry::Rect::new(0, 0, 100, 30),
                cell: shepr_core::geometry::CellPx::new(8, 16),
            });
        let candidates = app.pending_agent_resume_candidates();
        assert_eq!(
            candidates[0].geometry.cell(),
            shepr_core::geometry::CellPx::new(8, 16)
        );

        // A workspace laid out in an empty area has no geometry to launch with.
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 0, 0));
        assert!(!app.has_pending_agent_resume_candidates());
        assert!(app.pending_agent_resume_candidates().is_empty());

        // Neither has a workspace the server has not laid out.
        let mut unlaid = test_app();
        let workspace = shepr_mux::workspace::Workspace::test_new("unlaid");
        let unlaid_pane = workspace.tree().root();
        unlaid.state.test_set_workspaces(vec![workspace]);
        unlaid.state.seed_bookmark_index(Some(0));
        unlaid.state.terminal_mut(unlaid_pane).plan_agent_resume(
            crate::test_support::test_codex_plan(
                "shepr:codex\0codex\0Id\0probe-session",
                long_running_test_argv(),
            ),
        );
        assert!(!unlaid.has_pending_agent_resume_candidates());
        assert!(unlaid.pending_agent_resume_candidates().is_empty());
    }

    /// The plan stays on the terminal until its launch settles, so a save in
    /// between keeps the agent identity, and the pane is no longer a candidate
    /// once its runtime exists.
    #[tokio::test]
    async fn a_dispatched_resume_keeps_its_plan_until_the_launch_settles() {
        let _env = IsolatedEnv::new();
        let mut app = test_app();
        app.set_test_shell(shepr_test_support::fixture::idle_shell());
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        app.state.seed_bookmark_index(Some(0));
        app.state
            .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
        app.state
            .terminal_mut(pane_id)
            .plan_agent_resume(crate::test_support::test_codex_plan(
                "shepr:codex\0codex\0Id\0dispatched-session",
                long_running_test_argv(),
            ));
        report_test_host_theme(&mut app);

        assert!(app.start_pending_agent_resumes(Instant::now()).consumed);
        assert!(app.terminal_runtimes.get(&pane_id).is_some());
        assert!(
            app.state
                .terminal(pane_id)
                .expect("terminal")
                .agent_resume()
                .is_pending()
        );
        assert!(!app.has_pending_agent_resume_candidates());
        settle_resume_launches(&mut app).await;
        assert!(
            !app.state
                .terminal(pane_id)
                .expect("terminal")
                .agent_resume()
                .is_pending()
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
            let pane_id = workspace.tree().root();
            app.state.test_set_workspaces(vec![workspace]);
            app.state.seed_bookmark_index(Some(0));
            if missing_shell {
                app.set_test_shell("/__shepr_missing_resume_shell__");
            }
            let clock_now = app.clock.now;
            let terminal = app.state.terminal_mut(pane_id);
            if !missing_shell {
                let missing = crate::test_support::ScratchDir::new("resume-cwd")
                    .join("__shepr_missing_resume_cwd__");
                assert!(!missing.try_exists().expect("stat missing resume cwd"));
                // Restore builds a terminal from its saved cwd, which may have
                // disappeared; a live pane never reports a missing one.
                *terminal = shepr_mux::terminal::TerminalState::new(
                    shepr_core::absolute_path::AbsolutePath::new(missing)
                        .expect("scratch cwd is absolute"),
                );
            }
            let session = shepr_agent::resume::PersistedAgentSession::new(
                shepr_agent::AgentSource::parse("shepr:codex").expect("bundled source"),
                shepr_agent::Agent::Codex,
                shepr_agent::resume::AgentSessionRef::id("resume-test").expect("test precondition"),
            )
            .expect("test session is valid");
            terminal
                .ownership_mut()
                .set_persisted_agent_session(session.clone());
            terminal.plan_agent_resume(crate::test_support::test_codex_plan(
                "resume-test",
                long_running_test_argv(),
            ));
            // Restore seeds the resumed agent as detected, stamped by the
            // app's clock like the launch failure that later clears it: the
            // detector refuses an observation older than the last one.
            let _ = terminal
                .ownership_mut()
                .set_detected_state_with_screen_signals_at(
                    Some(shepr_agent::Agent::Codex),
                    shepr_agent::AgentState::Idle,
                    false,
                    false,
                    clock_now,
                );
            app.state
                .test_record_all_workspace_areas(Rect::new(0, 0, 100, 30));
            report_test_host_theme(&mut app);
            let now = Instant::now();
            assert!(app.start_pending_agent_resumes(now).consumed);
            settle_resume_launches(&mut app).await;
            assert!(app.terminal_runtimes.get(&pane_id).is_none());
            let terminal = app.state.terminal(pane_id).expect("terminal");
            assert!(!terminal.agent_resume().is_pending());
            assert_eq!(
                terminal.ownership().persisted_agent_session(),
                Some(&session)
            );
            assert!(terminal.restore_error().is_some());
            // No process will ever run here: the seeded detection goes.
            assert_eq!(terminal.ownership().detected_agent(), None);
            assert_eq!(terminal.ownership().effective_agent(), None);
            assert!(!app.has_pending_agent_resumes());
            assert!(!app.start_pending_agent_resumes(now).consumed);
        }
    }

    #[tokio::test]
    async fn a_resume_cwd_removed_before_launch_does_not_start_in_home() {
        use shepr_test_support::fixture::{self, Step};

        let _env = IsolatedEnv::new();
        let mut app = test_app();
        let scratch = ScratchDir::new("resume-cwd-race");
        let cwd = scratch.join("agent-session");
        std::fs::create_dir(&cwd).expect("create resume cwd");

        let shell = fixture::stand_in(
            scratch.path(),
            "resume-shell",
            &[Step::Sleep(std::time::Duration::from_secs(30))],
        );
        app.set_test_shell(&shell);

        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let plan =
            crate::test_support::test_codex_plan("resume-cwd-race", long_running_test_argv());
        let cwd =
            shepr_core::absolute_path::AbsolutePath::new(cwd).expect("scratch cwd is absolute");
        let terminal = app.state.terminal_mut(pane_id);
        *terminal = shepr_mux::terminal::TerminalState::new(cwd.clone());
        terminal.plan_agent_resume(plan.clone());

        // The directory the terminal was saved with disappears before the
        // launch; only the child's chdir finds out.
        std::fs::remove_dir(&cwd).expect("remove resume cwd");
        let outcome = app.start_pending_agent_resume(
            pane_id,
            &cwd,
            &plan,
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            Instant::now(),
        );
        assert_eq!(outcome, AttemptOutcome::Launched, "the fork is dispatched");
        settle_resume_launches(&mut app).await;

        assert!(
            app.terminal_runtimes.get(&pane_id).is_none(),
            "a stale resume cwd must not fall back to HOME"
        );
        let terminal = app.state.terminal(pane_id).expect("terminal");
        assert!(!terminal.agent_resume().is_pending());
        assert!(matches!(
            terminal.restore_error(),
            Some(shepr_mux::terminal::PaneStartFailure::DirectoryUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn pending_agent_resume_waits_for_live_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = shepr_mux::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        app.state.seed_bookmark_index(Some(0));
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        let terminal = app.state.terminal_mut(pane_id);
        terminal.plan_agent_resume(crate::test_support::test_codex_plan(
            "shepr:codex\0codex\0Id\0codex-session",
            marker_resume_test_argv(),
        ));

        assert!(!app.start_pending_agent_resumes(Instant::now()).consumed);
        assert!(app.terminal_runtimes.get(&pane_id).is_none());

        report_test_host_theme(&mut app);

        assert!(app.start_pending_agent_resumes(Instant::now()).consumed);
        assert!(app.terminal_runtimes.get(&pane_id).is_some());
        settle_resume_launches(&mut app).await;
        let terminal = app
            .state
            .terminal(pane_id)
            .expect("terminal should survive launch");
        assert!(!terminal.agent_resume().is_pending());

        let runtime = app
            .terminal_runtimes
            .get(&pane_id)
            .expect("pending resume should leave a shell runtime");
        let marker = "restored agent: shell quoted | marker";
        for _ in 0..20 {
            if runtime.read().detection_text().contains(marker) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime.read().detection_text().contains(marker),
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
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.seed_bookmark_index(Some(0));
        app.state
            .terminal_mut(pane_id)
            .plan_agent_resume(crate::test_support::test_codex_plan(
                "shepr:codex\0codex\0Id\0codex-session",
                long_running_test_argv(),
            ));

        let now = Instant::now();
        assert!(!app.start_pending_agent_resumes(now).consumed);
        assert!(
            app.start_pending_agent_resumes(now + PENDING_AGENT_RESUME_THEME_WAIT)
                .consumed
        );
        assert!(app.terminal_runtimes.get(&pane_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = shepr_mux::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.tree().root();
        let hidden_workspace = shepr_mux::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.tree().root();
        app.state
            .test_set_workspaces(vec![active_workspace, hidden_workspace]);
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.seed_bookmark_index(Some(0));
        report_test_host_theme(&mut app);
        for pane in [active_pane, hidden_pane] {
            app.state
                .terminal_mut(pane)
                .plan_agent_resume(crate::test_support::test_codex_plan(
                    &format!("shepr:codex\0codex\0Id\0{pane}"),
                    long_running_test_argv(),
                ));
        }

        let now = Instant::now();
        assert!(app.start_pending_agent_resumes(now).consumed);
        assert!(app.terminal_runtimes.get(&active_pane).is_some());
        assert!(app.terminal_runtimes.get(&hidden_pane).is_none());
        // The launch spaces the next one out; the wakeup is the barrier.
        let barrier = now + std::time::Duration::from_millis(100);
        assert!(!app.start_pending_agent_resumes(now).consumed);
        assert_eq!(app.pending_agent_resume_wakeup(), Some(barrier));
        assert!(app.start_pending_agent_resumes(barrier).consumed);
        assert!(app.terminal_runtimes.get(&hidden_pane).is_some());
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
        let hidden_pane = workspace.tree().root();
        // The split pane is focused, so it is the one the zoom shows.
        workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.set_zoomed(true);
        app.state.test_set_workspaces(vec![workspace]);
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.seed_bookmark_index(Some(0));
        report_test_host_theme(&mut app);
        app.state.terminal_mut(hidden_pane).plan_agent_resume(
            crate::test_support::test_codex_plan(
                "shepr:codex\0codex\0Id\0zoom-hidden-session",
                long_running_test_argv(),
            ),
        );

        assert!(app.start_pending_agent_resumes(Instant::now()).consumed);
        assert!(app.terminal_runtimes.get(&hidden_pane).is_some());
        settle_resume_launches(&mut app).await;
        assert!(
            !app.state
                .terminal(hidden_pane)
                .expect("hidden zoom pane terminal should still exist")
                .agent_resume()
                .is_pending(),
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
        let previous_pane = previous_workspace.tree().root();
        let current_workspace = shepr_mux::workspace::Workspace::test_new("current");
        app.state
            .test_set_workspaces(vec![previous_workspace, current_workspace]);
        app.state
            .test_record_all_workspace_areas(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.state.seed_bookmark_index(Some(1));
        report_test_host_theme(&mut app);
        app.state.terminal_mut(previous_pane).plan_agent_resume(
            crate::test_support::test_codex_plan(
                "shepr:codex\0codex\0Id\0codex-session",
                long_running_test_argv(),
            ),
        );

        assert!(app.start_pending_agent_resumes(Instant::now()).consumed);
        assert!(app.terminal_runtimes.get(&previous_pane).is_some());
        settle_resume_launches(&mut app).await;
        assert!(
            !app.state
                .terminal(previous_pane)
                .expect("previous terminal should still exist")
                .agent_resume()
                .is_pending(),
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
        let mut workspace = shepr_mux::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        let area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.test_set_workspaces(vec![workspace]);
        app.state.test_record_all_workspace_areas(area);
        app.state.seed_bookmark_index(Some(0));
        let target = crate::ui::SurfaceTarget {
            index: 0,
            id: app.state.ws(0).id(),
        };
        let content_rect = |app: &App| {
            let layout = crate::ui::compute_surface_for(
                &app.state,
                &app.terminal_runtimes,
                Some(target),
                area,
            );
            let info = layout
                .panes
                .iter()
                .find(|info| info.id == pane_id)
                .expect("the resumed pane is visible");
            (info.inner_rect.height, info.inner_rect.width)
        };
        let before_launch = content_rect(&app);
        report_test_host_theme(&mut app);
        app.state
            .terminal_mut(pane_id)
            .plan_agent_resume(crate::test_support::test_codex_plan(
                "shepr:codex\0codex\0Id\0codex-session",
                long_running_test_argv(),
            ));

        assert!(app.start_pending_agent_resumes(Instant::now()).consumed);
        let launched = app
            .terminal_runtimes
            .get(&pane_id)
            .expect("pending resume should launch")
            .current_size();
        assert_eq!(launched, before_launch);
        assert_eq!(launched, content_rect(&app));

        for (_, runtime) in app.terminal_runtimes.drain() {
            drop(runtime);
        }
    }
}
