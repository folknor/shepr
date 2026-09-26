# Consolidations

Decisions answered in more than one place, from the 2026-09-26 design hunt. One
entry per question; every site answering it belongs to that entry.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## CON-004 - Is pixel/cell geometry known, and how is cell size clamped for the protocol?

Sites:
- "Known": `HostCellSize::is_known`, `Terminal::has_pixel_geometry`, `handler::in_band_size_report`, `handler::text_area_pixels_report`, each with its own `w > 0 && h > 0`.
- Clamp to `MAX_CELL_SIZE_PX` and "exact only if both sides ≤ MAX": `client/handshake.rs:109-113`, `client/shell_runtime.rs:64-70`, `client/mod.rs:550-554`. The direct-attach `ClientMessage::Resize` at `client/mod.rs:791-797` does not clamp at all (already disagrees).
- Wire: `TerminalHello`, `Resize`, `ClientShellResize`, `EndpointClientHello` each spell out `cols/rows/cell_width_px/cell_height_px/pixel_mouse` with 0 as the "unavailable" sentinel.

Proposed owner: a `CellPx` that can only be built non-zero, a
`ProtocolCellSize::from_host(...)` constructor in `protocol`, and one wire
`TerminalGeometry { size, cell_px: Option<CellPx>, pixel_mouse }`.

Reported by: terminal-core, client, protocol.

## CON-008 - Colour, appearance and default-colour types exist twice

Sites: `ghostty::ColorScheme::report` and
`terminal_theme::HostAppearance::color_scheme_report` both hard-code
`\x1b[?997;1n` / `\x1b[?997;2n` over two identical Light/Dark enums;
`ghostty::RgbColor` vs `terminal_theme::RgbColor`; `ghostty::DefaultColor` vs
`terminal_theme::DefaultColorKind`; `server/clients.rs:362-393`
`update_host_theme` maps protocol colour/appearance enums inline (a second
mapping if the client maps the same enums).

Proposed owner: the vt module owns one set of types; protocol conversions are
`From` impls next to the protocol types.

Reported by: terminal-core, server.

## CON-017 - When to probe processes

Sites: `should_probe_foreground_job`,
`should_skip_process_probe_for_lifecycle_authority` and
`sync_content_change_acquisition` each read the acquisition window and
foreground-group change.

Proposed owner: one scheduler state machine.

Reported by: pane-detection.

## CON-023 - Which terminal does this target name?

Sites: `resolve_terminal_target` matches `agent_name` or
`effective_agent_label()`; `resolve_agent_target` matches only `agent_name` and
gates pane ids on `is_agent_terminal`. The hunter notes the difference may be
intended.

Proposed owner: one resolver parameterised by a `TargetKind`.

Reported by: app-state.

## CON-025 - Does this event or API method need a render?

Sites:
- `handle_internal_event_with_render_impact` special-cases GitStatus and TabBar; `handle_internal_event_with_pane_updates` re-dispatches the same two variants and discards the answer.
- `api::request_changes_ui` (`src/api/mod.rs:22`), a hand-maintained allowlist separate from dispatch, consulted at `api/server.rs:360` (logging) and `headless.rs:2230` (render). Missing `ClientWindowTitleSet`/`ClientWindowTitleClear`, which headless special-cases to `true` before consulting it. Guarded only by spot-check tests (`app/mod.rs:699-704`, `app/api/panes.rs:2427`).
- `DeferredRender { None, Full }` and `RenderImpact { None, Full }` are identical enums backed by `render_pending: bool`; the run loop keeps `needs_render` / `needs_full_render` as two bools.

Proposed owner: handlers and event dispatch return an outcome carrying render
impact, over one `RenderDemand` lattice (None < Partial < Full) with join.

Reported by: app-state, server.

## CON-028 - AppState copies config field by field

Sites: `App::new` builds `PaneGeometry` from `config.ui.*` directly while
`AppState::pane_geometry_in` builds it from copied state fields; `AppState`
copies ~20 config fields one by one and `test_new` repeats defaults by hand,
which can drift from `Config::default()`. The client has the same shape:
`ClientLoopConfig` fields copied into `ClientState` (mouse_scroll_lines,
redraw_on_focus_gained, pixel_geometry_enabled, mouse_capture_active) plus
positional arguments.

Proposed owner: a `UiSettings` (server) / `ClientSettings` (client) built once from
`Config`, used by `test_new` too.

Reported by: app-state, client.

## CON-031 - Config diagnostics are classified by substring and selected per client

Sites: `is_keybinding_config_diagnostic` (looks for `"keybinding"` / `"keys."`,
excludes `"config parse error:"` / `"config read error:"`);
`config_diagnostic_summary` (`"using defaults"`, `"unknown config key "`);
`collect_diagnostics` post-edits with `.replace("using cyan", ...)`; the server
precomputes `server_config_diagnostic` and `_without_keybindings`.

Proposed owner: typed `ConfigDiagnostic { key, kind, message }`; the per-client
choice becomes a function of `(diagnostics, KeybindingSource)`.

Reported by: config-cli, server.

## CON-040 - Which role is this client process?

Sites: `is_remote_client_process()` called in `run_client_with_mode`,
`run_client_loop`, `handshake_read_timeout`; the same env var read in
`errors.rs` for the reattach message and in `handshake.rs:30-39` for
`ClientShellKeybindingSource` (BUG-021).

Proposed owner: a typed `ClientProcessRole` resolved once at startup.

Reported by: client.

## CON-045 - Config keys, defaults and validation

Progress (wave B): default config moved to `src/config/default.toml`;
keybinding validation cached at load. The fixer stopped at the model fork noted
in `config/model.rs`.

Decision (owner): boot resolves the whole config surface once - config file,
environment, CLI flags, paths - into an immutable, typed `ValidatedConfig`; at
runtime nothing is parsed, looked up by name or re-validated. Provenance is part
of the type, not a side structure: every resolved value records where it came
from (default, config file key, environment variable, CLI flag), so "did the
user set this" and "why is this the value" can be interrogated
deterministically, e.g. by `config check`. `KeysConfigOverlay`, the `user_fields`
string sets and the second TOML parse go. Runtime preferences (sidebar drag
etc.) stay separate mutable state. A single-agent wave of its own.

The same type goes on the wire: a server publishes its whole resolved and
validated config, provenance included, as a positional-codec wire type, instead
of the ad hoc keybinding profile TOML (`local_keybindings_profile_toml`,
`keybindings_from_profile_toml` and the profile publishing path go). A client
decides what of a remote config applies by interrogating that typed value; no
side channel re-serialises a subset of config.

Sites: `Config` structs (serde source); `KNOWN_TOP_LEVEL_CONFIG_KEYS` (hand
list; `serde_ignored` already reports unknown keys); `KeysConfig` and the
parallel `KeysConfigOverlay`; `DEFAULT_CONFIG` text in `main.rs` (a test checks
keybindings only); `Default` impls; `ui.user_fields: BTreeSet<String>` /
`keys.user_fields: BTreeSet<&'static str>` with string lookups
(`is_user_configured("sidebar_width")`), obtained by re-parsing the document;
validators re-run on every accessor call (`keybinds()` re-runs
`validated_keybinds()`); `validated_sidebar_bounds` at config time and
presumably again at clamp sites.

Proposed owner: a `ValidatedConfig` built once at load, `Option<T>` for
user-overridable fields, overlay generated or removed.

Reported by: config-cli.

## CON-046 - Does this process own the data dir?

Sites: the lock claimed lazily in `persist::io::load` and (per comments) in
`SessionWriter`; `load_history` does not claim it; global `HELD` list with
idempotent re-claim.

Proposed owner: a `DataDirLease` acquired once at server start, the only way to
get a writable session path.

Reported by: config-cli.

## CON-048 - Where is home?

Sites: `pathutil::home_dir()` (rejects empty `HOME`); `config/io.rs`
`config_dir()` / `state_dir()` / `platform_config_dir()` (do not use it);
`platform::remote_ssh_config_paths()` reads `HOME` directly. See BUG-030.

Reported by: config-cli.

## CON-055 - The reconnect retry promise

Sites: `cli/machine.rs` `reconnect` output string ("within 30 seconds");
`MAX_RETRY_DELAY`, `ATTENTION_RETRY_DELAY`, `ATTEMPT_BUDGET < MAX_RETRY_DELAY`
(tested in the supervisor; the CLI text is a literal).

Reported by: remote.

## CON-057 - Workspace and agent row presentation in two sidebars

Sites: `client/shell/sidebar.rs` (local) and `client/shell/endpoint_sidebar.rs`
(multi-endpoint) render collapsed/expanded rows independently; selection
background private in `sidebar.rs:7` and inlined at `endpoint_sidebar.rs:125`;
agent rows in `agent_sidebar.rs` vs `endpoint_agents.rs` (seen only via
references). Already disagree on numbering (BUG-026); stale styling only in the
endpoint path.

Proposed owner (ui hunter, called the biggest payoff in the client shell): local
is the one-endpoint case of the endpoint sidebar; delete `sidebar.rs`'s row
rendering.

Reported by: ui.

## CON-058 - What to do on a poisoned lock

Sites: `pane.rs` `active_pending_release` returns `None` on poison; other pane
sites use `unwrap_or_else(into_inner)`; ghostty `Listener` recovers with
`PoisonError::into_inner` everywhere while the PTY actor treats a poisoned core as
fatal.

Reported by: pane-detection, terminal-core.

## CON-059 - Character width

Sites: the core grid uses unicode-width plus the U+FF9E/U+FF9F special case;
`copy_mode.rs` counts cells with `ghostty::unicode_codepoint_width`;
`agent_sidebar.rs:358` `put_text` iterates `chars()` and redefines
`display_width` locally although `render::display_width` exists. See BUG-003,
BUG-027.

Proposed owner: one width rule in the vt module.

Reported by: terminal-core, ui.
