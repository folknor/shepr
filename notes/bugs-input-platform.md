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

## INPLAT-004 - Host input can never parse legacy Alt+arrow

`crates/shepr-termio/src/input/raw_input.rs` (`drain_available_chunks`,
`split_coalesced_escape`) and `crates/shepr-termio/src/input/parse.rs`
(`parse_legacy_special_sequence`). The host framer (`for_host_input`) always
splits a leading `ESC ESC` into a lone Escape before extracting an event, so the
parser's `"\x1b\x1b[A"`-style Alt+arrow entries and the doubled-ESC recursion in
`complete_escape_sequence_len` can never match host input. A terminal sending
rxvt-style `ESC ESC [ A` for Alt+Up produces Esc then Up: an `alt+up` binding
never fires and the pane gets two keys. Claim broken: the parser's own table.
Fix: let the coalesced-escape split skip a doubled ESC that completes a known
sequence, or delete the dead table entries. Both files are upstream-tracked.

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

## INPLAT-012 - `ssh_control_path_under` budgets only literal `%C`

Lateral. It counts only literal `%C` in the runtime directory when it budgets the
staging length. Any other `%` token, or `%%`, in `XDG_RUNTIME_DIR` would also be
expanded by OpenSSH, changing both the length and the path; the ControlPath is
not escaped. Whether it matters depends on whether a runtime directory containing
`%` is worth refusing outright.

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

## INPLAT-017 - Structural: route `PaneGeometry` deserialization through the clamp

The pane minimum is enforced only by remembering to call
`PaneGeometry::clamped()` on received values ("Reapply the pane-grid boundary to
geometry received as a struct or wire value"). Routing `Deserialize` through
`#[serde(from = ..)]` with the clamp would make an under-minimum pane grid
unrepresentable.
