# Defects: terminal emulation, PTY and pane runtime

Filed from the defect hunt over `crates/shepr-vt`, `crates/shepr-pty` and
`crates/shepr-mux/src/pane` (except `agent_detection.rs`).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## TERM-006 - Test-only read, mode and launch surface kept in production crates

Scope: vt-pty (filed as simplification against AGENTS.md's smallest-surface
goal). Related: AGENT-032 (unused `TerminalReadSnapshot`).

None of these has a production caller:

- `Terminal::mode_set` (shepr-vt tests and `shepr-mux` runtime and terminal
  tests). Its DECCOLM routing, the "refuse 2026" branch and the
  `handler::private_mode` / `Setter` table column exist for it alone.
- `Terminal::read_text_viewport`, `read_ansi_viewport`, `read_ansi_screen`
  (callers are `#[cfg(test)]` helpers in `shepr-mux/src/pane/terminal/helpers.rs`
  and `osc.rs` tests), hence `read.rs`'s `Coordinates::Viewport` and
  `viewport_line` for reads, and `format.rs`'s `rectangle` mode and its
  `unwrap: false` path. Production uses only `read_text_screen` (selection,
  plain, unwrapped, non-rectangular) and `read_ansi_screen_carrying` (history).
- `screen_text_rows*`, `screen_cell`, `ScreenTextRow`, `ScreenTextCell`. The
  `ScreenTextCell` doc in `crates/shepr-vt/src/cell.rs` still justifies itself by
  "The one remaining builder of whole screens, the alternate-screen history
  read, copies a single viewport per poll step of an explicit API read, so the
  per-cell `Vec` is kept"; no production code calls `screen_text_rows`,
  `screen_text_rows_range` or `screen_cell`. The callers are
  `crates/shepr-vt/src/tests.rs`, the `#[cfg(test)]`
  `PaneTerminal::screen_text_snapshot`, the `#[cfg(test)]` `OwnedTextBuffer` in
  `shepr-mux/src/pane/terminal/text.rs`, and the tests module of
  `shepr-termio/src/blit.rs`.
- In shepr-pty: `PtyCommand::new`, `arg`, `args`, `Program::Argv`,
  `resolve_shell`, `passwd_shell`, `FALLBACK_SHELL` and the passwd buffer limits.
  Production only builds `PtyCommand::interactive_shell`
  (`shepr-mux/src/pane/launch.rs`). `backend::open_pty` and the public
  `spawn_in_pty` exist for `shepr-agent` detect tests and the actor tests.
  `home_dir`'s passwd fallback and `passwd_field` are still used by the cwd
  fallback.

The test doubles could build what they need from `interactive_shell` or a
test-support constructor, and the vt tests can assert through the production
readers.

## TERM-007 - The scanner's ground-state ESC search is not memchr

Scope: vt-pty.

`Scanner::scan` (`crates/shepr-vt/src/scan.rs`) finds the next ESC with
`iter().position(|&b| b == 0x1b)`, while vte uses `memchr` on the same bytes. It
runs on every byte of every pane's output, just before vte scans the same slice
again. `memchr` is only a transitive dependency through vte, so using it needs a
direct dependency edge in `shepr-vt/Cargo.toml` and a `Cargo.lock` update; a note
beside the search says so. Take the edge (it adds no new crate to the build) and
switch the search.

## TERM-014 - Deferred read effects block the PTY reader thread

Hunter's severity: Low (perf smell). Scope: mux-pane.

`PaneReadEffects::apply_deferred` runs as the read's `after_response_order` on
the PTY actor thread. It runs `current_transient_default_color_owner` (a
`foreground_job` `/proc` scan) for every OSC 10/11 set, and for OSC 7 a readlink
plus `UsableCwd::new` (a `stat`, which can hang on a dead network mount). The
ticket gate can also park the reader behind the sync-timeout timer's
`spawn_blocking` task. No lock is held, as the comments say, but the reader loop,
and so every write and resize for that pane, stalls meanwhile. A pane setting
OSC 11 per frame scans `/proc` per read. Moving these to a per-pane worker
(already ordered by tickets) would keep the reader IO-only.

## TERM-015 - Resize scroll recovery rebuilds the whole screen text up to 8x rows times under the core lock

Hunter's severity: Low (perf). Scope: mux-pane.

`PaneTerminal::resize` (`terminal/backend.rs`), when the viewport was scrolled
into history, loops up to `max(rows * 8, 24)` times calling
`terminal_visible_text(&mut core)`, each running `render_state.update` (a full
row copy, since the display offset changed) and building a `String` of the whole
screen, while holding the core lock the PTY reader, rendering and detection wait
on. A 60x200 pane can do 480 full-screen copies per resize. A per-row blank check
through `visit_screen_row_text` on the rows entering the viewport would do it in
O(rows).

## TERM-016 - The output-writer seam does not do what the PTY reader does

Hunter's severity: Low. Scope: mux-pane.

`PaneOutputWriter`'s doc: "The PTY reader writes through one; so does anything
else that feeds a pane its child's output", and `PaneRuntime::output_writer`:
"feeds this pane its child's output, as the PTY reader does".
`PaneOutputWrite::write` discards the whole `ProcessBytesResult` (terminal
replies, render and title requests, clipboard writes, cwd reports, the
synchronized-output timer) and never advances `detection_content_seq`. Its only
users are tests (`shepr-server/src/test_support.rs` and this crate), so tests
written against it silently skip the reader's effect path.

Related: `with_child_io` writes `screen` before `PaneTerminal::new`, which
discards only pending PTY replies, so title, clipboard, pwd or colour effects in
the seeded screen surface as live effects of the first real write
(`seed_history_ansi` uses `discard_core_effects` for this; `with_child_io` should
too). And `on_next_dirty_collection`'s hook is consumed even when the collection
then falls back (hyperlink present) and the snapshot is discarded.

## TERM-017 - Smaller pane runtime items

Scope: mux-pane.

- `PaneState` doc says "Viewport state for a pane"; AGENTS.md defines it as only
  the terminal link plus per-pane input flags, which is what it holds.
- The `DetectorState::new` initial-state mismatch this hunter noted is filed
  under AGENT-001, where the detection hunter traced its consequence.
- `try_send_focus_event` returns `true` when the send failed (the bool means
  "focus reporting is on"); the only caller ignores it.
- `collect_dirty_patch_snapshot` checks for an odd revision and re-reads the
  revision after collecting, both while holding `content_write_lock`, which every
  writer holds for its whole odd window, so neither check can fail. Harmless, but
  it suggests a lock-free protocol that is not what runs.
- `sanitized_osc_debug_payload` decides on the trailing `...` by counting all
  chars, control characters included, while truncation counts only kept ones, so
  a short payload with control characters gets a spurious ellipsis.
- `SHEPR_DEBUG_OSC_EVIDENCE` with a refused value logs and stays off rather than
  failing. It is an environment flag, not config, so outside the "config problem
  fails the launch" rule, but it is the one launch-time setting in this scope
  that falls back silently.

## TERM-018 - A nested client in a pane of a client-socket-only server derives the wrong client socket

Scope: mux-pane, config (residue of the pane socket export fix).

Every pane now exports `SHEPR_SOCKET_PATH` and `SHEPR_CLIENT_SOCKET_PATH` as the
server resolved them. `shepr-config` gives the API variable precedence and
derives the client socket from it, so in a pane of a server started with only a
`SHEPR_CLIENT_SOCKET_PATH` override, a nested `shepr` client derives
`<runtime>/shepr-client.sock` and misses its own server. The API variable cannot
simply be dropped there: every agent integration reports through it. AGENTS.md
names the limitation. Closing it needs the precedence rule to honour both
variables when both are set and consistent (for example, derive only when the
client variable is absent), which is a `shepr-config` change with its pinned
tests.
