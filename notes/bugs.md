# Defects

Filed from a nine-scope hunt (persistence, restore-resume, save-shutdown,
pane-lifecycle, agent-state, integrations, workspace-model, server-lifecycle,
remote). Each entry names the hunts that reported it. Hygiene findings from the
same hunt are in `notes/hygiene-*.md`; where a defect has a hygiene side, the
entry says which.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

---

## BUG-003 - The restore notice never names the session file that failed

Reported by: persistence.

`files::load` builds `SessionRestoreFailure` from `err.to_string()` or the serde
error. For `Unreadable`, `TooLarge` and `Unparseable` the detail has no file
path (serde says "expected value at line 1 column 1"). The client's notice names
the backup directory but not the file that failed; only the server log has the
path. Fix: carry the session path in `SessionRestoreNotice` beside `backup_dir`.
A protocol test can assert the rendered notice contains it. Related:
DEAD (the machine-readable failure taxonomy nothing reads).

## BUG-004 - A capture inconsistency silently drops a workspace from disk

Reported by: persistence.

`capture_workspace` returns `None` (logged) when a tree's focus or root lacks a
record, or its shape and records disagree. `capture_deferred` skips that
workspace and the save proceeds, replacing the session file without it and with
no backup (the backup policy covers only load-time losses). If every workspace
fails, the job is a `Save` of zero workspaces, not a `Clear`. The comment says
constructors and mutators rule this out, which is exactly when failing closed
costs nothing.

Fix: a capture inconsistency fails the whole job (a new `SaveError` variant, or
`capture_job` returning `Result`) so the last good file survives. Enforce with
a test that builds an inconsistent tree through a seam and asserts the file is
unchanged.

## BUG-005 - A permanently unreadable session file retries forever with no operator signal

Reported by: persistence.

When the session file is unreadable (EACCES), the load is `Unusable` and the
policy `PreserveExisting`. Every save then fails in `preserve_existing` (the
source cannot be opened to back it up) with a retryable `SaveError::Io`, so
autosave retries for the life of the boot, logging an error and a warning each
time. The operator sees the restore notice ("copied ... before the server first
saves over it") and nothing saying no save will ever land.
`SaveError::is_retryable` is the classifier; "cannot back up the source" is a
condition the operator must fix, which neither retryable nor refused models.

Fix: a distinct "blocked on backup" outcome the server projects to clients, the
way `session_saves_stopped` is projected. Testable at the writer and saver
level.

## BUG-006 - A duplicate saved workspace ID makes every client report lost panes when nothing was lost

Reported by: restore-resume.

`restore.rs` `plan_restore` sets `restore_damage = true` only when a saved
workspace ID repeats. That workspace is not dropped: it gets a fresh ID and all
its panes. `SessionRestorePlan::launch` then builds
`RestoreLoss::from_damage(dropped_workspaces, restore_damage)`, turning the flag
into `panes_pruned` / `RestoreLoss::Panes`, which `SessionRestoreLoss::Panes`
renders as "The saved session was restored in part: some saved panes could not
be restored." Nothing in restore prunes a pane (`PaneTree::plan` admits or
refuses whole workspaces). The producers are the renamed-ID case and, since id
exhaustion became a refusal, a duplicate ID that cannot be reassigned, which
drops its whole workspace; neither loses individual panes.

The `open.rs` test `restore_with_damage_backs_up_the_saved_session_before_the_first_save`
restores both workspaces in full and then asserts `SessionRestoreLoss::Panes`,
writing the false notice down as expected behaviour. The warn in `open.rs` logs
the same bit as `restore_damage = loss.panes_pruned()`, a third name.

Broken claims: `SessionRestoreLoss`'s doc ("Every variant loses something") and
the rendered operator text. Fix: give the renamed-ID case its own variant (or
make it backup-only with no notice) and drop `panes_pruned` until something
prunes. Enforce by type: build `RestoreLoss` from the events that happened
(dropped workspaces, renamed IDs), not from a bool named for something else.

## BUG-007 - An unconfirmed resume launch loses the saved agent session, with nothing logged

Reported by: restore-resume. See also BUG-019 (when "unconfirmed" happens with
the child alive).

`app/pane_launch.rs`, `LaunchOutcome::Unconfirmed` for
`LaunchKind::AgentResume`, abandons the resume with
`ResumeUnavailableReason::ShellLaunchUnconfirmed`, under the comment "The saved
identity is untouched, so the next restore still resumes the session."
`launch_status.rs` says of `Unconfirmed` that the pane's death follows as an
ordinary one, and an ordinary death goes through `App::apply_pane_removal`,
which removes the pane and its persisted agent session. The next save writes a
layout without it, so the next restore resumes nothing. The checkpoint, where
taken, only delays this.

The `restore_error` the abandonment records is never drawn (`ui/panes.rs` draws
the failure text only for a pane with no runtime, and this pane keeps its
runtime until the death removes it), and neither `handle_pane_launch_settled`
nor `launch_status.rs` logs anything for `Unconfirmed`. The agent pane vanishes,
its session leaves the saved layout, and nothing says so.

Fix: decide what an unconfirmed resume means for the pane (keep a placeholder
carrying the session, as a `Failed` launch is kept) and log it. Enforce with a
test that settles an `AgentResume` launch as `Unconfirmed`, delivers the death,
saves, and checks the saved `agent_session`.

## BUG-008 - A resume command whose send fails leaves a bare shell and an invisible error

Reported by: restore-resume.

`pane_launch.rs` `send_resume_command`, `Some(Err(_))`, warns and then
`abandon_agent_resume(.., CommandSendFailed)`. The pane has a live runtime, so
the recorded `restore_error` is never rendered (`ui/panes.rs` draws it only for
a runtimeless pane). The operator sees a plain shell where an agent was.
`terminal/state/detection.rs` `abandon_agent_resume`'s doc ("no runtime ever
existed here, so that detection is only the seed") is false for this caller and
for BUG-007's.

Fix: render a resume failure through something that does not need a runtimeless
pane, or route these two cases to a different record than `restore_error`.
Testable: assert the surface of a pane whose resume send failed carries the
failure text.

## BUG-009 - A saved agent session that fails to decode is dropped from disk with no backup

Reported by: restore-resume, persistence.

`persist/schema.rs` `deserialize_agent_session` turns an invalid saved session
into `None` with a warn. Restore never learns saved data was discarded, so
`restore_loss` stays `None`, `open.rs` sets `SessionBackupPolicy::NoBackupNeeded`,
and the first save overwrites the only copy of the session reference.

Broken claims: `RestoredSession::restore_loss` ("What saved data restore
discarded ... The caller preserves the source session file whenever this value
is present") and `plan_workspace`'s "The workspace is not lost on disk". A
resumable session ID is exactly what the owner wants back after a multi-day run.

The warn names no workspace, pane or agent, and (persistence) it fires from
every parse, including the fingerprint reads `SnapshotFingerprintCache` makes
of the session file and the newest snapshot copy, so one bad agent session in a
recovery copy logs again whenever that file's stamp changes, outside any
restore context.

Fix: report a discarded session as restore damage (backup before first save),
log it once with identifiers. Testable: a session file with one invalid
`agent_session`; open, save, assert a backup exists.

## BUG-010 - Restore silently repairs damaged focus, root and zoom, against its own stated policy

Reported by: restore-resume.

`restore.rs` `plan_workspace`'s doc: a saved-file defect drops that one
workspace "rather than refusing the whole session ... or repairing it (which
silently rewrites a corrupt file)". `PaneTree::plan` does repair: a saved
`focused` or `root_pane` naming no leaf falls back to the first leaf, and a
saved `zoomed: true` on a one-pane workspace or with a missing focus is dropped.
None of these sets `restore_damage`, so there is no backup and no log, and the
first save rewrites the file. shepr never writes a focus naming no leaf, so
these are corrupt-file cases by the doc's own definition.

Fix: return the repairs from `plan` and count them as damage (backup and log),
or refuse the workspace as documented. Testable as BUG-009. See POL (one rule
for damaged saved values).

## BUG-011 - The resume command is quoted for POSIX shells, but config accepts non-POSIX shells

Reported by: restore-resume.

`agent_resume.rs` `start_pending_agent_resume` types `plan.to_shell_command()`
plus `\r` into the pane's shell. `to_shell_command` is
`shepr_core::shell_quote::join_argv`, documented as POSIX word quoting.
`shepr-platform/src/executable.rs` `SHELL_NAMES` (what `default_shell` / `SHELL`
validation admits) includes fish, csh, tcsh, elvish, xonsh and nu. The
`'a'\''b'` concatenation is not valid in nu, and csh expands `!` inside single
quotes. Plain words pass unquoted, so UUID session IDs work; a Pi or OMP session
path with a space or quote does not. The only end-to-end test of the typed
command runs host `/bin/sh`.

Fix options: refuse resume (or the shell) where the quoting does not apply,
quote per shell family, or launch the resumed agent by a path that does not go
through the interactive shell's grammar. Enforce with a test per accepted shell
family, or a type pairing a resolved shell with its quoting.

## BUG-014 - The final save's failure is swallowed and described as a retry

Reported by: save-shutdown, persistence.

`save_session_before_teardown_async` discards the bool
`finish_final_session_save` returns (so does its test twin). The only trace of
a failed final save is the generic `warn!("session save failed", retry_ms = ..)`,
which promises a retry that never happens because the process is exiting.
`shepr stop` and `run()` report a clean stop either way. A `JoinError` from
`wait_off_the_runtime` is mapped to `SaveError::Abandoned`, which is
non-retryable and logs "session persister cannot accept further saves; disabling
session persistence for this boot" and marks the shell projection dirty, both
wrong for a final save; the `JoinError` (panic vs cancellation) itself is not
logged or kept. The final save also has no retry at all, unlike checkpoints, so a
transient EIO on the last save loses everything since the last autosave.

Fix: a `FinalSave` kind with its own outcome: an error-level line naming the
data directory and error, no retry wording, the `JoinError` kept as source, and
an exit-class signal so the operator learns of it. Consider giving it the
checkpoint's retries. Enforce with a `#[must_use]` outcome type and a test that
injects a failing persister into the final save.

## BUG-015 - The restart offer's stop budget is shorter than an unbounded final save

Reported by: save-shutdown, server-lifecycle.

`shepr-launch` `STOP_WAIT_TIMEOUT` (15 s) and `STOP_LEASE_WAIT_TIMEOUT` (10 s)
must outlast the server's worst stop: `SHUTDOWN_FLUSH_TIMEOUT` (1 s) plus the
final save (unbounded by design; see the "no forced stop" comment in `run`) plus
`PANE_TEARDOWN_WAIT` (3 s). A large session or slow filesystem makes the restart
offer report "did not stop within 15000ms ... kill with SIGKILL" while the save
is healthy, and following that advice loses the final save.

Fix: the stop guidance must not advise SIGKILL while a save may be running, or
the stop waits for a server that reports it is still saving. A `const` assert in
`shepr-daemon` (which links both crates) can hold the bounded part; say beside it
that the save term is unbounded.

## BUG-016 - `CHECKPOINT_RETRY_MAX_DELAY` never takes effect

Reported by: save-shutdown.

With `SESSION_SAVE_RETRY_MIN = 250ms`, `BACKOFF_MULTIPLIER = 2` and
`CHECKPOINT_MAX_FAILURES = 3`, the delays used are 250 ms and 500 ms; the third
failure abandons before a delay is computed, so the 1 s cap is never reached.
Its doc says it is the longest retry delay of a failed checkpoint. It also bears
no relation to logind's `InhibitDelayMaxSec` (default 5 s), the budget the host
checkpoint actually lives in. Enforce with a `const _: () = assert!(..)` that the
uncapped delay after `CHECKPOINT_MAX_FAILURES - 1` failures reaches the cap, or
remove the cap. See VAL (checkpoint retry minimum borrowed from autosave).

## BUG-017 - A launch-status read failure or dropped sender leaves a live pane in limbo

Reported by: pane-lifecycle. See also BUG-007.

`LaunchOutcome::Unconfirmed` is documented as "the child is gone without a
report, or the pane ended before the launch settled. The pane's death follows
and is an ordinary one", and the server relies on it. Several `settle()` paths
return `Unconfirmed` while the child is alive and has exec'd the shell: the
oneshot sender dropped without delivering (BUG-019), `AsyncFd::new(channel)`
failing, `channel.readable()` erroring, or `reader.read` returning a protocol
error. Nothing records an ending, so no death follows. The pane keeps a working
PTY, but `ChildLiveness` stays `Unconfirmed` forever: `live_process_id()` is
`None`, detection never starts, `/proc` cwd tracking is off, `child_pid()` is
`None`, and an agent resume is abandoned as `ShellLaunchUnconfirmed`.

Fix: split the outcome. "Child gone or pane ended" stays `Unconfirmed`; "status
unreadable, child alive" either keeps waiting for the child's exit and then
settles, or is a `Failed` that tears the pane down. Enforce with a test handing
`settle` a fault-injected channel while a fixture child sleeps.

## BUG-022 - `SHEPR_BIN_PATH` names the server binary, is set inconsistently, and nothing reads it

Reported by: pane-lifecycle, server-lifecycle.

`ChildEnv::SheprBinPath` is documented as "the shepr executable, set for every
pane so programs in it can call back into shepr", and `pane/launch.rs`
`launch_executable` as "The path panes are told to run shepr by". It is resolved
from `current_exe()` in the server process, i.e. `shepr-server`, whose argument
grammar is the daemon's, so `$SHEPR_BIN_PATH status` gets a usage error. When
resolution fails, `launch_executable().ok()` caches `None` with no log and the
variable is removed from every pane, so "set for every pane" is false too.
Nothing in the repository reads it (no hook asset, no Rust site besides the
setter), and resolving it is the only reason `init_pane_launches` stats the
server binary.

Fix: given that shepr offers panes no way to drive it, remove
`ChildEnv::SheprBinPath`, mux's `launch_executable()` and the init step; or point
it at `shepr` (`with_file_name(PROGRAM_NAME)`) and say what it is for. Also DEAD.

## BUG-023 - A directory whose name ends in " (deleted)" is treated as deleted

Reported by: pane-lifecycle, workspace-model.

`workspace::process_cwd_is_deleted` decides by the byte suffix ` (deleted)`, the
kernel's marker on a `/proc/<pid>/cwd` readlink. It is also applied to paths that
never came from `/proc`: `terminal_cwd` filters the stored cwd (an OSC 7 report
or a saved path), and `resolved_identity_cwd_from_root_pane` filters whatever it
is handed. A shell in a real directory named `x (deleted)` reads as `Deleted`:
its cwd is never used for saves, splits or the Git identity, and a workspace
rooted there falls back to its construction cwd. Nothing documents such
directories as unsupported.

Fix: apply the check only to the `/proc` observation (in
`PaneRuntime::cwd` / `follow_cwd` / `remembered_cwd`), not to stored state. See
VAL (the marker spelled in two crates).

## BUG-024 - Teardown's survivor report comes from a stale scan

Reported by: pane-lifecycle.

`terminate_pane_session` rescans session membership at the top of each round,
but after the last round builds `survivors` from that round's members, so a
process forked during the final SIGKILL round is neither signalled nor listed in
"pane session still alive after forced shutdown". Fix: one more
`session_members` call before reporting.

## BUG-025 - The inline teardown fallback stalls the event loop

Reported by: pane-lifecycle.

`shutdown_pane_processes` documents "Returns at once ... closing a workspace
never stalls the caller". When the teardown thread cannot be spawned it runs
`run_pane_teardown` inline: up to `PANE_TEARDOWN_BUDGET` (750 ms) of sleeps plus
full `/proc` scans on the caller, which is the event loop (the runtime is dropped
from `PaneRuntimeRegistry::remove` / `clear`). This breaks the rule that a pane
never stalls the server loop. `child_watcher::reap_on_detached_thread` makes the
opposite choice for the same failure. See POL (thread-spawn failure policy).

## BUG-026 - A blocking-pool cancellation drops a child unreaped

Reported by: pane-lifecycle.

`wait_for_child_exit_blocking` moves the `PaneChild` into `spawn_blocking`. A
task not yet started when the runtime shuts down is dropped with its closure,
and `PaneChild` neither kills nor reaps, so `UnreapedChild`'s promise ("it never
stays a zombie for the rest of the process") fails on that path. Low impact (the
process is exiting). Fix: move the `UnreapedChild` guard itself into the
blocking closure.

## BUG-027 - The pane-exit checkpoint gate on an intact terminal core is a pane-history leftover that now loses agent sessions

Reported by: restore-resume, workspace-model.

`app/events.rs` `decide_pane_exit` justifies gating the exit checkpoint on an
intact core with "a core that broke ... has nothing new to give a checkpoint
... since history capture leaves an unreadable terminal's cached history as it
was". `PaneEnding::needs_checkpoint` returns false whenever the core is broken,
and its result also goes to `set_pane_process_exit_at`. With `pane_history`
removed, a checkpoint saves layout, labels, cwd (from `/proc`) and agent
identity (from `AgentOwnership`), none of which come from the terminal core. So
a pane whose reader panicked and whose shell is then signalled (logout) is
removed without the checkpoint that would have kept its agent session for
resume.

Needs the owner's confirmation of intent; AGENTS.md also describes `PaneEnding`
as carrying whether the core is intact. If the identity checkpoint is wanted
regardless of the core, drop the gate and the stale comment.

## BUG-028 - Resume de-duplication misses one session saved as an id in one pane and a path in another

Reported by: restore-resume.

`AgentResumeKey` is the whole `PersistedAgentSession`, so for Pi and OMP
(`SessionRefPolicy::IdOrPath`) the same session saved as an id in one pane and as
a path in another is not detected as a duplicate, and both panes resume it. Rare,
since a report prefers the path when both are present.

## BUG-029 - `detect explain` cannot name the detector hold that withholds a screen verdict

Reported by: agent-state.

Explain now reports the pane's effective state and owner beside the current
screen verdict, and names a matched `skip_state_update` gate. What remains: it
cannot say which mux gate is withholding a screen verdict (the working-to-idle
`PendingIdleConfirmation` hold, the startup grace, the resume absence hold),
because those live privately in the mux detector state and the API handler has
no view of them; the handler says so in a comment. Fix: a mux diagnostic seam
exposing the active gate, carried in the explain payload. Enforce with a test
that explains a pane mid-hold through a real detector hold (the current server
test only builds a state mismatch).

## BUG-031 - `visible_working` claims a screen-evidence refresh that does not exist

Reported by: agent-state.

`Detection::Working`'s doc: "Visible working chrome refreshes screen evidence,
but never overrides hooks", and about forty manifest rules set
`visible_working = true`. Nothing reads it for a refresh: `publish_screen` sets
`last_visible_signal_refresh` for a visible blocker or visible working verdict,
but the only reader, `stable_visible_signal_refresh_due`, requires both previous
and next detections to be visible blockers. The server drops the flag
(`StateEvent::StateChanged` passes only the state and `visible_blocker()`). Its
only effects are an extra `StateChanged` event when visibility flips with the
state unchanged, and a field in `detect explain`.

Fix: implement the documented refresh (and decide what it is for), or delete
`visible_working` from the manifest schema, `Detection`, the API payload and
every manifest. Two ownership tests named "visible_working_does_not_override..."
pass no such flag (see the claims document).

## BUG-034 - Several manifest rules gate on words where their comments promise dialog controls

Reported by: agent-state.

- `opencode.toml` and `kilo.toml` say the permission header can linger, so they
  require it AND one of the dialog's reply controls. The controls are
  `contains = ["reject"]` and `["enter confirm"]` over `whole_recent`, so a
  lingering "Permission required" plus any later transcript text containing
  "reject" or "rejected" reads as Blocked. Only "earlier text only" is tested.
- `pi.toml` `working_literal` is `contains = ["Working..."]` over the whole
  snapshot, so transcript text containing it holds Working (masked while the Pi
  hook governs).
- `claude.toml` `legacy_no_prompt_blocker` blocks on "do you want to" plus "yes"
  anywhere on screen, with no visible-blocker flag and only an empty-prompt `not`.

AGENTS.md asks for invariant controls as explicit AND/OR gates. Enforceable only
by captured-screen tests (see the claims document, manifests without behaviour tests).

## BUG-035 - Letta reads ConEmu progress state 3 as Blocked where Qwen and Kiro read it as Working

Reported by: agent-state. Flagged as surprising, not proven.

`letta.toml` `osc_progress_blocked` is `^4;3(?:;|$)` at the highest priority;
`qwen.toml` `osc_tool_progress_working` and `kiro.toml` `osc_progress_working`
read the same indeterminate state as Working. One may be right for its agent,
but nothing records why Letta's indeterminate progress means a blocker. Needs a
capture.

## BUG-036 - Every JSON agent config except Claude's is fully re-serialized on install

Reported by: integrations.

`targets::prepare_json` (Codex `hooks.json`, Copilot `settings.json`, Devin
`config.json`, Droid `settings.json`, Cursor `hooks.json`, MastraCode
`hooks.json`) and the Antigravity branch of `targets::install` parse the user's
file into `serde_json::Value` and write it back with `to_string_pretty`.
`serde_json` is built without `preserve_order` (`Cargo.lock` lists no `indexmap`
for it), so maps are `BTreeMap`. Whenever a hook is installed or repaired (first
install, any asset change, any user edit to shepr's entries), the user's whole
file comes back with every object's keys sorted at every depth, re-indented to
two spaces, numbers re-spelled (`1e+02` becomes `100.0`), `\uXXXX` and `\/`
escapes rewritten, and duplicate keys silently collapsed. For Droid that is
`~/.factory/settings.json`, and for Copilot and Devin the agent's main settings,
not a shepr-owned side file.

Broken claim: `config_edit.rs` above `ensure_flat_command_hook`: "Keep the
helpers separate so install preserves unrelated hooks in each agent's native
format instead of normalizing user configuration." Claude's path
(`claude_settings.rs`) does it right: a CST edit of only the touched containers,
a duplicate-key refusal, and `verify_updated`. No test checks byte preservation
for the other targets.

Fix: one CST JSON edit engine for every JSON target (generalize
`claude_settings.rs`: remove-by-hook-path, append, verify), deleting
`prepare_json` and the Antigravity branch. `preserve_order` alone is a half fix.
Enforce with a byte-preservation test per JSON target, shaped like
`install_preserves_untouched_formatting_and_complete_trailing_suffix`.

## BUG-039 - Every shell-hook integration is silently inert on a host without `python3`

Reported by: integrations.

All ten shell hooks end with `command -v python3 >/dev/null 2>&1 || finish`. On a
host (or pane `PATH`) without `python3`, Claude, Codex, Copilot, Cursor, Devin,
Droid, Grok, Kimi, MastraCode and Antigravity never report, the installer reports
success, status reads Current, and nothing says so; session resume silently stops
for every one of them. AGENTS.md: integrations "report state and session IDs back
to shepr". At minimum, warn when installing a python-dependent hook and `python3`
does not resolve on the server's `PATH` (imperfect, since the pane `PATH` can
differ). Related: the todo item "Report a missing hook interpreter".

## BUG-043 - Relative agent config-dir overrides resolve against the server's cwd, except OMP's

Reported by: integrations.

`env::config_dir_from_env_or_home` returns a relative `CLAUDE_CONFIG_DIR`,
`CODEX_HOME`, `COPILOT_HOME`, `CURSOR_CONFIG_DIR`, `KIMI_CODE_HOME`, `GROK_HOME`,
`PI_CODING_AGENT_DIR` or `ANTIGRAVITY_CLI_CONFIG_DIR` unchanged (`EnvKind::Path`
accepts relative values), so every later `fs` call resolves it against the server
process's cwd; the agent resolves it against the pane's. `omp_extension_dir`
joins a relative `PI_CONFIG_DIR` onto `HOME` instead. `AgentIntegrationPaths`
claims install and status "never consult the process environment while choosing
files"; the cwd is process environment. Fix: one rule for every override (refuse
relative, or join `HOME`), in `config_dir_from_env_or_home`.

## BUG-044 - Stale hook registrations for an old hook path are never removed

Reported by: integrations.

Removal matches only commands for the current `hook_path`. If `HOME`, a
`*_CONFIG_DIR` / `*_HOME` override, or the symlink spelling of the home directory
changes between launches, the old entries stay registered and keep running the
old hook file, which is never updated again. An agent config shared across hosts
through a dotfiles symlink, where the hosts' home paths differ, gets one entry per
host, and on each other host that entry runs `sh '<missing path>'`, a failing hook
the agent may show.

## BUG-049 - A launched pane's cwd change does not advance the shell projection

Reported by: workspace-model.

`StateEvent::TerminalCwdReported` sets the cwd only when it differs and marks
both the session and the shell projection dirty. `handle_pane_launch_settled`
(`LaunchOutcome::Launched`) calls `set_cwd(cwd)` directly through `pub(super)`
fields, unconditionally, marks the session dirty, and does not advance the
projection revision. The launched cwd differs from the requested one whenever
the child's chdir fell back to `HOME` / passwd home / `/`, so the projected pane
cwd is stale until the 1 s `SHELL_CWD_REFRESH_INTERVAL` rebuild. The comment in
`apply_runtime_state_event` ("the projection revision is what says the cwd
moved") holds for only one writer. Fix: route the launch cwd through
`StateEvent::TerminalCwdReported`. See POL (projection invalidation has no
owner).

## BUG-050 - Lone-pane zoom has two contradicting policies

Reported by: workspace-model.

`AppState::toggle_pane_zoom` documents that a one-pane workspace's toggle
"changes nothing, though the pane is still focused" and implements that branch;
`handle_pane_zoom`, its only production caller, short-circuits a lone pane first
and documents "does nothing at all (no focus, no navigation)". The reducer's
lone-pane branch and its `set_zoomed` refusal are unreachable from production.
Fix: one rule in the reducer; delete the endpoint's copy.

## BUG-051 - A slow or unresponsive remote server is reported as needing an SSH login

Reported by: server-lifecycle, remote.

`src/limits.rs` `STATUS_ANSWER_TIMEOUT` is 20 s; the remote `shepr status --json`
and `status server --json` wait that long for `ping`, and `status --json` up to
another 20 s for `server.summary`. They run over SSH through
`RemoteSsh::sh_output`, whose per-command cap is `SSH_COMMAND_TIMEOUT` (15 s).
A command that uses its full budget is marked `authentication_candidate` and
classified `AuthenticationPending`. So:

- `status --all` prints "needs an SSH login: run `shepr` or ssh to it" for a
  remote server that listens but does not answer; AGENTS.md promises "not
  answering".
- `stop --all`'s explicit `Unresponsive` arm is unreachable for the case it was
  written for.
- `parse_remote_server_status_json`'s "a server that listens but does not answer
  fails the check" never happens; in preflight such a result gets the foreground
  interactive ssh attempt, so a wedged remote server triggers a login prompt.
- A remote server of this build whose app loop stalls answers `ping` at once
  (connection thread) but `server.summary` waits the server's 15 s
  `ORDINARY_REQUEST_TIMEOUT`, so `status --json` exceeds 15 s.

At runtime (remote) `MachineState::after_failure` maps
`PossibleAuthentication` to `NeedsLogin` too, so a host that merely stalls a
full round trip (overloaded sshd, kex stall) is shown as needing a login,
although no prompt can run after startup.

Fix: one owner for "how long a status probe waits" (launch's
`STATUS_REQUEST_TIMEOUT`, 2 s, is the right order); derive the remote budgets
from it; give `summary` a short bound or skip it after a slow ping; at runtime,
say what is known (no answer within the round trip). Enforce with a `const`
assert in `shepr-remote/src/limits.rs` that the remote status command's worst
case is below `SSH_COMMAND_TIMEOUT`. See VAL (four budgets for one ping).

## BUG-052 - A dead API listener leaves a deaf server that defeats every launcher

Reported by: server-lifecycle.

`shepr-api/src/server.rs` `start_server`: the listener "must outlive every accept
and spawn failure". `listener.rs` breaks out of the accept loop on
`Accepted::Fatal` (EBADF, EINVAL, ENOTSOCK), logs one `error!` and exits. The
refuser's channel ends, the request senders drop, and
`HeadlessServer::next_loop_event` logs "API request channel closed; API requests
are no longer served" and keeps running. Nothing initiates shutdown. The socket
file stays but nothing accepts, so `socket_is_live` reads stale (`Gone`);
`shepr stop` reports "not running"; a new `shepr` starts a daemon that exits
`AlreadyRunning` on the lease every 500 ms until `SERVER_READY_TIMEOUT`, ending in
a misleading message. Only a signal by pid recovers, and the pid is only in the
server log.

Fix: listener death and API channel closure latch the stop signal so the server
saves and exits in the documented order. Enforce with a test that closes the
listener fd and asserts the server reaches `ShutdownPhase::Stopping`.

## BUG-056 - The CLI's response reader never checks the response id

Reported by: server-lifecycle.

`shepr-api/src/client.rs` `read_json_line` decodes whatever line arrives, and
`request_value*` never compare the response id with the request's. One request
per connection makes a mismatch unlikely, but the test fakes show nothing checks
it: `local_server_tests.rs` `serve_pong_once` answers with
`"autodetect:server:status"`, an id no production code sends, and every test
passes. Fix: a typed request/response pairing that refuses a mismatch as
`InvalidData`; a test with a wrong id then fails.

## BUG-057 - `server.stop_if_boot` alone among the stop requests accepts unknown keys

Reported by: server-lifecycle.

In `schema/server.rs`, `ServerStopParams` and `ServerSummaryParams` carry
`deny_unknown_fields`; `ServerStopIfBootParams` and `PingParams` do not. The
`Request` decoder's comment and `api_service.rs` `stop_server`'s comment both
claim a stop is conditional "only when its one guard is where this method reads
it", which rests on refusing stray keys. A `server.stop_if_boot` with a
misspelled extra key beside a correct `expected_boot_id` decodes silently. It is
the one cross-build request. Fix: `deny_unknown_fields` on every socket-route
params type; a schema test that sends an extra key to each enforces it.

## BUG-058 - An override equal to the runtime socket is recognised only by lexical path equality

Reported by: server-lifecycle.

`ServerAddress::for_runtime_dir` treats an override equal to the runtime socket
as no override by `Path` equality. A pane-exported socket reaching the same file
through a symlinked `XDG_RUNTIME_DIR` (or the logind fallback vs an exported
`/run/user/<uid>` spelled differently) reads as an override: `status` prints "set
by SHEPR_SOCKET_PATH", the TUI will not start a server there, and the restart
offer is skipped. Fix: compare canonical paths, or the inode once the socket
exists.

## BUG-060 - The client never heartbeats the local server, though the heartbeat module says it probes every endpoint

Reported by: remote.

`shepr-launch/src/connection_health.rs`: "The client probes every endpoint, local
or SSH, after `HEARTBEAT_INTERVAL` of silence". `EndpointPolicy::uses_ssh_heartbeat`
is `Machine` only, `EndpointRegistry::insert_with_activity` gives Local
`health: None`, and two tests pin that Local is never probed. A Local server that
is alive but wedged (SIGSTOPped, deadlocked loop) is never detected; the client
waits on a silent socket forever. Fix the doc, or (better, since a wedged server
is what a heartbeat is for) probe Local too and delete `uses_ssh_heartbeat`.

## BUG-062 - Connect and Restart attempts are budgeted as if no server had to start

Reported by: remote, server-lifecycle.

`ConnectMode::Start` runs under `ATTEMPT_BUDGET` (`SSH_COMMAND_TIMEOUT +
SSH_ATTEMPT_SLACK`, 25 s, discovery included) and Restart under that plus
`REMOTE_STOP_SSH_TIMEOUT`. Neither contains the remote launch the mode exists
for: the remote bridge's `ensure_running` may legitimately spend
`SERVER_READY_TIMEOUT` (15 s) for the daemon, plus up to `SERVER_READY_TIMEOUT +
LAUNCH_LOCK_WAIT_GRACE` (20 s) waiting for the launch lock, plus transition waits
and the session restore, before it relays a byte, while the client's handshake
read stops at the attempt deadline. On a cold master or slow restore, Connect ends
`TimedOut`, `MachineState::after_failure` maps it to Offline (Starting... flips to
Offline), the request is consumed, and only a later automatic attach finds the
server. `SSH_RESTART_ATTEMPT_BUDGET`'s doc asserts the start fits; nothing checks
it.

Fix: a start budget in `shepr-remote/src/limits.rs` built from
`SERVER_READY_TIMEOUT` and the lock grace (or hand the remaining attempt budget
down to the bridge launch), used for Start and added to Restart, with a `const`
assert.

## BUG-065 - shepr reads the user's ssh config from `$HOME` while OpenSSH resolves keys from the passwd home

Reported by: remote.

`ssh_paths.rs` `remote_ssh_config_paths(app_paths.home_dir())` includes
`$HOME/.ssh/config`; because shepr passes `-F`, OpenSSH no longer reads its own
default, and its `~` expansion for `IdentityFile`, `UserKnownHostsFile` and so on
uses `pw_dir`. With `HOME` differing from the passwd home (sudo -E, a leaked test
env), shepr's ssh reads config from one home and keys and known hosts from
another. Low impact; pick one home and say which.

## BUG-066 - The remote bridge download busy-polls and its stdout join is unbounded

Reported by: remote.

`copy_reader_to_local_stream` sleeps `BRIDGE_IO_POLL` (1 ms) on every
`WouldBlock` of the nonblocking local stream, so a stalled client reader makes
the thread wake 1000 times a second for as long as it lasts; the upload side
already uses `StreamWake`. After ssh exits, `bridge_connection` joins the
download thread, which blocks on child stdout EOF with no grace, while stderr gets
`PIPE_DRAIN_GRACE`; if that grace's premise holds for stderr, stdout here is the
same hang risk, and the bridge's `Drop` joins this thread. Fix: poll for
writability, and bound the stdout join the same way.

## BUG-067 - A machine refusing authentication is retried every 30 seconds for the client's life

Reported by: remote.

A machine in `NeedsLogin` (Attention) is retried every `ATTENTION_RETRY_DELAY`
(30 s) for the life of the client with a BatchMode ssh that offers every key and
is refused each time. On a host with fail2ban or `MaxAuthTries` accounting this
can ban the client's address, turning an auth problem into Offline for every
client on it. Consider not retrying authentication refusals automatically (only
on operator action or a key-agent change), or a much longer interval.

## BUG-068 - Several launch-status connections arriving before registration overwrite one parked channel

Reported by: the launch-status router fix.

In `shepr-pty/src/launch.rs`, a connection for a ticket whose launch has not
registered yet is parked, but only one channel is kept per ticket: a second
connection before registration replaces the first. If the first was the real
child and the second a stray (or the reverse), the launch either waits on the
wrong channel or loses the real report. Fix: park per (ticket, pid), or refuse a
second parked connection for a ticket, and test it on the router seam that now
exists.

## BUG-069 - A pane spawn racing the launch listener's failure loses its delivery

Reported by: the launch-status router fix.

A fatal listener failure now poisons the launch service, so later spawns fail
loudly. A spawn that registered just before the failure still has its delivery
dropped, and `settle()` returns `Unconfirmed`, which the mux treats as "the
pane's death follows" while the child may be alive (the limbo of BUG-017). Fix: a delivery dropped by listener
failure settles as a distinct failed outcome the mux can act on.

## BUG-070 - `detect capture` and `detect explain` turn a failed screen read into an empty screen

Reported by: the detection read-failure fix.

The detector tick now treats a poisoned core or a `ReadError` as no observation.
The on-demand path does not: `pane/runtime/read.rs` `PaneRead` still supplies an
empty default on read failure, so `detect capture` prints empty text and
`detect explain` evaluates the manifest over it, presenting fabricated empty
evidence as the pane's screen. Fix: the read returns the failure and the API
answers with an error naming it. Testable with a poisoned core.

## BUG-071 - The first Kimi config update moves the blank lines after shepr's block in front of it

Reported by: the wave review.

Removal of shepr's Kimi block now preserves every byte outside it, but the update
re-appends the block at the end, so blank lines that followed the block end up
before it: the user's file changes on the first update (idempotent afterwards). The
re-appended block also uses LF line endings in a CRLF file (valid TOML, and status
compares independent of line endings). Fix: replace the block in place.
