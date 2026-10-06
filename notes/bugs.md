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

## BUG-028 - Resume de-duplication misses one session saved as an id in one pane and a path in another

Reported by: restore-resume.

`AgentResumeKey` is the whole `PersistedAgentSession`, so for Pi and OMP
(`SessionRefPolicy::IdOrPath`) the same session saved as an id in one pane and as
a path in another is not detected as a duplicate, and both panes resume it. Rare,
since a report prefers the path when both are present.

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

## BUG-065 - shepr reads the user's ssh config from `$HOME` while OpenSSH resolves keys from the passwd home

Reported by: remote.

`ssh_paths.rs` `remote_ssh_config_paths(app_paths.home_dir())` includes
`$HOME/.ssh/config`; because shepr passes `-F`, OpenSSH no longer reads its own
default, and its `~` expansion for `IdentityFile`, `UserKnownHostsFile` and so on
uses `pw_dir`. With `HOME` differing from the passwd home (sudo -E, a leaked test
env), shepr's ssh reads config from one home and keys and known hosts from
another. Low impact; pick one home and say which.
