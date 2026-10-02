# Bugs from the design hunt

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

Defects the design hunters turned up on the way. Unverified: each is a
hunter's reading, with the hunter's own confidence where they gave one. The
raw reports are in the commit that precedes this file's.

## Defects

## BUG-003 - Host terminal grid is clamped to the pane minimum

`shepr_core::geometry::HostGeometry` wraps a `PaneGeometry`, whose constructor
clamps through `GridSize::clamped_pane` to 4 by 2, while
`GridSize::clamped` documents "preserve host and protocol grids down to one
cell". `ClientState::set_host_size` clamps with `ClientSurfaceSize::clamped`
(minimum 1) and then stores through `HostGeometry::new`, so
`reported_geometry.cols()` is never below 4 and a narrower host gets frames
composed for a larger grid. The test
`client_host_size_clamps_the_grid_to_one_surface` asserts 1 column for a value
the stored geometry can never hold. Reported by foundation and client-core.

## BUG-067 - Local restart reports "occupant changed" when the server simply went away after a boot mismatch

`src/preflight.rs` `restart_local`: on a boot mismatch, a follow-up probe that
finds no server returns `LocalRestart::OccupantChanged`, though no server
there means `NoServer`. (wave-1 review)

## BUG-068 - An unexpected stop reply is shown to the operator as Rust Debug output

`crates/shepr-api/src/server_stop.rs` reports `unexpected stop result:
{result:?}`, a Debug rendering in a user-facing message. (wave-1 review)

## BUG-069 - A local process can flood the server log through `accept_peer`

`crates/shepr-platform/src/ipc.rs` `accept_peer` logs a context-free warning
for every rejected or unauthenticatable peer, so a local process connecting in
a loop fills the log. The bridge and the launch router logged the same way
before the helper existed. (wave-1 review)

## BUG-007 - The same unreadable remote status JSON is Attention through one command and a silent retry through the other

`discovery::remote_client_status` reports unparsable `status client` output as
`io::ErrorKind::InvalidData`, which `SshFailureDiagnostic::from_error` reads as
`Compatibility` (needs attention, preflight `Incompatible`).
`server_lifecycle::parse_remote_server_status_json` reports unparsable
`status server` output as `io::Error::other`, which reads as `Other` (silent
retry, preflight `Failed` with "the client keeps retrying it"). The decision is
whichever `ErrorKind` the author picked. The structural cause is filed among
the consolidations as the endpoint failure disposition. (edges)

## BUG-010 - The account-shell probe sends POSIX syntax to the account shell

`RemoteSsh::user_shell_output` wraps `command -v shepr` in
`posix_remote_output_command` (`$?`, `if [ ... ]; then ...; fi`) and hands it
to sshd's account shell. In fish, nushell or xonsh that is a syntax error, so
account-shell PATH discovery can never succeed there; the nonzero exit is read
as "not found" and discovery falls through to `/bin/sh` after a wasted cold
round trip. Comments on `bridge_command` show the account shell is known not
to be POSIX. (edges)

## BUG-014 - Remote bridge launch failures are retried forever

The remote bridge host (`remote/host.rs`) documents that a launch failure on
the remote host reaches the client only as stderr and an exit status, which it
classifies as an ordinary retryable failure. A remote `shepr-server` that
refuses its config is retried silently forever. Suggested: answer a launch
refusal the way a build mismatch is answered, with a typed refusal preamble
carrying the `DaemonExit` class, so the client can show Attention. (edges)

## BUG-015 - Preflight turns a panicked check thread into a value

`preflight::check_concurrently` converts a panicked check thread into
`MachineCheck::Failed`. Everywhere else in the client a panic ends the process
(`fatal_panic`). (edges, as a smell)

## BUG-017 - `foreground_cwd` and the live identity cwd can be a deleted directory

`PaneRuntime::foreground_cwd` and `foreground_member_cwd_different_from_shell`
use `absolute_process_cwd`, which keeps the kernel's ` (deleted)` suffix that
`readlink_process_cwd` documents as unusable. `PaneRuntime::cwd()` is likewise a
raw readlink with no usability filter, and it feeds
`Workspace::resolved_identity_cwd_from*`, so a shell sitting in a deleted
directory hands Git discovery and the workspace label a `"<path> (deleted)"`
path, and every refresh retries discovery on a path that cannot exist, while
the save path writes the last usable one. Reported by mux-panes and mux-state;
the cwd readers are filed among the consolidations.

## BUG-018 - No-op scrolls bump the content revision

`scroll_up`, `scroll_down`, `scroll_reset` and `set_scroll_offset_from_bottom`
add 2 to `content_revision` whether or not the viewport moved. Wheel events at
either end of history and repeated `scroll_reset` invalidate every client's
baseline for the pane and can trigger re-renders. `resize` already compares
before and after for the grid. (mux-panes)

## BUG-019 - A failed non-required cwd blames only the first candidate

`launch_status` settles `LaunchRecord::ChdirFailed(errno)` with
`cwd_candidates.first().cloned().unwrap_or_default()` (an empty `PathBuf` if
there were none). For a fresh pane whose requested directory, `HOME`, passwd
home and `/` all failed, the placeholder blames the first. The record should
carry the candidate index the way `ChdirOk` does. (mux-panes; foundation notes
the same empty-path sentinel)

## BUG-020 - The exec failure message takes its program name from `SHELL`

`PtySetup::start` builds `LaunchStatus.program` from
`cmd.get_env(ChildEnv::Shell)` with `String::new()` as fallback. If the pane's
extra environment ever set `SHELL` (it is an allowed variable), the message
would name the wrong program. A `PtyCommand::program()` accessor removes the
indirection. (mux-panes; foundation)

## BUG-021 - Persister refusals are retried forever, and a retired persister reports success

`persist/actor.rs` returns three refusals as `io::Error::other(<sentence>)`
(`abandoned()`, `lease_only()`, `stopped_after_panic()`), and a retired worker
returns `Ok(())` for a job it never ran. `App::finish_session_save` treats all
of them as transient: after a persister panic every autosave and checkpoint
fails, is re-armed with backoff and logged each time, and pane-exit
checkpoints run their failure budget before releasing exits. A checkpoint
submitted after retirement would be marked durable. Suggested:
`SaveError { Io, PublishedNotDurable, Abandoned, Refused(LeaseOnly |
StoppedAfterPanic | Retired) }`. (mux-state)

## BUG-022 - Saved workspace IDs are replaced without damage accounting

Restore silently gives a fresh ID to a saved `id` that fails to parse or
repeats an earlier workspace's. Other per-workspace defects set
`restore_damage` so the first save backs the file up; this one does not, so the
original IDs are overwritten without a backup. (mux-state)

## BUG-023 - A session path behind 17 to 40 symlinks passes startup and fails every save

`check_session_target` uses `fs::metadata` (kernel resolution, up to 40 hops);
`resolve_write_target` resolves by hand with `MAX_SESSION_PATH_SYMLINK_HOPS` =
16 and `ensure_replaceable` then refuses with `InvalidInput`. The startup check
exists to refuse exactly that case. A directory at the history path also stamps
as "no file" in `HistoryFileStamp::read`. The four checks are filed among the
consolidations. (mux-state)

## BUG-024 - Git config read errors are attributed to `.git/config`

`config_output` and `read_repository_format_value` map `run_git_output`'s typed
`GitReadError` into `io::Error::other`, and `read_config_for_status` maps every
`config_deps`/`branch_config` error back to `GitReadError::FileRead { path:
<common_dir>/config, .. }`. A Git spawn failure or timeout while listing config
origins is logged as "could not read .git/config"; `repo_context` does the same
with `git_ref_storage_is_reftable`. The error is also the server's dedup key
(`reported_git_read_errors`), so embedded `io::Error` text makes "the same
error" depend on wording. (mux-state)

## BUG-025 - Appearance changes may not be rendered

`set_host_terminal_appearance_state` updates every runtime but requests no
render and marks nothing dirty, while `set_host_terminal_theme` does both.
`promote_client_to_foreground` and `promote_latest_remaining_client` discard
the result of `sync_host_theme_from_foreground`; the `ClientShellHostTheme` arm
requests a recompute on every client when the theme changes, and the connect
arm marks the view changed without one. A theme that changes because the
foreground changed gets an epoch bump only, a reported one gets epoch plus
recompute. server-app asks for verification; server-serving says the sites
already disagree and one of the two behaviours is wrong.

## BUG-027 - A split host palette reply is replayed partially

`input::send_unix_input_chunks` batches palette replies up to
`PALETTE_COLOR_COUNT` but flushes early on an idle timeout; the shell's
`push_host_theme_update` merges consecutive `PaletteColors`; and
`ClientState::record_host_theme_update` keeps only the newest `PaletteColors`
update, replacing the previous one wholesale. A reply that straddles an idle
flush becomes two partial updates of which only the second is recorded, and
`view::turn_on` replays that partial palette to every endpoint viewed later.
Needs a slow host to trigger. (client-core)

## BUG-028 - A machine may be labelled `Local`

`MachineLabel::parse` and the duplicate-label validation accept `Local`, and
`ClientEndpointId::display_label` returns `"Local"` for both, so notices, the
sidebar and the navigator cannot tell them apart. Reserve the label or display
Local differently. (client-core)

## BUG-029 - `endpoint_lost` discards the failure class

`reconcile::endpoint_lost` builds the machine diagnostic with
`SshFailureDiagnostic::from_message(failure.message)`, so after a live
connection drops the badge diagnostic is always class `Other`, whatever the
transport reported (`TimedOut` from the heartbeat, `InvalidData` from a decode
or patch rejection). The status is always `Reconnecting`. A queue-full
backpressure failure (`endpoint::writer::queue_full`, a local condition) is
reported as "connection was lost". (client-core)

## BUG-031 - Client exit is classified by coincidence

`run_launched_client` treats `ConnectionLost` as a clean exit only when
terminal restoration also failed (`connection_lost_during_terminal_hangup`):
two independent failures read as one cause by inference. (client-core)

## BUG-032 - Possible request ledger leak after a skipped transport failure

`reconcile` skips a queued transport failure when a connection of another
generation is present, and nothing then disconnects the old command lane;
`handle_timer` drops expired commands whose generation is no longer accepted
without telling the shell. If the skip is reached with a command in flight,
the shell's ledger entry is never answered or dropped. The hunter thinks event
ordering makes the skip unreachable; if so the check and filter are dead.
(client-core)

## BUG-037 - Double-click word bounds may drift on emoji presentation sequences

`word_bounds_at_column` maps pane text to columns with
`shepr_vt::unicode_codepoint_width` per char, while server grid widths and the
client's chrome use `shepr_termio::blit::text_width` per grapheme (tests pin
VS16 emoji at width 2). The double-click column mapping may drift by one cell
per such sequence. Unverified; the hunter suggests a test with
`"\u{2764}\u{fe0f}"` before a word. (client-shell)

## BUG-038 - Pane rename to empty sends `Some("")`

Pane rename to empty sends `label: Some("")`, workspace rename treats empty as
"do nothing", and the context menu's Clear sends `None`. Whether the server
treats `Some("")` as a clear is decided elsewhere. One rule for "empty label"
is wanted. (client-shell; server-app separately notes three label
normalisations)

## BUG-039 - An OSC 7 URI with another scheme is taken as a literal path

`scan.rs` knows whether a working-directory report came from OSC 7 (a URI),
OSC 9;9 or OSC 1337 CurrentDir (paths), but emits a bare
`WorkingDirectoryReport(Vec<u8>)`; mux `parse_reported_cwd` then treats any
non-`file://` payload as a path, so kitty's `kitty-shell-cwd://` becomes a
literal path. Suggested `WorkingDirectoryReport::{Uri, Path}`. (terminal)

## BUG-040 - A line selection does not cover columns added by a widening resize

`Selection::line_range(pane, anchor_row, cursor_row, end_col)` encodes whole
lines as columns `0..end_col` taken at creation (the server API passes
`width.saturating_sub(1)`), so the selection no longer knows it is a line
selection. The client keeps that fact elsewhere (`ClientCopySelection::Line`).
Suggested `SelectionShape::{Range, Lines}` on `Selection`. (terminal)

## BUG-041 - `AbsRow::viewport_row` clamps silently

Rows above the viewport saturate to 0 and far below to `u16::MAX`, and the
client clamps again. A drag anchor scrolled above the viewport compares equal
to viewport row 0. The client also uses `AbsRow(0)` as "no scroll metrics" for
the drag anchor (`mouse.rs`). Suggested `ViewportPosition::{Above,
At(ViewportRow), Below}`. (terminal)

## BUG-042 - `read_boot_log_tail` and `open_boot_log` refuse the same file differently

They refuse a non-regular file with different `ErrorKind`s (`InvalidInput`
versus `PermissionDenied`) and in a different order, so a caller branching on
kind gets different answers for the same planted file. (foundation)

## Latent defects

## BUG-043 - Untagged runtime events would skip the generation check

`AppEvent::Runtime { pane_id, generation, event: Box<AppEvent> }` is optional:
`EventSender` has `From<mpsc::Sender<AppEvent>>` with `origin: None`, the
publish helpers in `pane/process_probe.rs` take `impl Into<EventSender>`, and
`App::admit_runtime_event` passes any non-`Runtime` event straight through. An
untagged `PaneDied` or `StateChanged` would skip the generation check that is
the point of the envelope. No production producer sends one today. The typed
envelope is filed among the types. Reported by mux-panes, mux-state and
server-app.

## BUG-044 - Synchronized-update expiry can be lost

`SyncUpdateTimeout::set_timeout` stores `deadline = now.and_then(|now|
now.checked_add(duration))` but sets `pending = true` unconditionally. If `now`
is unset or the add overflows, vte keeps buffering while `deadline` is `None`;
`tick()` never ends the frame, and `mode_get(SynchronizedOutput)` reports false
while output is withheld. Today every `advance` sets `now` first, but nothing
forces that order. Suggested `enum SyncState { Idle, Buffering { deadline } }`.
(terminal)

## BUG-045 - The detection sequence bump differs between the two sync-flush paths

An expired synchronized update flushed by `PaneTerminal::tick` bumps
`detection_content_seq`; the same flush inside `process_pty_bytes_locked` (via
`core.terminal.tick(now)` before parsing) bumps only the sync epoch and relies
on the read's bytes being non-empty. Harmless while a PTY read is never empty.
(mux-panes)

## BUG-047 - One terminal-core lock per input accessor gives inconsistent mode snapshots

`PaneTerminal`'s `mode_enabled`, `bracketed_paste_enabled`,
`focus_reporting_enabled`, `sgr_pixel_mouse_enabled`, `mouse_reporting_enabled`,
`modify_other_keys_level` and `negotiated_keyboard_protocol` each take the
core lock. The server input path calls several in a row for one event
(`sgr_pixel_mouse_enabled`, `wheel_routing`, then `encode_mouse_wheel`, which
reads the modes again), so the child can change modes between them. An
`InputModes` snapshot read under one lock fixes both. (terminal)

## BUG-048 - `client_shell_boot_id` is process-global

It is stored per server but comes from a process-global `OnceLock`, so two
`HeadlessServer`s in one process (the netside test runs two) share a boot id
and `StaleBoot` cannot tell them apart. Harmless in production.
(server-serving)

## BUG-052 - Indexing panics are one refactor away in `handle_layout_set_split_ratio`

It indexes `self.state.workspaces[ws_idx]` while every other handler uses
`.get`; safe only because the index was resolved a line earlier.
(server-app)

## Hot-path costs

## BUG-053 - A syscall per OSC 7 report in the PTY parse path

`shepr-mux/src/pane/osc.rs` calls `shepr_platform::hostname()` (gethostname) on
every OSC 7 parse, while `shepr-server/src/app/mod.rs` reads the hostname once
at startup (`unwrap_or_default`, empty meaning unknown). Two answers to "this
machine's name", taken at different times. (foundation)

## BUG-054 - The rotating logger stats the log path on every record

`logging.rs` `write_once` calls `fs::metadata` per write, under a shared flock:
every tracing line costs a stat and two flock calls. Correct for cross-process
rotation; a size counter with periodic re-check or an inotify watch would do.
(foundation)

## BUG-055 - Per-cell work that cannot contribute in `terminal_cell_paint`

`cells.fg_color()` is `None` exactly when `basic.style.fg_color` is `None`, so
the `.or_else(|| cells.fg_color()..)` arms never contribute, yet `cell_color`
is computed twice per cell. `terminal_buffer_symbol_into` re-measures
`symbol.width()` for every cell of every dirty row. Both run per cell per patch
collection. (terminal)

## BUG-056 - Runtime event admission walks every workspace

`admit_runtime_event`, `pane_exit_needs_checkpoint` and the detector-drop gate
in `handle_internal_event_inner` each scan all workspaces to find a pane's
runtime, because `PaneRuntimeRegistry` is keyed by `TerminalId` while events
carry `PaneId`. Every clipboard write, cwd report and detector update pays it.
The pane-to-runtime lookup is filed among the consolidations. (mux-panes,
server-app)

## BUG-057 - `render_plan` runs on every loop wake and locks visible terminal cores

It runs on every wake, sometimes twice, allocating and sorting
`render_targets`; for clients with surface debt `surface_deliverable` locks the
terminal core of every visible pane (`synchronized_output_state`). A cached
`held` per workspace per epoch, or a mux-side "synchronized output ended"
signal, avoids the locks. (server-serving)

## BUG-060 - Per-loop scans in the app

`start_pending_agent_resumes` runs every loop iteration and starts with a scan
of all terminals and a `retain` over `pending_resume_commands`;
`remove_unattached_terminal_ids` is terminals times panes; per client per
frame, `compute_surface_for`, `render_panes` and `surface_cursor` each resolve
the target `WorkspaceId` by linear scan; `Workspace::display_name()` and
`branch()` clone a `String` per read on projection and title paths. Reported
by server-app and mux-state.

## BUG-061 - `has_consistent_panes` runs on every drag-resize event

`set_split_ratio_at` and `resize_pane` re-prove layout and record agreement (a
`Vec` and a `HashSet`) per mouse-drag event. It disappears with a pane tree
that owns both (filed among the structure findings). (mux-state)

## BUG-062 - Git discovery runs three or four times per uncached workspace refresh

The server calls `git_status_cache_key` (a full discovery); `repo_context`
calls `git_worktree_info_with_errors` twice; the discovery walk finds a
`LocatedGitDir` and throws it away so `git_worktree_info_with_errors`
re-locates it (another stat round and possibly another `git config core.bare`
spawn, with a TOCTOU window). `GitSpaceMetadata` adds two `canonicalize` calls
and another `locate_git_dir` per refresh for fields nobody reads. (mux-state)

## BUG-063 - The client composes twice per input and recomputes connect options per turn

`handle_stdin_input`, the response arm of `handle_server_message` and
`handle_timer` compose a frame when `outcome.repaint`, then
`finish_client_shell_input` composes again and discards the first when
`dispatch_client_shell_actions` reports a repaint. `run_until_exit` builds
`EndpointConnectOptions` (including a layout computation for
`shell.surface_size`) before every wait, and `reconcile` computes
`view_geometry` and `surface_size` again even when no attempt or move is due.
Both run per pane patch event. (client-core)

## BUG-064 - Shell models are rebuilt per event instead of per snapshot

Every navigator key, wheel step and render calls `navigator_rows` over every
endpoint, workspace and pane, lowercasing each candidate; `End` and
`scroll_navigator_to` compute it again. `aggregate_agent_rows` does a linear
`find` per ordered pane id (quadratic per endpoint), and every compose builds
`AgentRowIndex`, sorts it, resolves tokens, builds a `HashMap` and sorts again;
`render_expanded` resolves every workspace's tokens twice. The active
endpoint's snapshot is deep-cloned per snapshot
(`apply_cached_endpoint_snapshot`, `activate_endpoint_projection`).
(client-shell)

## BUG-065 - `ValidatedClientConfig::live_keybinds()` clones the whole keymap per call

(contracts)
