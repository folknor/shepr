# shepr

A personal, Linux-only fork of [herdr](https://github.com/herdrdev/herdr): a
terminal multiplexer for AI coding agents. Workspaces and panes run in a
headless server; a workspace is one pane layout (there are no tabs); the TUI is a client. An agent sidebar shows every agent's
state (idle, working, blocked) across all panes, including panes on
other hosts reached over SSH.

There is no compatibility with upstream herdr installs. The fork is stripped
hard: the goal is the smallest code surface that does what the owner uses,
not parity with upstream.

Remove legacy fields and migration code freely.

## Scope

Kept:

- Server/client split, local and SSH endpoints (one client shows servers on
  several hosts; the owner installs the binary on each host manually, and a
  host never has more than one `shepr` installed)
- Terminal core: `alacritty_terminal` for emulation, a small libc PTY layer
  (`crates/shepr-pty/src/`), PTY hosting
- Workspaces, panes, layout, the agent sidebar
- Agent detection from bundled manifests (`crates/shepr-detect/src/manifests/*.toml`),
  compiled into the binary
- Agent integrations (`crates/shepr-integration/src/`): hooks installed into each agent's
  own config that report state and session IDs back to shepr. The server
  accepts a report only from these integrations, each under its own
  `shepr:<agent>` source naming its agent; any other source is refused. A
  release server installs or updates them at launch for every agent whose
  config directory exists on its host. Dev servers log that integration
  installation is skipped and use screen detection; release hooks report only
  from release panes. Agent session IDs and hook-only states are therefore
  unavailable in dev panes. There is no install or uninstall command
- Session restore (layout saved to disk, rebuilt with fresh shells) and agent
  resume on restore
- Git status in the sidebar (branch, ahead/behind)
- Mouse selection, copy mode, keybinding help, and the outer terminal's
  window title, which the client sets to `shepr: <local label>` when the TUI
  starts and keeps whichever machine is presented (no setting, no server part)
- The JSON API over the server socket. The TUI does not act on workspaces
  or panes through it: it sends typed commands on the TUI's connection to
  the server socket (`shepr_protocol::command::EndpointCommand`), none of
  which is an API method. The CLI acts on this host's server or state, with
  one exception: `status --all` and `stop --all` also take in every machine
  configured in `client.toml`, over BatchMode SSH (`shepr_remote::fleet`).
  No command can be aimed at a single machine. `status`, `stop`, `detect
  capture` and `detect explain <PANE>` talk to the local server over its
  socket; `detect explain --file` runs in the CLI process

shepr is for overseeing agents across machines, not for driving them.
Launching or steering agents through shepr is deliberately not kept, and
neither is driving panes: no CLI command or API method sends text or keys to
a pane, waits for pane output or reads pane history, and there is no
subscription that fires when text appears in a pane. The one pane input
setting kept is the TUI's own pane.input.set command, for its context menu.
Nor are local detection manifest overrides and their reload: a detection
change ships as a new build.

The CLI is small on purpose. `shepr` with no subcommand attaches the TUI, and
the subcommands are `status`, `stop`, `detect` and `man`. `shepr man` lists
the bundled user manuals (every end-user document in `docs/`, compiled into
the binary) and `shepr man <topic>` renders one to the terminal, colour off
when stdout is not a terminal or `NO_COLOR` is set. Detaching from the TUI
leaves every server running and prints how to attach again (`shepr`) and how
to stop the local server (`shepr stop`), spelt for this build and socket
override like the restart guidance. Workspaces and panes are managed from the TUI only; there
is no CLI group for them, and no CLI attach to a single terminal. `shepr
detect capture <pane>` prints the screen text and OSC title and progress the
detector evaluates for a pane, as JSON that `detect explain --file` reads back,
and `shepr detect explain <pane>` shows the pane's state and what owns it
(the screen, a hook or the process exit) beside the verdict the screen rules
give now and the rule that matched.
`shepr status` prints this installation (version, build profile and build
id, then the binary path, and a `shepr-server` line only when the sibling
binary is missing or of another version or build), then one line for the
server at the selected socket: not running, starting, running (with pid and
uptime, read from its boot id, and its workspace, pane and agent counts from
the `server.summary` API method), stopping, not answering, or running a
different build, the last with stop and restart guidance spelt like the other
operator guidance. `status --json`, `status server [--json]` and `status
client [--json]` keep their machine-readable `key: value` and JSON forms, which
the SSH discovery, the conditional stop and `--all` read; `status --json` adds
the server's counts when that server is of the reporting build.
`status --all` then prints one line per configured machine: its server's
state, pid, uptime and counts as that machine's own `shepr` reports them,
whatever its build, and a note when the server is not the build installed
there (a restart brings the installed one up) or the install is not this
build. `stop --all` stops every configured machine's server concurrently,
each by the boot its status named, then the local one, prints a line per
host, and exits 0 only when every host ended with no server. A machine that
needs an SSH login, is unreachable or has no `shepr` gets a line saying so;
nothing prompts.

Configuration is two files in the XDG config directory: `client.toml`, read
only by the TUI (and its internal `client` launch), and `server.toml`, read
only by `shepr-server`. CLI subcommands and the internal
`remote-client-bridge` and `remote-wait-for-server` launches read neither,
except that `status --all` and `stop --all` read `client.toml` for its
machines, validated as the TUI validates it. Each file is read and validated
once at launch, and a missing file means that program's defaults. There is no
reload and no config path override. Any config problem fails the launch; no
fallbacks. An unknown key is a config problem, so a setting placed in the
other program's file fails this program's launch; the error names the file
it belongs in when the other program reads it as written. Directories follow the XDG
spec. `crates/shepr-config/src/default-client.toml` and `default-server.toml`
document every setting of each file.

Config never crosses hosts. Each process uses only its own file on the host
it runs on, and the handshake welcome carries no config. (The client's hello
does report its mouse-capture preference, which comes from its own
`ui.mouse_capture`, so the server knows when to capture the mouse for that
client.) A setting belongs to whoever draws or interprets it, and lives in
that program's file. The client applies its own config to everything it draws
and interprets: keys, the sidebar, agent panel order, status indicators,
prompts, mouse and copy behaviour and their colours, the same whichever
machine is being presented, and the per-host sidebar colours derived from each
machine's `palette` and the host terminal's theme. Every colour is the
client's: the pane chrome a server draws (borders, their titles, scrollbars)
names each cell's role (`shepr_protocol::ChromeRole`) instead of a colour, and
the client colours the roles as it composes the surface. There is no colour
theme and no colour setting besides the hues: the client derives its whole
palette from the colours the host terminal reports (`host_tint::UiPalette`),
with the local server's hue as its accent, and uses the terminal's own ANSI
colours until a background is reported. The focused pane's border takes the
presented machine's own sidebar accent. `client.toml` holds those
`[ui]` settings, `[keys]`, `[local]` (the local server's `label`) and
`[[machines]]`. Each server applies its own config to what it
runs and renders into pane cells: shell and working directory, session,
scrollbars, whether split panes share a divider or draw adjacent borders, and
the cursor it reveals for CJK input methods. Pane borders are always drawn.
`server.toml` holds those `[ui]` settings, `[terminal]`, `[session]`,
`[server]`, `[advanced]` and `[experimental]`. A server always
computes a workspace's Git branch and ahead/behind, whatever any sidebar
shows.

Agent states are Working, Blocked and Idle. Unknown presents as Idle.

Machines are configured in `client.toml` as `[[machines]]` entries (a `label`, an
`ssh` target and a required `palette` hue), read once at launch like the rest of the config; there are no
commands to add, remove or list them. The client names the local server by
the `[local]` table's `label`, or this host's short hostname when it is unset,
never as "Local", and no name is reserved. A machine entry whose label equals
it in any ASCII case is this host's own entry: it is skipped, and its
`palette` is the local server's hue (blue without such an entry), so one
`client.toml` listing every host can be shared by all of them. The TUI connects to them without
prompting (BatchMode), so at startup, before it takes the terminal, `shepr`
checks every machine and runs interactive ssh for each one that needs
authentication, one at a time, on shepr's own control socket, then checks those
again; there is no command for it. A check command that uses its full round-trip
budget without a result is treated as a possible interactive-authentication
wait, including security-key presence, and gets the foreground attempt. A
command shortened by the overall attempt budget remains an offline result.
Host keys are never accepted automatically.
A running local server of a different build is then offered a restart (only at
the build profile's own runtime address: a socket override names a server this
client cannot relaunch, so it gets stop guidance instead) after the last
authentication prompt: the question says that the restart ends the server's
pane processes and that the layout is restored with fresh shells and agents
resumed. Consent is asked on the terminal and defaults to keeping the server;
with no terminal, or on refusal, the server is left running and unavailable and
shepr says how to stop it. Guidance uses `shepr` for a release build and the
running executable path for a dev build (or `brokkr run --` if the path cannot
be resolved), with the selected socket override. The stop names the boot
identity that was observed, so a server that replaced it in the meantime is not
stopped, and is offered again as a new occupant.

shepr never starts a server on another host by itself. Running `shepr` starts
that host's own server; every connection the client makes to a machine by
itself only attaches (the remote `remote-client-bridge` without `--start`), at
startup and after a dropped connection alike, and a machine with no server
running stays without one. `shepr stop` on a machine therefore leaves it
stopped. The sidebar lists every machine expanded (machines cannot be folded),
with its workspaces while it is connected and, while it is not, a state entry
in their place and no workspaces or agents of its last snapshot: Connect (no
server running; activating it starts the server through the bridge's start
mode and attaches), Starting... or Stopping..., Restart (a server of another
build; activating it asks first, with the same facts as the local restart
question, then stops that server by the boot its status named and starts this
build's), Offline (unreachable; the machine row's diagnostic badge carries the
reason, and it is retried with the reconnect backoff), or a login entry for a
machine whose SSH needs authentication (startup prompts for it; after startup
the entry says to run `shepr` again or ssh to it). Navigate mode steps onto a
Connect or Restart entry in its place among the workspaces and Enter opens it;
a click on the entry, or on its machine row, does the same, and the collapsed
sidebar strip shows each machine's state as the glyph on its machine row. A
machine that answers with no server is not polled: the client keeps
`shepr remote-wait-for-server` running there over the shared SSH control
connection, which watches the runtime directory with inotify and exits once a
server answers, and the client then attaches. With machines configured, losing
the local server does not end the client either: it keeps serving the remote
machines and reconnects once the local server is restarted.

Panes run agents and shells. Key encoding to pane children covers what those
use: legacy encoding, kitty disambiguate and the keys crossterm's `KeyCode`
models, and modifyOtherKeys for Enter, Esc, Tab and Backspace. Full kitty
report-all fidelity (F13 and above, lock and bare modifier keys, keypad
identity) and full modifyOtherKeys level 1/2 encoding are deliberately not
implemented; they need a shepr-owned key model, which the owner decided is
not worth it for agent and shell panes. Do not file these as defects.

## Workspace layout

The root `shepr` package is the binary. Extracted libraries live under
`crates/`. Dependencies follow the documented bottom-up layering: lower
crates never depend on higher ones. Test code shared across crates lives in
two dev-only crates, `shepr-test-support` (isolation) and
`shepr-test-fixtures` (fixtures and doubles built on the other crates'
public API); `brokkr.toml` forbids any normal or build edge to either. No
production crate has a test feature: where a double must reach inside a
production type, the production crate offers a seam (a trait or a public
constructor) instead.

The libraries, from lower layers to higher layers. `brokkr.toml`'s
dependency rules hold the layering; the one-line descriptions are
orientation, and nothing checks them:

- `shepr-core`: shared geometry (grids, host cells, pane pixel extents), layout
  and plain types.
- `shepr-platform`: Linux process, filesystem, IPC and terminal plumbing,
  including the generic owned runtime directory and the `ClipboardRoute` the
  client reads once at start. It holds no SSH policy.
- `shepr-term`: terminal vocabulary and pure encoding shared by the emulator,
  the server and the client: row and point coordinates, selections, scroll
  metrics and scrollbar geometry and paint rows, colours, DEC and keyboard
  modes, display widths and text column geometry, the VT spellings shepr writes, the host's observed theme and the per-host sidebar colours derived from it (`host_tint`), key identity and chord matching, child-facing key and mouse
  encoding, and the pixel mouse eligibility rule (which connection, pane and
  report may carry pixel positions). It keeps `alacritty_terminal` and `vte` out of the client binary.
- `shepr-vt`: terminal emulation and read formatting; it re-exports the
  `shepr-term` vocabulary it speaks.
- `shepr-pty`: PTY process launch and IO, using `shepr-platform` for fd plumbing, socket admission and process identities.
- `shepr-paths`: runtime layout and server address policy: `AppPaths` (XDG
  directories, `SHEPR_STARTUP_CWD`), `BuildProfile` and the pane markers,
  `ServerAddress` and the socket override rule, and the data-directory lease
  file name. It reads no config file, so api, remote and the CLI get the
  layout without the settings.
- `shepr-test-support`: shared environment isolation, scratch directories and hook asset capture for tests.
- `shepr-agent`: agent identity: the descriptor table, report origins,
  session and resume vocabulary, `AgentState` and label normalization.
- `shepr-detect`: detection manifests and their rule engine, agent process
  recognition over `shepr-platform`'s `/proc` readers, and per-pane ownership
  arbitration between screen detection and integration reports.
- `shepr-integration`: the agent integration installer and its bundled hook
  assets; only the server links it.
- `shepr-config`: configuration parsing and validation: one validate step per
  file, on the launch's `shepr-paths` layout, into validated types that hold
  only validated values.
- `shepr-protocol`: compact wire types, codec, framing and preamble, and the
  canonical construction and parsing of the identities they carry (it
  allocates none); it uses `shepr-core` for shared grid and input-batch
  resource budgets that config also borrows, and `shepr-agent` for the agent
  identity the client projection carries.
- `shepr-surface`: what a pane surface means above the wire: the server's
  delta planner, the client's decoder and its baseline, ratatui conversion,
  the wide-glyph rule for pane rows, the repair of glyphs that chrome laid
  over a frame splits (for the server's pane chrome and the client's
  compositor alike), and the client's frame composition over a shape-checked
  canvas.
- `shepr-api`: JSON API schema and client, and the server socket: its listener
  tells JSON requests from TUI connections, admits each kind, and hands JSON
  connections to the JSON service and TUI ones to the server's client
  protocol. The client's deadline-bounded requests and the decoded `ping`
  answer are the boundary launch drives. Its detect explain schema is built
  on `shepr-detect`'s explanation types.
- `shepr-launch`: the server lifecycle seen from outside the server: local
  server launch and probing (the launch lock, the sibling `shepr-server`, its
  boot log, the different-build policy), presence probing, conditional stop,
  the local restart offer, the invocation grammar and exit codes shepr processes
  share (the server's arguments and `DaemonExit`, the CLI's command words),
  the operator text naming the commands that reach a server, the endpoint
  failure vocabulary with its one disposition table, for every endpoint, and
  the connection heartbeat cadence the client keeps and the SSH bridge's idle
  expiry is checked against.
- `shepr-termio`: host terminal I/O: host input framing and parsing, the
  fixed and configured key tables the client's modes and overlays route by,
  copy-mode keys, the one-line text editor prompts edit with, frame blitting
  and host terminal modes, title, clipboard and theme queries.
- `shepr-remote`: configured machines and SSH connections, the SSH attempt
  timing, startup preflight, the remote-host side of the SSH bridge (its
  stdio relay and idle watchdog), which only attaches to its host's server
  unless started for the operator's Connect or Restart (then it starts that
  server through `shepr-launch`), and the remote wait for a server
  (`crates/shepr-remote/src/server_wait.rs`) with the client side that runs
  it. It owns the SSH path policy (OpenSSH `%C` expansion, control
  path naming, bridge socket naming; `crates/shepr-remote/src/ssh_paths.rs`). It classifies OpenSSH output into launch's failure
  vocabulary at its boundary and keeps discovery evidence to itself.
- `shepr-git`: Git status as one subsystem: checkout discovery, the Git
  command runner with its environment and deadline policy, config dependency
  tracking, the status computation, the refresh algorithm and its cache, and
  the long-lived worker thread that owns that cache. It takes targets (a cwd,
  a known checkout key and an opaque owner) and returns statuses and read
  errors; it knows nothing of workspaces, server events or refresh cadence.
- `shepr-mux`: terminals, panes, workspaces (each holding the Git status last
  applied to it), events and persistence.
- `shepr-server`: application state, UI and serving.
- `shepr-client`: endpoint management and TUI presentation.
- `shepr-daemon`: the `shepr-server` executable, a thin `main` over
  `shepr-server`, kept apart so the client binary links none of the
  server-only crates.

`shepr-test-fixtures` (dev-only) sits above config, protocol, pty and termio,
so only crates above those can take it.

`shepr-palette-preview` is a development tool, not part of an installation:
it prints the colours `shepr_term::host_tint` derives (the UI palette and each
host hue's sidebar colours) on the surfaces they are used on, with their
contrasts, for the terminal it runs in or a theme given as arguments. Run it
with `brokkr run shepr-palette-preview -- --help`.

## Build and test

`brokkr` is the only entry point. Never run raw `cargo`.

| instead of | run |
|---|---|
| `cargo build` / `cargo clippy` / `cargo test` | `brokkr check` (gremlins + clippy + tests, the gate) |
| `cargo test -p <pkg> <name>` | `brokkr test -p <pkg> <name>` |
| `cargo run -- <args>` | `brokkr run -- <args>` |
| `cargo fmt` | `brokkr fmt` |
| `cargo install --path .` | `brokkr install` |

- Two executables make one installation: `shepr` (client and CLI, the root
  package) and `shepr-server` (the headless server, in the `shepr-daemon`
  package under `crates/shepr-daemon`). They are separate packages so the
  `shepr` binary links neither `shepr-server` nor `shepr-mux`; both take
  their build id from the one `shepr-protocol` they link. `brokkr install`
  installs both packages, and the client launches `shepr-server` from its own
  directory. `brokkr run` builds and runs only the one target it names
  (`shepr` by default), so a run that needs the sibling builds it first
  (`brokkr run shepr-server` builds it and runs it in the foreground); a
  build of one binary is never a usable installation alone.
- `brokkr check` is the gate; run it before every commit.
- `brokkr test -p <pkg> <name>` is a substring filter over one package's unit
  and integration tests; this is a workspace with no default package, so `-p`
  is required (`-p shepr` for the root binary). It builds the dev profile
  here (`brokkr.toml`'s `[test] debug = true`; `--release` for release), and
  so does `brokkr check`'s test phase. Unlike `brokkr check`, it always
  passes `--include-ignored`, so a filter that matches an ignored test
  (root-only, or a re-exec entry point) runs it too.
- `brokkr man` lists the bundled docs (`man check`, `man config`, `man run`,
  ...). Read those rather than guessing at flags.
- Never run two brokkr/cargo invocations at once.

### Running a dev build next to the installed one

The build profile selects the runtime directory and the data directory (saved
layout, server log, lease). A release build keeps the plain XDG
locations; a dev build uses sibling `shepr-dev` directories, so it has its own
sockets and saved layout with no flag. Both config files, machines
included, are shared by every profile. The build identity also covers the profile
as well as the source, so a dev and a release build never talk to each other's
server: one that is reached anyway is refused with guidance naming the current
profile's entry point (`shepr` for release, the running executable path for
dev).

Run it with `brokkr run --debug -- [<command>]`, including from inside a pane
of the installed server. The TUI is always refused inside a pane of a server
of its own profile, so a dev TUI runs from a release pane and not from a dev
one. The dev client launches the `shepr-server` beside it
in `target/debug`, which `brokkr run` does not build: build it first with
`brokkr run --debug shepr-server -- --version`, or through `brokkr check`.
Every pane exports `SHEPR_SOCKET_PATH` as its server resolved it, which normally wins over the
per-profile runtime directory, and also `SHEPR_BUILD_PROFILE`, the profile
(`release` or `dev`) of the server that owns the pane. A process whose own
profile differs from that marker ignores the socket variable and resolves its
own profile's runtime directory. `SHEPR_SOCKET_PATH` selects the server socket.
A socket override names an existing server: the TUI attaches to it but never
starts a server there. A non-runtime path in `SHEPR_SOCKET_PATH` selects another
server, so a user can set it inside a pane. The variable stays exported because
every agent integration reports through it.

- The socket variable with no marker (set by a user or a script), or with a
  matching marker, still wins over the runtime directory.
- A marker that is neither `release` nor `dev` fails the launch.
- The saved layout is not affected by the override, only the socket is.
- `shepr stop` stops whatever server answers, whatever its build, with every
  pane in it. Its hidden `--expect-boot <boot id>` makes the stop conditional:
  the client sends `server.stop_if_boot` with the id from `status server`, and
  the server compares it with its own boot. The distinct method name means an
  older server rejects the request as invalid instead of ignoring the guard
  and stopping unconditionally. A server that replaced the observed one,
  at any point while the stop waits for the socket and the lease to go, keeps
  running and is reported (exit status 3).
- The cross-build JSON control surface is the `ping` response identity
  (`version`, `build_id`, `boot_id`), its `stopping` and `starting` flags
  (each read as false when an older build omits it) and the
  `server.stop_if_boot` request; keep their literal JSON fixtures in the
  `shepr-api` tests in sync with intentional wire changes. `server.summary`
  (the workspace, pane and agent counts `shepr status` shows) is not part of
  it: status asks it only of a running server of its own build.
- Startup and shutdown follow one order, written in `start_server` (which
  `run_server` calls) and in `HeadlessServer::release_socket_after_save`. Startup takes the data-directory
  lease, binds the server socket, restores panes, then opens the client
  protocol. The socket is live and answers `ping` from the moment it is bound,
  with `starting: true` until the client protocol opens; a TUI connection
  before that is refused as transient. Shutdown keeps the socket through the
  final save, retires the lease, then removes the socket. A socket that is
  absent or stale reads as gone, a live one answers by `ping` as starting,
  running (the default) or stopping, and a live one that does not answer reads
  as unresponsive unless it has gone by the time the answer is missed, which
  reads as gone. A launcher waits through starting, treats a server answering
  `stopping` as no server (it no longer accepts clients), and starts a
  successor once the socket is gone. Socket absence only permits a launch
  attempt: the lease decides which contender owns the data directory, even
  before the socket exists.

## Principles

- **State is separated from runtime.** `AppState` is the session's data,
  testable without PTYs or async. Each workspace's `PaneTree` owns its
  layout and its pane records together, and each `PaneRecord` owns its
  pane's `TerminalState`, which is plain data testable without a PTY;
  `PaneRuntime` (held by `App`, outside `AppState`) owns the PTY, its tasks
  and the state shared with them.
- **Render is pure.** `compute_surface_for()` in
  `crates/shepr-server/src/ui/surface.rs` reads `AppState` by shared
  reference and returns one workspace laid out for one client's surface; pane
  runtimes are resized by `App::apply_workspace_geometry`, from the same
  `PaneSurface` descriptions (`ui/pane_surface.rs`: rects, scrollbar gutter and
  track, cursor) that both the full render and the server's retained patches
  consume, and surface drawing takes shared references and only draws. Layout
  follows a geometry change at once; a change that may be one step of a
  gesture (a client surface resize, a split ratio or pane resize command)
  reaches the PTYs only once it has held for `PANE_RESIZE_SETTLE`
  (`app/pane_resize.rs`), and until then the old grid draws clipped or padded
  in the new content rect. What
  each client was last sent is its `CommittedBaseline` (surface and pane
  identities, committed together), owned by `ClientRenderState`.
  The client shell composes the same way: `ClientShellState::compose_frame`
  resolves a `ShellView` (layout, scroll and hit rects) and draws it, both by
  shared reference; `commit_frame` then stores the view and the resolved
  scroll positions in one step, and only once the host terminal took the
  frame, so a refused write leaves no trace in shell state. Drawing never
  writes shell state.
- **Presentation is per client.** Each connection on the server keeps its
  own surface size, outer focus and location; nothing projects
  one client's view into `AppState`. What panes have one of is decided from
  all the views in one place each: PTY size by the PTY size rule
  (`workspace_geometry_source` in `crates/shepr-server/src/server/headless/client_views.rs`,
  which records each workspace's applied area in `AppState`), pane focus reports by
  `sync_pane_focus` (each pane runtime records the focus it is told even while
  its child has focus reporting, DEC mode 1004, off, and a child that turns
  reporting on in a focused pane is told focus-in at once, ordered with the
  pane's other terminal replies), and the host theme by the foreground client: the active
  shell with the most recent user activity. Connection or surface activation,
  outer focus gain, pane interaction, and endpoint commands count as activity;
  a surface resize only changes geometry. What each client is
  owed (a projection, a surface, a patch) is likewise derived per client from
  its own location, baseline and render slot, with a server-wide view epoch
  only for changes every client depends on (`render_plan` in
  `crates/shepr-server/src/server/headless/render.rs`), so one client's slow
  link, resize or scroll never moves another client's render path.
- **No god objects.** `AppState` lives in `crates/shepr-server/src/app/state.rs`;
  `App` behavior is organized across modules under
  `crates/shepr-server/src/app/`. Keep it that way.
- **Linux only.** No `#[cfg(windows)]`, `#[cfg(target_os = "macos")]` or
  `cfg!` branches for other platforms. libc, `/proc` and helper-program
  plumbing lives in the flat `crates/shepr-platform/src/` crate (`lib.rs` plus
  self-contained submodules). Git command environment and deadline policy lives
  in `shepr-git`, and the logind shutdown monitor and session checkpoint
  lifecycle live in `shepr-server`. `shepr-platform` only tells whether a
  reaped child exited with a code or was signalled; `PaneEnding` in
  `shepr-mux`, next to the pane exit arbiter, carries why a pane ended and
  whether its terminal core is intact, and its `needs_checkpoint()` is the one
  answer to whether the exit gets a final session checkpoint. There is no
  per-OS layer and no shims standing in for other platforms.
- **Detection is decoupled.** The detector reads a screen snapshot and never
  touches the parser or viewport state. When changing a manifest, capture the
  pane with `shepr detect capture <pane>`, encode
  invariant controls as explicit AND/OR gates, and never use the user-visible
  viewport (users scroll it).
- **Hot paths multiply.** Work reachable from view computation, rendering,
  PTY parsing, detection or client frame fanout runs per byte or event, times
  panes, times clients. Inside those loops use narrow accessors, keep
  terminal-core locks short, and preserve the hidden-pane early exits.
- **No wire compatibility obligations.** Client and server are always the same
  build. Change the protocol freely; there are no frozen fixtures.
- **Wire encoding is shepr's own.** Frames are `[u32 LE length][payload]`;
  a server message too large for one frame spans several, the top bit of the
  length marking that more follow (`crates/shepr-protocol/src/framing.rs`),
  and payloads use the positional serde codec in `crates/shepr-protocol/src/codec.rs`
  (varints, no field names, not self-describing). Wire types must not use
  `skip_serializing_if`, `flatten`, `untagged` or tagged enums.

## Code conventions

- No `unwrap()` in production code. Use `tracing` for logging. `#[allow]` only
  with a comment saying why.
- Don't add dependencies without a reason; check what existing ones cover.
- Unit tests live next to the code (`#[cfg(test)] mod tests`). New `AppState`
  or `Workspace` behaviour should be testable with `AppState::test_new()` /
  `Workspace::test_new()`.
- Tests that read or write the process environment hold a
  `shepr_test_support::IsolatedEnv`; tests that only build and inspect an
  explicit command environment map do not need it. Tests that write files use a
  `shepr_test_support::ScratchDir`, never fixed or shared temp paths.

## Terminal core

The emulator is `alacritty_terminal`, pinned with `=` in `Cargo.toml` (bump it
deliberately, never through a loose requirement). Direct use of its types stays
in `crates/shepr-vt/src/`: `lib.rs` defines `shepr_vt::Terminal` and the adapter
boundary, with supporting implementation split across modules. `format.rs`
provides the plain/VT formatters used for reads;
`handler.rs` wraps the parser's `Handler` for dispatched input, while `scan.rs`
scans sequences vte does not dispatch that shepr still needs to answer or track.
Alacritty types must not leak out of `shepr-vt`. In the mux
layer, `crates/shepr-mux/src/pane/terminal.rs` defines `PaneTerminal`; its
`PaneTerminalCore` holds the `shepr_vt::Terminal` and pane-level render and OSC
state. When writing against alacritty's API, read the source instead of relying
on memory: the pinned `alacritty_terminal` release and the matching `vte` are
in the cargo registry
(`~/.cargo/registry/src/*/alacritty_terminal-<version>/`,
`~/.cargo/registry/src/*/vte-<version>/`, versions per `Cargo.lock`).

PTYs do not use `alacritty_terminal::tty`: it can only add environment
variables (panes must strip inherited host and agent ones), cannot set a
login-shell argv0, injects its own variables, blocks in `Drop` waiting for the
child, and exits the process on a failed resize. `crates/shepr-pty/src/` owns the PTY on
libc instead: `command.rs` (`PtyCommand`: configured pane shell with no extra
arguments, optional login argv0, full env control and cwd) builds the launch,
`backend.rs` opens the PTY and forks the child as a session leader with the PTY
as controlling terminal, and
`actor.rs`/`fd.rs` own the master fd, the IO loop and resizing.

Pane spawns run on the server's event loop, so nothing on the parent side of a
launch touches the user's filesystem. A hung mount must stall only its own
pane. `backend.rs` forks the child itself (not through `std::process::Command`,
whose `spawn` waits for the child's chdir and exec) and returns with a pidfd-backed child handle.
The child closes every inherited fd, does the chdir (with the
`HOME`/passwd-home/`/` fallback unless the cwd is required, as for restored
panes and agent resumes) and execs the absolute shell path config validation
resolved. It reports `ChdirOk` or a chdir or exec errno over a status socket it
connects after the fork (`launch.rs`). A child-made socket is one no other fork
can inherit. In shepr-mux, `pane/launch_status.rs` settles each launch from
those reports. Exec committed while the child lives opens observation of the
child (`ChildLiveness::live_process_id`) and starts detection. A reported failure leaves the pane as a
placeholder that says why. The child watcher, the PTY reader and the
runtime's teardown only record how the pane ended with its exit arbiter
(`pane/exit_arbiter.rs`), the first recording winning. The launch
coordinator is the pane's one publisher: it sends the settlement, then the
recorded death, so the settlement always reaches the app first whoever
decided. An ending recorded while the child may still be alive (a failed
reader or wait, or a closed terminal whose child was not reaped in time)
gives the launch a bounded time to settle, then settles it as
unconfirmed, so a child stuck in its chdir cannot keep the pane from ending.
The child is a `PaneChild`, which owns the process identity acquired immediately
after fork and its cached wait status. Mux shares that identity for liveness
and teardown instead of opening another handle. The pane runtime watches its
pidfd and asks `PaneChild` to reap with `waitid` when available, with a blocking
`waitpid` as the fallback. If the watcher is dropped before reaping, the child is handed to a
detached reaper thread.

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of the rules. They wrote them, so they know them.
- The user can exempt you from any rule at any time.

### Bash rules

- Never read or write from `/tmp`. All data lives in the project.
- Never run raw `cargo`, `curl`, `pkill`. Use `brokkr`.

## Document folders

The standing layout, across every project. Three live folders plus one retired,
split by durability first, subject second.

| Folder | Contents | Rule |
|---|---|---|
| `reference/` | Durable in-repo reference for anyone working on or with the code - how the thing is built and why: `architecture.md`, `technical-implementation-spec.md`, `performance.md` (the durable record of measured numbers over time), invariants, protocol contracts | Citable from source as a source of truth. What it says must be true. |
| `docs/` | Durable in-repo documentation of how the thing is used - guides, CLI reference, the consumer-facing API surface. Sometimes exposed as a hand-edited VitePress gh-pages site | Same must-be-true rule. |
| `notes/` | Transient - work items (`todo.md`), future plans, hypotheticals, bug reports, research, analysis. Things that will die | No truth guarantee. Nothing durable cites it. |
| `plans/` | Retired | Plan documents are transient: they go in `notes/`. |

`reference/` and `docs/` are both durable and both binding. The difference is
subject, not audience: `reference/` covers how the thing is built and why - what
you need in order to change it safely - while `docs/` covers how it is used. A
developer or library consumer reads both. Where a project publishes a site,
`docs/` is what gets published; the folder means the same thing either way.
`notes/` is neither durable nor binding, which is the whole point of keeping it
separate: a document that may be wrong must not sit where a document that must
be right is expected.

The dependency direction is therefore one-way. `notes/` may cite `docs/` and
`reference/`; nothing durable may cite `notes/` - not a code comment, not
`docs/`, not `reference/`. A code comment must carry its full context, because
it outlives the note.

**Root-level convention files are exempt.** `AGENTS.md`, `CLAUDE.md`,
`README.md`, `LICENSE`, `CHANGELOG.md` and their kin are found by tooling and by
convention at the repository root, and stay there. These folders govern
documents we chose where to put, not files whose location is dictated.

In `notes/`, `docs/` and `reference/` alike, avoid citing source line numbers -
they drift fast.
