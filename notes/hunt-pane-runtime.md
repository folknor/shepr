# Defect hunt: pane runtime

Scope: `crates/shepr-mux/src/pane.rs`, `pane/`, `terminal/`, `render_signal.rs`.
Followed into `shepr-pty` (actor), `shepr-vt` (render state, sync tick), vte
0.15 (synchronized update), `persist/snapshot.rs` and the server's
`retained_surface.rs` / `client_shell.rs` where a value crossed the boundary.

Severity is my estimate of user impact. Each finding names the claim it breaks.

## Findings

### 1. The first detection poll is about 550 ms after launch, not the documented 50 ms (low, certain)

Claim: `limits::INITIAL_DETECTION_DELAY`, "Delay before the detector first
polls a newly launched pane, giving the shell time to put initial output on
the screen."

The detection task in `PaneRuntime::spawn_with_initial_history` sleeps
`INITIAL_DETECTION_DELAY`, then enters the loop, whose first act is to sleep
`next_wake`, which starts at `PROCESS_RECHECK_NO_AGENT` (500 ms), before it
runs any tick. The first `detector.tick` therefore runs at about 550 ms. So
the 50 ms constant only shifts the phase. It does not set when the first poll
happens, despite its name and doc. Either tick once before the first sleep
(so 50 ms is real), or delete the constant and say the first poll waits one
no-agent recheck interval.

### 2. The detection task waits on the synchronous terminal-core lock on Tokio workers, against the runtime's own stated rule (low-medium, certain)

Claim: `PaneReadEffects::arm_sync_timeout`, "The terminal and content locks
are synchronous. Keep their wait off a Tokio worker when a timer fires." The
detection loop also says of the theme probe "keep that probe off this worker".

The detection task correctly moves the `/proc` probes into `spawn_blocking`.
But it takes the core mutex directly on the async worker at least three times
per tick:

- `terminal.has_theme_restore_candidate()`, twice.
- `terminal.agent_detection_inputs()`, which formats the whole live screen
  (`terminal_detection_text` scans from the bottom for the last content row,
  then copies every screen row) while holding the lock.
- `clear_osc_evidence_for_agent_transition` on an agent change.

The lock's other holders do a lot of work while holding it:

- the PTY reader parses each chunk;
- a history save formats a `SCAN_CHUNK_ROWS` chunk per hold;
- a copy-mode search scans a chunk per hold;
- render runs `render_into`.

Detection runs every 300 to 500 ms for every pane, hidden ones included. So
`panes x lock wait` lands on Tokio workers, and those same workers run the
event loop's other tasks. The rule is applied to the sync timer and broken by
the much more frequent detection tick.

Fix: do the whole tick body that touches the terminal (theme check, screen
read) in the same `spawn_blocking` as the probe. Better still, run detection
on one dedicated thread per server that walks every pane: it is a polling
loop, and gains nothing from being an async task per pane.

### 3. `PaneRuntime` says dropping it "aborts async tasks"; two kinds of task outlive it and keep acting (low, certain)

Claim: the doc comment on `PaneRuntime`, "Dropping this aborts async tasks and
closes the PTY."

`Drop for PaneRuntime` aborts only the detection task. Two kinds of task keep
running:

- **Synchronized-output timer tasks.** These are spawned by
  `PaneReadEffects::arm_sync_timeout` and hold `Arc<PaneReadEffects>`. After
  the pane is gone they still:
  - wake;
  - run `flush_expired_synchronized_output` on the blocking pool (taking the
    content and core locks);
  - call `render_dirty.request_pty(pane_id)` and `notify_one` for a pane that
    no longer exists;
  - send `ClipboardWrite` and `TerminalCwdReported` events;
  - run `resolve_default_color_owner`, which scans `/proc`.

  The events carry the runtime generation, so the app drops them. But a dead
  pane can still wake the server's render loop, and a clipboard write from
  the final frame can be queued after removal.
- **The child watcher.** It is deliberately left running so the child is
  reaped. That part is correct, and the doc should say so.

Either abort the timer task on drop (keep its `AbortHandle` in
`SyncTimeoutRender`) or reword the doc. A `Weak` in the timer would also stop
it keeping the terminal alive.

### 4. Two definitions of "the pane's cwd": restore uses the one the runtime calls inferior (medium-low, certain about behaviour, intent unclear)

Claims:

- `ReportedCwd`: "OSC 7 carries what /proc cannot: a logical path through
  symlinks, or the directory of a program the pane shell's /proc entry does
  not describe (a nested shell, a root shell under sudo)."
- `PaneRuntime::cwd`: the OSC 7 report wins while the shell's /proc cwd is
  unchanged.

Everything live follows that arbitration: `cwd()`, `follow_cwd()`, and
`TerminalState::cwd` (fed by `TerminalCwdReported`). The save does not.
`persist::snapshot::capture_workspace` takes `remembered_cwd()` and then
replaces it with `PaneCwdProbe::read()`, which is the raw `/proc` readlink of
the shell. Two consequences:

- A pane sitting in `~/proj` through a symlink, in a nested shell, or in a
  `sudo -s` shell is restored into the physical `/proc` path of the outer
  shell, not the directory the user saw and that splits would have
  inherited.
- `remembered_cwd()` prefers `persistence_cwd` (the last `/proc` value a save
  saw) over the OSC 7 report, whatever their age. Once one save has run, a
  newer OSC 7 report never reaches the save fallback again. This matches its
  own doc comment, but it contradicts the "OSC 7 wins until the shell moves"
  rule the type holds everywhere else.

If the intent is "restore exactly where splits would go", the probe should
return `ReportedCwd::resolve(reported, proc_cwd)`, not `proc_cwd`. If the
intent is "restore the physical path on purpose", `ReportedCwd`'s doc
should say persistence is the exception. The owner needs to choose.

### 5. A dirty-patch collection marks cells past the collected width clean, though the code keeps rows past the collected height dirty (low, conditional)

Claim: `terminal_collect_dirty_patch`, "Only clear it after every row has been
collected successfully ... Rows below the area were not collected: they stay
dirty, and so does the overall state, so it only reads Clean when no row is
left to send."

Rows with `y >= area_height` keep their dirty flag. A row with `y <
area_height` gets `row.clear_dirty()` even when only `area_width` of its
cells went into the patch. Any change in columns `area_width..cols` is
dropped from every later patch. The retained-surface path collects at the
largest `inner_rect` among its recipients. That is narrower than the
terminal's columns when the view that set the PTY size is not a recipient
(for example, it is deferred and skipped by `retained_surface`'s `continue`),
or while layout and PTY size disagree. A later wider collection that is not
a full render would miss those cells. I did not find a sequence that reaches
a wider patch without an intervening full render, so this may be latent. But
the asymmetry is real, and the comment only covers the row case. Either keep
a row dirty when `area_width < cols` and the row's dirty cells extend past
it, or document why the column case cannot matter.

### 6. `collect_dirty_patch_snapshot` claims the revision and metadata are paired, but only writers that take the content lock are excluded (low, certain)

Claim: "The guard waits for announced writes to finish and excludes new ones,
so the revision and the terminal metadata remain paired throughout."

The snapshot takes the content write lock, then the core lock five separate
times (`collect_dirty_patch`, `scroll_metrics`, `mouse_reporting_enabled`,
`sgr_pixel_mouse_enabled`, `alternate_screen_active`). These mutators change
render-visible state without the content write lock:

- `scroll_up`, `scroll_down`, `scroll_reset`, `set_scroll_offset_from_bottom`
  change the viewport, so they change both the patch rows and
  `scroll_metrics`;
- `apply_host_terminal_theme`;
- `maybe_restore_host_terminal_theme`, which runs on the blocking pool from
  the detection task, so it really is on another thread;
- `apply_host_terminal_appearance`.

None of them advances `content_seq` either, so `client_shell`'s "revision
stable across the render" check cannot see them. The scroll mutators run on
the event loop, the same thread as the collector, so today the pairing holds
by thread affinity, not by the lock the comment names. Theme changes affect
colours only, which the snapshot metadata does not carry. So I found no
wrong frame. The comment overstates what is guaranteed, and the five
separate lock holds per pane per frame cost real time on the hot path.

See the structural section: putting the revision in the core makes this one
lock hold and makes the claim true.

### 7. `on_next_dirty_collection` hooks do not run on the next collection when it falls back (low, test seam)

Claim: `PaneRuntime::on_next_dirty_collection`, "Run `hook` inside the next
dirty-patch collection."

`PaneTerminal::collect_dirty_patch` returns early on synchronized output and
on a `Fallback` outcome (a visible hyperlink, a poisoned core) without taking
the hook. The hook waits for the first non-fallback collection, which can be
arbitrarily later. It does run on `Clean`. This is only used by tests
(`shepr-server` `test_support`, `invariant_tests`), but a test that relies on
"next" around a hyperlink or a synchronized frame will hang or see the hook
fire on the wrong frame. The hook also runs while the core and content locks
are held, so a hook that reads the runtime deadlocks. That deserves a sentence
in the doc.

### 8. `DetectorState::reset` can re-report a process exit, or delay one, when lifecycle authority is (re)activated around an exit (low, moderate confidence)

Claim: `DetectorState::reset`, "Lifecycle authority resets screen evidence,
not the process identity that ties a later confirmed exit back to that hook
generation."

`reset` keeps `current_agent()`, which includes `pending_confirmed_process_exit`,
as the present agent. It also clears `pending_confirmed_process_exit`,
`pending_foreground_shell_clear` and `foreground_shell_exit_reported`. Two
consequences:

- If an exit was already published (`foreground_shell_exit_reported == true`)
  and authority goes inactive to active before the `ClearAgent` probe, the
  agent survives the reset with the flag cleared. The next shell-foreground
  probe takes `ReportProcessExit` again and publishes a second
  `StateChanged { process_exited: true }` for the same exit.
- If the exit had just passed miss confirmation (`pending_confirmed_process_exit`
  set), `reset` brings the agent back as present with zero misses. The exit
  is reported only after another `AGENT_MISS_CONFIRMATION_ATTEMPTS` probes.

Whether the server can turn authority on in that window depends on the hook
report path (`terminal/state/hooks.rs` suppresses reports after an exit). I
could not rule it out. A test that drives `reset()` between the exit publish
and the clearing probe would settle it.

### 9. Read failures are logged as "mutation was not applied" and use up the one-shot mutation report (low, certain)

`PaneTerminal::agent_detection_inputs` and the `maybe_restore_host_terminal_theme`
read half call `report_terminal_mutation_failure`. That logs "terminal core
lock poisoned; mutation was not applied" and sets the once-per-pane latch.
`agent_detection_inputs` is a read. When detection hits the poisoned core
first, which is likely since it polls, the log line is wrong and a later
real mutation failure is never logged. The field doc on `PaneTerminal::core`
says "operations without a failure return log their skipped operation once
per pane". Give reads their own latch, or do not log them: the actor already
reports the poisoned core.

### 10. Actor-startup failure blocks the caller on `child.wait()` (low)

In `spawn_with_initial_history`'s `PtyIoActor::spawn` error arm, the code
calls `child.kill()` then a synchronous `child.wait()` on the spawning thread,
which is the server event loop. SIGKILL is normally immediate. But a child in
uninterruptible sleep (a cwd on a hung network mount, which `require_cwd`
resumes make more likely) holds the event loop until the kernel releases it.
The normal path deliberately reaps off-thread (`UnreapedChild`, pidfd,
`spawn_blocking`). Hand the child to the same detached reaper here.

## Hot-path observations (not defects)

- `RenderSignal::request_pty` takes a mutex and does a `HashSet` insert on
  every PTY read of every pane, even when that pane is already pending. A
  per-pane `AtomicBool` "queued" flag held in `PaneReadEffects` (cleared by
  `take`) would make the common repeated read lock-free.
- Each PTY read does: content write lock, core lock, `request_pty` mutex,
  plus two atomics. With the revision inside the core (below), this drops to
  the core lock plus the render signal.

## Structural opportunities

- **Put the content revision inside `PaneTerminalCore`.** `content_seq`,
  `content_write_lock`, `ContentWriteGuard` (its odd/even protocol, `cancel`
  and its unwinding rules) and `PaneOutputWriter::try_begin` all exist only
  to pair a revision counter with core state across separate core-lock
  holds. Bump the revision under the core lock in every mutator and have the
  snapshot read patch, revision and metadata in one hold. This:
  - removes a whole lock layer and one lock-order level (`reply-order ->
    content -> core` becomes `reply-order -> core`);
  - makes finding 6's claim true by construction;
  - stops the theme and scroll mutators from bypassing the revision.

  `detection_content_seq` can live there too. It is already only "advanced
  after the parse" by convention.
- **One shared struct instead of about ten Arcs.** `PaneRuntime`,
  `PaneReadEffects` and `PaneOutputWriter` each hold separate `Arc`s to the
  same items:
  - `terminal`, `content_seq`, `content_write_lock`, `detection_content_seq`;
  - `reported_cwd`, `child_liveness`, `full_lifecycle_authority_active`;
  - `persistence_cwd`, `detect_reset_notify`.

  One `Arc<PaneShared>` would make clone sites and ownership readable. It
  would also make the drop story in finding 3 explicit: a `Weak<PaneShared>`
  in the timer.
- **Split `spawn_with_initial_history`.** It is about 450 lines and builds
  the terminal, the PTY, the read callback, the reader-exit callback, the
  child watcher and the whole detection loop inline. Extract a
  `DetectionTask` (or the dedicated detection thread from finding 2) that
  takes `PaneShared` and owns the tick loop, and a `ChildWatcher`. Then the
  detection loop's lock and blocking policy can be tested on its own.
- **Hook arbitration in `terminal/state`.** Many flags drive the logic in
  `hooks.rs`, `sessions.rs`, `source.rs`, `detection.rs` and `lifecycle.rs`:
  - `hook_authority`, `persisted_agent_session`, `recent_agent_process_exit`;
  - the per-source `HookGeneration` (`Open`, `AwaitingProcess`, `Cleared`),
    `pending_start`, `pending_replacement_report`, `stale_sessions`;
  - sequence re-anchoring.

  The entry points (`set_hook_report_at`, `set_agent_session_ref_*`,
  `set_detected_state_with_screen_signals_at`) each reimplement parts of the
  routing. I found no concrete defect in the time I had, but this is where
  the next ones will be. A single explicit per-source state machine (a
  `(generation, event) -> (generation, effects)` table) would replace about
  a dozen `pub(super)` predicates and make the invariants in the
  `HookSourceState` doc checkable.

## Checked and found sound

- **Deferred-effect ticket ordering.** `DeferredEffectOrder`: drop-finishes,
  out-of-order finishes, and condvar parking.
- **Lock order.** Reader, timer, resize and appearance all go reply-order,
  then content, then core.
- **Sync timer coalescing.** `SyncTimeoutRender`. vte's `stop_sync` always
  clears the mode, so a timer flush cannot leave an update open with no
  timer.
- **History cache soundness.**
  - Eviction, resumed chunks, the epoch reset on resize.
  - The alternate-screen refusal.
  - `Weak` identity: a dangling `Weak` pins the allocation, so the address
    cannot be reused.
- **Seeded-row masking.** `visit_screen_row_text_with_seeded` treats a live
  cell before a seeded one as live.
- **Pid-reuse guards.** Around every `/proc` read: `live_pid` is checked
  before and after.
- **Teardown tracker accounting.** Including the inline fallback when a
  thread cannot be spawned.
- **Env scrubbing.** `SHEPR_DEBUG_OSC_EVIDENCE`, `SHEPR_PANE_ID` and
  `SHEPR_STARTUP_CWD` cannot reach a child, even through `extra`. The socket
  pair and the build-profile marker are always exported, as AGENTS.md says.
- **OSC 7 dedupe.** A dropped report retries.
