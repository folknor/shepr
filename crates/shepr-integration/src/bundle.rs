//! The generator of the bundled hook assets.
//!
//! Every asset reports to the server socket in one envelope: the pane gate
//! (a release pane of a shepr server, with a socket and a pane id), one JSON
//! request line whose `id` is `<source>:<seq>`, the method and parameter
//! names, the action vocabulary, the descriptor's source and label, and a
//! bounded wait on the socket. Those facts live here and in the agent
//! descriptor table, and nowhere else. An asset is its agent's decoder
//! (what the agent's own payload means) appended to a preamble generated for
//! its language: `templates/hook_kit.py` under `templates/shell_hook.sh` for
//! the shell hooks, `templates/plugin_kit.js` for the OpenCode and Kilo server
//! plugins, `templates/tui_kit.js` for the OpenCode TUI plugin and
//! `templates/extension_kit.ts` for the Pi and OMP extensions. The OpenCode v2
//! loader entrypoint is another generated asset with its own small decoder
//! template. The reporting decoders under `templates/decoders` differ because
//! the agents' payloads and lifecycles differ, and they spell none of the
//! envelope.
//!
//! The assets under `assets/` are the generated output, committed because the
//! bun tests and the server crate's contract tests run them from disk. The
//! tests here fail when one is stale; `brokkr test -p shepr-integration
//! regenerate_bundled_assets` rewrites them. The generator is test-only
//! because the installer ships the committed bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shepr_agent::{IntegrationHookAction, IntegrationTarget};
use shepr_core::env::{EnvVar, SHEPR_ENV_IN_PANE};

use shepr_agent::resume::AgentSessionStartSource;
use shepr_api::schema::{
    Method, PaneReportAgentParams, PaneReportAgentSessionParams, PaneReportAgentState,
};
use shepr_paths::BuildProfile;

fn method_state() -> &'static str {
    Method::PaneReportAgent(PaneReportAgentParams {
        pane_id: String::new(),
        source: String::new(),
        agent: String::new(),
        state: PaneReportAgentState::Idle,
        seq: None,
        agent_session_id: None,
        agent_session_path: None,
    })
    .traits()
    .name
}

fn method_session() -> &'static str {
    Method::PaneReportAgentSession(PaneReportAgentSessionParams {
        pane_id: String::new(),
        source: String::new(),
        agent: String::new(),
        seq: None,
        agent_session_id: None,
        agent_session_path: None,
        session_start_source: None,
    })
    .traits()
    .name
}

const SOCKET_WAIT: Duration = crate::limits::HOOK_SOCKET_WAIT;

/// The shell around a Python decoder: its gates and bookkeeping.
#[derive(Clone, Copy)]
struct ShellHook {
    /// A file of agent-specific shell gates, run after the shared ones.
    gate: Option<&'static str>,
    /// Whether the shell stamps the report's seq when the hook starts.
    early_seq: bool,
    /// Whether every exit prints an empty JSON object, as the agent expects.
    empty_object: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Shell(ShellHook),
    /// The OpenCode and Kilo server plugins: one attempt per report.
    Plugin,
    /// The OpenCode TUI plugin, which keeps its own selection transport.
    Tui,
    /// The OpenCode v2 loader entrypoint that re-exports the generated TUI plugin.
    TuiEntrypoint,
    /// The Pi and OMP extensions: a retry and a coalescing state queue.
    Extension,
}

struct AssetSpec {
    target: IntegrationTarget,
    /// The `SHEPR_INTEGRATION_ID` header, the target's name when absent.
    id: Option<&'static str>,
    /// The generated file, relative to `assets/`.
    asset: &'static str,
    /// The agent's decoder, relative to `templates/decoders/`.
    decoder: &'static str,
    kind: Kind,
}

const fn shell(gate: Option<&'static str>, early_seq: bool, empty_object: bool) -> Kind {
    Kind::Shell(ShellHook {
        gate,
        early_seq,
        empty_object,
    })
}

const SPECS: [AssetSpec; 16] = [
    AssetSpec {
        target: IntegrationTarget::AntigravityCli,
        id: None,
        asset: "antigravity_cli/shepr-agent-state.sh",
        decoder: "antigravity_cli.py",
        kind: shell(None, false, true),
    },
    AssetSpec {
        target: IntegrationTarget::Claude,
        id: None,
        asset: "claude/shepr-agent-state.sh",
        decoder: "claude.py",
        kind: shell(Some("claude.gate.sh"), false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Codex,
        id: None,
        asset: "codex/shepr-agent-state.sh",
        decoder: "codex.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Copilot,
        id: None,
        asset: "copilot/shepr-agent-state.sh",
        decoder: "copilot.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Cursor,
        id: None,
        asset: "cursor/shepr-agent-state.sh",
        decoder: "cursor.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Devin,
        id: None,
        asset: "devin/shepr-agent-state.sh",
        decoder: "devin.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Droid,
        id: None,
        asset: "droid/shepr-agent-state.sh",
        decoder: "droid.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Grok,
        id: None,
        asset: "grok/shepr-agent-state.sh",
        decoder: "grok.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Kimi,
        id: None,
        asset: "kimi/shepr-agent-state.sh",
        decoder: "kimi.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Mastracode,
        id: None,
        asset: "mastracode/shepr-agent-state.sh",
        decoder: "mastracode.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Kilo,
        id: None,
        asset: "kilo/shepr-agent-state.js",
        decoder: "kilo.js",
        kind: Kind::Plugin,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
        id: None,
        asset: "opencode/shepr-agent-state.js",
        decoder: "opencode.js",
        kind: Kind::Plugin,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
        id: Some("opencode-tui"),
        asset: "opencode/shepr-tui-session.js",
        decoder: "opencode_tui.js",
        kind: Kind::Tui,
    },
    AssetSpec {
        target: IntegrationTarget::Pi,
        id: None,
        asset: "pi/shepr-agent-state.ts",
        decoder: "pi.ts",
        kind: Kind::Extension,
    },
    AssetSpec {
        target: IntegrationTarget::Omp,
        id: None,
        asset: "omp/shepr-agent-state.ts",
        decoder: "omp.ts",
        kind: Kind::Extension,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
        id: Some("opencode-tui-v2"),
        asset: "opencode/tui.js",
        decoder: "opencode_tui_entry.js",
        kind: Kind::TuiEntrypoint,
    },
];

/// The hook-reportable states, in the order the assets list them.
const STATES: [IntegrationHookAction; 3] = [
    IntegrationHookAction::Working,
    IntegrationHookAction::Blocked,
    IntegrationHookAction::Idle,
];

const EARLY_SEQ: &str = r#"# Stamp the report the moment the hook starts. Every event runs this script in
# a fresh process, and shepr drops a report whose seq is older than the last
# one it accepted, so taking the timestamp after python3 has started would let
# interpreter startup jitter reorder near-simultaneous events.
hook_seq="$(date +%s%N 2>/dev/null || true)""#;

const EMPTY_OBJECT: &str = r"  # Antigravity CLI expects a JSON object on stdout and this hook never injects
  # anything, so every exit path emits an empty object.
  printf '{}\n'";

fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/templates")
}

fn assets_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/assets")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn template(name: &str) -> String {
    read(&templates_dir().join(name))
}

fn decoder(name: &str) -> String {
    template(&format!("decoders/{name}"))
}

/// Replaces each `@NAME@`. A line holding only an empty value's token goes
/// away with it.
fn fill(text: &str, values: &[(&str, String)]) -> String {
    let mut text = text.to_owned();
    for (name, value) in values {
        let token = format!("@{name}@");
        if value.is_empty() {
            text = text.replace(&format!("{token}\n"), "");
        }
        text = text.replace(&token, value);
    }
    text
}

fn quoted(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| format!("\"{item}\"")).collect()
}

fn py_tuple(items: &[&str]) -> String {
    match items {
        [] => "()".to_owned(),
        [only] => format!("(\"{only}\",)"),
        _ => format!("({})", quoted(items).join(", ")),
    }
}

/// The events of each action, and every event once.
type EventTables = (Vec<(&'static str, Vec<&'static str>)>, Vec<&'static str>);

/// The hook events the descriptor registers for `target`, per action in the
/// order each action first appears, and every event once.
fn event_tables(target: IntegrationTarget) -> EventTables {
    let mut by_action: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    let mut all: Vec<&'static str> = Vec::new();
    for hook in target.hook_events() {
        if !all.contains(&hook.event) {
            all.push(hook.event);
        }
        let Some(action) = hook.action else {
            continue;
        };
        let name = action.as_str();
        let index = match by_action.iter().position(|(known, _)| *known == name) {
            Some(index) => index,
            None => {
                by_action.push((name, Vec::new()));
                by_action.len() - 1
            }
        };
        if !by_action[index].1.contains(&hook.event) {
            by_action[index].1.push(hook.event);
        }
    }
    (by_action, all)
}

fn integration_id(spec: &AssetSpec) -> String {
    if let Some(id) = spec.id {
        return id.to_owned();
    }
    match serde_json::to_value(spec.target) {
        Ok(serde_json::Value::String(name)) => name,
        other => panic!("{:?} has no serialized name: {other:?}", spec.target),
    }
}

/// The values every language's preamble spells.
fn common_facts(spec: &AssetSpec) -> Vec<(&'static str, String)> {
    let (by_action, all) = event_tables(spec.target);
    let by_action = if by_action.is_empty() {
        "{}".to_owned()
    } else {
        let entries: Vec<String> = by_action
            .iter()
            .map(|(action, events)| format!("\"{action}\": {}", py_tuple(events)))
            .collect();
        format!("{{{}}}", entries.join(", "))
    };
    let state_names: Vec<&str> = STATES
        .iter()
        .copied()
        .map(IntegrationHookAction::as_str)
        .collect();
    let states: Vec<String> = state_names
        .iter()
        .map(|name| format!("{name}: \"{name}\""))
        .collect();
    let starts = [
        AgentSessionStartSource::Startup,
        AgentSessionStartSource::Resume,
        AgentSessionStartSource::Select,
    ];
    let names: Vec<&str> = starts.iter().map(|source| source.as_str()).collect();
    let start_js = names
        .iter()
        .map(|name| format!("{name}: \"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let start_py = names
        .iter()
        .map(|name| format!("\"{name}\": \"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        ("START_JS", format!("{{ {start_js} }}")),
        ("START_PY", format!("{{{start_py}}}")),
        (
            "TUI_RETRY_MS",
            crate::limits::TUI_RETRY_WAIT.as_millis().to_string(),
        ),
        (
            "TUI_REQUEST_MS",
            crate::limits::TUI_REQUEST_WAIT.as_millis().to_string(),
        ),
        (
            "TUI_POLL_MS",
            crate::limits::TUI_ROUTE_POLL.as_millis().to_string(),
        ),
        (
            "TUI_SELECTION_DELAYS_MS",
            format!(
                "[{}]",
                crate::limits::TUI_SELECTION_RETRIES
                    .iter()
                    .map(|delay| delay.as_millis().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        (
            "OMP_IDLE_MS",
            crate::limits::OMP_IDLE_DEBOUNCE.as_millis().to_string(),
        ),
        (
            "OMP_GRACE_MS",
            crate::limits::OMP_RETRY_GRACE.as_millis().to_string(),
        ),
        ("LABEL", spec.target.label().to_owned()),
        ("SOURCE", spec.target.source().to_owned()),
        ("METHOD_SESSION", method_session().to_owned()),
        ("METHOD_STATE", method_state().to_owned()),
        (
            "ACTION_SESSION",
            IntegrationHookAction::Session.as_str().to_owned(),
        ),
        ("SOCKET_WAIT_MS", SOCKET_WAIT.as_millis().to_string()),
        ("SOCKET_WAIT_SECONDS", SOCKET_WAIT.as_secs_f64().to_string()),
        ("ENV_PROFILE", EnvVar::SheprBuildProfile.name().to_owned()),
        ("PROFILE_RELEASE", BuildProfile::Release.marker().to_owned()),
        ("ENV_MARKER", EnvVar::SheprEnv.name().to_owned()),
        ("ENV_MARKER_VALUE", SHEPR_ENV_IN_PANE.to_owned()),
        ("ENV_SOCKET", EnvVar::SheprSocketPath.name().to_owned()),
        ("ENV_PANE", EnvVar::SheprPaneId.name().to_owned()),
        ("STATES_JS", format!("{{ {} }}", states.join(", "))),
        ("STATE_UNION", quoted(&state_names).join(" | ")),
        ("EVENTS_BY_ACTION", by_action),
        ("EVENTS", py_tuple(&all)),
        ("SEQ_UNITS_NOTE", template("seq_units.txt")),
    ]
}

fn js_header(spec: &AssetSpec, version: &str) -> String {
    format!(
        "// installed by shepr\n\
         // managed by shepr; every release shepr server launch on this host rewrites this file.\n\
         // add custom hooks/plugins beside this file instead of editing it.\n\
         // SHEPR_INTEGRATION_ID={}\n\
         // SHEPR_INTEGRATION_VERSION={}\n",
        integration_id(spec),
        version
    )
}

fn render_shell(spec: &AssetSpec, hook: ShellHook, version: &str) -> String {
    let common = common_facts(spec);
    let python = format!(
        "{}{}",
        fill(&template("hook_kit.py"), &common),
        fill(&decoder(spec.decoder), &common)
    );
    let (by_action, _) = event_tables(spec.target);
    let actions: Vec<&str> = by_action.iter().map(|(action, _)| *action).collect();
    let action_gate = if actions.is_empty() {
        String::new()
    } else {
        format!(
            "case \"$action\" in\n  {}) ;;\n  *) finish ;;\nesac",
            actions.join("|")
        )
    };
    let shell_gate = match hook.gate {
        Some(name) => template(&format!("decoders/{name}")).trim_end().to_owned(),
        None => String::new(),
    };
    let finish_body = if hook.empty_object { EMPTY_OBJECT } else { "" };
    let early_seq = if hook.early_seq { EARLY_SEQ } else { "" };
    let mut values = common;
    values.extend([
        ("ID", integration_id(spec)),
        ("VERSION", version.to_owned()),
        ("FINISH_BODY", finish_body.to_owned()),
        ("EARLY_SEQ", early_seq.to_owned()),
        ("ACTION_GATE", action_gate),
        ("SHELL_GATE", shell_gate),
        ("PYTHON", python.trim_end_matches('\n').to_owned()),
    ]);
    fill(&template("shell_hook.sh"), &values)
}

/// The version marker is diagnostic metadata in install logs. Derive it from
/// the version-neutral generated bytes (FNV-1a), so an edit to a template or
/// decoder cannot leave a stale marker. Exact asset bytes, not this compact
/// marker, determine whether an installed integration is current.
fn content_version(content: &str) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in content.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn render(spec: &AssetSpec) -> String {
    let canonical = render_with_version(spec, "0");
    render_with_version(spec, &content_version(&canonical).to_string())
}

fn render_with_version(spec: &AssetSpec, version: &str) -> String {
    let text = match spec.kind {
        Kind::Shell(hook) => render_shell(spec, hook, version),
        Kind::Plugin => format!(
            "{}\n{}{}{}",
            js_header(spec, version),
            fill(&template("plugin_kit.js"), &common_facts(spec)),
            template("opencode_family.js"),
            fill(&decoder(spec.decoder), &common_facts(spec))
        ),
        Kind::Tui => format!(
            "{}\n{}{}",
            js_header(spec, version),
            fill(&template("tui_kit.js"), &common_facts(spec)),
            fill(&decoder(spec.decoder), &common_facts(spec))
        ),
        Kind::TuiEntrypoint => {
            format!("{}\n{}", js_header(spec, version), decoder(spec.decoder))
        }
        Kind::Extension => format!(
            "{}{}{}",
            js_header(spec, version),
            fill(&template("extension_kit.ts"), &common_facts(spec)),
            fill(&decoder(spec.decoder), &common_facts(spec))
        ),
    };
    let leftover = regex::Regex::new(r"@[A-Z_]+@").expect("test precondition");
    assert!(
        !leftover.is_match(&text),
        "{} keeps an unfilled placeholder",
        spec.asset
    );
    text
}

#[test]
fn bundled_assets_are_current() {
    for spec in &SPECS {
        let path = assets_dir().join(spec.asset);
        assert!(
            read(&path) == render(spec),
            "{} is stale: regenerate it with `brokkr test -p shepr-integration regenerate_bundled_assets`",
            spec.asset
        );
    }
}

#[test]
fn generated_versions_come_from_version_neutral_asset_content() {
    for spec in &SPECS {
        let canonical = render_with_version(spec, "0");
        let expected = content_version(&canonical);
        let rendered = render(spec);
        let marker = rendered
            .lines()
            .find_map(|line| line.split_once("SHEPR_INTEGRATION_VERSION="))
            .map(|(_, value)| value.trim())
            .expect("generated assets carry diagnostic version metadata");
        assert_eq!(marker.parse::<u32>().ok(), Some(expected), "{}", spec.asset);
    }
}

#[test]
fn opencode_v2_tui_entrypoint_is_managed_generated_asset() {
    let spec = SPECS
        .iter()
        .find(|spec| spec.asset == "opencode/tui.js")
        .expect("the v2 TUI entrypoint has a spec");
    let asset = render(spec);
    assert!(asset.contains(
        "// managed by shepr; every release shepr server launch on this host rewrites this file."
    ));
    assert!(asset.contains("// SHEPR_INTEGRATION_ID=opencode-tui-v2\n"));
    assert!(asset.ends_with("export { default } from \"../shepr-tui-session.js\";\n"));
}

/// Rewrites the committed assets from the templates. Ignored so the gate never
/// writes into the tree; a filter naming it runs it.
#[test]
#[ignore = "rewrites the committed assets: brokkr test -p shepr-integration regenerate_bundled_assets"]
fn regenerate_bundled_assets() {
    for spec in &SPECS {
        let path = assets_dir().join(spec.asset);
        fs::write(&path, render(spec))
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    }
}

#[test]
fn every_asset_with_a_decoder_is_generated() {
    // Every shipped asset is generated; test files are the only non-assets.
    let generated: Vec<&str> = SPECS.iter().map(|spec| spec.asset).collect();
    let mut files = Vec::new();
    super::tests::collect_asset_files(&assets_dir(), &mut files);
    for file in files {
        let relative = file
            .strip_prefix(assets_dir())
            .expect("an asset lies under the assets directory")
            .to_string_lossy()
            .into_owned();
        if relative.ends_with(".test.ts") {
            continue;
        }
        assert!(
            generated.contains(&relative.as_str()),
            "{relative} is an asset no spec generates"
        );
    }
}

/// The decoders differ by agent, and none spells the shared envelope: the
/// pane gate, the transport, the method names or the agent's own source. The
/// generated assets carry exactly one spelling of each.
#[test]
fn hook_assets_share_one_envelope() {
    let forbidden: Vec<String> = [
        method_state(),
        EnvVar::SheprBuildProfile.name(),
        EnvVar::SheprSocketPath.name(),
        EnvVar::SheprPaneId.name(),
        "createConnection",
        "settimeout",
        "AF_UNIX",
        "Math.random",
        "\"startup\"",
        "\"resume\"",
        "\"select\"",
        "import random",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(IntegrationTarget::all().map(|target| format!("\"{}\"", target.source())))
    .collect();
    let numeric_delay = regex::Regex::new(
        r"Date\.now\(\)\s*\+\s*[0-9]|AbortSignal\.timeout\(\s*[0-9]|(?:setTimeout|setInterval)\([^;]*,\s*[0-9][0-9_]*\s*\)",
    ).expect("test precondition");
    for spec in &SPECS {
        let name = format!("decoders/{}", spec.decoder);
        let text = decoder(spec.decoder);
        assert!(
            !numeric_delay.is_match(&text),
            "{name} spells a numeric delay"
        );
        for needle in &forbidden {
            assert!(
                !text.contains(needle.as_str()),
                "{name} spells `{needle}`, which only the generated preamble may"
            );
        }
    }
    for name in ["opencode_family.js", "decoders/claude.gate.sh"] {
        let text = template(name);
        for needle in &forbidden {
            assert!(
                !text.contains(needle.as_str()),
                "{name} spells `{needle}`, which only the generated preamble may"
            );
        }
    }

    let id = regex::Regex::new(r#""id": f"\{SOURCE\}:\{report_seq\}"|id: `\$\{(?:SOURCE|source)\}:\$\{seq\}`|id: `\$\{SOURCE\}:\$\{state === undefined \? seedSeq\(\) : seq\}`"#)
        .expect("test precondition");
    for spec in &SPECS {
        if matches!(spec.kind, Kind::TuiEntrypoint) {
            continue;
        }
        let text = render(spec);
        let name = spec.asset;
        assert!(
            id.is_match(&text),
            "{name} builds no `<source>:<seq>` request id"
        );
        assert!(
            text.contains(&format!("\"{}\"", spec.target.source())),
            "{name} does not report its descriptor source"
        );
        let wait = match spec.kind {
            Kind::Shell(_) => format!("SOCKET_WAIT_SECONDS = {}\n", SOCKET_WAIT.as_secs_f64()),
            Kind::Plugin | Kind::Tui | Kind::Extension => {
                format!("SOCKET_WAIT_MS = {};\n", SOCKET_WAIT.as_millis())
            }
            Kind::TuiEntrypoint => unreachable!("TUI entrypoints do not report"),
        };
        assert!(
            text.contains(&wait),
            "{name} does not use the socket wait limit"
        );
        for method in [method_session(), method_state()] {
            assert_eq!(
                text.matches(&format!("\"{method}\"")).count(),
                1,
                "{name} spells {method} other than once"
            );
        }
    }
}

/// A shell hook only acts on the actions and events the descriptor registers.
#[test]
fn shell_hook_gates_follow_the_descriptor_events() {
    let codex = render(&SPECS[2]);
    assert!(codex.contains("case \"$action\" in\n  session|working|idle) ;;"));
    assert!(codex.contains(
        "EVENTS_BY_ACTION = {\"session\": (\"SessionStart\",), \"working\": (\"UserPromptSubmit\",), \"idle\": (\"Stop\", \"Interrupt\")}"
    ));
    let devin = render(&SPECS[5]);
    assert!(devin.contains("EVENTS = (\"SessionStart\", \"UserPromptSubmit\")"));
    // Copilot's descriptor registers an event with no action, so its hook has
    // no action argument to gate on.
    let copilot = render(&SPECS[3]);
    assert!(!copilot.contains("case \"$action\""));
    assert!(copilot.contains("EVENTS_BY_ACTION = {}"));
}
