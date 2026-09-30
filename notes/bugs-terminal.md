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
runs on every PTY chunk of every pane in ground state, just before vte scans the
same slice again. Switching to `memchr` was done once and reverted: the
`shepr-vt-layer` dependency rule in `brokkr.toml` does not allow a direct
`memchr` edge, and the layer allow lists are the owner's to extend. `memchr` is
already in the build through vte, so the edge adds no crate. Needs the owner's
decision on the allow list first; then the change is two lines.

## TERM-016 - The dirty-collection hook is consumed even when collection falls back

Scope: mux-pane.

`on_next_dirty_collection`'s hook (`crates/shepr-mux/src/pane/terminal/backend.rs`)
is taken before collection runs, and is consumed even when the collection then
falls back (a hyperlink is present) and the snapshot is discarded. Take it only
once the collected snapshot is kept. (The output-writer seam is now documented
as parser-only and advances the detection revision.)

## TERM-017 - Smaller pane runtime items

Scope: mux-pane.

- `PaneState`'s doc says "Viewport state for a pane"; it holds only the terminal
  link and per-pane input flags.
- `try_send_focus_event` returns `true` when the send failed (the bool means
  "focus reporting is on"); its only callers are tests.
- `collect_dirty_patch_snapshot` checks for an odd revision and re-reads the
  revision after collecting, both while holding `content_write_lock`, which every
  writer holds for its whole odd window, so neither check can fail. It suggests a
  lock-free protocol that is not what runs.
