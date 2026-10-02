# Design hunt: `crates/shepr-agent`

Scope: agent identity (`agent/`), screen detection and the manifest rule
language (`detect/`, `detect/manifests/*.toml`), process identification
(`detect/proc_tree.rs`), and the integrations (`integration/`, including every
hook asset). Consumers in `shepr-mux`, `shepr-server`, `shepr-api`,
`shepr-client`, `shepr-config` and `shepr-core` were followed where they decide
something this crate should own. Tests were not reviewed except where a test is
the only thing holding two copies of a decision together.

## The short version

The crate already has the right nouns (`Agent`, `AgentDescriptor`,
`AgentSource`, `AgentSessionRef`, `IntegrationTarget`), but almost every one of
them is flattened back to a string or a bool at its boundary, and the
consumers re-derive the meaning from those strings again and again. Five moves
would remove most of the findings below:

1. **One typed report origin.** Replace the `(source: String, agent_label:
   String)` pair that travels from the API through `shepr-server` into
   `TerminalState` with `ReportOrigin = Official(Agent) | Custom { source,
   label }`. For an official reporter the agent alone determines both strings,
   so every `AgentSource::from_pair(source, label)` re-parse disappears
   (findings 1.1, 2.1, 4.1).
2. **One hook-authority class on the descriptor.** Replace the three bools
   `reserves_native_state`, `full_lifecycle_hook_authority` and
   `session_identity_only_integration` (plus the Codex "none of the above"
   case) with one enum, and make the descriptor's integration fields one
   `Option<IntegrationDescriptor>` (findings 1.2, 2.2, 2.3).
3. **A screen verdict, not four bools.** `AgentDetection` and every mirror of
   it in `shepr-mux` carry `state` plus `visible_idle`, `visible_blocker`,
   `visible_working` and `skip_state_update`, whose only legal combinations
   are "skip" or "one state, optionally visible". Model that, and compile the
   manifest rule into it (findings 1.3, 2.4).
4. **One declarative registration per integration target.** Install and status
   each decide what a target's hook registration looks like; make one
   function per target produce the expected registration value that install
   writes and status compares, as `grok_hook_config` and
   `antigravity_cli_hook_block` already do (findings 2.5, 3.4).
5. **Split the crate along its real seams.** Identity is a leaf every crate
   needs (client included); detection is a server-side engine; integration is
   a file-editing installer with heavy dependencies; `/proc` walking belongs
   in `shepr-platform` by the repo's own rule (section 3).

---

## 1. Axes that should be types

### 1.1 The hook report origin travels as two strings

`PaneReportAgentParams` / `PaneReportAgentSessionParams` (`shepr-api`) carry
`source: String` and `agent: String`. `App::parse_agent_report_identity`
(`shepr-server/src/app/api/panes/reports.rs`) parses an `AgentSource` and a
normalized label, then hands the label on as a `String`
(`normalize_reported_agent_label` in `api_helpers.rs` parses the label to an
`Agent` and immediately formats it back to `agent_label(agent).to_string()`).
`TerminalState::transition_report` (`shepr-mux/src/terminal/state/source/report.rs`)
receives the typed `AgentSource` and its first act is
`typed_source.to_source_string()`; from there `HookAuthority { source: String,
agent_label: String }`, `SuppressedFullLifecycleHookReport.agent_label`,
`StaleFullLifecycleHookSession.agent_label`, `hook_sources: HashMap<String, _>`
and roughly twenty `fn ...(source: &str, agent_label: &str)` helpers in
`terminal/state/source.rs` all re-parse the pair through
`AgentSource::from_pair`, `Agent::parse_canonical_label` or
`shepr_agent::detect::full_lifecycle_hook_authority(source, label)`.

The domain is closed: a report is either from a built-in integration, in which
case the `Agent` determines the source string *and* the label, or it is a
custom reporter with two free strings. Proposed type, owned by
`shepr-agent::agent`:

```rust
pub enum ReportOrigin {
    Official(Agent),                       // source and label implied
    Custom { source: String, label: CustomLabel },
}
```

Parsed once at the API boundary (where the "official source with another
agent's label" refusal already lives), stored in `HookAuthority`, keyed in
`hook_sources`, and queried through methods (`origin.agent()`,
`origin.authority_class()`), never through string comparison. Today a typo'd
or swapped `(agent_label, source)` argument pair compiles everywhere.

### 1.2 Integration capability as three bools plus correlated options

`AgentDescriptor` carries `integration_target: Option<IntegrationTarget>`,
`integration_source: Option<&str>`, `integration_hook_events: &[..]`,
`hook_session_policy`, `reserves_native_state: bool`,
`full_lifecycle_hook_authority: bool`, `session_identity_only_integration:
bool`. Their legal combinations:

| class | agents | flags |
|---|---|---|
| no integration | gemini, cline, kiro, amp, qodercli, qwen, letta, maki, muse | all false, all `None` |
| session-only, screen owns state ("reserved native") | claude, cursor, devin, copilot, droid, grok | `reserves_native_state` |
| session-only ("identity only") | agy | `session_identity_only_integration` |
| partial state hooks plus screen | codex | none set |
| full lifecycle | pi, omp, mastracode, opencode, kimi, kilo | `full_lifecycle_hook_authority` |

Nothing prevents two flags being true, a target without a source, or hook
events without a target; `descriptors_are_the_domain_source_for_agent_views`
checks some of the correlations pairwise. Codex is a real fourth class that the
flags express only by absence. Proposed:

```rust
pub struct AgentDescriptor {
    agent, label, aliases, executable,
    screen: Option<ScreenManifest>,          // see 2.6
    integration: Option<IntegrationDescriptor>,
    resume: Option<ResumeSupport>,
}
pub struct IntegrationDescriptor {
    target: IntegrationTarget, source: &'static str,
    authority: HookAuthorityClass,           // SessionOnly | PartialState | FullLifecycle
    hook_events: &'static [IntegrationHookEvent],
    session_policy: HookSessionPolicy,
}
```

The `.with_integration_hook_events(..)` builder exists only because the struct
literal already sets the field to `&[]`; it disappears with this shape.

### 1.3 The screen verdict is a state plus four bools

`AgentDetection { state, skip_state_update, visible_idle, visible_blocker,
visible_working }` admits 64 combinations; only these are meaningful: skip (and
then the state is irrelevant), or one of Idle/Working/Blocked with an optional
"visible" mark, or Unknown. The manifest validator enforces it at load
(`validate_manifest`), `rule_detection` re-masks it (`rule.visible_idle && state
== Idle`), and `shepr-mux` masks it a third time
(`decide_screen_detection_publish`) and then spreads it over
`DetectionPublishState`, `DetectionPublishDecision::Publish`,
`ScreenDetectionPublishInput.last_visible_*` and the pane runtime's
`AgentDetection` synthesized for a process exit. Proposed:

```rust
pub enum ScreenVerdict {
    Skip,                         // agent-owned viewer; keep prior state
    Unknown,
    Idle { visible: bool },
    Working { visible: bool },
    Blocked { visible: bool },
}
```

`detection_update_for_publish_with_osc` already turns `skip_state_update` into
`Option`; with `Skip` as a variant that filter is a match arm.

The manifest schema mirrors the same flaw (`state: Option<ManifestState>`,
three `visible_*` bools, `skip_state_update`). Every bundled rule sets `state`,
so `None` (which means Unknown, alongside `ManifestState::Unknown` which also
means Unknown) is an unused second spelling. The TOML could stay as is, but the
compiled rule should hold a `ScreenVerdict`, produced by validation, so that
`rule_detection` is a field read.

### 1.4 Manifest rule identity, region and fallback reason are strings

- `ManifestRule.region: String` is kept raw for `explain`, while matching uses
  `RegionSpec`. `RegionSpec::parse` trims, so `" whole_recent "` validates but
  explain echoes the untrimmed spelling. Hold the parsed `RegionSpec` and give
  it `Display`.
- `DetectionExplain.fallback_reason: Option<String>` is filled from three
  `pub const ..._FALLBACK: &str` values plus a literal `"unknown_agent"` in
  `explain_for_label`. A closed set: `enum FallbackReason { UnknownAgent,
  NoScreenManifest, ManifestUnknown, DefaultIdle }`.
- `skipped_update_reason: Option<String>` is always `format!("matched_rule:{id}")`,
  information already present in `matched_rule`. It is derived data, not a
  field.
- `DetectionExplain.agent: Option<String>` exists only so an unknown label can
  be echoed; everything else puts `agent_label(agent).to_string()` there.
- `hook_authority_explain_to_json_value(agent_label: &str, state, source: &str,
  skip_reason: &str)`: the skip reason is one of two literals chosen in
  `shepr-server/src/app/api/detect.rs` (`"full_lifecycle_hook_authority"` /
  `"hook_authority"`).

The explain payload as a whole should be a typed, `Serialize` struct defined
next to the API schema; see 3.6.

### 1.5 Process identification passes names, not identities

`identify_agent_in_job` returns `Option<(Agent, String)>`. Internally
`normalized_process_name` returns either the raw comm or a canonical label
string, `agent_name_from_basename` / `agent_name_from_known_package_path` /
`resolved_agent_name_from_path_token` all return `Option<String>` that is
always `agent_label(agent).to_string()`, and then `identify_agent` re-parses
the string. `process_priority` decides the candidate's rank by comparing that
string with `process.name` (changed means "normalized alias"). The `String`
that escapes is only logged (`detection_task.rs` debug line).

Proposed: identification returns `Identified { agent: Agent, via:
IdentifiedVia }` where `IdentifiedVia = Comm | Argv0 | WrappedScript { runtime:
Runtime } | PackagePath | ResolvedSymlink`, and `ProcessPriority` is a function
of `via`, not of a string diff. The log can print `via`.

The runtime classification is itself stringly: `"node" | "bun"` is matched in
`normalized_process_name`, `wrapped_agent_name_from_runtime_argv`,
`letta_entrypoint_index` and `is_generic_runtime_or_shell` (which adds
`"tmux"`), `is_python_runtime` parses `pythonX.Y`, and shells come from
`is_pane_shell_process_name`. One `enum Runtime { Node, Bun, Python, Shell,
Tmux }` with a single classifier removes four independent matches (see also 2.8).

### 1.6 Process ids, groups and states are bare integers and chars

`proc_tree.rs` passes `child_pid: u32` and `process_group_id: u32` side by
side (`foreground_process_group_members(child_pid, process_group_id)`,
`process_tree_pids([process_group_id, child_pid], ..)`), so the two are
swappable. `process_pgrp_comm_and_state` returns `(i32, String, char)`; the
kernel process state is compared as `'T'`, `'D' | 'Z' | 'X' | 'x'`. `0` is the
absence sentinel (`root > 0`, `child_pid > 0`, `process_cwd(0)` returns `None`,
`foreground_process_group_id` maps through `unwrap_or_default()` after already
checking `> 0`). Suggested: `Pid(NonZeroU32)`, `Pgid(NonZeroU32)`, `enum
ProcState { Running, Sleeping, DiskSleep, Stopped, Traced, Zombie, Dead, .. }`
with `allows_remote_memory_read()`. These belong in `shepr-platform` (3.3),
which `shepr-mux`'s `process_probe.rs` would then also use.

### 1.7 Build profile as a string

`install_present_integrations(paths, build_profile: &str)` compares against
`"release"`. A typed `shepr_config::BuildProfile` exists; the server calls
`BuildProfile::current().marker()` to flatten it to a string for this call
(`shepr-server/src/server/headless/bootstrap.rs`). `shepr-agent` sits below
`shepr-config`, so it cannot take the enum. Either move the release/dev
decision to the caller (the installer should not know about profiles at all;
the server decides whether to call it) or move `BuildProfile` down into
`shepr-core`. The first is better: "only release servers own agent configs" is
server policy.

### 1.8 Integration failures reach callers only as prose

Every install and status failure is `io::Error::other(format!(..))`:
"directory not found ... install X first", "must be a JSON object", "registers
the Shepr hook outside its managed block", "config has multiple hard links",
"changed while Shepr was preparing an update" (the last one is smuggled as
`ErrorKind::WouldBlock` so `install_target_inner` can branch on it and retry
once). A typed `InstallError { ConfigChanged, ConfigUnparseable { path },
ConfigShape { path, expected }, ManagedBlockConflict, HardLinked,
NotRegularFile, TooManySymlinks, AgentDirMissing, Io(io::Error) }` would let
the retry match on `ConfigChanged` instead of an overloaded `ErrorKind`, and
would let a future status surface say *why* a target is not current.
`IntegrationStatusKind::Outdated` likewise collapses "asset bytes differ" and
"asset current but registration missing or edited"; `Outdated { asset:
bool, registration: bool }` or two variants would keep that information.

### 1.9 Agent environment knowledge sits in `shepr-core`

`shepr_core::env::EnvVar` has `ClaudeConfigDir`, `CodexHome`, `KimiCodeHome`,
`CopilotHome`, `CursorConfigDir`, `AntigravityCliConfigDir`, `GrokHome`,
`PiCodingAgentDir`, `PiConfigDir` (config overrides) and `ClaudeCode`,
`ClaudeCodeChildSession`, `CodexThreadId`, `Ompcode`, ... (session markers
stripped from panes). Which agent owns which variable is a fact about the
agent; it is split from the descriptor and from `integration/env.rs`, which
maps variables to directories by hand. Keeping one typed env registry is a
deliberate repo policy, so the suggestion is narrower: let the descriptor (or
`IntegrationDescriptor`) name its `config_dir_override: Option<EnvVar>` and
`session_markers: &[EnvVar]`, so the pane-stripping list and the
integration-path list are derived from the agents rather than maintained
beside them.

### 1.10 `AgentResumePlan::argv: Vec<String>`

The argv is public, built from `ResumeArgs::{FlagValue, InlineFlag,
Subcommand}` and then flattened by `shepr_remote::interactive_shell_command`
into a shell line typed into the pane. The plan's invariant (non-empty
program, official source, accepted ref) is checked in `with_argv`, but the
field is `pub`, and `shepr-server/src/test_support.rs` overwrites `plan.argv`
directly. Make the fields private and offer `program()`, `args()` and
`to_shell_command()`; a test double that needs a different command should go
through a constructor seam, as the repo already does elsewhere.

---

## 2. Decisions made in more than one place

### 2.1 "Is this report from a built-in integration, and which agent?"

Answered by `AgentSource::from_pair` and friends at: `parse_report_session_ref`
(server), `TerminalState::warn_unrecognized_hook_identity`,
`persisted_agent_session_matches`, `conflicting_same_owner_session_ref`,
`session_report_allows_session_replacement`,
`is_unsequenced_opencode_selection`,
`foreground_agent_confirms_different_owner_takeover`,
`current_session_owner_conflicts` (which uses `Agent::parse_source` plus
`parse_canonical_label` instead, a different route to the same answer),
`hook_authority_conflicts_with_detected_agent`,
`known_agent_label_conflicts_with_detected_agent`,
`foreground_agent_confirms_session_owner` (uses `PersistedAgentSession::from_report`),
and the three `shepr-agent` wrappers `full_lifecycle_hook_authority`,
`session_identity_only_integration`, `is_reserved_native_state_source`,
`is_official_agent_source`. They agree today because they all funnel into
`from_pair`, but `current_session_owner_conflicts` already uses another path
(source parsed without the label, then compared). Single owner: parse once into
`ReportOrigin` (1.1) at the API; nothing downstream asks again.

### 2.2 "What may a state report from this integration do?"

Three sites, two flags, and they disagree:

- `shepr-server/src/app/actions/events.rs`: if `is_reserved_native_state_source`
  (claude, cursor, devin, copilot, droid, grok), a `HookStateReported` is
  turned into `set_agent_session_ref_at`: the state is dropped, the session
  ref in the report is kept.
- `shepr-mux/.../source/report.rs` `transition_report`: if the descriptor says
  `session_identity_only_integration` (agy), the whole report is dropped,
  session ref included.
- `transition_report` again: `full_lifecycle_hook_authority` routes through the
  full-lifecycle machinery; `hook_session_policy.state_requires_current_session`
  (codex only) drops state for another session.

So "a session-only integration sent a state report" is handled two different
ways depending on which flag the agent got. Since none of the shipped
session-only assets ever sends `pane.report_agent`, the only sender is a
custom or forged reporter using the official source, and the two classes treat
it differently for no stated reason. Single owner: `HookAuthorityClass`
(1.2) with one method, e.g. `fn admit_state_report(self, ..) -> StateReportAdmission`,
called at one place.

### 2.3 "Does a state report need a session id?"

- Rust: `HookSessionPolicy::state_requires_current_session` is true only for
  Codex, and only rejects a ref that *differs* from the current one.
- Assets: Kimi and MastraCode hooks refuse to send any state report without a
  session id ("A state event without its session identity cannot safely claim
  pane state"); Codex refuses too; Pi, OMP, OpenCode, Kilo send state only when
  they have a ref (`sendState` returns early on `!currentSessionRef()`;
  `reportState` returns early on `!sessionID`).

The server accepts a session-less state report from `shepr:kimi` or
`shepr:mastracode` that the shipped asset would never send. Whether that is a
deliberate leniency is a decision only one side records. Owner: the
descriptor's integration policy, with the asset contract tests asserting the
asset obeys it.

### 2.4 "Is this visible flag meaningful for this state?"

`validate_manifest` (load time), `rule_detection` (per match), and
`decide_screen_detection_publish` in `shepr-mux` (per publish) each answer it.
The first is the real decision; the other two are re-checks only because the
type does not carry the answer. 1.3 makes the question unaskable.

### 2.5 "What does this target's hook registration look like?"

Install (`integration/targets.rs`, one hand-written `install_*` per target)
and status (`integration/registry.rs`, `RegistrationCheck` plus `JsonShape`
plus `hook_registration_is_current`) each decide independently which events
get an entry, which entries carry an action argument, the timeout unit, the
matcher, and the entry shape. They are held together by the pairwise test
`every_target_reads_current_right_after_install`. Concrete drift already
latent:

- Cursor: `install_cursor` hard-codes `"sessionStart"` and `Some("session")`;
  status derives the expected entries from `CURSOR_HOOK_EVENTS`. Change the
  descriptor's events and install keeps writing the old one.
- Devin: `install_devin` writes an entry for every event, including ones whose
  action is `None`; status (`JsonShape::Nested` via `hook_event_commands`)
  expects only events with an action. Devin has no action-less event today, so
  they agree by accident.
- Copilot is the reverse: install and status (`JsonShape::Direct`) both include
  action-less events, through two separately written rules.
- Codex and MastraCode installs `continue` on action-less events; Devin and
  Droid do not. Four installers, two policies.

Single owner: per target, one pure function `expected_registration(hook_path)
-> Registration` (a JSON value, a TOML block, or a file body), used by install
to write and by status to compare. `grok_hook_config` and
`antigravity_cli_hook_block` already work this way and need no pairwise test.
The JSON shapes then become a small enum with a single
`render(event, command, timeout)` and a single `matches(entry)`.

### 2.6 "Which agents have a screen manifest, and which file is it?"

`AgentDescriptor.screen_manifest: bool` and the `BUNDLED_MANIFESTS` table in
`detect/manifest.rs` (keyed by label string, with file names that differ from
the key: `antigravity.toml` under `"agy"`, `github-copilot.toml` under
`"copilot"`) answer it twice; the manifest's own `id` field answers it a third
time (`parse_bundled_manifest` checks `id == key`).
`all_bundled_manifests_parse_validate_and_compile` holds them together. At
runtime a descriptor with `screen_manifest: true` and no table entry silently
degrades to Unknown with no log (`bundled_manifest` returns `None` from the
`find` without the error branch). Single owner: the descriptor holds
`screen: Option<&'static str>` with the `include_str!`, the `id` field leaves
the TOML (or is checked against `Agent::label` at compile time via a build
step), and the table goes away.

### 2.7 "Which glyph at the start of a title is agent activity?"

`AgentDescriptor.title_activity_glyphs` (only Claude's is non-empty) plus the
braille range in `Agent::has_title_activity_glyph`; the Claude manifest's
`osc_title_working` / `osc_title_idle` regexes; the Codex manifest's
`osc_title_working` braille subset; and `shepr-mux/src/terminal/title.rs`,
which ignores which agent runs and asks whether *any* agent recognises the
glyph. Since the only consumer unions all agents, the per-agent field is a
global set wearing a per-agent costume, and the manifests keep their own
copies of the same glyph vocabulary. Owner: one `TitleActivityGlyphs` set in
detection (global, honestly named), referenced by the manifest regions through
a named class if the rule language grows one.

### 2.8 "Is this process an interactive agent?" (Letta filter)

`agent != Agent::Letta || is_interactive_letta_process(process)` is written
three times: the leader branch and the candidate loop in
`identify_agent_in_job`, and `suspended_agent_processes`. One
`identify_process(&ForegroundProcess) -> Option<Identified>` that applies the
filter once would own it.

### 2.9 "Which rule wins?"

`detect_with_manifest` walks `priority_order` (a stable sort, descending) and
stops at the first match; `explain_loaded_manifest` walks manifest order and
keeps `previous.priority >= rule.priority`. The comment in `loaded_manifest`
says they agree; nothing checks it except tests that happen to cover ties.
Explain can evaluate every rule and then pick the winner by walking the same
`priority_order` over the matched set, so the tie rule is written once.
Similarly the fallback state is computed by `fallback_state` and again inside
`fallback_explain`.

### 2.10 "Which JSON fields carry a hook command?"

`["command", "bash"]` appears in `value_uses_hook_path` (`config_edit.rs`),
`cst_value_uses_hook_path` (`claude_settings.rs`), `collect_hook_path_commands`
(`registry.rs`) and `is_matching_direct_command_entry`;
`direct_command_field()` is a function returning `"bash"`. One constant set,
one predicate.

### 2.11 "Does an absent config file read as empty?"

`registry::read_config_content` (directory at the path is an error),
`file_ops::read_if_file` (directory reads as absent),
`targets::read_json_config` (`is_file` then default), `config_file::read_config_snapshot`,
and the inline `fs::read_to_string` match in `opencode_config::plugin_is_configured`.
Install and status read the same files through different helpers with
different answers for a non-regular file; they agree today only because
install's preflight (`check_config_targets`) rejects non-regular files first.
One reader, one policy.

### 2.12 Claude session-start sources: matcher versus replacement policy

`CLAUDE_SESSION_START_SOURCES` (the hook matcher in `claude_settings.rs`)
admits `startup`, `resume`, `clear`, `compact`, `fork`; `HookSessionPolicy::CLAUDE`
treats only `clear`, `resume`, `compact` as replacements. `startup` being
reported but not a replacement may be deliberate (a fresh process is handled
by process detection). `fork` being reported but never a replacement looks
like the two lists drifted. Which is intended cannot be read from the code;
worth a decision, then one list with a per-source role.

### 2.13 Hook asset contracts duplicated across languages

Each of the 16 assets independently spells: the environment gate
(`SHEPR_BUILD_PROFILE = release`, `SHEPR_ENV = 1`, `SHEPR_SOCKET_PATH`,
`SHEPR_PANE_ID`), the method names `pane.report_agent` /
`pane.report_agent_session`, the param names, the action vocabulary
(`session|working|blocked|idle`, which is `IntegrationHookAction::as_str`), the
`<source>:<seq>` id, the 500 ms socket wait, and its source and label strings
(which are the descriptor's). Several also re-check the event-to-action map
the descriptor declares: the Codex asset's `expected_events` dict is
`CODEX_HOOK_EVENTS` in Python; the Devin asset hard-codes `("SessionStart",
"UserPromptSubmit")`; Claude and Cursor check their one event. These are held
by pairwise tests (`hook_assets_share_one_envelope`,
`bundled_integration_assets_report_the_descriptor_identity`, the bun contract
traces). The module comment argues against templating because the payload
decoders differ; that is right for the decoders, but the envelope, gate,
identity and event map are not agent-specific. A small generated preamble per
language (Python and JS) carrying those constants from the descriptor, with
the agent-specific decoder appended, would remove the copies rather than test
them. Kilo and the OpenCode server plugin also duplicate
`SESSION_STATE_BY_STATUS`, `CHILD_EVENT_STATES`, `sessionIDFromProperties`,
`stateFromSessionStatus` and the transport; Pi and OMP duplicate about 150
lines of transport, seq and queue code.

### 2.14 "Which source decided this pane's state?" (detect explain)

`handle_detect_explain` (`shepr-server/src/app/api/detect.rs`) reconstructs
after the fact whether hook authority decided the effective state:
`(!full_lifecycle || terminal.full_lifecycle_hook_authority_active()) &&
terminal.state == authority.state`. The arbitration in `TerminalState` made the
real decision and did not record it. If screen detection happens to land on
the same state as the hook, explain attributes it to the hook. Owner:
`recompute_effective_state` should record `EffectiveStateSource { Hook {
class }, Screen, ProcessExit, .. }` and explain should read it.

### 2.15 "Is this persisted session resumable?"

`PersistedAgentSession::new` (source agent matches, ref accepted; custom
sources allowed), `plan` (official only, ref accepted), and
`AgentResumePlan::with_argv` (official, ref accepted, non-empty argv) each
check, and `foreground_agent_confirms_session_owner` uses `plan(..).is_some()`
(building an argv) as a yes/no predicate. Meanwhile `PersistedAgentSession`
derives `Deserialize` with public fields, so the snapshot path bypasses `new`
entirely and `plan` is the only real gate. See 4.4 for the type that removes
the triple check.

---

## 3. Structure

### 3.1 The crate is three crates

`shepr-agent` combines:

- **Identity** (`agent/mod.rs`, `agent/resume.rs` types): tiny, pure, needed by
  `shepr-config` (`ConfigAgent`), the client sidebar (`parse_agent_label`,
  `AgentState::attention_rank`), the mux and the server.
- **Detection** (`detect/manifest.rs`, `detect/mod.rs`): a regex engine over
  screen text plus a `/proc` prober. Server-only.
- **Integration** (`integration/`): a config-file editor with `toml_edit`,
  `jsonc-parser`, flock locks, atomic replace and 16 bundled assets. Server-only,
  launch-time only.

Because they share a crate, the client binary's dependency graph includes the
integration editor and the detection engine, and `shepr-config` depends on all
of it to get one enum and one shell-name predicate (`validated.rs` calls
`shepr_agent::detect::is_pane_shell_process_name`). Suggested split:
`shepr-agent` (identity, descriptor, `ReportOrigin`, `AgentSessionRef`,
`AgentState`), `shepr-detect` (manifests, rule engine, process identification
on top of platform `/proc` readers), `shepr-integration` (installer and
assets). The descriptor stays the single table; detect and integration read
their slices of it.

### 3.2 Integration facts are spread over five tables

For one target: `AgentDescriptor` (target, source, hook events, policy), the
`IntegrationTarget` enum and its hand-written `agent()` match (the inverse of
`descriptor.integration_target`), `INTEGRATION_SPECS` in `registry.rs`
(assets, directory key, config files, registration check, path, timeout,
action label, install fn), `DirectoryKey` plus its resolver in `env.rs`, and
the `*_INSTALL_NAME` / `*_ASSET` / `*_NAME` constants in `integration/mod.rs`.
`spec_for` returns `io::Error` "missing integration spec" at runtime, and
`every_target_has_exactly_one_spec` plus
`descriptors_are_the_domain_source_for_agent_views` hold the tables together.
Within the spec, `config_files` is a positional slice read as `config(0)`,
`config(1)` with a runtime error if absent; `hook_timeout: Option<Duration>` is
`None` for targets whose registration shape needs none and is unwrapped with a
runtime error by those that do; `assets.first()` is "the primary asset" by
position; `action_label` equals `target.label()` except for Antigravity.

Suggested shape: `IntegrationTarget` gets an exhaustive `const fn spec(self)
-> &'static IntegrationSpec` (a `match`, so a missing row fails to compile),
the registration check variants carry their own file names and timeouts
(`Codex { hooks: &str, config: &str, timeout }`, `Json { file, root, shape:
Nested { timeout_s } | Flat { timeout_ms } | ... }`), and `primary_asset` is a
named field. `DirectoryKey` collapses into the spec (`directory: fn(&Env) ->
io::Result<PathBuf>` plus an explicit `agent_dir_is_parent: bool` or, better, a
`presence_dir` resolver), which also removes the `PiExtension | OmpExtension`
special case that `agent_present` and its test each spell. `AgentIntegrationPaths`
stores a `HashMap<DirectoryKey, Result<..>>` with a "was not resolved" error arm
that cannot happen; a struct with one field per directory (or a fixed array
indexed by the enum) cannot be missing a key.

### 3.3 `/proc` plumbing lives in the wrong crate

AGENTS.md: "libc, `/proc` and helper-program plumbing lives in the flat
`crates/shepr-platform/src/` crate". `detect/proc_tree.rs` is exactly that
(stat parsing, task and children walking, cmdline reading, cwd readlink,
budgets), and `shepr-mux` reaches through `shepr_agent::detect::` for
`foreground_process_group_id`, `process_cwd`, `foreground_job`,
`foreground_group_leader_job`. Move the reader and the bounded walk to
`shepr-platform` (with the typed `Pid`/`Pgid`/`ProcState` from 1.6), keep only
"which of these processes is an agent" in detection. The `FOREGROUND_*` limits
move with it. `is_pane_shell_process_name` and `SHELL_NAMES` are shell
knowledge, not agent knowledge; they belong with the platform shell helpers,
which also removes `shepr-config`'s reason to depend on detection.

### 3.4 Fourteen installers with one skeleton

Every `install_*` repeats: resolve directory, `check_config_targets`,
`is_dir` with a prose "install X first" error (already answered by
`agent_present` one call earlier), take a config lock, read with default,
edit in memory, create the hook dir, write the asset, write the config, build
an `InstallOutcome` of artifacts. The deliberate ordering ("edit config in
memory first, then write hook, then config") is documented once in a comment
and re-implemented 14 times; Kilo and Grok skip `check_config_targets`, Grok
writes its hook before preparing its config (it has no user config, so the
ordering is moot, but the reader has to verify that). A generic
`install(target)` driven by the spec, with the per-target part reduced to the
`expected_registration` value of 2.5 plus an edit strategy (`MergeJson`,
`ManagedTomlBlock`, `OwnedFile`, `PluginList`), would keep the ordering in one
place. Claude's source-preserving editor and OpenCode's two-config dance stay
as named strategies.

### 3.5 The manifest loader validates, then compiles, then compiles again

`parse_manifest` deserializes, `validate_manifest` walks the gate tree
(`validate_gate` and `validate_not_gate`, two near-copies), then calls
`compile_manifest`, which walks the tree again, re-parses every region name
(`RegionTable::intern` calls `RegionSpec::parse` after
`validate_region_name` did), and stores the result in
`AgentManifest.compiled: Option<CompiledManifest>` (`#[serde(skip)]`).
`loaded_manifest` then `take()`s it, with a `None => compile_manifest(..)`
arm that production never reaches. `manifest_gate_from_rule` clones every
matcher vector of every rule twice (once for validation, once for compile) to
present a rule as a gate. A single `compile(raw) -> Result<CompiledManifest,
ManifestError>` that validates while building (parse, do not validate) would
remove the `Option`, the second walk, the clones and the dead arm. The raw
`ManifestRule` then only needs to survive for `explain` evidence, which could
be captured into the compiled rule (`id`, `priority`, `region`, matcher
spellings) so `LoadedManifest` holds one representation instead of two
parallel vectors indexed by position.

Manifest failures are `String` errors logged once; a bundled manifest is
compile-time data, so a failure is a build bug that should fail
`brokkr check`, not degrade an agent to Unknown in production (there is a
test that compiles them, which is the right gate; the runtime branch could
then be an `expect`-free `unreachable`-by-construction if compile moves to a
build script or a `const` check).

### 3.6 The explain payload is built in the wrong crate, untyped

`explain_to_json_value` and `hook_authority_explain_to_json_value` hand-build
`serde_json::json!` objects in `shepr-agent`, which the server returns as
`ResponseResult::DetectExplain { explain: serde_json::Value }` and the CLI
prints; `explain --file` reads the same shape back. The API schema crate owns
every other response type. The explain payload should be a typed `Serialize`
struct in `shepr-api` (with `FallbackReason`, `RegionSpec` display, the
`state_source` enum of 2.14), built from `DetectionExplain` by the server.
`agent_state_label` (a third lowercase spelling of `AgentState`, next to its
derived PascalCase serde and the API's `PaneAgentState`) goes away with it.

### 3.7 Codex-specific region semantics inside the generic engine

`RegionSpec::{AfterLastPromptMarker, BeforeCurrentPromptMarker,
WholeRecentWithoutCurrentPromptMarker}` are defined by Codex's prompt glyph
(U+203A) and block markers (U+2022, U+25A0, U+2717, U+2713) in
`codex_prompt_line` / `codex_block_marker_line`, but are named as generic
regions. `PromptBoxBody` and `LastNonEmptyAbovePromptBox` encode Claude's
U+2500-bordered box. Either name them as what they are (`codex_*`, `claude_*`) or
lift the marker definitions into the manifest (a `[prompt]` table naming the marker
and block-marker glyphs) so the engine stays agent-neutral and a Codex UI
change is a manifest edit, which is the stated goal of manifests.

### 3.8 Agent state has five mirrors

`AgentState` (4 variants, derives serde), `PresentedAgentState` (3),
`shepr_protocol::AgentStatus` (3, re-exported as `shepr_api::schema::AgentStatus`),
`shepr_api::schema::PaneAgentState` (4), `ManifestState` (4), plus
`IntegrationHookAction` overlapping on three names and the JS
`type AgentState = "working" | "blocked" | "idle"`. Conversions:
`api_helpers.rs` maps `PresentedAgentState` to `AgentStatus` and
`PaneAgentState` to `AgentState`; the client maps `AgentStatus` back to
`AgentState` (`status_priority` in `shepr-client/.../presentation/status.rs`)
only to call `attention_rank`, and spells the strings again in `status_text`.
`PresentedAgentState` and `AgentStatus` are the same type; one should exist,
low enough for both (`shepr-core` or the split identity crate), carrying
`attention_rank` and its lowercase label.

### 3.9 Resume types duplicate each other

`PersistedAgentSession { source, agent, session_ref }`,
`AgentResumeKey { source, agent, session_ref }` (the same three fields),
`AgentResumePlan { source, agent, argv, dedupe_key }` (the same fields again
plus argv), and `shepr-mux`'s `PaneAgentSessionSnapshot { source, agent,
session_ref }` (a fourth copy with its own lenient deserializer), converted by
`session_ref_from_snapshot`, which is a clone into `new`. See 4.4.

### 3.10 `AgentSessionRefKind` lives in `shepr-core` for a consumer that left

`shepr-core/src/agent_session.rs` says the kind lives there "because both name
it: agents parse and resume sessions by it, and pane info reports it to
clients". No protocol or API type uses it any more; its only users are
`AgentSessionRef::kind()` and `== AgentSessionRefKind::Id` comparisons in
`shepr-mux`. The comment is stale and the type can move into `resume.rs` or be
replaced by `AgentSessionRef::is_id()`.

---

## 4. Types that resolve to primitives

### 4.1 `AgentSource`

Escape hatches: `as_str()`, `to_source_string()`, `From<String>`,
`From<&str>`, `PartialEq<&str>`, `Display`. Uses: `transition_report` turns it
into a `String` on entry; `current.source.as_str() == source` comparisons in
`source.rs` and `report.rs`; `AgentSource::parse` accepts anything (an unknown
string becomes `Custom`), so `"shepr:claud"` silently becomes a custom
reporter. The `Official` variant is public, so `AgentSource::Official(Agent::Gemini)`
is constructible; its `as_str()` is `""` (`unwrap_or_default()` sentinel), it
serializes as `""`, and deserializes back as `Custom("")`. Replace with
`ReportOrigin` (1.1): no `From<&str>`, no `PartialEq<&str>`, `Official` only
constructible for agents with an integration (e.g. `Official(IntegrationTarget)`).

### 4.2 `Agent` compared with strings

`impl PartialEq<&str> for Agent` compares labels; `Agent::label()` is
routinely formatted into `String`s that are re-parsed
(`agent_label(agent).to_string()` throughout `detect/mod.rs`,
`normalize_reported_agent_label`, `DetectionExplain.agent`).
`agent_label` and `parse_canonical_agent_label` in `detect` are one-line
re-exports of `Agent` methods, giving two names for each operation. Drop the
`PartialEq<&str>`, the re-exports, and the round trips.

### 4.3 `SessionId` / `AbsoluteSessionPath` and `AgentSessionRef::value()`

`AgentSessionRef::value()` clones to `String` and `value_str()` borrows; the
consumers use them to build argv (`reference.value()` in `plan`) and to
compare. `kind()` returns the core enum only to be compared with `Id`. Offer
`as_resume_argument(&self) -> &str` (or have the plan build itself) and
`is_id()`. `AbsoluteSessionPath` wraps a `String` although it is a path;
`PathBuf` (validated absolute, no control chars) would say so.

### 4.4 `PersistedAgentSession`

Public fields plus derived `Deserialize` bypass `new`'s invariant; the source
field can be `Custom` although no path stores a custom session (the API refuses
custom refs, `plan` refuses custom sources). The type should be
`ResumableSession { target: IntegrationTarget (or Agent with resume support),
session_ref }` with a validating `Deserialize`, from which source, key and
plan are derived. Then `AgentResumeKey` is `ResumableSession` itself (it
already derives `Hash` + `Eq`), `AgentResumePlan` is a view (`session.plan()`),
and `PaneAgentSessionSnapshot` is `ResumableSession` with the existing lenient
"drop a bad saved session" wrapper.

### 4.5 `IntegrationHookEvent`

`event: &'static str` and `matcher: Option<&'static str>` are agent event names
and regex matchers, compared as strings by the Claude editor
(`event.event == "SessionStart"`) and the asset scripts. Fine as static data,
but `claude_hook_event` enforcing "exactly one SessionStart event" at runtime
means the descriptor permits a Claude event list the editor cannot install.
A Claude-specific typed field (`ClaudeHooks { session_start: Action }`) or a
const assertion would make it unrepresentable.

### 4.6 Sentinels and stringly enums

- `title_activity_glyphs: &'static str` with `""` meaning "none" (all agents
  but Claude); a `&'static [char]` or the global set of 2.7.
- `ManifestRule.state: Option<ManifestState>` where `None` and `Unknown` mean
  the same thing.
- `ForegroundProcess.cmdline: Option<String>` is always `argv.join(" ")` when
  argv exists and `None` otherwise (`proc_tree.rs` constructs it that way in all
  three places), so `cmdline_argv0_agent_name` and the `cmdline` fallback in
  `is_interactive_letta_process` re-split a string that was joined from the
  vector they could have used; in production the fallback can never fire with
  a value. Drop the field.
- `IntegrationStatus.installed_version: Option<u32>` from a
  `SHEPR_INTEGRATION_VERSION=` marker that is "diagnostic only"; the marker is
  bumped by hand in 16 assets and nothing compares it. Either drop it or
  derive it from the asset hash.
- `logging::integration_action(action, target, outcome: &'static str)` with
  `"ok"` / `"error"`.
- `AgentSessionStartSource::parse` trims and matches strings; the asset side
  invents `"startup"` defaults (Kimi, MastraCode, Kilo, OpenCode's chat hook,
  OMP) and `"select"` (OpenCode TUI). An unknown value is silently `None`,
  which changes replacement semantics (`allows_replacement(None)` falls back to
  `replace_without_start`). A typed `Option<Result<Source, Unrecognized>>` at
  the API would let the server log an asset sending a value it does not know.

---

## 5. Lateral findings

- **Possible hang in the OpenCode server plugin.** `assets/opencode/shepr-agent-state.js`
  resolves a child's root with `while (childSessions.has(rootSessionID))
  rootSessionID = childSessions.get(rootSessionID);` and no cycle guard. A
  cyclic or self-parented `info.parentID` (a buggy or replayed event) spins the
  agent's event loop forever, inside the user's opencode process. The Kilo copy
  of the same code has a `seen` set (`rootSessionOf`); the OpenCode copy was not
  updated. This is the drift 2.13 predicts.
- **OpenCode `reportState` mutates `reportedRootSessionID`** as a side effect,
  so any state report for a session (including child-attributed ones mapped to
  the root) changes which `session.updated` events are re-reported. Probably
  intended, but it couples two unrelated decisions in one assignment.
- **Session paths sent and ignored.** The Claude and Antigravity hooks send
  `agent_session_path`; both agents have `SessionRefPolicy::Id`, so
  `session_ref_for_agent_report` drops the path. Harmless, but it is dead wire
  data and misleads a reader into thinking those agents resume by path.
- **Dead branch in `fallback_explain`.** Both callers pass `Some(agent)`; the
  `None` arms (state `Unknown`, `fallback_reason: None`) cannot run.
- **`RegionSpec::extract`** has a two-stage match whose second stage carries an
  unreachable arm returning `""` for the four variants handled in the first.
  One match would do.
- **`is_cased` and `is_case_ignorable`** in `manifest.rs` are the same function
  with different names.
- **`every_line_regex_matches`** keeps a fallback for more than
  `MAX_MATCHERS_PER_GATE` regexes, which validation already forbids. A
  deliberate defensive re-check, but it is a second code path that is never
  exercised.
- **`suspended_processes`** checks `state != 'T'` only; a process stopped
  under a tracer reports `'t'` and is not treated as suspended. Probably fine,
  worth a comment if deliberate.
- **`IntegrationHookAction::Session` for Devin's `UserPromptSubmit`** relies on
  the Devin asset re-checking the event name; the descriptor says "session
  action", the asset says "only these two events". Same contract, two places
  (2.13).
- **The config lock directory** (`integration/env.rs`
  `resolve_config_update_lock_dir`) recomputes XDG state home and hard-codes
  `"shepr"`, independently of `shepr-config/src/io.rs`, which owns XDG
  resolution and `SHARED_APP_DIR_NAME`. Likewise `devin_dir`, `opencode_dir`,
  `kilo_dir`, `opencode_state_dir` each re-decide "XDG var or `~/.config` /
  `~/.local/state`". A shared `xdg_config_home()` / `xdg_state_home()` in
  `shepr-core` or `shepr-platform` would own it.
- **`DirectoryError` exists because `io::Error` is not `Clone`**: errors are
  captured as `(kind, message)` and rebuilt on every lookup. With a typed
  `InstallError` (1.8) the captured error can be the typed one and cloned.
- **`install_present_integrations` retries on `WouldBlock`** to detect a
  concurrent agent edit; `config_changed_error` is the only producer, but any
  other `WouldBlock` from the filesystem would also trigger the retry.
- **The doc comment on `AgentDetection.visible_working`** says the
  flag is not forwarded in `StateChanged`; `DetectionPublishDecision::Publish`
  in `shepr-mux` does carry `visible_working`. Check whether the comment is
  stale.
- **Python as a hard prerequisite.** Every shell asset exits silently when
  `python3` is missing, so on such a host every integration installs as Current
  and never reports. A status signal ("hook interpreter missing") would make
  that visible; today it is indistinguishable from an idle agent.

---

## Suggested target shape (for the rewrite)

```
shepr-agent        Agent, AgentDescriptor (single table), IntegrationTarget,
                   HookAuthorityClass, HookSessionPolicy, ReportOrigin,
                   ResumableSession / AgentSessionRef, AgentState + AgentStatus
                   (one presented enum), start-source enum. No IO, no regex.
shepr-platform     /proc reader and bounded walk (Pid, Pgid, ProcState),
                   shell names, XDG base dirs.
shepr-detect       Manifest compiler (one pass, typed errors), rule engine
                   producing ScreenVerdict, process identification producing
                   Identified { agent, via }, title glyph set.
shepr-integration  Spec per target via exhaustive match, expected_registration
                   per target shared by install and status, generic install
                   driver with edit strategies, typed InstallError, assets
                   with a generated common preamble.
shepr-api          Typed DetectExplain payload.
```

The highest-value single change is `ReportOrigin` plus `HookAuthorityClass`:
it touches the most code (`shepr-mux` terminal state is built on the string
pair) and closes the one decision that already disagrees (2.2).
