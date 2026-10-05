//! The generator of the bundled hook assets.
//!
//! Every asset reports to the server socket in one envelope: the pane gate
//! (a release pane of a shepr server, with a socket and a pane id), one JSON
//! request line whose `id` is `<source>:<seq>`, the method and parameter
//! names, the action vocabulary, the descriptor's source and label, and a
//! 500 ms wait on the socket. Those facts live here and in the agent
//! descriptor table, and nowhere else. An asset is its agent's decoder
//! (what the agent's own payload means) appended to a preamble generated for
//! its language: `templates/hook_kit.py` under `templates/shell_hook.sh` for
//! the shell hooks, `templates/plugin_kit.js` for the OpenCode and Kilo server
//! plugins, `templates/tui_kit.js` for the OpenCode TUI plugin and
//! `templates/extension_kit.ts` for the Pi and OMP extensions. The decoders
//! under `templates/decoders` differ because the agents' payloads and
//! lifecycles differ, and they spell none of the envelope.
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

/// The `SHEPR_BUILD_PROFILE` value of a release server's panes, the only
/// profile whose panes the shared agent configs report from. Spelled in
/// `shepr-paths`, which this crate cannot depend on.
const RELEASE_PROFILE: &str = "release";
/// The API methods the hooks call, as `shepr-api` names them. That crate is
/// above this one, so the tests in `shepr-server` replay each asset's requests
/// against the real handlers.
const METHOD_SESSION: &str = "pane.report_agent_session";
const METHOD_STATE: &str = "pane.report_agent";
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
    /// The Pi and OMP extensions: a retry and a coalescing state queue.
    Extension,
}

struct AssetSpec {
    target: IntegrationTarget,
    /// The `SHEPR_INTEGRATION_ID` header, the target's name when absent.
    id: Option<&'static str>,
    version: u32,
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

const SPECS: [AssetSpec; 15] = [
    AssetSpec {
        target: IntegrationTarget::AntigravityCli,
        id: None,
        version: 3,
        asset: "antigravity_cli/shepr-agent-state.sh",
        decoder: "antigravity_cli.py",
        kind: shell(None, false, true),
    },
    AssetSpec {
        target: IntegrationTarget::Claude,
        id: None,
        version: 6,
        asset: "claude/shepr-agent-state.sh",
        decoder: "claude.py",
        kind: shell(Some("claude.gate.sh"), false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Codex,
        id: None,
        version: 6,
        asset: "codex/shepr-agent-state.sh",
        decoder: "codex.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Copilot,
        id: None,
        version: 5,
        asset: "copilot/shepr-agent-state.sh",
        decoder: "copilot.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Cursor,
        id: None,
        version: 4,
        asset: "cursor/shepr-agent-state.sh",
        decoder: "cursor.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Devin,
        id: None,
        version: 5,
        asset: "devin/shepr-agent-state.sh",
        decoder: "devin.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Droid,
        id: None,
        version: 5,
        asset: "droid/shepr-agent-state.sh",
        decoder: "droid.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Grok,
        id: None,
        version: 5,
        asset: "grok/shepr-agent-state.sh",
        decoder: "grok.py",
        kind: shell(None, false, false),
    },
    AssetSpec {
        target: IntegrationTarget::Kimi,
        id: None,
        version: 5,
        asset: "kimi/shepr-agent-state.sh",
        decoder: "kimi.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Mastracode,
        id: None,
        version: 7,
        asset: "mastracode/shepr-agent-state.sh",
        decoder: "mastracode.py",
        kind: shell(None, true, false),
    },
    AssetSpec {
        target: IntegrationTarget::Kilo,
        id: None,
        version: 6,
        asset: "kilo/shepr-agent-state.js",
        decoder: "kilo.js",
        kind: Kind::Plugin,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
        id: None,
        version: 4,
        asset: "opencode/shepr-agent-state.js",
        decoder: "opencode.js",
        kind: Kind::Plugin,
    },
    AssetSpec {
        target: IntegrationTarget::Opencode,
        id: Some("opencode-tui"),
        version: 3,
        asset: "opencode/shepr-tui-session.js",
        decoder: "opencode_tui.js",
        kind: Kind::Tui,
    },
    AssetSpec {
        target: IntegrationTarget::Pi,
        id: None,
        version: 3,
        asset: "pi/shepr-agent-state.ts",
        decoder: "pi.ts",
        kind: Kind::Extension,
    },
    AssetSpec {
        target: IntegrationTarget::Omp,
        id: None,
        version: 3,
        asset: "omp/shepr-agent-state.ts",
        decoder: "omp.ts",
        kind: Kind::Extension,
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
    vec![
        ("LABEL", spec.target.label().to_owned()),
        ("SOURCE", spec.target.source().to_owned()),
        ("METHOD_SESSION", METHOD_SESSION.to_owned()),
        ("METHOD_STATE", METHOD_STATE.to_owned()),
        (
            "ACTION_SESSION",
            IntegrationHookAction::Session.as_str().to_owned(),
        ),
        ("SOCKET_WAIT_MS", SOCKET_WAIT.as_millis().to_string()),
        ("SOCKET_WAIT_SECONDS", SOCKET_WAIT.as_secs_f64().to_string()),
        ("ENV_PROFILE", EnvVar::SheprBuildProfile.name().to_owned()),
        ("PROFILE_RELEASE", RELEASE_PROFILE.to_owned()),
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

fn js_header(spec: &AssetSpec) -> String {
    format!(
        "// installed by shepr\n\
         // managed by shepr; every release shepr server launch on this host rewrites this file.\n\
         // add custom hooks/plugins beside this file instead of editing it.\n\
         // SHEPR_INTEGRATION_ID={}\n\
         // SHEPR_INTEGRATION_VERSION={}\n",
        integration_id(spec),
        spec.version
    )
}

fn render_shell(spec: &AssetSpec, hook: ShellHook) -> String {
    let common = common_facts(spec);
    let python = format!(
        "{}{}",
        fill(&template("hook_kit.py"), &common),
        decoder(spec.decoder)
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
        ("VERSION", spec.version.to_string()),
        ("FINISH_BODY", finish_body.to_owned()),
        ("EARLY_SEQ", early_seq.to_owned()),
        ("ACTION_GATE", action_gate),
        ("SHELL_GATE", shell_gate),
        ("PYTHON", python.trim_end_matches('\n').to_owned()),
    ]);
    fill(&template("shell_hook.sh"), &values)
}

fn render(spec: &AssetSpec) -> String {
    let text = match spec.kind {
        Kind::Shell(hook) => render_shell(spec, hook),
        Kind::Plugin => format!(
            "{}\n{}{}{}",
            js_header(spec),
            fill(&template("plugin_kit.js"), &common_facts(spec)),
            template("opencode_family.js"),
            decoder(spec.decoder)
        ),
        Kind::Tui => format!(
            "{}\n{}{}",
            js_header(spec),
            fill(&template("tui_kit.js"), &common_facts(spec)),
            decoder(spec.decoder)
        ),
        Kind::Extension => format!(
            "{}{}{}",
            js_header(spec),
            fill(&template("extension_kit.ts"), &common_facts(spec)),
            decoder(spec.decoder)
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
    // Everything under `assets/` that reports is generated; the TUI plugin's
    // one-line V2 entry point re-exports the generated reporter.
    let generated: Vec<&str> = SPECS.iter().map(|spec| spec.asset).collect();
    let mut files = Vec::new();
    super::tests::collect_asset_files(&assets_dir(), &mut files);
    for file in files {
        let relative = file
            .strip_prefix(assets_dir())
            .expect("an asset lies under the assets directory")
            .to_string_lossy()
            .into_owned();
        if relative.ends_with(".test.ts") || relative == "opencode/tui.js" {
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
        METHOD_STATE,
        EnvVar::SheprBuildProfile.name(),
        EnvVar::SheprSocketPath.name(),
        EnvVar::SheprPaneId.name(),
        "createConnection",
        "settimeout",
        "AF_UNIX",
        "Math.random",
        "import random",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(IntegrationTarget::all().map(|target| format!("\"{}\"", target.source())))
    .collect();
    for spec in &SPECS {
        let name = format!("decoders/{}", spec.decoder);
        let text = decoder(spec.decoder);
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
            Kind::Shell(_) => "SOCKET_WAIT_SECONDS = 0.5\n",
            Kind::Plugin | Kind::Tui | Kind::Extension => "SOCKET_WAIT_MS = 500;\n",
        };
        assert!(
            text.contains(wait),
            "{name} does not wait 500 ms on the socket"
        );
        for method in [METHOD_SESSION, METHOD_STATE] {
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
