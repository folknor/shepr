use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use super::{App, GIT_REPO_DISCOVERY_REFRESH_INTERVAL};
use crate::limits::GIT_REMOTE_STATUS_REFRESH_INTERVAL;
use shepr_mux::events::AppEvent;
use shepr_mux::git::{GitReadError, GitStatusCacheEntry, GitStatusDiscovery, WorkspaceGitStatus};

pub(crate) struct GitRefreshScheduler {
    pub(crate) next_git_remote_status_refresh: Instant,
    pub(crate) last_git_repo_discovery_refresh: Instant,
    pub(crate) git_refresh_in_flight: bool,
    pub(crate) git_refresh_due_after_in_flight: bool,
    pub(crate) git_identity_refresh_requested: bool,
    pub(crate) git_status_cache: HashMap<PathBuf, GitStatusCacheEntry>,
    reported_git_read_errors: HashSet<GitReadError>,
}

impl GitRefreshScheduler {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            // The initial refresh is due immediately. Represent that directly
            // instead of manufacturing an instant before the clock's origin.
            next_git_remote_status_refresh: now,
            last_git_repo_discovery_refresh: now,
            git_refresh_in_flight: false,
            git_refresh_due_after_in_flight: false,
            git_identity_refresh_requested: false,
            git_status_cache: HashMap::new(),
            reported_git_read_errors: HashSet::new(),
        }
    }

    fn deadline(&self, has_workspaces: bool) -> Option<Instant> {
        if self.git_refresh_in_flight || !has_workspaces {
            return None;
        }
        Some(self.next_git_remote_status_refresh)
    }

    fn mark_due(&mut self, now: Instant) {
        self.git_status_cache
            .retain(|_, entry| entry.fingerprint.is_some());
        if self.git_refresh_in_flight {
            self.git_refresh_due_after_in_flight = true;
            return;
        }
        self.next_git_remote_status_refresh = now;
        self.git_refresh_due_after_in_flight = false;
    }

    pub(crate) fn finish(
        &mut self,
        now: Instant,
        cache_updates: Vec<(PathBuf, GitStatusCacheEntry)>,
    ) {
        self.git_refresh_in_flight = false;
        let mut refreshed_keys = HashSet::with_capacity(cache_updates.len());
        for (key, entry) in cache_updates {
            refreshed_keys.insert(key.clone());
            for error in &entry.read_errors {
                if self.reported_git_read_errors.insert(error.clone()) {
                    tracing::warn!(%error, "git status read failed");
                }
            }
            self.git_status_cache.insert(key, entry);
        }
        if !refreshed_keys.is_empty() {
            // A successful refresh returns one update per current unique
            // workspace repository, releasing entries no workspace visits.
            self.git_status_cache
                .retain(|key, _| refreshed_keys.contains(key));
            let current_errors: HashSet<_> = self
                .git_status_cache
                .values()
                .flat_map(|entry| entry.read_errors.iter().cloned())
                .collect();
            // Keep error deduplication only while an active cache entry carries it.
            self.reported_git_read_errors
                .retain(|error| current_errors.contains(error));
        }
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshItem {
    workspace_id: String,
    resolved_identity_cwd: PathBuf,
    cache_key_hint: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshTarget {
    workspace_id: String,
    resolved_identity_cwd: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshJob {
    cache_key: PathBuf,
    cached: Option<GitStatusCacheEntry>,
    discovery: Option<GitStatusDiscovery>,
    targets: Vec<WorkspaceGitRefreshTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceGitRefreshOutput {
    results: Vec<WorkspaceGitStatus>,
    cache_updates: Vec<(PathBuf, GitStatusCacheEntry)>,
}

impl App {
    pub(crate) fn start_git_status_refresh_if_due(&mut self, now: Instant) {
        if self.state.workspaces.is_empty() {
            self.git_refresh.git_status_cache.clear();
            self.git_refresh.reported_git_read_errors.clear();
            return;
        }
        let Some(deadline) = self.git_refresh_deadline() else {
            return;
        };

        if now < deadline {
            return;
        }

        let refresh_repo_discovery = self.git_refresh.git_identity_refresh_requested
            || now.saturating_duration_since(self.git_refresh.last_git_repo_discovery_refresh)
                >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        let workspaces = self.workspace_git_refresh_items(refresh_repo_discovery);
        // Each client draws the sidebar from its own config, so the server
        // refresh always computes the complete Git status.
        if workspaces.is_empty() {
            self.git_refresh.git_status_cache.clear();
            self.git_refresh.reported_git_read_errors.clear();
            self.git_refresh.next_git_remote_status_refresh = refresh_deadline_after(now);
            self.git_refresh.git_identity_refresh_requested = false;
            return;
        }

        self.git_refresh.git_refresh_in_flight = true;
        let event_tx = self.event_tx.clone();
        let cache = self.git_refresh.git_status_cache.clone();
        self.git_refresh.git_identity_refresh_requested = false;
        if refresh_repo_discovery {
            self.git_refresh.last_git_repo_discovery_refresh = now;
        }
        // `git_refresh_in_flight` is only cleared by `GitStatusRefreshed`, so
        // the worker must send that event on every path. A panic inside the
        // refresh is caught and reported as an empty refresh; a failed thread
        // spawn clears the flag here. Otherwise one bad refresh would stop git
        // status updates for the rest of the process.
        let spawned = std::thread::Builder::new()
            .name("shepr-git-refresh".into())
            .spawn(move || {
                let output =
                    refresh_output_or_empty(|| refresh_workspace_git_statuses(workspaces, &cache));
                // Fails only once the app dropped its event receiver, which
                // takes the in-flight flag this event would clear with it.
                event_tx
                    .blocking_send(AppEvent::GitStatusRefreshed {
                        results: output.results,
                        cache_updates: output.cache_updates,
                    })
                    .ok();
            });
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "failed to spawn git status refresh thread");
            self.git_refresh.git_refresh_in_flight = false;
            self.git_refresh.next_git_remote_status_refresh = refresh_deadline_after(now);
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
    pub(crate) fn git_refresh_deadline(&self) -> Option<Instant> {
        self.git_refresh.deadline(!self.state.workspaces.is_empty())
    }

    fn workspace_git_refresh_items(
        &self,
        refresh_repo_discovery: bool,
    ) -> Vec<WorkspaceGitRefreshItem> {
        self.state
            .workspaces
            .iter()
            .filter_map(|ws| {
                let cwd =
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)?;
                let cache_key_hint = (!refresh_repo_discovery && ws.cached_identity_cwd == cwd)
                    .then(|| ws.cached_git_status_key.clone());
                Some(WorkspaceGitRefreshItem {
                    workspace_id: ws.id.to_string(),
                    resolved_identity_cwd: cwd,
                    cache_key_hint,
                })
            })
            .collect()
    }
}

fn deduplicate_git_refresh_items(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
) -> Vec<WorkspaceGitRefreshJob> {
    let mut indexes = HashMap::<PathBuf, usize>::new();
    let mut jobs = Vec::<WorkspaceGitRefreshJob>::new();

    for item in items {
        let reconcile = item.cache_key_hint.is_none();
        let (cache_key, discovery) = match item.cache_key_hint {
            Some(cache_key) => (cache_key, None),
            None => {
                let discovery = shepr_mux::git::git_status_discovery(&item.resolved_identity_cwd);
                (discovery.cache_key().to_path_buf(), Some(discovery))
            }
        };
        let target = WorkspaceGitRefreshTarget {
            workspace_id: item.workspace_id,
            resolved_identity_cwd: item.resolved_identity_cwd,
        };
        if let Some(&index) = indexes.get(&cache_key) {
            jobs[index].cached = jobs[index].cached.take().filter(|_| !reconcile);
            if jobs[index].discovery.is_none() {
                jobs[index].discovery = discovery;
            }
            jobs[index].targets.push(target);
            continue;
        }

        let cached = cache.get(&cache_key).filter(|_| !reconcile).cloned();
        indexes.insert(cache_key.clone(), jobs.len());
        jobs.push(WorkspaceGitRefreshJob {
            cache_key,
            cached,
            discovery,
            targets: vec![target],
        });
    }

    jobs
}

/// Runs a refresh, turning a panic into an empty result so the caller can
/// still report completion.
fn refresh_output_or_empty(
    refresh: impl FnOnce() -> WorkspaceGitRefreshOutput,
) -> WorkspaceGitRefreshOutput {
    #[expect(
        clippy::disallowed_methods,
        reason = "the git refresh thread must always send its completion event: the loop \
                  holds git_refresh_in_flight until it arrives, so an uncaught panic in a \
                  git probe would stop sidebar git refreshes for the server's lifetime"
    )]
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(refresh));
    caught.unwrap_or_else(|_| {
        tracing::warn!("git status refresh panicked; reporting an empty refresh");
        WorkspaceGitRefreshOutput {
            results: Vec::new(),
            cache_updates: Vec::new(),
        }
    })
}

fn refresh_workspace_git_statuses(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<PathBuf, GitStatusCacheEntry>,
) -> WorkspaceGitRefreshOutput {
    let mut results = Vec::new();
    let mut cache_updates = Vec::new();

    for job in deduplicate_git_refresh_items(items, cache) {
        let (snapshot, cache_entry) = match job.discovery {
            Some(discovery) => shepr_mux::git::git_status_snapshot_for_discovery(discovery),
            None => {
                shepr_mux::git::git_status_snapshot_for_cwd(&job.cache_key, job.cached.as_ref())
            }
        };
        if let Some(cache_entry) = cache_entry {
            cache_updates.push((job.cache_key.clone(), cache_entry));
        }
        results.extend(job.targets.into_iter().map(move |target| {
            snapshot.clone().into_workspace_status(
                target.workspace_id,
                target.resolved_identity_cwd,
                job.cache_key.clone(),
            )
        }));
    }

    WorkspaceGitRefreshOutput {
        results,
        cache_updates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_mux::workspace::Workspace;

    #[test]
    fn git_refresh_deduplicates_workspaces_with_same_cache_key() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = crate::test_support::ScratchDir::new("git-refresh-dedupe");
        let repo = scratch.to_path_buf();
        let nested = repo.join("nested");
        let other = repo.join("other");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        // The repository as Git lays one out, in plain files: deduplication
        // only needs the checkout discovered.
        std::fs::create_dir_all(repo.join(".git/objects")).expect("create git objects dir");
        std::fs::create_dir_all(repo.join(".git/refs/heads")).expect("create git refs dir");
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write git HEAD");

        let output = refresh_workspace_git_statuses(
            vec![
                WorkspaceGitRefreshItem {
                    workspace_id: "one".into(),
                    resolved_identity_cwd: nested.clone(),
                    cache_key_hint: None,
                },
                WorkspaceGitRefreshItem {
                    workspace_id: "two".into(),
                    resolved_identity_cwd: other.clone(),
                    cache_key_hint: None,
                },
            ],
            &HashMap::new(),
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(
            output.cache_updates[0].0,
            std::fs::canonicalize(&repo).expect("canonical repo path")
        );
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].workspace_id, "one");
        assert_eq!(output.results[0].resolved_identity_cwd, nested);
        assert_eq!(output.results[1].workspace_id, "two");
        assert_eq!(output.results[1].resolved_identity_cwd, other);
    }

    #[test]
    fn git_read_failures_are_reported_once_per_distinct_cause() {
        let now = Instant::now();
        let path = PathBuf::from("/repo");
        let error = GitReadError::Spawn {
            cwd: path.clone(),
            message: "git is unavailable".into(),
        };
        let cache_entry = || GitStatusCacheEntry {
            fingerprint: None,
            retry_after: None,
            snapshot: shepr_mux::git::WorkspaceGitStatusSnapshot {
                repo_root: None,
                branch: None,
                ahead_behind: None,
            },
            read_errors: vec![error.clone()],
        };
        let mut scheduler = GitRefreshScheduler::new(now);

        scheduler.finish(now, vec![(path.clone(), cache_entry())]);
        scheduler.finish(now, vec![(path, cache_entry())]);

        assert_eq!(scheduler.reported_git_read_errors.len(), 1);
    }

    #[test]
    fn shared_root_repo_refresh_keeps_workspace_specific_labels() {
        let cache_key = PathBuf::from("/");
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: Some(Instant::now() + std::time::Duration::from_secs(30)),
            snapshot: shepr_mux::git::WorkspaceGitStatusSnapshot {
                repo_root: Some(cache_key.clone()),
                branch: Some("main".into()),
                ahead_behind: None,
            },
            read_errors: Vec::new(),
        };
        let items = ["alpha", "beta"]
            .into_iter()
            .map(|name| WorkspaceGitRefreshItem {
                workspace_id: name.into(),
                resolved_identity_cwd: cache_key.join(name),
                cache_key_hint: Some(cache_key.clone()),
            })
            .collect();

        let output = refresh_workspace_git_statuses(items, &HashMap::from([(cache_key, cached)]));

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].auto_label, "alpha");
        assert_eq!(output.results[1].auto_label, "beta");
        assert_eq!(output.results[0].branch.as_deref(), Some("main"));
        assert_eq!(output.results[1].branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_refresh_item_collection_does_not_discover_uncached_cwd() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let scratch = crate::test_support::ScratchDir::new("uncached-cwd");
        let cwd = scratch.join("cwd");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(false);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].resolved_identity_cwd, cwd);
        assert_eq!(items[0].cache_key_hint, None);
    }

    #[test]
    fn git_refresh_item_collection_reuses_matching_cached_key() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let cache_key = PathBuf::from("/repo");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = cache_key.clone();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(false);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, Some(cache_key));
    }

    #[test]
    fn periodic_repo_discovery_ignores_cached_key_hints() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let cwd = PathBuf::from("/repo/deep/nested");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.cached_identity_cwd = cwd;
        ws.cached_git_status_key = PathBuf::from("/repo");
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(true);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, None);
        let cache_key = items[0].resolved_identity_cwd.clone();
        let cached = GitStatusCacheEntry {
            fingerprint: None,
            retry_after: None,
            snapshot: shepr_mux::git::WorkspaceGitStatusSnapshot {
                repo_root: None,
                branch: None,
                ahead_behind: None,
            },
            read_errors: Vec::new(),
        };
        let jobs = deduplicate_git_refresh_items(items, &HashMap::from([(cache_key, cached)]));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cached, None);
    }

    #[test]
    fn cwd_identity_refresh_runs_once() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let config = shepr_config::ServerConfig::default();
        let mut app = test_app(&config);
        let mut workspace = Workspace::test_new("test");
        let scratch = crate::test_support::ScratchDir::new("git-refresh-identity");
        workspace.identity_cwd = scratch.join("cwd");
        app.state.workspaces.push(workspace);
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
        ws.cached_identity_cwd = ws.identity_cwd.clone();
        ws.cached_git_status_key = ws.identity_cwd.clone();
        app.state.workspaces.push(ws);
        let now = Instant::now();
        app.mark_git_status_refresh_due(now);
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh.git_refresh_in_flight);
        let AppEvent::GitStatusRefreshed { results, .. } =
            app.event_rx.blocking_recv().expect("refresh")
        else {
            panic!("expected Git refresh");
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].branch.as_deref(), Some("main"));
        let counts = results[0].ahead_behind.expect("ahead/behind");
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
        app.state.workspaces.push(ws);
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
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items(false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key_hint, None);

        let now = Instant::now();
        app.mark_git_status_refresh_due(now);
        app.start_git_status_refresh_if_due(now);
        assert!(app.git_refresh.git_refresh_in_flight);
        wait_for_git_refresh(&mut app);
    }

    #[test]
    fn headless_deadline_can_suppress_git_refresh_timer() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        app.state.workspaces.push(Workspace::test_new("test"));
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
    fn explicit_git_refresh_invalidates_cached_non_git_results() {
        // The ceiling keeps the scratch directory from being discovered as
        // part of the checkout the scratch base sits in.
        let _env = crate::test_support::IsolatedEnv::new();
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let scratch = crate::test_support::ScratchDir::new("git-miss");
        let cwd = scratch.to_path_buf();
        let (_, entry) = shepr_mux::git::git_status_snapshot_for_cwd(&cwd, None);
        app.git_refresh
            .git_status_cache
            .insert(cwd.clone(), entry.expect("non-Git cache entry"));

        app.mark_git_status_refresh_due(Instant::now());

        assert!(app.git_refresh.git_status_cache.is_empty());
    }

    #[test]
    fn git_refresh_due_request_survives_in_flight_refresh() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        let now = Instant::now();
        app.git_refresh.git_refresh_in_flight = true;

        app.mark_git_status_refresh_due(now);
        assert!(app.git_refresh.git_refresh_due_after_in_flight);

        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!app.git_refresh.git_refresh_in_flight);
        assert!(!app.git_refresh.git_refresh_due_after_in_flight);
        assert_eq!(app.git_refresh_deadline(), None);

        app.state.workspaces.push(Workspace::test_new("test"));
        let deadline = app
            .git_refresh_deadline()
            .expect("refresh should be due once a workspace exists");
        assert!(deadline <= Instant::now());
    }

    #[test]
    fn panicking_git_refresh_still_reports_an_empty_result() {
        let output = refresh_output_or_empty(|| panic!("simulated git refresh failure"));

        assert!(output.results.is_empty());
        assert!(output.cache_updates.is_empty());
    }

    #[test]
    fn empty_refresh_after_a_panic_unwedges_the_refresh_deadline() {
        let mut app = test_app(&shepr_config::ServerConfig::default());
        app.state.workspaces.push(Workspace::test_new("test"));
        app.git_refresh.git_refresh_in_flight = true;
        assert_eq!(app.git_refresh_deadline(), None);

        let output = refresh_output_or_empty(|| panic!("simulated git refresh failure"));
        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            results: output.results,
            cache_updates: output.cache_updates,
        });

        assert!(!app.git_refresh.git_refresh_in_flight);
        assert!(app.git_refresh_deadline().is_some());
    }

    fn test_app(config: &shepr_config::ServerConfig) -> super::super::App {
        super::super::App::new(config, crate::app::AppPolicy::Test)
    }

    fn wait_for_git_refresh(app: &mut super::super::App) {
        let event = app
            .event_rx
            .blocking_recv()
            .expect("git refresh worker should report completion");
        assert!(matches!(event, AppEvent::GitStatusRefreshed { .. }));
    }
}
