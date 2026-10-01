# Defects: pane runtime

Filed from the defect hunt over `crates/shepr-mux/src/` `pane.rs`, `pane/`,
`terminal/` and `render_signal.rs`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PRUN-001 - The first detection poll is about 550 ms after launch, not the documented 50 ms

Hunter's rating: low, certain. Claim: `limits::INITIAL_DETECTION_DELAY`, "Delay
before the detector first polls a newly launched pane, giving the shell time to
put initial output on the screen."

The detection task in `PaneRuntime::spawn_with_initial_history` sleeps
`INITIAL_DETECTION_DELAY`, then enters the loop, whose first act is to sleep
`next_wake`, which starts at `PROCESS_RECHECK_NO_AGENT` (500 ms), before any
tick. The first `detector.tick` runs at about 550 ms; the 50 ms constant only
shifts the phase. Either tick once before the first sleep, or delete the
constant and say the first poll waits one no-agent recheck interval.

## PRUN-002 - The detection task waits on the synchronous terminal-core lock on Tokio workers

Hunter's rating: low-medium, certain. Claim: `PaneReadEffects::arm_sync_timeout`,
"The terminal and content locks are synchronous. Keep their wait off a Tokio
worker when a timer fires." The detection loop also says of the theme probe
"keep that probe off this worker".

The detection task moves the `/proc` probes into `spawn_blocking` but takes the
core mutex directly on the async worker at least three times per tick:
`terminal.has_theme_restore_candidate()` (twice), `terminal.agent_detection_inputs()`
(formats the whole live screen while holding the lock), and
`clear_osc_evidence_for_agent_transition` on an agent change. The lock's other
holders do a lot under it: the PTY reader parses each chunk, a history save
formats a `SCAN_CHUNK_ROWS` chunk per hold, copy-mode search scans a chunk per
hold, render runs `render_into`. Detection runs every 300 to 500 ms for every
pane, hidden ones included, so panes x lock wait lands on the workers that run
the event loop's other tasks.

Fix: do the whole tick body that touches the terminal in the same
`spawn_blocking` as the probe. Better: one dedicated detection thread per
server that walks every pane; it is a polling loop and gains nothing from being
an async task per pane.

## PRUN-003 - `PaneRuntime` says dropping it "aborts async tasks"; two kinds of task outlive it

Hunter's rating: low, certain. Claim: the doc on `PaneRuntime`, "Dropping this
aborts async tasks and closes the PTY."

`Drop for PaneRuntime` aborts only the detection task.

- Synchronized-output timer tasks, spawned by `PaneReadEffects::arm_sync_timeout`
  and holding `Arc<PaneReadEffects>`, still wake after the pane is gone: run
  `flush_expired_synchronized_output` on the blocking pool (content and core
  locks), call `render_dirty.request_pty(pane_id)` and `notify_one` for a pane
  that no longer exists, send `ClipboardWrite` and `TerminalCwdReported` events,
  and run `resolve_default_color_owner` (a `/proc` scan). The events carry the
  runtime generation so the app drops them, but a dead pane can still wake the
  render loop, and a clipboard write from the final frame can be queued after
  removal.
- The child watcher is deliberately left running so the child is reaped. That
  is correct; the doc should say so.

Either abort the timer task on drop (keep its `AbortHandle` in
`SyncTimeoutRender`) or reword the doc. A `Weak` in the timer would also stop it
keeping the terminal alive.

## PRUN-004 - Two definitions of "the pane's cwd": restore uses the one the runtime calls inferior

Hunter's rating: medium-low, certain about behaviour, intent unclear. Also
surfaced as a lateral in the mux and persistence hunt.

Claims: `ReportedCwd`: "OSC 7 carries what /proc cannot: a logical path through
symlinks, or the directory of a program the pane shell's /proc entry does not
describe (a nested shell, a root shell under sudo)." `PaneRuntime::cwd`: the
OSC 7 report wins while the shell's /proc cwd is unchanged.

Everything live follows that arbitration (`cwd()`, `follow_cwd()`,
`TerminalState::cwd` fed by `TerminalCwdReported`). The save does not:
`persist::snapshot::capture_workspace` takes `remembered_cwd()` and replaces it
with `PaneCwdProbe::read()`, the raw `/proc` readlink of the shell.

- A pane in `~/proj` through a symlink, in a nested shell, or in a `sudo -s`
  shell is restored into the physical `/proc` path of the outer shell, not the
  directory the user saw and that splits would have inherited.
- `remembered_cwd()` prefers `persistence_cwd` (the last `/proc` value a save
  saw) over the OSC 7 report, whatever their age, so after one save a newer
  OSC 7 report never reaches the save fallback again. The mux hunter adds: when
  a later probe fails (for example, the shell has just exited), the save writes
  the older probed cwd even though the shell reported a newer one.

If the intent is "restore exactly where splits would go", the probe should
return `ReportedCwd::resolve(reported, proc_cwd)`, not `proc_cwd`. If the intent
is "restore the physical path on purpose", `ReportedCwd`'s doc should say
persistence is the exception. The owner needs to choose.

## PRUN-005 - `on_next_dirty_collection` hooks do not run on the next collection when it falls back

Hunter's rating: low, test seam. Claim: `PaneRuntime::on_next_dirty_collection`,
"Run `hook` inside the next dirty-patch collection."

`PaneTerminal::collect_dirty_patch` returns early on synchronized output and on
a `Fallback` outcome (a visible hyperlink, a poisoned core) without taking the
hook, so it waits for the first non-fallback collection, which can be
arbitrarily later; it does run on `Clean`. Used only by tests (`shepr-server`
`test_support`, `invariant_tests`), but a test relying on "next" around a
hyperlink or synchronized frame will hang or see the hook fire on the wrong
frame. The hook also runs with the core and content locks held, so a hook that
reads the runtime deadlocks; the doc should say so.

## PRUN-006 - `collect_dirty_patch_snapshot` claims revision and metadata are paired, but only content-lock writers are excluded

Hunter's rating: low, certain. Claim: "The guard waits for announced writes to
finish and excludes new ones, so the revision and the terminal metadata remain
paired throughout."

The snapshot takes the content write lock, then the core lock five separate
times (`collect_dirty_patch`, `scroll_metrics`, `mouse_reporting_enabled`,
`sgr_pixel_mouse_enabled`, `alternate_screen_active`). These mutators change
render-visible state without the content write lock: `scroll_up`,
`scroll_down`, `scroll_reset`, `set_scroll_offset_from_bottom` (viewport, so
patch rows and `scroll_metrics`), `apply_host_terminal_theme`,
`maybe_restore_host_terminal_theme` (on the blocking pool from detection, so
another thread), `apply_host_terminal_appearance`. None advances `content_seq`,
so `client_shell`'s "revision stable across the render" check cannot see them.
The scroll mutators run on the event loop with the collector, so the pairing
holds today by thread affinity, not by the lock the comment names; theme changes
affect colours only. No wrong frame found; the comment overstates the guarantee,
and five lock holds per pane per frame cost real time on the hot path. See
PRUN-008.

## PRUN-007 - `RenderSignal::request_pty` takes a mutex on every PTY read

Hot-path observation. `request_pty` takes a mutex and does a `HashSet` insert on
every PTY read of every pane, even when the pane is already pending. A per-pane
`AtomicBool` "queued" flag in `PaneReadEffects` (cleared by `take`) would make
the common repeated read lock-free. Each PTY read today does: content write
lock, core lock, the `request_pty` mutex, plus two atomics; with the revision in
the core (PRUN-008) this drops to the core lock plus the render signal.

## PRUN-008 - Structural: put the content revision inside `PaneTerminalCore`

`content_seq`, `content_write_lock`, `ContentWriteGuard` (its odd/even protocol,
`cancel` and unwinding rules) and `PaneOutputWriter::try_begin` exist only to
pair a revision counter with core state across separate core-lock holds. Bump
the revision under the core lock in every mutator and have the snapshot read
patch, revision and metadata in one hold. This removes a lock layer and one
lock-order level (`reply-order -> content -> core` becomes
`reply-order -> core`), makes PRUN-006's claim true by construction, and stops
the theme and scroll mutators from bypassing the revision.
`detection_content_seq` can live there too.

## PRUN-009 - Structural: one shared struct instead of about ten Arcs

`PaneRuntime`, `PaneReadEffects` and `PaneOutputWriter` each hold separate
`Arc`s to the same items: `terminal`, `content_seq`, `content_write_lock`,
`detection_content_seq`, `reported_cwd`, `child_liveness`,
`full_lifecycle_authority_active`, `persistence_cwd`, `detect_reset_notify`. One
`Arc<PaneShared>` would make clone sites and ownership readable and make the
drop story in PRUN-003 explicit (a `Weak<PaneShared>` in the timer).

## PRUN-010 - Structural: split `spawn_with_initial_history`

About 450 lines building the terminal, the PTY, the read callback, the
reader-exit callback, the child watcher and the whole detection loop inline.
Extract a `DetectionTask` (or the dedicated detection thread from PRUN-002) that
takes `PaneShared` and owns the tick loop, and a `ChildWatcher`, so the
detection loop's lock and blocking policy can be tested alone.

## PRUN-011 - Structural: hook arbitration in `terminal/state` as one per-source state machine

Many flags drive the logic in `hooks.rs`, `sessions.rs`, `source.rs`,
`detection.rs` and `lifecycle.rs`: `hook_authority`, `persisted_agent_session`,
`recent_agent_process_exit`; the per-source `HookGeneration` (`Open`,
`AwaitingProcess`, `Cleared`), `pending_start`, `pending_replacement_report`,
`stale_sessions`; sequence re-anchoring. The entry points
(`set_hook_report_at`, `set_agent_session_ref_*`,
`set_detected_state_with_screen_signals_at`) each reimplement parts of the
routing. No concrete defect found, but the hunter expects the next ones here. A
single explicit `(generation, event) -> (generation, effects)` table would
replace about a dozen `pub(super)` predicates and make the invariants in the
`HookSourceState` doc checkable.
