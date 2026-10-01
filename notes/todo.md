# Later

Recurring chores and checks that wait for the situation to come up, not
defects to hunt.

## Review upstream changes

`scripts/upstream_watch.py` reports upstream herdr changes to the integration
assets and their tests, the detection manifests (bundled and published), the
manifest tooling and the detection and hook wiring since the baseline in
`scripts/upstream_baseline.txt` (the fork was 21d0ce6; the baseline advances
as upstream changes are ported or judged irrelevant). Run it periodically;
its docstring lists what it watches. Test coverage of the agent plugins grows by
porting upstream's tests, not by writing our own.

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `crates/shepr-agent/src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr detect capture <pane>`, and compare the dialog's labels with the gate. Fix the manifests if they differ.
- At the same time, check the Kilo plugin's `ownsLocalLifecycle` gate (`crates/shepr-agent/src/integration/assets/kilo/shepr-agent-state.js`): it assumes `process.argv.slice(2)` holds Kilo's own arguments and excludes the subcommands `acp`, `attach`, `console`, `daemon`, `serve` and `web`, names not verified against the Kilo CLI.

# Possible capabilities

Proposals that arrived as defects but would widen what shepr claims. None is
promised anywhere; each waits for the owner to want it.

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

