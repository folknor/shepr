# CLI surface reduction

Decisions from a review of the whole `shepr` command line (the clap model in
`src/cli/spec.rs` and `src/cli/spec/machine.rs`), with the reasoning behind
each one, so the design can be judged without the conversation that produced
it. The landing order is in `notes/cli-ux-spec.md`.

## Framing

shepr is for overseeing agents across machines, not for driving them
(AGENTS.md, Scope). The owner uses it through the TUI. The CLI grew out of
upstream herdr's scripting surface, and most of it exists so that scripts
can do what the TUI already does with keys. The test applied to every
command was: does the owner, or something inside shepr, actually invoke it?
If neither does, it goes.

Two principles already in AGENTS.md shaped several decisions:

- Config is read and validated once at launch, fails the launch on any
  problem, and is never reloaded.
- There is nothing on disk to stay compatible with, so stored formats
  (catalog, session state) can be dropped freely.

## Who calls the CLI from inside shepr

Checked before deciding, because a command something internal depends on
cannot just be deleted:

- Integration hooks mostly talk to the server socket directly with JSON
  (`pane.report_agent`, `pane.report_agent_session`). Only the letta,
  qodercli and qwen hooks shell out to `shepr pane report-agent-session`.
- No bundled hook calls `release_agent` or any `report_metadata` method,
  through the CLI or the API.
- Remote discovery runs `shepr status client` on the remote host over SSH to
  learn its build identity, and runs `remote-api-bridge --check`.
- Saved-machine connections run the hidden `remote-client-bridge` on the
  remote host. That bridge also starts the remote server daemon when it is
  not running.
- `remote-api-bridge` (and `SavedSshApiBridge`, `cached_remote_api_command`)
  serves only `--machine` CLI commands.
- Several user-facing messages name commands: reattach and bootstrap hints
  in `shepr-remote` build `shepr --remote <target> --session <name>`,
  `SessionId::attach_command` in `shepr-config` builds `shepr session attach
  <name>` (used by messages in `shepr-api`, `shepr-client` and
  `shepr-server`), and the client's machine diagnostics overlay suggests
  `shepr machine reconnect` or `shepr machine status`.

## Target surface

```
shepr                                   run the TUI (prompts for SSH auth first if a machine needs it)
shepr -V | --version
shepr -h | --help

shepr status [--json]                   local client + running server
shepr status server [--json]
shepr status client [--json]

shepr server                            run the headless server
shepr server stop [--force]

shepr detect capture <PANE>
shepr detect explain <PANE> [--json] [-v]
shepr detect explain --file <PATH> --agent <LABEL> [--json] [-v]

shepr integration install <TARGET>      (to become automatic, see below)
shepr integration uninstall <TARGET>
shepr integration status [--outdated-only]

hidden: client, remote-client-bridge
```

`status` and `server` have not been reviewed yet. `status client` stays
whatever happens to the rest, because remote discovery depends on it.

## Decisions

### Workspace, tab, pane, agent and terminal groups: removed

Removed: `workspace *`, `tab *`, `pane *`, `agent *` (except what becomes
`detect`) and `terminal *`.

- Create, focus, rename, close, split, resize, zoom, swap and move are
  scripting of things the TUI does with keys. The owner does not script
  shepr.
- `list` and `get` commands duplicate what the sidebar shows.
- `pane neighbor`, `edges`, `layout`, `process-info` and `current` only make
  sense for layout scripting.
- `terminal attach` and `agent attach` are a second client path (raw attach
  to one terminal, takeover, ctrl+b detach) next to the TUI, and lean
  towards driving rather than overseeing. `terminal title set/clear` is
  covered by the configured title template.
- `pane report-agent` and `pane release-agent` have no CLI caller.

With the groups gone, the `--pane`/`--current` selector, `--env`,
`--focus`/`--no-focus` and `--token` options disappear with them.

API methods, schema types and server handlers behind these commands go too
where nothing else uses them. The TUI may use some of the same methods; each
one is checked before removal. `pane.report_agent` and
`pane.report_agent_session` stay because the remaining hooks call them.

### Metadata reporting and release-agent: removed entirely

`pane report-metadata`, `workspace report-metadata` and `pane release-agent`
have no caller anywhere, CLI or API. The whole metadata feature goes with
them: tokens, TTLs, title and display-agent overrides, and their rendering
in the sidebar.

### letta, qodercli and qwen integrations: removed

These were the only hooks that shelled out to the CLI. The owner does not
need them. Their hook assets and `IntegrationTarget` entries go. Their
detection manifests stay, so they are still detected from the screen.

### Integrations: kept for now, to become automatic

Hooks are small scripts shepr installs into each agent's own config (for
Claude Code, the hooks section of `~/.claude/settings.json`). The agent runs
them on its own events and they report to the server socket of the pane they
run in. They report two things:

- State (working, blocked, idle) from the agent itself. Without it, shepr
  infers state from the screen using the detection manifests.
- Session identity: the agent's conversation id or transcript path. This is
  the only source of it; session references are set only from hook reports.
  Agent resume on restore depends on it (for example `claude --resume
  <id>`). Layout restore does not.

`install_target` is called only from `src/cli/integration.rs`, and the TUI
has no install action, so the commands cannot go until something else
installs the hooks. The plan is for the server to install or update the
hooks for configured agents at launch. The `integration` group is kept until
then.

### Agent read and explain: kept, renamed to `detect`

Detection manifests are the TOML files in
`crates/shepr-agent/src/detect/manifests/`, one per agent, compiled into the
binary. Each is a prioritized list of rules that match screen regions
against strings and regexes to decide working, blocked or idle. They matter
because:

- they are the only state signal for agents without an integration (amp,
  cline, gemini, kiro, maki, muse);
- they still matter for agents with hooks, covering missing hooks and
  dialogs the hooks do not report;
- they break whenever an agent changes its UI, so they need maintenance.

Maintaining them needs a capture of exactly what the detector saw, which only
the running server can produce, and a way to evaluate a manifest against a
capture. So the commands stay, renamed to say what they are for:

- `agent read <T> --source detection --format text` becomes `detect capture
  <PANE>`. The source is always detection and the format always plain text,
  so `--source`, `--format`, `--ansi` and `--lines` go.
- `agent explain` becomes `detect explain`, dropping `--format text|json`
  since it duplicates `--json`.
- Targets are pane ids. Agent-name targets belonged to the removed `agent`
  group.

AGENTS.md's instruction for capturing a pane changes to `shepr detect
capture <pane>`.

### Sessions: removed

A session is a whole separate server with its own sockets, saved layout and
history; config and the machine list are shared. The owner will not use
separate named sessions: workspaces and tabs already group work, and are
already one layer more than wanted.

Their only real job today is keeping a dev build apart from the installed
one (`brokkr run -- --session dev`). Instead, the runtime paths will depend
on the build profile: release builds use the default paths, dev builds their
own. `brokkr run` then works next to the installed server with no flags, and
AGENTS.md's `env -u ... --session dev` instructions go away.

Removed: `--session`, the `session` group (`attach` was a duplicate of
`--session` anyway), the session on saved machines, the session name
threaded through socket paths, bridges and messages, and `SessionId` with
its `attach_command` guidance.

### Direct remote attach and default config: removed

- `--remote` and `--remote-keybindings` are a second way to reach a remote
  host next to configured machines, with their own keybinding mode. Hints
  that point at them point at the startup authentication flow instead.
- `--default-config` goes.

### `--machine` and the API bridge: removed

With the API command groups gone, `--machine` only applies to `status`,
`status server` and `server stop`, and `ssh <host> shepr ...` does the
same. It is the only reason for `remote-api-bridge`, `SavedSshApiBridge`,
`cached_remote_api_command`, the stale-metadata retry in `src/cli/target.rs`
and the API-bridge check that discovery runs. All of it goes.

### `config check`: removed

Launch already validates config and fails hard, and there is no reload, so a
separate check command adds nothing.

### Machines: moved into config.toml, `machine` group removed

Today saved machines live in a catalog file in the state directory, separate
from config, with generated ids, a label, an SSH target and a session.
Clients watch the file and apply changes live, and the last selected machine
is remembered. `machine add` connects over SSH, locates the remote shepr
binary and checks its build, starts the remote daemon (asking to restart a
server that is not a detached daemon), and caches the binary's location.

New shape:

```toml
[[machines]]
label = "build"
ssh = "dev@build"
```

- Read and validated once at launch like the rest of config. Duplicate
  labels or a bad target fail the launch. Unreachable machines still fail
  soft at runtime.
- Labels are the identifiers. Generated ids, the catalog file, catalog
  watching and the remembered selection go. The client starts on the local
  host.
- The add-time preparation goes: the client bridge already starts the
  remote daemon and discovers the binary when nothing is cached, and the
  build check happens on connect. A running server that is not a detached
  daemon is reported as an error with instructions instead of a prompt.
- The cache of the remote binary's location stays, keyed by SSH target.
- `machine list` and `machine status` go; the sidebar shows each machine's
  state.

Authentication. The TUI runs ssh with `BatchMode=yes` and
`StrictHostKeyChecking=yes`, so any interactive prompt (password, key
passphrase not in ssh-agent, keyboard-interactive 2FA, FIDO PIN or touch)
fails the connection and the machine shows as needing authentication. Host
keys are never accepted by shepr; the host must already be in `known_hosts`.
shepr uses its own ControlMaster socket (passed with `-S`, from the managed
SSH config, `remote.manage_ssh_config`), so a plain `ssh <host>` by the user
does not help the TUI. `machine reconnect` exists to run an interactive ssh
on that socket.

Instead, at startup, before the client takes over the terminal:

1. check every configured machine in parallel, without prompting;
2. for each one that needs authentication, run the interactive ssh on
   shepr's control socket in the terminal, one machine at a time, saying
   which machine it is for;
3. start the TUI. Unreachable machines cost at most the connect timeout.

The shared connection lives while in use and for `ControlPersist=600` after.
If a network drop kills it mid-session on a host that needs a prompt, the
fix is restarting the client. A TUI action that suspends the screen and runs
the prompt can be added if that proves annoying; not before.

The machine diagnostics overlay's suggested commands change accordingly.

## Deferred and open

- `status` and `server`: not reviewed yet.
- Automatic integration install at server launch, after which the
  `integration` group can go.
- Workspaces plus tabs is one grouping layer too many. Flattening them is a
  separate, larger change (data model, persistence, sidebar, tab bar).

## Sequencing

Each step is one commit that passes `brokkr check`:

1. Remove the workspace, tab, pane, agent and terminal groups, `config
   check`, metadata reporting and release-agent, with the API methods,
   schema and handlers only they used.
2. Add `detect`; remove the letta, qodercli and qwen integrations.
3. Remove `--remote`, `--remote-keybindings`, `--default-config`,
   `--machine` and the API bridge.
4. Remove sessions; separate dev builds by build profile.
5. Move machines into config.toml, add startup authentication, remove the
   `machine` group and the catalog.

The steps share `src/cli.rs`, `src/cli/spec.rs` and the API schema, so they
run in order in one tree rather than in parallel.
