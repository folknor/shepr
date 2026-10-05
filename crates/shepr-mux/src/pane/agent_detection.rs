use shepr_agent::{Agent, AgentState, PresentedAgentState};
use shepr_detect::Detection;
use shepr_detect::manifest::screen_unknown_is_stable;

use crate::limits::{
    AGENT_PENDING_IDLE_CAP, AGENT_PENDING_IDLE_CONFIRMATIONS, STABLE_VISIBLE_SIGNAL_REFRESH,
};

#[derive(Debug, Default)]
pub(super) struct PendingIdleConfirmation {
    started_at: Option<std::time::Instant>,
    matching_observations: u8,
}

impl PendingIdleConfirmation {
    pub(super) fn active(&self) -> bool {
        self.started_at.is_some()
    }

    pub(super) fn started_at(&self) -> Option<std::time::Instant> {
        self.started_at
    }

    pub(super) fn clear(&mut self) {
        self.started_at = None;
        self.matching_observations = 0;
    }

    pub(super) fn should_hold_working_to_idle(
        &mut self,
        previous: Detection,
        next: Detection,
        agent_changed: bool,
        process_exited: bool,
        now: std::time::Instant,
    ) -> bool {
        let is_working_to_presented_idle = previous.state() == AgentState::Working
            && next.state().presentation_state() == PresentedAgentState::Idle
            && !next.visible_idle()
            && !next.visible_blocker()
            && !agent_changed
            && !process_exited;

        if !is_working_to_presented_idle {
            self.clear();
            return false;
        }

        let Some(started_at) = self.started_at else {
            self.started_at = Some(now);
            self.matching_observations = 1;
            if self.matching_observations >= AGENT_PENDING_IDLE_CONFIRMATIONS {
                self.clear();
                return false;
            }
            return true;
        };

        if now.duration_since(started_at) >= AGENT_PENDING_IDLE_CAP {
            self.clear();
            return false;
        }

        self.matching_observations = self.matching_observations.saturating_add(1);
        if self.matching_observations >= AGENT_PENDING_IDLE_CONFIRMATIONS {
            self.clear();
            return false;
        }

        true
    }
}

/// Whether an unchanged screen may be left unread this tick. Idle is stable
/// for every agent; Unknown is stable when the compiled screen detector can
/// report it (or has no usable manifest).
pub(super) fn should_skip_idle_screen_scan(input: DetectionScreenReadInput) -> bool {
    let stable_state = input.state == AgentState::Idle
        || (input.state == AgentState::Unknown
            && input.agent.is_some_and(screen_unknown_is_stable));
    if !stable_state || input.pending_idle_active || input.agent_changed || input.process_exited {
        return false;
    }

    input.last_screen_scan_detection_content_seq == Some(input.current_detection_content_seq)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionScreenReadDecision {
    Read,
    Skip,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DetectionScreenReadInput {
    pub(super) state: AgentState,
    pub(super) agent: Option<Agent>,
    pub(super) pending_idle_active: bool,
    pub(super) agent_changed: bool,
    pub(super) process_exited: bool,
    pub(super) current_detection_content_seq: u64,
    pub(super) last_screen_scan_detection_content_seq: Option<u64>,
}

pub(super) fn decide_detection_screen_read(
    input: DetectionScreenReadInput,
) -> DetectionScreenReadDecision {
    if should_skip_idle_screen_scan(input) {
        DetectionScreenReadDecision::Skip
    } else {
        DetectionScreenReadDecision::Read
    }
}

pub(super) fn should_publish_detection_update(
    previous: Detection,
    next: Detection,
    agent_changed: bool,
    process_exited: bool,
    stable_visible_signal_refresh_due: bool,
) -> bool {
    next != previous
        || agent_changed
        || process_exited
        || (stable_visible_signal_refresh_due
            && next.visible_blocker()
            && previous.visible_blocker())
}

pub(super) fn stable_visible_signal_refresh_due(
    previous: Detection,
    next: Detection,
    last_refresh: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let stable_visible_signal = next.visible_blocker() && previous.visible_blocker();

    stable_visible_signal
        && last_refresh.is_none_or(|last_refresh| {
            now.duration_since(last_refresh) >= STABLE_VISIBLE_SIGNAL_REFRESH
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionPublishDecision {
    NoPublish,
    Publish {
        detection: Detection,
        process_exited: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenDetectionPublishInput {
    pub(super) previous: Option<Detection>,
    pub(super) last_visible_signal_refresh: Option<std::time::Instant>,
    pub(super) screen_detection: Detection,
    pub(super) process_exited: bool,
    pub(super) agent_changed: bool,
    pub(super) now: std::time::Instant,
}

pub(super) fn decide_screen_detection_publish(
    input: ScreenDetectionPublishInput,
    pending_idle: &mut PendingIdleConfirmation,
) -> DetectionPublishDecision {
    let next_publish = input.screen_detection;
    let Some(previous_publish) = input.previous else {
        pending_idle.clear();
        return DetectionPublishDecision::Publish {
            detection: next_publish,
            process_exited: input.process_exited,
        };
    };
    if pending_idle.should_hold_working_to_idle(
        previous_publish,
        next_publish,
        input.agent_changed,
        input.process_exited,
        input.now,
    ) {
        return DetectionPublishDecision::NoPublish;
    }

    let stable_refresh_due = stable_visible_signal_refresh_due(
        previous_publish,
        next_publish,
        input.last_visible_signal_refresh,
        input.now,
    );
    if should_publish_detection_update(
        previous_publish,
        next_publish,
        input.agent_changed,
        input.process_exited,
        stable_refresh_due,
    ) {
        DetectionPublishDecision::Publish {
            detection: next_publish,
            process_exited: input.process_exited,
        }
    } else {
        DetectionPublishDecision::NoPublish
    }
}

/// Whether this tick's "no agent" report is held back. A resumed pane's detector
/// starts out believing nothing runs, and its first screen read would report
/// that (a differing state always publishes). For a fresh pane that report
/// changes nothing: the terminal already has no agent. It matters only
/// where the terminal already names an agent the detector has not seen yet,
/// which is a restored pane whose resume command was just typed:
/// `restored_terminal` seeds the resumed agent as detected so the sidebar
/// shows it, and withdrawing that seed while the shell is still starting up
/// makes the agent drop out of the sidebar until its process is identified.
///
/// So until `hold_until` passes, an absent agent is not reported. The hold
/// ends for good as soon as an agent is identified (from then on the
/// detector's own view is the truth) or once it expires, after which a pane
/// whose resume never produced the agent reports the absence as usual and
/// the seed goes.
pub(super) fn withhold_agent_absence(
    agent: Option<Agent>,
    hold_until: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let Some(until) = *hold_until else {
        return false;
    };
    if agent.is_some() || now >= until {
        *hold_until = None;
        return false;
    }
    true
}

pub(super) fn detection_update_for_publish_with_osc(
    agent: Option<Agent>,
    content: &str,
    osc_title: Option<&str>,
    osc_progress: Option<&str>,
    process_exited: bool,
) -> Option<Detection> {
    if process_exited {
        return Some(Detection::Idle { visible: true });
    }

    // Screen text has no indication of which rows came from the current
    // process. If restore seeds saved rows into the active screen, its caller
    // must preserve that provenance before state matching.
    let detection = shepr_detect::detect_agent_with_osc(agent, content, osc_title, osc_progress);
    detection.detection()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{AGENT_ABSENCE_STARTUP_HOLD, AGENT_PENDING_IDLE_RECHECK};

    fn publish_state(state: AgentState) -> Detection {
        Detection::new(state, false)
    }

    fn screen_detection(state: AgentState) -> Detection {
        Detection::new(
            state,
            matches!(state, AgentState::Idle | AgentState::Working),
        )
    }

    fn screen_publish_input(
        current_state: AgentState,
        screen_detection: Detection,
        now: std::time::Instant,
    ) -> ScreenDetectionPublishInput {
        ScreenDetectionPublishInput {
            previous: Some(Detection::new(current_state, false)),
            last_visible_signal_refresh: None,
            screen_detection,
            process_exited: false,
            agent_changed: false,
            now,
        }
    }

    fn screen_read_input(state: AgentState, current_seq: u64) -> DetectionScreenReadInput {
        DetectionScreenReadInput {
            state,
            agent: Some(Agent::Codex),
            pending_idle_active: false,
            agent_changed: false,
            process_exited: false,
            current_detection_content_seq: current_seq,
            last_screen_scan_detection_content_seq: Some(10),
        }
    }

    #[test]
    fn agent_absence_is_held_until_the_hold_expires() {
        let now = std::time::Instant::now();
        let mut hold = Some(now + AGENT_ABSENCE_STARTUP_HOLD);

        assert!(withhold_agent_absence(None, &mut hold, now));
        assert!(withhold_agent_absence(
            None,
            &mut hold,
            now + AGENT_ABSENCE_STARTUP_HOLD - std::time::Duration::from_millis(1)
        ));
        assert!(!withhold_agent_absence(
            None,
            &mut hold,
            now + AGENT_ABSENCE_STARTUP_HOLD
        ));
        assert_eq!(hold, None);
        // Expired for good: a later absence is reported at once.
        assert!(!withhold_agent_absence(None, &mut hold, now));
    }

    #[test]
    fn identifying_an_agent_ends_the_absence_hold() {
        let now = std::time::Instant::now();
        let mut hold = Some(now + AGENT_ABSENCE_STARTUP_HOLD);

        assert!(!withhold_agent_absence(Some(Agent::Codex), &mut hold, now));
        assert_eq!(hold, None);
        // The agent leaving again inside the original window is reported.
        assert!(!withhold_agent_absence(None, &mut hold, now));
    }

    #[test]
    fn screen_read_skips_unchanged_idle_bottom_buffer() {
        assert_eq!(
            decide_detection_screen_read(screen_read_input(AgentState::Idle, 10)),
            DetectionScreenReadDecision::Skip
        );
    }

    #[test]
    fn screen_read_skips_unchanged_ambiguous_codex_but_not_new_content_or_replacement() {
        let mut input = screen_read_input(AgentState::Unknown, 10);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Skip
        );
        input.current_detection_content_seq = 11;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
        input.current_detection_content_seq = 10;
        input.agent_changed = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
        input.agent_changed = false;
        input.agent = Some(Agent::Pi);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
    }

    #[test]
    fn screen_read_skips_unchanged_unknown_when_manifest_can_return_it() {
        for agent in [Agent::Omp, Agent::Mastracode] {
            let mut input = screen_read_input(AgentState::Unknown, 10);
            input.agent = Some(agent);
            assert_eq!(
                decide_detection_screen_read(input),
                DetectionScreenReadDecision::Skip
            );
            input.current_detection_content_seq = 11;
            assert_eq!(
                decide_detection_screen_read(input),
                DetectionScreenReadDecision::Read
            );
        }
        // Gemini cannot report a stable Unknown from its compiled manifest.
        let mut input = screen_read_input(AgentState::Unknown, 10);
        input.agent = Some(Agent::Gemini);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );

        input.agent = Some(Agent::Letta);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Skip
        );
    }

    #[test]
    fn screen_read_reads_when_idle_bottom_buffer_changes() {
        assert_eq!(
            decide_detection_screen_read(screen_read_input(AgentState::Idle, 11)),
            DetectionScreenReadDecision::Read
        );
    }

    #[test]
    fn screen_read_reads_for_transitions_and_changed_content_without_agent() {
        let mut input = screen_read_input(AgentState::Idle, 10);
        input.pending_idle_active = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );

        let mut input = screen_read_input(AgentState::Idle, 10);
        input.agent_changed = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );

        let mut input = screen_read_input(AgentState::Idle, 10);
        input.process_exited = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );

        let mut input = screen_read_input(AgentState::Idle, 10);
        input.agent = None;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Skip
        );
        input.current_detection_content_seq = 11;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
    }

    #[test]
    fn agent_detection_holds_working_to_plain_idle_until_confirmed() {
        let now = std::time::Instant::now();
        let previous = publish_state(AgentState::Working);
        let next = publish_state(AgentState::Idle);
        let mut pending = PendingIdleConfirmation::default();

        assert!(pending.should_hold_working_to_idle(previous, next, false, false, now));
        assert!(pending.should_hold_working_to_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK
        ));
        assert!(!pending.should_hold_working_to_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK * 2
        ));
    }

    #[test]
    fn agent_detection_holds_working_to_unknown_until_confirmed() {
        let now = std::time::Instant::now();
        let previous = publish_state(AgentState::Working);
        let next = publish_state(AgentState::Unknown);
        let mut pending = PendingIdleConfirmation::default();

        assert!(pending.should_hold_working_to_idle(previous, next, false, false, now));
        assert!(pending.should_hold_working_to_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK
        ));
        assert!(!pending.should_hold_working_to_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK * 2
        ));
    }

    #[test]
    fn visible_idle_bypasses_plain_idle_hold() {
        let now = std::time::Instant::now();
        let previous = publish_state(AgentState::Working);
        let next = Detection::Idle { visible: true };
        let mut pending = PendingIdleConfirmation::default();

        assert!(!pending.should_hold_working_to_idle(previous, next, false, false, now));
    }

    #[test]
    fn first_unknown_report_establishes_a_baseline() {
        let now = std::time::Instant::now();
        let mut input = screen_publish_input(AgentState::Unknown, Detection::Unknown, now);
        input.previous = None;
        let mut pending_idle = PendingIdleConfirmation::default();
        assert_eq!(
            decide_screen_detection_publish(input, &mut pending_idle),
            DetectionPublishDecision::Publish {
                detection: Detection::Unknown,
                process_exited: false,
            }
        );
        input.previous = Some(Detection::Unknown);
        assert_eq!(
            decide_screen_detection_publish(input, &mut pending_idle),
            DetectionPublishDecision::NoPublish
        );
    }

    #[test]
    fn screen_publish_publishes_visible_blocker() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();
        let detection = Detection::Blocked { visible: true };

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Idle, detection, now),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                detection: Detection::Blocked { visible: true },
                process_exited: false,
            }
        );
    }

    #[test]
    fn screen_publish_keeps_visible_working_without_pty_activity() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Idle, screen_detection(AgentState::Working), now,),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                detection: Detection::Working { visible: true },
                process_exited: false,
            }
        );
    }

    #[test]
    fn screen_publish_can_publish_idle_without_input_taint_delay() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Blocked, screen_detection(AgentState::Idle), now,),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                detection: Detection::Idle { visible: true },
                process_exited: false,
            }
        );
    }
}
