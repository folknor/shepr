# shepr

A personal, Linux-only fork of [herdr](https://github.com/herdrdev/herdr): a
terminal multiplexer for AI coding agents. Workspaces and panes run in a
headless server; a workspace is one pane layout (there are no tabs); the TUI is a client. An agent sidebar shows every agent's
state (idle, working, blocked) across all panes, including panes on
other hosts reached over SSH.

There is no compatibility with upstream herdr installs. The fork is stripped
hard: the goal is the smallest code surface that does what the owner uses,
not parity with upstream.

shepr has never been run: no config, session or other on-disk state
exists anywhere, so there is nothing to stay compatible with. Remove legacy
fields and migration code freely.

## Scope

Kept:

- Server/client split, local and SSH endpoints (one client shows servers on
  several hosts; the owner installs the binary on each host manually, and a
  host never has more than one `shepr` installed)
- Terminal core: `alacritty_terminal` for emulation, a small libc PTY layer
  (`crates/shepr-pty/src/`), PTY hosting
- Workspaces, panes, layout, the agent sidebar
- Agent detection from bundled manifests (`crates/shepr-agent/src/detect/manifests/*.toml`),
  compiled into the binary
- Agent integrations (`crates/shepr-agent/src/integration/`): hooks installed into each agent's
  own config that report state and session IDs back to shepr. The server
  installs or updates them at launch for every agent whose config directory
  exists on its host; there is no install or uninstall command
- Session restore (layout saved to disk, rebuilt with fresh shells) and agent
  resume on restore
- Git status in the sidebar (branch, ahead/behind)
- Mouse selection, copy mode, keybinding help, window title templating
- The JSON API over the server socket. The TUI does not act on workspaces
  or panes through it: it sends typed client-socket commands
  (`shepr_protocol::command::EndpointCommand`), none of which is an API
  method. The CLI is local-only: every
  subcommand acts on this host's server or state, and none can be aimed at a
  configured machine. `status`, `server stop`, `detect capture` and `detect explain
  <PANE>` talk to the local server over its socket; `detect explain --file`
  runs in the CLI process

shepr is for overseeing agents across machines, not for driving them.
Launching or steering agents through shepr is deliberately not kept, and
neither is driving panes: no CLI command or API method sends text or keys to
a pane, waits for pane output or reads pane history, and there is no
subscription that fires when text appears in a pane. The one pane input
setting kept is the TUI's own pane.input.set command, for its context menu.
Nor are local detection manifest overrides and their reload: a detection
change ships as a new build.

The CLI is small on purpose. `shepr` with no subcommand attaches the TUI, and
the subcommands are `status`, `server` and `detect`. Workspaces and panes are managed from the TUI only; there
is no CLI group for them, and no CLI attach to a single terminal. `shepr
detect capture <pane>` prints the screen text and OSC title and progress the
detector evaluates for a pane, as JSON that `detect explain --file` reads back,
and `shepr detect explain <pane>` says which rule decided its state.

Config is read and validated once at launch. There is no reload. Any config
problem fails the launch; no fallbacks. Directories follow the XDG spec.
One thing qualifies that: each connection's handshake welcome carries the
server's config, and the client validates it again when it decodes the welcome,
with the checks that only mean something on the sending host (the new-pane cwd
exists, the shell resolves) skipped. The server's config crosses hosts, and the
client rebuilds its runtime values from it; the build-identity handshake is
what guarantees both ends run the same validator. The config belongs to the
accepted connection generation: it arrives once per connection (a reconnect may
carry a different one), each endpoint keeps the one it was last given, the
client installs it before it processes that generation's snapshots, and a
handoff applies the destination endpoint's config at the presentation
transition. A config that fails to decode fails that handshake and shows as
that endpoint's Attention diagnostic; the other endpoints stay usable.

Agent states are Working, Blocked and Idle. Unknown presents as Idle.

Machines are configured in config.toml as `[[machines]]` entries (a `label` and
an `ssh` target), read once at launch like the rest of the config; there are no
commands to add, remove or list them. The TUI connects to them without
prompting (BatchMode), so at startup, before it takes the terminal, `shepr`
checks every machine and runs interactive ssh for each one that needs
authentication, one at a time, on shepr's own control socket, then checks those
again; there is no command for it. Host keys are never accepted automatically.
A running server of a different build, the local one or a machine's, is then
offered a restart in one pass after the last authentication prompt: the
question says that the restart ends the server's pane processes and that the
layout is restored with fresh shells and agents resumed. Consent is asked on
the terminal and defaults to keeping the server; with no terminal, or on
refusal, the server is left running and unavailable and shepr says how to stop
it. Guidance uses `shepr` for a release build and the running executable path
for a dev build (or `brokkr run --` if the path cannot be resolved), with the
selected socket override. The stop names the boot identity that was observed,
so a server that replaced it in the meantime is not stopped, and is offered
again as a new occupant. Unreachable machines fail soft. With
machines configured, losing the local server does not end the client either:
it keeps serving the remote machines and reconnects once the local server is
restarted.

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

- `shepr-core`: shared geometry, layout and plain types.
- `shepr-platform`: Linux process, filesystem, IPC and terminal plumbing.
- `shepr-vt`: terminal emulation and read formatting.
- `shepr-pty`: PTY process launch and IO.
- `shepr-test-support`: shared environment isolation and scratch directories for tests.
- `shepr-agent`: detection manifests and agent integrations.
- `shepr-config`: configuration parsing and validation.
- `shepr-protocol`: compact wire types and codec; it depends on `shepr-config`
  because the handshake welcome carries the server's validated config.
- `shepr-api`: JSON API schema, client and server transport.
- `shepr-termio`: terminal input and copy mode.
- `shepr-remote`: configured machines and SSH connections.
- `shepr-mux`: terminals, panes, workspaces, Git state, events and persistence.
- `shepr-server`: application state, UI and serving.
- `shepr-client`: endpoint management and TUI presentation.
- `shepr-daemon`: the `shepr-server` executable, a thin `main` over
  `shepr-server` (the one package the client binary never links).

`shepr-test-fixtures` (dev-only) sits above config, protocol, pty and termio,
so only crates above those can take it.

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
layout, history, server log, lease). A release build keeps the plain XDG
locations; a dev build uses sibling `shepr-dev` directories, so it has its own
sockets, saved layout and history with no flag. Config, machines included, is
shared by every profile. The build identity also covers the profile
as well as the source, so a dev and a release build never talk to each other's
server: one that is reached anyway is refused with guidance naming the current
profile's entry point (`shepr` for release, the running executable path for
dev).

Run it with plain `brokkr run -- [<command>]`, including from inside a pane
of the installed server. The dev client launches the `shepr-server` beside it
in `target/debug`, which `brokkr run` does not build: build it first with
`brokkr run shepr-server -- --version`, or through `brokkr check`. Every pane exports
`SHEPR_SOCKET_PATH` and `SHEPR_CLIENT_SOCKET_PATH` as its server resolved them,
which normally win over the per-profile runtime directory, and also
`SHEPR_BUILD_PROFILE`, the profile (`release` or `dev`) of the server that owns
the pane. A process whose own profile differs from that marker ignores both
socket variables and resolves its own profile's runtime directory.
`SHEPR_SOCKET_PATH` normally selects the API socket and derives the client
socket. When both variables are set and the API path is exactly the profile's
runtime `shepr.sock`, `SHEPR_CLIENT_SOCKET_PATH` selects the client socket. This
keeps a nested client on a server started with only a client socket override.
A non-runtime API path still takes precedence, so a user can set
`SHEPR_SOCKET_PATH` inside a pane to select another server. The API variable
stays exported because every agent integration reports through it.

- Socket variables with no marker (set by a user or a script) and ones with a
  matching marker still win over the runtime directory.
- A marker that is neither `release` nor `dev` fails the launch.
- The saved layout is not affected by the overrides, only the sockets are.
- `server stop` stops whatever server answers, whatever its build, with every
  pane in it. Its hidden `--expect-boot <boot id>` makes the stop conditional:
  the server compares the id (from its `status server` output) with its own
  boot and refuses a stop aimed at another one, so a server that replaced the
  observed one keeps running (exit status 3).

## Principles

- **State is separated from runtime.** `AppState` is pure data, testable
  without PTYs or async. Per-terminal state lives in `TerminalState`
  (in `AppState::terminals`), which is plain data testable without a PTY;
  `PaneRuntime` (held by `App`, outside `AppState`)
  owns the PTY, its tasks and the state shared with them. `PaneState` is only
  the pane's link to its terminal plus per-pane input flags.
- **Render is pure.** `compute_surface_for()` in
  `crates/shepr-server/src/ui/surface.rs` reads `AppState` by shared
  reference and returns one workspace laid out for one client's surface; pane
  runtimes are resized by explicit geometry paths (the ones taking a
  `PaneResizer`), and surface drawing takes shared references and only draws.
- **Presentation is per client.** Each connection on the server keeps its
  own surface size, outer focus, location and window title; nothing projects
  one client's view into `AppState`. What panes have one of is decided from
  all the views in one place each: PTY size by the PTY size rule
  (`workspace_geometry_source` in `crates/shepr-server/src/server/headless/client_views.rs`,
  which records each workspace's applied area in `AppState`), pane focus reports by
  `sync_pane_focus`, and the host theme by the foreground client (the one
  last active).
- **No god objects.** `AppState` lives in `crates/shepr-server/src/app/state.rs`;
  `App` behavior is organized across modules under
  `crates/shepr-server/src/app/`. Keep it that way.
- **Linux only.** No `#[cfg(windows)]`, `#[cfg(target_os = "macos")]` or
  `cfg!` branches for other platforms. libc, `/proc` and helper-program
  plumbing lives in the flat `crates/shepr-platform/src/` crate (`lib.rs`, plus
  self-contained submodules such as the logind shutdown monitor); there is no
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
provides the plain/VT formatters used for reads and history persistence;
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
libc instead: `command.rs` (`PtyCommand`: argv or login shell, full env
control, cwd) builds the launch, `backend.rs` opens the PTY and spawns the
child as a session leader with the PTY as controlling terminal, and
`actor.rs`/`fd.rs` own the master fd, the IO loop and resizing. The child is a
plain `std::process::Child`. The pane runtime watches its pidfd and reaps it
with `waitid` when available; `Child::wait` runs in a blocking task as the
fallback. If the watcher is dropped before reaping, the child is handed to a
detached reaper thread.

## Upstream tracking

These parts of shepr still follow upstream herdr, and fixes to them are
ported from it. `scripts/upstream_watch.py` reports upstream changes to their
upstream counterparts since the commit in `scripts/upstream_baseline.txt`;
its docstring says what each upstream path maps to.

- `crates/shepr-agent/src/agent/`
- `crates/shepr-agent/src/detect/`
- `crates/shepr-agent/src/detect/manifests/`
- `crates/shepr-agent/src/integration/`
- `crates/shepr-agent/src/integration/assets/`
- `crates/shepr-client/src/input.rs`
- `crates/shepr-mux/src/pane/agent_detection.rs`
- `crates/shepr-mux/src/terminal/state/`
- `crates/shepr-server/src/app/`
- `crates/shepr-termio/src/input/raw_input.rs`
- `src/autodetect.rs`

The list is the shepr side of the script's tables; the
`upstream-watch-paths` check fails when the two differ.

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
