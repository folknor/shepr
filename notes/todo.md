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

## A palette preview tool

Write a small dev-only CLI bin that shows the colours the client derives
(`shepr_term::host_tint::UiPalette` and `HostPillPalette`), so the derivation
can be judged by eye instead of only by its contrast tests. It should query
the running terminal's background, foreground and ANSI colours (or take them
as arguments, so other themes can be tried without switching terminals), then
print every palette token and every host hue as swatches with sample text on
the surfaces they are used on: panel, active and selected rows, the sidebar
tints and accents, the state colours, and a focused and unfocused pane border.
The contrast targets in `shepr-term/src/limits.rs` (`UI_*_CONTRAST`) are first
guesses waiting on exactly this check.

# Gaps and smells

Not defects: paths with no test, and code that works but reads worse than it
should.

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

## Report a missing hook interpreter

Every shell hook asset exits silently when `python3` is missing, so on such a
host every integration installs as current and never reports; its panes read
as idle agents. A status signal ("hook interpreter missing") would make that
visible. Nothing claims hooks work without `python3` or that a broken hook is
reported.

## Welcome panel on boot

When the TUI starts, the pane side shows a "welcome to shepr" panel in place of
the panes, whatever state the servers are in, with a short getting-started
guide: the prefix key and the few bindings that matter (new workspace, split,
navigate mode, detach, help), how machines appear in the sidebar and what
Connect does, and where `shepr man` and the config live. A key or click
dismisses it and shows the panes.

## Clear shell prompts on reflow (OSC 133)

A width change reflows a pane's grid, and a shell that redraws its prompt on
SIGWINCH (zsh with a right prompt, for one) then redraws at the wrong row,
leaving old prompt fragments behind. Debouncing PTY resizes cuts the number of
redraws, but any single width change can still leave one. Terminals such as
kitty fix this with shell integration: the shell marks its prompt with OSC 133,
and on reflow the terminal clears from the prompt start so the redraw lands
clean. shepr would need to track OSC 133 marks per pane in shepr-vt and clear
the marked prompt region on a width change, and the user's shell would need the
integration that emits the marks.

## Faster startup with unreachable machines

Preflight blocks the TUI until every check of a round finishes, up to
`PREFLIGHT_CHECK_BUDGET`, and a second round follows any successful prompt. A
blackholed host (no RST) costs the ssh `ConnectTimeout` at every launch, so
"fail soft" still means a slow start. This is the documented phase bound; a
shorter path (show the TUI first and finish checks behind it, or remember a
recently dead host) would be new behaviour.
