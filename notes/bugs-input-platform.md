# Defects: input and platform

Filed from the defect hunt over `crates/shepr-termio/src/`,
`crates/shepr-platform/src/` (except `remote_bridge.rs`) and
`crates/shepr-core/src/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## INPLAT-014 - Structural: one primitive for runtime artifacts that outlive a killed owner

Three schemes do the same job: socket staging directories with a `.owner`
marker; single-use socket sidecars with the identity written into the lock file
and the lock held; SSH config directories with the identity in the directory
name. Each has its own sweep, content checks and "unmarked is retained" rule. A
single `OwnedRuntimeEntry` (create, mark, hold, release, sweep) in
`shepr-platform` would replace about 250 lines of parallel logic in `ipc.rs` and
`ssh_paths.rs` and make "only provably dead owners are reclaimed" one place to
audit.

## INPLAT-016 - Structural: the raw input framer's holds as one enum

`RawInputByteFramer` tracks `discard_until`, `discarded_tail_bytes`,
`timed_out_mouse_prefix`, `lone_escape_recently_flushed`,
`held_pending_host_reply_esc`, `awaiting_mouse_tail_after`,
`paste_terminator_scanned`, `paste_last_progress`, `discarding_paste_tail` and
the reply policy. Each hold has its own ad hoc expiry; the unterminated OSC
10/11 hold (in `notes/bugs-rejected-candidates.md`) forgot one. A single
`enum Held { Paste{..}, PasteTail, ControlString{family, bytes}, MouseTail{..},
HostReplyEsc, .. }` where each variant gives a byte bound and a flush-count bound
would make "every hold ends" a type-level property.

## INPLAT-020 - Pane cell width is inferred because the wire cell has no wide flag

The blitter places pane cells by the spacer cells and visible successors the pane
renderer sends, so an emoji with VS16 that alacritty stores in one narrow cell no
longer hides the next character, and the full, diff and patch paths agree.
Residue: the wire `CellData` (`shepr-protocol/src/frame.rs`) carries no wide flag,
so a narrow VS16 pane cell followed by a real space, or at the row edge, is still
treated as two columns; and client composition's `split_glyph_cells`
(`shell/presentation/wire_cells.rs`) still sizes glyphs by grapheme width, so an
overlay edge that splits such a cell blanks the emoji or its right neighbour.
Carrying the grid cell's width from the pane renderer through `CellData` to the
blitter and to composition removes both. Also: the `cell_width` doc in `blit.rs`
says client composition uses it, but composition calls `text_width`.

## INPLAT-017 - Structural: an under-minimum `PaneGeometry` can still be built directly

`PaneGeometry` deserialization now clamps the pane grid. Residue: its fields are
public, so a value built directly can still be below the minimum, and the
defensive clamps in `shepr-pty/src/fd.rs` and `shepr-vt/src/lib.rs` stay
necessary. Private fields with a clamping constructor would make an
under-minimum pane grid unrepresentable.
