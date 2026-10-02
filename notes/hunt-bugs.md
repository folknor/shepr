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

## BUG-014 - Remote bridge launch failures are retried forever

The remote bridge host (`remote/host.rs`) documents that a launch failure on
the remote host reaches the client only as stderr and an exit status, which it
classifies as an ordinary retryable failure. A remote `shepr-server` that
refuses its config is retried silently forever. Suggested: answer a launch
refusal the way a build mismatch is answered, with a typed refusal preamble
carrying the `DaemonExit` class, so the client can show Attention. (edges)

## BUG-031 - Client exit is classified by coincidence

`run_launched_client` treats `ConnectionLost` as a clean exit only when
terminal restoration also failed (`connection_lost_during_terminal_hangup`):
two independent failures read as one cause by inference. (client-core)

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

## Latent defects

## BUG-073 - The unrecognized hook identity warning no longer names its pane

`AgentOwnership::warn_unrecognized_hook_identity` lost its `pane_id` field when
the ownership machine moved into shepr-agent, so the custom-source warning
does not say which pane reported. Log it at the server's `HookStateReported`
and `AgentSessionReported` call sites, or run those inside a pane span.
(wave-3 review)

## BUG-074 - A fresh pane whose every cwd candidate failed blames `/`

The launch status now reports the last failed cwd candidate with its errno. For
a fresh pane whose requested directory, `HOME`, passwd home and `/` all fail,
the placeholder names `/` rather than the requested directory. Record and
errno agree; which candidate the user should be shown is open. (wave-3 review)

## BUG-072 - Git status dependencies can differ between discovery and a cache hit

`git_status_discovery` replaces `info.repo_root` with the canonical cache key
but keeps `git_dir` and `git_common_dir` as discovered from the workspace's own
cwd. Discovery used to start from the canonical root, so the cached dependency
paths can now differ from the ones a cache hit would compute for the same key.
(wave-2 review)

## BUG-071 - Parked hook starts have no expiry or process attribution

Comments in the hook arbitration (`crates/shepr-mux/src/terminal/state/`)
already admit that a parked start never expires and is not tied to a process,
and that a replayed pane exit can consume a parked start. Neither is handled.
(wave-2 fixer)

## BUG-070 - A Claude background fork may take over the pane's session

Claude's `fork` SessionStart source now counts as a session replacement,
which is right for `--fork-session` and `/branch` (the pane's own process
moves to the new id). The same source is sent for a `/fork` background copy,
which runs under Claude's separate supervisor process and leaves the original
in the pane. If that supervisor inherits the pane's environment, its hook
reports through the pane and replaces the pane's session id with the
background copy's. The payload does not say which kind of fork it is. Check
whether a background fork reports through the pane; if it does, tell the two
apart (process identity, or another payload field). (agents adjudication)

## BUG-043 - Untagged runtime events would skip the generation check

`AppEvent::Runtime { pane_id, generation, event: Box<AppEvent> }` is optional:
`EventSender` has `From<mpsc::Sender<AppEvent>>` with `origin: None`, the
publish helpers in `pane/process_probe.rs` take `impl Into<EventSender>`, and
`App::admit_runtime_event` passes any non-`Runtime` event straight through. An
untagged `PaneDied` or `StateChanged` would skip the generation check that is
the point of the envelope. No production producer sends one today. The typed
envelope is filed among the types. Reported by mux-panes, mux-state and
server-app.

## BUG-047 - One terminal-core lock per input accessor gives inconsistent mode snapshots

`PaneTerminal`'s `mode_enabled`, `bracketed_paste_enabled`,
`focus_reporting_enabled`, `sgr_pixel_mouse_enabled`, `mouse_reporting_enabled`,
`modify_other_keys_level` and `negotiated_keyboard_protocol` each take the
core lock. The server input path calls several in a row for one event
(`sgr_pixel_mouse_enabled`, `wheel_routing`, then `encode_mouse_wheel`, which
reads the modes again), so the child can change modes between them. An
`InputModes` snapshot read under one lock fixes both. (terminal)

## Hot-path costs

## BUG-053 - Two answers to this machine's name

`crates/shepr-mux/src/pane/osc.rs` caches the hostname in its own `OnceLock`
on the first OSC 7 report, while `crates/shepr-server/src/app/mod.rs` reads it
once at startup (`unwrap_or_default`, empty meaning unknown). Two answers taken
at different times. The server's startup value should reach the OSC 7 parser
through pane runtime construction. (foundation)

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
