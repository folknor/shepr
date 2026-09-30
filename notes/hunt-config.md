# Defect hunt: shepr-config

Scope: `crates/shepr-config`, and the places its validated values are used
(client shell, server `AppSettings`, the handshake welcome). Findings are
ordered by impact. Each one names the claim it breaks.

---

## 1. The client applies only the keymap from the endpoint config it receives; everything else in the welcome is decoded, validated again, then thrown away

**Claim broken.** AGENTS.md: "The server's config crosses hosts, and the client
rebuilds its runtime values from it ... a handoff applies the destination
endpoint's config at the presentation transition." The `EndpointServerWelcome`
doc in `crates/shepr-protocol/src/endpoint.rs` says the same.

**What the code does.** `ClientShellState::apply_active_snapshot`
(`crates/shepr-client/src/shell/state.rs`) keeps the received
`Arc<ValidatedConfig>` in `active_resolved_config`, and its only effect is
`ClientShellConfig::apply_endpoint_config`
(`crates/shepr-client/src/shell/presentation/config.rs`), which copies
`live_keybinds()` and nothing more. Only `same_keybinding_resolution` ever reads
the received config. Everything else the client renders or acts on comes from
the client's own `config.toml` read at launch (`run_launched_client` ->
`ClientShellConfig::from_validated_config(config)`, `ClientSettings::resolve`):
palette, sidebar rows, sidebar bounds and width, agent panel sort, status
indicators, mouse and copy settings, prompts.

**Consequences.**
- Two configs apply to one screen. The server renders pane borders and chrome
  into wire cells with *its* palette (`AppSettings::palette`, from the server's
  config in `crates/shepr-server/src/app/state.rs`). The client draws the sidebar
  with *its* palette. A machine with another `theme.name` or `ui.accent` gets
  remote-themed pane borders next to a locally themed sidebar. The same holds
  for the local endpoint when `config.toml` was edited after the long-lived
  local server started: the client takes the new theme and sidebar and the
  server's old keymap. That is a partial reload, which "There is no reload"
  says does not exist.
- The keymap changes under the user during connect. The shell starts with the
  client's keymap (`from_validated_config`) and switches to the server's when
  the first snapshot arrives.
- A welcome can fail on sections the client never uses. The welcome decode runs
  `ValidatedConfig::deserialize` over the whole config, remote `AppPaths`,
  machines and provenance included. Any check there that fails makes the
  endpoint unusable ("fails that handshake and shows as that endpoint's
  Attention diagnostic") over data the client would have discarded.
- Every client connection is sent the host's full config: every `[[machines]]`
  ssh target, every resolved path, and a stringified copy of every leaf (see
  finding 7). No client needs any of it.

**Recommended fix (structural).** Decide which side owns each setting and put
that in the types. Two coherent options:
- (a) The endpoint owns presentation. The client rebuilds `ClientShellConfig`
  (palette, sidebar, keymap and the rest) from the endpoint config at every
  presentation transition, and keeps only host-terminal settings local
  (`host_cursor`, mouse capture, OSC 52).
- (b) The client owns presentation. The welcome carries a small typed
  `EndpointPresentation`: the keymap, and anything the server needs from the
  client, such as the git demand in finding 2. It stops carrying
  `ValidatedConfig`.

Either way, drop `ValidatedConfig: Serialize/Deserialize`, `WireConfig`, and
the "rebuild everything with received-value rules" code path (`CwdCheck::Received`,
`ShellCheck::Received`, `AppPaths`/`ServerAddress` wire validation). Nothing in
the client consumes them. That removes most of `wire.rs` and the next two
findings along with it.

---

## 2. The server decides which Git data to compute from its own `ui.sidebar.spaces`; the client renders from the client's

**Claim broken.** The sidebar config documents that the `branch` and
`git_status` tokens show branch and ahead/behind. AGENTS.md lists "Git status in
the sidebar (branch, ahead/behind)" as kept.

**What the code does.** `App::git_refresh_demand`
(`crates/shepr-server/src/app/git_refresh.rs`) scans
`self.state.settings.sidebar_spaces`. That is the server host's
`ui.sidebar.spaces.rows` (`AppSettings::from_config`). It sets
`demand.branch` / `demand.ahead_behind` only when those tokens appear there.
The client lays out and renders space rows from `ClientShellConfig.spaces`,
which comes from the client's own config.

**Failure.** Suppose a machine's `config.toml` sets
`[ui.sidebar.spaces] rows = [["state_icon","workspace"]]` and the client's
config keeps the default rows (`branch`, `git_status`). The remote server never
computes ahead/behind or branch, so the client's `git_status`/`branch` tokens
stay empty for that machine. The reverse case, a remote asking for data the
client never draws, wastes git work. The local endpoint is affected too
whenever the server's read and the client's read of the file differ. This is
the concrete case of finding 1.

**Fix.** Make the demand the presenting client's (send it in the hello or as an
endpoint command, and union it across attached clients), or always compute
both. The server should not read `ui.sidebar` at all.

---

## 3. Received configs skip every structural check that lives in a serde `Deserialize` impl, so the wire check is weaker than the launch check

**Claim broken.** AGENTS.md: "the client validates it again when it decodes the
welcome, with the checks that only mean something on the sending host skipped
(the new-pane cwd exists, the shell resolves)". Also the comment on
`impl Eq for SidebarTokenRule` (`crates/shepr-config/src/sidebar/rules.rs`):
"Deserialization rejects non-finite thresholds, so equality is reflexive."

**What the code does.** At launch these checks run only inside the TOML-facing
`Deserialize` impls:
- `deserialize_sidebar_rows`: the `MAX_SIDEBAR_ROWS` and
  `MAX_SIDEBAR_TOKENS_PER_ROW` caps.
- `deserialize_rows_by_agent`: canonical agent ids as keys, plus the row caps
  per agent.
- `RawSidebarToken::parts`: the `MAX_SIDEBAR_RULES` cap.
- `AgentSidebarToken`/`SpaceSidebarToken::deserialize`: `allows_rules` (no rules
  on `state_icon`/`git_status`).
- `TryFrom<RawRule>`: exactly one condition, finite `gt`/`lt`, and no
  `ignore_case` on numeric conditions.
- `deserialize_cjk_ime_agents`: dedup.

The wire path skips all of them. `WireAgentsSidebarConfig`,
`WireSpacesSidebarConfig`, `WireAgentSidebarToken`/`WireSpaceSidebarToken` and
`WireSidebarTokenRule` (`wire.rs`, `sidebar/rules.rs`) convert with infallible
`From` impls. `SidebarTokenRule::from_wire` accepts NaN/inf thresholds and
`ignore_case` on numeric rules. `WireAgentSidebarToken::Styled { token: Box<Self> }`
accepts `Styled` nested inside `Styled`, which TOML cannot produce and whose
`parts()` then returns a `Styled` as the "inner" token. `rows_by_agent` keys are
not checked. `ConfigResolution::parse` never runs any of these checks, so a
received config becomes a `ValidatedConfig` without them.

The pure checks on the shell are skipped too. `ShellCheck::Received` keeps
`terminal.default_shell` as sent, even empty, relative or unrecognized, though
`ValidatedTerminalConfig::default_shell` is documented as "Absolute, recognized
shell". Neither absoluteness nor the recognized-name check depends on the
receiving host.

**Why it matters today.** Mostly latent, because the client discards all of
this (finding 1). One effect is live: a received NaN threshold makes
`ValidatedConfig == itself` false, which contradicts the reflexive-`Eq` claim.

**Fix.** If `ValidatedConfig` keeps crossing the wire, move the structural
checks out of the serde impls into one validator over `Config` that
`ConfigResolution::parse` runs, so launch and receive share it. Make
`from_wire` fallible (`TryFrom`). Make the wire token shape non-recursive
(`Styled { token: Plain, .. }`). Better still, apply finding 1 (b) and delete
this path.

---

## 4. An out-of-range `ui.sidebar_width` is silently clamped instead of failing the launch

**Claim broken.** AGENTS.md: "Any config problem fails the launch; no
fallbacks."

**What the code does.** `ValidatedUiConfig::from_config`
(`crates/shepr-config/src/validated.rs`) stores
`bounds.clamp_width(config.sidebar_width)`, and there is no diagnostic for
`sidebar_width` outside `[sidebar_min_width, sidebar_max_width]`.
`sidebar_width = 80` with the default max of 36 launches with 36. The test
`validated_config_resolves_runtime_values_once` asserts this clamp (80 -> 30),
and `config_check_collects_all_semantic_diagnostics` includes
`sidebar_width = 80` without expecting a diagnostic for it. By contrast, an
inverted min/max is a diagnostic.

**Fix.** Report `ui.sidebar_width (N) must be between sidebar_min_width and
sidebar_max_width` and fail. The clamp in `SidebarBounds::clamp_width` stays for
runtime drags and remembered preferences.

---

## 5. `server.headless_cols` / `headless_rows` have no upper bound, though every client-supplied size is capped

**Claim broken.** The protocol bounds every grid: `MAX_SURFACE_DIMENSION = 4096`,
`MAX_SURFACE_CELLS = 1 << 22`, and `ClientSurfaceSize::clamped`. The server-side
comment in `limits.rs` presents these as the cap on what a pane grid may be.
The config value bypasses them.

**What the code does.** `ConfigResolution::parse` only requires non-zero
(`GridSize::new`). `AppSettings::headless_rect` feeds the raw `u16`s straight
into pane geometry (`app/creation.rs`, `app/mod.rs`, `app/state.rs`) whenever no
client is attached. `headless_cols = 65535, headless_rows = 65535` sizes every
PTY and `alacritty_terminal` grid at about 4.3G cells at server boot, with no
client in the loop to clamp it.

**Fix.** Validate against the same bounds the protocol uses (at most
`MAX_SURFACE_DIMENSION` each, product at most `MAX_SURFACE_CELLS`) and emit a
diagnostic.

---

## 6. Keybinding conflict detection compares exact combos, but matching is fuzzy, so two bindings can fire on one keypress without any diagnostic

**Claim broken.** Keybinding validation promises that conflicts fail the launch
(`keybinding_conflict_diagnostic`, "`\"x\"` is assigned to both A and B"), and
the dispatch assumes each key maps to at most one action.

**What the code does.** `BindingRegistry` (`crates/shepr-config/src/keybinds.rs`)
keys its maps on `normalize_key_combo`, which only folds Tab/BackTab. Matching
(`key_parts_match_combo`, `shifted_char_matches_expected`,
`legacy_shifted_ascii_letter_matches`, `IndexedKeybind::matched_index`) treats
several different combos as the same key:
- The default `help = "prefix+?"` (`('?', NONE)`) and a user binding
  `"prefix+shift+/"` (`('/', SHIFT)`): a kitty-protocol press of Shift+/
  (code `/`, SHIFT, shifted codepoint `?`) matches both. `"prefix+shift+?"`
  (`('?', SHIFT)`) also matches the same event.
- `"prefix+shift+1..9"`, which default.toml offers as an example, and
  `"prefix+!"`: a legacy `!` press is matched by the indexed legacy-shift path
  *and* by `'!'` directly.

The registry sees different keys, so validation passes, and which action runs
depends on dispatch order.

**Fix.** Canonicalize combos to one form before registering: fold shift+symbol
to the shifted symbol through the same table the matcher uses, and fold ASCII
uppercase to shift+lowercase. Either reject combos the matcher cannot tell
apart, or make the matcher exact. Better: one canonical key type shared by
parser, registry and matcher, so they cannot drift apart.

---

## 7. `ConfigProvenance.values` claims a use nobody has, and one of its values is stale

**Claim broken.** The doc on `ConfigProvenance` (`validated.rs`): "Every value
is stringified and shipped to each attached client for display."

**What the code does.** No client code displays `values()`. Its only consumers
are `key_is_configured` (whether a `keys.*` field is user-set) and
`same_keybinding_resolution`. The client reads provenance only through
`is_explicit` for three UI keys (`preferences.rs`). Every welcome still carries
a JSON-stringified copy of every config leaf, machine ssh targets and paths
included.

The copy is also wrong in one place. `ConfigProvenance::from_config` runs on
the loaded `Config` before `ValidatedConfig::from_loaded` rewrites
`config.terminal.default_shell` to the resolved path. The provenance value for
`terminal.default_shell` is therefore `""` while the config it describes holds
`/usr/bin/zsh`.

**Fix.** Replace `values` with what is actually asked: a typed set of
explicitly configured keys (a bitset over the keybinding table plus the four
`UiPreferenceKey` flags). That makes `same_keybinding_resolution` exact and
cheap, and removes the JSON round-trip (`serde_json::to_value(config)`,
`collect_paths`, the `RawRule` "optional fields remain present as null" coupling
in `SidebarTokenRule`'s serializer).

---

## 8. Remembered sidebar chrome is stored per local socket, not "per server" / "per endpoint"

**Claim broken.** default.toml, for `sidebar_width`,
`sidebar_start_collapsed` and `agent_panel_sort`: "While unset, mouse or key
changes ... are remembered per server." The `ClientChromePreferences` doc: "Sidebar
chrome the user changed by hand, remembered per endpoint across launches."

**What the code does.** `run_launched_client` calls `with_local_endpoint` once,
so `preferences_path` is one file named after the hashed *local* client socket
(`path_for_local_endpoint`). `persist_chrome_preferences` writes that one file
whichever endpoint is presented. Machines have no preferences file of their
own, and a width dragged while viewing a machine is saved as the local
endpoint's. Also, the "is it configured" test uses the client's config
provenance, not the endpoint's.

**Fix.** Either key preferences by `ClientEndpointId::storage_key()` and switch
files at the presentation transition, or reword both docs to "remembered per
client".

---

## 9. Stale or wrong statements in config docs

- `UiConfig::host_cursor` (`model.rs`): "Host cursor policy. Default: auto." The
  default is `Native`, and `"auto"` is rejected (the
  `ui_host_cursor_defaults_to_native_and_parses_overrides` test asserts that).
- default.toml `[ui]`: "Sidebar width (auto-scaled based on workspace names,
  this sets the default)". Nothing auto-scales the width. It is the configured
  value clamped to the bounds, or the remembered one.
- default.toml `[terminal]`: "CWD policy for new panes and workspaces when no
  explicit --cwd is provided." No `--cwd` exists. AGENTS.md says the CLI has no
  pane or workspace commands.

---

## 10. Lateral findings (outside the crate's core, or low impact)

- **Both socket variables set: the client one is silently ignored.**
  `ServerAddress::resolve_paths` (`address.rs`) returns right after the API
  override and derives the client socket from it, dropping
  `SHEPR_CLIENT_SOCKET_PATH`. `resolve_paths_from_env` then records the client
  socket's provenance as `SHEPR_SOCKET_PATH`. AGENTS.md says socket variables
  "win over the runtime directory". A variable that is set and ignored is a
  quiet fallback, and "no fallbacks" argues for refusing the pair or honouring
  both.
- **`ui.accent` can be set and still ignored.** `ValidatedConfig::from_values(config, None, ..)`
  treats every value as default, so `resolve_palette` drops a non-empty
  `config.ui.accent` (it applies only when `is_explicit(Accent)`). The config
  keeps the accent while the palette ignores it. Only tests and
  `shepr-protocol`/`shepr-termio` test helpers call this today. The accent's
  effect should depend on the value, since `Some` already means "set in the
  file" after `deserialize_ui_accent`, not on a provenance side channel.
- **Pointless work on the failure path.** `default_loaded_config` (`io.rs`) runs
  a full `ConfigResolution::parse` against `Config::default()`, including the
  `SHELL`/`PATH` shell lookup and cwd checks, only to throw the result away
  because the load already has diagnostics. `LoadedConfig` could carry `None`
  there.
- **Different agent-name rules in two settings.** `cjk_ime_agents` accepts
  aliases case-insensitively (`parse_config_agent`), while `rows_by_agent` keys
  accept only exact canonical labels. Both are documented, but one
  `ConfigAgent` config parser would remove the difference.
- **Hard-coded list in the template.** The agent list in default.toml
  (`cjk_ime_agents` "Accepted: pi, claude, ...") is enumerated by hand and will
  go stale. The AGENTS.md documentation rule prefers wording that does not
  enumerate, or a test that derives the list from `ConfigAgent::all()`.
