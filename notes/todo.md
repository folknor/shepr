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

## A dying agent's late session start can leave a ghost authority

During the provisional-exit grace (`AGENT_PROCESS_EXIT_RELEASE_GRACE`) process
evidence stays available, so `transition_start` admits a recognized session
replacement (Pi `New`, `Resume`, `Fork`) sent by the agent just before it died.
That moves the ownership epoch, the exit is voided, and the replacement
authority stays in charge with no process behind it.

Not fixable with the evidence shepr has, which was argued to a conclusion:

- A dying agent's late start and a genuine quick restart's start carry the
  same payload, receipt time and probe observations. Timestamps (the
  replacement process's start time against the hook's receipt) only rule
  emitters out; an older background agent or a delayed delivery passes them.
- Dropping every start received while an exit is pending trades the ghost for
  a worse regression: integrations that report their session only at startup
  (Claude, Copilot, Cursor, Droid, Grok) would lose a quick restart's resume
  identity for good, which today survives.
- Ancestry from the reporting socket's peer (`SO_PEERCRED`, then the ppid
  chain) is not attribution: hook reporters outlive or are reparented away
  from the agent, most agents run hooks through an extra shell (often in a new
  session), and pids can be reused before the walk.

What would close it: each report carrying a validated agent-runtime anchor
(pid plus `/proc` start time). In-process integrations (the Pi extension and
the other JS or TS plugins) can name themselves; each shell integration has to
prove how its runtime is identified or report unknown, and unknown must never
acquire ownership or cancel a release. The lifecycle would then key process
generations, pending exits and session selections on those anchors instead of
`ownership_epoch`.

# Gaps and smells

Not defects: paths with no test, and code that works but reads worse than it
should.

- A delivered `server.stop` cannot make a wedged server loop finish; forcing that would need its own mechanism and a decision about the final save.
- The client launch's own check for a helper-thread panic (`fatal.is_latched()` in `run_client_loop`, after the host helpers start) has no test: reaching it needs a real terminal. The loop's own latch checks are tested.

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
