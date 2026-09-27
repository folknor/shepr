# Hygiene hunt: `shepr-protocol` and `shepr-config`

Scope read: every file in `crates/shepr-protocol/src` and
`crates/shepr-config/src`, plus the consumers a value or rule led into
(`shepr-server/src/server/headless/bootstrap.rs`,
`shepr-server/src/server/client_transport.rs`,
`shepr-server/src/server/client_commands.rs`,
`shepr-client/src/shell/endpoints.rs`, `shepr-termio/src/input/`,
`shepr-test-support/src/lib.rs`, `shepr-core/src/pathutil.rs`, `src/main.rs`,
`brokkr.toml`).

Findings are grouped by the eight questions. Each carries an **Enforce:** line
saying whether the fixed version can be held mechanically and by what. Nothing
here is ranked.

Two headline answers first, because both in-scope claims are partly false.

---

## The two claims

### Claim A: "Wire types must not use `skip_serializing_if`, `flatten`, `untagged` or tagged enums."

**True today for the types that actually cross the codec. Not enforced by
anything, and the runtime backstop is data-dependent.**

- Nothing in `brokkr.toml`, `clippy.toml` or the workspace lint table mentions
  these attributes. The claim lives in `AGENTS.md` and in the `codec.rs` module
  doc only.
- The runtime backstop is incomplete and, for one of the four, *conditional*:
  `codec.rs`'s own test asserts
  `to_vec(&Skipping { value: None })` yields `CodecError::SkippedField` but
  `to_vec(&Skipping { value: Some(1) })` succeeds and returns `[1, 1]`. So a
  wire type carrying `skip_serializing_if` encodes fine for every value where
  the predicate is false and fails only in production, on the first message
  where the field happens to be absent. That is a guard keyed on data, not on
  shape.
- There is no test that `flatten`, `untagged` or an internally/adjacently
  tagged enum is rejected. `unsupported_shapes_are_rejected` covers a manual
  `serialize_seq(None)` and `IgnoredAny`, not the four named shapes. In
  practice `flatten` dies as `CodecError::UnknownLength` and `untagged` as
  `NotSelfDescribing` on decode, but only if a test happens to exercise that
  message.
- `shepr-config` contains **four** shapes the codec cannot encode, all of them
  `pub` or reachable from `pub` types:
  - `BindingConfig` - `#[serde(untagged)]` (`keybinds.rs`).
  - `TabBarRightEntryConfig` - `#[serde(tag = "type", ...)]`, an internally
    tagged enum (`tab_bar.rs`).
  - `RawRule` behind `SidebarTokenRule`'s `#[serde(try_from, into)]` - ten
    `skip_serializing_if = "Option::is_none"` fields (`sidebar/rules.rs`).
  - `AgentSidebarToken` / `SpaceSidebarToken` - hand-written `Serialize` using
    `serializer.serialize_map(None)` in `serialize_styled_token`, i.e.
    `CodecError::UnknownLength` (`sidebar.rs`).

  None of these reach the codec, solely because `shepr-config/src/wire.rs`
  hand-mirrors each one. But `WireConfig` *does* reuse raw config types
  unmirrored: `ThemeConfig`, `SessionConfig`, `ServerConfig`, `AdvancedConfig`,
  `RemoteConfig`, `SidebarCollapsedModeConfig`, `PaneBordersConfig`,
  `TabBarPositionConfig`, `StatusIndicatorStyle`, `AgentPanelSortConfig`,
  `ConfigAgent`, `ImeCursorShape`. So the boundary is not "config types never
  cross the wire"; it is "these twelve do and those four do not", with no
  marker, trait or naming rule distinguishing them. Adding a
  `skip_serializing_if` to `ServerConfig` for nicer TOML output would compile,
  pass every existing test, and break attach at runtime.

  **Enforce:** two things, both cheap. (1) A gremlin-style text rule in
  `brokkr.toml` forbidding `skip_serializing_if`, `flatten`, `untagged` and
  `serde(tag` under `crates/shepr-protocol/src` (excluding `#[cfg(test)]` - the
  one legitimate occurrence is `codec.rs`'s `Skipping` fixture; either exempt
  that file or move the fixture). (2) A marker trait (`trait WireSafe {}`)
  implemented only by mirrored types, with `WireConfig`'s fields bounded on it,
  so reusing a TOML-facing type in `WireConfig` stops compiling. Neither is
  possible today.

- Also unverified: **nothing round-trips a non-default `ValidatedConfig`
  through the codec.** The only exercise is
  `codec::to_vec(&ValidatedConfig::test_default())` in client tests
  (`shell/tests/mod.rs`, `shell_runtime.rs`, `endpoint/activation_tests.rs`).
  `test_default()` has empty `tab_bar_right`, default sidebar rows, no styled
  tokens, no rules and no `rows_by_agent`, so every interesting branch of
  `wire.rs` - `WireTabBarRightEntry::Command`,
  `WireAgentSidebarToken::Styled` with rules, `rows_by_agent` - is never
  encoded by any test. `shepr-config` cannot test this itself: it does not
  depend on `shepr-protocol` (correctly, by the layering in `brokkr.toml`).
  **Enforce:** one test in `shepr-client` (or a maximal fixture in
  `shepr-test-support`) that round-trips a config exercising every `wire.rs`
  variant and asserts equality. This is the single highest-value missing test
  in scope.

### Claim B: "Config is read and validated once at launch. No reload, no fallbacks. Any config problem fails the launch."

**True for the server's own config. False across a host boundary, and the
remote path swallows the failure.**

- The local path holds: `Config::load_validated` → `into_validated` returns
  `Err` if `diagnostics` is non-empty, `main.rs::load_validated_config_or_exit`
  exits 1, and `bootstrap.rs::encode_resolved_config` propagates an encode
  failure with `?` so the server refuses to boot.
- But `ValidatedConfig`'s `Deserialize` re-runs
  `from_resolution(..., CwdCheck::Received)`. A remote endpoint's config is
  therefore *validated at the moment of use*, on the client, at attach time -
  not at that client's launch. A config the server accepted can be rejected by
  the client. This duplication is forced (two hosts, two binaries, one config
  travelling between them) and what keeps the two validations in step is the
  exact-build preamble plus the shared crate - worth saying out loud in the
  `AGENTS.md` sentence, which currently reads as absolute.
- **Real defect on that path.** `shepr-client/src/shell/endpoints.rs`
  `cache_endpoint_snapshot` does
  `codec::from_slice_exact::<ValidatedConfig>(&snapshot.resolved_config).ok()`
  - the decode error is discarded, nothing is logged, and
  `endpoint.resolved_config` is set to `None`, discarding any previously good
  cached config. `resolve_snapshot_config` then decodes the *same bytes again*
  and this time propagates. So: the config is decoded twice per new snapshot on
  the fanout path, the first error is swallowed silently, and for a
  **non-active** endpoint the error is never surfaced at all
  (`apply_cached_endpoint_snapshot` only reports when
  `endpoint_id == self.active_endpoint_id`). A remote machine shipping an
  unreadable config looks healthy in the sidebar until you switch to it.
  **Enforce:** decode once, keep the `Result`, log at `warn` with the endpoint
  id, and set the endpoint's status to the error immediately. A test that a
  non-active endpoint with a corrupt `resolved_config` reports an error would
  hold it.
- The message that does reach the operator names no subject: an empty
  `resolved_config` with no cache produces
  `"invalid endpoint configuration: unexpected end of input: needed 1 bytes, 0
  remaining"` - no host, no endpoint, no session. See §4.

---

## 1. One value, one owner

**1.1 The keybinding action list is spelled eight times; three of the eight are
compiler-checked.** This is the largest single finding in scope.

| site | checked against anything? |
|---|---|
| `model.rs` `KeysConfig` - 51 `BindingConfig` fields | source of truth |
| `model.rs` `impl Default for KeysConfig` | yes, struct literal |
| `keybinds.rs` `Keybinds` + `NavigateKeybinds` structs | no |
| `keybinds.rs` `Keybinds { ... empty_action!() ... }` literal | yes, against `Keybinds` |
| `keybinds.rs` the ~51 `apply_action!`/`apply_indexed!`/`apply_navigate!` lines | **no** |
| `wire.rs` `key_binding_fields!` macro list | yes, `take_bindings!` builds `KeysConfig` |
| `default.toml` comment block | partly (see 1.2) |
| `shepr-termio/src/input/keybindings.rs` dispatch + `keybind_help.rs` entries | no |

The uncovered one that bites: add a field to `KeysConfig` and forget its
`apply_action!` line and it compiles, the user's binding parses, and the action
simply never fires - with no diagnostic. Same for a missing `keybindings.rs`
dispatch row or `keybind_help.rs` entry.

**Enforce:** collapse the list into one declarative table (one macro
invocation naming each action once with its kind - action / indexed /
navigate - its default binding and its help text) that generates `KeysConfig`,
its `Default`, `Keybinds`, the apply loop, the wire mapping and the help
entries. That makes every omission a compile error. I think this is worth the
rewrite; it is the only way the eight sites stop drifting.

**1.2 `KEY_BINDING_COUNT = 51` is a hand-maintained count of a
compile-time-known list** (`wire.rs`). It is correct today (verified: 51
`BindingConfig` fields). The macro list it guards is already tied to
`KeysConfig` by the compiler, so the constant's only job is a runtime
`Err("resolved config has N keybindings; expected 51")` that cannot trigger.
**Enforce:** derive it from the macro (`[$(stringify!($field)),*].len()`), or
delete it and the runtime check with it.

**1.3 Every default appears twice - once in a `Default` impl, once as a
`default.toml` comment - and only the keybindings are checked.**
`src/main.rs::default_config_documents_every_keybinding_with_its_default`
covers `[keys]` only (and only string-valued entries; it `continue`s past
anything that is not a TOML string). Unchecked: `sidebar_width = 26`,
`sidebar_min_width = 18`, `sidebar_max_width = 36`, `mouse_scroll_lines = 3`,
`headless_cols = 120`, `headless_rows = 40`, `scrollback_limit_bytes =
10000000`, `startup_per_agent_delay_ms = 100`, `row_gap = 0`, `window_title =
"{hostname}: {workspace}"`, every enum default, and the whole `[theme]` and
`[remote]` blocks. **Enforce:** generalise the existing test - serialise
`Config::default()` to a TOML table, walk every leaf, assert each appears as
`# key = <value>` in `DEFAULT_CONFIG`. The mechanism already exists in
`main.rs`; it is just scoped to one table.

**1.4 Already diverged: the built-in theme list.** `theme.rs` `THEME_NAMES`
holds 18 themes. `default.toml`'s comment lists 11. Every light variant -
`catppuccin-latte`, `tokyo-night-day`, `gruvbox-light`, `one-light`,
`solarized-light`, `kanagawa-lotus`, `rose-pine-dawn` - is implemented,
accepted by `canonical_theme_name`, named in the *error message* for an unknown
theme, and absent from the printed default config. This is a fact, not a
prediction. **Enforce:** assert every `THEME_NAMES` entry appears in
`DEFAULT_CONFIG` - same shape as the keybinding test.

Theme names are also spelled three times in code (`THEME_NAMES`,
`canonical_theme_name`'s match, `Palette::from_name`'s match) plus 18
constructor fns. `built_in_theme_names_resolve` covers
`THEME_NAMES → canonical → from_name` in one direction only; a palette
implemented but missing from `THEME_NAMES` is undetected, which is exactly the
failure mode that produced the divergence above.

**1.5 The default theme name `"catppuccin"` is a bare literal in
`theme_config.rs::resolve_palette`** (`config.theme.name.as_deref().unwrap_or("catppuccin")`),
restated in `default.toml`'s comment and in `THEME_NAMES[0]`. **Enforce:** a
`DEFAULT_THEME` const referenced from all three, plus 1.3's test.

**1.6 `right_click_passthrough_modifier`'s accepted-value set is spelled five
times**: the parser in `model.rs`, the hand-written error-message constant
`RIGHT_CLICK_PASSTHROUGH_MODIFIER_VALUES` (which restates the alias list by
hand), the `Serialize` impl (which emits a *narrower* set: `off`/`ctrl`/`alt`/`ctrl+alt`),
`WireRightClickModifier` in `wire.rs`, and `default.toml`'s comment.
**Enforce:** a table of `(&str alias, Option<KeyModifiers>)` that the parser,
the serialiser and the error message all read. A round-trip test over the table
would then be the whole check.

**1.7 What an empty string means is invented per config key.** `window_title =
""` means "leave the title alone". `right_click_passthrough_modifier = ""`
means "disabled". `terminal.default_shell = ""` means "$SHELL, then /bin/sh".
`terminal.new_cwd = ""` is a documented *error*. `ui.accent = ""` is a hard
launch failure (`parse_configured_color` on a non-optional `String`), even
though `default.toml` says "Unset uses the theme accent" - a user who reads that
and writes `accent = ""` gets a refused launch. Five keys, four rules.
**Enforce:** not mechanically. The fix is a type: `Option<NonEmpty<String>>`
or a small `ConfigOverride<T>` that spells "unset" once, at which point the
rule is in one place.

**1.8 `ui.accent`'s `Default` is `"cyan"`, which is never applied.**
`resolve_palette` only uses `ui.accent` when `provenance.is_explicit(Accent)`.
So the Rust default is a sentinel whose only live requirement is that it parses
as a colour, and `default.toml` documents a different value (`#89b4fa`) with a
different meaning ("unset uses the theme accent"). **Enforce:** make the field
`Option<String>`; then the "unset" case is representable and the sentinel goes
away. A test cannot express this one.

**1.9 The XDG variable-name set has three owners.** `shepr-config/src/io.rs`
reads `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `HOME`,
`SHEPR_CONFIG_PATH`, `SHEPR_SOCKET_PATH`, `SHEPR_CLIENT_SOCKET_PATH`,
`SHEPR_SESSION`. `shepr-core/src/pathutil.rs` owns the `HOME` rule.
`shepr-test-support/src/lib.rs` holds an independent list of names it clears or
points at scratch - and that list is what keeps tests out of the real
`~/.config`. This duplication is **forced** by the layering:
`shepr-test-support`'s dependency allowlist in `brokkr.toml` is `["libc"]`, so
it cannot import the names from anywhere. What keeps them in step today:
nothing. If `shepr-config` starts reading a new variable, isolation silently
stops covering it. **Enforce:** either widen the `shepr-test-support` allowlist
to `shepr-core` and export the name set from there, or add a test in
`shepr-config` asserting every variable it reads is in the isolation list (the
list would have to be exposed for that). Report it as a duplication with a
reason, not a non-finding.

**1.10 Aliases that create a second name for one value.**
`shepr-server/src/server/client_transport.rs` defines
`MAX_CLIENT_SHELL_DIMENSION`, `MAX_CLIENT_SHELL_CELLS`,
`MAX_CLIENT_CELL_SIZE_PX` and `client_commands.rs` defines
`ENDPOINT_RESPONSE_CHUNK_BYTES`, each a direct `= shepr_protocol::MAX_*`. They
are correct, but they read as server-local knobs; someone tuning one will edit
the alias and find it does nothing independent. **Enforce:** delete the
aliases and use the protocol constants directly. Nothing mechanical.

**1.11 `"shepr.sock"` and the `-client.sock` suffix.** The socket basename
lives in `session_id.rs::api_socket_path_under`; the client name is derived in
`address.rs::derive_client_socket_from_api_socket`. Good - one owner each. But
`ServerAddress`'s test-only `Default` spells both literally *and relatively*
(`"shepr.sock"`, `"shepr-client.sock"`), which its own `validate_paths` would
reject as non-absolute. `Deserialize` validates, `Default` does not. See §5.3.

**1.12 Positive note, for contrast.** The wire limits in
`shepr-protocol/src/limits.rs` are exemplary: every consumer across
`shepr-client`, `shepr-server` and `shepr-termio` reads the constant, there are
no magic `2 * 1024 * 1024` or `512 * 1024` restatements anywhere, and derived
values (`MAX_SURFACE_CELLS`, `MAX_TERMINAL_FRAME_BYTES`) are computed from
their bases. This is the model the config crate's defaults do not follow, and
it is worth saying which half of the scope is already right.

---

## 2. Values nobody can find, change, or trust

**2.1 There is no answer to "what are the tunables of `shepr-config`".**
`lib.rs` holds four (`DEFAULT_SCROLLBACK_LIMIT_BYTES`,
`DEFAULT_MOUSE_SCROLL_LINES`, `DEFAULT_HEADLESS_COLS`,
`DEFAULT_HEADLESS_ROWS`). The rest are scattered wherever first needed:
`MAX_SESSION_NAME_LEN = 64` (`session_id.rs`), `MAX_SIDEBAR_ROWS = 16`,
`MAX_SIDEBAR_TOKENS_PER_ROW = 16`, `DEFAULT_SIDEBAR_ROW_GAP = 0`
(`sidebar.rs`), `MAX_TAB_BAR_RIGHT_ENTRIES = 16`,
`MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS = 31_536_000`,
`MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS = 3_600`,
`DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS = 5`,
`DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS = 2` (`tab_bar.rs`),
`KEY_BINDING_COUNT = 51` (`wire.rs`), plus the ~40 values buried in `Default`
impls in `model.rs`. Several of the caps (`31_536_000` = a year;
`MAX_SESSION_NAME_LEN`; the sidebar 16s) are documented nowhere a user would
look - not in `default.toml`, and this repo has no `docs/` or `reference/`
folder yet. **Enforce:** structurally only - one `limits.rs` in
`shepr-config` mirroring what `shepr-protocol` already does, and the 1.3 test
to surface the user-visible ones in `default.toml`. No lint can find a
constant that is merely in the wrong file.

**2.2 A bare `16` with a named twin.** `sidebar.rs::RawSidebarToken::parts`
has `if token.rules.len() > 16 { return Err("sidebar tokens may contain at
most 16 rules") }` - the number appears twice on adjacent lines, in a file that
already has two named `= 16` constants for neighbouring limits.
**Enforce:** a named const plus `{MAX}` interpolation in the message; then a
`clippy` arbitrary-literal rule is unnecessary because the drift is
impossible.

**2.3 No injection point for the clock or the id counter.**
`TerminalId::alloc()` reads `SystemTime::now()` and a `static AtomicU64`
directly. A test cannot pin either, so any test asserting on terminal ids must
accept whatever it gets. `tab_bar.rs` `Command` entries carry
`interval_seconds`/`timeout_seconds` whose *effects* are equally untestable
without waiting. **Enforce:** pass a clock/id source into the allocator (a
`TerminalIdSource` struct holding the counter would also remove the global
mutable state - see §7.5). A type change, so the compiler holds it.

**2.4 Config read at the moment of use rather than at startup - one real
instance.** Covered under Claim B: remote `ValidatedConfig` is validated during
snapshot decode, at attach time. Everything else in the crate is genuinely
front-loaded, including the strftime compile
(`parse_tab_bar_datetime_format`), the window-title template parse, and the
keybind parse - all done once into `ValidatedConfig`. Credit where due.

---

## 3. One channel, one implementation

**3.1 Neither crate writes to stdout or stderr.** Verified by grep: no
`println!`/`eprintln!`/`print!` in `shepr-protocol/src` or
`shepr-config/src`. Correct, and worth recording so a future reader does not
have to re-check.

**3.2 `shepr-protocol` logs exactly once, and the line is not actionable.**
`surface_reuse.rs::message`: `tracing::warn!(%error, "failed to size surface
reuse")`. It omits the boot id, both revisions and the surface dimensions -
everything an operator would need. **Enforce:** a test cannot check log
content usefully; this is a review-time fix.

**3.3 The same failure is handled two different ways in sibling modules.**
`surface_reuse::message` logs a `warn` and returns `None` when
`codec::encoded_len` fails. `surface_delta::message` wraps the identical
failure as `SurfaceDeltaError::Encoding` and returns `Err`. Two policies for
one class of event, chosen by which file the code landed in.
**Enforce:** one signature. Making both return `Result` (and letting the caller
decide to log-and-fall-back once) is a type-level fix.

**3.4 Nothing is logged where something significant happens.** The swallowed
config decode in `shepr-client/src/shell/endpoints.rs` (Claim B) is the clear
case: a remote endpoint's configuration becomes unreadable and the only trace
is a `None`. Also: `config check` classifies every diagnostic into a
`ConfigDiagnostic` variant and then nothing ever reads the variant (see §8.2),
so the classification never reaches any channel.

**3.5 Operator-facing text assembled at the site.** `ConfigDiagnostic`'s
`Display` is `f.write_str(self.message())` - the strings are built at ~40 call
sites with ad-hoc prefixes (`"config read error: {err}"`, `"config parse error:
{err}"`, `"config provenance error: {error}"`, `"config path error: {error}"`,
`"session selection error: {error}"`, `"application paths could not be
resolved"`, `"state directory error: {error}"`, ...). The prefix is the variant
name restated in prose, at every site, by hand. **Enforce:** move the prefix
into `Display` keyed on the variant; then the prefix exists once and the
variant becomes load-bearing at the same time, which also fixes §8.2.

---

## 4. Errors

**4.1 Swallowed:** `shepr-client/src/shell/endpoints.rs` `.ok()` on the
`ValidatedConfig` decode. Detailed under Claim B. This is the one live defect I
would fix first.

**4.2 Context shed.** Messages that reach an operator naming no subject:

- `"invalid endpoint configuration: unexpected end of input: needed 1 bytes, 0
  remaining"` - no endpoint, host or session (Claim B).
- `FramingError::SurfaceDecode(String)` and `CodecError::Message(String)` flow
  to the client with no pane, boot id or revision attached.
- `ConfigDiagnostic::Validation("configuration values could not be resolved")`
  in `model.rs::into_validated` - the final refusal on the launch path and it
  names nothing at all. It fires when `resolution.values` is `None` while
  `diagnostics` is empty, i.e. exactly the case where nothing else explained
  the failure.
- `resolve_paths_from_env`'s `Err(vec!["application paths could not be
  resolved".to_string()])` - the same shape, on the same path.

**Enforce:** structured error types instead of `String` payloads (the
`ConfigDiagnostic(String)` design is what allows context to be dropped
silently). A test can pin the specific messages; a type makes omission
impossible.

**4.3 Aborting the process on operator-controlled input.** Only at the
intended boundary: `main.rs` `std::process::exit(1)` after printing
diagnostics. No `panic!`/`unwrap()`/`expect()` in production code in either
crate - the only `expect`s are in `#[cfg(test)]` blocks and in
`#[cfg(any(test, feature = "test-support"))]` helpers. Two `#[cfg]`-gated
exceptions worth naming because the `test-support` feature is a real build
configuration:

- `validated.rs::test_from_config` has three `expect`s ("test config document
  is valid", "test config values are serializable", "test config is valid").
- `keybinds.rs::ActionKeybinds::prefix`/`direct` `expect("prefix binding should
  parse")`.
- `sidebar/rules.rs:158` `unreachable!("validated condition count")` - this one
  is in production code. It is genuinely unreachable (the `count != 1` check
  above it guarantees `gt` or `lt` is `Some`), but the guarantee is a counted
  boolean array five lines up rather than a type. **Enforce:** build the
  `Condition` in the same match that counts, so the impossible case is not
  representable.

---

## 5. Tests that prove nothing

**5.1 A validation test run under a format production never uses.**
`geometry.rs::received_geometry_rejects_pixel_mouse_without_cells` exercises
`TerminalGeometry`'s `try_from` guard through `serde_json::from_value`. The
type crosses the shepr codec in production. The guard is format-independent so
the test is not *wrong*, but it proves the rejection for JSON only, and
`serde_json` is a dev-dependency of `shepr-protocol` used for nothing else
except one line in `wire_tests.rs`. **Enforce:** run it through
`codec::from_slice_exact` and drop the dev-dependency (`brokkr.toml`'s
dependency rules could then keep it out).

**5.2 A test that covers only the default value of the thing it is protecting.**
The `codec::to_vec(&ValidatedConfig::test_default())` sites (Claim A). Reads as
coverage of "config is codec-safe"; covers the empty case of every interesting
field.

**5.3 A test fixture that violates the invariant its own type enforces.**
`ServerAddress`'s `#[cfg(any(test, feature = "test-support"))] Default` builds
`api_socket: "shepr.sock"`, `client_socket: "shepr-client.sock"` - relative
paths, which `validate_paths` rejects and which `Deserialize` would refuse. So
every test using `ServerAddress::default()` is asserting against a value
production cannot produce. **Enforce:** route the fixture through
`ServerAddress::resolve_paths` with an absolute root (as
`AppPaths::test_with_context` already does), or have `Default` call
`validate_paths().expect(...)` so the fixture cannot drift.

**5.4 A test helper that silently produces nonsense.**
`PublicTabId`/`PublicPaneId`'s `#[cfg(any(test, feature = "test-support"))]
From<&str>` does `value.parse().unwrap_or_else(|_| Self { workspace_id: "",
number: 0, encoded: value })`. A typo'd id in a test becomes a valid-looking
`PublicPaneId` with workspace `""` and number 0 rather than a failure, and
equality against `&str` still passes because it compares `encoded`. Any test
built on a malformed literal quietly tests nothing. **Enforce:** make the
helper panic on a parse failure. One-line change; the compiler cannot express
it.

**5.5 A constant-pattern match that would silently become a wildcard on
rename.** `tab_bar.rs::tab_bar_entries_parse_with_command_defaults`:

```rust
assert!(matches!(&parsed.entries[4], TabBarRightEntryConfig::Command {
    interval_seconds: DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS,
    timeout_seconds: DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS, .. }));
```

This works today because both names resolve to `const`s, so they are constant
patterns. Rename either to lower case (or move it out of scope) and each
becomes a fresh binding that matches anything - the assertion then passes for
every value and rustc warns about nothing useful. **Enforce:**
`assert_eq!` on the destructured fields instead, which cannot degrade.

**5.6 Environment-dependent tests: none found in scope, and the isolation is
real.** Tests that touch paths use `shepr_test_support::IsolatedEnv` /
`ScratchDir` as `AGENTS.md` requires; `AppPaths::default()` deliberately points
at `/nonexistent/shepr-test-config` so a slip fails loudly.
`wire_tests.rs::framing_over_unix_socketpair` uses an in-process socket pair,
not the network. No wall-clock or installed-utility dependencies. See §6.1 for
the one hole in the isolation scheme.

---

## 6. Guards and claims that have stopped holding

**6.1 `app_dir_name()`'s comment is false outside its own crate.**
`shepr-config/src/io.rs`:

```rust
// Unit tests get a directory name of their own in every profile. ...
if cfg!(test) { "shepr-test" } else if cfg!(debug_assertions) { "shepr-dev" } else { "shepr" }
```

`cfg!(test)` is per-crate. It is true only while compiling `shepr-config`'s own
unit tests. A test in `shepr-server`, `shepr-client` or `shepr-remote` that
reaches `AppPaths::resolve()` compiles `shepr-config` as a normal dependency,
and `brokkr test` builds release by default, so `debug_assertions` is off too:
the directory name is **`shepr`**, the real `~/.config/shepr`. The comment says
this slip is kept out of both the release and dev directory; for the majority of
the workspace's tests it is not. What actually prevents damage is
`IsolatedEnv` pointing `HOME`/`XDG_*` at scratch - discipline, not the
`cfg!`. **Enforce:** make the guard positive rather than negative - have
`shepr-test-support` set a variable (e.g. `SHEPR_TEST_DIR_NAME`) that
`app_dir_name()` honours, so isolation is something a test opts *into* and the
name is wrong loudly rather than silently. Failing that, at minimum reword the
comment; today it is a claim the build does not check. This is the most
dangerous false claim I found.

**6.2 The empty/relative rule for an environment path is invented three times
in one function.** In `resolve_paths_from_env` / `platform_xdg_dir` /
`socket_path_override`:

| variable | empty | relative | reported? |
|---|---|---|---|
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | treated as unset, falls back to `HOME` | same | **silently**, and provenance says `Default` |
| `XDG_RUNTIME_DIR` | hard error | hard error | yes |
| `SHEPR_SOCKET_PATH`, `SHEPR_CLIENT_SOCKET_PATH` | diagnostic | diagnostic | yes |
| `SHEPR_CONFIG_PATH` | diagnostic | **joined to the cwd** | n/a |
| `SHEPR_SESSION` | parsed (empty name is an error) | n/a | yes |

Five variables, four rules, and the one that fails open is also the one that
silently sends config reads somewhere other than where the user pointed them.
The `AGENTS.md` sentence "Any config problem fails the launch; no fallbacks" is
false here: `XDG_CONFIG_HOME=relative/path` is a config problem that produces a
silent fallback. **Enforce:** one `env_path(variable, policy)` helper with an
explicit policy enum, used for all of them; a test table over the five
variables then pins the matrix.

**6.3 Guards keyed on a string name that go quiet when the name changes.**

- `sidebar.rs::RawSidebarToken::parts`:
  `matches!(token.token.as_str(), "state_icon" | "git_status")` rejects styling
  rules on non-text tokens **before** the string is parsed into an enum. Rename
  a token and rules silently become accepted on a token whose value is a glyph.
  It also conflates the two namespaces: `git_status` does not exist as an agent
  token and `state_icon` exists in both. **Enforce:** move the check after
  parsing and match on the enum; exhaustiveness then holds it.
- `sidebar.rs::parse_sidebar_token`'s built-in tables (nine agent entries, five
  space entries) are slices, not matches. Add an `AgentSidebarToken` variant
  and `agent_token_name` fails to compile (good) but the parse table does not -
  the new token silently becomes unparseable, reported as "unknown sidebar
  token". **Enforce:** one table used by both directions, or a
  `const fn all()`-style enumeration with an exhaustive match.
- `theme_config.rs::CustomThemeColors::parse` - the `color!()` list is tied to
  `ParsedThemeColors` by a struct literal, so a field added to *both* is caught,
  but a field added only to `CustomThemeColors` compiles and is silently
  ignored. **Enforce:** generate both structs from one field list.
- `validated.rs::ConfigProvenance::from_config` enumerates config keys by
  `serde_json::to_value(config)` and walking the tree. Any
  `skip_serializing_if` in a config type therefore **drops provenance keys
  silently** - and `RawRule` has ten of them today, so sidebar-rule fields that
  are `None` never appear in `config check`'s enumeration. The completeness of
  the provenance surface depends on an attribute nobody audits. **Enforce:**
  the §Claim A text rule, extended to `shepr-config`'s TOML-facing types with
  the `RawRule` exemption spelled out and justified in a comment.

**6.4 Claims asserted in comments that nothing checks.**

- `ids.rs`: "Opaque identity for a server-owned terminal ... callers must not
  derive it from a pane id or layout position." `TerminalId` has a public
  `From<String>` and a non-`cfg`-gated `pub fn test_new`, so deriving one from
  anything is a one-liner. Checkable: remove `From<String>`, gate `test_new`
  behind `cfg(any(test, feature = "test-support"))`, and the claim becomes
  structural. (`test_new` being reachable from production is also §7.6.)
- `limits.rs` on `MAX_TERMINAL_FRAME_BYTES`: "one byte for the `Terminal`
  variant index and three for the byte-length varint". The variant index is a
  varint, so it is one byte only while `ServerMessage` has fewer than 128
  variants - true today (20), and nothing notices if it stops being.
  Checkable with a test that encodes a maximal `TerminalFrame` and asserts it
  fits.
- `preamble.rs::encode`: "`BUILD_ID` is 16 hex digits (`build.rs`); anything
  shorter is padded with zeros and anything longer truncated". Checked - by
  `lib.rs::version_carries_the_build_fingerprint`, which asserts
  `BUILD_ID.len() == 16` and all-hex. Good example of the right pattern.
- `message.rs` on `PaneSurfacePatch`: "Keep it skipped so framing it fails" -
  checked by `wire_tests::internal_surface_patch_cannot_be_framed`. Also good.

**6.5 Documentation restating a list the code owns.** `default.toml` restates
the theme list (diverged, §1.4), the keybinding list (checked, §1.3), the
`cjk_ime_agents` accepted-name list (22 agent names, hand-written, generated by
`ConfigAgent::all()` - unchecked), the sidebar built-in token lists (unchecked),
and the `right_click_passthrough_modifier` alias list (unchecked).
**Enforce:** one test per list, all in the shape `main.rs` already uses.

**6.6 `default.toml` ships one *active* setting.** Every line in the file is
commented out except `pane_history = false` under `[experimental]`. Since the
file is only printed (`shepr --default-config`), never parsed, the effect is
that a user who redirects it to `~/.config/shepr/config.toml` gets a config
that explicitly pins one experimental flag while leaving everything else to
defaults. Almost certainly an editing slip. **Enforce:** a test asserting every
non-blank, non-`[section]` line in `DEFAULT_CONFIG` starts with `#`.

---

## 7. Policy invented per call site

**7.1 Overflow policy per call site, on the same counter type.**
`revision.rs`'s `counter!` macro gives every counter both `next()` (saturating)
and `checked_next()` (returns `None`), plus saturating `Add`/`AddAssign`.
`surface_reuse::Baseline::accepts` relies on `checked_next()`; other callers use
`next()`. A saturated `SurfaceRevision` at `u64::MAX` would silently stop
advancing and every subsequent delta would be rejected as a baseline mismatch,
forever, with nothing logged. Not reachable in practice, but the type offers two
answers and lets the call site pick. **Enforce:** keep one. If saturation is
never acceptable, delete `next()` and make `checked_next()` the only way
forward; the compiler then holds it.

**7.2 Clamp-or-reject decided per entry point.** `geometry.rs`
`ProtocolCellSize::from_host` clamps, `from_wire` rejects (both documented, and
the reasoning is sound). `input.rs::ClientSurfaceSize::clamped` clamps, while
`limits.rs::surface_grid_size` rejects - and the two express the same cell
budget by different arithmetic (division vs multiplication). That pair *is*
tied by `wire_tests::client_surface_clamp_fits_server_geometry_limit`, which is
the right way to hold it. Noting it as the pattern the other pairs in this
report lack.

**7.3 The "validate on deserialize via a shadow struct" idiom is hand-written
four times**: `shepr-protocol/src/geometry.rs` (`ReceivedTerminalGeometry`),
`shepr-config/src/address.rs` (`ServerAddress`'s `Wire`),
`shepr-config/src/io.rs` (`AppPaths`'s `Wire`),
`shepr-config/src/validated.rs` (`ValidatedConfig`'s `Wire`). Each repeats its
type's full field list and then a field-by-field move. Adding a field to the
outer type *is* a compile error in the struct literal, so the copies cannot
silently drift - credit where due - but it is four independent implementations
of one rule, and the `AppPaths` copy is 10 fields long. **Enforce:** a small
derive or macro (`#[validated_deserialize(validate = "validate_resolved")]`),
after which the rule exists once.

**7.4 Wire construction duplicated between sibling modules.**
`surface_reuse::message` and `surface_delta::message` both check
`Baseline::accepts` with the same five arguments and then build the same
six-field `SurfaceUpdate`, differing only in whether `spans` is empty.
**Enforce:** one constructor taking the spans; a type-level fix.

**7.5 Shared mutable state.** `ids.rs`'s `static NEXT_TERMINAL_ID:
AtomicU64` is process-global, `Ordering::Relaxed`, and combined with
`SystemTime::now()` in the id string. Uniqueness therefore rests on the clock
being monotonic across the process *or* the counter never wrapping - and
`duration_since(UNIX_EPOCH)` falls back to `.unwrap_or(0)` on a
before-epoch clock, at which point ids become `term_<counter>` only. No lock is
held across a suspension anywhere in either crate (checked).
**Enforce:** own the counter in a struct passed to callers (same fix as §2.3).

**7.6 Test-only shortcuts production code can reach.**
`TerminalId::test_new` is `pub` and ungated. `WorkspaceId::new`,
`BootId::from(&str)` and `RequestId::from(&str)` let any caller mint an
identity that is supposed to come from one place. `Config` is exported publicly
only under `feature = "test-support"` - which is the right pattern and shows the
crate knows how to do this. **Enforce:** `#[cfg(any(test, feature =
"test-support"))]` on the constructors; the compiler then holds it.

**7.7 Unbounded growth.** Nothing in scope grows without bound: every
collection on the wire is capped (`MAX_COLLECTION_ITEMS` by default, tighter
per field where declared), depth is bounded, and `FramePayloadBuffer` counts
excess bytes without retaining them. `ConfigProvenance::values` is the one
collection with no declared cap - it is one entry per config leaf, so it is
bounded by the config schema, but it is also shipped on the wire inside
`resolved_config` on every first snapshot per connection, and it exists purely
so `config check` and the preferences overlay can say "where did this value
come from". Worth knowing it is there.

**7.8 Secrets in diagnostics.** `ConfigProvenance` stringifies *every* config
value into `ConfigValueOrigin.value` and ships it to every attached client,
including `terminal.default_shell`, `terminal.new_cwd` (an absolute path),
`ui.tab_bar_right` command lines, and `AppPaths`'s full home/state/runtime
paths. Nothing here is a credential today, but `tab_bar_right` `Command`
entries are arbitrary shell command strings under user control and they land in
a structure designed to be displayed. No finding beyond noting the exposure
surface; no fix proposed.

---

## 8. Code that is no longer load-bearing

**8.1 Nine `serde` attribute pairs that enforce nothing.** In
`projection.rs` (7 fields) and adjacent types, `Vec` fields carry

```rust
#[serde(serialize_with = "codec::serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }, _, _>")]
```

but the codec's own `serialize_seq`/`read_collection_len` already enforce
`MAX_COLLECTION_ITEMS` on every sequence (`codec.rs`
`put_collection_len`/`read_collection_len`). These annotations state the
default cap, contradicting `serialize_bounded_vec`'s own doc comment ("lets
wire fields state a *tighter* rule"). Eighteen lines of attribute doing
nothing - and worse, they teach the reader that a `Vec` field *without* the
annotation is unbounded. What tells me they are dead: the caps are numerically
identical and the codec applies its cap unconditionally in both directions.
**Enforce:** delete them; the remaining annotations (`MAX_SURFACE_PANES`,
`MAX_SURFACE_SPLITS`, `MAX_SURFACE_HYPERLINKS`, `MAX_SURFACE_CELLS`,
`MAX_SURFACE_PATCH_SPANS`, `MAX_SURFACE_SPLIT_PATH`, `MAX_SURFACE_DIMENSION`)
are all genuinely tighter. A text rule could ban
`serialize_bounded_vec::<{ codec::MAX_COLLECTION_ITEMS }` specifically.

**8.2 `ConfigDiagnostic`'s six variants are never discriminated.** `Read`,
`Parse`, `Provenance`, `Unknown`, `Validation`, `Path` - every consumer in the
workspace calls `.message()` or `Display`. The only construction outside the
crate is `src/cli.rs:513` mapping into `ConfigDiagnostic::Path`. What tells me
it is dead: no `match` on the enum exists anywhere except `message()` itself,
which collapses all six arms into one. The classification is paid for at ~40
construction sites and read nowhere. **Enforce:** either make it load-bearing
(move the message prefix into `Display` per variant, §3.5) or collapse it to a
newtype. A `#[non_exhaustive]`-style lint cannot catch this; it is a review
finding.

**8.3 `ConfigSource::CliFlag(String)` has had one value since it was added.**
The only construction is `ConfigSource::CliFlag("--session".to_owned())` in
`io.rs::resolve_with_session`. **Enforce:** nothing mechanical. Either a
unit variant `CliFlag` documented as the session flag, or leave it and accept
the generality.

**8.4 `scroll.rs` is in the wrong crate and is not wire code.**
`shepr-protocol/src/scroll.rs` contains `ScrollMetrics` (which derives no
`Serialize`/`Deserialize` at all - it never crosses the wire) plus scrollbar
*rendering*: `render_scrollbar_buffer` writes into a `ratatui::buffer::Buffer`
and hardcodes the track glyph `"▕"` while taking `thumb_symbol` as a
parameter - one of two glyphs injected, the other not. Hit-testing
(`scrollbar_thumb_grab_offset`, `scrollbar_offset_from_row`,
`scrollbar_offset_from_drag_row`) is client interaction logic. What tells me
it does not belong: no type in the file is serialisable, and `ratatui`'s
`Buffer`/`Rect` are a presentation dependency the wire-protocol crate does not
otherwise need for drawing. **Enforce:** move it to `shepr-termio` or
`shepr-client` and then tighten `shepr-protocol`'s allowlist in `brokkr.toml`.
The dependency rule that follows the move is the mechanical part; `ratatui`
would still be needed for `ratatui_conversion.rs`, so the enforcement is the
move plus a text/structural rule about `Buffer` use, not the allowlist alone.

**8.5 `read_message`'s `max_frame_size` parameter has had one value at every
production call site.** Roughly fifteen call sites across `shepr-client` and
`shepr-server` all pass `shepr_protocol::MAX_FRAME_SIZE`; only
`wire_tests::oversized_input_rejected_custom_max` passes anything else. A
parameter nobody varies is both dead weight and a hazard - a call site can
weaken the cap and nothing notices. **Enforce:** drop the parameter from the
public function and keep a `#[cfg(any(test, feature = "test-support"))]`
variant for the one test. The signature then makes the bad spelling
unrepresentable.

**8.6 `PublicIdParseError` is exported but used only inside
`shepr-protocol`.** No consumer outside the crate names it (grep: zero hits
outside `ids.rs`). It reaches `pub` via `pub use ids::*`-style re-exports.
Minor; listed because it is one more symbol read as API.

**8.7 Two 120-line id types that are byte-for-byte the same shape.**
`PublicTabId` and `PublicPaneId` differ only in the discriminator character
(`'t'` vs `'p'`) - and that character is spelled twice per type (once in
`format!("{}:t{}", ...)`, once in `from_str`'s `parse_public_child_id(value,
't')`), with no link between the two. Everything else - `as_str`,
`workspace_id`, `number`, `Display`, `FromStr`, `Serialize`, `Deserialize`,
`Deref`, the two test-only `From`s, four `PartialEq` impls - is duplicated
verbatim. **Enforce:** one generic `PublicChildId<const KIND: char>` or a
macro; the discriminator then exists once and the duplication is structurally
impossible.

---

## Lateral findings (outside the eight questions)

**L1. `PublicTabId`/`PublicPaneId` allocate a `String` per id per
serialisation.** Both `Serialize` impls do
`serializer.serialize_str(&self.to_string())` when `self.as_str()` is right
there and already holds the encoded form. Every `ClientShellSnapshot` carries
one `PublicTabId` and one `PublicPaneId` per pane, tab and agent, and snapshots
fan out per client - `AGENTS.md`'s "hot paths multiply" applies directly. One
character of fix each.

**L2. The delta path traverses the message three times to size it.**
`surface_delta::message` calls `encoded_len(full)`, then `encoded_size(&span)`
per changed span, then `encoded_size(&message)` on the finished update. On the
per-frame fanout path. `encode_frame`'s doc comment explicitly avoids exactly
this pattern for the full-surface path ("Calling `encoded_len` first would
traverse every field again on the client fanout path") - the delta path does
what that comment says not to do. Worth measuring before changing, but the
inconsistency is deliberate-looking and probably is not.

**L3. `ServerMessage` is both the wire enum and an internal event channel.**
`PaneSurfacePatch` is `#[serde(skip)]` with the comment "Keep it skipped so
framing it fails". The guard works (and is tested), but a local-only variant
inside the wire enum means every reader of `ServerMessage` must know which
variants can actually travel. Splitting into `ServerMessage` (wire) and a local
event enum would make the distinction a type rather than an attribute plus a
comment plus a test.

**L4. `into_config`'s error type is `Result<Config, String>`** for a case that
cannot occur (§1.2) - a stringly-typed error on the config decode path, which
is where §4.2's context loss comes from.

---

## Summary of what the build could enforce but does not

Ordered by what a single mechanism buys, not by importance:

1. A text rule in `brokkr.toml` forbidding `skip_serializing_if` / `flatten` /
   `untagged` / `serde(tag` under `crates/shepr-protocol/src`. Closes Claim A's
   structural half.
2. One round-trip test of a *maximal* `ValidatedConfig` through the codec.
   Closes Claim A's data half and §5.2.
3. Generalising `main.rs`'s keybinding-documentation test to every leaf of
   `Config::default()`. Closes §1.3, §1.4, §6.5 and would have caught the
   theme-list divergence.
4. One declarative action table generating the eight keybinding sites. Closes
   §1.1 and §1.2. This is the rewrite I would actually recommend.
5. A `WireSafe` marker trait bounding `WireConfig`'s fields. Makes the
   twelve-reused/four-mirrored split legible and compiler-checked.
6. Dropping `read_message`'s `max_frame_size` parameter. §8.5.
7. `#[cfg]`-gating `TerminalId::test_new` and removing `From<String>`. Turns
   the "opaque identity" comment into a guarantee. §6.4, §7.6.
8. A positive test-directory signal replacing `cfg!(test)` in
   `app_dir_name()`. §6.1 - the most dangerous false claim in scope.

Not mechanically enforceable, and listed so nobody looks for a lint: §1.7
(empty-string meaning), §2.1 (constants in the wrong file), §3.2 (log content),
§8.2 (`ConfigDiagnostic`'s unread classification), §8.4's glyph inconsistency.
