use std::time::{Duration, Instant};

use clap::ArgMatches;

use crate::api::schema::{
    AgentPromptParams, AgentPromptWaitOptions, AgentReadParams, AgentRenameParams,
    AgentSendKeysParams, AgentStartParams, AgentStatus, AgentTarget, AgentWaitParams, EmptyParams,
    ErrorBody, ErrorResponse, Method, PaneProcessInfoParams, PaneTarget, ReadFormat, ReadSource,
    Request,
};

use super::matches::{flag, required, string, value, values};

const AGENT_START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PANE_SHELL_READINESS_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_AGENT_START_TIMEOUT_MS: u64 = 30_000;

pub(super) fn run_agent_command(
    matches: &ArgMatches,
    config: Option<crate::config::Config>,
    paths: &crate::config::AppPaths,
) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("list", _)) => agent_list(paths),
        Some(("get", matches)) => agent_get(paths, required(matches, "target")),
        Some(("read", matches)) => agent_read(paths, read_params(matches)),
        Some(("send-keys", matches)) => agent_send_keys(
            paths,
            AgentSendKeysParams {
                target: required(matches, "target"),
                keys: values::<String>(matches, "key"),
            },
        ),
        Some(("prompt", matches)) => agent_prompt(paths, prompt_params(matches)),
        Some(("rename", matches)) => agent_rename(
            paths,
            AgentRenameParams {
                target: required(matches, "target"),
                name: string(matches, "name"),
            },
        ),
        Some(("focus", matches)) => agent_focus(paths, required(matches, "target")),
        Some(("wait", matches)) => agent_wait(
            paths,
            AgentWaitParams {
                target: required(matches, "target"),
                until: values::<AgentStatus>(matches, "until"),
                timeout_ms: value::<u64>(matches, "timeout"),
            },
        ),
        Some(("attach", matches)) => agent_attach(
            &required(matches, "target"),
            flag(matches, "takeover"),
            config,
            paths,
        ),
        Some(("start", matches)) => agent_start(paths, matches),
        Some(("explain", matches)) => agent_explain(paths, matches),
        _ => Ok(super::missing_subcommand()),
    }
}

fn agent_explain(paths: &crate::config::AppPaths, matches: &ArgMatches) -> std::io::Result<i32> {
    // The spec enforces the two forms: `<TARGET>` alone, or `--file` together
    // with `--agent`.
    let json = flag(matches, "json") || string(matches, "format").as_deref() == Some("json");
    let verbose = flag(matches, "verbose");

    let explain = if let Some(path) = string(matches, "file") {
        let agent_label = required(matches, "agent");
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                let response = ErrorResponse {
                    id: "cli:agent:explain".into(),
                    error: ErrorBody {
                        code: "agent_explain_file_read_failed".into(),
                        message: format!("failed to read agent explain file {path}: {err}"),
                    },
                };
                let response = serde_json::to_string(&response).map_err(std::io::Error::other)?;
                eprintln!("{response}");
                return Ok(1);
            }
        };
        crate::detect::manifest::explain_to_json_value(&crate::detect::manifest::explain_for_label(
            &agent_label,
            &content,
        ))
    } else {
        let response = super::send_request(
            paths,
            &Request {
                id: "cli:agent:explain".into(),
                method: Method::AgentExplain(AgentTarget {
                    target: required(matches, "target"),
                }),
            },
        )?;
        if response.get("error").is_some() {
            eprintln!(
                "{}",
                serde_json::to_string(&response).map_err(std::io::Error::other)?
            );
            return Ok(1);
        }
        response["result"]["explain"].clone()
    };

    if json {
        println!("{explain}");
    } else {
        print_agent_explain_text(&explain, verbose);
    }
    Ok(0)
}

fn print_agent_explain_text(explain: &serde_json::Value, verbose: bool) {
    println!("agent: {}", explain["agent"].as_str().unwrap_or("unknown"));
    println!("state: {}", explain["state"].as_str().unwrap_or("unknown"));
    println!(
        "manifest: {}",
        explain["manifest_source"].as_str().unwrap_or("none")
    );
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
    if let Some(warning) = explain["warning"].as_str() {
        println!("warning: {warning}");
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

fn agent_start(paths: &crate::config::AppPaths, matches: &ArgMatches) -> std::io::Result<i32> {
    let name = &required(matches, "name");
    let kind = required(matches, "kind");
    let pane_id = required(matches, "pane");
    let timeout_ms = value::<u64>(matches, "timeout");
    // `--kind` is limited to the agent labels by the spec; this maps the label
    // to its canonical spelling for comparison with the detected agent.
    let Some(expected_kind) = crate::detect::parse_agent_label(&kind) else {
        return Ok(super::usage_error(&format!(
            "unsupported interactive agent kind: {kind}"
        )));
    };
    let expected_kind = crate::detect::agent_label(expected_kind).to_string();
    let agent_args = values::<String>(matches, "agent_args");
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_AGENT_START_TIMEOUT_MS));
    let retryable_timeout = timeout > crate::app::AGENT_START_SETTLE_DELAY
        && timeout <= crate::app::MAX_AGENT_START_TIMEOUT;
    let pinned_terminal_id = pane_terminal_id(paths, &pane_id)?;
    let mut retry_deadline = None;
    let mut previous_busy_response = None;
    let mut response = loop {
        if let Some(previous_busy_response) = previous_busy_response.as_ref() {
            let retry_expired = retry_deadline.is_some_and(|deadline| Instant::now() >= deadline);
            if retry_expired
                || pane_terminal_id(paths, &pane_id)? != pinned_terminal_id
                || !pane_shell_is_initializing(paths, &pane_id)?
            {
                return super::print_response(previous_busy_response);
            }
        }

        let response = super::send_request(
            paths,
            &Request {
                id: "cli:agent:start".into(),
                method: Method::AgentStart(AgentStartParams {
                    name: name.clone(),
                    kind: kind.clone(),
                    pane_id: pane_id.clone(),
                    args: agent_args.clone(),
                    timeout_ms,
                }),
            },
        )?;
        if response.get("error").is_none() {
            break response;
        }
        if response["error"]["code"].as_str() != Some("agent_pane_busy")
            || !retryable_timeout
            || pinned_terminal_id.is_none()
            || pane_terminal_id(paths, &pane_id)? != pinned_terminal_id
            || !pane_shell_is_initializing(paths, &pane_id)?
        {
            return super::print_response(&response);
        }

        let deadline = *retry_deadline
            .get_or_insert_with(|| Instant::now() + PANE_SHELL_READINESS_RETRY_TIMEOUT);
        previous_busy_response = Some(response);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero()
            && let Some(previous_busy_response) = previous_busy_response.as_ref()
        {
            return super::print_response(previous_busy_response);
        }
        std::thread::sleep(AGENT_START_POLL_INTERVAL.min(remaining));
    };

    let Some(expected_terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        return super::print_response(&cli_agent_error(
            "cli:agent:start",
            "agent_start_failed",
            "agent start response did not include terminal_id",
        ));
    };
    if pinned_terminal_id
        .as_deref()
        .is_some_and(|pinned| pinned != expected_terminal_id)
    {
        return super::print_response(&agent_name_lost_error("cli:agent:start", name));
    }
    let waited = wait_for_named_agent(
        paths,
        name,
        &pane_id,
        timeout,
        &expected_kind,
        expected_terminal_id,
    );
    match waited {
        Ok(Ok(agent)) => {
            response["result"]["agent"] = agent;
            super::print_response(&response)
        }
        Ok(Err(error)) => super::print_response(&error),
        Err(err) => {
            print_agent_transport_error(&err, "cli:agent:start", "agent_start_transport_failed")
        }
    }
}

fn agent_list(paths: &crate::config::AppPaths) -> std::io::Result<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:list".into(),
            method: Method::AgentList(EmptyParams::default()),
        },
    )?)
}

fn agent_get(paths: &crate::config::AppPaths, target: String) -> std::io::Result<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:get".into(),
            method: Method::AgentGet(AgentTarget { target }),
        },
    )?)
}

fn agent_focus(paths: &crate::config::AppPaths, target: String) -> std::io::Result<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:focus".into(),
            method: Method::AgentFocus(AgentTarget { target }),
        },
    )?)
}

fn agent_attach(
    target: &str,
    takeover: bool,
    config: Option<crate::config::Config>,
    paths: &crate::config::AppPaths,
) -> std::io::Result<i32> {
    let config = match config {
        Some(config) => config,
        None => super::load_validated_config(paths)?,
    };
    let response = resolve_agent_target(paths, target, "cli:agent:attach:resolve")?;
    if response.get("error").is_some() {
        eprintln!(
            "{}",
            serde_json::to_string(&response).map_err(std::io::Error::other)?
        );
        return Ok(1);
    }
    let Some(terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        eprintln!("agent attach failed: response did not include terminal_id");
        return Ok(1);
    };
    crate::client::run_terminal_attach(&config, paths, terminal_id.to_owned(), takeover)?;
    Ok(0)
}

fn agent_wait(paths: &crate::config::AppPaths, params: AgentWaitParams) -> std::io::Result<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:wait".into(),
            method: Method::AgentWait(params),
        },
    )?)
}

fn wait_for_named_agent(
    paths: &crate::config::AppPaths,
    name: &str,
    fallback_pane_id: &str,
    timeout: Duration,
    expected_kind: &str,
    expected_terminal_id: &str,
) -> std::io::Result<Result<serde_json::Value, serde_json::Value>> {
    let deadline = Instant::now().checked_add(timeout);
    let mut first_poll = true;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Let the server reconcile its matching startup deadline before
            // returning so the pending name is immediately reusable.
            let _ = resolve_agent_target_unchecked(paths, name, "cli:agent:start:timeout");
            return Ok(Err(agent_wait_timeout()));
        }
        let poll_id = "cli:agent:start";
        let mut response = if first_poll {
            first_poll = false;
            resolve_agent_target(paths, name, poll_id)?
        } else {
            resolve_agent_target_unchecked(paths, name, poll_id)?
        };
        if response.get("error").is_some() {
            response = resolve_agent_target_unchecked(paths, fallback_pane_id, poll_id)?;
            if response.get("error").is_some() {
                std::thread::sleep(AGENT_START_POLL_INTERVAL);
                continue;
            }
        }
        let agent = &response["result"]["agent"];
        let outcome = if agent["terminal_id"].as_str() != Some(expected_terminal_id) {
            Some(Err(agent_name_lost_error("cli:agent:start", name)))
        } else if let Some(actual) = agent["agent"]
            .as_str()
            .filter(|actual| *actual != expected_kind)
        {
            Some(Err(cli_agent_error(
                "cli:agent:start",
                "agent_kind_mismatch",
                format!("expected {expected_kind}, detected {actual}"),
            )))
        } else if agent["name"].as_str() != Some(name) {
            Some(Err(agent_name_lost_error("cli:agent:start", name)))
        } else {
            match agent["agent_status"].as_str() {
                Some("blocked") => Some(Err(cli_agent_error(
                    "cli:agent:start",
                    "agent_not_ready",
                    format!("agent {name} is blocked during startup and is not ready for prompts"),
                ))),
                Some("working") => None,
                Some("idle") if agent["interactive_ready"].as_bool() == Some(true) => {
                    Some(Ok(agent.clone()))
                }
                Some("idle") if !agent["launch_pending"].as_bool().unwrap_or(false) => {
                    Some(Err(cli_agent_error(
                        "cli:agent:start",
                        "agent_start_failed",
                        "agent process exited before becoming interactive",
                    )))
                }
                _ => None,
            }
        };
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
        std::thread::sleep(AGENT_START_POLL_INTERVAL);
    }
}

fn pane_terminal_id(
    paths: &crate::config::AppPaths,
    pane_id: &str,
) -> std::io::Result<Option<String>> {
    let response = super::send_request(
        paths,
        &Request {
            id: "cli:agent:start:pane".into(),
            method: Method::PaneGet(PaneTarget {
                pane_id: pane_id.to_owned(),
            }),
        },
    )?;
    Ok(response["result"]["pane"]["terminal_id"]
        .as_str()
        .map(str::to_owned))
}

fn pane_shell_is_initializing(
    paths: &crate::config::AppPaths,
    pane_id: &str,
) -> std::io::Result<bool> {
    let response = super::send_request(
        paths,
        &Request {
            id: "cli:agent:start:process_info".into(),
            method: Method::PaneProcessInfo(PaneProcessInfoParams {
                pane_id: Some(pane_id.to_owned()),
            }),
        },
    )?;
    Ok(process_info_shows_shell_initialization(
        &response["result"]["process_info"],
    ))
}

fn process_info_shows_shell_initialization(process_info: &serde_json::Value) -> bool {
    let Some(shell_pid) = process_info["shell_pid"].as_u64() else {
        return false;
    };
    if process_info["foreground_process_group_id"].as_u64() != Some(shell_pid) {
        return false;
    }
    process_info["foreground_processes"]
        .as_array()
        .is_some_and(|processes| {
            processes.iter().any(|process| {
                process["pid"].as_u64() == Some(shell_pid)
                    && (process["name"]
                        .as_str()
                        .is_some_and(crate::platform::is_pane_shell_process_name)
                        || process["argv"]
                            .as_array()
                            .and_then(|argv| argv.first())
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(crate::platform::is_pane_shell_process_name))
            })
        })
}

fn agent_name_lost_error(request_id: &str, expected_name: &str) -> serde_json::Value {
    cli_agent_error(
        request_id,
        "agent_name_not_found",
        format!("named agent {expected_name} no longer owns the target terminal"),
    )
}

fn print_agent_transport_error(
    err: &std::io::Error,
    request_id: &str,
    code: &str,
) -> std::io::Result<i32> {
    if super::protocol_mismatch_was_reported(err) {
        return Ok(1);
    }
    // A dead-server marker reaches here from `send_request` in the agent
    // startup path; surface its deferred response exactly once instead of
    // printing a second, generic transport-error line.
    if let Some(response) = super::server_not_running_reported_response(err) {
        let value = serde_json::to_value(response).map_err(std::io::Error::other)?;
        return super::print_response(&value);
    }
    super::print_response(&cli_agent_error(request_id, code, err.to_string()))
}

fn agent_wait_timeout() -> serde_json::Value {
    cli_agent_error(
        "cli:agent:start",
        "timeout",
        "timed out waiting for agent startup",
    )
}

fn cli_agent_error(id: &str, code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn resolve_agent_target(
    paths: &crate::config::AppPaths,
    target: &str,
    request_id: &str,
) -> std::io::Result<serde_json::Value> {
    super::send_request(paths, &agent_get_request(target, request_id))
}

fn resolve_agent_target_unchecked(
    paths: &crate::config::AppPaths,
    target: &str,
    request_id: &str,
) -> std::io::Result<serde_json::Value> {
    super::send_request_unchecked(paths, &agent_get_request(target, request_id))
}

fn agent_get_request(target: &str, request_id: &str) -> Request {
    Request {
        id: request_id.into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    }
}

fn agent_rename(
    paths: &crate::config::AppPaths,
    params: AgentRenameParams,
) -> std::io::Result<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:rename".into(),
            method: Method::AgentRename(params),
        },
    )?)
}

fn prompt_params(matches: &ArgMatches) -> AgentPromptParams {
    // `--until` and `--timeout` require `--wait` in the spec.
    AgentPromptParams {
        target: required(matches, "target"),
        text: required(matches, "text"),
        wait: flag(matches, "wait").then(|| AgentPromptWaitOptions {
            until: values::<AgentStatus>(matches, "until"),
            timeout_ms: value::<u64>(matches, "timeout"),
            submission_deadline: None,
        }),
    }
}

fn agent_prompt(
    paths: &crate::config::AppPaths,
    params: AgentPromptParams,
) -> std::io::Result<i32> {
    let response = super::send_request(
        paths,
        &Request {
            id: "cli:agent:prompt".into(),
            method: Method::AgentPrompt(params),
        },
    )?;
    super::print_response(&response)
}

fn agent_send_keys(
    paths: &crate::config::AppPaths,
    params: AgentSendKeysParams,
) -> std::io::Result<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:send-keys".into(),
            method: Method::AgentSendKeys(params),
        },
    )?)
}

fn read_params(matches: &ArgMatches) -> AgentReadParams {
    // `--ansi` conflicts with `--format` in the spec; either selects ANSI, and
    // an ANSI read of an agent keeps its escapes.
    let format = if flag(matches, "ansi") {
        ReadFormat::Ansi
    } else {
        value::<ReadFormat>(matches, "format").unwrap_or(ReadFormat::Text)
    };
    AgentReadParams {
        target: required(matches, "target"),
        source: value::<ReadSource>(matches, "source").unwrap_or(ReadSource::Recent),
        lines: value::<u32>(matches, "lines"),
        format,
        strip_ansi: format != ReadFormat::Ansi,
    }
}

fn agent_read(paths: &crate::config::AppPaths, params: AgentReadParams) -> std::io::Result<i32> {
    let response = super::send_request(
        paths,
        &Request {
            id: "cli:agent:read".into(),
            method: Method::AgentRead(params),
        },
    )?;
    super::print_read_response(&response)
}

#[cfg(test)]
mod parse_tests {
    use super::super::tests::command_matches;
    use crate::api::schema::{AgentStatus, ReadFormat, ReadSource};

    #[test]
    fn read_defaults_and_ansi_forms() {
        let params = super::read_params(&command_matches(&["agent", "read", "worker"]));
        assert_eq!(params.target, "worker");
        assert_eq!(params.source, ReadSource::Recent);
        assert_eq!(params.format, ReadFormat::Text);
        assert!(params.strip_ansi);

        for form in [&["--ansi"][..], &["--format", "ansi"], &["--format=ansi"]] {
            let mut args = vec![
                "agent",
                "read",
                "worker",
                "--source=detection",
                "--lines",
                "7",
            ];
            args.extend_from_slice(form);
            let params = super::read_params(&command_matches(&args));
            assert_eq!(params.source, ReadSource::Detection);
            assert_eq!(params.lines, Some(7));
            assert_eq!(params.format, ReadFormat::Ansi);
            assert!(!params.strip_ansi);
        }
    }

    #[test]
    fn prompt_wait_options_only_with_wait() {
        let params = super::prompt_params(&command_matches(&[
            "agent",
            "prompt",
            "worker",
            "--help me",
            "--wait",
            "--until",
            "idle",
            "--until=blocked",
            "--timeout",
            "500",
        ]));
        assert_eq!(params.text, "--help me");
        let wait = params.wait.expect("test precondition");
        assert_eq!(wait.until, vec![AgentStatus::Idle, AgentStatus::Blocked]);
        assert_eq!(wait.timeout_ms, Some(500));

        let params = super::prompt_params(&command_matches(&["agent", "prompt", "w", "hi"]));
        assert!(params.wait.is_none());
    }

    #[test]
    fn start_collects_agent_args_after_separator() {
        let start = command_matches(&[
            "agent",
            "start",
            "repro",
            "--pane",
            "p1",
            "--kind",
            "claude",
            "--",
            "--resume",
            "--session",
            "x",
        ]);
        assert_eq!(super::required(&start, "name"), "repro");
        assert_eq!(super::required(&start, "pane"), "p1");
        assert_eq!(
            super::values::<String>(&start, "agent_args"),
            vec!["--resume", "--session", "x"]
        );
    }
}
