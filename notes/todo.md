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

## Idle timer token for agent rows

An agent sidebar token (say `idle_time`) that counts up from when an agent last
became idle, usable in `[ui.sidebar.agents]` rows like any other text token
("4m", "1h12m"), and absent while the agent is working or blocked. The
projection carries only a `state_change_seq` order today, no time, so the server
would record when each agent entered Idle and send it. An elapsed duration at
projection time is safer than a wall-clock instant, since machines' clocks
differ; the client then ticks it forward locally. The sidebar needs a repaint
cadence for it (once a minute is enough at minute granularity), which nothing
schedules now. Open: whether Unknown, which presents as Idle, starts the timer,
and whether a server restart resets it.

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

## Overview of every agent across machines

Still being designed; nothing here is decided until the owner says so.

### The problem

The owner runs 3 to 6 agents at a time, spread over several machines, often
only one or two per machine. Workspaces with splits work fine for one machine.
What is missing is one place to watch all of them. A terminal cannot shrink one
pane's font, so a grid of six panes gives each one 60 to 90 columns, and diffs
and agent output wrap until they are unreadable. Width is what matters; height
matters much less, because agent TUIs draw their useful content (prompt,
latest hunk, status) at the bottom of the screen.

Replacing the TUI with a desktop GUI was considered and dropped: running in a
terminal is fine, the problem is the layout.

### The shape so far

- An **Overview** entry in the sidebar, above the machines list, showing the
  panes of every connected machine at once.
- It uses an **accordion** layout: panes stacked top to bottom, each at the
  full width of the content area. A collapsed pane is a one-row bar (title,
  agent, state, perhaps the last meaningful line). Expanded panes share the
  remaining height. Expanding changes only height, never width.
- A possible **peek** state between collapsed and expanded: the bottom N rows
  of the pane's real grid. Several agents can then be watched at once, full
  width.
- The Overview never resizes a PTY. It shows the pane's real grid,
  bottom-anchored and clipped to the rows it has, padded if the pane is
  narrower than the content area (it can never be wider, since a workspace and
  the Overview share the same content area). Agents are never disturbed.
- Collapse state is shared by every client.
- Accordion mode might also become a workspace layout mode beside today's
  split layout ("Split" rather than "Pane mode", since every layout holds
  panes). The owner's workspaces hold one or two panes each, so this is
  optional; with client-side rendering (below) it would cost little, as it is
  just another client layout.

### Client-side rendering

The Overview needs panes from several servers in one frame, and today each
server composes one workspace into a cell surface per client
(`compute_surface_for`, the server's `ui/` drawing, `ChromeRole` chrome), so
no single server can compose it. The owner's direction: the client owns
rendering.

- **The server keeps** PTYs, emulators, detection, git, usage, persistence,
  workspace layout as data (split tree and ratios, saved and shared), PTY size
  arbitration, and streaming per-pane grid deltas (cells, cursor, scroll,
  modes, title).
- **The client takes** laying out the workspace tree, the accordion and the
  Overview; all chrome (borders, titles, scrollbars, bars, dividers);
  composing cells from any number of servers into one frame; hit testing and
  mouse mapping into pane coordinates. Selection and copy mode probably move
  too.
- **Goes away:** server-side surface composition and drawing, `ChromeRole`,
  composed-surface baselines. The per-client `render_plan` shrinks to which
  panes a client is subscribed to and at what baseline.
- **Config moves:** scrollbars, divider style and the CJK cursor setting move
  from `server.toml` to `client.toml`, which fits "a setting belongs to whoever
  draws it" better than today.

Before writing a plan, inventory every server-side module that composes or
draws and what each becomes.

### Open questions

- **Size arbitration:** the client reports the size it shows each pane at, the
  server picks one size per PTY (today's `workspace_geometry_source` policy,
  fed per pane). Does the Overview only subscribe and never request a size?
- **Scrollback:** does a client scrolling a pane fetch history rows on demand?
- **Subscription granularity:** full grid, or bottom-N rows for collapsed and
  peeking panes, to keep them cheap over SSH.
- **Which panes appear:** agent panes only, or shells too? Grouped per machine
  in sidebar order, with the machine's hue on each bar?
- **Expanded state:** one expanded pane, several, or all peeking with
  automatic expansion on Blocked? Does a Blocked bar draw attention, as the
  sidebar does?
- **Interaction:** watch only, with Enter jumping to the pane in its
  workspace, or focus and type in place (input routed to the right server)?
- **Where shared collapse state lives:** the Overview belongs to no server.
  Per pane on its server (every client's Overview moves together), or client
  state (the client saves nothing today)?
- **Disconnected machines:** their panes disappear, or stay as dimmed bars?
- **Toggle on the Overview:** a split mode there means auto-tiling everything,
  the layout being escaped, so probably accordion only.

## Faster startup with unreachable machines

Preflight blocks the TUI until every check of a round finishes, up to
`PREFLIGHT_CHECK_BUDGET`, and a second round follows any successful prompt. A
blackholed host (no RST) costs the ssh `ConnectTimeout` at every launch, so
"fail soft" still means a slow start. This is the documented phase bound; a
shorter path (show the TUI first and finish checks behind it, or remember a
recently dead host) would be new behaviour.
