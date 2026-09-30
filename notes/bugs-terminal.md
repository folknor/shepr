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

## TERM-004 - vte buffers an unterminated OSC without limit, so one pane can exhaust the shared server's memory

Scope: vt-pty. Filed by the hunter as robustness.

No contract covers this directly, but it defeats the ones next to it.
`crates/shepr-vt/src/limits.rs` bounds every scanner buffer, "keeping attacker
supplied terminal input bounded per pane" (`MAX_OSC_BYTES`,
`MAX_XTGETTCAP_BYTES`), and `MAX_CLIPBOARD_BYTES` bounds "the payload that the
parser hands to its caller". alacritty depends on vte with `std`
(`alacritty_terminal-0.26.0/Cargo.toml`), and under `std` `Parser::osc_raw` is a
plain `Vec<u8>` that `action_osc_put` grows on every byte until a terminator. A
child that prints `ESC ] 52 ; c ;` and streams base64 (or forgets the
terminator) grows the server's heap without bound. When it does terminate,
alacritty base64-decodes the whole thing before shepr's 192 KiB check drops it.
The server is shared by every pane on the host, so one runaway pane takes all of
them down.

**Fix direction.** The scanner already tracks OSC framing exactly as vte does.
Have `Terminal::write_at` stop feeding OSC body bytes to vte once the scanner's
OSC buffer has overflowed: hand vte a CAN to end the OSC (vte dispatches, then
treats CAN as a harmless control) and skip bytes until the scanner leaves OSC
state. This is the one unbounded buffer: vte's DCS passthrough and SOS/PM/APC do
not retain bytes, and its sync buffer is capped at 2 MiB.

## TERM-005 - Panes do not export `SHEPR_CLIENT_SOCKET_PATH`, and an inherited one is overridden

Scope: vt-pty (lateral).

**Claim broken.** AGENTS.md: "Every pane exports `SHEPR_SOCKET_PATH` and
`SHEPR_CLIENT_SOCKET_PATH`".

**What the code does.** `apply_pane_launch_env`
(`crates/shepr-mux/src/pane/launch.rs`) sets only `SHEPR_SOCKET_PATH` (from
`PaneLaunchEnv::api_socket_path`). `SHEPR_CLIENT_SOCKET_PATH` is `Allowed`, so it
reaches the pane only if the server's own environment had it. Path resolution
gives an API-socket override priority over a client-socket override, deriving
the client socket from the API one (test
`client_socket_path_api_override_takes_precedence_over_client_override` in
`crates/shepr-server/src/server/socket_paths.rs`). For a server started with only
`SHEPR_CLIENT_SOCKET_PATH=<custom>`, every pane gets
`SHEPR_SOCKET_PATH=<runtime>/shepr.sock` plus the inherited custom client path,
and a `shepr` run inside it derives `<runtime>/shepr-client.sock` and misses its
own server. With no overrides the derivation lands right, which is why nobody
noticed.

**Fix.** Export the resolved client socket explicitly next to the API socket
(pass it in `PaneLaunchEnv`), or fix the doc and scrub the variable.

A fixer confirmed the mismatch and found the fix spans three places: the
resolved socket pair has to travel through `PaneSpawnHandles`
(`crates/shepr-mux/src/workspace.rs`) into `PaneLaunchEnv`, and a pane that
exports both variables only works if `shepr-config`'s precedence (the API
override wins and derives the client socket, now pinned by tests in
`address.rs`) yields the same pair the server resolved. A constraint note sits
beside the API path field in `launch.rs`. Give one fixer `launch.rs`,
`workspace.rs` and the `PaneLaunchEnv` construction sites together.

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

## TERM-007 - Smaller vt and pty notes

Scope: vt-pty.

- **Scanner ground-state search is not memchr.** `Scanner::scan`
  (`crates/shepr-vt/src/scan.rs`) finds the next ESC with
  `iter().position(|&b| b == 0x1b)`, while vte uses `memchr` on the same bytes.
  It runs on every byte of every pane's output, just before vte scans the same
  slice again; `memchr` is already in the tree through vte.
- **`write_at` re-enters `with_handler` per scan segment.** Each segment opens
  and closes a row batch, locks the event mutex and folds damage. Correct, and
  segments are rare. With TERM-001's adapter-owned reply queue, scanner replies
  could go into the same queue from inside one advance; the segmenting exists
  only to feed the injected spellings (`CSI 3 J`, `CSI > 4 ; Pv m`).
- **`PaneTerminal::resize` puts earlier replies behind the resize's own.**
  `backend.rs` takes pending core replies, resizes, drains the resize's replies
  into the actor's resize slot, then `restore_pty_responses` the earlier ones for
  "the next read", so anything pending goes out after the later resize reply.
  Every writer collects its replies straight away, so the queue should be empty
  (only the test-only `mode_set` leaves some). If that holds,
  `restore_pty_responses` can be deleted; if not, pending replies should go ahead
  of the resize replies.
- **`PtyCommand` hands the server's `PWD`/`OLDPWD` to every pane.** `base_env`
  copies the server environment whole and nothing resets `PWD` to the pane's
  cwd. Shells fix `PWD` at startup, so harmless for the only production launch,
  but `base_env`'s doc lists what must not reach a pane, and a stale `PWD` fits.
- **`Terminal::tick` returns `true` for an empty expired frame.** The doc says
  it "Returns whether anything was flushed". Callers only bump an epoch and
  request a render; harmless.

## TERM-010 - A transient default-colour override can pin a pane's detection loop at 20 Hz indefinitely

Hunter's severity: Medium, hot path. Scope: mux-pane.

`DetectorState::tick_interval` returns `PROCESS_RECHECK_TRANSIENT` (50 ms)
whenever `terminal.has_transient_default_color_override()`, i.e. whenever
`transient_default_color_owner_pgid` is `Some`. `resolve_default_color_owner`
sets the owner as soon as a non-shell foreground program sets OSC 10/11,
regardless of host theme. It is cleared only by
`restore_host_terminal_theme_if_needed`, which returns early when
`core.host_terminal_theme.is_empty()` (and
`should_probe_host_terminal_theme_restore` refuses to try), or when the child
resets the colours itself.

- With an empty host theme (no client has reported one, or the client's terminal
  does not answer colour queries; the server's theme comes from the foreground
  client), the override is never restored, so the pane runs detection at 20 Hz
  for the rest of its life: two core locks for the override check, a
  `spawn_blocking` `/proc` read for the foreground group, another
  `spawn_blocking` for the restore probe, plus screen-scan gating.
- With a known theme, the 20 Hz cadence holds for as long as the owner stays in
  the foreground or on the alternate screen (vim, or an agent TUI that sets OSC
  11 and runs for hours).

The limit's own doc says the cadence is for "when a visible state change is
expected immediately", which neither case is; AGENTS.md "Hot paths multiply".

**Fix.** Use the fast cadence only for a bounded window after the foreground
group changes (the only event that can make a restore possible), keyed off the
foreground-group-change signal the detector already computes; do not arm it at
all while the host theme is empty.

## TERM-011 - `follow_cwd` bypasses the OSC 7 arbitration in the common case

Hunter's severity: Medium-low. Scope: mux-pane.

`ReportedCwd` and `PaneRuntime::cwd()` document a rule: OSC 7 "carries what
/proc cannot: a logical path through symlinks, or the directory of a program the
pane shell's /proc entry does not describe", and wins while the shell's /proc
cwd is unchanged since the report.

`PaneRuntime::cwd()` has no production caller. Production uses `follow_cwd()`
(from `shepr-server/src/app/creation.rs::launch_cwd_for_terminal`, for splits and
new workspaces), which reads the foreground group leader's `/proc` cwd first and
falls back to `cwd()` only when that read fails. With the shell in the
foreground (an idle prompt, the usual moment for a split), the leader is the
shell, so the split gets the physical `/proc` cwd and the OSC 7 logical path is
ignored. The documented arbitration runs only when the leader's cwd is
unreadable.

**Fix.** When the foreground group is the pane shell's own, use `cwd()`; read the
group leader only for a different foreground group. Or document that splits
follow the physical path.

## TERM-012 - Pid use after reap is guarded in one accessor and not the others

Hunter's severity: Low. Scope: mux-pane.

`PaneCwdProbe::read` refuses to read `/proc/<pid>` once `wait_completed()`,
because "its numeric PID may belong to another process by now". The same hazard
is unguarded in:

- `PaneRuntime::cwd()`, `follow_cwd()`, `foreground_cwd()` and `child_pid()`;
- the detection task, which keeps probing `child_liveness.pid()`
  (`foreground_process_group_id`, `probe_foreground_process`,
  `maybe_restore_host_terminal_theme`) until the runtime is dropped. Between the
  watcher's reap and the event loop processing `PaneDied` (the channel can back
  up; the watcher `send().await`s), a reused pid can be identified as an agent and
  published as `AgentProcessDetected` for a dead pane.

`PaneCwdProbe::read` itself is check-then-read: the child can be reaped between
`wait_completed()` and the readlink. A zombie keeps its pid until reaped, so the
sound order is read first, then confirm the leader is still unreaped
(`leader.is_unreaped()`, which `ProcessHandle` offers).

**Fix.** One `ChildLiveness::live_pid()` returning the pid only while unreaped,
re-checked after the `/proc` read, used by every accessor; the detection loop
should exit once `wait_completed()`.

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
