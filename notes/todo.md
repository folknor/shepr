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

