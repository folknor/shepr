# Hygiene: policy invented per call site, and code that is no longer load-bearing

This file consolidates the findings of the nine-scope hygiene hunt for two of the
eight questions the hunters were asked: question 7 (one rule implemented
independently wherever it was needed, ambient dependencies reached from logic,
shared mutable state whose safety rests on call order, unbounded resources,
secrets in diagnostics, test-only shortcuts production can reach) and question 8
(modules, functions, flags and configuration keys that are no longer
load-bearing). Findings about duplicated or unfindable values, output channels
and error handling, and tests, guards and stale claims are filed in sibling
documents; live defects are in `notes/bugs.md`. This is a working document
assembled from reading, not from running anything: entries may be wrong, and a
later fix pass is expected to find phantoms. Where two hunters read the same
thing differently, or where a hunter marked a claim as an unverified inference,
the entry says so.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGP-151 - Renaming an agent while its resume is pending can cost it the name

`shepr-mux/src/terminal/state/names.rs` holds a restored agent's saved name
through its pending resume and adopts whatever session the resumed agent first
reports. But renaming the pane while the resume is still pending attaches the
saved session to the name again, so a resumed agent that then reports a fresh
session id loses the name. Narrow; make a rename during a pending resume keep
the adopt-first-session behaviour.

## HYGP-066 - Clock seam residue

The clock seam (time passed in, each converted subsystem held by a scoped
textlint) now covers `shepr-server/src/app/`, the headless loop, mux
`persist/`, `shepr-remote` (with marked I/O timing exceptions), the platform
deadline helpers, the client endpoint and activation paths, the agent version
probe and the vt synchronized-update timeout. Open:

- Textlints hold the headless loop, `shepr-platform`, `shepr-agent`,
  `shepr-client` and the vt/pty timing paths. `ClientShellState` mixes explicit
  `now` parameters with an ambient `self.now` that only the input and event
  entry points refresh, so a test that bypasses them runs on the construction
  time; pass `now` explicitly or refresh it in one place.
- `shepr-mux/src/terminal/state/`: `TerminalState`'s hook, session and
  lifecycle paths read `Instant::now()` inline (`lifecycle.rs` hook-clear and
  `release_agent_with_mutation`, several sites in `hooks.rs` and `sessions.rs`,
  the `set_detected_state_with_mutation` wrapper in `detection.rs`, and a
  `serde(skip, default = "Instant::now")` field in `mod.rs`). Take `now` as a
  parameter at those entry points and have callers pass the app clock.
- Other remaining reads: `shepr-config`'s `TerminalId::alloc` (HYGV-087),
  `shepr-server/src/server/client_transport.rs` and the `shepr-api` transport
  deadlines.
- Tests still sleeping on real time, each with a reason recorded at the site:
  platform process, clipboard helper, bridge and D-Bus tests; mux runtime (50 ms
  and 20 ms); client `handshake.rs` and `terminal_geometry.rs`; server
  `app/mod.rs`, `tab_bar_status.rs` and `client_transport.rs`.

## HYGP-005 - The working directory is a silent dependency on two paths

**Decision (partial):** the owner adopted broadarrow's rule that every child
process gets a stated working directory (a `clippy.toml` seal on
`std::process::Command::new`, with tests spawning through one helper that sets a
scratch working directory; B6 in `notes/broadarrow-ports.md`). That settles the
direction here - a test's directory comes from a `ScratchDir`, never from where
the runner was invoked - but the seal covers spawned children only and catches
none of these sites: `std::env::current_dir()` read as an input and the
`Path::new(".")` fallback are other spellings (A5's dot-directory textlint needs
a name after the dot). The platform IPC path and the mux fixtures are fixed.
Open, all in tests:

- `shepr-server/src/app/actions/tests.rs` reads `current_dir()` twice, and
  `shepr-server/src/test_support.rs` derives a fixture cwd from `current_dir()`
  with a `/` fallback.
- Fixed `/tmp` path literals in server fixtures (`app/mod.rs`,
  `app/actions/tests.rs`, `ui/panes.rs`).
- `src/cli.rs` binds a used `IsolatedEnv` as `_env`.

## HYGP-018 - The environment handed to panes is inherited wholesale and scrubbed by a denylist split across crates

Residue. `shepr-mux/src/pane/launch.rs` now has an exhaustive per-variable pane
policy over the whole `shepr-core` registry, held by a test, and the unsafe
`remove_var` is gone. Open: panes still inherit the server environment
wholesale (`shepr-pty/src/command.rs::base_env`), and agent variables are still
scrubbed from a separate list owned by `shepr-agent`
(`launch_env_to_scrub`), so the denylist is still split in two.

## HYGP-031 - Test-only code is compiled into production libraries through Cargo feature unification (`test-api`, `test-support`)

**Decision (partial):** piece 4 of the test-isolation work adopted from
broadarrow is landed: no production crate has a `[features]` table any more.
Shared test doubles live in `shepr-test-support` and the new dev-only
`shepr-test-fixtures` crate, server-only fixtures moved into `shepr-server`'s
own `#[cfg(test)]` module, and `brokkr.toml` forbids any normal or build edge to
either dev-only crate (`test-support-never-ships`,
`test-fixtures-never-ships`). An install feature check
(`install_feature_check = "always"`) compiles the shipped feature set the way
`cargo install` resolves it, closing the gate gap. The seam
`PaneRuntimeIo::TestChannel` needed is built: `shepr-pty::ChildIo` is a boxed
trait object `PaneRuntime` holds, with a `PaneOutputWriter` for the real PTY
read path and a `ChannelChildIo` test double in `shepr-test-fixtures`, so the
enum variant and its six `#[cfg]` match arms are gone. `shepr-agent`'s
`resume.rs::test_codex_plan`, `shepr-platform`'s `process.rs::signal_processes`,
the `ServerAddress` `Default` that validation would reject, and
`EventHub::events_after` are deleted outright rather than feature-gated.
`shepr-server`'s crate-wide `#[cfg_attr(feature = "test-api", allow(dead_code))]`
is gone along with the feature, and `dead_code` reports nothing in that crate
today. `#[allow]` gives way to `#[expect(.., reason)]` workspace-wide (B9 in
`notes/broadarrow-ports.md`). Open: `shepr-protocol`'s public id conversions
are test-only again (`PublicTabId`/`PublicPaneId`'s `From<&str>` are
`#[cfg(test)]` and panic on a non-canonical literal instead of the earlier
`unwrap_or_else` fallback), but `TerminalId::test_new`, `WorkspaceId::new` and
`WorkspaceId::from(&str)` remain `pub` and ungated, so any caller can still mint
an identity that is supposed to come from one place.

## HYGP-058 - Dependencies that production code does not use

- `shepr-core` depends on `ratatui`: `layout.rs` uses
  `ratatui::layout::{Direction, Rect}` and `brokkr.toml` allows it, putting a TUI
  rendering crate at the bottom of the layering where `shepr-mux`,
  `shepr-protocol` and `shepr-config` all inherit it. `Rect` and `Direction` are
  four `u16`s and a two-variant enum; owning them in `shepr-core` alongside
  `GridSize` would drop `ratatui` from the bottom four crates' dependency closure
  and remove a re-export the wire types currently share with the renderer.
