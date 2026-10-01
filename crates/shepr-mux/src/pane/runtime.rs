use std::cell::Cell;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use ratatui::layout::Rect;
use tokio::sync::{Notify, mpsc};
use tracing::{error, info, warn};

use super::PaneClearError;
use super::launch::*;
use super::process_probe::*;
use super::teardown::*;
use super::terminal::{PaneTerminal, ProcessBytesResult};
use super::*;
use crate::UsableCwd;
use crate::events::AppEvent;
use crate::render_signal::RenderSignal;
use shepr_core::layout::PaneId;
use shepr_pty::ChildIo;
use shepr_pty::actor::{PtyIoActor, PtyIoActorConfig, PtyIoActorHandle, PtyReadResult, ReaderExit};

pub struct TerminalDirtyPatchSnapshot {
    pub patch: TerminalDirtyPatchOutcome,
    pub content_revision: u64,
    pub scroll_metrics: Option<ScrollMetrics>,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub alternate_screen_active: bool,
}

// ---------------------------------------------------------------------------
// PaneRuntime - PTY, parser, channels, background tasks
// ---------------------------------------------------------------------------

/// Owns the pane child while its watcher awaits the pidfd. If the watcher is
/// dropped before it reaps (the runtime shutting down while the child still
/// runs), the child is handed to a detached thread that waits for it, so it
/// never stays a zombie for the rest of the process.
struct UnreapedChild(Option<std::process::Child>);

impl UnreapedChild {
    fn take(&mut self) -> Option<std::process::Child> {
        self.0.take()
    }
}

impl Drop for UnreapedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if let Ok(None) = child.try_wait() {
            let spawned = std::thread::Builder::new()
                .name("shepr-pane-reaper".into())
                .spawn(move || {
                    // The pane is gone, so its exit status has no reader; only
                    // a failed reap (a possible zombie) is worth a line.
                    if let Err(err) = child.wait() {
                        tracing::warn!(
                            pid = child.id(),
                            error = %err,
                            "could not reap an abandoned pane child"
                        );
                    }
                });
            if let Err(err) = spawned {
                tracing::warn!(error = %err, "could not start a reaper for an abandoned pane child");
            }
        }
    }
}

async fn wait_for_child_exit(
    child: std::process::Child,
    pidfd: Option<OwnedFd>,
) -> std::io::Result<std::process::ExitStatus> {
    let mut child = UnreapedChild(Some(child));
    let Some(pidfd) = pidfd else {
        return wait_for_child_exit_blocking(child).await;
    };
    let async_pidfd = match tokio::io::unix::AsyncFd::new(pidfd) {
        Ok(async_pidfd) => async_pidfd,
        Err(err) => {
            tracing::debug!(error = %err, "could not register child pidfd; falling back to child wait");
            return wait_for_child_exit_blocking(child).await;
        }
    };
    if let Err(err) = async_pidfd.readable().await {
        tracing::debug!(error = %err, "child pidfd readiness failed; falling back to child wait");
        return wait_for_child_exit_blocking(child).await;
    }

    match shepr_platform::reap_pidfd(async_pidfd.get_ref().as_fd()) {
        Ok(status) => {
            // waitid(P_PIDFD, WEXITED) reaps the child, so dropping its
            // std::process::Child wrapper cannot leave a zombie behind.
            drop(child.take());
            Ok(status)
        }
        Err(err) => {
            // Kernels may expose pidfd_open before waitid(P_PIDFD); the child
            // is ready by now, so Child::wait is only a short fallback reap.
            tracing::debug!(error = %err, "waitid on child pidfd failed; falling back to child wait");
            wait_for_child_exit_blocking(child).await
        }
    }
}

async fn wait_for_child_exit_blocking(
    mut child: UnreapedChild,
) -> std::io::Result<std::process::ExitStatus> {
    let Some(mut child) = child.take() else {
        return Err(std::io::Error::other("pane child was already reaped"));
    };
    // A blocking task keeps running once started even if this await is
    // dropped, so the fallback reaps on runtime shutdown too.
    tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(std::io::Error::other)?
}

/// The render a pane needs once a synchronized update (mode 2026) that never
/// ended is force-flushed by its timeout. Every PTY read inside the update
/// asks for it; one sleeping task per pane serves all of those requests
/// instead of one task per read.
#[derive(Debug, Default)]
struct SyncTimeoutRender {
    /// The latest wake-up asked for, while a task is armed; `None` when no
    /// task is sleeping.
    latest: Mutex<Option<std::time::Instant>>,
}

impl SyncTimeoutRender {
    /// Ask for a render at `at`. Returns the instant a new task must first
    /// wake at, or `None` when the armed task will cover it.
    fn arm(&self, at: std::time::Instant) -> Option<std::time::Instant> {
        let mut latest = shepr_vt::lock_auxiliary(&self.latest);
        match *latest {
            Some(armed) => {
                if at > armed {
                    *latest = Some(at);
                }
                None
            }
            None => {
                *latest = Some(at);
                Some(at)
            }
        }
    }

    /// Called by the armed task after waking for `woke_for`. Returns a later
    /// instant to sleep until when a later update asked for one meanwhile;
    /// otherwise disarms and returns `None`, and the task renders. Skipping
    /// the earlier wake is safe: a newer update only begins after the earlier
    /// one ended, and ending an update requests its own render.
    fn next_wake(&self, woke_for: std::time::Instant) -> Option<std::time::Instant> {
        let mut latest = shepr_vt::lock_auxiliary(&self.latest);
        match *latest {
            Some(later) if later > woke_for => Some(later),
            _ => {
                *latest = None;
                None
            }
        }
    }
}

/// Reads a pane shell's live working directory from any thread, so a save can
/// take the probe on the event loop and do the /proc read where the save runs.
pub struct PaneCwdProbe {
    child_liveness: Arc<ChildLiveness>,
    remembered: Arc<Mutex<Option<std::path::PathBuf>>>,
}

impl PaneCwdProbe {
    /// The shell's usable /proc cwd right now, or `None` when the shell has
    /// been reaped (its numeric PID may belong to another process by now),
    /// the cwd cannot be confirmed as a usable directory, or the read failed. A successful
    /// read is remembered for the pane's later saves. Persistence observations
    /// must not change OSC authority or follow-cwd behavior, so nothing else is
    /// touched.
    pub fn read(&self) -> Option<std::path::PathBuf> {
        let pid = self.child_liveness.live_pid()?;
        let cwd = super::process_probe::usable_process_cwd(pid)?;
        if self.child_liveness.live_pid() != Some(pid) {
            return None;
        }
        *shepr_vt::lock_auxiliary(&self.remembered) = Some(cwd.clone());
        Some(cwd)
    }
}

/// PTY runtime for a pane. Owns the terminal and PTY I/O. Dropping it aborts
/// the detection task and shuts down PTY I/O. The child watcher continues until
/// it reaps the child, handing it to a reaper thread if that async watcher is
/// dropped. An armed synchronized-output timer may finish its flush after the
/// runtime is dropped.
pub struct PaneRuntime {
    generation: crate::events::RuntimeGeneration,
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    io: Box<dyn ChildIo>,
    current_size: Cell<shepr_core::geometry::PaneGeometry>,
    child_liveness: Arc<ChildLiveness>,
    teardown_tracker: Arc<super::teardown::PaneTeardownTracker>,
    reported_cwd: Arc<Mutex<Option<ReportedCwd>>>,
    persistence_cwd: Arc<Mutex<Option<std::path::PathBuf>>>,
    full_lifecycle_authority_active: Arc<AtomicBool>,
    detect_reset_notify: Arc<Notify>,
    // Only detection is aborted directly; the child watcher must reap, and
    // synchronized-output timers hold effects weakly while sleeping.
    detect_handle: Option<tokio::task::AbortHandle>,
}

/// Hand a once-only terminal-reply closure to a [`ChildIo`], whose methods
/// take `FnMut` to stay object-safe.
fn write_terminal_response(io: &dyn ChildIo, response: impl FnOnce() -> Option<Bytes>) {
    let mut response = Some(response);
    io.write_terminal_response(&mut || response.take().and_then(|response| response()));
}

/// Feeds bytes to the pane terminal parser and advances its content and
/// detection revisions. This parser seam does not dispatch the read effects
/// that the PTY reader routes, such as terminal replies, render requests,
/// clipboard writes, cwd reports or synchronized-output timers.
#[derive(Clone)]
pub struct PaneOutputWriter {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
}

/// A parser write holding the terminal core, excluding snapshot collection.
pub struct PaneOutputWrite<'a> {
    writer: &'a PaneOutputWriter,
    core: Option<std::sync::MutexGuard<'a, super::terminal::PaneTerminalCore>>,
}

impl PaneOutputWriter {
    /// Acquire the core before parsing; acquiring alone publishes no revision.
    pub fn begin(&self) -> PaneOutputWrite<'_> {
        PaneOutputWrite {
            writer: self,
            core: shepr_vt::lock_terminal_core(&self.terminal.core).ok(),
        }
    }

    /// Return immediately if a snapshot or another mutation holds the core.
    pub fn try_begin(&self) -> Option<PaneOutputWrite<'_>> {
        Some(PaneOutputWrite {
            writer: self,
            core: Some(shepr_vt::try_lock_terminal_core(&self.terminal.core).ok()?),
        })
    }
}

impl PaneOutputWrite<'_> {
    /// Process `bytes` in the terminal parser and advance content revisions.
    /// Effects produced by the parser are intentionally not dispatched here;
    /// this seam is for tests that need to seed or mutate terminal contents.
    pub fn write(self, bytes: &[u8]) {
        let _ = self.process(bytes, std::time::Instant::now());
    }

    fn process(self, bytes: &[u8], now: std::time::Instant) -> ProcessBytesResult {
        let Some(core) = self.core else {
            return ProcessBytesResult {
                core_poisoned: true,
                ..ProcessBytesResult::default()
            };
        };
        self.writer
            .terminal
            .process_pty_bytes_locked(self.writer.pane_id, bytes, now, core)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

/// The last accepted OSC 7 report, with the pane shell's /proc cwd sampled
/// when it arrived.
///
/// OSC 7 carries what /proc cannot: a logical path through symlinks, or the
/// directory of a program the pane shell's /proc entry does not describe (a
/// nested shell, a root shell under `sudo`). It goes stale when the shell
/// changes directory without emitting a new report. The sample tells the two
/// apart: while the shell's /proc cwd still equals it, nothing the shell did
/// is newer than the report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportedCwd {
    path: std::path::PathBuf,
    shell_cwd_at_report: Option<std::path::PathBuf>,
}

impl ReportedCwd {
    /// The pane cwd given the shell's current /proc cwd: the report while the
    /// shell has not moved since it arrived, otherwise the shell's own cwd.
    fn resolve(
        reported: Option<&Self>,
        shell_cwd: Option<std::path::PathBuf>,
    ) -> Option<std::path::PathBuf> {
        match (shell_cwd, reported) {
            (Some(shell_cwd), Some(reported))
                if reported.shell_cwd_at_report.as_ref() == Some(&shell_cwd) =>
            {
                Some(reported.path.clone())
            }
            (Some(shell_cwd), _) => Some(shell_cwd),
            (None, reported) => reported.map(|reported| reported.path.clone()),
        }
    }
}

fn follow_cwd_from_processes(
    shell_pid: Option<u32>,
    foreground_pgid: Option<u32>,
    pane_cwd: impl FnOnce() -> Option<std::path::PathBuf>,
    foreground_group_cwd: impl FnOnce(u32) -> Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    match (shell_pid, foreground_pgid) {
        (Some(shell_pid), Some(foreground_pgid)) if shell_pid != foreground_pgid => {
            foreground_group_cwd(foreground_pgid).or_else(pane_cwd)
        }
        _ => pane_cwd(),
    }
}

fn publish_reported_cwd(
    pane_id: PaneId,
    child_liveness: &ChildLiveness,
    cwd: std::path::PathBuf,
    reported_cwd: &Arc<Mutex<Option<ReportedCwd>>>,
    events: &crate::events::EventSender,
) {
    let Some(cwd) = UsableCwd::new(cwd) else {
        return;
    };
    // One readlink per OSC 7, sampled before taking the lock.
    let shell_cwd_at_report = child_liveness.live_pid().and_then(|pid| {
        let shell_cwd = shepr_agent::detect::process_cwd(pid);
        (child_liveness.live_pid() == Some(pid))
            .then_some(shell_cwd)
            .flatten()
    });
    let mut last_reported = shepr_vt::lock_auxiliary(reported_cwd);
    if let Some(last) = last_reported.as_mut()
        && last.path == cwd.as_path()
    {
        // A repeated report is not a new event, but it is fresh evidence
        // that the path is current wherever the shell now is.
        last.shell_cwd_at_report = shell_cwd_at_report;
        return;
    }
    // The dedupe slot is updated only once the event is queued: if the shared
    // channel is full, the next identical OSC 7 must retry instead of being
    // swallowed as a duplicate of a report AppState never saw. Keep the lock
    // through the nonblocking enqueue and store so concurrent publishers queue
    // cwd changes in the same order they update the dedupe slot.
    match events.try_send(AppEvent::TerminalCwdReported {
        pane_id,
        cwd: cwd.clone(),
    }) {
        Ok(()) => {
            *last_reported = Some(ReportedCwd {
                path: cwd.into_path_buf(),
                shell_cwd_at_report,
            });
        }
        Err(err) => {
            drop(last_reported);
            warn!(
                pane = pane_id.raw(),
                error = %err,
                "failed to send terminal cwd report"
            );
        }
    }
}

/// What a pane's PTY read callback and its synchronized-output timer share,
/// behind one `Arc`: a read that defers work clones one pointer, not a dozen.
struct PaneReadEffects {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<RenderSignal>,
    reported_cwd: Arc<Mutex<Option<ReportedCwd>>>,
    events: crate::events::EventSender,
    child_liveness: Arc<ChildLiveness>,
    sync_timeout_render: SyncTimeoutRender,
    deferred_effect_order: Arc<DeferredEffectOrder>,
    /// The PTY actor's handle, set once the actor exists; the timer queues
    /// the replies of a flushed frame through it.
    timer_writer: std::sync::OnceLock<PtyIoActorHandle>,
    timer_reply_drop_reported: AtomicBool,
    rt: tokio::runtime::Handle,
}

/// The effects of a terminal write that may block: the `/proc` scan for the
/// default-colour owner and the readlink behind an OSC 7 report. They run
/// with no terminal or reply-order lock held.
struct DeferredEffects {
    ticket: DeferredEffectTicket,
    default_color_generation: Option<u64>,
    reported_cwd: Option<std::path::PathBuf>,
}

/// Serializes the blocking effects produced by ordered terminal writes. The
/// reply-order lock assigns tickets; this gate waits for earlier effects to
/// finish after that lock has been released. Only a write with deferred
/// effects takes a ticket, so the common read never touches it.
#[derive(Default)]
struct DeferredEffectOrder {
    state: Mutex<DeferredEffectOrderState>,
    ready: Condvar,
}

#[derive(Default)]
struct DeferredEffectOrderState {
    next_reserved: u64,
    next_to_apply: u64,
    /// Tickets finished out of turn: dropped without being applied (a panic
    /// or early return between reservation and application). The sequence
    /// skips them once every earlier ticket has finished.
    finished_early: std::collections::BTreeSet<u64>,
    /// Tickets parked in `apply` for an earlier one. Counted under the lock
    /// before waiting, so a finishing ticket only wakes the condvar when
    /// someone is parked on it.
    waiting: usize,
}

impl DeferredEffectOrderState {
    fn finish(&mut self, seq: u64) {
        if seq != self.next_to_apply {
            self.finished_early.insert(seq);
            return;
        }
        self.next_to_apply = self.next_to_apply.wrapping_add(1);
        while self.finished_early.remove(&self.next_to_apply) {
            self.next_to_apply = self.next_to_apply.wrapping_add(1);
        }
    }
}

/// One reserved place in the deferred-effect order. Dropping it finishes
/// that place, whether its effect ran, panicked or was never started, so a
/// lost ticket can never block later effects.
struct DeferredEffectTicket {
    order: Arc<DeferredEffectOrder>,
    seq: u64,
}

impl Drop for DeferredEffectTicket {
    fn drop(&mut self) {
        let mut state = shepr_vt::lock_auxiliary(&self.order.state);
        state.finish(self.seq);
        let parked = state.waiting > 0;
        drop(state);
        if parked {
            self.order.ready.notify_all();
        }
    }
}

impl DeferredEffectOrder {
    /// Called while the terminal reply-order lock is held.
    fn reserve(self: &Arc<Self>) -> DeferredEffectTicket {
        let mut state = shepr_vt::lock_auxiliary(&self.state);
        let seq = state.next_reserved;
        state.next_reserved = state.next_reserved.wrapping_add(1);
        DeferredEffectTicket {
            order: Arc::clone(self),
            seq,
        }
    }
}

impl DeferredEffectTicket {
    /// Runs the effect after every earlier ticket has finished, then
    /// finishes this one (also when the effect panics).
    fn apply(self, effect: impl FnOnce()) {
        let mut state = shepr_vt::lock_auxiliary(&self.order.state);
        if state.next_to_apply != self.seq {
            state.waiting += 1;
            while state.next_to_apply != self.seq {
                state = match self.order.ready.wait(state) {
                    Ok(state) => state,
                    Err(poisoned) => shepr_vt::recover_auxiliary_poison(poisoned),
                };
            }
            state.waiting -= 1;
        }
        drop(state);
        effect();
    }
}

fn has_deferred_effects(result: &ProcessBytesResult) -> bool {
    result.default_color_owner_pending || result.reported_cwd.is_some()
}

/// The initial screen for a child-I/O fixture is state, not output from a
/// live child. Clear every queued parser effect before later writes can collect
/// it as though the child had just produced it.
fn discard_initial_terminal_effects(terminal: &mut shepr_vt::Terminal) {
    let _ = terminal.take_pty_responses();
    let _ = terminal.take_clipboard_writes();
    let _ = terminal.take_dropped_clipboard_store_bytes();
    let _ = terminal.take_pwd_changes();
    let _ = terminal.take_title_update();
    let _ = terminal.take_progress_update();
    let _ = terminal.take_default_color_set();
}

impl PaneReadEffects {
    /// Applies the effects that never block (render and title requests,
    /// clipboard writes) and returns the ones that may, if any. A read with
    /// nothing to defer, the common case, allocates nothing for them.
    /// `ticket` is the write's place in the deferred-effect order, reserved
    /// under the reply-order lock exactly when `has_deferred_effects` held.
    fn apply_immediate(
        &self,
        result: ProcessBytesResult,
        ticket: Option<DeferredEffectTicket>,
    ) -> Option<DeferredEffects> {
        let pane_id = self.pane_id;
        let title_requested =
            result.terminal_title_changed && self.render_dirty.request_terminal_title(pane_id);
        let render_requested = result.request_render
            && self
                .render_dirty
                .request_pty_coalesced(pane_id, &self.terminal.render_queued);
        if title_requested || render_requested {
            self.render_notify.notify_one();
        }
        for content in result.clipboard_writes {
            if let Err(err) = self
                .events
                .try_send(AppEvent::ClipboardWrite { pane_id, content })
            {
                warn!(
                    pane = pane_id.raw(),
                    error = %err,
                    "failed to send OSC 52 clipboard write"
                );
            }
        }
        ticket.map(|ticket| DeferredEffects {
            ticket,
            default_color_generation: result
                .default_color_owner_pending
                .then_some(result.default_color_generation),
            reported_cwd: result.reported_cwd,
        })
    }

    /// Reserves the write's place in the deferred-effect order when it has
    /// deferred effects. Called under the reply-order lock.
    fn reserve_deferred(&self, result: &ProcessBytesResult) -> Option<DeferredEffectTicket> {
        has_deferred_effects(result).then(|| self.deferred_effect_order.reserve())
    }

    fn apply_deferred(&self, deferred: DeferredEffects) {
        // This still blocks the PTY actor after the reply-order lock is
        // released. Moving it off-thread needs a bounded queue ordered by
        // ticket reservation, including timer flushes. OSC 10/11 scans /proc,
        // and OSC 7 validates its path with stat, which can block on a remote
        // mount. An unbounded queue behind one blocked operation could grow
        // without limit; a bounded nonblocking queue needs a policy for
        // coalescing or dropping cwd reports while retaining their order.
        deferred.ticket.apply(|| {
            if let Some(generation) = deferred.default_color_generation {
                self.terminal.resolve_default_color_owner(
                    self.pane_id,
                    &self.child_liveness,
                    generation,
                );
            }
            if let Some(cwd) = deferred.reported_cwd {
                publish_reported_cwd(
                    self.pane_id,
                    &self.child_liveness,
                    cwd,
                    &self.reported_cwd,
                    &self.events,
                );
            }
        });
    }

    /// Makes sure a task will flush the synchronized update this read began
    /// or continued once `delay` passes, so a child that goes quiet inside an
    /// update it never ends still gets its frame shown and its queries
    /// answered. One task per pane serves every read's request.
    fn arm_sync_timeout(self: &Arc<Self>, delay: std::time::Duration) {
        let Some(first_wake) = self
            .sync_timeout_render
            .arm(std::time::Instant::now() + delay)
        else {
            return;
        };
        let effects = Arc::downgrade(self);
        self.rt.spawn(async move {
            let mut wake_at = first_wake;
            loop {
                tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)).await;
                let Some(current_effects) = effects.upgrade() else {
                    return;
                };
                match current_effects.sync_timeout_render.next_wake(wake_at) {
                    Some(later) => {
                        wake_at = later;
                        drop(current_effects);
                    }
                    None => {
                        // The weak reference keeps the pane alive only while
                        // the timer is actively flushing, not while it sleeps.
                        tokio::task::spawn_blocking(move || {
                            current_effects.flush_expired_synchronized_output();
                        });
                        return;
                    }
                }
            }
        });
    }

    /// The timer's half of the runtime tick: flush an expired update, queue
    /// its replies at one point in the reply order (taken before the core
    /// lock, as the reader does), then apply its effects with no lock held.
    fn flush_expired_synchronized_output(&self) {
        let mut tick_result = None;
        let mut deferred_ticket = None;
        let mut tick = || {
            let mut result = self.terminal.tick(std::time::Instant::now());
            deferred_ticket = self.reserve_deferred(&result);
            let replies = std::mem::take(&mut result.terminal_responses);
            tick_result = Some(result);
            replies
        };
        match self.timer_writer.get() {
            Some(writer) => writer.write_terminal_responses(tick),
            // The actor is set right after it spawns, so this is only a timer
            // that beat that store. Flush anyway: the frame must not stay
            // hidden until the child's next output. Its replies have no route.
            None => {
                let replies = tick();
                if !replies.is_empty()
                    && !self.timer_reply_drop_reported.swap(true, Ordering::Relaxed)
                {
                    warn!(
                        pane = self.pane_id.raw(),
                        dropped_replies = replies.len(),
                        "synchronized update replies had no PTY actor route"
                    );
                }
                drop(replies);
            }
        }
        let Some(result) = tick_result else {
            return;
        };
        if result.core_poisoned {
            // The PTY actor checks the poisoned core on every loop, including
            // idle polls, and reports that exit through its broken-core path.
            return;
        }
        if let Some(deferred) = self.apply_immediate(result, deferred_ticket) {
            self.apply_deferred(deferred);
        }
    }
}

impl PaneRuntime {
    pub fn generation(&self) -> crate::events::RuntimeGeneration {
        self.generation
    }

    pub fn apply_host_terminal_theme(&self, theme: shepr_termio::host_term::theme::TerminalTheme) {
        self.terminal.apply_host_terminal_theme(theme);
    }

    pub fn apply_host_terminal_appearance(
        &self,
        appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
    ) {
        write_terminal_response(self.io.as_ref(), || {
            self.terminal.apply_host_terminal_appearance(appearance)
        });
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "runtime construction threads PTY geometry, host context, launch policy, and render hooks"
    )]
    pub fn spawn(
        pane_id: PaneId,
        geometry: shepr_core::geometry::PaneGeometry,
        cwd: &std::path::Path,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: &mpsc::Sender<AppEvent>,
        render_notify: &Arc<Notify>,
        render_dirty: &Arc<RenderSignal>,
        pane_teardowns: &Arc<PaneTeardownTracker>,
    ) -> std::io::Result<Self> {
        Self::spawn_with_initial_history(
            pane_id,
            geometry,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            None,
            events,
            render_notify,
            render_dirty,
            pane_teardowns,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "runtime construction needs to thread PTY size, environment, theme, and render hooks together"
    )]
    pub(crate) fn spawn_with_initial_history(
        pane_id: PaneId,
        geometry: shepr_core::geometry::PaneGeometry,
        cwd: &std::path::Path,
        scrollback_limit_bytes: usize,
        host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
        host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        shell_config: PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        initial_history_ansi: Option<&str>,
        events: &mpsc::Sender<AppEvent>,
        render_notify: &Arc<Notify>,
        render_dirty: &Arc<RenderSignal>,
        pane_teardowns: &Arc<PaneTeardownTracker>,
    ) -> std::io::Result<Self> {
        let mut cmd = pane_shell_command_builder(shell_config);
        cmd.cwd(cwd);
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(&mut cmd, launch_env);
        let launch_purpose = launch_env.purpose();
        let teardown_tracker = Arc::clone(pane_teardowns);
        // The clamped geometry is what the PTY, the terminal and the cached
        // size all start from, so the first `TIOCSWINSZ` carries the pixel
        // dimensions and a later `resize` to the same size is a no-op.
        let geometry = geometry.clamped();
        let rows = geometry.rows();
        let cols = geometry.cols();
        crate::logging::pane_spawn_started(pane_id.raw(), rows, cols, scrollback_limit_bytes);

        let terminal = shepr_vt::Terminal::new(cols, rows, scrollback_limit_bytes);
        let pane_terminal = PaneTerminal::new_with_pane_id(pane_id, terminal);
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
        let terminal = Arc::new(pane_terminal);

        let spawned = shepr_pty::backend::spawn_pty(geometry, &cmd).inspect_err(
            |err| error!(pane = pane_id.raw(), error = %err, "failed to spawn shell"),
        )?;

        let mut child = spawned.child;
        let master_fd = spawned.master_fd;
        let pid = child.id();
        crate::logging::pane_spawned(pane_id.raw(), pid);
        // Opened before the watcher below exists, so nothing can have reaped
        // the child yet and the pid is certainly still this child's.
        let leader = shepr_platform::ProcessHandle::open(pid);
        if leader.is_none() {
            warn!(
                pane = pane_id.raw(),
                "no process handle for the pane's child; closing the pane cannot signal it directly"
            );
        }
        let child_liveness = Arc::new(ChildLiveness::new(pid, leader));
        let generation = crate::events::RuntimeGeneration::alloc();
        let events = crate::events::EventSender::runtime(events.clone(), pane_id, generation);
        let reported_cwd = Arc::new(Mutex::new(None));
        let full_lifecycle_authority_active = Arc::new(AtomicBool::new(false));
        let io: Box<dyn ChildIo> = {
            // The shadowed clone below moves into the read callback; the
            // startup-failure path needs its own handle on the same liveness.
            let startup_child_liveness = Arc::clone(&child_liveness);
            let health_terminal = Arc::clone(&terminal);
            let reader_exit_events = events.clone();
            let effects = Arc::new(PaneReadEffects {
                pane_id,
                terminal: Arc::clone(&terminal),
                render_notify: Arc::clone(render_notify),
                render_dirty: Arc::clone(render_dirty),
                reported_cwd: Arc::clone(&reported_cwd),
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
                terminal: Arc::clone(&terminal),
            };
            let on_read = Box::new(move |bytes: &[u8]| {
                let write = output.begin();
                // Ticks an expired synchronized update first, then parses; the
                // core lock is released when this returns.
                let mut result = write.process(bytes, std::time::Instant::now());
                if result.core_poisoned {
                    // The actor ends the loop and reports the pane dead.
                    return PtyReadResult {
                        terminal_responses: Vec::new(),
                        after_response_order: None,
                        core_broken: true,
                    };
                }
                let deferred_ticket = read_effects.reserve_deferred(&result);
                let terminal_responses = std::mem::take(&mut result.terminal_responses);
                if let Some(delay) = result.render_delay {
                    read_effects.arm_sync_timeout(delay);
                }
                let after_response_order: Option<Box<dyn FnOnce() + Send>> = read_effects
                    .apply_immediate(result, deferred_ticket)
                    .map(|deferred| {
                        let effects = Arc::clone(&read_effects);
                        let run: Box<dyn FnOnce() + Send> =
                            Box::new(move || effects.apply_deferred(deferred));
                        run
                    });
                PtyReadResult {
                    terminal_responses,
                    after_response_order,
                    core_broken: false,
                }
            });
            // Normal reader closure is followed by the child watcher reporting
            // PaneDied. A terminal-core panic or a hard reader IO failure can
            // leave the child alive with no reader, so report those exits and
            // let the app remove the pane and tear down its session. The IO
            // failure checkpoints the still-usable terminal; a panic does not.
            // A later child-watcher report is dropped after pane removal.
            let on_reader_exit: Box<dyn FnOnce(ReaderExit) + Send> = {
                Box::new(move |exit: ReaderExit| {
                    let exit_reason = match exit {
                        ReaderExit::Closed => return,
                        ReaderExit::Panicked => shepr_platform::ChildExitReason::ReaderPanicked,
                        ReaderExit::IoFailed => shepr_platform::ChildExitReason::ReaderIoFailed,
                    };
                    if let Err(err) = reader_exit_events.blocking_send(AppEvent::PaneDied {
                        pane_id,
                        exit_reason,
                    }) {
                        error!(
                            pane = pane_id.raw(),
                            error = %err,
                            "failed to report a pane whose PTY reader failed"
                        );
                    }
                })
            };
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
                        &teardown_tracker,
                    );
                    if let Err(kill_err) = child.kill() {
                        warn!(
                            pane = pane_id.raw(),
                            pid,
                            error = %kill_err,
                            "failed to kill pane child after PTY actor startup failed"
                        );
                    }
                    match child.wait() {
                        Ok(status) => {
                            crate::logging::pane_exited(pane_id.raw(), &status);
                        }
                        Err(wait_err) => {
                            crate::logging::pane_exit_failed(pane_id.raw(), &wait_err.to_string());
                        }
                    }
                    startup_child_liveness.mark_wait_completed();
                    return Err(err);
                }
            };
            // `timer_writer` was created empty above and this is its only
            // `set`, so it cannot already hold a handle.
            effects.timer_writer.set(actor.clone()).ok();
            Box::new(actor)
        };

        // Start the watcher only after the PTY actor exists. If actor setup
        // failed, the error path above reaps the child without reporting the
        // pane as dead before it was ever constructed.
        {
            let child_liveness = Arc::clone(&child_liveness);
            let events = events.clone();
            let pidfd = child_liveness
                .leader()
                .and_then(|leader| match leader.try_clone_pidfd() {
                    Ok(pidfd) => Some(pidfd),
                    Err(err) => {
                        tracing::debug!(
                            pane = pane_id.raw(),
                            pid,
                            error = %err,
                            "could not duplicate child pidfd; falling back to child wait"
                        );
                        None
                    }
                });
            // Await the owned pidfd so each live pane uses no blocking-pool
            // thread; waitid reaps it while Child::wait remains the fallback.
            tokio::spawn(async move {
                let exit_reason = match wait_for_child_exit(child, pidfd).await {
                    Ok(status) => {
                        let exit_reason = shepr_platform::classify_child_exit(&status);
                        crate::logging::pane_exited(pane_id.raw(), &status);
                        exit_reason
                    }
                    Err(e) => {
                        crate::logging::pane_exit_failed(pane_id.raw(), &e.to_string());
                        shepr_platform::ChildExitReason::WaitFailed
                    }
                };
                child_liveness.mark_wait_completed();
                // Wait for channel capacity so this critical pane exit is not dropped.
                if let Err(e) = events
                    .send(AppEvent::PaneDied {
                        pane_id,
                        exit_reason,
                    })
                    .await
                {
                    error!(pane = pane_id.raw(), error = %e, "failed to send PaneDied event");
                }
            });
        }

        // --- Detection task ---
        let (detect_handle, detect_reset_notify) = {
            use std::time::Instant;

            let child_liveness = Arc::clone(&child_liveness);
            let terminal = Arc::clone(&terminal);
            let state_events = events.clone();
            let full_lifecycle_authority_active_for_task =
                Arc::clone(&full_lifecycle_authority_active);
            let render_notify = Arc::clone(render_notify);
            let render_dirty = Arc::clone(render_dirty);
            let detect_reset_notify = Arc::new(Notify::new());
            let detect_reset = Arc::clone(&detect_reset_notify);

            let handle = tokio::spawn(async move {
                let mut detector = DetectorState::new(Instant::now(), launch_purpose);
                let mut next_wake = crate::limits::PROCESS_RECHECK_NO_AGENT;

                loop {
                    if child_liveness.wait_completed() {
                        break;
                    }
                    let tick = next_wake;
                    tokio::select! {
                        _ = tokio::time::sleep(tick) => {}
                        _ = detect_reset.notified() => {
                            detector.reset();
                        }
                    }

                    if child_liveness.wait_completed() {
                        break;
                    }
                    let now = Instant::now();
                    let Some(pid) = child_liveness.live_pid() else {
                        break;
                    };
                    let lifecycle_authority_active =
                        full_lifecycle_authority_active_for_task.load(Ordering::Acquire);
                    let foreground_pgid = match tokio::task::spawn_blocking(move || {
                        shepr_agent::detect::foreground_process_group_id(pid)
                    })
                    .await
                    {
                        Ok(pgid) => pgid,
                        Err(error) => {
                            tracing::warn!(?error, "foreground process group probe failed");
                            continue;
                        }
                    };
                    if child_liveness.live_pid() != Some(pid) {
                        break;
                    }
                    let theme_restore_candidate = terminal.has_theme_restore_candidate();
                    // One sequence per tick, read before any screen snapshot:
                    // the cache may key a snapshot to an older sequence (one
                    // extra read later), never to one newer than its text.
                    let content_seq = shepr_vt::lock_terminal_core(&terminal.core)
                        .map_or(0, |core| core.detection_content_seq);
                    let observations = |observation| DetectorObservations {
                        now,
                        foreground_group: foreground_pgid,
                        content_seq,
                        lifecycle_authority_active,
                        theme_restore_candidate,
                        observation,
                    };
                    let mut output = detector.tick(&observations(TickObservation::Begin));
                    if output.probe {
                        let probe = match tokio::task::spawn_blocking(move || {
                            probe_foreground_process(pid, foreground_pgid)
                        })
                        .await
                        {
                            Ok(probe) => probe,
                            Err(error) => {
                                tracing::warn!(?error, "foreground process probe failed");
                                next_wake = output.next_wake;
                                continue;
                            }
                        };
                        if child_liveness.live_pid() != Some(pid) {
                            break;
                        }
                        output = detector.tick(&observations(TickObservation::Probe(probe)));
                    }
                    if let Some(process_change) = output.process_change.take() {
                        if process_change.should_clear_osc_evidence {
                            clear_osc_evidence_for_agent_transition(
                                &terminal,
                                process_change.previous_agent,
                            );
                        }
                        if let Some(detected_agent) = process_change.process_detected {
                            publish_agent_process_detected_event(
                                state_events.clone(),
                                pane_id,
                                detected_agent,
                                now,
                            )
                            .await;
                        }
                        if process_change.agent_changed {
                            let agent = process_change.agent;
                            if let Some(process_name) = process_change.process_name {
                                info!(
                                    pane = pane_id.raw(),
                                    previous_agent = ?process_change.previous_agent,
                                    ?agent,
                                    process = %process_name,
                                    pgid = ?process_change.process_group_id,
                                    "agent changed"
                                );
                            } else {
                                info!(
                                    pane = pane_id.raw(),
                                    previous_agent = ?process_change.previous_agent,
                                    ?agent,
                                    pgid = ?process_change.process_group_id,
                                    "agent changed"
                                );
                            }
                        }
                    }

                    // The restore check reads /proc only when a known host
                    // theme can be restored; keep that probe off this worker.
                    if child_liveness.live_pid() != Some(pid) {
                        break;
                    }
                    if terminal.has_theme_restore_candidate() {
                        let theme_terminal = Arc::clone(&terminal);
                        let theme_child_liveness = Arc::clone(&child_liveness);
                        match tokio::task::spawn_blocking(move || {
                            theme_terminal
                                .maybe_restore_host_terminal_theme(pane_id, &theme_child_liveness)
                        })
                        .await
                        {
                            Ok(true) => {
                                if render_dirty
                                    .request_pty_coalesced(pane_id, &terminal.render_queued)
                                {
                                    render_notify.notify_one();
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(?error, "host terminal theme probe failed");
                            }
                        }
                    }
                    if child_liveness.live_pid() != Some(pid) {
                        break;
                    }

                    if output.screen {
                        // A plain shell never requests a core snapshot. Identified
                        // agents read screen and OSC together only on a cache miss.
                        output = detector.tick(&observations(TickObservation::Screen(
                            terminal.agent_detection_inputs(),
                        )));
                    }
                    next_wake = output.next_wake;
                    if let Some(update) = output.state_changed {
                        publish_state_changed_event(state_events.clone(), pane_id, update).await;
                    }
                }
            });
            (Some(handle.abort_handle()), detect_reset_notify)
        };

        Ok(Self {
            generation,
            pane_id,
            terminal,
            io,
            current_size: Cell::new(geometry),
            child_liveness,
            teardown_tracker,
            reported_cwd,
            persistence_cwd: Arc::new(Mutex::new(None)),
            full_lifecycle_authority_active,
            detect_reset_notify,
            detect_handle,
        })
    }

    /// A runtime whose child is reached through `io` instead of a spawned
    /// PTY: no child process, no child watcher and no detection task. The
    /// terminal starts with `screen` written to it. `detection_reset` is the
    /// signal a detection task would wait on; with none running, the caller
    /// may watch it to see the resets the runtime is asked for.
    pub fn with_child_io(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        screen: &[u8],
        io: Box<dyn ChildIo>,
        detection_reset: Arc<Notify>,
    ) -> Self {
        let mut terminal = shepr_vt::Terminal::new(cols, rows, scrollback_limit_bytes);
        terminal.write(screen);
        discard_initial_terminal_effects(&mut terminal);
        Self {
            generation: crate::events::RuntimeGeneration::alloc(),
            // Not installed under any layout pane, so it takes an id of its
            // own rather than one some real pane may hold.
            pane_id: PaneId::alloc(),
            terminal: Arc::new(PaneTerminal::new(terminal)),
            io,
            current_size: Cell::new(shepr_core::geometry::PaneGeometry::new(cols, rows, 0, 0)),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            // No child, so no teardown is ever started through this tracker.
            teardown_tracker: Arc::default(),
            reported_cwd: Arc::new(Mutex::new(None)),
            persistence_cwd: Arc::new(Mutex::new(None)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: detection_reset,
            detect_handle: None,
        }
    }

    /// A parser-only writer for tests that feed bytes into the terminal.
    /// The PTY reader uses the same parser but also dispatches its read effects.
    pub fn output_writer(&self) -> PaneOutputWriter {
        PaneOutputWriter {
            pane_id: self.pane_id,
            terminal: Arc::clone(&self.terminal),
        }
    }

    /// Run `hook` during the next dirty-patch collection attempt, including
    /// when it falls back for synchronized output or a full render. The hook
    /// runs while the terminal core lock is held, so it
    /// must not call methods that acquire that lock. A poisoned core
    /// prevents the hook from running.
    pub fn on_next_dirty_collection(&self, hook: Box<dyn FnOnce() + Send>) {
        self.terminal.on_next_dirty_collection(hook);
    }

    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        let previous = self
            .full_lifecycle_authority_active
            .swap(active, Ordering::AcqRel);
        if active && !previous {
            self.detect_reset_notify.notify_one();
        }
    }

    pub fn grid_size(&self) -> shepr_core::geometry::GridSize {
        self.current_size.get().grid
    }

    pub fn content_seq(&self) -> u64 {
        shepr_vt::lock_terminal_core(&self.terminal.core).map_or(0, |core| core.content_revision)
    }

    /// Resize if the dimensions actually changed.
    pub fn resize(&self, geometry: shepr_core::geometry::PaneGeometry) {
        let size = geometry.clamped();
        if self.current_size.get() == size {
            return;
        }
        self.current_size.set(size);
        self.io.resize(size, &mut || {
            // A PTY read holds the same actor reply-order lock while it
            // parses bytes and queues any replies. Resizing the terminal
            // under that lock keeps its replies in the same order as the
            // terminal state that produced them.
            self.terminal.resize(size)
        });
    }

    /// Scroll up by N lines (into scrollback history).
    pub fn scroll_up(&self, lines: usize) {
        self.terminal.scroll_up(lines);
    }

    /// Scroll down by N lines (toward live output).
    pub fn scroll_down(&self, lines: usize) {
        self.terminal.scroll_down(lines);
    }

    pub fn clear_screen(&self) -> Result<(), PaneClearError> {
        self.terminal.clear_screen()
    }

    /// Reset scroll to live view (offset = 0).
    pub fn scroll_reset(&self) {
        self.terminal.scroll_reset();
    }

    /// Set scrollback offset measured from the live bottom of the terminal.
    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.terminal.set_scroll_offset_from_bottom(lines);
    }

    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        self.terminal.scroll_metrics()
    }

    pub fn search_text_window(
        &self,
        query: &str,
        case_sensitive: bool,
        direction: crate::pane::TerminalSearchDirection,
        cursor: crate::pane::TerminalTextPoint,
        previous: Option<(
            crate::pane::TerminalTextPoint,
            crate::pane::TerminalTextPoint,
        )>,
        limit: usize,
    ) -> crate::pane::TerminalSearchWindow {
        self.terminal
            .search_text_window(query, case_sensitive, direction, cursor, previous, limit)
    }

    pub fn word_motion_target(
        &self,
        row: shepr_vt::AbsRow,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.terminal.word_motion_target(row, col, motion)
    }

    pub fn terminal_dimensions(&self) -> Option<(u16, u16)> {
        self.terminal.dimensions()
    }

    pub fn paragraph_motion_target(
        &self,
        row: shepr_vt::AbsRow,
        direction: i8,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.terminal.paragraph_motion_target(row, direction)
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.terminal.bracketed_paste_enabled()
    }

    pub fn focus_reporting_enabled(&self) -> bool {
        self.terminal.focus_reporting_enabled()
    }

    pub fn mouse_reporting_enabled(&self) -> bool {
        self.terminal.mouse_reporting_enabled()
    }

    pub fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.terminal.sgr_pixel_mouse_enabled()
    }

    pub fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.terminal.plain_page_keys_use_host_scrollback()
    }

    pub fn alternate_screen_active(&self) -> bool {
        self.terminal.alternate_screen_active()
    }

    pub fn cursor_state(&self, area: Rect, show_cursor: bool) -> Option<TerminalCursorState> {
        if !show_cursor {
            return None;
        }
        let cursor = self.terminal.cursor_state()?;
        if cursor.x >= area.width || cursor.y >= area.height {
            return None;
        }
        Some(TerminalCursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    /// Returns the synchronized-output flag and generation together. `None`
    /// means the terminal core is poisoned, so render callers must defer the
    /// frame instead of treating an invented state as a successful read.
    pub fn synchronized_output_state(&self) -> Option<(bool, u64)> {
        self.terminal.synchronized_output_state()
    }

    pub fn detection_text(&self) -> String {
        self.terminal.detection_text()
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.terminal.terminal_title()
    }

    /// The screen text, OSC title and OSC progress the detector evaluates,
    /// read together under one terminal lock like the live detection tick.
    /// Unchanged seeded history rows are excluded from the screen text.
    pub fn agent_detection_inputs(&self) -> super::AgentDetectionInputs {
        self.terminal.agent_detection_inputs()
    }

    /// The pane's primary-screen history, read now with nothing cached.
    pub fn snapshot_history(&self) -> Option<String> {
        self.terminal.primary_history_ansi()
    }

    /// A handle that reads this pane's history from any thread, so a save
    /// can take it on the event loop and format the history off it.
    pub fn history_source(&self) -> super::PaneHistorySource {
        super::PaneHistorySource(Arc::clone(&self.terminal))
    }

    pub fn extract_selection<P>(
        &self,
        selection: &shepr_vt::selection::Selection<P>,
    ) -> Option<String> {
        self.terminal.extract_selection(selection)
    }

    /// Draws the visible screen into `area` of a wire frame; see
    /// [`PaneTerminal::render_into`].
    pub fn render_into(&self, frame: &mut shepr_protocol::FrameData, area: Rect) {
        self.terminal.render_into(frame, area);
    }

    pub fn collect_dirty_patch_snapshot(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> Option<TerminalDirtyPatchSnapshot> {
        // Patch, revision and metadata are read in one terminal-core hold.
        self.terminal
            .collect_dirty_patch_snapshot(area_width, area_height)
    }

    pub fn keyboard_protocol(&self) -> shepr_termio::input::KeyboardProtocol {
        // Legacy only when the terminal core is unreadable (a poisoned lock).
        self.terminal
            .keyboard_protocol(shepr_termio::input::KeyboardProtocol::Legacy)
    }

    pub fn modify_other_keys_level(&self) -> u8 {
        self.terminal.modify_other_keys_level()
    }

    pub fn encode_terminal_key(&self, key: shepr_termio::input::TerminalKey) -> Vec<u8> {
        self.terminal
            .encode_terminal_key(key, self.keyboard_protocol())
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), shepr_pty::ChildIoSendError> {
        self.io.try_write_user_input(bytes)
    }

    pub fn try_send_paste(&self, text: String) -> Result<(), shepr_pty::ChildIoSendError> {
        self.try_send_bytes(self.paste_payload(text))
    }

    fn paste_payload(&self, text: String) -> Bytes {
        let bracketed = self.bracketed_paste_enabled();
        let payload = if bracketed {
            // Clipboard controls must not change how a child interprets the
            // bracketed wrapper. Preserve ordinary pasted whitespace only.
            let safe: String = text
                .replace("\x1b[201~", "")
                .replace("\x1b[200~", "")
                .chars()
                .filter(|ch| !ch.is_control() || matches!(*ch, '\t' | '\r' | '\n'))
                .collect();
            format!("\x1b[200~{safe}\x1b[201~")
        } else {
            text
        };
        Bytes::from(payload)
    }

    pub fn try_send_focus_event(&self, event: shepr_vt::FocusEvent) {
        if !self.focus_reporting_enabled() {
            return;
        }

        let bytes = shepr_vt::encode_focus(event);
        if let Err(err) = self.try_send_bytes(Bytes::from_static(bytes)) {
            warn!(error = %err, ?event, "failed to forward pane focus event");
        }
    }

    pub fn wheel_routing(&self) -> Option<WheelRouting> {
        self.terminal.wheel_routing()
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if !self.mouse_reporting_enabled() {
            return None;
        }
        self.terminal.encode_mouse_button(kind, position, modifiers)
    }

    pub fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal.encode_mouse_motion(kind, position, modifiers)
    }

    pub fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing()? != WheelRouting::MouseReport {
            return None;
        }
        self.terminal.encode_mouse_wheel(kind, position, modifiers)
    }

    pub fn pixel_size(&self) -> Option<(u32, u32)> {
        self.current_size
            .get()
            .text_area_px()
            .map(|(width, height)| (u32::from(width), u32::from(height)))
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing()? != WheelRouting::AlternateScroll {
            return None;
        }
        let key = match kind {
            crossterm::event::MouseEventKind::ScrollUp => crossterm::event::KeyCode::Up,
            crossterm::event::MouseEventKind::ScrollDown => crossterm::event::KeyCode::Down,
            _ => return None,
        };
        Some(
            self.encode_terminal_key(shepr_termio::input::TerminalKey::new(
                key,
                crossterm::event::KeyModifiers::empty(),
            )),
        )
    }

    /// Get the current working directory of the child shell process.
    ///
    /// The latest OSC 7 report wins while the shell's usable /proc cwd is
    /// unchanged since that report arrived; once the shell has moved without
    /// reporting, its usable /proc cwd wins. One /proc read per call.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let shell_cwd = self.child_liveness.live_pid().and_then(|pid| {
            let cwd = super::process_probe::usable_process_cwd(pid);
            (self.child_liveness.live_pid() == Some(pid))
                .then_some(cwd)
                .flatten()
        });
        ReportedCwd::resolve(
            shepr_vt::lock_auxiliary(&self.reported_cwd).as_ref(),
            shell_cwd,
        )
    }

    /// The cwd a save can use without a /proc read: the last one a save
    /// observed, else the shell's latest OSC 7 report. A save's capture takes
    /// this on the event loop and lets [`PaneCwdProbe::read`] improve on it
    /// where the save runs.
    pub fn remembered_cwd(&self) -> Option<std::path::PathBuf> {
        shepr_vt::lock_auxiliary(&self.persistence_cwd)
            .clone()
            .or_else(|| {
                shepr_vt::lock_auxiliary(&self.reported_cwd)
                    .as_ref()
                    .map(|reported| reported.path.clone())
            })
    }

    /// What another thread needs to read this shell's live cwd (see
    /// [`PaneCwdProbe`]); taking it reads nothing.
    pub fn cwd_probe(&self) -> PaneCwdProbe {
        PaneCwdProbe {
            child_liveness: Arc::clone(&self.child_liveness),
            remembered: Arc::clone(&self.persistence_cwd),
        }
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child_liveness.live_pid()
    }

    /// The cwd to inherit when a split or new workspace follows this pane.
    /// The shell's OSC 7 arbitration applies while its own process group is in
    /// the foreground; a foreground job's group leader takes precedence while
    /// a different group owns the terminal.
    pub fn follow_cwd(&self) -> Option<std::path::PathBuf> {
        let shell_pid = self.child_liveness.live_pid();
        let foreground_pgid = shell_pid.and_then(|pid| {
            let foreground_pgid = shepr_agent::detect::foreground_process_group_id(pid);
            (self.child_liveness.live_pid() == Some(pid))
                .then_some(foreground_pgid)
                .flatten()
        });
        let cwd = follow_cwd_from_processes(
            shell_pid,
            foreground_pgid,
            || self.cwd(),
            usable_process_cwd,
        );
        if shell_pid.is_some_and(|pid| self.child_liveness.live_pid() != Some(pid)) {
            None
        } else {
            cwd
        }
    }

    /// Get the current working directory of the process group controlling the pane PTY.
    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.child_liveness.live_pid()?;
        let foreground_pgid = shepr_agent::detect::foreground_process_group_id(pid);
        if self.child_liveness.live_pid() != Some(pid) {
            return None;
        }
        let leader_cwd = foreground_pgid.and_then(absolute_process_cwd);

        // The group leader's cwd is authoritative: a helper
        // process that chdirs elsewhere inside the same foreground group
        // must not override it. Scan other members only when the leader's
        // cwd cannot be read at all.
        let cwd = leader_cwd.or_else(|| {
            let shell_cwd = absolute_process_cwd(pid);
            foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref())
        });
        (self.child_liveness.live_pid() == Some(pid))
            .then_some(cwd)
            .flatten()
    }
}

impl Drop for PaneRuntime {
    fn drop(&mut self) {
        // Abort detection immediately, then stop PTY IO before tearing down
        // the child session. Test runtimes have no child process to tear down.
        if let Some(handle) = &self.detect_handle {
            handle.abort();
        }
        let owns_child_process = self.io.owns_child_process();
        self.io.shutdown();
        if owns_child_process {
            super::teardown::shutdown_pane_processes(
                self.pane_id,
                Arc::clone(&self.child_liveness),
                &self.teardown_tracker,
            );
        }
    }
}

#[cfg(test)]
use shepr_agent::detect::AgentState;

#[cfg(test)]
impl PaneRuntime {
    pub fn agent_detection_reset_notify_for_test(&self) -> Arc<Notify> {
        Arc::clone(&self.detect_reset_notify)
    }

    pub fn current_size(&self) -> (u16, u16) {
        let grid = self.grid_size();
        (grid.rows.get(), grid.cols.get())
    }

    pub fn visible_text(&self) -> String {
        self.terminal.visible_text()
    }

    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.terminal.recent_unwrapped_text(lines)
    }
}

/// This crate's own unit tests build runtimes through the same seam other
/// crates' tests use (`with_child_io` and the fixture channel).
#[cfg(test)]
impl PaneRuntime {
    pub fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, 0, &[], 4)
    }

    pub fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::test_with_scrollback_bytes(cols, rows, 0, bytes)
    }

    pub fn test_process_pty_bytes(&self, bytes: &[u8]) {
        self.output_writer().begin().write(bytes);
    }

    pub fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, scrollback_limit_bytes, bytes, 4).0
    }

    pub fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (io, rx) = shepr_test_fixtures::ChannelChildIo::new(channel_capacity);
        (
            Self::with_child_io(
                cols,
                rows,
                scrollback_limit_bytes,
                bytes,
                Box::new(io),
                Arc::new(Notify::new()),
            ),
            rx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_agent::detect::Agent;
    use shepr_pty::PtyCommand;
    use shepr_test_support::fixture::{self, Held, Signal, Step};

    fn shell_probe(
        previous_agent: Option<Agent>,
        identified_agent: Option<Agent>,
        foreground_is_pane_shell: bool,
        process_exit_reported: bool,
    ) -> ForegroundShellProbe {
        ForegroundShellProbe {
            previous_agent,
            identified_agent,
            foreground_is_pane_shell,
            process_exit_reported,
        }
    }

    #[tokio::test]
    async fn output_writer_holds_the_core_without_announcing_an_unwritten_mutation() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(20, 4);
        let writer = runtime.output_writer();
        let before = runtime.content_seq();
        let write = writer.begin();
        assert!(writer.try_begin().is_none());
        drop(write);
        assert_eq!(runtime.content_seq(), before);
        writer.try_begin().expect("unlocked core").write(b"hello");
        assert!(runtime.content_seq() > before);
        assert!(runtime.visible_text().contains("hello"));
    }

    #[tokio::test]
    async fn scroll_and_host_theme_mutations_advance_the_snapshot_revision() {
        let (runtime, _rx) =
            PaneRuntime::test_with_channel_and_scrollback_bytes(20, 4, 100_000, &[], 4);
        runtime.test_process_pty_bytes(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let before = runtime.content_seq();
        runtime.scroll_up(1);
        let snapshot = runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("snapshot");
        assert!(snapshot.content_revision > before);
        assert_eq!(snapshot.content_revision, runtime.content_seq());
        assert_eq!(snapshot.scroll_metrics, runtime.scroll_metrics());
        let before_theme = snapshot.content_revision;
        runtime
            .terminal
            .apply_host_terminal_theme(shepr_termio::host_term::theme::TerminalTheme::default());
        assert!(runtime.content_seq() > before_theme);
        let before_appearance = runtime.content_seq();
        let _ = runtime.terminal.apply_host_terminal_appearance(None);
        assert!(runtime.content_seq() > before_appearance);
    }

    #[tokio::test]
    async fn clear_pane_preserves_wrapped_input_and_unfinished_vt_sequence() {
        let (runtime, mut rx) = PaneRuntime::test_with_channel_and_scrollback_bytes(
            10,
            5,
            100_000,
            b"old\r\nold\r\nold\r\nold\r\nold\r\n\x1b[32m$ abcdefghijklmnop\x1b[1A\x1b[4G\x1b[",
            4,
        );
        let before = runtime.content_seq();
        runtime.scroll_up(1);
        runtime.clear_screen().expect("test precondition");
        let snapshot = runtime
            .collect_dirty_patch_snapshot(10, 5)
            .expect("test precondition");
        assert!(snapshot.content_revision > before);
        assert!(!matches!(snapshot.patch, TerminalDirtyPatchOutcome::Clean));
        let metrics = runtime.scroll_metrics().expect("test precondition");
        assert_eq!(metrics.max_offset_from_bottom, 0);
        assert_eq!(metrics.offset_from_bottom, 0);
        let text = runtime.recent_unwrapped_text(100);
        assert!(text.contains("$ abcdefghijklmnop"), "{text:?}");
        assert!(!text.contains("old"), "{text:?}");
        runtime.test_process_pty_bytes(b"5 q");
        assert!(!runtime.visible_text().contains("5 q"));
        assert!(rx.try_recv().is_err(), "clear must not send child input");
    }

    #[tokio::test]
    async fn clear_pane_preserves_alternate_screen_and_primary_history() {
        let runtime = PaneRuntime::test_with_scrollback_bytes(
            20,
            4,
            100_000,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\x1b[?1049halt app",
        );
        let before = runtime.visible_text();
        let content_seq = runtime.content_seq();
        let detection_content_seq = shepr_vt::lock_terminal_core(&runtime.terminal.core)
            .expect("core")
            .detection_content_seq;
        assert_eq!(
            runtime.clear_screen(),
            Err(PaneClearError::AlternateScreenActive)
        );
        assert_eq!(runtime.visible_text(), before);
        assert_eq!(runtime.content_seq(), content_seq);
        assert_eq!(
            shepr_vt::lock_terminal_core(&runtime.terminal.core)
                .expect("core")
                .detection_content_seq,
            detection_content_seq
        );
        runtime.test_process_pty_bytes(b"\x1b[?1049l");
        assert!(runtime.recent_unwrapped_text(100).contains("one"));
        runtime.clear_screen().expect("test precondition");
        assert!(!runtime.recent_unwrapped_text(100).contains("one"));
        assert!(runtime.visible_text().contains("five"));
    }

    #[tokio::test]
    async fn dirty_patch_snapshot_keeps_clean_metadata_and_terminal_fallback() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(20, 4);
        runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("initial snapshot");
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1016h");
        let snapshot = runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("mode snapshot");
        assert!(matches!(snapshot.patch, TerminalDirtyPatchOutcome::Clean));
        assert_eq!(snapshot.content_revision, runtime.content_seq());
        assert!(snapshot.content_revision.is_multiple_of(2));
        assert!(snapshot.mouse_reporting);
        assert!(snapshot.sgr_pixel_mouse);
        assert!(!snapshot.alternate_screen_active);

        runtime.test_process_pty_bytes(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        assert!(runtime.collect_dirty_patch_snapshot(20, 4).is_none());
        assert!(runtime.terminal.core.try_lock().is_ok());
    }

    #[tokio::test]
    async fn dirty_patch_snapshot_tracks_serialized_scroll_and_resize() {
        let runtime = PaneRuntime::test_with_scrollback_bytes(
            20,
            4,
            100_000,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix",
        );
        runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("live snapshot");
        runtime.scroll_up(1);
        let scrolled = runtime
            .collect_dirty_patch_snapshot(20, 4)
            .expect("scrolled snapshot");
        assert_eq!(
            scrolled.scroll_metrics.expect("metrics").offset_from_bottom,
            1
        );
        runtime.scroll_reset();
        runtime.resize(shepr_core::geometry::PaneGeometry::new(24, 5, 0, 0));
        let resized = runtime
            .collect_dirty_patch_snapshot(24, 5)
            .expect("resized snapshot");
        let metrics = resized.scroll_metrics.expect("resized metrics");
        assert_eq!(metrics.offset_from_bottom, 0);
        assert_eq!(metrics.viewport_rows, 5);
        assert!(resized.content_revision.is_multiple_of(2));
        let TerminalDirtyPatchOutcome::Patch(patch) = resized.patch else {
            panic!("resize must dirty the viewport");
        };
        assert_eq!(patch.rows.len(), 5);
        assert!(patch.rows.iter().all(|(_, cells)| cells.len() == 24));
    }

    #[tokio::test]
    async fn cwd_returns_accepted_report_without_rechecking_filesystem() {
        let cwd = crate::test_support::ScratchDir::new("reported-cwd").to_path_buf();

        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let (events, _event_rx) = mpsc::channel(1);
        publish_reported_cwd(
            runtime.pane_id,
            &runtime.child_liveness,
            cwd.clone(),
            &runtime.reported_cwd,
            &events.clone().into(),
        );
        assert_eq!(
            reported_path(&runtime),
            Some(cwd.clone()),
            "test setup must pass cache admission"
        );

        std::fs::remove_dir(&cwd).expect("remove reported cwd after admission");

        assert_eq!(runtime.cwd(), Some(cwd));
    }

    #[tokio::test]
    async fn dropped_cwd_report_is_resent_on_the_next_identical_report() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let (events, mut event_rx) = mpsc::channel(1);
        let scratch = crate::test_support::ScratchDir::new("cwd-report");
        let cwd = scratch.to_path_buf();
        let other = std::path::PathBuf::from("/");
        // Fill the channel so the first cwd report cannot be queued.
        events
            .try_send(AppEvent::TerminalCwdReported {
                pane_id: runtime.pane_id,
                cwd: UsableCwd::new(other).expect("root is usable"),
            })
            .expect("test precondition");

        publish_reported_cwd(
            runtime.pane_id,
            &runtime.child_liveness,
            cwd.clone(),
            &runtime.reported_cwd,
            &events.clone().into(),
        );
        assert!(
            shepr_vt::lock_auxiliary(&runtime.reported_cwd).is_none(),
            "an unsent report must not occupy the dedupe slot"
        );

        let _ = event_rx.recv().await.expect("drain filler event");
        publish_reported_cwd(
            runtime.pane_id,
            &runtime.child_liveness,
            cwd.clone(),
            &runtime.reported_cwd,
            &events.clone().into(),
        );
        let Ok(AppEvent::TerminalCwdReported { cwd: sent, .. }) = event_rx.try_recv() else {
            panic!("expected the retried cwd report");
        };
        assert_eq!(sent.as_path(), cwd);
        assert_eq!(reported_path(&runtime), Some(cwd));
    }

    fn reported_path(runtime: &PaneRuntime) -> Option<std::path::PathBuf> {
        shepr_vt::lock_auxiliary(&runtime.reported_cwd)
            .as_ref()
            .map(|reported| reported.path.clone())
    }

    #[test]
    fn reported_cwd_wins_until_the_shell_moves_without_reporting() {
        let report = |path: &str, shell: Option<&str>| ReportedCwd {
            path: path.into(),
            shell_cwd_at_report: shell.map(Into::into),
        };
        let shell = |path: &str| Some(std::path::PathBuf::from(path));

        // A nested or root shell reported its directory; the pane shell has
        // not moved since, so the report is the newest information.
        let nested = report("/srv/nested", Some("/home/u"));
        assert_eq!(
            ReportedCwd::resolve(Some(&nested), shell("/home/u")),
            shell("/srv/nested")
        );
        // The shell changed directory without a new report: the report is stale.
        assert_eq!(
            ReportedCwd::resolve(Some(&nested), shell("/tmp")),
            shell("/tmp")
        );
        // An unreadable /proc link keeps the report as the only evidence.
        assert_eq!(
            ReportedCwd::resolve(Some(&nested), None),
            shell("/srv/nested")
        );
        // A report taken while /proc was unreadable cannot vouch for itself.
        let unsampled = report("/srv/nested", None);
        assert_eq!(
            ReportedCwd::resolve(Some(&unsampled), shell("/home/u")),
            shell("/home/u")
        );
        assert_eq!(
            ReportedCwd::resolve(None, shell("/home/u")),
            shell("/home/u")
        );
        assert_eq!(ReportedCwd::resolve(None, None), None);
    }

    #[tokio::test]
    async fn a_repeated_report_refreshes_its_shell_sample_without_a_new_event() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let (events, mut event_rx) = mpsc::channel(4);
        let scratch = crate::test_support::ScratchDir::new("cwd-repeat");
        let cwd = scratch.to_path_buf();
        *shepr_vt::lock_auxiliary(&runtime.reported_cwd) = Some(ReportedCwd {
            path: cwd.clone(),
            shell_cwd_at_report: Some("/stale".into()),
        });

        // The test runtime has no shell, so the fresh sample is unreadable.
        publish_reported_cwd(
            runtime.pane_id,
            &runtime.child_liveness,
            cwd.clone(),
            &runtime.reported_cwd,
            &events.clone().into(),
        );

        assert!(event_rx.try_recv().is_err(), "a repeat is not a new event");
        assert_eq!(
            shepr_vt::lock_auxiliary(&runtime.reported_cwd).clone(),
            Some(ReportedCwd {
                path: cwd,
                shell_cwd_at_report: None,
            })
        );
    }

    #[test]
    fn process_cwd_does_not_require_traversing_the_directory_path() {
        use std::os::unix::fs::PermissionsExt;

        let base = crate::test_support::ScratchDir::new("process-cwd");
        let private = base.join("private");
        let cwd = private.join("cwd");
        std::fs::create_dir_all(&cwd).expect("create process cwd");

        let mut child = fixture::command(&[Step::Sleep(std::time::Duration::from_secs(30))])
            .current_dir(&cwd)
            .spawn()
            .expect("spawn process in cwd");
        let expected_cwd = shepr_agent::detect::process_cwd(child.id())
            .expect("resolve process cwd before restricting traversal");
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o000))
            .expect("make cwd path untraversable");

        // The probe thread cannot bypass the mode-000 directory, whoever runs
        // the test, so the stat below is always refused.
        let pid = child.id();
        let probe_cwd = cwd.clone();
        let probe = std::thread::spawn(move || {
            shepr_test_support::drop_dac_capabilities_on_this_thread();
            (
                std::fs::metadata(&probe_cwd).map(|_| ()),
                absolute_process_cwd(pid),
            )
        })
        .join();

        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755))
            .expect("restore cwd path permissions");
        child.kill().expect("kill the sleeping cwd fixture");
        child.wait().expect("reap the cwd fixture");

        let (stat, observed) = probe.expect("the cwd probe thread completes");
        assert_eq!(
            stat.expect_err("the cwd path must be untraversable for the probe")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(observed, Some(expected_cwd));
    }

    #[tokio::test]
    async fn follow_cwd_falls_back_to_reported_pane_cwd_without_foreground_group() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("follow-cwd");
        let cwd = scratch.to_path_buf();
        *shepr_vt::lock_auxiliary(&runtime.reported_cwd) = Some(ReportedCwd {
            path: cwd.clone(),
            shell_cwd_at_report: None,
        });

        assert_eq!(runtime.follow_cwd(), Some(cwd));
    }

    #[test]
    fn follow_cwd_uses_osc_report_when_the_shell_owns_the_foreground_group() {
        let shell_cwd = std::path::PathBuf::from("/home/user/project");
        let reported_path = std::path::PathBuf::from("/work/project");
        let reported = ReportedCwd {
            path: reported_path.clone(),
            shell_cwd_at_report: Some(shell_cwd.clone()),
        };
        let read_foreground_group = Cell::new(false);

        let cwd = follow_cwd_from_processes(
            Some(42),
            Some(42),
            || ReportedCwd::resolve(Some(&reported), Some(shell_cwd)),
            |_| {
                read_foreground_group.set(true);
                None
            },
        );

        assert_eq!(cwd, Some(reported_path));
        assert!(!read_foreground_group.get());
    }

    #[tokio::test]
    async fn bracketed_paste_neutralizes_embedded_markers() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        let payload = runtime.paste_payload("before\x1b[201~middle\x1b[200~after".into());
        assert_eq!(payload.as_ref(), b"\x1b[200~beforemiddleafter\x1b[201~");

        // Removing the inner start marker reassembles an end marker; the control
        // filter drops its ESC, so the payload still ends the paste exactly once.
        let nested = runtime.paste_payload("\x1b[20\x1b[200~1~\nrm -rf ~\n".into());
        assert_eq!(nested.as_ref(), b"\x1b[200~[201~\nrm -rf ~\n\x1b[201~");

        // Tabs and line endings are pasted as they are; other C0 and C1 controls
        // and DEL are dropped.
        let whitespace = runtime.paste_payload("a\tb\r\nc\x07\x7fd\u{9b}e\r".into());
        assert_eq!(whitespace.as_ref(), b"\x1b[200~a\tb\r\ncde\r\x1b[201~");

        // Without bracketed paste the text goes through untouched.
        runtime.test_process_pty_bytes(b"\x1b[?2004l");
        let raw = runtime.paste_payload("a\x1b[201~\x07b\r".into());
        assert_eq!(raw.as_ref(), b"a\x1b[201~\x07b\r");
    }

    #[tokio::test]
    async fn alternate_screen_does_not_replace_primary_saved_history() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"primary history");
        assert!(
            runtime
                .snapshot_history()
                .is_some_and(|text| text.contains("primary history"))
        );
        runtime.test_process_pty_bytes(b"\x1b[?1049halt frame");
        assert_eq!(runtime.snapshot_history(), None);
    }

    #[test]
    fn pane_teardown_reaches_background_jobs_after_the_leader_is_reaped() {
        // The common close path: the pane's child has exited and been reaped,
        // but it left a job behind in its session that ignores SIGHUP and
        // SIGTERM, as a daemonised dev server might.
        let scratch = crate::test_support::ScratchDir::new("pane-teardown-fixture");
        let program = fixture::stand_in(
            scratch.path(),
            "shepr-fixture",
            &[
                Step::Ignore(Signal::Hup),
                Step::Ignore(Signal::Term),
                Step::Spawn {
                    argv0: "dev-server".into(),
                    sleep: std::time::Duration::from_secs(30),
                    held: Held::All,
                },
                Step::Exit(0),
            ],
        );
        let cmd =
            PtyCommand::interactive_shell(program.to_str().expect("fixture path is UTF-8"), false);
        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
        )
        .expect("spawn session");
        let leader_pid = spawned.child.id();
        let leader = shepr_platform::ProcessHandle::open(leader_pid).expect("leader pidfd");
        let child_liveness = Arc::new(ChildLiveness::new(leader_pid, Some(leader)));
        spawned.child.wait().expect("reap the leader");
        assert!(child_liveness.has_exited());
        assert!(child_liveness.is_reaped());
        child_liveness.mark_wait_completed();

        let members = shepr_platform::session_member_handles(leader_pid, || true);
        assert_eq!(members.len(), 1, "the background job survives its leader");

        let tracker = Arc::new(PaneTeardownTracker::default());
        let started = std::time::Instant::now();
        shutdown_pane_processes(
            shepr_test_fixtures::fixed_pane_id(1),
            child_liveness,
            &tracker,
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "teardown must not block its caller through the grace periods"
        );

        assert!(tracker.wait(std::time::Duration::from_secs(10)));
        let handles: Vec<&shepr_platform::ProcessHandle> = members.iter().collect();
        assert!(
            shepr_platform::wait_for_process_exits(&handles, std::time::Duration::from_secs(1)),
            "the background job is killed once SIGHUP and SIGTERM are ignored"
        );
    }

    #[test]
    fn sync_timeout_render_arms_one_task_for_many_reads() {
        let timer = SyncTimeoutRender::default();
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(150);

        assert_eq!(
            timer.arm(deadline),
            Some(deadline),
            "first read arms a task"
        );
        for _ in 0..100 {
            assert_eq!(timer.arm(deadline), None, "later reads reuse the task");
        }
        // A later update's deadline extends the armed task instead.
        let later = deadline + std::time::Duration::from_millis(150);
        assert_eq!(timer.arm(later), None);
        assert_eq!(timer.next_wake(deadline), Some(later));
        assert_eq!(timer.next_wake(later), None, "the task disarms and renders");
        assert_eq!(timer.arm(later), Some(later), "a disarmed timer re-arms");
    }

    /// A child that opens a synchronized update and then goes quiet still gets
    /// its frame shown: the read arms the timeout task, which ticks the
    /// terminal and requests the render with no further PTY bytes.
    #[tokio::test]
    async fn an_update_left_open_by_a_quiet_child_is_flushed_by_the_timeout_task() {
        let pane_id = shepr_test_fixtures::fixed_pane_id(7);
        let terminal = Arc::new(PaneTerminal::new(shepr_vt::Terminal::new(20, 5, 0)));
        let (events, _events_rx) = mpsc::channel(8);
        let effects = Arc::new(PaneReadEffects {
            pane_id,
            terminal: Arc::clone(&terminal),
            render_notify: Arc::new(Notify::new()),
            render_dirty: Arc::new(RenderSignal::new()),
            reported_cwd: Arc::new(Mutex::new(None)),
            events: events.into(),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            sync_timeout_render: SyncTimeoutRender::default(),
            deferred_effect_order: Arc::default(),
            timer_writer: std::sync::OnceLock::new(),
            timer_reply_drop_reported: AtomicBool::new(false),
            rt: tokio::runtime::Handle::current(),
        });

        let begin = terminal.process_pty_bytes(pane_id, b"\x1b[?2026hframe");
        let delay = begin.render_delay.expect("the update is open");
        assert!(terminal.synchronized_output_active());
        let notified = effects.render_notify.notified();
        effects.arm_sync_timeout(delay);

        tokio::time::timeout(std::time::Duration::from_secs(10), notified)
            .await
            .expect("the timeout task requests a render");
        assert!(!terminal.synchronized_output_active());
        assert_eq!(
            shepr_vt::lock_terminal_core(&effects.terminal.core)
                .expect("core")
                .content_revision,
            4
        );
        assert_eq!(
            shepr_vt::lock_terminal_core(&effects.terminal.core)
                .expect("core")
                .detection_content_seq,
            2
        );
        assert!(effects.render_dirty.is_pending());
    }

    /// Returns once `count` tickets are parked in `apply`. The count is
    /// raised under the order lock before waiting, so seeing it here means
    /// the ticket has released the lock into its condvar wait.
    fn wait_until_parked(order: &DeferredEffectOrder, count: usize) {
        // A ticket that never parks is a failure, not a hang.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while shepr_vt::lock_auxiliary(&order.state).waiting < count {
            assert!(
                std::time::Instant::now() < deadline,
                "{count} ticket(s) never parked in apply"
            );
            std::thread::yield_now();
        }
    }

    /// A later write's deferred effect waits for an earlier one reserved
    /// before it, even when the later one is applied first.
    #[test]
    fn deferred_effects_apply_in_reservation_order() {
        let order = Arc::new(DeferredEffectOrder::default());
        let first = order.reserve();
        let second = order.reserve();
        let applied = Arc::new(Mutex::new(Vec::new()));

        let later = {
            let applied = Arc::clone(&applied);
            std::thread::spawn(move || {
                second.apply(|| applied.lock().expect("test lock").push(2));
            })
        };
        wait_until_parked(&order, 1);
        assert!(
            applied.lock().expect("test lock").is_empty(),
            "the second effect ran before the first"
        );
        first.apply(|| applied.lock().expect("test lock").push(1));
        later.join().expect("second effect thread");

        assert_eq!(*applied.lock().expect("test lock"), [1, 2]);
    }

    /// A ticket dropped without running (a panic or early return between
    /// reservation and application) never blocks the effects after it,
    /// whether it is dropped before or after they wait.
    #[test]
    fn a_dropped_deferred_ticket_does_not_block_later_effects() {
        let order = Arc::new(DeferredEffectOrder::default());
        let lost = order.reserve();
        let next = order.reserve();
        drop(lost);
        let ran = Cell::new(false);
        next.apply(|| ran.set(true));
        assert!(ran.get());

        let lost = order.reserve();
        let waiting = order.reserve();
        let after = order.reserve();
        let waiter = std::thread::spawn(move || waiting.apply(|| {}));
        wait_until_parked(&order, 1);
        drop(lost);
        waiter.join().expect("waiting effect thread");
        let ran = Cell::new(false);
        after.apply(|| ran.set(true));
        assert!(ran.get());
    }

    /// Dropping a later ticket out of turn is recorded and skipped once the
    /// earlier ones finish.
    #[test]
    fn a_ticket_dropped_out_of_turn_is_skipped_later() {
        let order = Arc::new(DeferredEffectOrder::default());
        let first = order.reserve();
        let skipped = order.reserve();
        let third = order.reserve();
        drop(skipped);
        first.apply(|| {});
        let ran = Cell::new(false);
        third.apply(|| ran.set(true));
        assert!(ran.get());
        let state = shepr_vt::lock_auxiliary(&order.state);
        assert_eq!(state.next_to_apply, 3);
        assert!(state.finished_early.is_empty());
    }

    #[test]
    fn pane_teardown_without_a_session_does_nothing() {
        let tracker = Arc::new(PaneTeardownTracker::default());
        shutdown_pane_processes(
            shepr_test_fixtures::fixed_pane_id(1),
            Arc::new(ChildLiveness::new(0, None)),
            &tracker,
        );
        assert!(tracker.wait(std::time::Duration::ZERO));
    }

    /// The `TERM` and `COLORTERM` a pane child sees, one per line.
    fn capture_terminal_identity(extra_env: &[(&str, &str)]) -> String {
        let scratch = crate::test_support::ScratchDir::new("pane-term");
        let output_path = scratch.join("output.txt");
        let process = fixture::stand_in(
            scratch.path(),
            "shepr-fixture",
            &[
                Step::To(output_path.clone()),
                Step::PrintEnv("TERM".into()),
                Step::PrintEnv("COLORTERM".into()),
            ],
        );
        let mut cmd =
            PtyCommand::interactive_shell(process.to_str().expect("fixture path is UTF-8"), false);
        cmd.cwd(scratch.path());
        cmd.env("TERM", "xterm-ghostty");
        cmd.env("COLORTERM", "falsecolor");
        apply_pane_terminal_env(&mut cmd);
        let extra_env = extra_env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        apply_pane_launch_env(
            &mut cmd,
            &PaneLaunchEnv::from_extra(extra_env, "/run/user/1000/shepr-test.sock".into()),
        );

        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
        )
        .expect("spawn in pty");
        let status = spawned.child.wait().expect("wait for the fixture");
        assert!(status.success(), "the fixture failed: {status:?}");

        std::fs::read_to_string(&output_path).expect("test precondition")
    }

    #[test]
    fn login_shell_builder_uses_one_resolved_path_for_exec_and_shell_env() {
        let scratch = crate::test_support::ScratchDir::new("pane-login-shell");
        let shell = fixture::stand_in(
            scratch.path(),
            "shepr-login-shell",
            &[Step::Sleep(std::time::Duration::from_secs(30))],
        );
        let shell = shell.to_str().expect("scratch shell path is UTF-8");
        let mut cmd = pane_shell_command_builder(PaneShellConfig::new(shell, true));
        cmd.cwd(scratch.path());

        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
        )
        .expect("spawn test shell");
        let command_line = std::fs::read(format!("/proc/{}/cmdline", spawned.child.id()));
        let shell_env = std::fs::read(format!("/proc/{}/environ", spawned.child.id()));
        spawned
            .child
            .kill()
            .expect("stop the sleeping shell fixture");
        spawned.child.wait().expect("reap the shell fixture");
        let command_line = command_line.expect("read the fixture command line");
        let argv0_end = command_line
            .iter()
            .position(|byte| *byte == 0)
            .expect("command line has an argv0 terminator");
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            std::ffi::OsStr::from_bytes(&command_line[..argv0_end]),
            std::ffi::OsStr::new("-shepr-login-shell")
        );
        assert!(command_line[argv0_end + 1..].is_empty());
        assert_eq!(
            child_shell_environment(&shell_env.expect("read fixture environment")),
            shell.as_bytes()
        );
    }

    #[test]
    fn non_login_shell_builder_execs_configured_shell_without_login_argv0() {
        let scratch = crate::test_support::ScratchDir::new("pane-non-login-shell");
        let shell = fixture::stand_in(
            scratch.path(),
            "fake-shell",
            &[Step::Sleep(std::time::Duration::from_secs(30))],
        );
        let shell = shell.to_str().expect("scratch shell path is UTF-8");
        let cmd = pane_shell_command_builder(PaneShellConfig::new(shell, false));
        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
        )
        .expect("spawn test shell");
        let command_line = std::fs::read(format!("/proc/{}/cmdline", spawned.child.id()));
        let shell_env = std::fs::read(format!("/proc/{}/environ", spawned.child.id()));
        spawned
            .child
            .kill()
            .expect("stop the sleeping shell fixture");
        spawned.child.wait().expect("reap the shell fixture");
        let command_line = command_line.expect("read the fixture command line");
        let argv0_end = command_line
            .iter()
            .position(|byte| *byte == 0)
            .expect("command line has an argv0 terminator");
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            std::ffi::OsStr::from_bytes(&command_line[..argv0_end]),
            std::ffi::OsStr::new(shell)
        );
        assert!(command_line[argv0_end + 1..].is_empty());
        assert_eq!(
            child_shell_environment(&shell_env.expect("read fixture environment")),
            shell.as_bytes()
        );
    }

    #[test]
    fn pane_shell_spawn_rejects_a_missing_configured_shell() {
        let cmd =
            pane_shell_command_builder(PaneShellConfig::new("/__shepr_missing_shell__", true));
        let err = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
        )
        .err()
        .expect("test precondition");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn pane_shell_spawn_resolves_a_bare_name_on_the_child_path() {
        let bin = crate::test_support::ScratchDir::new("bin");
        let shell = fixture::stand_in(
            bin.path(),
            "fake-shell",
            &[Step::Sleep(std::time::Duration::from_secs(30))],
        );

        let mut cmd = pane_shell_command_builder(PaneShellConfig::new("fake-shell", false));
        cmd.env("PATH", bin.as_os_str());
        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
        )
        .expect("spawn executable selected through PATH");
        let executable = std::fs::read_link(format!("/proc/{}/exe", spawned.child.id()));
        let shell_env = std::fs::read(format!("/proc/{}/environ", spawned.child.id()));
        spawned
            .child
            .kill()
            .expect("stop the sleeping shell fixture");
        spawned.child.wait().expect("reap the shell fixture");
        assert_eq!(executable.expect("read resolved executable"), shell);
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            child_shell_environment(&shell_env.expect("read fixture environment")),
            shell.as_os_str().as_bytes()
        );
    }

    fn child_shell_environment(environment: &[u8]) -> &[u8] {
        environment
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(b"SHELL="))
            .expect("child environment contains SHELL")
    }

    #[test]
    fn pane_terminal_identity_overrides_outer_terminal_env() {
        let output = capture_terminal_identity(&[]);
        assert_eq!(
            output,
            format!("{}\n{}\n", shepr_vt::PANE_TERM, shepr_vt::PANE_COLORTERM)
        );
    }

    #[test]
    fn pane_terminal_identity_allows_explicit_override() {
        let output = capture_terminal_identity(&[("TERM", "vt100"), ("COLORTERM", "24bit")]);
        assert_eq!(output, "vt100\n24bit\n");
    }

    #[tokio::test]
    async fn exited_shell_keeps_persistence_cwd_when_pid_is_reused() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("exited-cwd");
        let saved = scratch.join("saved");
        *shepr_vt::lock_auxiliary(&runtime.persistence_cwd) = Some(saved.clone());
        // A different live process now owns the exited shell's numeric PID.
        runtime.child_liveness.set_pid_for_test(std::process::id());
        runtime.child_liveness.mark_wait_completed();
        assert_eq!(runtime.cwd_probe().read(), None);
        assert_eq!(runtime.remembered_cwd(), Some(saved));
        *shepr_vt::lock_auxiliary(&runtime.persistence_cwd) = None;
        assert_eq!(runtime.remembered_cwd(), None);
    }

    #[tokio::test]
    async fn deleted_process_cwd_does_not_replace_remembered_or_reported_cwd() {
        struct ChildGuard(std::process::Child);

        impl Drop for ChildGuard {
            fn drop(&mut self) {
                self.0.kill().ok();
                self.0.wait().ok();
            }
        }

        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("deleted-process-cwd");
        let remembered = scratch.join("remembered");
        let reported = scratch.join("reported");
        let deleted = scratch.join("deleted");
        std::fs::create_dir(&remembered).expect("create remembered cwd");
        std::fs::create_dir(&reported).expect("create reported cwd");
        std::fs::create_dir(&deleted).expect("create process cwd");

        let child = ChildGuard(
            fixture::command(&[Step::Sleep(std::time::Duration::from_secs(30))])
                .current_dir(&deleted)
                .spawn()
                .expect("spawn process in cwd"),
        );
        let pid = child.0.id();
        assert_eq!(
            shepr_agent::detect::process_cwd(pid),
            Some(deleted.clone()),
            "test precondition: process starts in the selected cwd"
        );
        runtime.child_liveness.set_pid_for_test(pid);
        std::fs::remove_dir(&deleted).expect("unlink process cwd");
        let deleted_link = shepr_agent::detect::process_cwd(pid).expect("read unlinked cwd");
        assert!(deleted_link.to_string_lossy().ends_with(" (deleted)"));

        *shepr_vt::lock_auxiliary(&runtime.persistence_cwd) = Some(remembered.clone());
        *shepr_vt::lock_auxiliary(&runtime.reported_cwd) = Some(ReportedCwd {
            path: reported.clone(),
            shell_cwd_at_report: None,
        });

        assert_eq!(runtime.cwd_probe().read(), None);
        assert_eq!(runtime.remembered_cwd(), Some(remembered));
        assert_eq!(runtime.cwd(), Some(reported));
    }

    #[tokio::test]
    async fn scrollback_survives_shrink_and_grow_resize() {
        let suffix = "x".repeat(66);
        let history = (1..=2_000)
            .map(|line| format!("{line:05} {suffix}\r\n"))
            .collect::<String>();
        let runtime =
            PaneRuntime::test_with_scrollback_bytes(80, 45, 20_000_000, history.as_bytes());

        runtime.resize(shepr_core::geometry::PaneGeometry::new(80, 21, 0, 0));
        let snapshot = runtime.recent_unwrapped_text(usize::MAX);
        assert!(snapshot.contains("00001 "));
        assert!(snapshot.contains("02000 "));

        runtime.resize(shepr_core::geometry::PaneGeometry::new(80, 45, 0, 0));

        assert_eq!(runtime.current_size(), (45, 80));
        assert_eq!(runtime.terminal_dimensions(), Some((80, 45)));
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("test precondition")
                .viewport_rows,
            45
        );
        let snapshot = runtime.recent_unwrapped_text(usize::MAX);
        assert!(snapshot.contains("00001 "));
        assert!(snapshot.contains("02000 "));
    }

    #[tokio::test]
    async fn focus_events_are_forwarded_when_enabled() {
        let (io, mut rx) = shepr_test_fixtures::ChannelChildIo::new(4);
        let mut terminal = shepr_vt::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[?1004h");
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        let terminal = Arc::new(PaneTerminal::new(terminal));
        let runtime = PaneRuntime {
            generation: crate::events::RuntimeGeneration::alloc(),
            persistence_cwd: Arc::new(Mutex::new(None)),
            pane_id,
            terminal,
            io: Box::new(io),
            current_size: Cell::new(shepr_core::geometry::PaneGeometry::new(24, 80, 0, 0)),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            teardown_tracker: Arc::default(),
            reported_cwd: Arc::new(Mutex::new(None)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            detect_handle: Some(tokio::spawn(async {}).abort_handle()),
        };

        runtime.try_send_focus_event(shepr_vt::FocusEvent::Gained);
        assert_eq!(
            rx.recv().await.expect("test precondition"),
            Bytes::from_static(b"\x1b[I")
        );
    }

    #[tokio::test]
    async fn focus_events_are_suppressed_when_disabled() {
        let (io, mut rx) = shepr_test_fixtures::ChannelChildIo::new(4);
        let terminal = shepr_vt::Terminal::new(80, 24, 0);
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        let terminal = Arc::new(PaneTerminal::new(terminal));
        let runtime = PaneRuntime {
            generation: crate::events::RuntimeGeneration::alloc(),
            persistence_cwd: Arc::new(Mutex::new(None)),
            pane_id,
            terminal,
            io: Box::new(io),
            current_size: Cell::new(shepr_core::geometry::PaneGeometry::new(24, 80, 0, 0)),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            teardown_tracker: Arc::default(),
            reported_cwd: Arc::new(Mutex::new(None)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            detect_handle: Some(tokio::spawn(async {}).abort_handle()),
        };

        runtime.try_send_focus_event(shepr_vt::FocusEvent::Gained);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn subscribed_idle_child_receives_color_scheme_transition() {
        let (runtime, mut rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.apply_host_terminal_appearance(Some(
            shepr_termio::host_term::theme::HostAppearance::Dark,
        ));
        runtime.test_process_pty_bytes(b"\x1b[?2031h");

        runtime.apply_host_terminal_appearance(Some(
            shepr_termio::host_term::theme::HostAppearance::Light,
        ));

        assert_eq!(rx.recv().await, Some(Bytes::from_static(b"\x1b[?997;2n")));
    }

    #[test]
    fn foreground_shell_reports_process_exit_before_clearing_agent() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Codex), None, true, false)),
            ForegroundShellAgentAction::ReportProcessExit
        );
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Codex), None, true, true)),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn same_agent_after_reported_exit_is_a_replacement_process() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(
                Some(Agent::Pi),
                Some(Agent::Pi),
                false,
                true,
            )),
            ForegroundShellAgentAction::ReportReplacementProcess
        );
    }

    #[test]
    fn unknown_non_shell_foreground_job_is_not_immediate_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Claude), None, false, false,)),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    #[tokio::test]
    async fn first_agent_acquisition_keeps_osc_evidence_replacement_clears_it() {
        let runtime = PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]2;startup title\x1b\\\x1b]9;4;1;\x1b\\");

        clear_osc_evidence_for_agent_transition(&runtime.terminal, None);
        let inputs = runtime.agent_detection_inputs();
        assert_eq!(inputs.osc_title, "startup title");
        assert_eq!(inputs.osc_progress, "4;1;");

        clear_osc_evidence_for_agent_transition(&runtime.terminal, Some(Agent::Claude));
        let inputs = runtime.agent_detection_inputs();
        assert_eq!(inputs.osc_title, "");
        assert_eq!(inputs.osc_progress, "");
    }

    #[test]
    fn reported_process_exit_clears_before_unknown_foreground_probe() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(Some(Agent::Claude), None, false, true,)),
            ForegroundShellAgentAction::ClearAgent
        );
    }

    #[test]
    fn foreground_agent_job_is_not_clear_signal() {
        assert_eq!(
            foreground_shell_agent_action(shell_probe(
                Some(Agent::Claude),
                Some(Agent::OpenCode),
                true,
                false,
            )),
            ForegroundShellAgentAction::ObserveProbe
        );
    }

    fn foreground_process(pid: u32, name: &str) -> shepr_agent::detect::ForegroundProcess {
        shepr_agent::detect::ForegroundProcess {
            pid,
            name: name.to_string(),
            argv: None,
            cmdline: None,
        }
    }

    #[test]
    fn identifiable_foreground_leader_wins_over_other_job_members() {
        let job = shepr_agent::detect::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "codex"),
                foreground_process(100, "claude"),
            ],
        };

        let result = probe_foreground_process_from_jobs(42, Some(99), None, || Some(job));

        assert_eq!(result.agent(), Some(Agent::Codex));
        assert_eq!(result.process_name(), Some("codex"));
    }

    #[test]
    fn unidentified_leader_job_falls_through_to_foreground_job() {
        let leader_job = shepr_agent::detect::ForegroundJob {
            process_group_id: 99,
            processes: vec![foreground_process(99, "some_vm")],
        };
        let foreground_job = shepr_agent::detect::ForegroundJob {
            process_group_id: 99,
            processes: vec![
                foreground_process(99, "some_vm"),
                foreground_process(100, "codex"),
            ],
        };

        let result = probe_foreground_process_from_jobs(42, Some(99), Some(&leader_job), || {
            Some(foreground_job)
        });

        assert_eq!(result.agent(), Some(Agent::Codex));
        assert_eq!(result.process_name(), Some("codex"));
    }

    #[test]
    fn transient_process_miss_keeps_current_agent_detected() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        let changed = presence.observe_process_probe(None);

        assert!(!changed, "one miss should not clear the detected agent");
        assert_eq!(presence.current_agent(), Some(Agent::Pi));
    }

    #[test]
    fn agent_only_clears_after_confirmation_misses() {
        let mut presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));

        for attempt in 1..AGENT_MISS_CONFIRMATION_ATTEMPTS {
            let changed = presence.observe_process_probe(None);
            assert!(
                !changed,
                "miss {attempt} should stay in the confirmation window"
            );
            assert_eq!(presence.current_agent(), Some(Agent::Pi));
        }

        let changed = presence.observe_process_probe(None);
        assert!(changed, "last confirmation miss should clear the agent");
        assert_eq!(presence.current_agent(), None);
    }

    #[tokio::test]
    async fn set_full_lifecycle_authority_active_notifies_only_on_activation_transitions() {
        let runtime = PaneRuntime::test_with_screen_bytes(80, 24, b"");
        let reset_notify = runtime.agent_detection_reset_notify_for_test();

        runtime.set_full_lifecycle_authority_active(true);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("false-to-true transition should notify detection reset");

        runtime.set_full_lifecycle_authority_active(true);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reset_notify.notified()
            )
            .await
            .is_err(),
            "repeated true-to-true sync should not notify detection reset"
        );

        runtime.set_full_lifecycle_authority_active(false);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                reset_notify.notified()
            )
            .await
            .is_err(),
            "true-to-false transition should not notify detection reset"
        );

        runtime.set_full_lifecycle_authority_active(true);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reset_notify.notified(),
        )
        .await
        .expect("re-entering active authority should notify detection reset");
    }

    #[tokio::test]
    async fn state_changed_event_waits_for_queue_space_instead_of_dropping() {
        let (tx, mut rx) = mpsc::channel(1);
        let pane_id = shepr_test_fixtures::fixed_pane_id(42);

        tx.try_send(AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        })
        .expect("test precondition");

        let publish = publish_state_changed_event(
            tx.clone(),
            pane_id,
            StateChangedUpdate {
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                process_exited: false,
                observed_at: std::time::Instant::now(),
            },
        );
        tokio::pin!(publish);

        let blocked = tokio::time::timeout(std::time::Duration::from_millis(20), async {
            (&mut publish).await;
        })
        .await;
        assert!(
            blocked.is_err(),
            "publisher should wait for queue space instead of dropping StateChanged"
        );

        let first = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("queue should yield first event")
            .expect("sender still alive");
        assert!(matches!(first, AppEvent::GitStatusRefreshed { .. }));

        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            (&mut publish).await;
        })
        .await
        .expect("publisher should complete once queue space is available");

        let second = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("queue should yield second event")
            .expect("sender still alive");
        assert!(matches!(
            second,
            AppEvent::StateChanged {
                pane_id: delivered_pane,
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                process_exited: false,
                observed_at: _,
            } if delivered_pane == pane_id
        ));
    }
}
