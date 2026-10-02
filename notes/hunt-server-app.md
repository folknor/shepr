# Design hunt: server-app (crates/shepr-server minus src/server/)

Scope read in full (non-test code): `lib.rs`, `limits.rs`, `logging.rs`, `ui.rs`,
`ui/*`, `app/*` including `app/actions/*`, `app/api/*`, `app/api/panes/*`,
`app/session/*`, plus the contract harness (`agent_integration_contract_tests.rs`,
`agent_report_test_support.rs`) and `test_support.rs` for structure only.
Followed questions into `server/headless/{internal_events,lifecycle,render,retained_surface}.rs`,
`shepr-mux` (`events.rs`, `workspace.rs`, `workspace/geometry.rs`, terminal state),
`shepr-protocol` (`ids.rs`, `command.rs`, `frame.rs`), `shepr-agent` (`AgentSource`),
`shepr-api` schema and `shepr-config` (`ConfigAgent`).

No source line numbers below; sites are named by function.

## Headline moves (if only a few things get done)

1. **One owner for pane content geometry.** The "what rect does this pane's
   terminal get" rule is composed independently in four places (ui render,
   mux spawn sizing, agent resume, the server's retained patch path) and held
   together by two pairwise-agreement tests and a render-routing rule. Collapse
   into one mux function returning a typed per-pane content geometry. (2.1, 2.2, 2.3)
2. **One owner for "may the session be saved now".** `AppPolicy` (mutable, written
   by the server lifecycle), `SessionSaver::freeze_session_saves`, the host
   checkpoint's `Unsaved` latch, the persister kind (threaded vs lease-only) and
   `AppState::session_dirty` all answer parts of it. Fold them into the saver. (2.5, 2.6)
3. **Split `shepr_mux::events::AppEvent`.** It mixes runtime-origin events with an
   optional, recursive generation envelope, a server worker completion and two
   API-origin reports. Make the runtime envelope mandatory and typed, and keep
   API reports and Git completions out of mux. (3.3)
4. **Stop lowering typed agent identity to strings.** `AgentSource` is parsed at
   the API boundary, lowered to `String` in `HookAuthority`, then re-parsed by
   `full_lifecycle_hook_authority(&str, &str)` and `is_reserved_native_state_source`.
   Report parsing should live in shepr-agent and produce one typed report. (1.3, 2.10, 2.11, 3.5)
5. **Workspaces addressed by position.** `ws_idx: usize` is the currency of most
   of `App`, while every request names a `WorkspaceId`; geometry is keyed by
   `WorkspaceId::number()` as a bare `usize`. Key by id (or hand out borrowed
   workspace handles) and kill the "stale index must not panic" defensive code. (1.1)
6. **Typed outcomes instead of hand-assembled effects and revision diffs.** View
   change is detected by snapshotting `shell_projection_revision` around calls in
   at least six places, and every endpoint handler hand-writes its
   `EndpointEffects`. Mutators should return what they changed. (2.8, 2.9)

---

## 1. Axes that should be types

### 1.1 Workspace position used as identity

- `ws_idx: usize` is passed through `pane_info`, `workspace_info`, `public_pane_id`,
  `pane_launch_env`, `lookup_runtime`, `send_pane_focus_event`, `window_title_for`,
  `workspace_spawn_geometry`, `workspace_layout_area`, `launch_cwd_for_pane_in_workspace`,
  `resolved_new_workspace_cwd`, `runtime_for_pane_in_workspace`, and stored in
  `PaneRemovalPlan::workspace_index`, `PaneRemovalOutcome::workspace_index`,
  `WorkspaceCreationOutcome::workspace_index`, `PaneCreationOutcome::workspace_index`.
  Every handler resolves `WorkspaceId -> usize` (`endpoint_workspace`, `resolve_pane_id`)
  and then re-checks with `.get(ws_idx)` because the index may have gone stale
  (comments on `workspace_info` say exactly this). The mux `PaneRemovalPlan` already
  carries the `WorkspaceId`; the app wrapper's `workspace_index` is redundant with it.
- `AppState::workspace_geometry: HashMap<usize, SpawnGeometry>` is keyed by
  `WorkspaceId::number()`. A test inserts `usize::MAX`. The server's
  `retained_pane_layout` cache is keyed by `(usize, u16, u16)` the same way.
  Geometry is per-workspace session data; it should live on the workspace (or a
  map keyed by `WorkspaceId`), which also deletes `retain_live_workspace_geometry`
  and `has_workspace_without_area`'s set reconstruction.
- `WorkspaceInfo::number` is `index + 1` (display position) in `workspace_info`, while
  `WorkspaceId::number()` is the allocator's public number. Two different
  "workspace numbers" share a name and a type. Name the positional one
  `position` or give it a `WorkspacePosition` type.
- Proposal: `AppState` exposes `workspace(&WorkspaceId) -> Option<WorkspaceRef<'_>>`
  / `workspace_mut`, handlers keep the id, and positional indices exist only
  for ordering (`move_workspace`, bookmark repair).

### 1.2 Workspace id flattened to `String` for Git refresh

`workspace_git_refresh_items` does `ws.id.to_string()` into
`WorkspaceGitRefreshItem::workspace_id: String`; `WorkspaceGitStatus::workspace_id`
(mux) is a `String`; `live_workspace_identity_cwd(&str)` and
`apply_workspace_git_statuses` compare it back with `PartialEq<str>`. The git
refresh tests use `"one"` and `"two"`, ids the allocator can never issue. Carry
`WorkspaceId` end to end.

### 1.3 Agent report identity as strings

- API schema (`PaneReportAgentParams`, `PaneReportAgentSessionParams`): `pane_id`,
  `source`, `agent` are `String`; `agent_session_id` and `agent_session_path` are
  two independent `Option<String>`s (both may be set; `session_ref_for_agent_report`
  decides). A `SessionRef { Id, Path }` at the schema makes "both" unrepresentable.
- `normalize_reported_agent_label` returns `Option<String>`: either a canonical
  known label or arbitrary trimmed text. That is a closed-or-open set:
  `ReportedAgent { Known(Agent), Custom(CustomLabel) }`.
- `parse_report_session_ref` compares `agent.label() != agent_label` as strings.
- `AppEvent::HookStateReported` / `AgentSessionReported` and `StateEvent` carry
  `agent_label: String` beside a typed `AgentSource`.
- `HookAuthority { source: String, agent_label: String }` (mux) stores the lowered
  form; `handle_detect_explain` then calls `full_lifecycle_hook_authority(&authority.source,
  &authority.agent_label)` which re-parses both strings. `handle_state_event` calls
  `is_reserved_native_state_source(source.as_str(), &agent_label)`.
- `SnapshotAgent::agent: Option<String>` (from `effective_agent_label().map(str::to_string)`).
- Proposal: shepr-agent owns `AgentReport { source: AgentSource, agent: ReportedAgent,
  session: Option<AgentSessionRef>, seq }` with one fallible parser that does
  label normalisation, source/label agreement and session validation; `HookAuthority`
  stores `AgentSource` + `ReportedAgent`; predicates take those types.

### 1.4 Generations as bare `u64`

Pane-exit checkpoint generations travel as `u64` through
`PreparedPaneExit::Held(u64)`, `request_pane_exit_checkpoint() -> Option<u64>`,
`pane_exit_checkpoint_generation_settled(u64)`, `ExitTicket::generation`,
`NextSave::Checkpoint { exit_generation: Option<u64> }` and the
`PaneExitCheckpoint` machine, whose `through: 0` is the "nothing released yet"
sentinel. Next to it the lifecycle carries a logind warning generation
(`HostShutdownFreeze::generation: Option<u64>`, `frozen_warning_generation`) that is a
different axis of the same primitive. A `CheckpointGeneration` newtype (ordered,
minted only by the machine, with `is_released_by(through)`) removes the swap risk
and the zero sentinel.

### 1.5 Bool soup and tuples standing in for enums

- `handle_internal_event_inner(ev, prepared_checkpoint: Option<bool>)`: the
  three-state "not prepared / prepared unchecked / prepared checkpointed" exists
  as `PreparedPaneExit` already, but is collapsed to `Option<bool>` on entry
  (losing `Settled` vs `Held`).
- `ResumeSchedule::observe(now, has_pending_plans: bool, eligible: bool)`,
  `wakeup(now, eligible, live_theme_reported)`, `is_due(...)`. `(false, true)` is
  meaningless. A `ResumeDemand { None, Pending, Eligible }` argument fixes it;
  call sites like `observe(now, false, false)` read as noise today.
- `GitRefreshScheduler`: `git_refresh_in_flight`, `git_refresh_due_after_in_flight`,
  `git_identity_refresh_requested`, and `last_git_remote_status_refresh = now - INTERVAL`
  as the "due now" sentinel. `due_after_in_flight` only means something while in
  flight. An enum `{ Idle { next_due }, InFlight { rerun: bool } }` plus a
  `discovery_requested` flag would do.
- `AppState::host_terminal_appearance: Option<HostAppearance>` +
  `host_terminal_appearance_explicit: bool` (the same pair also exists on the
  client shell state in `server/`). One `HostAppearanceReport` value.
- `CheckpointTicket::host: bool`, `HostShutdownCheckpoint::take_result() -> Option<bool>`,
  `HostShutdownFreeze::persist_session: bool`, `frozen_session_policy() -> Option<bool>`.
  The "saved" bool and the "was persisting" bool are different questions answered
  with the same primitive; give them `HostCheckpointOutcome { Saved, Unsaved }` and
  reuse the policy enum.
- `Workspace::split_pane(..., shell_config, true, &spawn)` (12 positional args, a
  trailing bare `true`) and `commit_new_pane(..., public_number, true)`.
- `PaneRuntime::search_text_window(query, case_sensitive: bool, ...)` with the
  smart-case rule computed in `handle_pane_copy_search`; `paragraph_motion_target(row,
  direction: i8)` called with `-1` / `1`.
- `App::create_default_workspace -> bool` folds "already have one", "creation failed"
  and "held by backoff" into `false`.
- `AppPolicy` is a two-variant enum, but the server mutates it at runtime and
  saves/restores it through a bool (see 2.5).

### 1.6 Sentinels for absence and "not yet"

- `SpawnGeometry::cell_size: HostCellSize` with `HostCellSize::default()` (0x0)
  meaning "the host never reported one" (`headless_spawn_geometry`); `cell_px()`
  converts to `Option<CellPx>` only at some call sites (see 4.7). Store
  `Option<CellPx>`.
- `AppSettings::cjk_ime_agents: Vec<ConfigAgent>`: empty means "every agent"
  (`surface_cursor`). Use `enum AgentFilter { Any, Only(Vec<Agent>) }`.
- `App::hostname: String` from `hostname().unwrap_or_default()`: `""` for unknown.
- `agent_info`: `last_agent_state_change_seq.unwrap_or(0)`.
- `resume_layout_area` filters `width > 0 && height > 0`: recorded geometry may
  be a zero area and is treated as absent. A non-empty area type at
  `record_workspace_geometry` would make this a construction-time rule.
- `PaneChromeInfo::inner_rect` is initialised to `rect` ("not settled here"),
  then overwritten by every caller (ui, resume). A pre-settled value with a
  meaningful-looking wrong field.
- `PaneInfo::focused` and `WorkspaceInfo::focused` are built `false` in
  `pane_info` / `workspace_info` and fixed later by `fill_reply_focus`, which the
  server loop must remember to call. An app reply type without the field, mapped
  into the wire type by the loop together with the requester's location, makes
  forgetting impossible.
- `"/"` as the cwd of last resort, at four sites (see 2.13).
- `handle_pane_copy_motion`: `terminal_dimensions().map_or(1, ...)`.

### 1.7 Failures that reach callers only as prose

- Every app refusal on the endpoint path is `EndpointError::Rejected(String)`:
  `workspace_missing`, `pane_missing`, "split children not found", "ratio must be
  finite", "split pane belongs to another workspace", "the pane is on the
  alternate screen", "copy search query is too large", "cwd ... must be an absolute
  path", "the pane could not be split: {err}", "the new pane is unavailable". The
  `32f70f2` move typed the loop's errors but left the app's as one string
  variant. The client does not branch on them today, but a stale-target refusal
  (`WorkspaceGone(WorkspaceId)`, `PaneGone(PublicPaneId)`, `SplitGone`) is exactly
  what a client should branch on (drop the stale UI element rather than toast).
- `checkout_root` returns `Result<Option<String>, String>` and classifies "outside
  a repo" by `stderr.contains("not a git repository")`.
- `handle_detect_explain` passes `skip_reason` as `"full_lifecycle_hook_authority"`
  / `"hook_authority"` string literals, and the whole explain is a
  `serde_json::Value`.
- `logging::session_restored(..., outcome: &'static str)` with `"partial"`, `"empty"`,
  `"ok"`.
- `ApiError` for an unparseable pane id is `pane_not_found` (a syntax error and a
  missing pane are indistinguishable to a hook).

### 1.8 Typed config collapsed before use

- `cjk_ime_cursor_shape: u8` in `AppSettings` (`experimental.cjk_ime_cursor_shape.to_decscusr()`),
  turned back into `CursorShapeParam::from_decscusr`, whose `_ => Default` arm
  makes the round trip lossy by construction. Convert once in `AppSettings::from_config`
  and store `CursorShapeParam`.
- `default_shell: String` holds the absolute path config validation resolved, and
  `PaneShellConfig::new(&default_shell, login_shell)` is rebuilt at four sites
  (2.14). Store the `PaneShellConfig` (or a `ResolvedShell`) in settings.

### 1.9 Paths as strings

- `WorkspaceCreateSource::Cwd(String)` and `WorkspaceCheckoutRootParams::cwd: String`
  are validated by `api::cwd::launch_cwd` in the app. An `AbsolutePath` wire type
  that refuses relative text at decode would remove the app check and make the
  "a relative saved cwd breaks the whole snapshot" hazard unrepresentable.
- `prepare_workspace_checkout_root` returns `(PathBuf, Option<String>)` where the
  `String` is `$HOME` via `to_str` (non-UTF-8 home silently becomes "no home").
- `SnapshotPane::cwd` / `foreground_cwd` are `display().to_string()` (lossy).
- The checkout root comes back as `String`.

### 1.10 Labels

Three normalisations of user-supplied labels: `normalized_workspace_label`
(trim, empty clears), the inline trim/filter in `handle_pane_rename`, and
`normalize_reported_agent_label`. Plus `pane_border_title` trims again at
render. A `Label` (trimmed, non-empty) type minted once would let the stores hold
`Option<Label>` and the renderer stop re-trimming.

### 1.11 Parallel closed sets and identical structs hand-mapped in the app

- Mirror enums converted by hand in handlers: `SplitDirection -> Direction`,
  `PaneDirection -> NavDirection`, `PaneWordMotion -> TerminalWordMotion`,
  `PaneCopySearchDirection -> TerminalSearchDirection`, `PaneParagraphMotion -> i8`,
  `PaneAgentState -> AgentState` (`detect_state_from_api`),
  `PresentedAgentState -> AgentStatus` (`presented_agent_status`).
  shepr-protocol already depends on shepr-core, shepr-vt and (through
  shepr-config) shepr-agent, so most of these could be the same type.
- `{ row: AbsRow, col: u16 }` exists three times: `shepr_vt::Point<AbsRow>`,
  `shepr_mux::pane::TerminalTextPoint`, `shepr_protocol::command::PaneTextPoint`.
  `handle_pane_copy_search` and `handle_pane_copy_motion` copy fields between them
  about ten times.
- Three rect types: `ratatui::layout::Rect` (in `AppState`, `SpawnGeometry`,
  `PaneChromeInfo`), `shepr_core::geometry::Rect` (layout), and
  `shepr_protocol::SurfaceRect` (wire), with `layout_rect` / `ratatui_rect` free
  functions and manual copies in `retained_surface`.

### 1.12 Two clock-sample types

`AppClock { now, wall_now }` and `HookClockSample { monotonic, wall }` are the same
sample. `handle_internal_event_inner` builds one from the other, as do the harness
and tests. One type (in mux, since `HookClockSample` lives there).

### 1.13 Loosely keyed maps

- `PreservedLayout::terminal_ids: HashMap<(usize, u32), TerminalId>` keyed by
  (workspace index, `PaneId::raw()`).
- `App::pending_resume_commands: HashMap<TerminalId, Bytes>` beside
  `TerminalState::pending_agent_resume_plan` (see 2.4).

---

## 2. Decisions made in more than one place

### 2.1 What content rect a pane's terminal has

Question: given a workspace layout, an area, the chrome settings and the pane's
screen mode, what is the pane's terminal content rect (and so its PTY size)?

Sites:
- `ui::panes::compute_pane_infos_for_workspace`: `visible_panes` -> `pane_inner_rect`
  -> `terminal_content_rect(alt = runtime.alternate_screen_active())`, with a
  separate "no runtime means a fresh primary-screen shell" branch. Feeds both
  rendering and `resize_surface` (the PTY size rule).
- `shepr_mux::workspace::PaneGeometry::pane_size` (spawn sizing for splits and
  new workspaces): same composition with `alt = false`, but clamps to `max(1)`.
- `app::agent_resume::derived_pending_agent_resume_pane_infos`: same composition
  with `alt = false`, plus its own zoom override (hidden panes get the tiled size,
  the zoomed pane the full area).
- `server::headless::retained_surface::resolve_retained_panes` and
  `retained_pane_layout`: recompute `visible_panes` and the content rect from the
  wire pane's `alternate_screen_active` to validate a retained surface.

Held together by: the shared `terminal_content_rect` helper, and the pairwise
tests `rendered_content_rect_matches_the_size_new_panes_are_spawned_at` (ui vs
mux) and `pending_agent_resume_launches_at_the_size_its_first_resize_keeps`
(ui vs resume). Those tests exercise only the configurations they enumerate.

Already disagree: `pane_size` clamps rows/cols to at least 1; the ui path does
not, so a degenerate pane gets different answers at spawn and at the first
resize. The zoom treatment differs by design between render (hidden panes absent)
and resume (hidden panes at tiled size), and nothing names that difference.

Owner: one mux function, e.g. `PaneGeometry::content_layout(layout, zoomed,
include_hidden, screen_mode: impl Fn(PaneId) -> ScreenMode) -> Vec<PaneContent>`
with `PaneContent { id, rect, borders, content, gutter: Option<Rect> }`, where
`ScreenMode::{Primary, Alternate, NotStarted}` encodes the runtimeless rule.
`pane_size`, ui, resume and the retained validator all call it.

### 2.2 Whether the scrollbar shows, and where its gutter is

Sites: `ui::panes::stable_scrollbar_gutter` (+ `ui::scrollbar::should_show_scrollbar`:
`max_offset_from_bottom > 0`, gutter = rightmost column of `pane_inner`) and
`server::headless::retained_surface::retained_scrollbar_patch` (`max_offset_from_bottom > 0
&& pane_scrollbars && !alternate_screen_active`) plus the gutter formula in
`resolve_retained_panes`. The ui exports `render_pane_scrollbar_buffer` so the
patch path can draw, but not the visibility rule. Owner: part of `PaneContent`
from 2.1 (the gutter) plus one `scrollbar_visible(metrics, content)` in ui.

### 2.3 Where the cursor is and how it looks

Sites: `ui::surface_cursor` applies the CJK IME reveal (`reveal_hidden_cursor_for_cjk_ime`,
the agent filter, the configured shape); `retained_surface::retained_cursor` does
not. They disagree whenever reveal is on; `render_pass_with_boundary` compensates by
promoting every patch to a full render when `reveal_hidden_cursor_for_cjk_ime` is
set. That is a routing rule standing guard over a duplicated decision. Owner: one
`ui::pane_cursor(state, runtime, pane, content)` called by both paths, after which
the routing special case can go.

### 2.4 Whether a pane is waiting to resume, and where its resume is

Sites: `has_pending_agent_resume_candidates`, `pending_agent_resume_candidates`,
`pane_awaits_agent_resume`, the inline runtime-absent + plan-present check inside
`pending_agent_resume_candidates`, `has_pending_agent_resumes`, and
`handle_pane_launch_settled` (`resume_command.is_some() || terminal.pending_agent_resume_plan.is_some()`).
Held together by the pairwise test `candidate_probe_agrees_with_the_collected_candidates`
and the doc comment "Same rules as ...". The lifecycle itself is split across three
stores: `TerminalState::pending_agent_resume_plan`, `App::pending_resume_commands`
(pruned by `retain` on every pass), and runtime presence in the registry.
Owner: a `ResumeState { Planned(plan), Launching { plan, command }, ... }` on one
store (the terminal, or one App map), with "is a candidate" a method on it; the
cheap probe becomes `iter().any(ResumeState::is_candidate)`.

### 2.5 Whether this app persists the session / may a save start now

Sites in scope: `AppPolicy::persists_session` is consulted in
`sync_session_save_schedule`, `start_background_session_save`,
`pane_exit_checkpoint_settled`, `request_pane_exit_checkpoint`,
`pane_exit_checkpoint_generation_settled`, `request_host_shutdown_checkpoint`,
`save_session_before_teardown_async` (and the test copies). `SessionSaver::blocked`
adds the host checkpoint's `finished_unsaved` latch. The persister is chosen at
construction as threaded or `lease_only` from the same policy. Outside scope,
`server/headless/lifecycle.rs` writes `app.policy = Suspended`, calls
`session_saver.freeze_session_saves()`, stores the old policy as
`HostShutdownFreeze::persist_session: bool` and restores it via
`restored_policy()`; `headless.rs` also reads `persists_session`.
No site disagrees today, but a `Production` policy over a `lease_only`
persister is representable (`persist_for_test` swaps the persister to avoid it),
and the freeze is two mechanisms (policy flip + saver freeze) that must be
applied together. Owner: `SessionSaver` holds a `SavePolicy { Never, Persisting,
Frozen { resume_to } }` and is the only thing asked; `App` stops carrying
`policy` and the lifecycle calls `saver.freeze()` / `saver.thaw()`.

### 2.6 Whether the preserved pane-exit layout is still authoritative

Sites: `preserves_pane_exit_checkpoint` (`preserved().is_some() && !session_dirty`),
`capture_final_session_save_job` (same filter), `finish_session_save`
(`exit.layout.filter(|_| !session_dirty)`), `PaneExitCheckpoint::would_hold(session_dirty)`
and `request(session_dirty)`, and `finish_checkpointed_pane_exit_after_event`, which
writes `state.session_dirty = false` directly. The root cause is two mutation
channels: `AppState::session_dirty` (set by mutators, consumed once per loop pass by
`sync_session_save_schedule`) and `SessionSaver::note_mutation` (what the saver
actually reasons with). Between them is a window in which the saver's view is
stale, so each site re-reads the flag. Owner: a monotonically increasing
`MutationEpoch` in `AppState`; the saver records the epoch each capture saw and
compares, which makes "has anything changed since the capture" one comparison in
one place and removes the flag writes.

### 2.7 Whether a pane exit is checkpointed before removal

Sites: `prepare_pane_exit` (production) and the fallback in
`handle_internal_event_inner` (`prepared_checkpoint.unwrap_or_else(|| self.pane_exit_needs_checkpoint(..))`),
each also calling `prepare_pane_removal_by_id`. In production every `PaneDied` goes
through `HeadlessServer::handle_internal_event_with_forwarding`, which always
prepares, so the fallback only serves tests and direct callers; it logs a warning
when reached with an unsettled checkpoint. Owner: `PaneDied` applicable only as
`(AppEvent, PreparedPaneExit)`; `handle_internal_event` refuses it by type
(a separate `PaneExit` input), which deletes the fallback.

### 2.8 Did the view change / what must be invalidated

Sites that diff `shell_projection_revision` around a call:
`handle_api_request_with_render`, `handle_endpoint_command_with_render`,
`handle_internal_event_inner`, and (outside scope) three places in
`internal_events.rs` and one in `endpoint_requests.rs`. Sites that pick their own
invalidation triple (`mark_shell_projection_dirty`, `render_dirty.request_generic`,
`render_notify.notify_one`): `set_host_terminal_theme` (no projection mark),
`handle_git_status_refreshed` (all three), the cwd branch of
`handle_internal_event_inner` (render + notify + git refresh), `handle_pane_launch_settled`
failure arm (all three), `sync_pending_terminal_titles` (all three),
`set_host_terminal_appearance_state` (none). Owner: an `Invalidation` value (or a
sink on `App`) returned by every mutator and folded once per call; the revision
diffing then disappears.

### 2.9 What an endpoint command changed

Every handler hand-assembles `EndpointEffects` (6 bools). The truth lives in the
mutators: `close_workspace_at`, `commit_pane_removal`, `toggle_pane_zoom`
(`PaneZoomOutcome`), `focus_pane_in_workspace`, `move_workspace`, `resize_pane`,
`set_split_ratio_at`. `handle_pane_close` re-derives `focus_changed` by snapshotting
focus before and comparing after; `handle_pane_resize` decides that a resize does
not change the shell projection; `handle_workspace_create` and `handle_workspace_close`
each write the same four flags. Owner: outcomes from `AppState` mutators that
`impl From<Outcome> for EndpointEffects`.

### 2.10 Who owns a pane's agent state (hook vs screen)

Sites: `handle_detect_explain` decides "hook authority describes this state" as
`(!full_lifecycle || terminal.full_lifecycle_hook_authority_active()) &&
terminal.state == authority.state`, calling `full_lifecycle_hook_authority` twice
on re-parsed strings. The live decision lives in mux terminal state
(`source/detection.rs`, `source/report.rs`, `lifecycle.rs`, which call
`full_lifecycle_hook_authority` in at least six places), and the detection pause is
pushed to the runtime by `sync_pane_lifecycle_authority_detection_pause`. The
explain answer is reconstructed, not read. Owner:
`TerminalState::state_owner() -> StateOwner { FullLifecycleHook, Hook, Screen }`,
used by detection, the pause, and explain.

### 2.11 Whether an agent report is acceptable

Sites: `normalize_reported_agent_label` and `parse_report_session_ref` in the
server; "TerminalState also validates the label for non-API callers" (comment in
`reports.rs`); `AgentSource::from_pair` in shepr-agent. The schema doc says invalid
references "fail validation before dispatch"; they fail in the app handler.
Owner: one parser in shepr-agent (1.3).

### 2.12 Split ratio clamping

`handle_layout_set_split_ratio` clamps with `SplitRatio::clamped(..).get()` to
compare bits, then passes the raw `f32` to `set_split_ratio_at`, which clamps again.
The "both sides went through the same clamp" comment is the guarantee. Owner: pass
`SplitRatio` (better: make the wire field a `SplitRatio` that refuses non-finite
values at decode, which also removes the `is_finite` check).

### 2.13 The cwd of last resort

`creation::resolve_new_terminal_cwd` (three `"/"` fallbacks), `handle_pane_split`
(`paths.current_dir()` or `"/"` as `default_cwd`), `capture_session_save_job`
(`current_dir()` or `"/"`), and the PTY child's own chdir fallback (HOME, passwd
home, `/`). Owner: `AppPaths::fallback_cwd()` (or one `CwdPolicy`) used by all.

### 2.14 What shell a pane runs

`PaneShellConfig::new(&settings.default_shell, settings.login_shell)` is built in
`App::with_paths` (restore), `create_workspace_without_save`, `handle_pane_split` and
`start_pending_agent_resume` (with `.require_cwd()`). Owner: `AppSettings::pane_shell()`.
Relatedly, spawning goes through `PaneSpawnHandles` for workspaces and splits, but
`start_pending_agent_resume` passes the same four handles to `PaneRuntime::spawn`
individually.

### 2.15 Whether the automatic workspace label is visible

`apply_workspace_git_statuses` sets `changed |= ws.custom_name.is_none()` when the
auto label changes; `Workspace::display_name` encodes the same rule. Small, but
it is the display rule restated in the cache-apply code.

### 2.16 Retry backoff

`Autosave::record_failure` (250 ms doubling to 30 s), `checkpoint_retry_delay`
(250 ms doubling to 1 s), `App::create_default_workspace` (250 ms doubling to 30 s,
kept as two `Option`s), and the logind reconnect backoff in `server/`. The
autosave and default-workspace pairs have identical values under different
constant names. Mostly duplicated code, but a `Backoff { min, max }` value type
would also make the two-`Option` state of the default-workspace retry one field.

### 2.17 Pane id to runtime, and "has the pane's child exited"

`workspaces.iter().enumerate().any(|(i, _)| runtime_for_pane_in_workspace(.., i, pane))`
appears in `admit_runtime_event`, `pane_exit_needs_checkpoint` and the detector-drop
guard in `handle_internal_event_inner`; `find_pane`, `update_terminal_state`,
`sync_pane_lifecycle_authority_detection_pause` and `TerminalCwdReported` each
re-walk workspaces for pane -> terminal. An `AppState::terminal_of(PaneId)` and
`runtime_of(PaneId)` (with an index if it matters) gives one answer.

---

## 3. Structure

### 3.1 `App` is an open bag the server reaches into

Almost every field is `pub(crate)` or `pub`. The server loop writes `app.policy`,
calls `app.session_saver.freeze_session_saves()`, sets `app.state.should_quit`,
drains `app.event_rx`, reads `app.clock`, `app.terminal_runtimes`,
`app.last_render_at`, drains `app.runtimes_replaced_panes`. Render cadence
(`last_render_at`, `last_presentation_at`, `can_render_now`, `can_present_now`,
`next_headless_loop_deadline_with_git_refresh`) is loop state living on `App`.
Suggested split:
- `Session` (today's `AppState` plus its mutators, returning outcomes),
- `Runtimes` (registry, spawn handles, teardown tracker, render signals),
- `Persistence` (`SessionSaver` with its own policy, 2.5),
- schedulers the loop owns directly (git refresh, resume schedule, default
  workspace retry, render cadence).
`App` methods that coordinate them return effects instead of poking signals.

### 3.2 `AppState` is "pure data" without enforced invariants

- Public fields (`terminals`, `workspaces`, `bookmark`, `session_dirty`,
  `should_quit`, `host_*`, `next_agent_state_change_seq`). The bookmark doc says
  every write goes through two setters, but `bookmark` is `pub`. A `Bookmark`
  type with private `(id, last_position)` would enforce it.
- Terminals live in a global `HashMap<TerminalId, TerminalState>` beside the
  panes that attach them. The invariants (every pane has a terminal, no terminal
  is shared) exist only in `assert_invariants_for_test`. `remove_unattached_terminal_ids`
  scans every pane of every workspace per terminal to defend against sharing that
  never happens. Owning `TerminalState` from `WorkspacePane` (or a store keyed by
  pane) removes the scan, `ensure_test_terminals`, and the "pane attached to a
  missing terminal" state.
- `should_quit` is written only by `HeadlessServer::initiate_shutdown`, which also
  sets the lifecycle phase to `Stopping`; `stop_requested(app_quit)` checks both.
  It is a dead duplicate of the lifecycle phase living in pure app data.
- `AppState` depends on `shepr_termio::host_term` types (`TerminalTheme`,
  `HostAppearance`, `HostCellSize`) and on `ratatui::layout::Rect`. These are host
  and wire facts; owning them in termio (a terminal-input crate) pulls it into the
  pure state. They belong in core or protocol.
- `AppSettings` copies a dozen fields out of `ValidatedServerConfig`; it could
  hold the validated sections plus the few derived values.

### 3.3 `shepr_mux::events::AppEvent` mixes producers

Variants: runtime-origin events (`PaneLaunchSettled`, `PaneDied`,
`AgentProcessDetected`, `StateChanged`, `ClipboardWrite`, `TerminalCwdReported`),
a recursive `Runtime { pane_id, generation, event: Box<AppEvent> }` envelope, a
server worker completion (`GitStatusRefreshed`, produced by `app/git_refresh.rs`),
and two API-origin reports (`HookStateReported`, `AgentSessionReported`, produced
only by `app/api/panes/reports.rs`, which wraps them only for
`StateEvent::from_app_event` to unwrap them). The envelope is optional:
`admit_runtime_event` passes bare runtime events through unchecked, and the mux
`EventSender::origin` is an `Option`. The App drops `ClipboardWrite`; the server
handles it before the App sees it. Proposed shape:
- mux emits `RuntimeEvent { pane, generation, kind: PaneRuntimeEvent }` with the
  envelope mandatory and not nestable;
- Git completions are a server-local worker result type;
- API reports go straight to `StateEvent` from the parsed `AgentReport`;
- `StateEvent` becomes the App's input type rather than a re-mapped copy.

### 3.4 Git work and Git cache application live in the app

`app/api/checkout_root.rs` runs `git rev-parse` and parses stderr;
`app/git_refresh.rs` holds deduplication, the worker body, cache bookkeeping and
panic containment; `AppState::apply_workspace_git_statuses` writes six public
`cached_*` fields of mux `Workspace`. All of it is mux Git logic. Move to
`shepr_mux::git` with `Workspace::apply_git_status(result) -> bool`; the app keeps
only scheduling. `GitStatusRefreshDemand` is always `ALL` here ("keep this demand
full when porting upstream"); the `result.demand.branch` / `.ahead_behind` checks
are dead branches. Delete the axis.

### 3.5 Agent report parsing belongs with the agents

`agent_report_test_support.rs` (`AgentReportHarness`) and the contract test exist in
shepr-server only because report acceptance lives in the server's handlers. With
the parser in shepr-agent (1.3, 2.11), the contract test can sit beside the assets
it covers and drive the parser plus `TerminalState` directly, and the harness goes.

### 3.6 Odd dependency edges

- shepr-server depends on shepr-remote (the SSH crate) for one function,
  `interactive_shell_command`, used by `start_pending_agent_resume` to quote the
  resume argv. Shell quoting belongs in shepr-platform, or the `AgentResumePlan`
  should carry its shell text.
- `crossterm` is used in `app/state.rs` only for a test helper `key_matches` that
  tests `shepr_config::terminal_key_matches_combo` (client key config). It does
  not belong in the server's app state module.

### 3.7 The ui seam vs the patch renderer

`ui` is the pure render of one workspace for one client, but the server's
`retained_surface` patch path reimplements layout validation, scrollbar
visibility, gutter placement and cursor computation (2.1 to 2.3), importing only
`render_pane_scrollbar_buffer` and `pane_is_scrolled_back` from ui. The real seam
is "how a pane looks" (ui) vs "which bytes go to which client" (server). Move the
per-pane presentation decisions into ui as a `PaneSurface` description that both
the full render and the patch diff consume. `resize_surface` / `PaneResizer` (whose
doc admits it is not a barrier) is geometry application, not rendering, and fits
better with the geometry owner of 2.1.

### 3.8 `limits.rs` is a crate-wide grab-bag

Most constants serve `server/` (handshake, client queues, outbox frame counts,
shutdown flush, logind reconnect backoff, endpoint id bounds, held replies). The
app-only ones (session save debounce/retry, checkpoint failures, git intervals,
resume theme wait, copy query limits, `DEFAULT_PANE_RESIZE_AMOUNT`) would read
better beside their owners. `DEFAULT_PANE_RESIZE_AMOUNT: f32` is a split-ratio step
and should be typed as one.

### 3.9 Copy-mode motions split across layers

Word and paragraph motions are `PaneRuntime` methods in mux; the line motion
(`End`, `FirstNonBlank`) is composed in `handle_pane_copy_motion` from
`extract_selection` plus `shepr_termio::copy_mode` helpers, and the paragraph
result's column is patched in the handler. One `PaneRuntime::copy_motion(cursor,
CopyMotion) -> Point` would hold all of it.

### 3.10 Endpoint dispatch receives commands it must refuse

`dispatch_endpoint_command` matches `ClientShellSurfaceSet` and
`WorkspaceCheckoutRoot` only to log a routing bug and reject. Splitting
`EndpointCommand` into `LoopCommand` and `AppCommand` (the loop routes the
former, the app's dispatcher takes only the latter) makes the misroute
unrepresentable.

### 3.11 Smaller placement and naming issues

- `create_workspace` is a pass-through to `create_workspace_without_save`; both
  mark the session dirty (via `commit_workspace_creation`), so "without save" is
  a stale name.
- `window_title_template` is set by `configure_validated_window_title` after
  construction though config is immutable; `hostname` is a `String` field. The
  window title is a pure function of state, template and hostname and fits
  `AppSettings` plus a ui function.
- Two `SessionSnapshot` types: `app::api::session::SessionSnapshot` (the shell
  projection input) and `shepr_mux::persist::SessionSnapshot` (the saved file).
- `logging.rs` takes ids as `&str` / `u32` (`workspace_created(&str, u32)`), so
  callers deref the typed ids (4.1, 4.2).
- `api_helpers.rs` is `pub(crate)` while holding one `pane_not_found` wrapper and
  three enum mappings (1.11).
- `lookup_runtime` returns a `WorkspaceId` neither caller uses.
- `live_host_theme_reported()` is a private method returning a `pub(crate)` field.

### 3.12 Test layout mirroring accidents

- `save_session_before_teardown` (cfg(test)) duplicates the production
  `save_session_before_teardown_async` synchronously; `save_session_now` is another
  test-only save path. Tests that use them verify a copy, not the shipped path.
- `handle_internal_event_after_checkpoint` (cfg(test)) reimplements the server's
  hold-and-replay of checkpointed exits with a fixed `for _ in 0..4` retry loop.
  Session tests run against that reimplementation rather than the loop's.
- `AppPolicy::Test` is an alias of `Suspended` spelled as a const.
- `test_support.rs` defines fixture traits for mux types (`PaneRuntimeFixture`,
  `WorkspaceFixture`, `TerminalStateFixture`) because shepr-test-fixtures sits
  below mux. Other crates above mux that need them will duplicate them.

---

## 4. Types that resolve to primitives

### 4.1 `WorkspaceId` and `PublicPaneId`

Both `Deref<Target = str>`, implement `PartialEq<str>`, `PartialEq<&str>`,
`PartialEq<String>`, and `WorkspaceId` has `From<WorkspaceId> for String`;
`number() -> usize` hands out the allocator number. Escapes used in scope:
`ws.id.to_string()` into `WorkspaceGitRefreshItem` (1.2);
`workspace.id == *workspace_id` against `&str` in `live_workspace_identity_cwd`;
`workspace_geometry` keyed by `id.number()` (1.1); the retained layout cache keyed
by `number()`; `logging::workspace_created(&outcome.workspace_id, ..)` relying on
deref; `parse_pane_id(&pane_id)` re-parsing a typed `PublicPaneId` through deref in
tests. Offer instead: `Hash`-keyed maps by the id itself, a `tracing::Value`/`Display`
for logging, and drop `Deref` and the `PartialEq<str>` family so no caller compares
text.

### 4.2 `PaneId::raw() -> u32`

Used for log fields (`pane_id = root_pane.raw()`, `pane = pane_id.raw()`), as a map
key in `PreservedLayout::terminal_ids` (1.13), and in id tests. Offer `Display` /
`tracing::Value` and key maps by `PaneId`.

### 4.3 `AgentSource -> String`

`AgentSource::as_str()` / `to_source_string()` lower the typed source into
`HookAuthority::source: String`, and `is_reserved_native_state_source(source.as_str(), ..)`
takes text. `AgentSource::Official(agent).as_str()` is
`integration_source().unwrap_or_default()`, so an official agent without an
integration source becomes `""`. Store `AgentSource`; make predicates take it.

### 4.4 `Agent::label()` compared as text

`surface_cursor`: `configured.label() == agent.label()` where `ConfigAgent` is a
re-export of `shepr_agent::agent::Agent`, i.e. the same enum. Compare values.
`parse_report_session_ref`: `agent.label() != agent_label`.

### 4.5 `SplitRatio::get() -> f32`

`handle_layout_set_split_ratio` unwraps to `f32` to compare `to_bits()`, then hands
the raw input to `set_split_ratio_at(&path, f32)`. Give `SplitRatio` `PartialEq` and
make the setter take it (2.12).

### 4.6 `CursorShapeParam` via `u8`

See 1.8: typed config shape -> `u8` -> `from_decscusr` with a lossy default arm.

### 4.7 `HostCellSize` public pixel fields

`resize_pane_infos` passes `cell_size.width_px` / `height_px` straight into
`PaneGeometry::new`, while spawn sizing goes through `SpawnGeometry::cell_px()`,
which maps 0 to `None` via `CellPx::new`. The same "unknown cell size" value is
converted two ways on the two paths that size the same PTY. Store `Option<CellPx>`
and offer only that.

### 4.8 `AgentState` and its presentation

`record_agent_state_change_seq` compares `presentation_state()` of two states to
decide whether the sequence advances; `pane_agent_status` maps through
`presented_agent_status`. The presentation set (`PresentedAgentState`) and the wire
set (`AgentStatus`) are the same three values (1.11).

### 4.9 Scroll metrics

`pane_info` casts `ScrollMetrics` fields `as u64`; `handle_pane_scroll` does
`usize::try_from(offset).unwrap_or(usize::MAX)`; `handle_pane_copy_search` clamps
counts with `try_from(..).unwrap_or(u64::MAX)`. A wire-side scroll metrics type
built by one conversion would keep the casts in one place.

---

## Lateral findings (bugs, smells, perf)

- **Possible missed render on appearance change.** `set_host_terminal_appearance_state`
  updates every runtime but requests no render and marks nothing dirty, while
  `set_host_terminal_theme` does both. `HeadlessServer::promote_client_to_foreground`
  also discards the `changed` result of `sync_host_theme_from_foreground`. If
  appearance affects drawn colours, an appearance-only change on foreground
  promotion waits for an unrelated render. Worth verifying.
- **Instant underflow.** `GitRefreshScheduler::new` and `mark_due` compute
  `now - GIT_REMOTE_STATUS_REFRESH_INTERVAL`. `Instant - Duration` panics on
  underflow; the comment assumes the monotonic clock is never within 1.5 s of its
  origin "by the time a server runs". A server started by a unit very early after
  boot is the case that breaks it. Use `checked_sub` or the enum from 1.5.
- **Degenerate pane size disagreement** between `PaneGeometry::pane_size` (clamped
  to 1) and the ui content rect (unclamped), see 2.1.
- **Stale doc in shepr-api schema**: "An explicitly supplied invalid official
  reference fails validation before dispatch"; it fails inside the app handler.
- **Dead flag**: `AppState::should_quit` (3.2).
- **Dead axis**: `GitStatusRefreshDemand` (3.4).
- **Hot-path repetition**: per client per frame, `compute_surface_for`,
  `render_panes` and `surface_cursor` each resolve the target `WorkspaceId` to an
  index by linear scan; `SurfaceLayout` could carry the resolved index.
  `Workspace::display_name()` clones a `String` per call (window title, every
  `workspace_info` in every session snapshot).
- **Per-iteration scans**: `start_pending_agent_resumes` runs every loop iteration
  and starts with `has_pending_agent_resumes` (scan of all terminals) and a
  `retain` over `pending_resume_commands`; cheap today, but it is work proportional
  to session size on every wakeup.
- **Quadratic removal**: `remove_unattached_terminal_ids` is terminals times panes
  (3.2).
- **Nestable envelope**: `AppEvent::Runtime` can wrap another `Runtime`, and the
  inner event repeats the outer `pane_id`; `admit_runtime_event` recurses.
- **Panic-capable index**: `handle_layout_set_split_ratio` indexes
  `self.state.workspaces[ws_idx]` while every other handler uses `.get`. Safe today
  only because the index was resolved a line earlier.
- **Double resolve**: `handle_detect_capture` parses the pane id to `(ws_idx, pane)`
  and then rebuilds the same `PublicPaneId` with `public_pane_id`.
