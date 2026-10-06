//! The screen half of a tick: whether the screen is read, the cache that spares
//! an unchanged screen a second match, and what reaches the server.

use tracing::warn;

use super::state::{DetectorState, TickContext};
use crate::pane::agent_detection::{
    DetectionPublishDecision, DetectionScreenReadDecision, DetectionScreenReadInput,
    ScreenDetectionPublishInput, decide_detection_screen_read, decide_screen_detection_publish,
    detection_update_for_publish_with_osc, withhold_agent_absence,
};
use crate::pane::terminal::AgentDetectionInputs;
use shepr_agent::{Agent, AgentState};
use shepr_detect::Detection;

#[derive(Debug, Clone, Copy)]
pub(in crate::pane) struct StateChangedUpdate {
    pub(in crate::pane) agent: Option<Agent>,
    pub(in crate::pane) detection: Detection,
    // This observes agent absence, not the pane child's exit reason. The app
    // applies it under a live child; after child death the watcher decides
    // whether the resume identity belongs in the pane-exit checkpoint.
    pub(in crate::pane) process_exited: bool,
    pub(in crate::pane) observed_at: std::time::Instant,
}

pub(in crate::pane) async fn publish_state_changed_event(
    state_events: crate::events::EventSender,
    update: StateChangedUpdate,
) {
    // This runs on the async detector task, not the PTY reader thread.
    // Waiting for queue space here preserves correctness-critical state transitions
    // without blocking pane I/O. Carry the complete screen verdict to arbitration.
    let pane_id = state_events.pane_id();
    if let Err(e) = state_events
        .send(crate::events::RuntimeEvent::StateChanged {
            agent: update.agent,
            detection: update.detection,
            process_exited: update.process_exited,
            observed_at: update.observed_at,
        })
        .await
    {
        warn!(
            pane = %pane_id,
            error = %e,
            "failed to deliver StateChanged event"
        );
    }
}

pub(in crate::pane) async fn publish_agent_process_detected_event(
    state_events: crate::events::EventSender,
    agent: Agent,
    observed_at: std::time::Instant,
) {
    let pane_id = state_events.pane_id();
    if let Err(e) = state_events
        .send(crate::events::RuntimeEvent::AgentProcessDetected { agent, observed_at })
        .await
    {
        warn!(
            pane = %pane_id,
            error = %e,
            "failed to deliver AgentProcessDetected event"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScreenDetectionCacheEntry {
    agent: Option<Agent>,
    process_exited: bool,
    detection_content_seq: u64,
    result: Option<Detection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenDetectionCacheLookup {
    Miss,
    Hit(Option<Detection>),
}

/// How a tick's screen half starts. Only an identified agent needs the screen
/// text; with no agent the matcher sees none and the tick finishes at once.
pub(super) enum ScreenStep {
    Finished(Option<StateChangedUpdate>),
    NeedsScreen,
}

impl DetectorState {
    /// Runs the gates and the cache. Resuming a `NeedsScreen` goes straight to
    /// `complete_screen`: nothing changes between the two, so the gates would
    /// answer as they did here.
    pub(super) fn begin_screen(&mut self, tick: &TickContext) -> ScreenStep {
        let agent = self.current_agent();
        let process_exited = self.process_exited();
        if !self.may_scan_screen(tick, process_exited)
            || !self.should_read_screen(tick, agent, process_exited)
        {
            return ScreenStep::Finished(None);
        }
        match self.cached_screen_detection(agent, process_exited, tick.content_seq) {
            ScreenDetectionCacheLookup::Hit(result) => ScreenStep::Finished(self.publish_screen(
                tick,
                agent,
                process_exited,
                result,
                false,
            )),
            ScreenDetectionCacheLookup::Miss if agent.is_some() => ScreenStep::NeedsScreen,
            ScreenDetectionCacheLookup::Miss => {
                ScreenStep::Finished(self.complete_screen(tick, &AgentDetectionInputs::default()))
            }
        }
    }

    /// Matches `screen` against the detector's agent, remembers the result and
    /// publishes it if it differs from what was last published.
    pub(super) fn complete_screen(
        &mut self,
        tick: &TickContext,
        screen: &AgentDetectionInputs,
    ) -> Option<StateChangedUpdate> {
        let agent = self.current_agent();
        let process_exited = self.process_exited();
        let changed = self.observe_screen_sequence(tick.content_seq) && agent.is_none();
        let detection = detection_update_for_publish_with_osc(
            agent,
            &screen.screen_text,
            screen.osc_title.as_deref(),
            screen.osc_progress.as_deref(),
            process_exited,
        );
        self.last_screen_detection = Some(ScreenDetectionCacheEntry {
            agent,
            process_exited,
            detection_content_seq: tick.content_seq,
            result: detection,
        });
        self.publish_screen(tick, agent, process_exited, detection, changed)
    }

    fn publish_screen(
        &mut self,
        tick: &TickContext,
        agent: Option<Agent>,
        process_exited: bool,
        detection: Option<Detection>,
        content_changed: bool,
    ) -> Option<StateChangedUpdate> {
        let Some(detection) = detection else {
            self.pending_idle.clear();
            return None;
        };
        self.scheduler.content_changed(
            tick.now,
            self.current_agent(),
            tick.group_changed,
            content_changed,
        );
        // Expiry means no agent was identified before the deadline, not that
        // the agent rejected its session. This pure detector has neither the
        // public pane identity nor the saved session reference, so it logs
        // nothing here; a log of the expiry belongs to a caller holding both.
        // Wiring that observation requires the runtime to carry the resume
        // session and public pane id, not reconstruct them from screen text.
        // An identified process also cannot confirm that it accepted a session.
        if withhold_agent_absence(agent, &mut self.agent_absence_hold_until, tick.now) {
            self.pending_idle.clear();
            return None;
        }
        let DetectionPublishDecision::Publish {
            detection,
            process_exited,
        } = decide_screen_detection_publish(
            ScreenDetectionPublishInput {
                screen_detection: detection,
                previous: self.last_published,
                last_visible_signal_refresh: self.last_visible_signal_refresh,
                process_exited,
                agent_changed: tick.agent_changed,
                now: tick.now,
            },
            &mut self.pending_idle,
        )
        else {
            return None;
        };
        self.last_published = Some(detection);
        self.last_visible_signal_refresh =
            if detection.visible_blocker() || detection.visible_working() {
                Some(tick.now)
            } else {
                None
            };
        if process_exited {
            self.exit_phase.report();
        }
        Some(StateChangedUpdate {
            agent,
            detection,
            process_exited,
            observed_at: tick.now,
        })
    }

    pub(super) fn may_scan_screen(&mut self, tick: &TickContext, process_exited: bool) -> bool {
        if tick.lifecycle_authority_active && !process_exited {
            self.pending_idle.clear();
            return false;
        }
        if let Some(until) = self.agent_startup_grace_until {
            if process_exited {
                self.agent_startup_grace_until = None;
                self.last_screen_scan_detection_content_seq = None;
                self.pending_idle.clear();
            } else if tick.now < until {
                self.pending_idle.clear();
                return false;
            } else {
                self.agent_startup_grace_until = None;
                self.pending_idle.clear();
            }
        }
        true
    }

    pub(super) fn should_read_screen(
        &self,
        tick: &TickContext,
        agent: Option<Agent>,
        process_exited: bool,
    ) -> bool {
        // Without a published baseline the first report must read the screen,
        // including while a restored agent's absence hold is waiting to expire.
        if self.last_published.is_none() {
            return true;
        }
        matches!(
            decide_detection_screen_read(DetectionScreenReadInput {
                state: self
                    .last_published
                    .map_or(AgentState::Unknown, Detection::state),
                agent,
                pending_idle_active: self.pending_idle.active(),
                agent_changed: tick.agent_changed,
                process_exited,
                current_detection_content_seq: tick.content_seq,
                last_screen_scan_detection_content_seq: self.last_screen_scan_detection_content_seq,
            }),
            DetectionScreenReadDecision::Read
        )
    }

    pub(super) fn observe_screen_sequence(&mut self, sequence: u64) -> bool {
        let changed = self.last_screen_scan_detection_content_seq != Some(sequence);
        self.last_screen_scan_detection_content_seq = Some(sequence);
        changed
    }

    fn cached_screen_detection(
        &self,
        agent: Option<Agent>,
        process_exited: bool,
        detection_content_seq: u64,
    ) -> ScreenDetectionCacheLookup {
        match self.last_screen_detection {
            Some(entry)
                if entry.agent == agent
                    && entry.process_exited == process_exited
                    && entry.detection_content_seq == detection_content_seq =>
            {
                ScreenDetectionCacheLookup::Hit(entry.result)
            }
            _ => ScreenDetectionCacheLookup::Miss,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::probe::AgentDetectionPresence;
    use super::*;
    use crate::events::AppEvent;
    use crate::limits::{AGENT_STARTUP_GRACE_WINDOW, PROCESS_RECHECK_NO_AGENT};
    use crate::pane::detect::DetectorGateDiagnostics;
    use crate::pane::launch::LaunchKind;
    use std::future::Future;
    use std::task::Poll;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn tick(now: Instant, content_seq: u64) -> TickContext {
        TickContext::new(now, None, content_seq, false, false)
    }

    #[test]
    fn agent_detection_does_not_skip_before_first_published_report() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::AgentResume);
        assert_eq!(detector.current_agent(), None);
        assert_eq!(detector.tick_interval(now, false), PROCESS_RECHECK_NO_AGENT);
        assert!(withhold_agent_absence(
            None,
            &mut detector.agent_absence_hold_until,
            now
        ));
        assert!(detector.should_read_screen(&tick(now, 1), None, false));
        assert!(detector.observe_screen_sequence(1));
        assert!(detector.should_read_screen(&tick(now, 1), None, false));
        detector.reset();
        assert!(detector.should_read_screen(&tick(now, 2), None, false));
    }

    #[test]
    fn agent_detection_allows_scan_at_startup_grace_deadline() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        let deadline = now + AGENT_STARTUP_GRACE_WINDOW;
        detector.agent_startup_grace_until = Some(deadline);

        assert!(!detector.may_scan_screen(&tick(deadline - Duration::from_millis(1), 0), false));
        assert!(detector.may_scan_screen(&tick(deadline, 0), false));
        assert_eq!(detector.agent_startup_grace_until, None);
    }

    #[test]
    fn diagnostics_name_the_real_pending_idle_confirmation_hold() {
        let now = Instant::now();
        let mut detector = DetectorState::new(now, LaunchKind::Fresh);
        detector.agent_presence = AgentDetectionPresence::from_agent(Some(Agent::Pi));
        detector.last_published = Some(Detection::Working { visible: false });

        let update = detector.publish_screen(
            &tick(now, 1),
            Some(Agent::Pi),
            false,
            Some(Detection::Idle { visible: false }),
            true,
        );
        assert!(update.is_none(), "the first working-to-idle scan is held");

        let diagnostics = DetectorGateDiagnostics::default();
        diagnostics.update(&detector);
        assert_eq!(
            diagnostics.active_gate(now),
            Some(crate::pane::detect::DetectorGate::PendingIdleConfirmation)
        );
    }

    #[tokio::test]
    async fn state_changed_event_waits_for_queue_space_instead_of_dropping() {
        let (tx, mut rx) = mpsc::channel(1);
        let pane_id = shepr_test_fixtures::fixed_pane_id(42);

        tx.try_send(AppEvent::GitStatusRefreshed {
            outcome: shepr_git::RefreshOutcome::empty(),
        })
        .expect("test precondition");

        let publish = publish_state_changed_event(
            crate::events::EventSender::runtime(
                tx.clone(),
                pane_id,
                crate::events::RuntimeGeneration::alloc(),
            ),
            StateChangedUpdate {
                agent: Some(Agent::Pi),
                detection: shepr_detect::Detection::Idle { visible: false },
                process_exited: false,
                observed_at: Instant::now(),
            },
        );
        tokio::pin!(publish);

        let pending =
            std::future::poll_fn(|cx| Poll::Ready(publish.as_mut().poll(cx).is_pending())).await;
        assert!(pending, "publisher should wait for queue space");

        let first = rx.recv().await.expect("sender still alive");
        assert!(matches!(first, AppEvent::GitStatusRefreshed { .. }));

        (&mut publish).await;

        let second = rx.recv().await.expect("sender still alive");
        let AppEvent::Runtime { event, .. } = second else {
            panic!("runtime envelope required");
        };
        assert!(matches!(
            *event,
            crate::events::RuntimeEvent::StateChanged {
                agent: Some(Agent::Pi),
                detection: shepr_detect::Detection::Idle { visible: false },
                process_exited: false,
                observed_at: _,
            }
        ));
    }
}
