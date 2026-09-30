# Hunt: shepr-mux pane runtime (`crates/shepr-mux/src/pane.rs`, `crates/shepr-mux/src/pane/`)

Scope read: `runtime.rs`, `teardown.rs`, `launch.rs`, `process_probe.rs`,
`osc.rs`, `cursor.rs`, `state.rs`, `runtime_registry.rs`, `terminal.rs` and
`terminal/{backend,helpers,history,text}.rs`. `agent_detection.rs` and
`crates/shepr-mux/src/terminal` were left to their own hunters. Followed values
into `shepr-pty/src/actor.rs`, `shepr-vt/src/render.rs`,
`shepr-platform/src/process.rs`, `shepr-server` (`client_shell.rs`,
`creation.rs`, `pane_input.rs`, `retained_surface.rs`) and
`shepr-mux/src/persist/{restore,snapshot}.rs`.

Ordered by severity.

---

## 1. Bracketed-paste sanitizer is bypassable by nesting the markers (high)

`PaneRuntime::paste_payload` (`runtime.rs`):

```rust
let safe = text.replace("\x1b[201~", "").replace("\x1b[200~", "");
format!("\x1b[200~{safe}\x1b[201~")
```

Two single-pass replacements do not reach a fixed point. Removing one marker
can assemble the other out of the surrounding bytes:

- `"\x1b[20\x1b[200~1~"`: the first pass finds no `ESC[201~`; the second
  removes `ESC[200~` and leaves `ESC[201~`.
- The mirror case `"\x1b[20\x1b[201~0~"` survives as `ESC[200~` (harmless
  inside a paste, but shows the same flaw).

So a pasted string like `"\x1b[20\x1b[200~1~\nrm -rf ~\n"` ends the bracketed
paste early and the rest reaches the shell as typed input, which is exactly the
paste-jacking the sanitization exists to stop. The claim it breaks: the
`safe` variable and the test `bracketed_paste_neutralizes_embedded_markers`,
which only checks the non-nested case. The text comes from the client's
clipboard through `shepr-server/src/server/pane_input.rs::send_paste`.

Fix: strip every ESC (and arguably all C0 except `\t\r\n`) from the payload
while bracketed paste is on, or loop the removal until nothing changes. Add
the nested case to the test.

## 2. `SHEPR_PANE_ID` from the server's own environment leaks into panes (medium)

`launch.rs`: `EnvVar::SheprPaneId` has policy `Allowed`, and
`PaneLaunchEnv::pane_id` is an `Option` whose `None` is documented as "inherits
whatever the server environment carries". Nothing anywhere removes
`SHEPR_PANE_ID` from the server's environment (grep finds no `env_remove`
for it outside the explicit set in `apply_pane_launch_env`).

`persist/restore.rs` builds the launch env with `with_pane_id` only when the
old id maps to a public id that parses; otherwise the pane launches with
`pane_id: None`. The server process inherits `SHEPR_PANE_ID` whenever it was
started from inside a shepr pane, which is the documented dev-next-to-release
workflow (AGENTS.md: "Run it with plain `brokkr run` ... including from inside
a pane of the installed server"). Such a restored pane then carries the outer
server's pane id while its `SHEPR_SOCKET_PATH` points at this server, so the
integration hooks (every `assets/*/shepr-agent-state.*` reads
`SHEPR_PANE_ID`) report state for a pane id that belongs to another server,
and if the string names a pane here too, the state is attributed to the wrong
pane.

Claim broken: the pane-env policy doc ("the inherited value describes
something outside this pane ... is removed" is exactly what this variable is),
and the integration contract that hooks report for their own pane.

Fix: make the pane id mandatory in `PaneLaunchEnv` (every spawn path can
produce one; restore can allocate a fresh public id when the old one is
missing) and classify `SheprPaneId` as `Scrubbed` so an absent id is never an
inherited one.

## 3. A transient default-colour override can pin a pane's detection loop at 20 Hz indefinitely (medium, hot path)

`DetectorState::tick_interval` returns `PROCESS_RECHECK_TRANSIENT` (50 ms)
whenever `terminal.has_transient_default_color_override()` is true, i.e.
whenever `transient_default_color_owner_pgid` is `Some`. The owner is set by
`resolve_default_color_owner` as soon as a non-shell foreground program sets
OSC 10/11, regardless of the host theme. It is only cleared by
`restore_host_terminal_theme_if_needed`, which returns early when
`core.host_terminal_theme.is_empty()` (and `should_probe_host_terminal_theme_restore`
refuses to even try), or when the child itself resets the colours.

Consequences:

- With an empty host theme (no client has reported one yet, or the client's
  terminal does not answer colour queries; the server's theme comes from the
  foreground client), the override can never be restored, so the pane runs
  its whole detection iteration at 20 Hz for the rest of its life: two locks
  of the core for the override check, a `spawn_blocking` `/proc` read for the
  foreground group, another `spawn_blocking` for the restore probe, plus the
  screen-scan gating.
- With a known theme, the same 20 Hz cadence holds for as long as the owner
  stays in the foreground or on the alternate screen (vim or an agent TUI
  that sets OSC 11 and runs for hours).

The limit's own doc says the cadence is for "when a visible state change is
expected immediately", which neither case is. AGENTS.md "Hot paths multiply":
this is per pane, forever.

Fix: only use the fast cadence for a bounded window after the foreground
group changes (the only event that can make a restore possible), keyed off the
foreground-group-change signal the detector already computes; do not arm it at
all while the host theme is empty.

## 4. `follow_cwd` bypasses the OSC 7 arbitration in the common case (medium-low)

`ReportedCwd` and `PaneRuntime::cwd()` document a careful rule: OSC 7 "carries
what /proc cannot: a logical path through symlinks, or the directory of a
program the pane shell's /proc entry does not describe", and wins while the
shell's /proc cwd is unchanged since the report.

`PaneRuntime::cwd()` has no production caller. The production path is
`follow_cwd()` (used by `shepr-server/src/app/creation.rs::launch_cwd_for_terminal`
for splits and new workspaces), which first reads the foreground process
group leader's `/proc` cwd and only falls back to `cwd()` when that read fails.
When the shell itself is in the foreground (an idle prompt, the usual moment
for a split), the group leader is the shell, so the split gets the shell's
physical `/proc` cwd and the OSC 7 logical path is ignored. The documented
arbitration only runs when the foreground leader's cwd is unreadable.

Fix: when the foreground group is the pane shell's own group, use `cwd()`
(the arbitration); read the group leader only for a different foreground
group. Or state in the docs that splits follow the physical path.

## 5. Pid use after reap is guarded in one accessor and not the others (low)

`PaneCwdProbe::read` refuses to read `/proc/<pid>` once `wait_completed()`,
because "its numeric PID may belong to another process by now". The same
hazard is unguarded in:

- `PaneRuntime::cwd()`, `follow_cwd()`, `foreground_cwd()` and `child_pid()`;
- the detection task, which keeps probing `child_liveness.pid()`
  (`foreground_process_group_id`, `probe_foreground_process`,
  `maybe_restore_host_terminal_theme`) until the runtime is dropped. Between
  the watcher's reap and the event loop processing `PaneDied` (the channel can
  be backed up; the watcher `send().await`s), a reused pid can be identified
  as an agent and published as `AgentProcessDetected` for a dead pane.

`PaneCwdProbe::read` itself is check-then-read: the child can be reaped
between `wait_completed()` and the readlink. Since a zombie keeps its pid
until reaped, the sound order is read first, then confirm the leader is still
unreaped (`leader.is_unreaped()`), which the `ProcessHandle` already offers.

Fix: one `ChildLiveness::live_pid()` that returns the pid only while
unreaped, re-checked after the `/proc` read, used by every accessor; the
detection loop should exit once `wait_completed()`.

## 6. `read_primary_history` clears the cache on some failures, contrary to `refresh`'s doc (low)

`PaneHistorySource::refresh` documents "`false`, with the cache left as it
was, while the alternate screen is active ... or when the terminal cannot be
read". Several failure paths in `read_primary_history` (`format_chunk`
returning `None`, `screen_row_wrap` failing, `terminal_recent_read_range`
erroring, the "open line with no rows left" branch) call `cache.clear()` and
return `None`. `persist/snapshot.rs::HistoryCarry` relies on the cache being
"the one copy of a pane's history that outlives a save", so a cleared cache
saves as no history and the pane's saved history is lost. These paths should
be rare, but the contract says the cache survives them.

Fix: on failure leave the cache untouched (drop only the chunk being built),
or change the doc and have the save treat `false` from a clearing path as
"keep the previous saved history".

## 7. Deferred read effects block the PTY reader thread (low, perf smell)

`PaneReadEffects::apply_deferred` runs as the read's `after_response_order`
on the PTY actor thread. It performs `current_transient_default_color_owner`
(a `foreground_job` `/proc` scan) for every OSC 10/11 set, and for OSC 7 a
readlink plus `UsableCwd::new` (a `stat`, which can hang on a dead network
mount). The ticket gate can also park the reader behind the sync-timeout
timer's `spawn_blocking` task. The comments are accurate (no lock is held),
but the reader loop, and therefore every write and resize for that pane,
stalls for the duration. A pane that sets OSC 11 per frame would scan `/proc`
per read. Moving these to a per-pane worker (they are already ordered by
tickets) would keep the reader thread IO-only.

## 8. Resize scroll recovery rebuilds the whole screen text up to 8x rows times under the core lock (low, perf)

`PaneTerminal::resize` (`terminal/backend.rs`), when the viewport was
scrolled into history, loops up to `max(rows * 8, 24)` times calling
`terminal_visible_text(&mut core)`, each of which runs `render_state.update`
(a full row copy, since the display offset changed) and builds a `String` of
the whole screen, while holding the core lock that the PTY reader, rendering
and detection wait on. For a 60x200 pane that is up to 480 full-screen copies
per resize. A per-row blank check through `visit_screen_row_text` on the rows
entering the viewport would do the same in O(rows).

## 9. The output-writer seam does not do what the PTY reader does (low)

`PaneOutputWriter`'s doc: "The PTY reader writes through one; so does anything
else that feeds a pane its child's output", and `PaneRuntime::output_writer`:
"feeds this pane its child's output, as the PTY reader does".
`PaneOutputWrite::write` discards the whole `ProcessBytesResult`: terminal
replies, render and title requests, clipboard writes, cwd reports, the
synchronized-output timer, and it never advances `detection_content_seq`.
Its only users are tests (`shepr-server/src/test_support.rs` and this crate),
so tests written against it silently skip the reader's effect path.
Related: `with_child_io` writes `screen` before `PaneTerminal::new`, which
discards only pending PTY replies, so any title, clipboard, pwd or colour
effects in the seeded screen surface as live effects of the first real write
(`seed_history_ansi` uses `discard_core_effects` for this; `with_child_io`
should too). Also `on_next_dirty_collection`'s hook is consumed even when the
collection then falls back (hyperlink present) and the snapshot is discarded.

## 10. Smaller items

- `PaneState` doc says "Viewport state for a pane"; AGENTS.md defines it as
  only the terminal link plus per-pane input flags, which is what it holds.
- `DetectorState::new` starts `state` at `AgentState::Idle`, while `reset()`
  and every agent change set `AgentState::Unknown`; the first publish decision
  of a fresh pane compares against a different baseline than a reset pane.
- `try_send_focus_event` returns `true` when the send failed (the bool means
  "focus reporting is on"); the only caller ignores it, so the return value
  can go.
- `collect_dirty_patch_snapshot` checks for an odd revision and re-reads the
  revision after collecting, both while holding `content_write_lock`, which
  every writer holds for its whole odd window: neither check can fail. Harmless
  but it suggests a lock-free protocol that is not what runs.
- `sanitized_osc_debug_payload` decides on the trailing `...` by counting all
  chars, control characters included, while truncation counts only the kept
  ones, so a short payload with control characters gets a spurious ellipsis.
- `SHEPR_DEBUG_OSC_EVIDENCE` with a refused value logs and stays off rather
  than failing; it is an environment flag, not config, so it is outside the
  "config problem fails the launch" rule, but it is the one launch-time
  setting in this scope that falls back silently.
