# Later

Recurring chores and checks that wait for the situation to come up.

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

# Gaps and smells

Not defects: paths with no test, and code that works but reads worse than it
should.

- Client shell tests still seed copy-search prompts and copy sessions directly, and some component tests write `config.copy_on_select`, `config.spaces.row_gap`, navigator fields, `mode.set` and the palette, so `ClientCopySearchPrompt` and the copy session fields stay shell-wide. Driving those through copy-mode keys and the config constructor would let them narrow.
- `terminal_collect_dirty_patch` builds one owned `String` per dirty cell for the wire's `CellData` symbol (plus padding). Not contention on glibc; worth checking if a many-pane, all-redrawing profile ever shows the allocator in the render path.

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

## Report a missing hook interpreter

Every shell hook asset exits silently when `python3` is missing, so on such a
host every integration installs as current and never reports; its panes read
as idle agents. A status signal ("hook interpreter missing") would make that
visible. Nothing claims hooks work without `python3` or that a broken hook is
reported.

## Faster startup with unreachable machines

Preflight blocks the TUI until every check of a round finishes, up to
`PREFLIGHT_CHECK_BUDGET`, and a second round follows any successful prompt. A
blackholed host (no RST) costs the ssh `ConnectTimeout` at every launch, so
"fail soft" still means a slow start. This is the documented phase bound; a
shorter path (show the TUI first and finish checks behind it, or remember a
recently dead host) would be new behaviour.
