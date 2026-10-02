//! Application orchestration.
//!
//! `AppState` holds pure application data. `App` coordinates it with live
//! runtime concerns across focused modules under `app/`.

pub(crate) mod actions;
mod agent_resume;
mod agents;
mod api;
pub(crate) mod api_helpers;
mod creation;
mod events;
mod git_refresh;
mod host_theme;
mod ids;
mod pane_launch;
mod resume_schedule;
mod runtime;
mod session;
pub mod state;
mod terminal_titles;
mod window_title;

pub(crate) use events::PreparedPaneExit;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// One sample supplied by the server at the start of an iteration.
#[derive(Clone, Copy)]
pub(crate) struct AppClock {
    pub(crate) now: Instant,
    pub(crate) wall_now: SystemTime,
}

pub(crate) struct Outcome {
    pub(crate) response: shepr_api::error::ApiResult,
    pub(crate) view_changed: bool,
}

use crate::limits::{
    DEFAULT_WORKSPACE_RETRY_MAX, DEFAULT_WORKSPACE_RETRY_MIN, GIT_REMOTE_STATUS_REFRESH_INTERVAL,
    GIT_REPO_DISCOVERY_REFRESH_INTERVAL, PENDING_AGENT_RESUME_THEME_WAIT,
};

use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use shepr_mux::events::AppEvent;

pub(crate) use api::EndpointContext;
pub(crate) use api::session::SessionSnapshot;
pub use state::AppState;
pub(crate) use state::SpawnGeometry;

/// Whether the app restores a saved session at startup and persists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppPolicy {
    Production,
    Suspended,
}

impl AppPolicy {
    pub(crate) fn persists_session(self) -> bool {
        matches!(self, Self::Production)
    }
}

/// Full application: the pure `AppState` plus the runtime concerns it must
/// not hold - live pane runtimes, the event channels and render signals they
/// report through, and async I/O.
pub struct App {
    pub state: AppState,
    pub(crate) clock: AppClock,
    pub(crate) terminal_runtimes: shepr_mux::pane::PaneRuntimeRegistry,
    pub event_tx: mpsc::Sender<AppEvent>,
    pub(crate) event_rx: mpsc::Receiver<AppEvent>,
    pub(crate) policy: AppPolicy,
    pub(crate) git_refresh: git_refresh::GitRefreshScheduler,
    /// When deferred agent resumes may be attempted; see `resume_schedule`.
    pub(crate) resume_schedule: resume_schedule::ResumeSchedule,
    /// True after a live foreground client reports host colors this boot. A
    /// restored session theme remains the fallback until this report arrives.
    pub(crate) live_host_theme_reported: bool,
    /// The next time automatic workspace creation may retry after a failure;
    /// the loop's deadline wakes it then.
    default_workspace_retry_at: Option<Instant>,
    /// The delay the last failed automatic creation waited out, doubled on
    /// each consecutive failure up to its cap; `None` after a success.
    default_workspace_retry_delay: Option<Duration>,
    /// Panes whose runtime was replaced since the server last synced pane
    /// focus (an agent resume starting its shell). The new runtime has not
    /// been told about focus; `sync_pane_focus` drains this and re-sends the
    /// focus-in report for the ones that hold focus.
    pub(crate) runtimes_replaced_panes: Vec<shepr_core::layout::PaneId>,
    /// Deferred agent resume commands whose shell is still launching; each is
    /// typed into its pane when the launch settles (`pane_launch`).
    pending_resume_commands: std::collections::HashMap<shepr_protocol::TerminalId, bytes::Bytes>,
    pub(crate) session_saver: session::SessionSaver,
    /// Host name resolved once for the window title.
    hostname: String,
    /// Parsed `ui.window_title`.
    window_title_template: Option<shepr_config::WindowTitleTemplate>,
    pub(crate) persist_pane_history: bool,
    /// Last render-loop attempt, including a throttled hidden-only PTY skip.
    pub(crate) last_render_at: Option<Instant>,
    /// Last attempt that could update a connected presentation surface.
    pub(crate) last_presentation_at: Option<Instant>,
    pub render_notify: Arc<Notify>,
    /// This app's pane session teardowns, handed to every pane it spawns and
    /// waited on at exit.
    pane_teardowns: Arc<shepr_mux::pane::PaneTeardownTracker>,
    pub(crate) render_dirty: Arc<shepr_mux::render_signal::RenderSignal>,
    pub(crate) paths: shepr_config::AppPaths,
    /// Set when this boot's restore did not bring the saved session back in
    /// full; sent to every client that connects, for the life of the boot.
    pub(crate) restore_notice: Option<shepr_protocol::SessionRestoreNotice>,
}

pub(crate) use crate::limits::{APP_EVENT_CHANNEL_CAPACITY, APP_EVENT_DRAIN_LIMIT};

impl App {
    /// API requests reach the app through `HeadlessServer`, which owns their
    /// bounded receiver; the app holds no API channel.
    pub(crate) fn with_paths(
        config: &shepr_config::ValidatedServerConfig,
        paths: &shepr_config::AppPaths,
        lease: shepr_mux::persist::DataDirLease,
        policy: AppPolicy,
        clock: AppClock,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel::<AppEvent>(APP_EVENT_CHANNEL_CAPACITY);
        let render_notify = Arc::new(Notify::new());
        let pane_teardowns = Arc::new(shepr_mux::pane::PaneTeardownTracker::default());
        let render_dirty = Arc::new(shepr_mux::render_signal::RenderSignal::new());
        let settings = state::AppSettings::from_config(config);
        let hostname = shepr_platform::hostname().unwrap_or_default();

        // Try to restore previous session
        let mut restored_terminals = std::collections::HashMap::new();
        let mut restored_terminal_runtimes = shepr_mux::pane::PaneRuntimeRegistry::new();
        let mut pane_history_carry = shepr_mux::persist::HistoryCarry::default();
        let paths = paths.clone();
        let load = policy
            .persists_session()
            .then(|| shepr_mux::persist::load(&lease));
        let backup_dir = || {
            shepr_mux::persist::session_backup_directory(lease.directory())
                .display()
                .to_string()
        };
        // What every client of this boot is told about a session that did
        // not come back in full; `None` when there is nothing to tell.
        let mut restore_notice = None;
        let mut history_digest = None;
        let snapshot = match load {
            Some(shepr_mux::persist::SessionLoad::Loaded {
                snapshot,
                history_digest: digest,
            }) => {
                history_digest = digest;
                Some(snapshot)
            }
            Some(shepr_mux::persist::SessionLoad::Unusable(reason)) => {
                restore_notice = Some(shepr_protocol::SessionRestoreNotice {
                    loss: shepr_protocol::SessionRestoreLoss::Unusable { reason },
                    backup_dir: backup_dir(),
                });
                None
            }
            Some(shepr_mux::persist::SessionLoad::Missing) | None => None,
        };
        let restored_host_theme = snapshot
            .as_ref()
            .map_or_default(|snapshot| snapshot.host_theme.to_theme());
        // Whether the first save must copy the on-disk session into
        // `session-backups` before replacing it: the file either could not be
        // loaded, or restore discarded saved workspace or pane data still only
        // present in it.
        let mut protect_unloaded = policy.persists_session() && snapshot.is_none();
        let (workspaces, active) = if let Some(snap) = snapshot {
            let history = config
                .experimental()
                .pane_history
                .then(|| shepr_mux::persist::load_history(&lease, history_digest.as_deref()))
                .flatten();
            // No view exists yet, so each workspace is laid out in the headless
            // area (what the server lays out against until a client
            // attaches), and each restored pane starts at its own size in
            // it. The saved host theme supplies colours until a live client
            // reports its own.
            let socket_path = paths.server_address().socket().to_path_buf();
            let restored = shepr_mux::persist::restore(
                &snap,
                history.as_ref(),
                settings.pane_geometry_in(settings.headless_rect()),
                settings.pane_scrollback_limit_bytes,
                shepr_mux::pane::PaneShellConfig::new(
                    &settings.default_shell,
                    settings.login_shell,
                ),
                &socket_path,
                config.session().resume_agents_on_restore,
                &event_tx,
                &render_notify,
                &render_dirty,
                &pane_teardowns,
                clock.now,
            );
            restored_terminals = restored.terminals;
            restored_terminal_runtimes = restored.terminal_runtimes.into();
            pane_history_carry = restored.history_carry;
            let loss = shepr_protocol::SessionRestoreLoss::partial(
                restored.dropped_workspaces,
                restored.restore_damage,
            );
            let restore_was_partial = loss.is_some();
            if let Some(loss) = loss {
                protect_unloaded = true;
                warn!(
                    dropped_workspaces = restored.dropped_workspaces,
                    restore_damage = restored.restore_damage,
                    "session restore discarded saved data; the saved session is backed up to session-backups before the first save"
                );
                restore_notice = Some(shepr_protocol::SessionRestoreNotice {
                    loss,
                    backup_dir: backup_dir(),
                });
            }
            let outcome = if restore_was_partial {
                "partial"
            } else if restored.workspaces.is_empty() {
                "empty"
            } else {
                "ok"
            };
            crate::logging::session_restored(
                &lease
                    .directory()
                    .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME),
                restored.workspaces.len(),
                outcome,
            );
            if restored.workspaces.is_empty() {
                (Vec::new(), None)
            } else {
                (restored.workspaces, restored.active)
            }
        } else {
            (Vec::new(), None)
        };
        // From here on the persister is the one owner of the data directory:
        // it holds the lease, the writer and the carried pane history. An app
        // that persists nothing only holds the lease, with no thread and no
        // writer. Each finished job fires `save_finished`, which the event
        // loop waits on to reap the save.
        let save_finished = std::sync::Arc::new(tokio::sync::Notify::new());
        let persister = if policy.persists_session() {
            shepr_mux::persist::SessionPersister::spawn(
                lease,
                protect_unloaded,
                pane_history_carry,
                std::sync::Arc::clone(&save_finished),
            )
        } else {
            shepr_mux::persist::SessionPersister::lease_only(
                lease,
                std::sync::Arc::clone(&save_finished),
            )
        };

        info!(
            pane_scrollback_limit_bytes = settings.pane_scrollback_limit_bytes,
            "using pane scrollback configuration"
        );

        let mut state = AppState {
            clock_now: clock.now,
            terminals: std::collections::HashMap::new(),
            workspaces,
            bookmark: None,
            bookmark_position: 0,
            should_quit: false,
            workspace_geometry: std::collections::HashMap::new(),
            settings,
            next_agent_state_change_seq: 0,
            host_terminal_appearance: None,
            host_terminal_appearance_explicit: false,
            host_terminal_theme: restored_host_theme,
            session_dirty: false,
            shell_projection_revision: 0,
        };

        state.set_bookmark_index(active);
        state.terminals = restored_terminals;
        // Restored workspaces get their Git identity (label, branch, space)
        // from the first background Git refresh, not from a synchronous walk
        // here: the refresh is due immediately (see
        // `last_git_remote_status_refresh` below) and discovers every
        // workspace whose resolved cwd differs from its cached identity.

        let mut app = Self {
            state,
            clock,
            terminal_runtimes: restored_terminal_runtimes,
            event_tx,
            event_rx,
            git_refresh: git_refresh::GitRefreshScheduler::new(clock.now),
            resume_schedule: resume_schedule::ResumeSchedule::new(
                PENDING_AGENT_RESUME_THEME_WAIT,
                Duration::from_millis(config.session().startup_per_agent_delay_ms.into()),
            ),
            live_host_theme_reported: false,
            default_workspace_retry_at: None,
            default_workspace_retry_delay: None,
            runtimes_replaced_panes: Vec::new(),
            pending_resume_commands: std::collections::HashMap::new(),
            session_saver: session::SessionSaver::new(persister, save_finished),
            hostname,
            window_title_template: None,
            persist_pane_history: config.experimental().pane_history,
            last_render_at: None,
            last_presentation_at: None,
            policy,
            render_notify,
            pane_teardowns,
            render_dirty,
            paths,
            restore_notice,
        };
        app.configure_validated_window_title(config.ui().window_title.as_ref());
        app
    }

    /// The server supplies a fresh sample before dispatching an iteration.
    pub(crate) fn set_clock(&mut self, clock: AppClock) {
        self.clock = clock;
        self.state.clock_now = clock.now;
    }

    /// The channels a newly spawned pane runtime reports through. Every call
    /// that spawns a pane (workspace or split creation) takes these;
    /// the workspace tree does not keep them.
    pub(crate) fn pane_spawn_handles(&self) -> shepr_mux::workspace::PaneSpawnHandles {
        shepr_mux::workspace::PaneSpawnHandles {
            events: self.event_tx.clone(),
            render_notify: Arc::clone(&self.render_notify),
            render_dirty: Arc::clone(&self.render_dirty),
            pane_teardowns: Arc::clone(&self.pane_teardowns),
            socket_path: self.paths.server_address().socket().to_path_buf(),
        }
    }

    /// Block until this app's pane session teardowns have finished, or
    /// `timeout` passes. Returns whether they all finished.
    pub(crate) fn wait_for_pane_teardowns(&self, timeout: Duration) -> bool {
        self.pane_teardowns.wait(timeout)
    }

    /// Creates the workspace an empty session gets, sized for `geometry`.
    /// False when the session already has a workspace, creation failed, or a
    /// retry is still held by backoff.
    /// The caller chooses the geometry and settles the clients' locations and
    /// geometry controllers (`create_automatic_workspace` on the server loop).
    pub(crate) fn create_default_workspace(&mut self, geometry: SpawnGeometry) -> bool {
        if !self.state.workspaces.is_empty() {
            self.default_workspace_retry_at = None;
            self.default_workspace_retry_delay = None;
            return false;
        }
        if self
            .default_workspace_retry_at
            .is_some_and(|retry_at| self.clock.now < retry_at)
        {
            return false;
        }
        self.default_workspace_retry_at = None;

        let cwd = self.resolve_new_terminal_cwd(None);
        let preserve_checkpoint = self.preserves_pane_exit_checkpoint();

        match self.create_workspace_without_save(&cwd, geometry) {
            Ok(_index) => {
                self.default_workspace_retry_delay = None;
                // Callers include non-mutating API requests and client
                // connects, so the shell projection is invalidated here.
                self.state.mark_shell_projection_dirty();
                if preserve_checkpoint {
                    // Automatic replacement is part of pane removal, not a new user mutation.
                    self.finish_checkpointed_pane_exit();
                }
                true
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to create default workspace");
                let retry_delay = self
                    .default_workspace_retry_delay
                    .map_or(DEFAULT_WORKSPACE_RETRY_MIN, |delay| {
                        delay.saturating_mul(2).min(DEFAULT_WORKSPACE_RETRY_MAX)
                    });
                self.default_workspace_retry_delay = Some(retry_delay);
                self.default_workspace_retry_at = Some(self.clock.now + retry_delay);
                false
            }
        }
    }
}

#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
pub(crate) use api::session::SnapshotAgent;
#[cfg(test)]
pub(crate) use api::test_support::exiting_test_command;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::SESSION_SAVE_DEBOUNCE;
    use crate::test_support::IsolatedEnv;
    use crate::test_support::*;
    use shepr_agent::detect::{Agent, AgentState};
    use shepr_config::ServerConfig;
    use shepr_mux::workspace::Workspace;
    use shepr_protocol::command::{
        EndpointCommand, EndpointReply, PaneSplitParams, PaneTarget, SplitDirection,
    };

    // Tests build apps that never persist their session; `AppPolicy::Test`
    // names that intent and behaves exactly as `Suspended`.
    impl AppPolicy {
        #[expect(
            non_upper_case_globals,
            reason = "spelled like a variant so call sites read as one"
        )]
        pub(crate) const Test: Self = Self::Suspended;
    }

    impl App {
        /// Test constructor: the app's files live in a fresh scratch directory.
        pub(crate) fn new(config: &ServerConfig, policy: AppPolicy) -> Self {
            use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
            let scratch = crate::test_support::ScratchDir::new("app");
            let paths = shepr_config::AppPaths::test_at(&scratch);
            let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
                config.clone(),
                paths.clone(),
            );
            let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir())
                .expect("test session lease");
            Self::with_paths(&config, &paths, lease, policy, test_clock())
        }

        /// Installs `runtime` for a pane in the same registry production uses.
        /// Panics if the pane is not in a workspace.
        pub(crate) fn insert_test_runtime(
            &mut self,
            pane_id: shepr_core::layout::PaneId,
            runtime: shepr_mux::pane::PaneRuntime,
        ) {
            let terminal_id = self
                .state
                .workspaces
                .iter()
                .find_map(|ws| ws.terminal_id(pane_id))
                .cloned()
                .expect("test runtime pane must be in a workspace");
            self.terminal_runtimes.insert(terminal_id, runtime);
        }

        /// Looks up a pane runtime through its workspace terminal link.
        pub(crate) fn test_runtime(
            &self,
            pane_id: shepr_core::layout::PaneId,
        ) -> &shepr_mux::pane::PaneRuntime {
            self.state
                .workspaces
                .iter()
                .find_map(|ws| ws.terminal_id(pane_id))
                .and_then(|terminal_id| self.terminal_runtimes.get(terminal_id))
                .expect("pane must have a live runtime")
        }
    }

    pub(super) fn test_clock() -> AppClock {
        AppClock {
            now: Instant::now(),
            wall_now: SystemTime::now(),
        }
    }

    fn test_app() -> App {
        let mut app = App::new(&ServerConfig::default(), crate::app::AppPolicy::Test);
        app.state.settings.default_shell = exiting_test_command().into();
        app
    }

    #[tokio::test]
    async fn restore_that_prunes_a_pane_backs_up_the_saved_session_before_the_first_save() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
        use shepr_mux::persist::snapshot::{
            DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SessionSnapshot, WorkspaceSnapshot,
        };

        let scratch = crate::test_support::ScratchDir::new("pruned-pane-backup");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            ServerConfig::default(),
            paths.clone(),
        );
        let data_dir = paths.data_dir().to_path_buf();
        let lease =
            shepr_mux::persist::DataDirLease::acquire(&data_dir).expect("test session lease");

        let pane = |cwd: std::path::PathBuf| PaneSnapshot {
            cwd,
            public_number: None,
            label: None,
            agent_session: None,
        };
        let snapshot = SessionSnapshot {
            version: shepr_mux::persist::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: Some("surviving workspace".into()),
                identity_cwd: scratch.path().to_path_buf(),
                next_public_pane_number: 0,
                layout: LayoutSnapshot::Split {
                    direction: DirectionSnapshot::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Pane(1)),
                    second: Box::new(LayoutSnapshot::Pane(2)),
                },
                panes: std::collections::HashMap::from([
                    (1, pane("relative-cwd".into())),
                    (2, pane(scratch.join("missing-cwd"))),
                ]),
                zoomed: false,
                focused: Some(2),
                root_pane: Some(2),
            }],
            active: Some(0),
        };
        let original = serde_json::to_vec(&snapshot).expect("encode the saved session");
        let session_file = data_dir.join("session.json");
        std::fs::write(&session_file, &original).expect("write the saved session");

        let mut app = App::with_paths(&config, &paths, lease, AppPolicy::Production, test_clock());
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.terminals.len(), 1);

        assert!(app.save_session_now(), "first save");
        let backups = data_dir.join("session-backups");
        let backup_files = std::fs::read_dir(&backups)
            .expect("backup directory")
            .map(|entry| entry.expect("backup entry").path())
            .collect::<Vec<_>>();
        assert_eq!(backup_files.len(), 1);
        assert_eq!(
            std::fs::read(&backup_files[0]).expect("read backup"),
            original
        );

        app.policy = AppPolicy::Test;
    }

    #[test]
    fn git_refresh_deadline_is_suppressed_while_in_flight() {
        let mut app = test_app();
        app.state.workspaces.push(Workspace::test_new("one"));
        app.git_refresh.git_refresh_in_flight = true;

        assert_eq!(app.git_refresh_deadline(), None);
    }

    #[test]
    fn unchanged_git_status_event_has_no_render_impact() {
        let mut app = test_app();
        app.git_refresh.git_refresh_in_flight = true;

        let changed = app.handle_internal_event_with_view_change(AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!changed);
        assert!(!app.git_refresh.git_refresh_in_flight);
    }

    #[test]
    fn git_status_event_clears_in_flight_refresh() {
        let mut app = test_app();
        app.git_refresh.git_refresh_in_flight = true;
        let previous_refresh = Instant::now() - Duration::from_secs(10);
        app.git_refresh.last_git_remote_status_refresh = previous_refresh;

        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!app.git_refresh.git_refresh_in_flight);
        assert!(app.git_refresh.last_git_remote_status_refresh > previous_refresh);
    }

    #[test]
    fn git_status_event_marks_render_dirty_when_status_changes() {
        let mut app = test_app();
        app.state.workspaces.push(Workspace::test_new("one"));
        let _ = app.render_dirty.take();
        let workspace_id = app.state.workspaces[0].id.to_string();
        let resolved_identity_cwd = app.state.workspaces[0]
            .resolved_identity_cwd()
            .expect("test precondition");

        app.handle_internal_event(AppEvent::GitStatusRefreshed {
            results: vec![shepr_mux::git::WorkspaceGitStatus {
                workspace_id,
                resolved_identity_cwd: resolved_identity_cwd.clone(),
                status_cache_key: resolved_identity_cwd,
                demand: shepr_mux::git::GitStatusRefreshDemand::ALL,
                auto_label: "one".into(),
                branch: Some("render-dirty-test".into()),
                ahead_behind: Some(shepr_mux::git::AheadBehind {
                    ahead: 1,
                    behind: 0,
                }),
                space: None,
            }],
            cache_updates: Vec::new(),
        });

        assert!(app.render_dirty.is_pending());
    }

    #[test]
    fn theme_uses_configured_name() {
        let mut config = ServerConfig::default();
        config.theme.name = Some("tokyo-night".to_string());

        let app = App::new(&config, crate::app::AppPolicy::Test);

        assert_eq!(app.state.settings.palette, state::Palette::tokyo_night());
    }

    #[test]
    fn theme_accent_applies_only_when_set_and_not_overridden_by_the_theme() {
        use ratatui::style::Color;

        let theme_accent = state::Palette::catppuccin().accent;
        assert_ne!(theme_accent, Color::Cyan, "test precondition");

        // Unset: the theme's accent.
        let config = ServerConfig::default();
        assert_eq!(
            config
                .resolve_palette()
                .expect("valid default theme")
                .accent,
            theme_accent
        );

        // Set explicitly: it applies over the theme accent.
        let mut config = ServerConfig::default();
        config.theme.accent = Some("cyan".into());
        assert_eq!(
            config.resolve_palette().expect("valid cyan accent").accent,
            Color::Cyan
        );

        config.theme.accent = Some("magenta".into());
        assert_eq!(
            config
                .resolve_palette()
                .expect("valid magenta accent")
                .accent,
            Color::Magenta
        );

        // `theme.custom.accent` wins over `theme.accent`.
        config.theme.custom = Some(shepr_config::CustomThemeColors {
            accent: Some("#010203".into()),
            ..Default::default()
        });
        assert_eq!(
            config
                .resolve_palette()
                .expect("valid custom accent")
                .accent,
            Color::Rgb(1, 2, 3)
        );
    }

    #[tokio::test]
    async fn create_default_workspace_creates_one_workspace_only_when_none_exist() {
        let mut app = test_app();
        let geometry = app.headless_spawn_geometry();

        assert!(app.create_default_workspace(geometry));
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspace_spawn_geometry(0), Some(geometry));
        assert!(!app.create_default_workspace(geometry));
        assert_eq!(app.state.workspaces.len(), 1);
    }

    #[test]
    fn workspace_seed_cwd_comes_from_the_named_workspace_not_the_bookmark() {
        let mut app = test_app();
        let mut first = Workspace::test_new("shepr");
        first.identity_cwd = std::path::PathBuf::from("/shepr-test/shepr");
        let mut second = Workspace::test_new("pion");
        second.identity_cwd = std::path::PathBuf::from("/shepr-test/pion");

        app.state.workspaces = vec![first, second];
        app.state.set_bookmark_index(Some(0));

        let followed = app
            .resolve_workspace_id(&app.state.workspaces[1].id.clone())
            .expect("test precondition");
        let seed_cwd = app
            .seed_cwd_from_workspace(followed)
            .expect("test precondition");

        assert_eq!(followed, 1);
        assert_eq!(seed_cwd, std::path::PathBuf::from("/shepr-test/pion"));
    }

    #[test]
    fn new_terminal_cwd_follow_uses_source_cwd() {
        let cwd = creation::resolve_new_terminal_cwd(
            &shepr_config::NewTerminalCwd::Follow,
            None,
            None,
            Some(std::path::PathBuf::from("/shepr-test/shepr-source")),
        );

        assert_eq!(cwd, std::path::PathBuf::from("/shepr-test/shepr-source"));
    }

    #[test]
    fn new_terminal_cwd_follow_without_source_uses_home() {
        let env = IsolatedEnv::new();
        let home = env.home();

        let cwd = creation::resolve_new_terminal_cwd(
            &shepr_config::NewTerminalCwd::Follow,
            Some(home.as_path()),
            None,
            None,
        );

        assert_eq!(cwd, env.home());
    }

    #[test]
    fn new_terminal_cwd_path_uses_configured_path() {
        let cwd = creation::resolve_new_terminal_cwd(
            &shepr_config::NewTerminalCwd::Path("/shepr-test/shepr-fixed".into()),
            None,
            None,
            Some(std::path::PathBuf::from("/shepr-test/shepr-source")),
        );

        assert_eq!(cwd, std::path::PathBuf::from("/shepr-test/shepr-fixed"));
    }

    fn split_of(app: &App, ws_idx: usize) -> EndpointCommand {
        let target_pane = app.state.workspaces[ws_idx].root_pane();
        EndpointCommand::PaneSplit(PaneSplitParams {
            pane_id: app
                .public_pane_id(ws_idx, target_pane)
                .expect("test precondition"),
            direction: SplitDirection::Right,
        })
    }

    fn shut_down_runtimes(app: &mut App) {
        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pane_split_request_focuses_the_new_pane_and_navigates_the_requester_only() {
        let env = IsolatedEnv::new();
        env.set("SHELL", exiting_test_command());

        let mut app = test_app();
        app.state.workspaces = vec![
            Workspace::test_new("api-pane-split-focused"),
            Workspace::test_new("api-pane-split-background"),
        ];
        app.state.ensure_test_terminals();
        app.state.set_bookmark_index(Some(0));
        let command = split_of(&app, 1);

        let outcome = app.handle_endpoint_command_in(command, &EndpointContext::without_geometry());
        let Ok(EndpointReply::PaneInfo { pane }) = outcome.result else {
            panic!("expected pane info");
        };

        let (reply_workspace, reply_pane) = app
            .resolve_pane_id(&pane.pane_id)
            .expect("test precondition");
        assert_eq!(reply_workspace, 1);
        assert!(app.state.workspaces[reply_workspace].contains_pane(reply_pane));
        assert_eq!(app.state.workspaces[1].focused_pane_id(), reply_pane);
        // A split navigates its requester to the split workspace; the effect is
        // returned and never applied to the session's bookmark.
        assert_eq!(outcome.navigate, app.public_workspace_id(1));
        assert_eq!(app.state.bookmark_index(), Some(0));
        // Whether the pane is focused depends on the requester's location,
        // which the server loop fills in.
        assert!(!pane.focused);
        let mut reply = EndpointReply::PaneInfo { pane };
        let viewed = app.public_workspace_id(1);
        app.fill_reply_focus(&mut reply, viewed.as_ref());
        assert!(matches!(&reply, EndpointReply::PaneInfo { pane } if pane.focused));
        let viewing_another = app.public_workspace_id(0);
        app.fill_reply_focus(&mut reply, viewing_another.as_ref());
        assert!(matches!(&reply, EndpointReply::PaneInfo { pane } if !pane.focused));

        shut_down_runtimes(&mut app);
    }

    #[tokio::test]
    async fn a_split_pane_child_sees_the_public_id_the_reply_names() {
        let _env = IsolatedEnv::new();
        let scratch = crate::test_support::ScratchDir::new("split-pane-id");
        let seen = scratch.join("pane-id");
        let shell = shepr_test_support::fixture::stand_in(
            scratch.path(),
            "sh",
            &[
                shepr_test_support::fixture::Step::To(seen.clone()),
                shepr_test_support::fixture::Step::PrintEnv("SHEPR_PANE_ID".into()),
                shepr_test_support::fixture::Step::Sleep(Duration::from_secs(30)),
            ],
        );

        let mut app = test_app();
        app.state.settings.default_shell = shell.to_str().expect("test precondition").into();
        // Its next public number differs from both its raw pane ids and its
        // pane count, so only the number the split took can match.
        app.state.workspaces = vec![Workspace::test_adversarial_identity_state()];
        app.state.ensure_test_terminals();
        app.state.set_bookmark_index(Some(0));
        let command = split_of(&app, 0);

        let outcome = app.handle_endpoint_command_in(command, &EndpointContext::without_geometry());
        let Ok(EndpointReply::PaneInfo { pane }) = outcome.result else {
            panic!("expected pane info");
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut child_pane_id = String::new();
        while Instant::now() < deadline {
            child_pane_id = std::fs::read_to_string(&seen).unwrap_or_default();
            if child_pane_id.ends_with('\n') {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(child_pane_id.trim_end(), pane.pane_id.as_str());
        let (ws_idx, pane_id) = app
            .resolve_pane_id(&pane.pane_id)
            .expect("the reply names a live pane");
        assert_eq!(app.public_pane_id(ws_idx, pane_id), Some(pane.pane_id));

        shut_down_runtimes(&mut app);
    }

    #[tokio::test]
    async fn pane_split_request_splits_in_half_and_keeps_default_input_routing() {
        let env = IsolatedEnv::new();
        env.set("SHELL", exiting_test_command());

        let mut app = test_app();
        app.state.workspaces = vec![Workspace::test_new("api-pane-split-half")];
        app.state.ensure_test_terminals();
        let command = split_of(&app, 0);

        let result = app.handle_endpoint_command(command);
        let Ok(EndpointReply::PaneInfo { pane }) = result else {
            panic!("expected pane info");
        };

        let splits = app.state.workspaces[0]
            .layout()
            .splits(shepr_core::geometry::Rect::new(0, 0, 100, 20));
        assert_eq!(splits.len(), 1);
        assert!((splits[0].ratio - 0.5).abs() < f32::EPSILON);
        let (_, response_pane_id) = app
            .resolve_pane_id(&pane.pane_id)
            .expect("test precondition");
        assert!(
            !app.state.workspaces[0]
                .pane_state(response_pane_id)
                .expect("test precondition")
                .right_click_passthrough
        );

        shut_down_runtimes(&mut app);
    }

    #[tokio::test]
    async fn a_split_sizes_against_the_recorded_geometry_and_only_then_the_requesters() {
        let env = IsolatedEnv::new();
        env.set("SHELL", exiting_test_command());

        let mut app = test_app();
        app.state.settings.pane_borders = shepr_config::PaneBordersConfig::Off;
        app.state.settings.pane_scrollbars = false;
        app.state.workspaces = vec![
            Workspace::test_new("recorded"),
            Workspace::test_new("unrecorded"),
        ];
        app.state.ensure_test_terminals();
        let recorded = SpawnGeometry {
            area: ratatui::layout::Rect::new(0, 0, 100, 20),
            cell_size: shepr_termio::host_term::cell_size::HostCellSize {
                width_px: 8,
                height_px: 16,
            },
        };
        let requester = SpawnGeometry {
            area: ratatui::layout::Rect::new(0, 0, 60, 10),
            cell_size: shepr_termio::host_term::cell_size::HostCellSize {
                width_px: 9,
                height_px: 18,
            },
        };
        let first_id = app.state.workspaces[0].id.clone();
        app.state.record_workspace_geometry(&first_id, recorded);
        let ctx = EndpointContext {
            requester_geometry: Some(requester),
        };

        for (ws_idx, expected) in [(0, recorded), (1, requester)] {
            let command = split_of(&app, ws_idx);
            let Ok(EndpointReply::PaneInfo { pane }) =
                app.handle_endpoint_command_in(command, &ctx).result
            else {
                panic!("expected pane info");
            };
            let (_, new_pane) = app
                .resolve_pane_id(&pane.pane_id)
                .expect("test precondition");
            let runtime = app.test_runtime(new_pane);
            let grid = runtime.grid_size();
            assert_eq!(
                u32::from(grid.rows.get()),
                u32::from(expected.area.height),
                "workspace {ws_idx} is sized against the geometry it has or the requester's"
            );
            assert_eq!(
                runtime.pixel_size(),
                Some((
                    u32::from(grid.cols.get()) * expected.cell_size.width_px,
                    u32::from(grid.rows.get()) * expected.cell_size.height_px
                )),
                "workspace {ws_idx}"
            );
        }

        shut_down_runtimes(&mut app);
    }

    #[test]
    fn pane_close_request_closes_only_the_target_pane_when_others_remain() {
        let mut app = test_app();
        let mut workspace = Workspace::test_new("api-pane-close");
        let target_pane = workspace.root_pane();
        workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_bookmark_index(Some(0));

        let target_pane_id = app
            .pane_info(0, target_pane)
            .expect("test precondition")
            .pane_id;

        let result = app.handle_endpoint_command(EndpointCommand::PaneClose(PaneTarget {
            pane_id: target_pane_id,
        }));

        assert!(matches!(result, Ok(EndpointReply::Done)));
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].pane_count(), 1);
        assert_eq!(app.state.workspaces[0].display_name(), "api-pane-close");
    }

    #[test]
    fn pane_close_request_closes_workspace_when_it_removes_the_last_pane() {
        let mut app = test_app();
        let workspace = Workspace::test_new("api-pane-close-last");
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_bookmark_index(Some(0));

        let target_pane = app.state.workspaces[0].root_pane();
        let target_pane_id = app
            .pane_info(0, target_pane)
            .expect("test precondition")
            .pane_id;

        let result = app.handle_endpoint_command(EndpointCommand::PaneClose(PaneTarget {
            pane_id: target_pane_id,
        }));

        assert!(matches!(result, Ok(EndpointReply::Done)));
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn session_dirty_flag_schedules_debounced_save() {
        let mut app = test_app();
        app.persist_for_test();
        let sample = AppClock {
            now: app.clock.now + Duration::from_secs(42),
            wall_now: app.clock.wall_now,
        };
        app.set_clock(sample);
        app.state.session_dirty = true;

        app.sync_session_save_schedule();

        assert!(!app.state.session_dirty);
        assert_eq!(
            app.session_saver.autosave_deadline(),
            Some(sample.now + SESSION_SAVE_DEBOUNCE)
        );
    }

    #[test]
    fn headless_next_loop_deadline_ignores_resize_poll() {
        let mut app = test_app();
        let now = Instant::now();
        app.session_saver
            .set_autosave_deadline(Some(now + Duration::from_secs(2)));

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            app.session_saver.autosave_deadline()
        );
    }

    #[test]
    fn headless_next_loop_deadline_returns_none_when_resize_poll_is_only_deadline() {
        let mut app = test_app();
        let now = Instant::now();
        app.session_saver.set_autosave_deadline(None);
        app.state.workspaces.clear();

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            None
        );
    }

    #[test]
    fn due_session_save_starts_background_writer() {
        let mut app = test_app();
        app.persist_for_test();
        app.state.workspaces = vec![Workspace::test_new("autosave")];
        app.state.ensure_test_terminals();
        app.session_saver
            .set_autosave_deadline(Some(Instant::now() - Duration::from_secs(1)));

        app.start_background_session_save();

        assert!(app.session_saver.save_in_flight());
        assert!(app.session_saver.autosave_deadline().is_none());
        app.save_session_now();
        assert!(
            app.paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME)
                .try_exists()
                .expect("stat session file")
        );
    }

    #[test]
    fn background_session_save_reschedules_when_writer_is_busy() {
        let mut app = test_app();
        app.persist_for_test();
        let release = app.session_saver.hold_test_save_in_flight();
        app.session_saver
            .set_autosave_deadline(Some(Instant::now() - Duration::from_secs(1)));

        app.start_background_session_save();

        assert!(app.session_saver.save_in_flight());
        assert!(app.session_saver.autosave_deadline().is_some());

        release.complete(Ok(()));
        app.policy = AppPolicy::Test;
        app.save_session_now();
    }

    #[test]
    fn final_session_save_joins_background_writer_before_returning() {
        let mut app = test_app();
        app.policy = AppPolicy::Test;
        let release = app.session_saver.hold_test_save_in_flight();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let releaser = std::thread::spawn(move || {
            // Keep the save in flight while the final-save call reaches its wait.
            std::thread::sleep(Duration::from_millis(30));
            done_tx.send(()).expect("test precondition");
            release.complete(Ok(()));
        });

        app.save_session_now();

        done_rx
            .try_recv()
            .expect("the final save returned only after the save in flight finished");
        releaser.join().expect("test precondition");
        assert!(!app.session_saver.save_in_flight());
    }

    #[tokio::test]
    async fn pane_exit_checkpoint_survives_automatic_workspace_creation_on_shutdown() {
        let mut app = test_app();
        app.persist_for_test();
        let mut workspace = Workspace::test_new("preserved");
        let first_pane = workspace.root_pane();
        let second_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();

        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: first_pane,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
            ended_at: std::time::Instant::now(),
        });
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: second_pane,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
            ended_at: std::time::Instant::now(),
        });
        assert!(app.state.workspaces.is_empty());
        let geometry = app.headless_spawn_geometry();
        assert!(app.create_default_workspace(geometry));

        app.save_session_before_teardown_async().await;
        app.retire_session_writer();

        let lease =
            shepr_mux::persist::DataDirLease::acquire(app.paths.data_dir()).expect("test lease");
        let snapshot = shepr_mux::persist::load(&lease)
            .into_snapshot()
            .expect("checkpointed session should survive");
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].panes.len(), 2);
    }

    #[tokio::test]
    async fn detector_release_before_pane_exit_keeps_checkpoint_resume_identity() {
        use shepr_agent::agent::resume::{AgentSessionRef, PersistedAgentSession};
        let _env = crate::test_support::IsolatedEnv::new();
        let mut app = test_app();
        let geometry = app.headless_spawn_geometry();
        assert!(app.create_default_workspace(geometry));
        let pane_id = app.state.workspaces[0].root_pane();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal")
            .clone();
        // Let the real child exit, but keep its PaneDied queued. This exercises
        // the gap where the detector release can reach the app first.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .child_has_exited()
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("pane child exits");
        let session = PersistedAgentSession::from_report(
            "shepr:claude",
            "claude",
            AgentSessionRef::id("checkpoint-resume").expect("session id"),
        )
        .expect("official session");
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.set_detected_agent_process_at(Agent::Claude, app.clock.now);
        terminal.set_persisted_agent_session(session.clone());
        for (agent, process_exited) in [(Some(Agent::Claude), true), (None, false)] {
            app.handle_internal_event(AppEvent::StateChanged {
                pane_id,
                agent,
                state: AgentState::Idle,
                visible_blocker: false,
                process_exited,
                observed_at: app.clock.now,
            });
        }
        assert_eq!(
            app.state.terminals[&terminal_id].detected_agent,
            Some(Agent::Claude),
        );
        assert_eq!(
            app.state.terminals[&terminal_id].current_session_identity_for_persistence(),
            Some(session.clone()),
        );
        app.persist_for_test();
        app.state.mark_session_dirty();
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
            ended_at: std::time::Instant::now(),
        });
        app.save_session_before_teardown_async().await;
        app.retire_session_writer();
        let lease =
            shepr_mux::persist::DataDirLease::acquire(app.paths.data_dir()).expect("test lease");
        let snapshot = shepr_mux::persist::load(&lease)
            .into_snapshot()
            .expect("saved checkpoint");
        let saved = snapshot.workspaces[0]
            .panes
            .values()
            .next()
            .and_then(|pane| pane.agent_session.as_ref())
            .expect("saved resume identity");
        assert_eq!(saved.source, session.source);
        assert_eq!(saved.agent, session.agent);
        assert_eq!(saved.session_ref, session.session_ref);
    }

    /// A pane with a live agent session, ready to have its agent released by
    /// the detector while its shell still runs.
    async fn app_with_agent_session() -> (
        App,
        shepr_core::layout::PaneId,
        shepr_protocol::TerminalId,
        shepr_agent::agent::resume::PersistedAgentSession,
    ) {
        use shepr_agent::agent::resume::{AgentSessionRef, PersistedAgentSession};
        let mut app = test_app();
        let geometry = app.headless_spawn_geometry();
        assert!(app.create_default_workspace(geometry));
        let pane_id = app.state.workspaces[0].root_pane();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal")
            .clone();
        let session = PersistedAgentSession::from_report(
            "shepr:claude",
            "claude",
            AgentSessionRef::id("group-killed").expect("session id"),
        )
        .expect("official session");
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.set_detected_agent_process_at(Agent::Claude, app.clock.now);
        terminal.set_persisted_agent_session(session.clone());
        (app, pane_id, terminal_id, session)
    }

    /// The detector's exit report for the pane's agent, then its withdrawal,
    /// while the shell still runs.
    fn release_agent(app: &mut App, pane_id: shepr_core::layout::PaneId) {
        for (agent, process_exited) in [(Some(Agent::Claude), true), (None, false)] {
            app.handle_internal_event(AppEvent::StateChanged {
                pane_id,
                agent,
                state: AgentState::Idle,
                visible_blocker: false,
                process_exited,
                observed_at: app.clock.now,
            });
        }
    }

    /// The resume identity the saved session holds for its only pane.
    fn saved_agent_session(app: &App) -> shepr_mux::persist::snapshot::PaneAgentSessionSnapshot {
        let lease =
            shepr_mux::persist::DataDirLease::acquire(app.paths.data_dir()).expect("test lease");
        shepr_mux::persist::load(&lease)
            .into_snapshot()
            .expect("saved session")
            .workspaces[0]
            .panes
            .values()
            .next()
            .and_then(|pane| pane.agent_session.clone())
            .expect("saved resume identity")
    }

    #[tokio::test]
    async fn a_signal_death_just_after_the_agents_exit_checkpoints_its_identity() {
        let _env = crate::test_support::IsolatedEnv::new();
        let (mut app, pane_id, terminal_id, session) = app_with_agent_session().await;
        release_agent(&mut app, pane_id);
        // The release took effect at once: no agent, nothing to resume.
        assert_eq!(app.state.terminals[&terminal_id].detected_agent, None);
        assert_eq!(
            app.state.terminals[&terminal_id].current_session_identity_for_persistence(),
            None
        );
        app.persist_for_test();
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
            ended_at: app.clock.now + Duration::from_millis(100),
        });
        app.save_session_before_teardown_async().await;
        app.retire_session_writer();
        let saved = saved_agent_session(&app);
        assert_eq!(saved.session_ref, session.session_ref);
    }

    #[tokio::test]
    async fn a_signal_shutdown_just_after_the_agents_exit_saves_its_identity() {
        let _env = crate::test_support::IsolatedEnv::new();
        let (mut app, pane_id, terminal_id, session) = app_with_agent_session().await;
        release_agent(&mut app, pane_id);
        app.persist_for_test();
        // The final save after a signal: the pane's death is never processed.
        app.state
            .adopt_checkpoint_candidates_for_shutdown(app.clock.now + Duration::from_millis(100));
        assert_eq!(
            app.state.terminals[&terminal_id]
                .current_session_identity_for_persistence()
                .map(|identity| identity.session_ref),
            Some(session.session_ref.clone())
        );
        app.save_session_before_teardown_async().await;
        app.retire_session_writer();
        let saved = saved_agent_session(&app);
        assert_eq!(saved.session_ref, session.session_ref);
    }

    #[test]
    fn normal_autosave_replaces_a_signaled_exit_checkpoint() {
        let mut app = test_app();
        app.persist_for_test();
        let workspace = Workspace::test_new("closed");
        let pane_id = workspace.root_pane();
        app.state.workspaces = vec![workspace];
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();

        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
            ended_at: std::time::Instant::now(),
        });
        // The app still holds the data-dir lease, so the checkpoint is parsed
        // directly rather than through `persist::load`.
        let checkpoint = std::fs::read_to_string(
            app.paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME),
        )
        .expect("the pane exit writes a checkpoint");
        assert!(shepr_mux::persist::snapshot::parse_snapshot(&checkpoint).is_ok());
        assert!(
            app.session_saver.autosave_deadline().is_some(),
            "the pane exit schedules the normal autosave"
        );

        // The loop starts the autosave once its debounce has elapsed.
        app.session_saver
            .set_autosave_deadline(Some(Instant::now() - Duration::from_secs(1)));
        app.start_background_session_save();
        assert!(app.session_saver.save_in_flight());
        app.wait_for_session_save();
        app.save_session_before_teardown();
        app.retire_session_writer();

        assert!(
            !app.paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME)
                .try_exists()
                .expect("test stat")
        );
    }

    #[test]
    fn reader_panic_removes_the_pane_without_a_checkpoint() {
        let mut app = test_app();
        app.persist_for_test();
        let workspace = Workspace::test_new("broken");
        let pane_id = workspace.root_pane();
        app.state.workspaces = vec![workspace];
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::ReaderPanicked,
            ended_at: std::time::Instant::now(),
        });

        assert!(app.state.workspaces.is_empty());
        assert!(
            !app.paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME)
                .try_exists()
                .expect("test stat")
        );
    }

    #[test]
    fn durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown() {
        for another_interrupted_exit in [false, true] {
            let mut app = test_app();
            app.persist_for_test();
            let workspace = Workspace::test_new("old");
            let pane_id = workspace.root_pane();
            app.state.workspaces = vec![workspace];
            app.state.set_bookmark_index(Some(0));
            app.state.ensure_test_terminals();

            app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
                pane_id,
                exit_reason: shepr_platform::ChildExitReason::Interrupted,
                ended_at: std::time::Instant::now(),
            });
            app.state.workspaces = vec![Workspace::test_new("newer")];
            app.state.set_bookmark_index(Some(0));
            app.state.ensure_test_terminals();
            app.state.mark_session_dirty();
            if another_interrupted_exit {
                app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
                    pane_id: app.state.workspaces[0].root_pane(),
                    exit_reason: shepr_platform::ChildExitReason::Interrupted,
                    ended_at: std::time::Instant::now(),
                });
            }
            app.save_session_before_teardown();
            app.retire_session_writer();

            let lease = shepr_mux::persist::DataDirLease::acquire(app.paths.data_dir())
                .expect("test lease");
            let snapshot = shepr_mux::persist::load(&lease)
                .into_snapshot()
                .expect("newer session should be saved");
            assert_eq!(snapshot.workspaces.len(), 1);
            assert_eq!(snapshot.workspaces[0].custom_name.as_deref(), Some("newer"));
        }
    }
}
