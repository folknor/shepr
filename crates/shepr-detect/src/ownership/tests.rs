use super::*;
use shepr_agent::resume::PersistedAgentSession;
use std::time::Duration;

std::thread_local! {
    static TEST_SESSION_ROOT: shepr_test_support::ScratchDir =
        shepr_test_support::ScratchDir::new("terminal-state-session-paths");
}

fn test_terminal() -> AgentOwnership {
    AgentOwnership::new()
}

fn bundled_source(value: &str) -> AgentSource {
    AgentSource::parse(value).expect("bundled integration source")
}

fn set_codex_hook_state(terminal: &mut AgentOwnership, state: AgentState) {
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:codex",
            state,
            shepr_agent::resume::AgentSessionRef::id("codex-session"),
            None,
            Instant::now(),
        )
        .expect("Codex state reports require a session id");
}

/// The session reference the Pi tests anchor their root session on.
fn pi_root_session_ref() -> Option<shepr_agent::resume::AgentSessionRef> {
    shepr_agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
}

#[test]
fn official_state_reports_require_the_session_reference_declared_by_the_descriptor() {
    for (agent, source) in [
        (Agent::Pi, "shepr:pi"),
        (Agent::Codex, "shepr:codex"),
        (Agent::Omp, "shepr:omp"),
        (Agent::Mastracode, "shepr:mastracode"),
        (Agent::OpenCode, "shepr:opencode"),
        (Agent::Kimi, "shepr:kimi"),
        (Agent::Kilo, "shepr:kilo"),
    ] {
        assert!(
            agent
                .descriptor()
                .hook_session_policy()
                .state_requires_session_ref
        );
        let mut terminal = test_terminal();
        assert!(
            terminal
                .set_hook_authority_at(source, AgentState::Working, None, None, Instant::now())
                .is_none()
        );
        assert_eq!(terminal.state, AgentState::Unknown);
        assert!(terminal.hook_authority.is_none());
    }
}

#[test]
fn ownership_test_seams_use_the_supplied_observation_time() {
    let mut terminal = test_terminal();
    let detected_at = Instant::now();
    let reported_at = detected_at + Duration::from_secs(1);
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Idle, detected_at);

    terminal
        .set_hook_authority_with_session_ref(
            "shepr:codex",
            AgentState::Working,
            shepr_agent::resume::AgentSessionRef::id("codex-session"),
            None,
            reported_at,
        )
        .expect("Codex state reports require a session id");

    assert_eq!(
        terminal
            .hook_authority()
            .map(|authority| authority.reported_at),
        Some(reported_at)
    );
}

fn test_session_path(name: &str) -> String {
    TEST_SESSION_ROOT.with(|root| root.join(name).display().to_string())
}

fn anchor_full_lifecycle_session(
    terminal: &mut AgentOwnership,
    agent: Agent,
    source: &str,
    session_ref: shepr_agent::resume::AgentSessionRef,
) {
    terminal.set_detected_state_at(Some(agent), terminal.fallback_state, Instant::now());
    terminal.set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            shepr_agent::AgentSource::parse(source).expect("bundled test source"),
            session_ref,
        )
        .expect("test precondition"),
    );
}

#[test]
fn hook_sequence_drops_stragglers_until_an_explicit_generation_reset() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(1_000), t0));
    for delay in [
        Duration::from_millis(50),
        Duration::from_secs(5),
        Duration::from_secs(10),
    ] {
        assert!(!terminal.accept_hook_report_at("shepr:kimi", Some(999), t0 + delay));
        assert!(!terminal.accept_hook_report_at("shepr:kimi", Some(1_000), t0 + delay));
    }
    assert!(terminal.accept_hook_report_at("shepr:pi", Some(5), t0));
    terminal.clear_hook_report_sequence("shepr:kimi");
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(10), t0 + Duration::from_secs(10)));
    assert!(!terminal.accept_hook_report_at("shepr:kimi", Some(10), t0 + Duration::from_secs(11)));
}

#[test]
fn hook_authority_overrides_fallback_for_same_agent() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        pi_root_session_ref(),
        None,
        Instant::now(),
    );

    assert_eq!(terminal.detected_agent, Some(Agent::Pi));
    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.effective_agent(), Some(Agent::Pi));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn process_exit_suppresses_only_full_lifecycle_sources() {
    for (agent, source, label, full_lifecycle) in [
        (Agent::Claude, "shepr:claude", "claude", false),
        (Agent::Codex, "shepr:codex", "codex", false),
        (Agent::Pi, "shepr:pi", "pi", true),
    ] {
        let mut terminal = test_terminal();
        let session_ref = if full_lifecycle {
            shepr_agent::resume::AgentSessionRef::path(test_session_path("exit.jsonl"))
        } else {
            shepr_agent::resume::AgentSessionRef::id(format!("{label}-exit"))
        }
        .expect("test precondition");
        anchor_full_lifecycle_session(&mut terminal, agent, source, session_ref);

        terminal.confirmed_detection_for_test(
            Some(agent),
            AgentState::Idle,
            false,
            true,
            Instant::now() + Duration::from_millis(1),
        );

        assert_eq!(
            terminal.suppressed_hook_source(source).is_some(),
            full_lifecycle,
            "{label}"
        );
    }
}

#[test]
fn omp_hook_authority_overrides_detected_fallback() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Omp), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Omp,
        "shepr:omp",
        shepr_agent::resume::AgentSessionRef::id("omp-root").expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:omp",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("omp-root"),
        None,
        Instant::now(),
    );

    assert_eq!(terminal.detected_agent, Some(Agent::Omp));
    assert_eq!(terminal.effective_agent(), Some(Agent::Omp));
    assert_eq!(terminal.state, AgentState::Working);

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Omp),
        AgentState::Blocked,
        true,
        false,
        Instant::now(),
    );

    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn session_only_report_does_not_create_hook_authority() {
    for (agent, source, session_id) in [
        (Agent::Codex, "shepr:codex", "codex-session"),
        (Agent::Devin, "shepr:devin", "devin-session"),
    ] {
        let mut terminal = test_terminal();
        terminal.set_detected_state_at(Some(agent), AgentState::Idle, Instant::now());

        let mutation = terminal.set_agent_session_ref(
            source,
            shepr_agent::resume::AgentSessionRef::id(session_id),
            Some(1),
            Instant::now(),
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
fn session_only_state_reports_keep_identity_without_owning_state() {
    use shepr_agent::{ReportOrigin, resume::AgentSessionRef};

    for agent in [
        Agent::Claude,
        Agent::Cursor,
        Agent::Devin,
        Agent::GithubCopilot,
        Agent::Droid,
        Agent::Grok,
        Agent::Antigravity,
    ] {
        let mut terminal = test_terminal();
        // clock-io-ok: injected report and detector observation times.
        let now = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Idle,
            false,
            false,
            now,
        );
        let origin = ReportOrigin::official(agent).expect("session-only integration");
        let session_ref = AgentSessionRef::id("current-session").expect("session id");
        let mutation = terminal
            .set_hook_report_at(
                origin,
                AgentState::Blocked,
                Some(session_ref.clone()),
                Some(10),
                (now + Duration::from_millis(1)).into(),
            )
            .expect("a state report still contributes identity evidence");
        assert!(mutation.session_ref_changed, "{agent}");
        assert!(terminal.hook_authority().is_none(), "{agent}");
        assert_eq!(terminal.state, AgentState::Idle, "{agent}");
        assert_eq!(
            terminal
                .current_session_identity_for_persistence()
                .expect("identity")
                .session_ref()
                .clone(),
            session_ref,
            "{agent}"
        );

        let sources = terminal.hook_sources.clone();
        for (incoming, seq) in [(None, 11), (Some(session_ref.clone()), 9)] {
            assert!(
                terminal
                    .set_hook_report_at(
                        origin,
                        AgentState::Working,
                        incoming,
                        Some(seq),
                        (now + Duration::from_millis(2)).into(),
                    )
                    .is_none(),
                "{agent}"
            );
            assert_eq!(terminal.hook_sources, sources, "{agent}");
            assert!(terminal.hook_authority().is_none(), "{agent}");
            assert_eq!(
                terminal
                    .current_session_identity_for_persistence()
                    .expect("identity")
                    .session_ref()
                    .clone(),
                session_ref,
                "{agent}"
            );
        }
    }
}

#[test]
fn startup_session_claim_activates_full_lifecycle_integrations() {
    for (agent, source, label) in [
        (Agent::Kimi, "shepr:kimi", "kimi"),
        (Agent::Kilo, "shepr:kilo", "kilo"),
    ] {
        let mut terminal = test_terminal();
        terminal.set_detected_state_at(Some(agent), AgentState::Idle, Instant::now());
        let session_ref = shepr_agent::resume::AgentSessionRef::id(format!("{label}-root"));

        let session = terminal.set_agent_session_ref_for_session_start(
            source,
            session_ref.clone(),
            Some(10),
            Some("startup"),
            Instant::now(),
        );
        let working = terminal.set_hook_authority_with_session_ref(
            source,
            AgentState::Working,
            session_ref,
            Some(11),
            Instant::now(),
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
    terminal.set_detected_state_at(Some(agent), AgentState::Idle, Instant::now());
    let first_ref = shepr_agent::resume::AgentSessionRef::id(format!("{label}-root"))
        .expect("test precondition");
    let first = terminal.set_agent_session_ref_for_session_start(
        source,
        Some(first_ref.clone()),
        Some(10),
        start_source,
        Instant::now(),
    );

    assert!(first.is_some(), "{label} should accept its session");
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(PersistedAgentSession::session_ref),
        Some(&first_ref)
    );

    terminal.set_detected_state_at(Some(agent), AgentState::Working, Instant::now());
    let replacement_ref = shepr_agent::resume::AgentSessionRef::id(format!("{label}-replacement"))
        .expect("test precondition");
    let replacement = terminal.set_agent_session_ref_for_session_start(
        source,
        Some(replacement_ref.clone()),
        Some(11),
        start_source,
        Instant::now(),
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
            .map(PersistedAgentSession::session_ref),
        Some(&replacement_ref)
    );

    let identity_from_state = terminal.set_hook_authority_with_session_ref(
        source,
        AgentState::Blocked,
        Some(replacement_ref.clone()),
        Some(12),
        Instant::now(),
    );
    assert!(identity_from_state.is_some_and(|mutation| {
        !mutation.session_ref_changed && mutation.effective_state_change.is_none()
    }));
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Working);

    terminal.set_detected_state_at(None, AgentState::Unknown, Instant::now());
    let background_ref = shepr_agent::resume::AgentSessionRef::id(format!("{label}-background"))
        .expect("test precondition");
    let background_replacement = terminal.set_agent_session_ref_for_session_start(
        source,
        Some(background_ref.clone()),
        Some(13),
        replacement_source,
        Instant::now(),
    );
    assert!(
        background_replacement.is_none(),
        "{label} should reject a background replacement"
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(PersistedAgentSession::session_ref),
        Some(&replacement_ref)
    );

    terminal.set_detected_state_at(Some(agent), AgentState::Idle, Instant::now());
    let retried_replacement = terminal.set_agent_session_ref_for_session_start(
        source,
        Some(background_ref.clone()),
        Some(14),
        replacement_source,
        Instant::now(),
    );
    assert!(
        retried_replacement.is_some_and(|mutation| mutation.session_ref_changed),
        "{label} should replace the session once detected"
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(PersistedAgentSession::session_ref),
        Some(&background_ref)
    );
}

#[test]
fn pi_session_replacement_reports_reanchor_full_lifecycle_authority() {
    for reason in ["new", "resume", "fork"] {
        let mut terminal = test_terminal();
        let old_session = test_session_path(&format!("pi-{reason}-old.jsonl"));
        let new_session = test_session_path(&format!("pi-{reason}-new.jsonl"));
        terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
        terminal.set_hook_authority_with_session_ref(
            "shepr:pi",
            AgentState::Idle,
            shepr_agent::resume::AgentSessionRef::path(old_session),
            Some(10),
            Instant::now(),
        );

        let session_report = terminal.set_agent_session_ref_for_session_start(
            "shepr:pi",
            shepr_agent::resume::AgentSessionRef::path(new_session.clone()),
            Some(11),
            Some(reason),
            Instant::now(),
        );

        assert!(
            session_report.is_some(),
            "{reason} should replace the previous Pi session"
        );
        assert!(terminal.hook_authority.is_none());

        let working = terminal.set_hook_authority_with_session_ref(
            "shepr:pi",
            AgentState::Working,
            shepr_agent::resume::AgentSessionRef::path(new_session.clone()),
            Some(12),
            Instant::now(),
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
            shepr_agent::resume::AgentSessionRef::path(new_session)
        );
    }
}

#[test]
fn pi_resume_reactivates_a_previously_stale_session() {
    let mut terminal = test_terminal();
    let session_a = test_session_path("pi-session-a.jsonl");
    let session_b = test_session_path("pi-session-b.jsonl");
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::path(session_a.clone()),
        Some(10),
        Instant::now(),
    );

    terminal.set_agent_session_ref_for_session_start(
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(session_b.clone()),
        Some(11),
        Some("new"),
        Instant::now(),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::path(session_b.clone()),
        Some(12),
        Instant::now(),
    );

    let resumed = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(session_a.clone()),
        Some(13),
        Some("resume"),
        Instant::now(),
    );
    let working = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_a.clone()),
        Some(14),
        Instant::now(),
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
        shepr_agent::resume::AgentSessionRef::path(session_a)
    );

    let late_session_b = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::path(session_b),
        Some(15),
        Instant::now(),
    );
    assert!(late_session_b.is_none());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn pi_startup_preserves_persisted_session_without_live_authority() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("pi-startup-old.jsonl");
    let new_session = test_session_path("pi-startup-new.jsonl");
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    terminal.set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            bundled_source("shepr:pi"),
            shepr_agent::resume::AgentSessionRef::path(old_session.clone())
                .expect("test session path should be valid"),
        )
        .expect("test session should be valid"),
    );

    let startup = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(new_session),
        Some(11),
        Some("startup"),
        Instant::now(),
    );

    // Held for a relaunch the detector might confirm, but never applied
    // without one.
    assert_eq!(startup, Some(AgentOwnershipMutation::default()));
    assert_eq!(
        terminal.current_session_identity_for_persistence(),
        Some(
            shepr_agent::resume::PersistedAgentSession::new(
                shepr_agent::AgentSource::parse("shepr:pi").expect("bundled test source"),
                shepr_agent::resume::AgentSessionRef::path(old_session)
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
        terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
        anchor_full_lifecycle_session(
            &mut terminal,
            Agent::Pi,
            "shepr:pi",
            shepr_agent::resume::AgentSessionRef::path(old_session.clone())
                .expect("test precondition"),
        );
        terminal.set_hook_authority_with_session_ref(
            "shepr:pi",
            AgentState::Idle,
            shepr_agent::resume::AgentSessionRef::path(old_session.clone()),
            Some(10),
            Instant::now(),
        );

        let session_report = terminal.set_agent_session_ref_for_session_start(
            "shepr:pi",
            shepr_agent::resume::AgentSessionRef::path(new_session.clone()),
            Some(11),
            reason,
            Instant::now(),
        );
        let working = terminal.set_hook_authority_with_session_ref(
            "shepr:pi",
            AgentState::Working,
            shepr_agent::resume::AgentSessionRef::path(new_session),
            Some(12),
            Instant::now(),
        );

        // A `startup` is held for a relaunch the detector might confirm
        // (parked, changing nothing); the other sources are refused.
        if reason == Some("startup") {
            assert_eq!(session_report, Some(AgentOwnershipMutation::default()));
        } else {
            assert!(session_report.is_none());
        }
        assert!(working.is_none());
        assert_eq!(terminal.state, AgentState::Idle);
        assert_eq!(
            terminal
                .hook_authority
                .as_ref()
                .expect("test precondition")
                .session_ref,
            shepr_agent::resume::AgentSessionRef::path(old_session),
            "{reason:?} must not replace the current Pi session"
        );
    }
}

#[test]
fn omp_resume_session_report_reanchors_full_lifecycle_authority() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("omp-old.jsonl");
    let new_session = test_session_path("omp-new.jsonl");
    terminal.set_detected_state_at(Some(Agent::Omp), AgentState::Idle, Instant::now());
    terminal.set_hook_authority_with_session_ref(
        "shepr:omp",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(old_session.clone()),
        Some(10),
        Instant::now(),
    );

    let session_report = terminal.set_agent_session_ref_for_session_start(
        "shepr:omp",
        shepr_agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(11),
        Some("resume"),
        Instant::now(),
    );

    assert!(session_report.is_some());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .expect("test precondition")
            .session_ref()
            .clone(),
        shepr_agent::resume::AgentSessionRef::path(new_session.clone()).expect("test precondition")
    );

    let blocked = terminal.set_hook_authority_with_session_ref(
        "shepr:omp",
        AgentState::Blocked,
        shepr_agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(12),
        Instant::now(),
    );

    assert!(blocked.is_some());
    assert_eq!(terminal.state, AgentState::Blocked);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .expect("test precondition")
            .session_ref,
        shepr_agent::resume::AgentSessionRef::path(new_session)
    );

    let stale = terminal.set_hook_authority_with_session_ref(
        "shepr:omp",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(old_session),
        Some(13),
        Instant::now(),
    );

    assert!(stale.is_none());
    assert_eq!(terminal.state, AgentState::Blocked);
}

#[test]
fn late_full_lifecycle_hook_with_same_session_after_process_exit_does_not_reacquire_authority() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    let session_path = test_session_path("pi.jsonl");
    terminal.set_detected_state_at(
        Some(Agent::Pi),
        AgentState::Working,
        now - Duration::from_millis(1),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(20),
        now,
    );

    terminal.confirmed_detection_for_test(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );
    let late = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path),
        Some(21),
        now + Duration::from_millis(2),
    );

    assert!(late.is_some_and(
        |mutation| mutation.effective_state_change.is_none() && !mutation.session_ref_changed
    ));
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn live_full_lifecycle_hook_rejects_different_session_ref_for_same_source() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(test_session_path("one.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(test_session_path("one.jsonl")),
        Some(20),
        Instant::now(),
    );

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::path(test_session_path("two.jsonl")),
        Some(21),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(shepr_agent::resume::AgentSessionRef::value_str),
        Some(test_session_path("one.jsonl").as_str())
    );

    // The stray report is cross-talk, not a replacement generation: the live
    // session's own next report is still accepted.
    let follow_up = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::path(test_session_path("one.jsonl")),
        Some(22),
        Instant::now(),
    );

    assert!(follow_up.is_some());
    assert_eq!(terminal.state, AgentState::Idle);
    assert!(terminal.suppressed_hook_source("shepr:pi").is_none());
}

#[test]
fn fresh_detected_process_keeps_old_session_suppressed_after_process_exit() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("old-process-exit.jsonl");
    let new_session = test_session_path("new-process-exit.jsonl");
    let initial_observation = Instant::now();
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, initial_observation);
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(old_session.clone()),
        Some(1000),
        initial_observation + Duration::from_millis(1),
    );
    let process_exit_seen_at = Instant::now() + Duration::from_secs(1);
    terminal.confirmed_detection_for_test(
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
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(old_session),
        Some(500),
        fresh_process_seen_at + Duration::from_millis(1),
    );
    let fresh_new = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(501),
        fresh_process_seen_at + Duration::from_millis(2),
    );

    assert!(late_old.is_some());
    assert!(fresh_new.is_some());
    // Both are parked for the session start, and neither takes authority
    // before it: the fresh process has not started a session yet.
    assert!(terminal.hook_authority.is_none());
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:pi",
            shepr_agent::resume::AgentSessionRef::path(new_session),
            Some(400),
            Some("startup"),
            fresh_process_seen_at + Duration::from_millis(3),
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
    terminal.set_detected_state_at(
        Some(Agent::Pi),
        AgentState::Idle,
        now - Duration::from_millis(1),
    );
    terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(1000),
        now,
    );
    terminal.confirmed_detection_for_test(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let lower_sequence = terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(1001),
        now + Duration::from_millis(2),
    );
    let missing_sequence = terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        None,
        now + Duration::from_millis(3),
    );
    let buffered_working = terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(2001),
        now + Duration::from_millis(4),
    );
    let startup = terminal.set_agent_session_ref_for_session_start(
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(session_path),
        Some(2000),
        Some("startup"),
        now + Duration::from_millis(4),
    );
    assert!(startup.is_some());
    assert!(lower_sequence.is_some());
    assert!(missing_sequence.is_none());
    assert!(buffered_working.is_some());
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
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(old_session.clone()).expect("test precondition"),
    );
    let now = Instant::now();
    terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(old_session),
        Some(1000),
        now,
    );
    terminal.confirmed_detection_for_test(
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
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(shared_session.clone()),
        Some(500),
        now + Duration::from_millis(3),
    );

    terminal.confirmed_detection_for_test(
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
            "shepr:pi",
            shepr_agent::resume::AgentSessionRef::path(shared_session),
            Some(100),
            Some("startup"),
            now + Duration::from_millis(6),
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
    terminal.set_detected_state_at(
        Some(Agent::Pi),
        AgentState::Idle,
        process_exit_at - Duration::from_millis(2),
    );
    terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(1000),
        process_exit_at - Duration::from_millis(1),
    );
    terminal.confirmed_detection_for_test(
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
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(session_path),
        Some(2000),
        Some("startup"),
        Instant::now(),
    );

    assert!(startup.is_some());
}

#[test]
fn different_session_after_process_exit_waits_for_fresh_process_evidence() {
    let mut terminal = test_terminal();
    let old_session = test_session_path("old-before-process-exit.jsonl");
    let new_session = test_session_path("new-after-process-exit.jsonl");
    let now = Instant::now();
    terminal.set_detected_state_at(
        Some(Agent::Pi),
        AgentState::Idle,
        now - Duration::from_millis(1),
    );
    terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(old_session),
        Some(1000),
        now,
    );
    terminal.confirmed_detection_for_test(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let early_new = terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(new_session.clone()),
        Some(500),
        now + Duration::from_millis(2),
    );

    assert!(early_new.is_some());
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
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(new_session),
        Some(400),
        Some("startup"),
        now + Duration::from_millis(5),
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
    terminal.set_detected_state_at(
        Some(Agent::Pi),
        AgentState::Idle,
        now - Duration::from_millis(1),
    );
    terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(old_session),
        Some(1000),
        now,
    );
    terminal.confirmed_detection_for_test(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let early_without_session = terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
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
        "shepr:pi",
        AgentState::Working,
        None,
        Some(500),
        now + Duration::from_millis(5),
    );
    assert!(fresh_without_session.is_none());
    assert!(terminal.hook_authority.is_none());

    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:pi",
            shepr_agent::resume::AgentSessionRef::path(test_session_path(
                "fresh-after-nosession-process-exit.jsonl",
            )),
            Some(600),
            Some("startup"),
            now + Duration::from_millis(5),
        )
        .expect("fresh root session should claim the process generation");
    let child_update = terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(test_session_path(
            "fresh-after-nosession-process-exit.jsonl",
        )),
        Some(601),
        now + Duration::from_millis(6),
    );

    assert!(child_update.is_some());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn mastracode_session_start_replaces_current_root_session() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Mastracode), AgentState::Idle, Instant::now());
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:mastracode",
            shepr_agent::resume::AgentSessionRef::id("mastracode-old"),
            Some(20),
            Some("startup"),
            Instant::now(),
        )
        .expect("initial root session");

    let replacement = terminal.set_agent_session_ref_for_session_start(
        "shepr:mastracode",
        shepr_agent::resume::AgentSessionRef::id("mastracode-new"),
        Some(21),
        Some("startup"),
        Instant::now(),
    );

    assert!(replacement.is_some_and(|mutation| mutation.session_ref_changed));
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("mastracode-new")
    );
}

#[test]
fn omp_reacquires_full_lifecycle_hook_after_process_exit_with_fresh_process_and_session_ref() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(
        Some(Agent::Omp),
        AgentState::Idle,
        now - Duration::from_millis(1),
    );
    terminal.set_hook_authority_at(
        "shepr:omp",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("omp-old"),
        Some(1000),
        now,
    );
    terminal.confirmed_detection_for_test(
        Some(Agent::Omp),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );

    let stale = terminal.set_hook_authority_with_session_ref(
        "shepr:omp",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("omp-old"),
        Some(500),
        now + Duration::from_millis(2),
    );
    assert!(stale.is_some_and(
        |mutation| mutation.effective_state_change.is_none() && !mutation.session_ref_changed
    ));
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
            "shepr:omp",
            shepr_agent::resume::AgentSessionRef::id("omp-new"),
            Some(400),
            Some("startup"),
            now + Duration::from_millis(4),
        )
        .expect("fresh process and session should claim the pane");
    let fresh = terminal.set_hook_authority_with_session_ref(
        "shepr:omp",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("omp-new"),
        Some(500),
        now + Duration::from_millis(5),
    );

    assert!(fresh.is_some());
    assert!(terminal.hook_authority.is_some());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn visible_blocker_overrides_non_blocked_hook_for_same_agent() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Idle, Instant::now());
    set_codex_hook_state(&mut terminal, AgentState::Working);

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Blocked,
        true,
        false,
        Instant::now(),
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
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        pi_root_session_ref(),
        None,
        Instant::now(),
    );

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Pi),
        AgentState::Blocked,
        true,
        false,
        Instant::now(),
    );

    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn weak_blocked_fallback_does_not_override_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Idle, Instant::now());
    set_codex_hook_state(&mut terminal, AgentState::Working);

    let change = terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Blocked,
        false,
        false,
        Instant::now(),
    );

    assert_eq!(terminal.fallback_state, AgentState::Blocked);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn hook_blocked_wins_over_visible_blocker() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Working, Instant::now());
    set_codex_hook_state(&mut terminal, AgentState::Blocked);

    terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Blocked,
        true,
        false,
        Instant::now(),
    );

    assert_eq!(terminal.state, AgentState::Blocked);
    assert!(terminal.hook_authority.is_some());
}

#[test]
fn fallback_idle_does_not_override_hook_working() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Working, Instant::now());
    terminal.set_hook_authority_at(
        "shepr:codex",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("codex-session"),
        None,
        now,
    );

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
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
    terminal.set_detected_state_at(Some(Agent::OpenCode), AgentState::Working, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        shepr_agent::resume::AgentSessionRef::id("opencode-root").expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:opencode",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("opencode-root"),
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

    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn detected_working_does_not_override_hook_idle_for_same_agent() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Idle, now);
    terminal.set_hook_authority_at(
        "shepr:codex",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::id("codex-session"),
        None,
        now,
    );

    let change = terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
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
fn detected_working_does_not_override_full_lifecycle_hook_idle() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Kimi), AgentState::Idle, now);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Kimi,
        "shepr:kimi",
        shepr_agent::resume::AgentSessionRef::id("kimi-root").expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:kimi",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::id("kimi-root"),
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

    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.state, AgentState::Idle);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn detected_working_fallback_is_ignored_under_full_lifecycle_hook_authority() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Kilo), AgentState::Idle, now);
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Kilo,
        "shepr:kilo",
        shepr_agent::resume::AgentSessionRef::id("kilo-root").expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:kilo",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::id("kilo-root"),
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

    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.state, AgentState::Idle);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn detected_working_does_not_hold_against_newer_hook_idle() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Working,
        false,
        false,
        now,
    );

    let change = terminal.set_hook_authority_at(
        "shepr:codex",
        AgentState::Idle,
        shepr_agent::resume::AgentSessionRef::id("codex-session"),
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
fn repeated_detected_working_does_not_override_newer_hook_blocked() {
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
        "shepr:codex",
        AgentState::Blocked,
        shepr_agent::resume::AgentSessionRef::id("codex-session"),
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
fn fallback_idle_does_not_override_hook_working_when_screen_agent_is_absent() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    // With no screen identity, the Codex hook is the only live owner; an Idle
    // fallback still must not mask its Working state.
    terminal.set_detected_state_at(None, AgentState::Idle, now);
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:codex",
            AgentState::Working,
            shepr_agent::resume::AgentSessionRef::id("codex-session"),
            None,
            now + Duration::from_millis(1),
        )
        .expect("Codex state reports require a session id");

    let change = terminal.set_detected_state_with_visible_blocker(
        None,
        AgentState::Idle,
        false,
        false,
        now + Duration::from_millis(2),
    );

    assert_eq!(terminal.fallback_state, AgentState::Idle);
    assert_eq!(terminal.effective_agent(), Some(Agent::Codex));
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.is_none());
}

#[test]
fn known_hook_authority_does_not_override_different_detected_agent() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Grok), AgentState::Working, Instant::now());
    let change = terminal.set_hook_authority_with_session_ref(
        "shepr:codex",
        AgentState::Blocked,
        shepr_agent::resume::AgentSessionRef::id("codex-session"),
        None,
        Instant::now(),
    );

    assert!(change.is_none());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::Grok));
    assert_eq!(terminal.effective_agent(), Some(Agent::Grok));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn detected_agent_clears_conflicting_known_hook_authority() {
    let mut terminal = test_terminal();
    set_codex_hook_state(&mut terminal, AgentState::Blocked);

    terminal.set_detected_state_at(Some(Agent::Grok), AgentState::Working, Instant::now());

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::Grok));
    assert_eq!(terminal.effective_agent(), Some(Agent::Grok));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn hook_authority_survives_a_detector_that_never_saw_its_agent() {
    let mut terminal = test_terminal();
    set_codex_hook_state(&mut terminal, AgentState::Working);

    terminal.set_detected_state_at(None, AgentState::Unknown, Instant::now());

    assert!(terminal.hook_authority.is_some());
    assert_eq!(terminal.detected_agent, None);
    assert_eq!(terminal.effective_agent(), Some(Agent::Codex));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn full_lifecycle_hook_authority_ignores_detected_agent_clear_without_process_exit() {
    let now = Instant::now();
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Working,
        pi_root_session_ref(),
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
    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert_eq!(terminal.state, AgentState::Working);
    assert!(change.effective_state_change.is_none());
}

#[test]
fn detected_agent_clear_clears_matching_hook_authority() {
    for state in [AgentState::Idle, AgentState::Working] {
        let mut terminal = test_terminal();
        terminal.set_detected_state_at(Some(Agent::Codex), state, Instant::now());
        set_codex_hook_state(&mut terminal, state);

        terminal.set_detected_state_at(None, AgentState::Unknown, Instant::now());

        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, None);
        assert_eq!(terminal.fallback_state, AgentState::Unknown);
        assert_eq!(terminal.effective_agent(), None);
        assert_eq!(terminal.state, AgentState::Unknown);
    }
}

#[test]
fn process_exit_clears_matching_hook_authority_before_reporting_idle() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Working, Instant::now());
    set_codex_hook_state(&mut terminal, AgentState::Working);

    terminal.set_detected_state_with_visible_blocker(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        true,
        Instant::now(),
    );

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::Codex));
    assert_eq!(terminal.effective_agent(), None);
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn stale_visible_screen_signal_does_not_override_newer_hook_authority() {
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
        "shepr:codex",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("codex-session"),
        Some(1),
        observed + Duration::from_secs(1),
    );

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        false,
        observed,
    );

    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn partial_state_authority_reanchors_sequence_after_process_restart() {
    let mut terminal = test_terminal();
    let observed = Instant::now();
    let session = || shepr_agent::resume::AgentSessionRef::id("codex-session");
    terminal.set_detected_state_at(
        Some(Agent::Codex),
        AgentState::Idle,
        observed - Duration::from_millis(1),
    );
    terminal.set_hook_authority_at(
        "shepr:codex",
        AgentState::Working,
        session(),
        Some(100),
        observed,
    );
    terminal.confirmed_detection_for_test(
        Some(Agent::Codex),
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
            .set_hook_authority_with_session_ref(
                "shepr:codex",
                AgentState::Working,
                session(),
                Some(1),
                observed + Duration::from_millis(2),
            )
            .is_none()
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        false,
        observed + Duration::from_millis(3),
    );
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:codex",
                AgentState::Working,
                session(),
                Some(1),
                observed + Duration::from_millis(4),
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
    for (seq, reported_at) in [(1, observed), (2, observed + Duration::from_secs(1))] {
        terminal
            .set_hook_authority_at(
                "shepr:codex",
                AgentState::Working,
                shepr_agent::resume::AgentSessionRef::id("codex-session"),
                Some(seq),
                reported_at,
            )
            .expect("Codex report");
    }

    let mutation = terminal.confirmed_detection_for_test(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        true,
        observed,
    );

    assert!(mutation.agent_released);
    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
    assert_eq!(terminal.effective_agent(), None);
}

#[test]
fn detected_agent_change_clears_previous_matching_hook_authority() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Idle, Instant::now());
    set_codex_hook_state(&mut terminal, AgentState::Idle);

    terminal.set_detected_state_at(Some(Agent::OpenCode), AgentState::Working, Instant::now());

    assert!(terminal.hook_authority.is_none());
    assert_eq!(terminal.detected_agent, Some(Agent::OpenCode));
    assert_eq!(terminal.effective_agent(), Some(Agent::OpenCode));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn stale_hook_report_sequence_is_ignored_for_same_source() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        pi_root_session_ref(),
        Some(20),
        Instant::now(),
    );

    let change = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        pi_root_session_ref(),
        Some(19),
        Instant::now(),
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
        shepr_agent::resume::AgentSessionRef::path(session_path.clone())
            .expect("test precondition"),
    );
    let mutation = terminal
        .set_hook_authority_with_session_ref(
            "shepr:pi",
            AgentState::Working,
            shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
            Some(20),
            Instant::now(),
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
            shepr_agent::resume::AgentSessionRefKind::Path,
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
        shepr_agent::resume::AgentSessionRef::path(session_path.clone())
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path.clone()),
        Some(20),
        Instant::now(),
    );

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(new_session_path),
        Some(19),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(shepr_agent::resume::AgentSessionRef::value_str),
        Some(session_path.as_str())
    );
}

#[test]
fn hook_report_without_session_ref_is_rejected_and_preserves_current_generation() {
    let mut terminal = test_terminal();
    let session_path = test_session_path("pi.jsonl");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(session_path.clone())
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path(session_path),
        Some(20),
        Instant::now(),
    );

    // Pi declares that state reports need a session reference, so a report
    // without one is dropped and the current generation is untouched.
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:pi",
                AgentState::Idle,
                None,
                Some(21),
                Instant::now(),
            )
            .is_none()
    );
    assert_eq!(terminal.state, AgentState::Working);
    assert!(
        terminal
            .hook_authority
            .as_ref()
            .expect("test precondition")
            .session_ref
            .is_some()
    );
    let identity = terminal
        .current_session_identity_for_persistence()
        .expect("resume identity");
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:pi",
                AgentState::Idle,
                Some(identity.session_ref().clone()),
                Some(22),
                Instant::now()
            )
            .is_some()
    );
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn different_same_agent_session_ref_is_ignored_until_current_session_clears() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:claude",
            shepr_agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_agent_session_ref(
        "shepr:claude",
        shepr_agent::resume::AgentSessionRef::id("nested-session"),
        Some(21),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .hook_sources
            .get(&bundled_source("shepr:claude"))
            .and_then(HookSourceState::sequence_value),
        Some(20)
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("claude-session")
    );
}

#[test]
fn claude_startup_session_ref_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:claude",
            shepr_agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:claude",
        shepr_agent::resume::AgentSessionRef::id("nested-session"),
        Some(21),
        Some("startup"),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("claude-session")
    );
}

#[test]
fn claude_lifecycle_session_ref_replaces_existing_session_ref() {
    for session_start_source in ["clear", "resume", "compact"] {
        let mut terminal = test_terminal();
        terminal
            .set_agent_session_ref(
                "shepr:claude",
                shepr_agent::resume::AgentSessionRef::id("claude-session"),
                Some(20),
                Instant::now(),
            )
            .expect("initial session should be accepted");

        let next_session = format!("{session_start_source}-session");
        let mutation = terminal
            .set_agent_session_ref_for_session_start(
                "shepr:claude",
                shepr_agent::resume::AgentSessionRef::id(&next_session),
                Some(21),
                Some(session_start_source),
                Instant::now(),
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
                .map(|session| session.session_ref().value_str()),
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
                "shepr:codex",
                shepr_agent::resume::AgentSessionRef::id("codex-session"),
                Some(20),
                Instant::now(),
            )
            .expect("initial session should be accepted");

        let next_session = format!("codex-{session_start_source}-session");
        let mutation = terminal
            .set_agent_session_ref_for_session_start(
                "shepr:codex",
                shepr_agent::resume::AgentSessionRef::id(&next_session),
                Some(21),
                Some(session_start_source),
                Instant::now(),
            )
            .unwrap_or_else(|| panic!("{session_start_source} should replace the session"));

        assert!(mutation.session_ref_changed);
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| session.session_ref().value_str()),
            Some(next_session.as_str())
        );
    }
}

#[test]
fn codex_hook_turn_lifecycle_preserves_session_and_beats_stale_working_screen() {
    fn session_id(terminal: &AgentOwnership) -> Option<String> {
        terminal
            .current_session_identity_for_persistence()
            .map(|session| session.session_ref().value_str().to_owned())
    }
    fn turn_report(
        terminal: &mut AgentOwnership,
        state: AgentState,
        session: &str,
        seq: u64,
    ) -> Option<AgentOwnershipMutation> {
        terminal.set_hook_authority_with_session_ref(
            "shepr:codex",
            state,
            shepr_agent::resume::AgentSessionRef::id(session),
            Some(seq),
            Instant::now(),
        )
    }

    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Codex), AgentState::Working, Instant::now());

    let mut seq = 0;
    for (session, start_source) in [("first", None), ("second", Some("clear"))] {
        seq += 1;
        terminal
            .set_agent_session_ref_for_session_start(
                "shepr:codex",
                shepr_agent::resume::AgentSessionRef::id(session),
                Some(seq),
                start_source,
                Instant::now(),
            )
            .unwrap_or_else(|| panic!("{session} session report should be accepted"));
        assert_eq!(session_id(&terminal).as_deref(), Some(session));

        seq += 1;
        turn_report(&mut terminal, AgentState::Working, session, seq)
            .unwrap_or_else(|| panic!("{session} working report should be accepted"));
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(session_id(&terminal).as_deref(), Some(session));

        seq += 1;
        let change = turn_report(&mut terminal, AgentState::Idle, session, seq)
            .and_then(|mutation| mutation.effective_state_change)
            .unwrap_or_else(|| panic!("{session} idle report should end the turn"));
        assert_eq!(change.previous_state, AgentState::Working);
        assert_eq!(change.state, AgentState::Idle);
        assert_eq!(terminal.state, AgentState::Idle);
        assert_eq!(session_id(&terminal).as_deref(), Some(session));
    }

    seq += 1;
    turn_report(&mut terminal, AgentState::Working, "second", seq)
        .expect("a new turn in the current session should be accepted");
    assert_eq!(terminal.state, AgentState::Working);

    // A late Stop from the replaced session must not end the current turn.
    seq += 1;
    assert!(turn_report(&mut terminal, AgentState::Idle, "first", seq).is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(session_id(&terminal).as_deref(), Some("second"));
}

#[test]
fn grok_new_session_ref_replaces_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:grok",
            shepr_agent::resume::AgentSessionRef::id("grok-old"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let mutation = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:grok",
            shepr_agent::resume::AgentSessionRef::id("grok-new"),
            Some(21),
            Some("new"),
            Instant::now(),
        )
        .expect("new should replace the grok session");

    assert!(mutation.session_ref_changed);
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("grok-new")
    );
}

#[test]
fn opencode_server_new_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::OpenCode), AgentState::Idle, Instant::now());
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode",
            shepr_agent::resume::AgentSessionRef::id("opencode-visible"),
            None,
            Some("select"),
            Instant::now(),
        )
        .expect("local selection should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode",
        shepr_agent::resume::AgentSessionRef::id("opencode-attached-client"),
        Some(21),
        Some("new"),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("opencode-visible")
    );
}

#[test]
fn opencode_server_resume_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::OpenCode), AgentState::Idle, Instant::now());
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode",
            shepr_agent::resume::AgentSessionRef::id("opencode-visible"),
            None,
            Some("select"),
            Instant::now(),
        )
        .expect("local selection should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode",
        shepr_agent::resume::AgentSessionRef::id("opencode-attached-client"),
        Some(21),
        Some("resume"),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("opencode-visible")
    );
}

#[test]
fn opencode_tui_selection_anchors_after_process_detection() {
    let mut terminal = test_terminal();
    let startup_selection = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode",
        shepr_agent::resume::AgentSessionRef::id("opencode-startup-selection"),
        None,
        Some("select"),
        Instant::now(),
    );
    assert!(startup_selection.is_some());
    assert_eq!(
        terminal
            .suppressed_hook_source("shepr:opencode")
            .and_then(|suppressed| suppressed
                .pending_start
                .as_ref()
                .map(PersistedAgentSession::session_ref))
            .map(shepr_agent::resume::AgentSessionRef::value_str),
        Some("opencode-startup-selection")
    );

    terminal.set_detected_state_at(Some(Agent::OpenCode), AgentState::Idle, Instant::now());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("opencode-startup-selection")
    );
    assert!(terminal.suppressed_hook_source("shepr:opencode").is_none());

    terminal
        .hook_sources
        .entry(bundled_source("shepr:opencode"))
        .or_default()
        .transition(HookSourceEvent::Release(
            FullLifecycleHookSuppressionReason::AwaitingProcess,
            SuppressedFullLifecycleHookReport {
                agent: Agent::OpenCode,
                session_ref: None,
                observed_at: Instant::now(),
                pending_start: None,
                pending_replacement_report: None,
            },
        ));
    let selected = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode",
            shepr_agent::resume::AgentSessionRef::id("opencode-reselected"),
            None,
            Some("select"),
            Instant::now(),
        )
        .expect("local TUI selection should reconcile generation suppression");

    assert!(selected.session_ref_changed);
    assert!(terminal.suppressed_hook_source("shepr:opencode").is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("opencode-reselected")
    );
    assert!(
        !terminal
            .hook_sources
            .get(&bundled_source("shepr:opencode"))
            .is_some_and(|record| record.sequence_value().is_some())
    );
}

#[test]
fn opencode_child_prompt_reports_with_root_id_preserve_lifecycle_authority() {
    let mut terminal = test_terminal();
    let root =
        shepr_agent::resume::AgentSessionRef::id("opencode-root").expect("test precondition");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
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
                "shepr:opencode",
                state,
                Some(root.clone()),
                Some(seq),
                Instant::now(),
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
        "shepr:opencode",
        AgentState::Blocked,
        shepr_agent::resume::AgentSessionRef::id("opencode-other-root"),
        Some(24),
        Instant::now(),
    );
    assert!(foreign_child_prompt.is_none());
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn opencode_tui_selection_reanchors_full_lifecycle_authority() {
    let mut terminal = test_terminal();
    let old_session =
        shepr_agent::resume::AgentSessionRef::id("opencode-newer").expect("test precondition");
    let selected_session = shepr_agent::resume::AgentSessionRef::id("opencode-selected-older")
        .expect("test precondition");
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        old_session.clone(),
    );
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:opencode",
            AgentState::Idle,
            Some(old_session.clone()),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should own lifecycle state");
    let attached_session = shepr_agent::resume::AgentSessionRef::id("opencode-attached-client")
        .expect("test precondition");
    let attached = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode",
        AgentState::Working,
        Some(attached_session.clone()),
        Some(21),
        Instant::now(),
    );
    assert!(attached.is_none());
    assert!(terminal.suppressed_hook_source("shepr:opencode").is_none());

    let selected = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode",
            Some(selected_session.clone()),
            None,
            Some("select"),
            Instant::now(),
        )
        .expect("selected session should replace the previous session");

    assert!(selected.session_ref_changed);
    assert!(terminal.hook_authority.is_none());
    assert!(terminal.suppressed_hook_source("shepr:opencode").is_none());
    assert_eq!(
        terminal
            .hook_sources
            .get(&bundled_source("shepr:opencode"))
            .and_then(HookSourceState::sequence_value),
        None
    );
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(PersistedAgentSession::session_ref),
        Some(&selected_session)
    );

    terminal
        .set_hook_authority_with_session_ref(
            "shepr:opencode",
            AgentState::Working,
            Some(selected_session.clone()),
            Some(21),
            Instant::now(),
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

    let ordered_generation = terminal.hook_sources.clone();
    assert!(
        terminal
            .set_agent_session_ref_for_session_start(
                "shepr:opencode",
                Some(selected_session.clone()),
                None,
                Some("select"),
                Instant::now()
            )
            .is_some()
    );
    assert_eq!(terminal.hook_sources, ordered_generation);
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:opencode",
                AgentState::Idle,
                Some(selected_session.clone()),
                Some(20),
                Instant::now()
            )
            .is_none()
    );
    assert_eq!(terminal.state, AgentState::Working);

    let late_old_session = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode",
        AgentState::Idle,
        Some(old_session),
        Some(22),
        Instant::now(),
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
        "shepr:opencode",
        AgentState::Blocked,
        Some(attached_session),
        Some(23),
        Instant::now(),
    );
    assert!(late_attached_session.is_none());
    assert_eq!(terminal.state, AgentState::Working);

    let final_session = shepr_agent::resume::AgentSessionRef::id("opencode-final-selection")
        .expect("test precondition");
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode",
            Some(final_session.clone()),
            None,
            Some("select"),
            Instant::now(),
        )
        .expect("another local selection should remain authoritative");
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(PersistedAgentSession::session_ref),
        Some(&final_session)
    );
    assert!(terminal.suppressed_hook_source("shepr:opencode").is_none());
}

#[test]
fn opencode_session_ref_without_start_source_does_not_replace_existing() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::OpenCode), AgentState::Idle, Instant::now());
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode",
            shepr_agent::resume::AgentSessionRef::id("opencode-old"),
            None,
            Some("select"),
            Instant::now(),
        )
        .expect("local selection should be accepted");

    // session.updated reports carry no session_start_source, so a different
    // id must not displace the established session (cross-talk guard).
    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:opencode",
        shepr_agent::resume::AgentSessionRef::id("opencode-other"),
        Some(21),
        None,
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("opencode-old")
    );
}

#[test]
fn different_owner_session_ref_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:droid",
            shepr_agent::resume::AgentSessionRef::id("droid-session"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:claude",
        shepr_agent::resume::AgentSessionRef::id("claude-session"),
        Some(21),
        Some("resume"),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| (session.source().as_str(), session.session_ref().value_str())),
        Some(("shepr:droid", "droid-session"))
    );
}

#[test]
fn grok_new_session_does_not_replace_a_different_owner() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            bundled_source("shepr:claude"),
            shepr_agent::resume::AgentSessionRef::id("claude-session").expect("test precondition"),
        )
        .expect("test session should be valid"),
    );
    terminal.set_detected_state_at(Some(Agent::Grok), AgentState::Idle, Instant::now());

    let mutation = terminal.set_agent_session_ref_for_session_start(
        "shepr:grok",
        shepr_agent::resume::AgentSessionRef::id("grok-session"),
        Some(21),
        Some("new"),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| (session.source().as_str(), session.session_ref().value_str())),
        Some(("shepr:claude", "claude-session"))
    );
}

#[test]
fn foreground_agent_session_replaces_stale_different_owner_session_ref() {
    for session_start_source in ["resume", "startup"] {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(
            shepr_agent::resume::PersistedAgentSession::new(
                bundled_source("shepr:codex"),
                shepr_agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test precondition"),
            )
            .expect("test session should be valid"),
        );
        terminal.set_detected_state_at(Some(Agent::Claude), AgentState::Idle, Instant::now());

        let mutation = terminal
            .set_agent_session_ref_for_session_start(
                "shepr:claude",
                shepr_agent::resume::AgentSessionRef::id("claude-session"),
                Some(21),
                Some(session_start_source),
                Instant::now(),
            )
            .unwrap_or_else(|| panic!("{session_start_source} should replace stale codex session"));

        assert!(mutation.session_ref_changed);
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| (session.source().as_str(), session.session_ref().value_str())),
            Some(("shepr:claude", "claude-session")),
            "{session_start_source} should store claude session"
        );
    }
}

#[test]
fn foreground_agent_session_requires_lifecycle_source_to_replace_different_owner() {
    for session_start_source in [None, Some("other")] {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(
            shepr_agent::resume::PersistedAgentSession::new(
                bundled_source("shepr:codex"),
                shepr_agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test precondition"),
            )
            .expect("test session should be valid"),
        );
        terminal.set_detected_state_at(Some(Agent::Claude), AgentState::Idle, Instant::now());

        let mutation = terminal.set_agent_session_ref_for_session_start(
            "shepr:claude",
            shepr_agent::resume::AgentSessionRef::id("claude-session"),
            Some(21),
            session_start_source,
            Instant::now(),
        );

        assert!(
            mutation.is_none(),
            "{session_start_source:?} should not replace"
        );
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| (session.source().as_str(), session.session_ref().value_str())),
            Some(("shepr:codex", "codex-session"))
        );
    }
}

#[test]
fn different_owner_session_ref_requires_matching_detected_agent() {
    for session_start_source in ["startup", "resume"] {
        for detected_agent in [None, Some(Agent::Codex)] {
            let mut terminal = test_terminal();
            terminal.set_persisted_agent_session(
                shepr_agent::resume::PersistedAgentSession::new(
                    bundled_source("shepr:codex"),
                    shepr_agent::resume::AgentSessionRef::id("codex-session")
                        .expect("test precondition"),
                )
                .expect("test session should be valid"),
            );
            terminal.set_detected_state_at(detected_agent, AgentState::Idle, Instant::now());

            let mutation = terminal.set_agent_session_ref_for_session_start(
                "shepr:claude",
                shepr_agent::resume::AgentSessionRef::id("claude-session"),
                Some(21),
                Some(session_start_source),
                Instant::now(),
            );

            assert!(
                mutation.is_none(),
                "{session_start_source} with {detected_agent:?} should not replace"
            );
            assert_eq!(
                terminal
                    .persisted_agent_session
                    .as_ref()
                    .map(|session| (session.source().as_str(), session.session_ref().value_str())),
                Some(("shepr:codex", "codex-session"))
            );
        }
    }
}

#[test]
fn foreground_agent_session_replaces_stale_different_owner_hook_authority() {
    let mut terminal = test_terminal();
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        shepr_agent::resume::AgentSessionRef::id("opencode-session").expect("test precondition"),
    );
    // Taken after the anchor's own detector observation: an observation
    // older than the last one is refused as stale.
    let now = std::time::Instant::now();
    terminal
        .set_hook_authority_at(
            "shepr:opencode",
            AgentState::Working,
            shepr_agent::resume::AgentSessionRef::id("opencode-session"),
            Some(20),
            now + Duration::from_millis(1),
        )
        .expect("initial hook authority should be accepted");
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        false,
        // Activating the opencode authority withdrew the screen verdict at the
        // hook's report time, so an older observation would be refused.
        now + Duration::from_millis(1),
    );

    let mutation = terminal
        .set_agent_session_ref_for_session_start(
            "shepr:codex",
            shepr_agent::resume::AgentSessionRef::id("codex-session"),
            Some(21),
            Some("startup"),
            now + Duration::from_millis(2),
        )
        .expect("foreground codex should replace stale hook authority");

    assert!(mutation.session_ref_changed);
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal.current_session_identity_for_persistence(),
        Some(
            shepr_agent::resume::PersistedAgentSession::new(
                shepr_agent::AgentSource::parse("shepr:codex").expect("bundled test source"),
                shepr_agent::resume::AgentSessionRef::id("codex-session")
                    .expect("test session ID should be valid"),
            )
            .expect("test session identity should be valid")
        )
    );
    let late_old_session = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("opencode-session"),
        Some(22),
        now + Duration::from_millis(3),
    );
    assert!(late_old_session.is_none());

    terminal.set_detected_state_at(
        Some(Agent::OpenCode),
        AgentState::Idle,
        now + Duration::from_millis(4),
    );
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:opencode",
            shepr_agent::resume::AgentSessionRef::id("opencode-new-session"),
            None,
            Some("select"),
            now + Duration::from_millis(5),
        )
        .expect("fresh local selection");
    let fresh_session = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("opencode-new-session"),
        Some(24),
        now + Duration::from_millis(6),
    );
    assert!(fresh_session.is_some());
}

#[test]
fn different_owner_full_lifecycle_hook_does_not_replace_existing_session_ref() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:droid",
            shepr_agent::resume::AgentSessionRef::id("droid-session"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::path("/tmp/pi-session.jsonl"),
        Some(21),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| (session.source().as_str(), session.session_ref().value_str())),
        Some(("shepr:droid", "droid-session"))
    );
}

#[test]
fn repeated_same_agent_session_ref_is_accepted_without_session_change() {
    let mut terminal = test_terminal();
    terminal
        .set_agent_session_ref(
            "shepr:claude",
            shepr_agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let mutation = terminal
        .set_agent_session_ref(
            "shepr:claude",
            shepr_agent::resume::AgentSessionRef::id("claude-session"),
            Some(21),
            Instant::now(),
        )
        .expect("same session should be accepted");

    assert!(!mutation.session_ref_changed);
}

#[test]
fn hook_authority_rejects_state_from_a_different_session() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::OpenCode), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::OpenCode,
        "shepr:opencode",
        shepr_agent::resume::AgentSessionRef::id("opencode-session").expect("test precondition"),
    );
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:opencode",
            AgentState::Working,
            shepr_agent::resume::AgentSessionRef::id("opencode-session"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let mutation = terminal.set_hook_authority_with_session_ref(
        "shepr:opencode",
        AgentState::Blocked,
        shepr_agent::resume::AgentSessionRef::id("nested-session"),
        Some(21),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(terminal.state, AgentState::Working);
    assert_eq!(
        terminal
            .hook_authority
            .as_ref()
            .and_then(|authority| authority.session_ref.as_ref())
            .map(shepr_agent::resume::AgentSessionRef::value_str),
        Some("opencode-session")
    );
}

#[test]
fn detected_agent_clear_does_not_clear_current_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Claude), AgentState::Working, Instant::now());
    terminal
        .set_agent_session_ref(
            "shepr:claude",
            shepr_agent::resume::AgentSessionRef::id("claude-session"),
            Some(20),
            Instant::now(),
        )
        .expect("initial session should be accepted");

    let clear =
        terminal.set_detected_state_with_mutation(None, AgentState::Unknown, Instant::now());
    assert!(!clear.session_ref_changed);

    let mutation = terminal.set_agent_session_ref(
        "shepr:claude",
        shepr_agent::resume::AgentSessionRef::id("new-session"),
        Some(21),
        Instant::now(),
    );

    assert!(mutation.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| session.session_ref().value_str()),
        Some("claude-session")
    );
}

#[test]
fn process_exit_clears_matching_persisted_session_ref() {
    let mut terminal = test_terminal();
    let session_ref = shepr_agent::resume::AgentSessionRef::path(test_session_path("pi.jsonl"))
        .expect("test precondition");
    terminal.set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            bundled_source("shepr:pi"),
            session_ref.clone(),
        )
        .expect("test session should be valid"),
    );
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Working, Instant::now());

    let mutation = terminal.confirmed_detection_for_test(
        Some(Agent::Pi),
        AgentState::Idle,
        false,
        true,
        std::time::Instant::now(),
    );

    assert!(mutation.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_none());

    let delayed =
        terminal.set_agent_session_ref("shepr:pi", Some(session_ref), Some(21), Instant::now());
    assert!(delayed.is_none());
    assert!(terminal.persisted_agent_session.is_none());
}

#[test]
fn process_exit_preserves_foreign_persisted_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            bundled_source("shepr:claude"),
            shepr_agent::resume::AgentSessionRef::id("claude-session").expect("test precondition"),
        )
        .expect("test session should be valid"),
    );
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Working, Instant::now());

    let mutation = terminal.confirmed_detection_for_test(
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
            .map(|session| session.session_ref().value_str()),
        Some("claude-session")
    );
}

#[test]
fn detected_conflict_preserves_session_only_report_identity() {
    let mut terminal = test_terminal();
    terminal.set_hook_authority_with_session_ref(
        "shepr:claude",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("claude-session"),
        Some(20),
        Instant::now(),
    );

    assert!(terminal.hook_authority.is_none());
    let mutation = terminal.set_detected_state_with_mutation(
        Some(Agent::Grok),
        AgentState::Idle,
        Instant::now(),
    );

    assert!(!mutation.session_ref_changed);
    assert!(terminal.hook_authority.is_none());
    assert_eq!(
        terminal
            .persisted_agent_session
            .as_ref()
            .map(|session| (session.source().as_str(), session.session_ref().value_str())),
        Some(("shepr:claude", "claude-session"))
    );
}

#[test]
fn detected_agent_disappearance_does_not_clear_full_lifecycle_hook_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Kimi), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Kimi,
        "shepr:kimi",
        shepr_agent::resume::AgentSessionRef::id("kimi-session").expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:kimi",
        AgentState::Working,
        shepr_agent::resume::AgentSessionRef::id("kimi-session"),
        Some(20),
        Instant::now(),
    );

    let mutation =
        terminal.set_detected_state_with_mutation(None, AgentState::Unknown, Instant::now());

    assert!(!mutation.session_ref_changed);
    assert!(terminal.hook_authority.is_some());
    assert!(terminal.persisted_agent_session.is_none());
    assert_eq!(terminal.effective_agent(), Some(Agent::Kimi));
}

#[test]
fn detected_agent_disappearance_preserves_matching_persisted_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            bundled_source("shepr:opencode"),
            shepr_agent::resume::AgentSessionRef::id("opencode-session")
                .expect("test precondition"),
        )
        .expect("test session should be valid"),
    );

    let first = terminal.set_detected_state_with_mutation(
        Some(Agent::OpenCode),
        AgentState::Idle,
        Instant::now(),
    );
    assert!(!first.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_some());

    let second =
        terminal.set_detected_state_with_mutation(None, AgentState::Unknown, Instant::now());
    assert!(!second.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_some());
}

#[test]
fn initial_unknown_detection_preserves_restored_session_ref() {
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(
        shepr_agent::resume::PersistedAgentSession::new(
            bundled_source("shepr:codex"),
            shepr_agent::resume::AgentSessionRef::id("codex-session").expect("test precondition"),
        )
        .expect("test session should be valid"),
    );

    let mutation =
        terminal.set_detected_state_with_mutation(None, AgentState::Unknown, Instant::now());
    assert!(!mutation.session_ref_changed);
    assert!(terminal.persisted_agent_session.is_some());
}

#[test]
fn unsequenced_hook_report_is_ignored_after_source_uses_sequence() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        shepr_agent::resume::AgentSessionRef::path(test_session_path("root.jsonl"))
            .expect("test precondition"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        pi_root_session_ref(),
        Some(20),
        Instant::now(),
    );

    let change = terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        pi_root_session_ref(),
        None,
        Instant::now(),
    );

    assert!(change.is_none());
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn same_sequence_from_different_sources_is_independent() {
    let mut terminal = test_terminal();
    // clock-io-ok: synthetic observations for report ordering.
    let now = Instant::now();
    assert!(terminal.accept_hook_report_at("shepr:pi", Some(20), now));
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(19), now));
    assert!(!terminal.accept_hook_report_at("shepr:pi", Some(19), now));
    assert_eq!(
        terminal.hook_sources[&bundled_source("shepr:kimi")].sequence_value(),
        Some(19)
    );
    assert_eq!(
        terminal.hook_sources[&bundled_source("shepr:pi")].sequence_value(),
        Some(20)
    );
}

/// Stale session identities per official source keep only the newest ones.
#[test]
fn stale_full_lifecycle_sessions_are_capped_per_source() {
    let mut terminal = test_terminal();
    let total = MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE + 3;
    for index in 0..total {
        terminal
            .hook_sources
            .entry(bundled_source("shepr:codex"))
            .or_default()
            .transition(HookSourceEvent::Retire(StaleFullLifecycleHookSession {
                agent: Agent::Codex,
                session_ref: shepr_agent::resume::AgentSessionRef::id(format!("session-{index}"))
                    .expect("test precondition"),
            }));
    }
    let sessions = terminal.hook_sources[&bundled_source("shepr:codex")].stale_sessions();
    assert_eq!(
        sessions.len(),
        MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE
    );
    let newest = shepr_agent::resume::AgentSessionRef::id(format!("session-{}", total - 1))
        .expect("test precondition");
    let oldest_kept = shepr_agent::resume::AgentSessionRef::id(format!(
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

#[test]
fn recognized_kimi_and_kilo_session_starts_replace_identity_and_release_old_state() {
    for (agent, source, start) in [
        (Agent::Kimi, "shepr:kimi", "new"),
        (Agent::Kilo, "shepr:kilo", "startup"),
    ] {
        let mut terminal = test_terminal();
        let old = shepr_agent::resume::AgentSessionRef::id("old").expect("session");
        let new = shepr_agent::resume::AgentSessionRef::id("new").expect("session");
        anchor_full_lifecycle_session(&mut terminal, agent, source, old.clone());
        assert!(
            terminal
                .set_hook_authority_with_session_ref(
                    source,
                    AgentState::Blocked,
                    Some(old.clone()),
                    Some(10),
                    Instant::now()
                )
                .is_some()
        );
        let changed = terminal
            .set_agent_session_ref_for_session_start(
                source,
                Some(new.clone()),
                Some(11),
                Some(start),
                Instant::now(),
            )
            .expect("recognized selection");
        assert!(changed.session_ref_changed);
        assert!(terminal.hook_authority.is_none());
        assert_eq!(
            terminal
                .current_session_identity_for_persistence()
                .expect("identity")
                .session_ref()
                .clone(),
            new.clone()
        );
        assert!(
            terminal
                .set_hook_authority_with_session_ref(
                    source,
                    AgentState::Working,
                    Some(new.clone()),
                    Some(12),
                    Instant::now()
                )
                .is_some()
        );
        assert!(
            terminal
                .set_hook_authority_with_session_ref(
                    source,
                    AgentState::Idle,
                    Some(new),
                    Some(11),
                    Instant::now()
                )
                .is_none()
        );
        assert!(
            terminal
                .set_hook_authority_with_session_ref(
                    source,
                    AgentState::Blocked,
                    Some(old),
                    Some(13),
                    Instant::now()
                )
                .is_none()
        );
        assert_eq!(terminal.state, AgentState::Working);
    }
}

#[test]
fn refused_session_replacement_preserves_authority_and_source_ordering() {
    let mut terminal = test_terminal();
    let old = shepr_agent::resume::AgentSessionRef::id("old").expect("session");
    anchor_full_lifecycle_session(&mut terminal, Agent::Kilo, "shepr:kilo", old.clone());
    terminal.set_hook_authority_with_session_ref(
        "shepr:kilo",
        AgentState::Working,
        Some(old.clone()),
        Some(10),
        Instant::now(),
    );
    let sources = terminal.hook_sources.clone();
    let authority = terminal.hook_authority.clone();
    assert!(
        terminal
            .set_agent_session_ref_for_session_start(
                "shepr:kilo",
                shepr_agent::resume::AgentSessionRef::id("different"),
                Some(100),
                Some("unknown"),
                Instant::now()
            )
            .is_none()
    );
    assert_eq!(terminal.hook_sources, sources);
    assert_eq!(terminal.hook_authority, authority);
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:kilo",
                AgentState::Idle,
                Some(old),
                Some(11),
                Instant::now()
            )
            .is_some()
    );
}

#[test]
fn invalid_session_kind_another_source_and_a_missing_session_do_not_change_arbitration() {
    let mut terminal = test_terminal();
    let old = shepr_agent::resume::AgentSessionRef::id("old").expect("session");
    anchor_full_lifecycle_session(&mut terminal, Agent::Kimi, "shepr:kimi", old.clone());
    terminal.set_hook_authority_with_session_ref(
        "shepr:kimi",
        AgentState::Working,
        Some(old),
        Some(10),
        Instant::now(),
    );
    let sources = terminal.hook_sources.clone();
    let authority = terminal.hook_authority.clone();
    let invalid = shepr_agent::resume::AgentSessionRef::path(test_session_path("invalid-kind"));
    assert!(
        terminal
            .set_agent_session_ref_for_session_start(
                "shepr:kimi",
                invalid.clone(),
                Some(100),
                Some("new"),
                Instant::now()
            )
            .is_none()
    );
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:kimi",
                AgentState::Blocked,
                invalid,
                Some(100),
                Instant::now()
            )
            .is_none()
    );
    assert!(
        terminal
            .set_agent_session_ref_for_session_start(
                "shepr:kilo",
                shepr_agent::resume::AgentSessionRef::id("other"),
                Some(100),
                Some("startup"),
                Instant::now()
            )
            .is_none()
    );
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:kimi",
                AgentState::Blocked,
                None,
                Some(100),
                Instant::now()
            )
            .is_none()
    );
    assert_eq!(terminal.hook_sources, sources);
    assert_eq!(terminal.hook_authority, authority);
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn older_pending_report_returns_none_without_changing_the_generation() {
    let mut terminal = test_terminal();
    let session = shepr_agent::resume::AgentSessionRef::id("pending").expect("session");
    let pending = terminal
        .set_hook_authority_with_session_ref(
            "shepr:kimi",
            AgentState::Working,
            Some(session.clone()),
            Some(100),
            Instant::now(),
        )
        .expect("accepted pending report");
    assert!(pending.effective_state_change.is_none());
    assert!(!pending.session_ref_changed);
    assert!(terminal.hook_authority.is_none());
    let sources = terminal.hook_sources.clone();
    assert!(
        terminal
            .set_hook_authority_with_session_ref(
                "shepr:kimi",
                AgentState::Idle,
                Some(session),
                Some(99),
                Instant::now()
            )
            .is_none()
    );
    assert_eq!(terminal.hook_sources, sources);
}

#[test]
fn claude_clear_replaces_session_before_and_after_next_state_report() {
    let mut terminal = test_terminal();
    let old = shepr_agent::resume::AgentSessionRef::id("before-clear").expect("session");
    let new = shepr_agent::resume::AgentSessionRef::id("after-clear").expect("session");
    terminal.set_detected_state_at(Some(Agent::Claude), AgentState::Idle, Instant::now());
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:claude",
            AgentState::Working,
            Some(old),
            Some(10),
            Instant::now(),
        )
        .expect("old identity");
    terminal
        .set_agent_session_ref_for_session_start(
            "shepr:claude",
            Some(new.clone()),
            Some(11),
            Some("clear"),
            Instant::now(),
        )
        .expect("clear start");
    assert_eq!(
        terminal
            .current_session_identity_for_persistence()
            .expect("identity")
            .session_ref()
            .clone(),
        new
    );
    terminal
        .set_hook_authority_with_session_ref(
            "shepr:claude",
            AgentState::Idle,
            Some(new.clone()),
            Some(12),
            Instant::now(),
        )
        .expect("identity from state report");
    assert_eq!(
        terminal
            .current_session_identity_for_persistence()
            .expect("identity")
            .session_ref()
            .clone(),
        new
    );
}

#[test]
fn hook_ledger_uses_injected_clock_pair_and_tolerates_small_wall_reversal() {
    let mut terminal = test_terminal();
    let monotonic = Instant::now();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let sample = HookClockSample { monotonic, wall };
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(1_000), sample));
    // Ten seconds of silence with matching clocks never reopens ordering.
    assert!(!terminal.accept_hook_report_at(
        "shepr:kimi",
        Some(999),
        HookClockSample {
            monotonic: monotonic + Duration::from_secs(10),
            wall: wall + Duration::from_secs(10),
        }
    ));
    let stepped = HookClockSample {
        monotonic: monotonic + Duration::from_millis(100),
        wall: wall - Duration::from_millis(100),
    };
    assert!(terminal.accept_hook_report_at("shepr:kimi", Some(999), stepped));
    // Validation and commit use the same injected samples, so a duplicate
    // remains a duplicate immediately after re-anchoring.
    assert!(!terminal.accept_hook_report_at(
        "shepr:kimi",
        Some(999),
        HookClockSample {
            monotonic: stepped.monotonic + Duration::from_millis(100),
            wall: stepped.wall + Duration::from_millis(100),
        }
    ));
}

fn codex_origin() -> ReportOrigin {
    ReportOrigin::parse("shepr:codex").expect("test origin")
}

#[test]
fn a_rejected_report_is_kept_until_a_later_one_from_its_source_applies() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    let sample = HookClockSample::from(t0);

    // Codex state reports must name their session.
    let outcome =
        terminal.report_hook_outcome_at(codex_origin(), AgentState::Working, None, Some(3), sample);
    assert_eq!(
        outcome,
        HookOutcome::Rejected(HookRejection::MissingSession)
    );
    assert_eq!(
        terminal.last_unapplied_hook_report(t0),
        Some(UnappliedHookReport {
            origin: codex_origin(),
            kind: HookReportKind::State(AgentState::Working),
            seq: Some(3),
            session_ref: None,
            received: sample,
            disposition: UnappliedHookDisposition::Rejected(HookRejection::MissingSession),
        })
    );

    let session = shepr_agent::resume::AgentSessionRef::id("codex-session");
    let applied = terminal.report_hook_outcome_at(
        codex_origin(),
        AgentState::Working,
        session.clone(),
        Some(5),
        HookClockSample::from(t0 + Duration::from_millis(1)),
    );
    assert!(matches!(applied, HookOutcome::Applied(_)), "{applied:?}");
    assert_eq!(
        terminal.last_unapplied_hook_report(t0 + Duration::from_millis(1)),
        None
    );

    // A straggler behind the applied sequence is recorded with what it was.
    let straggler_sample = HookClockSample::from(t0 + Duration::from_millis(2));
    let straggler = terminal.report_hook_outcome_at(
        codex_origin(),
        AgentState::Idle,
        session.clone(),
        Some(4),
        straggler_sample,
    );
    assert_eq!(straggler, HookOutcome::Rejected(HookRejection::OutOfOrder));
    let last = terminal
        .last_unapplied_hook_report(straggler_sample.monotonic)
        .expect("the straggler is recorded");
    assert_eq!(last.kind, HookReportKind::State(AgentState::Idle));
    assert_eq!(last.seq, Some(4));
    assert_eq!(last.session_ref, session);
    assert_eq!(last.received, straggler_sample);
    assert_eq!(
        last.disposition,
        UnappliedHookDisposition::Rejected(HookRejection::OutOfOrder)
    );
}

#[test]
fn an_applied_report_from_another_source_keeps_the_rejection() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    terminal.report_hook_outcome_at(
        codex_origin(),
        AgentState::Working,
        None,
        Some(1),
        HookClockSample::from(t0),
    );
    let session_path = test_session_path("other-source.jsonl");
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Idle, Instant::now());
    let pi = ReportOrigin::parse("shepr:pi").expect("test origin");
    let outcome = terminal.report_session_start_outcome_at(
        &pi,
        shepr_agent::resume::AgentSessionRef::path(session_path),
        Some(1),
        ReportedSessionStart::Known(AgentSessionStartSource::Startup),
        t0 + Duration::from_millis(1),
    );
    assert!(matches!(outcome, HookOutcome::Applied(_)), "{outcome:?}");
    assert_eq!(
        terminal
            .last_unapplied_hook_report(t0 + Duration::from_millis(1))
            .map(|last| last.disposition),
        Some(UnappliedHookDisposition::Rejected(
            HookRejection::MissingSession
        ))
    );
}

#[test]
fn a_parked_start_is_recorded_until_process_evidence_promotes_it() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    let kimi = ReportOrigin::parse("shepr:kimi").expect("test origin");
    let start = ReportedSessionStart::Known(AgentSessionStartSource::Startup);

    let outcome = terminal.report_session_start_outcome_at(
        &kimi,
        shepr_agent::resume::AgentSessionRef::id("kimi-root"),
        Some(10),
        start,
        t0,
    );
    assert_eq!(outcome, HookOutcome::Parked);
    let parked = terminal
        .last_unapplied_hook_report(t0)
        .expect("the parked start is recorded");
    assert_eq!(parked.kind, HookReportKind::SessionStart(start));
    assert_eq!(
        parked.disposition,
        UnappliedHookDisposition::Parked(ParkedHookAwaiting::Process {
            expires_at: t0 + crate::limits::PARKED_START_LIFETIME,
        })
    );
    assert!(terminal.persisted_agent_session().is_none());

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Kimi),
        AgentState::Idle,
        false,
        false,
        t0 + Duration::from_millis(1),
    );
    assert!(
        terminal.persisted_agent_session().is_some(),
        "process evidence promotes the parked start"
    );
    assert_eq!(
        terminal.last_unapplied_hook_report(t0 + Duration::from_millis(1)),
        None
    );
}

#[test]
fn an_expired_parked_start_is_no_longer_recorded_as_parked() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    let kimi = ReportOrigin::parse("shepr:kimi").expect("test origin");
    let start = ReportedSessionStart::Known(AgentSessionStartSource::Startup);

    let outcome = terminal.report_session_start_outcome_at(
        &kimi,
        shepr_agent::resume::AgentSessionRef::id("kimi-root"),
        Some(10),
        start,
        t0,
    );
    assert_eq!(outcome, HookOutcome::Parked);
    let deadline = t0 + crate::limits::PARKED_START_LIFETIME;
    let expired = deadline + Duration::from_nanos(1);
    assert!(terminal.last_unapplied_hook_report(deadline).is_some());
    // No process observation has applied the expiry yet; the read alone
    // judges it.
    assert_eq!(terminal.last_unapplied_hook_report(expired), None);

    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Kimi),
        AgentState::Idle,
        false,
        false,
        expired,
    );
    assert!(
        terminal.persisted_agent_session().is_none(),
        "an expired start is not promoted"
    );
    // The observation forgot the start's instant, so the record itself must
    // be gone, not merely filtered.
    assert_eq!(terminal.last_unapplied_hook_report(expired), None);
    assert_eq!(terminal.last_unapplied_hook_report(t0), None);
}

/// A full-lifecycle hook governs only while the detector reports its agent
/// with no exit recorded, so the detector observations it overrides never
/// change the detected agent, and a start for that agent finds its process
/// present and is settled when it is admitted, not left for an observation.
#[test]
fn a_governing_hook_overrides_observations_without_moving_the_detected_agent() {
    let mut terminal = test_terminal();
    let first =
        shepr_agent::resume::AgentSessionRef::path(test_session_path("governed-first.jsonl"));
    let second =
        shepr_agent::resume::AgentSessionRef::path(test_session_path("governed-second.jsonl"));
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        first.clone().expect("test precondition"),
    );
    let now = Instant::now();
    assert!(
        terminal
            .set_hook_authority_at("shepr:pi", AgentState::Working, first, Some(1_000), now)
            .is_some()
    );
    assert!(terminal.full_lifecycle_hook_authority_active());

    for (offset, observed) in [(1, None), (2, Some(Agent::Pi))] {
        terminal.set_detected_state_with_screen_signals_at(
            observed,
            AgentState::Idle,
            false,
            false,
            now + Duration::from_millis(offset),
        );
        assert_eq!(terminal.detected_agent, Some(Agent::Pi));
        assert!(terminal.full_lifecycle_hook_authority_active());
    }

    let second = second.expect("test precondition");
    let started = terminal.report_session_start_outcome_at(
        &ReportOrigin::parse("shepr:pi").expect("test origin"),
        Some(second.clone()),
        Some(2_000),
        ReportedSessionStart::Known(AgentSessionStartSource::New),
        now + Duration::from_millis(3),
    );
    assert!(matches!(started, HookOutcome::Applied(_)), "{started:?}");
    assert_eq!(
        terminal
            .current_session_identity_for_persistence()
            .map(|session| session.session_ref().clone()),
        Some(second),
        "the start is settled at admission"
    );
    assert_eq!(
        terminal.last_unapplied_hook_report(now + Duration::from_millis(3)),
        None,
        "nothing is left parked"
    );
}

#[test]
fn a_rejection_is_not_aged_out_by_a_parked_start_lifetime() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    terminal.report_hook_outcome_at(
        codex_origin(),
        AgentState::Working,
        None,
        Some(1),
        HookClockSample::from(t0),
    );
    assert!(
        terminal
            .last_unapplied_hook_report(
                t0 + crate::limits::PARKED_START_LIFETIME + Duration::from_secs(1)
            )
            .is_some()
    );
}

fn kimi_origin() -> ReportOrigin {
    ReportOrigin::parse("shepr:kimi").expect("test origin")
}

fn kimi_root() -> Option<shepr_agent::resume::AgentSessionRef> {
    shepr_agent::resume::AgentSessionRef::id("kimi-root")
}

fn parked_disposition(terminal: &AgentOwnership, now: Instant) -> Option<UnappliedHookDisposition> {
    terminal
        .last_unapplied_hook_report(now)
        .map(|last| last.disposition)
}

#[test]
fn a_parked_report_an_exit_consumed_is_no_longer_recorded_as_parked() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Kimi),
        AgentState::Idle,
        false,
        false,
        t0,
    );
    let reported_at = t0 + Duration::from_millis(1);
    let outcome = terminal.report_hook_outcome_at(
        kimi_origin(),
        AgentState::Working,
        kimi_root(),
        Some(10),
        HookClockSample::from(reported_at),
    );
    assert_eq!(outcome, HookOutcome::Parked);
    // The process is present, but process evidence never promotes a report:
    // it waits for a start of its session.
    assert_eq!(
        parked_disposition(&terminal, reported_at),
        Some(UnappliedHookDisposition::Parked(
            ParkedHookAwaiting::SessionStart
        ))
    );

    let exited_at = t0 + Duration::from_millis(2);
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Kimi),
        AgentState::Idle,
        false,
        true,
        exited_at,
    );
    // The exit consumed the pending report; nothing can promote it now.
    assert_eq!(terminal.last_unapplied_hook_report(exited_at), None);
}

#[test]
fn a_report_riding_a_parked_start_awaits_process_evidence_until_the_start_expires() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    assert_eq!(
        terminal.report_session_start_outcome_at(
            &kimi_origin(),
            kimi_root(),
            Some(10),
            ReportedSessionStart::Known(AgentSessionStartSource::Startup),
            t0,
        ),
        HookOutcome::Parked
    );
    let reported_at = t0 + Duration::from_millis(1);
    assert_eq!(
        terminal.report_hook_outcome_at(
            kimi_origin(),
            AgentState::Working,
            kimi_root(),
            Some(11),
            HookClockSample::from(reported_at),
        ),
        HookOutcome::Parked
    );
    let last = terminal
        .last_unapplied_hook_report(reported_at)
        .expect("the parked report is recorded");
    assert_eq!(last.kind, HookReportKind::State(AgentState::Working));
    assert_eq!(
        last.disposition,
        UnappliedHookDisposition::Parked(ParkedHookAwaiting::Process {
            expires_at: t0 + crate::limits::PARKED_START_LIFETIME,
        })
    );
    assert_eq!(
        terminal.last_unapplied_hook_report(
            t0 + crate::limits::PARKED_START_LIFETIME + Duration::from_nanos(1)
        ),
        None
    );
}

#[test]
fn a_report_then_an_older_start_with_the_process_present_promotes_both_at_once() {
    let mut terminal = test_terminal();
    let t0 = Instant::now();
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Kimi),
        AgentState::Idle,
        false,
        false,
        t0,
    );
    assert_eq!(
        terminal.report_hook_outcome_at(
            kimi_origin(),
            AgentState::Working,
            kimi_root(),
            Some(10),
            HookClockSample::from(t0 + Duration::from_millis(1)),
        ),
        HookOutcome::Parked
    );
    let started_at = t0 + Duration::from_millis(2);
    let started = terminal.report_session_start_outcome_at(
        &kimi_origin(),
        kimi_root(),
        Some(9),
        ReportedSessionStart::Known(AgentSessionStartSource::Startup),
        started_at,
    );
    assert!(matches!(started, HookOutcome::Applied(_)), "{started:?}");
    assert_eq!(
        terminal
            .current_session_identity_for_persistence()
            .map(|session| session.session_ref().clone()),
        kimi_root()
    );
    let authority = terminal
        .hook_authority()
        .expect("the report rode the start");
    assert_eq!(authority.state, AgentState::Working);
    assert_eq!(authority.session_ref, kimi_root());
    assert_eq!(terminal.last_unapplied_hook_report(started_at), None);
}

#[test]
fn first_presence_sampled_before_a_report_still_promotes_the_start_after_it() {
    // A fresh pane: the report arrives before its start, and both before the
    // detector's first tick that sees the agent, which was stamped before
    // either hook was applied.
    let mut terminal = test_terminal();
    let reported_at = Instant::now();
    assert_eq!(
        terminal.report_hook_outcome_at(
            kimi_origin(),
            AgentState::Working,
            kimi_root(),
            Some(10),
            HookClockSample::from(reported_at),
        ),
        HookOutcome::Parked
    );
    assert_eq!(
        terminal.report_session_start_outcome_at(
            &kimi_origin(),
            kimi_root(),
            Some(11),
            ReportedSessionStart::Known(AgentSessionStartSource::Startup),
            reported_at + Duration::from_secs(1),
        ),
        HookOutcome::Parked
    );
    terminal.set_detected_agent_process_at(Agent::Kimi, reported_at - Duration::from_millis(1));
    assert_eq!(
        terminal
            .current_session_identity_for_persistence()
            .map(|session| session.session_ref().clone()),
        kimi_root(),
        "the first presence tick promotes the parked start"
    );
}

#[test]
fn a_parked_report_reanchors_its_order_after_a_backwards_clock_step() {
    let mut terminal = test_terminal();
    let first = HookClockSample::from(Instant::now());
    assert_eq!(
        terminal.report_hook_outcome_at(
            kimi_origin(),
            AgentState::Working,
            kimi_root(),
            Some(1_000),
            first,
        ),
        HookOutcome::Parked
    );
    // The wall clock stepped back, so the reporter's next seq is smaller.
    let stepped = HookClockSample {
        monotonic: first.monotonic + Duration::from_millis(100),
        wall: first.wall - Duration::from_millis(100),
    };
    assert_eq!(
        terminal.report_hook_outcome_at(
            kimi_origin(),
            AgentState::Idle,
            kimi_root(),
            Some(999),
            stepped,
        ),
        HookOutcome::Parked
    );
    let last = terminal
        .last_unapplied_hook_report(stepped.monotonic)
        .expect("the newer report is recorded");
    assert_eq!(last.seq, Some(999));
    assert_eq!(
        last.disposition,
        UnappliedHookDisposition::Parked(ParkedHookAwaiting::SessionStart)
    );
    // Without a step, a smaller seq after it is still a straggler.
    assert_eq!(
        terminal.report_hook_outcome_at(
            kimi_origin(),
            AgentState::Working,
            kimi_root(),
            Some(998),
            HookClockSample {
                monotonic: stepped.monotonic + Duration::from_millis(100),
                wall: stepped.wall + Duration::from_millis(100),
            },
        ),
        HookOutcome::Rejected(HookRejection::OutOfOrder)
    );
}

/// Parks a recognized kimi start with seq 1000 while the process is absent and
/// returns its sample.
fn park_kimi_start_at_seq_1000(terminal: &mut AgentOwnership) -> HookClockSample {
    let started = HookClockSample::from(Instant::now());
    assert_eq!(
        terminal.report_session_start_outcome_at(
            &kimi_origin(),
            kimi_root(),
            Some(1_000),
            ReportedSessionStart::Known(AgentSessionStartSource::Startup),
            started,
        ),
        HookOutcome::Parked
    );
    started
}

#[test]
fn a_report_no_newer_than_a_pending_start_is_refused_on_arrival() {
    // Nothing a parked start's promotion would carry is parked: a report at or
    // below the start's seq, with no clock step, is out of order on arrival.
    let mut terminal = test_terminal();
    let started = park_kimi_start_at_seq_1000(&mut terminal);
    for (seq, after) in [(1_000, 1), (999, 2)] {
        let at = Duration::from_millis(after);
        assert_eq!(
            terminal.report_hook_outcome_at(
                kimi_origin(),
                AgentState::Working,
                kimi_root(),
                Some(seq),
                HookClockSample {
                    monotonic: started.monotonic + at,
                    wall: started.wall + at,
                },
            ),
            HookOutcome::Rejected(HookRejection::OutOfOrder),
            "seq {seq}"
        );
    }
    let last = terminal
        .last_unapplied_hook_report(started.monotonic)
        .expect("the refusal is recorded");
    assert_eq!(
        last.disposition,
        UnappliedHookDisposition::Rejected(HookRejection::OutOfOrder)
    );
}

#[test]
fn a_report_after_a_backwards_clock_step_rides_the_start_it_followed() {
    // The wall clock stepped back between a parked start and the next report,
    // so the reporter's seq went down. The source's ordering rule admitted the
    // report as the newer one; promotion must carry it, not drop it as older.
    let mut terminal = test_terminal();
    let started = park_kimi_start_at_seq_1000(&mut terminal);
    let stepped = HookClockSample {
        monotonic: started.monotonic + Duration::from_millis(100),
        wall: started.wall - Duration::from_millis(100),
    };
    assert_eq!(
        terminal.report_hook_outcome_at(
            kimi_origin(),
            AgentState::Working,
            kimi_root(),
            Some(999),
            stepped,
        ),
        HookOutcome::Parked
    );
    assert_eq!(
        parked_disposition(&terminal, stepped.monotonic),
        Some(UnappliedHookDisposition::Parked(
            ParkedHookAwaiting::Process {
                expires_at: started.monotonic + crate::limits::PARKED_START_LIFETIME,
            }
        ))
    );

    terminal.set_detected_agent_process_at(Agent::Kimi, stepped.monotonic);
    let authority = terminal
        .hook_authority()
        .expect("the report rode the promoted start");
    assert_eq!(authority.state, AgentState::Working);
    assert_eq!(authority.session_ref, kimi_root());
    assert_eq!(terminal.last_unapplied_hook_report(stepped.monotonic), None);
}

#[test]
fn repeated_full_lifecycle_reports_preserve_activation_watermark() {
    let mut terminal = test_terminal();
    terminal.set_detected_state_at(Some(Agent::Pi), AgentState::Blocked, Instant::now());
    anchor_full_lifecycle_session(
        &mut terminal,
        Agent::Pi,
        "shepr:pi",
        pi_root_session_ref().expect("test session"),
    );
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Working,
        pi_root_session_ref(),
        None,
        Instant::now(),
    );
    let activated_at = terminal.fallback_observed_at;
    assert_eq!(terminal.fallback_state, AgentState::Unknown);
    assert!(!terminal.fallback_visible_blocker);
    terminal.set_hook_authority_with_session_ref(
        "shepr:pi",
        AgentState::Idle,
        pi_root_session_ref(),
        None,
        Instant::now() + Duration::from_secs(1),
    );
    assert_eq!(terminal.fallback_observed_at, activated_at);
    assert_eq!(terminal.state, AgentState::Idle);
    let outcome = terminal.report_session_start_outcome_at(
        &ReportOrigin::parse("shepr:pi").expect("test origin"),
        shepr_agent::resume::AgentSessionRef::path(test_session_path("replacement.jsonl")),
        None,
        ReportedSessionStart::Known(AgentSessionStartSource::New),
        Instant::now() + Duration::from_secs(2),
    );
    assert!(matches!(outcome, HookOutcome::Applied(_)), "{outcome:?}");
    assert!(!terminal.full_lifecycle_hook_authority_active());
    assert_eq!(terminal.state, AgentState::Unknown);
}

#[test]
fn partial_state_reports_resume_after_same_agent_replacement_presence() {
    let mut terminal = test_terminal();
    let now = Instant::now();
    terminal.set_detected_agent_process_at(Agent::Codex, now);
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Codex),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(1),
    );
    let session =
        shepr_agent::resume::AgentSessionRef::id("replacement-codex").expect("test session");
    assert_eq!(
        terminal.report_hook_outcome_at(
            codex_origin(),
            AgentState::Working,
            Some(session.clone()),
            None,
            HookClockSample::from(now + Duration::from_millis(2)),
        ),
        HookOutcome::Rejected(HookRejection::ProcessExited)
    );
    terminal.set_detected_agent_process_at(Agent::Codex, now + Duration::from_millis(3));
    assert!(matches!(
        terminal.report_hook_outcome_at(
            codex_origin(),
            AgentState::Working,
            Some(session),
            None,
            HookClockSample::from(now + Duration::from_millis(4)),
        ),
        HookOutcome::Applied(_)
    ));
    assert_eq!(terminal.state, AgentState::Working);
}

#[test]
fn persisted_paths_reject_unrecognized_same_owner_replacements() {
    for (agent, source) in [(Agent::Pi, "shepr:pi"), (Agent::Omp, "shepr:omp")] {
        for reason in [None, Some("reload")] {
            let mut terminal = test_terminal();
            let now = Instant::now();
            terminal.set_detected_agent_process_at(agent, now);
            let old =
                shepr_agent::resume::AgentSessionRef::path(test_session_path("anchored.jsonl"))
                    .expect("test path");
            terminal.set_persisted_agent_session(
                PersistedAgentSession::new(bundled_source(source), old.clone())
                    .expect("test session"),
            );
            let replacement = terminal.set_agent_session_ref_for_session_start(
                source,
                shepr_agent::resume::AgentSessionRef::path(test_session_path("stray.jsonl")),
                Some(1),
                reason,
                now + Duration::from_millis(1),
            );
            assert!(replacement.is_none());
            assert_eq!(
                terminal
                    .persisted_agent_session()
                    .map(PersistedAgentSession::session_ref),
                Some(&old)
            );
        }
    }
}

fn startup_source() -> ReportedSessionStart {
    ReportedSessionStart::Known(AgentSessionStartSource::Startup)
}

/// A pane whose detector holds a live `agent` process at `now`, with `old` as
/// the pane's session and no hook authority: a restored identity, or a
/// screen-owned agent's own startup.
fn running_agent_with_session(
    agent: Agent,
    old: &shepr_agent::resume::AgentSessionRef,
    now: Instant,
) -> AgentOwnership {
    let mut terminal = test_terminal();
    terminal.set_detected_agent_process_at(agent, now);
    let origin = ReportOrigin::official(agent).expect("official integration");
    terminal.set_persisted_agent_session(
        PersistedAgentSession::new(*origin.source(), old.clone()).expect("test session"),
    );
    terminal
}

fn current_session_ref(terminal: &AgentOwnership) -> Option<shepr_agent::resume::AgentSessionRef> {
    terminal
        .current_session_identity_for_persistence()
        .map(|session| session.session_ref().clone())
}

/// A screen-owned agent (Claude) and a full-lifecycle one (Pi, whose restored
/// path is the same case as a live one), each with an old and a new session.
fn relaunch_rows() -> Vec<(
    Agent,
    shepr_agent::resume::AgentSessionRef,
    shepr_agent::resume::AgentSessionRef,
)> {
    vec![
        (
            Agent::Claude,
            shepr_agent::resume::AgentSessionRef::id("claude-old").expect("session id"),
            shepr_agent::resume::AgentSessionRef::id("claude-new").expect("session id"),
        ),
        (
            Agent::Pi,
            shepr_agent::resume::AgentSessionRef::path(test_session_path("relaunch-old.jsonl"))
                .expect("session path"),
            shepr_agent::resume::AgentSessionRef::path(test_session_path("relaunch-new.jsonl"))
                .expect("session path"),
        ),
    ]
}

#[test]
fn a_relaunch_startup_that_beats_the_exit_is_admitted_with_the_replacement() {
    for (agent, old, new) in relaunch_rows() {
        let now = Instant::now();
        let mut terminal = running_agent_with_session(agent, &old, now);
        let origin = ReportOrigin::official(agent).expect("official integration");
        // `claude; claude`: the new process's startup reaches the server
        // before the probe that sees the new process group.
        let received = now + Duration::from_millis(10);
        assert_eq!(
            terminal.report_session_start_outcome_at(
                &origin,
                Some(new.clone()),
                Some(10),
                startup_source(),
                received,
            ),
            HookOutcome::Parked,
            "{agent}"
        );
        assert_eq!(current_session_ref(&terminal), Some(old.clone()), "{agent}");
        assert_eq!(
            terminal
                .last_unapplied_hook_report(received)
                .map(|report| report.disposition),
            Some(UnappliedHookDisposition::Parked(
                ParkedHookAwaiting::Process {
                    expires_at: received + REPLACEMENT_START_EXIT_WINDOW,
                }
            )),
            "{agent}"
        );

        // That probe reports the old process's exit, which clears its session,
        // and the next one the replacement process.
        terminal.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Idle,
            false,
            true,
            now + Duration::from_millis(300),
        );
        assert_eq!(current_session_ref(&terminal), None, "{agent}");
        let presence =
            terminal.set_detected_agent_process_at(agent, now + Duration::from_millis(600));

        assert!(presence.session_ref_changed, "{agent}");
        assert_eq!(current_session_ref(&terminal), Some(new), "{agent}");
        assert!(terminal.replacement_start.is_none(), "{agent}");
        assert!(
            terminal
                .last_unapplied_hook_report(now + Duration::from_millis(600))
                .is_none(),
            "{agent}"
        );
    }
}

#[test]
fn a_fresh_pi_before_the_restored_one_was_identified_takes_the_pane() {
    // A restored Pi path, and a detector that has not identified any process
    // yet: the restored Pi quit early and a fresh one started. Its startup
    // parks for process evidence, which the first presence supplies.
    let (agent, restored, fresh) = relaunch_rows().remove(1);
    let now = Instant::now();
    let mut terminal = test_terminal();
    let origin = ReportOrigin::official(agent).expect("official Pi");
    terminal.set_persisted_agent_session(
        PersistedAgentSession::new(*origin.source(), restored.clone()).expect("test session"),
    );
    assert_eq!(
        terminal.report_session_start_outcome_at(
            &origin,
            Some(fresh.clone()),
            Some(10),
            startup_source(),
            now + Duration::from_millis(10),
        ),
        HookOutcome::Parked
    );
    assert_eq!(current_session_ref(&terminal), Some(restored));
    terminal.set_detected_agent_process_at(agent, now + Duration::from_millis(500));
    assert_eq!(current_session_ref(&terminal), Some(fresh));
}

#[test]
fn a_full_lifecycle_relaunch_carries_its_reports_parked_after_the_exit() {
    let (agent, old, new) = relaunch_rows().remove(1);
    let now = Instant::now();
    let mut terminal = running_agent_with_session(agent, &old, now);
    terminal.set_hook_authority_at(
        "shepr:pi",
        AgentState::Idle,
        Some(old.clone()),
        Some(5),
        now + Duration::from_millis(1),
    );
    let origin = ReportOrigin::official(agent).expect("official Pi");
    assert_eq!(
        terminal.report_session_start_outcome_at(
            &origin,
            Some(new.clone()),
            Some(10),
            startup_source(),
            now + Duration::from_millis(10),
        ),
        HookOutcome::Parked
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(agent),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(300),
    );
    // A report of the relaunch's session between the exit and the presence
    // waits for a start, which the held one then supplies.
    assert_eq!(
        terminal.report_hook_outcome_at(
            origin,
            AgentState::Working,
            Some(new.clone()),
            Some(11),
            HookClockSample::from(now + Duration::from_millis(350)),
        ),
        HookOutcome::Parked
    );
    terminal.set_detected_agent_process_at(agent, now + Duration::from_millis(600));

    assert_eq!(current_session_ref(&terminal), Some(new.clone()));
    assert!(terminal.full_lifecycle_hook_authority_active());
    assert_eq!(terminal.state, AgentState::Working);
    assert!(
        terminal
            .set_hook_authority_at(
                "shepr:pi",
                AgentState::Idle,
                Some(new),
                Some(12),
                now + Duration::from_millis(700),
            )
            .is_some()
    );
    assert_eq!(terminal.state, AgentState::Idle);
}

#[test]
fn a_nested_startup_is_never_admitted_without_a_relaunch() {
    for (agent, old, nested) in relaunch_rows() {
        let now = Instant::now();
        let mut terminal = running_agent_with_session(agent, &old, now);
        let origin = ReportOrigin::official(agent).expect("official integration");
        let received = now + Duration::from_millis(10);
        assert_eq!(
            terminal.report_session_start_outcome_at(
                &origin,
                Some(nested),
                Some(10),
                startup_source(),
                received,
            ),
            HookOutcome::Parked,
            "{agent}"
        );
        // The pane's own agent keeps running in its process group: the
        // detector reports it present, never gone, until the window ends.
        terminal.set_detected_agent_process_at(agent, now + Duration::from_secs(1));
        assert!(terminal.replacement_start.is_some(), "{agent}");
        let late = received + REPLACEMENT_START_EXIT_WINDOW + Duration::from_millis(1);
        assert_eq!(terminal.last_unapplied_hook_report(late), None, "{agent}");
        terminal.set_detected_agent_process_at(agent, late);
        assert!(terminal.replacement_start.is_none(), "{agent}");
        assert_eq!(current_session_ref(&terminal), Some(old), "{agent}");

        // A later quit and relaunch does not bring the nested session back.
        terminal.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Idle,
            false,
            true,
            late + Duration::from_secs(1),
        );
        terminal.set_detected_agent_process_at(agent, late + Duration::from_millis(1300));
        assert_eq!(current_session_ref(&terminal), None, "{agent}");
    }
}

#[test]
fn a_relaunch_the_detector_confirms_too_late_loses_its_session() {
    let exit_window = REPLACEMENT_START_EXIT_WINDOW;
    let gap = REPLACEMENT_START_PRESENCE_GAP;
    // (exit after the start, presence after the exit)
    for (exit_after, presence_after) in [
        (
            exit_window + Duration::from_millis(1),
            Duration::from_millis(300),
        ),
        (Duration::from_millis(300), gap + Duration::from_millis(1)),
    ] {
        for (agent, old, new) in relaunch_rows() {
            let now = Instant::now();
            let mut terminal = running_agent_with_session(agent, &old, now);
            let origin = ReportOrigin::official(agent).expect("official integration");
            let received = now + Duration::from_millis(10);
            terminal.report_session_start_outcome_at(
                &origin,
                Some(new),
                Some(10),
                startup_source(),
                received,
            );
            let exited_at = received + exit_after;
            terminal.set_detected_state_with_screen_signals_at(
                Some(agent),
                AgentState::Idle,
                false,
                true,
                exited_at,
            );
            terminal.set_detected_agent_process_at(agent, exited_at + presence_after);
            // The old session is gone with its process; the unconfirmed one
            // is never installed in its place.
            assert_eq!(current_session_ref(&terminal), None, "{agent}");
            assert!(terminal.replacement_start.is_none(), "{agent}");
        }
    }
}

#[test]
fn a_later_start_from_the_source_supersedes_a_held_one() {
    let now = Instant::now();
    let old = shepr_agent::resume::AgentSessionRef::id("claude-old").expect("session id");
    let mut terminal = running_agent_with_session(Agent::Claude, &old, now);
    let origin = ReportOrigin::official(Agent::Claude).expect("official Claude");
    for (seq, id) in [(10, "claude-nested"), (11, "claude-relaunch")] {
        assert_eq!(
            terminal.report_session_start_outcome_at(
                &origin,
                shepr_agent::resume::AgentSessionRef::id(id),
                Some(seq),
                startup_source(),
                now + Duration::from_millis(seq),
            ),
            HookOutcome::Parked
        );
    }
    terminal.set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Idle,
        false,
        true,
        now + Duration::from_millis(300),
    );
    terminal.set_detected_agent_process_at(Agent::Claude, now + Duration::from_millis(600));
    assert_eq!(
        current_session_ref(&terminal),
        shepr_agent::resume::AgentSessionRef::id("claude-relaunch")
    );
}

#[test]
fn only_a_startup_against_a_live_process_is_held() {
    let now = Instant::now();
    let old = shepr_agent::resume::AgentSessionRef::id("claude-old").expect("session id");
    let origin = ReportOrigin::official(Agent::Claude).expect("official Claude");
    // An omitted or unknown start source is no process start.
    for source in [
        ReportedSessionStart::Omitted,
        ReportedSessionStart::Unrecognized,
    ] {
        let mut terminal = running_agent_with_session(Agent::Claude, &old, now);
        assert_eq!(
            terminal.report_session_start_outcome_at(
                &origin,
                shepr_agent::resume::AgentSessionRef::id("claude-other"),
                Some(10),
                source,
                now + Duration::from_millis(10),
            ),
            HookOutcome::Rejected(HookRejection::ReplacedSession)
        );
        assert!(terminal.replacement_start.is_none());
    }
    // With no live process held, nothing can confirm a relaunch.
    let mut terminal = test_terminal();
    terminal.set_persisted_agent_session(
        PersistedAgentSession::new(*origin.source(), old.clone()).expect("test session"),
    );
    assert_eq!(
        terminal.report_session_start_outcome_at(
            &origin,
            shepr_agent::resume::AgentSessionRef::id("claude-other"),
            Some(10),
            startup_source(),
            now + Duration::from_millis(10),
        ),
        HookOutcome::Rejected(HookRejection::ReplacedSession)
    );
    assert!(terminal.replacement_start.is_none());
}

#[test]
fn the_pane_ending_discards_a_held_relaunch_start() {
    let now = Instant::now();
    let old = shepr_agent::resume::AgentSessionRef::id("claude-old").expect("session id");
    let mut terminal = running_agent_with_session(Agent::Claude, &old, now);
    let origin = ReportOrigin::official(Agent::Claude).expect("official Claude");
    terminal.report_session_start_outcome_at(
        &origin,
        shepr_agent::resume::AgentSessionRef::id("claude-new"),
        Some(10),
        startup_source(),
        now + Duration::from_millis(10),
    );
    terminal.set_pane_process_exit_at(true, now + Duration::from_millis(20));
    assert!(terminal.replacement_start.is_none());
    // The checkpoint keeps what the pane held when it died.
    assert_eq!(current_session_ref(&terminal), Some(old));
}
