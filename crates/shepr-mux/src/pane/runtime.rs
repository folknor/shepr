mod cwd;
mod input;
mod read;
mod read_effects;
mod spawn;

pub use cwd::PaneCwdProbe;
use cwd::*;
pub use read::PaneRead;
use read_effects::*;
pub use spawn::{LaunchPresentation, PaneLaunchRequest, PaneLauncher, PaneSpawnHandles};

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use ratatui::layout::Rect;
use tokio::sync::{Notify, mpsc};
use tracing::{error, warn};

use super::PaneClearError;
use super::exit_arbiter::{PaneEnding, PaneExitArbiter};
use super::launch::*;
use super::process_probe::*;
use super::teardown::*;
use super::terminal::{
    DefaultColorGeneration, PaneTerminal, ProcessBytesEffects, ProcessBytesResult, RenderRequest,
    TerminalDirtyPatchSnapshot,
};
use super::*;
use crate::UsableCwd;
use crate::events::AppEvent;
use crate::render_signal::RenderSignal;
use crate::workspace::SurfaceChange;
use shepr_core::layout::PaneId;
use shepr_pty::ChildIo;
use shepr_pty::actor::{
    PtyIoActor, PtyIoActorConfig, PtyIoActorHandle, PtyReadEffects, PtyReadResult, ReaderExit,
};

// ---------------------------------------------------------------------------
// PaneRuntime - PTY, parser, channels, background tasks
// ---------------------------------------------------------------------------

/// PTY runtime for a pane. Owns the terminal and PTY I/O. Dropping it aborts
/// the async detection loop and shuts down PTY I/O. A running blocking detection
/// tick finishes its current operation and stops at its next cancellation check.
/// The child watcher continues until
/// it reaps the child, handing it to a reaper thread if that async watcher is
/// dropped. An armed synchronized-output timer may finish its flush after the
/// runtime is dropped. The timer holds effects weakly while asleep, but its
/// blocking flush is not cancellable once started. A late flush can tick the
/// detached terminal, request a redundant render wake, and queue
/// runtime-generation events; the app rejects those events after the pane
/// runtime is removed or replaced.
// Keep task capabilities separate: a cwd save probe must not retain the
// terminal or actor, and the child watcher must survive runtime teardown.
// Read effects already form the timer's single weakly-held ownership bundle.
pub struct PaneRuntime {
    generation: crate::events::RuntimeGeneration,
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    io: Box<dyn ChildIo>,
    current_size: shepr_core::geometry::PaneGeometry,
    child_liveness: Arc<ChildLiveness>,
    teardown_tracker: Arc<super::teardown::PaneTeardownTracker>,
    /// Shared with the child watcher and the PTY reader; dropping the runtime
    /// decides it first, so a requested teardown publishes no pane death.
    exit_arbiter: Arc<PaneExitArbiter>,
    cwd: Arc<PaneCwdState>,
    full_lifecycle_authority_active: Arc<AtomicBool>,
    detect_reset_notify: Arc<Notify>,
    // Detection is aborted directly. The child watcher must reap, and a
    // synchronized-output timer's already-started blocking flush cannot be
    // cancelled here; it may tick a detached terminal, and its runtime events
    // are rejected by generation after removal.
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
// This parser seam owns only terminal access. Sharing all runtime handles here
// would keep cwd, child and detector state alive for a parser-only writer.
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

    /// Return immediately if the core is busy or poisoned.
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
        self.process(bytes, std::time::Instant::now()).ok();
    }

    fn process(self, bytes: &[u8], now: std::time::Instant) -> ProcessBytesResult {
        let Some(core) = self.core else {
            return Err(shepr_vt::TerminalCorePoisoned);
        };
        self.writer
            .terminal
            .process_pty_bytes_locked(self.writer.pane_id, bytes, now, core)
    }
}

impl PaneRuntime {
    /// Narrow terminal access for drawing, detection snapshots and copy reads.
    /// This handle has no PTY, cwd, child, or input capabilities.
    pub fn read(&self) -> PaneRead<'_> {
        PaneRead {
            terminal: &self.terminal,
        }
    }

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
            current_size: shepr_core::geometry::PaneGeometry::new(cols, rows, 0, 0),
            child_liveness: Arc::new(ChildLiveness::absent()),
            // No child, so no teardown is ever started through this tracker.
            teardown_tracker: Arc::default(),
            exit_arbiter: Arc::default(),
            cwd: Arc::default(),
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

    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        let previous = self
            .full_lifecycle_authority_active
            .swap(active, Ordering::AcqRel);
        if active && !previous {
            self.detect_reset_notify.notify_one();
        }
    }

    /// The full-lifecycle authority the detector task currently reads.
    pub fn full_lifecycle_authority_active(&self) -> bool {
        self.full_lifecycle_authority_active.load(Ordering::Acquire)
    }

    pub fn grid_size(&self) -> shepr_core::geometry::GridSize {
        self.current_size.grid()
    }

    /// A full draw spans multiple core holds. Only unchanged, available reads
    /// certify its cells; retained patches collect everything in one hold.
    pub fn surface_content_revision(before: Option<u64>, after: u64) -> u64 {
        if before == Some(after) && after.is_multiple_of(2) {
            after
        } else {
            after | 1
        }
    }

    /// Resize if the dimensions actually changed.
    pub fn resize(&mut self, size: shepr_core::geometry::PaneGeometry) {
        if self.current_size == size {
            return;
        }
        self.current_size = size;
        self.io.resize(size, &mut || {
            // A PTY read holds the same actor reply-order lock while it
            // parses bytes and queues any replies. Resizing the terminal
            // under that lock keeps its replies in the same order as the
            // terminal state that produced them.
            self.terminal.resize(size)
        });
    }

    /// Scroll up by N lines (into scrollback history).
    pub fn scroll_up(&self, lines: usize) -> SurfaceChange {
        self.terminal.scroll_up(lines)
    }

    /// Scroll down by N lines (toward live output).
    pub fn scroll_down(&self, lines: usize) -> SurfaceChange {
        self.terminal.scroll_down(lines)
    }

    pub fn clear_screen(&self) -> Result<SurfaceChange, PaneClearError> {
        self.terminal.clear_screen()
    }

    /// Reset scroll to live view (offset = 0).
    pub fn scroll_reset(&self) -> SurfaceChange {
        self.terminal.scroll_reset()
    }

    /// Set scrollback offset measured from the live bottom of the terminal.
    pub fn set_scroll_offset_from_bottom(&self, lines: usize) -> SurfaceChange {
        self.terminal.set_scroll_offset_from_bottom(lines)
    }

    /// Takes the screen-flip flag; the caller is about to re-apply geometry.
    pub fn take_screen_flip(&self) -> bool {
        self.terminal.take_screen_flip()
    }

    /// With a process handle, includes zombies before the watcher reaps them
    /// and publishes PaneDied. Without one, only the completed wait is known:
    /// an unreaped exit cannot yet be distinguished from a live shell.
    /// Detection cannot distinguish agent completion from pane interruption
    /// after this point; the watcher owns that decision.
    pub fn child_has_exited(&self) -> bool {
        self.child_liveness.has_exited()
    }

    /// Whether a panic broke the pane's terminal core. The server skips the
    /// exit checkpoint of a pane whose core is broken when it decides, whatever
    /// ended the pane: the watcher can report an ordinary exit before the
    /// reader notices the panic.
    pub fn terminal_core_broken(&self) -> bool {
        self.terminal.core_poisoned()
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child_liveness.live_pid()
    }

    /// Whether the pane's shell exec committed. Before that the pane has a PTY
    /// but no shell: its live screen holds nothing a save should prefer over
    /// history carried for it.
    pub fn launched(&self) -> bool {
        self.child_liveness.is_launched()
    }
}

impl Drop for PaneRuntime {
    fn drop(&mut self) {
        // Abort the async loop; its drop guard cancels a running blocking tick
        // at its next boundary. Stop PTY IO before tearing down the child
        // session. Test runtimes have no child process to tear down.
        if let Some(handle) = &self.detect_handle {
            handle.abort();
        }
        // Decided before anything is torn down, so the exits the teardown
        // causes publish nothing. An observer that already decided keeps its
        // publication, which the generation check then sorts out.
        self.exit_arbiter.decide(PaneEnding::Silent);
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
use spawn::reader_exit_callback;
#[cfg(test)]
use std::cell::Cell;

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

    #[tokio::test]
    async fn output_writer_does_not_keep_runtime_cwd_alive() {
        // Guards the ownership rule on `PaneOutputWriter`: it holds only the
        // terminal, so a field sharing another runtime handle (cwd here) fails
        // this. It says nothing about the PTY reader's `PaneReadEffects`,
        // which does hold cwd for as long as the reader runs; this fixture
        // starts no reader.
        let runtime = PaneRuntime::test_with_screen_bytes(80, 24, b"");
        let cwd = Arc::downgrade(&runtime.cwd);
        let writer = runtime.output_writer();
        drop(runtime);
        assert!(cwd.upgrade().is_none());
        writer.begin().write(b"still usable");
    }

    /// Runs the reader's exit callback and returns the reason it recorded and
    /// whether it was a confirmed exit, `None` when it recorded nothing new.
    fn reader_exit(
        exit: ReaderExit,
        arbiter: &Arc<PaneExitArbiter>,
        grace: std::time::Duration,
    ) -> Option<(shepr_platform::ChildExitReason, bool)> {
        let before = arbiter.ending();
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        reader_exit_callback(pane_id, Arc::clone(arbiter), grace)(exit);
        let after = arbiter.ending();
        match after {
            Some(PaneEnding::Observed {
                reason,
                child_exit_confirmed,
                ..
            }) if after != before => Some((reason, child_exit_confirmed)),
            _ => None,
        }
    }

    fn unconfirmed(
        reason: shepr_platform::ChildExitReason,
    ) -> Option<(shepr_platform::ChildExitReason, bool)> {
        Some((reason, false))
    }

    static REAPED_AT: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);

    fn reaped() -> PaneEnding {
        PaneEnding::Observed {
            reason: shepr_platform::ChildExitReason::Exited,
            child_exit_confirmed: true,
            ended_at: *REAPED_AT,
        }
    }

    #[test]
    fn a_closed_terminal_leaves_the_exit_to_a_watcher_that_reported() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        assert!(arbiter.decide(reaped()), "the watcher decides first");
        let grace = std::time::Duration::from_secs(30);
        assert_eq!(reader_exit(ReaderExit::Closed, &arbiter, grace), None);
        assert_eq!(arbiter.ending(), Some(reaped()));
    }

    #[test]
    fn a_closed_terminal_whose_child_keeps_running_ends_the_pane() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        let grace = std::time::Duration::from_millis(10);
        assert_eq!(
            reader_exit(ReaderExit::Closed, &arbiter, grace),
            unconfirmed(shepr_platform::ChildExitReason::TerminalClosed)
        );
        assert!(
            !arbiter.decide(reaped()),
            "a later watcher report changes nothing"
        );
    }

    #[test]
    fn reader_io_failure_reports_the_pane_for_teardown() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        assert_eq!(
            reader_exit(ReaderExit::IoFailed, &arbiter, std::time::Duration::ZERO),
            unconfirmed(shepr_platform::ChildExitReason::ReaderIoFailed)
        );
    }

    #[test]
    fn a_decided_ending_keeps_later_reader_exits_silent() {
        for exit in [
            ReaderExit::IoFailed,
            ReaderExit::Panicked,
            ReaderExit::ShutdownRequested,
        ] {
            let arbiter = Arc::new(PaneExitArbiter::default());
            arbiter.decide(PaneEnding::Silent);
            assert_eq!(
                reader_exit(exit, &arbiter, std::time::Duration::ZERO),
                None,
                "{exit:?}"
            );
        }
    }

    #[test]
    fn a_requested_shutdown_publishes_no_pane_death() {
        let arbiter = Arc::new(PaneExitArbiter::default());
        assert_eq!(
            reader_exit(
                ReaderExit::ShutdownRequested,
                &arbiter,
                std::time::Duration::ZERO
            ),
            None
        );
        assert_eq!(arbiter.ending(), None);
    }
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

    /// Poisons the core by panicking on another thread while it holds the
    /// mutex; the join reports that panic instead of catching it here.
    fn poison_terminal_core(writer: &PaneOutputWriter) {
        let terminal = std::sync::Arc::clone(&writer.terminal);
        let outcome: std::thread::Result<()> = std::thread::spawn(move || {
            let _core = terminal.core.lock().expect("test core starts unpoisoned");
            panic!("poison core for writer test");
        })
        .join();
        assert!(outcome.is_err(), "the core mutex must be poisoned");
    }

    #[tokio::test]
    async fn output_writer_holds_the_core_without_announcing_an_unwritten_mutation() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(20, 4);
        let writer = runtime.output_writer();
        let before = runtime.read().content_seq();
        let write = writer.begin();
        assert!(writer.try_begin().is_none());
        drop(write);
        assert_eq!(runtime.read().content_seq(), before);
        writer.try_begin().expect("unlocked core").write(b"hello");
        assert!(runtime.read().content_seq() > before);
        assert!(runtime.visible_text().contains("hello"));
    }

    #[tokio::test]
    async fn output_writer_try_begin_returns_none_for_a_poisoned_core() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(20, 4);
        let writer = runtime.output_writer();
        poison_terminal_core(&writer);

        assert!(writer.try_begin().is_none());
    }

    #[tokio::test]
    async fn scroll_and_host_theme_mutations_advance_the_snapshot_revision() {
        let (runtime, _rx) =
            PaneRuntime::test_with_channel_and_scrollback_bytes(20, 4, 100_000, &[], 4);
        runtime.test_process_pty_bytes(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let before = runtime.read().content_seq();
        runtime.scroll_up(1);
        let snapshot = runtime
            .read()
            .collect_dirty_patch_snapshot(20, 4)
            .expect("snapshot");
        assert!(snapshot.content_revision > before);
        assert_eq!(snapshot.content_revision, runtime.read().content_seq());
        assert_eq!(
            Some(snapshot.scroll_metrics),
            runtime.read().scroll_metrics()
        );
        let before_theme = snapshot.content_revision;
        runtime
            .terminal
            .apply_host_terminal_theme(shepr_termio::host_term::theme::TerminalTheme::default());
        assert!(runtime.read().content_seq() > before_theme);
        let before_appearance = runtime.read().content_seq();
        let _ = runtime.terminal.apply_host_terminal_appearance(Some(
            shepr_termio::host_term::theme::HostAppearance::Dark,
        ));
        assert!(runtime.read().content_seq() > before_appearance);
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
        let before = runtime.read().content_seq();
        runtime.scroll_up(1);
        runtime.clear_screen().expect("test precondition");
        let snapshot = runtime
            .read()
            .collect_dirty_patch_snapshot(10, 5)
            .expect("test precondition");
        assert!(snapshot.content_revision > before);
        assert!(snapshot.patch.is_some());
        let metrics = runtime.read().scroll_metrics().expect("test precondition");
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
        let content_seq = runtime.read().content_seq();
        let detection_content_seq = shepr_vt::lock_terminal_core(&runtime.terminal.core)
            .expect("core")
            .detection_content_seq;
        assert_eq!(
            runtime.clear_screen(),
            Err(PaneClearError::AlternateScreenActive)
        );
        assert_eq!(runtime.visible_text(), before);
        assert_eq!(runtime.read().content_seq(), content_seq);
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
            .read()
            .collect_dirty_patch_snapshot(20, 4)
            .expect("initial snapshot");
        runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1016h");
        let snapshot = runtime
            .read()
            .collect_dirty_patch_snapshot(20, 4)
            .expect("mode snapshot");
        assert!(snapshot.patch.is_none());
        assert_eq!(snapshot.content_revision, runtime.read().content_seq());
        assert!(snapshot.content_revision.is_multiple_of(2));
        assert!(snapshot.mouse_reporting);
        assert!(snapshot.sgr_pixel_mouse);
        assert!(!snapshot.alternate_screen_active);

        runtime.test_process_pty_bytes(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        assert!(runtime.read().collect_dirty_patch_snapshot(20, 4).is_none());
        assert!(runtime.terminal.core.try_lock().is_ok());
    }

    #[tokio::test]
    async fn dirty_patch_snapshot_tracks_serialized_scroll_and_resize() {
        let mut runtime = PaneRuntime::test_with_scrollback_bytes(
            20,
            4,
            100_000,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix",
        );
        runtime
            .read()
            .collect_dirty_patch_snapshot(20, 4)
            .expect("live snapshot");
        runtime.scroll_up(1);
        let scrolled = runtime
            .read()
            .collect_dirty_patch_snapshot(20, 4)
            .expect("scrolled snapshot");
        assert_eq!(scrolled.scroll_metrics.offset_from_bottom, 1);
        runtime.scroll_reset();
        runtime.resize(shepr_core::geometry::PaneGeometry::new(24, 5, 0, 0));
        let resized = runtime
            .read()
            .collect_dirty_patch_snapshot(24, 5)
            .expect("resized snapshot");
        let metrics = resized.scroll_metrics;
        assert_eq!(metrics.offset_from_bottom, 0);
        assert_eq!(metrics.viewport_rows, 5);
        assert!(resized.content_revision.is_multiple_of(2));
        let Some(patch) = resized.patch else {
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
            &runtime.cwd.reported,
            &crate::events::EventSender::runtime(events, runtime.pane_id, runtime.generation),
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

        let sender = crate::events::EventSender::runtime(
            events.clone(),
            runtime.pane_id,
            runtime.generation,
        );
        publish_reported_cwd(
            runtime.pane_id,
            &runtime.child_liveness,
            cwd.clone(),
            &runtime.cwd.reported,
            &sender,
        );
        assert!(
            shepr_vt::lock_auxiliary(&runtime.cwd.reported).is_none(),
            "an unsent report must not occupy the dedupe slot"
        );

        let _ = event_rx.recv().await.expect("drain filler event");
        publish_reported_cwd(
            runtime.pane_id,
            &runtime.child_liveness,
            cwd.clone(),
            &runtime.cwd.reported,
            &sender,
        );
        let Ok(AppEvent::Runtime { event, .. }) = event_rx.try_recv() else {
            panic!("expected the retried cwd report");
        };
        let AppEvent::TerminalCwdReported { cwd: sent, .. } = *event else {
            panic!("expected the retried cwd report");
        };
        assert_eq!(sent.as_path(), cwd);
        assert_eq!(reported_path(&runtime), Some(cwd));
    }

    fn reported_path(runtime: &PaneRuntime) -> Option<std::path::PathBuf> {
        shepr_vt::lock_auxiliary(&runtime.cwd.reported)
            .as_ref()
            .map(|reported| reported.path.clone())
    }

    #[test]
    fn reported_cwd_wins_until_the_shell_moves_without_reporting() {
        let report = |path: &str, shell: Option<&str>| ReportedCwd {
            path: path.into(),
            shell_cwd_at_report: shell.map(Into::into),
            generation: 0,
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
        *shepr_vt::lock_auxiliary(&runtime.cwd.reported) = Some(ReportedCwd {
            path: cwd.clone(),
            shell_cwd_at_report: Some("/stale".into()),
            generation: 0,
        });

        // The test runtime has no shell, so the fresh sample is unreadable.
        publish_reported_cwd(
            runtime.pane_id,
            &runtime.child_liveness,
            cwd.clone(),
            &runtime.cwd.reported,
            &crate::events::EventSender::runtime(events, runtime.pane_id, runtime.generation),
        );

        assert!(event_rx.try_recv().is_err(), "a repeat is not a new event");
        assert_eq!(
            shepr_vt::lock_auxiliary(&runtime.cwd.reported).clone(),
            Some(ReportedCwd {
                path: cwd,
                shell_cwd_at_report: None,
                generation: 1,
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
                readlink_process_cwd(pid),
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
        *shepr_vt::lock_auxiliary(&runtime.cwd.reported) = Some(ReportedCwd {
            path: cwd.clone(),
            shell_cwd_at_report: None,
            generation: 0,
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
            generation: 0,
        };
        let read_foreground_group = Cell::new(false);

        let cwd = follow_cwd_from_processes(
            shepr_platform::Pid::new(42),
            shepr_platform::Pgid::new(42),
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
        let source = runtime.read().history_source();
        let mut history = PaneHistoryCache::default();
        runtime.test_process_pty_bytes(b"primary history");
        assert!(source.refresh(&mut history));
        assert!(history.text().contains("primary history"));
        let saved_history = history.text();
        runtime.test_process_pty_bytes(b"\x1b[?1049halt frame");
        assert!(!source.refresh(&mut history));
        assert_eq!(history.text(), saved_history);
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
        let cmd = PtyCommand::interactive_shell(&fixture::resolved_shell(&program), false);
        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
            Box::new(drop),
        )
        .expect("spawn session");
        let leader_pid = spawned.child.id();
        let leader = spawned.child.handle();
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
            cwd: Arc::default(),
            events: crate::events::EventSender::runtime(
                events,
                pane_id,
                crate::events::RuntimeGeneration::alloc(),
            ),
            child_liveness: Arc::new(ChildLiveness::absent()),
            sync_timeout_render: SyncTimeoutRender::default(),
            deferred_effect_order: Arc::default(),
            timer_writer: std::sync::OnceLock::new(),
            timer_reply_drop_reported: AtomicBool::new(false),
            rt: tokio::runtime::Handle::current(),
        });

        let begin = terminal.process_pty_bytes(pane_id, b"\x1b[?2026hframe");
        let RenderRequest::After(delay) = begin.render_request else {
            panic!("the open update has a flush deadline");
        };
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
            Arc::new(ChildLiveness::absent()),
            &tracker,
        );
        assert!(tracker.wait(std::time::Duration::ZERO));
    }

    /// The `TERM` and `COLORTERM` a pane child sees, one per line.
    fn capture_terminal_identity() -> String {
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
        let mut cmd = PtyCommand::interactive_shell(&fixture::resolved_shell(&process), false);
        cmd.cwd(scratch.path());
        cmd.env("TERM", "xterm-ghostty");
        cmd.env("COLORTERM", "falsecolor");
        apply_pane_terminal_env(&mut cmd);
        apply_pane_launch_env(
            &mut cmd,
            &PaneLaunchEnv::new("/run/user/1000/shepr-test.sock".into()),
        );

        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
            Box::new(drop),
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
        let resolved = fixture::resolved_shell(&shell);
        let shell = shell.to_str().expect("scratch shell path is UTF-8");
        let mut cmd = pane_shell_command_builder(
            PaneShellConfig::new(&resolved, true),
            crate::pane::LaunchKind::Fresh,
        );
        cmd.cwd(scratch.path());

        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
            Box::new(drop),
        )
        .expect("spawn test shell");
        let command_line = exec_command_line(spawned.child.id(), b"-shepr-login-shell");
        let shell_env = std::fs::read(format!("/proc/{}/environ", spawned.child.id()));
        spawned
            .child
            .kill()
            .expect("stop the sleeping shell fixture");
        spawned.child.wait().expect("reap the shell fixture");
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
        let resolved = fixture::resolved_shell(&shell);
        let shell = shell.to_str().expect("scratch shell path is UTF-8");
        let cmd = pane_shell_command_builder(
            PaneShellConfig::new(&resolved, false),
            crate::pane::LaunchKind::Fresh,
        );
        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
            Box::new(drop),
        )
        .expect("spawn test shell");
        let command_line = exec_command_line(spawned.child.id(), shell.as_bytes());
        let shell_env = std::fs::read(format!("/proc/{}/environ", spawned.child.id()));
        spawned
            .child
            .kill()
            .expect("stop the sleeping shell fixture");
        spawned.child.wait().expect("reap the shell fixture");
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
    fn a_missing_configured_shell_fails_in_the_child_not_at_the_fork() {
        let shell = fixture::resolved_shell("/__shepr_missing_shell__");
        let cmd = pane_shell_command_builder(
            PaneShellConfig::new(&shell, true),
            crate::pane::LaunchKind::Fresh,
        );
        let mut spawned = shepr_pty::backend::spawn_pty(
            shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0),
            &cmd,
            Box::new(drop),
        )
        .expect("the fork does not wait for exec");
        let status = spawned.child.wait().expect("reap the failed launch");
        assert_eq!(status.code(), Some(127));
    }

    /// The child's command line once it exec'd a program whose argv0 is
    /// `argv0`: the fork returns before the exec, and until then /proc
    /// describes a copy of the test binary.
    fn exec_command_line(pid: u32, argv0: &[u8]) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let command_line =
                std::fs::read(format!("/proc/{pid}/cmdline")).expect("read the child command line");
            if command_line.starts_with(argv0) && command_line.get(argv0.len()) == Some(&0) {
                return command_line;
            }
            assert!(std::time::Instant::now() < deadline, "the child execs");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn child_shell_environment(environment: &[u8]) -> &[u8] {
        environment
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(b"SHELL="))
            .expect("child environment contains SHELL")
    }

    #[test]
    fn pane_terminal_identity_overrides_outer_terminal_env() {
        let output = capture_terminal_identity();
        assert_eq!(
            output,
            format!("{}\n{}\n", shepr_vt::PANE_TERM, shepr_vt::PANE_COLORTERM)
        );
    }

    #[tokio::test]
    async fn exited_shell_keeps_persistence_cwd_when_pid_is_reused() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("exited-cwd");
        let saved = scratch.join("saved");
        *shepr_vt::lock_auxiliary(&runtime.cwd.remembered) = Some(PersistedCwd {
            path: saved.clone(),
            report_generation: None,
        });
        // A different live process now owns the exited shell's numeric PID.
        runtime.child_liveness.set_pid_for_test(std::process::id());
        runtime.child_liveness.mark_wait_completed();
        assert_eq!(runtime.cwd_probe().read(), Some(saved.clone()));
        assert_eq!(runtime.remembered_cwd(), Some(saved));
        *shepr_vt::lock_auxiliary(&runtime.cwd.remembered) = None;
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

        *shepr_vt::lock_auxiliary(&runtime.cwd.remembered) = Some(PersistedCwd {
            path: remembered.clone(),
            report_generation: Some(0),
        });
        *shepr_vt::lock_auxiliary(&runtime.cwd.reported) = Some(ReportedCwd {
            path: reported.clone(),
            shell_cwd_at_report: None,
            generation: 0,
        });

        assert_eq!(runtime.cwd_probe().read(), Some(remembered.clone()));
        assert_eq!(runtime.remembered_cwd(), Some(remembered));
        assert_eq!(runtime.cwd(), Some(reported));
    }

    #[tokio::test]
    async fn save_fallback_prefers_a_report_newer_than_its_last_probe() {
        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("cwd-save-report-order");
        let probed = scratch.join("probed");
        let reported = scratch.join("reported");
        *shepr_vt::lock_auxiliary(&runtime.cwd.remembered) = Some(PersistedCwd {
            path: probed,
            report_generation: Some(3),
        });
        *shepr_vt::lock_auxiliary(&runtime.cwd.reported) = Some(ReportedCwd {
            path: reported.clone(),
            shell_cwd_at_report: Some(scratch.join("shell")),
            generation: 4,
        });

        assert_eq!(runtime.remembered_cwd(), Some(reported.clone()));
        // Checkpoint captures use this probe too. With no live shell, it must
        // preserve the same current fallback as an ordinary save.
        assert_eq!(runtime.cwd_probe().read(), Some(reported));
    }

    #[test]
    fn save_fallback_keeps_a_probe_that_observed_the_shell_after_an_old_report() {
        let reported = ReportedCwd {
            path: "/logical/old".into(),
            shell_cwd_at_report: Some("/physical/old".into()),
            generation: 3,
        };
        let persisted = PersistedCwd {
            path: "/physical/new".into(),
            report_generation: Some(3),
        };

        assert_eq!(
            remembered_cwd_for_save(Some(reported), Some(persisted)),
            Some("/physical/new".into())
        );
    }

    #[tokio::test]
    async fn save_probe_keeps_the_logical_osc_path_through_a_symlink() {
        use std::os::unix::fs::symlink;

        struct ChildGuard(std::process::Child);

        impl Drop for ChildGuard {
            fn drop(&mut self) {
                self.0.kill().ok();
                self.0.wait().ok();
            }
        }

        let (runtime, _rx) = PaneRuntime::test_with_channel(80, 24);
        let scratch = crate::test_support::ScratchDir::new("cwd-save-symlink");
        let physical = scratch.join("physical");
        let logical = scratch.join("logical");
        std::fs::create_dir(&physical).expect("create physical cwd");
        symlink(&physical, &logical).expect("create logical cwd symlink");
        let child = ChildGuard(
            fixture::command(&[Step::Sleep(std::time::Duration::from_secs(30))])
                .current_dir(&physical)
                .spawn()
                .expect("spawn process in cwd"),
        );
        let pid = child.0.id();
        let shell_cwd = shepr_agent::detect::process_cwd(pid).expect("read shell cwd");
        assert_eq!(shell_cwd, physical);
        runtime.child_liveness.set_pid_for_test(pid);
        *shepr_vt::lock_auxiliary(&runtime.cwd.reported) = Some(ReportedCwd {
            path: logical.clone(),
            shell_cwd_at_report: Some(shell_cwd),
            generation: 0,
        });

        assert_eq!(runtime.cwd(), Some(logical.clone()));
        assert_eq!(runtime.cwd_probe().read(), Some(logical.clone()));
        assert_eq!(runtime.remembered_cwd(), Some(logical));
    }

    #[tokio::test]
    async fn scrollback_survives_shrink_and_grow_resize() {
        let suffix = "x".repeat(66);
        let history = (1..=2_000)
            .map(|line| format!("{line:05} {suffix}\r\n"))
            .collect::<String>();
        let mut runtime =
            PaneRuntime::test_with_scrollback_bytes(80, 45, 20_000_000, history.as_bytes());

        runtime.resize(shepr_core::geometry::PaneGeometry::new(80, 21, 0, 0));
        let snapshot = runtime.recent_unwrapped_text(usize::MAX);
        assert!(snapshot.contains("00001 "));
        assert!(snapshot.contains("02000 "));

        runtime.resize(shepr_core::geometry::PaneGeometry::new(80, 45, 0, 0));

        assert_eq!(runtime.current_size(), (45, 80));
        assert_eq!(runtime.read().terminal_dimensions(), Some((80, 45)));
        assert_eq!(
            runtime
                .read()
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
            pane_id,
            terminal,
            io: Box::new(io),
            current_size: shepr_core::geometry::PaneGeometry::new(24, 80, 0, 0),
            child_liveness: Arc::new(ChildLiveness::absent()),
            teardown_tracker: Arc::default(),
            exit_arbiter: Arc::default(),
            cwd: Arc::default(),
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
            pane_id,
            terminal,
            io: Box::new(io),
            current_size: shepr_core::geometry::PaneGeometry::new(24, 80, 0, 0),
            child_liveness: Arc::new(ChildLiveness::absent()),
            teardown_tracker: Arc::default(),
            exit_arbiter: Arc::default(),
            cwd: Arc::default(),
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
    async fn agent_transition_clears_retained_osc_evidence() {
        let runtime = PaneRuntime::test_with_screen_bytes(80, 24, b"");
        runtime.test_process_pty_bytes(b"\x1b]2;startup title\x1b\\\x1b]9;4;1;\x1b\\");

        let inputs = runtime.read().agent_detection_inputs();
        assert_eq!(inputs.osc_title, "startup title");
        assert_eq!(inputs.osc_progress, "4;1;");

        clear_osc_evidence_for_agent_transition(&runtime.terminal);
        let inputs = runtime.read().agent_detection_inputs();
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
            crate::events::EventSender::runtime(
                tx.clone(),
                pane_id,
                crate::events::RuntimeGeneration::alloc(),
            ),
            pane_id,
            StateChangedUpdate {
                agent: Some(Agent::Pi),
                detection: shepr_agent::detect::Detection::Idle { visible: false },
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
        let AppEvent::Runtime { event, .. } = second else {
            panic!("runtime envelope required");
        };
        assert!(matches!(
            *event,
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
