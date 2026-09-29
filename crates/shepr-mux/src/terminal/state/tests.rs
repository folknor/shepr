use super::*;
use std::time::Duration;

std::thread_local! {
    static TEST_SESSION_ROOT: crate::test_support::ScratchDir =
        crate::test_support::ScratchDir::new("terminal-state-session-paths");
}

fn test_terminal() -> TerminalState {
    TerminalState::new(TerminalId::alloc(), "/tmp".into())
}

fn test_session_path(name: &str) -> String {
    TEST_SESSION_ROOT.with(|root| root.join(name).display().to_string())
}

fn anchor_full_lifecycle_session(
    terminal: &mut TerminalState,
    agent: Agent,
    source: &str,
    agent_label: &str,
    session_ref: shepr_agent::agent::resume::AgentSessionRef,
) {
    terminal.set_detected_state(Some(agent), terminal.fallback_state);
    terminal.set_persisted_agent_session(
        shepr_agent::agent::resume::PersistedAgentSession::from_report(
            source,
            agent_label,
            session_ref,
        )
        .expect("test precondition"),
    );
}

#[test]
fn revision_advances_and_saturates_instead_of_wrapping() {
    let mut terminal = test_terminal();
    let start = terminal.revision();
    terminal.bump_revision();
    assert_eq!(terminal.revision(), start + 1);

    terminal.revision = u64::MAX - 1;
    terminal.bump_revision();
    assert_eq!(terminal.revision(), u64::MAX);
    terminal.bump_revision();
    assert_eq!(terminal.revision(), u64::MAX);
}

#[test]
fn hook_sequence_drops_stragglers_but_survives_a_clock_stepping_back() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(1_000), t0));
    // A racing hook process delivering an older report moments later.
    assert!(!terminal.accept_hook_report_at(
        "shepr:kimi",
        Some(999),
        t0 + Duration::from_millis(50)
    ));
    assert!(!terminal.accept_hook_report_at(
        "shepr:kimi",
        Some(1_000),
        t0 + Duration::from_millis(50)
    ));
    // Rejections do not extend the window.
    assert!(!terminal.accept_hook_report_at(
        "shepr:kimi",
        Some(10),
        t0 + HOOK_SEQUENCE_REANCHOR_AFTER - Duration::from_millis(1)
    ));
    // The wall clock stepped back: after the window the lower seq is
    // accepted and becomes the new anchor.
    let t1 = t0 + HOOK_SEQUENCE_REANCHOR_AFTER;
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(10), t1));
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(11), t1 + Duration::from_millis(10)));
    assert!(!terminal.accept_hook_report_at(
        "shepr:kimi",
        Some(10),
        t1 + Duration::from_millis(20)
    ));
    // Other sources keep their own order.
    assert!(terminal.accept_hook_report_at("shepr:pi", Some(5), t1));
}

#[test]
fn hook_authority_overrides_fallback_for_same_agent() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
    );

    assert_eq!(terminal.detected_agent, Some(Agent::Pi));
    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.effective_agent_label(), Some("pi"));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn hook_authority_can_override_with_unknown_agent_label() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority(
        "shepr:custom".into(),
        "custom-agent".into(),
        AgentState::Working,
        None,
        None,
    );

    assert_eq!(terminal.detected_agent, Some(Agent::Pi));
    assert_eq!(terminal.effective_agent_label(), Some("custom-agent"));
    assert_eq!(terminal.effective_known_agent(), None);
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn omp_hook_authority_overrides_detected_fallback() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Omp), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Omp,
        "shepr:omp",
        "omp",
        shepr_agent::agent::resume::AgentSessionRef::id("omp-root").expect("test precondition"),
    );
    terminal.set_hook_authority(
        "shepr:omp".into(),
        "omp".into(),
        AgentState::Working,
        None,
        None,
    );

    assert_eq!(terminal.detected_agent, Some(Agent::Omp));
    assert_eq!(terminal.effective_agent_label(), Some("omp"));
    assert_eq!(terminal.effective_known_agent(), Some(Agent::Omp));
    assert_eq!(terminal.state, AgentState::Working);

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Omp),
        AgentState::Blocked,
        true,
        false,
        false,
    );

    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn session_only_report_does_not_create_hook_authority() {
    for (agent, source, label, session_id) in [
        (Agent::Codex, "shepr:codex", "codex", "codex-session"),
        (Agent::Devin, "shepr:devin", "devin", "devin-session"),
    ] {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(agent), AgentState::Idle);

        let mutation = terminal.set_agent_session_ref(
            source.into(),
            label.into(),
            shepr_agent::agent::resume::AgentSessionRef::id(session_id),
            Some(1),
        );

        assert!(mutation.is_some());
        assert!(terminal.hook_authority.is_none());
        assert!(!terminal.full_lifecycle_hook_authority_active());
        assert_eq!(terminal.state, AgentState::Idle);

        terminal.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Working,
            false,
            false,
            Instant::now(),
        );

        assert_eq!(terminal.state, AgentState::Working);
    }
}

#[test]
fn startup_session_claim_activates_full_lifecycle_integrations() {
    for (agent, source, label) in [
        (Agent::Kimi, "shepr:kimi", "kimi"),
        (Agent::Kilo, "shepr:kilo", "kilo"),
    ] {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(agent), AgentState::Idle);
        let session_ref = shepr_agent::agent::resume::AgentSessionRef::id(format!("{label}-root"));

        let session = terminal.set_agent_session_ref_for_session_start(
            source.into(),
            label.into(),
            session_ref.clone(),
            Some(10),
            Some("startup"),
        );
        let working = terminal.set_hook_authority_with_session_ref(
            source.into(),
            label.into(),
            AgentState::Working,
            None,
            session_ref,
            Some(11),
        );

        assert!(
            session.is_some(),
            "{label} should accept its startup session"
        );
        assert!(
            working.is_some(),
            "{label} should accept state after startup"
        );
        assert_eq!(terminal.state, AgentState::Working);
    }
}

#[test]
fn session_identity_claims_leave_state_to_detection() {
    // Antigravity reports its session with no start source.
    let (source, label, agent) = ("shepr:agy", "agy", Agent::Antigravity);
    let start_source: Option<&str> = None;
    let replacement_source: Option<&str> = None;
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(agent), AgentState::Idle);
    let first_ref = shepr_agent::agent::resume::AgentSessionRef::id(format!("{label}-root"))
        .expect("test precondition");
    let first = terminal.set_agent_session_ref_for_session_start(
        source.into(),
        label.into(),
        Some(first_ref.clone()),
        Some(10),
        start_source,
    );

    assert!(first.is_some(), "{label} should accept its session");
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| &session.session_ref),
        Some(&first_ref)
    );

    terminal.set_detected_state(Some(agent), AgentState::Working);
    let replacement_ref =
        shepr_agent::agent::resume::AgentSessionRef::id(format!("{label}-replacement"))
            .expect("test precondition");
    let replacement = terminal.set_agent_session_ref_for_session_start(
        source.into(),
        label.into(),
        Some(replacement_ref.clone()),
        Some(11),
        start_source,
    );

    assert!(
        replacement.is_some_and(|mutation| mutation.session_ref_changed),
        "{label} should replace its detected session"
    );
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| &session.session_ref),
        Some(&replacement_ref)
    );

    let legacy_state = terminal.set_hook_authority_with_session_ref(
        source.into(),
        label.into(),
        AgentState::Blocked,
        None,
        Some(replacement_ref.clone()),
        Some(12),
    );
    assert!(legacy_state.is_none());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Working);

    terminal.set_detected_state(None, AgentState::Unknown);
    let background_ref =
        shepr_agent::agent::resume::AgentSessionRef::id(format!("{label}-background"))
            .expect("test precondition");
    let background_replacement = terminal.set_agent_session_ref_for_session_start(
        source.into(),
        label.into(),
        Some(background_ref.clone()),
        Some(13),
        replacement_source,
    );
    assert!(
        background_replacement.is_none(),
        "{label} should reject a background replacement"
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| &session.session_ref),
        Some(&replacement_ref)
    );

    terminal.set_detected_state(Some(agent), AgentState::Idle);
    let retried_replacement = terminal.set_agent_session_ref_for_session_start(
        source.into(),
        label.into(),
        Some(background_ref.clone()),
        Some(14),
        replacement_source,
    );
    assert!(
        retried_replacement.is_some_and(|mutation| mutation.session_ref_changed),
        "{label} should replace the session once detected"
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| &session.session_ref),
        Some(&background_ref)
    );
}

#[test]
fn pi_session_replacement_reports_reanchor_full_lifecycle_authority() {
    for reason in ["new", "resume", "fork"] {
        let mut terminal = test_terminal();
        let old_session = test_session_path(&format!("pi-{reason}-old.jsonl"));
        let new_session = test_session_path(&format!("pi-{reason}-new.jsonl"));
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.set_hook_authority_with_session_ref(
            "shepr:pi".into(),
            "pi".into(),
            AgentState::Idle,
            None,
            shepr_agent::agent::resume::AgentSessionRef::path(old_session),
            Some(10),
        );

        let session_report = terminal.set_agent_session_ref_for_session_start(
            "shepr:pi".into(),
            "pi".into(),
            shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
            Some(11),
            Some(reason),
        );

        assert!(
            session_report.is_some(),
            "{reason} should replace the previous Pi session"
        );
        assert!(terminal.hook_authority.is_none());

        let working = terminal.set_hook_authority_with_session_ref(
            "shepr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
            Some(12),
        );

        assert!(
            working.is_some(),
            "{reason} should accept working for the replacement session"
        );
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(
            terminal
                .hook_authority
                .as_ref()
                .expect("test precondition")
                .session_ref,
            shepr_agent::agent::resume::AgentSessionRef::path(new_session)
        );
    }
}

#[test]
fn pi_resume_reactivates_a_previously_stale_session() {
    let mut terminal = test_terminal();
    let session_a = test_session_path("pi-session-a.jsonl");
    let session_b = test_session_path("pi-session-b.jsonl");
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_a.clone()),
        Some(10),
    );

    terminal.set_agent_session_ref_for_session_start(
        "shepr:pi".into(),
        "pi".into(),
        shepr_agent::agent::resume::AgentSessionRef::path(session_b.clone()),
        Some(11),
        Some("new"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_b.clone()),
        Some(12),
    );

    let resumed = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi".into(),
        "pi".into(),
        shepr_agent::agent::resume::AgentSessionRef::path(session_a.clone()),
        Some(13),
        Some("resume"),
    );
    let working = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_a.clone()),
        Some(14),
    );

    assert!(resumed.is_some());
    assert!(working.is_some());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .expect("test precondition")
            .session_ref,
        shepr_agent::agent::resume::AgentSessionRef::path(session_a)
    );

    let late_session_b = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_b),
        Some(15),
    );
    assert!(late_session_b.is_none());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn pi_startup_adopts_persisted_session_without_live_authority() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("pi-startup-old.jsonl");
    let new_session = test_session_path("pi-startup-new.jsonl");
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:pi".into(),
        agent: shepr_agent::agent::Agent::Pi,
        session_ref: shepr_agent::agent::resume::AgentSessionRef::path(old_session)
            .expect("test session path should be valid"),
    });

    let startup = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi".into(),
        "pi".into(),
        shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(11),
        Some("startup"),
    );

    assert!(startup.is_some());
    assert_eq!(
        terminal.current_session_identity_for_persistence(),
        Some(
            shepr_agent::agent::resume::PersistedAgentSession::from_report(
                "shepr:pi",
                "pi",
                shepr_agent::agent::resume::AgentSessionRef::path(new_session)
                    .expect("test session path should be valid"),
            )
            .expect("test session identity should be valid")
        )
    );
}

#[test]
fn pi_non_replacement_reports_preserve_full_lifecycle_authority() {
    for reason in [None, Some("reload"), Some("startup")] {
        let mut terminal = test_terminal();
        let old_session = test_session_path("pi-current.jsonl");
        let new_session = test_session_path("pi-unexpected.jsonl");
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        anchor_full_lifecycle_session(
            &mut terminal,
            Agent::Pi,
            "shepr:pi",
            "pi",
            shepr_agent::agent::resume::AgentSessionRef::path(old_session.clone())
                .expect("test precondition"),
        );
        terminal.set_hook_authority_with_session_ref(
            "shepr:pi".into(),
            "pi".into(),
            AgentState::Idle,
            None,
            shepr_agent::agent::resume::AgentSessionRef::path(old_session.clone()),
            Some(10),
        );

        let session_report = terminal.set_agent_session_ref_for_session_start(
            "shepr:pi".into(),
            "pi".into(),
            shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
            Some(11),
            reason,
        );
        let working = terminal.set_hook_authority_with_session_ref(
            "shepr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            shepr_agent::agent::resume::AgentSessionRef::path(new_session),
            Some(12),
        );

        assert!(session_report.is_none());
        assert!(working.is_none());
        assert_eq!(terminal.state, AgentState::Idle);
        assert_eq!(
            terminal
                .hook_authority
                .as_ref()
                .expect("test precondition")
                .session_ref,
            shepr_agent::agent::resume::AgentSessionRef::path(old_session),
            "{reason:?} must not replace the current Pi session"
        );
    }
}

#[test]
fn omp_resume_session_report_reanchors_full_lifecycle_authority() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("omp-old.jsonl");
    let new_session = test_session_path("omp-new.jsonl");
    terminal.set_detected_state(Some(Agent::Omp), AgentState::Idle);
    terminal.set_hook_authority_with_session_ref(
        "shepr:omp".into(),
        "omp".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(old_session.clone()),
        Some(10),
    );

    let session_report = terminal.set_agent_session_ref_for_session_start(
        "shepr:omp".into(),
        "omp".into(),
        shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(11),
        Some("resume"),
    );

    assert!(session_report.is_some());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .expect("test precondition")
            .session_ref,
        shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone())
            .expect("test precondition")
    );

    let blocked = terminal.set_hook_authority_with_session_ref(
        "shepr:omp".into(),
        "omp".into(),
        AgentState::Blocked,
        Some("waiting".to_string()),
        shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(12),
    );

    assert!(blocked.is_some());
    assert_eq!(terminal.state, AgentState::Blocked);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .expect("test precondition")
            .session_ref,
        shepr_agent::agent::resume::AgentSessionRef::path(new_session)
    );

    let stale = terminal.set_hook_authority_with_session_ref(
        "shepr:omp".into(),
        "omp".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(old_session),
        Some(13),
    );

    assert!(stale.is_none());
    assert_eq!(terminal.state, AgentState::Blocked);
}

#[test]
fn late_full_lifecycle_hook_with_same_session_after_process_exit_does_not_reacquire_authority() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    let session_path = test_session_path("pi.jsonl");
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(20),
    );

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );
    let late = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path),
        Some(21),
    );

    assert!(late.is_none());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn live_full_lifecycle_hook_rejects_different_session_ref_for_same_source() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("one.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("one.jsonl")),
        Some(20),
    );

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("two.jsonl")),
        Some(21),
    );

    assert!(mutation.is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(shepr_agent::agent::resume::AgentSessionRef::value_str),
        Some(test_session_path("one.jsonl").as_str())
    );
}

#[test]
fn fresh_detected_process_keeps_old_session_suppressed_after_process_exit() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("old-process-exit.jsonl");
    let new_session = test_session_path("new-process-exit.jsonl");
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(old_session.clone()),
        Some(1000),
    );
    let process_exit_seen_at = Instant::now() + Duration::from_secs(1);
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        process_exit_seen_at,
    );

    let fresh_process_seen_at = process_exit_seen_at + Duration::from_millis(1);
    terminal.set_detected_state_with_screen_signals_at(
        None,
        AgentState::Unknown,
        false,
        false,
        fresh_process_seen_at,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Unknown,
        false,
        false,
        fresh_process_seen_at + Duration::from_millis(1),
    );

    let late_old = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(old_session),
        Some(500),
    );
    let fresh_new = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(501),
    );

    assert!(late_old.is_none());
    assert!(fresh_new.is_none());
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:pi".into(),
            "pi".into(),
            shepr_agent::agent::resume::AgentSessionRef::path(new_session),
            Some(400),
            Some("startup"),
        )
        .expect("fresh session should activate the buffered report");
    assert!(terminal.hook_authority.is_some());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn rapid_restart_replays_reports_that_arrive_before_process_evidence() {
    let mut terminal = test_terminal();
    let session_path = test_session_path("reports-before-process-evidence.jsonl");
    let now = Instant::now();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(1000),
        now,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let lower_sequence = terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(1001),
        now + Duration::from_millis(2),
    );
    let missing_sequence = terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        None,
        now + Duration::from_millis(3),
    );
    let buffered_working = terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(2001),
        now + Duration::from_millis(4),
    );
    let startup = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi".into(),
        "pi".into(),
        shepr_agent::agent::resume::AgentSessionRef::path(session_path),
        Some(2000),
        Some("startup"),
    );
    assert!(startup.is_none());
    assert!(lower_sequence.is_none());
    assert!(missing_sequence.is_none());
    assert!(buffered_working.is_none());
    assert!(!terminal.full_lifecycle_hook_authority_active());

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        false,
        now + Duration::from_millis(5),
    );

    assert!(terminal.full_lifecycle_hook_authority_active());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn process_exit_discards_unclaimed_buffered_state_from_that_generation() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("buffered-exit-old.jsonl");
    let shared_session = test_session_path("buffered-exit-shared.jsonl");
    let now = Instant::now();
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(old_session.clone())
            .expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(old_session),
        Some(1000),
        now,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        false,
        now + Duration::from_millis(2),
    );
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(shared_session.clone()),
        Some(500),
        now + Duration::from_millis(3),
    );

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(4),
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        false,
        now + Duration::from_millis(5),
    );
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:pi".into(),
            "pi".into(),
            shepr_agent::agent::resume::AgentSessionRef::path(shared_session),
            Some(100),
            Some("startup"),
        )
        .expect("new generation session claim");

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn queued_fresh_process_evidence_uses_process_exit_observation_time() {
    let mut terminal = test_terminal();
    let session_path = test_session_path("queued-after-process-exit.jsonl");
    let process_exit_at = Instant::now() - Duration::from_secs(1);
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(1000),
        process_exit_at - Duration::from_millis(1),
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        process_exit_at,
    );

    terminal.set_detected_state_with_screen_signals_at(
        None,
        AgentState::Unknown,
        false,
        false,
        process_exit_at + Duration::from_millis(1),
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        false,
        process_exit_at + Duration::from_millis(2),
    );
    let startup = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi".into(),
        "pi".into(),
        shepr_agent::agent::resume::AgentSessionRef::path(session_path),
        Some(2000),
        Some("startup"),
    );

    assert!(startup.is_some());
}

#[test]
fn different_session_after_process_exit_waits_for_fresh_process_evidence() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("old-before-process-exit.jsonl");
    let new_session = test_session_path("new-after-process-exit.jsonl");
    let now = Instant::now();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(old_session),
        Some(1000),
        now,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let early_new = terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(500),
        now + Duration::from_millis(2),
    );

    assert!(early_new.is_none());
    assert!(terminal.hook_authority.is_none());

    terminal.set_detected_state_with_screen_signals_at(
        None,
        AgentState::Unknown,
        false,
        false,
        now + Duration::from_millis(3),
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Unknown,
        false,
        false,
        now + Duration::from_millis(4),
    );
    let fresh_new = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi".into(),
        "pi".into(),
        shepr_agent::agent::resume::AgentSessionRef::path(new_session),
        Some(400),
        Some("startup"),
    );

    assert!(fresh_new.is_some());
    assert!(terminal.hook_authority.is_some());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn missing_session_after_process_exit_waits_for_fresh_process_evidence() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("old-before-nosession-process-exit.jsonl");
    let now = Instant::now();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(old_session),
        Some(1000),
        now,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let early_without_session = terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        Some(500),
        now + Duration::from_millis(2),
    );

    assert!(early_without_session.is_none());
    assert!(terminal.hook_authority.is_none());

    terminal.set_detected_state_with_screen_signals_at(
        None,
        AgentState::Unknown,
        false,
        false,
        now + Duration::from_millis(3),
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Unknown,
        false,
        false,
        now + Duration::from_millis(4),
    );
    let fresh_without_session = terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        Some(500),
        now + Duration::from_millis(5),
    );
    assert!(fresh_without_session.is_none());
    assert!(terminal.hook_authority.is_none());

    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:pi".into(),
            "pi".into(),
            shepr_agent::agent::resume::AgentSessionRef::path(test_session_path(
                "fresh-after-nosession-process-exit.jsonl",
            )),
            Some(600),
            Some("startup"),
        )
        .expect("fresh root session should claim the process generation");
    let child_update = terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        Some(601),
        now + Duration::from_millis(6),
    );

    assert!(child_update.is_some());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn mastracode_session_start_replaces_current_root_session() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Mastracode), AgentState::Idle);
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:mastracode".into(),
            "mastracode".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("mastracode-old"),
            Some(20),
            Some("startup"),
        )
        .expect("initial root session");

    let replacement = terminal.set_agent_session_ref_for_session_start(
        "shepr:mastracode".into(),
        "mastracode".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("mastracode-new"),
        Some(21),
        Some("startup"),
    );

    assert!(replacement.is_some_and(|mutation| mutation.session_ref_changed));
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("mastracode-new")
    );
}

#[test]
fn omp_reacquires_full_lifecycle_hook_after_process_exit_with_fresh_process_and_session_ref() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Omp), AgentState::Idle);
    terminal.set_hook_authority_at(
        "shepr:omp".into(),
        "omp".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("omp-old"),
        Some(1000),
        now,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Omp),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let stale = terminal.set_hook_authority_with_session_ref(
        "shepr:omp".into(),
        "omp".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("omp-old"),
        Some(500),
    );
    assert!(stale.is_none());
    assert!(terminal.hook_authority.is_none());

    terminal.set_detected_state_with_screen_signals_at(
        None,
        AgentState::Unknown,
        false,
        false,
        now + Duration::from_millis(2),
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Omp),
        AgentState::Unknown,
        false,
        false,
        now + Duration::from_millis(3),
    );
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:omp".into(),
            "omp".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("omp-new"),
            Some(400),
            Some("startup"),
        )
        .expect("fresh process and session should claim the pane");
    let fresh = terminal.set_hook_authority_with_session_ref(
        "shepr:omp".into(),
        "omp".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("omp-new"),
        Some(500),
    );

    assert!(fresh.is_some());
    assert!(terminal.hook_authority.is_some());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn visible_blocker_overrides_non_blocked_hook_for_same_agent() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
    terminal.set_hook_authority(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Working,
        None,
        None,
    );

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Blocked,
        true,
        false,
        false,
    );

    assert_eq!(terminal.fallback_state, AgentState::Blocked);
    assert_eq!(terminal.state, AgentState::Blocked);
    assert_eq!(
        change.expect("test precondition").previous_state,
        AgentState::Working
    );
}

#[test]
fn visible_blocker_does_not_override_full_lifecycle_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
    );

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Pi),
        AgentState::Blocked,
        true,
        false,
        false,
    );

    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn weak_blocked_fallback_does_not_override_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
    terminal.set_hook_authority(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Working,
        None,
        None,
    );

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Blocked,
        false,
        false,
        false,
    );

    assert_eq!(terminal.fallback_state, AgentState::Blocked);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn hook_blocked_wins_over_visible_blocker() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
    terminal.set_hook_authority(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Blocked,
        None,
        None,
    );

    terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Blocked,
        true,
        false,
        false,
    );

    assert_eq!(terminal.state, AgentState::Blocked);
    assert!(terminal.hook_authority.is_some());
}

#[test]
fn visible_blocker_does_not_override_different_agent_hook() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(None, AgentState::Unknown);
    terminal.set_hook_authority(
        "custom:agent".into(),
        "custom-agent".into(),
        AgentState::Working,
        None,
        None,
    );

    terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Blocked,
        true,
        false,
        false,
    );

    assert_eq!(terminal.effective_agent_label(), Some("custom-agent"));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn fallback_idle_does_not_override_hook_working() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
    terminal.set_hook_authority_at(
        "shepr:claude".into(),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        None,
        now,
    );

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Idle,
        false,
        false,
        now + Duration::from_secs(10),
    );

    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn fallback_idle_does_not_override_full_lifecycle_hook_working() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        "opencode",
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-root")
            .expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Working,
        None,
        None,
        None,
        now,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::OpenCode),
        AgentState::Idle,
        false,
        false,
        now + Duration::from_secs(10),
    );

    assert_eq!(terminal.fallback_state, AgentState::Working);
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn visible_working_does_not_override_hook_idle_for_same_agent() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
    terminal.set_hook_authority_at(
        "shepr:claude".into(),
        "claude".into(),
        AgentState::Idle,
        None,
        None,
        None,
        now,
    );

    let change = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        now + Duration::from_millis(1),
    );

    assert_eq!(terminal.fallback_state, AgentState::Working);
    assert_eq!(terminal.state, AgentState::Idle);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn visible_working_does_not_override_full_lifecycle_hook_idle() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Kimi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Kimi,
        "shepr:kimi",
        "kimi",
        shepr_agent::agent::resume::AgentSessionRef::id("kimi-root").expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:kimi".into(),
        "kimi".into(),
        AgentState::Idle,
        None,
        None,
        None,
        now,
    );

    let change = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Kimi),
        AgentState::Working,
        false,
        false,
        now + Duration::from_millis(1),
    );

    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Idle);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn detected_working_fallback_is_ignored_under_full_lifecycle_hook_authority() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Kilo), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Kilo,
        "shepr:kilo",
        "kilo",
        shepr_agent::agent::resume::AgentSessionRef::id("kilo-root").expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:kilo".into(),
        "kilo".into(),
        AgentState::Idle,
        None,
        None,
        None,
        now,
    );

    let change = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Kilo),
        AgentState::Working,
        false,
        false,
        now + Duration::from_millis(1),
    );

    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Idle);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn visible_working_does_not_hold_against_newer_claude_hook_idle() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        now,
    );

    let change = terminal.set_hook_authority_at(
        "shepr:claude".into(),
        "claude".into(),
        AgentState::Idle,
        None,
        None,
        None,
        now + Duration::from_millis(100),
    );

    assert_eq!(terminal.state, AgentState::Idle);
    assert_eq!(
        change
            .expect("test precondition")
            .effective_state_change
            .expect("test precondition")
            .previous_state,
        AgentState::Working
    );
}

#[test]
fn refreshed_visible_working_does_not_override_newer_hook_blocked() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Working,
        false,
        false,
        now,
    );
    terminal.set_hook_authority_at(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Blocked,
        None,
        None,
        None,
        now + Duration::from_millis(1201),
    );

    assert_eq!(terminal.state, AgentState::Blocked);

    let change = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Working,
        false,
        false,
        now + Duration::from_millis(2000),
    );

    assert_eq!(terminal.fallback_state, AgentState::Working);
    assert_eq!(terminal.state, AgentState::Blocked);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn fallback_idle_does_not_override_other_agent_hook_working() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
    terminal.set_hook_authority(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Working,
        None,
        None,
    );

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        true,
        false,
    );

    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn known_hook_authority_does_not_override_different_detected_agent() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Grok), AgentState::Working);
    let change = terminal.set_hook_authority(
        "shepr:claude".into(),
        "claude".into(),
        AgentState::Blocked,
        None,
        None,
    );

    assert!(change.is_none());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::Grok));
    assert_eq!(terminal.effective_agent_label(), Some("grok"));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn detected_agent_clears_conflicting_known_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_hook_authority(
        "shepr:claude".into(),
        "claude".into(),
        AgentState::Blocked,
        None,
        None,
    );

    terminal.set_detected_state(Some(Agent::Grok), AgentState::Working);

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::Grok));
    assert_eq!(terminal.effective_agent_label(), Some("grok"));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn border_label_prefers_manual_label_over_agent_label() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);

    assert_eq!(terminal.border_label(false), None);
    assert_eq!(terminal.border_label(true).as_deref(), Some("claude"));

    terminal.set_manual_label(" reviewer ".into());
    assert_eq!(terminal.border_label(false).as_deref(), Some("reviewer"));
    assert_eq!(terminal.border_label(true).as_deref(), Some("reviewer"));

    terminal.set_manual_label("   ".into());
    assert_eq!(terminal.border_label(true).as_deref(), Some("claude"));

    terminal.set_manual_label("reviewer".into());
    terminal.clear_manual_label();
    assert_eq!(terminal.border_label(true).as_deref(), Some("claude"));
}

#[test]
fn hook_authority_survives_unrelated_detected_agent_clear() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority(
        "shepr:custom".into(),
        "custom-agent".into(),
        AgentState::Working,
        None,
        None,
    );

    terminal.set_detected_state(None, AgentState::Unknown);

    assert!(terminal.hook_authority.is_some());
    assert_eq!(terminal.detected_agent, None);
    assert_eq!(terminal.effective_agent_label(), Some("custom-agent"));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn full_lifecycle_hook_authority_ignores_detected_agent_clear_without_process_exit() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        None,
        now,
    );

    let change = terminal.set_detected_state_with_screen_signals_at(
        None,
        AgentState::Unknown,
        false,
        false,
        now + Duration::from_millis(1),
    );

    assert!(terminal.hook_authority.is_some());
    assert_eq!(terminal.detected_agent, Some(Agent::Pi));
    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn detected_agent_clear_clears_matching_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Cursor), AgentState::Idle);
    terminal.set_hook_authority(
        "shepr:cursor".into(),
        "cursor".into(),
        AgentState::Idle,
        None,
        None,
    );

    terminal.set_detected_state(None, AgentState::Unknown);

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, None);
    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.effective_agent_label(), None);
    assert_eq!(terminal.state, AgentState::Unknown);
}

#[test]
fn detected_agent_clear_clears_matching_working_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
    terminal.set_hook_authority(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Working,
        None,
        None,
    );

    terminal.set_detected_state(None, AgentState::Unknown);

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, None);
    assert_eq!(terminal.effective_agent_label(), None);
    assert_eq!(terminal.state, AgentState::Unknown);
}

#[test]
fn process_exit_clears_matching_hook_authority_before_reporting_idle() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
    terminal.set_hook_authority(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Working,
        None,
        None,
    );

    terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        false,
        true,
    );

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::Codex));
    assert_eq!(terminal.effective_agent_label(), None);
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn stale_visible_screen_signal_does_not_override_newer_hook_authority() {
    let mut terminal = test_terminal();
    let observed = Instant::now();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        observed,
    );
    terminal.set_hook_authority_at(
        "shepr:claude".into(),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        Some(1),
        observed + Duration::from_secs(1),
    );

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Idle,
        false,
        false,
        observed,
    );

    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn stale_process_exit_preserves_newer_custom_authority() {
    let mut terminal = test_terminal();
    let observed = Instant::now();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        false,
        observed,
    );
    terminal.set_hook_authority_at(
        "custom:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        Some(100),
        observed + Duration::from_secs(1),
    );

    let mutation = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        observed,
    );

    assert!(!mutation.agent_released);
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .map(|hook| hook.source.as_str()),
        Some("custom:pi")
    );
}

#[test]
fn custom_authority_reanchors_sequence_after_process_restart() {
    let mut terminal = test_terminal();
    let observed = Instant::now();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority_at(
        "custom:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        Some(100),
        observed,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        observed + Duration::from_millis(1),
    );
    terminal.set_detected_state_with_screen_signals_at(
        None,
        AgentState::Unknown,
        false,
        false,
        observed + Duration::from_millis(2),
    );

    assert!(
        terminal
            .set_hook_authority(
                "custom:pi".into(),
                "pi".into(),
                AgentState::Working,
                None,
                Some(1),
            )
            .is_none()
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        false,
        observed + Duration::from_millis(3),
    );
    assert!(
        terminal
            .set_hook_authority(
                "custom:pi".into(),
                "pi".into(),
                AgentState::Working,
                None,
                Some(1),
            )
            .is_some()
    );
}

#[test]
fn process_exit_clears_newer_same_agent_hook_authority() {
    let mut terminal = test_terminal();
    let observed = Instant::now();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Working,
        false,
        false,
        observed,
    );
    terminal.set_hook_authority_at(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Working,
        None,
        None,
        Some(1),
        observed,
    );
    terminal.set_hook_authority_at(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Working,
        None,
        None,
        Some(2),
        observed + Duration::from_secs(1),
    );

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        true,
        observed,
    );

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
    assert_eq!(terminal.effective_agent_label(), None);
}

#[test]
fn detected_agent_change_clears_previous_matching_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
    terminal.set_hook_authority(
        "shepr:codex".into(),
        "codex".into(),
        AgentState::Idle,
        None,
        None,
    );

    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::OpenCode));
    assert_eq!(terminal.effective_agent_label(), Some("opencode"));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn stale_hook_report_sequence_is_ignored_for_same_source() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        Some(20),
    );

    let change = terminal.set_hook_authority(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        Some(19),
    );

    assert!(change.is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .expect("test precondition")
            .state,
        AgentState::Working
    );
}

#[test]
fn accepted_hook_report_stores_session_ref() {
    let mut terminal = test_terminal();
    let session_path = test_session_path("pi.jsonl");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone())
            .expect("test precondition"),
    );
    let mutation = terminal
        .set_hook_authority_with_session_ref(
            "shepr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
            Some(20),
        )
        .expect("accepted report");

    assert!(!mutation.session_ref_changed);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(|session_ref| (session_ref.kind(), session_ref.value_str())),
        Some((
            shepr_agent::agent::resume::AgentSessionRefKind::Path,
            session_path.as_str()
        ))
    );
}

#[test]
fn stale_hook_report_cannot_overwrite_session_ref() {
    let mut terminal = test_terminal();
    let session_path = test_session_path("pi.jsonl");
    let new_session_path = test_session_path("new.jsonl");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone())
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(20),
    );

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(new_session_path),
        Some(19),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(shepr_agent::agent::resume::AgentSessionRef::value_str),
        Some(session_path.as_str())
    );
}

#[test]
fn accepted_hook_report_without_session_ref_clears_previous_ref() {
    let mut terminal = test_terminal();
    let session_path = test_session_path("pi.jsonl");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(session_path.clone())
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path(session_path),
        Some(20),
    );

    let mutation = terminal
        .set_hook_authority_with_session_ref(
            "shepr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
            Some(21),
        )
        .expect("accepted report");

    assert!(mutation.session_ref_changed);
    assert!(mutation.effective_state_change.is_none());
    assert!(
        terminal
            .hook_authority
            .as_ref()
            .expect("test precondition")
            .session_ref
            .is_none()
    );
}

#[test]
fn different_same_agent_session_ref_is_ignored_until_current_session_clears() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:claude".into(),
            "claude".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_agent_session_ref(
        "shepr:claude".into(),
        "claude".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("nested-session"),
        Some(21),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal.hook_report_sequences.get("shepr:claude"),
        Some(&21)
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("claude-session")
    );
}

#[test]
fn claude_startup_session_ref_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:claude".into(),
            "claude".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:claude".into(),
        "claude".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("nested-session"),
        Some(21),
        Some("startup"),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("claude-session")
    );
}

#[test]
fn claude_lifecycle_session_ref_replaces_existing_session_ref() {
    for session_start_source in ["clear", "resume", "compact"] {
        let mut terminal = test_terminal();
        terminal
            .set_agent_session_ref(
                "shepr:claude".into(),
                "claude".into(),
                shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
                Some(20),
            )
            .expect("initial session should be accepted");

        let next_session = format!("{session_start_source}-session");
        let mutation = terminal
            .set_agent_session_ref_for_session_start(
                "shepr:claude".into(),
                "claude".into(),
                shepr_agent::agent::resume::AgentSessionRef::id(&next_session),
                Some(21),
                Some(session_start_source),
            )
            .unwrap_or_else(|| panic!("{session_start_source} should replace the session"));

        assert!(
            mutation.session_ref_changed,
            "{session_start_source} should mark the session changed"
        );
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| session.session_ref.value_str()),
            Some(next_session.as_str()),
            "{session_start_source} should store the replacement session"
        );
    }
}

#[test]
fn codex_lifecycle_session_ref_replaces_existing_session_ref() {
    for session_start_source in ["startup", "clear", "resume", "compact"] {
        let mut terminal = test_terminal();
        terminal
            .set_agent_session_ref(
                "shepr:codex".into(),
                "codex".into(),
                shepr_agent::agent::resume::AgentSessionRef::id("codex-session"),
                Some(20),
            )
            .expect("initial session should be accepted");

        let next_session = format!("codex-{session_start_source}-session");
        let mutation = terminal
            .set_agent_session_ref_for_session_start(
                "shepr:codex".into(),
                "codex".into(),
                shepr_agent::agent::resume::AgentSessionRef::id(&next_session),
                Some(21),
                Some(session_start_source),
            )
            .unwrap_or_else(|| panic!("{session_start_source} should replace the session"));

        assert!(mutation.session_ref_changed);
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| session.session_ref.value_str()),
            Some(next_session.as_str())
        );
    }
}

#[test]
fn grok_new_session_ref_replaces_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:grok".into(),
            "grok".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("grok-old"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let mutation = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:grok".into(),
            "grok".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("grok-new"),
            Some(21),
            Some("new"),
        )
        .expect("new should replace the grok session");

    assert!(mutation.session_ref_changed);
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("grok-new")
    );
}

#[test]
fn opencode_server_new_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode".into(),
            "opencode".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("opencode-visible"),
            None,
            Some("select"),
        )
        .expect("local selection should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode".into(),
        "opencode".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-attached-client"),
        Some(21),
        Some("new"),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("opencode-visible")
    );
}

#[test]
fn opencode_server_resume_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode".into(),
            "opencode".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("opencode-visible"),
            None,
            Some("select"),
        )
        .expect("local selection should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode".into(),
        "opencode".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-attached-client"),
        Some(21),
        Some("resume"),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("opencode-visible")
    );
}

#[test]
fn opencode_tui_selection_anchors_after_process_detection() {
    let mut terminal = test_terminal();
    let startup_selection = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode".into(),
        "opencode".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-startup-selection"),
        None,
        Some("select"),
    );
    assert!(startup_selection.is_none());
    assert_eq!(
        terminal
            .suppressed_full_lifecycle_hook_reports
            .get("shepr:opencode")
            .and_then(|suppressed| suppressed.replacement_session_ref.as_ref())
            .map(shepr_agent::agent::resume::AgentSessionRef::value_str),
        Some("opencode-startup-selection")
    );

    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("opencode-startup-selection")
    );
    assert!(
        !terminal
            .suppressed_full_lifecycle_hook_reports
            .contains_key("shepr:opencode")
    );

    terminal.suppressed_full_lifecycle_hook_reports.insert(
        "shepr:opencode".into(),
        SuppressedFullLifecycleHookReport {
            agent_label: "opencode".into(),
            session_ref: None,
            observed_at: Instant::now(),
            reason: FullLifecycleHookSuppressionReason::ProcessExit,
            replacement_session_ref: None,
            pending_replacement_report: None,
        },
    );
    let selected = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode".into(),
            "opencode".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("opencode-reselected"),
            None,
            Some("select"),
        )
        .expect("local TUI selection should reconcile generation suppression");

    assert!(selected.session_ref_changed);
    assert!(
        !terminal
            .suppressed_full_lifecycle_hook_reports
            .contains_key("shepr:opencode")
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("opencode-reselected")
    );
    assert!(
        !terminal
            .hook_report_sequences
            .contains_key("shepr:opencode")
    );
}

#[test]
fn opencode_child_prompt_reports_with_root_id_preserve_lifecycle_authority() {
    let mut terminal = test_terminal();
    let root = shepr_agent::agent::resume::AgentSessionRef::id("opencode-root")
        .expect("test precondition");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        "opencode",
        root.clone(),
    );

    // The plugin projects child permission/question events onto their root.
    for (seq, state) in [
        (20, AgentState::Working),
        (21, AgentState::Blocked),
        (22, AgentState::Working),
        (23, AgentState::Idle),
    ] {
        let mutation = terminal
            .set_hook_authority_with_session_ref(
                "shepr:opencode".into(),
                "opencode".into(),
                state,
                None,
                Some(root.clone()),
                Some(seq),
            )
            .expect("root-scoped lifecycle report should be accepted");
        assert!(!mutation.session_ref_changed);
        assert_eq!(terminal.state, state);
        assert_eq!(
            terminal
                .hook_authority
                .as_ref()
                .expect("test precondition")
                .session_ref
                .as_ref(),
            Some(&root)
        );
    }

    let foreign_child_prompt = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Blocked,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-other-root"),
        Some(24),
    );
    assert!(foreign_child_prompt.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn opencode_tui_selection_reanchors_full_lifecycle_authority() {
    let mut terminal = test_terminal();
    let old_session = shepr_agent::agent::resume::AgentSessionRef::id("opencode-newer")
        .expect("test precondition");
    let selected_session =
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-selected-older")
            .expect("test precondition");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        "opencode",
        old_session.clone(),
    );
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:opencode".into(),
            "opencode".into(),
            AgentState::Idle,
            None,
            Some(old_session.clone()),
            Some(20),
        )
        .expect("initial session should own lifecycle state");
    let attached_session =
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-attached-client")
            .expect("test precondition");
    let attached = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Working,
        None,
        Some(attached_session.clone()),
        Some(21),
    );
    assert!(attached.is_none());
    assert!(
        !terminal
            .suppressed_full_lifecycle_hook_reports
            .contains_key("shepr:opencode")
    );

    let selected = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode".into(),
            "opencode".into(),
            Some(selected_session.clone()),
            None,
            Some("select"),
        )
        .expect("selected session should replace the previous session");

    assert!(selected.session_ref_changed);
    assert!(terminal.hook_authority.is_none());
    assert!(
        !terminal
            .suppressed_full_lifecycle_hook_reports
            .contains_key("shepr:opencode")
    );
    assert_eq!(
        terminal.hook_report_sequences.get("shepr:opencode"),
        Some(&20)
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| &session.session_ref),
        Some(&selected_session)
    );

    terminal
        .set_hook_authority_with_session_ref(
            "shepr:opencode".into(),
            "opencode".into(),
            AgentState::Working,
            None,
            Some(selected_session.clone()),
            Some(21),
        )
        .expect("selected session should regain lifecycle authority");
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref()),
        Some(&selected_session)
    );

    let late_old_session = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Idle,
        None,
        Some(old_session),
        Some(22),
    );
    assert!(late_old_session.is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref()),
        Some(&selected_session)
    );

    let late_attached_session = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Blocked,
        None,
        Some(attached_session),
        Some(23),
    );
    assert!(late_attached_session.is_none());
    assert_eq!(terminal.state, AgentState::Working);

    let final_session = shepr_agent::agent::resume::AgentSessionRef::id("opencode-final-selection")
        .expect("test precondition");
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode".into(),
            "opencode".into(),
            Some(final_session.clone()),
            None,
            Some("select"),
        )
        .expect("another local selection should remain authoritative");
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| &session.session_ref),
        Some(&final_session)
    );
    assert!(
        !terminal
            .suppressed_full_lifecycle_hook_reports
            .contains_key("shepr:opencode")
    );
}

#[test]
fn opencode_session_ref_without_start_source_does_not_replace_existing() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode".into(),
            "opencode".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("opencode-old"),
            None,
            Some("select"),
        )
        .expect("local selection should be accepted");

    // session.updated reports carry no session_start_source, so a different
    // id must not displace the established session (cross-talk guard).
    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode".into(),
        "opencode".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-other"),
        Some(21),
        None,
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("opencode-old")
    );
}

#[test]
fn different_owner_session_ref_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:droid".into(),
            "droid".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("droid-session"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:claude".into(),
        "claude".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
        Some(21),
        Some("resume"),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal.persisted_agent_session.as_ref().map(|session| (
            session.source.as_str(),
            session.agent.label(),
            session.session_ref.value_str()
        )),
        Some(("shepr:droid", "droid", "droid-session"))
    );
}

#[test]
fn grok_new_session_does_not_replace_a_different_owner() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:claude".into(),
        agent: shepr_agent::agent::Agent::Claude,
        session_ref: shepr_agent::agent::resume::AgentSessionRef::id("claude-session")
            .expect("test precondition"),
    });
    terminal.set_detected_state(Some(Agent::Grok), AgentState::Idle);

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:grok".into(),
        "grok".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("grok-session"),
        Some(21),
        Some("new"),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal.persisted_agent_session.as_ref().map(|session| (
            session.source.as_str(),
            session.agent.label(),
            session.session_ref.value_str()
        )),
        Some(("shepr:claude", "claude", "claude-session"))
    );
}

#[test]
fn foreground_agent_session_replaces_stale_different_owner_session_ref() {
    for session_start_source in ["resume", "startup"] {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
            source: "shepr:codex".into(),
            agent: shepr_agent::agent::Agent::Codex,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                .expect("test precondition"),
        });
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);

        let mutation = terminal
            .set_agent_session_ref_for_session_start(
                "shepr:claude".into(),
                "claude".into(),
                shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
                Some(21),
                Some(session_start_source),
            )
            .unwrap_or_else(|| panic!("{session_start_source} should replace stale codex session"));

        assert!(mutation.session_ref_changed);
        assert_eq!(
            terminal.persisted_agent_session.as_ref().map(|session| (
                session.source.as_str(),
                session.agent.label(),
                session.session_ref.value_str()
            )),
            Some(("shepr:claude", "claude", "claude-session")),
            "{session_start_source} should store claude session"
        );
    }
}

#[test]
fn foreground_agent_session_requires_lifecycle_source_to_replace_different_owner() {
    for session_start_source in [None, Some("other")] {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
            source: "shepr:codex".into(),
            agent: shepr_agent::agent::Agent::Codex,
            session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                .expect("test precondition"),
        });
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);

        let mutation = terminal.set_agent_session_ref_for_session_start(
            "shepr:claude".into(),
            "claude".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
            Some(21),
            session_start_source,
        );

        assert!(
            mutation.is_none(),
            "{session_start_source:?} should not replace"
        );
        assert_eq!(
            terminal.persisted_agent_session.as_ref().map(|session| (
                session.source.as_str(),
                session.agent.label(),
                session.session_ref.value_str()
            )),
            Some(("shepr:codex", "codex", "codex-session"))
        );
    }
}

#[test]
fn different_owner_session_ref_requires_matching_detected_agent() {
    for session_start_source in ["startup", "resume"] {
        for detected_agent in [None, Some(Agent::Codex)] {
            let mut terminal = test_terminal();
            terminal.set_persisted_agent_session(
                shepr_agent::agent::resume::PersistedAgentSession {
                    source: "shepr:codex".into(),
                    agent: shepr_agent::agent::Agent::Codex,
                    session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                        .expect("test precondition"),
                },
            );
            terminal.set_detected_state(detected_agent, AgentState::Idle);

            let mutation = terminal.set_agent_session_ref_for_session_start(
                "shepr:claude".into(),
                "claude".into(),
                shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
                Some(21),
                Some(session_start_source),
            );

            assert!(
                mutation.is_none(),
                "{session_start_source} with {detected_agent:?} should not replace"
            );
            assert_eq!(
                terminal.persisted_agent_session.as_ref().map(|session| (
                    session.source.as_str(),
                    session.agent.label(),
                    session.session_ref.value_str()
                )),
                Some(("shepr:codex", "codex", "codex-session"))
            );
        }
    }
}

#[test]
fn custom_session_report_does_not_replace_different_owner_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:codex".into(),
        agent: shepr_agent::agent::Agent::Codex,
        session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
            .expect("test precondition"),
    });
    terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "custom:claude".into(),
        "claude".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
        Some(21),
        Some("resume"),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal.persisted_agent_session.as_ref().map(|session| (
            session.source.as_str(),
            session.agent.label(),
            session.session_ref.value_str()
        )),
        Some(("shepr:codex", "codex", "codex-session"))
    );
}

#[test]
fn foreground_agent_session_replaces_stale_different_owner_hook_authority() {
    let mut terminal = test_terminal();
    let now = std::time::Instant::now();
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        "opencode",
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-session")
            .expect("test precondition"),
    );
    terminal
        .set_hook_authority_at(
            "shepr:opencode".into(),
            "opencode".into(),
            AgentState::Working,
            None,
            shepr_agent::agent::resume::AgentSessionRef::id("opencode-session"),
            Some(20),
            now + Duration::from_millis(1),
        )
        .expect("initial hook authority should be accepted");
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        false,
        now,
    );

    let mutation = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:codex".into(),
            "codex".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("codex-session"),
            Some(21),
            Some("startup"),
        )
        .expect("foreground codex should replace stale hook authority");

    assert!(mutation.session_ref_changed);
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal.current_session_identity_for_persistence(),
        Some(
            shepr_agent::agent::resume::PersistedAgentSession::from_report(
                "shepr:codex",
                "codex",
                shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test session ID should be valid"),
            )
            .expect("test session identity should be valid")
        )
    );
    let late_old_session = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-session"),
        Some(22),
    );
    assert!(late_old_session.is_none());

    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode".into(),
            "opencode".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("opencode-new-session"),
            None,
            Some("select"),
        )
        .expect("fresh local selection");
    let fresh_session = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-new-session"),
        Some(24),
    );
    assert!(fresh_session.is_some());
}

#[test]
fn different_owner_full_lifecycle_hook_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:droid".into(),
            "droid".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("droid-session"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::path("/tmp/pi-session.jsonl"),
        Some(21),
    );

    assert!(mutation.is_none());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal.persisted_agent_session.as_ref().map(|session| (
            session.source.as_str(),
            session.agent.label(),
            session.session_ref.value_str()
        )),
        Some(("shepr:droid", "droid", "droid-session"))
    );
}

#[test]
fn repeated_same_agent_session_ref_is_accepted_without_session_change() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:claude".into(),
            "claude".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let mutation = terminal
        .set_agent_session_ref(
            "shepr:claude".into(),
            "claude".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
            Some(21),
        )
        .expect("same session should be accepted");

    assert!(!mutation.session_ref_changed);
}

#[test]
fn hook_authority_rejects_state_from_a_different_session() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        "opencode",
        shepr_agent::agent::resume::AgentSessionRef::id("opencode-session")
            .expect("test precondition"),
    );
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:opencode".into(),
            "opencode".into(),
            AgentState::Working,
            None,
            shepr_agent::agent::resume::AgentSessionRef::id("opencode-session"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode".into(),
        "opencode".into(),
        AgentState::Blocked,
        Some("needs approval".into()),
        shepr_agent::agent::resume::AgentSessionRef::id("nested-session"),
        Some(21),
    );

    assert!(mutation.is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(shepr_agent::agent::resume::AgentSessionRef::value_str),
        Some("opencode-session")
    );
}

#[test]
fn detected_agent_clear_does_not_clear_current_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
    terminal
        .set_agent_session_ref(
            "shepr:claude".into(),
            "claude".into(),
            shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
        )
        .expect("initial session should be accepted");

    let clear = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);
    assert!(!clear.session_ref_changed);

    let mutation = terminal.set_agent_session_ref(
        "shepr:claude".into(),
        "claude".into(),
        shepr_agent::agent::resume::AgentSessionRef::id("new-session"),
        Some(21),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("claude-session")
    );
}

#[test]
fn launch_command_alone_does_not_make_a_terminal_an_agent() {
    let terminal = test_terminal().with_launch_argv(vec!["just".into(), "dev".into()]);

    assert!(!terminal.is_agent_terminal());
}

#[test]
fn process_exit_clears_matching_persisted_session_ref() {
    let mut terminal = test_terminal();
    let session_ref =
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("pi.jsonl"))
            .expect("test precondition");
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:pi".into(),
        agent: shepr_agent::agent::Agent::Pi,
        session_ref: session_ref.clone(),
    });
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);

    let mutation = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        std::time::Instant::now(),
    );

    assert!(mutation.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_none());

    let delayed =
        terminal.set_agent_session_ref("shepr:pi".into(), "pi".into(), Some(session_ref), Some(21));
    assert!(delayed.is_none());
    assert!(terminal.persisted_agent_session.is_none());
}

#[test]
fn process_exit_preserves_foreign_persisted_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:claude".into(),
        agent: shepr_agent::agent::Agent::Claude,
        session_ref: shepr_agent::agent::resume::AgentSessionRef::id("claude-session")
            .expect("test precondition"),
    });
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);

    let mutation = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        std::time::Instant::now(),
    );

    assert!(!mutation.session_ref_changed);
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref.value_str()),
        Some("claude-session")
    );
}

#[test]
fn detected_conflict_clears_live_hook_but_preserves_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_hook_authority_with_session_ref(
        "shepr:claude".into(),
        "claude".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("claude-session"),
        Some(20),
    );

    let mutation = terminal.set_detected_state_with_mutation(Some(Agent::Grok), AgentState::Idle);

    assert!(!mutation.session_ref_changed);
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal.persisted_agent_session.as_ref().map(|session| (
            session.source.as_str(),
            session.agent.label(),
            session.session_ref.value_str()
        )),
        Some(("shepr:claude", "claude", "claude-session"))
    );
}

#[test]
fn detected_agent_disappearance_does_not_clear_full_lifecycle_hook_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Kimi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Kimi,
        "shepr:kimi",
        "kimi",
        shepr_agent::agent::resume::AgentSessionRef::id("kimi-session").expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:kimi".into(),
        "kimi".into(),
        AgentState::Working,
        None,
        shepr_agent::agent::resume::AgentSessionRef::id("kimi-session"),
        Some(20),
    );

    let mutation = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);

    assert!(!mutation.session_ref_changed);
    assert!(terminal.hook_authority.is_some());
    assert!(terminal.persisted_agent_session.is_none());
    assert_eq!(terminal.effective_agent_label(), Some("kimi"));
}

#[test]
fn detected_agent_disappearance_preserves_matching_persisted_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:opencode".into(),
        agent: shepr_agent::agent::Agent::OpenCode,
        session_ref: shepr_agent::agent::resume::AgentSessionRef::id("opencode-session")
            .expect("test precondition"),
    });

    let first = terminal.set_detected_state_with_mutation(Some(Agent::OpenCode), AgentState::Idle);
    assert!(!first.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_some());

    let second = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);
    assert!(!second.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_some());
}

#[test]
fn initial_unknown_detection_preserves_restored_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(shepr_agent::agent::resume::PersistedAgentSession {
        source: "shepr:codex".into(),
        agent: shepr_agent::agent::Agent::Codex,
        session_ref: shepr_agent::agent::resume::AgentSessionRef::id("codex-session")
            .expect("test precondition"),
    });

    let mutation = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);
    assert!(!mutation.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_some());
}

#[test]
fn unsequenced_hook_report_is_ignored_after_source_uses_sequence() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        "pi",
        shepr_agent::agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        Some(20),
    );

    let change =
        terminal.set_hook_authority("shepr:pi".into(), "pi".into(), AgentState::Idle, None, None);

    assert!(change.is_none());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn same_sequence_from_different_sources_is_independent() {
    let mut terminal = test_terminal();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority(
        "shepr:pi".into(),
        "pi".into(),
        AgentState::Working,
        None,
        Some(20),
    );

    terminal.set_hook_authority(
        "custom:pi".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        Some(19),
    );

    assert_eq!(terminal.state, AgentState::Idle);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .expect("test precondition")
            .source,
        "custom:pi"
    );
}

/// A pane's hook ordering marks stay bounded: once the source cap is reached,
/// marks that protect nothing current are dropped for a new source, the two
/// maps stay in step, and a source that is still protected keeps its mark.
#[test]
fn hook_report_sources_are_capped_and_the_sequence_maps_stay_in_step() {
    let mut terminal = test_terminal();
    let now = Instant::now();
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_hook_authority(
        "custom:kept".into(),
        "pi".into(),
        AgentState::Idle,
        None,
        Some(1),
    );
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .map(|authority| authority.source.as_str()),
        Some("custom:kept"),
        "test precondition"
    );
    assert!(terminal.hook_report_sequences.contains_key("custom:kept"));
    for index in 1..MAX_HOOK_REPORT_SOURCES {
        assert!(terminal.accept_hook_report_at(&format!("custom:{index}"), Some(1), now));
    }
    assert_eq!(
        terminal.hook_report_sequences.len(),
        MAX_HOOK_REPORT_SOURCES
    );

    assert!(terminal.accept_hook_report_at("custom:new", Some(1), now));
    assert!(terminal.hook_report_sequences.len() <= MAX_HOOK_REPORT_SOURCES);
    assert!(terminal.hook_report_sequences.contains_key("custom:kept"));
    assert!(terminal.hook_report_sequences.contains_key("custom:new"));
    let mut sequence_sources: Vec<_> = terminal.hook_report_sequences.keys().collect();
    let mut accepted_sources: Vec<_> = terminal.hook_report_accepted_at.keys().collect();
    sequence_sources.sort();
    accepted_sources.sort();
    assert_eq!(sequence_sources, accepted_sources);

    terminal.clear_hook_report_sequence("custom:new");
    assert!(!terminal.hook_report_sequences.contains_key("custom:new"));
    assert!(!terminal.hook_report_accepted_at.contains_key("custom:new"));
}

/// Stale session identities per official source keep only the newest ones.
#[test]
fn stale_full_lifecycle_sessions_are_capped_per_source() {
    let mut terminal = test_terminal();
    let total = MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE + 3;
    for index in 0..total {
        terminal.remember_stale_full_lifecycle_hook_session(
            "shepr:codex".into(),
            "codex".into(),
            shepr_agent::agent::resume::AgentSessionRef::id(format!("session-{index}"))
                .expect("test precondition"),
        );
    }
    let sessions = &terminal.stale_full_lifecycle_hook_sessions["shepr:codex"];
    assert_eq!(
        sessions.len(),
        MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE
    );
    let newest = shepr_agent::agent::resume::AgentSessionRef::id(format!("session-{}", total - 1))
        .expect("test precondition");
    let oldest_kept = shepr_agent::agent::resume::AgentSessionRef::id(format!(
        "session-{}",
        total - MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE
    ))
    .expect("test precondition");
    assert!(
        sessions
            .last()
            .is_some_and(|last| last.session_ref == newest)
    );
    assert!(
        sessions
            .first()
            .is_some_and(|first| first.session_ref == oldest_kept)
    );
}
