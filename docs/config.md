# Configuration

shepr is configured with two TOML files, one for each of its two programs:

| File | Read by | Holds |
|---|---|---|
| `client.toml` | the TUI (`shepr` with no subcommand) | keys, the sidebar, mouse and copy behaviour, prompts, the machines shown, the colours of everything the client draws |
| `server.toml` | `shepr-server` | the pane shell and working directory, session restore, pane borders, gaps and scrollbars and their colours, scrollback |

Every setting is optional. An empty or missing file means that program's
defaults.

## Where the files live

Both files live in shepr's directory under the XDG config directory:
`$XDG_CONFIG_HOME/shepr/`, which is `~/.config/shepr/` when
`XDG_CONFIG_HOME` is unset or empty. `XDG_CONFIG_HOME` must be an absolute
path; a relative one fails the launch.

Release and dev builds read the same two files. (They keep separate sockets,
saved layouts and server logs, but not separate config.)

## When the files are read

Each file is read and validated once, when its program starts. There is no
reload and no option to point at another file.

- To apply a change to `client.toml`, quit the TUI (`detach`, `prefix+q` by
  default) and run `shepr` again.
- To apply a change to `server.toml`, the server has to restart. `shepr stop`
  stops the local server, ending every pane process in it; the next `shepr`
  starts a new server, which restores the saved layout with fresh shells and
  resumes agents (see `[session]`). For a machine reached over SSH, run
  `shepr stop` on that machine, or `shepr stop --all` to stop every host's
  server at once. A stopped machine stays stopped: a client showing it lists
  it with a Connect entry, which starts its server again when you choose it,
  and so does running `shepr` on that machine.

The CLI subcommands (`shepr status`, `shepr stop`, `shepr detect ...`) read
neither file, except that `shepr status --all` and `shepr stop --all` read
`client.toml` for its `[[machines]]`. They reach each machine over SSH
without prompting, use whatever build of `shepr` is installed there, and
report a machine that needs a login or does not answer instead of waiting on
it.

## Errors fail the launch

Any problem in a file stops its program from starting. There are no
fallbacks and no partially applied files. Problems include:

- TOML syntax errors and values of the wrong type,
- values outside what a setting allows (an unknown palette name, a colour that
  does not parse, a keybinding conflict, a shell that does not exist),
- unknown keys and unknown sections, including misspellings.

The error names the file and the key, for example
`unknown config key ui.mouse_captur`. Where it can, shepr reports every
problem in the file at once; a syntax error, or a value that cannot be read
as its type at all, is reported alone.

A setting placed in the other program's file is an unknown key there. For
example, `[terminal]` in `client.toml` fails the TUI's launch, and `[keys]` in
`server.toml` fails the server's. Both files have a `[ui]` table, but each
accepts only its own `[ui]` settings. When the other program would accept the
setting as written, the error says so and names that program's file, as in
`unknown config section [terminal] in .../client.toml; it belongs in
.../server.toml`. A section is sent on only when every key in it belongs to
the other program.

## Which program owns a setting

A setting belongs to whoever draws or interprets it.

The client draws the sidebar, overlays, prompts and copy mode, and it
interprets your keys and mouse. Those settings live in `client.toml` and
apply the same way whichever machine you are looking at. The client also
owns the title of the terminal it runs in (what window managers show in
title, tab and group bars): it sets it to `shepr: <label>`, where the label is
the local server's (`[local] label`, or this host's short hostname), and
keeps it whichever machine you are looking at. On exit it restores the
terminal's previous title where the terminal keeps a title stack, and leaves
`shepr` elsewhere. There is no setting for it.

Each server runs the panes and renders what goes inside the pane area: the
pane contents and the borders and scrollbars around them. Those settings live
in `server.toml`. Every colour is the client's, the pane chrome included:
a server draws the borders and scrollbars, and your client colours them, on
every machine it shows (see [Colours](#colours)).

Config never crosses hosts. When the client shows a machine over SSH, that
machine's server uses its own `server.toml`, on that machine, and nothing from
your local files. So a remote machine's panes use the shell, borders and
scrollbars set on that machine. Nothing is sent from one
host's config to another. (The client does tell each server whether it
captures the mouse, from its own `ui.mouse_capture`.)

# client.toml

## [ui]

| Setting | Type | Default | What it does |
|---|---|---|---|
| `sidebar_width` | integer (columns) | 26 | Width of the expanded sidebar. Must lie between `sidebar_min_width` and `sidebar_max_width`. |
| `sidebar_min_width` | integer (columns) | 18 | Narrowest expanded sidebar. Dragging the sidebar edge below it collapses the sidebar; dragging back past it expands it again. |
| `sidebar_max_width` | integer (columns) | 36 | Widest expanded sidebar. Must not be less than `sidebar_min_width`. |
| `sidebar_start_collapsed` | boolean | `false` | Start with the sidebar collapsed. |
| `mouse_capture` | boolean | `true` | Capture the mouse for shepr's own mouse UI (selection, sidebar, menus). Set `false` to let the terminal handle normal clicks, such as clicking URLs. Programs in panes that ask for the mouse still get it. |
| `copy_on_select` | boolean | `true` | Copy mouse selections as soon as the button is released. With `false` the selection stays until `ctrl+c` copies and clears it. See [Clipboard](clipboard.md). |
| `host_cursor` | `"native"` or `"drawn"` | `"native"` | `native` uses your terminal's own cursor; `drawn` draws shepr's cursor as cell content. |
| `right_click_passthrough_modifier` | string | `"off"` | A modifier that, held with a right-click hold or drag, sends the gesture to the program in the pane instead of opening shepr's pane menu. Accepts `ctrl` (or `control`), `alt` (or `option`, `meta`), or `ctrl+alt`, in any case. `""`, `off`, `none` and `disabled` turn it off. Shift and the `cmd`/`super`/`hyper` modifiers are refused, because terminal mouse reports carry only ctrl and alt, and terminals usually reserve shift with the mouse. |
| `redraw_on_focus_gained` | boolean | `true` | Redraw the whole screen when your terminal regains focus. Set `false` to avoid a visible flash when switching back; rare terminal surface corruption may then persist until the next full redraw. |
| `mouse_scroll_lines` | integer, 1 to 4096 | 3 | Scrollback lines moved per mouse wheel notch. |
| `confirm_close` | boolean | `true` | Ask for confirmation before closing a workspace. |
| `prompt_new_workspace_name` | boolean | `true` | Ask for a name when you create a workspace. The prompt names the machine the workspace is created on (the one shown) and starts with the name of the new workspace's directory, and a blank answer keeps that name. With `false`, workspaces are created at once and named after their directory. |
| `agent_panel_sort` | `"spaces"` or `"priority"` | `"spaces"` | Order of the agent panel: `spaces` groups agents by workspace, `priority` orders them as an attention queue. |
| `status_indicators` | `"dots"` or `"symbols"` | `"dots"` | How agent states are marked: compact coloured dots, or a distinct glyph for each of Working, Blocked and Idle. |

The default `sidebar_width` also has to fit the bounds: if you raise
`sidebar_min_width` above 26 or lower `sidebar_max_width` below it, set
`sidebar_width` too.

**Remembered sidebar state.** While `sidebar_width`,
`sidebar_start_collapsed` or `agent_panel_sort` is absent from the file, a
change you make by hand (dragging the sidebar edge, collapsing it, toggling
the panel order) is remembered by the client and used at the next launch.
Once the key is set in the file, the file wins at every launch; changes made
by hand then last only until you quit.

## Sidebar layouts: [ui.sidebar.agents] and [ui.sidebar.spaces]

The expanded sidebar has two lists: workspaces ("spaces") and agents. You
choose what each entry shows, line by line.

| Setting | Type | Default |
|---|---|---|
| `ui.sidebar.spaces.rows` | rows of space tokens | `[["state_icon", "workspace"], []]` |
| `ui.sidebar.spaces.row_gap` | integer | 0 |
| `ui.sidebar.agents.rows` | rows of agent tokens | `[["state_icon", "machine", "workspace"], ["agent"]]` |
| `ui.sidebar.agents.row_gap` | integer | 0 |
| `ui.sidebar.agents.rows_by_agent` | table of agent name to rows | empty |

`rows` is an array of lines, and each line is an array of tokens. A token
with nothing to show is left out (an unnamed pane, a workspace outside a Git
checkout), and a line left with no tokens takes no space. `row_gap` is the
number of blank lines between entries.

A layout may have at most 16 lines, a line at most 16 tokens, and a styled
token at most 16 rules.

### Tokens

Space tokens:

| Token | Shows |
|---|---|
| `state_icon` | the workspace's state indicator |
| `state_text` | the workspace's state as a word: `working`, `blocked` or `idle` |
| `workspace` | the workspace name |
| `branch` | the Git branch of the workspace's directory |
| `git_status` | commits ahead of and behind upstream; left out when both are zero |

Agent tokens:

| Token | Shows |
|---|---|
| `state_icon` | the agent's state indicator |
| `state_text` | the agent's state as a word: `working`, `blocked` or `idle` |
| `machine` | the name of the machine the agent runs on; shown only when the client shows more than one server |
| `workspace` | the name of the agent's workspace |
| `pane` | the pane's name, if it has been given one |
| `agent` | the agent's name, such as `claude` or `codex` |
| `terminal_title` | the title the program in the pane set |
| `terminal_title_stripped` | the same title with spinner frames removed |

An unknown token name fails the launch.

### Per-agent layouts

`rows_by_agent` replaces `rows` for a particular agent. Keys are canonical
agent names, written exactly as shepr names the agent (lowercase, such as
`claude`, `codex`, `pi`, `gemini` or `opencode`). Aliases and other spellings
are refused.

```toml
[ui.sidebar.agents]
rows = [["state_icon", "machine", "workspace"], ["agent"]]

[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "machine", "workspace"], ["terminal_title_stripped"], ["agent"]]
```

### Token styles

Any token can be written as an inline table instead of a string, to style
that one occurrence:

```toml
{ token = "workspace", fg = "#89b4fa", bold = true, dim = false }
```

- `fg` is a foreground colour, strictly `#rgb` or `#rrggbb`. Colour names and
  `rgb(...)` are not accepted here.
- `bold` and `dim` turn those attributes on or off. `false` removes the
  attribute even where the sidebar would add it.
- A field you leave out keeps the look the sidebar would otherwise give the
  token, including a machine's palette colours.

### Rules

A styled text token can carry `rules`, which restyle or hide it depending on
its value. Rules work on every token except `state_icon` and `git_status`.

Each rule has exactly one condition:

| Condition | Matches when the value |
|---|---|
| `equals = "text"` | is exactly the text |
| `contains = "text"` | contains the text |
| `starts_with = "text"` | starts with the text |
| `gt = number` | is a number greater than this one |
| `lt = number` | is a number less than this one |

and any of:

| Field | Effect |
|---|---|
| `ignore_case = true` | compare text ignoring ASCII case (text conditions only) |
| `fg`, `bold`, `dim` | the style to use when the rule matches, as for a token |
| `hide = true` | leave the token out when the rule matches |

The rules are tried in order and the first match wins: its `fg`, `bold` and
`dim` override the token's own, and anything it leaves out comes from the
token. When no rule matches, the token keeps its own style. A numeric
condition matches only a value that is wholly a finite number (`90`, not
`90%`).

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", { token = "machine", bold = true, rules = [{ equals = "build", fg = "#a6e3a1" }] }, "workspace"],
  [{ token = "agent", rules = [{ starts_with = "cl", ignore_case = true, fg = "#fab387" }] }, { token = "pane", rules = [{ equals = "scratch", hide = true }] }],
]

[ui.sidebar.spaces]
row_gap = 1
rows = [["state_icon", "workspace"], [{ token = "branch", dim = true, rules = [{ equals = "main", hide = true }] }, "git_status"]]
```

Inline tables must stay on one line in TOML; the outer arrays may span lines.

## [keys]

### Key syntax

A key is written as modifiers and one key joined by `+`, such as `ctrl+b`,
`alt+shift+left` or `f12`. Names are case-insensitive.

- Modifiers: `ctrl` (or `control`), `alt` (or `option`; `meta` is also alt),
  `shift`, `super` (or `cmd`, `command`), and `hyper`.
- Named keys: `enter` (or `return`), `esc` (or `escape`), `tab`, `space`,
  `backspace` (or `bs`), `left`, `right`, `up`, `down`, `home`, `end`,
  `pageup` (or `pgup`), `pagedown` (or `pgdn`), `delete` (or `del`), `insert`
  (or `ins`), and `f1` through `f35`.
- Named punctuation: `minus`, `plus`, `comma`, `period`, `slash`,
  `backslash`, `quote`, `double_quote`, `semicolon`, `colon`, `percent`,
  `ampersand`, `backtick`.
- Any other single character stands for itself, including non-ASCII ones. An
  uppercase letter means shift with that letter: `prefix+N` is
  `prefix+shift+n`.

The most dependable direct shortcuts are ctrl with a letter, function keys,
and explicit modified chords. Whether `alt`, `super` and modified punctuation
reach shepr can depend on your terminal and on tmux.

### The prefix

`prefix` (default `ctrl+b`) is the key that enters prefix mode. A binding
written `prefix+<key>` fires when you press the prefix and then the key. A
binding without `prefix+` is a direct shortcut that fires in normal use.

Pressing the prefix twice sends the prefix key itself to the pane, so
`prefix+<the prefix>` cannot be bound.

### Binding values

- A string binds one key: `zoom = "prefix+z"`.
- An array binds several: `zoom = ["prefix+z", "f11"]`.
- `""` leaves the action unbound.

Validation refuses:

- the same key for two actions;
- a direct shortcut on a plain character key (no modifier, or only shift),
  since it would swallow typing; write it as `prefix+<key>` instead;
- a binding that collides with a default you have not changed. If you bind
  `prefix+t` and a default already uses it, or set a prefix that a default
  uses, the error names the default's setting: set that one too, to another
  key or to `""`. For example `prefix = "esc"` needs `navigate_back` set,
  because `esc` is its default, and `prefix = "-"` needs `split_horizontal`
  set, because its default `prefix+minus` would then be the prefix twice.

### Actions

| Setting | Default | Action |
|---|---|---|
| `help` | `prefix+?` | open keybinding help |
| `detach` | `prefix+q` | detach this client from its server |
| `workspace_picker` | `prefix+w` | enter navigate mode |
| `goto` | `prefix+g` | open the session navigator |
| `new_workspace` | `prefix+shift+n` | create a workspace |
| `rename_workspace` | `prefix+shift+w` | rename the selected workspace |
| `close_workspace` | `prefix+shift+d` | close the selected workspace |
| `previous_workspace` | `prefix+p` | select the previous workspace |
| `next_workspace` | `prefix+n` | select the next workspace |
| `switch_workspace` | `prefix+1..9` | switch to workspace 1 to 9 (indexed, see below) |
| `previous_agent` | unbound | focus the previous agent in the sidebar |
| `next_agent` | unbound | focus the next agent in the sidebar |
| `focus_agent` | unbound | focus sidebar agent 1 to 9 (indexed, see below) |
| `split_vertical` | `prefix+v` | split the focused pane side by side |
| `split_horizontal` | `prefix+minus` | split the focused pane into stacked panes |
| `close_pane` | `prefix+x` | close the focused pane |
| `rename_pane` | `prefix+shift+p` | rename the focused pane |
| `clear_pane` | unbound | clear the focused pane |
| `copy_mode` | `prefix+[` | enter keyboard copy mode |
| `zoom` | `prefix+z` | toggle zoom of the focused pane |
| `resize_mode` | `prefix+r` | enter pane resize mode |
| `resize_pane_left`, `resize_pane_down`, `resize_pane_up`, `resize_pane_right` | unbound | resize the focused pane without entering resize mode, for example `ctrl+shift+alt+left` |
| `toggle_sidebar` | `prefix+b` | collapse or expand the sidebar |
| `focus_pane_left`, `focus_pane_down`, `focus_pane_up`, `focus_pane_right` | `prefix+h`, `prefix+j`, `prefix+k`, `prefix+l` | focus the neighbouring pane |
| `swap_pane_left`, `swap_pane_down`, `swap_pane_up`, `swap_pane_right` | `prefix+shift+h`, `prefix+shift+j`, `prefix+shift+k`, `prefix+shift+l` | swap the focused pane with its neighbour |
| `cycle_pane_next` | `prefix+tab` | cycle to the next pane |
| `cycle_pane_previous` | `prefix+shift+tab` | cycle to the previous pane |
| `last_pane` | unbound | go back to the last focused pane, across workspaces |

### Indexed bindings

`switch_workspace` and `focus_agent` take digit keys, `1` for the first
entry through `9` for the ninth. Write a whole range as `1..9` with the
modifiers to use, such as `"prefix+1..9"` or `"prefix+alt+1..9"`, or list
single keys: `["prefix+1", "prefix+2"]`. Every key must be a digit from 1 to
9, and the range form is accepted only for these two actions.

### Navigate mode

`workspace_picker` opens navigate mode. While it is open only these keys do
anything; every other key, the prefix included, is ignored. They are plain
keys and must not include `prefix+`.

| Setting | Default | Action |
|---|---|---|
| `navigate_back` | `esc` | leave navigate mode |
| `navigate_up` | `up` | move the selection up through the agents, then the workspaces above them |
| `navigate_down` | `down` | move the selection down the workspaces, then the agents below them; a machine's Connect or Restart entry is a stop in place of its workspaces |
| `navigate_open` | `enter` | open the selected workspace, focus the selected agent's pane, or choose the selected Connect or Restart entry |

Navigate keys may reuse keys that actions use, but not each other's keys or
the prefix.

Keys inside copy mode, resize mode and prompts are fixed and not
configurable.

### Example

```toml
[keys]
prefix = "ctrl+a"
split_vertical = ["prefix+v", "prefix+|"]
split_horizontal = "prefix+minus"
focus_agent = "prefix+alt+1..9"
next_agent = "ctrl+alt+n"
previous_agent = "ctrl+alt+p"
last_pane = "prefix+space"
clear_pane = "prefix+ctrl+k"
```

## Machines: [local] and [[machines]]

The client shows the local server and, next to it, the servers of every
machine listed as a `[[machines]]` entry. Machines are read once at launch;
there are no commands to add or remove them. shepr has to be installed on
each machine.

### [[machines]]

| Field | Required | What it is |
|---|---|---|
| `label` | yes | the name shepr shows for the machine |
| `ssh` | yes | how to reach it: an ssh host alias, `user@host`, or an `ssh://` URL |
| `palette` | yes | the machine's hue (see [Host colours](#host-colours)) |

Rules:

- A label must not be blank, contain control characters, or start or end with
  whitespace. Inner spaces and non-ASCII characters are fine.
- Labels must be unique.
- An ssh target must not be empty, start with `-`, contain control
  characters, include a password (`user:password@host`), or be longer than
  1024 bytes.
- Any other field in an entry is an unknown key.

shepr runs ssh through a generated config that includes your `~/.ssh/config`
first, so your host aliases, users, keys and jump hosts apply. It adds
keepalive settings (`ServerAliveInterval` and `ServerAliveCountMax`) only as
fallbacks; values you set yourself win. The first authenticated connection to
a machine is reused through a private OpenSSH control socket. Machines that
need you to authenticate are prompted for at startup, one at a time, before
the TUI takes the terminal; host keys are never accepted automatically. A
machine that cannot be reached does not stop the client.

The client never starts a server on a machine by itself: it attaches to a
server that is running there, at startup and whenever the connection drops,
and leaves a machine with no server as it is. While a machine is not
connected, the sidebar shows an entry in place of its workspaces, and none of
its workspaces or agents:

| Entry | What it means |
|---|---|
| Connect | No server runs there. Choosing it starts the machine's server and attaches. Until then the client waits, over its SSH connection, for a server to appear (say, from `shepr` run on that machine) and attaches when one does. |
| Starting... / Stopping... | The machine's server is starting or stopping. |
| Restart (other build) | The machine's server is another shepr build. Choosing it asks first: restarting ends every pane process on that machine, and the saved layout is restored with fresh shells and agents resumed. |
| Offline | The machine did not answer. The badge on the machine's row shows why; the client keeps trying. |
| Needs SSH login | SSH refused the client. Run `shepr` again to be prompted, or ssh to the machine yourself. |
| Unavailable | The machine answered but cannot be used until it is fixed there. The badge on its row shows why. |

Choose an entry with a click, or in navigate mode (`prefix+w`) by moving onto
it and pressing Enter. In the collapsed sidebar a glyph on the machine's row
shows its state, and clicking the row acts as its entry does.

The private control socket has to fit Linux's limit on Unix socket path
length. If `XDG_RUNTIME_DIR` is so long that it leaves no room for shepr's
runtime directory and OpenSSH's own suffix, that machine's setup fails.

### [local]

| Field | Default | What it is |
|---|---|---|
| `label` | this host's short hostname | the name shepr shows for the local server, and in the terminal's title (`shepr: <label>`) |

The short hostname is the part of the hostname before the first dot. A
`label` follows the same rules as a machine label. If the hostname cannot be
read, or is not a valid label, the launch fails until `label` is set.

### One file for every host

An entry whose label is the local server's label, ignoring ASCII case, is
this host's own entry: the client skips it rather than connecting to itself.
That lets one `client.toml` that lists every host be copied unchanged to all
of them. The skipped entry's `palette` is the local server's hue. A host
with no entry of its own gets `blue`.

```toml
# The same client.toml on desk, build and gpu. Each host's short hostname
# matches one label, and that entry is skipped on that host.

[[machines]]
label = "desk"
ssh = "me@desk.lan"
palette = "blue"

[[machines]]
label = "build"
ssh = "dev@build"
palette = "green"

[[machines]]
label = "gpu"
ssh = "ssh://gpu.example.com"
palette = "purple"
```

On `build`, this shows `build` (the local server, in green) with `desk` and
`gpu` next to it. If a host's hostname differs from the label you want, set
`[local] label` on that host, which then means keeping a slightly different
file there.

## Host colours

`palette` takes one of `red`, `orange`, `yellow`, `green`, `cyan`, `blue`,
`purple` or `magenta`, written in lowercase. A missing or unknown value fails
the launch.

The expanded sidebar draws each machine's workspace and agent entries in its
hue. The colours are derived from the background and foreground your terminal
reports, so they fit whatever theme your terminal uses:

- the entry gets a tinted background, stronger on the focused entry;
- the first line is drawn in a main text colour and later lines in a dimmer
  one, each kept readable against the tint;
- an accent colour marks the workspace number and the machine name in agent
  entries, and every host's accent is equally bright.

Where your terminal reports its own colour of that name, its hue is used.
If the terminal reports no background, there is no tint: the accent is the
terminal's own ANSI colour and later lines are drawn dim. The local server's
entries lose the colour while it reconnects. Token styles from
[`[ui.sidebar]`](#token-styles) still win over the palette.

## Colours

There is no colour theme to choose. The client derives every colour it
draws (the sidebar, overlays, menus, prompts, navigate mode and copy mode,
and the pane borders and scrollbars each server draws, on every machine
shown) from the background, foreground and ANSI colours your terminal
reports, so shepr matches the terminal's own theme and follows it when the
terminal switches between light and dark:

- surfaces (panels, the active and selected rows, separators) sit a little
  off the terminal's background, toward its foreground;
- text and muted text keep a readable contrast against every surface;
- the accent (highlights, navigation) is the local server's hue, and the
  agent state colours (green for Idle, yellow for Working, red for Blocked)
  and branch names share its brightness. Where your terminal reports its own
  colour of a hue, that hue is used;
- the focused pane's border is in the shown machine's own accent, the colour
  its sidebar entries carry, so the border tells you which machine you are
  looking at.

Until the terminal reports a background, and on a terminal that never does,
shepr draws with the terminal's own default and ANSI colours.

# server.toml

## [terminal]

| Setting | Type | Default | What it does |
|---|---|---|---|
| `default_shell` | string | `$SHELL`, or `/bin/sh` | The shell new interactive panes run. |
| `login_shell` | boolean | `false` | Start pane shells as login shells. |
| `new_cwd` | string | `"follow"` | Where new panes and workspaces start. |

**`default_shell`** is a path or a bare name looked up on `PATH`. It must
resolve to an executable that shepr recognizes as a shell by its file name:
`sh`, `bash`, `dash`, `zsh`, `fish`, `ksh`, `mksh`, `csh`, `tcsh`, `elvish`,
`xonsh` or `nu`. A value with leading or trailing whitespace is refused.

When it is unset, the server uses `SHELL` from its environment, held to the
same rules: a `SHELL` that does not exist, is not executable or is not a
recognized shell fails the launch rather than falling back. Only an unset or
empty `SHELL` means `/bin/sh`.

**`new_cwd`** takes:

- `"follow"`: the directory of the pane or workspace the new one is made
  from;
- `"home"`: your home directory;
- `"current"`: the directory shepr was launched from;
- any other string: a fixed path, such as `"~/Projects"`. `~` expands to your
  home directory, and a relative path is taken from the directory shepr was
  launched from.

The directory is resolved when the server starts and must exist then. An
empty string is an error; write `"follow"` for the default.

## [session]

| Setting | Type | Default | What it does |
|---|---|---|---|
| `resume_agents_on_restore` | boolean | `true` | When the server restarts and restores its layout, resume supported agent panes into their own conversations. |
| `startup_per_agent_delay_ms` | integer (milliseconds) | 100 | Pause between automatic agent resumes, so they do not all start at once. `0` starts them without spacing. |

Resuming needs the agent's session, which shepr learns from the hooks a
release server installs into each agent's own config at launch, for every
agent present on that host. Dev builds do not install them, so dev panes do
not resume agents.

## [server]

| Setting | Type | Default | What it does |
|---|---|---|---|
| `headless_cols` | integer | 120 | Width of the virtual terminal used while no client is attached. |
| `headless_rows` | integer | 40 | Height of the virtual terminal used while no client is attached. |

Attached clients always use their own terminal size. Both values must be
greater than zero, each at most 4096, and together at most 4194304 cells.

## [ui] in server.toml

| Setting | Type | Default | What it does |
|---|---|---|---|
| `pane_borders` | `"auto"`, `"always"` or `"off"` | `"auto"` | `auto` draws borders around split panes only; `always` also frames a lone pane (only while `pane_outer_borders` is on, since every edge of a lone pane is an outer edge); `off` draws none. |
| `pane_outer_borders` | boolean | `true` | Draw borders along the outside edge of the pane area. Turn off for tmux-style internal dividers without an outer frame. |
| `pane_scrollbars` | boolean | `true` | Draw interactive scrollbars beside panes. Turn off to reclaim the column and keep it out of selections the terminal makes itself. |
| `pane_gaps` | boolean | `true` | Keep split panes visually apart instead of sharing divider borders. |
| `show_agent_labels_on_pane_borders` | boolean | `false` | Show the detected agent's name in a split pane's border when the pane has no name of its own. |

## [experimental]

| Setting | Type | Default | What it does |
|---|---|---|---|
| `reveal_hidden_cursor_for_cjk_ime` | boolean | `false` | Show the focused pane's cursor position to your terminal even when the program hid its cursor, so an input method (fcitx5, ibus) keeps its candidate window at the input position in programs that paint their own cursor, such as Claude Code, pi or codex. Trade-off: an extra cursor shows in programs that hide the cursor without painting one, such as vim in normal mode. |
| `cjk_ime_agents` | array of agent names | `[]` | Limit `reveal_hidden_cursor_for_cjk_ime` to focused panes running one of these agents. Empty means every focused pane. Agent names and their aliases are accepted in any case (`claude`, `claude-code`); an unknown name fails the launch. |
| `cjk_ime_cursor_shape` | string | `"steady_block"` | The cursor shape shown when `reveal_hidden_cursor_for_cjk_ime` is on: `block`, `steady_block`, `underline`, `steady_underline`, `bar` or `steady_bar`. |

## [advanced]

| Setting | Type | Default | What it does |
|---|---|---|---|
| `scrollback_limit_bytes` | integer | 10000000 | Approximate scrollback budget per pane, in bytes, turned into a line count for the pane's width. `0` turns scrollback off. |

The budget is not a hard cap: any nonzero value keeps at least 1000 lines,
and a pane that is widened keeps the history it already has rather than
dropping it.
