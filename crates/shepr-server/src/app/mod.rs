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
mod outputs;
mod pane_launch;
mod pane_resize;
mod resume_schedule;
mod runtime;
mod session;
pub(crate) mod state;
mod terminal_titles;

pub(crate) use events::{Admitted, PaneDeath, PaneExitPrepared, PreparedPaneExit};
pub(crate) use pane_resize::PaneResizeTiming;
pub(crate) use session::{CheckpointGeneration, HostCheckpointOutcome};

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// One sample supplied by the server at the start of an iteration.
///
/// The same pair as `HookClockSample`, which `shepr-detect` owns and orders
/// hook reports by; the two convert freely and are kept apart so the server's
/// clock does not take its field names from the detector.
#[derive(Clone, Copy)]
pub(crate) struct AppClock {
    pub(crate) now: Instant,
    pub(crate) wall_now: SystemTime,
}

impl AppClock {
    /// The same sample as the pair hook reports are ordered by.
    pub(crate) fn hook_sample(self) -> shepr_detect::ownership::HookClockSample {
        shepr_detect::ownership::HookClockSample {
            monotonic: self.now,
            wall: self.wall_now,
        }
    }
}

impl From<shepr_detect::ownership::HookClockSample> for AppClock {
    fn from(sample: shepr_detect::ownership::HookClockSample) -> Self {
        Self {
            now: sample.monotonic,
            wall_now: sample.wall,
        }
    }
}

pub(crate) struct Outcome {
    pub(crate) response: shepr_api::error::ApiResult,
    pub(crate) view_changed: bool,
}

use crate::limits::{APP_EVENT_CHANNEL_CAPACITY, PENDING_AGENT_RESUME_THEME_WAIT};

use tokio::sync::{Notify, mpsc};
use tracing::info;

use shepr_mux::events::AppEvent;

pub(crate) use api::session::ProjectionInput;
pub(crate) use api::{EndpointContext, Invalidation};
pub(crate) use shepr_mux::workspace::SpawnGeometry;
pub(crate) use state::{AppState, HostAppearanceReport};

/// What `App::create_default_workspace` found or did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefaultWorkspace {
    /// The session already has a workspace; nothing was created.
    Exists,
    Created,
    Failed,
}

/// Full application: the pure `AppState` plus the runtime concerns it must
/// not hold - live pane runtimes and async I/O. What the runtimes publish
/// (events, render requests, finished saves) goes to the `AppOutputs` that
/// `App::open` returns, which the loop owns.
pub(crate) struct App {
    state: AppState,
    clock: AppClock,
    terminal_runtimes: shepr_mux::pane::PaneRuntimeRegistry,
    git_refresh: git_refresh::GitRefreshScheduler,
    /// When deferred agent resumes may be attempted; see `resume_schedule`.
    resume_schedule: resume_schedule::ResumeSchedule,
    session_saver: session::SessionSaver,
    /// Workspaces whose panes wait for their geometry to settle before their
    /// PTYs are resized; see `pane_resize`.
    pending_pane_resizes: pane_resize::PendingPaneResizes,
    /// This app's pane session teardowns, handed to every pane it spawns and
    /// waited on at exit.
    pane_teardowns: Arc<shepr_mux::pane::PaneTeardownTracker>,
    pane_launcher: shepr_mux::pane::PaneLauncher,
    paths: shepr_paths::AppPaths,
    /// Set when this boot's restore did not bring the saved session back in
    /// full; sent to every client that connects, for the life of the boot.
    restore_notice: Option<shepr_protocol::SessionRestoreNotice>,
}

/// The pure render inputs: shared references to the state and the runtimes.
#[derive(Clone, Copy)]
pub(crate) struct RenderView<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) runtimes: &'a shepr_mux::pane::PaneRuntimeRegistry,
}

pub(crate) use outputs::{AppOutputs, AppWake};

impl App {
    /// Opens the session and returns the app with the outputs the loop owns:
    /// the receiving sides of the event channel and the render and save
    /// signals. API requests reach the app through `HeadlessServer`, which
    /// owns their bounded receiver; the app holds no API channel.
    pub(crate) fn open(
        config: &shepr_config::ValidatedServerConfig,
        paths: &shepr_paths::AppPaths,
        lease: shepr_mux::persist::DataDirLease,
        clock: AppClock,
    ) -> (Self, AppOutputs) {
        let (event_tx, event_rx) = mpsc::channel::<AppEvent>(APP_EVENT_CHANNEL_CAPACITY);
        let render_notify = Arc::new(Notify::new());
        let pane_teardowns = Arc::new(shepr_mux::pane::PaneTeardownTracker::default());
        let render_dirty = Arc::new(shepr_mux::render_signal::RenderSignal::new());
        let settings = state::AppSettings::from_config(config);
        // Host names resolved once at startup, `None` when they could not be:
        // the pane launcher matches OSC 7 cwd reports against them (full and
        // short form).
        let hostname = shepr_platform::host_names().map(Arc::new);

        let paths = paths.clone();
        let save_finished = std::sync::Arc::new(tokio::sync::Notify::new());
        // The launch settings go from the validated config straight to the
        // launcher, their one holder; `AppSettings` keeps no copy.
        let pane_scrollback = config.scrollback();
        let pane_launcher = shepr_mux::pane::PaneLauncher::new(
            shepr_mux::pane::PaneSpawnHandles {
                events: event_tx.clone(),
                render_notify: Arc::clone(&render_notify),
                render_dirty: Arc::clone(&render_dirty),
                pane_teardowns: Arc::clone(&pane_teardowns),
                socket_path: paths.server_address().socket().to_path_buf(),
            },
            shepr_mux::pane::PaneShellConfig::new(
                &config.terminal().default_shell,
                config.terminal().login_shell,
            ),
            pane_scrollback,
            hostname,
        );
        let shepr_mux::persist::OpenedSession {
            workspaces,
            terminal_runtimes: restored_terminal_runtimes,
            host_theme,
            persister,
            restore_notice,
        } = shepr_mux::persist::open_session(
            lease,
            &shepr_mux::persist::SessionOpenOptions {
                geometry: settings.pane_geometry_in(settings.headless_rect()),
                launcher: &pane_launcher,
                resume_agents_on_restore: config.session().resume_agents_on_restore,
                now: clock.now,
            },
            std::sync::Arc::clone(&save_finished),
        );

        info!(
            pane_scrollback_limit_bytes = pane_scrollback.bytes(),
            "using pane scrollback configuration"
        );

        let state = AppState::new(settings, workspaces, host_theme);
        // Restored workspaces get their Git identity (label and status)
        // from the first background Git refresh, not from a synchronous walk
        // here. The scheduler starts due immediately and discovers every
        // workspace whose resolved cwd differs from its cached identity.

        let git_refresh = git_refresh::GitRefreshScheduler::new(clock.now, event_tx.clone());
        let mut app = Self {
            state,
            clock,
            terminal_runtimes: shepr_mux::pane::PaneRuntimeRegistry::new(),
            git_refresh,
            resume_schedule: resume_schedule::ResumeSchedule::new(
                PENDING_AGENT_RESUME_THEME_WAIT,
                config.session().agent_resume_spacing,
            ),
            session_saver: session::SessionSaver::new(persister),
            pending_pane_resizes: pane_resize::PendingPaneResizes::default(),
            pane_teardowns,
            pane_launcher,
            paths,
            restore_notice,
        };
        // Restore keys its runtimes by pane; `install_runtime` drops one whose
        // pane is not in the restored state.
        for (pane_id, runtime) in restored_terminal_runtimes {
            app.install_runtime(pane_id, runtime);
        }
        let outputs = AppOutputs::new(
            event_rx,
            render_dirty,
            render_notify,
            save_finished,
            event_tx,
        );
        (app, outputs)
    }

    /// The server supplies a fresh sample before dispatching an iteration.
    pub(crate) fn set_clock(&mut self, clock: AppClock) {
        self.clock = clock;
    }

    /// The loop's current clock sample.
    pub(crate) fn clock(&self) -> AppClock {
        self.clock
    }

    /// Launches a pane shell for a live server: the current host theme and
    /// appearance. Restore launches through the same
    /// launcher with its saved theme (`SessionRestorePlan::launch`).
    pub(super) fn launch_pane(
        &self,
        pane_id: shepr_core::layout::PaneId,
        public_id: shepr_protocol::PublicPaneId,
        geometry: shepr_core::geometry::PaneGeometry,
        cwd: &shepr_core::absolute_path::AbsolutePath,
        kind: shepr_mux::pane::LaunchKind,
    ) -> std::io::Result<shepr_mux::pane::PaneRuntime> {
        self.pane_launcher
            .launch(shepr_mux::pane::PaneLaunchRequest {
                pane_id,
                public_id,
                geometry,
                cwd,
                kind,
                presentation: shepr_mux::pane::LaunchPresentation::Live {
                    theme: self.state.host_terminal_theme(),
                    appearance: self.state.host_terminal_appearance(),
                },
            })
    }

    /// Block until this app's pane session teardowns have finished, or
    /// `timeout` passes. Returns whether they all finished.
    fn wait_for_pane_teardowns(&self, timeout: Duration) -> bool {
        self.pane_teardowns.wait(timeout)
    }

    /// Creates the workspace an empty session gets, sized for `geometry`.
    /// The caller chooses the geometry, owns the retry backoff after a failure
    /// and settles the clients' locations and geometry controllers
    /// (`create_automatic_workspace` on the server loop).
    pub(crate) fn create_default_workspace(&mut self, geometry: SpawnGeometry) -> DefaultWorkspace {
        if !self.state.workspaces.is_empty() {
            return DefaultWorkspace::Exists;
        }

        let cwd = self.resolve_new_terminal_cwd(None);
        let preserve_checkpoint = self.preserves_pane_exit_checkpoint();

        match self.create_workspace(&cwd, geometry) {
            Ok(_workspace_id) => {
                // Callers include non-mutating API requests and client
                // connects, so the shell projection is invalidated here.
                self.state.mark_shell_projection_dirty();
                if preserve_checkpoint {
                    // Automatic replacement is part of pane removal, not a new user mutation.
                    self.finish_checkpointed_pane_exit();
                }
                DefaultWorkspace::Created
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to create default workspace");
                DefaultWorkspace::Failed
            }
        }
    }

    /// The earliest instant the app itself needs the loop awake: the Git
    /// refresh (when `git_refresh` asks for it), the next pending agent
    /// resume, the session save and the deferred pane resizes.
    pub(crate) fn next_deadline(&self, git_refresh: bool) -> Option<Instant> {
        [
            git_refresh.then(|| self.git_refresh_deadline()).flatten(),
            self.pending_agent_resume_wakeup(),
            self.session_saver.deadline(),
            self.pane_resize_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// The session state, read only. Every mutation from outside `app/` is a
    /// named `App` method.
    pub(crate) fn state(&self) -> &AppState {
        &self.state
    }

    /// The live runtime of `pane_id`; `None` for a closed or unknown pane and
    /// for one whose shell failed to start or awaits its agent resume.
    pub(crate) fn pane_runtime(
        &self,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<&shepr_mux::pane::PaneRuntime> {
        self.lookup_runtime(pane_id)
    }

    /// The pure render inputs: shared references to state and runtimes.
    pub(crate) fn render_view(&self) -> RenderView<'_> {
        RenderView {
            state: &self.state,
            runtimes: &self.terminal_runtimes,
        }
    }

    /// The notice sent to every client that connects this boot, if the
    /// restore did not bring the saved session back in full.
    pub(crate) fn restore_notice(&self) -> Option<&shepr_protocol::SessionRestoreNotice> {
        self.restore_notice.as_ref()
    }

    /// Reaps a finished save and starts the next one when it is due.
    pub(crate) fn service_session_saves(&mut self, now: Instant) {
        // The persister's completion signal wakes the loop when a save ends;
        // reaping it may make the next save (or a held checkpoint) startable.
        let save_reaped = self.reap_finished_session_save();
        if save_reaped || self.session_saver.is_due(now) {
            self.start_background_session_save();
        }
    }

    /// Drops every runtime and waits for their teardowns; false on timeout.
    pub(crate) fn shut_down_pane_runtimes(&mut self, timeout: Duration) -> bool {
        self.terminal_runtimes.clear();
        self.wait_for_pane_teardowns(timeout)
    }

    /// The bookmark follows an active client's navigation. Returns whether
    /// the bookmark moved.
    pub(crate) fn navigate_bookmark(&mut self, workspace_id: &shepr_protocol::WorkspaceId) -> bool {
        self.state.set_bookmark(workspace_id)
    }
}

/// Seams for server tests that set up or inspect state the loop never
/// reaches into. Tests under `app/` use the fields directly.
#[cfg(test)]
impl App {
    pub(crate) fn test_state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub(crate) fn test_runtimes_mut(&mut self) -> &mut shepr_mux::pane::PaneRuntimeRegistry {
        &mut self.terminal_runtimes
    }

    pub(crate) fn test_saver(&mut self) -> &mut session::SessionSaver {
        &mut self.session_saver
    }

    pub(crate) fn test_paths(&self) -> &shepr_paths::AppPaths {
        &self.paths
    }

    pub(crate) fn test_set_restore_notice(
        &mut self,
        notice: Option<shepr_protocol::SessionRestoreNotice>,
    ) {
        self.restore_notice = notice;
    }
}

#[cfg(test)]
impl App {
    /// Makes every later pane launch run `shell` as a non-login shell. The
    /// launcher is the one holder of the shell, so this is the only way a
    /// test changes it; there is no settings copy to fall out of step.
    pub(crate) fn set_test_shell(&mut self, shell: impl AsRef<std::path::Path>) {
        let shell = shepr_test_support::fixture::resolved_shell(shell);
        self.pane_launcher = self
            .pane_launcher
            .clone()
            .with_shell(shepr_mux::pane::PaneShellConfig::new(&shell, false));
    }
}

#[cfg(test)]
mod test_app;
#[cfg(test)]
pub(crate) use test_app::TestApp;
#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
pub(crate) use api::session::SnapshotAgent;
#[cfg(test)]
pub(crate) use api::test_support::exiting_test_command;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::IsolatedEnv;
    use crate::test_support::*;
    use shepr_config::ServerConfig;
    use shepr_mux::workspace::Workspace;

    impl App {
        /// Test constructor: the app's files live in a fresh scratch directory.
        #[expect(
            clippy::new_ret_no_self,
            reason = "builds the app together with its outputs, the pair a test holds as one harness"
        )]
        pub(crate) fn new(config: &ServerConfig) -> TestApp {
            use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
            let scratch = crate::test_support::ScratchDir::new("app");
            let paths = shepr_paths::AppPaths::test_at(&scratch);
            let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
                config.clone(),
                paths.clone(),
            );
            let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir())
                .expect("test session lease");
            let (app, outputs) = Self::open(&config, &paths, lease, test_clock());
            TestApp::new(app, outputs)
        }

        /// Installs `runtime` for a pane in the same registry production uses.
        /// Panics if the pane is not in a workspace.
        pub(crate) fn insert_test_runtime(
            &mut self,
            pane_id: shepr_core::layout::PaneId,
            runtime: shepr_mux::pane::PaneRuntime,
        ) {
            assert!(
                self.state.pane(pane_id).is_some(),
                "test runtime pane must be in a workspace"
            );
            self.install_runtime(pane_id, runtime);
        }

        /// Installs an idle test runtime for a pane, so events can be
        /// delivered as that runtime's with `from_pane_runtime`.
        pub(crate) fn insert_idle_test_runtime(&mut self, pane_id: shepr_core::layout::PaneId) {
            self.insert_test_runtime(
                pane_id,
                shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b""),
            );
        }

        /// `event` as the pane's current runtime publishes it: inside the
        /// envelope carrying that runtime's generation, which admission
        /// checks. Panics if the pane has no runtime.
        ///
        /// A runtime-produced event has no bare form: the transport type only
        /// carries it in its envelope. There is deliberately no test-only way
        /// past admission either: the generation check is what the envelope
        /// is for.
        #[expect(
            clippy::wrong_self_convention,
            reason = "reads as `app.from_pane_runtime(pane, event)` at every test call site: the event as the pane's runtime publishes it"
        )]
        pub(crate) fn from_pane_runtime(
            &self,
            pane_id: shepr_core::layout::PaneId,
            event: shepr_mux::events::RuntimeEvent,
        ) -> AppEvent {
            event.enveloped(pane_id, self.test_runtime(pane_id).generation())
        }

        /// Looks up a pane's runtime in the registry.
        pub(crate) fn test_runtime(
            &self,
            pane_id: shepr_core::layout::PaneId,
        ) -> &shepr_mux::pane::PaneRuntime {
            self.terminal_runtimes
                .get(&pane_id)
                .expect("pane must have a live runtime")
        }
    }

    pub(super) fn test_clock() -> AppClock {
        AppClock {
            now: Instant::now(),
            wall_now: SystemTime::now(),
        }
    }

    pub(super) fn test_app() -> TestApp {
        let mut app = App::new(&ServerConfig::default());
        app.set_test_shell(exiting_test_command());
        app
    }

    #[tokio::test]
    async fn create_default_workspace_creates_one_workspace_only_when_none_exist() {
        let mut app = test_app();
        let geometry = app.headless_spawn_geometry();

        assert_eq!(
            app.create_default_workspace(geometry),
            DefaultWorkspace::Created
        );
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.ws(0).spawn_geometry(), Some(geometry));
        assert_eq!(
            app.create_default_workspace(geometry),
            DefaultWorkspace::Exists
        );
        assert_eq!(app.state.workspaces.len(), 1);
    }

    #[test]
    fn workspace_seed_cwd_comes_from_the_named_workspace_not_the_bookmark() {
        let mut app = test_app();
        let first = Workspace::test_at(Some("shepr"), std::path::Path::new("/shepr-test/shepr"));
        let second = Workspace::test_at(Some("pion"), std::path::Path::new("/shepr-test/pion"));

        app.state.test_set_workspaces(vec![first, second]);
        app.state.seed_bookmark_index(Some(0));

        let followed = app.state.ws(1).id();
        let seed_cwd = app
            .seed_cwd_from_workspace(&followed)
            .expect("test precondition");

        assert_eq!(seed_cwd, std::path::PathBuf::from("/shepr-test/pion"));
    }

    #[test]
    fn new_terminal_cwd_follow_uses_source_cwd() {
        let cwd = creation::resolve_new_terminal_cwd(
            &shepr_config::NewTerminalCwd::Follow,
            None,
            &shepr_core::absolute_path::AbsolutePath::root(),
            Some(
                shepr_core::absolute_path::AbsolutePath::new("/shepr-test/shepr-source")
                    .expect("absolute"),
            ),
        );

        assert_eq!(cwd, std::path::PathBuf::from("/shepr-test/shepr-source"));
    }

    #[test]
    fn new_terminal_cwd_follow_without_source_uses_home() {
        let env = IsolatedEnv::new();
        let home = shepr_core::absolute_path::AbsolutePath::new(env.home()).expect("absolute home");

        let cwd = creation::resolve_new_terminal_cwd(
            &shepr_config::NewTerminalCwd::Follow,
            Some(&home),
            &shepr_core::absolute_path::AbsolutePath::root(),
            None,
        );

        assert_eq!(cwd, env.home());
    }

    #[test]
    fn new_terminal_cwd_path_uses_configured_path() {
        let cwd = creation::resolve_new_terminal_cwd(
            &shepr_config::NewTerminalCwd::Path(
                shepr_core::absolute_path::AbsolutePath::new("/shepr-test/shepr-fixed")
                    .expect("absolute"),
            ),
            None,
            &shepr_core::absolute_path::AbsolutePath::root(),
            Some(
                shepr_core::absolute_path::AbsolutePath::new("/shepr-test/shepr-source")
                    .expect("absolute"),
            ),
        );

        assert_eq!(cwd, std::path::PathBuf::from("/shepr-test/shepr-fixed"));
    }

    #[test]
    fn app_deadline_is_the_save_deadline() {
        let mut app = test_app();
        let now = Instant::now();
        app.session_saver
            .set_autosave_deadline(Some(now + Duration::from_secs(2)));

        assert_eq!(
            app.next_deadline(true),
            app.session_saver.autosave_deadline()
        );
    }

    #[test]
    fn app_deadline_is_none_when_idle() {
        let mut app = test_app();
        app.session_saver.set_autosave_deadline(None);
        app.state.test_set_workspaces(Vec::new());

        assert_eq!(app.next_deadline(true), None);
    }
}
