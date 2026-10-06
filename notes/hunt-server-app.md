# Defect hunt: server app and render (`crates/shepr-server/src/app/`, `crates/shepr-server/src/ui/`)

Reconnaissance only; nothing was edited. Findings are ordered by how much they
matter. Each one names the claim it breaks. A list of areas that were checked
and held up comes after the findings, so the next pass need not repeat them.

## F1. Title changes on panes with no agent bump the shared projection and the server-wide view epoch

- Where: `AppState::sync_terminal_titles` (`app/state.rs`), `App::sync_terminal_titles`
  (`app/terminal_titles.rs`), and its callers in `server/headless.rs` (the render
  loop's `sync_terminal_title_sources` followed by `mark_view_changed`, and
  `sync_pending_terminal_titles` in `dispatch_api_request` and in
  `endpoint_requests.rs`).
- What happens: `AppState::sync_terminal_titles` stores the new title for every
  pane the PTY parser reported. When any raw or stripped title changed, it calls
  `mark_shell_projection_dirty()`, whatever the pane is. The only titles that
  reach a client are in `ClientShellAgent` (`shepr-protocol/src/projection.rs`),
  which `App::agent_info` builds only for panes with an `effective_agent()`.
  `ClientShellPane` and `ClientShellWorkspace` carry no title, and the outer
  window title is the client's own `shepr: <label>`. So a title change on a plain
  shell pane changes nothing any client can see. It still:
  - advances `shell_projection_revision`, which makes
    `refresh_stale_shell_session_cache` rebuild `projection_input()`. That
    rebuild runs `snapshot_pane` for every pane of every workspace, including the
    `foreground_cwd_for_pane` `/proc` reads, and advances the session generation;
  - returns `true` to the render loop, which calls `mark_view_changed()`. That
    moves the server-wide view epoch and sends every client through a full
    planning pass.
- Claims broken:
  - The comment on `App::sync_terminal_titles` says "Any changed title also
    changes the shell agent metadata". That is false for panes with no agent.
  - AGENTS.md, under "Presentation is per client", promises "a server-wide view
    epoch only for changes every client depends on".
  - The "Hot paths multiply" principle. A shell whose prompt or preexec hook sets
    the title with OSC 0 or 2 (common in zsh and bash setups, and in vim and
    htop) sets this off on every command, in every pane, for every client.
- Fix direction: only mark the projection dirty when a changed title belongs to
  a pane that has an effective agent. Keep storing the title for every pane, so
  an agent detected later has its title at once; the state change that makes
  the agent effective already bumps the revision. The server's
  `sync_terminal_title_sources` should then call `mark_view_changed` only when
  the projection moved. The simplest way is to return `observe_projection_change`
  instead of the raw `TerminalTitleChange` flags.

## F2. The final save under stopped or blocked persistence is reported as an error, and the documented `stopped` / `blocked_on_backup` outcomes are never logged

- Where: `App::save_session_before_teardown_async` (`app/session.rs`) and the
  final-save logging in `server/headless.rs` (the `kind = "final"` block).
- What happens: if the policy is `Stopped` or `BlockedOnBackup`,
  `submit_final_session_save` returns `Ok(None)`. The
  `Ok(None) if self.session_saver.policy.is_unavailable()` arm then turns that
  into `Err("session persistence was blocked before the final save")`. The
  caller logs every `Err` at ERROR with `outcome = Error`. Its INFO branch has
  arms for `Outcome::Stopped` and `Outcome::BlockedOnBackup`, but those arms
  cannot run: with either mode set, the app always returns `Err`. The only
  non-error outcomes that can actually be logged are `ok` and `frozen`.
  The `Err` also becomes `RunServerError::Runtime` (an unclean exit), and every
  accepted `server.stop` gets it as a failed final save, so `shepr stop` fails.
- Claims broken: `reference/session-save-shutdown.md` says the
  `persist.save kind = "final"` log records "`ok`, `error`, `stopped`,
  `blocked_on_backup` or `frozen`", with failures at error level and "any other
  outcome at info level". The message also says "blocked" for the `Stopped`
  mode, which is not what happened.
- Which side is wrong: the server's dead arms and the document agree with each
  other. That suggests the intent was to report these as their own outcomes,
  which points at the app's `Err` as the wrong side. Whether a stop should
  *fail* when persistence was already off for the boot is a policy question for
  the owner. Either way, one side has to change, and so does the "blocked"
  wording for `Stopped`. No test covers a final save under `Stopped` or
  `BlockedOnBackup`.

## F3. A combined checkpoint waits for the later of the two retry schedules (`max`), so the host checkpoint inherits the pane-exit backoff and the reverse

- Where: `SessionSaver::deadline` (`app/session.rs`):
  `self.exit.retry_at().into_iter().chain(self.host.retry_at()).max()`.
- What happens: one save answers both a held pane exit and the logind warning.
  After a combined failure, each machine arms its own retry, from its own
  failure count (`failed_with_config`), and the save waits for the *later* one.
  - If the pane exit had earlier consecutive failures, the host checkpoint waits
    out the exit's longer backoff, while logind's delay inhibitor is held.
  - In the other direction, a pane exit newly requested after a host failure
    starts with `retry_at: None`, which reads as "start at once". It still waits
    for the host's retry, and the dead pane stays on screen the whole time.
- Claim broken: `request_host_checkpoint` calls `exit.expedite_retry()` so that
  "the combined save starts at once instead of waiting for it". The intent is
  that the host checkpoint does not wait on the exit's schedule. That holds only
  until the first combined failure, after which `max` restores the coupling.
- Severity: low. `CHECKPOINT_MAX_FAILURES` is 3 and `CHECKPOINT_RETRY_MIN` is
  250 ms, so the extra wait is a few hundred ms. Taking `min` (run the combined
  save as soon as either machine is due) matches the stated intent.

## F4. A workspace created with an explicit cwd silently starts elsewhere when that directory cannot be entered, and keeps the name of the directory it is not in

- Where: `App::handle_workspace_create` (`app/api/workspaces.rs`) together with
  `create_workspace_outcome` (`app/creation.rs`).
- What happens: `WorkspaceCreateSource::Cwd(raw)` is only checked lexically
  (`launch_cwd`) and then launched with `LaunchKind::Fresh`. Fresh launches fall
  back to `HOME`, the passwd home or `/` when the chdir fails
  (`LaunchKind::requires_cwd` is false for `Fresh`). `prepare_workspace(cwd)`
  has already named the workspace after the requested path. If the user typed a
  wrong or nonexistent path:
  - the command answers `Done`;
  - the workspace appears under the requested directory's name but runs in
    `$HOME`;
  - the only trace is a WARN `pane.cwd outcome = Fallback` in the server log.
- Claim broken: `WorkspaceCreateSource` documents itself as "Where a new
  workspace's first pane starts", with `Cwd` as "An explicit working
  directory". Its sibling, `Default`, resolves through the server's
  new-terminal-cwd policy, where a fallback is reasonable. An explicit path the
  user named is a different case.
- Direction: either launch an explicit `Cwd` with a required cwd (a failure
  becomes the existing placeholder, "Pane directory is unavailable"), or report
  the fallback to the requester. The workspace name should follow the cwd the
  launch settled in, or the refusal.

## F5. An agent resume whose launch status became unreadable is recorded and logged as "shell launch unconfirmed", without its cause

- Where: `App::handle_pane_launch_settled`, the
  `LaunchOutcome::StatusUnavailable(error)` arm with
  `kind == LaunchKind::AgentResume` (`app/pane_launch.rs`).
- What happens: it calls
  `fail_agent_resume(pane_id, ResumeUnavailableReason::ShellLaunchUnconfirmed, None)`.
  The same arm for a non-resume pane records
  `PaneStartFailure::launch_unobservable(&error)`, which keeps the cause.
  - The resume placeholder tells the user the launch was "unconfirmed", which
    is the reason for the timeout case (`Unconfirmed`). Here the real cause was
    the status channel failing.
  - The `error` is dropped from the `fail_agent_resume` log line, which its own
    doc comment calls "the one log line for the failure". The mux coordinator
    did log the channel error, but as a separate ERROR line under another event.
- Claim broken: the `fail_agent_resume` doc ("`detail` carries the cause a
  caller observed ... so the caller does not log it again"), and the
  distinction `ResumeUnavailableReason` exists to draw.
- Severity: low (a diagnostic only).

## F6 (doc drift, minor). `App::sync_terminal_titles` and `AppState::sync_terminal_titles` overstate what a title change affects

Same root as F1. This is called out on its own because the same claim appears
in a second comment ("a changed title updates the shell agent metadata, so it
requires a projection", on `HeadlessServer::sync_terminal_title_sources`). Fix
both comments together with F1.

## Lateral observations (outside the immediate question, or below defect level)

- **Hot path, a double core read per cursor.** `PaneSurface::cursor`
  (`ui/pane_surface.rs`) reads the runtime twice per focused pane per render:
  once through `runtime.read().cursor(area)` and again through
  `pane_is_scrolled_back(runtime)`, which calls `read().scroll_metrics()`.
  `compute_pane_surfaces` likewise takes two reads per pane (alternate-screen
  state, then scroll metrics). These are per pane, per client, per pass. One
  read returning both would follow "keep terminal-core locks short".
- **Stale wording, `ui/panes.rs`.** The test name
  `alternate_screen_reclaims_scrollbar_gutter_without_resizing_panes` describes
  the pure computation only. The server does resize the PTY when the screen
  flips (`settle_workspace_geometry_before_plan`, tested by
  `first_shell_surface_resizes_a_pane_that_entered_alternate_screen`). The
  behaviour is consistent, but a reader of the ui test alone could conclude the
  opposite. Renaming it to say "computes without resizing" would remove the
  trap.
- **Retired-plan scan.** `has_pending_agent_resumes` walks every pane record on
  every loop iteration until the schedule retires. A plan whose pane already
  has a runtime (`candidate(true)` is `None`) but stays `is_pending()` would
  keep the schedule unretired, and the scan running, for the boot. I found no
  path that creates that state today: restore mints plans only on runtimeless
  panes. If one ever appears, the cost is silent.
- **Unreachable branch.** `handle_workspace_create` ends with
  `if self.state.workspace(&workspace_id).is_none() { internal_with_effects(...) }`,
  which cannot fire. `commit_workspace_creation` just inserted the workspace and
  nothing runs in between.
- **Needless `&mut self`.** `handle_detect_capture` and `handle_detect_explain`
  take `&mut self` but only read.
- **Reaching the dispatcher during a host-shutdown freeze.** When saves are
  frozen or stopped, `request_pane_exit_checkpoint` returns `None` and
  `decide_pane_exit` records the decision as `CheckpointDecision::Settled`
  ("checkpointed, and the checkpoint is already settled"). Nothing was
  checkpointed. The removal behaves correctly, because `preserved()` decides
  what `finish_checkpointed_pane_exit_after_event` does. Still, the name says
  something false, and a future reader of `checkpointed()` could act on it. A
  distinct `Released` decision would keep it honest.

## Checked and found sound

So the next pass need not redo these:

- **Runtime generation admission.** Envelope checks, the re-check in
  `handle_prepared_pane_exit`, and a resume runtime replacing the pane's
  runtime under the same key.
- **Lifecycle authority mirror.** `update_terminal_state` always marks the pane,
  `forget_removed_panes` drops pending entries, and `install_runtime` seeds new
  runtimes.
- **Pane-exit checkpoint machine** (`exit_checkpoint.rs`). Generations never
  repeat; an older layout releases newer holds only while no mutation was seen;
  the `session_dirty` filter catches mutations the loop has not consumed; the
  freeze releases held exits; abandonment ends on any success.
- **Host checkpoint and lifecycle.** Cancel voids the host half of an in-flight
  ticket; a refreshed warning gets a fresh checkpoint whether the server is
  warned or frozen; stopped persistence completes the host request
  synchronously; the inhibitor is released only for the current warning
  generation. Matches `reference/session-save-shutdown.md` apart from F2.
- **Deferred pane resizes** (`pane_resize.rs`). Per-workspace deadlines; an
  unchanged target keeps its deadline; returning to the current size settles
  the workspace; a first layout is always immediate. Screen flips are re-applied
  immediately by the server, and topology and zoom commands are immediate.
  Ratio and resize commands are `Settled` through `gesture_step`.
- **Spawn and resume sizing.** These agree with the drawn content rect: the
  primary screen with the gutter reserved, and zoom-hidden panes at their tiled
  size.
- **Launch settlement arms.** `Failed`, `Unconfirmed` and `StatusUnavailable`
  for Fresh, Restored and AgentResume: the runtime is retired before its queued
  death can remove a resume placeholder.
- **Border, junction, focus-mark and title arithmetic** in `ui/panes.rs`.
  Index and overflow edges included: the inclusive loops are guarded by
  `contains` before any `+ 1`. Split hit rects match the divider columns the
  border grid draws.
- **Endpoint effects and invalidation mapping** (`endpoint.rs`). Every
  projected field changed by a reducer (zoom, focus, pane input, labels, order)
  marks the projection. Geometry edits rely on the effects' shared render
  instead, which is correct, since no projected field depends on split ratios.
