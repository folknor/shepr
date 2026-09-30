# Defects: wire protocol, JSON API and configuration

Filed from the defect hunt over `crates/shepr-protocol`, `crates/shepr-api` and
`crates/shepr-config`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## WIRE-005 - Immediate endpoint refusals and the surface-set acknowledgement can overtake held replies

Scopes: protocol-api and server-serving-ui (both found it independently; the
serving hunter also cites `flush_endpoint_replies` "in the order the commands
ran", and offers dropping the ordering claim as the alternative if the client
matches replies only by request id).

**Claim broken.** `handle_client_shell_endpoint_request` doc
(`shepr-server/src/server/headless/endpoint_requests.rs`): "Commands from one
client run in arrival order on this loop, so a second command sent before the
first was answered simply runs after it, and the replies leave in the same
order."

Normal replies go to `endpoint_replies` and are flushed after the next render
(`flush_endpoint_replies`), which the render cadence can hold back across loop
iterations. `StaleBoot`, `SurfaceInactive` and the `ClientShellSurfaceSet`
acknowledgement go straight to the control lane with `send_to_client`, without
flushing the outbox. If command A is held and a later B is refused or is a
surface-set, B's reply reaches the client first.
`reject_endpoint_request_for_shutdown` flushes held replies first for exactly
this reason; the other immediate paths do not.

Limited impact today: the client matches responses by request id and serializes
its command lane. But an activation handoff that sends `surface.set` while a lane
command is in flight sees the replies reordered. Fix: route every reply through
the outbox (or flush before any immediate send).

## WIRE-006 - The client handles a shutdown notice in place of the welcome, but the server never sends one

Scope: protocol-api.

**Claim broken.** `do_handshake` (`shepr-client/src/handshake.rs`): "A server that
is going down answers the hello with its shutdown notice. That is a transient
condition to report as such, not a malformed welcome."

`handle_client_handshake` (`shepr-server/src/server/client_transport.rs`) either
returns without writing anything when `should_quit` is set (before the preamble,
or after reading the hello), or writes the welcome and then queues
`ServerShutdown` behind it (`send_shutdown_to_unregistered_client`, and the
`ClientShellConnected` arm of the stopping loop in `headless.rs`). No path writes
`ServerShutdown` in the welcome's place, so the branch is dead. A stopping server
gives a bare EOF during the preamble or welcome read (reported as
`UnexpectedEof`), or a welcome followed by a shutdown. Fix the doc, or have the
server answer a hello it will not serve with the shutdown notice.

## WIRE-007 - `ServerMessage::PaneSurfacePatch` is a client-only variant inside the wire enum, safe only while it stays last

Scope: protocol-api. Latent; no claim broken today.

serde_derive numbers a skipped variant differently on the two sides:
serialization uses each variant's declared position, deserialization numbers only
non-skipped variants (`deserialized_fields.iter().enumerate()` in
`serde_derive/src/de/identifier.rs`). `PaneSurfacePatch` is the last
`ServerMessage` variant, so nothing shifts; a variant added after it would encode
at index N+1 and decode as index N, a silent cross-variant decode within one
build. The comment ("Keep it skipped so framing it fails") does not warn about
this, in a type the codec module doc describes as positional ("enum: variant
index as varint").

Related dead code: `surface_reuse::Decoder::decode` has a second-stage
`ServerMessage::PaneSurfacePatch` arm that validates and applies a patch. No wire
message can produce that variant, and the decoder's own patch path returns early
with `return Ok(ServerMessage::PaneSurfacePatch(patch))`, so the arm is
unreachable.

**Recommendation (structural).** Split the enum: `ServerMessage` for exactly what
crosses the wire, and a client-side `DecodedServerMessage` (or a decoder-owned
enum) adding `PaneSurfacePatch`. That removes the skip, the unreachable arm, and
the `write_message` failure test that guards the skip.

## WIRE-011 - The client applies only the keymap from the endpoint config it receives

Scopes: config; also surfaced as lateral findings in client-endpoint and
client-shell.

**Claim broken.** AGENTS.md: "The server's config crosses hosts, and the client
rebuilds its runtime values from it ... a handoff applies the destination
endpoint's config at the presentation transition." The `EndpointServerWelcome`
doc in `crates/shepr-protocol/src/endpoint.rs` says the same, and
`ClientShellEndpoint::config`'s doc says a reconnect "to a server launched with
another config shows that config".

**What the code does.** `ClientShellState::apply_active_snapshot`
(`crates/shepr-client/src/shell/state.rs`) keeps the received
`Arc<ValidatedConfig>` in `active_resolved_config`; its only effect is
`ClientShellConfig::apply_endpoint_config`
(`crates/shepr-client/src/shell/presentation/config.rs`), which copies
`live_keybinds()` and nothing else, and only when `same_keybinding_resolution`
differs. Everything else the client renders or acts on comes from its own
`config.toml` read at launch (`run_launched_client` ->
`ClientShellConfig::from_validated_config(config)`, `ClientSettings::resolve`):
palette, sidebar rows, bounds and width, agent panel sort, status indicators,
mouse and copy settings, confirm prompts.

**Consequences.**

- Two configs apply to one screen. The server renders pane borders and chrome
  into wire cells with its palette (`AppSettings::palette`, from the server's
  config in `crates/shepr-server/src/app/state.rs`); the client draws the sidebar
  with its own. A machine with another `theme.name` or `ui.accent` gets
  remote-themed borders next to a locally themed sidebar. For the local endpoint,
  when `config.toml` was edited after the long-lived local server started, the
  client takes the new theme and sidebar and the server's old keymap: a partial
  reload, which "There is no reload" says does not exist.
- The keymap changes under the user during connect: the shell starts with the
  client's keymap and switches to the server's when the first snapshot arrives.
- A welcome can fail on sections the client never uses: the decode runs
  `ValidatedConfig::deserialize` over the whole config (remote `AppPaths`,
  machines, provenance), and any failing check makes the endpoint unusable over
  data the client would discard.
- Every client connection is sent the host's full config: every `[[machines]]`
  ssh target, every resolved path, and a stringified copy of every leaf
  (WIRE-017). No client needs any of it.

The client-endpoint and client-shell hunters each framed this as "either the doc
overclaims or the shell under-applies"; the client-shell hunter adds that what
"shows" covers is not stated, so if only the keymap is meant, the doc wording
should say so.

**Recommended fix (config hunter, structural).** Decide which side owns each
setting and put it in the types:

- (a) The endpoint owns presentation: the client rebuilds `ClientShellConfig`
  (palette, sidebar, keymap and the rest) from the endpoint config at every
  presentation transition, keeping only host-terminal settings local
  (`host_cursor`, mouse capture, OSC 52).
- (b) The client owns presentation: the welcome carries a small typed
  `EndpointPresentation` (the keymap, and what the server needs from the client,
  such as the git demand in WIRE-012) and stops carrying `ValidatedConfig`.

Either way, drop `ValidatedConfig: Serialize/Deserialize`, `WireConfig`, and the
received-value rebuild path (`CwdCheck::Received`, `ShellCheck::Received`,
`AppPaths`/`ServerAddress` wire validation), which removes most of `wire.rs` and
WIRE-012/WIRE-013 with it.

## WIRE-012 - The server picks which Git data to compute from its own `ui.sidebar.spaces`; the client renders from its own

Scope: config. The concrete case of WIRE-011.

**Claim broken.** The sidebar config documents that the `branch` and `git_status`
tokens show branch and ahead/behind; AGENTS.md keeps "Git status in the sidebar
(branch, ahead/behind)".

`App::git_refresh_demand` (`crates/shepr-server/src/app/git_refresh.rs`) scans
`self.state.settings.sidebar_spaces`, the server host's `ui.sidebar.spaces.rows`
(`AppSettings::from_config`), and sets `demand.branch` / `demand.ahead_behind`
only when those tokens appear there. The client lays out and renders space rows
from `ClientShellConfig.spaces`, from its own config.

If a machine's `config.toml` sets `[ui.sidebar.spaces] rows =
[["state_icon","workspace"]]` and the client keeps the default rows (`branch`,
`git_status`), the remote never computes branch or ahead/behind and those tokens
stay empty for that machine. The reverse wastes git work. The local endpoint is
affected whenever the server's and the client's reads of the file differ.

**Fix.** Make the demand the presenting client's (send it in the hello or as an
endpoint command, union across attached clients), or always compute both. The
server should not read `ui.sidebar` at all.

## WIRE-013 - Received configs skip every structural check that lives in a serde `Deserialize` impl

Scope: config.

**Claims broken.** AGENTS.md: "the client validates it again when it decodes the
welcome, with the checks that only mean something on the sending host skipped
(the new-pane cwd exists, the shell resolves)". The comment on
`impl Eq for SidebarTokenRule` (`crates/shepr-config/src/sidebar/rules.rs`):
"Deserialization rejects non-finite thresholds, so equality is reflexive."

At launch these checks run only inside the TOML-facing `Deserialize` impls:

- `deserialize_sidebar_rows`: the `MAX_SIDEBAR_ROWS` and
  `MAX_SIDEBAR_TOKENS_PER_ROW` caps.
- `deserialize_rows_by_agent`: canonical agent ids as keys, plus per-agent row
  caps.
- `RawSidebarToken::parts`: the `MAX_SIDEBAR_RULES` cap.
- `AgentSidebarToken`/`SpaceSidebarToken::deserialize`: `allows_rules` (no rules
  on `state_icon`/`git_status`).
- `TryFrom<RawRule>`: exactly one condition, finite `gt`/`lt`, no `ignore_case`
  on numeric conditions.
- `deserialize_cjk_ime_agents`: dedup.

The wire path skips them all. `WireAgentsSidebarConfig`,
`WireSpacesSidebarConfig`, `WireAgentSidebarToken`/`WireSpaceSidebarToken` and
`WireSidebarTokenRule` (`wire.rs`, `sidebar/rules.rs`) convert with infallible
`From` impls. `SidebarTokenRule::from_wire` accepts NaN/inf thresholds and
`ignore_case` on numeric rules. `WireAgentSidebarToken::Styled { token: Box<Self>
}` accepts `Styled` nested in `Styled`, which TOML cannot produce and whose
`parts()` then returns a `Styled` as the inner token. `rows_by_agent` keys are not
checked. `ConfigResolution::parse` runs none of these, so a received config
becomes a `ValidatedConfig` without them.

The pure shell checks are skipped too: `ShellCheck::Received` keeps
`terminal.default_shell` as sent (even empty, relative or unrecognized), though
`ValidatedTerminalConfig::default_shell` is documented as "Absolute, recognized
shell"; neither check depends on the receiving host.

Mostly latent because the client discards all of this (WIRE-011). One effect is
live: a received NaN threshold makes `ValidatedConfig == itself` false,
contradicting the reflexive-`Eq` claim.

**Fix.** If `ValidatedConfig` keeps crossing the wire, move the structural checks
out of the serde impls into one validator over `Config` that
`ConfigResolution::parse` runs for launch and receive alike; make `from_wire`
fallible (`TryFrom`); make the wire token shape non-recursive
(`Styled { token: Plain, .. }`). Better: WIRE-011 option (b) and delete the path.

## WIRE-017 - `ConfigProvenance.values` claims a use nobody has, and one of its values is stale

Scope: config.

**Claim broken.** The doc on `ConfigProvenance` (`validated.rs`): "Every value is
stringified and shipped to each attached client for display."

No client code displays `values()`. Its consumers are `key_is_configured`
(whether a `keys.*` field is user-set) and `same_keybinding_resolution`; the
client reads provenance only through `is_explicit` for three UI keys
(`preferences.rs`). Every welcome still carries a JSON-stringified copy of every
config leaf, machine ssh targets and paths included.

`ConfigProvenance::from_config` runs on the loaded `Config` before
`ValidatedConfig::from_loaded` rewrites `config.terminal.default_shell` to the
resolved path, so the provenance value for `terminal.default_shell` is `""` while
the config holds `/usr/bin/zsh`.

**Fix.** Replace `values` with what is asked: a typed set of explicitly configured
keys (a bitset over the keybinding table plus the four `UiPreferenceKey` flags).
That makes `same_keybinding_resolution` exact and cheap and removes the JSON
round-trip (`serde_json::to_value(config)`, `collect_paths`, the `RawRule`
"optional fields remain present as null" coupling in `SidebarTokenRule`'s
serializer).

## WIRE-018 - Remembered sidebar chrome is stored per local socket, not per server or per endpoint

Scope: config.

**Claim broken.** default.toml, for `sidebar_width`, `sidebar_start_collapsed`
and `agent_panel_sort`: "While unset, mouse or key changes ... are remembered per
server." The `ClientChromePreferences` doc: "Sidebar chrome the user changed by
hand, remembered per endpoint across launches."

`run_launched_client` calls `with_local_endpoint` once, so `preferences_path` is
one file named after the hashed local client socket (`path_for_local_endpoint`),
and `persist_chrome_preferences` writes that file whichever endpoint is
presented. Machines have no preferences file; a width dragged while viewing a
machine is saved as the local endpoint's. The "is it configured" test uses the
client's config provenance, not the endpoint's.

**Fix.** Key preferences by `ClientEndpointId::storage_key()` and switch files at
the presentation transition, or reword both docs to "remembered per client".

## WIRE-021 - `ui.accent` can be set and still ignored

Scope: config (lateral).

`ValidatedConfig::from_values(config, None, ..)` treats every value as default,
so `resolve_palette` drops a non-empty `config.ui.accent` (applied only when
`is_explicit(Accent)`). The config keeps the accent while the palette ignores it.
Only tests and `shepr-protocol`/`shepr-termio` test helpers call this today. The
accent's effect should depend on the value, since `Some` already means "set in
the file" after `deserialize_ui_accent`, not on a provenance side channel.

## WIRE-023 - The build identity may not be recomputed when `CARGO_PROFILE_*` environment overrides change

Scope: protocol-api. Unverified; the hunter proposes one experiment.

**Claim broken (if confirmed).** Root `build.rs` module doc: the identity covers
"every `CARGO_PROFILE_*` override set through the environment ... Any change to
an input yields a new identity".

The script declares `rerun-if-changed` for the tree and `rerun-if-env-changed`
only for `OPTIONAL_PROFILE_VARS`. Once any rerun directive is emitted, Cargo
reruns the script only on those triggers. The comment argues that the variables
Cargo derives itself live in a per-profile, per-target output directory, which
holds for `PROFILE` and `TARGET`; but a `CARGO_PROFILE_RELEASE_LTO` or
`..._CODEGEN_UNITS` set in the environment neither changes the output directory
nor appears in a rerun directive. The script hashes those variables when it runs
but may not run again when they change, leaving a stale `BUILD_ID` on a binary
built differently.

Experiment: build, change a `CARGO_PROFILE_RELEASE_*` environment variable,
build, compare `shepr --version`. If confirmed, emit `rerun-if-env-changed` for
each `CARGO_PROFILE_*` and `CARGO_CFG_*` name the script saw (it enumerates them
anyway). Overrides appearing for the first time remain unobservable, and the doc
should say so.

## WIRE-024 - Smaller protocol and API observations

Scope: protocol-api (structural and lateral).

- **Two spellings of a split path on the wire.** `LayoutSetSplitRatioParams.path`
  is `Vec<bool>`, while `PaneSurfaceSplit.path` (what the client read the split
  from) is `Vec<SplitBranch>`; the server translates in
  `handle_layout_set_split_ratio`. `SplitBranch` in both removes a translation
  and a convention ("`true` descends into the second branch") that lives only in
  a comment.
- **`#[serde(default)]` on `PaneInfo.scroll`** (`command.rs`) has no meaning on a
  positional type (every field is always present) and suggests missing fields are
  tolerated. Drop it.
- **Copy motion and search claim shell geometry.** `EndpointCommandTraits` for
  `pane.copy_motion` and `pane.copy_search` set `claims_shell_geometry: true`
  with `mutates_ui: false`, so `handle_client_shell_command` runs
  `claim_shell_workspace_geometry` / `resize_shell_workspaces_sized_for` for a
  read-only copy-mode step, which can resize PTYs as a side effect of moving a
  copy cursor. If intended (copy mode claims the workspace), say so in the trait
  doc; otherwise set it `false`.
- **The client-status doc refers to an older client.** `ClientStatusJson.server`
  says "`None` from a client that predates the field". Remote discovery does read
  other builds' JSON, so the case can exist, but it contradicts "no compatibility
  with ... any older shepr". The status JSON is part of the cross-build surface in
  PLAT-031 and should be treated as such, or not relied on across builds.
- **`request_value_until` returns `Ok(())` without sending** when the deadline has
  already passed (`send_stop_request`'s first check); the following wait then
  reports `TimedOut` for a stop never sent. Unreachable with a 15 s budget;
  returning a timeout error there would be clearer.

## WIRE-025 - Canonical key matching drops shifted-digit bindings on non-US layouts

Scope: config keybindings (regression from the canonical key identity).

Keybindings now share one `CanonicalKey` for conflict detection and matching,
which folds shift plus a symbol through a US-layout table. A kitty host on a
non-US layout reports a shifted digit by its own shifted codepoint: on a German
layout `shift+7` arrives as code `7`, Shift, shifted codepoint `/`. The key
canonicalizes to `/`, while the configured combo `shift+7` canonicalizes through
the US table to `&`, so the binding no longer matches; before, the exact code and
modifiers matched. No default binding uses a shifted digit, so only user config
reaches it. Canonicalize the key from its reported shifted codepoint and the
combo so both sides fold the same way, or match on the unshifted code plus
modifiers first and fall back to the canonical form.
