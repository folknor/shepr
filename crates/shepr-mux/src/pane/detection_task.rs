use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::info;

use super::detect::{
    DetectorGateDiagnostics, DetectorState, Step, Tick, TickContext, TickOutput,
    publish_agent_process_detected_event, publish_state_changed_event,
};
use super::exit_arbiter::PaneExitArbiter;
use super::launch::LaunchKind;
use super::launch_status::LaunchWatch;
use super::process_probe::probe_foreground_process;
use super::teardown::ChildLiveness;
use super::terminal::PaneTerminal;
use crate::events::EventSender;
use crate::render_signal::{PaneRenderSlot, RenderSignal};
use shepr_core::layout::PaneId;
use shepr_platform::Pid;

/// Handles moved together between the async loop and its blocking tick.
pub(super) struct DetectionHandles {
    pub(super) terminal: Arc<PaneTerminal>,
    pub(super) child_liveness: Arc<ChildLiveness>,
    /// The pane's ending, once decided, stops detection: see `live`.
    pub(super) exit_arbiter: Arc<PaneExitArbiter>,
    pub(super) lifecycle_authority: Arc<AtomicBool>,
    pub(super) reset: Arc<Notify>,
    pub(super) detector_gate_diagnostics: DetectorGateDiagnostics,
    pub(super) events: EventSender,
    pub(super) render_notify: Arc<Notify>,
    pub(super) render_dirty: Arc<RenderSignal>,
    /// This pane's coalescing state in `render_dirty`.
    pub(super) pty_render: PaneRenderSlot,
}

/// Async scheduling and publication surround one blocking job per tick. All
/// terminal access, screen matching and /proc I/O stays in that job: even a
/// metadata read can wait behind a parser or renderer holding the core mutex.
pub(super) struct DetectionTask {
    pane_id: PaneId,
    handles: DetectionHandles,
    detector: DetectorState,
    next_wake: Duration,
    cancelled: Arc<AtomicBool>,
}

/// Aborting an async task cannot stop a blocking job already running. Tell
/// that job to stop at its next boundary, including after a core lock wait.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl DetectionTask {
    /// Detection starts once the pane's shell launched: before exec commits
    /// the child is the server's own image, and its startup windows count from
    /// the launch, not from the fork.
    pub(super) fn spawn(
        pane_id: PaneId,
        launch_purpose: LaunchKind,
        mut launch: LaunchWatch,
        handles: DetectionHandles,
    ) -> tokio::task::AbortHandle {
        tokio::spawn(async move {
            if !launch.launched().await {
                return;
            }
            let detector = DetectorState::new(Instant::now(), launch_purpose);
            handles.detector_gate_diagnostics.update(&detector);
            let task = Self {
                pane_id,
                handles,
                detector,
                next_wake: crate::limits::PROCESS_RECHECK_NO_AGENT,
                cancelled: Arc::new(AtomicBool::new(false)),
            };
            task.run().await;
        })
        .abort_handle()
    }

    async fn run(mut self) {
        let _cancel_on_drop = CancelOnDrop(Arc::clone(&self.cancelled));
        loop {
            if self.handles.child_liveness.wait_completed()
                || self.handles.exit_arbiter.is_decided()
            {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(self.next_wake) => {}
                _ = self.handles.reset.notified() => {
                    self.detector.reset();
                    self.handles.detector_gate_diagnostics.update(&self.detector);
                },
                () = self.handles.exit_arbiter.cancelled() => return,
            }
            let (task, output) = match self.blocking_tick().await {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(?error, "pane detection tick failed");
                    return;
                }
            };
            self = task;
            let Some((now, mut output)) = output else {
                return;
            };
            self.next_wake = output.next_wake;
            // Queue capacity is awaited outside the blocking pool. Preserve
            // process-before-state publication and never overlap pane ticks.
            if let Some(change) = output.process_change.take()
                && let Some(agent) = change.process_detected
            {
                publish_agent_process_detected_event(self.handles.events.clone(), agent, now).await;
            }
            if let Some(update) = output.state_changed {
                publish_state_changed_event(self.handles.events.clone(), update).await;
            }
        }
    }

    async fn blocking_tick(
        mut self,
    ) -> Result<(Self, Option<(Instant, TickOutput)>), tokio::task::JoinError> {
        tokio::task::spawn_blocking(move || {
            let now = Instant::now();
            let output = self.tick(now).map(|output| (now, output));
            self.handles
                .detector_gate_diagnostics
                .update(&self.detector);
            (self, output)
        })
        .await
    }

    // Keep checkpoints around side effects as well as observations: a single
    // observe around the whole tick would validate its return value only after
    // stale work had already cleared OSC evidence or restored the theme. The
    // pane's decided ending is a lock-free flag, so a checkpoint costs no more
    // than the cancellation flag beside it.
    fn live(&self, pid: Pid) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && !self.handles.exit_arbiter.is_decided()
            && self.handles.child_liveness.is_live_process(pid)
    }

    fn tick(&mut self, now: Instant) -> Option<TickOutput> {
        let pid = self.handles.child_liveness.live_process_id()?;
        if !self.live(pid) {
            return None;
        }
        let foreground_pgid = shepr_platform::foreground_process_group_id(pid);
        if !self.live(pid) {
            return None;
        }
        let theme_restore_candidate = self.handles.terminal.has_theme_restore_candidate();
        if !self.live(pid) {
            return None;
        }
        // Read before collecting text: an older cache sequence can cause one
        // extra scan, but cannot mark old text as the latest screen.
        let content_seq = self
            .handles
            .terminal
            .detection_seq()
            .map_or(0, super::DetectionSeq::get);
        if !self.live(pid) {
            return None;
        }
        let lifecycle_authority_active = self.handles.lifecycle_authority.load(Ordering::Acquire);
        let tick = TickContext::new(
            now,
            foreground_pgid,
            content_seq,
            lifecycle_authority_active,
            theme_restore_candidate,
        );
        let mut process_change = None;
        let step = match self.detector.begin(tick) {
            Tick::Done(output) => Step::Done(output),
            Tick::NeedsScreen(screen) => Step::NeedsScreen(screen),
            Tick::NeedsProbe(probe_tick) => {
                let probe = probe_foreground_process(pid, foreground_pgid);
                if !self.live(pid) {
                    return None;
                }
                let (change, step) = probe_tick.resume(&mut self.detector, &probe);
                process_change = Some(change);
                step
            }
        };
        if let Some(change) = &process_change {
            if change.should_clear_osc_evidence {
                // Drops retained OSC evidence after the detector confirms a
                // transition away from an identified agent.
                self.handles.terminal.clear_agent_osc_state();
            }
            if change.agent_changed {
                info!(
                    pane = %self.pane_id,
                    previous_agent = ?change.previous_agent,
                    agent = ?change.agent,
                    process = ?change.process_name,
                    pgid = ?change.process_group_id,
                    "agent changed"
                );
            }
        }
        if !self.live(pid) {
            return None;
        }
        if self.handles.terminal.has_theme_restore_candidate()
            && self.live(pid)
            && super::runtime::maybe_restore_host_terminal_theme(
                &self.handles.terminal,
                self.pane_id,
                &self.handles.child_liveness,
            )
            && self.live(pid)
            && self
                .handles
                .render_dirty
                .request_pty_coalesced(self.pane_id, &self.handles.pty_render)
        {
            self.handles.render_notify.notify_one();
        }
        if !self.live(pid) {
            return None;
        }
        let mut output = match step {
            Step::Done(output) => output,
            Step::NeedsScreen(screen) => {
                let inputs = self.handles.terminal.agent_detection_inputs();
                if !self.live(pid) {
                    return None;
                }
                screen.resume(&mut self.detector, inputs.as_ref())
            }
        };
        // The identity transition from the probe is published before the
        // state the screen resulted in.
        output.process_change = process_change;
        // The output is evidence from this completed tick, so check child
        // liveness again after all potentially blocking terminal work.
        self.live(pid).then_some(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> DetectionTask {
        let (events, _rx) = tokio::sync::mpsc::channel(16);
        DetectionTask {
            pane_id: shepr_test_fixtures::fixed_pane_id(1),
            handles: DetectionHandles {
                terminal: Arc::new(PaneTerminal::new(shepr_vt::Terminal::new(
                    shepr_core::geometry::PaneGeometry::cells_only(80, 24),
                    shepr_core::scrollback::ScrollbackBudget::new(0),
                ))),
                child_liveness: Arc::new(ChildLiveness::running_with_handle(Arc::new(
                    shepr_platform::ProcessHandle::open(
                        shepr_platform::Pid::new(std::process::id()).expect("test pid"),
                    )
                    .expect("current process handle"),
                ))),
                exit_arbiter: Arc::default(),
                lifecycle_authority: Arc::new(AtomicBool::new(false)),
                reset: Arc::new(Notify::new()),
                detector_gate_diagnostics: DetectorGateDiagnostics::default(),
                events: EventSender::runtime(
                    events,
                    shepr_test_fixtures::fixed_pane_id(1),
                    crate::events::RuntimeGeneration::alloc(),
                ),
                render_notify: Arc::new(Notify::new()),
                render_dirty: Arc::new(RenderSignal::new()),
                pty_render: PaneRenderSlot::default(),
            },
            detector: DetectorState::new(Instant::now(), LaunchKind::Fresh),
            next_wake: Duration::ZERO,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn core_contention_does_not_park_the_async_worker() {
        let task = task();
        let terminal = Arc::clone(&task.handles.terminal);
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _core = terminal.core.lock().expect("lock core");
            locked_tx.send(()).expect("announce held core");
            // A finite fallback also lets this test fail without hanging if
            // detection ever starts waiting on the current-thread worker.
            release_rx.recv_timeout(Duration::from_secs(2)).ok();
        });
        locked_rx.await.expect("core is held");
        let started = Instant::now();
        let detection = tokio::spawn(task.run());
        tokio::time::sleep(Duration::from_millis(30)).await;
        let elapsed = started.elapsed();
        release_tx.send(()).ok();
        holder.join().expect("core holder finished");
        detection.abort();
        detection.await.ok();
        assert!(
            elapsed < Duration::from_secs(1),
            "worker parked for {elapsed:?}"
        );
    }

    #[test]
    fn cancellation_stops_a_tick_before_it_waits_for_the_core() {
        let mut task = task();
        let terminal = Arc::clone(&task.handles.terminal);
        let _core = terminal.core.lock().expect("lock core");
        let guard = CancelOnDrop(Arc::clone(&task.cancelled));
        drop(guard);
        assert!(task.tick(Instant::now()).is_none());
    }

    #[test]
    fn a_decided_ending_stops_a_tick_before_it_waits_for_the_core() {
        let mut task = task();
        let terminal = Arc::clone(&task.handles.terminal);
        let _core = terminal.core.lock().expect("lock core");
        task.handles
            .exit_arbiter
            .decide(crate::pane::exit_arbiter::RecordedEnding::Silent);
        assert!(task.tick(Instant::now()).is_none());
    }

    #[tokio::test]
    async fn a_decided_ending_ends_the_detection_loop() {
        let task = task();
        let arbiter = Arc::clone(&task.handles.exit_arbiter);
        let detection = tokio::spawn(task.run());
        tokio::task::yield_now().await;
        arbiter.decide(crate::pane::exit_arbiter::RecordedEnding::Silent);
        tokio::time::timeout(Duration::from_secs(5), detection)
            .await
            .expect("detection loop stops once the pane has ended")
            .expect("detection task");
    }
}
