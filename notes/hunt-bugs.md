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

## BUG-014 - Remote launch failures without a daemon exit class are retried forever

A remote daemon boot exit now carries its `DaemonExit` class through the bridge
(`remote/host.rs`, `remote/bridge.rs`), and `ConfigRefused` and `Failed` map to
`EndpointFailure::Repair`. Launch failures that end before a daemon boot exit
exists (a missing or non-executable sibling, a launch lock or boot timeout, an
unresponsive occupant) still reach the client only as stderr and an exit
status and are retried as ordinary failures. (edges)

## Latent defects

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

## BUG-057 - `render_plan` runs on every loop wake and locks visible terminal cores

It runs on every wake, sometimes twice, allocating and sorting
`render_targets`; for clients with surface debt `surface_deliverable` locks the
terminal core of every visible pane (`synchronized_output_state`). A cached
`held` per workspace per epoch, or a mux-side "synchronized output ended"
signal, avoids the locks. (server-serving)

## BUG-060 - Per-loop scans in the app

`start_pending_agent_resumes` runs every loop iteration and starts with a scan
of all terminals and a `retain` over `pending_resume_commands`; per client per
frame, `compute_surface_for`, `render_panes` and `surface_cursor` each resolve
the target `WorkspaceId` by linear scan; `Workspace::display_name()` and
`branch()` clone a `String` per read on projection and title paths. Reported
by server-app and mux-state.

## BUG-061 - `has_consistent_panes` runs on every drag-resize event

`set_split_ratio_at` and `resize_pane` re-prove layout and record agreement (a
`Vec` and a `HashSet`) per mouse-drag event. It disappears with a pane tree
that owns both (filed among the structure findings). (mux-state)

## BUG-065 - `ValidatedClientConfig::live_keybinds()` clones the whole keymap per call

(contracts)
