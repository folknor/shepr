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
entry is a deletion or a rewording rather than a redesign. Unverified: the
raw reports are in the commit that precedes this file's.

## Dead code and dead state

## CLN-023 - The client shell has no item-level visibility pass

Every item in `crates/shepr-client/src/shell/` is either `pub(crate)` or the
blanket `pub(in crate::shell)`, none narrower, even where only the parent
module uses it. `OverlayRender`, `render_client_overlay`, `render_global_menu`
and `render_context_menu` in `overlays/mod.rs` are `pub(crate)` with callers
only inside the shell, as are most items in `endpoints.rs`, `state.rs` and
`sidebar/token_definitions.rs`. (`fixed_keys.rs` and `selection_render.rs` are
the exceptions to "none narrower".) An item-level visibility pass would make
the module tree mean something. Spec: `notes/spec-shell-render.md` landing 9.
(wave-7 review)

## CLN-031 - Leftovers from the eleventh light-loop wave

- `crates/shepr-platform/src/config_file.rs`: `create_config_temporary` and
  `write_config_temporary` are used only by platform tests now that the agent
  integration publishes through `PreparedFile`.
- `crates/shepr-platform/src/publish_file.rs`: `publish_file()` has no caller;
  the module has no tests of its own (hard-link no-clobber, withdraw on
  directory-sync failure, symlink refusal); the non-replace path uses
  `hard_link`, which fails on filesystems without hard links.
- `crates/shepr-remote/src/remote/machine_ssh.rs`: `MachineProbe::ensure_ssh`
  duplicates `ensure_managed_ssh_config` for the connector state.
- `crates/shepr-server/src/app/api/detect.rs`: `handle_detect_capture` repeats
  `json_pane`'s parse and error because it needs the parsed id.
- `crates/shepr-protocol/src/identity.rs`: `RequestId::allocate` uses a plain
  `fetch_add` that would wrap at `u64::MAX` (unreachable, but undocumented now).
- `crates/shepr-protocol/src/ids.rs`: the comment above the public-number
  helpers states a plan (keep the exports until a mux test changes) rather than
  a fact; the mux workspace test could assert through the public id types and
  the helpers could narrow.

(wave-11 review and gate)

## CLN-032 - Leftovers from the bug round

- `crates/shepr-launch/src/local_server.rs`: `From<LaunchError> for io::Error`
  may have no production caller now that the remote bridge host classifies
  launch errors itself; `running_server_status` still turns `Unresponsive`
  into `io::Error::other`, dropping the typed error.
- `crates/shepr-server/src/app/actions/events.rs`: the private
  `HookReportKind`, used only as a log tag, duplicates
  `shepr_detect::ownership::HookReportKind`.
- `crates/shepr-git/src/refresh.rs`: `GitRefresher` is public but used only
  inside shepr-git.

(bug round)

## CLN-033 - Leftovers the specs absorb

Small findings of the spec writers that a spec landing already deletes; listed
so they are not hunted again before then.

- `notes/spec-data-model.md`: `PaneRemovalCommit::Stale` and its warning are
  unreachable (both phases always run in one call); `capture_preserved_layout`'s
  pane-count check guards a pane that cannot exist; `commit_new_pane`'s focus
  knob is dead; `Workspace::mark_identity_undiscovered` has no production
  caller; `resolved_identity_cwd_from` returns an `Option` that is always
  `Some`; `Workspace::test_from_pane` silently overwrites the record's public
  number with `FIRST`; `AppState::move_workspace` is `pub` beside
  `pub(crate)` siblings.
- `notes/spec-app-loop.md`: `AppState::clock_now` is read only by one test;
  `test_headless_server` duplicates `HeadlessServer::new`'s field list; the
  headless test submodule declarations carry redundant `cfg(test)` and `path`
  attributes, one missing its `cfg`; runtime events are admitted twice on the
  pane-exit path; `observe_projection_change` gives the loop arbitrary `&mut
  App` access.
- `notes/spec-pixel-geometry.md`: mux's "pixels without 1016" encoder branch is
  dead; `PaneGeometry::cell_width` and `cell_height` are dead;
  `PaneRuntime::current_size()` is an alias of `grid_size()`; host cell values
  are validated twice.
- `notes/spec-shell-requests.md`: the "endpoint not active" drop branches in
  `dispatch.rs` and `settle_expired_endpoint_commands` never find a ledger
  entry (every change of presented endpoint resets the ledger first), and
  `DropReason::Interrupted`'s doc describes that unreachable case;
  `dispatch_client_shell_actions` returns a `Result` that never fails;
  `focus_endpoint_target` throws away its outcome's repaint;
  `ScrollLanes::sent` overwrites the whole lane, relying on callers never
  sending with a flight out; `endpoint_choice_mut()` is public only for the
  netside test.
- `notes/spec-shell-render.md`: about 790 lines of navigator tests live in
  `shell/tests/copy.rs` (moved in landing 7).

## CLN-034 - Shell leftovers outside the render spec

- `ClientShellConfig::agent_panel_sort` is changed at runtime and duplicates
  `agent_panel_sort_chrome`.
- `compose` commits before the host write, so a failed write can start a
  notice's lifetime for a frame never shown.
- `handle_resize` writes the size twice.

(spec C findings)

## Test-only twins and test seams in production

## Unused dependencies and edges

## Stale documentation
