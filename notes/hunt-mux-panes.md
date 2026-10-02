# Design hunt: mux-panes

Scope: `crates/shepr-mux/src/pane.rs`, `crates/shepr-mux/src/pane/` and
`crates/shepr-mux/src/terminal/`, read in full apart from test modules. Callers
in `shepr-server`, `persist/`, `workspace/` and `events.rs` were followed where
a decision crosses into them. File and function names are cited; line numbers
are not.

The short version:

- The pane runtime is sound underneath (arbiter, launch coordinator, deferred
  effect ordering, history cache are all careful) but its edges leak: runtime
  events can be sent untagged, a fixture-only "no child" case is carried in
  production types as pid `0` and `Option<ProcessHandle>`, and "is this still
  our child" is re-checked by hand at a dozen sites.
- `terminal/state` (hook and detector arbitration) receives a typed
  `AgentSource` and immediately collapses it to a `String`, then re-parses the
  `(source, label)` string pair at every decision. That is the biggest
  types-that-resolve-to-primitives finding in the scope, and it is also why
  shepr-agent grew a family of `fn(source: &str, label: &str) -> bool`
  predicates.
- Several rules exist twice with one copy test-only, and the tests exercise
  the copy, not the production rule (`InputState::plain_page_keys_use_host_scrollback`,
  `terminal_normalize_buffer_symbol`).
- Revision bookkeeping (`content_revision += 2`, detection sequence, sync
  epoch, history epoch) is decided at each mutation site, and the parity bit
  the `+2` preserves is interpreted in the server while a mux comment says
  parity is no longer used.

---

## 1. Axes that should be types

### 1.1 Runtime events are an `AppEvent` wrapping an `AppEvent`

`events.rs` defines `AppEvent::Runtime { pane_id, generation, event: Box<AppEvent> }`.
The runtime's `EventSender::runtime(...)` tags events with it, and
`App::admit_runtime_event` (`shepr-server/src/app/events.rs`) unwraps it and
checks the generation against the registered runtime.

What the type allows that should be impossible:

- An untagged runtime event. `EventSender` also has `From<mpsc::Sender<AppEvent>>`
  with `origin: None`, and `publish_state_changed_event` /
  `publish_agent_process_detected_event` (`pane/process_probe.rs`) take
  `impl Into<EventSender>`, so a plain sender compiles. `admit_runtime_event`
  passes any event that is not `Runtime` straight through (`event => Some(event)`),
  so an untagged `PaneDied` or `StateChanged` skips the generation check that
  is the whole point of the envelope. Today every production producer happens
  to use the tagged sender; nothing enforces it.
- A nested envelope (`Runtime { event: Runtime { .. } }`). `admit_runtime_event`
  recurses to cope.
- A non-runtime payload in the envelope (`GitStatusRefreshed`, `HookStateReported`).
- A `pane_id` on the envelope that differs from the `pane_id` inside the
  payload. Every runtime payload variant repeats `pane_id`.

Proposed shape: a `RuntimeEvent` enum holding only what runtimes emit
(`LaunchSettled(LaunchSettlement)`, `Died { reason, ended_at }`,
`AgentProcessDetected { agent, observed_at }`, `DetectorState(StateChangedUpdate)`,
`ClipboardWrite(Vec<u8>)`, `CwdReported(UsableCwd)`), with no `pane_id`, and
`AppEvent::Runtime(RuntimeEnvelope { pane_id, generation, event: RuntimeEvent })`.
The only way to send one is a `RuntimeEventSender` that owns the
`(pane_id, generation)` pair; the publish helpers take that type, not
`impl Into<EventSender>`. Hook reports and Git results stay plain `AppEvent`
variants. Admission then has exactly one shape to check and cannot be bypassed.

### 1.2 Process ids and process-group ids are bare `u32`

Throughout `pane/`: `ChildLiveness::pid() -> u32`, `live_pid() -> Option<u32>`,
`ForegroundShellProbe`, `ProcessProbeResult::process_group_id: Option<u32>`,
`ProcessProbeScheduler::last_foreground_group: Option<u32>`,
`PaneTerminalCore::transient_default_color_owner_pgid: Option<u32>`,
`osc::should_restore_host_terminal_theme(owner_pgid: u32, shell_pid: u32, ..)`
(two adjacent `u32`s; the tests pass `42, 7` positionally),
`runtime::follow_cwd_from_processes(shell_pid: Option<u32>, foreground_pgid: Option<u32>, ..)`.

`follow_cwd_from_processes` decides "the shell is in the foreground" by
`shell_pid != foreground_pgid`, comparing a pid with a pgid. It is right only
because the shell is a session and group leader (`setsid` in the fork). A
`Pid` / `Pgid` pair (owned by `shepr-platform`, used by
`shepr_agent::detect::ForegroundJob` too) with an explicit
`Pgid::led_by(Pid)` would make that assumption visible and stop a pgid being
passed where a pid is expected. `0` as "no process" (see 1.3) also goes away.

### 1.3 `ChildLiveness`: a lifecycle spread across three atomics and an `Option`

`pane/teardown.rs`:

```rust
pub(super) struct ChildLiveness {
    pid: AtomicU32,
    wait_completed: AtomicBool,
    launched: AtomicBool,
    leader: Option<shepr_platform::ProcessHandle>,
}
```

- `pid` is atomic only so `set_pid_for_test` can change it; production never
  writes it after construction.
- `pid == 0` means "no child". It exists for `PaneRuntime::with_child_io`
  (`ChildLiveness::new(0, None)`), the cross-crate fixture seam. Because of it
  production code tests `pid != 0` in `live_pid` and `session_id == 0` in
  `shutdown_pane_processes`.
- `leader: Option<ProcessHandle>` is `None` only for fixtures and the in-crate
  tests (`ChildLiveness::new(pid, None)`). Production always has one:
  `PtySetup::start` refuses a launch without a handle. Yet `has_exited`,
  `is_reaped` and `launch_status::settle` all carry the `None` fallback
  (`wait_completed` stands in for the handle).
- `launched` + `wait_completed` + "handle says unreaped" encode a phase
  (Launching, Running, Exited-unreaped, Reaped) as independent booleans that
  can be combined in ways the comments rule out.

And `launch_status::LaunchProgress { launched: Option<bool> }` is a second copy
of "did exec commit": the coordinator sets `ChildLiveness::mark_launched()`
and then sends `launched = Some(true)` on the watch. Two stores of one fact,
written back to back.

Proposed shape: the runtime holds `Option<Arc<ChildLiveness>>` (or an enum
`PaneChild::{Detached, Process(Arc<ChildLiveness>)}`) so the fixture case is
modelled where it lives. `ChildLiveness { pid: Pid, leader: ProcessHandle, phase: AtomicU8 }`
with a `ChildPhase` enum, and the launch watch carries `ChildPhase` (or
`LaunchOutcome::{Pending, Launched, NotLaunched}`) read from the same source.

### 1.4 `ProcessBytesResult` mixes a failure with a success payload

`pane/terminal.rs`:

- `core_poisoned: bool` next to a full set of default-valued effect fields.
  Every producer of a poisoned result builds `ProcessBytesResult { core_poisoned: true, ..default() }`
  and every consumer must check the flag first (`PaneReadEffects::read`,
  `flush_expired_synchronized_output`). This is `Result<CoreEffects, CorePoisoned>`.
- `default_color_owner_pending: bool` plus `default_color_generation: u64`:
  the generation is only meaningful when the flag is set, and
  `apply_immediate` converts them back with `.then_some(..)`. One
  `default_color_owner: Option<DefaultColorGeneration>` field.
- `request_render: bool` plus `render_delay: Option<Duration>`: the delay
  exists exactly when `request_render` is false because a synchronized update
  is open. A `RenderRequest::{Now, After(Duration), None}` says it.

`shepr_pty::actor::PtyReadResult` has the same `core_broken: bool` shape one
layer down.

### 1.5 `TerminalDirtyPatchSnapshot` and the patch outcome

`collect_dirty_patch_snapshot` returns `Option<TerminalDirtyPatchSnapshot>`
and folds three different reasons into `None`: a poisoned core, an open
synchronized update, and a fallback (which carries a reason string that is
only logged once per pane). Its `patch: TerminalDirtyPatchOutcome` can then
only be `Clean` or `Patch`, never `Fallback`, but the type allows `Fallback`,
and `retained_surface.rs` has a dead arm for it. `scroll_metrics:
Option<ScrollMetrics>` is always `Some`. `TerminalDirtyPatch.rows` is
`Vec<(u16, Vec<CellData>)>`. The fallback reason is `Option<&'static str>`
built by a `fallback!` macro taking a literal.

Proposed: `Result<DirtyPatchSnapshot, PatchUnavailable>` with
`PatchUnavailable::{CorePoisoned, SynchronizedOutput, Fallback(PatchFallback)}`
and `PatchFallback` an enum (`HyperlinkPresent`, ...). The snapshot's patch is
`Vec<PatchRow { y, cells }>` (empty means clean) and `scroll_metrics` is not
optional. The server's own fallback labels (`source_fallback!("terminal_snapshot")`)
could then carry the real reason instead of a generic one.

`TerminalDirtyPatchSnapshot` is also declared `pub` inside the private
`runtime` module and not re-exported from `pane.rs`, so it is returned by a
public method but cannot be named outside the crate.

### 1.6 Detection publication: three visibility booleans that are one fact

`pane/agent_detection.rs`, `pane/process_probe.rs`, `shepr_agent::detect::AgentDetection`:

- `AgentDetection { state, skip_state_update, visible_idle, visible_blocker, visible_working }`.
  `skip_state_update: bool` is "no detection"; `detection_update_for_publish_with_osc`
  turns it into `Option` immediately (`(!detection.skip_state_update).then_some(..)`).
- `decide_screen_detection_publish` normalises
  `visible_idle && state == Idle`, `visible_blocker && state == Blocked`,
  `visible_working && state == Working`. After that, at most one can be true
  and it always matches `state`. `DetectionPublishState`,
  `DetectionPublishDecision::Publish`, `AgentDetectionPublishUpdate` and
  `DetectorState::last_visible_*` all carry the three as separate bools.

Proposed: `Detection { state: AgentState, visible: bool }` (the visible flag
applies to the state itself), and `AgentDetection` returning
`Option<Detection>` from shepr-agent. The normalisation happens once, at the
type boundary, instead of being re-derived by every reader.

`DetectorState` also encodes "no published baseline yet" twice:
`state: AgentState::Idle` as a sentinel (the comment in `DetectorState::new`
says so) and `has_detection_baseline: false`. `Option<Detection>` for the last
published value removes both.

### 1.7 The detector's agent-exit lifecycle is three loose fields

`DetectorState` has `pending_foreground_shell_clear: bool`,
`foreground_shell_exit_reported: bool` and
`pending_confirmed_process_exit: Option<Agent>`. Together with
`AgentDetectionPresence { current_agent, consecutive_misses }` they encode
"agent present / exit confirmed, report owed / exit reported, clear owed /
cleared". `observe_process_probe` sets them in five different combinations
per `ForegroundShellAgentAction`, and `process_exited(agent)` reads two of
them. An `AgentExitPhase` enum with explicit transitions would make the
illegal combinations unrepresentable and give the five-way match a target.

### 1.8 The detector tick is a coroutine flattened into fields

`DetectorState::tick` takes `TickObservation::{Begin, Probe, Screen}` and
keeps per-tick scratch in `tick_schedule`, `tick_agent_changed` and
`tick_group_changed` between calls. `TickOutput { probe: bool, screen: bool, .. }`
asks the caller for more input through two booleans. The doc comment explains
that every step re-runs the screen path and relies on its gates being
idempotent so the resume lands in the right place.

A typestate would carry the scratch instead of the detector:
`detector.begin(obs) -> Tick` where `Tick::{Done(TickOutput), NeedsProbe(ProbeTick), NeedsScreen(ScreenTick)}`
and `ProbeTick::resume(probe_result)`, `ScreenTick::resume(inputs)`. The
scratch cannot leak into the next tick, and the "gates must be idempotent"
invariant stops being load-bearing.

Related parameter-object sprawl in the same file: `ProcessProbeRequest`,
`ProcessProbeScheduleInput`, `ProcessProbeCompletion`, `ScreenScanGate`,
`ScreenReadRequest`, `ScreenPublishContext`, `DetectionScreenReadInput`,
`ScreenDetectionPublishInput` each re-bundle some of `now`, `agent`,
`agent_changed`, `process_exited` and `content_seq`. One per-tick context
value would replace most of them.

### 1.9 Launch failures are prose by the time anything can branch on them

`pane/launch_status.rs` and `terminal/state/mod.rs`:

- `LaunchStatus.program: String` is read from the command's `SHELL` variable,
  with `String::new()` when it is absent, and used only to format an error.
  `PtyCommand` already knows its program; there is no accessor, so the launch
  takes a second answer from the environment.
- `LaunchRecord::ExecFailed(errno)` becomes
  `RestoreFailure::ShellStartFailed { error: format!("{program}: {error}") }`.
- `LaunchRecord::ChdirFailed(errno)` takes `cwd_candidates.first().cloned().unwrap_or_default()`
  as the path (an empty `PathBuf` if there were none) even though, for a
  non-required cwd, the child tried every candidate before failing. The
  record does not say which candidate failed.
- `RestoreFailure` stores `error: String` (its doc says so deliberately). The
  API's `detect` path and the pane placeholder both read it as text.
- The name is wrong for most uses: a fresh split's failed exec is a
  `RestoreFailure` too.

Proposed: `PaneStartFailure { stage: StartStage, cause: Errno }` with
`StartStage::{EnterDirectory { path }, ExecShell { program }, ResumeUnavailable(ResumeUnavailable)}`
and an `Errno` newtype (or `io::ErrorKind` plus raw code) so the placeholder,
the API and logs each render their own prose. `LaunchRecord::ChdirFailed`
should carry the candidate index the way `ChdirOk` does.

### 1.10 Hook report outcomes: `None` means a dozen different things

`terminal/state/source/report.rs` (`transition_report`) and
`source/start.rs` (`transition_start`) return `Option<TerminalStateMutation>`.
`None` is returned for: built-in source naming another agent, a
session-identity-only integration, an invalid session ref, a report for a
replaced session, a report after a confirmed process exit, a label that
conflicts with the detected agent, an owner conflict without a foreground
takeover, a stale or cross-talk report (via `FullLifecycleHookReportRoute::Ignore`),
an out-of-order sequence, and a full source table. `Some(default)` means
"parked". `Some(mutation)` means applied.

The server (`update_terminal_state`) collapses all of it into
`StateUpdate::Unchanged`, the reporter gets nothing back, and `detect explain`
cannot say why a hook was ignored. A `HookOutcome::{Applied(TerminalStateMutation), Parked, Rejected(HookRejection)}`
with a closed `HookRejection` enum would let the server log once per reason,
let `detect explain` show the last rejection, and turn the tests' implicit
"returns None" assertions into named ones.

### 1.11 Mutation results are recovered by diffing revisions

`PaneRuntime::clear_screen`, `scroll_up`, `scroll_down`, `scroll_reset`,
`set_scroll_offset_from_bottom` return nothing (or `Result<(), _>`). Callers
work out whether the surface changed by reading `content_seq()` before and
after (`shepr-server/src/app/api/panes/copy.rs`) or comparing
`scroll_metrics()` snapshots. Meanwhile every scroll path bumps
`content_revision` unconditionally, so a scroll that hit the top or bottom
still reports a change (see 5.4). A `SurfaceChange::{Changed, Unchanged}`
return from each mutation, decided where the mutation happens, removes the
diffing and the false positives.

### 1.12 Launch kind is three knobs

- `PaneLaunchEnv::purpose: LaunchPurpose::{Fresh, AgentResume}` (drives the
  detector's absence hold),
- `PaneShellConfig::require_cwd: bool` (restored panes and resumes),
- `PaneLaunchEnv::extra: Vec<(String, String)>`.

A restored pane is `Fresh` with `require_cwd`; a resume is `AgentResume` with
`require_cwd`; nothing stops `AgentResume` without `require_cwd`. The server
additionally decides "this launch was a resume" from
`pending_resume_commands` / `pending_agent_resume_plan` in
`pane_launch.rs` (see 2.12). A single `LaunchKind::{Fresh, Restored, AgentResume}`
from which `require_cwd`, the detector hold and the settlement handling all
follow would make the combinations explicit.

`extra` is `Vec::new()` at every production construction (`workspace.rs`
twice, `persist/restore.rs`, `shepr-server/src/app/ids.rs`). The
"explicit launch env opts back into scrubbed variables" machinery and its
test exist only for an input nothing provides. Remove it, or type it as
`Vec<(EnvVar, OsString)>` if it is meant to come back.

`PaneShellConfig { default_shell: &str, login_shell: bool }`: config resolves
the shell to an absolute path at launch, `PtyCommand::interactive_shell`
trims it again and `launch_spec` rejects a relative path again. A
`ResolvedShell(PathBuf)` produced by config validation and accepted by the
PTY layer makes the re-checks unnecessary.

### 1.13 Small primitive axes in the copy-mode surface

- `paragraph_motion_target(row, direction: i8)`. The only caller
  (`copy.rs`) has a typed `PaneParagraphMotion::{Previous, Next}` and maps it
  to `-1` / `1`; `paragraph_motion_in` treats `0` as "no motion". Take the
  enum.
- `search_text_window(query, case_sensitive: bool, direction, cursor, previous: Option<(TerminalTextPoint, TerminalTextPoint)>, limit)`:
  `previous` is an unnamed `(start, end)` pair; `TextSearch::new` picks `.0`
  or `.1` by direction. A `TextRange { start, end }` (or the
  `TerminalTextMatch` itself) avoids a swap.
- `TerminalSearchWindow { current: Option<usize>, current_global: Option<usize>, .. }`:
  both are `None` exactly when the window is empty. One
  `Option<SearchCursor { local, global }>`.
- `PaneRuntime::cursor_state(area, show_cursor: bool)` returns `None` when the
  bool is false; the caller can skip the call.

### 1.14 History cache edges

- `PaneHistoryCache::revision: u64` with `0` meaning "never held anything".
- `PaneHistorySource(pub(crate) Arc<PaneTerminal>)`: the runtime builds it by
  reaching into the tuple field.
- `PaneHistorySource::refresh -> bool` and `read_primary_history_inner -> Option<()>`
  fold "alternate screen active" and "core unreadable" together. The save
  path treats both as "keep the previous cache", so this is fine today, but
  `Option<()>` as a result type is a smell; a small
  `HistoryUnavailable::{AlternateScreen, CorePoisoned}` costs nothing.

---

## 2. Decisions made in more than one place

### 2.1 "Is this observation still about our live child?"

`ChildLiveness::live_pid()` is the declared single gate, but the
"sample pid, do the /proc read, check the pid is still live" protocol around
it is re-implemented by hand at each call:

- `PaneRuntime::cwd`, `PaneRuntime::follow_cwd`, `PaneRuntime::foreground_cwd`
  (twice), `PaneCwdProbe::read`, `runtime::publish_reported_cwd`;
- `PaneTerminal::maybe_restore_host_terminal_theme`,
  `PaneTerminal::resolve_default_color_owner` (`terminal/backend.rs`);
- `DetectionTask::live`, called eight times in `DetectionTask::tick`.

On top of that, two later gates answer a neighbouring question for detector
events:

- the server drops `StateChanged` / `AgentProcessDetected` when
  `PaneRuntime::child_has_exited()` is true (`handle_internal_event_inner`);
- `TerminalState::transition_detector_observation` drops them once
  `pane_ended` is set by `transition_pane_exit`.

These use different facts: `has_exited` counts a zombie, `pane_ended` is the
applied exit, and the comment on `pane_ended` explains the case where the
child outlives a failed reader, which the first gate misses. They agree on
today's paths because of event ordering, not because they share an owner.

Owner: `ChildLiveness::observe(|pid| ...) -> Option<T>` doing the
sample-act-recheck once, so no caller writes the protocol. For detector
events, the runtime is the natural single owner: once the pane's exit arbiter
has decided, the detection task stops and the launch coordinator drops any
later detector output, or the envelope from 1.1 carries "observed before the
ending" and admission checks that alone.

### 2.2 "Is the pane shell in the foreground?"

Answered five ways:

- `runtime::follow_cwd_from_processes`: `shell_pid != foreground_pgid`;
- `osc::foreground_job_is_shell`: the job's process list contains the shell pid;
- `process_probe::process_probe_result` and
  `probe_foreground_process_from_jobs`: the same membership test, inline,
  twice;
- `osc::current_transient_default_color_owner`: via `foreground_job_is_shell`;
- `process_probe::foreground_member_cwd_different_from_shell`: skips the
  shell's pid while scanning members.

The pid/pgid comparison and the membership test are equivalent only because
the shell is its own group leader. Owner: one foreground probe in
shepr-agent's detect layer returning `Foreground::{Shell, Job { pgid, processes }, Unknown}`,
used by cwd following, theme restore and agent probing alike.

### 2.3 "What is a process's cwd?"

Four readings of `/proc/<pid>/cwd` with different validation:
`absolute_process_cwd` (absolute only), `readlink_process_cwd` (also drops a
` (deleted)` target), `usable_process_cwd` (stat through `UsableCwd`), and the
raw `shepr_agent::detect::process_cwd` used for
`ReportedCwd::shell_cwd_at_report` in `publish_reported_cwd`.

They already disagree: `readlink_process_cwd`'s doc says a ` (deleted)` path
"is not a cwd anyone can use", but `PaneRuntime::foreground_cwd` and
`foreground_member_cwd_different_from_shell` use `absolute_process_cwd` and
can return one. And `ReportedCwd::resolve` compares a raw sample against a
filtered one; equal for ordinary paths, not equal by construction.

Owner: one `ProcessCwd` reader returning a typed value
(`Live(PathBuf) | Deleted(PathBuf) | Unavailable`) with the stat-level check
as a separate, explicit step for off-loop callers.

### 2.4 "What is this pane's cwd?"

The runtime arbitrates OSC 7 against `/proc` (`ReportedCwd`, `PersistedCwd`,
`remembered_cwd_for_save`), and offers four answers: `cwd`, `follow_cwd`,
`foreground_cwd`, `remembered_cwd` (plus `PaneCwdProbe::read`). Then each
caller builds its own fallback chain onto `TerminalState::cwd()`:

- `Workspace::cwd_for_pane` (`workspace/pane_tree.rs`): `runtime.cwd()` then
  `terminal.cwd()`;
- `creation::launch_cwd_for_terminal` (server): `runtime.follow_cwd()` then
  `terminal.cwd()`;
- `persist/snapshot.rs`: `runtime.remembered_cwd()` then `terminal.cwd()` then
  a fallback;
- `agent_resume.rs`: `terminal.cwd()` alone.

There are also two copies of "the last OSC 7 report": `PaneCwdState::reported`
in the runtime and `TerminalState::cwd`, which is written from the
`TerminalCwdReported` event and from `LaunchSettlement::Launched { cwd }`.
They are kept in step by the event; nothing checks it.

Owner: a `PaneCwd` query in mux taking the runtime and the terminal state
together with an explicit purpose (`Identity`, `FollowForNewPane`, `Save`,
`Resume`), so the fallback order per purpose lives in one function.

### 2.5 Hook-source classification from strings

The question "what kind of hook owner is `(source, agent_label)`" (official,
custom, full-lifecycle, session-identity-only, reserves native state) is
answered by re-parsing strings at every use:

- `TerminalState::effective_agent` calls `Agent::parse_canonical_label` and
  `shepr_agent::detect::full_lifecycle_hook_authority(&source, &label)` on
  every call, and `effective_agent` runs for `is_agent_terminal`,
  `border_label`, `effective_agent_label`, `effective_known_agent`,
  `full_lifecycle_hook_authority_active` and every recompute;
- `source.rs`: `hook_authority_conflicts_with_detected_agent`,
  `suppress_current_full_lifecycle_hook_authority`,
  `route_full_lifecycle_hook_report`,
  `same_owner_full_lifecycle_hook_authority_session_ref`,
  `current_session_owner_conflicts`, `conflicting_same_owner_session_ref`,
  `session_report_allows_session_replacement`,
  `is_unsequenced_opencode_selection`,
  `foreground_agent_confirms_*`, `persisted_agent_session_matches`,
  `known_agent_label_conflicts_with_detected_agent`;
- `source/detection.rs`: `is_official_agent_source(&source, &label)` and
  `parse_canonical_label` three more times;
- `source/report.rs` checks session-identity-only through
  `typed_source.agent().descriptor().session_identity_only_integration`,
  while `source/start.rs` checks the same thing through
  `shepr_agent::detect::session_identity_only_integration(&source, &label)`;
- the server routes reserved-native-state sources to the session path with
  `is_reserved_native_state_source(source.as_str(), &label)`
  (`shepr-server/src/app/actions/events.rs`) before `TerminalState` sees them.

The predicates in shepr-agent (`full_lifecycle_hook_authority`,
`session_identity_only_integration`, `is_official_agent_source`,
`is_reserved_native_state_source`) all do `AgentSource::from_pair(..).and_then(agent).is_some_and(descriptor flag)`.
They exist only because callers hold strings. Owner: resolve once at
ingestion into a typed `HookOwner { source: AgentSource, label: AgentLabel, agent: Option<Agent>, kind: HookOwnerKind }`
and store that in `HookAuthority`, `SuppressedFullLifecycleHookReport`,
`StaleFullLifecycleHookSession` and the `hook_sources` key (see 4.1). The
routing the server does would become a method on it.

### 2.6 Revision bookkeeping is decided at every mutation site

In `pane/terminal/backend.rs` the core's counters are bumped by hand:

- `content_revision.wrapping_add(2)` at about a dozen sites (process, tick,
  seed, resize, every scroll, clear, host theme, host appearance, theme
  restore);
- `detection_content_seq` through `observe_detection_content_change` (only
  for non-empty bytes) and `mark_detection_content_changed` (tick flush,
  resize, clear), free functions in `agent_detection.rs` operating on
  `&mut u64`;
- `synchronized_output_epoch` at four sites;
- `history_epoch` in `resize`.

They already disagree once: an expired synchronized update flushed by
`PaneTerminal::tick` bumps `detection_content_seq`, but the same flush
performed inside `process_pty_bytes_locked` (via `core.terminal.tick(now)`
before parsing) bumps only the sync epoch and relies on the read's bytes
being non-empty for the detection bump.

The `+2` keeps the revision even. `PaneTerminalCore::content_revision`'s doc
says the parity scheme is no longer used ("exclusion is now provided by the
core"), but `shepr-server/src/server/client_shell.rs` still marks a torn read
with `after | 1` and tests `after.is_multiple_of(2)`. So the encoding of a
revision is split across two crates through a raw `u64`, and the doc on one
side contradicts the code on the other.

Owner: a `CoreRevisions` value inside the core with
`record(Mutation::{Output { nonempty }, SyncFlush, Resize { grid_changed }, Viewport, Presentation, Clear})`
deciding all four counters, and a `ContentRevision` newtype whose "stable or
torn" reading is a method rather than a parity convention the server
re-implements.

### 2.7 "Is mouse reporting on, and in which protocol?"

- `shepr_vt::Terminal::mouse_tracking_enabled` (`TermMode::MOUSE_MODE` or the
  X10 flag) feeds `PaneTerminal::mouse_reporting_enabled`, `wheel_routing`,
  `plain_page_keys_use_host_scrollback` and the dirty-patch snapshot;
- `PaneTerminal::encode_mouse_event` derives the mode itself from a cascade of
  `DecMode` checks and returns `None` when none is set;
- the test-only `input_state` derives mode and encoding with a third cascade
  (ranking `MouseSgrPixels` first, which the encoder handles separately).

On the gating side, `PaneRuntime::encode_mouse_button` checks
`mouse_reporting_enabled()` first, `encode_mouse_motion` does not,
`encode_mouse_wheel` checks `wheel_routing()`, and the terminal's encoder
checks again. Owner: shepr-vt exposes one `mouse_protocol() -> Option<MouseProtocol { mode, encoding, sgr_pixels }>`
and the encoder and every predicate read it.

### 2.8 Rules with a test-only twin, where the tests exercise the twin

- `PaneTerminal::plain_page_keys_use_host_scrollback` (production,
  `terminal/backend.rs`) and `InputState::plain_page_keys_use_host_scrollback`
  (`#[cfg(test)]`, `pane/terminal.rs`). The test
  `plain_page_keys_host_scroll_for_shell_like_decckm_with_bracketed_paste`
  builds an `InputState` by hand and calls the twin. The production rule is
  not what that test checks.
- `terminal_buffer_symbol_into` (production render path, `helpers.rs`) and
  `terminal_normalize_buffer_symbol` (`#[cfg(test)]`, same file). The
  grapheme-width tests in `terminal/tests.rs` call the twin.

These are worse than pairwise-agreement tests: there is no agreement test,
and the copies can drift with every test still green. Delete the twins and
point the tests at the production functions (pull the width decision out of
`terminal_buffer_symbol_into` into a pure `fn normalized_symbol(&str, CellWide) -> &str`
that both the render path and the tests call).

### 2.9 Cell width from `CellWide`

`helpers.rs` maps `CellWide` to a width four times
(`terminal_blank_symbol_for_width`, `terminal_grid_width`, and the
`expected_width` match in both `terminal_buffer_symbol_into` and its test
twin), and `text.rs` `TextBufferBuilder::push_cell` does it a fifth time.
Owner: `shepr_vt::CellWide::columns()` and `CellWide::grid_width()`.

### 2.10 Pane environment policy for names in both vocabularies

`pane/launch.rs` decides a pane policy per `EnvVar` (`pane_env_policy`) and
per `ChildEnv` (`pane_child_env_policy`). `SHELL` and `PATH` are in both
vocabularies, so their policy is decided twice; the test
`a_name_in_both_vocabularies_has_one_pane_policy` holds the two in step.
That is a pairwise-agreement test guarding two copies. Owner: one registry in
`shepr-core` where each variable name appears once (a `ChildEnv` that is also
interpreted refers to its `EnvVar`, or the two enums merge with a "who reads
it" property), and the pane policy is a property of the entry.

### 2.11 Pane launch assembly

`PaneLaunchEnv::from_extra(Vec::new(), socket).with_pane_id(PublicPaneId::new(ws, n))`
is written at four sites: `Workspace::spawn`, `Workspace::launch_env_for_new_pane`,
`persist/restore.rs`, and `App::pane_launch_env` in the server. Every call of
`PaneRuntime::spawn` / `spawn_with_initial_history` threads the same settings
(`scrollback_limit_bytes`, host theme, host appearance, shell config) and the
same four handles, which `workspace.rs` already bundles as
`PaneSpawnHandles` but the runtime does not accept. Restore passes host
appearance `None` while every other site passes the current one; that may be
intended (no client yet), but it is decided implicitly by one call site.

Owner: a `PaneSpawner` (or `PaneLaunchContext`) built once by the app holding
the handles, socket and settings, with
`spawn(PaneLaunchRequest { pane_id, public_id, geometry, cwd, kind, initial_history })`.
The 12-argument constructors and their `#[expect(clippy::too_many_arguments)]`
go away, and the public id and launch kind become required inputs rather
than optional builder calls.

### 2.12 "Is this launch an agent resume?"

Held in four places: `TerminalState::pending_agent_resume_plan`,
`App::pending_resume_commands` (server), `PaneLaunchEnv::purpose`
(`LaunchPurpose::AgentResume`) and `PaneShellConfig::require_cwd`.
`handle_pane_launch_settled` (`shepr-server/src/app/pane_launch.rs`) decides
how to record a failure from `resume_command.is_some() || terminal.pending_agent_resume_plan.is_some()`,
not from the runtime's own purpose. Owner: the launch kind from 1.12 carried
by the runtime and returned with the settlement
(`LaunchSettlement` names the kind it settles), so the server branches on
what was launched rather than on side tables.

### 2.13 Full-lifecycle authority mirrored into the runtime by hand

`TerminalState::full_lifecycle_hook_authority_active()` is derived state. The
detection task needs it, so the server copies it into
`PaneRuntime::full_lifecycle_authority_active: Arc<AtomicBool>` through
`sync_pane_lifecycle_authority_detection_pause`, called after
`handle_state_event` for the touched pane and after
`publish_pane_process_exit`. Other `TerminalState` mutations
(`set_persisted_agent_session` during restore, `abandon_agent_resume`, a
freshly installed runtime for a terminal that already has authority) do not
call it, so the runtime's copy is correct only when the last mutation went
through one of the two synced paths. Owner: report the change in
`TerminalStateMutation` (`lifecycle_authority: Option<bool>`) and apply it at
the one choke point that applies mutations (`update_terminal_state`), and set
it on runtime installation from the terminal's current value.

### 2.14 Clearing OSC evidence on an agent change

`DetectorState::observe_process_probe` computes
`should_clear_osc_evidence = should_reset_detection && previous_agent.is_some()`,
and `clear_osc_evidence_for_agent_transition` checks `previous_agent.is_some()`
again before calling `PaneTerminal::clear_agent_osc_state`. Small, but it is
the same rule written twice; the helper should just clear.

### 2.15 "Draw nothing during a synchronized update"

`PaneTerminal::render_into` and `collect_dirty_patch_snapshot` return early
while mode 2026 is set; the server checks `synchronized_output_active()` or
`synchronized_output_state()` before calling them
(`ui/surface.rs`, `retained_surface.rs`, `client_shell.rs`). The server needs
the answer to defer, and `render_into` returning `()` cannot tell it whether
it drew. This is a deliberate double check today; a typed result from the
draw (`Drawn | Deferred(SynchronizedOutput) | Unreadable`) would make it one
answer, read once, under one lock.

---

## 3. Structure

### 3.1 Two modules called "terminal" that are about different things

`pane/terminal/` is the emulator wrapper (`PaneTerminal`, rendering,
history, search). `terminal/state/` is agent ownership and hook arbitration
for a pane (`TerminalState`, about 3,000 lines of production code in
`source.rs` and its children). Nothing in `terminal/state` touches a
terminal. The naming makes "terminal" mean three things in this crate (the
VT, the per-pane agent record, and `TerminalId`, the durable pane record id).

Recommendation: rename `terminal/state` to something like `agent_record` /
`PaneAgentState`, and split it further (3.2).

### 3.2 The hook-source machine is agent-domain logic living in mux

`HookSourceState`, `HookGeneration`, the report and start routing, sequence
re-anchoring against clock steps, stale-session retirement and the checkpoint
candidate are pure functions of shepr-agent's descriptors
(`hook_session_policy`, `full_lifecycle_hook_authority`,
`session_identity_only_integration`) plus `ChildExitReason`. They use nothing
from mux except constants in `limits.rs` and `TerminalId` for a log field.

Moving the machine into shepr-agent as an `AgentOwnership` type (with the
typed `HookOwner` from 2.5) would put the policy next to the descriptors it
interprets, delete the string-pair predicates shepr-agent exports only for
mux, and leave `TerminalState` as a thin composition of cwd, title, label,
restore error and `AgentOwnership`. Test layout follows: the 2,000+ lines of
transition tests move with it.

Inside the machine, `HookSourceState::transition` is presented as a
"generation/event table", but most arms ignore the generation (`(_, ...)`) and
several events are pure queries (`OrderAllows`, `DetectorObservation`,
`Report`, `Start`) answered through effect variants
(`OrderAllowed(bool)`, `DetectorObservationAllowed(bool)`, `Report(route)`,
`Start(route)`) that callers destructure with `let ... else { return None }`.
Queries should be methods returning their own types; the event table should
contain only state changes. That shrinks `HookSourceEffects` to the
`Commit`/`ProcessObserved` effects that actually write pane slots.

### 3.3 `TerminalState` public fields bypass the arbitration it centralises

`terminal/state/mod.rs` opens with "Effective state arbitration is
intentionally centralized here", but `state`, `detected_agent`,
`fallback_state`, `terminal_title`, `manual_label`,
`pending_agent_resume_plan`, `restore_error` and
`last_agent_state_change_seq` are `pub`. `state` is a cache of
`effective_agent().state` that `recompute_effective_state` maintains; any
writer can desync it. Production writers today are limited
(`restore_error`, `pending_agent_resume_plan`, `last_agent_state_change_seq`
in the server and restore), but server tests set `detected_agent` and
`state` directly (`terminal_titles.rs`, `api/detect.rs`), building states the
machine cannot produce. Make the fields private, give the legitimate writers
methods (`record_start_failure`, `clear_resume_plan`, ...), and give tests a
fixture constructor that goes through the real transitions.

`set_hook_authority_at` is a public production method documented as a
"convenience seam for fixtures, taking the source as a string". It should be
test-only or deleted once fixtures can build a typed `HookOwner`.

### 3.4 `PaneRuntime` is a 60-method facade with policy hidden in the forwarding

Roughly 60 public methods on `PaneRuntime` forward one-to-one to
`PaneTerminal`. A handful of them carry policy that is easy to miss among
the forwards: `paste_payload` (bracketed-paste sanitising),
`encode_mouse_button`'s extra gate, `encode_alternate_scroll` (wheel to
arrow keys), `cursor_state` (clipping to an area), `try_send_focus_event`
(gating on focus reporting). Meanwhile `runtime.rs` (1,600 production lines)
also holds cwd arbitration, the deferred-effect ticket order, the
synchronized-output timer, the read-effect dispatch and PTY setup.

Suggested split:

- `pane/cwd.rs`: `ReportedCwd`, `PersistedCwd`, `PaneCwdState`,
  `PaneCwdProbe`, `publish_reported_cwd`, `follow_cwd_from_processes`, and
  the cwd half of 2.4;
- `pane/read_effects.rs`: `PaneReadEffects`, `DeferredEffectOrder` and its
  ticket, `SyncTimeoutRender`;
- `pane/spawn.rs`: `PtySetup`, `prepare_terminal`, the spawner from 2.11;
- `pane/input.rs`: key, mouse, paste and focus encoding policy, so the input
  rules live together instead of half in `runtime.rs` and half in
  `terminal/backend.rs`.

Expose the terminal's read surface through a narrow handle (or let callers
borrow `&PaneTerminal` through one accessor) instead of mirroring it
method by method.

### 3.5 Dependency direction inside `pane/`

- `pane/terminal/backend.rs` constructs `super::super::runtime::TerminalDirtyPatchSnapshot`
  and returns `crate::pane::WheelRouting`, both declared in `runtime.rs`. The
  terminal model depends on the runtime layer that owns it. Both types belong
  in `pane/terminal.rs`.
- `PaneTerminal` takes `&ChildLiveness` (`maybe_restore_host_terminal_theme`,
  `resolve_default_color_owner`) and scans `/proc` through `osc.rs`
  (`current_transient_default_color_owner`, `should_restore_host_terminal_theme`).
  The emulator wrapper is doing process observation. The process questions
  ("who owns this override", "is the shell back in front") belong to the
  runtime / detection side; the terminal should only offer
  `note_default_color_owner(generation, Pgid)` and
  `drop_default_color_overrides_if(owner)`.
- `PaneTerminal::render_queued: Arc<AtomicBool>` is render-scheduler state
  stored on the terminal so `RenderSignal::request_pty_coalesced` can be
  handed `&terminal.render_queued` by both the read path and the detection
  task. It belongs with `RenderSignal`'s per-pane state.
- `DetectionTask::tick` and the osc tests reach into
  `terminal.core` and read `core.detection_content_seq` directly; all of
  `PaneTerminalCore`'s fields are `pub` or `pub(super)`. A
  `PaneTerminal::detection_content_seq()` accessor and private core fields
  would let the revision rules in 2.6 actually be enforced.
- Every mutex in mux (arbiter, cwd state, teardown tracker, deferred order,
  sync timer) uses `shepr_vt::lock_auxiliary` / `recover_auxiliary_poison`.
  A poison policy for non-terminal state is owned by the terminal emulation
  crate; it belongs in `shepr-core` or `shepr-platform`.

### 3.6 `osc.rs` does four unrelated jobs

It parses OSC 7 `file://` URIs (with hostname matching and percent
decoding), runs a byte-level OSC framing state machine for the debug log,
keeps the agent title/progress evidence tracker, and implements the
host-theme restore policy for OSC 10/11 overrides (which scans `/proc`).
Split into `osc7.rs` (cwd report parsing), `osc_debug.rs`,
`agent_osc.rs` (title/progress evidence) and the theme-override logic
moving per 3.5.

### 3.7 `process_probe.rs` holds the whole detector

The file is named for the `/proc` probe but contains `DetectorState`, the
tick protocol, the screen cache, the probe scheduler, agent presence counting
and the publish helpers. `agent_detection.rs` holds a few pure decision
functions the detector calls. Natural layout: `detect/state.rs`
(`DetectorState` and the tick), `detect/schedule.rs`
(`ProcessProbeScheduler`), `detect/probe.rs` (the /proc probe and
`ForegroundShellProbe`), `detect/publish.rs` (the pending-idle and publish
decisions now in `agent_detection.rs`), and `detection_task.rs` as the async
shell. The runtime tests that exercise `foreground_shell_agent_action`
(through a `#[cfg(test)]` wrapper in `process_probe.rs`) live in
`runtime.rs`'s test module today and would move next to the detector.

### 3.8 Copy-mode motion is split across three crates

Line motions are computed in the server (`copy.rs` reads a row with
`extract_selection`, then calls `shepr_termio::copy_mode::last_character_col`
/ `first_non_blank_col`); word and paragraph motions and search are in
`pane/terminal/text.rs`; the server keeps the column for paragraph motions
and clamps line motions to `terminal_dimensions()`. "Where does a copy-mode
motion land" has three homes. Either all motions move into the pane terminal
(one `motion_target(point, Motion)` taking the protocol's motion enum) or the
text engine moves to shepr-termio next to the existing copy-mode helpers and
mux only supplies row text.

### 3.9 `PaneRuntimeRegistry` keys by terminal id, events key by pane id

Runtime events carry `PaneId` (and `PaneRuntime` knows its `pane_id`), but
the registry is `HashMap<TerminalId, PaneRuntime>`. Admitting one event
(`admit_runtime_event`) walks every workspace to map pane to terminal to
runtime; `pane_exit_needs_checkpoint` and the detector gate in
`handle_internal_event_inner` walk them again. Either key the registry by
`PaneId` (with the terminal id on the runtime) or put the terminal id in the
runtime envelope so admission is a single lookup.

### 3.10 `PaneSpawnHandles` lives in `workspace.rs`

It is the pane runtime's spawn context and is consumed only to call
`PaneRuntime::spawn`. It belongs in `pane/` and should be what `spawn`
accepts (2.11).

### 3.11 Test-only API on production types

`PaneTerminal` has a `#[cfg(test)]` `input_state()` and the `InputState` /
`ScrollPosition` types exist only for tests; `PaneRuntime::detection_text`
and `snapshot_history` look production but `detection_text` is used only by
a server test, and `primary_history_ansi` duplicates the cached history path
for `agent_resume.rs`. Prune the production methods nothing production calls,
and keep test probes in test modules.

---

## 4. Types that resolve to primitives

### 4.1 `AgentSource` collapsed to `String` at the door

`transition_report` and `transition_start` receive `source: AgentSource` and
the first thing each does is `let source = typed_source.to_source_string();`.
From there:

- `HookAuthority { source: String, agent_label: String, .. }`;
- `hook_sources: HashMap<String, HookSourceState>`;
- `SuppressedFullLifecycleHookReport::agent_label: String`,
  `StaleFullLifecycleHookSession::agent_label: String`;
- comparisons like `session.source.as_str() == source && session.agent.label() == agent_label`
  (typed values projected to strings to compare with strings) appear in
  `report.rs`, `start.rs` and `source.rs` at least eight times;
- `AgentSource::from_pair(source, agent_label)` is called again to get the
  typed value back in `persisted_agent_session_matches`,
  `conflicting_same_owner_session_ref`,
  `session_report_allows_session_replacement`,
  `is_unsequenced_opencode_selection`,
  `foreground_agent_confirms_different_owner_takeover`,
  `warn_unrecognized_hook_identity`;
- `Agent::parse_canonical_label(&authority.agent_label)` recovers the agent
  from the label at every read.

`AgentSource::as_str` itself has a sentinel: an `Official(agent)` without an
integration source yields `""` (`unwrap_or_default()`).

What the type should offer instead: keep `AgentSource` (and a typed label,
`AgentLabel`, or `Option<Agent>` plus the custom label) in every record;
implement `Hash`/`Eq` for the map key; give `HookOwner` (2.5) methods for
every predicate (`is_full_lifecycle()`, `allows_session_replacement(start)`,
`owns(&PersistedAgentSession)`), so nothing compares `as_str()` output.

### 4.2 Content and detection revisions are raw `u64`

`PaneTerminalCore::content_revision`, `detection_content_seq`,
`synchronized_output_epoch`, `history_epoch`, `default_color_generation`;
`PaneRuntime::content_seq() -> u64` (returning `0` when the core is
poisoned); `synchronized_output_state() -> Option<(bool, u64)>`;
`TerminalDirtyPatchSnapshot::content_revision: u64`. The server computes on
them (`| 1`, `is_multiple_of(2)`, before/after equality). `DetectorState`
keeps `last_screen_scan_detection_content_seq: Option<u64>` and threads
`Some(input.content_seq)` into three functions that accept `Option<u64>` but
are never given `None`. Newtypes per counter (`ContentRevision`,
`DetectionSeq`, `SyncEpoch`, `HistoryEpoch`, `DefaultColorGeneration`) with
the operations callers need (`bump`, `is_stable`, `changed_since`) and the
dead `Option` removed. `synchronized_output_state` returns a named
`SyncOutputState { open: bool, epoch: SyncEpoch }`.

### 4.3 `modify_other_keys_level() -> u8`

`PaneTerminal::modify_other_keys_level` turns
`shepr_vt::ModifyOtherKeysLevel` into a `u8` (`as_u8()`, with `0` on a
poisoned core), `PaneRuntime` forwards it, and the server compares
`runtime.modify_other_keys_level() > 0` (`server/headless/render.rs`).
`encode_terminal_key_once` also builds `KeyEncodeModes { modify_other_keys: u8 }`.
Return the enum, give it `is_enabled()`, and let `KeyEncodeModes` hold it.

### 4.4 Tuples standing in for named pairs

- `PaneTerminal::dimensions()` / `PaneRuntime::terminal_dimensions() -> Option<(u16, u16)>`
  is `(cols, rows)`; the test-only `PaneRuntime::current_size() -> (u16, u16)`
  is `(rows, cols)`. The server tests assert
  `terminal_dimensions() == Some((grown.1, grown.0))`. Return `GridSize`
  (already in `shepr_core::geometry`).
- `PaneRuntime::pixel_size() -> Option<(u32, u32)>`; a named pixel size.
- `helpers::terminal_recent_read_range -> Option<(usize, usize, u16)>` is
  `(start, end, cols)`.
- `PaneHistoryCache::parts()` yields `(&Arc<str>, Option<usize>, bool)`.
- `TerminalDirtyPatch.rows: Vec<(u16, Vec<CellData>)>`.
- `previous: Option<(TerminalTextPoint, TerminalTextPoint)>` (1.13).
- `pane/terminal/helpers.rs` `osc_rgb_response(command: &str, r, g, b)` takes three bytes
  rather than an `RgbColor`, and `color_query_response` builds the command as
  a string (`"10"`, `"11"`, `"12"`, `format!("4;{index}")`) from what is a
  typed `ColorQueryTarget`.

### 4.5 Sentinels standing in for absence

- `ChildLiveness` pid `0` (1.3).
- `AgentOscStateTracker::latest_title()` / `latest_progress()` return `""`
  for none; `AgentDetectionInputs { osc_title: String, osc_progress: String }`
  and the detector's `screen.map_or("", ..)` carry the empty string as "no
  evidence". `Option<&str>` through to shepr-agent's matcher.
- `PaneRuntime::content_seq()` returns `0`, `modify_other_keys_level()`
  returns `0`, `agent_detection_inputs()` returns empty strings, all for a
  poisoned core. `PaneTerminal`'s doc explains the choice (the actor ends the
  pane within a second); fine as a policy, but the detection task then
  caches against revision `0`, and `content_seq` `0` reads as an even,
  stable revision to the server.
- `LaunchStatus.program` `""` and the empty-`PathBuf` chdir failure path
  (1.9).
- `PaneHistoryCache::revision` `0` (1.14).
- `paragraph_motion_in` direction `0` (1.13).
- `PaneTerminalCore::initial_default_foreground` /
  `initial_default_background: Option<RgbColor>` are always `Some` (set in
  `new_inner`), so `terminal_default_fg` / `terminal_default_bg` carry a dead
  `None` branch. Plain `RgbColor`.

### 4.6 String-typed closed sets

- `OscDebugEvent::command: String` is one of `"0"`, `"2"`, `"9"`, `"21337"`
  (matched as byte literals in `parse_osc_debug_event`).
- `report_terminal_mutation_failure(operation: &'static str)` and
  `report_dirty_patch_fallback(reason: &'static str)`: operation and reason
  are closed sets written as literals at each call.
- OSC 9;4 progress is stored and matched as the raw payload string
  (`"4;3;"`); its state is a small closed set the manifests could match on
  typed.

### 4.7 `From<PaneClearError> for String`

An escape hatch from a typed error to prose. The one caller
(`copy.rs::handle_pane_clear`) matches the enum and writes its own message,
duplicating `PaneClearError`'s `Display` text for `AlternateScreenActive`.
The `From` impl appears unused; delete it. `Display` and the server message
should be one string.

### 4.8 `PaneLaunchEnv::extra: Vec<(String, String)>`

A loosely keyed environment list, unused in production (1.12). If it
returns, key it by `EnvVar` / `ChildEnv` (or a validated custom name).

---

## 5. Lateral findings

### 5.1 Untagged runtime events would skip the generation check (latent bug)

See 1.1. No production producer sends one today, but the type permits it and
admission would accept it. Worth closing before someone adds a producer with
`EventSender::from(tx)`.

### 5.2 Tests exercising test-only twins (test-coverage gap)

See 2.8. The production `plain_page_keys_use_host_scrollback` and the
production symbol normalisation in the render path are not what those tests
check.

### 5.3 `foreground_cwd` can return a deleted directory

See 2.3. `PaneRuntime::foreground_cwd` uses `absolute_process_cwd`, which
keeps the kernel's ` (deleted)` suffix that `readlink_process_cwd` documents
as unusable. Whoever uses the foreground cwd (Git identity via
`Workspace::foreground_cwd_for_pane`) may be handed a path that does not
exist.

### 5.4 No-op scrolls bump the content revision

`scroll_up`, `scroll_down`, `scroll_reset` and `set_scroll_offset_from_bottom`
add 2 to `content_revision` whether or not the viewport moved. Wheel events at
the top or bottom of history, and repeated `scroll_reset` calls, invalidate
every client's baseline for that pane and can trigger re-renders. Checking the
scrollbar before and after (as `resize` already does for the grid) is cheap.

### 5.5 Detection-sequence bump differs between the two flush paths

See 2.6. Harmless today because a PTY read is never empty, but the
`process_pty_bytes_locked` flush depends on that rather than on the flush
itself.

### 5.6 The `content_revision` doc contradicts the server

`PaneTerminalCore::content_revision` says parity is no longer used; the
server's `client_shell.rs` uses parity to mark torn reads (2.6). One of the
two is stale; the code says the doc is.

### 5.7 Stale comment in `note_default_color_change`

It says "`shell_pid` 0 (no child yet) is handled there", but
`resolve_default_color_owner` takes `live_pid()`, an `Option`; there is no
`shell_pid` 0 path any more.

### 5.8 `launch_status` reads the program from `SHELL`

`PtySetup::start` builds the launch's `program` from
`cmd.get_env(ChildEnv::Shell)`, which `PtyCommand::interactive_shell` set from
the configured shell. If `PaneLaunchEnv::extra` ever sets `SHELL` (it is an
`Allowed` variable), the exec-failure message would name the wrong program.
A `PtyCommand::program()` accessor removes the indirection.

### 5.9 Settlement of a non-required cwd failure names the first candidate

`directory_failure(cwd_candidates.first())`: for a fresh pane whose requested
directory, `HOME`, passwd home and `/` all failed, the placeholder blames
only the first. Unlikely in practice; the record should carry the index (1.9).

### 5.10 `effective_agent()` re-parses strings on render paths

`border_label`, `is_agent_terminal` and `effective_agent_label` run per pane
when the sidebar and borders are projected; each call parses the label and
re-derives the source classification (2.5). Not hot in the per-byte sense,
but it scales with panes times projections, and the typed owner makes it
free.

### 5.11 Admission walks every workspace per runtime event

`admit_runtime_event`, `pane_exit_needs_checkpoint` and the detector gate
each scan all workspaces to find a pane's runtime (3.9). Every PTY clipboard
write, cwd report and detector update pays it.

### 5.12 `PaneRuntime::resize` takes `&self` with a `Cell`

`current_size: Cell<PaneGeometry>` makes `PaneRuntime` `!Sync` and hides a
mutation behind `&self`. It is only ever resized from the app thread, so
`&mut self` states that honestly.

---

## 6. If this were rewritten

A sequence that front-loads the payoffs:

1. Typed runtime envelope and `RuntimeEventSender` (1.1, 3.9). Small, closes
   a latent bug, and simplifies admission.
2. Typed `HookOwner` resolved at ingestion; move the hook-source machine to
   shepr-agent as `AgentOwnership`; private `TerminalState` fields; typed
   `HookOutcome` (1.10, 2.5, 3.2, 3.3, 4.1). Largest deletion of re-derived
   decisions.
3. Core revision bookkeeping in one place with newtypes, and private core
   fields (2.6, 3.5, 4.2, 5.4 to 5.6).
4. `ChildLiveness` phase enum with the fixture case lifted out, `Pid`/`Pgid`,
   `observe(|pid| ..)`, one foreground probe, one process-cwd reader, one
   pane-cwd query (1.2, 1.3, 2.1 to 2.4).
5. `PaneSpawner` with `LaunchKind` and typed start failures (1.9, 1.12, 2.11,
   2.12).
6. File splits for `runtime.rs`, `osc.rs` and `process_probe.rs`, and the
   detector typestate (1.6 to 1.8, 3.4, 3.6, 3.7).
7. Delete the test-only twins and point tests at production rules (2.8,
   3.11).
