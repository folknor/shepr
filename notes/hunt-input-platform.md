# Defect hunt: input and platform

Scope: `crates/shepr-termio/src/`, `crates/shepr-platform/src/` (except
`remote_bridge.rs`), `crates/shepr-core/src/`. Findings are ordered by how
much they matter in practice. Each one names the claim it breaks.

## Findings

### 1. Wayland clipboard reads return the clipboard text with a newline appended

`crates/shepr-platform/src/clipboard.rs`, `read_clipboard_text_commands`.

Both Wayland read commands are `wl-paste --type text/plain;charset=utf-8` and
`wl-paste --type text/plain`, with no `--no-newline`. `wl-paste` adds a
trailing newline to text MIME types unless `-n`/`--no-newline` is passed (it
only drops the newline on its own for non-text types and `--watch`). The X11
helpers (`xclip -out`, `xsel --output`) return the selection unchanged, so the
same clipboard reads differently depending on the display server.

Claim broken: `read_clipboard_text` returns the clipboard's text. The modal
paste caller (`shepr-client/src/shell/input/input.rs`) passes it to
`TextEditor::insert`, which turns `\n` into a space. On Wayland, copying `foo`
and pasting it into the rename prompt, the help/navigator search or the copy
mode search prompt inserts `foo ` with a trailing space. A copy-mode search for
a word at the end of a line then misses, and a rename picks up a trailing
blank.

Fix: add `--no-newline` to both `wl-paste` invocations. Keep the X11 commands
as they are.

### 2. An unterminated OSC 10/11 reply holds host input with no bound or timeout

`crates/shepr-termio/src/input/raw_input.rs`, `RawInputByteFramer::flush_timeout`,
the `starts_with_incomplete_default_color_response` branch.

When the buffer starts with `ESC ] 1 0 ;` or `ESC ] 1 1 ;` and has no BEL or
ST, the idle flush returns early ("waiting for host color response
terminator"). Nothing limits that wait:

- It does not check `host_replies.awaiting_reply()`, so it holds even when no
  color query is outstanding.
- It never counts bytes against `MAX_DISCARDED_CONTROL_TAIL_BYTES`.
- It has no flush count or deadline.

Every later push appends to the buffer. `extract_one_event` sees an incomplete
OSC control string and returns `None`, so everything typed afterwards
(Enter included) piles up in the "OSC body". When a BEL (Ctrl+G) or `ESC \`
finally arrives, the whole sequence fails `parse_default_color_response` and
is dropped as `Unsupported`, keystrokes and all.

Claim broken: `MAX_DISCARDED_CONTROL_TAIL_BYTES` in `limits.rs` says the
ceiling exists for "bounding malformed or unterminated control input". Every
other incomplete control string goes through the bounded `discard_until` path
with its plausibility check. The paste hold has `MAX_PENDING_PASTE_BYTES` and
`PASTE_STALL_TIMEOUT`. This hold has neither.

Trigger: a color reply whose terminator is lost or truncated, for example a
terminal or multiplexer that drops the tail of a long reply. Rare, but when it
happens the client looks hung until the user happens to press Ctrl+G.

Fix: only hold while `awaiting_reply()` is true, hold for one flush (the same
pattern `held_pending_host_reply_esc` uses), then fall through to the
`ControlString::Incomplete` branch so the bounded, plausibility-checked
discard takes over. That file is upstream-tracked; check upstream herdr for the
same behaviour before porting.

### 3. Git's ceiling list is read as strict text, so one bad entry drops every ceiling

`crates/shepr-core/src/env.rs` (`EnvVar::GitCeilingDirectories` is
`EnvKind::Text`). The consequence shows up in
`crates/shepr-mux/src/git/discovery.rs`, `GitCeilings::from_env`.

The registry declares `GIT_CEILING_DIRECTORIES` as `Text`, so the whole value
is refused when it is not UTF-8 or has surrounding whitespace.
`GitCeilings::from_env` then logs the refusal and uses no ceilings at all.

Claims broken:

- The variant doc says "shepr's own discovery honours it as Git does".
- `GitCeilings::from_env` says the refusal is read as no ceiling "as Git
  ignores an entry it cannot use". Git ignores the one entry, not the whole
  list, and accepts non-UTF-8 path bytes.
- The module doc says that values whose grammar belongs to Git are `Raw`
  ("Git's command-scope config variables, whose grammar belongs to Git").
  This variable has Git grammar but is declared `Text`.

Effect: with one non-UTF-8 directory in the list (or a stray trailing space),
shepr's own repository discovery walks past ceilings that Git stops at. It can
then report a branch for a repository Git would not find from that
directory, and it stats directories the ceiling was set to keep it away from
(slow network mounts are the usual reason to set it).

Fix: declare it `Raw`, split the bytes on `:`, and drop only the entries that
are unusable. `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` (declared `Path`)
have the same mismatch for non-UTF-8 paths. Their consumer
(`git_config_override_path`) propagates the refusal with `?`, so Git config
discovery fails instead of using the path.

### 4. The `Presence` kind does interpret the value, and refused values have two different outcomes

`crates/shepr-core/src/env.rs`, `EnvKind::Presence` and `resolve`. The
consumers are `shepr-platform/src/lib.rs` (`env_present`) and
`shepr-termio/src/input/model.rs` (`host_modify_other_keys_mode`).

`EnvKind::Presence` is documented as "Presence alone is the answer; the value
is never interpreted." `resolve` still runs the UTF-8 and surrounding
whitespace checks before it reaches the `Presence` arm. So a non-UTF-8 or
padded value is refused, and the result depends on which variable it was:

- `TMUX` and `WEZTERM_PANE` refusals propagate out of
  `host_modify_other_keys_mode` (`?`) and fail the client launch in
  `shepr-client/src/loop_config.rs`. `TMUX` holds the tmux socket path, which
  is non-UTF-8 whenever `TMUX_TMPDIR` is.
- `DISPLAY`, `WAYLAND_DISPLAY`, `SSH_CONNECTION`, `SSH_TTY` and
  `VSCODE_IPC_HOOK_CLI` refusals are logged by `env_present` and read as unset.
  A non-UTF-8 `SSH_CONNECTION` therefore flips the OSC 52 decision towards the
  local helpers.

Claim broken: the `Presence` doc. `resolve_present` says "whether it is set
and non-empty", which is not what it computes either.

Fix: in `resolve`, check `Presence` before `to_str` and answer
`!raw.is_empty()`. Then `env_present`'s error branch and the launch failure go
away, because a presence variable can no longer be refused.

### 5. A failed remove leaves the layout's placeholder as the root

`crates/shepr-core/src/layout.rs`, `TileLayout::close_focused` and
`TileLayout::close_pane`.

Both functions `mem::replace` the root with `Node::Pane(PaneId::PLACEHOLDER)`,
then call `remove_pane(old, target)`. On `None` they return `false` and leave
the placeholder installed as the whole layout. `remove_pane` consumes `old`, so
nothing restores it.

Claim broken: `PaneId::PLACEHOLDER` "must not outlive the operation", and
`collect_validated_ids` relies on it ("the edits that write it replace it
before return").

Reachability: today `None` needs every leaf to be `target`, and the
uniqueness and count guards rule that out. So this is latent, but the failure
branch is exactly the one that leaves the tree corrupt instead of unchanged.

Fix: have `remove_pane` take `&mut Node` and splice in place, or return the
untouched subtree on failure (for example `Result<Node, Node>`), so `false`
means unchanged.

### 6. Host input can never parse legacy Alt+arrow

`crates/shepr-termio/src/input/raw_input.rs` (`drain_available_chunks`,
`split_coalesced_escape`) and `crates/shepr-termio/src/input/parse.rs`
(`parse_legacy_special_sequence`).

The host framer (`for_host_input`) always splits a leading `ESC ESC` into a
lone Escape before extracting an event. The parser's
`"\x1b\x1b[A"`-style entries for Alt+Up/Down/Left/Right, and the doubled-ESC
recursion in `complete_escape_sequence_len`, can therefore never match host
input. A terminal that sends rxvt-style `ESC ESC [ A` for Alt+Up produces Esc
followed by Up, so an `alt+up` binding never fires and the pane gets two keys.

Claim broken: the parser's own table, which says these sequences are Alt+arrow.

Fix: either let the coalesced-escape split skip a doubled ESC that completes
one of these known sequences, or delete the dead table entries so the parser
stops claiming to support them. Both files are upstream-tracked.

### 7. `#`-form colour replies are scaled instead of left-justified

`crates/shepr-termio/src/host_term/theme.rs`, `parse_rgb_color` (`#` branch).

XParseColor reads `#RGB`, `#RRRGGGBBB` and `#RRRRGGGGBBBB` as the most
significant bits of each channel, unscaled. Only `rgb:` components are scaled.
The parser scales both forms the same way, so `#f00` yields red 255 instead of
240 and `#8` digits yield 136 instead of 128. Two-digit (`#rrggbb`) and
four-digit replies come out the same either way, so only one- and three-digit
replies are affected, and terminals rarely send those. It feeds the host theme
(selection colours, the pane theme sent to children).

Fix: shift `#` components left to 16 bits and take the high byte, instead of
scaling them.

### 8. A keyboard-protocol write that fails part way leaves the push/pop bookkeeping wrong

`crates/shepr-termio/src/host_term/modes.rs`, `set_host_keyboard_protocol`.

The function writes the pop (`CSI < 1 u`), the push (`CSI > flags u`) and the
modifyOtherKeys change, flushes, and only then updates `*active`. If the flush
or a later write fails after the pop reached the terminal, `active` still
records a kitty entry that no longer exists. The next change or
`restore_host_keyboard_protocol` then pops an entry shepr does not own (the
user's shell's or an outer multiplexer's).

Claim broken: the doc comment ("later changes replace only the entry recorded
in `active`").

This is low: host tty writes rarely fail part way. Fix: record the pop in
`active` as soon as it is issued, or clear `kitty_flags` on any error after a
pop was queued.

## Lateral findings and smells

- **Legacy Super chords on special keys leak the bare key.**
  `encode_terminal_key` (`shepr-termio/src/input/encode.rs`) gives Super only
  on `KeyCode::Char` the CSI u form, under the comment "Super has no legacy
  character encoding. Preserve the chord with CSI-u instead of leaking the
  unmodified character". For Up, Enter, F-keys and so on, `xterm_modifier`
  ignores SUPER. So in a legacy pane Super+Up sends a plain Up and Super+Enter
  sends a plain `\r`, which runs whatever is on the prompt. The comment's
  reasoning applies to these keys too.
- **Kitty flags without DISAMBIGUATE (low confidence).** In
  `encode_terminal_key`, any non-zero flags make every modified Char go through
  `try_encode_csi_u`. With only REPORT_ALTERNATE_KEYS (4), or only
  REPORT_EVENT_TYPES (2), Ctrl+a becomes `CSI 97;5u` / `CSI 97;5:1u`. The kitty
  spec describes 0b100 as affecting only keys "represented as escape codes due
  to the other enhancements in effect", which suggests the legacy `0x01` there.
  It is worth checking against kitty's `key_encoding.c` before changing
  anything. Children nearly always push DISAMBIGUATE, so this rarely matters.
- **blit's dead error path and a wrong comment.**
  `blit_frame_to_with_cursor_memory_and_clear_policy` has a non-IO
  `InvalidData` return for a malformed previous frame. `encode_inner` discards
  the result with `drop(...)` under the comment "The sink is a Vec<u8> ... so
  there is no failure to act on here". In practice the path cannot be reached
  (`commit` never stores a malformed frame, and `encode_inner` pre-checks the
  new one), so the check and the comment contradict each other. Separately,
  `encode_inner` turns a malformed frame into an empty `EncodedBlit` with no
  log, so a bad frame freezes the screen without a trace.
- **`SocketBusy` doc vs a regular file at the path.** The doc says every busy
  refusal is `AddrInUse`, including "a file that raced the bind into place". A
  regular file already at the path before the bind is refused through `probe`
  as `Unreachable(AlreadyExists)` instead (pinned by
  `a_regular_file_at_the_socket_path_is_unreachable_and_survives_a_bind`). The
  behaviour is reasonable. The doc should say that only files that appear
  during the bind are reported as busy.
- **Leftover aliases.** In `ipc.rs`, `pub type DeadlineReader<'a> =
  LocalStreamDeadlineReader<'a>` ("Preserve the public path used by API and
  client crates") and `set_local_stream_polling` (a one-line wrapper over
  `set_nonblocking`) are compatibility shims in a repo that has no
  compatibility to keep.
- **The staging sweep checks modes inconsistently.**
  `sweep_stale_socket_staging_dirs` masks the parent's mode with `0o7777` and
  each staging directory's with `0o777`. A setgid or sticky bit on a staging
  directory still lets it be swept. This is harmless, but the masks should
  match.
- **Possibly missed OpenSSH expansions.** `ssh_control_path_under` counts only
  literal `%C` in the runtime directory when it budgets the staging length.
  Every other `%` token, or a `%%`, in `XDG_RUNTIME_DIR` would also be expanded
  by OpenSSH, changing both the length and the actual path. The ControlPath is
  not escaped. Whether this matters depends on whether a runtime directory
  containing `%` is worth refusing outright.

## Structural opportunities

- **Drop `interprocess` from `shepr-platform`.** Connect
  (`connect_local_stream_within`), peer credentials (`peer_uid`), nonblocking
  mode, shutdown and readiness polling are all already done in raw libc or
  std. What the crate still provides is the `Listener` type and a one-variant
  `Stream::UdSocket` enum. That enum forces irrefutable `let
  LocalStream::UdSocket(..) = ..` destructuring in `client_stream.rs`,
  `remote_bridge_io.rs` and `ipc.rs`. `std::os::unix::net::{UnixListener,
  UnixStream}` would cover all of it, remove one dependency, and shrink the
  surface the client and API crates code against.
- **One primitive for runtime artifacts that outlive a killed owner.** Three
  schemes do the same job:
  - socket staging directories, with a `.owner` marker file inside;
  - single-use socket sidecars, with the identity written into the lock file
    and the lock held;
  - SSH config directories, with the identity encoded in the directory name.

  Each has its own sweep, its own content checks and its own "unmarked is
  retained" rule. A single `OwnedRuntimeEntry` (create, mark, hold, release,
  sweep) in `shepr-platform` would replace about 250 lines of parallel logic in
  `ipc.rs` and `ssh_paths.rs`, and would make the "only provably dead owners
  are reclaimed" invariant one place to audit.
- **Classify environment kinds by who owns the grammar.** Findings 3 and 4 have
  the same root cause: `EnvKind` mixes "shepr parses this strictly" with "a
  foreign program owns this value". If every variable whose grammar belongs to
  Git, the shell, tmux or sshd were `Raw` or `Presence` (byte-preserving,
  never refused), the strict `Text`/`Path` kinds would be left only for
  shepr's own variables. Then a foreign value could never fail a shepr launch.
- **The raw input framer's state is ten loosely coupled fields.**
  `RawInputByteFramer` tracks `discard_until`, `discarded_tail_bytes`,
  `timed_out_mouse_prefix`, `lone_escape_recently_flushed`,
  `held_pending_host_reply_esc`, `awaiting_mouse_tail_after`,
  `paste_terminator_scanned`, `paste_last_progress`, `discarding_paste_tail`
  and the reply policy. Each hold has its own ad hoc expiry, and finding 2 is a
  hold that forgot one. A single `enum Held { Paste{..}, PasteTail, ControlString{family, bytes},
  MouseTail{..}, HostReplyEsc, .. }`, where each variant must give a byte bound
  and a flush-count bound, would make "every hold ends" a type-level property.
  The file is upstream-tracked, so this rewrite cuts against porting upstream
  fixes; weigh that cost.
- **`PaneGeometry` lets wire and struct values skip the pane minimum.** The
  pane minimum is enforced only by remembering to call `PaneGeometry::clamped()`
  on received values ("Reapply the pane-grid boundary to geometry received as
  a struct or wire value"). Routing `Deserialize` through `#[serde(from = ..)]`
  with the clamp would make an under-minimum pane grid unrepresentable instead
  of something each caller has to remember.

## Checked and found sound

- Socket binding: the lock is taken before the path is prepared, stale
  sockets are reclaimed only under the lock, the staged hard-link never
  replaces an existing path, and the in-place fallback is bounded. The
  connect side matches an exact uid while accept-side admission also takes
  root, and the nonblocking connect deadline works.
- Single-use socket sidecars: the identity is written only after the flock is
  held, the sweep locks before it removes anything, and the inode is
  re-checked before unlinking.
- The rotating log keeps its shared/exclusive flock protocol across processes,
  follows another process's rotation through the inode check, and records
  and resumes after write gaps.
- `ProcessHandle`, `session_member_handles`, `reap_pidfd` and the waitid
  status mapping. The `SpawnedDaemon` group kill happens only while the leader
  is unreaped.
- Boot log open/tail (no-follow, owner check, FIFO-safe). Config temporary
  writes go through the created handle, with ACL and owner copy.
- Clipboard helper deadlines, write timeouts that cannot block, and selection
  owner detachment.
- Mouse report encoding (X10, normal, button-motion, any-motion; default,
  UTF-8, SGR and SGR-pixels encodings), the coordinate limits, and
  host-pixel-to-pane mapping.
- Legacy, kitty-disambiguate and modifyOtherKeys key encoding for the
  documented key set. Kitty Enter/Tab/Backspace release suppression and
  DECCKM rewriting are correct.
- Window title and hyperlink URI sanitisation (all C0 and C1 removed). Pane
  cell symbols cannot carry control characters, because alacritty drops chars
  without a width.
- Paste bounding (`MAX_PENDING_PASTE_BYTES`, stall delivery) and the
  incremental terminator scan.
- `PaneId` allocation exhaustion, rejecting the placeholder through serde,
  split extent minimums, and directional navigation tie-breaking.
- `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS` parsing match Git's
  `strtoul` and `sq_dequote` behaviour, including the cases the tests pin.
