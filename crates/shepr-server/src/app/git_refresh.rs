//! When Git status refreshes run, and what they are asked about. The
//! `shepr-git` worker owns the refresh and its cache; this side picks each
//! workspace's resolved cwd and known checkout key, and the completion event
//! is matched back to workspaces in `events.rs`.

use std::time::Instant;

use super::{App, GIT_REPO_DISCOVERY_REFRESH_INTERVAL};
use crate::limits::GIT_REMOTE_STATUS_REFRESH_INTERVAL;
use shepr_mux::events::AppEvent;
use shepr_protocol::WorkspaceId;

pub(crate) struct GitRefreshScheduler {
    pub(crate) next_git_remote_status_refresh: Instant,
    pub(crate) last_git_repo_discovery_refresh: Instant,
    pub(crate) git_refresh_in_flight: bool,
    pub(crate) git_refresh_due_after_in_flight: bool,
    pub(crate) git_identity_refresh_requested: bool,
    /// While a refresh is in flight, when the loop next wakes to ask the
    /// worker whether that refresh was lost.
    lost_refresh_check_at: Instant,
    worker: shepr_git::GitStatusWorker<WorkspaceId>,
}

impl GitRefreshScheduler {
    pub(crate) fn new(now: Instant, event_tx: tokio::sync::mpsc::Sender<AppEvent>) -> Self {
        Self {
            // The initial refresh is due immediately. Represent that directly
            // instead of manufacturing an instant before the clock's origin.
            next_git_remote_status_refresh: now,
            last_git_repo_discovery_refresh: now,
            git_refresh_in_flight: false,
            git_refresh_due_after_in_flight: false,
            git_identity_refresh_requested: false,
            lost_refresh_check_at: now,
            worker: shepr_git::GitStatusWorker::new(move |outcome| {
                // Fails only once the app dropped its event receiver, which
                // takes the in-flight flag this event would clear with it.
                // `blocking_send` can only panic before it delivers, which
                // the worker then reports as a lost refresh.
                event_tx
                    .blocking_send(AppEvent::GitStatusRefreshed { outcome })
                    .ok();
            }),
        }
    }

    fn deadline(&self, has_workspaces: bool) -> Option<Instant> {
        if !has_workspaces {
            return None;
        }
        if self.git_refresh_in_flight {
            return Some(self.lost_refresh_check_at);
        }
        Some(self.next_git_remote_status_refresh)
    }

    pub(crate) fn refresh_due_at(&self) -> Option<Instant> {
        (!self.git_refresh_in_flight).then_some(self.next_git_remote_status_refresh)
    }

    /// Settles an in-flight refresh the worker lost, and moves the next
    /// lost-refresh check past `now` so the loop does not spin on it.
    fn observe_worker(&mut self, now: Instant) {
        if self.worker.take_lost_refresh() {
            tracing::warn!(
                "git status worker stopped without publishing an accepted refresh; \
                 scheduling the next refresh on a new worker"
            );
            self.finish(now);
        }
        if self.git_refresh_in_flight && now >= self.lost_refresh_check_at {
            self.lost_refresh_check_at = refresh_deadline_after(now);
        }
    }

    fn mark_due(&mut self, now: Instant) {
        self.worker.invalidate();
        if self.git_refresh_in_flight {
            self.git_refresh_due_after_in_flight = true;
            return;
        }
        self.next_git_remote_status_refresh = now;
        self.git_refresh_due_after_in_flight = false;
    }

    pub(crate) fn finish(&mut self, now: Instant) {
        self.git_refresh_in_flight = false;
        if self.git_refresh_due_after_in_flight {
            self.mark_due(now);
        } else {
            self.next_git_remote_status_refresh = refresh_deadline_after(now);
        }
    }
}

fn refresh_deadline_after(now: Instant) -> Instant {
    now.checked_add(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
        .unwrap_or(now)
}

impl App {
    pub(crate) fn start_git_status_refresh_if_due(&mut self, now: Instant) {
        self.git_refresh.observe_worker(now);
        if self.state.workspaces.is_empty() {
            self.git_refresh.worker.clear();
            return;
        }
        let Some(deadline) = self.git_refresh.refresh_due_at() else {
            return;
        };

        if now < deadline {
            return;
        }

        let refresh_repo_discovery = self.git_refresh.git_identity_refresh_requested
            || now.saturating_duration_since(self.git_refresh.last_git_repo_discovery_refresh)
                >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        let targets = self.workspace_git_refresh_targets(refresh_repo_discovery);
        // Each client draws the sidebar from its own config, so the server
        // refresh always computes the complete Git status.
        if targets.is_empty() {
            self.git_refresh.worker.clear();
            self.git_refresh.next_git_remote_status_refresh = refresh_deadline_after(now);
            self.git_refresh.git_identity_refresh_requested = false;
            return;
        }

        self.git_refresh.git_identity_refresh_requested = false;
        if refresh_repo_discovery {
            self.git_refresh.last_git_repo_discovery_refresh = now;
        }
        // `git_refresh_in_flight` is cleared by `GitStatusRefreshed`, which
        // the worker publishes once for every refresh it accepts, a
        // panicking one included, or by `observe_worker` when the worker
        // thread stopped before publishing. A worker that cannot be started
        // accepts nothing, so the flag stays clear and the next refresh is
        // scheduled here. Otherwise one bad refresh would stop Git status
        // updates for the rest of the process.
        match self.git_refresh.worker.refresh(targets) {
            Ok(()) => {
                self.git_refresh.git_refresh_in_flight = true;
                self.git_refresh.lost_refresh_check_at = refresh_deadline_after(now);
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to start the git status worker");
                self.git_refresh.next_git_remote_status_refresh = refresh_deadline_after(now);
            }
        }
    }

    pub(crate) fn request_git_identity_refresh(&mut self, now: Instant) {
        self.git_refresh.git_identity_refresh_requested = true;
        self.mark_git_status_refresh_due(now);
    }

    pub(crate) fn mark_git_status_refresh_due(&mut self, now: Instant) {
        self.git_refresh.mark_due(now);
    }

    /// Poll Git status and the workspace identity while workspaces exist.
    /// Runtime cwd changes without OSC 7 are discovered on this schedule too.
    /// While a refresh is in flight this is the next lost-refresh check.
    pub(crate) fn git_refresh_deadline(&self) -> Option<Instant> {
        self.git_refresh.deadline(!self.state.workspaces.is_empty())
    }

    /// One target per workspace: its resolved cwd and, unless repositories
    /// are being rediscovered, the checkout key its admitted identity holds
    /// for that cwd.
    fn workspace_git_refresh_targets(
        &self,
        refresh_repo_discovery: bool,
    ) -> Vec<shepr_git::RefreshTarget<WorkspaceId>> {
        self.state
            .workspaces
            .iter()
            .filter_map(|ws| {
                let cwd =
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)?;
                let known_key = if refresh_repo_discovery {
                    None
                } else {
                    ws.git_status_key_for_cwd(&cwd).cloned()
                };
                Some(shepr_git::RefreshTarget {
                    owner: ws.id,
                    cwd,
                    known_key,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::test_support::*;
    use shepr_git::{GitBranch, GitStatus, GitStatusKey};
    use shepr_mux::workspace::Workspace;

    fn admit_cached_identity(ws: &mut Workspace, cwd: &Path, key: PathBuf) {
        let status = GitStatus {
            cwd: cwd.to_path_buf(),
            key: GitStatusKey::Checkout(key),
            label: "test".into(),
            branch: GitBranch::OutsideRepository,
            ahead_behind: None,
        };
        ws.apply_git_status(status, Some(cwd));
    }

    fn refreshed(event: AppEvent) -> shepr_git::RefreshOutcome<WorkspaceId> {
        let AppEvent::GitStatusRefreshed { outcome } = event else {
            panic!("expected Git refresh");
        };
        outcome
    }

    #[test]
    fn git_refresh_target_collection_does_not_discover_uncached_cwd() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let scratch = crate::test_support::ScratchDir::new("uncached-cwd");
        let cwd = scratch.join("cwd");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        app.state.test_push_workspace(ws);

        let targets = app.workspace_git_refresh_targets(false);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].cwd, cwd);
        assert_eq!(targets[0].known_key, None);
    }

    #[test]
    fn git_refresh_target_collection_reuses_matching_cached_key() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let cache_key = PathBuf::from("/repo");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        admit_cached_identity(&mut ws, &cwd, cache_key.clone());
        app.state.test_push_workspace(ws);

        let targets = app.workspace_git_refresh_targets(false);

        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].known_key,
            Some(GitStatusKey::Checkout(cache_key))
        );
    }

    #[test]
    fn periodic_repo_discovery_ignores_cached_key_hints() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        admit_cached_identity(&mut ws, &cwd, PathBuf::from("/repo"));
        app.state.test_push_workspace(ws);

        let targets = app.workspace_git_refresh_targets(true);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].known_key, None);
    }

    #[test]
    fn cwd_identity_refresh_runs_once() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let config = shepr_config::ServerConfig::default();
        let mut app = test_app(&config);
        let mut workspace = Workspace::test_new("test");
        let scratch = crate::test_support::ScratchDir::new("git-refresh-identity");
        workspace.identity_cwd = scratch.join("cwd");
        app.state.test_push_workspace(workspace);
        let now = Instant::now();

        app.request_git_identity_refresh(now);

        assert!(app.git_refresh_deadline().is_some());
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh.git_refresh_in_flight);
        assert!(!app.git_refresh.git_identity_refresh_requested);
        wait_for_git_refresh(&mut app);
    }

    #[test]
    fn server_refreshes_branch_and_ahead_behind_without_client_settings() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("git-full-demand");
        let git = |args: &[&str]| {
            let output = shepr_test_support::command_in_scratch("git", "git-full-demand-command")
                .current_dir(scratch.path())
                .args(args)
                .output()
                .expect("run fixture Git");
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        git(&["init", "--initial-branch=main"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        git(&["branch", "upstream"]);
        git(&["branch", "--set-upstream-to=upstream"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "ahead",
        ]);
        let config = shepr_config::ServerConfig::default();
        let mut app = test_app(&config);
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = scratch.path().to_path_buf();
        let cwd = ws.identity_cwd.clone();
        admit_cached_identity(&mut ws, &cwd, cwd.clone());
        app.state.test_push_workspace(ws);
        let now = Instant::now();
        app.mark_git_status_refresh_due(now);
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh.git_refresh_in_flight);
        let outcome = refreshed(app.event_rx.blocking_recv().expect("refresh"));
        assert_eq!(outcome.statuses.len(), 1);
        let status = &outcome.statuses[0].status;
        assert_eq!(status.branch.as_deref(), Some("main"));
        let counts = status.ahead_behind.expect("ahead/behind");
        assert_eq!(counts.ahead, 1);
        assert_eq!(counts.behind, 0);
    }

    #[test]
    fn moved_cwd_without_osc7_rediscovers_the_label_identity() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let config = shepr_config::ServerConfig::default();
        let mut app = test_app(&config);
        let mut ws = Workspace::test_new("test");
        ws.custom_name = None;
        // The shell `cd`ed without reporting OSC 7: only the resolved cwd moved.
        let scratch = crate::test_support::ScratchDir::new("moved-cwd");
        ws.identity_cwd = scratch.join("moved");
        app.state.test_push_workspace(ws);
        let now = Instant::now();
        app.mark_git_status_refresh_due(now);

        app.start_git_status_refresh_if_due(now);

        assert!(app.git_refresh.git_refresh_in_flight);
        wait_for_git_refresh(&mut app);
    }

    #[test]
    fn undiscovered_workspace_identity_is_refreshed() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let config = shepr_config::ServerConfig::default();
        let mut app = test_app(&config);
        let mut ws = Workspace::test_new("test");
        let scratch = crate::test_support::ScratchDir::new("undiscovered-workspace");
        ws.identity_cwd = scratch.join("cwd");
        ws.mark_identity_undiscovered();
        app.state.test_push_workspace(ws);

        let targets = app.workspace_git_refresh_targets(false);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].known_key, None);

        let now = Instant::now();
        app.mark_git_status_refresh_due(now);
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh.git_refresh_in_flight);
        wait_for_git_refresh(&mut app);
    }

    #[test]
    fn refreshed_status_is_applied_to_its_workspace() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let config = shepr_config::ServerConfig::default();
        let mut app = test_app(&config);
        let mut ws = Workspace::test_new("test");
        ws.custom_name = None;
        let scratch = crate::test_support::ScratchDir::new("refreshed-label");
        ws.identity_cwd = scratch.join("labelled");
        std::fs::create_dir_all(&ws.identity_cwd).expect("create cwd");
        app.state.test_push_workspace(ws);
        let now = Instant::now();
        app.request_git_identity_refresh(now);
        app.start_git_status_refresh_if_due(now);

        let event = app
            .event_rx
            .blocking_recv()
            .expect("git refresh worker should report completion");
        app.handle_internal_event(event);

        assert!(!app.git_refresh.git_refresh_in_flight);
        assert_eq!(app.state.workspaces[0].display_name(), "labelled");
        assert_eq!(
            app.state.workspaces[0].branch_state(),
            Some(&GitBranch::OutsideRepository)
        );
    }

    #[test]
    fn headless_deadline_can_suppress_git_refresh_timer() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        app.state.test_push_workspace(Workspace::test_new("test"));
        let now = Instant::now();
        app.mark_git_status_refresh_due(now);

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, false),
            None
        );
        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            Some(now)
        );
    }

    #[test]
    fn git_refresh_due_request_survives_in_flight_refresh() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let now = Instant::now();
        app.git_refresh.git_refresh_in_flight = true;

        app.mark_git_status_refresh_due(now);
        assert!(app.git_refresh.git_refresh_due_after_in_flight);

        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            outcome: shepr_git::RefreshOutcome::empty(),
        });

        assert!(!app.git_refresh.git_refresh_in_flight);
        assert!(!app.git_refresh.git_refresh_due_after_in_flight);
        assert_eq!(app.git_refresh_deadline(), None);

        app.state.test_push_workspace(Workspace::test_new("test"));
        let deadline = app
            .git_refresh_deadline()
            .expect("refresh should be due once a workspace exists");
        assert!(deadline <= Instant::now());
    }

    #[test]
    fn empty_refresh_after_a_panic_unwedges_the_refresh_deadline() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        app.state.test_push_workspace(Workspace::test_new("test"));
        app.git_refresh.git_refresh_in_flight = true;
        assert_eq!(app.git_refresh.refresh_due_at(), None);

        // A panicking refresh is published as an empty outcome.
        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            outcome: shepr_git::RefreshOutcome::empty(),
        });

        assert!(!app.git_refresh.git_refresh_in_flight);
        assert!(app.git_refresh.refresh_due_at().is_some());
    }

    #[test]
    fn lost_refresh_clears_in_flight_and_schedules_the_next_refresh() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let mut ws = Workspace::test_new("test");
        let scratch = crate::test_support::ScratchDir::new("git-refresh-lost");
        ws.identity_cwd = scratch.join("cwd");
        app.state.test_push_workspace(ws);
        let (sender, outcomes) = std::sync::mpsc::channel();
        let panicked = std::sync::atomic::AtomicBool::new(false);
        app.git_refresh.worker = shepr_git::GitStatusWorker::new(move |outcome| {
            if !panicked.swap(true, std::sync::atomic::Ordering::SeqCst) {
                panic!("publish failed");
            }
            sender.send(outcome).ok();
        });
        let start = Instant::now();
        app.mark_git_status_refresh_due(start);
        app.start_git_status_refresh_if_due(start);
        assert!(app.git_refresh.git_refresh_in_flight);

        let give_up = start + std::time::Duration::from_secs(30);
        let lost_at = loop {
            let now = Instant::now();
            app.start_git_status_refresh_if_due(now);
            if !app.git_refresh.git_refresh_in_flight {
                break now;
            }
            assert!(now < give_up, "lost refresh was not observed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        let next = app
            .git_refresh
            .refresh_due_at()
            .expect("next refresh is scheduled");
        assert!(next > lost_at);
        assert_eq!(app.git_refresh_deadline(), Some(next));

        app.start_git_status_refresh_if_due(next);
        assert!(app.git_refresh.git_refresh_in_flight);
        let outcome: shepr_git::RefreshOutcome<WorkspaceId> = outcomes
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("refresh on a new worker publishes");
        assert_eq!(outcome.statuses.len(), 1);
    }

    fn test_app(config: &shepr_config::ServerConfig) -> super::super::App {
        super::super::App::new(config, crate::app::AppPolicy::Suspended)
    }

    fn wait_for_git_refresh(app: &mut super::super::App) {
        let event = app
            .event_rx
            .blocking_recv()
            .expect("git refresh worker should report completion");
        assert!(matches!(event, AppEvent::GitStatusRefreshed { .. }));
    }
}
