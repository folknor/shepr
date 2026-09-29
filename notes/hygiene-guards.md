# Hygiene: tests that prove nothing, guards and claims that stopped holding

This file consolidates the findings for questions 5 and 6 of the nine-scope
hygiene hunt over the shepr workspace: tests that depend on the environment they
run in rather than on anything this repository builds, tests that cannot fail,
checks that fail open when a name stops matching, and invariants asserted in
comments, documentation or commit messages that nothing in the build would
notice becoming false. It is a working document assembled from the raw hunter
reports; nothing here has been verified, and some entries may be phantoms. A fix
pass should expect that.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGG-067 - Claim: "Wire types must not use `skip_serializing_if`, `flatten`, `untagged` or tagged enums"

`AGENTS.md` and the `shepr-protocol/src/codec.rs` module doc. The hunter's
verdict: **true today for the types that actually cross the codec, not enforced
by anything, and the runtime backstop is data-dependent.**

- Nothing in `brokkr.toml`, `clippy.toml` or the workspace lint table mentions
  these attributes.
- The runtime backstop is incomplete and, for one of the four, conditional:
  `codec.rs`'s own test asserts `to_vec(&Skipping { value: None })` yields
  `CodecError::SkippedField` but `to_vec(&Skipping { value: Some(1) })` succeeds
  and returns `[1, 1]`. So a wire type carrying `skip_serializing_if` encodes
  fine for every value where the predicate is false and fails only in
  production, on the first message where the field happens to be absent - a
  guard keyed on data, not on shape.
- There is no test that `flatten`, `untagged` or an internally or adjacently
  tagged enum is rejected. `unsupported_shapes_are_rejected` covers a manual
  `serialize_seq(None)` and `IgnoredAny`, not the four named shapes. In practice
  `flatten` dies as `CodecError::UnknownLength` and `untagged` as
  `NotSelfDescribing` on decode, but only if a test happens to exercise that
  message.
- `shepr-config` contains four shapes the codec cannot encode, all `pub` or
  reachable from `pub` types: `BindingConfig` (`#[serde(untagged)]`,
  `keybinds.rs`); `TabBarRightEntryConfig` (`#[serde(tag = "type", ...)]`, an
  internally tagged enum, `tab_bar.rs`); `RawRule` behind `SidebarTokenRule`'s
  `#[serde(try_from, into)]` with ten `skip_serializing_if =
  "Option::is_none"` fields (`sidebar/rules.rs`); and
  `AgentSidebarToken`/`SpaceSidebarToken`, whose hand-written `Serialize` uses
  `serializer.serialize_map(None)` in `serialize_styled_token`, i.e.
  `CodecError::UnknownLength` (`sidebar.rs`). None reach the codec solely
  because `shepr-config/src/wire.rs` hand-mirrors each one.
- But `WireConfig` *does* reuse raw config types unmirrored: `ThemeConfig`,
  `SessionConfig`, `ServerConfig`, `AdvancedConfig`, `RemoteConfig`,
  `SidebarCollapsedModeConfig`, `PaneBordersConfig`, `TabBarPositionConfig`,
  `StatusIndicatorStyle`, `AgentPanelSortConfig`, `ConfigAgent`,
  `ImeCursorShape`. So the boundary is not "config types never cross the wire";
  it is "these twelve do and those four do not", with no marker, trait or naming
  rule distinguishing them. Adding a `skip_serializing_if` to `ServerConfig` for
  nicer TOML output would compile, pass every existing test, and break attach at
  runtime.

Enforcement named, both called cheap: (1) a gremlin-style text rule in
`brokkr.toml` forbidding `skip_serializing_if`, `flatten`, `untagged` and
`serde(tag` under `crates/shepr-protocol/src`, excluding `#[cfg(test)]` - the one
legitimate occurrence is `codec.rs`'s `Skipping` fixture, so either exempt that
file or move the fixture; (2) a marker trait (`trait WireSafe {}`) implemented
only by mirrored types, with `WireConfig`'s fields bounded on it, so reusing a
TOML-facing type in `WireConfig` stops compiling. Neither is possible today. See
HYGG-023 for the data half.

## HYGG-068 - Claim: "Config is read and validated once at launch. No reload, no fallbacks. Any config problem fails the launch"

`AGENTS.md`. Five scopes report this claim as **partly false today**, at
different sites. Each site is filed in full elsewhere; this entry is the index
for the claim.

- Remote config is validated on the client at attach time: HYGV-089. The recommendation there is to state
  the forced cross-host revalidation in the `AGENTS.md` sentence, which reads
  as absolute.
- XDG path variables get four empty/relative rules and `XDG_CONFIG_HOME` falls
  back silently: HYGV-008.
- `SHEPR_DEBUG_OSC_EVIDENCE` is read per pane and documented nowhere: HYGV-013.
- A manifest override that does not compile only warns: HYGC-023.
- Sidebar chrome preferences discover an unwritable state dir mid-session:
  HYGV-104.

The `shepr-protocol`/`shepr-config` hunter also records the contrasting
positive: everything else in that crate is genuinely front-loaded, including the
strftime compile (`parse_tab_bar_datetime_format`), the window-title template
parse and the keybind parse. The `shepr-server` hunter verified the same for its
scope and reports no finding there.

## HYGG-086 - `AGENTS.md` describes `reference/` and `docs/` as binding in-repo folders that do not exist

**False today**: neither directory exists in the tree; only `notes/` does.
Reported from `shepr-platform`, which adds why it matters there - several of the
durable claims the hunts want written down (the tunable inventory, the env-var
registry, the logging level policy) have nowhere to live that the document says
they must. `shepr-protocol`/`shepr-config` and `shepr-server` both note the same
absence in passing when saying that user-facing caps
(`MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS = 31_536_000`, `MAX_SESSION_NAME_LEN`,
the sidebar 16s, the roughly forty `shepr-server` tunables) are documented
nowhere a user would look. `shepr-mux` and `shepr-agent` likewise propose
`reference/`-level inventories that currently have no home.

## HYGG-087 - `AGENTS.md`'s crate list restates responsibilities nothing checks

"`shepr-platform`: Linux process, filesystem, IPC and terminal plumbing" and its
siblings. `brokkr.toml`'s `[[dependency_rule]]` entries already encode the
layering mechanically; the prose adds the responsibilities, which nothing
checks - and `logging.rs`'s domain catalogue is already outside its stated
responsibility (HYGG-085). `shepr-agent` reports the same shape: `AGENTS.md`
restates the agent state vocabulary and the crate layering, the latter enforced
by `brokkr.toml` and the former not.

## HYGG-091 - `AGENTS.md`'s "No god objects" principle names only `shepr-server/src/app/`, so it does not reach the crate's actual largest types

The `shepr-mux` hunter's reading: `PaneRuntime` is 2960 lines with about 90
public methods, roughly forty of them one-line delegations, and the principle as
written does not cover it; `terminal/metadata.rs` (1438 lines) and
`persist/restore.rs` (2365) are in the same position. If the principle is meant
generally, the enforcement should be a per-file or per-impl size rule in
`brokkr.toml`. Overlaps with HYGG-090's proposed file-length rule.

## HYGG-092 - Claim: `PaneState` is separate from `PaneRuntime`

`AGENTS.md`. The `shepr-mux` hunter's verdict: technically yes, vacuously.
`PaneState` is two fields (`attached_terminal_id`, `right_click_passthrough`);
everything that was pane state now lives in `TerminalState`, itself a 30-field
struct with 14 public mutable fields including two derived ones (`state`,
`revision`) whose invariants rest on callers remembering to call
`recompute_effective_state` and to bump the counter. The separation that matters -
pure data testable without PTYs - holds for `TerminalState` (it is tested
extensively and takes `now` as a parameter, which is the real evidence) and does
not hold for `PaneRuntime`, which mixes a PTY actor handle, a tokio abort handle,
four `Arc`-shared atomics, three mutexes, a `Cell` and forty pure-read
delegations in one type. The claim is "true about the name and misleading about
the shape."

## HYGG-126 - The `skip_after` test marker is anchored to column 0

Every `skip_after = '^#\[cfg\(test\)\]'` textlint (the persist clock rule, the
library print rule) releases only a top-level `#[cfg(test)]`. A test-only
helper inside a production `impl` is indented, so it is flagged rather than
skipped. That errs safe, but it pushed one fixer into rewriting test helpers to
dodge the rule, which changed what the tests exercised. Either allow leading
whitespace in the pattern and teach `scripts/check_skip_after_scopes.py` the
same, or state at the rules that test helpers belong in `mod tests`.
