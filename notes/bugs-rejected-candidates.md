# Defects proposed for rejection

Findings from the same defect hunt, set apart for the owner to rule on before any
fix pass. Each falls in one of three classes:

- **Peer hardening.** Hardening against malformed input that only shepr's own
  same-build peer could send.
- **Latent.** The hunter could not reach the path: "safe today", "no sequence
  found", "cannot happen under the lease", or reachable only after a future
  change.
- **Failure on failure.** The trigger is itself an earlier failure: a thread
  that cannot spawn, a part-way tty or socket write, a loop that has already
  stalled, a panic in a helper thread, a lost terminal event.

An entry confirmed as rejected owes its rule-3 comment at the code site. An entry
the owner keeps moves to `notes/todo.md` under "Open defects".

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## REJ-001 - Client-to-server collections are not bounded at decode by their stated caps

Class: peer hardening. Scope: wire and config.

Files: `crates/shepr-protocol/src/input.rs` (`ClientMessage::ClientShellPaneInput
{ events: Vec<ClientPaneInputEvent> }`), `crates/shepr-protocol/src/command.rs`
(`LayoutSetSplitRatioParams.path: Vec<SplitBranch>`,
`PaneCopySearchParams.query`), `crates/shepr-protocol/src/limits.rs`.

Claims: `MAX_COLLECTION_ITEMS`: "Fields with tighter protocol caps apply those
through `serialize_bounded_vec` / `deserialize_bounded_vec` as well."
`MAX_INPUT_EVENT_BATCH`: "the server refuses a message past it".
`MAX_SURFACE_SPLIT_PATH` bounds "traversal work even for a malformed split path".

`events` and `path` use plain `Vec` serde, so the only decode-time bound is the
general 4 Mi item cap plus the 2 MiB frame. A same-user client can make the server
decode several hundred thousand `ClientPaneInputEvent`s before the batch check
refuses; an over-long split path is decoded and compared against every split. Fix:
apply the bounded-vec adapters with `MAX_INPUT_EVENT_BATCH` and
`MAX_SURFACE_SPLIT_PATH`. The hunter notes the impact is small because the peer is
the same user and passes the build preamble.

## REJ-003 - `server.stop` accepts a boot guard placed outside `params` and ignores it

Class: peer hardening. Scope: wire and config.

`crates/shepr-api/src/schema.rs` (`Request` with `#[serde(flatten)] method`, no
`deny_unknown_fields`), `schema/server.rs`. `server.stop_if_boot` has its own
method name so a guard is never ignored, and `ServerStopParams` has
`deny_unknown_fields` for the same reason. But
`{"id":"x","method":"server.stop","params":{},"expected_boot_id":"..."}` decodes
as an unconditional stop; the flattened `Request` drops the stray key. Shepr's own
client never sends this. Closing it needs a hand-written `Deserialize` or an
explicit `method`/`params` pair, since `deny_unknown_fields` does not combine
with `flatten`.

## REJ-004 - A dirty-patch collection marks cells past the collected width clean

Class: latent. Scope: pane runtime.

Claim: `terminal_collect_dirty_patch`: "Only clear it after every row has been
collected successfully ... Rows below the area were not collected: they stay
dirty, and so does the overall state, so it only reads Clean when no row is left
to send." Rows with `y >= area_height` keep their dirty flag, but a row with
`y < area_height` gets `row.clear_dirty()` even when only `area_width` of its
cells went into the patch, so changes in columns `area_width..cols` drop from
every later patch. The retained path collects at the largest `inner_rect` among
its recipients, narrower than the terminal when the view that set the PTY size is
not a recipient (deferred and skipped by `retained_surface`'s `continue`) or while
layout and PTY size disagree. The hunter found no sequence reaching a wider patch
without an intervening full render. Either keep a row dirty when
`area_width < cols` and its dirty cells extend past it, or document why the column
case cannot matter.

## REJ-005 - `DetectorState::reset` can re-report or delay a process exit around an authority change

Class: latent (moderate confidence; the hunter could not rule the window in or
out). Scope: pane runtime.

Claim: `DetectorState::reset`, "Lifecycle authority resets screen evidence, not
the process identity that ties a later confirmed exit back to that hook
generation." `reset` keeps `current_agent()` (including
`pending_confirmed_process_exit`) as the present agent and clears
`pending_confirmed_process_exit`, `pending_foreground_shell_clear` and
`foreground_shell_exit_reported`. If an exit was already published and authority
goes inactive to active before the `ClearAgent` probe, the next shell-foreground
probe publishes a second `StateChanged { process_exited: true }`. If the exit had
just passed miss confirmation, `reset` brings the agent back present with zero
misses and the exit is reported only after another
`AGENT_MISS_CONFIRMATION_ATTEMPTS` probes. Whether the server can turn authority
on in that window depends on the hook report path (`terminal/state/hooks.rs`
suppresses reports after an exit). A test driving `reset()` between the exit
publish and the clearing probe would settle it.

## REJ-006 - "Created exclusively" recovery copies are not exclusive

Class: latent (cannot happen under the data-directory lease). Scope: mux and
persistence.

`RECOVERY_SEQUENCE_LIMIT` says "The copy is created exclusively, so a concurrent
writer that picked the same timestamp moves on to the next one."
`publish_private_file` first unlinks whatever sits at the pending name
(`remove_stale_temporary`), and the `replace == false` refusal is a
`symlink_metadata` check followed later by a plain `rename(2)`, which overwrites.
With two writers one could publish the other's partial temporary under the
recovery name. Under the lease there is one writer, so the sequence loop and the
doc defend against nothing, and the `AlreadyExists` branches in
`preserve_existing_in` are effectively dead. Either make the no-replace publish
exclusive (`renameat2(RENAME_NOREPLACE)`, or `link(2)` then unlink) or drop the
sequence loop and say the lease makes names unique.

## REJ-007 - A split's public number and layout are re-derived at commit, not carried from prepare

Class: latent (safe today because the one caller runs both halves in one
synchronous handler). Scope: mux and persistence.

`workspace.rs` `split_pane` / `commit_new_pane`, `workspace/pane_tree.rs`
`commit_prepared_split`. `split_pane` builds the child's `SHEPR` pane identity
from `self.next_public_pane_number` at prepare time; `commit_new_pane` reads it
again and registers the pane under whatever it holds then. `commit_prepared_split`
checks only that the prepared layout's pane-ID set is the current set plus the new
pane, so a layout prepared before an intervening resize, swap, ratio or focus
change would pass and silently revert it. Carry the reserved number and a layout
generation in `NewPane` and refuse a stale commit, or collapse both phases into
one `&mut self` call (the spawn already happens synchronously inside
`split_pane_shell`).

## REJ-009 - `toggle_pane_zoom` commits a focus change it then reports as failed

Class: latent (only if `set_zoomed` refuses an inconsistent pane tree). Scope:
server app.

`toggle_pane_zoom` commits the focus change (and marks the session dirty) before
it can return `None`; `handle_pane_zoom` then answers "pane not found" with no
effects, so the committed focus change is not rendered.

## REJ-010 - A closed `api_rx` would spin the server loop

Class: latent (never closes in production). Scope: server loop and transport.

`HeadlessServer::run`'s select treats a closed `api_rx` as `Timer`. A closed
channel resolves at once on every poll, so the loop would spin. It never closes
in production only because `run_server` keeps its original `api_tx` alive in a
local for the whole `block_on`; nothing documents that the local is load-bearing.

## REJ-013 - An oversized OSC title cut by the adapter still reaches the parser

Class: latent (unconfirmed; the terminal-core hunter left it for the mux side to
confirm). Scope: terminal core.

The adapter feeds CAN, which makes vte's `osc_end` dispatch the truncated OSC.
For OSC 52 this is covered (the truncated base64 fails to decode or decodes over
the cap). An OSC 0/2 title gets dispatched at up to `MAX_PARSER_OSC_BYTES` (about
512 KiB). Whether the title path caps that was not checked.

## REJ-016 - Read failures are logged as "mutation was not applied" and use up the one-shot report

Class: failure on failure (a poisoned terminal core, i.e. after a panic). Scope:
pane runtime.

`PaneTerminal::agent_detection_inputs` and the read half of
`maybe_restore_host_terminal_theme` call `report_terminal_mutation_failure`,
which logs "terminal core lock poisoned; mutation was not applied" and sets the
once-per-pane latch. When detection hits the poisoned core first (likely, since
it polls), the log line is wrong and a later real mutation failure is never
logged. The field doc on `PaneTerminal::core` says "operations without a failure
return log their skipped operation once per pane". Give reads their own latch, or
do not log them (the actor already reports the poisoned core).

## REJ-018 - The inline persister does not keep the module's panic contract

Class: failure on failure (the persister thread could not be spawned, or a job
panicked). Scope: mux and persistence.

`persist/actor.rs` `Worker::Inline`, `SessionPersister::submit`. The module doc:
"If a worker job panics, it fails closed: it reports that job and later
submissions as abandoned, keeps the writer (and lease) alive, and releases the
lease only after the persister is retired." Only the thread worker implements it.
`Worker::Inline` calls `done.complete(state.run(job, now))` with no
`catch_unwind` and no `accepting_jobs` latch, so a panic unwinds into the event
loop, and if caught above, later jobs run against a possibly half-updated
`PersistState`. Inline is also the fallback when the persister thread cannot be
spawned for a production app (`spawn_error` -> `Worker::Inline`), and then every
save does the expensive work (`/proc` cwd reads, scrollback formatting,
serializing, fsync) on the event loop, against the module doc's opening
paragraph. Structural note from the hunter: a `Suspended` app constructs a full
`SessionWriter` (with `protect_unloaded` and a `HistoryCarry`) only to hold a
lease, and nothing in the type stops it writing; an owner that persists nothing
should hold the `DataDirLease` alone, leaving one worker shape with the panic
latch in one place.

## REJ-019 - `resolve_write_target` reads every stat error as "not a symlink"

Class: failure on failure (a stat error other than `NotFound`). Scope: mux and
persistence.

`persist/io.rs`: `Err(_) => return Ok(current)` treats `EACCES`, `EIO` and the
like as `NotFound`, so a link the writer cannot inspect is written over as a
plain path; writes later fail on the same error in most cases. It contradicts
`missing_directory_chain` ("A stat error other than `NotFound` is returned: read
as absence it would ... [be] wrong"). `clear_path` uses the same resolver, so a
clear can target the link's own path instead of its target.

## REJ-020 - History and layout are paired by layout shape, not by save

Class: failure on failure (a save commits the layout and then fails to write the
history). Scope: mux and persistence.

`persist/snapshot.rs` `layout_fingerprint`, `persist/restore.rs` history filter.
The history file is accepted when its fingerprint equals the layout's; the
fingerprint covers the tree and pane IDs, which restore reassigns
deterministically, so a layout left unchanged has the same fingerprint every
boot. If one save commits the layout and then fails to write the history (a
failure the writer tolerates), the history on disk can be from an earlier boot,
and restore replays scrollback older than its layout. The writer calls history
"the history that pairs with it". Low severity: the content belongs to the same
panes. Stamp a save generation into both files and pair on it, or publish layout
and history as one directory by a single rename.

## REJ-024 - A crash-looping Local server is retried without backoff

Class: failure on failure (a server that accepts and then dies). Scope: client
endpoints.

`EndpointSupervisors::record_status` resets Local's attempts on every `Online`,
while SSH waits for `STABLE_CONNECTION_PERIOD` ("A brief maintenance wake can
complete a handshake without restoring the link"). A Local server that accepts
and then dies (a crash loop, or a stopping server that welcomes then sends
`ServerShutdown`) is retried every `INITIAL_RETRY_DELAY` forever.

## REJ-027 - A stop that was never delivered is reported as a timeout with "socket still reachable"

Class: failure on failure (the server closed without answering). Scope: wire and
config.

`send_stop_request` treats `EmptyResponse` (connection closed with no answer) as
an accepted stop. The server also closes without answering when the connection
thread fails to spawn, when the peer-credential check refuses, or when the
request line is oversized. A stop never delivered then waits out the full 15 s and
reports `TimedOut` with "the socket at <path> is still reachable", which misnames the failure.

## REJ-029 - A keyboard-protocol write that fails part way leaves the push/pop bookkeeping wrong

Class: failure on failure (a host tty write fails part way). Scope: input and
platform.

`crates/shepr-termio/src/host_term/modes.rs`, `set_host_keyboard_protocol`
writes the pop (`CSI < 1 u`), the push (`CSI > flags u`) and the modifyOtherKeys
change, flushes, and only then updates `*active`. If the flush or a later write
fails after the pop reached the terminal, `active` still records a kitty entry
that no longer exists, and the next change or `restore_host_keyboard_protocol`
pops an entry shepr does not own (the shell's or an outer multiplexer's). Claim
broken: "later changes replace only the entry recorded in `active`". Record the
pop in `active` as soon as it is issued, or clear `kitty_flags` on any error after
a pop was queued.

## REJ-030 - A remote server is judged restartable on a boot id the stop command rejects

Class: failure on failure (a server that reports a non-canonical boot id). Scope:
remote and launch.

`judge_remote_server` accepts any non-empty printable token as the boot id
(`printable_remote_token`) and returns `DifferentBuild`. The stop then runs the
remote `shepr server stop --expect-boot <id>`, whose clap parser
(`spec::boot_id`) requires a canonical `shepr_protocol::BootId`, so any other form
exits 2, which `stop_remote_server` reports as `RestartResult::Failed` after the
operator consented. `judge_remote_server`'s doc says a server without a usable
boot identity "cannot be stopped as a specific instance, so it is an error the
operator has to act on"; it should parse with the stop's `BootId` rule so such a
server is classified up front.

## REJ-031 - Lost-release drags are only half recovered

Class: failure on failure (the terminal lost a mouse release). Scope: client
shell.

`MouseEventKind::Down(MouseButton::Left)` in `handle_mouse_with_accounting`. A
`SidebarWidth` drag whose release never arrived gets its owed resize on the next
press ("still owes the endpoint its resize") but not its
`persist_chrome_preferences`. A lost `SidebarSection` release is not persisted
either, and a lost `PaneSplit` release never sends its final throttled ratio.
`ClientChromePreferences` claims to remember manual chrome changes across
launches.
