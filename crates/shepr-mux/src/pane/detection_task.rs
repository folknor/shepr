use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::info;

use super::launch::LaunchPurpose;
use super::launch_status::LaunchWatch;
use super::process_probe::*;
use super::teardown::ChildLiveness;
use super::terminal::PaneTerminal;
use crate::events::EventSender;
use crate::render_signal::RenderSignal;
use shepr_core::layout::PaneId;

/// Handles moved together between the async loop and its blocking tick.
pub(super) struct DetectionHandles {
    pub(super) terminal: Arc<PaneTerminal>,
    pub(super) child_liveness: Arc<ChildLiveness>,
    pub(super) lifecycle_authority: Arc<AtomicBool>,
    pub(super) reset: Arc<Notify>,
    pub(super) events: EventSender,
    pub(super) render_notify: Arc<Notify>,
    pub(super) render_dirty: Arc<RenderSignal>,
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
    provisional_release: Option<StateChangedUpdate>,
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
        launch_purpose: LaunchPurpose,
        mut launch: LaunchWatch,
        handles: DetectionHandles,
    ) -> tokio::task::AbortHandle {
        tokio::spawn(async move {
            if !launch.launched().await {
                return;
            }
            let task = Self {
                pane_id,
                handles,
                detector: DetectorState::new(Instant::now(), launch_purpose),
                next_wake: crate::limits::PROCESS_RECHECK_NO_AGENT,
                cancelled: Arc::new(AtomicBool::new(false)),
                provisional_release: None,
            };
            task.run().await;
        })
        .abort_handle()
    }

    async fn run(mut self) {
        let _cancel_on_drop = CancelOnDrop(Arc::clone(&self.cancelled));
        loop {
            if self.handles.child_liveness.wait_completed() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(self.next_wake) => {}
                _ = self.handles.reset.notified() => self.detector.reset(),
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
                publish_agent_process_detected_event(
                    self.handles.events.clone(),
                    self.pane_id,
                    agent,
                    now,
                )
                .await;
            }
            if let Some(update) = output.state_changed {
                publish_state_changed_event(self.handles.events.clone(), self.pane_id, update)
                    .await;
            }
        }
    }

    async fn blocking_tick(
        mut self,
    ) -> Result<(Self, Option<(Instant, TickOutput)>), tokio::task::JoinError> {
        tokio::task::spawn_blocking(move || {
            let now = Instant::now();
            let output = self.tick(now).map(|output| (now, output));
            (self, output)
        })
        .await
    }

    fn live(&self, pid: u32) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self.handles.child_liveness.live_pid() == Some(pid)
    }

    fn tick(&mut self, now: Instant) -> Option<TickOutput> {
        let pid = self.handles.child_liveness.live_pid()?;
        if !self.live(pid) {
            return None;
        }
        let foreground_pgid = shepr_agent::detect::foreground_process_group_id(pid);
        if !self.live(pid) {
            return None;
        }
        let theme_restore_candidate = self.handles.terminal.has_theme_restore_candidate();
        if !self.live(pid) {
            return None;
        }
        // Read before collecting text: an older cache sequence can cause one
        // extra scan, but cannot mark old text as the latest screen.
        let content_seq = shepr_vt::lock_terminal_core(&self.handles.terminal.core)
            .map_or(0, |core| core.detection_content_seq);
        if !self.live(pid) {
            return None;
        }
        let lifecycle_authority_active = self.handles.lifecycle_authority.load(Ordering::Acquire);
        let observations = |observation| DetectorObservations {
            now,
            foreground_group: foreground_pgid,
            content_seq,
            lifecycle_authority_active,
            theme_restore_candidate,
            observation,
        };
        let mut output = self.detector.tick(&observations(TickObservation::Begin));
        if output.probe {
            let probe = probe_foreground_process(pid, foreground_pgid);
            if !self.live(pid) {
                return None;
            }
            output = self
                .detector
                .tick(&observations(TickObservation::Probe(probe)));
        }
        let process_change = output.process_change.take();
        if let Some(change) = &process_change {
            if change.should_clear_osc_evidence {
                clear_osc_evidence_for_agent_transition(
                    &self.handles.terminal,
                    change.previous_agent,
                );
            }
            if change.agent_changed {
                info!(
                    pane = self.pane_id.raw(),
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
            && self
                .handles
                .terminal
                .maybe_restore_host_terminal_theme(self.pane_id, &self.handles.child_liveness)
            && self.live(pid)
            && self
                .handles
                .render_dirty
                .request_pty_coalesced(self.pane_id, &self.handles.terminal.render_queued)
        {
            self.handles.render_notify.notify_one();
        }
        if !self.live(pid) {
            return None;
        }
        if output.screen {
            let inputs = self.handles.terminal.agent_detection_inputs();
            if !self.live(pid) {
                return None;
            }
            output = self
                .detector
                .tick(&observations(TickObservation::Screen(inputs)));
        }
        // Screen resume produces a new output. Keep the identity transition
        // from the probe so it is published before the resulting state.
        output.process_change = process_change;
        self.confirm_live_shell_release(&mut output, now);
        // Confirmation is evidence from this completed tick, so check child
        // liveness again after all potentially blocking terminal work.
        self.live(pid).then_some(output)
    }

    fn confirm_live_shell_release(&mut self, output: &mut TickOutput, now: Instant) {
        if output
            .process_change
            .as_ref()
            .is_some_and(|change| change.process_detected.is_some())
            || output
                .state_changed
                .is_some_and(|update| update.agent.is_some() && !update.process_exited)
        {
            self.provisional_release = None;
        }
        if let Some(pending) = self.provisional_release {
            let deadline = pending.observed_at + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE;
            if now >= deadline {
                self.provisional_release = None;
                // Any later live-shell observation confirms the release at the
                // terminal, so keep this tick's own update when it has one: the
                // detector has already committed it. A quiet live shell has
                // none, so republish the release itself.
                if output.state_changed.is_none() {
                    output.state_changed = Some(StateChangedUpdate {
                        observed_at: now,
                        ..pending
                    });
                }
            } else {
                output.next_wake = output.next_wake.min(deadline.duration_since(now));
            }
        } else if let Some(update) = output.state_changed.filter(|update| update.process_exited) {
            self.provisional_release = Some(update);
            output.next_wake = output
                .next_wake
                .min(crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE);
        }
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
                terminal: Arc::new(PaneTerminal::new(shepr_vt::Terminal::new(80, 24, 0))),
                child_liveness: Arc::new(ChildLiveness::new(std::process::id(), None)),
                lifecycle_authority: Arc::new(AtomicBool::new(false)),
                reset: Arc::new(Notify::new()),
                events: events.into(),
                render_notify: Arc::new(Notify::new()),
                render_dirty: Arc::new(RenderSignal::new()),
            },
            detector: DetectorState::new(Instant::now(), LaunchPurpose::Fresh),
            next_wake: Duration::ZERO,
            cancelled: Arc::new(AtomicBool::new(false)),
            provisional_release: None,
        }
    }

    fn quiet_output() -> TickOutput {
        TickOutput {
            probe: false,
            screen: false,
            process_change: None,
            state_changed: None,
            next_wake: crate::limits::PROCESS_RECHECK_NO_AGENT,
        }
    }

    #[test]
    fn quiet_shell_tick_republishes_exit_after_grace() {
        let mut task = task();
        // clock-io-ok: synthetic detector confirmation times.
        let now = Instant::now();
        let mut output = quiet_output();
        output.state_changed = Some(StateChangedUpdate {
            agent: Some(shepr_agent::agent::Agent::Pi),
            state: shepr_agent::detect::AgentState::Idle,
            visible_blocker: false,
            process_exited: true,
            observed_at: now,
        });
        task.confirm_live_shell_release(&mut output, now);
        let mut quiet = quiet_output();
        let halfway = now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE / 2;
        task.confirm_live_shell_release(&mut quiet, halfway);
        assert!(quiet.state_changed.is_none());
        assert_eq!(
            quiet.next_wake,
            crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE / 2
        );
        let confirmed_at = now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE;
        let mut quiet = quiet_output();
        task.confirm_live_shell_release(&mut quiet, confirmed_at);
        let update = quiet.state_changed.expect("quiet shell confirms exit");
        assert!(update.process_exited);
        assert_eq!(update.observed_at, confirmed_at);
        assert!(task.provisional_release.is_none());
        let mut quiet = quiet_output();
        task.confirm_live_shell_release(&mut quiet, confirmed_at);
        assert!(quiet.state_changed.is_none());
    }

    #[test]
    fn replacement_state_cancels_scheduled_confirmation() {
        let mut task = task();
        // clock-io-ok: synthetic detector confirmation times.
        let now = Instant::now();
        task.provisional_release = Some(StateChangedUpdate {
            agent: Some(shepr_agent::agent::Agent::Pi),
            state: shepr_agent::detect::AgentState::Idle,
            visible_blocker: false,
            process_exited: true,
            observed_at: now,
        });
        let mut output = quiet_output();
        output.state_changed = Some(StateChangedUpdate {
            agent: Some(shepr_agent::agent::Agent::Pi),
            state: shepr_agent::detect::AgentState::Working,
            visible_blocker: false,
            process_exited: false,
            observed_at: now,
        });
        task.confirm_live_shell_release(&mut output, now);
        assert!(task.provisional_release.is_none());
        assert!(
            !output
                .state_changed
                .expect("replacement state")
                .process_exited
        );
    }

    #[test]
    fn confirming_tick_keeps_its_own_committed_update() {
        let mut task = task();
        // clock-io-ok: synthetic detector confirmation times.
        let now = Instant::now();
        task.provisional_release = Some(StateChangedUpdate {
            agent: Some(shepr_agent::agent::Agent::Pi),
            state: shepr_agent::detect::AgentState::Idle,
            visible_blocker: false,
            process_exited: true,
            observed_at: now,
        });
        let confirmed_at = now + crate::limits::AGENT_PROCESS_EXIT_RELEASE_GRACE;
        let withdrawal = StateChangedUpdate {
            agent: None,
            state: shepr_agent::detect::AgentState::Unknown,
            visible_blocker: false,
            process_exited: false,
            observed_at: confirmed_at,
        };
        let mut output = quiet_output();
        output.state_changed = Some(withdrawal);
        task.confirm_live_shell_release(&mut output, confirmed_at);
        // The terminal confirms the release on any later observation, so the
        // detector's own withdrawal is published instead of being replaced.
        assert!(task.provisional_release.is_none());
        let update = output.state_changed.expect("withdrawal");
        assert!(update.agent.is_none());
        assert!(!update.process_exited);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn core_contention_does_not_park_the_async_worker() {
        let task = task();
        let terminal = Arc::clone(&task.handles.terminal);
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _core = shepr_vt::lock_terminal_core(&terminal.core).expect("lock core");
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
        let _core = shepr_vt::lock_terminal_core(&terminal.core).expect("lock core");
        let guard = CancelOnDrop(Arc::clone(&task.cancelled));
        drop(guard);
        assert!(task.tick(Instant::now()).is_none());
    }
}
