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

## BUG-014 - A failed final save is retried never and reported to no operator command

Reported by: save-shutdown, persistence.

The final save now has its own must-use outcome, logs the data directory and the
error without retry wording, keeps the join error as source, and makes `run()`
exit with failure. What remains: the final save has no retry at all, unlike
checkpoints, so a transient EIO on the last save loses everything since the last
autosave; and `shepr stop` reports only that the server went away, so the
operator who stopped it learns of a failed final save only from the server log.
Consider giving the final save the checkpoint's retries, and carrying its
outcome to the stopping client (or the stop's exit status).

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

## BUG-060 - The client never heartbeats the local server, though the heartbeat module says it probes every endpoint

Reported by: remote.

`shepr-launch/src/connection_health.rs`: "The client probes every endpoint, local
or SSH, after `HEARTBEAT_INTERVAL` of silence". `EndpointPolicy::uses_ssh_heartbeat`
is `Machine` only, `EndpointRegistry::insert_with_activity` gives Local
`health: None`, and two tests pin that Local is never probed. A Local server that
is alive but wedged (SIGSTOPped, deadlocked loop) is never detected; the client
waits on a silent socket forever. Fix the doc, or (better, since a wedged server
is what a heartbeat is for) probe Local too and delete `uses_ssh_heartbeat`.

## BUG-065 - shepr reads the user's ssh config from `$HOME` while OpenSSH resolves keys from the passwd home

Reported by: remote.

`ssh_paths.rs` `remote_ssh_config_paths(app_paths.home_dir())` includes
`$HOME/.ssh/config`; because shepr passes `-F`, OpenSSH no longer reads its own
default, and its `~` expansion for `IdentityFile`, `UserKnownHostsFile` and so on
uses `pw_dir`. With `HOME` differing from the passwd home (sudo -E, a leaked test
env), shepr's ssh reads config from one home and keys and known hosts from
another. Low impact; pick one home and say which.

## BUG-066 - The remote bridge download busy-polls a stalled local reader

Reported by: remote.

`copy_reader_to_local_stream` sleeps `BRIDGE_IO_POLL` (1 ms) on every
`WouldBlock` of the nonblocking local stream, so a stalled client reader makes
the thread wake 1000 times a second for as long as it lasts; the upload side
already uses `StreamWake`, which waits only for readability. (The stdout join is
now bounded by `PIPE_DRAIN_GRACE`.) Fix: a cancellable writable-readiness wait in
`shepr-platform` beside `StreamWake`, used here; the constraint is commented at
the retry.

## BUG-067 - A machine refusing authentication is retried every 30 seconds for the client's life

Reported by: remote.

A machine in `NeedsLogin` (Attention) is retried every `ATTENTION_RETRY_DELAY`
(30 s) for the life of the client with a BatchMode ssh that offers every key and
is refused each time. On a host with fail2ban or `MaxAuthTries` accounting this
can ban the client's address, turning an auth problem into Offline for every
client on it. Consider not retrying authentication refusals automatically (only
on operator action or a key-agent change), or a much longer interval.

## BUG-072 - Restore silently repairs an out-of-range saved bookmark

Reported by: the restore damage fix.

Every other discard or repair of saved data now backs up the file and reports
it. `restore.rs` `remap_saved_index` still quietly repairs a saved bookmark
index that is out of range, so the first save rewrites a corrupt value with no
backup and no notice. Fix: count it as restore damage like the others.

## BUG-073 - A failed actor start whose teardown thread cannot spawn leaves the leader alive

Reported by: the wave review.

The inline teardown fallback and the extra SIGKILL on the actor-startup failure
path are gone (a pane never stalls the loop). When the actor fails to start and
the teardown thread also cannot spawn (thread exhaustion), the leader gets only
SIGHUP; a child ignoring SIGHUP lives on and its detached reaper waits forever.
Fix: without a teardown thread, send SIGKILL to the leader directly (one syscall,
no wait) before handing it to the reaper.

## BUG-074 - Two descriptor entries for one event with different matchers collapse into one hook group

Reported by: the wave review.

`ensure_command_hook`'s duplicate check keys on event plus command only, so two
descriptor entries for the same event that differ only in matcher would install
as one group and lose the second matcher. No current descriptor does this, so
latent. Fix: key the check on the matcher too, or refuse such a descriptor in a
test over every spec.
