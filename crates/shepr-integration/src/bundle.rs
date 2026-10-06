//! The generator of the bundled hook assets.
//!
//! Every asset reports to the server socket in one envelope: the pane gate
//! (a release pane of a shepr server, with a socket and a pane id), one JSON
//! request line whose `id` is `<source>:<seq>`, the method and parameter
//! names, the action vocabulary, the descriptor's source, and a
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
//! tests here fail when one is stale; `python3 scripts/regenerate_bundled_assets.py`
//! rewrites them. The generator is test-only
//! because the installer ships the committed bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shepr_agent::{IntegrationHookAction, IntegrationTarget};
use shepr_core::env::{ChildEnv, EnvVar, SHEPR_ENV_IN_PANE};

use shepr_agent::resume::AgentSessionStartSource;
use shepr_api::schema::MethodKind;
use shepr_paths::BuildProfile;

fn method_state() -> &'static str {
    MethodKind::PaneReportAgent.name()
}

fn method_session() -> &'static str {
    MethodKind::PaneReportAgentSession.name()
}

const SOCKET_WAIT: Duration = crate::limits::HOOK_SOCKET_WAIT;

/// The shell around a Python decoder: its gates and bookkeeping.
#[derive(Clone, Copy)]
struct ShellHook {
    /// A file of agent-specific shell gates, run after the shared ones.
    gate: Option<&'static str>,
    /// Whether the shell stamps the report's seq when the hook starts.
    early_seq: bool,
    /// Whether exits print an empty JSON object, as the agent expects.
    empty_object: bool,
}

/// Each kind keeps its own delivery policy, written by hand in its kit: the
/// plugins try once, the extensions retry once in the same queue slot, and the
/// TUI plugin retries while its selection is current. They differ because the
/// agents' lifecycles do; one shared policy is not wanted.
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

/// The asset list. It is also spelled in `lib.rs` (`include_str!` needs a
/// literal path) and in the server's contract test lists; the tests here and
/// the server's asset coverage check hold the copies in step, which is kept
/// over a macro that would generate them.
const SPECS: [AssetSpec; 16] = [
    AssetSpec {
        target: IntegrationTarget::AntigravityCli,
        asset: "antigravity_cli/shepr-agent-state.sh",
        decoder: "antigravity_cli.py",
        kind: shell(None, false, true),
    },
    AssetSpec {
        target: IntegrationTarget::Claude,
        asset: "claude/shepr-agent-state.sh",
        decoder: "claude.py",
        kind: shell(Some("claude.gate.sh"), false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Codex,
        asset: "codex/shepr-agent-state.sh",
        decoder: "codex.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Copilot,
        asset: "copilot/shepr-agent-state.sh",
        decoder: "copilot.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Cursor,
        asset: "cursor/shepr-agent-state.sh",
        decoder: "cursor.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Devin,
        asset: "devin/shepr-agent-state.sh",
        decoder: "devin.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Droid,
        asset: "droid/shepr-agent-state.sh",
        decoder: "droid.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Grok,
        asset: "grok/shepr-agent-state.sh",
        decoder: "grok.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Kimi,
        asset: "kimi/shepr-agent-state.sh",
        decoder: "kimi.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Mastracode,
        asset: "mastracode/shepr-agent-state.sh",
        decoder: "mastracode.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Kilo,
        asset: "kilo/shepr-agent-state.js",
        decoder: "kilo.js",
        kind: Kind::Plugin,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
        asset: "opencode/shepr-agent-state.js",
        decoder: "opencode.js",
        kind: Kind::Plugin,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
        asset: "opencode/shepr-tui-session.js",
        decoder: "opencode_tui.js",
        kind: Kind::Tui,
    },
    AssetSpec {
        target: IntegrationTarget::Pi,
        asset: "pi/shepr-agent-state.ts",
        decoder: "pi.ts",
        kind: Kind::Extension,
    },
    AssetSpec {
        target: IntegrationTarget::Omp,
        asset: "omp/shepr-agent-state.ts",
        decoder: "omp.ts",
        kind: Kind::Extension,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
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
  # content, so normal and error exits print a neutral object.
  printf '{}\n' || true";

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
            "TUI_MAX_RETAINED",
            crate::limits::TUI_MAX_RETAINED_EVENTS.to_string(),
        ),
        (
            "OMP_IDLE_MS",
            crate::limits::OMP_IDLE_DEBOUNCE.as_millis().to_string(),
        ),
        (
            "OMP_GRACE_MS",
            crate::limits::OMP_RETRY_GRACE.as_millis().to_string(),
        ),
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
        ("ENV_PANE", ChildEnv::SheprPaneId.name().to_owned()),
        ("STATES_JS", format!("{{ {} }}", states.join(", "))),
        ("STATE_UNION", quoted(&state_names).join(" | ")),
        ("EVENTS_BY_ACTION", by_action),
        ("EVENTS", py_tuple(&all)),
        ("SEQ_UNITS_NOTE", template("seq_units.txt")),
    ]
}

fn js_header() -> &'static str {
    "// installed by shepr\n\
     // managed by shepr; every release shepr server launch on this host rewrites this file.\n\
     // add custom hooks/plugins beside this file instead of editing it.\n"
}

fn render_shell(spec: &AssetSpec, hook: ShellHook) -> String {
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
    let finish_trap_reset = if hook.empty_object {
        "  trap - EXIT HUP INT TERM"
    } else {
        ""
    };
    let exit_trap = if hook.empty_object {
        "trap 'finish' EXIT"
    } else {
        ""
    };
    let early_seq = if hook.early_seq { EARLY_SEQ } else { "" };
    let mut values = common;
    values.extend([
        ("FINISH_BODY", finish_body.to_owned()),
        ("FINISH_TRAP_RESET", finish_trap_reset.to_owned()),
        ("EXIT_TRAP", exit_trap.to_owned()),
        ("EARLY_SEQ", early_seq.to_owned()),
        ("ACTION_GATE", action_gate),
        ("SHELL_GATE", shell_gate),
        (
            "PYTHON",
            python.trim_end_matches('\n').replace('\'', "'\"'\"'"),
        ),
    ]);
    fill(&template("shell_hook.sh"), &values)
}

fn render(spec: &AssetSpec) -> String {
    let text = match spec.kind {
        Kind::Shell(hook) => render_shell(spec, hook),
        Kind::Plugin => format!(
            "{}\n{}{}{}",
            js_header(),
            fill(&template("plugin_kit.js"), &common_facts(spec)),
            template("opencode_family.js"),
            fill(&decoder(spec.decoder), &common_facts(spec))
        ),
        Kind::Tui => format!(
            "{}\n{}{}",
            js_header(),
            fill(&template("tui_kit.js"), &common_facts(spec)),
            fill(&decoder(spec.decoder), &common_facts(spec))
        ),
        Kind::TuiEntrypoint => {
            format!("{}\n{}", js_header(), decoder(spec.decoder))
        }
        Kind::Extension => format!(
            "{}{}{}",
            js_header(),
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
            "{} is stale: regenerate it with `python3 scripts/regenerate_bundled_assets.py`",
            spec.asset
        );
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
    assert!(asset.ends_with("export { default } from \"../shepr-tui-session.js\";\n"));
}

/// Emits generated bytes for the manual regeneration script. Even broad
/// brokkr test filters may run ignored tests, so this test must never write.
#[test]
#[ignore = "emits asset JSON for scripts/regenerate_bundled_assets.py"]
fn emit_bundled_assets() {
    let assets: std::collections::BTreeMap<&str, String> = SPECS
        .iter()
        .map(|spec| (spec.asset, render(spec)))
        .collect();
    println!(
        "SHEPR_BUNDLED_ASSETS={}",
        serde_json::to_string(&assets).expect("generated assets serialize")
    );
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

// This witnesses ordinary transport APIs and imports, including renamed imports.
// It is not a parser or a sandbox for arbitrary dynamic decoder code.
fn decoder_owns_transport(text: &str) -> bool {
    let dependency = regex::Regex::new(
        r#"(?m)\b(?:from|import)\s+(?:socket|random)\b|(?:from\s*|require\s*\(\s*|import\s*\(\s*)[\"'](?:node:)?(?:net|tls|http|https|dgram|child_process)[\"']|\b(?:net\s*\.\s*connect|socket\s*\.\s*create_connection)\s*\("#,
    )
    .expect("test precondition");
    dependency.is_match(text)
}

#[test]
fn envelope_transport_witness_catches_alternative_apis_and_import_aliases() {
    for text in [
        "net.connect(path)",
        "socket.create_connection(path)",
        "import net from 'node:net'",
        "const n = require('net')",
        "import socket as s",
        "from socket import socket as connect",
    ] {
        assert!(decoder_owns_transport(text), "{text}");
    }
    assert!(!decoder_owns_transport("decode(payload)"));
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
        ChildEnv::SheprPaneId.name(),
        "createConnection",
        "net.connect",
        "socket.create_connection",
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
    // Do not ban timers by name: OMP's two timers model agent state, and the
    // TUI decoder polls its route and retries its current selection through
    // the shared transport. Only these complete, reviewed calls belong in
    // decoders.
    let allowed_timers = [
        "setTimeout(() => {\n      idleTimer = undefined;\n      publishState();\n    }, idleDebounceMs)",
        "setTimeout(() => {\n      retryTimer = undefined;\n      retryHoldActive = false;\n      failureBlocked = true;\n      publishState();\n    }, retryGraceMs)",
        "setTimeout(() => {\n      retryTimer = undefined;\n      publish();\n    }, RETRY_WAIT_MS)",
        "setInterval(syncSelection, ROUTE_POLL_INTERVAL_MS)",
    ];
    let timer_call =
        regex::Regex::new(r"\b(?:setTimeout|setInterval)\s*\(").expect("test precondition");
    assert!(timer_call.is_match("setTimeout(deliver, wait)"));
    let numeric_delay = regex::Regex::new(
        r"Date\.now\(\)\s*\+\s*[0-9]|AbortSignal\.timeout\(\s*[0-9]|(?:setTimeout|setInterval)\([^;]*,\s*[0-9][0-9_]*\s*\)",
    ).expect("test precondition");
    for spec in &SPECS {
        let name = format!("decoders/{}", spec.decoder);
        let text = decoder(spec.decoder);
        assert!(
            !decoder_owns_transport(&text),
            "{name} owns a transport dependency"
        );
        assert!(
            !numeric_delay.is_match(&text),
            "{name} spells a numeric delay"
        );
        let without_reviewed_timers = allowed_timers
            .iter()
            .fold(text.clone(), |text, timer| text.replace(timer, ""));
        assert!(
            !timer_call.is_match(&without_reviewed_timers),
            "{name} owns an unreviewed timer"
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
        assert!(
            !decoder_owns_transport(&text),
            "{name} owns a transport dependency"
        );
        let without_reviewed_timers = allowed_timers
            .iter()
            .fold(text.clone(), |text, timer| text.replace(timer, ""));
        assert!(
            !timer_call.is_match(&without_reviewed_timers),
            "{name} owns an unreviewed timer"
        );
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
    let spec = |target| {
        SPECS
            .iter()
            .find(|spec| spec.target == target)
            .expect("target has a bundled hook")
    };
    let codex = render(spec(IntegrationTarget::Codex));
    assert!(codex.contains("case \"$action\" in\n  session|working|idle) ;;"));
    assert!(codex.contains(
        "EVENTS_BY_ACTION = {\"session\": (\"SessionStart\",), \"working\": (\"UserPromptSubmit\",), \"idle\": (\"Stop\", \"Interrupt\")}"
    ));
    let devin = render(spec(IntegrationTarget::Devin));
    assert!(devin.contains("EVENTS = (\"SessionStart\", \"UserPromptSubmit\")"));
    // Copilot's descriptor registers an event with no action, so its hook has
    // no action argument to gate on.
    let copilot = render(spec(IntegrationTarget::Copilot));
    assert!(!copilot.contains("case \"$action\""));
    assert!(copilot.contains("EVENTS_BY_ACTION = {}"));
}
