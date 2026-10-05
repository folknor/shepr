//! `shepr detect`: the manifest-maintenance commands. `capture` prints the
//! screen and OSC values the detector evaluates for a pane; `explain` shows a
//! pane's effective state beside the current screen verdict, or evaluates a
//! saved capture locally.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use shepr_api::schema::{
    DetectionCapture, DetectionExplanation, DetectionStateSource, ParkedHookAwaiting, Request,
    ResponseResult, UnappliedHookOutcome, UnappliedHookReport, UnappliedHookReportKind,
};

use super::matches::{try_flag, try_string};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Capture { pane: shepr_protocol::PublicPaneId },
    Explain(ExplainArgs),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ExplainSource {
    Pane(shepr_protocol::PublicPaneId),
    File { path: PathBuf, agent: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExplainArgs {
    pub(super) source: ExplainSource,
    pub(super) json: bool,
    pub(super) verbose: bool,
}

pub(super) fn parse(matches: &ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("capture", command)) => Some(Command::Capture {
            // The spec marks the pane required; `None` means spec and handler disagree.
            pane: super::matches::try_value(command, "pane").ok()??,
        }),
        Some(("explain", command)) => {
            let pane = super::matches::try_value(command, "pane").ok()?;
            let file = try_string(command, "file").ok()?.map(PathBuf::from);
            let agent = try_string(command, "agent").ok()?;
            let source = match (pane, file, agent) {
                (Some(pane), None, None) => ExplainSource::Pane(pane),
                (None, Some(path), Some(agent)) => ExplainSource::File { path, agent },
                _ => return None,
            };
            Some(Command::Explain(ExplainArgs {
                source,
                json: try_flag(command, "json").ok()?,
                verbose: try_flag(command, "verbose").ok()?,
            }))
        }
        _ => None,
    }
}

pub(super) fn run_detect_command(
    command: Command,
    paths: &shepr_paths::AppPaths,
) -> super::CliResult<i32> {
    match command {
        Command::Capture { pane } => capture(paths, &pane),
        Command::Explain(args) => match args.source {
            ExplainSource::Pane(pane) => explain(paths, &pane, args.json, args.verbose),
            ExplainSource::File { path, agent } => {
                run_file_explain(&path, &agent, args.json, args.verbose)
            }
        },
    }
}

/// The request behind `detect capture`: the server answers with the complete
/// detector input for that pane, whether or not an agent is currently detected.
fn capture_request(pane: &shepr_protocol::PublicPaneId) -> Request {
    Request::detect_capture(pane.to_string())
}

fn capture(
    paths: &shepr_paths::AppPaths,
    pane: &shepr_protocol::PublicPaneId,
) -> super::CliResult<i32> {
    let capture = match super::send_request(paths, &capture_request(pane))? {
        ResponseResult::DetectCapture { capture, .. } => capture,
        other => return Err(super::unexpected_result(&other)),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&capture).map_err(std::io::Error::other)?
    );
    Ok(0)
}

pub(super) fn explain(
    paths: &shepr_paths::AppPaths,
    target: &shepr_protocol::PublicPaneId,
    json: bool,
    verbose: bool,
) -> super::CliResult<i32> {
    let request = Request::detect_explain(target.to_string());
    let explain = match super::send_request(paths, &request)? {
        ResponseResult::DetectExplain { explain } => explain,
        other => return Err(super::unexpected_result(&other)),
    };
    print_explain_output(&explain, json, verbose)?;
    Ok(0)
}

/// Evaluates a saved capture in this process. `cli::run` sends a `--file`
/// explain here before it resolves any application paths.
pub(super) fn run_file_explain(
    path: &Path,
    agent_label: &str,
    json: bool,
    verbose: bool,
) -> super::CliResult<i32> {
    let explain = explain_file(path, agent_label)?;
    print_explain_output(&explain, json, verbose)?;
    Ok(0)
}

fn print_explain_output(
    explain: &DetectionExplanation,
    json: bool,
    verbose: bool,
) -> super::CliResult<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(explain).map_err(std::io::Error::other)?
        );
    } else {
        print_explain_text(explain, verbose);
    }
    Ok(())
}

/// Evaluates a saved capture against an agent's compiled manifest. Runs in
/// the CLI process and needs no server.
pub(super) fn explain_file(
    path: &Path,
    agent_label: &str,
) -> super::CliResult<DetectionExplanation> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            return Err(super::CliError::Message(format!(
                "failed to read explain file {}: {err}",
                path.display()
            )));
        }
    };
    let capture: DetectionCapture = serde_json::from_str(&content).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "failed to parse detection capture {}: {error}",
                path.display()
            ),
        )
    })?;
    Ok(shepr_detect::manifest::explain_for_label(
        agent_label,
        shepr_detect::manifest::DetectionInput {
            screen: &capture.screen,
            osc_title: capture.osc_title_evidence(),
            osc_progress: capture.osc_progress_evidence(),
        },
    )
    .into())
}

/// One line saying what became of the report, which report it was and how
/// long ago the server admitted it.
fn unapplied_hook_report_text(report: &UnappliedHookReport) -> String {
    let seconds = |millis: u64| std::time::Duration::from_millis(millis).as_secs_f64();
    let outcome = match report.outcome {
        UnappliedHookOutcome::Parked {
            awaiting: ParkedHookAwaiting::SessionStart,
        } => "parked awaiting a session start".to_owned(),
        UnappliedHookOutcome::Parked {
            awaiting: ParkedHookAwaiting::Process { expires_in_ms },
        } => format!(
            "parked awaiting process evidence (expires in {:.1}s)",
            seconds(expires_in_ms)
        ),
        UnappliedHookOutcome::Rejected { reason } => format!("rejected ({reason})"),
    };
    let kind = match report.report {
        UnappliedHookReportKind::State { state } => format!("state report ({state})"),
        UnappliedHookReportKind::SessionStart {
            start_source: Some(source),
        } => format!("session start ({source})"),
        UnappliedHookReportKind::SessionStart { start_source: None } => "session start".to_owned(),
    };
    let seq = report
        .seq
        .map_or_else(String::new, |seq| format!(" seq={seq}"));
    let session = report
        .session_ref
        .as_ref()
        .map_or_else(String::new, |session_ref| {
            format!(" session={}", session_ref.value_str())
        });
    let age = seconds(report.age_ms);
    format!(
        "{outcome}: {kind} from {}{seq}{session}, {age:.1}s ago",
        report.hook_source
    )
}

pub(super) fn print_explain_text(explain: &DetectionExplanation, verbose: bool) {
    println!("agent: {}", explain.agent);
    println!("state: {}", explain.state);
    match &explain.state_source {
        DetectionStateSource::Screen => println!("state_source: screen"),
        DetectionStateSource::ProcessExit => println!("state_source: process_exit"),
        DetectionStateSource::HookAuthority { hook_source, .. } => {
            println!("state_source: hook_authority ({hook_source})");
        }
    }
    if let Some(screen_state) = explain.screen_state {
        println!("screen_state: {screen_state}");
        if screen_state != explain.state
            && matches!(&explain.state_source, DetectionStateSource::Screen)
            && explain.detector_gate.is_none()
        {
            println!(
                "screen_state_note: differs from the effective state; no active mux gate was reported"
            );
        }
    }
    if let Some(gate) = explain.detector_gate {
        println!("detector_gate: {gate}");
    }
    if let Some(rule) = &explain.matched_rule {
        println!(
            "rule: {} (region={} priority={})",
            rule.id, rule.region, rule.priority
        );
        if let Some(rule) = explain
            .evaluated_rules
            .iter()
            .find(|evaluated| evaluated.id == rule.id)
            && !rule.evidence.region_preview.is_empty()
        {
            println!("evidence: {:?}", rule.evidence.region_preview);
        }
    } else {
        println!("rule: none");
    }
    if let Some(reason) = explain.fallback_reason {
        println!("fallback_reason: {reason}");
    }
    if let DetectionStateSource::HookAuthority { skip_reason, .. } = &explain.state_source {
        println!("screen_detection_skip_reason: {skip_reason}");
    }
    if let Some(shepr_detect::manifest::SkippedUpdateReason::MatchedRule { rule_id }) =
        &explain.skipped_update_reason
    {
        println!("skipped_update_reason: matched_rule:{rule_id}");
    }
    if explain.skip_state_update {
        println!("screen_gate: skip_state_update");
    }
    if let Some(report) = &explain.last_unapplied_hook_report {
        println!(
            "last_unapplied_hook_report: {}",
            unapplied_hook_report_text(report)
        );
    }
    if !verbose {
        return;
    }
    println!(
        "visible: idle={} blocker={} working={}",
        explain.visible_idle, explain.visible_blocker, explain.visible_working
    );
    if !explain.evaluated_rules.is_empty() {
        println!("evaluated_rules:");
        for rule in &explain.evaluated_rules {
            println!(
                "  {} {} priority={} region={} state={}",
                if rule.matched { "\u{2713}" } else { "\u{2717}" },
                rule.id,
                rule.priority,
                rule.region,
                rule.state
            );
            let evidence = &rule.evidence;
            println!(
                "    matchers: contains={:?} regex={:?} line_regex={:?} all={} any={} not={}",
                evidence.contains,
                evidence.regex,
                evidence.line_regex,
                evidence.all_count,
                evidence.any_count,
                evidence.not_count
            );
            println!(
                "    region: bytes={} preview={:?}",
                evidence.region_bytes, evidence.region_preview
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::group_matches;
    use super::*;
    use shepr_api::schema::{Method, SuccessResponse};

    fn command(args: &[&str]) -> Command {
        parse(&group_matches(args)).expect("test precondition")
    }

    fn rejected(args: &[&str]) -> clap::Error {
        let mut argv = vec!["shepr"];
        argv.extend_from_slice(args);
        match super::super::spec::command().try_get_matches_from(argv) {
            Ok(_) => panic!("{args:?} should be rejected"),
            Err(error) => error,
        }
    }

    #[test]
    fn capture_takes_exactly_a_pane_id() {
        assert_eq!(
            command(&["detect", "capture", "w1:p1"]),
            Command::Capture {
                pane: "w1:p1".parse().expect("test precondition")
            }
        );
        for args in [
            &["detect", "capture"][..],
            &["detect", "capture", "w1:p1", "w1:p2"],
        ] {
            assert_eq!(rejected(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn capture_rejects_the_removed_read_switches() {
        for switch in [
            &["--source", "recent"][..],
            &["--source=detection"],
            &["--lines", "5"],
            &["--format", "text"],
            &["--ansi"],
        ] {
            let mut args = vec!["detect", "capture", "w1:p1"];
            args.extend_from_slice(switch);
            assert_eq!(rejected(&args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn live_targets_are_pane_ids_not_agent_names() {
        for args in [
            &["detect", "capture", "reviewer"][..],
            &["detect", "capture", "codex"],
            &["detect", "explain", "reviewer"],
            &["detect", "explain", "p_12"],
            &["detect", "explain", "w1:t1"],
        ] {
            assert_eq!(rejected(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn explain_grammar() {
        let Command::Explain(args) = command(&["detect", "explain", "w1:p1"]) else {
            panic!("expected detect explain");
        };
        assert!(matches!(
            args.source,
            ExplainSource::Pane(ref pane) if pane.to_string() == "w1:p1"
        ));
        assert!(!args.json && !args.verbose);

        let Command::Explain(args) = command(&["detect", "explain", "w1:p1", "--json", "-v"])
        else {
            panic!("expected detect explain");
        };
        assert!(args.json && args.verbose);

        let Command::Explain(args) = command(&[
            "detect",
            "explain",
            "--file",
            "screen.txt",
            "--agent",
            "codex",
            "--verbose",
        ]) else {
            panic!("expected detect explain");
        };
        assert!(matches!(
            args.source,
            ExplainSource::File { ref path, ref agent }
                if path.as_path() == Path::new("screen.txt") && agent == "codex"
        ));
        assert!(args.verbose);
    }

    #[test]
    fn explain_rejects_bad_combinations_and_the_old_format_switch() {
        for args in [
            &["detect", "explain"][..],
            &["detect", "explain", "--file", "screen.txt"],
            &["detect", "explain", "--agent", "codex"],
            &[
                "detect", "explain", "w1:p1", "--file", "s.txt", "--agent", "codex",
            ],
            &["detect", "explain", "w1:p1", "--format", "json"],
        ] {
            assert_eq!(rejected(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn capture_request_names_the_pane_and_nothing_else() {
        let pane = "w1:p1".parse().expect("valid pane id");
        let request = capture_request(&pane);
        let Method::DetectCapture(target) = request.method.clone() else {
            panic!("capture should use detect.capture");
        };
        assert_eq!(target.pane_id, "w1:p1");
        let json = serde_json::to_value(&request).expect("test precondition");
        assert_eq!(json["method"], "detect.capture");
        assert_eq!(json["params"], serde_json::json!({ "pane_id": "w1:p1" }));
    }

    #[test]
    fn renamed_explanation_fields_are_a_decode_error() {
        let explain: DetectionExplanation = shepr_detect::manifest::explain_for_label(
            "codex",
            shepr_detect::manifest::DetectionInput {
                screen: "",
                osc_title: None,
                osc_progress: None,
            },
        )
        .into();
        let mut response = serde_json::to_value(SuccessResponse {
            id: "test".into(),
            result: ResponseResult::DetectExplain {
                explain: Box::new(explain),
            },
        })
        .expect("encode explanation response");
        assert!(shepr_api::client::parse_response_value(response.clone()).is_ok());
        let fields = response["result"]["explain"]
            .as_object_mut()
            .expect("explanation object");
        let state = fields.remove("state").expect("state field");
        fields.insert("renamed_state".into(), state);
        assert!(shepr_api::client::parse_response_value(response).is_err());
    }

    #[test]
    fn unapplied_hook_reports_render_their_outcome_report_and_age() {
        let mut report: UnappliedHookReport = serde_json::from_value(serde_json::json!({
            "hook_source": "shepr:codex",
            "agent": "codex",
            "report": { "kind": "state", "state": "idle" },
            "seq": 4,
            "session_ref": { "kind": "id", "value": "codex-session" },
            "received_unix_ms": 1_700_000_000_000_u64,
            "age_ms": 3_240,
            "outcome": { "kind": "rejected", "reason": "out_of_order" },
        }))
        .expect("decode report");
        assert_eq!(
            unapplied_hook_report_text(&report),
            "rejected (out_of_order): state report (idle) from shepr:codex seq=4 \
             session=codex-session, 3.2s ago"
        );

        let parked: UnappliedHookReport = serde_json::from_value(serde_json::json!({
            "hook_source": "shepr:kimi",
            "agent": "kimi",
            "report": { "kind": "state", "state": "working" },
            "seq": 1_700_000_000_000_u64,
            "session_ref": { "kind": "id", "value": "kimi-root" },
            "received_unix_ms": 1_700_000_000_000_u64,
            "age_ms": 312_400,
            "outcome": { "kind": "parked", "awaiting": { "kind": "session_start" } },
        }))
        .expect("decode parked report");
        assert_eq!(
            unapplied_hook_report_text(&parked),
            "parked awaiting a session start: state report (working) from shepr:kimi \
             seq=1700000000000 session=kimi-root, 312.4s ago"
        );

        report.report = UnappliedHookReportKind::SessionStart {
            start_source: Some(shepr_api::schema::ReportedStartSource::Startup),
        };
        report.seq = None;
        report.session_ref = None;
        report.outcome = UnappliedHookOutcome::Parked {
            awaiting: ParkedHookAwaiting::Process {
                expires_in_ms: 41_000,
            },
        };
        assert_eq!(
            unapplied_hook_report_text(&report),
            "parked awaiting process evidence (expires in 41.0s): session start (startup) \
             from shepr:codex, 3.2s ago"
        );
    }

    #[test]
    fn explain_file_evaluates_without_a_server() {
        let scratch = crate::test_support::ScratchDir::new("detect-explain-file");
        let path = scratch.join("capture.json");
        let capture = DetectionCapture {
            screen: "press enter to confirm or esc to cancel".into(),
            osc_title: String::new(),
            osc_progress: String::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&capture).expect("encode capture"))
            .expect("write capture");

        let explain =
            explain_file(&path, "codex").expect("file evaluation should not need a server");
        assert_eq!(explain.state, shepr_api::schema::PaneAgentState::Blocked);
        assert_eq!(
            explain.matched_rule.as_ref().expect("matched rule").id,
            "live_strong_blocker"
        );

        let missing = scratch.join("missing.txt");
        assert!(explain_file(&missing, "codex").is_err());
    }

    #[test]
    fn explain_file_uses_the_captured_osc_title() {
        let scratch = crate::test_support::ScratchDir::new("detect-explain-osc-file");
        let path = scratch.join("capture.json");
        let capture = DetectionCapture {
            screen: "screen without a blocker".into(),
            osc_title: "Action Required".into(),
            osc_progress: "4;3;".into(),
        };
        std::fs::write(&path, serde_json::to_vec(&capture).expect("encode capture"))
            .expect("write capture");

        let explain =
            explain_file(&path, "codex").expect("file evaluation should not need a server");
        assert_eq!(explain.state, shepr_api::schema::PaneAgentState::Blocked);
        assert_eq!(
            explain.matched_rule.as_ref().expect("matched rule").id,
            "osc_title_blocked"
        );
    }

    #[test]
    fn explain_file_uses_the_captured_osc_progress() {
        let scratch = crate::test_support::ScratchDir::new("detect-explain-progress-file");
        let path = scratch.join("capture.json");
        let capture = DetectionCapture {
            screen: "screen without a blocker".into(),
            osc_title: String::new(),
            osc_progress: "4;3;".into(),
        };
        std::fs::write(&path, serde_json::to_vec(&capture).expect("encode capture"))
            .expect("write capture");

        let explain =
            explain_file(&path, "letta").expect("file evaluation should not need a server");
        assert_eq!(explain.state, shepr_api::schema::PaneAgentState::Blocked);
        assert_eq!(
            explain.matched_rule.as_ref().expect("matched rule").id,
            "osc_progress_blocked"
        );
    }
}
