# Consolidations from the design hunt

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

One domain question answered independently in more than one place. The unit
of an entry is the question: every site that answers it belongs to the one
entry, with whether the sites already disagree and where the single owner
should live. Unverified: the raw reports are in the commit that precedes this
file's.

## Platform, process and files

## CON-001 - Does this IO error mean the peer is gone?

| Site | Kinds |
|---|---|
| `shepr-platform/src/ipc.rs` `is_connection_closed_error` | BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected, UnexpectedEof, WriteZero |
| `shepr-platform/src/remote_bridge_io.rs` `is_closed_socket` | BrokenPipe, ConnectionReset, NotConnected |
| `shepr-client/src/shell_runtime.rs` `endpoint_disconnect_notice` | UnexpectedEof, BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected |
| `shepr-api/src/server_stop.rs` `stop_request_error_allows_wait` | BrokenPipe, ConnectionReset, UnexpectedEof, NotConnected, TimedOut, WouldBlock |
| `shepr-api/src/status.rs` `status_probe_has_no_answer` | ConnectionRefused, NotFound, BrokenPipe, ConnectionReset, UnexpectedEof, NotConnected, TimedOut, WouldBlock |
| `shepr-remote/src/lib.rs` `is_ssh_link_error_kind` | TimedOut, ConnectionRefused, ConnectionReset, AddrInUse, Host/NetworkUnreachable, NetworkDown |

They already disagree (ConnectionAborted and WriteZero in one, not another).
`status.rs` has a pairwise test
(`launch_and_stop_share_status_transport_failure_classification`) holding two
in step. Owner: shepr-platform, `classify_stream_error(&io::Error) ->
StreamFailure::{PeerGone, NoListener, TimedOut, Other}`, each consumer matching
the arms it cares about. (foundation)

## CON-003 - Parsing `/proc/<pid>/stat`, and "is this process dead"

`process.rs` `session_and_tty_from_stat` (skip past the last `)`, then
`skip(3)`), `process_identity.rs` `process_snapshot` (its own split, `nth(18)`,
dead means `Z | X`), and in shepr-agent `proc_tree.rs`
`foreground_process_group_id` (field 5) and
`process_pgrp_comm_and_state_from_stat` with
`process_state_allows_remote_memory_read` refusing `D | Z | X | x`. Field
arithmetic is repeated with different offsets and the finished states differ
(`x` only in agent). Owner: shepr-platform `ProcStat::read(Pid)` with typed
fields and `ProcState::is_finished()`. agents adds that all of `proc_tree.rs`
(task and children walking, cmdline, cwd readlink, budgets) is `/proc`
plumbing that AGENTS.md puts in platform, and mux reaches through
`shepr_agent::detect::` for it (`foreground_process_group_id`, `process_cwd`,
`foreground_job`, `foreground_group_leader_job`). Reported by foundation and
agents.

## CON-004 - What is a valid split ratio, and how is one clamped?

`SplitRatio::new` and `SplitRatio::clamped` in core (non-finite becomes
`EVEN_SPLIT`); `sidebar_tokens.rs` `SectionSplit` (same bounds, same finite
check, same fallback, its own serde); client `mouse.rs` `pane_split_ratio`
clamping before sending; `handle_layout_set_split_ratio` rejecting non-finite,
clamping through `SplitRatio::clamped(..).get()` to compare bits, then passing
the raw `f32` to `set_split_ratio_at`, which clamps again (the "both sides went
through the same clamp" comment is the guarantee). They agree because the
constants are shared. Owner: `SplitRatio` on the wire and in `SectionSplit`.
Reported by foundation, server-app and client-shell.

## CON-005 - When is a cell size exact, how big may it be, and how small may a host grid be?

"Pixel coordinates are exact only with a known cell" is decided in
`HostGeometry::new` (core), `TerminalGeometry::new` and its `TryFrom` (protocol)
and `ProtocolCellSize::from_host`, which adds a `MAX_CELL_SIZE_PX` bound core
does not know: a 100000 px cell is exact to core and not to protocol. The cell
size limit is three policies: `client_shell_geometry_error` refuses,
`ProtocolCellSize::from_wire` nulls, `ProtocolCellSize::from_host` clamps. The
minimum host grid is decided by `GridSize::clamped` (one cell),
`HostGeometry` through `PaneGeometry` (4 by 2) and `ClientHostSize::new` through
`ClientSurfaceSize::clamped`; two disagree (filed as a bug). The grid upper
budget is filed separately. Owner: core `HostGeometry` with its own grid rule and
a `CellPx` that owns the pixel bound. Reported by foundation and server-serving.

## CON-009 - How is a private file published durably and atomically?

`persist/io.rs` `publish_private_file` (private temp, copy, fsync, rename, sync
dir, `Published::{Durable, NotDurable}`, no symlink check at the target);
`machine/ssh_metadata.rs` `store_private_json_with_directory_sync` (refuses a
symlink or non-file target, its own temp naming with `unpredictable_token` and a
sequence, dir sync failure only logged); `integration/atomic_replace.rs` with
platform's `config_file.rs` (copying owner, ACL xattrs and mode). Temp naming,
symlink policy and the meaning of a failed dir sync differ. Owner: platform
`publish_file(target, contents, Options { preserve_metadata_from,
refuse_symlink_target, durability })`; `config_file.rs` is the start of it but is
named for one consumer. (foundation)

## CON-013 - The socket path type is thrown away after the check

`SocketPath` in `crates/shepr-core/src/socket_path.rs` owns the length check
and the platform connect uses it, but `resolve_paths_checked`
(`crates/shepr-config/src/address.rs`) validates and drops it, so `AppPaths`
and `ServerAddress` still hold a plain `PathBuf`. Carry `SocketPath` there so
the check is a type fact. (wave-3 review)

## CON-015 - What does a pane's exit mean, and does it get a checkpoint?

pty `ReaderExit::{ShutdownRequested, Closed, IoFailed, Panicked}` (severity by
derive order), platform `ChildExitReason` with `requires_session_checkpoint`,
mux `reader_exit_callback` mapping one onto the other; "does this exit get a
checkpoint" is `requires_session_checkpoint` plus a second clause in
`shepr-server/src/app/events.rs` `pane_exit_needs_checkpoint` ("and the core is
not broken"), plus the method again in mux `transition_pane_exit` and
`CheckpointCandidate::qualifies`. AGENTS.md says checkpoint policy lives in
shepr-server; the method lives in platform. Owner: one `PaneEnding` in mux next
to the exit arbiter, carrying the reason and whether the core is intact, with a
single `needs_checkpoint()`; platform keeps only `ExitKind::{Exited(code),
Signalled(sig)}`. (foundation)

## CON-016 - Whitespace in shell values

`env.rs`'s doctrine is that interpreted values refuse surrounding whitespace.
`SHELL` is `Raw`, then `shepr-core/src/shell.rs` `trim_shell_value` trims it
(with a non-UTF-8 path that is moot because `shell_path_string` refuses non-UTF-8
afterwards); `terminal.default_shell` is trimmed in `validated.rs`;
`PtyCommand::interactive_shell` trims again. Owner: config validation, once,
under one rule. (foundation)

## CON-018 - What does a child's environment contain, and under which policy?

`ChildEnv` documents "every name shepr writes into a child in one place", but
mux writes `SHEPR_ENV`, `SHEPR_SOCKET_PATH`, `SHEPR_BUILD_PROFILE`,
`SHEPR_PANE_ID` and `TERM_PROGRAM` through `EnvVar`, pty writes `PWD` and
removes `OLDPWD` by literal, `PtyCommand::env` accepts any `AsRef<OsStr>`, and
`SHELL` is set in `interactive_shell` and overwritten in `launch_spec`. mux
`pane/launch.rs` decides a pane policy per `EnvVar` (`pane_env_policy`) and per
`ChildEnv` (`pane_child_env_policy`); `SHELL` and `PATH` are in both, held in
step by the pairwise test `a_name_in_both_vocabularies_has_one_pane_policy`.
Which agent owns which variable (`ClaudeConfigDir`, `CodexHome`, ..., and session
markers stripped from panes) is a fact about the agent, split from the
descriptor and from `integration/env.rs`, which maps variables to directories by
hand. Owner: one registry where each name appears once with its pane policy as a
property, `PtyCommand` taking registered names for everything shepr sets, and the
descriptor naming its `config_dir_override` and `session_markers` so the
stripping and integration-path lists derive from the agents. Reported by
foundation, mux-panes and agents.

## CON-019 - Where is the server log, and where are the XDG directories?

`data_dir.join(SERVER_LOG_FILE)` is composed in `bootstrap.rs` (twice),
`remote/local_server.rs` and implicitly in `logging::help_log_paths_summary`;
owner `AppPaths::server_log()`. `XDG_CONFIG_HOME` is read by
`shepr-config/src/io.rs` and again per call by mux `git/config.rs`
`git_user_config_paths_at` (errors swallowed). The integration config lock
directory (`integration/env.rs` `resolve_config_update_lock_dir`) recomputes the
XDG state home and hard-codes `"shepr"` independently of config's
`SHARED_APP_DIR_NAME`, and `devin_dir`, `opencode_dir`, `kilo_dir` and
`opencode_state_dir` each re-decide "XDG var or `~/.config`/`~/.local/state`".
Owner: shared `xdg_config_home()`/`xdg_state_home()` in core or platform.
Reported by foundation and agents.

## Terminal emulation and input

## CON-022 - Does a mouse report use pixels or cells, and is pixel mouse eligible?

Client `mouse.rs` sends pixels when `hit.sgr_pixel_mouse && hit.pixel_width >
0`; server `pane_input::downgrade_ineligible_pixel_mouse` downgrades unless the
client geometry equals the runtime grid and pixel extent;
`apply_client_pane_input_event` maps `Pixels` only if
`runtime.sgr_pixel_mouse_enabled()`; mux `encode_mouse_event` maps cells to
pixels through `cell_pitch` under 1016 and back otherwise; the server's connect
and resize arms decide `pixel_mouse && observed.is_known()`; the
`ClientShellPaneInput` arm uses `client.pixel_mouse &&
outbox.told_sgr_pixels()` (the outbox's dedupe memo as an authority);
`stream_host_mouse_capture_mode` uses `client.pixel_mouse &&
runtime.sgr_pixel_mouse_enabled()`; protocol's `TerminalGeometry` and
`ProtocolCellSize` decide again. They agree only because each re-checks the same
mode bit, each under its own lock. Owner: pixel mode as one value on the connection,
recomputed when its inputs change, and one function next to the mouse protocol
type that takes the pane's pixel extent and the report. Reported by terminal and
server-serving.

## CON-023 - How big is a pane in pixels?

vt `PaneGeometry::text_area_px()` (drives `CSI 14 t`, the 2048 report and
`width_px()/height_px()`); server `client_shell.rs` `inner_rect.width *
HostCellSize.width_px` with `(0, 0)` as unknown, published as
`PaneSurfacePane.pixel_width/height`; mux `cell_pitch` (`width_px() / cols`, at
least 1). They agree only while the pane's cell equals the client's and the grid
equals `inner_rect`; with several clients of different cell sizes, the server
publishes per-client extents while the child was told the geometry-source
client's. Owner: `PaneGeometry`, with the wire carrying `Option<PixelExtent>`
from it. (terminal)

## CON-024 - Where is the viewport, counted from the bottom?

vt `scrollbar()` returns from-top numbers; mux `terminal_scroll_metrics`,
`terminal_set_scroll_offset_from_bottom` and an inline copy in
`PaneTerminal::resize` each compute `total - (offset + len)` and `total - len`;
termio `ScrollMetrics::viewport_top_row` computes the inverse; the client's copy
mode reimplements it as `viewport_top`; four field-by-field conversions run
between termio's and protocol's scroll metrics. Owner: vt returning one
`ScrollMetrics`, also the wire type. Reported by terminal and client-shell.

## CON-027 - How are styles, colours and colour replies spelled in VT sequences?

`UnderlineStyle` to SGR is in `format.rs` `UNDERLINE_SGR` and `blit.rs`
`style_to_sgr_parts`; colour numbering in `format.rs` `push_color` and `blit.rs`
`color_to_sgr_fg/bg`. OSC colour replies: vt `color_query_format` (`{:02x}{:02x}`)
and mux `osc_rgb_response` (`{:04x}` of `x * 257`, with its own target-to-OSC
mapping), and termio `host_term/theme.rs` parses the same form. Focus `CSI
I`/`CSI O` in vt `encode_focus` and termio `raw_input`; DSR 997 reports in vt
`ColorScheme::report()` and termio constants, DSR 996 in `scan.rs` and
`HOST_COLOR_SCHEME_QUERY_SEQUENCE`; modifyOtherKeys `CSI > 4 ; n m` in vt
`apply_scan_event`, handler `report_modify_other_keys`,
`set_host_keyboard_protocol` and `HOST_MODIFY_OTHER_KEYS_RESET_SEQUENCE`; kitty
flag bits in vt (inline), protocol `KittyKeyboardFlags` and crossterm
(`from_bits_retain`); DEC mode numbers as literals in `blit.rs` and
`host_term/modes.rs` although `DecMode::number()` exists; bracketed paste markers.
All agree today. Owner: `UnderlineStyle::sgr_param()`, a small shared SGR writer,
`ColorQuery::reply(color, ReplyForm)` in vt, and a `seq` module of typed builders
and matchers used by both the pane side and the host side. (terminal)

## CON-028 - The mouse button byte is encoded and parsed by mirrored code

Functional keys, key modifier bits and control-byte aliases now come from one
table (`crates/shepr-termio/src/input/tables.rs`) read in both directions. The
mouse button and modifier byte is still written twice: `encode_mouse_cb` in
`input/encode.rs` and `parse_mouse_cb` in `input/raw_input.rs`, held in step only
by round-trip tests. The parser legitimately accepts forms the encoder never
emits (release information and extended motion), so a shared table covers only
the common part. (terminal)

## CON-029 - What character does a key produce?

`copy_mode_command_char` (also used by `keybind_help_text_char`), encode's
`text_char_for_key`/`shifted_text_char`/`is_shifted_ascii_punctuation`,
`TerminalKey::with_text_commit` (uppercase means Shift), parse's
`parse_legacy_key_sequence`, keybindings' `generated_character_key`, and
`BindingKey::canonical_key`. Two pairwise tests keep some in step. The context
policies legitimately differ (copy mode applies the US shift table to letters
too), but "Shift plus this base key gives which char" should be one
`TerminalKey::produced_char()` with each context's policy on top. (terminal)

## CON-030 - How wide is text?

vt `unicode_codepoint_width`/`unicode_text_width` (grid widths with the
voiced-mark override); termio `blit::text_width` (and aliases `symbol_width`,
`cell_width`: grapheme width plus one per U+FF9E/U+FF9F, hard-coding the
codepoints instead of using vt's predicate); mux `terminal_buffer_symbol_into`
(measures `symbol.width()` against `CellWide` with vt's predicates); client
`word_bounds` (re-measures row text with `unicode_codepoint_width`). The grid and
grapheme rules differ on purpose, but the voiced-mark rule is written twice and
the predicates three times. Cell width from `CellWide` is mapped four times in
`helpers.rs` (`terminal_blank_symbol_for_width`, `terminal_grid_width`, the
`expected_width` match in the render path and its test twin) and a fifth in
`text.rs` `TextBufferBuilder::push_cell`. Owner: one width module exposing both
rules by name, and `CellWide::columns()`/`grid_width()`. Reported by terminal and
mux-panes.

## CON-035 - Where does an OSC end?

vte decides; `scan.rs::Scanner` mirrors vte's framing to find working directory,
progress and oversized OSCs; mux `osc.rs::OscStreamCollector` mirrors it a third
time for the opt-in debug log (and says so). If the collector drifts, the debug
log shows sequences the terminal did not see. Owner: the scanner, emitting an
`OscBody` event when the debug log is on. Reported by terminal and mux-panes.

## CON-037 - Synchronized output: two notions, one draw gate

DECRQM ?2026 answers from `ExtraModes.synchronized_update` (replay order) and
`mode_get(SynchronizedOutput)` from the parser deadline. This is documented and
correct; the terminal hunter asks only that the two be named apart
(`sync_update_in_replay`, `sync_update_buffering`) so nobody unifies them.
Separately, `PaneTerminal::render_into` and `collect_dirty_patch_snapshot`
return early while mode 2026 is set, and the server checks
`synchronized_output_active()` or `synchronized_output_state()` before calling
them (`ui/surface.rs`, `retained_surface.rs`, `client_shell.rs`): a deliberate
double check today because `render_into` returning `()` cannot say whether it
drew. Owner: a typed draw result (`Drawn | Deferred | Unreadable`) read once under
one lock. Reported by terminal and mux-panes.

## CON-038 - What is a displayable title, and what is a word?

Title: vt cuts at `MAX_TITLE_BYTES` (bytes), mux `sanitize_agent_osc_string`
filters controls and caps at `AGENT_OSC_MAX_CHARS` (chars), termio
`write_window_title` strips controls again for the host; config's
`sanitize_window_title_text` is render-time sanitizing used by the server. Each
has a reason, but nothing owns the rule. Word: mux `text_class`
(`COPY_MODE_WORD_SEPARATORS`) for copy-mode motions, client
`word_bounds::is_word_separator` (including CJK punctuation) for double-click,
and `TextEditor::word_boundary`. The first two apply to the same pane text and
differ; whether that is intended is written nowhere. Reported by terminal,
client-shell and contracts.

## Agents and hooks

## CON-041 - Who decided this pane's state: hook or screen?

`handle_detect_explain` reconstructs after the fact whether hook authority
decided the effective state: `(!full_lifecycle ||
terminal.full_lifecycle_hook_authority_active()) && terminal.state ==
authority.state`, calling `full_lifecycle_hook_authority` twice on re-parsed
strings. If screen detection lands on the same state as the hook, explain
credits the hook. The live decision is in mux terminal state (`source/detection.rs`,
`source/report.rs`, `lifecycle.rs`, which call `full_lifecycle_hook_authority` in
at least six places). Owner: `recompute_effective_state` records
`EffectiveStateSource::{FullLifecycleHook, Hook, Screen, ProcessExit}` (server-app
names it `TerminalState::state_owner()`), used by detection, the detection pause
and explain. Reported by agents and server-app.

## CON-042 - Full-lifecycle authority is mirrored into the runtime by hand

`TerminalState::full_lifecycle_hook_authority_active()` is derived state; the
server copies it into `PaneRuntime::full_lifecycle_authority_active:
Arc<AtomicBool>` through `sync_pane_lifecycle_authority_detection_pause`, called
after `handle_state_event` for the touched pane and after
`publish_pane_process_exit`. Other mutations (`set_persisted_agent_session`
during restore, `abandon_agent_resume`, a fresh runtime for a terminal that
already has authority) do not call it, so the copy is correct only when the last
mutation went through one of the two synced paths. Owner: report the change in
`TerminalStateMutation`, apply it at `update_terminal_state`, and set it on
runtime installation. (mux-panes)

## CON-049 - Hook asset contracts are spelled in every asset

Each of the 16 assets spells the environment gate (`SHEPR_BUILD_PROFILE =
release`, `SHEPR_ENV = 1`, `SHEPR_SOCKET_PATH`, `SHEPR_PANE_ID`), the method and
param names, the action vocabulary (`IntegrationHookAction::as_str`), the
`<source>:<seq>` id, the 500 ms socket wait, and the descriptor's source and
label. Several re-check the event-to-action map: Codex's `expected_events` is
`CODEX_HOOK_EVENTS` in Python, Devin hard-codes `("SessionStart",
"UserPromptSubmit")` (the descriptor says "session action" and the asset says
"only these two events"), Claude and Cursor check their one event. Pairwise tests
hold them (`hook_assets_share_one_envelope`,
`bundled_integration_assets_report_the_descriptor_identity`, the bun traces). Kilo
and the OpenCode plugin duplicate `SESSION_STATE_BY_STATUS`,
`CHILD_EVENT_STATES`, `sessionIDFromProperties`, `stateFromSessionStatus` and the
transport (and have already drifted: the cycle guard filed as a bug); Pi and OMP
duplicate about 150 lines of transport, sequence and queue code. Owner: a
generated preamble per language carrying the envelope, gate, identity and event
map from the descriptor, with the agent-specific decoder appended. The module
comment argues against templating because decoders differ; that holds for the
decoders only. (agents)

## CON-050 - Is this persisted session resumable, and is this launch a resume?

Resumability: `PersistedAgentSession::new` (custom sources allowed), `plan`
(official only) and `AgentResumePlan::with_argv` (non-empty argv) each check, the
snapshot path bypasses `new` through derived `Deserialize`, and
`foreground_agent_confirms_session_owner` builds an argv as a predicate (the type
is filed among the types). Resume state lives in four places:
`TerminalState::pending_agent_resume_plan`, `App::pending_resume_commands`
(pruned by `retain` every pass), `PaneLaunchEnv::purpose` and
`PaneShellConfig::require_cwd`; runtime presence in the registry is a fifth.
Candidates are decided in `has_pending_agent_resume_candidates`,
`pending_agent_resume_candidates`, `pane_awaits_agent_resume`, an inline check
inside `pending_agent_resume_candidates`, `has_pending_agent_resumes`, and
`handle_pane_launch_settled`, which decides how to record a failure from
`resume_command.is_some() || terminal.pending_agent_resume_plan.is_some()` rather
than from the runtime's own purpose; held together by the pairwise test
`candidate_probe_agrees_with_the_collected_candidates`. Owner: a `ResumeState::{
Planned(plan), Launching { plan, command }, ..}` on one store with "is a
candidate" as a method, and the launch kind carried by the runtime and returned
with the settlement. Reported by agents, mux-panes and server-app.

## Pane runtime and workspace

## CON-051 - Is this observation still about our live child?

`ChildLiveness::live_pid()` is the declared gate, but the "sample pid, do the
/proc read, check the pid is still live" protocol is re-implemented in
`PaneRuntime::cwd`, `follow_cwd`, `foreground_cwd` (twice), `PaneCwdProbe::read`,
`publish_reported_cwd`, `PaneTerminal::maybe_restore_host_terminal_theme`,
`resolve_default_color_owner`, and `DetectionTask::live` (eight times in
`tick`). Two later gates answer a neighbouring question with different facts:
the server drops `StateChanged`/`AgentProcessDetected` when
`child_has_exited()` (which counts a zombie), and
`TerminalState::transition_detector_observation` drops them once `pane_ended`
(the applied exit) is set; they agree on today's paths by event ordering. Owner:
`ChildLiveness::observe(|pid| ..) -> Option<T>`, and for detector events the
runtime (the detection task stops and the coordinator drops later output once the
exit arbiter has decided). (mux-panes)

## CON-052 - Is the pane shell in the foreground?

`follow_cwd_from_processes` (`shell_pid != foreground_pgid`),
`osc::foreground_job_is_shell` (membership), `process_probe_result` and
`probe_foreground_process_from_jobs` (the membership test inline, twice),
`osc::current_transient_default_color_owner` (via `foreground_job_is_shell`) and
`foreground_member_cwd_different_from_shell` (skips the shell's pid). The
comparison and the membership test are equivalent only because the shell is its
own group leader. Owner: one probe returning `Foreground::{Shell, Job { pgid,
processes }, Unknown}`. (mux-panes)

## CON-053 - What is a process's cwd, and what is a pane's or workspace's cwd?

One `ProcessCwd::{Live, Deleted, Unavailable}` reader, a purpose-based terminal
cwd resolution (identity, follow, save, resume), capture through the workspace
identity resolver and `AppPaths::fallback_cwd()` now exist. Still open:
`Workspace::cwd_for_pane` (in the mux workspace pane tree) builds its own
fallback onto the terminal cwd instead of the purpose-based resolution, and the
runtime's arbitrated cwd (`PaneCwdState::reported`) and `TerminalState::cwd`
remain two copies of the last OSC 7 report kept in step by the event (kept
deliberately so save probes survive without terminal state; the reason is at
the code). Reported by mux-panes, mux-state and server-app.

## CON-054 - Pane counter bookkeeping is partly outside the one rule

`backend.rs` now records every mutation through one `CoreMutation` and
`record_mutation` rule (`pane/terminal.rs`) that decides the content,
detection, sync and history counters. Still open: the detection increment
helpers in `crates/shepr-mux/src/pane/agent_detection.rs` (free functions on
`&mut u64`) sit outside that rule, and `PaneTerminalCore`'s fields are still
`pub`/`pub(super)`, so `DetectionTask::tick` and the osc tests read
`core.detection_content_seq` directly. The counter types are filed among the
types. Reported by mux-panes, terminal and server-serving.

## CON-055 - How is a pane launched?

`PaneLaunchEnv::from_extra(Vec::new(), socket).with_pane_id(PublicPaneId::new(ws,
n))` is written at four sites (`Workspace::spawn`,
`Workspace::launch_env_for_new_pane`, `persist/restore.rs`,
`App::pane_launch_env`). Every `PaneRuntime::spawn` and
`spawn_with_initial_history` call threads the same settings (scrollback, host
theme, host appearance, shell config) and four handles, which `workspace.rs`
bundles as `PaneSpawnHandles` but the runtime does not accept;
`start_pending_agent_resume` passes the four individually, and restore re-bundles
them as its own `RestoreRuntimeContext` with twelve parameters. Restore passes
host appearance `None` while every other site passes the current one, decided
implicitly by one call site. `PaneShellConfig::new(&settings.default_shell,
settings.login_shell)` is built at four sites. Owner: a `PaneSpawner` (or
`PaneLauncher`) built once by the app with handles, socket and settings, and
`spawn(PaneLaunchRequest { pane_id, public_id, geometry, cwd, kind,
initial_history })`; workspaces and restore produce pure plans it executes.
Reported by mux-panes, mux-state and server-app.

## CON-056 - What content rect does a pane's terminal get, and where is its scrollbar?

The composition `visible_panes` then `pane_inner_rect` then
`terminal_content_rect` is done independently by `ui::panes::compute_pane_infos_for_workspace`
(with `alt` from the runtime and a "no runtime means a fresh primary shell"
branch; it feeds rendering and the PTY size rule), mux
`PaneGeometry::pane_size` (spawn sizing, `alt = false`, clamped to at least 1),
`agent_resume::derived_pending_agent_resume_pane_infos` (`alt = false`, with its
own zoom override giving hidden panes the tiled size), and the server's
`retained_surface::resolve_retained_panes` and `retained_pane_layout` (from the
wire pane's alternate-screen flag), plus `ui/panes.rs` twice more.
`PaneChromeInfo::inner_rect` and `scrollbar_rect` are placeholders at
construction ("not settled here") that every caller overwrites. The "narrow pane"
threshold `<= 4` columns is in `terminal_content_rect` and again in
`ui/panes.rs`. Held together by the shared helper and the pairwise tests
`rendered_content_rect_matches_the_size_new_panes_are_spawned_at` and
`pending_agent_resume_launches_at_the_size_its_first_resize_keeps`. They already
disagree: `pane_size` clamps to 1 and the ui path does not, so a degenerate pane
gets different answers at spawn and first resize; the zoom treatment differs by
design between render and resume and nothing names it. The scrollbar gutter is
derived in `ui::panes::stable_scrollbar_gutter` and again in
`resolve_retained_panes`, and visibility in `ui::scrollbar::should_show_scrollbar`
and again in `retained_scrollbar_patch` (`max_offset_from_bottom > 0 &&
pane_scrollbars && !alternate_screen`). Owner: one mux function such as
`PaneGeometry::content_layout(layout, zoomed, include_hidden, screen_mode) ->
Vec<PaneContent { id, rect, borders, content, gutter }>` with
`ScreenMode::{Primary, Alternate, NotStarted}`, and one `scrollbar_visible`.
Reported by mux-state, server-app and server-serving.

## CON-057 - Which panes does a surface of a workspace show?

`Workspace::visible_pane_ids()` now answers it for `sync_immediate_pty_sources`
and `visible_pane_runtimes`. Still answering it themselves:
`retained_pane_layout` in `server/headless/retained_surface.rs` and the surface
pane builder in `ui::panes`, which pass `workspace.zoomed()` to
`PaneGeometry::visible_panes` (which assumes the zoomed pane is the focus).
(server-serving)

## CON-059 - How does a layout tree collapse, and which panes are adjacent?

Pruning and pane-id collection now live on core's `Node`, and core's
`find_in_direction` uses one directional helper. Still open: mux
`workspace/geometry.rs` decides adjacency (`ranges_overlap`, `pane_to_right`,
`pane_below`, `u16` saturating) separately from core's helper (`u32` ends), and
the two differ at the right or bottom edge of a `u16::MAX` area. (mux-state)

## CON-060 - How does a snapshot key its panes?

`capture_workspace` keys each pane by `(workspace index, PaneId::raw())`; the
server's `capture_preserved_layout` builds `HashMap<(usize, u32), TerminalId>` by
the same rule independently and then compares only the total count. Owner:
capture returns the index it used (`SavedPaneRef -> TerminalId`) and the
checkpoint code takes that value. (mux-state)

## CON-061 - Which pane maps to which terminal and runtime?

`AppState` now keeps a `PaneId -> TerminalId` index with `terminal_of` and
`runtime_of`, used by six paths. Still open: `find_pane` in
`crates/shepr-server/src/app/ids.rs` re-walks workspaces, and state assembled
directly (outside the creation, restore and removal paths that maintain the
index) still falls back to a scan, so the index is a second record kept in step
by hand. Reported by server-app and mux-panes.

## CON-062 - Where does a copy-mode motion land?

Line motions (`End`, `FirstNonBlank`) are composed in the server's
`handle_pane_copy_motion` from `extract_selection` plus
`shepr_termio::copy_mode::last_character_col`/`first_non_blank_col`, clamped to
`terminal_dimensions()`; word and paragraph motions and search are
`PaneRuntime` methods in mux `pane/terminal/text.rs`; the paragraph result's
column is patched in the handler. Owner: one `PaneRuntime::copy_motion(cursor,
CopyMotion) -> Point`, or the text engine moving to termio with mux supplying row
text. Reported by mux-panes and server-app.

## CON-063 - What is a workspace's automatic label, and is it visible?

`Workspace::mark_identity_undiscovered` (`fallback_label_from_cwd(identity_cwd)`),
the Git status snapshot (from the cache key, see cleanup),
`WorkspaceGitStatusSnapshot::into_workspace_status` (from the real cwd and repo
root), and the client from the server's checkout-root answer. Visibility:
`apply_workspace_git_statuses` sets `changed |= ws.custom_name.is_none()` and
`Workspace::display_name` encodes the same rule. Owner: the label as a pure
function of `(cwd, checkout root, home)` (already `shepr_core::workspace_label`)
computed once at admission. Reported by mux-state and server-app.

## Persistence and session saves

## CON-069 - Is a pane exit checkpointed before removal?

The two `prepare_pane_removal_by_id` calls serve different moments (before the
hold and after the checkpoint) and are documented as such, and the runtime
envelope requeue is one helper. Still open: the server's
`replaying_checkpointed_pane_exit` field passes a parameter through `self`
(`handle_scheduled_tasks_headless` sets it, `handle_internal_event_with_forwarding`
`take()`s it, early returns clear it by hand), and the direct `AppEvent`
fallback survives for callers outside the prepare-and-hold path. Owner: `PaneDied`
applicable only as a `(event, PreparedPaneExit)` input, and an explicit
`Origin::Replay(generation)` argument, which needs the pending queue and
scheduler in `server/headless.rs` to carry it. Reported by server-app and
server-serving.

## App and server loop

## CON-070 - Did the view change, and what must be invalidated?

Sites diffing `shell_projection_revision` around a call:
`handle_api_request_with_render`, `handle_endpoint_command_with_render`,
`handle_internal_event_inner`, three places in `internal_events.rs` (two return
points of `handle_internal_event_with_forwarding`) and one in
`endpoint_requests.rs`. "Changed" is also declared as
`effects.shell_projection_changed` and reconciled against the counter in
`app/api.rs`, and `handle_client_shell_command` special-cases
`EndpointCommand::PaneScroll` by variant (whether a change is viewer-local is
decided in the server by matching the command). Sites picking their own
invalidation triple (`mark_shell_projection_dirty`,
`render_dirty.request_generic`, `render_notify.notify_one`):
`set_host_terminal_theme` (no projection mark), `handle_git_status_refreshed`
(all three), the cwd branch (render, notify, git refresh),
`handle_pane_launch_settled`'s failure arm, `sync_pending_terminal_titles`,
`set_host_terminal_appearance_state` (none; filed as a bug). Every endpoint
handler hand-writes its six-bool `EndpointEffects` while the truth lives in the
mutators (`handle_pane_close` re-derives `focus_changed` by snapshotting focus;
`handle_pane_resize` decides a resize does not change the projection;
`handle_workspace_create` and `handle_workspace_close` write the same four
flags). Owner: mutators return outcomes (`impl From<Outcome> for
EndpointEffects`) and an `Invalidation` value folded once per call, carrying
`Invalidate::PaneViewers(pane)` for viewer-local changes. Reported by server-app
and server-serving.

## CON-071 - Which client's geometry sizes a workspace?

`client_views::workspace_geometry_source`: controller if viewing, else lowest-id
outer-focused viewer, else lowest-id viewer.
`client_views::reapply_controlled_shell_workspace_geometry`: the same rule over a
sorted list, writing the answer back with `set_geometry_controller`. Both copies
are live (the source function keeps its own fallback because navigation can make
it stale first). Fragments of the same policy:
`surface_interest::set_client_shell_surface_active` computes
`focused_viewer_already_owns_workspace`; `ClientRegistry::claim_geometry` and
`claim_unowned_geometry` check `is_active_shell_client` but
`set_geometry_controller` does not; `handle_client_shell_command` has a
four-branch claim policy keyed on `claims_shell_geometry` and `changes_topology`
including a `let _ = self.clients.claim_geometry(..)` whose result is ignored;
claims are triggered by focus gain, pane interaction, connect (unowned only),
activation (unless a focused viewer exists) and navigating commands. When the
rule is applied is decided by client events, command completion, departures,
pane death, API topology change (comparing `workspace_order()` before and after),
and inside the render pass: `render_full` re-applies a workspace's geometry
through a forty-line closure when its source's baseline shows an
alternate-screen flip, and `render_pass_with_boundary` applies headless geometry
for workspaces without an area, which mutates PTYs from within rendering, keyed
on the sent baseline. Owner: a `GeometryArbiter` owning `geometry_controllers` and
the rule, with `claim(client, ClaimReason)` and `settle(&views)`, run as a
settlement step before the render plan, with alternate-screen transitions
reported by mux as a PTY event. (server-serving)

## CON-074 - What cursor does a client see?

`ui::surface_cursor` (full render: synchronized output, scrollback hiding, the
CJK IME reveal with its agent filter and shape) and
`retained_surface::retained_cursor` (patch path: synchronized output and
scrollback only). They disagree on the CJK reveal, hidden by a third site:
`render_pass_with_boundary` sends every patch candidate to the full step when
`reveal_hidden_cursor_for_cjk_ime` is set, so with that setting every client loses
retained rendering. Owner: one cursor function both paths call, after which the
routing special case goes. Reported by server-app and server-serving.

## CON-075 - Wire pane metadata is built twice

`client_shell::render_pane_surface` builds the content revision (with the parity
trick), mouse flags, alternate screen, scroll metrics with `as u64` casts and
pixel size; `retained_surface::render_patches` rebuilds the same metadata from the
dirty patch snapshot with its own scroll metrics conversion and without the
parity rule. Owner: one builder from a runtime snapshot. (server-serving)

## CON-076 - Does a projection need recomputing?

`render_plan` uses `!settled || projected_location_generation !=
location.generation() || snapshot.is_none()`; `render_client_full` also compares
`session_generation != shell_session_generation`. The plan relies on every bump
of the session generation also calling `mark_view_changed`, which holds at both
bump sites today with nothing tying them. Owner: one
`ClientShellState::projection_due(&SessionGeneration)`. Projection identity
resolution is also repeated (`snapshot_from_session` resolves the focused
workspace twice, `shell_target_for_client` applies the same filter,
`focus_target_for_surface`, `shell_focused_runtime` and `visible_pane_runtimes`
each go id to index to workspace to runtime, `send_pane_focus` uses a linear
`position`): a `ViewedWorkspace { index, workspace }` resolved once per client
per pass. (server-serving)

## Wire, handshake and surfaces

## CON-080 - Is this update a patch against unchanged topology, and may it apply?

`SurfaceTopology`, `SurfaceBaseline::admits`, named revision transitions and one
patch application are now shared by the server and both decoder paths. Still
open: the client's `endpoint/choice/preparing.rs` keeps a second full baseline
and applies patches to it instead of reading the connection decoder's baseline
(`Decoder::current_surface`); the boundary is commented there. Reported by
contracts and server-serving.

## CON-081 - How large may a grid be?

`shepr-config` `limits.rs::terminal_grid_cells` (dimension and cell caps);
protocol `ClientSurfaceSize::clamped` re-derives the row bound
(`MAX_SURFACE_CELLS / cols`, min `MAX_SURFACE_DIMENSION`); core
`GridSize::clamped` clamps only the minimum, so a decoded `TerminalGeometry`
holds any grid up to 65535 by 65535 until `client_shell_geometry_error` checks
it. They agree. Owner: core, as a bounded grid type that refuses over-budget
grids, which also lets protocol drop its config edge. (contracts)

## CON-082 - How is a handshake answered?

`shepr-api` `client_protocol.rs::refuse_client` reads the preamble and hello,
then writes the preamble and refusal in one write; `shepr-server`
`client_transport::handle_client_handshake` reads the preamble, writes the
preamble at once, then reads the hello. Both decide what `DifferentBuild` and
`NotShepr` mean and which `PreambleError` outcomes get this build's preamble
back; they order their writes differently, compatible with a client that writes
preamble and hello together but pinned by nothing. Owner: one handshake function
returning `Hello | Foreign | Silent | NotShepr` (or a validated hello) and
writing the preamble once by one rule. Reported by contracts and server-serving.

## Config and keybindings

## CON-088 - Fixed-key surfaces: routing and help text

Copy-mode keys are routed by char literals in `route_copy_mode_key` and described
by literals in `render_mode_bar` (`"h/j/k/l w/b/e { }"`, `"y/enter"`); resize keys
by `route_resize_key` and the resize bar text; navigator and Help footers state
their keys as literals with a comment pointing at `route_overlay_key`.
`copy_mode_command_char -> Option<char>` is a char-typed enum. Owner: a small
command table per fixed-key surface (commands, keys, labels) that routing and
help both read. (client-shell)

## Edges

## CON-093 - Command lines: producers and parsers are separate copies

`shepr`: `RemoteCliCommand::args` (shepr-remote) produces argv; the clap builder
in `src/cli/spec.rs` plus typed parsers (`status::parse`, `server::parse`,
`detect::parse`) consume it. Shared `COMMAND_*`/`FLAG_*` constants keep spelling
in step, but shape is held only by the pairwise test
`generated_remote_cli_arguments_parse_with_the_cli_spec`; the spec and the typed
parsers are two copies held by `every_cli_spec_root_has_typed_parser` and
`every_cli_spec_leaf_parses_to_a_typed_command`; `root_exit_flags_before_subcommand`
is a fourth copy of the subcommand list with `"detect"` as a bare literal (as in
`CliCommand::from_matches`); the binary's clap spec imports its names from the
SSH crate. `shepr-server`: `local_server.rs` produces `--client-spawned` and
`--version` (a literal) and `shepr-daemon` parses both with its own constant; no
test ties them. The `server stop` command is spelled three times:
`RemoteCliCommand::ServerStop`, `src/preflight.rs::remote_stop_command`
(hand-formatted `"ssh {} {} server stop --expect-boot {}"`) and
`ServerAddress::stop_command`. A rename would move the spec, the producer and the
round-trip test together while the two printed copies keep telling the operator
the old spelling. Which commands need application paths is decided in
`cli::run` and re-checked downstream. Owner: one `SheprInvocation` and one
`ServerInvocation` enum, each with `argv()` and `parse()`, next to `daemon_exit`
and `server_stop` (or clap derive); printed commands render the same value.
Reported by edges and contracts.

## CON-095 - The local restart offer echoes socket ids raw

Remote output is now a `RemoteText` sanitized once at the SSH boundary, and the
client no longer filters it again. The local restart offer in `src/preflight.rs`
still prints the build and boot ids the local socket reported without the same
treatment. (edges)

## Client

## CON-097 - Which endpoint is shown, and what is its status?

Shown: `EndpointChoice::shown()` ("the only owner of that fact") and
`ClientShellState::active_endpoint_id`, read by core through `endpoint_is_active`
in `handle_endpoint_supervisor`, the `Connected` failure branch,
`handle_server_message`, `handle_timer`, and inside the shell by
`mark_endpoint_disconnected`; kept in step only by `commit_move` calling
`activate_endpoint_projection`. They disagree after `Lost::Shown`: `shown()` is
`None` while the shell still names the lost endpoint. Status: the supervisor's
`ReconnectState` and the shell's `ClientShellEndpoint::status`, written from
`run_client_loop`, the `Status` arm, the `Connected` arm (twice), the reader-spawn
failure branch, `install_client_shell_snapshot` (Online on every snapshot, any
role), `commit_move` and `mark_endpoint_disconnected`; the shell's Local default
is Online and is overridden when Local is absent. In the shell,
`endpoint_projection_available` and `endpoint_is_online` are the same predicate
(Online and snapshot present); `navigation_target_valid` adds generation and boot;
`CachedEndpointSnapshot::stale`, `handle_endpoint_navigation`,
`move_navigate_workspace`, the navigator and both sidebars test `status ==
Online` directly; `handle_endpoint_machine_click`, `activate_endpoint` and
`focus_or_activate` add "or it is Local"; "Online with no snapshot" is
representable. The Attention label is `"attention"` in
`endpoint_status_presentation` and `"! error"` in `render_endpoint_row`. Owner:
one `Endpoints` owner (id, connection, generation, supervisor state, status,
snapshot, role) with the shell reading a projection, and an endpoint state enum
(`Connecting`, `Online { snapshot, generation }`, `Stale { last }`, `Attention {
last }`) with `usable()`/`stale()`. Reported by client-core and client-shell.

## CON-103 - Is a host terminal write failure fatal?

Mouse mode writes (`handle_resize`, the `MouseCapture` arm,
`clear_endpoint_host_effects`) and keyboard report-all writes map failure to
`ClientError::HostTerminal` and end the client; frame and patch writes, window
titles, clipboard writes and host queries log once and carry on. Owner:
`ClientState` or `HostModes` with a single policy. Mouse mode is also set up
twice (`setup_terminal` builds `HostModes::new(false, mouse_capture)` with a
placeholder, then `run_client_loop` replaces it and swaps the `Arc` mirrors), and
clipboard writes go through `forward_clipboard` and the `ClipboardWrite` arm with
their own logging. (client-core)

## CON-104 - Notices: wording, deduplication and lifetime

A `Notices` component in the client shell now owns suppression, the boot-card
queue, diagnostic replacement, dismissal and drawn expiry. Still open: the
wording. `"{label}: {message}"` is assembled in `handle_endpoint_supervisor`'s
`Status` arm, its `Connected` failure branch, `run_client_loop`,
`reconcile::fail_move` callers, `endpoint_lost` and `waiting_notice`; a typed
`EndpointNotice { endpoint, kind }` rendered once would end that. Reported by
client-core and client-shell.

## CON-108 - What does compose draw over pane cells?

`fast_path_blocker` (`surface_patch.rs`) predicts compose's layers (mode bar,
overlay, endpoint error, notice card, selection, copy-mode owner, unknown pane
hits, surface overflow) as `&'static str` reasons that production throws away;
`render_mode_bar` decides the bar is drawn when `mode != Terminal ||
endpoint_error.is_some()`; compose also draws a lifecycle banner and hides the
cursor when the active endpoint is not Online, which the blocker does not check
(covered only because compose clears `hits.panes`; a cursor-only patch still
takes the fast path). The patch path also rebuilds `PaneHit` fields by hand
(scrollbar rect offset, scroll metric conversion, mouse and pixel flags). Owner:
compose records what it occluded in a `LastComposition` that the fast path
consults, and one `PaneHit::from_wire(pane, origin, clip)`. Mouse capture off
disables hits by clearing them in `render_shell` and in `compose` plus a check in
the right-click arm. (client-shell)

## CON-109 - Which side of a split is a pane on?

The client's `split_child_panes` (`rect.x < split.pos` means first) and
`pane_surface_topology_signature` (`rect.x >= split.pos` means second; outside
means neither, hashed with FNV) answer it separately, both reconstructing the
server's layout tree, which the server reverse-maps again in
`split_path_for_children`; `pane_split_topology_matches_hit` and
`pane_split_target_is_current` layer further checks on the hash. Owner: the
server. `PaneSurfaceSplit` already carries `path`; adding a server-minted layout
epoch (or the child lists) lets the client send `(workspace, path, epoch,
ratio)` and drop the hash and the rect classification. The split hit-rect
geometry in the server (`client_shell.rs::split_hit_rect`) is layout policy that
could sit with the border rules. Reported by client-shell and server-serving.
