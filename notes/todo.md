# Later

Recurring chores and checks that wait for the situation to come up.

## Decide what stays of the sidebar layout settings

`[ui.sidebar.agents]` and `[ui.sidebar.spaces]` let `client.toml` choose each
sidebar entry's lines from tokens, per agent (`rows_by_agent`), with inline
token styles and value rules that restyle or hide a token. It is roughly 1,900
lines: parsing and validation in `shepr-config` (`sidebar.rs`,
`sidebar/rules.rs`) and rendering in the client (`sidebar_tokens.rs`,
`token_definitions.rs`). Deferred while other settings were removed; decide
whether any of it stays configurable.

# Gaps and smells

Not defects: paths with no test, and code that works but reads worse than it
should.

## Manifest rules that gate on words where their comments promise dialog controls

AGENTS.md asks for invariant controls as explicit AND/OR gates; these match
loose words instead. Look for an upstream herdr fix first
(`scripts/upstream_watch.py`); a local change needs captures of the agent to
test against.

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

# Possible capabilities

Proposals that arrived as defects but would widen what shepr claims. None is
promised anywhere; each waits for the owner to want it.

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

## Show progress while a machine starts

An operator Connect or Restart re-verifies the remote executable, may fall
back to discovery, and then launches the server, so against a slow or hung host
the machine row can sit on Starting... (or Stopping...) for minutes; the
budgets in `crates/shepr-remote/src/limits.rs` are each exactly the work they
bound. The entry could say which phase it is in (checking the install,
discovering, starting the server) so a long wait reads as progress rather than
a hang.

## Home view of every pane, or collapsible panes

The aim: one place to keep an eye on every pane without stepping through the
workspaces. There are two candidate shapes. Pick one, or neither, before
designing.

### Option 1: a Home (or All) entry

A synthetic entry at the top of the sidebar's workspace list. Selecting it
shows every pane from every workspace at once.

It is not a real workspace, so no pane can be opened in it. New-pane and split
are refused there with a short hint, or they open a picker that asks where the
pane should go. A picker cannot stop at a host, because a pane has to live in a
workspace, so it would really be a host-then-workspace picker. I lean toward
refusing, since switching to the workspace already does the job.

The layout is presumably an automatic grid, grouped by workspace with a small
header per group, in sidebar order. It would be computed, not saved, and not
editable: no splits, no resizing, no equalize.

Questions:

- **Which machines does it cover?**
  - One Home per machine is the cheaper version: the server can build the view
    itself.
  - One global Home across every connected host means the client composites
    panes from several servers into one frame. Today it draws one server's
    surface at a time.
- **What size does each terminal get?**
  - A pane has exactly one terminal size, taken from its own workspace's
    layout, and a Home tile is much smaller. There are three options:
    - Resize the terminals while Home is shown. Agents redraw small, and
      redraw again on the way back.
    - Keep the real size and show a clipped or downscaled view. This is
      enough to see state.
    - Make Home a list of pane cards (title, agent, state, the last few lines)
      instead of live terminals.
  - I lean toward keeping the real size, because it never disturbs the agents.
  - Wanting to type into panes from Home would change that.
- **Watching only, or working too?**
  - If you can focus a pane and type in Home, it needs focus, navigation and a
    zoom.
  - If it is only for watching, Enter on a pane jumps to that pane in its real
    workspace. That is simpler and fits "oversee, not drive".
- **Smaller questions:**
  - Does it show shell-only panes, or only agent panes?
  - Does closing a pane from Home close it in its workspace?
  - Is "Home is selected" remembered across restarts?

### Option 2: collapsible panes

The alternative is to keep the workspaces and let a pane collapse inside its
layout. A collapsed pane is a one-row bar showing its title, agent and state,
and its siblings take the freed space. Many agents then fit in one workspace,
and the panes that matter stay full size.

This is only a sketch so far. The owner named the idea but not its details.

Questions:

- **What does a collapsed pane look like?**
  - A one-row bar in place, the same as a minimized split.
  - Or does it leave the layout entirely and live in a strip or the sidebar?
- **Terminal size:**
  - Does the collapsed pane keep its PTY size while it is hidden, as a zoomed
    workspace's other panes do?
  - Or is it resized to the bar? I'd keep the size, since a 1-row resize would
    make every agent redraw.
- **Expanding:**
  - Expanding could be a key, a click on the bar, or automatic when the agent
    turns Blocked.
  - Should a Blocked agent's bar draw attention, as the sidebar does?
- **Split geometry:**
  - The split ratio has to give the bar exactly one row.
  - A collapse that empties one side of a split collapses that whole side.
- **Scope and state:**
  - Is collapse state saved with the session layout?
  - Is it shared by every client, as layout is, or per client, as focus is?
    Layout is per workspace and shared, so a per-client collapse would be a new
    kind of state.

## Faster startup with unreachable machines

Preflight blocks the TUI until every check of a round finishes, up to
`PREFLIGHT_CHECK_BUDGET`, and a second round follows any successful prompt. A
blackholed host (no RST) costs the ssh `ConnectTimeout` at every launch, so
"fail soft" still means a slow start. This is the documented phase bound; a
shorter path (show the TUI first and finish checks behind it, or remember a
recently dead host) would be new behaviour.
