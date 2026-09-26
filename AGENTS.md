# shepr

A personal, Linux-only fork of [herdr](https://github.com/herdrdev/herdr): a
terminal multiplexer for AI coding agents. Workspaces, tabs and panes run in a
headless server; the TUI is a client. An agent sidebar shows every agent's
state (idle, working, blocked, done) across all panes, including panes on
other hosts reached over SSH.

There is no compatibility with upstream herdr installs. The fork is stripped
hard: the goal is the smallest code surface that does what the owner uses,
not parity with upstream.

## Scope

Kept:

- Server/client split, local and SSH endpoints (one client shows servers on
  several hosts; the owner installs the binary on each host manually)
- Terminal core: `alacritty_terminal` for emulation, a small libc PTY layer
  (`src/pty/`), PTY hosting
- Workspaces, tabs, panes, layout, the tab bar, the agent sidebar
- Agent detection from bundled manifests (`src/detect/manifests/*.toml`),
  plus local override files and `shepr server reload-agent-manifests`
- Agent integrations (`src/integration/`): hooks installed into each agent's
  own config that report state and session IDs back to shepr
- Session restore (layout saved to disk, rebuilt with fresh shells) and agent
  resume on restore
- Git status in the sidebar (branch, ahead/behind)
- Mouse selection, copy mode, keybinding help, window title templating
- The JSON API over the server socket; every CLI subcommand that acts on a
  running server goes through it. Commands that manage local state (`config
  check`, `session list/delete`, `integration`, `machine`, `agent explain
  --file`) run in the CLI process and cannot be sent with `--machine`

Config is read and validated once at launch. There is no reload.

## Build and test

`brokkr` is the only entry point. Never run raw `cargo`.

| instead of | run |
|---|---|
| `cargo build` / `cargo clippy` / `cargo test` | `brokkr check` (gremlins + clippy + tests, the gate) |
| `cargo test <name>` | `brokkr test <name>` |
| `cargo run -- <args>` | `brokkr run -- <args>` |
| `cargo fmt` | `brokkr fmt` |
| `cargo install --path .` | `brokkr install` |

- `brokkr check` is the gate; run it before every commit.
- `brokkr test <name>` is a substring filter over unit and integration tests,
  release profile by default (`--debug` for dev).
- `brokkr man` lists the bundled docs (`man check`, `man config`, `man run`,
  ...). Read those rather than guessing at flags.
- Never run two brokkr/cargo invocations at once. Subagents read and edit
  code; building and testing happens in the main conversation.

When testing a new build from inside a running shepr session, clear the
inherited socket overrides so the debug binary talks to its own server:
`env -u SHEPR_SOCKET_PATH -u SHEPR_CLIENT_SOCKET_PATH brokkr run -- <command>`.

## Principles

- **State is separated from runtime.** `AppState` is pure data, testable
  without PTYs or async. `PaneState` is separate from `PaneRuntime`.
- **Render is pure.** `compute_view()` handles geometry and mutations;
  `render()` takes `&AppState` and only draws.
- **No god objects.** `app/` is split into state, actions and input; keep it
  that way.
- **Linux only.** No `#[cfg(windows)]`, `#[cfg(target_os = "macos")]` or
  `cfg!` branches for other platforms. libc, `/proc` and helper-program
  plumbing lives in the flat `src/platform/` module (`mod.rs`, plus
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
  and payloads use the positional serde codec in `src/protocol/codec.rs`
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
  `crate::test_support::IsolatedEnv`, and tests that write files use a
  `ScratchDir` from the same module, never fixed or shared temp paths.

## Terminal core

The emulator is `alacritty_terminal`, pinned with `=` in `Cargo.toml` (bump it
deliberately, never through a loose requirement). Everything that touches it
lives in `src/ghostty/` (the name predates the switch; a rename to `vt` is
planned): `mod.rs` is the adapter the rest of the tree uses, `format.rs` the
plain/VT formatters used for reads and history persistence, `scan.rs` a scanner
for sequences alacritty ignores (OSC 7, modes 9/1016/2031/2048, CSI ? 996 n,
CSI 16 t, XTGETTCAP, modifyOtherKeys) and for the halfwidth katakana voiced
marks U+FF9E/U+FF9F, which unicode-width calls zero-width but terminals give
their own column. Alacritty types must not leak out of that module. When
writing against its API, read the source instead of relying on memory: a
checkout of the pinned `alacritty_terminal` release and the matching `vte` live
under `research/` (`research/alacritty/alacritty_terminal/`, `research/vte/`).

PTYs do not use `alacritty_terminal::tty`: it can only add environment
variables (panes must strip inherited host and agent ones), cannot set a
login-shell argv0, injects its own variables, blocks in `Drop` waiting for the
child, and exits the process on a failed resize. `src/pty/` owns the PTY on
libc instead: `command.rs` (`PtyCommand`: argv or login shell, full env
control, cwd) builds the launch, `backend.rs` opens the PTY and spawns the
child as a session leader with the PTY as controlling terminal, and
`actor.rs`/`fd.rs` own the master fd, the IO loop and resizing. The child is a
plain `std::process::Child`, reaped by a blocking `wait()` in the pane runtime.
