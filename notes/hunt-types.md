# Types from the design hunt

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

Domain facts that travel as primitives, and types that exist but collapse back
to a string or a number through `Deref`, `From`/`Into`, public fields,
cross-type `PartialEq` or a routinely unwrapped accessor. Includes type
aliases posing as types, sentinel values and string-typed closed sets.

## Identities

## TYP-045 - Live cwds and launch inputs are still plain paths

The saved pane cwd is a `shepr_core::absolute_path::AbsolutePath` checked at
deserialization, carried through restore and into `PtyCommand::cwd`. Still
open: `TerminalState` holds its cwd as `PathBuf`; `terminal_cwd`,
`prepare_workspace`, `prepare_split`, `resolve_new_terminal_cwd`, the config's
`NewTerminalCwd::Path`, `Workspace::from_tree` and the workspace identity cwd
pass plain paths, so `App::launch_pane` and the capture paths convert at their
boundary; and mux's `UsableCwd` (absolute and existing) overlaps
`AbsolutePath` in concept. (mux-state, contracts, server-app)

## TYP-053 - The raw local stream alias remains

The listener alias and the tuple bind APIs are gone and the socket lock outcome
is typed. Still open: `LocalStream` is a plain alias of `UnixStream` used in
about a hundred places across client, server, api, remote and platform, beside
the typed `TrustedServerStream`. (foundation)

## Geometry and coordinates

## TYP-049 - Absolute rows and surface coordinates are still integers

The cursor accessor returns a `ScreenRow`, scroll metrics and `RowView::y()`
are typed, `CursorViewport` holds a `Point<ViewportRow>`, and
`Selection::ordered_cells()` is gone. Still open: `AbsRow(pub u64)` keeps its
public field, `From<u64>` and `saturating_add/sub(u64)` with `.0` arithmetic
across mux and the client (wanted: a private field, `checked_offset_from` and
an `AbsRange`); `ViewportRow` is built from raw loop counters in the client
(`ViewportRow(row - pane.y)`, `ViewportRow(cursor.y - inner.y)`); `Selection`'s
`pane_id` is public and compared field-wise at about forty client sites
(wanted: `belongs_to`); and surface-local and screen coordinates are both
`Rect`/`(u16, u16)` with the surface-origin translation written in several
places (wanted: a `SurfaceRect`/`ScreenRect` split with one
`SurfaceOrigin::to_screen`). (terminal, client-shell)

## TYP-052 - Mouse coordinates: what the pixel spec left

The pane encoder now takes typed positions (`Position::Pixels { column, row,
x, y }`, `PixelReport`, `encode_pane_mouse_report`). Not re-verified: the host
input framer's `RawInputEvent::Mouse` holding pixels minus one under host mode
1016, decided afterwards by inspecting the raw bytes and an `AtomicBool`;
termio's `mouse::Position` beside protocol's `ClientMousePosition`; and mouse
modifiers round-tripping through `u8`. (terminal, client-core)

## Wire, API and config

## TYP-064 - A frame's link indices are valid only once checked

`FrameData` has private fields, a validating constructor and `try_from`
deserialization, and `GridCellWidth` names the wide tail. Still open:
`FrameData::cells_mut` can set a hyperlink index the table lacks, caught only
by `validate()`, `Canvas::new` or the decoder, so a frame edited in place is
not valid by construction. (contracts, client-shell)

## TYP-070 - The help screen's insert-after column is a label string

Help groups are a `HelpGroup` enum, labels come from `Display` on the trigger,
and ranges are one `IndexedRange`. Still open: the keybinding table's "insert
the indexed row after this row" column is still a label string matched with
`==` against the static row labels, because changing its macro pattern to an
identifier needs the matching edit in the client's `input/mod.rs`. (contracts)

## Bool parameters, tuples and sentinels

## TYP-085 - Bool parameters and bool return pairs that remain

Left deliberately or not reached; each is a two-valued fact passed
positionally: `CheckpointTicket.host` and `NextSave::Checkpoint.host` (named
fields, about a dozen test sites); `do_handshake`'s `mouse_capture` and
`surface_active`, which map to `EndpointClientHello` wire fields;
`EndpointRegistry::insert(.., viewed: bool, ..)`; `HostInputProbe::new`'s
bools; and in mux persistence `PendingHistory::resolve_for_save(..,
allow_unchanged)`, `preserve_existing_in -> io::Result<bool>`,
`snapshot_history_decision`'s `Option` meaning "after the write" and
`RestoredPaneStart::Running { duplicate_agent_session: bool }`. (foundation,
client-core, server-app, mux-state)

## TYP-086 - Server tuples and paired options that remain

- `ShutdownLifecycle { phase, freeze }`: folding the freeze into the phase would
  make the `Copy` phase enum carried by `UnexpectedPhase` hold data.
- `ClientRenderState`'s debt, recompute and committed baseline are not
  orthogonal; combining them changes render semantics.
- `PreparedRender::Semantic`'s optional committed surface, recovered by
  matching the message.
- `PaneSurfacePatch` and `PaneSurfaceFrame` carry a `surface_revision`
  placeholder the producer fills with a dummy and `ClientRenderState`
  overwrites: a draft type without the field.
- `ClientPaneIdentity` duplicates `ShellFocusTarget` in `clients.rs`.

(server-serving)

## TYP-087 - Mirror types that remain

The split, navigation and copy-motion enums, the surface rect and the host
colour types are one type each now. Still open: the server UI's `PaneSurface`
is ratatui-typed with an adapter from core chrome; the client shell keeps
`ClientShellState.now` beside a `now` parameter on many methods; persist's
`DirectionSnapshot` mirrors core `Direction` on disk; and `AppClock { now,
wall_now }` and `HookClockSample { monotonic, wall }` are still two structs
with a conversion. (server-app, client-shell, mux-state)
