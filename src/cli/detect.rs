//! `shepr detect`: the manifest-maintenance commands. `capture` prints the
//! screen and OSC values the detector evaluates for a pane; `explain` shows
//! which rule decided a pane's state, or evaluates a saved capture locally.

use clap::ArgMatches;

use shepr_api::schema::{DetectionCapture, ErrorBody, ErrorResponse, Method, PaneTarget, Request};

use super::matches::{flag, string};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Capture { pane: String },
    Explain(ExplainArgs),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExplainArgs {
    /// A live target. Only a pane id passes the spec's value parser.
    pub(super) pane: Option<String>,
    pub(super) file: Option<String>,
    pub(super) agent: Option<String>,
    pub(super) json: bool,
    pub(super) verbose: bool,
}

pub(super) fn parse(matches: &ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("capture", command)) => Some(Command::Capture {
            // The spec marks the pane required; `None` means spec and handler disagree.
            pane: string(command, "pane")?,
        }),
        Some(("explain", command)) => Some(Command::Explain(ExplainArgs {
            pane: string(command, "pane"),
            file: string(command, "file"),
            agent: string(command, "agent"),
            json: flag(command, "json"),
            verbose: flag(command, "verbose"),
        })),
        _ => None,
    }
}

pub(super) fn run_detect_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::Capture { pane } => capture(paths, &pane),
        Command::Explain(args) => explain(paths, args),
    }
}

/// The request behind `detect capture`: the server answers with the complete
/// detector input for that pane, whether or not an agent is currently detected.
fn capture_request(pane: &str) -> Request {
    Request {
        id: "cli:detect:capture".into(),
        method: Method::DetectCapture(PaneTarget {
            pane_id: pane.to_owned(),
        }),
    }
}

fn capture(paths: &super::target::CliContext, pane: &str) -> super::CliResult<i32> {
    let response = super::send_request(paths, &capture_request(pane))?;
    if response.get("error").is_some() {
        print_detect_error(&response)?;
        return Ok(1);
    }
    let capture: DetectionCapture = serde_json::from_value(response["result"]["capture"].clone())
        .map_err(std::io::Error::other)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&capture).map_err(std::io::Error::other)?
    );
    Ok(0)
}

pub(super) fn explain(
    paths: &super::target::CliContext,
    args: ExplainArgs,
) -> super::CliResult<i32> {
    let explain = if let Some(path) = args.file {
        let agent_label = args
            .agent
            .ok_or_else(|| super::CliError::Usage("--file requires --agent".into()))?;
        explain_file(&path, &agent_label)?
    } else {
        let target = args.pane.ok_or_else(|| {
            super::CliError::Usage("explain requires PANE unless --file is used".into())
        })?;
        let response = super::send_request(
            paths,
            &Request {
                id: "cli:detect:explain".into(),
                method: Method::DetectExplain(PaneTarget { pane_id: target }),
            },
        )?;
        if response.get("error").is_some() {
            print_detect_error(&response)?;
            return Ok(1);
        }
        response["result"]["explain"].clone()
    };

    if args.json {
        println!("{explain}");
    } else {
        print_explain_text(&explain, args.verbose);
    }
    Ok(0)
}

fn print_detect_error(response: &serde_json::Value) -> super::CliResult<()> {
    if response["error"]["code"] == "pane_terminal_unavailable"
        && let Some(message) = response["error"]["message"].as_str()
    {
        eprintln!("{message}");
    } else {
        eprintln!(
            "{}",
            serde_json::to_string(response).map_err(std::io::Error::other)?
        );
    }
    Ok(())
}

/// Evaluates a saved capture against an agent's compiled manifest. Runs in
/// the CLI process and needs no server.
pub(super) fn explain_file(path: &str, agent_label: &str) -> super::CliResult<serde_json::Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            return Err(super::CliError::Response(ErrorResponse {
                id: "cli:detect:explain".into(),
                error: ErrorBody::new(
                    &shepr_api::error::ApiErrorCode::AgentExplainFileReadFailed,
                    format!("failed to read explain file {path}: {err}"),
                ),
            }));
        }
    };
    let capture: DetectionCapture = serde_json::from_str(&content).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to parse detection capture {path}: {error}"),
        )
    })?;
    Ok(shepr_agent::detect::manifest::explain_to_json_value(
        &shepr_agent::detect::manifest::explain_for_label(
            agent_label,
            shepr_agent::detect::manifest::DetectionInput {
                screen: &capture.screen,
                osc_title: &capture.osc_title,
                osc_progress: &capture.osc_progress,
            },
        ),
    ))
}

pub(super) fn print_explain_text(explain: &serde_json::Value, verbose: bool) {
    println!("agent: {}", explain["agent"].as_str().unwrap_or("unknown"));
    println!("state: {}", explain["state"].as_str().unwrap_or("unknown"));
    if let Some(rule) = explain["matched_rule"].as_object() {
        let rule_id = rule
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("-");
        println!(
            "rule: {} (region={} priority={})",
            rule_id,
            rule.get("region")
                .and_then(|value| value.as_str())
                .unwrap_or("-"),
            rule.get("priority")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
        );
        if let Some(preview) = matched_rule_region_preview(explain, rule_id) {
            println!("evidence: {preview:?}");
        }
    } else {
        println!("rule: none");
    }
    if let Some(reason) = explain["fallback_reason"].as_str() {
        println!("fallback_reason: {reason}");
    }
    if let Some(reason) = explain["screen_detection_skip_reason"].as_str() {
        println!("screen_detection_skip_reason: {reason}");
    }
    if let Some(reason) = explain["skipped_update_reason"].as_str() {
        println!("skipped_update_reason: {reason}");
    }

    if !verbose {
        return;
    }

    println!(
        "visible: idle={} blocker={} working={}",
        explain["visible_idle"].as_bool().unwrap_or(false),
        explain["visible_blocker"].as_bool().unwrap_or(false),
        explain["visible_working"].as_bool().unwrap_or(false)
    );
    if let Some(evaluated_rules) = explain["evaluated_rules"]
        .as_array()
        .filter(|rules| !rules.is_empty())
    {
        println!("evaluated_rules:");
        for rule in evaluated_rules {
            println!(
                "  {} {} priority={} region={} state={}",
                if rule["matched"].as_bool().unwrap_or(false) {
                    "\u{2713}"
                } else {
                    "\u{2717}"
                },
                rule["id"].as_str().unwrap_or("-"),
                rule["priority"].as_i64().unwrap_or(0),
                rule["region"].as_str().unwrap_or("-"),
                rule["state"].as_str().unwrap_or("unknown")
            );
            let evidence = &rule["evidence"];
            println!(
                "    matchers: contains={:?} regex={:?} line_regex={:?} all={} any={} not={}",
                evidence["contains"],
                evidence["regex"],
                evidence["line_regex"],
                evidence["all_count"].as_u64().unwrap_or(0),
                evidence["any_count"].as_u64().unwrap_or(0),
                evidence["not_count"].as_u64().unwrap_or(0)
            );
            println!(
                "    region: bytes={} preview={:?}",
                evidence["region_bytes"].as_u64().unwrap_or(0),
                evidence["region_preview"].as_str().unwrap_or("")
            );
        }
    }
}

fn matched_rule_region_preview<'a>(
    explain: &'a serde_json::Value,
    rule_id: &str,
) -> Option<&'a str> {
    explain["evaluated_rules"]
        .as_array()?
        .iter()
        .find(|rule| rule["id"].as_str() == Some(rule_id))?["evidence"]["region_preview"]
        .as_str()
        .filter(|preview| !preview.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::tests::group_matches;
    use super::*;

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
                pane: "w1:p1".into()
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
        assert_eq!(args.pane.as_deref(), Some("w1:p1"));
        assert!(!args.json && !args.verbose && args.file.is_none());

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
        assert_eq!(args.file.as_deref(), Some("screen.txt"));
        assert_eq!(args.agent.as_deref(), Some("codex"));
        assert_eq!(args.pane, None);
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
        let request = capture_request("w1:p1");
        let Method::DetectCapture(target) = request.method.clone() else {
            panic!("capture should use detect.capture");
        };
        assert_eq!(target.pane_id, "w1:p1");
        let json = serde_json::to_value(&request).expect("test precondition");
        assert_eq!(json["method"], "detect.capture");
        assert_eq!(json["params"], serde_json::json!({ "pane_id": "w1:p1" }));
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

        let explain = explain_file(path.to_str().expect("utf8 path"), "codex")
            .expect("file evaluation should not need a server");
        assert_eq!(explain["state"], "blocked");
        assert_eq!(explain["matched_rule"]["id"], "live_strong_blocker");

        let missing = scratch.join("missing.txt");
        assert!(explain_file(missing.to_str().expect("utf8 path"), "codex").is_err());
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

        let explain = explain_file(path.to_str().expect("utf8 path"), "codex")
            .expect("file evaluation should not need a server");
        assert_eq!(explain["state"], "blocked");
        assert_eq!(explain["matched_rule"]["id"], "osc_title_blocked");
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

        let explain = explain_file(path.to_str().expect("utf8 path"), "letta")
            .expect("file evaluation should not need a server");
        assert_eq!(explain["state"], "blocked");
        assert_eq!(explain["matched_rule"]["id"], "osc_progress_blocked");
    }
}
