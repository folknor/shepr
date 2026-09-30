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

## TERM-001 - OSC colour query answers are resolved at the end of the parse, not at the query's position

Scope: vt-pty.

**Claim broken.** `crates/shepr-vt/src/color.rs`, `ColorQuery` doc: "`core_color`
is what the terminal itself would report (...), captured at the query's position
in the stream." `ColorQuery::child_override`: "answered from the child's own OSC
10/11 override *at the moment it was asked*". The lib.rs module doc promises
query replies "in byte order".

**What the code does.** `Term::dynamic_color_sequence` only pushes
`Event::ColorRequest(index, fmt)` into the listener queue. The colour is resolved
later in `Terminal::drain_events` (`crates/shepr-vt/src/lib.rs`), which
`with_handler` runs once after `parser.advance` has consumed the whole segment,
so every colour change later in the segment is already applied when
`core_query_color` and `default_color_override` run.

Input in one read: `ESC ] 11 ; ? BEL   ESC ] 11 ; rgb:11/22/33 BEL`. Contract:
answer the host background (or nothing if unset) with `child_override == false`.
Actual: `core_color = 11/22/33`, `child_override = true`, so the pane echoes the
new colour back in the child's own form
(`crates/shepr-mux/src/pane/terminal/helpers.rs`, `color_query_response`). Same
for OSC 4 palette and OSC 12 queries, and for everything in a
synchronized-update frame, since a frame is replayed as one `advance`
(`Processor::stop_sync_internal`), including when `tick` flushes it. "Query the
background, then set a new one" in one write is what theme-switching tools do.
The tests only cover set-then-query (`tests.rs`, `]11;rgb:...` then `]11;?`).

**Fix.** Resolve the colour at dispatch: `CoreHandler::dynamic_color_sequence`
runs at the right moment; give the handler the host fg/bg and default palette (it
already carries `cell`), compute `core_color` and `child_override` there, and
queue a typed adapter event. More broadly, stop using alacritty's `Event` enum as
the reply queue: have `CoreHandler` push shepr-typed replies (bytes, resolved
colour query, title, clipboard) into its own `Vec`, so every effect is captured
at its byte position by construction, and the `Arc<Mutex<Vec<Event>>>` listener
plus the `set_history_lines` truncation trick shrink to the few events only
`Term` can emit (title from `set_options`/`pop_title`).

## TERM-002 - Pane clear on the alternate screen reports success but does nothing

Scope: vt-pty.

**Claim broken.** `Terminal::clear_screen` returns the typed
`ClearScreenOutcome::AlternateScreenActive` so the caller can tell a no-op from a
clear ("A no-op returning `AlternateScreenActive` while the alternate screen is
active").

**What the code does.** `PaneTerminal::clear_screen`
(`crates/shepr-mux/src/pane/terminal/backend.rs`) does
`let _ = core.terminal.clear_screen(); Ok(())`. `PaneClearError`
(`crates/shepr-mux/src/pane/terminal.rs`) only has `TerminalLockPoisoned`, so
`App::handle_pane_clear` (`crates/shepr-server/src/app/api/panes/copy.rs`)
answers `Handled::done()` for a clear that did not happen.
`PaneRuntime::clear_screen` also bumps the detection content sequence anyway.

**Fix.** Carry the outcome through (an `AlternateScreenActive` variant, or return
the outcome) and have the endpoint reject with "the pane is on the alternate
screen".

## TERM-003 - Clearing history does not bring a widened pane back under its scrollback budget

Scope: vt-pty.

**Claim broken.** `Terminal::resize` comment (`crates/shepr-vt/src/lib.rs`): "a
widened pane holds more than its byte budget until it narrows again (or its
history is cleared)."

**What the code does.** The inflated line limit (`history_lines`) is only
recomputed inside `resize`. ED 3 (`CoreHandler::clear_screen`), RIS
(`CoreHandler::reset_state`) and the host clear (`Terminal::clear_screen`) empty
the history but leave `history_lines` (and alacritty's `max_scroll_limit`) at the
widened value. New output refills history to that limit at the wide column
count, above the byte budget, until a later resize recomputes it.

**Fix.** After any purge that leaves `history_size() == 0` on the primary screen,
call `set_history_lines(scrollback_lines(max_scrollback, cols))` (from
`Terminal`, since the handler would need `max_scrollback`; or give the handler a
"history purged" flag that `with_handler` acts on). Otherwise drop the
parenthetical.

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

## TERM-008 - Bracketed-paste sanitizer is bypassed by nesting the markers

Hunter's severity: High. Scope: mux-pane.

`PaneRuntime::paste_payload` (`runtime.rs`):

```rust
let safe = text.replace("\x1b[201~", "").replace("\x1b[200~", "");
format!("\x1b[200~{safe}\x1b[201~")
```

Two single-pass replacements do not reach a fixed point; removing one marker can
assemble the other from the surrounding bytes:

- `"\x1b[20\x1b[200~1~"`: the first pass finds no `ESC[201~`; the second removes
  `ESC[200~` and leaves `ESC[201~`.
- The mirror `"\x1b[20\x1b[201~0~"` survives as `ESC[200~` (harmless inside a
  paste, same flaw).

So a pasted `"\x1b[20\x1b[200~1~\nrm -rf ~\n"` ends the bracketed paste early and
the rest reaches the shell as typed input: the paste-jacking the sanitization
exists to stop. Claim broken: the `safe` variable and the test
`bracketed_paste_neutralizes_embedded_markers`, which only checks the non-nested
case. The text comes from the client's clipboard through
`shepr-server/src/server/pane_input.rs::send_paste`.

**Fix.** Strip every ESC (and arguably all C0 except `\t\r\n`) from the payload
while bracketed paste is on, or loop the removal until nothing changes. Add the
nested case to the test.

## TERM-009 - `SHEPR_PANE_ID` from the server's own environment leaks into panes

Hunter's severity: Medium. Scope: mux-pane; also surfaced as a lateral finding
in agent-integration.

In `launch.rs`, `EnvVar::SheprPaneId` has policy `Allowed`, and
`PaneLaunchEnv::pane_id` is an `Option` whose `None` is documented as "inherits
whatever the server environment carries". Nothing removes `SHEPR_PANE_ID` from
the server's environment (no `env_remove` for it outside the explicit set in
`apply_pane_launch_env`).

`persist/restore.rs` builds the launch env with `with_pane_id` only when the old
id maps to a public id that parses; otherwise the pane launches with
`pane_id: None`. The server inherits `SHEPR_PANE_ID` whenever it was started from
inside a shepr pane, which is the documented dev-next-to-release workflow
(AGENTS.md: "Run it with plain `brokkr run` ... including from inside a pane of
the installed server"). Such a restored pane carries the outer server's pane id
while its `SHEPR_SOCKET_PATH` points at this server, so the integration hooks
(every `assets/*/shepr-agent-state.*` reads `SHEPR_PANE_ID`) report for a pane id
belonging to another server, and if the string names a pane here too, the state
lands on the wrong pane.

**Claims broken.** The pane-env policy doc ("the inherited value describes
something outside this pane ... is removed" is exactly this variable), and the
integration contract that hooks report for their own pane.

The integration hunter traced the inheritance itself:
`build_server_daemon_command` (`shepr-remote/src/remote/local_server.rs`) passes
the caller's environment through, so a server started from inside another
server's pane inherits that pane's `SHEPR_PANE_ID` and `SHEPR_ENV`. In that
hunter's reading the usual outcome is "pane not found", and in principle it could
hit an unrelated pane.

**Fix.** Make the pane id mandatory in `PaneLaunchEnv` (every spawn path can
produce one; restore can allocate a fresh public id when the old one is missing)
and classify `SheprPaneId` as `Scrubbed` (the integration hunter suggests
`Scrubbed` or `ServerOnly`) so an absent id is never an inherited one.

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

## TERM-013 - `read_primary_history` clears the history cache on some failures, contrary to `refresh`'s doc

Hunter's severity: Low. Scope: mux-pane.

`PaneHistorySource::refresh` documents "`false`, with the cache left as it was,
while the alternate screen is active ... or when the terminal cannot be read".
Several failure paths in `read_primary_history` (`format_chunk` returning
`None`, `screen_row_wrap` failing, `terminal_recent_read_range` erroring, the
"open line with no rows left" branch) call `cache.clear()` and return `None`.
`persist/snapshot.rs::HistoryCarry` relies on the cache being "the one copy of a
pane's history that outlives a save", so a cleared cache saves as no history and
the pane's saved history is lost. Rare, but the contract says the cache survives.

**Fix.** On failure leave the cache untouched (drop only the chunk being built),
or change the doc and have the save treat `false` from a clearing path as "keep
the previous saved history".

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
