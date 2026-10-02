# Hunt: contracts (shepr-config, shepr-protocol, shepr-api)

Design reconnaissance of the three crates that define what crosses a
boundary. Findings only; nothing was changed. Item names are given instead of
line numbers. Where a finding reaches into a consumer crate, the consumer is
named.

## Headline recommendations

1. Give every identity that crosses a boundary one typed form end to end:
   `BootId`, the build identity, `WorkspaceId`, the workspace position, the
   agent, the agent state and the projection revision all leak back to
   `String`, `&str`, `usize` or `u64` at the JSON API, in server state, in the
   client or in config. Most of the repo's typed-id work stops at the wire
   crate's edge.
2. Collapse the surface delta scheme around one `SurfaceBaseline` type. Today
   the encoder, the decoder (two branches) and a second client baseline each
   decide "is this a patch against unchanged topology" and "apply this patch"
   on their own, held together by a pairwise-agreement test.
3. Cut `shepr-protocol -> shepr-config`. The edge exists for four constants
   (grid budget and input batch). They belong in `shepr-core` next to
   `GridSize`, which should own the upper bound too.
4. Split `shepr-config`. It is a TOML loader, the process runtime layout
   (`AppPaths`, XDG, `BuildProfile`, `ServerAddress`, operator guidance), a
   per-keypress keybinding matcher, and a theme palette library. `shepr-api`
   depends on it only for `AppPaths`.
5. Replace config provenance (string key-path lookups against the TOML
   document) with an explicit/default distinction in the model itself.
6. Make failures that callers branch on typed: `EndpointError::Rejected(String)`,
   the stop exit codes, the CLI's server status, `ShutdownReason`, and the
   API error code that `server_stop` matches as a string.

---

## 1. Axes that should be types

### 1.1 Boot identity is typed only inside the TUI wire

`shepr_protocol::BootId` exists and validates its spelling, but everything on
the JSON side and in the stop and launch flows carries the boot as `String`:

- `shepr_api::schema::ResponseResult::Pong { boot_id: String, .. }`, built in
  `server::route_request` by `BootId::for_this_process().to_string()`.
- `ServerStopIfBootParams::expected_boot_id: String`.
- `RuntimeStatus::boot_id: String`, `ServerStatusJson::boot_id: Option<String>`.
- `server_stop`: `stop_active_server(expected_boot_id: Option<&str>)`,
  `BootProbe::Changed(String)`, `BootStopWait::Changed(String)`,
  `LeaseWait::NewBoot(String)`, `ServerStopError::{BootMismatch,
  OccupantChanged}` with `expected_boot_id: String` and `actual_boot_id: String`.
- `server::stop_server` compares `BootId::for_this_process()` with the raw
  `&str` through `PartialEq<&str>`.
- `surface_reuse::Baseline` holds `boot_id: &'a str` and `accepts` takes
  `&str`, although every caller has a `BootId`.
- Consumers: `src/cli/status.rs` `ServerRuntimeStatus::Running { boot_id:
  String }`, `shepr-remote` `DifferentBuildServer::boot_id`.

Consequence worth fixing on its own: because `expected_boot_id` is never
parsed, a malformed `--expect-boot` value is not an invalid request. The server
answers `server_boot_mismatch`, the CLI exits with `BOOT_MISMATCH_EXIT_CODE`,
and `shepr-remote::stop_remote_server` reads that exit as
`RemoteStop::BootChanged`. A typo is reported as "the occupant changed".

`BootId` itself is a `String` newtype: `process_id()` re-parses its own text.
It should be a struct of `{ pid: u32, clock: Result<Duration, Duration> }` with
`Display`/`FromStr`/serde at the boundary, used by the API schema
(`expected_boot_id: BootId` deserializes and refuses junk as `invalid_request`),
by `RuntimeStatus`, and by every stop type.

### 1.2 Build identity is a string with a sentinel

`BUILD_ID: &str` is either sixteen hex digits or the marker
`"unidentifiable--"` (from `build.rs`). Every comparison goes through free
functions over `&str`: `is_identifiable_build_id`, `builds_match`,
`is_this_build`. `PeerBuild { build_id: String }` (preamble),
`Pong::build_id: String`, `RuntimeStatus::build_id: String`,
`ClientStatusJson::build_id`, `SiblingServerJson::build_id`,
`ServerStatusJson::build_id` all carry it raw. `preamble_for(&str)` pads or
truncates whatever it is given. `is_this_build` is called from eight sites
across `src/cli.rs`, `src/cli/status.rs` (twice), `src/preflight.rs`,
`shepr-remote` `host.rs` and `local_server.rs` (twice).

Proposed: `enum BuildIdentity { Known([u8; 16]), Unidentifiable }` with
`fn matches(self, other) -> bool` (false when either is `Unidentifiable`), a
fixed-size preamble encoding, and `RuntimeStatus` carrying a classification
computed once at parse (`BuildMatch::{This, Other(BuildIdentity)}`), so no
consumer can forget the check or compare with `==`.

### 1.3 Two different "workspace numbers", both `usize`

- `WorkspaceId::number()` is the allocator's public number (`w` + bijective
  base 32 of it).
- `ClientShellWorkspace::number` and `command::WorkspaceInfo::number` are the
  1-based display position: `shepr-server` `App::workspace_info` writes
  `number: index + 1`; the snapshot copies `workspace.number` from the session
  snapshot.

Same field name, same type, different meaning, both reachable from one
`ClientShellWorkspace` (`workspace.workspace_id.number()` vs
`workspace.number`). One is a swap away from a wrong jump target. The
position should be `WorkspacePosition(NonZeroUsize)` (or dropped from the wire
and derived from list order, since the snapshot's `workspaces` vector is
already ordered).

### 1.4 Agent identity crosses the wire as a string

`ClientShellAgent::agent: Option<String>` carries `Agent::label()` text. On the
client it is used as a lookup key into
`AgentsSidebarConfig::rows_by_agent: BTreeMap<String, AgentSidebarRows>`
through `rows_for_agent(Option<&str>)`; the config side validates each key
with `ConfigAgent::parse_canonical_label` and then throws the parsed agent
away, keeping the string. The client also substitutes `"terminal"` for `None`
in `shell/overlays/overlays.rs`. Client and server are always the same build,
so the closed set is known on both sides.

Proposed: `ClientShellAgent::agent: Option<shepr_agent::Agent>` on the wire
(protocol may depend on `shepr-agent`; it is lower), and
`rows_by_agent: BTreeMap<Agent, AgentSidebarRows>` in config, with
`rows_for_agent(Option<Agent>)`.

The JSON API has the same axis: `PaneReportAgentParams::{source, agent}` and
`PaneReportAgentSessionParams::{source, agent, session_start_source}` are
`String`, parsed later in `shepr-server` `app/api/panes/reports.rs` into
`shepr_agent`'s `AgentSource`. Built-in sources are spelled `"shepr:<label>"`
and matched as strings. Parsing at the schema boundary (custom `Deserialize`
into `AgentSource` and `Agent`) would turn bad reports into
`invalid_request`/`invalid_agent` at one place.

### 1.5 Agent state exists as four enums, two of them mirrors

- `shepr_agent::detect::AgentState { Idle, Working, Blocked, Unknown }`
  (already `Serialize`/`Deserialize`).
- `shepr_api::schema::PaneAgentState { Idle, Working, Blocked, Unknown }`: an
  exact mirror.
- `shepr_agent::detect::PresentedAgentState { Idle, Working, Blocked }`.
- `shepr_protocol::AgentStatus { Idle, Working, Blocked }`: an exact mirror.

`shepr-server` `app/api_helpers.rs` hand-converts each pair. `shepr-api`
already depends on `shepr-agent`. Use `AgentState` in the API schema (with a
snake_case serde rename) and `PresentedAgentState` on the wire, and delete the
mirrors and converters.

### 1.6 Revision counters are unwrapped everywhere

`ProjectionRevision`, `SurfaceRevision`, `ConnectionGeneration` are generated
by `revision.rs::counter!` with `From<u64>`, `Into<u64>`, `PartialEq<u64>`,
`PartialOrd<u64>`, `new`, `get`. Callers use those hatches as the normal path:

- `command::EndpointReply::ClientShellSurfaceSet { projection_revision: u64 }`
  is a raw `u64` on the typed wire.
- `shepr-server` `app/state.rs` `shell_projection_revision: u64`;
  `server/headless/render.rs` field `projection_revision: u64` and several
  `.get()` calls; `surface_interest.rs` returns `(bool, u64)`;
  `client_shell::snapshot_from_session(revision: u64)` then `revision.into()`.
- `shepr-client` `endpoint/choice/preparing.rs`: `floor: Option<u64>`,
  `coherent_surface(minimum_revision: u64)` comparing `.get()` values;
  `endpoint/view.rs` and `shell/state.rs` unwrap too.
- `PaneSurfacePane::content_revision: u64` and
  `ClientShellAgent::state_change_seq: u64` are revision-like counters with no
  type at all.

Drop `From<u64>`, `PartialEq<u64>`, `PartialOrd<u64>`; keep `ZERO`,
`checked_next` and ordering between values of the same counter. Mint only
through the owning allocator. Type `content_revision` and `state_change_seq`.

### 1.7 Endpoint failures that a client could branch on are prose

`command::EndpointError::Rejected(String)` is the only failure variant for
anything the app refuses. Producers in `shepr-server` include "workspace {id}
not found", "pane {id} not found" (`app/api/endpoint.rs`), "split children not
found", "ratio must be finite" (`app/api/layouts.rs`), "cwd ... must be an
absolute path" (`app/api/cwd.rs`), and "the response could not be encoded"
(`server/client_commands.rs`). A vanished target (a benign race the client
might retry or ignore silently) is indistinguishable from a real refusal or an
internal encoding failure.

Proposed variants: `WorkspaceGone(WorkspaceId)`, `PaneGone(PublicPaneId)`,
`InvalidArgument(InvalidArgument)` (a closed enum), `Internal`, plus
`Rejected(String)` only for messages that really are for the user.

`LayoutSetSplitRatioParams::ratio: f32` can arrive as NaN; the server checks
"ratio must be finite". A `SplitRatio` newtype whose deserialization refuses
non-finite or out-of-range values would make the bad value unrepresentable
past decode.

### 1.8 `ShutdownReason` is a one-variant enum around prose

`ShutdownReason::Message(String)` is the only variant;
`ServerMessage::ServerShutdown { reason: Option<ShutdownReason> }`. Every
production site in `shepr-server` (`server/headless.rs`,
`server/headless/lifecycle.rs`, `server/client_transport.rs`) sends the same
literal "server is shutting down". Make it
`enum ShutdownReason { Stopping, .. }` with `Display` on the client, and drop
the `Option` unless a reasonless shutdown is a real case.

`HandshakeRefusal::InvalidSurface(String)` is prose for a geometry check the
server makes (`client_shell_geometry_error`); `ConnectionLimit(u32)` is built
from a `usize` constant with `u32::try_from(..).unwrap_or(u32::MAX)`.

### 1.9 "Over a size limit" has six shapes

- `FramingError::Oversized { claimed: usize, max: usize }`
- `NoticeKind::PasteRejected { size: usize, max: usize }`
- `NoticeKind::OversizedSurface { claimed: usize, max: usize }`
- `EndpointError::ResponseTooLarge { size: u64, limit: u64 }`
- `CodecError::CollectionLimitExceeded { len: u64, max: usize }`
- `codec::serialize_bounded_vec` and `deserialize_bounded_vec` report the same
  condition as `CodecError::Message(String)` prose, so a field cap and the
  codec's global cap produce different error kinds for the same question.

One `LimitExceeded { limit: Limit, actual: u64 }` with `Limit` a closed enum
naming which cap (frame, message, paste, surface, collection field) would let
logs, notices and tests branch on it. `CodecError::Invalid(&'static str)` has
no producer anywhere and can go.

### 1.10 Bounded collections are an attribute, not a type

The `#[serde(serialize_with = "codec::serialize_bounded_vec::<N, _, _>",
deserialize_with = ...)]` pair is repeated on about fifteen fields across
`frame.rs`, `surface.rs`, `input.rs`, `command.rs`. A field that forgets it
falls back to `MAX_COLLECTION_ITEMS` (the whole-surface cell budget).
`BoundedVec<T, const N: usize>` with its own `Serialize`/`Deserialize` makes
the cap part of the type, removes the boilerplate, and gives the error a typed
home (1.9).

### 1.11 The wide-glyph tail is a sentinel

A wide glyph's tail is "`symbol` is empty and `grid_width == One`"
(`pane_row.rs::is_tail`); a lead is "`grid_width == Two` and symbol non-empty".
An empty `Two` cell is representable and treated as broken. `CellData::skip:
bool` is a ratatui diff hint carried on the wire. Proposed:
`enum GridCellWidth { Grapheme, One, WideLead, WideTail }` (the tail carries no
symbol), making half the states `broken_cells` hunts for unrepresentable, and
`normalize_pane_row` only has to handle crops.

### 1.12 `FrameData` has public fields and no invariant

`cells.len() == width * height`, "every `hyperlink` index is inside
`hyperlinks`", and the grid budget are checked at decode time in
`surface_reuse::Decoder::decode` (several times per branch),
`apply_patch_to_surface`, `surface_delta::apply_rows`,
`surface_delta::metadata_fits` and `surface_delta::message`; the
hyperlink-index check alone appears five times in `surface_reuse.rs`, each
with an `index as usize` cast. `FrameData::intern_hyperlink`'s doc explains
that a cache cannot live in `FrameData` because the vector is public. Make the
grid a validated type (private fields, `try_from` deserialization, mutation
through methods), so a decoded `PaneSurfaceFrame` is valid by construction and
the decoder only checks what relates two values (baseline vs update).

### 1.13 Config provenance is a string lookup that exists because "unset" is not modelled

`ConfigProvenance` collects every TOML key path of the document as strings and
answers `is_explicit(UiPreferenceKey)` by matching the literals
`"ui.sidebar_width"`, `"ui.sidebar_start_collapsed"`, `"ui.agent_panel_sort"`,
and keybinding validation asks `key_is_configured(&format!("keys.{field}"))`.
These literals are a third copy of the field names (after the struct and the
template), checked by nothing.

It exists because `ClientUiConfig::sidebar_width: u16` (documented "while
unset, the client shell remembers a width") cannot be unset: `Default` fills
26. The client then reassembles the fact in
`shell/overlays/preferences.rs::ConfiguredChrome`. Proposed: the raw model uses
`Option<T>` for every setting whose absence means something, validation
produces `Setting<T> { Explicit(T), Default(T) }` (or the client-preference
fields as `Option<T>`), and keybinding validation receives
`Option<BindingConfig>` per field. Provenance, its path formatter
(`format_config_path`), and `UiPreferenceKey` disappear.

### 1.14 Other config axes

- `TerminalConfig::default_shell: String` uses `""` to mean "use `$SHELL`";
  should be `Option<String>`. The validated
  `ValidatedTerminalConfig::default_shell: String` is an absolute path but is
  kept as `String`, which is why `shell_path_string` must reject a non-UTF-8
  shell path. Make it a `PathBuf` newtype (`ShellPath`) and accept any path.
- `SessionConfig::startup_per_agent_delay_ms: u32` is converted to `Duration`
  in `shepr-server` `app/mod.rs`; validation should produce the `Duration`.
- `AdvancedConfig::scrollback_limit_bytes: usize` is threaded raw through
  about twenty signatures in `shepr-mux` (`pane/runtime.rs`, `workspace.rs`,
  `persist/restore.rs`, `pane_tree.rs`, `logging.rs`) next to `u16` cols and
  rows. A `ScrollbackBudget` type that owns the documented policy ("0 disables;
  any non-zero keeps at least 1000 lines") would stop that policy being
  re-derived wherever the number lands.
- `ExperimentalConfig::cjk_ime_cursor_shape: ImeCursorShape` goes through
  `ImeCursorShape::to_decscusr() -> u8`, is stored as `u8` in
  `shepr-server` `app/state.rs` settings, and comes back through
  `CursorShapeParam::from_decscusr(u8)`, which maps unknown values to
  `Default`. A direct `From<ImeCursorShape> for CursorShapeParam` (or one enum)
  removes the integer hop and the silent fallback.
- `ThemeConfig::name: Option<String>` is canonicalized by
  `canonical_theme_name` into a `&'static str`, then `Palette::from_name`
  canonicalizes again. A `ThemeName` enum generated by
  `define_builtin_themes!` (with aliases in `FromStr`) makes the
  "theme {canonical} has no built-in palette" branch in
  `theme_config::resolve_palette` unrepresentable (it is unreachable today).
- `ValidatedClientUiConfig::mouse_scroll_lines: NonZeroU16` while the raw
  model has `Option<NonZeroUsize>` and `DEFAULT_MOUSE_SCROLL_LINES: usize`;
  one type would do.

### 1.15 Keybindings: tuple alias, labels as data, help groups as strings

- `KeyCombo = (KeyCode, KeyModifiers)` is a type alias exposed through
  `LiveKeybindConfig::prefix`, `BindingTrigger`, `format_key_combo`,
  `normalize_key_combo`, `terminal_key_matches_combo`. The real identity type,
  `CanonicalKey`, is private. Callers outside config normalize tuples
  themselves (see 2.5).
- `ResolvedBinding::label: String` and `IndexedKeybind::label: String` are
  derived from the trigger and then re-parsed: `ActionKeybinds::prefix_rhs_label`
  strips `"prefix+"`, and `shepr-termio` `input/keybind_help.rs::indexed_label`
  reconstructs ranges by `strip_suffix` on digits. Labels should be computed by
  `Display` on the trigger, and a range binding should stay one value
  (`IndexedRange { trigger_kind, modifiers }`) rather than nine labelled
  bindings that help has to regroup.
- Help groups `"global"`, `"workspaces"`, `"panes"`, `"navigation"` are string
  literals in `keybinding_table!` and again in `keybind_help_groups`. A typo in
  the table drops an entry into a group that is never shown. Make the group an
  enum ident column of the table.

### 1.16 Config diagnostics carry their key path in prose

`ConfigDiagnostic::{Read, Parse, Unknown, Validation, Path}(String)`; every
validation message embeds its key path ("ui.sidebar_width (..) must be ...",
"invalid keybinding: keys.zoom = ..."), `with_file` splices the file path into
the message text, `AppPaths::resolve` returns `Vec<String>`, and
`KeybindValidation::diagnostics` is `Vec<String>`. Tests find diagnostics with
`contains("keys.prefix")`. A `ConfigDiagnostic { file, key: Option<KeyPath>,
kind }` keeps the CLI rendering identical and lets tests and any future
"open the config at this key" behaviour branch.

The resolution structs (`ClientConfigResolution`, `ServerConfigResolution`)
are `{ diagnostics: Vec<String>, values: Option<..> }`, which represents the
impossible "no diagnostics and no values" state; three places
(`ValidatedClientConfig::from_resolution`, `ValidatedServerConfig::from_values`,
`LoadedConfig::into_validated_with`) produce a runtime error string for it.
`Result<Values, Vec<ConfigDiagnostic>>` makes it unrepresentable.

### 1.17 JSON API schema axes

- `ServerStatusJson { running: bool, version: Option, build_id: Option,
  boot_id: Option, compatible: Option<bool>, socket, restart_needed: bool }`
  smears a state machine over booleans and options: `running: false` with
  `compatible: Some(true)` is constructible. `restart_needed` is
  `!compatible` when running.
- `SiblingServerJson` documents "either the identity (`version` and
  `build_id`) is present, or `error` says why" but models it as four
  `Option`s; `print_client_status_body` and `shepr-remote` discovery each
  decide which combination means what.
- `ResponseResult::DetectExplain { explain: serde_json::Value }` is untyped.
- `RuntimeStatus::version: Option<String>` is always `Some` (the only
  constructor is `client::runtime_status`).
- `ErrorBody::code: String`. `ApiErrorCode`'s doc says "nothing parses a wire
  code back into this enum", yet `server_stop::send_stop_request` classifies a
  refusal by `error["code"].as_str() == Some(ApiErrorCode::ServerBootMismatch.as_str())`.
  Derive `Deserialize` for `ApiErrorCode` (with an `Unknown` fallback for
  cross-build reads) and parse `ErrorResponse` properly.
- Request ids are `String` with `""` standing for "no id" (`request_id_from_line`,
  `hand_off` calls `send_busy_refusal(stream, "")`). `ErrorResponse::id:
  Option<String>` states it.

### 1.18 Logging outcome is stringly typed

`ApiLogOutcome { Ok, Timeout, Error }` is converted to `&'static str`, then
`server::finish_api_response` adds a fourth outcome, `"client_disconnected"`,
as a bare literal, and `logging::api_request_completed` decides the log level
with `outcome != "ok"`. The logging functions also take
`(name, mutates_ui, routine)` unpacked instead of `MethodTraits`. Add
`ClientDisconnected` to the enum and pass the enum and the traits.

### 1.19 Geometry: a zero sentinel round trip

`TerminalGeometry` holds `cell: Option<CellPx>`, but its `width()`/`height()`
return `0` for `None`, and `shepr-server` `client_transport.rs` rebuilds
`ProtocolCellSize::from_wire(hello.geometry.width(), hello.geometry.height(),
..)` from those zeros. `ProtocolCellSize` and `shepr-core` `PaneGeometry` have
the same zero accessors. `ClientMouseGeometry { cols, rows, width_px,
height_px }` and `PaneSurfacePane::{pixel_width, pixel_height}: u32` are two
more shapes of the same fact. Prefer passing `Option<CellPx>` through and
deleting the zero accessors; a single `GridAndCell` type can serve mouse
geometry and pane metadata.

### 1.20 Smaller ones

- `ClientShellWorkspace::git_ahead_behind: Option<(usize, usize)>` is a bare
  tuple; `shepr-mux` already has the counts as a struct.
- Paths as `String` on the wire: `WorkspaceCreateSource::Cwd`,
  `WorkspaceCheckoutRootParams::cwd`, `EndpointReply::WorkspaceCheckoutRoot {
  root, home }`, `ClientShellWorkspace::new_workspace_cwd` (where `""` means
  "none", via `map_or_default` in `client_shell.rs`),
  `ClientShellPane::{cwd, foreground_cwd}`, `SessionRestoreNotice::backup_dir`.
  A `RemotePath(String)` newtype (a path on the server's host, never opened by
  the client) documents that they are not local paths.
- `PaneCopySearch { total: u64, current: Option<u32>, current_global:
  Option<u64> }` mixes widths for the same count.
- `SidebarTokenRule::hide: Option<bool>` where only `Some(true)` matters;
  `style_for_value -> Option<SidebarTokenStyle>` uses `None` for "hidden".
  `enum TokenRendering { Hidden, Styled(SidebarTokenStyle) }` says it.
- `AgentSidebarToken::Styled { token: Box<Self>, .. }` permits nesting that the
  parser never produces. `struct SidebarTokenSpec<T> { token: T, style, rules }`
  over a plain token enum removes the box and the recursion in `allows_rules`
  and `parts`.
- Exit codes `NO_SERVER_EXIT_CODE` and `BOOT_MISMATCH_EXIT_CODE` are bare
  `i32` constants, unlike `DaemonExit`, which owns `code()` and `from_code()`
  (see 2.12).

---

## 2. Decisions made in more than one place

### 2.1 "Is this update a patch against unchanged topology?" (four sites)

- Encoder: `surface_reuse::Baseline::update` picks `SurfaceMeta::Patch` when
  projection revision, width, height, hyperlinks, splits and pane ids (in
  order) all match.
- Decoder: `Decoder::decode`, `SurfaceMeta::Projection` branch, turns the
  update into an internal patch when projection revision, splits, hyperlinks
  and pane ids match (width and height checked separately just above).
- Decoder: the `Patch`/`None` branch keys only on projection revision.
- Planner: `surface_delta::unchanged_plan` and
  `projection_metadata_is_unchanged` each compare a different field set
  (whole frame, whole panes, cursor included).

They agree today. Nothing ties them: adding a field to the topology (say a
per-pane scrollbar layout) requires remembering all four. Owner: a
`SurfaceTopology` value (or a digest of it) computed from a surface, with
`same_topology(a, b)`, used by `Baseline::update`, the decoder and the planner.

### 2.2 "Apply a patch to a baseline" (two implementations, a second baseline, a pairwise test)

`surface_reuse::Decoder::decode` applies a patch in place on its
`CellBaseline` (pane-membership check, `apply_rows`, pane metadata replace,
cursor, revision). `surface_reuse::apply_patch_to_surface` does the same to a
`PaneSurfaceFrame` with its own checks. `shepr-client`
`endpoint/choice/preparing.rs` keeps a second full baseline and runs
`apply_patch_to_surface` on it "to keep its full-surface baseline at the same
revision as the decoder". The test
`surface_update_keeps_same_projection_as_an_internal_patch` asserts the two
stay equal for one case.

This is the shape the brief names: two copies held in step by a
pairwise-agreement test. Owner: one `SurfaceBaseline` type with
`apply(&Patch)` used by the decoder, and the client reading the decoder's
baseline (`Decoder::current_surface` already exists) or holding a cheap handle
to it instead of a second copy.

### 2.3 Grid validity and hyperlink indices (five-plus sites)

See 1.12. "Cells fill the grid", "grid within budget" and "hyperlink index in
range" are each asked in several functions of `surface_reuse.rs` and
`surface_delta.rs`. Owner: a validated grid type.

### 2.4 Navigate-mode arrow aliases (four sites)

- `keybinding_table!` `navigate` rows carry an alias column (`Left`, `Right`,
  `None`).
- `shepr-config` `keybinds.rs::reserve_navigate_runtime_keys` hardcodes
  `KeyCode::Left` and `KeyCode::Right` instead of reading the column.
- `shepr-client` `shell/input/input.rs` has `navigate_alias_matches_left` and
  `navigate_alias_matches_right` building the combos again.
- `shepr-termio` `input/keybind_help.rs` maps the alias ident to the label
  strings `"left"` and `"right"`.

Adding an alias to the table updates help and matching only if two hand-written
macro arms are added, and the conflict reservation not at all. Owner: generate
a `NavigateAlias` enum from the table with `combo()` and `label()`, used by
all four.

### 2.5 Indexed bindings: range and modifier preference

- `shepr-config` `limits.rs` has `FIRST_INDEXED_BINDING_KEY = '1'`,
  `LAST_INDEXED_BINDING_KEY = '9'`, and `INDEXED_BINDING_RANGE_SYNTAX = "1..9"`
  (the last derivable from the first two, kept in step by hand).
- `shepr-termio` `keybind_help.rs::indexed_label` and `indexed_range_prefix`
  hardcode `"1..9"`, a run length of 9 and `b'1'`.
- `shepr-client` `input.rs::navigate_indexed_binding_index` re-derives
  modifier equivalence with `normalize_key_combo` and adds its own
  "exact modifiers first" preference, outside `IndexedKeybind::matched_index`.

Owner: an `IndexedRange` type in config that owns parse, label and matching
(including the preference), so help and the client ask it.

### 2.6 Modifier name vocabulary (two tables)

`model.rs::RIGHT_CLICK_MODIFIER_ALIASES` and `keybinds.rs::parse_modifier_token`
both decide what `ctrl`, `control`, `alt`, `option` and `meta` mean. They agree
today ("meta" is Alt in both). The default template promises the right-click
setting accepts "keybinding aliases". Owner: one alias table; the right-click
parser applies its extra restriction (ctrl and alt only) on top.

### 2.7 Configured colour parsing (two parsers)

`sidebar.rs::SidebarTokenColor` parses `#rgb`/`#rrggbb`; `theme_config.rs::
try_parse_color` parses hex, `rgb()`, names and reset. They deliberately accept
different sets, but the hex rule is implemented twice (different code, same
question). Owner: one hex parser that `try_parse_color` and the strict sidebar
form both call.

### 2.8 The palette token list (five copies)

`theme.rs::Palette` fields, `ParsedThemeColors` fields,
`theme_config.rs::define_custom_theme_colors!` list, `Palette::with_overrides`
arms, and the destructures in `lib.rs` tests. The macro covers only one of
them. Owner: one `palette_tokens!` list generating all.

### 2.9 Grid budget (three sites, three crates)

- `shepr-config` `limits.rs::terminal_grid_cells` (dimension and cell caps).
- `shepr-protocol` `ClientSurfaceSize::clamped` re-derives the row bound
  (`MAX_SURFACE_CELLS / cols`, min `MAX_SURFACE_DIMENSION`) instead of calling
  the function.
- `shepr-core` `GridSize::clamped` clamps the minimum only; a decoded
  `TerminalGeometry` holds any `GridSize` up to 65535 by 65535, checked later
  by `shepr-server` `client_shell_geometry_error`.

They agree today. Owner: `shepr-core`, as a bounded grid type
(`GridSize::new` refusing over-budget grids, `GridSize::clamped` clamping both
ends), which also lets 3.1 drop the protocol-to-config edge.

### 2.10 Handshake answering (two implementations)

`shepr-api` `server/client_protocol.rs::refuse_client` and `shepr-server`
`server/client_transport.rs` (the accepting handshake) each read the preamble,
decide which `PreambleError` outcomes get this build's preamble back, read the
hello with the handshake cap, and answer a welcome. They already differ in
order: the refuser writes its preamble after reading the hello, the accepting
path writes it before. Both are compatible with a client that writes preamble
and hello together, but nothing pins that. Owner: a handshake function in
`shepr-protocol` (or `shepr-api`) that returns
`Hello | Foreign | Silent | NotShepr` and writes the preamble once by one
rule; the refuser and the server differ only in the welcome they send.

### 2.11 Stop outcome and exit code (encode in one crate, decode in another)

`src/cli/error.rs::CliError::exit_code` maps `ServerStopError` to
`BOOT_MISMATCH_EXIT_CODE`/`NO_SERVER_EXIT_CODE`; `shepr-remote`
`remote/launch.rs::stop_remote_server` maps those codes back to
`RemoteStop::BootChanged` (folding "no server" into "boot changed").
`DaemonExit` shows the right shape: one enum with `code()` and `from_code()`.
Owner: `shepr_api::server_stop::StopExit` with both directions, used by the
CLI and by `shepr-remote`.

### 2.12 What state is the server in? (two classifiers that disagree)

`shepr_api::read_server_presence_at` classifies `Gone`, `Starting`,
`Running`, `Stopping`, `Unresponsive`, with "stopping wins over starting".
`src/cli/status.rs` builds its own `ServerRuntimeStatus { Running, NotRunning }`
from `ApiClient::status()` and ignores `stopping` and `starting`. So
`shepr status server` prints `status: running` and `build_compatible: yes`
for a server that is still restoring or already stopping, and the JSON says
`running: true`. That is a live disagreement. Owner: the CLI renders
`ServerPresence`.

### 2.13 Build compatibility, derived twice in one file

`src/cli/status.rs::build_compatible_bool` and `restart_needed_bool` each call
`is_this_build` on the same field; `ServerStatusJson` serializes both. Folded
into 1.2: classify once at parse.

### 2.14 Is this JSON reply an error? (two parsers)

`client::parse_response_value` and `server_stop::send_stop_request` both
decide with `value.get("error").is_some()`. Because `send_stop_request` uses
`request_value_until` (raw `Value`), its
`Err(..ApiClientError::ErrorResponse(..))` arm is unreachable, and its
`Ok(_)` arm accepts any non-error success as "stop accepted" without checking
it is `ResponseResult::Ok`. Owner: one typed parse into
`Result<ResponseResult, ErrorResponse>`, used by both.

### 2.15 Shell quoting and stop-command spelling (disagree)

`shepr-config` `address.rs::shell_quote` and `shepr-remote`
`remote/launch.rs::shell_quote` answer "is this a plain shell word" with the
same character set, but only the remote one quotes a leading `=` (zsh
expansion). The command spelling `server stop` / `--expect-boot` is built in
`address.rs::ServerAddress::stop_command`, `src/preflight.rs::remote_stop_command`
and `shepr-remote` `RemoteCliCommand::ServerStop`. Owner: one quoting function
(in `shepr-platform` or core), and one `CliInvocation` builder for the
commands shepr tells operators to run.

### 2.16 Which methods the socket thread answers (a table plus a hand map)

`schema.rs::define_methods!` generates `Method`, `MethodKind` and traits.
`AppMethod` is a hand-written subset, `AppMethod::traits` hand-maps each arm
back to a `MethodKind`, and `server::route_request` hand-maps `Method` to
`AppMethod`. The test `method_traits_carry_routing_and_log_facts` checks one
arm (`app_capture == capture`). Exhaustiveness catches a missing arm, not a
wrong one. Owner: add a `route: socket | app` column to `define_methods!` and
generate `AppMethod`, its traits and the routing match.

### 2.17 The data-directory lease (two owners of one protocol)

`shepr-mux` `persist/lock.rs` owns `DataDirLease`; `shepr-api`
`server_stop.rs::data_dir_lease_is_free` probes the same file with its own
`acquire_flock_lock(path, false)`. Only the file name is shared (through
config). `shepr-api` sits below `shepr-mux`, so it cannot call the owner.
Owner: put the lease (acquire and probe) in `shepr-platform`, with the file
name, and have both use it.

### 2.18 Not findings (deliberate re-checks)

- `MAX_INPUT_EVENT_BATCH` is enforced by the client batcher, the bounded wire
  field and the server's expanded-count check, all through
  `ClientPaneInputEvent::expanded_event_count`: one answer checked at each
  chokepoint.
- The build identity check is one function (`is_this_build`); the problem in
  1.2 is that its input is untyped, not that the answer is duplicated.

---

## 3. Structure

### 3.1 `shepr-protocol -> shepr-config` points the wrong way

The edge exists for `MAX_INPUT_EVENT_BATCH`, `MAX_TERMINAL_GRID_CELLS`,
`MAX_TERMINAL_GRID_DIMENSION` and `terminal_grid_cells`; nothing else in
protocol touches config. Through it, protocol's graph includes `toml`,
`serde_ignored`, `shepr-agent`, `shepr-platform` and crossterm. The grid
budget belongs to `shepr-core` geometry (2.9); the input batch cap is a
resource budget that config only borrows to cap one scroll step, so it also
belongs in core. After the move protocol depends on core and vt only, and
`AGENTS.md`'s description of the edge changes.

### 3.2 `shepr-config` does four unrelated jobs

1. TOML model, loading, unknown-key detection, validation (`model.rs`,
   `validated.rs`, `sidebar*`, `window_title.rs`, most of `io.rs`).
2. Process runtime layout: `AppPaths` (XDG resolution, `SHEPR_STARTUP_CWD`),
   `BuildProfile` and the `SHEPR_BUILD_PROFILE` marker policy,
   `ServerAddress` and the socket override rule, `DATA_DIR_LEASE_FILE_NAME`,
   `operator_entrypoint`, and operator guidance prose
   (`ServerAddress::build_mismatch_guidance`). `shepr-api` depends on config
   only for `AppPaths`; `shepr-remote`, the client and the CLI use it for
   layout, not for settings.
3. Keybinding runtime matching: `BindingKey`, `CanonicalKey`,
   `terminal_key_matches_combo`, `ActionKeybinds::matches_*` run per keypress
   in the client. Config should produce typed bindings; matching belongs with
   key input in `shepr-termio`.
4. Theme library: eighteen built-in palettes, plus `sanitize_window_title_text`,
   which is render-time sanitizing used by the server.

`io.rs` alone holds the build profile, path resolution, the loader pipeline,
BOM repair and unknown-key reporting. Proposed: a small `shepr-layout` crate
(or a module of `shepr-platform`) for job 2, matching moved to termio, and
config left as "parse and validate the two files". The config to
`shepr-platform` and `shepr-agent` edges then serve only shell validation
(`resolve_default_shell`, which reads `SHELL` and `PATH` and probes the
filesystem), which is really server launch policy and could move to
`shepr-server` with config holding the raw string.

### 3.3 `shepr-protocol` mixes the wire with policy and state

- Wire types, codec, framing, preamble: the crate's job.
- Allocators: `TerminalId::alloc` (a process-global counter and clock stamp)
  and `BootId::for_this_process` mint identities in the wire crate. The
  allocation policy belongs to `shepr-mux`/`shepr-server`; protocol should
  only parse and print.
- The surface delta planner (`surface_delta::message`, including the
  "six bytes per cell" density heuristic) is server-side policy; the decoder
  (`surface_reuse::Decoder`) is client-side state. They share the encoding,
  which argues for one module built around one baseline type (2.1, 2.2), but
  they are not wire types.
- `PaneSurfacePatch` derives `Serialize`/`Deserialize` though it never
  crosses the wire (it is produced by the decoder, `DecodedServerMessage`
  says so).
- `ratatui_conversion.rs` and `pane_row.rs` (wide-glyph normalization) are
  rendering rules.

Proposed: protocol keeps types, codec, framing and preamble; a
`shepr-surface` crate (or a clearly separate module) owns grid validation,
the baseline, planning and decoding; allocators move up.

### 3.4 `shepr-api` holds CLI workflows

Besides the schema, client and listener, it holds `server_stop.rs` (about 1100
lines of stop orchestration: boot probing, lease polling, socket waiting),
`status.rs::read_server_presence_at`, `daemon_exit.rs` (the server binary's
exit codes and file name) and `guidance.rs` (one-variant operator prose). The
stop and presence flows are launcher logic used by the CLI and `shepr-remote`;
they would sit better in a launcher module of the root binary or in
`shepr-remote` (which already does launches), leaving `shepr-api` as the
contract. `guidance.rs::OperatorGuidance` has one variant and duplicates the
style of `ServerAddress::build_mismatch_guidance` in config; one home for
operator guidance prose would avoid two voices.

### 3.5 Listener admission caps are not bound to their counters

`ConnectionSlot::try_acquire(&Arc<AtomicUsize>, cap)` takes the cap at every
call; `listener.rs::Dispatch` holds the counters and each call site picks the
matching constant. A wrong pairing compiles. An `Admission { count, cap,
limit: Limit }` type, constructed once per kind, binds them and gives refusals
their message (`EndpointBusy` text, `HandshakeRefusal::ConnectionLimit`) from
the same value. `listener.rs` also reaches back into `server.rs` for
`handle_connection`, `reject_busy_connection` and `send_busy_refusal`; the
JSON connection service could be its own module beside `client_protocol.rs`,
making the listener a pure classifier and dispatcher.

### 3.6 Validated config keeps raw config

`ValidatedClientConfig` stores the whole raw `ClientConfig` only to answer
`machines()`. `ValidatedServerConfig` stores the raw `ServerConfig` and hands
out `session()`, `advanced()` and `experimental()` unvalidated (1.14). The
validated types should own validated values only.

The loader machinery (`LoadedConfig<C, R>`, trait `ConfigResolution`,
`*Resolution` structs, `from_values`/`from_loaded`/`from_resolution`, two
`resolve_*_config` wrappers taking unused arguments, a test-only
`parse_document` that fakes `/bin/sh`) is generic over two roles that differ
only in their validate step. One `fn validate(raw, &LaunchContext) ->
Result<Validated, Vec<ConfigDiagnostic>>` per role, plus the shared document
reader, removes most of it.

### 3.7 Dead dependencies

`shepr-api`'s `Cargo.toml` lists `shepr-agent`, `shepr-core` and `shepr-vt`;
none is used in its sources. `shepr-protocol` lists `tracing`, unused. These
edges inflate the layering picture `brokkr.toml` checks.

### 3.8 Positional wire enums with JSON naming

`command.rs` enums (`SplitDirection`, `PaneRightClickTarget`,
`PaneDirection`, `PaneCopySearchDirection`) carry
`#[serde(rename_all = "snake_case")]`, which the positional codec ignores.
They read as though they were JSON. `AgentStatus` legitimately crosses both
(1.5 resolves it).

---

## 4. Types that resolve to primitives

| Type | Escape hatch | Where it is used raw | What it should offer |
|---|---|---|---|
| `WorkspaceId` | `Deref<Target = str>`, `From<WorkspaceId> for String`, `PartialEq<str/&str/String>`, `number()` | `shepr-server` `app/state.rs` keys `workspace_geometry` by `id.number()`; `retained_surface.rs` keys by `(workspace_id.number(), w, h)` | Make it `Copy` over `NonZeroUsize` with `Display`; use it as the key. Carry the number on the positional wire, not the text. |
| `PublicPaneId` | `Deref<str>`, `PartialEq<str/&str/String>` | string comparisons in tests and API paths | Same: `{ workspace: WorkspaceId, number: NonZeroUsize }`, `Copy`, text only at boundaries. `PublicPaneId::new` panics on zero in production; take `NonZeroUsize` like `WorkspaceId::from_number` returns `Option`. |
| `BootId` | `Deref<str>`, `Borrow<str>`, `PartialEq<str/&str/String>` | `server::stop_server` (`!= expected`), `surface_reuse::Baseline` (`&str`), whole stop flow (1.1) | Structured value, no string comparisons. |
| `RequestId` | `From<String>`, `From<&str>`, `Deref<str>`, `PartialEq<str>` | any text is a valid id; the client mints `client-shell...` strings | A client-minted counter (`u64` newtype) on the TUI wire; JSON ids stay text. |
| Revision counters | `From<u64>`, `Into<u64>`, `PartialEq<u64>`, `PartialOrd<u64>`, `get()` | see 1.6 | Ordering and `checked_next` only. |
| `SshTarget` | `Deref<Target = str>`; `IntoSshTarget for &String` clones | ssh argument building in `shepr-remote` | `as_ssh_arg()`; drop `Deref`. (`MachineLabel` is the good model: no `Deref`.) |
| `KeyCombo` | type alias for a tuple | `normalize_key_combo` called by the client | Public `CanonicalKey` with `matches(&impl BindingKey)`. |
| `ConfigAgent` | alias of `shepr_agent::Agent` re-exported by config | config API only | Fine as a re-export; the problem is that `rows_by_agent` keys by `String` (1.4). |

Sentinels standing in for absence or unknown:

- `"unidentifiable--"` build id (1.2).
- `decode_public_number("") == Some(0)`; `encode_public_number(0) == ""`.
  Both functions are public only for `shepr-mux` tests; they can be private
  with their tests moved into protocol.
- `TerminalGeometry::width()/height()`, `ProtocolCellSize::width()/height()`,
  `PaneGeometry::cell_width()/cell_height()` return `0` for "no cell size"
  (1.19).
- `TerminalConfig::default_shell == ""` means "use `$SHELL`" (1.14).
- Request id `""` means "no id" (1.17).
- `CellData::symbol == ""` marks a wide tail (1.11).
- `ClientShellWorkspace::new_workspace_cwd == ""` when the workspace index did
  not resolve.
- CLI labels `"unknown"` for absent version or build id
  (`src/cli/status.rs::option_label`); `"terminal"` for "no agent" in the
  client overlays.
- `SidebarTokenRule::hide: Option<bool>` (1.20).

String-typed enums:

- Help group names in `keybinding_table!` and `keybind_help_groups` (1.15).
- API log outcomes, including the ad hoc `"client_disconnected"` (1.18).
- API error codes compared as strings in `server_stop` (1.17).
- Theme names (1.14).
- Agent sources `"shepr:<agent>"` and agent labels in API params and on the
  wire (1.4).

---

## Lateral findings

- Bug: `shepr status server` reports a starting or stopping server as
  `running` and `build_compatible: yes` (2.12).
- Bug-shaped: a malformed `--expect-boot` id is reported as a boot mismatch
  (exit 3), which `shepr-remote` reads as "boot changed" (1.1).
- `server_stop::send_stop_request` treats any successful reply as an accepted
  stop and has an unreachable `ErrorResponse` arm (2.14).
- `CodecError::Invalid` has no producer.
- `codec::Decoder::finish`, `from_slice` and `Decoder` itself are public but
  used only by tests; `validated_sidebar_bounds` is public but used only in
  config.
- `theme_config::resolve_palette` has an unreachable
  "has no built-in palette" error branch.
- `ValidatedClientConfig::live_keybinds()` clones the whole keymap on every
  call.
- In `surface_reuse::Decoder::decode`, the `Some(SurfaceMeta::Projection(_))`
  arm inside the compact branch is unreachable (the enclosing `if` excludes
  it) and, unlike every other error, returns `MetadataMismatch` without a
  subject.
- `format_key_combo` falls back to `format!("{code:?}").to_lowercase()` for
  key codes `parse_key_combo` cannot read back, so "a printed combo reads back
  as the same binding" holds only for the parsed set.
- `Pong::version` is `"<pkg>+<BUILD_ID>"`, so the build id is sent twice in a
  pong.
- Every caller of `ServerAddress::build_mismatch_guidance` and
  `attach_command` passes `operator_entrypoint()`; the parameter exists for
  tests. A default method with a test seam would stop five call sites from
  restating the choice.
- Stale wording to fix when touched (reword rather than update, per the
  documentation rule):
  - `crates/shepr-config/src/io.rs`, `AppPaths::resolve_for_server` doc, says
    `new_terminal_cwd = "current"` and "a relative `new_terminal_cwd`"; the key
    is `terminal.new_cwd`.
  - `crates/shepr-protocol/src/command.rs`, `WorkspaceCreateSource::Follow`
    doc, also says `new_terminal_cwd`.
  - `crates/shepr-config/src/default-client.toml`, `[[machines]]` comment,
    hard-codes "26 path bytes ... 22 in a dev build", a drifting specific.
  - `crates/shepr-protocol/src/ids.rs`, `TerminalId` doc, speaks of "the
    pane-backed transition", which reads as a finished migration.
