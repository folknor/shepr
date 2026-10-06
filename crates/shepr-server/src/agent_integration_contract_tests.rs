//! The agent integrations checked against the server's acceptance contract.
//! Each shell hook asset runs through a scripted agent session against a fake
//! API socket, and each plugin's pinned trace is loaded. Every request is
//! replayed whole through the server's own report handlers, on an App holding
//! one pane. It is a unit test of this crate so the harness that reaches those
//! handlers stays test-only.

// Keep this contract at the server boundary even though arbitration lives in
// shepr-agent: replaying whole API requests verifies the actual handlers and
// their parsing/admission path. Moving it below API/server would either invert
// crate layering or replace that dispatch with a test-only copy.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::agent_report_test_support::AgentReportHarness;
use serde_json::Value;
use shepr_agent::AgentState;
use shepr_agent::resume::{AgentSessionRef, PersistedAgentSession};
use shepr_agent::{Agent, AgentSource, ReportOrigin};
use shepr_api::schema::Request;
use shepr_detect::ownership::HookClockSample;
use shepr_test_support::{IsolatedEnv, ScratchDir, capture_hook, command_in_scratch};

const PANE_ID: &str = "w1:p1";

struct ShellStep {
    action: Option<&'static str>,
    input: Value,
}

struct AssetContract {
    asset: &'static str,
    agent: Agent,
    session: ContractSessionRef,
    state: Option<AgentState>,
}

#[derive(Clone, Copy)]
enum ContractSessionRef {
    Id(&'static str),
    Path(&'static str),
}

const SHELL_ASSETS: &[AssetContract] = &[
    AssetContract {
        asset: "antigravity_cli/shepr-agent-state.sh",
        agent: Agent::Antigravity,
        session: ContractSessionRef::Id("agy-contract-session"),
        state: None,
    },
    AssetContract {
        asset: "claude/shepr-agent-state.sh",
        agent: Agent::Claude,
        session: ContractSessionRef::Id("claude-contract-session"),
        state: None,
    },
    AssetContract {
        asset: "codex/shepr-agent-state.sh",
        agent: Agent::Codex,
        session: ContractSessionRef::Id("codex-contract-session"),
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "copilot/shepr-agent-state.sh",
        agent: Agent::GithubCopilot,
        session: ContractSessionRef::Id("copilot-contract-session"),
        state: None,
    },
    AssetContract {
        asset: "cursor/shepr-agent-state.sh",
        agent: Agent::Cursor,
        session: ContractSessionRef::Id("cursor-contract-session"),
        state: None,
    },
    AssetContract {
        asset: "devin/shepr-agent-state.sh",
        agent: Agent::Devin,
        session: ContractSessionRef::Id("devin-contract-session"),
        state: None,
    },
    AssetContract {
        asset: "droid/shepr-agent-state.sh",
        agent: Agent::Droid,
        session: ContractSessionRef::Id("droid-contract-session"),
        state: None,
    },
    AssetContract {
        asset: "grok/shepr-agent-state.sh",
        agent: Agent::Grok,
        session: ContractSessionRef::Id("grok-contract-session"),
        state: None,
    },
    AssetContract {
        asset: "kimi/shepr-agent-state.sh",
        agent: Agent::Kimi,
        session: ContractSessionRef::Id("kimi-contract-session"),
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "mastracode/shepr-agent-state.sh",
        agent: Agent::Mastracode,
        session: ContractSessionRef::Id("mastracode-contract-session"),
        state: Some(AgentState::Working),
    },
];

const BUN_ASSETS: &[AssetContract] = &[
    AssetContract {
        asset: "pi/shepr-agent-state.ts",
        agent: Agent::Pi,
        session: ContractSessionRef::Path("/nonexistent/pi-new.jsonl"),
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "omp/shepr-agent-state.ts",
        agent: Agent::Omp,
        session: ContractSessionRef::Id("omp-contract-session"),
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "opencode/shepr-agent-state.js",
        agent: Agent::OpenCode,
        session: ContractSessionRef::Id("local-session"),
        state: Some(AgentState::Working),
    },
    AssetContract {
        asset: "opencode/shepr-tui-session.js",
        agent: Agent::OpenCode,
        session: ContractSessionRef::Id("session-a"),
        state: None,
    },
    AssetContract {
        asset: "opencode/tui.js",
        agent: Agent::OpenCode,
        session: ContractSessionRef::Id("v2-session"),
        state: None,
    },
    AssetContract {
        asset: "kilo/shepr-agent-state.js",
        agent: Agent::Kilo,
        session: ContractSessionRef::Id("kilo-contract-session"),
        state: Some(AgentState::Blocked),
    },
];

#[test]
fn every_bundled_agent_asset_replays_through_server_report_validation() {
    let _environment = IsolatedEnv::new();
    let scratch = ScratchDir::new("agent-integration-contract");
    let integration = Path::new(env!("CARGO_MANIFEST_DIR")).join("../shepr-integration/src");
    let assets = integration.join("assets");

    assert_asset_coverage(&assets);
    for contract in SHELL_ASSETS {
        let mut app = AgentReportHarness::new(scratch.path(), contract.agent, clock_sample(0))
            .unwrap_or_else(|error| panic!("build App for {}: {error}", contract.asset));
        let script = assets.join(contract.asset);
        let requests = capture_shell_asset(
            &script,
            &shell_session_steps(contract),
            &scratch,
            contract.agent.label(),
            app.pane_id(),
        );
        replay_and_assert_contract(contract, &mut app, &requests);
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
            PANE_ID,
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
    for contract in BUN_ASSETS {
        let name = bun_trace_name(contract.asset);
        let mut requests = traces
            .get(name)
            .unwrap_or_else(|| panic!("no trace {name} for {}", contract.asset))
            .clone();
        let mut app = AgentReportHarness::new(scratch.path(), contract.agent, clock_sample(0))
            .unwrap_or_else(|error| panic!("build App for {}: {error}", contract.asset));
        // Traces use a stub pane id; route each complete request to this App's
        // real test pane before replay.
        for request in &mut requests {
            let pane_id = request
                .pointer_mut("/params/pane_id")
                .unwrap_or_else(|| panic!("trace {name} names no pane"));
            assert!(pane_id.is_string(), "trace {name} pane id is not text");
            *pane_id = Value::String(app.pane_id().to_owned());
        }
        replay_and_assert_contract(contract, &mut app, &requests);
    }
    assert_eq!(
        traces.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BUN_ASSETS
            .iter()
            .map(|contract| bun_trace_name(contract.asset))
            .collect::<BTreeSet<_>>(),
        "every plugin trace belongs to one plugin asset"
    );
}

#[test]
fn unknown_session_start_source_records_a_session_but_never_replaces_one() {
    let _environment = IsolatedEnv::new();
    let scratch = ScratchDir::new("unknown-session-start-source");
    // Antigravity's policy replaces its session on a report with no start
    // source, so an unknown source read as an omitted one would replace here.
    // A dropped report would instead leave the pane with nothing to resume.
    let mut app = AgentReportHarness::new(scratch.path(), Agent::Antigravity, clock_sample(0))
        .expect("build App");
    let pane_id = app.pane_id().to_owned();
    let request = |session_id: &str, session_start_source: Option<&str>, seq: u64| -> Request {
        let mut params = serde_json::json!({
            "pane_id": pane_id.clone(),
            "source": "shepr:agy",
            "agent_session_id": session_id,
            "seq": seq
        });
        if let Some(source) = session_start_source {
            params["session_start_source"] = source.into();
        }
        serde_json::from_value(serde_json::json!({
            "id": format!("test:{seq}"),
            "method": "pane.report_agent_session",
            "params": params
        }))
        .expect("build session report")
    };
    let current_session = |app: &AgentReportHarness| {
        app.terminal_state()
            .expect("test pane keeps terminal")
            .ownership()
            .current_session_identity_for_persistence()
            .expect("a session is stored")
            .session_ref()
            .value_str()
            .to_owned()
    };

    app.apply_request(
        request("first-session", Some("future-source"), 1),
        clock_sample(1),
    )
    .expect("apply first session with an unknown start source");
    assert_eq!(
        current_session(&app),
        "first-session",
        "an unknown start source still records a session when none conflicts"
    );
    app.apply_request(
        request("unknown-session", Some("future-source"), 2),
        clock_sample(2),
    )
    .expect("unknown session start is accepted");
    assert_eq!(current_session(&app), "first-session");

    // The premise: the same report with no start source does replace.
    app.apply_request(request("omitted-session", None, 3), clock_sample(3))
        .expect("apply session report without a start source");
    assert_eq!(current_session(&app), "omitted-session");
}

fn shell_session_steps(contract: &AssetContract) -> Vec<ShellStep> {
    let ContractSessionRef::Id(session_id) = contract.session else {
        panic!("shell asset {} must use an id session", contract.asset);
    };
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
    pane_id: &str,
) -> Vec<Value> {
    let mut requests = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let socket_path = scratch.join(format!("{socket_label}-{index}.sock"));
        // host-program-ok: the shipped agent hook is the shell script under test.
        let mut command = command_in_scratch("sh", "agent-integration-shell-asset");
        command.arg(script);
        // Hook assets report only from panes of a release server.
        command.env(
            shepr_core::env::EnvVar::SheprBuildProfile.name(),
            shepr_paths::BuildProfile::Release.marker(),
        );
        if let Some(action) = step.action {
            command.arg(action);
        }
        let mut input = step.input.to_string().into_bytes();
        input.push(b'\n');
        let output = capture_hook(command, &socket_path, scratch.path(), pane_id, &input);
        assert!(
            output.status.success(),
            "{} exited unsuccessfully",
            script.display()
        );
        requests.extend(output.requests.into_iter().map(|line| {
            serde_json::from_str(&line).expect("decode captured hook request as JSON")
        }));
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

fn replay_and_assert_contract(
    contract: &AssetContract,
    app: &mut AgentReportHarness,
    requests: &[Value],
) {
    assert!(
        !requests.is_empty(),
        "{} emitted no requests",
        contract.asset
    );
    let session_ref = match contract.session {
        ContractSessionRef::Id(id) => AgentSessionRef::id(id),
        ContractSessionRef::Path(path) => AgentSessionRef::path(path),
    }
    .expect("valid contract session reference");
    let origin =
        ReportOrigin::official(contract.agent).expect("a bundled asset has an integration");
    let expected_session = PersistedAgentSession::new(*origin.source(), session_ref)
        .expect("supported agent session identity");

    for (index, request) in requests.iter().enumerate() {
        // Another bundled source would be accepted too; a bundled asset must
        // report under its own.
        let source = request
            .pointer("/params/source")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{} request {index} names no source", contract.asset));
        assert_eq!(
            AgentSource::parse(source).as_ref(),
            Some(origin.source()),
            "{} request {index} used another source",
            contract.asset
        );
        let request: Request = serde_json::from_value(request.clone()).unwrap_or_else(|error| {
            panic!("{} request {index} is invalid: {error}", contract.asset)
        });
        app.apply_request(request, clock_sample(index + 1))
            .unwrap_or_else(|error| {
                panic!(
                    "{} request {index} was rejected: {}",
                    contract.asset,
                    error.into_message()
                )
            });
    }

    let terminal = app
        .terminal_state()
        .expect("the test pane keeps its terminal");
    assert_eq!(
        terminal
            .ownership()
            .current_session_identity_for_persistence()
            .as_ref(),
        Some(&expected_session),
        "{} did not persist the scripted session",
        contract.asset
    );

    match contract.state {
        Some(expected_state) => {
            let authority = terminal
                .ownership()
                .hook_authority()
                .unwrap_or_else(|| panic!("{} did not establish hook authority", contract.asset));
            assert_eq!(authority.origin, origin);
            assert_eq!(authority.state, expected_state);
            assert_eq!(
                authority.session_ref,
                Some(expected_session.session_ref().clone())
            );
        }
        None => assert!(
            terminal.ownership().hook_authority().is_none(),
            "{} should report session identity without state authority",
            contract.asset
        ),
    }
}

/// The server loop's clock for replay step `step`: the agent is detected at
/// step 0 and request `n` lands at step `n + 1`, a millisecond apart.
fn clock_sample(step: usize) -> HookClockSample {
    let step = u64::try_from(step).expect("small replay step");
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
