//! Application orchestration.
//!
//! `AppState` holds pure application data. `App` coordinates it with live
//! runtime concerns across focused modules under `app/`.

pub(crate) mod actions;
mod agent_resume;
mod agents;
mod api;
pub(crate) mod api_helpers;
pub(crate) use api_helpers::limit_snapshot_lines;
mod creation;
mod events;
mod git_refresh;
mod host_theme;
mod ids;
mod runtime;
mod session;
pub mod state;
mod tab_bar_status;
mod terminal_targets;
mod terminal_titles;
mod window_title;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// One sample supplied by the server at the start of an iteration.
#[derive(Clone, Copy)]
pub(crate) struct AppClock {
    pub(crate) now: Instant,
    pub(crate) wall_now: SystemTime,
}

/// How much of the server view an app operation requires the loop to render.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RenderDemand {
    #[default]
    None,
    Partial,
    Full,
}

impl RenderDemand {
    pub(crate) fn join(&mut self, other: Self) {
        *self = (*self).max(other);
    }
}

pub(crate) struct Outcome {
    pub(crate) response: shepr_api::error::ApiResult,
    pub(crate) render: RenderDemand,
}

use crate::limits::{
    GIT_REMOTE_STATUS_REFRESH_INTERVAL, GIT_REPO_DISCOVERY_REFRESH_INTERVAL,
    PENDING_AGENT_RESUME_THEME_WAIT,
};

use ratatui::layout::Rect;
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

use shepr_mux::events::AppEvent;

pub use state::{AppState, Mode, ViewState};

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
    pub(crate) pixel_mouse_available: bool,
    pub(crate) terminal_runtimes: shepr_mux::pane::PaneRuntimeRegistry,
    pub event_tx: mpsc::Sender<AppEvent>,
    pub(crate) event_rx: mpsc::Receiver<AppEvent>,
    pub(crate) api_rx: tokio::sync::mpsc::UnboundedReceiver<shepr_api::ApiRequestMessage>,
    pub(crate) event_hub: shepr_api::EventHub,
    pub(crate) last_focus: Option<(usize, shepr_core::layout::PaneId)>,
    pub(crate) policy: AppPolicy,
    pub(crate) git_refresh: git_refresh::GitRefreshScheduler,
    pub(crate) agent_metadata_deadline: Option<Instant>,
    pub(crate) pending_agent_resume_deadline: Option<Instant>,
    startup_per_agent_delay: Duration,
    next_agent_resume_at: Option<Instant>,
    pub(crate) session_saver: session::SessionSaver,
    tab_bar_status: tab_bar_status::TabBarStatus,
    /// Host name resolved once for the title and tab bar.
    hostname: String,
    /// Parsed `ui.window_title`.
    window_title_template: Option<shepr_config::WindowTitleTemplate>,
    pub(crate) persist_pane_history: bool,
    /// Pane history kept across saves (restored panes not yet running, the
    /// last primary screen of panes on the alternate screen); every history
    /// capture takes it.
    pub(crate) pane_history_carry: shepr_mux::persist::HistoryCarry,
    /// Last render-loop attempt, including a throttled hidden-only PTY skip.
    pub(crate) last_render_at: Option<Instant>,
    /// Last attempt that could update a connected presentation surface.
    pub(crate) last_presentation_at: Option<Instant>,
    pub render_notify: Arc<Notify>,
    /// This app's pane session teardowns, handed to every pane it spawns and
    /// waited on at exit.
    pane_teardowns: Arc<shepr_mux::pane::PaneTeardownTracker>,
    pub(crate) render_dirty: Arc<shepr_mux::render_signal::RenderSignal>,
    pub(crate) full_redraw_pending: bool,
    pub(crate) paths: shepr_config::AppPaths,
}

pub(crate) use crate::limits::{APP_EVENT_CHANNEL_CAPACITY, APP_EVENT_DRAIN_LIMIT};

impl App {
    pub(crate) fn with_paths(
        config: &shepr_config::ValidatedConfig,
        paths: &shepr_config::AppPaths,
        lease: shepr_mux::persist::DataDirLease,
        policy: AppPolicy,
        api_rx: tokio::sync::mpsc::UnboundedReceiver<shepr_api::ApiRequestMessage>,
        event_hub: shepr_api::EventHub,
        agent_manifest_summaries: Vec<shepr_agent::detect::manifest::AgentManifestSummary>,
        clock: AppClock,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel::<AppEvent>(APP_EVENT_CHANNEL_CAPACITY);
        let render_notify = Arc::new(Notify::new());
        let pane_teardowns = Arc::new(shepr_mux::pane::PaneTeardownTracker::default());
        let render_dirty = Arc::new(shepr_mux::render_signal::RenderSignal::new());
        let settings = state::AppSettings::from_config(config);
        let hostname = shepr_platform::hostname().unwrap_or_default();

        // `agent_manifest_summaries` come from bootstrap, which builds the
        // process-wide registry before restore can start PTY detection.

        // Try to restore previous session
        let mut restored_terminals = std::collections::HashMap::new();
        let mut restored_terminal_runtimes = shepr_mux::pane::PaneRuntimeRegistry::new();
        let mut pane_history_carry = shepr_mux::persist::HistoryCarry::default();
        let paths = paths.clone();
        let snapshot = policy
            .persists_session()
            .then(|| shepr_mux::persist::load(&lease))
            .flatten();
        let restored_host_theme = snapshot
            .as_ref()
            .map_or_default(|snapshot| snapshot.host_theme.to_theme());
        // Whether the first save must copy the on-disk session into
        // `session-backups` before replacing it: the file either could not be
        // loaded, or restore dropped saved tabs that are still only in it.
        let mut protect_unloaded = policy.persists_session() && snapshot.is_none();
        let (workspaces, active, selected) = if let Some(snap) = snapshot {
            let history = config
                .experimental()
                .pane_history
                .then(|| shepr_mux::persist::load_history(&lease))
                .flatten();
            // No view exists yet, so restored panes start at the headless size
            // (what the server lays out against until a client attaches); the
            // first view computation resizes each to its split. The saved
            // host theme supplies colours until a live client reports its own.
            let (restore_rows, restore_cols) = settings
                .pane_geometry_in(settings.headless_rect())
                .sole_pane_size();
            let api_socket_path = shepr_api::socket_path(&paths);
            let restored = shepr_mux::persist::restore(
                &snap,
                history.as_ref(),
                restore_rows,
                restore_cols,
                settings.pane_scrollback_limit_bytes,
                shepr_mux::pane::PaneShellConfig::new(
                    &settings.default_shell,
                    settings.login_shell,
                ),
                &api_socket_path,
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
            if restored.dropped_tabs > 0 {
                protect_unloaded = true;
                warn!(
                    dropped_tabs = restored.dropped_tabs,
                    "session restore dropped saved tabs; the saved session is backed up to session-backups before the first save"
                );
            }
            let outcome = if restored.dropped_tabs > 0 {
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
                paths.session_id().display_name(),
                restored.workspaces.len(),
                outcome,
            );
            if restored.workspaces.is_empty() {
                (Vec::new(), None, 0)
            } else {
                (restored.workspaces, restored.active, restored.selected)
            }
        } else {
            (Vec::new(), None, 0)
        };
        let session_writer = Arc::new(std::sync::Mutex::new(
            shepr_mux::persist::SessionWriter::new(lease, protect_unloaded),
        ));

        info!(
            pane_scrollback_limit_bytes = settings.pane_scrollback_limit_bytes,
            "using pane scrollback configuration"
        );

        let mode = if active.is_some() {
            state::Mode::Terminal
        } else {
            state::Mode::Navigate
        };

        let active_id =
            active.and_then(|index| workspaces.get(index).map(|workspace| workspace.id.clone()));
        let selected_id = workspaces
            .get(selected)
            .map(|workspace| workspace.id.clone());
        let mut state = AppState {
            clock_now: clock.now,
            terminals: std::collections::HashMap::new(),
            direct_attach_resize_locks: std::collections::HashSet::new(),
            public_pane_id_aliases: std::collections::HashMap::new(),
            workspaces,
            active: active_id,
            active_tab_id: None,
            previous_pane_focus: None,
            selected: selected_id,
            mode,
            should_quit: false,
            view: state::ViewState {
                terminal_area: Rect::default(),
                pane_infos: Vec::new(),
            },
            outer_terminal_focus: None,
            settings,
            next_agent_state_change_seq: 0,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: String::new(),
            host_terminal_appearance: None,
            host_terminal_appearance_explicit: false,
            agent_manifest_summaries,
            host_terminal_theme: restored_host_theme,
            host_cell_size: shepr_termio::host_term::cell_size::HostCellSize::default(),
            session_dirty: false,
            shell_projection_revision: 0,
        };

        state.refresh_active_tab_id();
        state.terminals = restored_terminals;
        // Restored workspaces get their Git identity (label, branch, space)
        // from the first background Git refresh, not from a synchronous walk
        // here: the refresh is due immediately (see
        // `last_git_remote_status_refresh` below) and discovers every
        // workspace whose resolved cwd differs from its cached identity.

        let last_focus = state.active_index().and_then(|idx| {
            state
                .workspaces
                .get(idx)
                .map(|ws| (idx, ws.focused_pane_id()))
        });
        let mut app = Self {
            state,
            clock,
            pixel_mouse_available: false,
            terminal_runtimes: restored_terminal_runtimes,
            event_tx,
            event_rx,
            git_refresh: git_refresh::GitRefreshScheduler::new(clock.now),
            agent_metadata_deadline: None,
            pending_agent_resume_deadline: None,
            startup_per_agent_delay: Duration::from_millis(
                config.session().startup_per_agent_delay_ms.into(),
            ),
            next_agent_resume_at: None,
            session_saver: session::SessionSaver::new(session_writer),
            tab_bar_status: tab_bar_status::TabBarStatus::default(),
            hostname,
            window_title_template: None,
            persist_pane_history: config.experimental().pane_history,
            pane_history_carry,
            last_render_at: None,
            last_presentation_at: None,
            api_rx,
            event_hub,
            last_focus,
            policy,
            render_notify,
            pane_teardowns,
            render_dirty,
            full_redraw_pending: false,
            paths,
        };
        app.configure_tab_bar_status(
            &config.ui().tab_bar_right,
            &config.ui().tab_bar_right_separator,
        );
        app.configure_validated_window_title(config.ui().window_title.as_ref());
        app
    }

    /// The server supplies a fresh sample before dispatching an iteration.
    pub(crate) fn set_clock(&mut self, clock: AppClock) {
        self.clock = clock;
        self.state.clock_now = clock.now;
    }

    /// The channels a newly spawned pane runtime reports through. Every call
    /// that spawns a pane (workspace, tab or split creation) takes these;
    /// the workspace tree does not keep them.
    pub(crate) fn pane_spawn_handles(&self) -> shepr_mux::workspace::PaneSpawnHandles {
        shepr_mux::workspace::PaneSpawnHandles {
            events: self.event_tx.clone(),
            render_notify: Arc::clone(&self.render_notify),
            render_dirty: Arc::clone(&self.render_dirty),
            pane_teardowns: Arc::clone(&self.pane_teardowns),
            api_socket_path: shepr_api::socket_path(&self.paths),
        }
    }

    /// Block until this app's pane session teardowns have finished, or
    /// `timeout` passes. Returns whether they all finished.
    pub(crate) fn wait_for_pane_teardowns(&self, timeout: Duration) -> bool {
        self.pane_teardowns.wait(timeout)
    }

    pub(crate) fn ensure_default_workspace(&mut self) -> bool {
        if !self.state.workspaces.is_empty() {
            return false;
        }

        let cwd = self.resolve_new_terminal_cwd(None);
        let preserve_checkpoint =
            self.session_saver.pane_exit_checkpoint_pending && !self.state.session_dirty;

        match self.create_workspace_with_options(&cwd, true) {
            Ok(index) => {
                // Callers include non-mutating API requests and client
                // connects, so the shell projection is invalidated here.
                self.state.mark_shell_projection_dirty();
                self.emit_workspace_open_events(index);
                if preserve_checkpoint {
                    // Automatic replacement is part of pane removal, not a new user mutation.
                    self.session_saver.pane_exit_checkpoint_pending = true;
                    self.finish_checkpointed_pane_exit();
                }
                true
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to create default workspace");
                self.state.mode = Mode::Navigate;
                false
            }
        }
    }
}

#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
pub(crate) use api::test_support::exiting_test_command;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::SESSION_SAVE_DEBOUNCE;
    use crate::test_support::IsolatedEnv;
    use crate::test_support::*;
    use shepr_agent::detect::{Agent, AgentState};
    use shepr_config::Config;
    use shepr_mux::workspace::Workspace;

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
        pub(crate) fn new(
            config: &Config,
            policy: AppPolicy,
            api_rx: tokio::sync::mpsc::UnboundedReceiver<shepr_api::ApiRequestMessage>,
            event_hub: shepr_api::EventHub,
        ) -> Self {
            use crate::test_support::{AppPathsFixture as _, ValidatedConfigFixture as _};
            let scratch = crate::test_support::ScratchDir::new("app");
            let paths = shepr_config::AppPaths::test_at(&scratch);
            let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
                config.clone(),
                None,
                paths.clone(),
            );
            let lease =
                shepr_mux::persist::DataDirLease::acquire(&shepr_api::session::data_dir(&paths))
                    .expect("test session lease");
            Self::with_paths(
                &config,
                &paths,
                lease,
                policy,
                api_rx,
                event_hub,
                Vec::new(),
                test_clock(),
            )
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

    #[test]
    fn render_demand_join_keeps_strongest_request() {
        let mut demand = RenderDemand::None;
        demand.join(RenderDemand::Partial);
        assert_eq!(demand, RenderDemand::Partial);
        demand.join(RenderDemand::None);
        assert_eq!(demand, RenderDemand::Partial);
        demand.join(RenderDemand::Full);
        assert_eq!(demand, RenderDemand::Full);
    }

    pub(super) fn test_clock() -> AppClock {
        AppClock {
            now: Instant::now(),
            wall_now: SystemTime::now(),
        }
    }

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            shepr_api::EventHub::default(),
        );
        app.state.settings.default_shell = exiting_test_command().into();
        app
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

        let changed = app.handle_internal_event_with_render_impact(AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!changed);
        assert!(!app.git_refresh.git_refresh_in_flight);
    }

    #[test]
    fn tab_bar_command_events_render_only_when_visible_output_changes() {
        let mut app = test_app();
        app.configure_tab_bar_status_config(
            &[shepr_config::TabBarRightEntryConfig::Command {
                command: "status".into(),
                interval_seconds: 5,
                timeout_seconds: 2,
            }],
            " ",
        );
        let event = |segment_index, output: Option<&str>| AppEvent::TabBarCommandFinished {
            segment_index,
            result: Ok(output.map(str::to_string)),
        };

        assert!(!app.handle_internal_event_with_render_impact(event(0, None)));
        assert!(app.handle_internal_event_with_render_impact(event(0, Some("ready"))));
        assert!(!app.handle_internal_event_with_render_impact(event(0, Some("ready"))));
        assert!(!app.handle_internal_event_with_render_impact(event(7, Some("unknown"))));
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
    fn unchanged_git_status_drain_has_no_render_impact() {
        let mut app = test_app();
        app.git_refresh.git_refresh_in_flight = true;
        app.event_tx
            .try_send(AppEvent::GitStatusRefreshed {
                results: Vec::new(),
                cache_updates: Vec::new(),
            })
            .expect("test precondition");

        assert!(!app.drain_internal_events());
        assert!(!app.git_refresh.git_refresh_in_flight);
    }

    #[test]
    fn internal_event_drain_limits_work_per_tick() {
        let mut app = test_app();
        for _ in 0..=APP_EVENT_DRAIN_LIMIT {
            app.event_tx
                .try_send(AppEvent::GitStatusRefreshed {
                    results: Vec::new(),
                    cache_updates: Vec::new(),
                })
                .expect("test precondition");
        }

        app.drain_internal_events();

        assert!(app.event_rx.try_recv().is_ok());
        assert!(app.event_rx.try_recv().is_err());
    }

    #[test]
    fn api_request_drains_all_pending_internal_events_before_reading_state() {
        let mut app = test_app();
        for _ in 0..=APP_EVENT_DRAIN_LIMIT {
            app.event_tx
                .try_send(AppEvent::GitStatusRefreshed {
                    results: Vec::new(),
                    cache_updates: Vec::new(),
                })
                .expect("test precondition");
        }

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_workspace_list_after_events".into(),
            method: shepr_api::schema::Method::WorkspaceList(
                shepr_api::schema::EmptyParams::default(),
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "workspace_list");
        assert!(app.event_rx.try_recv().is_err());
    }

    #[test]
    fn theme_uses_configured_name() {
        let mut config = Config::default();
        config.theme.name = Some("tokyo-night".to_string());
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();

        let app = App::new(
            &config,
            crate::app::AppPolicy::Test,
            api_rx,
            shepr_api::EventHub::default(),
        );

        assert_eq!(app.state.settings.palette, state::Palette::tokyo_night());
    }

    #[test]
    fn ui_accent_applies_only_when_set_and_not_overridden_by_the_theme() {
        use ratatui::style::Color;

        let theme_accent = state::Palette::catppuccin().accent;
        assert_ne!(theme_accent, Color::Cyan, "test precondition");

        // Unset: the theme's accent.
        let config = Config::default();
        assert_eq!(
            config
                .resolve_palette_with_ui_accent(false)
                .expect("valid default theme")
                .accent,
            theme_accent
        );

        // Set explicitly: it applies over the theme accent.
        let mut config = Config::default();
        config.ui.accent = Some("cyan".into());
        assert_eq!(
            config
                .resolve_palette_with_ui_accent(true)
                .expect("valid cyan accent")
                .accent,
            Color::Cyan
        );

        config.ui.accent = Some("magenta".into());
        assert_eq!(
            config
                .resolve_palette_with_ui_accent(true)
                .expect("valid magenta accent")
                .accent,
            Color::Magenta
        );

        // `theme.custom.accent` wins over `ui.accent`.
        config.theme.custom = Some(shepr_config::CustomThemeColors {
            accent: Some("#010203".into()),
            ..Default::default()
        });
        assert_eq!(
            config
                .resolve_palette_with_ui_accent(true)
                .expect("valid custom accent")
                .accent,
            Color::Rgb(1, 2, 3)
        );
    }

    #[test]
    fn read_only_api_requests_do_not_force_rerender() {
        let read_only = shepr_api::schema::Request {
            id: "req_1".into(),
            method: shepr_api::schema::Method::WorkspaceList(
                shepr_api::schema::EmptyParams::default(),
            ),
        };
        let mutating = shepr_api::schema::Request {
            id: "req_2".into(),
            method: shepr_api::schema::Method::WorkspaceFocus(shepr_api::schema::WorkspaceTarget {
                workspace_id: "w1".into(),
            }),
        };
        let pane_rename = shepr_api::schema::Request {
            id: "req_3".into(),
            method: shepr_api::schema::Method::PaneRename(shepr_api::schema::PaneRenameParams {
                pane_id: "w1:p1".into(),
                label: Some("logs".into()),
            }),
        };
        let pane_swap = shepr_api::schema::Request {
            id: "req_6".into(),
            method: shepr_api::schema::Method::PaneSwap(shepr_api::schema::PaneSwapParams {
                pane_id: Some("w1:p1".into()),
                direction: Some(shepr_api::schema::PaneDirection::Right),
                ..shepr_api::schema::PaneSwapParams::default()
            }),
        };
        let pane_focus_direction = shepr_api::schema::Request {
            id: "req_7".into(),
            method: shepr_api::schema::Method::PaneFocusDirection(
                shepr_api::schema::PaneFocusDirectionParams {
                    pane_id: Some("w1:p1".into()),
                    direction: shepr_api::schema::PaneDirection::Right,
                },
            ),
        };
        let pane_resize = shepr_api::schema::Request {
            id: "req_8".into(),
            method: shepr_api::schema::Method::PaneResize(shepr_api::schema::PaneResizeParams {
                pane_id: Some("w1:p1".into()),
                direction: shepr_api::schema::PaneDirection::Right,
                amount: Some(0.05),
            }),
        };
        assert!(!read_only.method.traits().mutates_ui);
        assert!(mutating.method.traits().mutates_ui);
        assert!(pane_rename.method.traits().mutates_ui);
        assert!(pane_swap.method.traits().mutates_ui);
        assert!(pane_focus_direction.method.traits().mutates_ui);
        assert!(pane_resize.method.traits().mutates_ui);
    }

    #[test]
    fn workspace_create_response_includes_initial_tab_and_root_pane() {
        let mut app = test_app();
        app.state.workspaces = vec![Workspace::test_new("api-root-pane")];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let shepr_api::schema::ResponseResult::WorkspaceCreated {
            workspace,
            tab,
            root_pane,
        } = app.workspace_created_result(0).expect("test precondition")
        else {
            panic!("expected workspace_created response");
        };

        assert_eq!(workspace.label, "api-root-pane");
        assert_eq!(tab.workspace_id, workspace.workspace_id);
        assert_eq!(root_pane.workspace_id, workspace.workspace_id);
        assert_eq!(root_pane.tab_id, tab.tab_id);
        assert!(root_pane.terminal_id.starts_with("term_"));
        assert_ne!(root_pane.terminal_id, root_pane.pane_id);
    }

    #[tokio::test]
    async fn ensure_default_workspace_emits_creation_events() {
        let event_hub = shepr_api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            event_hub.clone(),
        );
        app.state.settings.default_shell = exiting_test_command().into();

        assert!(app.ensure_default_workspace());

        let events = event_hub.events_after(0);
        assert_eq!(
            events
                .iter()
                .map(|(_, event)| event.data.kind())
                .collect::<Vec<_>>(),
            [
                shepr_api::schema::EventKind::WorkspaceCreated,
                shepr_api::schema::EventKind::TabCreated,
                shepr_api::schema::EventKind::PaneCreated,
                shepr_api::schema::EventKind::LayoutUpdated,
            ]
        );
    }

    #[test]
    fn tab_create_response_includes_root_pane() {
        let mut app = test_app();
        let mut workspace = Workspace::test_new("api-tab-root-pane");
        workspace.test_add_tab(None);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let shepr_api::schema::ResponseResult::TabCreated { tab, root_pane } =
            app.tab_created_result(0, 1).expect("test precondition")
        else {
            panic!("expected tab_created response");
        };

        assert_eq!(tab.workspace_id, root_pane.workspace_id);
        assert_eq!(root_pane.tab_id, tab.tab_id);
        assert_eq!(tab.pane_count, 1);
    }

    #[test]
    fn tab_info_number_uses_stable_public_tab_number() {
        let mut app = test_app();
        let mut workspace = Workspace::test_new("api-tab-public-number");
        let removed_tab = workspace.test_add_tab(None);
        let survivor_tab = workspace.test_add_tab(None);
        let survivor_pane = workspace.tabs()[survivor_tab].root_pane();
        assert!(workspace.close_tab(removed_tab).is_some());
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let survivor_idx = app.state.workspaces[0]
            .find_tab_index_for_pane(survivor_pane)
            .expect("test precondition");

        let tab = app.tab_info(0, survivor_idx).expect("test precondition");

        assert_eq!(tab.tab_id, format!("{}:t3", app.state.workspaces[0].id));
        assert_eq!(tab.number, 3);
        assert_eq!(tab.label, "2");
    }

    #[test]
    fn bare_tab_position_is_rejected_even_when_public_numbers_differ() {
        let mut app = test_app();
        let mut workspace = Workspace::test_new("legacy-tab-id");
        let removed_tab = workspace.test_add_tab(None);
        workspace.test_add_tab(None);
        let public_four_tab = workspace.test_add_tab(None);
        let fourth_position_tab = workspace.test_add_tab(None);
        let public_four_pane = workspace.tabs()[public_four_tab].root_pane();
        let fourth_position_pane = workspace.tabs()[fourth_position_tab].root_pane();
        assert!(workspace.close_tab(removed_tab).is_some());
        app.state.workspaces = vec![workspace];

        let public_four_idx = app.state.workspaces[0]
            .find_tab_index_for_pane(public_four_pane)
            .expect("test precondition");
        let fourth_position_idx = app.state.workspaces[0]
            .find_tab_index_for_pane(fourth_position_pane)
            .expect("test precondition");

        assert_eq!(app.state.workspaces[0].tabs()[public_four_idx].number(), 4);
        assert_eq!(
            app.state.workspaces[0].tabs()[fourth_position_idx].number(),
            5
        );
        assert_eq!(
            app.parse_tab_id(&format!("{}:t4", app.state.workspaces[0].id)),
            Some((0, public_four_idx))
        );
        assert_eq!(
            app.parse_tab_id(&format!("{}:4", app.state.workspaces[0].id)),
            None
        );
    }

    #[test]
    fn workspace_creation_in_navigate_mode_uses_selected_workspace_seed_cwd() {
        let mut app = test_app();
        let mut first = Workspace::test_new("shepr");
        first.identity_cwd = std::path::PathBuf::from("/shepr-test/shepr");
        let mut second = Workspace::test_new("pion");
        second.identity_cwd = std::path::PathBuf::from("/shepr-test/pion");

        app.state.workspaces = vec![first, second];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(1));
        app.state.mode = Mode::Navigate;

        let context = app
            .state
            .resolve_pane_context(None, None, actions::PaneContextFallback::WorkspaceCreation)
            .expect("test precondition");
        let seed_cwd = app
            .seed_cwd_from_workspace(context.workspace_index)
            .expect("test precondition");

        assert_eq!(context.workspace_index, 1);
        assert_eq!(context.tab_index, 0);
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

    #[test]
    fn workspace_list_request_keeps_server_running() {
        let mut app = test_app();

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_workspace_list".into(),
            method: shepr_api::schema::Method::WorkspaceList(
                shepr_api::schema::EmptyParams::default(),
            ),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "workspace_list");
        assert!(!app.state.should_quit);
    }

    #[test]
    fn pane_rename_request_sets_and_clears_manual_label() {
        let mut app = test_app();
        let workspace = Workspace::test_new("api-pane-rename");
        let pane = workspace.tabs()[0].root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let pane_id = app.pane_info(0, pane).expect("test precondition").pane_id;
        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_pane_rename".into(),
            method: shepr_api::schema::Method::PaneRename(shepr_api::schema::PaneRenameParams {
                pane_id: pane_id.clone(),
                label: Some("reviewer".into()),
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "pane_info");
        assert_eq!(response["result"]["pane"]["label"], "reviewer");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .expect("test precondition")
                .manual_label
                .as_deref(),
            Some("reviewer")
        );

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_pane_rename_clear".into(),
            method: shepr_api::schema::Method::PaneRename(shepr_api::schema::PaneRenameParams {
                pane_id,
                label: None,
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "pane_info");
        assert!(response["result"]["pane"].get("label").is_none());
        assert!(
            app.state
                .terminals
                .get(&terminal_id)
                .expect("test precondition")
                .manual_label
                .is_none()
        );
    }

    #[test]
    fn terminal_and_agent_targets_treat_terminal_ids_differently() {
        let mut app = test_app();
        let workspace = Workspace::test_new("terminal-target-id");
        let pane = workspace.tabs()[0].root_pane();
        let terminal_id = workspace
            .terminal_id(pane)
            .expect("test precondition")
            .to_string();
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let resolved = app
            .resolve_terminal_target(&terminal_id)
            .expect("test precondition");
        assert_eq!(resolved.pane_id, pane);
        assert_eq!(resolved.terminal_id.as_str(), terminal_id);

        assert!(matches!(
            app.resolve_agent_target(resolved.terminal_id.as_str()),
            Err(crate::app::terminal_targets::TerminalTargetError::NotFound { .. })
        ));
    }

    #[test]
    fn agent_target_rejects_a_pane_that_only_has_a_launch_command() {
        let mut app = test_app();
        let workspace = Workspace::test_new("terminal-target-command");
        let pane = workspace.tabs()[0].root_pane();
        let terminal_id = workspace
            .terminal_id(pane)
            .expect("test precondition")
            .clone();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .launch_argv = Some(vec!["just".into(), "dev".into()]);
        let pane_id = app.public_pane_id(0, pane).expect("test precondition");

        assert!(app.resolve_terminal_target(&pane_id).is_ok());
        assert!(matches!(
            app.resolve_agent_target(&pane_id),
            Err(crate::app::terminal_targets::TerminalTargetError::NotFound { .. })
        ));
    }

    #[test]
    fn terminal_target_resolves_pane_id_for_an_agent() {
        let mut app = test_app();
        let workspace = Workspace::test_new("terminal-target-pane");
        let pane = workspace.tabs()[0].root_pane();
        let terminal_id = workspace
            .terminal_id(pane)
            .expect("test precondition")
            .to_string();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let attached_terminal_id = app.state.workspaces[0]
            .terminal_id(pane)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&attached_terminal_id)
            .expect("test precondition")
            .set_detected_state(
                Some(shepr_agent::detect::Agent::Pi),
                shepr_agent::detect::AgentState::Idle,
            );
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        let pane_id = app.public_pane_id(0, pane).expect("test precondition");

        let resolved = app
            .resolve_terminal_target(&pane_id)
            .expect("test precondition");

        assert_eq!(resolved.pane_id, pane);
        assert_eq!(resolved.terminal_id.as_str(), terminal_id);
    }

    #[test]
    fn terminal_target_resolves_unique_agent_name() {
        let mut app = test_app();
        let workspace = Workspace::test_new("terminal-target-name");
        let pane = workspace.tabs()[0].root_pane();
        let terminal_id = workspace
            .terminal_id(pane)
            .expect("test precondition")
            .to_string();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let attached_terminal_id = app.state.workspaces[0]
            .pane_state(pane)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&attached_terminal_id)
            .expect("test precondition")
            .set_agent_name("reviewer".into());
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let resolved = app
            .resolve_terminal_target("reviewer")
            .expect("test precondition");

        assert_eq!(resolved.pane_id, pane);
        assert_eq!(resolved.terminal_id.as_str(), terminal_id);
    }

    #[test]
    fn terminal_target_matches_detected_agent_but_agent_target_needs_name() {
        let mut app = test_app();
        let workspace = Workspace::test_new("terminal-target-detected-agent");
        let pane = workspace.tabs()[0].root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_detected_state(
                Some(shepr_agent::detect::Agent::Pi),
                shepr_agent::detect::AgentState::Idle,
            );

        let resolved = app
            .resolve_terminal_target("pi")
            .expect("detected label resolves to terminal");
        assert_eq!(resolved.pane_id, pane);
        assert!(matches!(
            app.resolve_agent_target("pi"),
            Err(crate::app::terminal_targets::TerminalTargetError::NotFound { .. })
        ));
    }

    #[test]
    fn agent_target_treats_legacy_pane_syntax_as_a_name() {
        let mut app = test_app();
        let workspace = Workspace::test_new("agent-target-name");
        let pane = workspace.tabs()[0].root_pane();
        let terminal_id = workspace
            .terminal_id(pane)
            .expect("test precondition")
            .clone();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(
            Some(shepr_agent::detect::Agent::Pi),
            shepr_agent::detect::AgentState::Idle,
        );
        terminal.set_agent_name("p_1".into());

        let resolved = app.resolve_agent_target("p_1").expect("test precondition");

        assert_eq!(resolved.pane_id, pane);
        assert_eq!(resolved.terminal_id, terminal_id);
    }

    #[test]
    fn terminal_target_reports_missing_target() {
        let mut app = test_app();
        app.state.workspaces = vec![Workspace::test_new("terminal-target-missing")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let err = app
            .resolve_terminal_target("missing-agent")
            .expect_err("test precondition");

        assert_eq!(
            err,
            crate::app::terminal_targets::TerminalTargetError::NotFound {
                target: "missing-agent".into()
            }
        );
    }

    #[test]
    fn terminal_target_reports_ambiguous_duplicate_agent_name() {
        let mut app = test_app();
        let mut workspace = Workspace::test_new("terminal-target-ambiguous");
        let first = workspace.tabs()[0].root_pane();
        let second = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let first_terminal_id = app.state.workspaces[0]
            .pane_state(first)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&first_terminal_id)
            .expect("test precondition")
            .set_agent_name("worker".into());
        let second_terminal_id = app.state.workspaces[0]
            .pane_state(second)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&second_terminal_id)
            .expect("test precondition")
            .set_agent_name("worker".into());
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let err = app
            .resolve_terminal_target("worker")
            .expect_err("test precondition");

        let crate::app::terminal_targets::TerminalTargetError::Ambiguous { target, candidates } =
            err
        else {
            panic!("expected ambiguous terminal target");
        };
        assert_eq!(target, "worker");
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|candidate| {
            candidate.terminal_id.as_str().starts_with("term_")
                && candidate
                    .pane_id
                    .to_string()
                    .starts_with(app.state.workspaces[0].id.as_str())
                && candidate.workspace_id == app.state.workspaces[0].id
                && candidate.cwd.is_some()
        }));
    }

    #[tokio::test]
    async fn pane_split_request_focuses_new_pane_when_requested() {
        let env = IsolatedEnv::new();
        env.set("SHELL", exiting_test_command());

        let mut app = test_app();
        let mut workspace = Workspace::test_new("api-pane-split-focus-background-tab");
        let background_tab = workspace.test_add_tab(Some("worker"));
        workspace.switch_tab(0);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let target_pane = app.state.workspaces[0].tabs()[background_tab].root_pane();
        let target_pane_id = app
            .pane_info(0, target_pane)
            .expect("test precondition")
            .pane_id;
        let target_tab_id = app
            .public_tab_id(0, background_tab)
            .expect("test precondition");

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_pane_split_focus_background_tab".into(),
            method: shepr_api::schema::Method::PaneSplit(shepr_api::schema::PaneSplitParams {
                workspace_id: None,
                target_pane_id: Some(target_pane_id),
                direction: shepr_api::schema::SplitDirection::Right,
                ratio: None,
                cwd: None,
                focus: true,
                right_click: Default::default(),
                env: Default::default(),
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "pane_info");
        assert_eq!(response["result"]["pane"]["tab_id"], target_tab_id);
        assert_eq!(response["result"]["pane"]["focused"], true);
        assert_eq!(app.state.active_index(), Some(0));
        assert_eq!(app.state.workspaces[0].active_tab_index(), background_tab);

        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pane_split_request_applies_ratio() {
        let env = IsolatedEnv::new();
        env.set("SHELL", exiting_test_command());

        let mut app = test_app();
        let workspace = Workspace::test_new("api-pane-split-ratio");
        let target_pane = workspace.tabs()[0].root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let target_pane_id = app
            .pane_info(0, target_pane)
            .expect("test precondition")
            .pane_id;

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_pane_split_ratio".into(),
            method: shepr_api::schema::Method::PaneSplit(shepr_api::schema::PaneSplitParams {
                workspace_id: None,
                target_pane_id: Some(target_pane_id),
                direction: shepr_api::schema::SplitDirection::Right,
                ratio: Some(0.333),
                cwd: None,
                focus: false,
                right_click: shepr_api::schema::PaneRightClickTarget::Pane,
                env: Default::default(),
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "pane_info");
        let splits = app.state.workspaces[0].tabs()[0]
            .layout()
            .splits(shepr_core::geometry::Rect::new(0, 0, 100, 20));
        assert_eq!(splits.len(), 1);
        assert!((splits[0].ratio - 0.333).abs() < f32::EPSILON);
        let response_pane_id = response["result"]["pane"]["pane_id"]
            .as_str()
            .expect("test precondition");
        let (_, response_pane_id) = app
            .parse_pane_id(response_pane_id)
            .expect("test precondition");
        assert!(
            app.state.workspaces[0]
                .pane_state(response_pane_id)
                .expect("test precondition")
                .right_click_passthrough
        );

        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            drop(runtime);
        }
    }

    #[tokio::test]
    async fn pane_split_request_uses_active_focused_pane_when_target_is_omitted() {
        let env = IsolatedEnv::new();
        env.set("SHELL", exiting_test_command());

        let mut app = test_app();
        let workspace = Workspace::test_new("api-pane-split-current");
        let target_pane = workspace.tabs()[0].root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.focus_pane_in_workspace(0, target_pane);

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_pane_split_current".into(),
            method: shepr_api::schema::Method::PaneSplit(shepr_api::schema::PaneSplitParams {
                workspace_id: None,
                target_pane_id: None,
                direction: shepr_api::schema::SplitDirection::Right,
                ratio: None,
                cwd: None,
                focus: false,
                right_click: Default::default(),
                env: Default::default(),
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "pane_info");
        assert_eq!(app.state.workspaces[0].tabs()[0].layout().pane_count(), 2);
        assert_eq!(
            app.state.workspaces[0].tabs()[0].layout().focused(),
            target_pane
        );

        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            drop(runtime);
        }
    }

    #[test]
    fn pane_close_request_closes_only_the_target_tab_when_other_tabs_exist() {
        let mut app = test_app();
        let mut workspace = Workspace::test_new("api-pane-close");
        let second_tab = workspace.test_add_tab(Some("logs"));
        workspace.switch_tab(second_tab);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let target_pane = app.state.workspaces[0].tabs()[second_tab].root_pane();
        let target_pane_id = app
            .pane_info(0, target_pane)
            .expect("test precondition")
            .pane_id;

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_pane_close".into(),
            method: shepr_api::schema::Method::PaneClose(shepr_api::schema::PaneTarget {
                pane_id: target_pane_id,
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "ok");
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].tabs().len(), 1);
        assert_eq!(app.state.workspaces[0].display_name(), "api-pane-close");
    }

    #[test]
    fn pane_close_request_closes_workspace_when_it_removes_the_last_pane() {
        let mut app = test_app();
        let workspace = Workspace::test_new("api-pane-close-last");
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));

        let target_pane = app.state.workspaces[0].tabs()[0].root_pane();
        let target_pane_id = app
            .pane_info(0, target_pane)
            .expect("test precondition")
            .pane_id;

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "req_pane_close_last".into(),
            method: shepr_api::schema::Method::PaneClose(shepr_api::schema::PaneTarget {
                pane_id: target_pane_id,
            }),
        });
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");

        assert_eq!(response["result"]["type"], "ok");
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn session_dirty_flag_schedules_debounced_save() {
        let mut app = test_app();
        app.policy = AppPolicy::Production;
        let sample = AppClock {
            now: app.clock.now + Duration::from_secs(42),
            wall_now: app.clock.wall_now,
        };
        app.set_clock(sample);
        app.state.session_dirty = true;

        app.sync_session_save_schedule();

        assert!(!app.state.session_dirty);
        assert_eq!(
            app.session_saver.session_save_deadline,
            Some(sample.now + SESSION_SAVE_DEBOUNCE)
        );
    }

    #[test]
    fn headless_next_loop_deadline_ignores_resize_poll() {
        let mut app = test_app();
        let now = Instant::now();
        app.session_saver.session_save_deadline = Some(now + Duration::from_secs(2));

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            app.session_saver.session_save_deadline
        );
    }

    #[test]
    fn headless_next_loop_deadline_returns_none_when_resize_poll_is_only_deadline() {
        let mut app = test_app();
        let now = Instant::now();
        app.session_saver.session_save_deadline = None;
        app.state.workspaces.clear();

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            None
        );
    }

    #[test]
    fn due_session_save_starts_background_writer() {
        let mut app = test_app();
        app.policy = AppPolicy::Production;
        app.state.workspaces = vec![Workspace::test_new("autosave")];
        app.state.ensure_test_terminals();
        app.session_saver.session_save_deadline = Some(Instant::now() - Duration::from_secs(1));

        app.start_background_session_save();

        assert!(app.session_saver.session_save_thread.is_some());
        assert!(app.session_saver.session_save_deadline.is_none());
        app.save_session_now();
        assert!(
            shepr_api::session::data_dir(&app.paths)
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME)
                .try_exists()
                .expect("stat session file")
        );
    }

    #[test]
    fn failed_session_saves_back_off_and_recover() {
        let mut app = test_app();
        let now = Instant::now();
        let mut previous = Duration::ZERO;
        for _ in 0..12 {
            app.session_saver.session_save_deadline = None;
            assert!(!app.record_session_save_result(Err(std::io::Error::other("disk full")), now));
            let delay = app
                .session_saver
                .session_save_deadline
                .expect("a failed save schedules a retry")
                - now;
            assert!(delay >= previous, "retry delay never shrinks while failing");
            assert!(delay <= Duration::from_secs(30), "retry delay is capped");
            previous = delay;
        }
        assert_eq!(previous, Duration::from_secs(30));

        assert!(app.record_session_save_result(Ok(()), now));
        app.session_saver.session_save_deadline = None;
        app.record_session_save_result(Err(std::io::Error::other("disk full")), now);
        assert_eq!(
            app.session_saver.session_save_deadline,
            Some(now + Duration::from_millis(250)),
            "a success resets the backoff"
        );
    }

    #[test]
    fn background_session_save_reschedules_when_writer_is_busy() {
        let mut app = test_app();
        app.policy = AppPolicy::Production;
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        app.session_saver.session_save_thread = Some(std::thread::spawn(move || {
            release_rx.recv().expect("test releases the save thread");
            Ok(())
        }));
        app.session_saver.session_save_deadline = Some(Instant::now() - Duration::from_secs(1));

        app.start_background_session_save();

        assert!(app.session_saver.session_save_thread.is_some());
        assert!(app.session_saver.session_save_deadline.is_some());

        release_tx.send(()).expect("test precondition");
        app.policy = AppPolicy::Test;
        app.save_session_now();
    }

    #[test]
    fn final_session_save_joins_background_writer_before_returning() {
        let mut app = test_app();
        app.policy = AppPolicy::Test;
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        app.session_saver.session_save_thread = Some(std::thread::spawn(move || {
            release_rx.recv().expect("test releases the save thread");
            done_tx.send(()).expect("test precondition");
            Ok(())
        }));
        let releaser = std::thread::spawn(move || {
            // Keep the writer blocked while the final-save call reaches its join.
            std::thread::sleep(Duration::from_millis(30));
            release_tx.send(()).expect("test precondition");
        });

        app.save_session_now();

        releaser.join().expect("test precondition");
        done_rx.try_recv().expect("test precondition");
        assert!(app.session_saver.session_save_thread.is_none());
    }

    #[tokio::test]
    async fn pane_exit_checkpoint_survives_automatic_workspace_creation_on_shutdown() {
        let mut app = test_app();
        app.policy = AppPolicy::Production;
        let mut workspace = Workspace::test_new("preserved");
        let first_pane = workspace.tabs()[0].root_pane();
        let second_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();

        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: first_pane,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: second_pane,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        assert!(app.state.workspaces.is_empty());
        assert!(app.ensure_default_workspace());

        app.save_session_before_teardown();
        app.retire_session_writer();

        let lease =
            shepr_mux::persist::DataDirLease::acquire(&shepr_api::session::data_dir(&app.paths))
                .expect("test lease");
        let snapshot =
            shepr_mux::persist::load(&lease).expect("checkpointed session should survive");
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].tabs[0].panes.len(), 2);
    }

    #[test]
    fn normal_autosave_replaces_a_signaled_exit_checkpoint() {
        let mut app = test_app();
        app.policy = AppPolicy::Production;
        let workspace = Workspace::test_new("closed");
        let pane_id = workspace.tabs()[0].root_pane();
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();

        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        // The app still holds the data-dir lease, so the checkpoint is parsed
        // directly rather than through `persist::load`.
        let checkpoint = std::fs::read_to_string(
            shepr_api::session::data_dir(&app.paths)
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME),
        )
        .expect("the pane exit writes a checkpoint");
        assert!(shepr_mux::persist::snapshot::parse_snapshot(&checkpoint).is_ok());
        assert!(
            app.session_saver.session_save_deadline.is_some(),
            "the pane exit schedules the normal autosave"
        );

        // The loop starts the autosave once its debounce has elapsed.
        app.session_saver.session_save_deadline = Some(Instant::now() - Duration::from_secs(1));
        app.start_background_session_save();
        assert!(app.session_saver.session_save_thread.is_some());
        if let Some(thread) = app.session_saver.session_save_thread.take() {
            thread
                .join()
                .expect("test precondition")
                .expect("session save succeeds");
        }
        app.save_session_before_teardown();
        app.retire_session_writer();

        assert!(
            !shepr_api::session::data_dir(&app.paths)
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME)
                .try_exists()
                .expect("test stat")
        );
    }

    #[test]
    fn reader_panic_removes_the_pane_without_a_checkpoint() {
        let mut app = test_app();
        app.policy = AppPolicy::Production;
        let workspace = Workspace::test_new("broken");
        let pane_id = workspace.tabs()[0].root_pane();
        app.state.workspaces = vec![workspace];
        app.state.set_active_index(Some(0));
        app.state.ensure_test_terminals();

        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::ReaderPanicked,
        });

        assert!(app.state.workspaces.is_empty());
        assert!(
            !shepr_api::session::data_dir(&app.paths)
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME)
                .try_exists()
                .expect("test stat")
        );
    }

    #[test]
    fn durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown() {
        for another_interrupted_exit in [false, true] {
            let mut app = test_app();
            app.policy = AppPolicy::Production;
            let workspace = Workspace::test_new("old");
            let pane_id = workspace.tabs()[0].root_pane();
            app.state.workspaces = vec![workspace];
            app.state.set_active_index(Some(0));
            app.state.ensure_test_terminals();

            app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
                pane_id,
                exit_reason: shepr_platform::ChildExitReason::Interrupted,
            });
            app.state.workspaces = vec![Workspace::test_new("newer")];
            app.state.set_active_index(Some(0));
            app.state.ensure_test_terminals();
            app.state.mark_session_dirty();
            if another_interrupted_exit {
                app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
                    pane_id: app.state.workspaces[0].tabs()[0].root_pane(),
                    exit_reason: shepr_platform::ChildExitReason::Interrupted,
                });
            }
            app.save_session_before_teardown();
            app.retire_session_writer();

            let lease = shepr_mux::persist::DataDirLease::acquire(&shepr_api::session::data_dir(
                &app.paths,
            ))
            .expect("test lease");
            let snapshot = shepr_mux::persist::load(&lease).expect("newer session should be saved");
            assert_eq!(snapshot.workspaces.len(), 1);
            assert_eq!(snapshot.workspaces[0].custom_name.as_deref(), Some("newer"));
        }
    }

    #[tokio::test]
    async fn full_internal_event_queue_eventually_applies_working_to_idle_transition() {
        let mut app = test_app();
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs()[0].root_pane();

        app.state.workspaces = vec![ws];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.mode = Mode::Terminal;

        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Working,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .expect("test precondition")
                .state,
            AgentState::Working
        );

        for _ in 0..APP_EVENT_CHANNEL_CAPACITY {
            app.event_tx
                .try_send(AppEvent::GitStatusRefreshed {
                    results: Vec::new(),
                    cache_updates: Vec::new(),
                })
                .expect("test precondition");
        }

        let tx = app.event_tx.clone();
        let send = tx.send(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        tokio::pin!(send);

        let blocked =
            tokio::time::timeout(Duration::from_millis(20), async { (&mut send).await }).await;
        assert!(
            blocked.is_err(),
            "state change sender should wait for queue space instead of failing"
        );

        app.drain_internal_events();

        tokio::time::timeout(Duration::from_millis(50), async { (&mut send).await })
            .await
            .expect("state change should enqueue once queue space is available")
            .expect("app event receiver should still be alive");

        let max_drains = (APP_EVENT_CHANNEL_CAPACITY / APP_EVENT_DRAIN_LIMIT) + 2;
        for _ in 0..max_drains {
            if app
                .state
                .terminals
                .get(&terminal_id)
                .expect("test precondition")
                .state
                == AgentState::Idle
            {
                break;
            }
            app.drain_internal_events();
        }

        assert_eq!(
            app.state
                .terminals
                .get(&terminal_id)
                .expect("test precondition")
                .state,
            AgentState::Idle,
            "Working→Idle should still apply after temporary queue pressure"
        );
    }
}
