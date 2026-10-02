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

## CON-002 - What does this accept failure mean, and may this peer in?

- `ipc::accept_failed_for_one_connection` (used by `shepr-api` `listener.rs`):
  EINTR, ECONNABORTED, EPROTO, EPERM retry at once; everything else backs off.
- `pty/src/launch.rs` `Router::accept_loop`: EINTR, ECONNABORTED, EPROTO, EAGAIN
  retry; EMFILE, ENFILE, ENOBUFS, ENOMEM back off; anything else returns.
- `remote/bridge.rs` accept thread: WouldBlock sleeps; everything else breaks.

Peer admission is decided three times: `ipc::peer_is_same_user` (api listener,
remote bridge) and an inline `SO_PEERCRED` plus `uid != geteuid()` in pty's
`accept_hello`. They disagree, and two of the disagreements are filed as bugs.
Owner: one platform accept helper returning `Accepted::{Peer(AdmittedPeer),
RetryNow, Backoff, Fatal}` with the credential check folded in and the peer pid
exposed; this needs pty to depend on platform (see the structure findings).
(foundation)

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

## CON-006 - Which profile owns this pane, and what is the startup cwd?

`SHEPR_BUILD_PROFILE`: `src/main.rs` `should_block_nested_for_env` treats any
marker other than the current one as another profile (so `staging` is not
refused there), while `shepr-config/src/io.rs` `resolve_paths_from_env` refuses
it as a launch error through the private `BuildProfile::from_marker`. They
agree only because `main` says "not nested" and resolve then fails.
`SHEPR_STARTUP_CWD`: `io.rs` `resolve_current_dir` requires it absolute;
`bootstrap.rs` `read_startup_cwd` reads it again with no check (the env kind is
`Handoff`, with no absolute rule). Owner: config exposes a
`PaneOwner::{NotInPane, SameProfile, OtherProfile}` (or a typed `PaneMarker`)
read once, and the startup cwd is read once into `AppPaths` and passed to
bootstrap. Reported by foundation and edges.

## CON-007 - Is this file or directory private enough to trust?

Owner and type checks written out in `daemon.rs` `open_boot_log` (regular, uid,
chmod 0600) and `read_boot_log_tail` (regular then uid, different errors),
`ipc.rs` `acquire_flock_lock` (regular, uid, chmod 0600), `owned_runtime.rs`
sweep (regular, uid, mode exactly `RUNTIME_MARKER_MODE`, size cap) and
`private_directory` (dir, uid, exactly 0700), and `ssh_paths.rs`
`validate_shared_ssh_dir` (dir, uid, exactly 0700 via the literal `0o7777`
while `limits::PERMISSION_BITS` exists). Mode `0o600` is a literal in three
places plus `PRIVATE_SOCKET_MODE`, `LOG_FILE_MODE` and `RUNTIME_MARKER_MODE`.
The SSH runtime directory must be exactly 0700, but the server socket's
directory is created through `create_private_directory_all`, which accepts an
existing directory whatever its mode (the server relies on `SO_PEERCRED`); that
may be deliberate, but nothing records it in one place. Owner: platform
`PrivateFile::open_or_create(path, Policy)` and `PrivateDir::require(path)` with
typed refusals. (foundation)

## CON-008 - The owned runtime artifact layout is restated in six places

Per `DirectoryKind`, `owned_runtime.rs` decides the name prefix in
`create_directory` (`.s`, `shepr-ssh-` with a 16-hex token) and again in
`sweep`, the allowed content in `contents_owned` (`s` socket, `config` file) and
again in `release`; outside the module `ipc.rs` `bind_via_private_staging`
joins `"s"` and `remote/ssh.rs` joins `"config"`. If remote renamed its file,
sweeps would refuse to reclaim and `release` would leave the directory, silently.
Owner: methods on `DirectoryKind` and an entry handing out `content_path()`.
(foundation)

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

## CON-010 - Does an absent or non-regular config file read as empty?

`registry::read_config_content` (directory at the path is an error),
`file_ops::read_if_file` (directory reads as absent), `targets::read_json_config`
(`is_file` then default), `config_file::read_config_snapshot`, and the inline
`fs::read_to_string` in `opencode_config::plugin_is_configured`. Install and
status read the same files through different helpers with different answers for
a non-regular file; they agree only because install's preflight
`check_config_targets` rejects non-regular files first. Owner: one reader.
(agents)

## CON-011 - When is a session path a usable regular file?

`check_session_target` (`fs::metadata`, kernel resolution up to 40 hops),
`ensure_replaceable` after `resolve_write_target` (manual resolution, 16 hops),
`open_regular` (platform helper) and `HistoryFileStamp::read` (a non-file stamps
as absent). They disagree (filed as a bug). Owner: one `SessionPath::resolve(path)
-> Resolved { target, state: Absent | Regular | NotRegular(kind) }` used by the
check, saves, clears, reads and stamps. (mux-state)

## CON-012 - Has a file changed since it was stamped?

Git `config.rs` stamps by mtime and length plus canonical target;
`persist/writer.rs` `HistoryFileStamp` stamps by device, inode, length, mtime ns
and ctime ns (also reused under its history name for layout files in
`SnapshotFingerprintCache`). They disagree: the Git stamp misses an atomic
replace with equal size and mtime, which editors and `git config` both do.
Owner: one `FileStamp` in platform with the persist semantics. (mux-state)

## CON-013 - What is the Unix socket path limit?

`shepr-core/src/socket_path.rs` says it is owned once and every site asks it;
`ipc.rs` `connect_local_stream_within` decides with `bytes.len() >=
address.sun_path.len()`. Same answer today, different source. The main server
socket path is never checked up front; it fails at bind. Owner: core, through a
`SocketPath` constructed with the check and used by `AppPaths` and the connect.
(foundation)

## CON-014 - How does a deadline become a poll timeout?

`child_io.rs` `poll_timeout_until` (with `MIN_POLL_TIMEOUT_MILLISECONDS`) and
`pty/src/fd.rs` `poll_pty_and_wake` (inline, with its own `MIN_POLL_TIMEOUT_MS`)
each decide what "at least 1 ms" means. `set_nonblocking` also exists in
`clipboard.rs`, `fd.rs` and inline in `ipc.rs` (duplicated code that the
pty-on-platform move removes). (foundation)

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

## CON-017 - Is this HOME usable?

`shepr-core/src/pathutil.rs` `home_dir_from_env_value` (absolute, no padding,
UTF-8) exists for a HOME captured in a child environment, but
`shepr-pty/src/command.rs` `cwd_candidates` uses `Path::is_absolute` only, so
`"/home/me "` is refused by core and a cwd candidate to pty. (foundation)

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

## CON-020 - The data-directory lease has two owners

mux `persist/lock.rs` owns `DataDirLease`; `shepr-api` `server_stop.rs`
`data_dir_lease_is_free` probes the same file with its own
`acquire_flock_lock(path, false)`. Only the file name is shared (through config),
and api sits below mux. Owner: the lease (acquire and probe) in platform with the
file name. (contracts)

## Terminal emulation and input

## CON-021 - Which mouse protocol does a pane speak?

vt's handler models the mode exclusivities; vt `mouse_tracking_enabled()` reads
the bits; mux `PaneTerminal::encode_mouse_event` (production) derives a
precedence ladder (AnyMotion, ButtonMotion, PressRelease, X10; SGR, UTF-8,
default) and returns `None` when none is set; the test-only `input_state` has a
second ladder with a different encoding precedence (SgrPixels first); server
`PaneRuntime::wheel_routing` and `plain_page_keys_use_host_scrollback` read the
modes again. On the gating side, `encode_mouse_button` checks
`mouse_reporting_enabled()` first, `encode_mouse_motion` does not,
`encode_mouse_wheel` checks `wheel_routing()`, and the encoder checks again.
Owner: vt `Terminal::mouse_protocol() -> Option<MouseProtocol { mode, encoding,
pixels_requested }>`, read by the encoder and every predicate. Reported by
terminal and mux-panes.

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

## CON-025 - Is an absolute row retained?

`AbsRow::screen_row(origin)` checks only the lower bound;
`Terminal::screen_row_for_absolute(row)` checks both. They disagree for rows past
the end; mux `terminal_extract_selection` uses the lower-bound form and relies on
`read_text_screen`'s prose error. Owner: `Terminal`; remove `AbsRow::screen_row`.
(terminal)

## CON-026 - Is this colour light or dark, and what RGB is a named colour?

vt `RgbColor::inferred_appearance` uses BT.601 luma on gamma-encoded channels
(threshold 128) and feeds the `ColorScheme` reported to children (DSR 997) and
the server's host appearance; termio `selection_render` uses WCAG relative
luminance `< 0.5` for the highlight direction and a contrast ratio for the
foreground. They disagree: a grey of 150 is Light to vt and dark to the
selection code, and any background with luma between 128 and about 188 is
reported light to children while selection treats it as dark. Named colours also
disagree by construction: vt `default_palette()` says ANSI red is `#cc6666`,
termio `selection_render::color_to_rgb` says `(128, 0, 0)`. Owner: one
`RgbColor::appearance()` and `contrast_with` in a shared value crate, and the
termio table resolved through the palette in force. (terminal)

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

## CON-028 - Key encode and parse are mirrored tables

Functional keys: encode has `encode_kitty_functional_key`,
`encode_modified_special`, `encode_legacy_inner`, `encode_f_key`,
`apply_application_cursor` and the list inside `try_encode_csi_u`; parse has
`parse_legacy_special_sequence`, `parse_xterm_modified_special_sequence` and
`kitty_codepoint_to_keycode`. Modifier bits: `xterm_modifier`/`kitty_modifier`
versus `key_modifiers_from_u8`. Ctrl bytes: `encode_legacy_inner` versus
`parse_legacy_ctrl_char`. Mouse button byte: `encode_mouse_cb` versus
`parse_mouse_cb`. Round-trip tests keep the pairs in step and catch only the
pairs they exercise. Owner: one `FunctionalKey { code, legacy, kitty_codepoint }`
table and one modifiers-to-bits mapping, each read in both directions.
(terminal)

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

## CON-031 - Which cells does blit repaint?

`write_all_cells`, `write_changed_cells` and `blit_patch_to` each decide which
cells to repaint, when to reposition the cursor and how wide a cell is. Two
equality rules exist (`cells_equal` by hyperlink index, `cells_visually_equal`
by sanitised URI); they agree only because `patch_rows_fit` refuses any patch
touching a hyperlink. The patch walker repaints a wide glyph's omitted successor
that the diff walker handles through `invalidated`. A test asserts patch bytes
equal diff bytes. Owner: one row painter parameterised by a "previous cell at (x,
y)" source. (terminal)

## CON-032 - Which effects does the terminal queue?

vt keeps seven queues (`responses`, `pwd_changes`, `clipboard_writes`,
`dropped_clipboard_store_bytes`, `title_update`, `progress_update`,
`default_color_set`); mux lists them three times: `collect_core_effects`, and two
identical drop lists `discard_core_effects` (`helpers.rs`) and
`discard_initial_terminal_effects` (`runtime.rs`). Owner: `Terminal::take_effects()
-> TerminalEffects`, `#[must_use]`, discarded by dropping. (terminal)

## CON-033 - History capacity after a purge

`Terminal::restore_scrollback_budget_after_history_purge` plus
`set_history_lines`, and `CoreHandler::restore_scrollback_budget_after_history_purge`,
which inlines its own copy including the trick of truncating the synthetic title
event `set_options` emits. They agree. The handler cannot call `Terminal`
methods because `with_handler` destructures twelve fields into it (filed among
the structure findings). Owner: a `HistoryCapacity` the handler borrows.
(terminal)

## CON-034 - Is the alternate screen active?

`rows.rs::primary_active`, `CoreHandler::primary_screen_active`, the
`active_keyboard_depth` test and four raw `mode().contains(ALT_SCREEN)` checks in
`lib.rs`; mux compares `active_screen() == Alternate` in many more places.
`RowOrigin` and the keyboard-depth mirror depend on it meaning exactly
alacritty's grid swap. One accessor should be the only reader. (terminal)

## CON-035 - Where does an OSC end?

vte decides; `scan.rs::Scanner` mirrors vte's framing to find working directory,
progress and oversized OSCs; mux `osc.rs::OscStreamCollector` mirrors it a third
time for the opt-in debug log (and says so). If the collector drifts, the debug
log shows sequences the terminal did not see. Owner: the scanner, emitting an
`OscBody` event when the debug log is on. Reported by terminal and mux-panes.

## CON-036 - Is the kitty protocol on?

`matches!(protocol, Kitty { flags } if flags != 0)` in `encode_terminal_key`,
`modes.kitty_flags != 0` in `encode_terminal_key_with_modes`, and
`KeyboardProtocol::from_kitty_flags`; `Kitty { flags: 0 }` is constructible
through the public variant, which is why the first site checks. Owner:
`KeyboardProtocol` with a private typed payload and only a `from_flags`
constructor. (terminal)

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

## CON-039 - Should OSC evidence be cleared on an agent change?

`DetectorState::observe_process_probe` computes `should_clear_osc_evidence =
should_reset_detection && previous_agent.is_some()`, and
`clear_osc_evidence_for_agent_transition` checks `previous_agent.is_some()`
again before calling `clear_agent_osc_state`. The helper should just clear.
(mux-panes)

## Agents and hooks

## CON-040 - What may a state report from a session-only integration do?

Three sites, two flags, and they disagree:

- `shepr-server/src/app/actions/events.rs`: if `is_reserved_native_state_source`
  (claude, cursor, devin, copilot, droid, grok), a `HookStateReported` becomes
  `set_agent_session_ref_at`: the state is dropped, the session ref kept.
- mux `source/report.rs` `transition_report`: if the descriptor says
  `session_identity_only_integration` (agy), the whole report is dropped,
  session ref included.
- `transition_report` again: `full_lifecycle_hook_authority` routes through the
  full-lifecycle machinery, and `hook_session_policy.state_requires_current_session`
  (codex only) drops state for another session.

None of the shipped session-only assets sends `pane.report_agent`, so the only
sender is a custom or forged reporter using the official source, and the two
classes treat it differently for no stated reason. `source/report.rs` checks
identity-only through `typed_source.agent().descriptor()` while `source/start.rs`
checks it through `session_identity_only_integration(&source, &label)`. Owner:
the `HookAuthorityClass` on the descriptor with one `admit_state_report` method
called at one place. The agents hunter calls this the one decision in its scope
that already disagrees. Reported by agents and mux-panes.

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

## CON-043 - Does a state report need a session id?

Rust: `HookSessionPolicy::state_requires_current_session` is true only for Codex
and only rejects a ref that differs from the current one. Assets: Kimi and
MastraCode refuse to send state without a session id, Codex refuses too, and Pi,
OMP, OpenCode and Kilo send state only when they have a ref. The server accepts a
session-less state report from `shepr:kimi` or `shepr:mastracode` that the shipped
asset would never send; whether that leniency is deliberate is recorded on one
side only. Owner: the descriptor's integration policy, with the asset contract
tests asserting the asset obeys it. (agents)

## CON-044 - What does a target's hook registration look like?

Install (`integration/targets.rs`, one hand-written `install_*` per target) and
status (`integration/registry.rs`, `RegistrationCheck`, `JsonShape`,
`hook_registration_is_current`) each decide which events get an entry, which
carry an action argument, the timeout unit, the matcher and the entry shape,
held together by the pairwise test `every_target_reads_current_right_after_install`.
Latent drift: `install_cursor` hard-codes `"sessionStart"` and `Some("session")`
while status derives from `CURSOR_HOOK_EVENTS`; `install_devin` writes an entry
for every event including action-less ones while status expects only events with
an action (agreeing by accident); Copilot is the reverse with two separately
written rules; Codex and MastraCode installs skip action-less events, Devin and
Droid do not. Which JSON fields carry a hook command (`["command", "bash"]`) is
also spelled in `value_uses_hook_path`, `cst_value_uses_hook_path`,
`collect_hook_path_commands` and `is_matching_direct_command_entry`
(`direct_command_field()` is a function returning `"bash"`). Owner: one pure
`expected_registration(hook_path) -> Registration` per target, used by install
to write and status to compare (as `grok_hook_config` and
`antigravity_cli_hook_block` already do), and one set of command fields.
(agents)

## CON-045 - Which agents have a screen manifest, and which file is it?

`AgentDescriptor.screen_manifest: bool`, the `BUNDLED_MANIFESTS` table in
`detect/manifest.rs` (keyed by label, with file names that differ from the key:
`antigravity.toml` under `"agy"`, `github-copilot.toml` under `"copilot"`), and
the manifest's own `id` (checked by `parse_bundled_manifest`), held together by
`all_bundled_manifests_parse_validate_and_compile`. A descriptor with
`screen_manifest: true` and no table entry silently degrades to Unknown with no
log. Owner: the descriptor holds the `include_str!` and the table goes. (agents)

## CON-046 - Which glyph at the start of a title is agent activity?

`AgentDescriptor.title_activity_glyphs` (only Claude's is non-empty) plus the
braille range in `Agent::has_title_activity_glyph`; the Claude manifest's
`osc_title_working`/`osc_title_idle` regexes; the Codex manifest's braille
subset; and mux `terminal/title.rs`, which asks whether any agent recognises the
glyph. Since the only consumer unions all agents, the per-agent field is a
global set, and the manifests keep their own copies. Owner: one global
`TitleActivityGlyphs` set in detection, referenced by the manifests through a
named class if the rule language grows one. (agents)

## CON-047 - Is this process an interactive agent, and which rule wins?

`agent != Agent::Letta || is_interactive_letta_process(process)` is written in
the leader branch and the candidate loop of `identify_agent_in_job` and in
`suspended_agent_processes`; owner: one `identify_process` applying it once.
Rule winner: `detect_with_manifest` walks `priority_order` (stable sort,
descending) and stops at the first match, while `explain_loaded_manifest` walks
manifest order and keeps `previous.priority >= rule.priority`; a comment says
they agree and only tie-covering tests check it. Owner: explain evaluates every
rule then picks the winner by walking the same `priority_order`. (agents)

## CON-048 - Claude session-start sources: matcher versus replacement policy

`CLAUDE_SESSION_START_SOURCES` (the hook matcher) admits `startup`, `resume`,
`clear`, `compact`, `fork`; `HookSessionPolicy::CLAUDE` treats only `clear`,
`resume`, `compact` as replacements. `startup` reported but not a replacement may
be deliberate; `fork` looks like drift. Which is intended cannot be read from the
code; it needs a decision, then one list with a per-source role. (agents)

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

Process cwd: `absolute_process_cwd` (absolute only), `readlink_process_cwd` (also
drops a ` (deleted)` target), `usable_process_cwd` (stat through `UsableCwd`),
and the raw `shepr_agent::detect::process_cwd` used for
`ReportedCwd::shell_cwd_at_report`; `ReportedCwd::resolve` compares a raw sample
with a filtered one. They disagree (filed as a bug).

Pane cwd: the runtime arbitrates OSC 7 against `/proc` and offers `cwd`,
`follow_cwd`, `foreground_cwd`, `remembered_cwd` and `PaneCwdProbe::read`; each
caller then builds its own fallback chain onto `TerminalState::cwd()`:
`Workspace::cwd_for_pane` (runtime `cwd()` then terminal),
`creation::launch_cwd_for_terminal` (`follow_cwd()` then terminal),
`persist/snapshot.rs` `capture_workspace` (`remembered_cwd()` then terminal then
the server's fallback), `PendingCwds` (probe `read()`), `agent_resume.rs`
(terminal alone). There are two copies of the last OSC 7 report
(`PaneCwdState::reported` and `TerminalState::cwd`, written from the
`TerminalCwdReported` event and from `LaunchSettlement::Launched { cwd }`), kept
in step by the event.

Workspace identity cwd (the root pane's cwd, else the stored one):
`Workspace::resolved_identity_cwd_from*` (live, through `cwd_for_pane`),
`capture_workspace` (save), `PendingCwds::resolve` plus `root_pane_cwd` (save,
after the probe), `restore::plan_workspace` (a non-absolute saved value is
replaced by the root pane's). The live path skips the usability filter the save
path applies.

Cwd of last resort: `creation::resolve_new_terminal_cwd` (three `"/"`
fallbacks), `handle_pane_split` (`paths.current_dir()` or `"/"`),
`capture_session_save_job` (`current_dir()` or `"/"`), and the PTY child's own
chdir fallback.

Owner: one `ProcessCwd` reader returning `Live | Deleted | Unavailable` with the
stat check a separate explicit step; a `PaneCwd` query in mux taking runtime and
terminal state with an explicit purpose (`Identity`, `FollowForNewPane`, `Save`,
`Resume`), or the runtime returning one `ObservedCwd` with freshness; one
workspace `identity_cwd` both live and capture call (and the saved field dropped,
see cleanup); and `AppPaths::fallback_cwd()`. Reported by mux-panes, mux-state
and server-app.

## CON-054 - Content, detection, sync and history counters are bumped at each mutation site

In `pane/terminal/backend.rs`: `content_revision.wrapping_add(2)` at about a dozen
sites (process, tick, seed, resize, every scroll, clear, host theme, host
appearance, theme restore); `detection_content_seq` through
`observe_detection_content_change` (non-empty bytes only) and
`mark_detection_content_changed` (tick flush, resize, clear), free functions on
`&mut u64`; `synchronized_output_epoch` at four sites; `history_epoch` in
`resize`. They already disagree once (the two flush paths, filed as a latent
bug), and no-op scrolls bump the revision (filed as a bug). The `+2` keeps the
revision even for the server's torn-read parity mark, which mux's doc says is no
longer used (filed under cleanup). `DetectionTask::tick` and the osc tests read
`core.detection_content_seq` directly because all of `PaneTerminalCore`'s fields
are `pub` or `pub(super)`. Owner: a `CoreRevisions` value inside the core with
`record(Mutation::{Output { nonempty }, SyncFlush, Resize { grid_changed },
Viewport, Presentation, Clear})` deciding all four counters, a `ContentRevision`
whose stable-or-torn reading is a method, and private core fields. Reported by
mux-panes, terminal and server-serving.

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

`sync_immediate_pty_sources` (zoomed means the focused pane, else
`layout().pane_ids()`), `visible_pane_runtimes` (the same rule again),
`any_shell_surface_contains_pane` and `shell_client_views_pane`
(`workspace.shows_pane`), `retained_pane_layout`
(`pane_geometry_in(area).visible_panes(layout, zoomed)`), and
`ui::compute_surface_for`. `PaneGeometry::visible_panes` assumes the zoomed pane
is the focus. Owner: `Workspace::visible_pane_ids()` beside `shows_pane`.
(server-serving)

## CON-058 - May a workspace be zoomed?

`Workspace::set_zoomed` (refuses with fewer than 2 panes),
`Workspace::from_restored` (`zoomed && panes.len() > 1`),
`restore::plan_workspace` (`snap.zoomed && pane_ids.len() > 1 &&
saved_focus_survived`), `detach_pane` and `commit_prepared_split` (force
`false`), and the test invariant checker. They agree. Owner: zoom lives in the
pane tree as a state that cannot be entered with one pane and is cleared by the
operations that change the pane set. (mux-state)

## CON-059 - How does a layout tree collapse, and which panes are adjacent?

`restore::prune_restored_node` decides how a split collapses when a child goes,
`shepr_core::layout::remove_pane` decides it for live closes;
`restore::collect_pane_ids`/`collect_ids_inner` duplicate `TileLayout::pane_ids`;
the `Direction` to `DirectionSnapshot` mapping and its inverse are in
`capture_node` and `remap_inner`; `workspace/geometry.rs` decides adjacency
(`ranges_overlap`, `pane_to_right`, `pane_below`, `u16` saturating) separately
from core's `find_in_direction`/`ranges_overlap` (`u32` ends), and the two differ
at the right or bottom edge of a `u16::MAX` area. Owner: `shepr-core::layout`
with `Node::prune(&surviving)`, `Node::pane_ids` and one adjacency helper.
(mux-state)

## CON-060 - How does a snapshot key its panes?

`capture_workspace` keys each pane by `(workspace index, PaneId::raw())`; the
server's `capture_preserved_layout` builds `HashMap<(usize, u32), TerminalId>` by
the same rule independently and then compares only the total count. Owner:
capture returns the index it used (`SavedPaneRef -> TerminalId`) and the
checkpoint code takes that value. (mux-state)

## CON-061 - Which pane maps to which terminal and runtime?

`workspaces.iter().enumerate().any(|(i, _)| runtime_for_pane_in_workspace(.., i,
pane))` appears in `admit_runtime_event`, `pane_exit_needs_checkpoint` and the
detector-drop guard; `find_pane`, `update_terminal_state`,
`sync_pane_lifecycle_authority_detection_pause` and the `TerminalCwdReported`
branch re-walk workspaces for pane to terminal. `PaneRuntimeRegistry` is keyed by
`TerminalId` while events and the runtime know `PaneId`. Owner:
`AppState::terminal_of(PaneId)` and `runtime_of(PaneId)`, or key the registry by
`PaneId`, or carry the terminal id in the runtime envelope. Reported by
server-app and mux-panes (the cost is filed as a bug).

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

## CON-064 - What is the checkout root of a directory?

mux `git/discovery.rs` walks the filesystem honouring `GIT_CEILING_DIRECTORIES`,
gitfiles and bare repositories (`core.bare` via a `git config` spawn), skipping
`.git` directories without `HEAD`, stopping on unreadable entries; the server's
`app/api/checkout_root.rs` runs `git rev-parse --show-toplevel` and classifies
"outside" by `stderr.contains("not a git repository")`. They disagree: inside a
bare repository mux says the bare directory is the root while `--show-toplevel`
fails; `safe.directory` refusals make the server error where mux reads files; a
`.git` without `HEAD` is skipped by mux and makes Git error. A new workspace's
default label and its label after the first refresh can therefore differ. Within
mux, "is this a checkout root" is answered three times with the same
`locate_git_dir` then `git_head_file_is_readable` match
(`git_worktree_info_with_errors`, `git_dir_for_repo_root`,
`git_repo_root_below_with_errors`). Which Git results mean "no answer" is decided
in `git_trimmed_stdout` by matching `args.first()` against `"symbolic-ref"` and
`"rev-parse"` plus stderr `"Needed a single revision"`, in
`read_repository_format_value` and `read_bare` (exit 1), and in the server's
stderr match. Owner: one `discover(cwd) -> Discovery::{Checkout(info), Outside,
Unreadable(err)}` that the server's checkout-root answer also calls, and probes
that declare their own "absent" outcome next to their argv. Reported by mux-state
and server-app.

## CON-065 - Which Git cache entries are kept, and when are they retried?

`status.rs` decides retry timing; the server's `GitRefreshScheduler::mark_due`
drops negative entries by peeking at `fingerprint.is_some()`, and `finish` drops
entries not refreshed this round and prunes the error-dedup set. The cache
travels: the app clones it into the worker, the worker returns `cache_updates`
inside `AppEvent::GitStatusRefreshed`, the app merges them back. Owner: a
`GitStatusCache` type next to the status code with its retention policy (the
crate split is filed among the structure findings). Reported by mux-state and
server-app.

## Persistence and session saves

## CON-066 - May a session save run now?

`DataDirLease::file` (`None` after release), `SessionWriter::lease` (`None` after
retire, `may_write`), `PersistState::accepting_jobs` (false after a panic),
`Worker::LeaseOnly` (refuse with error), `Worker::Retired` (accept and report
success), and `load`'s `lease.is_active()`: five flags across three types and
three answers to "this save will not happen". In the app, `AppPolicy::persists_session`
is consulted in `sync_session_save_schedule`, `start_background_session_save`,
`pane_exit_checkpoint_settled`, `request_pane_exit_checkpoint`,
`pane_exit_checkpoint_generation_settled`, `request_host_shutdown_checkpoint`
and `save_session_before_teardown_async`; `SessionSaver::blocked` adds the host
checkpoint's `finished_unsaved` latch; the persister is chosen at construction as
threaded or `lease_only` from the same policy; the server lifecycle writes
`app.policy = Suspended`, calls `freeze_session_saves()`, stores the old policy as
`HostShutdownFreeze::persist_session: bool` and restores it via
`restored_policy()`. No site disagrees today, but a `Production` policy over a
`lease_only` persister is representable (`persist_for_test` swaps the persister
to avoid it), and the freeze is two mechanisms that must be applied together.
Owner: the persister's worker enum as the state machine, and `SessionSaver`
holding `SavePolicy::{Never, Persisting, Frozen { resume_to }}` as the only thing
asked, with `freeze()`/`thaw()`. Reported by mux-state and server-app.

## CON-067 - Must the on-disk session be preserved before the first overwrite?

The server computes `protect_unloaded = persists && snapshot.is_none()`, folding
`SessionLoad::Missing` (including "lease inactive") in with `Unusable`, then sets
it again on `SessionRestoreLoss::partial(dropped, damage)` (a protocol-crate
function deciding what counts as loss); the writer decides when protection is
discharged (`preserve_existing` returns `false` on `NotFound` and keeps it armed).
The rule spans three crates. `App::with_paths` also sequences `persist::load`, the
history gate, `load_history`, `persist::restore`, the loss decision, the restore
log, an empty-workspace fallback that re-decides `active = None`, and the
persister spawn; `session.rs` decides Clear versus Save and the fallback cwd.
Owner: `persist::open_session(lease, policy, ..) -> OpenedSession { restored,
persister, notice }` deciding protection from its own outcomes, and
`persist::capture_job(..)` owning Clear versus Save. (mux-state)

## CON-068 - Is the preserved pane-exit layout still authoritative?

`preserves_pane_exit_checkpoint` (`preserved().is_some() && !session_dirty`),
`capture_final_session_save_job` (same filter), `finish_session_save`
(`exit.layout.filter(|_| !session_dirty)`), `PaneExitCheckpoint::would_hold` and
`request` (both taking `session_dirty`), and
`finish_checkpointed_pane_exit_after_event`, which writes `state.session_dirty =
false` directly. The cause is two mutation channels: `AppState::session_dirty`
(consumed once per pass by `sync_session_save_schedule`) and
`SessionSaver::note_mutation`, with a window in which the saver's view is stale.
Owner: a monotonic `MutationEpoch` in `AppState` that the saver records per
capture and compares. (server-app)

## CON-069 - Is a pane exit checkpointed before removal?

`prepare_pane_exit` (production) and the fallback in `handle_internal_event_inner`
(`prepared_checkpoint.unwrap_or_else(|| self.pane_exit_needs_checkpoint(..))`)
each also call `prepare_pane_removal_by_id`. In production every `PaneDied` goes
through `handle_internal_event_with_forwarding`, which always prepares, so the
fallback serves tests and direct callers and logs a warning when reached
unsettled. The server's `replaying_checkpointed_pane_exit` field passes a
parameter through `self` (`handle_scheduled_tasks_headless` sets it,
`handle_internal_event_with_forwarding` `take()`s it, three early returns clear
it by hand), and the re-wrapping of `AppEvent::Runtime` for re-queueing is
written twice in the `PaneDied` arm. Owner: `PaneDied` applicable only as a
`(event, PreparedPaneExit)` input, and an explicit `Origin::Replay(generation)`
argument. Reported by server-app and server-serving.

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

## CON-072 - Which clients count?

`presents_surface` (active and attached) for `workspace_geometry_source` and
`automatic_creation_source`; inline `is_active_shell_client() &&
outbox.is_attached()` in `app_client_count`, `pane_viewers`,
`reapply_controlled_shell_workspace_geometry`, `sync_immediate_pty_sources`,
`any_shell_surface_contains_pane`; active only in `latest_shell_client`,
`promote_to_foreground`, `claim_geometry`, `window_title_clients`,
`panes_holding_focus`, `stream_shell_keyboard_mode` and
`stream_host_mouse_capture_mode` (which reads `surface_active` directly);
attached only in `render_targets`, with `render_plan` and `render_full` filtering
by active again. `attached` is false only for fixtures, so production agrees, but
the choice among predicates is made at about fifteen sites by accident
(`create_automatic_workspace` gates on active-only but picks its source with
active-and-attached). Owner: one `ClientRegistry::presenting()` and no
`attached` state. (server-serving)

## CON-073 - What happens when a client stops presenting?

`reap_closed_clients` (remove, then if not stopping reapply geometry and mark the
view changed), `remove_client_and_resize_if_needed` (remove, reapply; the caller
marks the view changed), and `set_client_shell_surface_active(false)` (removes
controllers, promotes the latest remaining client, reapplies, without going
through `remove_client`). Visible-state bookkeeping (`immediate_pty_sources_dirty`,
`host_input_modes_dirty`) is set by hand at about ten sites (connect, resize,
`remove_client`, `create_automatic_workspace`, pane death, navigation and
reconcile, surface activation computed outside the function from a captured
`surface_active` although the function knows `changed`, and `run`); the doc
admits a missed mark only delays a repaint. `handle_server_event`'s
`may_move_focus` list classifies variants by effect though several arms already
sync focus. Owner: one registry operation returning a typed departure outcome
applied in one place, and a cheap per-client view key recomputed when it
changes. (server-serving)

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

## CON-077 - Is the server stopping?

`stop_requested(should_quit)` is evaluated nine times per loop path;
`handle_api_request_with_shutdown_check_inner` and `handle_client_shell_command`
each do "if stop requested, initiate shutdown, then if stopping, reject".
`ClientShellSurfaceSet` and `WorkspaceCheckoutRoot` bypass the second check
because they are dispatched earlier, safe only because `run` never dispatches
server events once a stop is requested. Owner: one gate at the dispatch entry.
(server-serving)

## CON-078 - How is an unencodable message handled?

`ClientOutbox::frame` and `ControlSender::send` each implement "encode, warn and
close on failure". Many `Delivery` results are discarded (`send_to_all_clients`,
`tell_*` in the `stream_*` functions, `complete_reply`); where `Closed` only means
"the reap will handle it" consider returning nothing, and `#[must_use]` where it
matters. (server-serving)

## CON-079 - Retry backoff policies

`Autosave::record_failure` (250 ms doubling to 30 s), `checkpoint_retry_delay`
(250 ms doubling to 1 s), `App::create_default_workspace` (250 ms doubling to
30 s, kept as two `Option`s) and the logind reconnect backoff in `server/`; the
autosave and default-workspace pairs have identical values under different names.
Mostly duplicated code; a `Backoff { min, max }` value also makes the
default-workspace retry one field. (server-app)

## Wire, handshake and surfaces

## CON-080 - Is this update a patch against unchanged topology, and may it apply?

Encoder: `surface_reuse::Baseline::update` picks `SurfaceMeta::Patch` when
projection revision, width, height, hyperlinks, splits and pane ids all match.
Decoder: `Decoder::decode`'s `Projection` branch turns an update into an internal
patch on its own field set; its `Patch`/`None` branch keys only on projection
revision. Planner: `surface_delta::unchanged_plan` and
`projection_metadata_is_unchanged` each compare a different set. Applying:
`Decoder::decode` applies a patch in place on its `CellBaseline`,
`apply_patch_to_surface` does the same to a frame with its own checks, and the
client's `endpoint/choice/preparing.rs` keeps a second full baseline and runs
`apply_patch_to_surface` on it, held by the pairwise test
`surface_update_keeps_same_projection_as_an_internal_patch`. Admission:
`ClientRenderState::prepare_pane_surface_patch` (`Baseline::accepts`, revision
equality, `validate_patch_rows`, pane membership), `render_stream::apply_pane_surface_patch`
(the same four plus grid size), and the client decoder; `Baseline::accepts` takes
five positional arguments including two pairs of swappable revisions. Owner: a
`SurfaceTopology` (or digest) with `same_topology`, one `SurfaceBaseline` with
`apply(&Patch)` and `admits(&Patch) -> Result<(), PatchRefusal>`, and the client
reading the decoder's baseline (`Decoder::current_surface` exists) instead of a
second copy. Reported by contracts and server-serving.

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

## CON-083 - How is an input batch charged?

`client_transport::pane_input_event_limit` and `classify_input_event_size` sum
`expanded_event_count` and `text_bytes` against `MAX_INPUT_EVENT_BATCH` and
`MAX_INPUT_PAYLOAD`; the client's `shell/input/events.rs` batching and
`shell/input/input.rs` paste pre-check sum the same quantities against the same
constants ("clients pre-check pastes with the same accounting"). contracts files
the batch limit itself as a deliberate re-check at each chokepoint, not a
finding; server-serving asks for one `InputBatchCharge { add, fits }` in protocol
that classifies paste versus input overflow, so the sum is written once.
Reported by server-serving; contracts' reading recorded alongside.

## CON-084 - Which methods does the socket thread answer?

`schema.rs::define_methods!` generates `Method`, `MethodKind` and traits;
`AppMethod` is a hand-written subset, `AppMethod::traits` hand-maps each arm back
to a `MethodKind`, and `server::route_request` hand-maps `Method` to `AppMethod`.
The test `method_traits_carry_routing_and_log_facts` checks one arm;
exhaustiveness catches a missing arm, not a wrong one. Owner: a route column in
`define_methods!` generating `AppMethod`, its traits and the routing match.
Endpoint dispatch has the same shape: `dispatch_endpoint_command` matches
`ClientShellSurfaceSet` and `WorkspaceCheckoutRoot` only to log a routing bug and
reject (filed among the structure findings). (contracts)

## CON-085 - Which commands change focus?

`ledger::submit` decides with its own `matches!` that `WorkspaceFocus`,
`PaneFocus`, `PaneFocusDirection`, `WorkspaceCreate` and `PaneSplit` change focus
(to drop the pending workspace highlight). `EndpointCommandTraits` is "one
exhaustive table so the client's notices, the server's logs and the server loop's
routing agree"; `changes_focus` belongs there, and `WorkspaceClose`, `PaneClose`
and `PaneSwap` deserve an explicit answer. (client-shell)

## Config and keybindings

## CON-086 - Navigate-mode arrow aliases and indexed bindings

Aliases: the `keybinding_table!` navigate rows carry an alias column;
`keybinds.rs::reserve_navigate_runtime_keys` hardcodes `KeyCode::Left`/`Right`
instead of reading it; the client's `navigate_alias_matches_left`/`_right` build
the combos again; termio `keybind_help.rs` maps the alias ident to `"left"` and
`"right"`. Adding an alias updates help and matching only with two hand-written
macro arms, and reservation not at all. Indexed bindings: `limits.rs` has the
first and last indexed keys and a derived range syntax kept by hand; termio
`indexed_label`/`indexed_range_prefix` hardcode `"1..9"`, a run of 9 and `b'1'`;
the client's `navigate_indexed_binding_index` re-derives modifier equivalence and
adds its own "exact modifiers first" preference outside
`IndexedKeybind::matched_index`. Owner: a generated `NavigateAlias` enum with
`combo()` and `label()`, and an `IndexedRange` type in config owning parse, label
and matching. (contracts)

## CON-087 - Modifier names, colour parsing and the palette token list

`model.rs::RIGHT_CLICK_MODIFIER_ALIASES` and `keybinds.rs::parse_modifier_token`
both decide what `ctrl`, `control`, `alt`, `option` and `meta` mean (the template
promises right-click accepts keybinding aliases); owner one alias table with the
right-click restriction on top. `sidebar.rs::SidebarTokenColor` and
`theme_config.rs::try_parse_color` implement the hex rule twice (they accept
different sets on purpose); owner one hex parser. The palette token list is
written five times (`Palette` fields, `ParsedThemeColors` fields,
`define_custom_theme_colors!`, `Palette::with_overrides` arms, test destructures);
owner one `palette_tokens!` list. (contracts)

## CON-088 - Fixed-key surfaces: routing and help text

Copy-mode keys are routed by char literals in `route_copy_mode_key` and described
by literals in `render_mode_bar` (`"h/j/k/l w/b/e { }"`, `"y/enter"`); resize keys
by `route_resize_key` and the RESIZE bar text; navigator and Help footers state
their keys as literals with a comment pointing at `route_overlay_key`.
`copy_mode_command_char -> Option<char>` is a char-typed enum. Owner: a small
command table per fixed-key surface (commands, keys, labels) that routing and
help both read. (client-shell)

## CON-089 - Initial chrome is computed twice

`ClientShellState::new_at` and `ClientShellConfig::initial_surface_size` each
compute the starting `sidebar_collapsed` and `sidebar_width` from preferences and
config (`preferences.sidebar_collapsed.unwrap_or(..)` and `clamp_width`), pinned
by the pairwise test `initial_surface_size_uses_persisted_endpoint_chrome`; the
second exists because the shell state does not exist when the first handshake
runs. Owner: one `InitialChrome::resolve(config, preferences)`, or build the
shell state before the launch handshake. Reported by client-shell and
client-core.

## Edges

## CON-090 - What does this endpoint failure mean for the operator?

Attention, retry, prompt or incompatible is answered by:
`SshFailure::needs_attention`; `SshFailureDiagnostic::from_error`'s `ErrorKind`
mapping (`TimedOut`/`ConnectionRefused`/`AddrInUse` to `Link`,
`InvalidData`/`Unsupported` to `Compatibility`, everything else `Other`);
`remote/preflight.rs` `classify_check` (its own ladder) and
`check_after_authentication` (downgrades an auth wait to `Failed`); the client's
`endpoint::supervisor::handshake_error` (picks `ErrorKind`s so `from_error`
lands right: `ConnectionLimit` and `ServerStarting` become `ConnectionAborted`
"so it is retried", a rejected handshake `Unsupported` "so it needs attention",
an early close `UnexpectedEof` to stay out of `InvalidData`);
`handshake::preamble_error` (rewrites to `FramingError` so it classifies as
transient); `hello_write_error` (keeps the socket kind because the supervisor
decides by it); `connect_once` (remaps Local `NotFound` to `ConnectionRefused`);
`endpoint::writer::queue_full` (`ConnectionAborted`);
`transport::framing_error_to_io` (`InvalidData`); `errors::endpoint_setup_failure`
(picks a constructor by variant); `ClientEndpointStatus::after_failure`; the
supervisor's join-error arm; `reconcile::endpoint_lost` (always `Reconnecting`
and `from_message`); `shell_runtime::endpoint_disconnect_notice` (a third reading
of `ErrorKind`, for text); `is_launch_fatal_setup_error` (any `InvalidInput` is
permanently fatal); `StoredSetupError` (captures kind and message, dropping the
typed source); and `src/preflight.rs::result_notices`, whose prose predicts what
the client will do using `needs_attention()` again. `SshFailure::Compatibility`
can come from an `Io(InvalidData | Unsupported)` origin, so
`is_remote_compatibility()` (origin based) and `failure == Compatibility` (class
based) can disagree; `classify_check` checks the first and `needs_attention` the
second. They already disagree: the same `InvalidData` is Attention during the
handshake and Reconnecting once live; a shutdown during the handshake becomes a
retry while one after it loses the typed `ShutdownReason`; the two remote status
parse failures differ (filed as a bug). Owner: one typed endpoint failure built
where the fact is known (edges: `EndpointFailure::{NoRemoteResult, RemoteCommand,
RemoteIncompatible, LocalSetup, Handshake, Message}`; client-core:
`EndpointFailure { cause, class: Transient | NeedsRepair, phase }`) with a single
`disposition()`, `hint()` and rendering, `SshFailureDiagnostic` demoted to the
SSH-process cause, and `from_error` kept only at the raw IO boundary. Reported by
edges and client-core.

## CON-091 - What does a failure prove about the remote install?

`DiscoveryProgress::advance` keeps progress only on
`is_transient_network_failure() || is_authentication_wait_timeout()`;
`run_remaining` ends the pass on `failed_before_remote_result` and otherwise
records the first candidate's rejection; `MachineProbe::resolve` keeps a cached
hint on `failed_before_remote_result`, drops it on `is_remote_candidate_mismatch`,
keeps it otherwise; `observe_failure` invalidates on remote exit `126 | 127`;
`path_lookup_result_with_rejected_candidate` turns a nonzero lookup into "not
found" unless `failed_before_remote_result`. After a host-key or authentication
refusal, `resolve` keeps the hint while `advance` wipes progress: two documented
models of what an SSH refusal proves, kept consistent by prose. Owner: one
`evidence() -> {NothingLearned, InstallStale, CandidateMismatch, InstallChanged,
RemoteFault}` on the failure. Preflight and the connectors also resolve each
machine twice (filed among the structure findings). (edges)

## CON-092 - The restart offer is one engine written twice

`src/preflight.rs` `restart_local` (limit `MAX_LOCAL_OFFERS`, outcome
`LocalRestart`, text `local_offer`) and `remote/preflight.rs`
`restart_different_builds` (limit `MAX_RESTART_OFFERS`, outcome `RestartResult`,
text `remote_offer`). They diverge: "no server left to stop" (local maps
`NotRunning` straight to `OccupantChanged`, remote re-checks and may offer
again); a kept or unasked server (remote prints a "left running, run this to
stop it" notice, local prints nothing); identities (local prints the raw build
and boot ids from the socket, remote sanitized ones). Owner: one
`restart_offers(targets, prompter)` in the crate that owns launching, over a
`RestartTarget { observe, stop }` trait with local and machine implementations;
`src/preflight.rs` keeps only the wording. (edges)

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

## CON-094 - Shell quoting: four implementations

`shepr-remote/src/remote/launch.rs::shell_quote` (quotes a leading `=`, uses
`RemoteExecutable::is_shell_plain_word`); `shepr-config/src/address.rs::shell_quote`
(same set, no `=`; filed as a bug); `shepr-agent/src/integration/command.rs` (the
`'"'"'` escape); `shepr-test-support/src/fixture.rs`. `shepr-server` depends on
`shepr-remote` only to call `interactive_shell_command` in
`app/agent_resume.rs`. Owner: one quoting module (`quote`, `join_argv`,
`is_plain_word`) in core or platform. Reported by edges, contracts and
server-app.

## CON-095 - Sanitizing remote text

`server_lifecycle::printable_remote_text`, `printable_remote_value` and
`printable_remote_token` run at `command_failed`, `ssh_bridge_exit_error` and the
discovery messages (controls become `?`, tabs and newlines kept, CR dropped); the
client's `MachineDiagnostics::insert_machine_diagnostic` filters again with
`!c.is_control() || c == '\n'` (controls dropped, tabs dropped), because the
diagnostic cannot say whether its text was sanitized (`from_message` and
`with_context` accept anything). The local restart offer echoes the local
socket's ids raw. Owner: a `RemoteText` newtype minted once at the SSH output
boundary whose renderer is the only way to show it. (edges)

## CON-096 - Stop outcomes and exit codes are encoded in one crate and decoded in another

`src/cli/error.rs::CliError::exit_code` maps `ServerStopError` to
`BOOT_MISMATCH_EXIT_CODE`/`NO_SERVER_EXIT_CODE`; `remote/launch.rs::stop_remote_server`
maps them back (folding "no server" into "boot changed", filed as a bug).
`DaemonExit` shows the right shape. Owner: `ServerStopExit` with `code()` and
`from_code()` used by both. Reported by contracts and edges.

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

## CON-098 - Will a pick be abandoned, and is the target ready?

`shell_runtime::dispatch_client_shell_actions` predicts what `view::start_move`
will do on the next reconcile to choose a notice: `abandoned =
connection.is_none() && !endpoint_id.is_local() && choice.shown().is_some()`
mirrors `start_move`'s abandon rule, and `metadata_ready =
shell.endpoint_snapshot_identity(..).is_some()` mirrors its Waiting check. Owner:
`EndpointChoice::select` (or a dry run) returning the outcome. (client-core)

## CON-099 - What geometry does an endpoint render, and what is the host's?

`do_handshake_for_link` builds `TerminalGeometry` from `HandshakeGeometry` after
its own `bounded_cell_geometry`; `shell_runtime::view_geometry` builds the same
from `reported_geometry` and `surface_size` with its own bounding;
`run_until_exit` builds the `HandshakeGeometry` for `spawn_due` through
`ProtocolCellSize::from_host` and a fresh `HostGeometry`. The host's pixel
geometry is read by `initial_terminal_geometry` (sets `exact`),
`host_cell_size_query_required` (reads the ioctl again to decide whether to
query), `resize_poll_loop` (every poll) and the stdin reader per chunk via
`HostPixelExtent::current()` (used to map SGR pixel reports), with
`AtomicCellSize` holding the host-reported size; reads moments apart can disagree,
and the server's cell size and the shell's pixel-to-cell mapping can differ across
a resize. Owner: `view_geometry` as the one producer passed to the handshake, and
one host-geometry source (the poller) publishing a snapshot the stdin thread
reads, with the launch query decision `!geometry.exact`. (client-core)

## CON-100 - May pane content be presented now, and is an inbound message move evidence?

`PresentationGate::decide` drops pane frames not from the shown endpoint;
`ClientState::present_frame` and `present_surface_patch` re-check
`frames_frozen()` (unreachably; see cleanup); `present_chrome` deliberately skips
it; about eighteen call sites run `state.shell.compose(cols, rows)` and pick
`present_chrome` or `present_frame` by hand. The gate returns `Buffer` for
surfaces, patches and move responses but `Apply` for `EndpointSnapshot`
regardless of role, and `handle_server_message` separately checks `role ==
Target` to feed `Preparing::receive_snapshot`; the gate takes `move_response:
bool` computed by asking `preparing().accepts_response(..)`; new message variants
fall into the gate's `_` arm as shown-only without anyone deciding. Owner: a
per-turn dirty mark (Chrome or Pane) with one present at the end of `handle_event`
and one at the end of `reconcile`, and a gate that takes the choice and returns
`Apply | Buffer | ApplyAndBuffer | Drop` for every kind. (client-core)

## CON-101 - Attaching Local: the launch path and the supervisor path

The launch connects, handshakes and builds the transport itself
(`run_launched_client`, `start_endpoint_transport`); the supervisor does the same
for every later attempt (`connect_once`, `establish`, `spawn_endpoint_reader` on
the loop thread). Decisions that differ: an absent socket is silent Connecting at
launch and a rewritten `ConnectionRefused` in the supervisor; Local's
build-mismatch guidance is computed in both (`ConnectTarget::Local` claims it is
resolved once); launch handshakes with `surface_active: true` and no deadline,
supervisor attempts with `false` and the attempt budget; launch shows Local by
fiat with no coherent pair, bypassing `commit_move`; launch sends
`ClientShellFocus { focused: true }` unconditionally while a commit sends
`host_focus_baseline()`; `ends_client_for(&Local)` is evaluated three times in
launch. A connection is assembled two ways and the reader-spawn failure branch
duplicates the `Status` arm (filed among the structure findings). Owner: one
attach routine used synchronously at launch, with `LocalFailurePolicy` applied
once to its typed outcome. (client-core)

## CON-102 - Local versus machine policy

"Is this Local or a machine" is asked separately for heartbeat
(`EndpointRegistry::crosses_ssh`), handshake read timeout (`HandshakeLinkKind`
from `EndpointLink`), attempt-counter reset on Online (`record_status`'s
`is_local()`), abandon when unconnected (`start_move` and
`dispatch_client_shell_actions`), fatal on loss (`LocalFailurePolicy`) and
mismatch guidance (`ConnectTarget::Local` only): three enums
(`ClientEndpointId`, `EndpointLink`, `HandshakeLinkKind`) plus `ConnectTarget`
and `AttemptTarget` for one axis. Owner: an `EndpointPolicy` derived once from the
id. (client-core)

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

`"{label}: {message}"` is assembled in `handle_endpoint_supervisor`'s `Status`
arm, its `Connected` failure branch, `run_client_loop` (with the literal
`"Local"`), `reconcile::fail_move` callers, `endpoint_lost` and `waiting_notice`.
In the shell, the Timeout notice key is built in `answer_request`'s error branch
and rebuilt in its success branch to clear suppression;
`push_endpoint_notice_at_boot` dedupes Rejected/Unavailable by equality with the
visible card and Timeout by `endpoint_notice_seen`; `receive_restore_notice` has
its own `restore_notice_seen`; `handle_machine_badge_event` bypasses both and
writes `visible_endpoint_notice` directly; the visible notice's lifetime is a
deadline tuple compared in `endpoint_notice_drawn`, `tick_transient_banners` and
`next_timer_deadline` (the restore-queue stall is filed as a bug). Owner: a typed
`EndpointNotice { endpoint, kind }` rendered once, and a `Notices` component
(visible card, restore queue, seen sets, deadline) with `push`, `dismiss`,
`drawn`, `tick` and `deadline`. Reported by client-core and client-shell.

## CON-105 - Timer inventory

`next_timer_deadline` enumerates six deadlines; `lib.rs` separately calls
`tick_selection_autoscroll`, `tick_selection_highlight`,
`tick_workspace_highlight`, `tick_endpoint_error` and `tick_transient_banners`.
The two lists are kept in step by hand; `tick_transient_banners` also does work
with no deadline. Only `word_selection` uses `limits::Deadline`. Owner: one
`ShellTimers` registry or a `deadline()` plus `tick(now)` trait the shell
iterates. (client-shell)

## CON-106 - Is copy mode the live mode?

`copy_or_terminal_mode` (copy pane equals focused pane); `route_key_press`'s
Prefix arm (the same expression inline); `mouse.rs` pane-scrollbar press (copy
pane equals hit pane and focused equals hit pane); `apply_active_snapshot`
(promotes Terminal to Copy when the copy pane is focused, demotes otherwise);
`copy_mode_owns_input` (mode is Copy, no overlay, copy pane focused);
`insert_copy_search_text` (mode is Copy, no overlay, no focus check). The same
split exists between `Navigate` and `navigate_workspace_id` (filed as a bug).
Owner: the mode carries its state (`Mode::Copy(CopySession)` with parked
sessions held apart, `Mode::Navigate { preview: Option<PinnedLocation> }`) or
copy mode is derived in one function and never assigned. (client-shell)

## CON-107 - Which state goes with a selection?

Sites ending a selection and the fields each clears: `route_key_press` (two
branches) and `prepare_committed_text` (selection, autoscroll, highlight
deadline); `apply_active_snapshot`'s focus-loss branch (all seven); its
copy-pane-removed and unfocused branches (three); `presented_surface_changed`
(word gesture plus three); `cancel_word_selection` (not the highlight deadline);
`tick_selection_highlight` (selection and deadline); `enter_copy_mode`,
`exit_copy_mode` and the mouse left press (their own subsets). They disagree
(`selection_focus_pending` survives a key-cleared selection;
`cancel_word_selection` leaves a deadline armed), harmless only because readers
re-check `selection.is_some()`. `ClientCopyModeState::selection` plus
`sync_copy_selection` is a second copy held in step; copy mode's search is seven
`search_*` fields whose clearing is written in `route_copy_mode_key` and
`presented_surface_changed`. Owner: a `MouseSelection` struct with `clear()`, copy
mode driving it, and one `Option<CopySearch>`. (client-shell)

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

## CON-110 - Agent order and agent rows

Priority order is coded in `sort_agent_refs` (`agent_sidebar.rs`), again in
`AgentRowIndex::new`, and a third cross-endpoint version in `sort_aggregate_rows`
(stale first, then rank, then client-side recency instead of
`state_change_seq`). The panel computes sorted rows per endpoint, keys them by
`(ClientEndpointId, pane_id.to_string())`, then reorders by
`aggregate_agent_rows`. "An agent row needs its workspace" is decided in
`aggregate_agent_rows` and again in `AgentRowIndex::agent_row`. Single endpoint
versus federated selects between `hits.agents` (PaneFocus directly) and
`hits.endpoint_agents` (`focus_or_activate`). Owner: one `AgentPanelModel` built
per snapshot change, used by both sidebars, keyboard agent navigation and
`indexed_navigation_target_exists`. (client-shell)

## CON-111 - Cycling and reveal-into-view

Previous and next with wraparound are written in `agent_target_index`,
`handle_endpoint_navigation`, `move_navigate_workspace` and
`endpoint_command_for_action` (workspaces and `CyclePane*`); they disagree on
not-found (federated paths pick the first or last entry, the single-endpoint
workspace path starts from index 0 so Next lands on the second workspace, and
`CyclePane*` does the same). Reveal: single-endpoint agent navigation jumps
(`agent_scroll = index`), federated calls `reveal_endpoint_agent` (minimal
scroll via `list_scroll_start_to_reveal`), `reveal_workspace` jumps for one
endpoint, and `render_expanded` and `render_collapsed` reveal minimally with two
different algorithms. The federated workspace cycle list is built twice. Owner: one
cycle helper and one reveal rule. (client-shell)

## CON-112 - Smaller duplicated answers in the client shell

- Panel contrast colour: `status::panel_contrast_fg`, `overlays::contrast`, and
  inline copies in `render_mode_bar` and the copy cursor in `compose`.
- Workspace selection background: `sidebar::workspace_selection_background` and
  an inline copy in `render_collapsed`.
- Scrollbar drawing: `scroll::render_list_scrollbar` (one-eighth block),
  `termio::scroll::render_scrollbar_buffer` (half block) and an inline loop in
  `render_help_overlay`.
- The sidebar section threshold `< 6` in `sidebar_section_heights` and
  `sidebar_section_divider_rect`.
- `preferences.rs`'s FNV path hashing duplicates the FNV in `topology.rs`.
- A surface sized for this client is checked in `Preparing::receive_surface` and
  `ViewEvidence::coherent_surface` by comparing width and height by hand (a
  deliberate re-check; a `PaneSurfaceFrame::is_sized_for(size)` would make it one
  answer).

Reported by client-shell and client-core.
