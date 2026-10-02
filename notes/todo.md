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
stays frozen. Bound it: once the grace has elapsed, clear a cancelled marker and
apply its deferred withdrawal without the release. Related, smaller: during the
window the dying agent's late hook reports are admitted as live because process
evidence stays available, so the sidebar can briefly show them before
confirmation clears the authority; and `DetectionTask::provisional_release` is
not cleared on a detector reset (harmless, since the terminal ignores a
confirmation for an exit it already resolved).

## Pane spawn does blocking filesystem work on the server event loop

Formerly TCORE-004, filed as a design observation rather than a broken claim.
`PtyCommand::to_std_command` stats the requested cwd (`usable_directory`) and
walks `PATH` (`resolve_executable` with `classify_candidate`, a `stat` plus
`access` per candidate) on the calling thread, and the parent's
`Command::spawn` waits for the child's chdir and exec. Every pane spawn and
restore runs this on the server's event loop, so a hung mount in the cwd,
`HOME` or any `PATH` entry stalls the whole server, not just resume. This is
wider than the note in `agent_resume.rs` acknowledges.

## A reconnected connection's first surface can meet the old snapshot

`PaneSurfaces` in `crates/shepr-client/src/shell/presentation/surfaces.rs`
drops its baseline when the endpoint's connection generation changes from one
connection to another. `receive_pane_surface` is not told which connection
generation a surface came from. If a reconnected connection's first full
surface reached the shell before its first snapshot, it could pair with the
old connection's snapshot when their projection revisions happen to match, or
else be dropped by `lose_baseline`, so the next patch is
`Rejected(NoBaseline)` and the connection is torn down and reconnected. Today
the server writes the snapshot before the surface, so neither happens; the
code before the request-ledger rewrite relied on the same ordering. Fix: pass
the message's connection generation into `receive_pane_surface` and drop a
surface from any generation but the current one.

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

