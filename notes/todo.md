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

## Faster startup with unreachable machines

Preflight blocks the TUI until every check of a round finishes, up to
`PREFLIGHT_CHECK_BUDGET`, and a second round follows any successful prompt. A
blackholed host (no RST) costs the ssh `ConnectTimeout` at every launch, so
"fail soft" still means a slow start. This is the documented phase bound; a
shorter path (show the TUI first and finish checks behind it, or remember a
recently dead host) would be new behaviour.
