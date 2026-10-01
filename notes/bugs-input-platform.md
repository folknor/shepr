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

## INPLAT-017 - Structural: an under-minimum `PaneGeometry` can still be built directly

`PaneGeometry` deserialization now clamps the pane grid. Residue: its fields are
public, so a value built directly can still be below the minimum, and the
defensive clamps in `shepr-pty/src/fd.rs` and `shepr-vt/src/lib.rs` stay
necessary. Private fields with a clamping constructor would make an
under-minimum pane grid unrepresentable.

## INPLAT-022 - `OwnedRuntimeEntry` mixes directory and socket-sidecar shapes

Lateral, `shepr-platform/src/owned_runtime.rs`. `create_directory` returns an
error for `RuntimeKind::Socket`, a sign the kind enum covers two shapes (the
staging and SSH config directories, and the single-use socket sidecar). Split
the directory kinds from the sidecar so that arm disappears. Also comment the
deliberate branch in `bind_single_use_private_socket` that keeps the sidecar when
removing the socket file fails (the sweep reclaims it later).

## INPLAT-021 - A run of incomplete CSI prefixes has no cumulative byte cap

Lateral, `shepr-termio/src/input/raw_input.rs`. The framer's holds are one
`Held` enum with an ending rule per variant, and control strings are now charged
as bytes arrive. Residue: a continuous stream of incomplete CSI prefixes relies
on the idle flush to end its hold rather than on a cumulative byte bound, so
input that never goes idle can keep it growing. Give the `Sequence` hold a byte
bound like the control-string variants.
