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

## INPLAT-004 - Legacy Alt+arrow split across two reads still parses as Esc then the arrow

`crates/shepr-termio/src/input/raw_input.rs`. The host framer now keeps a
complete, recognised doubled-ESC sequence (rxvt-style `ESC ESC [ A` for Alt+Up)
together when it is buffered in one piece. Residue: when `ESC ESC` arrives in
one read and the arrow tail in a later one, the framer still splits the escapes,
so the pane gets Esc then Up and an `alt+up` binding does not fire. The file is
upstream-tracked.

## INPLAT-007 - Kitty flags without DISAMBIGUATE send CSI u for modified characters <!-- shout-ok -->

Lateral, low confidence. In `encode_terminal_key`, any non-zero flags make every
modified Char go through `try_encode_csi_u`. With only REPORT_ALTERNATE_KEYS (4)
or only REPORT_EVENT_TYPES (2), Ctrl+a becomes `CSI 97;5u` / `CSI 97;5:1u`. The
kitty spec describes 0b100 as affecting only keys "represented as escape codes
due to the other enhancements in effect", which suggests the legacy `0x01`
there. Check kitty's `key_encoding.c` before changing anything; children nearly
always push DISAMBIGUATE. <!-- shout-ok -->

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

## INPLAT-015 - Structural: classify environment kinds by who owns the grammar

`EnvKind` mixes "shepr parses this strictly" with "a foreign program owns this
value". The Git variables are now `Raw` and `Presence` can no longer refuse a
value, but other foreign variables remain strict: `TERM_PROGRAM` is still
interpreted text, so a non-UTF-8 or padded value still fails client setup. If
every variable whose grammar belongs to Git, the shell, the terminal, tmux or
sshd were `Raw` or `Presence` (byte-preserving, never refused), strict
`Text`/`Path` would remain only for shepr's own variables, and a foreign value
could never fail a shepr launch.

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
