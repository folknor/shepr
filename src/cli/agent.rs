use std::time::{Duration, Instant};

use clap::ArgMatches;

use shepr_api::schema::{
    AgentPromptParams, AgentPromptWaitOptions, AgentReadParams, AgentRenameParams,
    AgentSendKeysParams, AgentStartParams, AgentStatus, AgentTarget, AgentWaitParams, EmptyParams,
    ErrorBody, ErrorResponse, Method, PaneProcessInfoParams, PaneTarget, ReadFormat, ReadSource,
    Request,
};

use super::matches::{flag, required, string, value, values};

const AGENT_START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PANE_SHELL_READINESS_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_AGENT_START_TIMEOUT_MS: u64 = 30_000;

struct AgentStartTiming {
    poll_interval: Duration,
    shell_readiness_retry_timeout: Duration,
    default_timeout_ms: u64,
    retryable_timeout_min: Duration,
    retryable_timeout_max: Duration,
}

impl AgentStartTiming {
    fn production() -> Self {
        Self {
            poll_interval: AGENT_START_POLL_INTERVAL,
            shell_readiness_retry_timeout: PANE_SHELL_READINESS_RETRY_TIMEOUT,
            default_timeout_ms: DEFAULT_AGENT_START_TIMEOUT_MS,
            retryable_timeout_min: shepr_server::app::AGENT_START_SETTLE_DELAY,
            retryable_timeout_max: shepr_server::app::MAX_AGENT_START_TIMEOUT,
        }
    }
}

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
    Start(AgentStartArgs),
    Explain(ExplainArgs),
}

#[derive(Clone)]
pub(crate) struct AgentStartArgs {
    name: String,
    kind: String,
    pane_id: String,
    timeout_ms: Option<u64>,
    agent_args: Vec<String>,
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
            Self::Start(_) => "start",
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
            | Self::Wait(_)
            | Self::Start(_) => true,
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
        Some(("start", command)) => Some(Command::Start(AgentStartArgs {
            name: required(command, "name")?,
            kind: required(command, "kind")?,
            pane_id: required(command, "pane")?,
            timeout_ms: value::<u64>(command, "timeout"),
            agent_args: values::<String>(command, "agent_args"),
        })),
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
        Command::Start(args) => agent_start(paths, args),
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

fn agent_start(paths: &super::target::CliContext, args: AgentStartArgs) -> super::CliResult<i32> {
    let timing = AgentStartTiming::production();
    agent_start_with_timing(paths, args, &timing)
}

fn agent_start_with_timing(
    paths: &super::target::CliContext,
    args: AgentStartArgs,
    timing: &AgentStartTiming,
) -> super::CliResult<i32> {
    let AgentStartArgs {
        name,
        kind,
        pane_id,
        timeout_ms,
        agent_args,
    } = args;
    // `--kind` is limited to the agent labels by the spec; this maps the label
    // to its canonical spelling for comparison with the detected agent.
    let Some(expected_kind) = shepr_agent::detect::parse_agent_label(&kind) else {
        return Ok(super::usage_error(&format!(
            "unsupported interactive agent kind: {kind}"
        )));
    };
    let expected_kind = shepr_agent::detect::agent_label(expected_kind).to_string();
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(timing.default_timeout_ms));
    // `send_request` checks local and --machine server builds, so these
    // server-owned launch bounds match this CLI build.
    let params = AgentStartParams {
        name: name.clone(),
        kind,
        pane_id: pane_id.clone(),
        args: agent_args,
        timeout_ms,
    };
    let (mut response, pinned_terminal_id) = send_agent_start_with_retry(
        &params,
        timeout,
        timing,
        |pane| pane_terminal_id(paths, pane),
        |pane| pane_shell_is_initializing(paths, pane),
        |request| super::send_request(paths, request),
    )?;
    let Some(expected_terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        if response.get("error").is_some() {
            return super::print_response(&response);
        }
        return super::print_response(&cli_agent_error(
            "cli:agent:start",
            &shepr_api::error::ApiErrorCode::AgentStartFailed,
            "agent start response did not include terminal_id",
        ));
    };
    if pinned_terminal_id
        .as_deref()
        .is_some_and(|pinned| pinned != expected_terminal_id)
    {
        return super::print_response(&agent_name_lost_error("cli:agent:start", &name));
    }
    let waited = wait_for_named_agent(
        paths,
        &name,
        &pane_id,
        timeout,
        &expected_kind,
        expected_terminal_id,
        timing,
    );
    match waited {
        Ok(Ok(agent)) => {
            response["result"]["agent"] = agent;
            super::print_response(&response)
        }
        Ok(Err(error)) => super::print_response(&error),
        Err(err) => print_agent_transport_error(
            &err,
            "cli:agent:start",
            &shepr_api::error::ApiErrorCode::AgentStartTransportFailed,
        ),
    }
}

/// Sends `agent.start`, retrying an `agent_pane_busy` refusal while the pane's
/// shell is still initializing in the same terminal. Returns the final
/// response and the terminal the pane held before the first attempt.
fn send_agent_start_with_retry(
    params: &AgentStartParams,
    timeout: Duration,
    timing: &AgentStartTiming,
    mut read_terminal_id: impl FnMut(&str) -> super::CliResult<Option<String>>,
    mut shell_is_initializing: impl FnMut(&str) -> super::CliResult<bool>,
    mut send_start: impl FnMut(&Request) -> super::CliResult<serde_json::Value>,
) -> super::CliResult<(serde_json::Value, Option<String>)> {
    let pane_id = params.pane_id.as_str();
    let retryable_timeout =
        timeout > timing.retryable_timeout_min && timeout <= timing.retryable_timeout_max;
    let pinned_terminal_id = read_terminal_id(pane_id)?;
    let mut retry_deadline: Option<Instant> = None;
    let mut previous_busy_response: Option<serde_json::Value> = None;
    let response = loop {
        if let Some(previous_busy_response) = previous_busy_response.as_ref() {
            let retry_expired = retry_deadline.is_some_and(|deadline| Instant::now() >= deadline);
            if retry_expired
                || read_terminal_id(pane_id)? != pinned_terminal_id
                || !shell_is_initializing(pane_id)?
            {
                return Ok((previous_busy_response.clone(), pinned_terminal_id));
            }
        }

        let response = send_start(&Request {
            id: "cli:agent:start".into(),
            method: Method::AgentStart(params.clone()),
        })?;
        if response.get("error").is_none() {
            break response;
        }
        if response["error"]["code"].as_str()
            != Some(shepr_api::error::ApiErrorCode::AgentPaneBusy.as_str())
            || !retryable_timeout
            || pinned_terminal_id.is_none()
            || read_terminal_id(pane_id)? != pinned_terminal_id
            || !shell_is_initializing(pane_id)?
        {
            break response;
        }

        let deadline = *retry_deadline
            .get_or_insert_with(|| Instant::now() + timing.shell_readiness_retry_timeout);
        previous_busy_response = Some(response);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero()
            && let Some(previous_busy_response) = previous_busy_response.as_ref()
        {
            return Ok((previous_busy_response.clone(), pinned_terminal_id));
        }
        std::thread::sleep(timing.poll_interval.min(remaining));
    };
    Ok((response, pinned_terminal_id))
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

fn wait_for_named_agent(
    paths: &super::target::CliContext,
    name: &str,
    fallback_pane_id: &str,
    timeout: Duration,
    expected_kind: &str,
    expected_terminal_id: &str,
    timing: &AgentStartTiming,
) -> super::CliResult<Result<serde_json::Value, serde_json::Value>> {
    let deadline = Instant::now().checked_add(timeout);
    let mut first_poll = true;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Let the server reconcile its matching startup deadline before
            // returning so the pending name is immediately reusable. The poll is
            // a courtesy: the timeout below is this command's answer whatever it
            // returns, and a server that cannot answer it will fail the caller's
            // next request loudly anyway.
            drop(resolve_agent_target_unchecked(
                paths,
                name,
                "cli:agent:start:timeout",
            ));
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
                std::thread::sleep(timing.poll_interval);
                continue;
            }
        }
        let agent = &response["result"]["agent"];
        let outcome = named_agent_start_outcome(agent, name, expected_kind, expected_terminal_id);
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
        std::thread::sleep(timing.poll_interval);
    }
}

fn named_agent_start_outcome(
    agent: &serde_json::Value,
    name: &str,
    expected_kind: &str,
    expected_terminal_id: &str,
) -> Option<Result<serde_json::Value, serde_json::Value>> {
    if agent["terminal_id"].as_str() != Some(expected_terminal_id) {
        return Some(Err(agent_name_lost_error("cli:agent:start", name)));
    }
    if let Some(actual) = agent["agent"]
        .as_str()
        .filter(|actual| *actual != expected_kind)
    {
        return Some(Err(cli_agent_error(
            "cli:agent:start",
            &shepr_api::error::ApiErrorCode::AgentKindMismatch,
            format!("expected {expected_kind}, detected {actual}"),
        )));
    }
    if agent["name"].as_str() != Some(name) {
        return Some(Err(agent_name_lost_error("cli:agent:start", name)));
    }
    match agent["agent_status"].as_str() {
        Some("blocked") => Some(Err(cli_agent_error(
            "cli:agent:start",
            &shepr_api::error::ApiErrorCode::AgentNotReady,
            format!("agent {name} is blocked during startup and is not ready for prompts"),
        ))),
        Some("idle") if agent["interactive_ready"].as_bool() == Some(true) => {
            Some(Ok(agent.clone()))
        }
        Some("idle") if !agent["launch_pending"].as_bool().unwrap_or(false) => {
            Some(Err(cli_agent_error(
                "cli:agent:start",
                &shepr_api::error::ApiErrorCode::AgentStartFailed,
                "agent process exited before becoming interactive",
            )))
        }
        // Working, or idle with its launch still pending: keep polling.
        _ => None,
    }
}

fn pane_terminal_id(
    paths: &super::target::CliContext,
    pane_id: &str,
) -> super::CliResult<Option<String>> {
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
    paths: &super::target::CliContext,
    pane_id: &str,
) -> super::CliResult<bool> {
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
                        .is_some_and(shepr_agent::detect::is_pane_shell_process_name)
                        || process["argv"]
                            .as_array()
                            .and_then(|argv| argv.first())
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(shepr_agent::detect::is_pane_shell_process_name))
            })
        })
}

fn agent_name_lost_error(request_id: &str, expected_name: &str) -> serde_json::Value {
    cli_agent_error(
        request_id,
        &shepr_api::error::ApiErrorCode::AgentNameNotFound,
        format!("named agent {expected_name} no longer owns the target terminal"),
    )
}

fn print_agent_transport_error(
    err: &super::CliError,
    request_id: &str,
    code: &shepr_api::error::ApiErrorCode,
) -> super::CliResult<i32> {
    if matches!(err, super::CliError::Response(_)) {
        err.print();
        return Ok(1);
    }
    super::print_response(&cli_agent_error(request_id, code, err.to_string()))
}

fn agent_wait_timeout() -> serde_json::Value {
    cli_agent_error(
        "cli:agent:start",
        &shepr_api::error::ApiErrorCode::Timeout,
        "timed out waiting for agent startup",
    )
}

fn cli_agent_error(
    id: &str,
    code: &shepr_api::error::ApiErrorCode,
    message: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "error": ErrorBody::new(code, message),
    })
}

fn resolve_agent_target(
    paths: &super::target::CliContext,
    target: &str,
    request_id: &str,
) -> super::CliResult<serde_json::Value> {
    super::send_request(paths, &agent_get_request(target, request_id))
}

fn resolve_agent_target_unchecked(
    paths: &super::target::CliContext,
    target: &str,
    request_id: &str,
) -> super::CliResult<serde_json::Value> {
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

    #[test]
    fn start_collects_agent_args_after_separator() {
        let super::Command::Start(start) = command(&[
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
        ]) else {
            panic!("expected agent start");
        };
        assert_eq!(start.name, "repro");
        assert_eq!(start.pane_id, "p1");
        assert_eq!(start.agent_args, vec!["--resume", "--session", "x"]);
    }
}

#[cfg(test)]
mod start_tests {
    use super::*;
    use std::collections::VecDeque;

    fn test_timing() -> AgentStartTiming {
        AgentStartTiming {
            poll_interval: Duration::ZERO,
            shell_readiness_retry_timeout: Duration::from_secs(1),
            default_timeout_ms: 100,
            retryable_timeout_min: Duration::ZERO,
            retryable_timeout_max: Duration::from_secs(1),
        }
    }

    fn start_params() -> AgentStartParams {
        AgentStartParams {
            name: "reviewer".into(),
            kind: "claude".into(),
            pane_id: "pane-1".into(),
            args: Vec::new(),
            timeout_ms: Some(100),
        }
    }

    #[test]
    fn busy_retry_uses_injected_timing_and_callbacks() {
        let mut responses = VecDeque::from([
            serde_json::json!({ "error": { "code": "agent_pane_busy" } }),
            serde_json::json!({ "result": { "agent": { "terminal_id": "terminal" } } }),
        ]);
        let mut sends = 0;
        let (response, pinned_terminal_id) = send_agent_start_with_retry(
            &start_params(),
            Duration::from_millis(100),
            &test_timing(),
            |_| Ok(Some("terminal".into())),
            |_| Ok(true),
            |_| {
                sends += 1;
                Ok(responses.pop_front().expect("test response is queued"))
            },
        )
        .expect("fake API callbacks succeed");

        assert_eq!(sends, 2);
        assert_eq!(pinned_terminal_id.as_deref(), Some("terminal"));
        assert_eq!(response["result"]["agent"]["terminal_id"], "terminal");
    }

    #[test]
    fn busy_retry_returns_the_first_refusal_when_terminal_changes() {
        let busy = serde_json::json!({ "error": { "code": "agent_pane_busy" } });
        let mut terminal_reads = VecDeque::from([
            Some("terminal".to_owned()),
            Some("terminal".to_owned()),
            Some("replacement".to_owned()),
        ]);
        let mut sends = 0;
        let (response, pinned_terminal_id) = send_agent_start_with_retry(
            &start_params(),
            Duration::from_millis(100),
            &test_timing(),
            |_| Ok(terminal_reads.pop_front().expect("terminal read is queued")),
            |_| Ok(true),
            |_| {
                sends += 1;
                Ok(busy.clone())
            },
        )
        .expect("fake API callbacks succeed");

        assert_eq!(sends, 1);
        assert_eq!(pinned_terminal_id.as_deref(), Some("terminal"));
        assert_eq!(response, busy);
    }

    #[test]
    fn readiness_requires_the_expected_identity_and_interactive_idle_state() {
        let base_agent = serde_json::json!({
            "name": "reviewer",
            "agent": "claude",
            "terminal_id": "terminal",
            "agent_status": "idle",
            "interactive_ready": true,
            "launch_pending": false,
        });

        assert!(
            named_agent_start_outcome(&base_agent, "reviewer", "claude", "terminal",)
                .is_some_and(|result| result.is_ok())
        );

        let working = serde_json::json!({
            "name": "reviewer",
            "agent": "claude",
            "terminal_id": "terminal",
            "agent_status": "working",
            "interactive_ready": false,
            "launch_pending": true,
        });
        assert!(named_agent_start_outcome(&working, "reviewer", "claude", "terminal").is_none());

        let blocked = serde_json::json!({
            "name": "reviewer",
            "agent": "claude",
            "terminal_id": "terminal",
            "agent_status": "blocked",
        });
        let blocked_error = named_agent_start_outcome(&blocked, "reviewer", "claude", "terminal")
            .and_then(Result::err)
            .expect("blocked startup is a final error");
        assert_eq!(blocked_error["error"]["code"], "agent_not_ready");

        let exited = serde_json::json!({
            "name": "reviewer",
            "agent": "claude",
            "terminal_id": "terminal",
            "agent_status": "idle",
            "interactive_ready": false,
            "launch_pending": false,
        });
        let exited_error = named_agent_start_outcome(&exited, "reviewer", "claude", "terminal")
            .and_then(Result::err)
            .expect("an exited startup is a final error");
        assert_eq!(exited_error["error"]["code"], "agent_start_failed");

        let wrong_kind = serde_json::json!({
            "name": "reviewer",
            "agent": "codex",
            "terminal_id": "terminal",
            "agent_status": "idle",
        });
        let kind_error = named_agent_start_outcome(&wrong_kind, "reviewer", "claude", "terminal")
            .and_then(Result::err)
            .expect("a different agent kind is a final error");
        assert_eq!(kind_error["error"]["code"], "agent_kind_mismatch");
    }
}
