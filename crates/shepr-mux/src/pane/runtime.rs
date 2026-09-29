use std::cell::Cell;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use bytes::Bytes;
use ratatui::{Frame, layout::Rect};
use tokio::sync::{Notify, mpsc};
use tracing::{error, info, warn};

use super::PaneClearError;
use super::agent_detection::{
    DetectionPublishDecision, detection_update_for_publish_with_osc,
    mark_detection_content_changed, observe_detection_content_change,
};
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
    /// The shell's absolute /proc cwd right now, or `None` when the shell has
    /// been reaped (its numeric PID may belong to another process by now) or
    /// the read failed. A successful read is remembered for the pane's later
    /// saves. Persistence observations must not change OSC authority or
    /// follow-cwd behavior, so nothing else is touched.
    pub fn read(&self) -> Option<std::path::PathBuf> {
        if self.child_liveness.wait_completed() {
            return None;
        }
        let cwd = shepr_agent::detect::process_cwd(self.child_liveness.pid())
            .filter(|cwd| cwd.is_absolute())?;
        *shepr_vt::lock_auxiliary(&self.remembered) = Some(cwd.clone());
        Some(cwd)
    }
}

/// PTY runtime for a pane. Owns the terminal, I/O channels, and background tasks.
/// Dropping this aborts async tasks and closes the PTY.
pub struct PaneRuntime {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    io: Box<dyn ChildIo>,
    current_size: Cell<shepr_core::geometry::PaneGeometry>,
    child_liveness: Arc<ChildLiveness>,
    teardown_tracker: Arc<super::teardown::PaneTeardownTracker>,
    reported_cwd: Arc<Mutex<Option<ReportedCwd>>>,
    persistence_cwd: Arc<Mutex<Option<std::path::PathBuf>>>,
    content_seq: Arc<AtomicU64>,
    content_write_lock: Arc<Mutex<()>>,
    detection_content_seq: Arc<AtomicU64>,
    full_lifecycle_authority_active: Arc<AtomicBool>,
    detect_reset_notify: Arc<Notify>,
    // Task handles for deterministic shutdown
    detect_handle: Option<tokio::task::AbortHandle>,
}

/// Hand a once-only terminal-reply closure to a [`ChildIo`], whose methods
/// take `FnMut` to stay object-safe.
fn write_terminal_response(io: &dyn ChildIo, response: impl FnOnce() -> Option<Bytes>) {
    let mut response = Some(response);
    io.write_terminal_response(&mut || response.take().and_then(|response| response()));
}

/// Writes the child's output into the pane terminal. Each write is announced
/// through the content revision, odd while it is in progress and even once it
/// has landed, under the content write lock that a render also holds while it
/// pairs a snapshot with its revision (`collect_dirty_patch_snapshot`). The
/// PTY reader writes through one; so does anything else that feeds a pane its
/// child's output.
#[derive(Clone)]
pub struct PaneOutputWriter {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    content_seq: Arc<AtomicU64>,
    content_write_lock: Arc<Mutex<()>>,
}

/// A write that holds the content write lock and has announced itself.
pub struct PaneOutputWrite<'a> {
    writer: &'a PaneOutputWriter,
    _guard: ContentWriteGuard<'a>,
}

/// Keep the content revision odd for the whole terminal mutation, including
/// unwinding, and release it before unlocking the writer mutex.
struct ContentWriteGuard<'a> {
    seq: &'a AtomicU64,
    _lock: std::sync::MutexGuard<'a, ()>,
}

impl<'a> ContentWriteGuard<'a> {
    fn new(seq: &'a AtomicU64, lock: &'a Mutex<()>) -> Self {
        let guard = shepr_vt::lock_auxiliary(lock);
        Self::from_lock(seq, guard)
    }

    fn try_new(seq: &'a AtomicU64, lock: &'a Mutex<()>) -> Option<Self> {
        shepr_vt::try_lock_auxiliary(lock).map(|guard| Self::from_lock(seq, guard))
    }

    fn from_lock(seq: &'a AtomicU64, guard: std::sync::MutexGuard<'a, ()>) -> Self {
        seq.fetch_add(1, Ordering::AcqRel);
        Self { seq, _lock: guard }
    }
}

impl Drop for ContentWriteGuard<'_> {
    fn drop(&mut self) {
        self.seq.fetch_add(1, Ordering::Release);
    }
}

impl PaneOutputWriter {
    /// Wait for the content write lock, then announce the write.
    pub fn begin(&self) -> PaneOutputWrite<'_> {
        PaneOutputWrite {
            writer: self,
            _guard: ContentWriteGuard::new(&self.content_seq, &self.content_write_lock),
        }
    }

    /// Announce the write only if no render or other write holds the content
    /// write lock.
    pub fn try_begin(&self) -> Option<PaneOutputWrite<'_>> {
        Some(PaneOutputWrite {
            writer: self,
            _guard: ContentWriteGuard::try_new(&self.content_seq, &self.content_write_lock)?,
        })
    }
}

impl PaneOutputWrite<'_> {
    /// Process `bytes` as the child's output and land the write.
    pub fn write(self, bytes: &[u8]) {
        let _ = self.process(bytes, std::time::Instant::now());
    }

    fn process(self, bytes: &[u8], now: std::time::Instant) -> ProcessBytesResult {
        self.writer
            .terminal
            .process_pty_bytes_at(self.writer.pane_id, bytes, now)
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

fn publish_reported_cwd(
    pane_id: PaneId,
    shell_pid: u32,
    cwd: std::path::PathBuf,
    reported_cwd: &Arc<Mutex<Option<ReportedCwd>>>,
    events: &mpsc::Sender<AppEvent>,
) {
    let Some(cwd) = UsableCwd::new(cwd) else {
        return;
    };
    // One readlink per OSC 7, sampled before taking the lock.
    let shell_cwd_at_report = shepr_agent::detect::process_cwd(shell_pid);
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
    events: mpsc::Sender<AppEvent>,
    content_write_lock: Arc<Mutex<()>>,
    content_seq: Arc<AtomicU64>,
    detection_content_seq: Arc<AtomicU64>,
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
/// with no terminal, content or reply-order lock held.
struct DeferredEffects {
    ticket: DeferredEffectTicket,
    shell_pid: u32,
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

impl PaneReadEffects {
    /// Applies the effects that never block (render and title requests,
    /// clipboard writes) and returns the ones that may, if any. A read with
    /// nothing to defer, the common case, allocates nothing for them.
    /// `ticket` is the write's place in the deferred-effect order, reserved
    /// under the reply-order lock exactly when `has_deferred_effects` held.
    fn apply_immediate(
        &self,
        shell_pid: u32,
        result: ProcessBytesResult,
        ticket: Option<DeferredEffectTicket>,
    ) -> Option<DeferredEffects> {
        let pane_id = self.pane_id;
        let title_requested =
            result.terminal_title_changed && self.render_dirty.request_terminal_title(pane_id);
        let render_requested = result.request_render && self.render_dirty.request_pty(pane_id);
        if title_requested || render_requested {
            self.render_notify.notify_one();
        }
        for content in result.clipboard_writes {
            if let Err(err) = self.events.try_send(AppEvent::ClipboardWrite { content }) {
                warn!(
                    pane = pane_id.raw(),
                    error = %err,
                    "failed to send OSC 52 clipboard write"
                );
            }
        }
        ticket.map(|ticket| DeferredEffects {
            ticket,
            shell_pid,
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
        deferred.ticket.apply(|| {
            if let Some(generation) = deferred.default_color_generation {
                self.terminal.resolve_default_color_owner(
                    self.pane_id,
                    deferred.shell_pid,
                    generation,
                );
            }
            if let Some(cwd) = deferred.reported_cwd {
                publish_reported_cwd(
                    self.pane_id,
                    deferred.shell_pid,
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
        let effects = Arc::clone(self);
        self.rt.spawn(async move {
            let mut wake_at = first_wake;
            loop {
                tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)).await;
                match effects.sync_timeout_render.next_wake(wake_at) {
                    Some(later) => wake_at = later,
                    None => break,
                }
            }
            // The terminal and content locks are synchronous. Keep their wait
            // off a Tokio worker when a timer fires.
            tokio::task::spawn_blocking(move || effects.flush_expired_synchronized_output());
        });
    }

    /// The timer's half of the runtime tick: flush an expired update, queue
    /// its replies at one point in the reply order (taken before the content
    /// and core locks, as the reader does), then apply its effects with no
    /// lock held.
    fn flush_expired_synchronized_output(&self) {
        let mut tick_result = None;
        let mut deferred_ticket = None;
        let mut tick = || {
            let content_write_guard =
                ContentWriteGuard::new(&self.content_seq, &self.content_write_lock);
            let mut result = self.terminal.tick(std::time::Instant::now());
            drop(content_write_guard);
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
        if result.request_render {
            // A timer has no PTY input bytes to count; only a flushed frame
            // advances detection's screen-content revision here.
            self.detection_content_seq.fetch_add(1, Ordering::AcqRel);
        }
        let shell_pid = self.child_liveness.pid();
        if let Some(deferred) = self.apply_immediate(shell_pid, result, deferred_ticket) {
            self.apply_deferred(deferred);
        }
    }
}

impl PaneRuntime {
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
        rows: u16,
        cols: u16,
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
            rows,
            cols,
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
        rows: u16,
        cols: u16,
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
        let size = shepr_core::geometry::GridSize::clamped_pane(cols, rows);
        let rows = size.rows.get();
        let cols = size.cols.get();
        crate::logging::pane_spawn_started(pane_id.raw(), rows, cols, scrollback_limit_bytes);

        let terminal = shepr_vt::Terminal::new(cols, rows, scrollback_limit_bytes);
        let pane_terminal = PaneTerminal::new_with_pane_id(pane_id, terminal);
        pane_terminal.apply_host_terminal_theme(host_terminal_theme);
        let _ = pane_terminal.apply_host_terminal_appearance(host_terminal_appearance);
        if let Some(ansi) = initial_history_ansi {
            pane_terminal.seed_history_ansi(ansi);
        }
        let terminal = Arc::new(pane_terminal);
        let content_write_lock = Arc::new(Mutex::new(()));

        let spawned = shepr_pty::backend::spawn_pty(rows, cols, &cmd).inspect_err(
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
        let reported_cwd = Arc::new(Mutex::new(None));
        let content_seq = Arc::new(AtomicU64::new(0));
        let detection_content_seq = Arc::new(AtomicU64::new(0));
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
                content_write_lock: Arc::clone(&content_write_lock),
                content_seq: Arc::clone(&content_seq),
                detection_content_seq: Arc::clone(&detection_content_seq),
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
                content_seq: Arc::clone(&content_seq),
                content_write_lock: Arc::clone(&content_write_lock),
            };
            let on_read = Box::new(move |bytes: &[u8]| {
                let write = output.begin();
                let shell_pid = read_effects.child_liveness.pid();
                // Ticks an expired synchronized update first, then parses; the
                // content write lock is released when this returns.
                let mut result = write.process(bytes, std::time::Instant::now());
                if result.core_poisoned {
                    // The actor ends the loop and reports the pane dead.
                    return PtyReadResult {
                        terminal_responses: Vec::new(),
                        after_response_order: None,
                        core_broken: true,
                    };
                }
                observe_detection_content_change(bytes, &read_effects.detection_content_seq);
                let deferred_ticket = read_effects.reserve_deferred(&result);
                let terminal_responses = std::mem::take(&mut result.terminal_responses);
                if let Some(delay) = result.render_delay {
                    read_effects.arm_sync_timeout(delay);
                }
                let after_response_order: Option<Box<dyn FnOnce() + Send>> = read_effects
                    .apply_immediate(shell_pid, result, deferred_ticket)
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
            let detection_content_seq = Arc::clone(&detection_content_seq);
            let full_lifecycle_authority_active_for_task =
                Arc::clone(&full_lifecycle_authority_active);
            let render_notify = Arc::clone(render_notify);
            let render_dirty = Arc::clone(render_dirty);
            let detect_reset_notify = Arc::new(Notify::new());
            let detect_reset = Arc::clone(&detect_reset_notify);

            let handle = tokio::spawn(async move {
                let mut detector = DetectorState::new(Instant::now(), launch_purpose);

                tokio::time::sleep(crate::limits::INITIAL_DETECTION_DELAY).await;

                loop {
                    let tick =
                        detector.tick_interval(terminal.has_transient_default_color_override());
                    tokio::select! {
                        _ = tokio::time::sleep(tick) => {}
                        _ = detect_reset.notified() => {
                            detector.reset();
                        }
                    }

                    let now = Instant::now();
                    let pid = child_liveness.pid();
                    let lifecycle_authority_active =
                        full_lifecycle_authority_active_for_task.load(Ordering::Acquire);
                    let foreground_pgid = if pid > 0 {
                        match tokio::task::spawn_blocking(move || {
                            shepr_agent::detect::foreground_process_group_id(pid)
                        })
                        .await
                        {
                            Ok(pgid) => pgid,
                            Err(error) => {
                                tracing::warn!(?error, "foreground process group probe failed");
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let probe_schedule = detector.schedule_process_probe(&ProcessProbeRequest {
                        now,
                        observed_foreground_group: foreground_pgid,
                        lifecycle_authority_active,
                    });
                    let process_group_changed = probe_schedule.foreground_group_changed();

                    let mut agent_changed = false;
                    if pid > 0 && probe_schedule.should_probe() {
                        detector.probe_started(now);
                        let probe = match tokio::task::spawn_blocking(move || {
                            probe_foreground_process(pid, foreground_pgid)
                        })
                        .await
                        {
                            Ok(probe) => probe,
                            Err(error) => {
                                tracing::warn!(?error, "foreground process probe failed");
                                continue;
                            }
                        };
                        let process_change = detector.observe_process_probe(
                            &probe,
                            now,
                            foreground_pgid,
                            probe_schedule,
                        );
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
                            agent_changed = true;
                        }
                    }

                    let pid = child_liveness.pid();
                    // The restore check reads /proc only while an override is
                    // active; keep that rare probe off the runtime worker too.
                    if pid > 0 && terminal.has_transient_default_color_override() {
                        let theme_terminal = Arc::clone(&terminal);
                        match tokio::task::spawn_blocking(move || {
                            theme_terminal.maybe_restore_host_terminal_theme(pane_id, pid)
                        })
                        .await
                        {
                            Ok(true) => {
                                if render_dirty.request_pty(pane_id) {
                                    render_notify.notify_one();
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(?error, "host terminal theme probe failed");
                            }
                        }
                    }

                    let agent = detector.current_agent();
                    let process_exited = detector.process_exited(agent);
                    if !detector.may_scan_screen(ScreenScanGate {
                        now,
                        lifecycle_authority_active,
                        process_exited,
                    }) {
                        continue;
                    }

                    let current_detection_content_seq =
                        Some(detection_content_seq.load(Ordering::Relaxed));
                    if !detector.should_read_screen(ScreenReadRequest {
                        agent,
                        agent_changed,
                        process_exited,
                        detection_content_seq: current_detection_content_seq,
                    }) {
                        continue;
                    }

                    // Without an identified agent, detection reports `Unknown`
                    // whatever the screen shows, and the screen would only feed
                    // the content-change signal for process acquisition. The PTY
                    // read counter gives that signal without copying the screen
                    // out of the terminal core, which plain shell panes would
                    // otherwise do on every tick.
                    let identified_agent_text = agent.is_some().then(|| terminal.detection_text());
                    let (content, content_changed) = detector.detection_content(
                        agent,
                        current_detection_content_seq,
                        identified_agent_text,
                    );
                    let (osc_title, osc_progress) = if agent.is_some() {
                        (terminal.agent_osc_title(), terminal.agent_osc_progress())
                    } else {
                        (String::new(), String::new())
                    };
                    let screen_detection = detection_update_for_publish_with_osc(
                        agent,
                        &content,
                        &osc_title,
                        &osc_progress,
                        process_exited,
                    );
                    let Some(screen_detection) = screen_detection else {
                        detector.clear_pending_idle();
                        continue;
                    };
                    detector.note_content_change(now, process_group_changed, content_changed);
                    if detector.withhold_agent_absence(agent, now) {
                        detector.clear_pending_idle();
                        continue;
                    }
                    match detector.screen_publish_decision(
                        screen_detection,
                        ScreenPublishContext {
                            now,
                            process_exited,
                            agent_changed,
                        },
                    ) {
                        DetectionPublishDecision::NoPublish => {}
                        DetectionPublishDecision::Publish {
                            state: new_state,
                            visible_idle,
                            visible_blocker,
                            visible_working,
                            process_exited: publish_process_exited,
                        } => {
                            detector
                                .apply_publish_update(
                                    state_events.clone(),
                                    pane_id,
                                    agent,
                                    AgentDetectionPublishUpdate {
                                        state: new_state,
                                        visible_idle,
                                        visible_blocker,
                                        visible_working,
                                        process_exited: publish_process_exited,
                                    },
                                    now,
                                )
                                .await;
                        }
                    }
                }
            });
            (Some(handle.abort_handle()), detect_reset_notify)
        };

        Ok(Self {
            pane_id,
            terminal,
            io,
            current_size: Cell::new(shepr_core::geometry::PaneGeometry::new(cols, rows, 0, 0)),
            child_liveness,
            teardown_tracker,
            reported_cwd,
            persistence_cwd: Arc::new(Mutex::new(None)),
            content_seq,
            content_write_lock,
            detection_content_seq,
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
        Self {
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
            content_seq: Arc::new(AtomicU64::new(0)),
            content_write_lock: Arc::new(Mutex::new(())),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: detection_reset,
            detect_handle: None,
        }
    }

    /// A writer that feeds this pane its child's output, as the PTY reader
    /// does.
    pub fn output_writer(&self) -> PaneOutputWriter {
        PaneOutputWriter {
            pane_id: self.pane_id,
            terminal: Arc::clone(&self.terminal),
            content_seq: Arc::clone(&self.content_seq),
            content_write_lock: Arc::clone(&self.content_write_lock),
        }
    }

    /// Run `hook` inside the next dirty-patch collection, while it holds the
    /// terminal core and the content write lock.
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
        self.content_seq.load(Ordering::Acquire)
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
            let content_write_guard =
                ContentWriteGuard::new(&self.content_seq, &self.content_write_lock);
            let terminal_responses = self.terminal.resize(size);
            drop(content_write_guard);
            terminal_responses
        });
        mark_detection_content_changed(&self.detection_content_seq);
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
        let guard = ContentWriteGuard::new(&self.content_seq, &self.content_write_lock);
        let result = self.terminal.clear_screen();
        drop(guard);
        mark_detection_content_changed(&self.detection_content_seq);
        result
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

    pub fn agent_osc_title(&self) -> String {
        self.terminal.agent_osc_title()
    }

    pub fn agent_osc_progress(&self) -> String {
        self.terminal.agent_osc_progress()
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

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, show_cursor: bool) {
        self.terminal.render(frame, area, show_cursor);
    }

    pub fn collect_dirty_patch_snapshot(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> Option<TerminalDirtyPatchSnapshot> {
        // PTY/resize writers announce changes before locking the terminal core.
        // Exclude them until rows and metadata have been paired with their revision.
        let _content_guard = shepr_vt::lock_auxiliary(&self.content_write_lock);
        let revision = self.content_seq();
        if !revision.is_multiple_of(2) {
            return None;
        }
        let patch = self.terminal.collect_dirty_patch(area_width, area_height);
        if matches!(patch, TerminalDirtyPatchOutcome::Fallback) {
            return None;
        }
        let snapshot = TerminalDirtyPatchSnapshot {
            patch,
            content_revision: revision,
            scroll_metrics: self.scroll_metrics(),
            mouse_reporting: self.mouse_reporting_enabled(),
            sgr_pixel_mouse: self.sgr_pixel_mouse_enabled(),
            alternate_screen_active: self.alternate_screen_active(),
        };
        (self.content_seq() == revision).then_some(snapshot)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.terminal.visible_hyperlinks(area)
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
            let safe = text.replace("\x1b[201~", "").replace("\x1b[200~", "");
            format!("\x1b[200~{safe}\x1b[201~")
        } else {
            text
        };
        Bytes::from(payload)
    }

    pub fn try_send_focus_event(&self, event: shepr_vt::FocusEvent) -> bool {
        if !self.focus_reporting_enabled() {
            return false;
        }

        let bytes = shepr_vt::encode_focus(event);
        if let Err(err) = self.try_send_bytes(Bytes::from_static(bytes)) {
            warn!(error = %err, ?event, "failed to forward pane focus event");
        }
        true
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
    /// The latest OSC 7 report wins while the shell's /proc cwd is unchanged
    /// since that report arrived; once the shell has moved without reporting,
    /// its /proc cwd wins. One /proc read per call.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let shell_cwd = shepr_agent::detect::process_cwd(self.child_liveness.pid());
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
        let pid = self.child_liveness.pid();
        (pid > 0).then_some(pid)
    }

    pub fn follow_cwd(&self) -> Option<std::path::PathBuf> {
        let leader_cwd = self
            .child_pid()
            .and_then(shepr_agent::detect::foreground_process_group_id)
            .and_then(usable_process_cwd);
        leader_cwd.or_else(|| self.cwd())
    }

    /// Get the current working directory of the process group controlling the pane PTY.
    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.child_liveness.pid();
        let foreground_pgid = shepr_agent::detect::foreground_process_group_id(pid);
        let leader_cwd = foreground_pgid.and_then(absolute_process_cwd);

        // The group leader's cwd is authoritative: a helper
        // process that chdirs elsewhere inside the same foreground group
        // must not override it. Scan other members only when the leader's
        // cwd cannot be read at all.
        leader_cwd.or_else(|| {
            let shell_cwd = absolute_process_cwd(pid);
            foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref())
        })
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

    pub fn recent_unwrapped_text_snapshot(
        &self,
        lines: usize,
    ) -> crate::terminal::TerminalReadSnapshot {
        self.terminal.recent_unwrapped_text_snapshot(lines)
    }

    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.recent_unwrapped_text_snapshot(lines).text
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
        let text = runtime.recent_unwrapped_text_snapshot(100).text;
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
        runtime.clear_screen().expect("test precondition");
        assert_eq!(runtime.visible_text(), before);
        runtime.test_process_pty_bytes(b"\x1b[?1049l");
        assert!(
            runtime
                .recent_unwrapped_text_snapshot(100)
                .text
                .contains("one")
        );
        runtime.clear_screen().expect("test precondition");
        assert!(
            !runtime
                .recent_unwrapped_text_snapshot(100)
                .text
                .contains("one")
        );
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
        assert!(shepr_vt::try_lock_auxiliary(&runtime.content_write_lock).is_some());
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
            runtime.child_liveness.pid(),
            cwd.clone(),
            &runtime.reported_cwd,
            &events,
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

        let shell_pid = runtime.child_liveness.pid();
        publish_reported_cwd(
            runtime.pane_id,
            shell_pid,
            cwd.clone(),
            &runtime.reported_cwd,
            &events,
        );
        assert!(
            shepr_vt::lock_auxiliary(&runtime.reported_cwd).is_none(),
            "an unsent report must not occupy the dedupe slot"
        );

        let _ = event_rx.recv().await.expect("drain filler event");
        publish_reported_cwd(
            runtime.pane_id,
            shell_pid,
            cwd.clone(),
            &runtime.reported_cwd,
            &events,
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
            0,
            cwd.clone(),
            &runtime.reported_cwd,
            &events,
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

    #[tokio::test]
    async fn bracketed_paste_neutralizes_embedded_markers() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        let payload = runtime.paste_payload("before\x1b[201~middle\x1b[200~after".into());
        assert_eq!(payload.as_ref(), b"\x1b[200~beforemiddleafter\x1b[201~");
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
        let mut cmd = PtyCommand::new(fixture::path());
        cmd.args(fixture::args(&[
            Step::Ignore(Signal::Hup),
            Step::Ignore(Signal::Term),
            Step::Spawn {
                argv0: "dev-server".into(),
                sleep: std::time::Duration::from_secs(30),
                held: Held::All,
            },
            Step::Exit(0),
        ]));
        let mut spawned = shepr_pty::backend::spawn_pty(24, 80, &cmd).expect("spawn session");
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
            events,
            content_write_lock: Arc::new(Mutex::new(())),
            content_seq: Arc::new(AtomicU64::new(0)),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
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
        assert_eq!(effects.content_seq.load(Ordering::Acquire), 2);
        assert_eq!(effects.detection_content_seq.load(Ordering::Acquire), 1);
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
        let mut cmd = PtyCommand::new(fixture::path());
        cmd.args(fixture::args(&[
            Step::To(output_path.clone()),
            Step::PrintEnv("TERM".into()),
            Step::PrintEnv("COLORTERM".into()),
        ]));
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

        let mut spawned = shepr_pty::backend::spawn_pty(24, 80, &cmd).expect("spawn in pty");
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
        assert!(cmd.is_login_shell());
        let std_cmd = cmd.to_std_command().expect("test precondition");
        assert_eq!(std_cmd.get_program(), std::ffi::OsStr::new(shell));
        assert_eq!(std_cmd.get_args().count(), 0);
        assert_eq!(
            std_cmd
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new("SHELL"))
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new(shell))
        );

        let mut spawned = shepr_pty::backend::spawn_pty(24, 80, &cmd).expect("spawn test shell");
        let command_line = std::fs::read(format!("/proc/{}/cmdline", spawned.child.id()));
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
    }

    #[test]
    fn non_login_shell_builder_execs_configured_shell_without_login_argv0() {
        let shell = fixture::path_str();
        let cmd = pane_shell_command_builder(PaneShellConfig::new(shell, false));
        assert!(!cmd.is_login_shell());
        let std_cmd = cmd.to_std_command().expect("test precondition");
        assert_eq!(std_cmd.get_program(), std::ffi::OsStr::new(shell));
        assert_eq!(std_cmd.get_args().count(), 0);
    }

    #[test]
    fn pane_shell_spawn_rejects_a_missing_configured_shell() {
        let cmd =
            pane_shell_command_builder(PaneShellConfig::new("/__shepr_missing_shell__", true));
        let err = cmd.to_std_command().expect_err("test precondition");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn pane_shell_spawn_resolves_a_bare_name_on_the_child_path() {
        let bin = crate::test_support::ScratchDir::new("bin");
        let shell = bin.join("fake-shell");
        // Never run: resolution asks access(2) whether it could execute the
        // file, without executing it.
        std::fs::write(&shell, "content").expect("test precondition");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))
                .expect("test precondition");
        }

        let mut cmd = pane_shell_command_builder(PaneShellConfig::new("fake-shell", false));
        cmd.env("PATH", bin.as_os_str());
        let std_cmd = cmd.to_std_command().expect("test precondition");
        assert_eq!(std_cmd.get_program(), shell.as_os_str());
        assert_eq!(
            std_cmd
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new("SHELL"))
                .and_then(|(_, value)| value),
            Some(shell.as_os_str())
        );
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
    async fn scrollback_survives_shrink_and_grow_resize() {
        let suffix = "x".repeat(66);
        let history = (1..=2_000)
            .map(|line| format!("{line:05} {suffix}\r\n"))
            .collect::<String>();
        let runtime =
            PaneRuntime::test_with_scrollback_bytes(80, 45, 20_000_000, history.as_bytes());

        runtime.resize(shepr_core::geometry::PaneGeometry::new(80, 21, 0, 0));
        let snapshot = runtime.recent_unwrapped_text_snapshot(usize::MAX);
        assert!(snapshot.text.contains("00001 "));
        assert!(snapshot.text.contains("02000 "));

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
        let snapshot = runtime.recent_unwrapped_text_snapshot(usize::MAX);
        assert!(snapshot.text.contains("00001 "));
        assert!(snapshot.text.contains("02000 "));
    }

    #[tokio::test]
    async fn focus_events_are_forwarded_when_enabled() {
        let (io, mut rx) = shepr_test_fixtures::ChannelChildIo::new(4);
        let mut terminal = shepr_vt::Terminal::new(80, 24, 0);
        terminal
            .mode_set(shepr_vt::DecMode::FocusEvents, true)
            .expect("test precondition");
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        let terminal = Arc::new(PaneTerminal::new(terminal));
        let runtime = PaneRuntime {
            persistence_cwd: Arc::new(Mutex::new(None)),
            pane_id,
            terminal,
            io: Box::new(io),
            current_size: Cell::new(shepr_core::geometry::PaneGeometry::new(24, 80, 0, 0)),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            teardown_tracker: Arc::default(),
            reported_cwd: Arc::new(Mutex::new(None)),
            content_seq: Arc::new(AtomicU64::new(0)),
            content_write_lock: Arc::new(Mutex::new(())),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            detect_handle: Some(tokio::spawn(async {}).abort_handle()),
        };

        assert!(runtime.try_send_focus_event(shepr_vt::FocusEvent::Gained));
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
            persistence_cwd: Arc::new(Mutex::new(None)),
            pane_id,
            terminal,
            io: Box::new(io),
            current_size: Cell::new(shepr_core::geometry::PaneGeometry::new(24, 80, 0, 0)),
            child_liveness: Arc::new(ChildLiveness::new(0, None)),
            teardown_tracker: Arc::default(),
            reported_cwd: Arc::new(Mutex::new(None)),
            content_seq: Arc::new(AtomicU64::new(0)),
            content_write_lock: Arc::new(Mutex::new(())),
            detection_content_seq: Arc::new(AtomicU64::new(0)),
            full_lifecycle_authority_active: Arc::new(AtomicBool::new(false)),
            detect_reset_notify: Arc::new(Notify::new()),
            detect_handle: Some(tokio::spawn(async {}).abort_handle()),
        };

        assert!(!runtime.try_send_focus_event(shepr_vt::FocusEvent::Gained));
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
        assert_eq!(runtime.agent_osc_title(), "startup title");
        assert_eq!(runtime.agent_osc_progress(), "4;1;");

        clear_osc_evidence_for_agent_transition(&runtime.terminal, Some(Agent::Claude));
        assert_eq!(runtime.agent_osc_title(), "");
        assert_eq!(runtime.agent_osc_progress(), "");
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
            argv0: None,
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
