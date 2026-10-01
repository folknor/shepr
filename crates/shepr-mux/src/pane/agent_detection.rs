use crate::limits::AGENT_PENDING_IDLE_CONFIRMATIONS;
pub(super) use crate::limits::{
    AGENT_ABSENCE_STARTUP_HOLD, AGENT_PENDING_IDLE_CAP, AGENT_PENDING_IDLE_RECHECK,
    AGENT_STARTUP_GRACE_WINDOW, STABLE_VISIBLE_SIGNAL_REFRESH,
};

use shepr_agent::detect::manifest::screen_unknown_is_stable;
use shepr_agent::detect::{Agent, AgentDetection, AgentState, PresentedAgentState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetectionPublishState {
    pub(super) state: AgentState,
    pub(super) visible_idle: bool,
    pub(super) visible_blocker: bool,
    pub(super) visible_working: bool,
}

#[derive(Debug, Default)]
pub(super) struct PendingIdleConfirmation {
    started_at: Option<std::time::Instant>,
    matching_observations: u8,
}

impl PendingIdleConfirmation {
    pub(super) fn active(&self) -> bool {
        self.started_at.is_some()
    }

    pub(super) fn clear(&mut self) {
        self.started_at = None;
        self.matching_observations = 0;
    }

    pub(super) fn should_hold_working_to_idle(
        &mut self,
        previous: DetectionPublishState,
        next: DetectionPublishState,
        agent_changed: bool,
        process_exited: bool,
        now: std::time::Instant,
    ) -> bool {
        let is_working_to_presented_idle = previous.state == AgentState::Working
            && next.state.presentation_state() == PresentedAgentState::Idle
            && !next.visible_idle
            && !next.visible_blocker
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

    input.current_detection_content_seq.is_some()
        && input.last_screen_scan_detection_content_seq == input.current_detection_content_seq
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
    pub(super) current_detection_content_seq: Option<u64>,
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
    previous: DetectionPublishState,
    next: DetectionPublishState,
    agent_changed: bool,
    process_exited: bool,
    stable_visible_signal_refresh_due: bool,
) -> bool {
    next.state != previous.state
        || next.visible_idle != previous.visible_idle
        || next.visible_blocker != previous.visible_blocker
        || next.visible_working != previous.visible_working
        || agent_changed
        || process_exited
        || (stable_visible_signal_refresh_due && next.visible_blocker && previous.visible_blocker)
}

pub(super) fn stable_visible_signal_refresh_due(
    previous: DetectionPublishState,
    next: DetectionPublishState,
    last_refresh: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let stable_visible_signal = next.visible_blocker && previous.visible_blocker;

    stable_visible_signal
        && last_refresh.is_none_or(|last_refresh| {
            now.duration_since(last_refresh) >= STABLE_VISIBLE_SIGNAL_REFRESH
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionPublishDecision {
    NoPublish,
    Publish {
        state: AgentState,
        visible_idle: bool,
        visible_blocker: bool,
        visible_working: bool,
        process_exited: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenDetectionPublishInput {
    pub(super) current_state: AgentState,
    pub(super) last_visible_idle: bool,
    pub(super) last_visible_blocker: bool,
    pub(super) last_visible_working: bool,
    pub(super) last_visible_signal_refresh: Option<std::time::Instant>,
    pub(super) screen_detection: AgentDetection,
    pub(super) process_exited: bool,
    pub(super) agent_changed: bool,
    pub(super) now: std::time::Instant,
}

pub(super) fn decide_screen_detection_publish(
    input: ScreenDetectionPublishInput,
    pending_idle: &mut PendingIdleConfirmation,
) -> DetectionPublishDecision {
    let detection = input.screen_detection;
    // Published as detected: debouncing lives in the pending-idle hold below,
    // not in a separate stabilisation step.
    let new_state = detection.state;
    let visible_idle = detection.visible_idle && new_state == AgentState::Idle;
    let visible_blocker = detection.visible_blocker && new_state == AgentState::Blocked;
    let visible_working = detection.visible_working && new_state == AgentState::Working;

    let previous_publish = DetectionPublishState {
        state: input.current_state,
        visible_idle: input.last_visible_idle,
        visible_blocker: input.last_visible_blocker,
        visible_working: input.last_visible_working,
    };
    let next_publish = DetectionPublishState {
        state: new_state,
        visible_idle,
        visible_blocker,
        visible_working,
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
            state: new_state,
            visible_idle,
            visible_blocker,
            visible_working,
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
    osc_title: &str,
    osc_progress: &str,
    process_exited: bool,
) -> Option<shepr_agent::detect::AgentDetection> {
    if process_exited {
        return Some(shepr_agent::detect::AgentDetection {
            state: AgentState::Idle,
            skip_state_update: false,
            visible_idle: true,
            visible_blocker: false,
            visible_working: false,
        });
    }

    // Screen text has no indication of which rows came from the current
    // process. If restore seeds saved rows into the active screen, its caller
    // must preserve that provenance before state matching.
    let detection =
        shepr_agent::detect::detect_agent_with_osc(agent, content, osc_title, osc_progress);
    (!detection.skip_state_update).then_some(detection)
}

pub(super) fn observe_detection_content_change(bytes: &[u8], detection_content_seq: &mut u64) {
    if !bytes.is_empty() {
        *detection_content_seq = detection_content_seq.wrapping_add(1);
    }
}

pub(super) fn mark_detection_content_changed(detection_content_seq: &mut u64) {
    *detection_content_seq = detection_content_seq.wrapping_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish_state(state: AgentState) -> DetectionPublishState {
        DetectionPublishState {
            state,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
        }
    }

    fn screen_detection(state: AgentState) -> AgentDetection {
        AgentDetection {
            state,
            skip_state_update: false,
            visible_idle: state == AgentState::Idle,
            visible_blocker: false,
            visible_working: state == AgentState::Working,
        }
    }

    fn screen_publish_input(
        current_state: AgentState,
        screen_detection: AgentDetection,
        now: std::time::Instant,
    ) -> ScreenDetectionPublishInput {
        ScreenDetectionPublishInput {
            current_state,
            last_visible_idle: false,
            last_visible_blocker: false,
            last_visible_working: false,
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
            current_detection_content_seq: Some(current_seq),
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
    fn restored_claude_dialog_is_excluded_from_a_new_working_frame() {
        let pane = crate::pane::PaneTerminal::new(shepr_vt::Terminal::new(80, 12, 4096));
        pane.seed_history_ansi("Run a dynamic workflow?\r\nChoose a workflow\r\nEsc to cancel");
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        pane.process_pty_bytes(pane_id, b"* Waiting for 1 background agent to finish\r\n");

        let inputs = pane.agent_detection_inputs();
        assert!(!inputs.screen_text.contains("Run a dynamic workflow?"));
        assert!(
            inputs
                .screen_text
                .contains("Waiting for 1 background agent")
        );
        let detection = detection_update_for_publish_with_osc(
            Some(Agent::Claude),
            &inputs.screen_text,
            &inputs.osc_title,
            &inputs.osc_progress,
            false,
        )
        .expect("screen detector reports a state");

        assert_eq!(detection.state, AgentState::Working);
        assert!(!detection.visible_blocker);
    }

    #[test]
    fn rewriting_a_seeded_row_makes_it_live_detection_evidence() {
        let pane = crate::pane::PaneTerminal::new(shepr_vt::Terminal::new(80, 12, 4096));
        pane.seed_history_ansi("saved first row\r\nsaved second row");
        assert!(pane.agent_detection_inputs().screen_text.trim().is_empty());
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        pane.process_pty_bytes(pane_id, b"\x1b[H\x1b[2Klive first row");
        let inputs = pane.agent_detection_inputs();
        assert!(inputs.screen_text.contains("live first row"));
        assert!(!inputs.screen_text.contains("saved second row"));
        // Once rewritten, this row belongs to live output even if
        // the child later prints the original text again.
        pane.process_pty_bytes(pane_id, b"\x1b[H\x1b[2Ksaved first row");
        assert!(
            pane.agent_detection_inputs()
                .screen_text
                .contains("saved first row")
        );
    }

    #[test]
    fn seeded_rows_remain_masked_after_column_reflow() {
        let pane = crate::pane::PaneTerminal::new(shepr_vt::Terminal::new(80, 12, 4096));
        pane.seed_history_ansi("restored agent dialog that wraps after a resize");
        pane.resize(shepr_core::geometry::PaneGeometry::new(12, 12, 0, 0));

        assert!(pane.agent_detection_inputs().screen_text.trim().is_empty());
    }

    #[test]
    fn seeded_rows_remain_masked_when_zero_scrollback_evicts_them() {
        let pane = crate::pane::PaneTerminal::new(shepr_vt::Terminal::new(80, 4, 0));
        pane.seed_history_ansi("saved first row\r\nsaved second row");
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        pane.process_pty_bytes(pane_id, b"live one\r\nlive two\r\n");

        let inputs = pane.agent_detection_inputs();
        assert!(inputs.screen_text.contains("live two"));
        assert!(!inputs.screen_text.contains("saved second row"));
    }

    #[test]
    fn scrolling_seeded_rows_does_not_make_them_live() {
        let pane = crate::pane::PaneTerminal::new(shepr_vt::Terminal::new(80, 4, 4096));
        pane.seed_history_ansi("saved first row\r\nsaved second row");
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        pane.process_pty_bytes(pane_id, b"live one\r\nlive two\r\n");
        let inputs = pane.agent_detection_inputs();
        assert!(inputs.screen_text.contains("live two"));
        assert!(!inputs.screen_text.contains("saved second row"));
        assert!(pane.recent_unwrapped_text(10).contains("saved second row"));
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
        input.current_detection_content_seq = Some(11);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
        input.current_detection_content_seq = Some(10);
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
            input.current_detection_content_seq = Some(11);
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
        input.current_detection_content_seq = Some(11);
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
        let mut next = publish_state(AgentState::Idle);
        next.visible_idle = true;
        let mut pending = PendingIdleConfirmation::default();

        assert!(!pending.should_hold_working_to_idle(previous, next, false, false, now));
    }

    #[test]
    fn screen_publish_publishes_visible_blocker() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();
        let mut detection = screen_detection(AgentState::Blocked);
        detection.visible_blocker = true;

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Idle, detection, now),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                state: AgentState::Blocked,
                visible_idle: false,
                visible_blocker: true,
                visible_working: false,
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
                state: AgentState::Working,
                visible_idle: false,
                visible_blocker: false,
                visible_working: true,
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
                state: AgentState::Idle,
                visible_idle: true,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
            }
        );
    }

    #[test]
    fn detection_content_change_tracks_raw_nonempty_reads_for_scan_scheduling() {
        let mut seq = 0;

        observe_detection_content_change(b"", &mut seq);
        assert_eq!(seq, 0);

        observe_detection_content_change(b"\x1b[?2026h", &mut seq);
        assert_eq!(seq, 1);

        observe_detection_content_change(b"body bytes", &mut seq);
        assert_eq!(seq, 2);
    }

    #[test]
    fn local_terminal_mutations_can_invalidate_idle_scan_skip() {
        let mut seq = 0;

        mark_detection_content_changed(&mut seq);

        assert_eq!(seq, 1);
    }
}
