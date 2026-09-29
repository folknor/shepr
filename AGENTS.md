# shepr

A personal, Linux-only fork of [herdr](https://github.com/herdrdev/herdr): a
terminal multiplexer for AI coding agents. Workspaces, tabs and panes run in a
headless server; the TUI is a client. An agent sidebar shows every agent's
state (idle, working, blocked) across all panes, including panes on
other hosts reached over SSH.

There is no compatibility with upstream herdr installs. The fork is stripped
hard: the goal is the smallest code surface that does what the owner uses,
not parity with upstream.

shepr has never been run: no config, catalog, session or other on-disk state
exists anywhere, so there is nothing to stay compatible with. Remove legacy
fields and migration code freely.

## Scope

Kept:

- Server/client split, local and SSH endpoints (one client shows servers on
  several hosts; the owner installs the binary on each host manually, and a
  host never has more than one `shepr` installed)
- Terminal core: `alacritty_terminal` for emulation, a small libc PTY layer
  (`crates/shepr-pty/src/`), PTY hosting
- Workspaces, tabs, panes, layout, the tab bar, the agent sidebar
- Agent detection from bundled manifests (`crates/shepr-agent/src/detect/manifests/*.toml`),
  plus local override files and `shepr server reload-agent-manifests`
- Agent integrations (`crates/shepr-agent/src/integration/`): hooks installed into each agent's
  own config that report state and session IDs back to shepr
- Session restore (layout saved to disk, rebuilt with fresh shells) and agent
  resume on restore
- Git status in the sidebar (branch, ahead/behind)
- Mouse selection, copy mode, keybinding help, window title templating
- The JSON API over the server socket; every CLI subcommand that acts on a
  running server goes through it. Commands that manage local state (`config
  check`, `session list/delete`, `integration`, `machine`, `agent explain
  --file`) run in the CLI process and cannot be sent with `--machine`

shepr is for overseeing agents across machines, not for driving them.
Launching or steering agents through shepr (agent start, managed agents,
agent prompt, agent send-keys and agent wait) is deliberately not kept. Pane
driving commands (send-text, send-keys, run, wait-for-output and input) and
their JSON API methods are also not kept; pane.input.set remains for the TUI
context menu.

Config is read and validated once at launch. There is no reload. Any config
problem fails the launch; no fallbacks. Directories follow the XDG spec.
Two things qualify that:

- A client validates each server's config again when it decodes the attach
  snapshot, with the checks that only mean something on the sending host
  (the new-pane cwd exists, the shell resolves) skipped. The server's config
  crosses hosts, and the client rebuilds its runtime values from it; the
  build-identity handshake is what guarantees both ends run the same
  validator.
- Detection manifest overrides are the one input that reloads, through
  `shepr server reload-agent-manifests`. A bad override fails the launch;
  on reload it leaves that agent on its bundled manifest and the reply
  carries the warning.

Agent states are Working, Blocked and Idle. Unknown presents as Idle.

Saved machines are add/remove only. Unreachable ones fail soft. With saved
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
- `shepr-protocol`: compact wire types and codec.
- `shepr-config`: configuration parsing and validation.
- `shepr-api`: JSON API schema, client and server transport.
- `shepr-termio`: terminal input and copy mode.
- `shepr-remote`: saved machines and SSH connections.
- `shepr-mux`: terminals, panes, workspaces, Git state, events and persistence.
- `shepr-server`: application state, UI and serving.
- `shepr-client`: endpoint management and TUI presentation.

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

Every build profile uses the same config, state and runtime directories.
What keeps a dev run apart from the installed server is a named session, and
what keeps them from talking is the build identity: it covers the build
profile as well as the source, so a dev and a release build of one tree
differ, and a server of another build is always refused with guidance. To
try a new build from inside a running shepr session, give it a session of
its own:

`env -u SHEPR_SOCKET_PATH -u SHEPR_CLIENT_SOCKET_PATH brokkr run -- --session dev [<command>]`

- Use the `--session` flag, not `SHEPR_SESSION`: only an explicit
  `--session` outranks a socket override.
- The `env -u` prefix drops the socket overrides every pane exports, so the
  dev build resolves its sockets from the session alone.
- Without `--session` the dev build targets the installed server's default
  session, and while that server runs the dev build is refused. Its `server
  stop` is refused too, naming both builds; `--force` overrides that and
  stops the installed server with every pane in it.
- A named session has its own saved layout and history. A dev server run
  without `--session` shares the default session's: if the installed server
  is down, it restores that layout and saves over it. That is by design, since
  the session is what separates them. Config and the saved-machine catalog
  are shared whatever the session.

## Principles

- **State is separated from runtime.** `AppState` is pure data, testable
  without PTYs or async. Per-terminal state lives in `TerminalState`
  (in `AppState::terminals`), which is plain data testable without a PTY;
  `PaneRuntime` (held by `App`, outside `AppState`)
  owns the PTY, its tasks and the state shared with them. `PaneState` is only
  the pane's link to its terminal plus per-pane input flags.
- **Render is pure.** `compute_view()` in `crates/shepr-server/src/ui.rs`
  reads `AppState` by shared reference and returns the view its caller stores
  in `AppState::view`; pane runtimes are resized by explicit geometry paths
  (the ones taking a `PaneResizer`), and surface drawing takes shared
  references and only draws.
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
  pane with `shepr agent read <pane> --source detection --format text`, encode
  invariant controls as explicit AND/OR gates, and never use the user-visible
  viewport (users scroll it).
- **Hot paths multiply.** Work reachable from view computation, rendering,
  PTY parsing, detection or client frame fanout runs per byte or event, times
  panes, times clients. Inside those loops use narrow accessors, keep
  terminal-core locks short, and preserve the hidden-pane early exits.
- **No wire compatibility obligations.** Client and server are always the same
  build. Change the protocol freely; there are no frozen fixtures.
- **Wire encoding is shepr's own.** Frames are `[u32 LE length][payload]`,
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
- Tests that touch the process environment hold a
  `shepr_test_support::IsolatedEnv`, and tests that write files use a
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
