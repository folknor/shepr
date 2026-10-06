use super::*;
use crate::limits::TERMINAL_CLOSED_EXIT_GRACE;

// Every pane ending is recorded with the pane's exit arbiter, and the launch
// coordinator publishes the first one. Reader failure can leave a live child
// without an output reader, so it decides at once and the app tears the pane
// down; `PaneEnding` decides which of these are checkpointed. A
// closed terminal is how a pane normally ends, so the child watcher gets
// `closed_grace` (`TERMINAL_CLOSED_EXIT_GRACE` in production) to record the
// real exit. If it has not by then, the pane ends anyway: usually the child
// closed its terminal and kept going, though a watcher that was merely slow
// looks the same. The reader never knows the child is gone, so its endings
// are unconfirmed. The actor calls this after closing the PTY master, so the
// wait holds none.
pub(super) fn reader_exit_callback(
    pane_id: PaneId,
    arbiter: Arc<PaneExitArbiter>,
    closed_grace: std::time::Duration,
) -> Box<dyn FnOnce(ReaderExit) + Send> {
    // clock-io-ok: when the reader saw the ending (a closed terminal ends
    // when it closed, not when its grace runs out).
    let ending = |reason| RecordedEnding::Observed {
        ending: PaneEnding::new(reason),
        child_exit_confirmed: false,
        // clock-io-ok: timestamp the actual reader ending at the IO callback.
        ended_at: std::time::Instant::now(),
    };
    Box::new(move |exit| match exit {
        ReaderExit::ShutdownRequested => {}
        ReaderExit::Closed => {
            let closed = ending(PaneEndReason::TerminalClosed);
            if arbiter.decide_after(closed_grace, closed) {
                warn!(
                    pane = %pane_id,
                    "pane terminal closed and its child's exit was not reported in time; ending the pane"
                );
            }
        }
        ReaderExit::Panicked => {
            arbiter.decide(ending(PaneEndReason::ReaderPanicked));
        }
        ReaderExit::IoFailed => {
            arbiter.decide(ending(PaneEndReason::ReaderIoFailed));
        }
    })
}

pub(super) fn prepare_terminal(
    pane_id: PaneId,
    geometry: shepr_core::geometry::PaneGeometry,
    scrollback: shepr_core::scrollback::ScrollbackBudget,
    host_terminal_theme: shepr_term::host::TerminalTheme,
    host_terminal_appearance: Option<shepr_term::host::HostAppearance>,
    local_host: Option<Arc<shepr_platform::HostNames>>,
) -> Arc<PaneTerminal> {
    let terminal = shepr_vt::Terminal::new(geometry, scrollback);
    let pane_terminal = PaneTerminal::new_with_pane_id(pane_id, terminal, local_host);
    // The terminal was built from `geometry`, cell size included, so this
    // `resize` is a no-op for it. Nothing has enabled in-band size reports on
    // a fresh terminal, so there is no reply to route.
    let _ = pane_terminal.resize(geometry);
    pane_terminal.apply_host_terminal_theme(host_terminal_theme);
    let _ = pane_terminal.apply_host_terminal_appearance(host_terminal_appearance);
    Arc::new(pane_terminal)
}

/// Startup borrows the owners it wires into the actor. On success the caller
/// starts the child watcher; on failure this path schedules teardown and reaping.
struct PtySetup<'a> {
    pane_id: PaneId,
    geometry: shepr_core::geometry::PaneGeometry,
    cmd: &'a shepr_pty::PtyCommand,
    terminal: &'a Arc<PaneTerminal>,
    cwd_state: &'a Arc<PaneCwdState>,
    events: &'a crate::events::EventSender,
    render_notify: &'a Arc<Notify>,
    render_dirty: &'a Arc<RenderSignal>,
    pty_render: &'a PaneRenderSlot,
    teardown_tracker: &'a Arc<PaneTeardownTracker>,
    exit_arbiter: &'a Arc<PaneExitArbiter>,
}

struct StartedPty {
    child: shepr_pty::backend::PaneChild,
    child_liveness: Arc<ChildLiveness>,
    io: Box<dyn ChildIo>,
    launch: super::launch_status::LaunchStatus,
}

#[derive(Debug)]
struct PtySetupFailure {
    stage: &'static str,
    source: std::io::Error,
}

impl std::fmt::Display for PtySetupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.stage, self.source)
    }
}

impl std::error::Error for PtySetupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl PtySetupFailure {
    fn into_io_error(self) -> std::io::Error {
        let kind = self.source.kind();
        std::io::Error::new(kind, self)
    }
}

impl PtySetup<'_> {
    fn start(self) -> Result<StartedPty, PtySetupFailure> {
        let Self {
            pane_id,
            geometry,
            cmd,
            terminal,
            cwd_state,
            events,
            render_notify,
            render_dirty,
            pty_render,
            teardown_tracker,
            exit_arbiter,
        } = self;
        let (status_sender, status_channel) = tokio::sync::oneshot::channel();
        // The fork returns at once; the child changes directory and execs on
        // its own and reports through its status channel (`launch_status`).
        let spawned = shepr_pty::backend::spawn_pty(
            geometry,
            cmd,
            Box::new(move |channel| {
                // A launch whose runtime is gone has no reader for it.
                status_sender.send(channel).ok();
            }),
        )
        .map_err(|source| PtySetupFailure {
            stage: "pty_spawn",
            source,
        })?;

        let child = spawned.child;
        let master_fd = spawned.master_fd;
        let launch = super::launch_status::LaunchStatus {
            channel: status_channel,
            registration: spawned.status,
            cwd_candidates: spawned.cwd_candidates,
            program: std::path::PathBuf::from(cmd.program()),
        };
        let pid = child.process_id();
        crate::pane::logging::pane_spawned(pane_id, pid);
        let child_liveness = Arc::new(ChildLiveness::launching(child.handle()));
        let io: Box<dyn ChildIo> = {
            // Failure cleanup and read effects use the same child identity.
            let startup_child_liveness = Arc::clone(&child_liveness);
            let health_terminal = Arc::clone(terminal);
            let actor = PtyIoActor::spawn_prepared(pane_id, |timer_writer| {
                let effects = Arc::new(PaneReadEffects {
                    pane_id,
                    terminal: Arc::clone(terminal),
                    render_notify: Arc::clone(render_notify),
                    render_dirty: Arc::clone(render_dirty),
                    pty_render: pty_render.clone(),
                    cwd: Arc::clone(cwd_state),
                    events: events.clone(),
                    child_liveness: Arc::clone(&child_liveness),
                    sync_timeout_render: SyncTimeoutRender::default(),
                    deferred_effect_order: Arc::default(),
                    timer_writer: TimerReplyRoute::Actor(timer_writer),
                    rt: tokio::runtime::Handle::current(),
                    // clock-io-ok: the actor and timer are the terminal IO adapters.
                    now: Arc::new(std::time::Instant::now),
                });
                let read_effects = Arc::clone(&effects);
                let output = PaneOutputWriter {
                    pane_id,
                    terminal: Arc::clone(terminal),
                };
                let on_read = move |bytes: &[u8]| read_effects.read(&output, bytes);
                let on_reader_exit = reader_exit_callback(
                    pane_id,
                    Arc::clone(exit_arbiter),
                    TERMINAL_CLOSED_EXIT_GRACE,
                );
                PtyIoActorConfig::new(
                    pane_id,
                    master_fd,
                    on_read,
                    on_reader_exit,
                    // A render, detection or API read that panicked while holding
                    // the core lock breaks it for good; end the pane within the
                    // actor's idle poll even if the child never prints again.
                    move || health_terminal.core_poisoned(),
                )
            });
            let actor = match actor {
                Ok(actor) => actor,
                Err(err) => {
                    // Actor startup consumes and closes the PTY master on
                    // failure, but the child and any session members still
                    // need the pane teardown sequence. Keep its grace periods:
                    // killing the leader here would preempt session escalation.
                    shutdown_pane_processes(pane_id, &startup_child_liveness, teardown_tracker);
                    // Startup is synchronous on its caller. Keep a delayed
                    // child exit from stalling the server loop by handing it
                    // to the child watcher's detached reaper.
                    super::child_watcher::reap_after_startup_failure(
                        pane_id,
                        child,
                        Some(startup_child_liveness),
                    );
                    return Err(PtySetupFailure {
                        stage: "pty_actor_spawn",
                        source: err,
                    });
                }
            };
            Box::new(actor)
        };

        Ok(StartedPty {
            child,
            child_liveness,
            io,
            launch,
        })
    }
}

/// App-owned launch capabilities and validated shell policy. Workspace and
/// restore plans carry identities and geometry, never channels or PTYs.
#[derive(Clone)]
pub struct PaneLauncher {
    handles: PaneSpawnHandles,
    // Captured once per launcher, including HOME; clones and shell changes
    // preserve it while later server instances sample their own environment.
    inherited_command: shepr_pty::PtyCommand,
    scrollback: shepr_core::scrollback::ScrollbackBudget,
    /// The server's host names, resolved once at its startup (`None` when
    /// they could not be), which every pane matches OSC 7 `file://` reports
    /// against.
    local_host: Option<Arc<shepr_platform::HostNames>>,
}

#[derive(Clone)]
pub struct PaneSpawnHandles {
    pub events: mpsc::Sender<AppEvent>,
    pub render_notify: Arc<Notify>,
    pub render_dirty: Arc<RenderSignal>,
    pub pane_teardowns: Arc<PaneTeardownTracker>,
    pub socket_path: std::path::PathBuf,
}

/// Presentation is sampled at execution, since a client can report a new host
/// theme between planning and launching. Restore deliberately has no live
/// appearance: it runs before a client supplies one.
#[derive(Clone, Copy)]
pub enum LaunchPresentation {
    Live {
        theme: shepr_term::host::TerminalTheme,
        appearance: Option<shepr_term::host::HostAppearance>,
    },
    Saved(shepr_term::host::TerminalTheme),
}

#[derive(Clone, Copy)]
pub struct PaneLaunchRequest<'a> {
    pub pane_id: PaneId,
    pub public_id: shepr_protocol::PublicPaneId,
    pub geometry: shepr_core::geometry::PaneGeometry,
    pub cwd: &'a shepr_core::absolute_path::AbsolutePath,
    pub kind: LaunchKind,
    pub presentation: LaunchPresentation,
}

impl PaneLauncher {
    pub fn new(
        handles: PaneSpawnHandles,
        shell: PaneShellConfig<'_>,
        scrollback: shepr_core::scrollback::ScrollbackBudget,
        local_host: Option<Arc<shepr_platform::HostNames>>,
    ) -> Self {
        Self {
            handles,
            inherited_command: pane_shell_command_builder(shell, LaunchKind::Fresh),
            scrollback,
            local_host,
        }
    }

    /// The same launcher with another shell: the seam a test uses to run a
    /// stand-in shell without rebuilding the launcher's handles. The launcher
    /// stays the one holder of the shell, so there is no second copy to keep
    /// in step.
    #[must_use]
    pub fn with_shell(mut self, shell: PaneShellConfig<'_>) -> Self {
        self.inherited_command = self
            .inherited_command
            .clone()
            .with_shell(shell.default_shell, shell.login_shell);
        self
    }

    fn shell_command(&self, launch_kind: LaunchKind) -> shepr_pty::PtyCommand {
        let mut cmd = self.inherited_command.clone();
        if launch_kind.requires_cwd() {
            cmd.require_cwd();
        }
        cmd
    }

    /// Starts the pane's shell. Returns once the child is forked: nothing on
    /// this side of the launch touches the user's filesystem, and the child
    /// reports its chdir and exec through the launch settlement.
    pub fn launch(&self, request: PaneLaunchRequest<'_>) -> std::io::Result<PaneRuntime> {
        let PaneLaunchRequest {
            pane_id,
            public_id,
            geometry,
            cwd,
            kind: launch_kind,
            presentation,
        } = request;
        let launch_env = PaneLaunchEnv::new(self.handles.socket_path.clone(), public_id);
        let (host_terminal_theme, host_terminal_appearance) = match presentation {
            LaunchPresentation::Live { theme, appearance } => (theme, appearance),
            LaunchPresentation::Saved(theme) => (theme, None),
        };
        let scrollback = self.scrollback;
        let render_notify = &self.handles.render_notify;
        let render_dirty = &self.handles.render_dirty;
        let mut cmd = self.shell_command(launch_kind);
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, &launch_env);
        let teardown_tracker = Arc::clone(&self.handles.pane_teardowns);
        // One geometry is what the PTY, the terminal and the cached size all
        // start from, so the first `TIOCSWINSZ` carries the pixel dimensions
        // and a later `resize` to the same size is a no-op.
        let rows = geometry.rows();
        let cols = geometry.cols();
        crate::pane::logging::pane_spawn_started(
            pane_id,
            rows,
            cols,
            scrollback,
            launch_kind,
            cwd,
            std::path::Path::new(self.inherited_command.program()),
        );

        let terminal = prepare_terminal(
            pane_id,
            geometry,
            scrollback,
            host_terminal_theme,
            host_terminal_appearance,
            self.local_host.clone(),
        );

        let generation = crate::events::RuntimeGeneration::alloc();
        let events =
            crate::events::EventSender::runtime(self.handles.events.clone(), pane_id, generation);
        let cwd_state = Arc::new(PaneCwdState::default());
        let full_lifecycle_authority_active = Arc::new(AtomicBool::new(false));
        // Created before the actor, which may end before the watcher exists;
        // the one instance goes to the actor, the watcher and the runtime.
        let exit_arbiter = Arc::new(PaneExitArbiter::default());
        // The pane's render-coalescing state, shared by its read effects and
        // its detection task; the terminal does not hold it.
        let pty_render = PaneRenderSlot::default();
        let started = PtySetup {
            pane_id,
            geometry,
            cmd: &cmd,
            terminal: &terminal,
            cwd_state: &cwd_state,
            events: &events,
            render_notify,
            render_dirty,
            pty_render: &pty_render,
            teardown_tracker: &teardown_tracker,
            exit_arbiter: &exit_arbiter,
        }
        .start();
        let StartedPty {
            child,
            child_liveness,
            io,
            launch,
        } = match started {
            Ok(started) => started,
            Err(failure) => {
                error!(
                    event = "pane.spawn.failure",
                    subsystem = "pane",
                    outcome = "error",
                    pane = %pane_id,
                    public_pane_id = %public_id,
                    kind = ?launch_kind,
                    cwd = %cwd.display(),
                    stage = failure.stage,
                    error = %failure.source,
                    "failed to start pane"
                );
                return Err(failure.into_io_error());
            }
        };

        // Actor setup failures reap the child above without publishing an exit
        // for a pane that was never constructed: the coordinator, the pane's
        // one publisher, starts only here. An ending the reader recorded
        // before it existed is still published.
        let launch = super::launch_status::spawn(
            pane_id,
            launch_kind,
            launch,
            Arc::clone(&child_liveness),
            Arc::clone(&exit_arbiter),
            events.clone(),
        );
        super::child_watcher::spawn(
            pane_id,
            child,
            Arc::clone(&child_liveness),
            Arc::clone(&exit_arbiter),
        );

        let detect_reset_notify = Arc::new(Notify::new());
        let detector_gate_diagnostics = DetectorGateDiagnostics::default();
        let detect_handle = Some(super::detection_task::DetectionTask::spawn(
            pane_id,
            launch_kind,
            launch,
            super::detection_task::DetectionHandles {
                terminal: Arc::clone(&terminal),
                child_liveness: Arc::clone(&child_liveness),
                exit_arbiter: Arc::clone(&exit_arbiter),
                lifecycle_authority: Arc::clone(&full_lifecycle_authority_active),
                reset: Arc::clone(&detect_reset_notify),
                detector_gate_diagnostics: detector_gate_diagnostics.clone(),
                events: events.clone(),
                render_notify: Arc::clone(render_notify),
                render_dirty: Arc::clone(render_dirty),
                pty_render,
            },
        ));

        Ok(PaneRuntime {
            generation,
            pane_id,
            terminal,
            io,
            current_size: geometry,
            child_liveness,
            teardown_tracker,
            exit_arbiter,
            cwd: cwd_state,
            full_lifecycle_authority_active,
            detect_reset_notify,
            detector_gate_diagnostics,
            detect_handle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_environment_is_stable_but_a_new_launcher_samples_again() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("launcher-environment");
        let shell =
            shepr_test_support::fixture::resolved_shell(shepr_test_support::fixture::path_str());
        let make = || {
            let (events, _) = mpsc::channel(1);
            PaneLauncher::new(
                PaneSpawnHandles {
                    events,
                    render_notify: Arc::new(Notify::new()),
                    render_dirty: Arc::new(RenderSignal::new()),
                    pane_teardowns: Arc::default(),
                    socket_path: scratch.join("server.sock"),
                },
                PaneShellConfig::new(&shell, false),
                shepr_core::scrollback::ScrollbackBudget::new(0),
                None,
            )
        };
        env.set(shepr_core::env::EnvVar::Home, "/first");
        let first = make();
        env.set(shepr_core::env::EnvVar::Home, "/second");
        let second = make();
        for launcher in [
            first.clone().with_shell(PaneShellConfig::new(&shell, true)),
            first,
        ] {
            assert_eq!(
                launcher
                    .shell_command(LaunchKind::Fresh)
                    .get_env(shepr_core::env::EnvVar::Home),
                Some(std::ffi::OsStr::new("/first")),
            );
        }
        assert_eq!(
            second
                .shell_command(LaunchKind::Fresh)
                .get_env(shepr_core::env::EnvVar::Home),
            Some(std::ffi::OsStr::new("/second")),
        );
    }
}
