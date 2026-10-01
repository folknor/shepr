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

## INPLAT-010 - Compatibility aliases in `ipc.rs`

Lateral. `pub type DeadlineReader<'a> = LocalStreamDeadlineReader<'a>` ("Preserve
the public path used by API and client crates") and `set_local_stream_polling` (a
one-line wrapper over `set_nonblocking`) are compatibility shims in a repo with no
compatibility to keep.

## INPLAT-013 - Structural: drop `interprocess` from `shepr-platform`

Connect (`connect_local_stream_within`), peer credentials (`peer_uid`),
nonblocking mode, shutdown and readiness polling are already raw libc or std.
What the crate still provides is the `Listener` type and a one-variant
`Stream::UdSocket` enum, which forces irrefutable `let LocalStream::UdSocket(..)
= ..` destructuring in `client_stream.rs`, `remote_bridge_io.rs` and `ipc.rs`.
`std::os::unix::net::{UnixListener, UnixStream}` would cover it, remove a
dependency, and shrink the surface the client and API crates code against.

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
would make "every hold ends" a type-level property. The file is
upstream-tracked, so this rewrite cuts against porting upstream fixes.

## INPLAT-017 - Structural: an under-minimum `PaneGeometry` can still be built directly

`PaneGeometry` deserialization now clamps the pane grid. Residue: its fields are
public, so a value built directly can still be below the minimum, and the
defensive clamps in `shepr-pty/src/fd.rs` and `shepr-vt/src/lib.rs` stay
necessary. Private fields with a clamping constructor would make an
under-minimum pane grid unrepresentable.

## INPLAT-018 - A doubled Escape is now held until the idle flush

Lateral, `shepr-termio/src/input/raw_input.rs`. The framer now holds `ESC ESC`
(or `ESC ESC [` with parameter bytes) until the idle flush, so two quick Esc
presses deliver both Escapes one idle timeout late, where the first used to go
through at once. Check whether the hold is needed for a real sequence or can
release the first Escape immediately.
