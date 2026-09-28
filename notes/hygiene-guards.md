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

## HYGG-004 - `failed_cli_registration_preserves_existing_config` re-executes the test binary through the host `bash`

**Decision:** the host `bash` re-exec is resolved: the test now re-executes
itself through `shepr_test_support::fixture::command` (`Ignore`, `LimitFileSize`
and `Exec` steps), so it no longer depends on the host shell or on `trap ''
XFSZ` semantics. Open: `SHEPR_TEST_3970_CONFIG_DIR` is still a raw
`std::env::var_os` read under a scoped `#[expect]`, carries an issue number
nobody can look up in this repository, and is a test-only name in a
production-visible namespace; neither a registry entry nor another naming
scheme was chosen for it.

## HYGG-005 - Tests that assert on the wall clock

**Decision (partial):** the clock seam is adopted from broadarrow, incrementally
as part of the hygiene work: time is passed in rather than read inside logic,
held per subsystem by scoped textlints in the shape of broadarrow's
`control-loop-reads-the-clock-seam` (full finding: HYGP-001 in
`notes/hygiene-policy.md`). That is the injection point these tests lack, and it
answers the enforcement named below with text rules rather than a workspace
`disallowed_methods` entry. The `legacy_bridge_has_no_idle_deadline` sleep is
gone along with `--idle-timeout-v1` (formerly HYGP-039, now resolved and
removed). Open: each remaining test, as its subsystem gets the seam.

Reported from every scope that has timing at all. None can be made
deterministic while the timeouts are `Instant`-based constants with no injection
point.

`shepr-platform`: `ipc.rs::deadline_reader_cuts_off_a_trickling_peer` (300 ms
deadline, 50 ms trickle, 2 s bound, real thread sleeps); `tests.rs` 200 ms
deadlines with 5 s bounds; the wl-copy owner test polling a marker file for 2 s;
`remote_bridge_tests.rs` (300 ms `TIMEOUT`, 60 ms sleeps, 3 s waits, and
`legacy_bridge_has_no_idle_deadline` proving a negative by sleeping
`TIMEOUT * 2`); `shutdown.rs` (5 s timeouts, 5 ms polls).

`shepr-vt`: `synchronized_output_buffers_until_end_or_timeout` sleeps through
vte's 150 ms timeout, and asserts `!flush_expired...` right after a write, which
the hunter expects to flake under load. `shepr-mux`'s `runtime.rs` teardown
tests assert `< 500ms`, `>= delay/2`, `< 200ms`.

`shepr-agent`: `version_probe_deadline_includes_inherited_stdout` asserts
`elapsed < 250ms` after a real 300 ms sleep - a flake on a loaded machine or a
debug build, and on a fast one it proves the deadline only coincidentally.

`shepr-remote`: `ssh_agent::registration_retries_when_the_api_is_initially_missing`
sleeps in 10 ms increments against a 5 second wall-clock deadline
(`assert!(Instant::now() < deadline, "registration did not retry")`);
`wait_for_server_socket_succeeds_after_delay` sleeps 50 ms in a spawned thread
and allows 2 s.

`shepr-server`: `app/mod.rs` sleeps 30 ms, `tab_bar_status.rs` sleeps 1100 ms
and 400 ms, `client_transport.rs` sleeps 5 ms in loops - the hunter ties all of
them to there being no injection point for `Instant::now()` at roughly 25
production sites in a loop that already has a `now`.

`shepr-client`: `shell/input/input.rs` has one clipboard test that sleeps 400 ms
inside a fake clipboard reader and asserts `started.elapsed() < 300ms` (a coin
flip on a loaded machine, and the 400 ms is paid on every run of the suite), and
another that sleeps 10 ms. The bounded-read helper already takes a `Duration`
and a closure, so only the clock it measures against is missing.

Enforcement rules named: a `Clock` trait or a plain `fn now() -> Instant` passed
in, living in `shepr-core`, plus a `clippy.toml disallowed_methods` entry for
`std::time::Instant::now` / `SystemTime::now` outside designated modules. The
`shepr-platform` hunter calls this the root cause of most of its question-5
section and notes that `SshAgentLease::refresh_at(now)`, `Activity`,
`DeadlineReader`, `wait_child_until` and `wait_for_process_exits` already take
deadlines and could take a clock. `shepr-client`'s `endpoint/health.rs` and
`shepr-mux`'s `terminal/state/**` and `pane/process_probe.rs` are named as the
in-repo models that already thread `now` and need no sleeps.

## HYGG-006 - A persist test fabricates a file mtime a day in the future to walk past a gate

Residue. The snapshot-preservation logic in `shepr-mux/src/persist/writer.rs`
takes `now` and the mtime fabrication is gone. Open:

- The public `save` and `clear` entry points still read `SystemTime::now()`
  (each with a boundary comment); moving the read out means
  `shepr-server/src/app/session.rs` passes `now` in.
- `persist/restore.rs` has a runtime clock read of its own.
- The textlint holding it is not wired. Proposed by the fixer:

```toml
[[textlint]]
name = "persist-snapshot-clock-is-injected"
pattern = '\b(?:SystemTime|Instant)::now\s*\('
paths = ["crates/shepr-mux/src/persist/writer.rs"]
region = "code"
allow_marker = "persist-clock-boundary-ok"
allow_marker_above = 2
message = "pass now into snapshot preservation logic; clock reads need an explicit boundary exception"
```

Once `save`/`clear` take `now`, widen `paths` to `src/persist/` and drop the
allow marker.

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

## HYGG-011 - `clear_integration_path_env` is a hand-maintained duplicate of the agent env-var inventory

**Decision:** piece 1 (the `shepr-core` environment registry, after broadarrow's
`core::env`): every agent directory variable is a registry entry and
`IsolatedEnv` isolates from the registry, so the hand list is replaced by the
one inventory the readers themselves go through.

`shepr-agent/src/integration/tests.rs` lists fifteen variables to remove so
paths resolve against the fake `HOME`. `env.rs` defines fourteen `*_ENV_VAR`
constants plus the two XDG names. Add an agent env var and forget this list, and
every install test for that agent silently inherits the developer's real value:
the test passes on the author's machine, writes into the author's real agent
config, and means nothing. The hunter calls this the clearest example in the
crate of a test that depends on the environment it runs in.

Enforcement rule named: expose `pub(crate) const INTEGRATION_PATH_ENV_VARS:
&[&str]` in `env.rs`, have both the `resolve()`-adjacent code and the test
helper read it, and add a test asserting the list covers every variable the
resolvers consult - checkable by construction if the resolvers take their
variable name from the list. Related: HYGG-053 (the same shape one layer up, in
`IsolatedEnv`).

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

## HYGG-016 - The login-shell tests never check the `-sh` argv0 that makes a shell a login shell

`login_shell_execs_shell_env_without_arguments` in `shepr-pty`, and the
`shepr-mux` twin `login_shell_builder_uses_one_resolved_path...`.
`std::process::Command` does not expose `arg0`, so only a spawn test can check
it - which means the property the tests are named for is unverified.

## HYGG-017 - `pane_terminal_identity_allows_explicit_override` applies the override by a path production does not use

`shepr-mux`. The test applies the override with a raw `cmd.env` after
`apply_pane_terminal_env`, while the production override path is
`PaneLaunchEnv::extra` in `apply_pane_launch_env`. The test therefore cannot
fail.

## HYGG-018 - Two pane-terminal-identity tests restate the values and the list they are checking

**Decision (partial):** `pane_terminal_identity_overrides_outer_terminal_env`
no longer runs `printf` through the host shell and reads `shepr_vt::PANE_TERM`
and `PANE_COLORTERM` directly instead of the hard-coded
`"xterm-256color\ntruecolor\n"`. Open: the restated scrub list in
`pane_terminal_identity_removes_outer_terminal_identity`.

`pane_terminal_identity_removes_outer_terminal_identity` restates the production
scrub list verbatim, so a key added to production is not tested. Enforcement
named: export the list and iterate it.

## HYGG-021 - A geometry size assertion that cannot fail

`shepr-core/src/geometry.rs` (the entry originally placed it in
`shepr-platform`): `assert!(size_of::<PaneGeometry>() <= size_of::<(u16, u16,
u32, u32)>())` asserts a property of the compiler's layout choices, not of this
code; it passes for any plausible field arrangement. Delete it, or state what
size bound actually matters and why. Review only.

## HYGG-023 - The only codec exercise of `ValidatedConfig` covers the empty case of every interesting field

`codec::to_vec(&ValidatedConfig::test_default())` in `shepr-client`
(`shell/tests/mod.rs`, `shell_runtime.rs`, `endpoint/activation_tests.rs`) is
the whole of it. `test_default()` has empty `tab_bar_right`, default sidebar
rows, no styled tokens, no rules and no `rows_by_agent`, so every interesting
branch of `shepr-config/src/wire.rs` - `WireTabBarRightEntry::Command`,
`WireAgentSidebarToken::Styled` with rules, `rows_by_agent` - is encoded by no
test. It reads as coverage of "config is codec-safe". `shepr-config` cannot test
this itself: it does not depend on `shepr-protocol`, correctly, by the layering
in `brokkr.toml`.

Enforcement named: one test in `shepr-client` (or a maximal fixture in
`shepr-test-support`) that round-trips a config exercising every `wire.rs`
variant and asserts equality. The hunter calls this the single highest-value
missing test in that scope, and the data half of HYGG-067.

## HYGG-034 - `EventHub::events_after` cannot report what its production sibling reports

`crates/shepr-api/src/event_hub.rs`. The test-support method returns
`Vec::new()` on a poisoned lock and has no `Lost` signal, while
`events_after_checked` distinguishes both. Eleven call sites in `shepr-server`
tests use it (for example `app/api/panes/tests.rs`,
`assert!(app.event_hub.events_after(0).is_empty())`), and an assertion that a
history is empty cannot distinguish "no events were emitted" from "the lock is
poisoned" - a test that can pass for the wrong reason. Enforcement named: delete
`events_after` and have tests use `events_after_checked(..).expect(..)`.

## HYGG-035 - `status_exposes_only_dynamic_server_capabilities` asserts the absence of fields no type has

`src/cli/status.rs`:
`assert!(value["capabilities"].get("surface_interest").is_none())` and
`..get("health_check").is_none()`. `ServerCapabilitiesJson` has exactly two
fields, so both assertions are true for any possible value of the struct - they
cannot fail. They read as a guard against re-adding removed capabilities, but
nothing connects them to that intent; the real guard is the struct definition.

## HYGG-041 - `attach.rs` is a 1112-line test file named after a subject it does not contain

`shepr-remote/src/lib.rs` declares
`#[cfg(test)] #[path = "remote/attach.rs"] mod attach;`. There is no attach
code; the file is `mod tests { ... }` holding tests for the bridge, the managed
ssh config, the teardown registry, the process pipes, path sanitising, the
output framing, the reattach command and remote discovery. Anyone looking for
attach logic reads a test file; anyone changing `bridge.rs` does not think to
look in `attach.rs`. The project's own convention is that unit tests live next
to the code, and `discovery_tests.rs` in the same directory already shows the
correct naming, so the crate contradicts itself.

Enforcement named: a brokkr rule that a `#[path]`-included module file matching
`*_tests.rs` is allowed and anything else must be non-test, or simply that
`mod X` where `X.rs` contains only `mod tests` is an error.

## HYGG-053 - `IsolatedEnv` guarantees isolation from a list it does not own

**Decision:** piece 1 (the `shepr-core` environment registry, after broadarrow's
`core::env`) is the `shepr-platform` hunter's fix: `IsolatedEnv` iterates the
registry rather than its own lists, which means `shepr-test-support`'s
`["libc"]` allowlist widens to `shepr-core`, and raw environment reads are banned
in `clippy.toml` so a variable cannot be read without being an entry.

`shepr-test-support`'s doc comment claims the guard means "nothing under test
can reach the user's real config, state or agent directories, or the live shepr
server a test run was started from". That guarantee rests on three name lists
inside the crate: `XDG_BASE_DIR_VARS` (four names), the `SHEPR_` prefix, and
`HOME`/`XDG_RUNTIME_DIR`. `shepr-config/src/io.rs` reads `XDG_CONFIG_HOME`,
`XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `HOME`, `SHEPR_CONFIG_PATH`,
`SHEPR_SOCKET_PATH`, `SHEPR_CLIENT_SOCKET_PATH` and `SHEPR_SESSION`;
`shepr-agent/src/integration/env.rs` reads a per-agent set of `*_HOME`-style
variables (`GROK_HOME` among them, per its own tests). The day a variable is
added on the reading side and not here, every test keeps passing while reaching
into the developer's real `$HOME`-adjacent state, and reports nothing.

The `shepr-protocol`/`shepr-config` hunter reports the same thing from the other
end and adds that the duplication is *forced* by the layering:
`shepr-test-support`'s dependency allowlist in `brokkr.toml` is `["libc"]`, so
it cannot import the names from anywhere - and calls for it to be reported as a
duplication with a reason rather than dismissed. Its proposed fixes: either
widen the allowlist to `shepr-core` and export the name set from there, or add a
test in `shepr-config` asserting every variable it reads is in the isolation
list.

The `shepr-platform` hunter calls this the highest-value mechanical fix in its
report: if every environment variable name lives in one `shepr-core::env`
module, `IsolatedEnv` can iterate that module's full list instead of restating a
subset, and a `brokkr.toml` text rule forbidding env-name literals elsewhere
keeps the list complete by construction. Related: HYGG-011 (the same shape
inside `shepr-agent`'s own test helper).

## HYGG-057 - `is_posix_acl_xattr` is keyed on the `system.posix_acl_` prefix

`shepr-platform/src/config_file.rs`. Correct today; a filesystem that expresses
access control under another prefix (a security label, richacl) falls into the
best-effort branch that ignores failures. The comment says as much, which the
hunter calls the honest version.

## HYGG-058 - The `SHEPR_` prefix scrub silently keeps a non-UTF-8 key

**Decision:** piece 1 (the `shepr-core` environment registry): `IsolatedEnv`
isolates from the registry's entries rather than by scanning for a `SHEPR_`
prefix, so the fail-open string test goes.

`shepr-test-support`:
`key.to_str().is_some_and(|k| k.starts_with("SHEPR_"))`. Not reachable in
practice, but it is the fail-open shape.

## HYGG-061 - `hook_registration_is_current` fails open for five targets and the coupling is invisible

`shepr-agent/src/integration/registry.rs`:
`Target::Pi | Omp | Kilo | Grok | Opencode => true` is documented for Pi, Omp
and Kilo (directory-loaded plugins) and for Grok and Opencode (checked by their
own helpers earlier in `integration_status_at`). The coupling is invisible from
either site: if the Grok special case above it were deleted, this `true` would
report a broken Grok install as Current with nothing noticing.

Enforcement named: make the spec row carry a `RegistrationCheck` variant
(`SelfRegistering | Json { file, root, depth } | Custom(fn)`) so the match is
exhaustive over data rather than over a target list.

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

## HYGG-072 - `server_not_running`'s test helpers string-match the code their own comment says they do not

`src/cli/server_not_running.rs`. `was_reported` / `reported_response` are
`#[cfg(test)]` helpers that `matches!` on `response.error.code ==
"server_not_running"`, while the doc comment at the call site in `src/cli.rs`'s
test says "The typed error preserves the response without string matching." If
the code is renamed, both helpers become silent no-ops and the test
`maps_dead_server_connect_failure_to_friendly_error` fails loudly - so this one
fails closed, which the hunter calls fine. **The claim in the comment is what is
false.**

## HYGG-075 - `run_on_machine`'s comment names an exclusion a different file enforces

`src/cli/target.rs::validate_machine_command`'s comment says `--machine`
excludes "no local file evaluation (`agent explain --file`)". That is enforced by
`agent::Command::is_api_command` (`Self::Explain(args) => args.file.is_none()`)
in a different file, and nothing ties the comment to it. True today, and the
existing test covers the `--file` case, so the hunter marks this one held.

## HYGG-076 - `machine_mutation_commands_only_expose_add_and_remove` is a blocklist of three strings

`shepr-remote`:

```rust
for command in ["rename", "enable", "disable"] { assert!(spec.try_get_matches_from(...).is_err()) }
```

This is the only enforcement of `AGENTS.md`'s "Saved machines are add/remove
only" claim at the CLI surface. It checks that three specific historical
subcommand names are absent, so adding `machine update`, `machine set-label`,
`machine edit` or `machine relabel` passes. The test reads as an invariant and
is a blocklist. The hunter marks it as not holding the claim it is named for.

Enforcement named: assert the *set* of `machine` subcommands equals
`{list, status, reconnect, add, remove}` - clap can enumerate them via
`command.get_subcommands()`. Cheap, and turns a fail-open name check into a
closed set. Related and better, recorded so it is not re-hunted:
`EndpointCatalog::apply_profile_delta` *does* enforce add/remove structurally -
"updating saved endpoint {id} is not supported" - at the storage layer. So the
claim is enforced where it matters and guarded by a name list where it is
exposed; the hunter asks for that to be said at both sites.

## HYGG-077 - `REMOTE_MISE_SHIM_SUFFIX` is a fail-open name guard

`shepr-remote/src/machine/executable.rs` rejects a discovered path ending in
`/mise/shims/shepr`. mise's shim directory is relocatable (`MISE_DATA_DIR`), and
the equivalent problem exists for asdf, rtx's legacy layout,
`~/.local/share/pipx`, and any other shim dir. When the name stops matching the
guard becomes a silent no-op and shepr caches a shim path that re-execs
something else - precisely the failure the guard was written for, reported as
nothing. Not enforceable as a name rule. The structural answer named: test the
candidate rather than its spelling - the status probe already runs
`status client --json` on the candidate and compares `build_id`, so a shim that
resolves to the right binary is fine and one that does not already fails.
Consider deleting the guard in favour of the probe and keeping only a diagnostic
note when a rejected candidate looked like a shim.

## HYGG-078 - `ssh_config_include` fails open on `is_file()`

**Decision (partial):** the `Path::exists` seal (`clippy.toml`) is extended to
`Path::is_file` and `Path::is_dir`. The `is_file()` filter therefore becomes an
explicit metadata match: `NotFound` skips the include, and any other error is
reported instead of silently dropping it. Open: the `debug` log of emitted and
skipped includes.

`shepr-remote/src/remote/ssh.rs`: `path.filter(|path| path.is_file())`. A
symlink to a file passes (fine), an absent file is silently dropped (fine), and
if OpenSSH on this host reads its system config from somewhere else entirely
(`/etc/ssh/ssh_config.d/*`, a distro override) the managed config silently omits
settings the user believes are active, with no line logged. The include ordering is now tested but still not
observable at runtime. Fix named: log
at `debug` which includes were emitted and which paths were skipped; the path
list itself cannot be enforced against OpenSSH's actual search order.

## HYGG-080 - The remote `status --json` contract is two independent structs with no shared type

`src/cli/status.rs` defines `ServerStatusJson`/`ClientStatusJson` (`Serialize`);
`shepr-remote` defines `RemoteServerStatusJson`/`RemoteClientStatusJson`
(`Deserialize`) and parses the same JSON over SSH. Field names (`running`,
`version`, `build_id`, `capabilities.detached_server_daemon`) are spelled
independently on both sides. Renaming `running` in `status.rs` breaks every
saved machine at runtime and the build says nothing. The only thing pinning them
is a hardcoded JSON literal in `attach.rs`, written by hand, which can drift
from `status.rs` freely. Additionally, `ServerStatusJson` carries both
`status: "running"|"not_running"` and `running: bool` - two representations of
one fact - and `shepr-remote` reads only `running`, so the `status` string is
unread by the only programmatic consumer.

Enforcement named: put the shape in `shepr-api::schema` and have both sides use
the one type; this is a cross-process contract inside one build, so no copy is
needed at all. Failing that, a test that serialises `ServerStatusJson` and
deserialises it as `RemoteServerStatusJson` - possible today and absent. A
related rule the same hunter proposes for the producer side: a test that
round-trips each generated remote command string
(`"remote-client-bridge"`, `"remote-api-bridge"`,
`"--check"`, `"status"`, `"client"`, `"server"`, `"--json"`, `"server stop"`,
`"--session"`) through `cli::spec::command().try_get_matches_from`, so the
parser proves the producer - the only current check is byte-for-byte golden
strings in `attach.rs`, which pin the producer to itself and say nothing about
the parser.

## HYGG-081 - Two `shepr-remote` comment claims nothing checks

- `RemoteSsh` doc: "no noninteractive command runs past it [the attempt
  deadline]" - `sh_output` and `framed_user_shell_output` honour it;
  `SshStdioBridge::start` and the `establish` callback do not consult it (the
  supervisor holds them to it separately). Checkable with a fake clock.
- `bridge.rs`: "Each local API request has its own stream and therefore its own
  SSH stdio process. The streams are served serially" - true by construction (a
  single accept loop) but nothing asserts it; a future `thread::spawn` per stream
  would break the claim silently.

## HYGG-085 - `shepr-platform`'s `lib.rs` says domain rules live in modules that do not exist there

`crates/shepr-platform/src/lib.rs`: "domain rules live with their consumers in
`detect`, `remote`, and the app". There is no `detect` or `remote` module in
this crate or at that path in this workspace - they are
`crates/shepr-agent/src/detect` and `crates/shepr-remote`. **False today**, and
false in a second way: 25 domain event functions (`workspace_created`,
`tab_renamed`, `pane_spawned`, `session_saved`, `api_request_started`,
`integration_action`, ...) live in `logging.rs` in this crate, named after
concepts the crate knows nothing about.

Enforcement named for the second half, and the hunter says `brokkr.toml`
already expresses this kind of rule: a text rule forbidding the identifiers
`workspace`/`tab`/`pane`/`api`/`session` in `shepr-platform` public item names,
or simply moving the functions so the existing dependency allowlists do the
work.

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

## HYGG-088 - `AGENTS.md`'s description of `shepr-vt`'s `scan.rs` names things that live elsewhere and omits the module list

`AGENTS.md` says `scan.rs` scans "modes 9/1016/2031/2048 ... and the halfwidth
katakana voiced marks". Those live in `handler.rs`. `scan.rs` explicitly forbids
them and has a test for it
(`modes_and_reset_are_left_to_the_parser_handler`). `AGENTS.md` also omits
OSC 9;4, 9;9, 1337 and `CSI ? 3 J`, and its module list for `shepr-vt` omits
`handler.rs`, `modes.rs`, `rows.rs`, `selection.rs`, `coords.rs` and `locks.rs`.
The hunter files this as documentation restating a list the code owns.

## HYGG-089 - `AGENTS.md` says the pane child is reaped by a blocking `wait()` in the pane runtime

It is reaped through a pidfd `waitid`; blocking wait is only the fallback, per
commit b7c14da. **False today.**

## HYGG-090 - `AGENTS.md` and `app/mod.rs` both say `src/app/` is split into state, actions and input

**False as written**, per the `shepr-server` hunter. There is no `input` module
under `src/app/` at all; `app/` holds 20 modules (`actions`, `agent_resume`,
`agents`, `api`, `api_helpers`, `creation`, `events`, `git_refresh`,
`host_theme`, `ids`, `runtime`, `session`, `state`, `tab_bar_status`,
`terminal_targets`, `terminal_titles`, `window_title`, `snapshot_tests`, plus
the `api/` and `actions/` subtrees), and input lives in
`server/pane_input.rs` and `server/input_wire.rs`. The module doc at
`app/mod.rs` restates the same stale claim, listing only `state.rs` and
`actions.rs` - a doc comment enumerating a list the directory generates. The
no-god-object intent is honoured in substance (nothing is a 3000-line
monolith), but `impl App` is spread over 25 blocks in 20 files and `impl
AppState` over 6, so the "three parts" framing no longer describes anything.

Enforcement named: restate the claim as what is true and enforceable - `AppState`
is pure data, `App` owns runtime, no single file over N lines - and add a
file-length rule to `brokkr.toml`. The prose itself is not lintable and should
stop naming a module count that drifts.

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

## HYGG-093 - Claim: `AppState` is pure data

`AGENTS.md`. The `shepr-server` hunter's verdict: **holds** - no channels, no
runtime, no `Arc`, and `test_new()` works. One leak recorded:
`app/state.rs`'s `terminal_runtime_shutdowns: Vec<TerminalId>` ("runtimes that
should be shut down by the app/runtime layer") is a pending-effects queue, not
state. It is data-shaped so the claim survives literally, but it is the seam
through which pure state schedules side effects, and nothing stops the next such
field from being a channel. Returning the shutdown list from the mutating call
instead of parking it on state removes the field.

## HYGG-094 - Claim: render is pure

`AGENTS.md`: "`compute_view()` updates only `AppState::view`; pane runtimes are
resized by explicit geometry paths, and surface drawing takes shared references
and only draws." The `shepr-server` hunter's verdict: **holds in behaviour, not
in structure.** `compute_view` does touch only `view`, but it takes
`&mut AppState` so nothing enforces it. `resize_pane_infos` (`ui/panes.rs`)
takes `app: &AppState` and a `&PaneRuntimeRegistry` and calls `rt.resize(...)`
through them, and so do `resize_tab_surface`, `resize_tab_surface_layout` and
`resize_all_tab_surfaces`; the draw path (`render_tab_surface`) and the resize
paths take *identical* signatures `(&AppState, &PaneRuntimeRegistry, ...)`.
Nothing in the type system distinguishes "only draws" from "resizes every PTY in
the tab"; the invariant rests entirely on which function name a caller types, and
in `render.rs` the two are interleaved in one function, which is where a mistake
would land.

Enforcement named: give the resize paths a distinct receiver - a
`PaneResizer<'a>` newtype over the registry, constructed only on the explicit
geometry paths - so a drawing function cannot reach `resize`; and make
`compute_view` return `ViewState` instead of taking `&mut AppState`, which makes
"touches only `view`" true by signature.

The `shepr-vt`/`shepr-pty` hunter reports a violation of the same claim in
`shepr-mux/src/pane/terminal/backend.rs` (`render()`,
`collect_dirty_patch()`, `synchronized_output_active()` and
`synchronized_output_state()` all call `flush_expired_synchronized_output`, which
feeds the buffered frame through the parser and mutates the grid), but files it
as a live defect, so a fix pass should expect that entry in the bug document
rather than here.

## HYGG-099 - Most workspace invariants are enforced only by opt-in test calls

**Partly resolved:** the non-empty tab list and the in-range focused tab are now
structural - a private `FocusedTabs` in `shepr-mux/src/workspace.rs` owns both,
and the `Deref` impls and their `expect` are gone. `debug_assert!` is banned, so
checking after every mutation means a real `assert!` or more structure. Open:
the rest of what `assert_invariants_for_test` states (unique public tab and pane
numbers, layout pane set equal to the pane records, focused pane in the layout,
root pane present) still runs only where a test calls it.

`shepr-mux/src/workspace.rs`. `Workspace::assert_invariants_for_test` (150
lines, `#[cfg(any(test, feature = "test-api"))]`) is the real statement of the
invariant - non-empty tabs, `active_tab` in range, unique public tab and pane
numbers, layout pane set exactly equal to the pane record set, focused pane in
layout, root pane present. It is called from 25 places, all of them individual
tests that chose to call it; nothing calls it after a mutation in production and
no mutating method calls it. The recent commit "Enforce the one-tab workspace
invariant" gets its enforcement from the `Deref` panic plus these opt-in calls.

Which are checkable: all of them, and they are already written down as
assertions - the gap is only *when* they run. Which are false today: none the
hunter could find by reading; the adversarial constructor
`test_adversarial_identity_state` exists precisely to exercise the divergences.

Enforcement named: either call the checker `debug_assert`-style from every
mutating method's exit, or restructure so the checks are unnecessary -
`tabs: Vec<Tab>` plus `active_tab: usize` is the whole problem and a
non-empty-vec type with a focused index is the structural answer. The hunter
prefers the second given the project's posture. The `Deref` half (an `expect` in
a `Deref` impl, with `active_tab` public and `tabs_mut()` handing out
`&mut [Tab]` to any crate, so a server-side caller can put `active_tab` out of
range and the next `ws.panes` aborts the server) is filed by the hunter under
errors, so a sibling document may carry it too.

## HYGG-103 - The same shape one level down: `SHEPR_*` names spelled as literals in shipped assets with no membership test

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) owns every name in `shepr-core` rather than
`shepr-config`, and includes the asset-walking test that every `SHEPR_*`
literal is a registry member. Open: the two bare Rust writers of
`SHEPR_BIN_PATH` (no text rule against `"SHEPR_` literals was decided).

`shepr-mux/src/pane/launch.rs` exports `SHEPR_PANE_ID_ENV_VAR` publicly but
writes `"SHEPR_BIN_PATH"` as a bare literal; `shepr-server/src/app/tab_bar_status.rs`
writes `"SHEPR_BIN_PATH"` as a bare literal too, so there are two independent
writers of one name with no shared definition. Roughly twenty shipped hook
assets read both names as literals, and `shepr-agent` reports that every asset
restates the whole contract - `SHEPR_ENV` must equal `"1"`,
`SHEPR_SOCKET_PATH` and `SHEPR_PANE_ID` must be non-empty, `SHEPR_BIN_PATH`
falls back to the bare name `shepr` - field by field, in four languages. The
assets are a genuinely forced copy; nothing keeps them in step today.

Enforcement named: one module (`shepr-config`) exporting every `SHEPR_*` name, a
`brokkr.toml` text rule forbidding `"SHEPR_` string literals elsewhere, and one
test that walks `crates/shepr-agent/src/integration/assets/**` extracting
`SHEPR_[A-Z_]+` and asserts set membership. The `shepr-mux` hunter says that
last test is the only thing that can ever keep the deployment-forced copies
honest, and it does not exist.

## HYGG-111 - `manifest.rs`'s module doc enumerates region names, matcher keys, gate keys and limits in prose

`shepr-agent`, next to `RegionSpec::parse`, `ManifestRule`, `ManifestGate` and
the `MAX_*` constants - including "at most eight levels total". The hunter says
the doc-comment case is enforceable only by a doc test that parses the prose,
which is not worth it, and that the honest fix is to shorten the prose to the
concepts and point at the enum. Same entry carries `notes/todo.md`'s stale
paths (`src/integration/assets/...`, `src/detect/manifests/...`,
`src/ghostty/rows.rs`, `src/protocol/wire.rs`, `src/client/shell/state.rs`) -
none of which exist since the crate split, all now under `crates/`. `notes/`
carries no truth guarantee, but the paths are stale enough to send a reader
nowhere.

## HYGG-112 - `HeadlessServer`'s module doc names socket files the config owns

`shepr-server/src/server/headless.rs` claims the server listens on `shepr.sock`
and `shepr-client.sock`. Those names live in `shepr-config::address` and are
session-dependent: a named session listens on
`sessions/<name>/shepr-client.sock`, per `socket_paths.rs`. Documentation
restating a list the code generates. Fix named: delete the names from the
comment.

## HYGG-113 - Four module docs in the terminal core cite a dependency and a directory that are not there

**Decision (partial):** the root `Cargo.toml` citation of `research/alacritty`
(and `shepr-mux`'s `pane/osc.rs` citation of `research/vte/src/lib.rs`, the same
shape) is reworded to point at the pinned `alacritty_terminal` and `vte` sources
in the cargo registry, as `AGENTS.md` already does. Open: the other three
bullets and the Ghostty naming.

- `shepr-vt/src/format.rs`'s module doc says the VT output is replayed "after
  some resizes"; `shepr-mux`'s `backend.rs::resize` says that replay was
  removed.
- `shepr-vt/src/lib.rs`'s doc for `DEFAULT_FOREGROUND` says it matches "what the
  libghostty-vt render state reported". That backend is gone.
- The root `Cargo.toml` comment cites a "reference checkout in
  `research/alacritty`". No `research/` directory exists.
- `shepr-mux/src/pane/terminal/backend.rs` says a live workaround exists because
  "the libghostty core loses rows on resize" - the stated reason for a workaround
  in live code cites a dependency that is not present, so nobody can now check
  whether `alacritty_terminal` has that behaviour and the workaround is
  unfalsifiable. `backend.rs` also emits an operator-facing log line naming a
  component that does not exist: `error!(pane = ..., "ghostty core lock poisoned
  in reader")`.

All marked stale today. The `shepr-mux` hunter adds that roughly 200 production
references to Ghostty naming remain (`GhosttyPaneTerminal`, `GhosttyPaneCore`,
`PaneTerminal { ghostty }`, about forty `ghostty_*` free functions) and proposes
the cheapest enforcement in its report: a `brokkr.toml` text rule forbidding
`ghostty`/`Ghostty` outside a comment that explains a historical decision, or
forbidding it outright after a rename - the same mechanism the gremlin scan
already uses. The two workaround comments need a human to decide whether the
workaround is still needed against the real emulator, which cannot be answered
by reading; `AGENTS.md` already directs the reader to the pinned
`alacritty_terminal` source in the cargo registry.

## HYGG-116 - Claim: alacritty types never leak out of `shepr-vt`

The dependency rule enforces this at crate level, and the hunter found no
alacritty types in public signatures. Two soft spots recorded: the public
`impl From<Rgb> for RgbColor` and `From<RgbColor> for Rgb`; and `CellStyle`,
which is `pub` in a private module and reachable through `CellBasicData.style`
but not re-exported, so callers cannot name it.

## HYGG-117 - `PtyIoInbox` assumes an entry's `order` identifies one entry

`shepr-pty`. `insert_resize_replies` inserts several entries with the *same*
order, and `next_entry_index(current_order)` uses `position(order == order)`. It
is correct only because those replies are contiguous and written front to back.
Enforcement named: give each reply its own order.

## HYGG-118 - A mutation rule in `shepr-vt` that nothing checks

"Every parser-driven mutation must use `with_handler`", and every mutation must
end with `collect_damage()` (or `bump_full_damage`). Callers do this by hand:
`write`, `flush`, `mode_set`, `resize`, the scroll methods. Enforcement named,
structurally: call `collect_damage` inside `with_handler`.

## HYGG-124 - `HostModes::apply_mouse` records the restore flag only when a parameter that means something else is true

`shepr-client`/`shepr-termio`. `apply_mouse` records
`RESTORE_MOUSE_CAPTURE` only when called with `reassert == true`. Both setup
paths do pass `true`, so it holds today - but the flag that decides whether
mouse capture gets turned off at exit is set by a parameter that means
"re-send even if unchanged". A future caller with `reassert: false` that enables
capture leaves the user's terminal in mouse mode after shepr exits.

The surrounding structure is what makes it unverifiable: `HostModes` guards
`HostModesState` behind a `Mutex` and `restore_state` behind an `AtomicU8`, with
a comment explaining that the panic hook must restore without taking a lock the
panicking thread may hold - deliberate and sound for the panic case - but it
means the restore intent and the mode state are kept consistent only by each
setter remembering to call a recorder before and after its write, and
`set_keyboard_enhancement_flags`, `set_direct_keyboard_protocol` and
`set_modify_other_keys` each do that pairing slightly differently (the first
records `false` for modify-other-keys on success unconditionally; the second and
third record the computed value). Whether those three agree is not checkable
from the types. The hunter says it is not mechanically enforceable and the
lock-free restore path is worth keeping; the honest statement is that the
recorder pairing is a convention maintained by three call sites, and a single
`set_keyboard(...)` entry point that computes the flags itself would reduce it
to one.

## HYGG-125 - The schema macro builds `MethodTraits` twice, and its test pins a count

`crates/shepr-api/src/schema.rs`: the method macro now spells each wire name
once, but `traits()` and `traits_for_name()` each construct `MethodTraits`, so
the flags (including the client-shell lane) are generated at two sites that
must agree. The schema test asserts that 27 methods are classified for the
client-shell lane - a count, not the set - so swapping one method in and another
out passes. Have `traits_for_name` resolve the name to a variant and reuse
`traits()`, and assert the lane as a named set.
