use super::*;

// Every pane ending is recorded with the pane's exit arbiter, and the launch
// coordinator publishes the first one. Reader failure can leave a live child
// without an output reader, so it decides at once and the app tears the pane
// down; IO failure checkpoints the usable terminal, a core panic does not. A
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
    let ending = |reason| PaneEnding::Observed {
        reason,
        child_exit_confirmed: false,
        ended_at: std::time::Instant::now(),
    };
    Box::new(move |exit| match exit {
        ReaderExit::ShutdownRequested => {}
        ReaderExit::Closed => {
            let closed = ending(shepr_platform::ChildExitReason::TerminalClosed);
            if arbiter.decide_after(closed_grace, closed) {
                warn!(
                    pane = pane_id.raw(),
                    "pane terminal closed and its child's exit was not reported in time; ending the pane"
                );
            }
        }
        ReaderExit::Panicked => {
            arbiter.decide(ending(shepr_platform::ChildExitReason::ReaderPanicked));
        }
        ReaderExit::IoFailed => {
            arbiter.decide(ending(shepr_platform::ChildExitReason::ReaderIoFailed));
        }
    })
}

pub(super) fn prepare_terminal(
    pane_id: PaneId,
    geometry: shepr_core::geometry::PaneGeometry,
    scrollback_limit_bytes: usize,
    host_terminal_theme: shepr_term::host::TerminalTheme,
    host_terminal_appearance: Option<shepr_term::host::HostAppearance>,
    initial_history_ansi: Option<&str>,
    local_host: Option<Arc<str>>,
) -> Arc<PaneTerminal> {
    let cols = geometry.cols();
    let rows = geometry.rows();
    let terminal = shepr_vt::Terminal::new(cols, rows, scrollback_limit_bytes);
    let pane_terminal = PaneTerminal::new_with_pane_id(pane_id, terminal, local_host);
    // The cached size below claims the cell size, so the terminal learns it
    // now: a later `resize` to the same geometry is a no-op and would never
    // tell it. Nothing has enabled in-band size reports on a fresh
    // terminal, so there is no reply to route.
    let _ = pane_terminal.resize(geometry);
    pane_terminal.apply_host_terminal_theme(host_terminal_theme);
    let _ = pane_terminal.apply_host_terminal_appearance(host_terminal_appearance);
    if let Some(ansi) = initial_history_ansi {
        // Seeding records row provenance before the child can write. The
        // detector excludes unchanged saved rows from its live snapshot.
        pane_terminal.seed_history_ansi(ansi);
    }
    Arc::new(pane_terminal)
}

/// Startup borrows the owners it wires into the actor. On success the caller
/// starts the child watcher; on failure this path tears down and reaps first.
struct PtySetup<'a> {
    pane_id: PaneId,
    geometry: shepr_core::geometry::PaneGeometry,
    cmd: &'a shepr_pty::PtyCommand,
    terminal: &'a Arc<PaneTerminal>,
    cwd_state: &'a Arc<PaneCwdState>,
    events: &'a crate::events::EventSender,
    render_notify: &'a Arc<Notify>,
    render_dirty: &'a Arc<RenderSignal>,
    teardown_tracker: &'a Arc<PaneTeardownTracker>,
    exit_arbiter: &'a Arc<PaneExitArbiter>,
}

struct StartedPty {
    child: shepr_pty::backend::PaneChild,
    child_liveness: Arc<ChildLiveness>,
    io: Box<dyn ChildIo>,
    launch: super::launch_status::LaunchStatus,
}

impl PtySetup<'_> {
    fn start(self) -> std::io::Result<StartedPty> {
        let Self {
            pane_id,
            geometry,
            cmd,
            terminal,
            cwd_state,
            events,
            render_notify,
            render_dirty,
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
        .inspect_err(|err| error!(pane = pane_id.raw(), error = %err, "failed to spawn shell"))?;

        let mut child = spawned.child;
        let master_fd = spawned.master_fd;
        let launch = super::launch_status::LaunchStatus {
            channel: status_channel,
            registration: spawned.status,
            cwd_candidates: spawned.cwd_candidates,
            // Launch status stores this as text and rebuilds the failure path;
            // preserving non-UTF-8 bytes needs a path type through that payload.
            program: cmd.program().to_string_lossy().into_owned(),
        };
        let pid = child.process_id();
        crate::logging::pane_spawned(pane_id.raw(), pid);
        let child_liveness = Arc::new(ChildLiveness::launching(child.handle()));
        let io: Box<dyn ChildIo> = {
            // Failure cleanup and read effects use the same child identity.
            let startup_child_liveness = Arc::clone(&child_liveness);
            let health_terminal = Arc::clone(terminal);
            let effects = Arc::new(PaneReadEffects {
                pane_id,
                terminal: Arc::clone(terminal),
                render_notify: Arc::clone(render_notify),
                render_dirty: Arc::clone(render_dirty),
                cwd: Arc::clone(cwd_state),
                events: events.clone(),
                child_liveness: Arc::clone(&child_liveness),
                sync_timeout_render: SyncTimeoutRender::default(),
                deferred_effect_order: Arc::default(),
                timer_writer: std::sync::OnceLock::new(),
                timer_reply_drop_reported: AtomicBool::new(false),
                rt: tokio::runtime::Handle::current(),
            });
            let read_effects = Arc::clone(&effects);
            let output = PaneOutputWriter {
                pane_id,
                terminal: Arc::clone(terminal),
            };
            let on_read = Box::new(move |bytes: &[u8]| read_effects.read(&output, bytes));
            let on_reader_exit = reader_exit_callback(
                pane_id,
                Arc::clone(exit_arbiter),
                crate::limits::TERMINAL_CLOSED_EXIT_GRACE,
            );
            let actor = PtyIoActor::spawn(PtyIoActorConfig {
                pane_id,
                master_fd,
                on_read,
                on_reader_exit,
                // A render, detection or API read that panicked while holding
                // the core lock breaks it for good; end the pane within the
                // actor's idle poll even if the child never prints again.
                core_broken: Box::new(move || health_terminal.core_poisoned()),
            });
            let actor = match actor {
                Ok(actor) => actor,
                Err(err) => {
                    // Actor startup consumes and closes the PTY master on
                    // failure, but the child and any session members still
                    // need the pane teardown sequence before we return.
                    shutdown_pane_processes(
                        pane_id,
                        Arc::clone(&startup_child_liveness),
                        teardown_tracker,
                    );
                    if let Err(kill_err) = child.kill() {
                        warn!(
                            pane = pane_id.raw(),
                            %pid,
                            error = %kill_err,
                            "failed to kill pane child after PTY actor startup failed"
                        );
                    }
                    // Startup is synchronous on its caller. Keep a delayed
                    // child exit from stalling the server loop by handing it
                    // to the child watcher's detached reaper.
                    super::child_watcher::reap_after_startup_failure(
                        pane_id,
                        child,
                        Some(startup_child_liveness),
                    );
                    return Err(err);
                }
            };
            // `timer_writer` was created empty above and this is its only
            // `set`, so it cannot already hold a handle.
            effects.timer_writer.set(actor.clone()).ok();
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
    shell: shepr_core::shell::ResolvedShell,
    login_shell: bool,
    scrollback_limit_bytes: usize,
    /// The server's host name, resolved once at its startup (`None` when it
    /// could not be), which every pane matches OSC 7 `file://` reports
    /// against.
    local_host: Option<Arc<str>>,
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
    pub cwd: &'a std::path::Path,
    pub kind: LaunchKind,
    pub initial_history: Option<&'a str>,
    pub presentation: LaunchPresentation,
}

impl PaneLauncher {
    pub fn new(
        handles: PaneSpawnHandles,
        shell: PaneShellConfig<'_>,
        scrollback_limit_bytes: usize,
        local_host: Option<Arc<str>>,
    ) -> Self {
        Self {
            handles,
            shell: shell.default_shell.clone(),
            login_shell: shell.login_shell,
            scrollback_limit_bytes,
            local_host,
        }
    }

    /// The same launcher with another shell: the seam a test uses to run a
    /// stand-in shell without rebuilding the launcher's handles. The launcher
    /// stays the one holder of the shell, so there is no second copy to keep
    /// in step.
    #[must_use]
    pub fn with_shell(mut self, shell: PaneShellConfig<'_>) -> Self {
        self.shell = shell.default_shell.clone();
        self.login_shell = shell.login_shell;
        self
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
            initial_history,
            presentation,
        } = request;
        let launch_env =
            PaneLaunchEnv::new(self.handles.socket_path.clone()).with_pane_id(public_id);
        let (host_terminal_theme, host_terminal_appearance) = match presentation {
            LaunchPresentation::Live { theme, appearance } => (theme, appearance),
            LaunchPresentation::Saved(theme) => (theme, None),
        };
        let scrollback_limit_bytes = self.scrollback_limit_bytes;
        let render_notify = &self.handles.render_notify;
        let render_dirty = &self.handles.render_dirty;
        let mut cmd = pane_shell_command_builder(
            PaneShellConfig::new(&self.shell, self.login_shell),
            launch_kind,
        );
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, &launch_env);
        let teardown_tracker = Arc::clone(&self.handles.pane_teardowns);
        // One geometry is what the PTY, the terminal and the cached size all
        // start from, so the first `TIOCSWINSZ` carries the pixel dimensions
        // and a later `resize` to the same size is a no-op.
        let rows = geometry.rows();
        let cols = geometry.cols();
        crate::logging::pane_spawn_started(pane_id.raw(), rows, cols, scrollback_limit_bytes);

        let terminal = prepare_terminal(
            pane_id,
            geometry,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            initial_history,
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
        let StartedPty {
            child,
            child_liveness,
            io,
            launch,
        } = PtySetup {
            pane_id,
            geometry,
            cmd: &cmd,
            terminal: &terminal,
            cwd_state: &cwd_state,
            events: &events,
            render_notify,
            render_dirty,
            teardown_tracker: &teardown_tracker,
            exit_arbiter: &exit_arbiter,
        }
        .start()?;

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
        let detect_handle = Some(super::detection_task::DetectionTask::spawn(
            pane_id,
            launch_kind,
            launch,
            super::detection_task::DetectionHandles {
                terminal: Arc::clone(&terminal),
                child_liveness: Arc::clone(&child_liveness),
                lifecycle_authority: Arc::clone(&full_lifecycle_authority_active),
                reset: Arc::clone(&detect_reset_notify),
                events: events.clone(),
                render_notify: Arc::clone(render_notify),
                render_dirty: Arc::clone(render_dirty),
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
            detect_handle,
        })
    }
}
