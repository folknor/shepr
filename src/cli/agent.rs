use clap::ArgMatches;

use shepr_api::schema::{
    AgentPromptParams, AgentPromptWaitOptions, AgentReadParams, AgentRenameParams,
    AgentSendKeysParams, AgentStatus, AgentTarget, AgentWaitParams, EmptyParams, ErrorBody,
    ErrorResponse, Method, ReadFormat, ReadSource, Request,
};

use super::matches::{flag, required, string, value, values};

#[derive(Clone)]
pub(crate) enum Command {
    List,
    Get { target: String },
    Read(AgentReadParams),
    SendKeys(AgentSendKeysParams),
    Prompt(AgentPromptParams),
    Rename(AgentRenameParams),
    Focus { target: String },
    Wait(AgentWaitParams),
    Attach { target: String, takeover: bool },
    Explain(ExplainArgs),
}

#[derive(Clone)]
pub(crate) struct ExplainArgs {
    target: Option<String>,
    file: Option<String>,
    agent: Option<String>,
    json: bool,
    verbose: bool,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Get { .. } => "get",
            Self::Read(_) => "read",
            Self::SendKeys(_) => "send-keys",
            Self::Prompt(_) => "prompt",
            Self::Rename(_) => "rename",
            Self::Focus { .. } => "focus",
            Self::Wait(_) => "wait",
            Self::Attach { .. } => "attach",
            Self::Explain(_) => "explain",
        }
    }

    pub(super) fn can_run_on_machine(&self) -> bool {
        match self {
            Self::List
            | Self::Get { .. }
            | Self::Read(_)
            | Self::SendKeys(_)
            | Self::Prompt(_)
            | Self::Rename(_)
            | Self::Focus { .. }
            | Self::Wait(_) => true,
            Self::Explain(args) => args.file.is_none(),
            Self::Attach { .. } => false,
        }
    }
}

pub(super) fn parse(matches: &ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("list", _)) => Some(Command::List),
        Some(("get", command)) => Some(Command::Get {
            target: required(command, "target")?,
        }),
        Some(("read", command)) => Some(Command::Read(read_params(command)?)),
        Some(("send-keys", command)) => Some(Command::SendKeys(AgentSendKeysParams {
            target: required(command, "target")?,
            keys: values::<String>(command, "key"),
        })),
        Some(("prompt", command)) => Some(Command::Prompt(prompt_params(command)?)),
        Some(("rename", command)) => Some(Command::Rename(AgentRenameParams {
            target: required(command, "target")?,
            name: string(command, "name"),
        })),
        Some(("focus", command)) => Some(Command::Focus {
            target: required(command, "target")?,
        }),
        Some(("wait", command)) => Some(Command::Wait(AgentWaitParams {
            target: required(command, "target")?,
            until: values::<AgentStatus>(command, "until"),
            timeout_ms: value::<u64>(command, "timeout"),
        })),
        Some(("attach", command)) => Some(Command::Attach {
            target: required(command, "target")?,
            takeover: flag(command, "takeover"),
        }),
        Some(("explain", command)) => Some(Command::Explain(ExplainArgs {
            target: string(command, "target"),
            file: string(command, "file"),
            agent: string(command, "agent"),
            json: flag(command, "json") || string(command, "format").as_deref() == Some("json"),
            verbose: flag(command, "verbose"),
        })),
        _ => None,
    }
}

pub(super) fn run_agent_command(
    command: Command,
    config: Option<shepr_config::ValidatedConfig>,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::List => agent_list(paths),
        Command::Get { target } => agent_get(paths, target),
        Command::Read(params) => agent_read(paths, params),
        Command::SendKeys(params) => agent_send_keys(paths, params),
        Command::Prompt(params) => agent_prompt(paths, params),
        Command::Rename(params) => agent_rename(paths, params),
        Command::Focus { target } => agent_focus(paths, target),
        Command::Wait(params) => agent_wait(paths, params),
        Command::Attach { target, takeover } => agent_attach(&target, takeover, config, paths),
        Command::Explain(args) => agent_explain(paths, args),
    }
}

fn agent_explain(paths: &super::target::CliContext, args: ExplainArgs) -> super::CliResult<i32> {
    let explain = if let Some(path) = args.file {
        let agent_label = args.agent.ok_or_else(|| {
            super::CliError::Usage("agent explain --file requires --agent".into())
        })?;
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                return Err(super::CliError::Response(ErrorResponse {
                    id: "cli:agent:explain".into(),
                    error: ErrorBody::new(
                        &shepr_api::error::ApiErrorCode::AgentExplainFileReadFailed,
                        format!("failed to read agent explain file {path}: {err}"),
                    ),
                }));
            }
        };
        shepr_agent::detect::manifest::explain_to_json_value(
            &shepr_agent::detect::manifest::explain_for_label(
                &agent_label,
                &content,
                paths.config_dir(),
            ),
        )
    } else {
        let target = args.target.ok_or_else(|| {
            super::CliError::Usage("agent explain requires TARGET unless --file is used".into())
        })?;
        let response = super::send_request(
            paths,
            &Request {
                id: "cli:agent:explain".into(),
                method: Method::AgentExplain(AgentTarget { target }),
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

    if args.json {
        println!("{explain}");
    } else {
        print_agent_explain_text(&explain, args.verbose);
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

fn agent_list(paths: &super::target::CliContext) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:list".into(),
            method: Method::AgentList(EmptyParams::default()),
        },
    )?)
}

fn agent_get(paths: &super::target::CliContext, target: String) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:get".into(),
            method: Method::AgentGet(AgentTarget { target }),
        },
    )?)
}

fn agent_focus(paths: &super::target::CliContext, target: String) -> super::CliResult<i32> {
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
    config: Option<shepr_config::ValidatedConfig>,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
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
        return Err(super::CliError::Failed {
            message: "agent attach failed: response did not include terminal_id".into(),
            hints: Vec::new(),
        });
    };
    crate::init_client_logging(paths)?;
    super::finish_client(shepr_client::run_terminal_attach(
        &config,
        paths,
        terminal_id.to_owned(),
        takeover,
    ))
}

fn agent_wait(paths: &super::target::CliContext, params: AgentWaitParams) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:wait".into(),
            method: Method::AgentWait(params),
        },
    )?)
}

fn resolve_agent_target(
    paths: &super::target::CliContext,
    target: &str,
    request_id: &str,
) -> super::CliResult<serde_json::Value> {
    super::send_request(paths, &agent_get_request(target, request_id))
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
    paths: &super::target::CliContext,
    params: AgentRenameParams,
) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:rename".into(),
            method: Method::AgentRename(params),
        },
    )?)
}

fn prompt_params(matches: &ArgMatches) -> Option<AgentPromptParams> {
    // `--until` and `--timeout` require `--wait` in the spec.
    Some(AgentPromptParams {
        target: required(matches, "target")?,
        text: required(matches, "text")?,
        wait: flag(matches, "wait").then(|| AgentPromptWaitOptions {
            until: values::<AgentStatus>(matches, "until"),
            timeout_ms: value::<u64>(matches, "timeout"),
            submission_deadline: None,
        }),
    })
}

fn agent_prompt(
    paths: &super::target::CliContext,
    params: AgentPromptParams,
) -> super::CliResult<i32> {
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
    paths: &super::target::CliContext,
    params: AgentSendKeysParams,
) -> super::CliResult<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: "cli:agent:send-keys".into(),
            method: Method::AgentSendKeys(params),
        },
    )?)
}

fn read_params(matches: &ArgMatches) -> Option<AgentReadParams> {
    // `--ansi` conflicts with `--format` in the spec; either selects ANSI, and
    // an ANSI read of an agent keeps its escapes.
    let format = if flag(matches, "ansi") {
        ReadFormat::Ansi
    } else {
        value::<ReadFormat>(matches, "format").unwrap_or(ReadFormat::Text)
    };
    Some(AgentReadParams {
        target: required(matches, "target")?,
        source: value::<ReadSource>(matches, "source").unwrap_or(ReadSource::Recent),
        lines: value::<u32>(matches, "lines"),
        format,
        strip_ansi: format != ReadFormat::Ansi,
    })
}

fn agent_read(paths: &super::target::CliContext, params: AgentReadParams) -> super::CliResult<i32> {
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
    use super::super::tests::group_matches;
    use shepr_api::schema::{AgentStatus, ReadFormat, ReadSource};

    fn command(args: &[&str]) -> super::Command {
        super::parse(&group_matches(args)).expect("test precondition")
    }

    #[test]
    fn read_defaults_and_ansi_forms() {
        let super::Command::Read(params) = command(&["agent", "read", "worker"]) else {
            panic!("expected agent read");
        };
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
            let super::Command::Read(params) = command(&args) else {
                panic!("expected agent read");
            };
            assert_eq!(params.source, ReadSource::Detection);
            assert_eq!(params.lines, Some(7));
            assert_eq!(params.format, ReadFormat::Ansi);
            assert!(!params.strip_ansi);
        }
    }

    #[test]
    fn prompt_wait_options_only_with_wait() {
        let super::Command::Prompt(params) = command(&[
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
        ]) else {
            panic!("expected agent prompt");
        };
        assert_eq!(params.text, "--help me");
        let wait = params.wait.expect("test precondition");
        assert_eq!(wait.until, vec![AgentStatus::Idle, AgentStatus::Blocked]);
        assert_eq!(wait.timeout_ms, Some(500));

        let super::Command::Prompt(params) = command(&["agent", "prompt", "w", "hi"]) else {
            panic!("expected agent prompt");
        };
        assert!(params.wait.is_none());
    }
}
