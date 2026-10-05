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

## One colour system for hosts and themes

Two colour systems exist side by side and do not know about each other:

- Themes: `[theme]` (`name`, `accent`, `[theme.custom]` tokens) in both
  `client.toml` and `server.toml`. The client's theme colours what it draws
  (sidebar, overlays, toasts, mode bars); each server's own theme colours what
  it draws into pane cells (borders, gaps, scrollbars), from `ui/panes.rs` and
  `ui/scrollbar.rs`.
- Per-host palettes: `palette` on `[[machines]]` and `[local]` in
  `client.toml`, from which the client derives a tint, main and dim text and an
  accent per host against the terminal's reported colours
  (`shepr-term/src/host_tint.rs`, `shell/sidebar/host_colors.rs`). Only the
  sidebar uses them.

The expectation is that a host's palette also colours its pane chrome (the
focused border at least), and it does not: the server never sees the
client's palette, since config never crosses hosts. Decide how the two
systems become one: whether the client hands each server the colours to draw
its view in (per client, beside the other per-client presentation state) or
draws the chrome accent itself; what is left of `[theme]` on the server side;
and how the built-in themes relate to the derived per-host colours. The
per-colour overrides (`[theme.custom]` and `theme.accent`) are to be removed
as part of this.

## Shut down the fleet from one host

Stopping every server means running `shepr stop` on each host by hand. Add a
way to stop all of them from one place: the local server and every configured
machine's server, with the stop of each remote one going over SSH as the
conditional stop already does (`stop --expect-boot`). Decide where it lives:
the CLI is local-only by design (no command can be aimed at a configured
machine, and the CLI reads no `client.toml`), so either that rule gets an
exception or the action belongs in the TUI (a global menu entry with a
confirmation that says every host's pane processes end). Unreachable hosts
are reported, not waited on.

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
