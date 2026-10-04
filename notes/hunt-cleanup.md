# Cleanup from the design hunt

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

Dead code, dead state, test-only twins of production rules, public surface
that exists only for tests, unused dependencies and stale documentation. Each
entry is a deletion or a rewording rather than a redesign.

## Dead code and dead state

## CLN-023 - The client shell's visibility pass is unfinished

The narrowing pass covered `endpoints.rs`, `state.rs`, `ledger.rs`, `copy/`,
`overlays/mod.rs` and `sidebar/`. Still open: `view/`, `presentation/`,
`notices/`, `navigation/`, `input/` and `transitions.rs` keep blanket
`pub(in crate::shell)` items, item by item, and several `CopySession` fields
stay shell-wide only because tests in `shell/tests/` read them (moving those
tests into `copy/` lets them narrow). No dead-code sweep followed the pass.

## CLN-032 - Leftovers from the bug round

- `crates/shepr-launch/src/local_server.rs`: `From<LaunchError> for io::Error`
  may have no production caller now that the remote bridge host classifies
  launch errors itself; `running_server_status` still turns `Unresponsive`
  into `io::Error::other`, dropping the typed error.

## CLN-033 - Leftovers the specs did not reach

Unverified after the spec landings: check each before acting.

- `crates/shepr-mux/src/workspace.rs`: `Workspace::test_from_pane` may still
  overwrite the record's public number with `FIRST`;
  `resolved_identity_cwd_from_root_pane` returns a plain path now, check its
  callers' leftover `?`.
- `crates/shepr-mux/src/pane/runtime.rs`: `PaneRuntime::current_size()` and the
  server test fixture's `current_size` may duplicate `grid_size()`.
- `crates/shepr-client/src/shell/endpoints.rs`: `endpoint_choice_mut()` is
  public only for the netside test.
- Runtime events may be admitted twice on the server's pane-exit path.

## CLN-034 - Shell leftovers outside the render spec

Unverified after the render spec landed: check each before acting.

- `ClientShellConfig::agent_panel_sort` is changed at runtime and duplicates
  `agent_panel_sort_chrome`.
- The composition commits before the host write, so a failed write can start a
  notice's lifetime for a frame never shown.
- `handle_resize` writes the size twice.

## Test-only twins and test seams in production

## CLN-035 - A resumed runtime's focus-in has no test

`sync_pane_focus_after` re-sends focus-in to panes whose runtime an agent
resume replaced, but no test covers it: the resume launches a real PTY shell,
which only accepts the focus report once it has turned on focus reporting, and
nothing lets a test observe a launched runtime's input. A seam for that (or a
way to pre-enable focus reporting) would let
`a_resumed_runtime_in_a_focused_pane_is_told_focus_in` be written.
(app-loop spec)
