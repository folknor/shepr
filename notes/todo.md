# Later

Recurring chores and checks that wait for the situation to come up.

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `crates/shepr-agent/src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr detect capture <pane>`, and compare the dialog's labels with the gate. Fix the manifests if they differ.
- At the same time, check the Kilo plugin's `ownsLocalLifecycle` gate (`crates/shepr-agent/src/integration/assets/kilo/shepr-agent-state.js`): it assumes `process.argv.slice(2)` holds Kilo's own arguments and excludes the subcommands `acp`, `attach`, `console`, `daemon`, `serve` and `web`, names not verified against the Kilo CLI.

## Decide whether to keep `pane_history`

`experimental.pane_history` (off by default) makes every session save also
write each pane's screen and scrollback to `session-history.json`, and a
restore replays that text above each fresh shell's prompt. Weigh what it buys
(seeing what a pane showed before a restart or reboot) against what it costs:
larger saves (scrollback can reach the default 10 MB budget per pane), screen
contents, possibly secrets, written to disk, and its code in shepr-mux
persistence, the server save and checkpoint paths, and
`spawn_with_initial_history`. Either keep it, and make it a plain
`server.toml` setting rather than an experimental one, or remove it along with
the history file and its restore path.

# Open defects

## A cancelled provisional process-exit release can freeze the detector

Formerly PRUN-020. `terminal/state/` in shepr-mux (`ProvisionalProcessExit`,
`source/detection.rs`). A detector process-exit release is held for
`AGENT_PROCESS_EXIT_RELEASE_GRACE` and cancelled by new process evidence or an
ownership change. A cancelled marker is cleared only by a later detector update
that names an agent. Until then every detector observation without an agent is
dropped, the deferred withdrawal is never applied, and a later exit or
confirmation is ignored. If ownership was replaced during the window (a custom
hook commit, say) and no agent process comes back, the pane's detector state
stays frozen.

Agreed design (sparred with codex to consensus):

- Replace the `cancelled` flag with an ownership epoch. `TerminalState` holds an
  `ownership_epoch: u64`, bumped in `apply_source_effect` whenever the authority
  identity (presence, source, agent label, session ref) or the persisted session
  slot differs after the effect from before it. That covers `Keep`, `Clear` and
  `Set` commits, `RestoreSession` (a commit) and the `ProcessObserved`
  authority install, and it catches ownership that changed away and back. The
  marker records the epoch at exit. `cancel_provisional_process_exit` goes away.
- The confirmation trigger is unchanged: the first post-grace observation that
  does not name a live agent confirms. An agent-naming non-exit observation
  still clears the marker as a replacement process. The detection task always
  wakes at the deadline and either republishes the release or publishes its own
  update, so every marker resolves at the first post-grace tick. Compare the
  epoch once, before any release effect can itself bump it.
- (a) Epoch unchanged: today's confirmed release. A real incoming or deferred
  withdrawal keeps today's arbitration through `transition_detection`. When
  neither exists, the detector fields are resolved directly (`detected_agent`
  `None`, fallback `Idle`, no visible blocker, `fallback_observed_at` the
  confirming tick) and the effective state recomputed, without ownership
  arbitration. A fabricated observation fed through `transition_detection` would
  clear a newer custom authority that the release deliberately preserved.
- (b) Epoch changed: the old exit is void. The marker is cleared with no release
  effects: no ownership slot, source generation, suppression, `ProcessExited` or
  sequence change. A process-exit republish is consumed; the newer of the
  deferred and incoming withdrawals goes through ordinary `transition_detection`
  arbitration against the new owner. No synthesized withdrawal: keeping
  `detected_agent` for a same-agent full-lifecycle owner keeps a quick restart
  working. That is a new agent started inside the grace whose start hook landed
  before the probe saw its process; its reports need `process_present`.
- The mutation reported is the effective state and persistence identity before
  confirmation against after the whole operation (release, withdrawal and direct
  cleanup combined).
- Claim to state in comments: the voided exit performs no release effects,
  genuine withdrawals keep normal arbitration, and the provisional marker can no
  longer freeze detection. It does not promise freedom from the stale-evidence
  hazards below.

## Clearing hook authority can overwrite a newer persisted session

Found while sparring the provisional-exit fix; exists without it. The
authority-clear branch at the end of `transition_detection` (shepr-mux
`terminal/state/source/detection.rs`) commits `durable_session` built from the
authority. A sessionless authority clears the persisted slot to `None`. One that
names session A overwrites a persisted slot that was explicitly replaced with B.
`current_session_identity_for_persistence` prefers the authority, so this keeps
the effective identity, but nothing decides which of the two slots is newer when
they disagree. Needs an explicit ownership rule (or ordering stamp) for
authority versus persisted identity.

## A dying agent's delayed start can leave a ghost authority

Found while sparring the provisional-exit fix. During the provisional-exit grace
process evidence stays available, so `transition_start` admits a recognized
session replacement (Pi `New`, `Resume`, `Fork`) sent by the agent just before
it died. That bumps the ownership epoch, the exit is voided, and the replacement
authority stays in charge with no process behind it. Telling it apart from a
genuine quick restart needs process evidence the pane does not have at
confirmation time.

## A detector reset can re-report an exit

`DetectorState::reset` keeps the agent identity but clears the exit-report
bookkeeping, and `DetectionTask::provisional_release` survives the reset. A
later probe can publish a second exit for the same disappearance, which opens a
new provisional marker against the current ownership epoch. Replaying an exit is
not idempotent for hook-source bookkeeping: it can consume a pending start,
discard a pending report and clear ordering. Confirmation would need to identify
the exit generation it resolves.

## Pane spawn does blocking filesystem work on the server event loop

Formerly TCORE-004, filed as a design observation rather than a broken claim.
`PtyCommand::to_std_command` stats the requested cwd (`usable_directory`) and
walks `PATH` (`resolve_executable` with `classify_candidate`, a `stat` plus
`access` per candidate) on the calling thread, and the parent's
`Command::spawn` waits for the child's chdir and exec. When the cwd is unusable,
the `HOME` fallback (`home_dir`) stats `HOME`, then reads the passwd entry with
`getpwuid_r` and stats its home directory, also inline. Every pane spawn (split,
new pane, restore and agent resume) runs this on the server's event loop, so a
hung mount in the cwd, `HOME`, the passwd home or any `PATH` entry stalls the
whole server, not just resume. This is wider than the note in
`agent_resume.rs` acknowledges, which covers only a chdir hanging after the
worker's cwd check. Two more loop-side filesystem calls sit outside
`PtyCommand`: restore's `std::fs::metadata` on each saved cwd
(`persist/restore.rs`), and creation's `follow_cwd`, which builds a `UsableCwd`
(a stat) for the source pane's cwd.

Design, sparred with codex to consensus. The requirement is that the event loop never waits
for a launch that depends on the filesystem. Rejected along the way:

- A trampoline exec of `/proc/self/exe`: exec still reads the server binary and
  its ELF interpreter from disk, and an exit status cannot carry a launch
  failure.
- Off-loop `spawn_blocking` workers: the child forked inside a stuck
  `Command::spawn` has no owner, std loses the failure stage, and hung workers
  exhaust the shared blocking pool.

Agreed:

- shepr-pty owns the fork for pane children instead of std `Command`. On the
  loop, with no filesystem access:
  - open the PTY;
  - prebuild every byte the child needs: argv with the login argv0, one envp
    per cwd candidate since `PWD` differs, the cwd candidates as CStrings, and
    the validated absolute shell path (`PtyCommand` requires an absolute program;
    config validation already resolves it, so there is no `PATH` walk);
  - block signals on the calling thread, `_Fork` (not `fork`, which runs atfork
    handlers), and restore the mask in the parent.

  The parent gets the pid at once. It closes its slave copy and opens the pidfd.
  If `pidfd_open` fails, the launch is killed and rejected.
- The child path is a function returning `!`. It makes only async-signal-safe
  calls: no allocation, no drop, no panic, errno captured right after each
  failing call, `_exit` on failure. Order:
  1. reset dispositions with signals still blocked;
  2. normalize descriptors above 2, then dup2 the slave to 0-2;
  3. close every other fd with `close_range(.., 0)` (not close-on-exec marking), so a
     child stuck in chdir holds no lease, socket or other pane's fd;
  4. `setsid`, then `TIOCSCTTY` on fd 0, then the empty mask;
  5. chdir through the candidates: Required is the requested directory only;
     Fallback is requested, `HOME` from the launch env, the passwd home resolved
     once at server start, then `/`;
  6. `execve` with the envp of the selected candidate.

  No stat anywhere: chdir itself is the classification. `ENOENT` or `ENOTDIR` at
  chdir means directory unavailable, any other chdir errno means unreadable, and
  an exec errno means the shell failed to start.
- The runtime is assembled synchronously on the loop as today: terminal, IO
  actor, child watcher and detection task. The pid exists from the start, so
  close-before-launch and shutdown use the normal pane teardown. A child stuck in
  chdir is the server's own child: SIGKILL kills it when the syscall returns,
  before any exec. An owned raw-child type (pidfd wait with a `waitpid`
  fallback) replaces `std::process::Child` and is acquired before any fallible
  assembly step.
- A single coordinator task per launch owns settlement and outlives pane removal.
  It publishes the settlement with an awaited send, never `try_send`. The child
  watcher waits for that settlement to be published before it reports
  `PaneDied`; reaping does not wait on it.
- Observation is gated on a shared atomic launch state, with the owned pid kept
  separate from the pid that may be observed. Gated: detection, cwd
  publication, the runtime cwd accessors, save cwd probes, workspace cwd
  selection, theme probes. Carried history wins over the live terminal until
  launch succeeds.
- A failed launch becomes the existing runtimeless placeholder with a
  stage-specific `RestoreFailure` and its carried history, instead of the pane
  being removed. Restore stops calling `metadata`; restored shells launch with
  the Required policy. `follow_cwd` stops stat-ing on the loop and hands the
  unvalidated path to the Fallback chain.
- Agent resume keeps the plan and command in loop memory, writes the command
  and records Launched only when the launch settles as successful, and maps
  failures to `DirectoryUnavailable`, `DirectoryUnreadable` or
  `ShellStartFailed`. The worker cwd check goes away. A terminal whose launch is
  unsettled is not eligible for resume. Per-agent spacing is taken at dispatch.

How the parent learns that exec happened (agreed). A status pipe created in the
parent can be inherited by a foreign std-spawned helper (git hung in its own
chdir, which std performs before `pre_exec`) and delay EOF indefinitely. PTY
output proves nothing, because the tty echoes input. So the child creates its
status channel after fork:

- At startup the server binds one listening `SOCK_SEQPACKET` socket by abstract
  autobind, with no filesystem path, and keeps the exact binary address length
  from `getsockname`.
- The child connects to it after closing its fds and before any filesystem
  step. Its first packet carries the launch ticket, and it then sends
  `ChdirOk(index)` or a `Chdir`/`Exec` errno record.
- An always-running accept task routes connections by ticket together with the
  `SO_PEERCRED` pid and uid, one connection per ticket. Unregistered
  connections park for a bounded time, and late connections for retired tickets
  are dropped. Packet sizes and the record sequence are validated; a reset or
  read error is a transport failure.
- `ChdirOk` followed by EOF while the child is alive means ExecCommitted: the
  kernel closes close-on-exec fds after exec's point of no return, but before the new
  image is fully mapped. The resume command and ordinary death handling use this
  boundary. It is not a promise that the shell image already runs.
- EOF after exit, or exit before EOF, is an ordinary pane death. Setup failures
  before the connect are not reported by stage; the coordinator finishes a
  launch that never connects once the child has exited.
- The coordinator's settled result is durable shared state, separate from its
  publication to the app. History capture and observation gates read the shared
  result, not app bookkeeping, because the shell can print before the app hears
  of the settlement.

# Possible capabilities

Proposals that arrived as defects but would widen what shepr claims. None is
promised anywhere; each waits for the owner to want it.

## Name the right file for a misplaced config setting

A setting put in the wrong one of `client.toml` and `server.toml` fails the
launch as an unknown key ("unknown config key ui.window_title (.../client.toml)").
For keys that are valid in the other file, the error could say so and name it.

## Keep a corrupt pane-history file instead of overwriting it

`App::with_paths` loads pane history with `load_history`; a read or parse
failure is only a `warn!` in shepr-mux, the restore notice says nothing, and
`protect_unloaded` covers the session file but not the history file, so the
first save overwrites it. The restore notice is scoped to the session file, so
nothing promises otherwise. Pane history is the bulk of what a user wants back,
so a backup of the unreadable file and a line in the restore notice may be
worth having.

## Survive Kimi rewriting its own config.toml

`build_kimi_config_with_hooks` refuses a config with a top-level `hooks = []` or
`[hooks]` table (tested, deliberate). If Kimi Code ever writes such a default
itself, the integration fails on every launch until the file is hand-edited. If
Kimi rewrites `config.toml` through a TOML serializer, the
`# >>> shepr kimi integration` marker comments are lost and a later reinstall
appends a second set of `[[hooks]]` beside the unmarked first, so each event
fires twice. Nothing does this today; act if Kimi starts to.

## Faster startup with unreachable machines

Preflight blocks the TUI until every check of a round finishes, up to
`PREFLIGHT_CHECK_BUDGET`, and a second round follows any successful prompt. A
blackholed host (no RST) costs the ssh `ConnectTimeout` at every launch, so
"fail soft" still means a slow start. This is the documented phase bound; a
shorter path (show the TUI first and finish checks behind it, or remember a
recently dead host) would be new behaviour.

