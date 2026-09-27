# Hygiene hunt: crates/shepr-agent

Scope read: agent descriptors (`src/agent/`), resume (`src/agent/resume.rs`),
detection (`src/detect/`: manifest compiler, bundled manifests, overrides,
process-tree probe), integrations (`src/integration/`: registry, targets,
config editing, env contract, versions, hook assets under
`src/integration/assets/`), plus the receiving side of the hook contract
wherever it lives (`crates/shepr-api/src/schema/panes.rs`,
`crates/shepr-mux/src/pane/launch.rs`, `crates/shepr-mux/src/terminal/state/`,
`src/main.rs`, `src/cli/`).

I only read code. Nothing was built or run. Findings are grouped by question,
not ranked. Each says whether the consolidated version can be held
mechanically and by what.

Existing enforcement in this scope that already works, for calibration: the
gremlin exclusion for `detect/manifests`, the `shepr-agent-layer` dependency
allowlist in `brokkr.toml`, `agent/mod.rs::descriptors_are_the_domain_source_for_agent_views`
(pins the `#[repr(usize)]` index into `AGENTS`),
`manifest/tests.rs::all_bundled_manifests_parse_validate_and_compile`, and
`integration/tests.rs::bundled_integration_asset_versions_match_expected_versions`.
Several findings below are "extend one of those to cover the rest".

---

## Real defects tripped over along the way

1. **`shepr agent explain --file` silently ignores local manifest overrides.**
   `src/cli/agent.rs` calls `manifest::explain_for_label`, which reaches
   `manifest::registry()`. `registry()` initialises the process-wide
   `MANIFESTS` `OnceLock` with `override_dir = None` when nothing has called
   `reload_manifests(config_dir)` first. Only the server does that
   (`shepr-server/src/server/headless/bootstrap.rs`, `app/api.rs`,
   `api_dispatcher.rs`). So in the CLI process the bundled manifests are the
   only ones ever loaded, and the documented workflow ("capture the pane, edit
   the override, re-explain") explains against rules that are not the ones the
   server is using. The `source: bundled` field in the output is the only hint.
   Fix: make the override directory an argument of the explain entry point, or
   have the CLI resolve config paths and call `reload_manifests` first.
   Enforceable by signature: if `explain*` took an explicit
   `&ManifestRegistry` (or an override dir) rather than reaching a global,
   the bad spelling would be unrepresentable. A test that writes an override
   and calls the CLI-facing function would also catch it.

2. **First caller decides the override policy for the whole process.**
   Same root cause, stated as a hazard rather than a bug: `registry()` and
   `reload_manifests()` race for the same `OnceLock`. Whichever runs first
   fixes whether overrides exist for the process lifetime. In the server the
   ordering happens to be right today (bootstrap runs before any detection
   tick), and nothing in the build would notice if a future early call to
   `has_screen_manifest` or `detect_with_osc` moved ahead of bootstrap: the
   result is silently bundled-only detection, no warning. Enforceable by
   removing the global (pass the registry down), or, weakly, by a debug
   assertion that `detect_with_osc` is never the initialiser.

3. **Hermes's declared hook events are fiction.**
   `HERMES_HOOK_EVENTS` in `agent/mod.rs` declares one event, `SessionStart`,
   with action `Session`. The actual asset (`assets/hermes/__init__.py`)
   registers `on_session_start`, `on_session_reset` and `pre_llm_call`, and
   invents three start sources (`startup`, `new`, `resume`). Nothing consumes
   the Hermes row, so nothing notices. Same shape for **Grok**: its descriptor
   carries `integration_hook_events: &[]`, yet `targets.rs::grok_hook_config`
   writes a real `SessionStart` hook with action `session`. Any generic
   consumer of `IntegrationTarget::hook_events()` therefore sees Grok as
   hookless. Enforceable: a test asserting that a target with a
   config-registered hook has a non-empty event list, plus deriving the
   written config from the event list instead of hand-writing it.

4. **Grok reimplements `hook_command` with a different interpreter.**
   `command.rs::hook_command` produces `bash '<path>' <action>`;
   `targets.rs::grok_hook_command` produces `sh '<path>' session`. The grok
   asset is `#!/bin/sh`, so `sh` is probably intentional, but the choice is
   invisible at the one place that owns how shepr invokes hook scripts, and
   the action string `session` is spelled here rather than taken from
   `IntegrationHookAction::as_str`. Enforceable by giving `hook_command` an
   interpreter parameter (or reading it off the spec row) so every call site
   goes through one function.

5. **The three bun test files never run.**
   `assets/shepr-agent-state.test.ts`, `assets/opencode/shepr-agent-state.test.ts`
   and `assets/opencode/shepr-tui-session.test.ts` import `bun:test`. There is
   no `package.json`, no bun/vitest config, and `brokkr.toml` runs cargo only.
   They are read as coverage for the JavaScript and TypeScript hook assets
   (the Pi, OMP, opencode, Kilo integrations) and they provide none. They also
   write sockets into the system temp directory and mutate `process.env`
   globally. `notes/todo.md` has an open item ("Resolve typescript question"),
   so this is known, but the files sit beside code as if live. Either wire a
   bun step into `brokkr check` or delete them; a test that cannot run is
   worse than no test.

---

## 1. One value, one owner

6. **The hook environment contract is restated in about twenty assets.**
   `SHEPR_ENV` must equal `"1"`, `SHEPR_SOCKET_PATH` and `SHEPR_PANE_ID` must
   be non-empty, `SHEPR_BIN_PATH` falls back to the bare name `shepr`. The
   producing side is `shepr-mux/src/pane/launch.rs::apply_pane_launch_env`
   (`SHEPR_ENV_VAR`/`SHEPR_ENV_VALUE` locally defined, `SOCKET_PATH_ENV_VAR`
   imported from `shepr-config`, `SHEPR_PANE_ID_ENV_VAR` exported, and
   `"SHEPR_BIN_PATH"` as a bare literal). Every asset restates the whole
   resolution rule field by field, in four languages. This is the deployment
   constraint case: the scripts are shipped into other agents' configs and
   cannot link Rust constants, so copies are forced. What keeps them in step
   today: nothing. What could: a test that greps every asset in
   `INTEGRATION_SPECS` for each contract variable name (the names are the
   part that can drift silently; the semantics are not checkable from Rust),
   and a single Rust module owning the names so `SHEPR_BIN_PATH` stops being
   a literal in `launch.rs` and `shepr-server/src/app/tab_bar_status.rs`.

7. **`SHEPR_ENV` name and value are defined twice.** `src/main.rs:3-4`
   (`SHEPR_ENV_VAR`, `SHEPR_ENV_VALUE`, for the nested-launch refusal) and
   `shepr-mux/src/pane/launch.rs:1-2` (same two names, for setting it). The
   two sites are the writer and the reader of one variable and each owns its
   own spelling. Enforceable: one definition in `shepr-config` or
   `shepr-platform` beside `SOCKET_PATH_ENV_VAR`, plus a text rule forbidding
   the bare string outside that module.

8. **`SHEPR_AGENT` is spelled in three places with three shapes.**
   `agent/mod.rs::LAUNCH_ENV_TO_SCRUB = &["SHEPR_AGENT"]` (the scrub list),
   `detect/proc_tree.rs::parse_agent_env_hint` matches the byte literal
   `b"SHEPR_AGENT="` when reading `/proc/<pid>/environ`, and
   `shepr-mux/src/pane/runtime.rs:1902` passes `"SHEPR_AGENT"` when setting
   it. Three owners of one variable, one of them a byte-string with the `=`
   baked in. Enforceable: one `pub const`, with the probe deriving its prefix
   from it.

9. **Each agent's own config file name is spelled two to four times.**
   For every target the same file name appears in `check_config_targets`, in
   the install body, in the uninstall body, and again in
   `registry.rs::hook_registration_is_current`: `"settings.json"` at nine
   sites, `"hooks.json"` at eight, `"config.toml"` at five, `"config.yaml"`
   at four, `"config.json"` at three, `"cli.json"` at five (four of them in
   `opencode_config.rs`), `"tui.json"` at four. None of it is a constant, and
   `TUI_CONFIG_NAME` is the lone counterexample. Enforceable: put the config
   file name (and the ancestor depth, see finding 11) on the
   `IntegrationSpec` row and have install, uninstall and status read that
   row; a clippy `disallowed_script_idents`-style rule cannot express this,
   but the structural fix removes the sites entirely.

10. **The agent config directory registry is keyed by free-form strings.**
    `env.rs::AgentIntegrationPaths` holds a
    `HashMap<&'static str, CapturedDirectory>` populated from a literal list
    of twenty keys (`"pi_extension"`, `"claude"`, `"opencode_state"`, ...),
    read back by `paths.directory("claude")` at forty call sites in
    `targets.rs` and by `spec.directory` in `registry.rs`. A typo or a rename
    produces a runtime `NotFound` error at install time only, on the one
    target exercised. Enforceable by type: make the key an enum (or index the
    array by `IntegrationTarget` plus a small `DirectoryRole`), and the bad
    spelling stops compiling.

11. **Ancestor depths in `hook_registration_is_current` mirror the install
    paths by hand.** `json_in(2, "settings.json", ...)` for Claude because
    the hook lives at `<dir>/hooks/<name>`, `json_in(1, ...)` for Codex
    because it lives at `<dir>/<name>`, `ancestor(hook_path, 3)` for Hermes.
    Those numbers are `spec.path.len() + 1` and nothing says so. Change a
    spec path and status quietly reports Outdated forever (the hook is fine,
    the check is looking in the wrong directory) with no log line.
    Enforceable: derive the depth from `spec.path.len()`, or better, keep the
    config path on the spec row and stop walking upward from the hook path.

12. **One ten-second hook timeout, four spellings, two units.**
    `LETTA_HOOK_TIMEOUT_MS = 10_000`, `MASTRACODE_HOOK_TIMEOUT_MS = 10_000`,
    `ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC = 10`, and a bare `10` in
    `claude_settings.rs::canonical_hook_value` and `canonical_hook_input`, in
    `claude_settings.rs::install`'s `ensure_command_hook(..., 10, ...)`, in
    `config_edit.rs::kimi_hook_table` (`timeout = 10` inside a format
    string), and in `targets.rs::grok_hook_config`. The agents disagree about
    the unit, which is a real reason for separate values, but not for
    anonymous ones. Enforceable: one `HOOK_TIMEOUT: Duration` with per-agent
    unit conversion at the edit site, plus a `clippy` lint is not available
    here, so the mechanical part is a type (`Duration`) that cannot be
    written as a bare integer into JSON.

13. **The "shepr:<agent>" source string is hard-coded in every asset.**
    `assets/claude/...sh` has `source = "shepr:claude"`, kimi has
    `"shepr:kimi"`, hermes has `_SOURCE = "shepr:hermes"`, and so on, with
    the agent label duplicated next to it (`"agent": "claude"`). The owner is
    `AgentDescriptor::integration_source`. A typo in an asset makes the
    report arrive as `AgentSource::Custom`, which silently loses
    `full_lifecycle_hook_authority` and `session_identity_only_integration`
    in `shepr-mux/src/terminal/state/hooks.rs` with no log line at all.
    Today only two assertions exist anywhere
    (`tests.rs:2829` for qwen, `tests.rs:3023` for letta), both incidental.
    Enforceable and cheap: a test iterating `INTEGRATION_SPECS` asserting the
    asset text contains its target's `integration_source` and canonical
    label. This is the single highest-value mechanical check in the crate.

14. **The asset version parity test carries a hand-written list.**
    `bundled_integration_asset_versions_match_expected_versions` enumerates
    eighteen `(name, asset, version)` triples. It omits
    `OPENCODE_TUI_PLUGIN_ASSET`, `OPENCODE_V2_TUI_PLUGIN_ASSET` and
    `HERMES_PLUGIN_MANIFEST_ASSET` (whose `version: "1.0"` in
    `assets/hermes/plugin.yaml` is a fourth spelling of the Hermes version
    that nothing reads). A new target added without extending the list is
    silently uncovered. `registry::integration_asset(target)` already exists,
    so iterating `INTEGRATION_SPECS` would make the test exhaustive by
    construction.

15. **`assets/hermes/plugin.yaml`'s `name:` duplicates
    `HERMES_PLUGIN_INSTALL_NAME`.** The install directory is
    `<hermes>/plugins/shepr-agent-state` (Rust const) and the manifest inside
    it declares `name: shepr-agent-state` (YAML asset). Divergence means the
    plugin is installed under a directory Hermes will not associate with the
    manifest; status only checks that `plugin.yaml` exists, not what it says.
    Checkable with a test that parses (or greps) the asset.

16. **Claude's event and action are re-spelled six times.**
    `claude_settings.rs` hard-codes `"SessionStart"` in `HOOK_REMOVALS`, in
    `install`, in `canonical_hook_value`, in `canonical_hook_input`, and in
    the `installing && event == "SessionStart"` guard, plus action `"session"`
    four times, while `CLAUDE_HOOK_EVENTS` in `agent/mod.rs` already declares
    exactly that pair. Kimi, Copilot, Devin, Droid, Qodercli, Qwen, Cursor
    and Mastracode all drive their edits off `integration_hook_events`;
    Claude and Grok do not. Enforceable: make `claude_settings` take the
    event list, then the descriptor really is the domain source the module
    doc-comment claims it is.

17. **Shell-name lists have already diverged, three ways.**
    `proc_tree.rs::is_pane_shell_process_name` knows twelve shells
    (`sh bash dash zsh fish ksh mksh csh tcsh elvish xonsh nu`);
    `detect/mod.rs::is_generic_runtime_or_shell` knows four plus
    `tmux node bun`; `wrapped_agent_name_from_runtime_argv` matches four.
    Consequence today: a pane shell that is `dash`, `nu`, `ksh` or `xonsh`
    running an agent through `-c` is not unwrapped, so the agent is not
    identified, so there is no detection for that pane. That is a fact, not a
    prediction. Enforceable: one `ShellKind` table with per-use predicates
    (`is_pane_shell`, `supports_dash_c`) derived from it.

18. **`option_takes_value` and the eval-flag lists are per call site.**
    `script_arg_agent_name` takes `eval_flags`/`module_flags` as arguments
    and `letta_entrypoint_index` re-implements the same walk inline with its
    own copy of `["-e", "--eval", "-p", "--print"]` and its own `+= if
    option_takes_value(arg) { 2 } else { 1 }`. Two walkers over one argv
    grammar. Enforceable by having `letta_entrypoint_index` call the shared
    walker (it needs the index, so the walker should return it).

19. **`"--conversation"` is spelled three times.**
    `AGENTS[Antigravity].resume_args = FlagValue("--conversation")` and twice
    literally in `resume.rs::plan`'s `LettaConversation` arm. Enforceable
    only by restructuring `ResumeArgs::LettaConversation` to carry the flag,
    or by accepting it as a documented one-off.

20. **The session-start-source vocabulary is spelled three times and the
    copies disagree.** `resume.rs::AgentSessionStartSource::parse` accepts
    eight values (`startup resume clear compact branch new fork select`);
    `claude_settings.rs::SESSION_START_MATCHER` is
    `^(startup|resume|clear|compact|fork)$` (five); the kimi asset defaults
    to the literal `"startup"`; the hermes asset emits `startup`, `new`,
    `resume`. The comment above `SESSION_START_MATCHER` says Grok "uses
    new/load", and `load` is in none of the three lists, so a grok-imported
    Claude hook firing with `load` normalises to `None` and is treated as
    unrecognised (`session_start_source_is_recognized` in
    `shepr-mux/.../hooks.rs`). Enforceable: derive the matcher regex from the
    enum's variant strings, and give the enum a single `as_str` so the assets
    can be grepped against it.

21. **`AgentState`/hook action strings are re-validated by hand.**
    `IntegrationHookAction::as_str` owns `session working blocked idle`; the
    assets spell them in their `case "$action"` guards; `PaneAgentState` in
    `shepr-api` owns the wire spelling; and
    `shepr-server/src/app/api/panes.rs::normalize_state_labels` re-checks
    `matches!(status.as_str(), "idle" | "working" | "blocked")` against a
    fresh literal list. Enforceable: `normalize_state_labels` should parse
    into `PaneAgentState` and let serde be the validator.

22. **Two owners for "does this agent have a screen manifest".**
    `AgentDescriptor::screen_manifest` (the flag) and
    `manifest::has_screen_manifest(agent)` (whether the registry actually
    loaded one). They agree today because
    `all_bundled_manifests_parse_validate_and_compile` pins it, which is
    exactly the right kind of enforcement; worth noting only because
    `BUNDLED_MANIFESTS` is a third list, keyed by label string
    (`("agy", include_str!("manifests/antigravity.toml"))`), so a label
    rename breaks the join. The existing test catches it, so this is a
    non-finding with an answer: the test is the enforcement, keep it.

---

## 2. Values nobody can find, change, or trust

23. **There is no answer to "what are this crate's tunables".** The knobs are
    scattered by first need: detection limits (`MAX_RULES_PER_MANIFEST`,
    `MAX_GATE_DEPTH`, `MAX_TOTAL_GATES`, `MAX_MATCHERS_PER_GATE`,
    `MAX_REGIONS_PER_MANIFEST`, `MAX_TOTAL_MATCHERS`, `MAX_MATCHER_CHARS`) in
    `manifest.rs`; probe budgets (`CHILD_GROUPS_SCAN_LIMIT`,
    `FOREGROUND_TREE_SCAN_LIMIT`, `FOREGROUND_TASK_ENTRY_LIMIT`,
    `FOREGROUND_CHILD_BYTE_LIMIT`, `FOREGROUND_CHILD_PID_LIMIT`) in
    `proc_tree.rs`; version-probe budgets
    (`VERSION_PROBE_TIMEOUT`, `VERSION_PROBE_POLL_INTERVAL`,
    `MAX_VERSION_PROBE_OUTPUT`) in `version.rs`; session-ref caps
    (`MAX_SESSION_ID_LEN`, `MAX_SESSION_PATH_LEN`) in `resume.rs`;
    hook timeouts in `integration/mod.rs`; retry pacing
    (`SHEPR_OMP_IDLE_DEBOUNCE_MS`, `SHEPR_OMP_RETRY_GRACE_MS`) only inside
    the OMP TypeScript asset; the 12-line lookback in
    `contains_recent_non_whitespace` and the 32-char needle cap next to it;
    `128` temp-name attempts in both `file_ops.rs` and `config_file.rs`. Each
    is individually defined once, which is why none of them reads as a
    finding on its own. Not enforceable as a rule; the fix is a
    `reference/`-level inventory plus co-locating the detection and probe
    budgets in one `limits` module per subsystem.

24. **`SHEPR_PROCESS_DETECTION` is read at the moment of use, not validated
    at startup.** `proc_tree.rs::process_detection_mode` reads the variable
    behind a `OnceLock` on the first foreground probe, and an unrecognised
    value produces `tracing::warn!` and native mode. AGENTS.md says config is
    read and validated once at launch and that any problem fails the launch
    with no fallbacks. This variable is config in all but name and breaks
    both halves of that rule: a typo is discovered hours in, on whichever
    pane probed first, and is tolerated rather than refused. It is also
    invisible to `shepr config check`. Enforceable: move it into the config
    file (or validate it during launch and pass the mode down), and the
    `parse_process_detection_mode` function is already the right shape for it.

25. **`ChildGroups` detection mode has no injection point and is almost
    certainly a switch with one value.** The only way to exercise it is the
    process-wide `OnceLock`, so no test can flip it without leaking into
    other tests in the process. The two `*_with` seams
    (`child_groups_foreground_process_group_with`) exist precisely because
    the mode itself is untestable. The comment justifies the mode for
    "environments that do not expose terminal foreground groups", but shepr
    is Linux-only and `/proc/<pid>/stat` always exposes `tpgid`; the mode
    looks like a WSL-era leftover. See finding 47.

26. **Version-probe timing has no injection point.** `enforce_agent_version`
    hard-codes `VERSION_PROBE_TIMEOUT` (5s) at the call, while
    `run_version_probe` takes a timeout parameter, so the test can only
    exercise the inner function. `version_probe_deadline_includes_inherited_stdout`
    then sleeps 300ms plus 50ms of real wall clock to let a grandchild die.
    Enforceable: have `enforce_agent_version` take the timeout (or a small
    `ProbeBudget`), and the test stops needing the wall clock.

27. **`AgentIntegrationPaths::resolve()` captures the environment but the
    resolvers still read it directly.** `env.rs` documents the boundary
    ("install and status code receives this value and never consults the
    process environment"), and holds it for install/status. But every
    `*_dir()` function reads `std::env::var_os` itself and is `pub(crate)`,
    so `hermes_plugin_dir` calls `hermes_dir` (a second environment read
    behind `resolve`), and tests must manipulate real environment variables
    through `IsolatedEnv` to steer paths. Enforceable by signature: give the
    resolvers an explicit environment argument (a `&dyn Fn(&str) ->
    Option<OsString>` or a captured map) so the only environment read in the
    crate is at `resolve()`.

---

## 3. One channel, one implementation

28. **`print_outdated_update_notice` writes to stderr and re-formats operator
    text with a string hack.** `registry.rs:270-285` calls `eprintln!` from
    inside a library crate, then strips backticks out of
    `integration_update_instructions` with `.replace('`', "")` because that
    function was written for a different medium. So one message exists in two
    renderings, one of them produced by deleting characters from the other.
    Enforceable: return the message (or a structured value) and let the CLI
    own printing; a text rule forbidding `eprintln!`/`println!` in
    `crates/shepr-agent` would hold it, and `brokkr.toml` already shows this
    kind of scan is available.

29. **`actions.rs` assembles operator text ad hoc in eighteen match arms.**
    Every target hand-writes its own sentences ("installed claude
    integration hook to {}", "ensured claude settings at {}", "requires kimi
    code {KIMI_MIN_VERSION} or newer"), 700 lines of near-identical
    formatting, with the phrasing drifting between arms ("installed X
    integration to", "installed X integration hook to", "ensured X settings
    at", "ensured X config at"). Enforceable structurally: have each install
    return a `Vec<(InstalledArtifact, PathBuf)>` and format once.

30. **`INSTALL_WARNING_PREFIX` is a string prefix standing in for a level.**
    `version.rs` builds three warnings by prepending `"warning:"` to a
    formatted string, which the CLI then presumably prints verbatim. The
    project has `tracing` and a CLI output path; a prefix constant is a
    severity encoded in text. Enforceable: return a typed
    `InstallWarning` and let the printer decide the prefix.

31. **The one place that silently degrades detection logs nothing.** When a
    hook reports a source shepr does not recognise, `AgentSource::parse`
    yields `Custom`, and `hooks.rs::set_hook_authority_at` takes a different
    branch: no authority, no session identity, no log. The same for an
    `agent_label` that is not a canonical label
    (`PersistedAgentSession::from_report` returns `None`). These are exactly
    the failures an asset typo produces, and they are invisible. A
    `tracing::warn!` with pane id, source and label costs nothing on this
    path (it is once per session report, not per byte).

32. **Log lines that do carry identifiers, for contrast.**
    `installed_integration_statuses` logs `integration` and `error`;
    `process_detection_mode` logs `variable` and `value`;
    `config_file.rs::Drop` logs the temp path. Those are fine. The gap is
    coverage, not quality: nothing is logged when an override manifest is
    rejected (the warning is stored on the `LoadedManifest` and only surfaces
    through `explain`/`reload` summaries, so a bad override at server boot is
    only visible if someone asks), and nothing is logged when
    `hook_registration_is_current` returns false, which is the single most
    likely "why is my agent not reporting" question.

---

## 4. Errors

33. **Every hook asset swallows everything, by design, and reports nowhere.**
    `except Exception: pass` in the Python bodies, `|| true` on the
    heredocs, `2>/dev/null`, `client.recv` wrapped in a bare `try`. The
    reason is sound and documented in the claude asset (a traceback would be
    shown to the user by the agent). But the consequence is that a hook that
    cannot reach the socket, or that shepr rejects, is indistinguishable from
    no hook at all, forever. Mitigation worth considering: have the receiving
    side own the observability (log unrecognised or malformed reports, see
    finding 31) since the sending side structurally cannot.

34. **The antigravity asset's `emit_and_exit` path.** It prints a JSON
    document to stdout on every early return (missing `SHEPR_ENV`, missing
    socket, missing pane id), because Antigravity CLI expects a hook
    response. That means the "not running under shepr" case and the "running
    under shepr and reported" case produce the same visible artifact, and a
    genuinely broken install cannot be distinguished from a hook running
    outside shepr.

35. **Manifest override rejection travels only as a string.**
    `load_manifest_uncached` collapses three distinct failures (unreadable /
    unparseable, id mismatch, compile failure) into one `warning: String`
    attached to the fallback manifest. The subject is in the text, which is
    good, but the caller cannot act on the category, and `build_manifest_cache`
    has no way to refuse. Given AGENTS.md's "any config problem fails the
    launch", an override that does not compile arguably ought to fail
    `shepr config check` rather than warn at runtime. Enforceable: a typed
    `ManifestOverrideError` plus a `config check` path that loads overrides.

36. **`Agent::descriptor` indexes an array with `self as usize`.**
    `&AGENTS[self as usize]` in a `const fn`, relying on `#[repr(usize)]`
    and on declaration order matching the array. This is a panic on
    mis-ordering, not on caller input, and
    `descriptors_are_the_domain_source_for_agent_views` pins it, so it is
    enforced. Recorded only because the enforcement is a test rather than a
    type: a `match` or a build-time `const` assertion per variant would make
    the mis-ordering unrepresentable.

37. **Poisoned locks are unwrapped into inner values everywhere in
    `manifest.rs`** (`unwrap_or_else(PoisonError::into_inner)`,
    `Err(poisoned) => poisoned.into_inner()`, five sites). That is the right
    call for a cache, and it is consistent, but the choice is re-made at each
    site. A small `fn read_cache(&self)` / `write_cache(&self)` pair would
    make it one decision.

---

## 5. Tests that prove nothing

38. **`clear_integration_path_env` is a hand-maintained duplicate of the env
    var inventory.** `integration/tests.rs:98-118` lists fifteen variables to
    remove so paths resolve against the fake `HOME`. `env.rs` defines
    fourteen `*_ENV_VAR` constants plus the two XDG names. Add an agent env
    var and forget this list, and every install test for that agent silently
    inherits the developer's real value: the test passes on the author's
    machine, writes into the author's real agent config, and means nothing.
    This is the clearest example in the crate of a test that depends on the
    environment it runs in. Enforceable: expose
    `pub(crate) const INTEGRATION_PATH_ENV_VARS: &[&str]` in `env.rs`, have
    both `resolve()`-adjacent code and the test helper read it, and add a
    test asserting the list covers every variable the resolvers consult
    (checkable by construction if the resolvers take their variable name
    from the list).

39. **`failed_cli_registration_preserves_existing_config` re-executes the
    test binary through the host `bash` with `ulimit -f 0`.**
    `opencode_config.rs:324-370`. It depends on the host shell, on
    `trap '' XFSZ` semantics, on `std::env::current_exe`, and on the literal
    test path string `integration::opencode_config::tests::failed_cli_registration_preserves_existing_config`
    duplicating the function's own name. It does fail closed (the stdout
    assertion catches a filter that matches nothing), which is why it is a
    hygiene note rather than a defect. The env var it invents,
    `SHEPR_TEST_3970_CONFIG_DIR`, carries an issue number nobody can look up
    in this repository and is a test-only name in production-visible
    namespace.

40. **`resume.rs` tests build paths from `std::env::current_dir()`**
    (`absolute_test_path`). The test only needs "some absolute path", so this
    is an ambient dependency taken for convenience; a fixed absolute literal
    or a `ScratchDir` would be hermetic.

41. **`version_probe_deadline_includes_inherited_stdout` asserts against the
    wall clock** (`elapsed < 250ms` after a real 300ms sleep) and spawns
    `/bin/sh`. Under a loaded machine or a debug build this is a flake, and
    under a fast one it proves the deadline only coincidentally. See finding
    26 for the injection point that would remove the clock.

42. **`every_minimum_agent_version_parses` asserts over a single hard-coded
    target.** It takes `IntegrationTarget::Kimi`, calls
    `agent_version_requirement`, and checks that requirement parses. Since
    Kimi is the only target with a requirement, the test cannot fail for any
    other target and will not start covering a second one when it is added.
    `agent_version_requirement_only_set_for_kimi` next to it pins the
    "only Kimi" fact, so the pair is coherent, but the parse test should
    iterate `IntegrationTarget::all()`.

43. **`every_agent_has_a_canonical_interactive_executable` restates the
    table it checks.** A 24-entry literal list of `(Agent, executable)`
    copied from `AGENTS`. `assert_eq!(expected.len(), Agent::all().len())`
    forces it to be extended, so it is a real pin (a deliberate second copy
    that catches accidental edits), not a no-op. Keep it; noted because
    `identify_known_agents` and `parse_known_agent_labels` next to it are the
    same shape without the length assertion, so they can silently stop
    covering new agents.

44. **`test_support.rs::symlink_file` returns `true` unconditionally**
    after `expect("create symlink")`. A helper whose return value cannot be
    false; call sites presumably `assert!(symlink_file(...))`, which asserts
    nothing.

45. **`opencode_config.rs` and `manifest.rs` both keep `#[cfg(test)]`
    production wrappers** (`add_tui_plugin`, `add_cli_plugin`, `detect`,
    `detect_state`). Those are fine as test seams, but `resume.rs`'s
    `test_codex_plan` is gated on `any(test, feature = "test-support")`, and
    the root `Cargo.toml` dev-dependencies and `shepr-server`'s
    dev-dependencies both enable `shepr-agent/test-support`. Under cargo
    feature unification any workspace test or `clippy --all-targets` build
    compiles that `pub fn` into the library that production code links
    against. It only panics on its own bad input, so the risk is low, but it
    is a test-only shortcut production code can reach.

---

## 6. Guards and claims that have stopped holding

46. **`hook_registration_is_current` fails open for five targets and fails
    silently for the rest.** `Target::Pi | Omp | Kilo | Grok | Opencode =>
    true` is documented for Pi, Omp and Kilo (directory-loaded plugins) and
    for Grok and Opencode (checked by their own helpers earlier in
    `integration_status_at`). The coupling is invisible from either site: if
    the Grok special case above it were deleted, this `true` would report a
    broken Grok install as Current with nothing noticing. Enforceable: make
    the spec row carry a `RegistrationCheck` variant
    (`SelfRegistering | Json { file, root, depth } | Custom(fn)`) so the
    match is exhaustive over data rather than over a target list.

47. **`json_hook_commands_registered` is a substring search.** Install
    writes an exact shape (four different shapes across
    `ensure_command_hook`, `ensure_flat_command_hook`,
    `ensure_direct_command_hook`, `ensure_simple_command_hook`), and status
    verifies by walking the event's value recursively for a matching command
    string anywhere inside it (`json_contains_string`). So a command string
    sitting in a disabled block, in a comment-like field, or in an unrelated
    nested entry counts as registered. A check keyed on a name that no longer
    means what it meant. Enforceable: have status reuse the install shape
    (`is_matching_command_hook` already exists for one of the four) instead
    of a generic search.

48. **`is_pane_shell_process_name` fails open on a name it does not know.**
    Any shell outside the twelve-name list (or a wrapper like `nix-shell`,
    `toolbox`, a user's `$SHELL` symlink named something else) is treated as
    not-a-pane-shell, which changes `available_pane_shell` and the whole
    child-groups path, reporting nothing. Checkable only against the
    configured default shell: the config already knows the user's shell, so
    a launch-time check ("your configured shell is not one shepr recognises
    as a pane shell") is possible and would turn a silent no-op into a
    warning at boot.

49. **`parse_agent_env_hint` fails open on a renamed variable.** It matches
    `b"SHEPR_AGENT="` textually; if the setter in
    `shepr-mux/src/pane/runtime.rs` changed the name, the hint would simply
    never be found and the pane would fall back to process-name detection,
    with no log. Finding 8's shared constant makes it a compile error instead.

50. **Documentation that restates code-generated lists.** `manifest.rs`'s
    module doc enumerates the region names, the matcher keys, the gate keys
    and the limits ("at most eight levels total") in prose next to
    `RegionSpec::parse`, `ManifestRule`, `ManifestGate` and the `MAX_*`
    constants. AGENTS.md restates the agent state vocabulary and the crate
    layering (the latter is enforced by `brokkr.toml`, the former is not).
    `notes/todo.md` cites `src/integration/assets/...`,
    `src/detect/manifests/...`, `src/ghostty/rows.rs`, `src/protocol/wire.rs`
    and `src/client/shell/state.rs`, none of which exist since the crate
    split (they are now under `crates/`); `notes/` carries no truth
    guarantee, but the paths are stale enough to send a reader nowhere. The
    doc-comment case is enforceable only by a doc test that parses the prose,
    which is not worth it; the honest fix is to shorten the prose to the
    concepts and point at the enum.

51. **`ProcessDetectionMode::ChildGroups` is a claim nothing exercises.**
    No test sets `SHEPR_PROCESS_DETECTION=child-groups` end to end (the two
    `*_with` tests call the inner function directly), and on Linux
    `foreground_process_group_id` always succeeds when `/proc` is readable,
    so the mode's trigger condition (native detection returning `None`) is
    rare to nonexistent. If it is a WSL accommodation it should say so; if
    not, it is finding 55.

---

## 7. Policy invented per call site

52. **Two transports and two timeout budgets for one report.** The claude,
    codex, kimi, copilot, devin, droid, grok, cursor, antigravity, mastracode
    and opencode assets open the API socket directly with a 0.5s timeout and
    hand-build the JSON-RPC envelope (`{"id": ..., "method": ..., "params":
    ...}` plus a newline, then a best-effort `recv(4096)`). The hermes, qwen,
    qodercli and letta assets instead exec `shepr pane report-agent-session`
    with a 1s (hermes) or unspecified timeout. Two implementations of one
    protocol, in about fifteen copies, plus two request-id formats
    (`f"{source}:{ms}:{rand:06d}"` in claude, `f"shepr:kimi:{seq}"` in kimi)
    and two `seq` sources (`date +%s%N` in the shell prologue vs
    `time.time_ns()` in Python). The right consolidation is one code path:
    every asset shells out to `shepr pane report-*` (the CLI already exists,
    `SHEPR_BIN_PATH` is already exported, and it removes the hand-built
    envelope entirely), leaving socket framing owned solely by Rust. That is
    a real reduction, not a tidy-up: it deletes the JSON-RPC client from
    fifteen shipped scripts.

53. **Retry and debounce policy exists in exactly one asset.** Only the OMP
    TypeScript asset has `SHEPR_OMP_IDLE_DEBOUNCE_MS` (250) and
    `SHEPR_OMP_RETRY_GRACE_MS` (2500); every other asset fires once and
    gives up. Whether that asymmetry is deliberate is not recorded anywhere.

54. **Cleanup on the error path is implemented twice, nearly identically.**
    `file_ops.rs::write_managed_asset` and
    `config_file.rs::Replacement` both do "unique sibling temp name from
    pid plus an `AtomicU64`, up to 128 attempts, write, publish by rename,
    remove the temp on failure", with two separate counters
    (`NEXT_ASSET_TEMP`, `NEXT_TEMP`), two temp-name formats
    (`.{name}.shepr-{pid}-{seq}.tmp`, `.shepr-config-{pid}-{seq}.tmp`), and
    two failure cleanups (explicit in one, `Drop` in the other). The
    difference that matters (managed assets get fresh permissions, user
    configs preserve the original's) is real; the allocation loop is not.
    Enforceable: one `AtomicReplace` helper parameterised by the permission
    policy.

55. **A hand-rolled YAML editor for one agent.** `config_edit.rs` carries
    about twenty `yaml_*` helpers (indent parsing, inline comments, flow
    sequences, scalar quoting, list-item matching at indent) plus
    `hermes_yaml_layout_is_editable` and a "give up and tell the user to edit
    it by hand" error, all to toggle one key in Hermes's `config.yaml`. This
    is the largest per-call-site policy in the crate. The dependency
    allowlist for `shepr-agent` has no YAML crate; adding one (or shipping
    the Hermes enablement differently, for example a drop-in file if Hermes
    supports one) would delete several hundred lines and the class of bug
    that comes with hand-parsing an indentation-sensitive format.

56. **Ambient dependencies reached from logic.** `std::env::var_os` in
    fourteen `env.rs` resolvers (finding 27); `std::process::id()` in both
    temp-name generators; `Instant::now()` inside `run_version_probe`;
    `time.time_ns()` / `random.randrange` inside each asset;
    `std::fs::canonicalize("/proc/<pid>/cwd")` inside
    `resolved_agent_name_from_path_token`, reached from the pure-looking
    `normalized_process_name`. The last one is the notable one: a function
    named like a string transformation touches the filesystem, which is why
    its callers need the blocking-context doc comments that are repeated in
    three places.

57. **Unbounded growth upstream misbehaviour can cause.** The `/proc` walk is
    carefully bounded (five named budgets, documented, round-robin frontiers)
    and `MAX_VERSION_PROBE_OUTPUT` bounds the probe. The unbounded ones left:
    `explain_loaded_manifest` builds an `EvaluatedRule` per rule with a
    `region_preview` string per rule (bounded by the manifest limits, so
    fine), and `install_target` returns `Vec<String>` messages (bounded).
    Nothing alarming; recorded so the absence is on the record.

58. **Personal data in diagnostics.** `explain` includes
    `region_preview` (verbatim screen text from the user's pane) and
    `ManifestSource::Override(path)` (a home-directory path) in output that
    goes over the API and into CLI output. The hook assets deliberately keep
    payloads to ids. Nothing writes agent transcript text to logs, which is
    the right call; `agent_session_path` (a transcript path) does travel in
    reports and could reach logs through a future `tracing` call on that
    path.

59. **Shared mutable state whose safety rests on call order.** The manifest
    `OnceLock` pair (findings 1, 2) is the one instance:
    `MANIFESTS` plus `MANIFEST_INIT_LOCK`, with `reload_manifests` and
    `registry()` each doing their own double-checked init. `reload_lock`
    correctly serialises reloads, and the test comment
    ("tests build their own `ManifestRegistry` ... so they never touch this")
    shows the design is understood. Enforceable only by removing the global.

---

## 8. Code that is no longer load-bearing

60. **`AgentDescriptor` fields with one value.**
    `prompt_observation` is `true` for Codex alone, and
    `Agent::prompt_ready` hard-codes Codex's own prompt text
    (`"›AskCodextodoanything"`, `"model:loading"`, `"Resumingsession"`) plus a
    Codex-specific KMP matcher inside a generic method on `Agent`. So the
    flag is a switch with one value and the method is Codex logic wearing a
    generic signature. Either move it into the Codex manifest (it is screen
    text; the manifest engine already has `bottom_lines(N)` regions and gate
    semantics that express exactly this) or name it what it is. The manifest
    route is the right one: it deletes `contains_recent_non_whitespace`, the
    32-char needle array and the 12-line constant.

61. **`title_activity_glyphs` is non-empty for Claude alone**
    (`CLAUDE_ACTIVITY_GLYPHS`); every other agent has `""`. That is fine as
    data, but the field reads as a general mechanism and is one agent's
    detail.

62. **`session_ref_policy: Option<SessionRefPolicy>` encodes two facts as
    three states.** `None` means "no resume", and every agent with
    `resume_args: None` also has `session_ref_policy: None`; the two fields
    are never independently set. They should be one `Option<ResumeSupport>`
    carrying both, which would make `session_ref_from_report`'s
    `_ => None` arm unnecessary.

63. **`AgentState::Unknown` versus the "Idle" presentation.** AGENTS.md says
    "Unknown presents as Idle", and `attention_rank` gives them the same
    rank. Four states where three are presentable, with the distinction
    carried by convention in several crates. Not dead, but worth asking
    whether `Option<AgentState>` with three variants would say it better.

64. **`AgentSource::to_source_string` and `as_str` and `Display` are three
    ways to spell the same projection**, plus `PartialEq<&str>` for both
    `AgentSource` and `Agent`. `to_source_string` is `as_str().to_owned()`.
    Fine to keep, cheap to collapse.

65. **`integration/command.rs` is fifteen lines holding two functions, one of
    which (`shell_single_quote`) is imported separately by `targets.rs` to
    build the Grok command that bypasses the other.** Merging it into the
    module that owns hook command construction (see finding 4) removes a file.

66. **`types.rs` holds thirty-six structs of two shapes.**
    `<Agent>InstallPaths` (one to four `PathBuf` fields) and
    `<Agent>UninstallResult` (the same paths plus two or three `bool`s). They
    exist so `actions.rs` can format per-agent sentences (finding 29). Both
    families collapse into one `InstallOutcome { artifacts: Vec<(Role,
    PathBuf)> }` / `UninstallOutcome { removed: Vec<(Role, PathBuf)>,
    updated: Vec<(Role, PathBuf)> }`, which deletes `types.rs`, most of
    `actions.rs` and a third of `targets.rs`.

67. **`GROK_CONFIG_DIR` exists only as a test seam.** `env.rs` says so in a
    comment ("a shepr-level override only (primarily a test seam); the grok
    CLI does not honor it"). It is a production environment variable whose
    only purpose is testing, which is exactly the shortcut finding 27's
    injected-environment fix would remove.

68. **`ProcessDetectionMode::ChildGroups`** (findings 25, 51): if the WSL
    story is over, this mode, `CHILD_GROUPS_SCAN_LIMIT`,
    `child_groups_foreground_process_group*`, `PROCESS_DETECTION_ENV_VAR` and
    `parse_process_detection_mode` all go, along with the `running_inside_wsl`
    branch in `process_allows_remote_memory_read`. What tells me it may be
    dead: shepr is documented Linux-only, native `tpgid` reading has no
    documented failure mode on Linux, nothing in the repository sets the
    variable, and no test drives the mode through its public entry point.
    What tells me to be careful: `shepr_platform::running_inside_wsl()` still
    exists and is called from three places here, so somebody deliberately
    supported WSL at some point. Ask before deleting.

69. **`assets/hermes/plugin.yaml`'s `version: "1.0"`** is read by nothing on
    shepr's side (status parses `__init__.py`'s marker). It may matter to
    Hermes; if not, it is a third version number for one integration.

70. **`agent_name_from_known_package_path` hard-codes six npm package
    layouts** (`@earendil-works/pi-coding-agent`, `@oh-my-pi/...`,
    `@moonshot-ai/kimi-code`, `@qwen-code/qwen-code`, `mastracode`,
    `@letta-ai/letta-code`), two of them twice for a `dist/bundle` variant.
    These are upstream-version-specific paths: a package layout change makes
    the arm dead code that still reads as live, and nothing in the build can
    tell. This is the compatibility-path case: it should carry the version or
    date it was observed, and `notes/todo.md`'s "monitor upstream changes"
    item is the right home for re-checking it.

---

## Suggested consolidations, largest payoff first

- **Make `INTEGRATION_SPECS` the only table.** Put the config file name, the
  config path depth, the hooks root, the registration check strategy, the
  directory key (as an enum), the asset, the version, the events and the
  timeout on the row. That single change subsumes findings 9, 10, 11, 12, 14,
  16, 46, 47 and most of 66, and converts four hand-maintained parallel lists
  into one.
- **Delete the JSON-RPC client from the shipped assets** (finding 52): every
  asset invokes `shepr pane report-*`. One transport, one timeout, one seq
  policy, one envelope, and the assets shrink to "collect fields, exec".
- **Add the asset-contract test** (finding 13): iterate the spec table and
  assert each asset text contains its source string, its agent label and the
  contract variable names. Cheapest mechanical win in the crate.
- **Remove the manifest global** (findings 1, 2, 59): pass a registry or an
  override directory. Fixes a live defect and removes a first-caller-wins
  hazard.
- **Take the environment as an argument in `env.rs`** (findings 27, 38, 67):
  the resolvers stop reading the process environment, the test helper's
  hand-maintained clear-list disappears, and `GROK_CONFIG_DIR` stops being a
  production variable that exists for tests.
- **Move Codex prompt readiness into the Codex manifest** (finding 60).
- **Replace the hand-rolled YAML editor** (finding 55).
