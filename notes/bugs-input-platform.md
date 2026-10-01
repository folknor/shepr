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

## INPLAT-001 - Wayland clipboard reads return the clipboard text with a newline appended

`crates/shepr-platform/src/clipboard.rs`, `read_clipboard_text_commands`. Both
Wayland read commands are `wl-paste --type text/plain;charset=utf-8` and
`wl-paste --type text/plain`, with no `--no-newline`. `wl-paste` adds a trailing
newline to text MIME types unless `-n`/`--no-newline` is passed. The X11 helpers
(`xclip -out`, `xsel --output`) return the selection unchanged, so the same
clipboard reads differently by display server.

Claim broken: `read_clipboard_text` returns the clipboard's text. The modal paste
caller (`shepr-client/src/shell/input/input.rs`) passes it to
`TextEditor::insert`, which turns `\n` into a space. On Wayland, copying `foo`
and pasting it into the rename prompt, the help/navigator search or the copy-mode
search inserts `foo ` with a trailing space: a copy-mode search for a word at the
end of a line misses, and a rename picks up a trailing blank.

Fix: add `--no-newline` to both `wl-paste` invocations.

## INPLAT-002 - Git's ceiling list is read as strict text, so one bad entry drops every ceiling

`crates/shepr-core/src/env.rs` (`EnvVar::GitCeilingDirectories` is
`EnvKind::Text`); consequence in `crates/shepr-mux/src/git/discovery.rs`,
`GitCeilings::from_env`. The registry refuses the whole value when it is not
UTF-8 or has surrounding whitespace, and `GitCeilings::from_env` then logs the
refusal and uses no ceilings at all.

Claims broken: the variant doc ("shepr's own discovery honours it as Git
does"); `GitCeilings::from_env` (the refusal is read as no ceiling "as Git
ignores an entry it cannot use"; Git ignores the one entry, not the list, and
accepts non-UTF-8 path bytes); the module doc (values whose grammar belongs to
Git are `Raw`).

Effect: with one non-UTF-8 directory in the list, or a stray trailing space,
shepr's discovery walks past ceilings Git stops at, can report a branch for a
repository Git would not find from that directory, and stats directories the
ceiling was set to keep it away from (slow network mounts are the usual reason).

Fix: declare it `Raw`, split the bytes on `:`, and drop only unusable entries.
`GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` (declared `Path`) have the same
mismatch for non-UTF-8 paths; their consumer (`git_config_override_path`)
propagates the refusal with `?`, so Git config discovery fails instead of using
the path.

## INPLAT-003 - The `Presence` kind does interpret the value, and refused values have two different outcomes

`crates/shepr-core/src/env.rs`, `EnvKind::Presence` and `resolve`; consumers
`shepr-platform/src/lib.rs` (`env_present`) and
`shepr-termio/src/input/model.rs` (`host_modify_other_keys_mode`).

`EnvKind::Presence` is documented as "Presence alone is the answer; the value is
never interpreted." `resolve` still runs the UTF-8 and surrounding-whitespace
checks before the `Presence` arm, so a non-UTF-8 or padded value is refused, and
the result depends on the variable:

- `TMUX` and `WEZTERM_PANE` refusals propagate out of
  `host_modify_other_keys_mode` (`?`) and fail the client launch in
  `shepr-client/src/loop_config.rs`. `TMUX` holds the tmux socket path, which is
  non-UTF-8 whenever `TMUX_TMPDIR` is.
- `DISPLAY`, `WAYLAND_DISPLAY`, `SSH_CONNECTION`, `SSH_TTY` and
  `VSCODE_IPC_HOOK_CLI` refusals are logged by `env_present` and read as unset;
  a non-UTF-8 `SSH_CONNECTION` flips the OSC 52 decision towards the local
  helpers.

`resolve_present` says "whether it is set and non-empty", which is not what it
computes either. Fix: in `resolve`, check `Presence` before `to_str` and answer
`!raw.is_empty()`, so a presence variable can no longer be refused.

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

## INPLAT-005 - `#`-form colour replies are scaled instead of left-justified

`crates/shepr-termio/src/host_term/theme.rs`, `parse_rgb_color` (`#` branch).
XParseColor reads `#RGB`, `#RRRGGGBBB` and `#RRRRGGGGBBBB` as the most
significant bits of each channel, unscaled; only `rgb:` components are scaled.
The parser scales both, so `#f00` yields red 255 instead of 240 and `#8` yields
136 instead of 128. Two- and four-digit forms come out the same either way, so
only one- and three-digit replies are affected, and terminals rarely send those.
It feeds the host theme (selection colours, the pane theme sent to children).
Fix: shift `#` components left to 16 bits and take the high byte.

## INPLAT-006 - Legacy Super chords on special keys leak the bare key

Lateral. `encode_terminal_key` (`shepr-termio/src/input/encode.rs`) gives Super
the CSI u form only on `KeyCode::Char`, under the comment "Super has no legacy
character encoding. Preserve the chord with CSI-u instead of leaking the
unmodified character". For Up, Enter, F-keys and so on, `xterm_modifier` ignores
SUPER, so in a legacy pane Super+Up sends a plain Up and Super+Enter sends a
plain `\r`, which runs whatever is on the prompt. The comment's reasoning applies
to these keys too.

## INPLAT-007 - Kitty flags without DISAMBIGUATE send CSI u for modified characters

Lateral, low confidence. In `encode_terminal_key`, any non-zero flags make every
modified Char go through `try_encode_csi_u`. With only REPORT_ALTERNATE_KEYS (4)
or only REPORT_EVENT_TYPES (2), Ctrl+a becomes `CSI 97;5u` / `CSI 97;5:1u`. The
kitty spec describes 0b100 as affecting only keys "represented as escape codes
due to the other enhancements in effect", which suggests the legacy `0x01`
there. Check kitty's `key_encoding.c` before changing anything; children nearly
always push DISAMBIGUATE.

## INPLAT-008 - blit's unreachable error path contradicts its comment, and a malformed frame freezes the screen silently

Lateral. `blit_frame_to_with_cursor_memory_and_clear_policy` has a non-IO
`InvalidData` return for a malformed previous frame; `encode_inner` discards the
result with `drop(...)` under "The sink is a Vec<u8> ... so there is no failure
to act on here". The path cannot be reached (`commit` never stores a malformed
frame, and `encode_inner` pre-checks the new one), so the check and the comment
contradict each other. Separately, `encode_inner` turns a malformed frame into an
empty `EncodedBlit` with no log, so a bad frame freezes the screen without a
trace.

## INPLAT-009 - The `SocketBusy` doc says every busy refusal is `AddrInUse`

Lateral. The doc says every busy refusal is `AddrInUse`, including "a file that
raced the bind into place". A regular file already at the path before the bind is
refused through `probe` as `Unreachable(AlreadyExists)` (pinned by
`a_regular_file_at_the_socket_path_is_unreachable_and_survives_a_bind`). The
behaviour is reasonable; the doc should say only files appearing during the bind
are reported busy.

## INPLAT-010 - Compatibility aliases in `ipc.rs`

Lateral. `pub type DeadlineReader<'a> = LocalStreamDeadlineReader<'a>` ("Preserve
the public path used by API and client crates") and `set_local_stream_polling` (a
one-line wrapper over `set_nonblocking`) are compatibility shims in a repo with no
compatibility to keep.

## INPLAT-011 - The staging sweep masks modes inconsistently

Lateral. `sweep_stale_socket_staging_dirs` masks the parent's mode with `0o7777`
and each staging directory's with `0o777`, so a setgid or sticky bit on a staging
directory still lets it be swept. Harmless; the masks should match.

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

INPLAT-002 and INPLAT-003 share a root cause: `EnvKind` mixes "shepr parses this
strictly" with "a foreign program owns this value". If every variable whose
grammar belongs to Git, the shell, tmux or sshd were `Raw` or `Presence`
(byte-preserving, never refused), strict `Text`/`Path` would remain only for
shepr's own variables, and a foreign value could never fail a shepr launch.

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
