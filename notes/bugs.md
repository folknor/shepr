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

## BUG-067 - A machine refusing authentication is retried every 30 seconds for the client's life

Reported by: remote.

A machine in `NeedsLogin` (Attention) is retried every `ATTENTION_RETRY_DELAY`
(30 s) for the life of the client with a BatchMode ssh that offers every key and
is refused each time. On a host with fail2ban or `MaxAuthTries` accounting this
can ban the client's address, turning an auth problem into Offline for every
client on it. Consider not retrying authentication refusals automatically (only
on operator action or a key-agent change), or a much longer interval.
