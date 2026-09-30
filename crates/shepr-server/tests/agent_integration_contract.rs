//! The agent integrations checked against the server's acceptance contract.
//! Each shell hook asset runs through a scripted agent session against a fake
//! API socket, and each plugin's pinned trace is loaded; both request streams
//! are replayed into a `TerminalState`, and the test asserts on the persisted
//! session and hook authority that result rather than on request shapes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use shepr_agent::agent::resume::{
    AgentSessionRef, PersistedAgentSession, normalize_session_start_source,
    session_ref_for_agent_report,
};
use shepr_agent::agent::{Agent, AgentSource};
use shepr_agent::detect::AgentState;
use shepr_api::schema::{Method, PaneAgentState, Request};
use shepr_mux::terminal::state::{HookClockSample, TerminalState};
use shepr_protocol::TerminalId;
use shepr_test_support::{IsolatedEnv, ScratchDir, command_in_scratch};

const PANE_ID: &str = "w1:p1";

struct ShellStep {
    action: Option<&'static str>,
    input: Value,
}

struct AssetContract {
    asset: &'static str,
    agent: Agent,
    session_id: &'static str,
    state: Option<AgentState>,
}

const SHELL_ASSETS: &[AssetContract] = &[
    AssetContract {
        asset: "antigravity_cli/shepr-agent-state.sh",
        agent: Agent::Antigravity,
        session_id: "agy-contract-session",
        state: None,
    },
    AssetContract {
        asset: "claude/shepr-agent-state.sh",
        agent: Agent::Claude,
        session_id: "claude-contract-session",
        state: None,
    },
    AssetContract {
        asset: "codex/shepr-agent-state.sh",
        agent: Agent::Codex,
        session_id: "codex-contract-session",
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "copilot/shepr-agent-state.sh",
        agent: Agent::GithubCopilot,
        session_id: "copilot-contract-session",
        state: None,
    },
    AssetContract {
        asset: "cursor/shepr-agent-state.sh",
        agent: Agent::Cursor,
        session_id: "cursor-contract-session",
        state: None,
    },
    AssetContract {
        asset: "devin/shepr-agent-state.sh",
        agent: Agent::Devin,
        session_id: "devin-contract-session",
        state: None,
    },
    AssetContract {
        asset: "droid/shepr-agent-state.sh",
        agent: Agent::Droid,
        session_id: "droid-contract-session",
        state: None,
    },
    AssetContract {
        asset: "grok/shepr-agent-state.sh",
        agent: Agent::Grok,
        session_id: "grok-contract-session",
        state: None,
    },
    AssetContract {
        asset: "kimi/shepr-agent-state.sh",
        agent: Agent::Kimi,
        session_id: "kimi-contract-session",
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "mastracode/shepr-agent-state.sh",
        agent: Agent::Mastracode,
        session_id: "mastracode-contract-session",
        state: Some(AgentState::Working),
    },
];

const BUN_ASSETS: &[AssetContract] = &[
    AssetContract {
        asset: "pi/shepr-agent-state.ts",
        agent: Agent::Pi,
        session_id: "pi-new",
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "omp/shepr-agent-state.ts",
        agent: Agent::Omp,
        session_id: "omp-contract-session",
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "opencode/shepr-agent-state.js",
        agent: Agent::OpenCode,
        session_id: "local-session",
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "opencode/shepr-tui-session.js",
        agent: Agent::OpenCode,
        session_id: "session-a",
        state: None,
    },
    AssetContract {
        asset: "opencode/tui.js",
        agent: Agent::OpenCode,
        session_id: "v2-session",
        state: None,
    },
    AssetContract {
        asset: "kilo/shepr-agent-state.js",
        agent: Agent::Kilo,
        session_id: "kilo-contract-session",
        state: Some(AgentState::Blocked),
    },
];

#[test]
fn every_bundled_agent_asset_replays_through_terminal_state() {
    let environment = IsolatedEnv::new();
    environment.remove("CURSOR_VERSION");
    environment.remove("GROK_SESSION_ID");
    let scratch = ScratchDir::new("agent-integration-contract");
    let integration = Path::new(env!("CARGO_MANIFEST_DIR")).join("../shepr-agent/src/integration");
    let assets = integration.join("assets");

    assert_asset_coverage(&assets);
    let mut shell_results = Vec::new();
    for contract in SHELL_ASSETS {
        let script = assets.join(contract.asset);
        let requests = capture_shell_asset(
            &script,
            &shell_session_steps(contract),
            &scratch,
            contract.agent.label(),
        );
        shell_results.push((contract, PANE_ID.to_owned(), requests));
    }

    for (relative, agent, action) in [
        ("codex/shepr-agent-state.sh", Agent::Codex, "working"),
        ("kimi/shepr-agent-state.sh", Agent::Kimi, "working"),
        (
            "mastracode/shepr-agent-state.sh",
            Agent::Mastracode,
            "working",
        ),
    ] {
        let script = assets.join(relative);
        let requests = capture_shell_asset(
            &script,
            &[ShellStep {
                action: Some(action),
                input: serde_json::json!({}),
            }],
            &scratch,
            "sessionless-state",
        );
        assert!(
            requests.is_empty(),
            "{} reported state without a session reference",
            agent.label()
        );
    }

    // The plugins run under bun, which these tests do not have. Their bun tests
    // pin what each plugin sends to a trace in this file, and the trace is
    // what replays here.
    let traces = load_plugin_traces(&integration.join("contract_traces.toml"));
    let mut bun_results = Vec::new();
    for contract in BUN_ASSETS {
        let name = bun_trace_name(contract.asset);
        let requests = traces
            .get(name)
            .unwrap_or_else(|| panic!("no trace {name} for {}", contract.asset))
            .clone();
        let pane_id = requests
            .first()
            .and_then(|request| request.pointer("/params/pane_id"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("trace {name} names no pane"))
            .to_owned();
        bun_results.push((contract, pane_id, requests));
    }
    assert_eq!(
        traces.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BUN_ASSETS
            .iter()
            .map(|contract| bun_trace_name(contract.asset))
            .collect::<BTreeSet<_>>(),
        "every plugin trace belongs to one plugin asset"
    );

    for (contract, pane_id, requests) in shell_results.into_iter().chain(bun_results) {
        replay_and_assert_contract(contract, &pane_id, &requests);
    }

    prove_replay_rejects_a_broken_asset(&assets, &scratch);
}

fn shell_session_steps(contract: &AssetContract) -> Vec<ShellStep> {
    let session_id = contract.session_id;
    let (session_action, session_input) = match contract.agent {
        Agent::Antigravity => (
            Some("session"),
            serde_json::json!({"conversationId": session_id}),
        ),
        Agent::Claude | Agent::Codex => (
            Some("session"),
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "session_id": session_id,
                "source": "startup",
            }),
        ),
        Agent::GithubCopilot => (
            None,
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "session_id": session_id,
            }),
        ),
        Agent::Cursor => (
            Some("session"),
            serde_json::json!({
                "hook_event_name": "sessionStart",
                "session_id": session_id,
            }),
        ),
        Agent::Droid => (
            Some("session"),
            serde_json::json!({"session_id": session_id}),
        ),
        Agent::Devin | Agent::Grok => (
            Some("session"),
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "session_id": session_id,
            }),
        ),
        Agent::Kimi | Agent::Mastracode => (
            Some("session"),
            serde_json::json!({"session_id": session_id, "source": "startup"}),
        ),
        _ => panic!("{} is not a shell integration", contract.agent.label()),
    };
    let mut steps = vec![ShellStep {
        action: session_action,
        input: session_input,
    }];
    if contract.state.is_some() {
        let (action, event_name) = match contract.agent {
            Agent::Codex | Agent::Kimi | Agent::Mastracode => ("working", "UserPromptSubmit"),
            _ => panic!("{} has no scripted state hook", contract.agent.label()),
        };
        steps.push(ShellStep {
            action: Some(action),
            input: serde_json::json!({
                "hook_event_name": event_name,
                "session_id": session_id,
            }),
        });
    }
    steps
}

fn capture_shell_asset(
    script: &Path,
    steps: &[ShellStep],
    scratch: &ScratchDir,
    socket_label: &str,
) -> Vec<Value> {
    let socket_path = scratch.join(format!("{socket_label}.sock"));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove old test socket {}: {error}", socket_path.display()),
    }
    let listener = UnixListener::bind(&socket_path)
        .unwrap_or_else(|error| panic!("bind test socket {}: {error}", socket_path.display()));
    listener
        .set_nonblocking(true)
        .expect("make fake socket nonblocking");
    let (stop, stopped) = mpsc::channel();
    let server = thread::spawn(move || capture_socket_requests(&listener, &stopped));

    for step in steps {
        // host-program-ok: the shipped agent hook is the shell script under test.
        let mut command = command_in_scratch("sh", "agent-integration-shell-asset");
        command
            .arg(script)
            .env("SHEPR_ENV", "1")
            .env("SHEPR_SOCKET_PATH", &socket_path)
            .env("SHEPR_PANE_ID", PANE_ID)
            .env("TMPDIR", scratch.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(action) = step.action {
            command.arg(action);
        }
        let mut child = command.spawn().expect("start shell hook asset");
        let mut stdin = child.stdin.take().expect("hook stdin is piped");
        stdin
            .write_all(step.input.to_string().as_bytes())
            .expect("write scripted hook input");
        stdin.write_all(b"\n").expect("finish scripted hook input");
        drop(stdin);
        let output = child.wait_with_output().expect("wait for shell hook");
        assert!(
            output.status.success(),
            "{} exited unsuccessfully",
            script.display()
        );
    }

    stop.send(()).expect("stop fake API socket");
    let requests = server.join().expect("join fake API socket thread");
    fs::remove_file(&socket_path).expect("remove fake API socket");
    requests
}

fn capture_socket_requests(listener: &UnixListener, stopped: &mpsc::Receiver<()>) -> Vec<Value> {
    // A hook has connected and written its request before it exits, even when
    // its reply wait times out, so the stop is honoured only once the backlog
    // is empty. A panicking test drops the sender, which also ends the loop.
    let mut requests = Vec::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("set fake socket read deadline");
                let mut line = String::new();
                BufReader::new(stream.try_clone().expect("clone accepted stream"))
                    .read_line(&mut line)
                    .expect("read captured hook request");
                assert!(line.ends_with('\n'), "hook request was not line framed");
                requests.push(
                    serde_json::from_str(&line).expect("decode captured hook request as JSON"),
                );
                // A hook whose reply wait already timed out has closed its
                // end; its request is captured, and the reply has no reader.
                if let Err(error) = stream.write_all(b"{}\n") {
                    assert!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        ),
                        "write fake API reply: {error}"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                match stopped.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept fake API connection: {error}"),
        }
    }
    requests
}

/// The plugin traces by name, each a list of JSON-RPC requests.
fn load_plugin_traces(path: &Path) -> BTreeMap<String, Vec<Value>> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read plugin traces {}: {error}", path.display()));
    let table: toml::Table = toml::from_str(&text)
        .unwrap_or_else(|error| panic!("parse plugin traces {}: {error}", path.display()));
    table
        .into_iter()
        .map(|(name, trace)| {
            let requests = serde_json::to_value(trace)
                .ok()
                .and_then(|value| serde_json::from_value::<Vec<Value>>(value).ok())
                .unwrap_or_else(|| panic!("trace {name} is not a list of requests"));
            (name, requests)
        })
        .collect()
}

fn replay_and_assert_contract(contract: &AssetContract, pane_id: &str, requests: &[Value]) {
    assert!(
        !requests.is_empty(),
        "{} emitted no requests",
        contract.asset
    );
    let mut terminal = new_terminal(contract.agent);
    let expected_session = PersistedAgentSession::new(
        AgentSource::Official(contract.agent),
        contract.agent,
        AgentSessionRef::id(contract.session_id).expect("valid contract session id"),
    )
    .expect("supported agent session identity");

    for (index, request) in requests.iter().enumerate() {
        replay_request(&mut terminal, pane_id, request, index).unwrap_or_else(|error| {
            panic!("{} request {index} was rejected: {error}", contract.asset)
        });
    }

    assert_eq!(
        terminal.current_session_identity_for_persistence().as_ref(),
        Some(&expected_session),
        "{} did not persist the scripted session",
        contract.asset
    );

    match contract.state {
        Some(expected_state) => {
            let authority = terminal
                .hook_authority
                .as_ref()
                .unwrap_or_else(|| panic!("{} did not establish hook authority", contract.asset));
            assert_eq!(
                authority.source,
                contract.agent.integration_source().unwrap_or_default()
            );
            assert_eq!(authority.agent_label, contract.agent.label());
            assert_eq!(authority.state, expected_state);
            assert_eq!(authority.session_ref, Some(expected_session.session_ref));
        }
        None => assert!(
            terminal.hook_authority.is_none(),
            "{} should report session identity without state authority",
            contract.asset
        ),
    }
}

fn replay_request(
    terminal: &mut TerminalState,
    pane_id: &str,
    value: &Value,
    index: usize,
) -> Result<(), String> {
    let request: Request = serde_json::from_value(value.clone())
        .map_err(|error| format!("request did not match the API schema: {error}"))?;
    let sample = clock_sample(index);
    match request.method {
        Method::PaneReportAgentSession(params) => {
            validate_pane_id(&params.pane_id, pane_id)?;
            let (source, agent, session_ref) = decode_report_identity(
                &params.source,
                &params.agent,
                params.agent_session_id,
                params.agent_session_path,
            )?;
            let start_source =
                normalize_session_start_source(params.session_start_source.as_deref());
            let _mutation = terminal.set_agent_session_ref_for_typed_start_source_at(
                source,
                agent.label().to_owned(),
                session_ref,
                params.seq,
                start_source,
                sample,
            );
            Ok(())
        }
        Method::PaneReportAgent(params) => {
            validate_pane_id(&params.pane_id, pane_id)?;
            let (source, agent, session_ref) = decode_report_identity(
                &params.source,
                &params.agent,
                params.agent_session_id,
                params.agent_session_path,
            )?;
            let _mutation = terminal.set_hook_report_at(
                source,
                agent.label().to_owned(),
                agent_state(params.state),
                session_ref,
                params.seq,
                sample,
            );
            Ok(())
        }
        other => Err(format!("unexpected API method {other:?}")),
    }
}

/// The report validation the server's API handler runs before dispatch
/// (`parse_report_session_ref` beside `handle_pane_report_agent`), restated
/// because the App is private to the server crate. Official assets use
/// canonical labels, so a label the server would accept only as custom is
/// refused here.
fn decode_report_identity(
    source_text: &str,
    agent_text: &str,
    session_id: Option<String>,
    session_path: Option<String>,
) -> Result<(AgentSource, Agent, Option<AgentSessionRef>), String> {
    let source = AgentSource::parse(source_text);
    let agent = Agent::parse_canonical_label(agent_text.trim())
        .ok_or_else(|| format!("unknown agent label {agent_text:?}"))?;
    if source
        .agent()
        .is_some_and(|source_agent| source_agent != agent)
    {
        return Err(format!(
            "source {source_text:?} does not match agent {}",
            agent.label()
        ));
    }
    if source.agent().is_none() {
        return Ok((source, agent, None));
    }
    let supplied_reference = session_id.is_some() || session_path.is_some();
    let session_ref = session_ref_for_agent_report(agent, session_id, session_path);
    if supplied_reference && session_ref.is_none() {
        return Err("request supplied an invalid session reference".to_owned());
    }
    Ok((source, agent, session_ref))
}

fn validate_pane_id(pane_id: &str, expected: &str) -> Result<(), String> {
    if pane_id == expected {
        Ok(())
    } else {
        Err(format!(
            "request targeted pane {pane_id:?}, expected {expected:?}"
        ))
    }
}

fn agent_state(state: PaneAgentState) -> AgentState {
    match state {
        PaneAgentState::Idle => AgentState::Idle,
        PaneAgentState::Working => AgentState::Working,
        PaneAgentState::Blocked => AgentState::Blocked,
        PaneAgentState::Unknown => AgentState::Unknown,
    }
}

fn new_terminal(agent: Agent) -> TerminalState {
    let observed_at = Instant::now();
    let mut terminal = TerminalState::new(TerminalId::alloc(), PathBuf::from("/"));
    terminal.set_detected_agent_process_at(agent, observed_at);
    terminal
}

fn clock_sample(index: usize) -> HookClockSample {
    let step = u64::try_from(index + 1).expect("small request index");
    let offset = Duration::from_millis(step);
    HookClockSample {
        monotonic: Instant::now() + offset,
        wall: SystemTime::now() + offset,
    }
}

fn bun_trace_name(asset: &str) -> &'static str {
    match asset {
        "pi/shepr-agent-state.ts" => "pi",
        "omp/shepr-agent-state.ts" => "omp",
        "opencode/shepr-agent-state.js" => "opencode",
        "opencode/shepr-tui-session.js" => "opencode_tui_v1",
        "opencode/tui.js" => "opencode_tui_v2",
        "kilo/shepr-agent-state.js" => "kilo",
        _ => panic!("no bun trace for {asset}"),
    }
}

fn assert_asset_coverage(assets: &Path) {
    let found = discover_report_assets(assets);
    let expected = SHELL_ASSETS
        .iter()
        .chain(BUN_ASSETS)
        .map(|contract| contract.asset.to_owned())
        .collect::<BTreeSet<_>>();
    let discovered = found.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(
        discovered, expected,
        "asset contract coverage must stay complete"
    );
}

fn discover_report_assets(root: &Path) -> Vec<String> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read agent asset directory") {
            let entry = entry.expect("read agent asset entry");
            let path = entry.path();
            if entry
                .file_type()
                .expect("read agent asset file type")
                .is_dir()
            {
                pending.push(path);
                continue;
            }
            let extension = path.extension().and_then(|value| value.to_str());
            if !matches!(extension, Some("sh" | "js" | "ts"))
                || path.to_string_lossy().ends_with(".test.ts")
            {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .expect("asset is below its root")
                .to_string_lossy()
                .into_owned();
            found.push(relative);
        }
    }
    found.sort();
    found
}

fn prove_replay_rejects_a_broken_asset(assets: &Path, scratch: &ScratchDir) {
    let source = fs::read_to_string(assets.join("claude/shepr-agent-state.sh"))
        .expect("read Claude asset for mutation probe");
    let broken = source.replacen("\"agent\": \"claude\"", "\"agent\": \"codex\"", 1);
    assert_ne!(broken, source, "mutation probe must change the asset");
    let broken_path = scratch.join("broken-claude-state.sh");
    fs::write(&broken_path, broken).expect("write broken asset copy in scratch");

    let requests = capture_shell_asset(
        &broken_path,
        &[ShellStep {
            action: Some("session"),
            input: serde_json::json!({
                "hook_event_name": "SessionStart",
                "session_id": "claude-contract-session",
                "source": "startup",
            }),
        }],
        scratch,
        "broken-claude",
    );
    fs::remove_file(&broken_path).expect("remove broken asset copy");
    assert_eq!(requests.len(), 1, "broken asset still reports its session");
    let mut terminal = new_terminal(Agent::Claude);
    let error = replay_request(&mut terminal, PANE_ID, &requests[0], 0)
        .expect_err("mismatched source and agent must be rejected");
    assert!(error.contains("does not match"), "{error}");
    assert!(
        terminal
            .current_session_identity_for_persistence()
            .is_none()
    );
}
