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

## HYGG-010 - Tests that take their inputs from the developer's directory layout

**Decision (partial):** `snapshot_tests.rs`'s raw `std::env::var("HOME")` is
banned by piece 1 (the `shepr-core` environment registry, raw reads denied in
`clippy.toml`), and piece 2 (scratch under the project's `target/` tree) is where
its existing cwd comes from instead. The owner also adopted broadarrow's rule
that every child gets a stated working directory (a `clippy.toml` seal on
`std::process::Command::new`, tests spawning through one helper that sets a
scratch working directory), which settles that a test's directory comes from a
`ScratchDir`; but the seal covers spawned children, not a test reading
`std::env::current_dir()`, so it catches none of the sites below (HYGP-005).
Open: the `std::env::current_dir()` sites, the fixed `/tmp/...` missing-cwd
literal, and the `_env` binding name.

`shepr-agent/src/agent/resume.rs` tests build paths from
`std::env::current_dir()` (`absolute_test_path`). The test only needs "some
absolute path", so this is an ambient dependency taken for convenience; a fixed
absolute literal or a `ScratchDir` would be hermetic.

`shepr-mux`: `pane/runtime.rs`, eight sites in `persist/restore.rs`, and
`workspace.rs`'s `test_adversarial_identity_state` use
`std::env::current_dir()` as a test cwd, so the tests depend on where the runner
was invoked.

`shepr-server/src/app/snapshot_tests.rs` reads the real `$HOME` with no
`IsolatedEnv`:

```rust
cwd: PathBuf::from("/tmp/this-directory-does-not-exist-for-shepr-test"),
...
cwd: std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp")),
```

The test means "one pane with a missing cwd, one with an existing cwd" and gets
the second from the developer's `$HOME`; if `$HOME` is unset the fallback
silently changes the test's meaning. `ScratchDir` gives both cases
deterministically. The hunter notes this breaks two of the project's own rules
at once (tests touching the environment hold an `IsolatedEnv`; never read or
write from `/tmp`) - see HYGG-012.

`shepr-api`/CLI: `terminal_and_agent_attach_reject_invalid_config_before_connecting`
in `src/cli.rs` binds the env guard as `let _env = ...::IsolatedEnv::new();` and
then calls `_env.set(..)`. It works, but the underscore prefix conventionally
means "held only for Drop", so a reader deleting the apparently unused binding
breaks the test's isolation.

The `shepr-protocol`/`shepr-config` hunter reports the contrasting positive:
in that scope no environment-dependent tests were found, path-touching tests use
`IsolatedEnv` / `ScratchDir` as `AGENTS.md` requires, `AppPaths::default()`
deliberately points at `/nonexistent/shepr-test-config` so a slip fails loudly,
and `wire_tests::framing_over_unix_socketpair` uses an in-process socket pair
rather than the network.

Enforcement rule named: a brokkr text rule forbidding `"/tmp` literals and
`env::var("HOME")` in `crates/*/src` outside `shepr-test-support`.

## HYGG-012 - Fixed `/tmp` literals remain in test data, standing in for a scratch directory

**Decision (partial):** piece 2 of the test-isolation work adopted from
broadarrow (scratch directories move under the project's `target/` tree, named
by fixed-width digests budgeted against `sun_path`, with per-process slot locks
so a rerun reuses and clears its trees in place, a claim registry, and
`std::env::temp_dir` banned) closes the scratch root's own `/tmp` use and the
SIGKILL leak that came with it. Open: the fixed `/tmp` literals below, which are
test data rather than the scratch root itself.

Fixed `/tmp` paths inside test data elsewhere: `shepr-mux/src/persist/writer.rs`'s
`snapshot()` helper builds JSON containing
`"identity_cwd": "/tmp/shepr-writer-test"`, and
`src/workspace/aggregate.rs`'s tests use `"/tmp".into()` as a `TerminalState`
cwd. Nothing is written there, so the rule is not broken in effect - but these
are fixed shared paths standing in for a scratch directory, and if validation is
ever added at the snapshot boundary they become load-bearing on `/tmp` existing
and being a directory. `shepr-server/src/server/socket_paths.rs` also uses
`/tmp/...` path strings, only as env values never touched on disk - harmless,
though it defeats the same grep. `shepr-server/src/app/snapshot_tests.rs` has a
real one (HYGG-010).

Enforcement rules named: a gremlin-style text rule forbidding `temp_dir`
outside `shepr-test-support` plus a documented exemption; a text rule against
`/tmp` literals in `shepr-mux` and in `crates/*/src` generally.

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

## HYGG-116 - Claim: alacritty types never leak out of `shepr-vt`

The dependency rule enforces this at crate level, and the hunter found no
alacritty types in public signatures. Two soft spots recorded: the public
`impl From<Rgb> for RgbColor` and `From<RgbColor> for Rgb`; and `CellStyle`,
which is `pub` in a private module and reachable through `CellBasicData.style`
but not re-exported, so callers cannot name it.

## HYGG-128 - A permission test stays ignored where a per-thread capability drop would run it

`crates/shepr-remote/src/remote/local_server.rs`:
`is_server_listening_returns_permission_errors_instead_of_false` is ignored,
so it runs nowhere. `crates/shepr-mux/src/pane/runtime.rs` now makes a similar
test meaningful for every user by running the probe on a thread that drops
`CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH` from its own effective set. Move
that helper into `shepr-test-support` and use it here.

## HYGG-126 - The `skip_after` test marker is anchored to column 0

Every `skip_after = '^#\[cfg\(test\)\]'` textlint (the persist clock rule, the
library print rule) releases only a top-level `#[cfg(test)]`. A test-only
helper inside a production `impl` is indented, so it is flagged rather than
skipped. That errs safe, but it pushed one fixer into rewriting test helpers to
dodge the rule, which changed what the tests exercised. Either allow leading
whitespace in the pattern and teach `scripts/check_skip_after_scopes.py` the
same, or state at the rules that test helpers belong in `mod tests`.
