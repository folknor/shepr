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

## CLN-009 - Dead pieces in the client shell

`ClientShellWorkspace.custom_label` (defined in
`crates/shepr-protocol/src/projection.rs`) is not read anywhere in shell
production code; drop it from the projection or find the reader it was meant
for. (client-shell)

## CLN-010 - Dead pieces in the edges

- `DifferentBuildServer.version` is populated and stored but read only in
  tests (one initializer is in `src/preflight.rs`).
- `ServerStatusJson.compatible` and `.restart_needed`
  (`crates/shepr-api/src/schema/server.rs`) are still written by
  `src/cli/status.rs` while the remote preflight parse
  (`crates/shepr-remote/src/remote/server_lifecycle.rs`) recomputes
  compatibility from `build_id`; drop the fields with that consumer.
- `forward_remote_bridge_stdio_with_timeout` in
  `crates/shepr-platform/src/remote_bridge_io.rs` takes `Option<Duration>`;
  production always passes `Some`, and only a test passes anything else.

Reported by edges; contracts also notes the doubled compatibility derivation.

## CLN-011 - Dead pieces in the contracts crates

`RuntimeStatus::version: Option<String>` is always `Some` (ping decoding in
`crates/shepr-api/src/client.rs` supplies it). Its consumers in
`crates/shepr-remote/src/remote/local_server.rs` and the fixtures in
`src/preflight.rs` still format absence as `"unknown"`; make the field plain
and drop those branches together. (contracts)

## CLN-014 - Dead pieces in mux state and core layout

`ProcessIdentity::tag(token)` is always called with `0`, and the sweep accepts
only `Some((owner, 0))` from `parse_tag`: the token field is dead format.
Removing it means changing the tag writers and the sweep parse together
(`crates/shepr-platform/src/process_identity.rs`). (foundation)

## CLN-023 - Small leftovers from the first fixes

- `crates/shepr-remote/src/remote/local_server.rs`:
  `wait_for_overridden_server` repeats the poll loop of
  `wait_for_server_socket_to_settle_until` (same deadline arithmetic and
  sleep); it could call that function and then probe once.
- `src/cli/status.rs`: `type ServerRuntimeStatus = ServerPresence;` only
  renames; the functions could take `ServerPresence`.
- `crates/shepr-client/src/shell/sidebar/endpoint_sidebar.rs`:
  `.workspaces.iter().enumerate().map(|(entry, _)| ..)` is `0..len` spelled
  the long way.

- `crates/shepr-server/src/app/`: `AppPolicy::Test` is kept as a const equal
  to `Suspended`, with a comment calling Suspended a different state, while
  every test teardown now spells `Suspended`. One spelling should win.

- `crates/shepr-remote/src/remote/preflight.rs`: `RestartResult::offer` checks
  for a missing decision callback before its loop and again inside it, so the
  inner `NoTerminal` branch cannot be reached.
- `crates/shepr-client/src/shell/state.rs`: `MouseSelection` is built field by
  field in `ClientShellState::new`; a derived `Default` would do.

- `crates/shepr-client/src/shell/`: most items are `pub(in crate::shell)` where
  only their parent module uses them, and `OverlayRender` and
  `render_client_overlay` in `overlays/mod.rs` are `pub(crate)`; an item-level
  visibility pass would make the new module tree mean something.
- `Notices::queue_boot` calls `dismiss()` to show the first queued card; an
  `advance` name would say what it does.

- `crates/shepr-server/src/app/`: `AppPolicy` and the saver's `SavePolicy` are
  now separate state, and about twenty tests still end with
  `app.policy = Suspended`, which no longer touches the saver.
- `crates/shepr-mux/src/pane/runtime.rs`: every pane input encoder exists both
  mode-free and `_with_modes`; the mode-free public ones look used only by tests.

(wave-1 review and gate, wave-3 review, wave-5 fixer and review, wave-7 and wave-8 reviews)

## CLN-024 - Leftovers in the terminal input crates

- `KeybindMatch` (generated in `crates/shepr-termio/src/input/keybindings.rs`)
  has the single variant `Action(KeybindAction)`, matched at some 46 client
  sites.
- `InputLeaseTable::normalize_press` returns its input unchanged; it only drops
  an old lease.
- `HostReplyPolicy` in `crates/shepr-termio/src/input/raw_input.rs` is a trait
  with one implementation and `NoHostReplies` is an alias of `HostReplies`, so
  the policy generic on `RawInputFramer` and `RawInputByteFramer` can go.
- `KeyboardProtocol::from_kitty_flags(u16)` is public but used only by tests
  (about 60 sites) now that `from_flags` exists.

(wave-6 review)

## Test-only twins and test seams in production

## CLN-016 - Production rules with a test-only twin that the tests exercise instead

- `PaneTerminal::plain_page_keys_use_host_scrollback` (production) and
  `InputState::plain_page_keys_use_host_scrollback` (`#[cfg(test)]`); the test
  `plain_page_keys_host_scroll_for_shell_like_decckm_with_bracketed_paste`
  calls the twin.
- `terminal_buffer_symbol_into` (render path) and
  `terminal_normalize_buffer_symbol` (`#[cfg(test)]`); the grapheme-width tests
  in `terminal/tests.rs` call the twin. Suggested: a pure
  `normalized_symbol(&str, CellWide) -> &str` both call.
- `PaneTerminal::input_state()` and the `InputState`/`ScrollPosition` types
  exist only for tests, and `input_state` derives mouse mode and encoding with a
  third precedence ladder.
- `RenderSignal::request_pty` (test-only) restates `request_pty_coalesced`
  minus the flag; the coalescing tests test a copy.
- `save_session_before_teardown` (cfg(test)) duplicates the production
  `save_session_before_teardown_async` synchronously, and `save_session_now` is
  another test-only save path.
- `handle_internal_event_after_checkpoint` (cfg(test)) reimplements the
  server's hold-and-replay of checkpointed exits with a fixed `for _ in 0..4`
  loop; session tests run against it rather than the loop's.

There is no agreement test for any of these: the copies can drift with every
test green. Reported by mux-panes, mux-state and server-app.

## CLN-017 - Production types shaped by test fixtures

- `ClientOutbox.attached` (`crates/shepr-server/src/server/outbox.rs`) is a
  production field again, false only for `detached()` fixtures, and
  `ClientRegistry::presenting()` checks it; making it test-only tripped the
  rule against production code below the first test cfg, so the fixture seam
  needs another shape.
- `client_read_loop_with_endpoint_controls(.., Option<&ControlSender>)` is
  always `Some` in production.
- `SurfaceBoundary` holds function pointers for encode and render so a test can
  inject an oversize encode failure.
- `ClientRegistry::get/get_mut/contains_key/Index` are generic over
  `K: Copy + Into<ClientId>` so tests can write `clients.get(&1)` through
  cfg(test) `From<u64>`/`From<i32>`; `ActivityStamp` likewise.
- `ChildLiveness`: `pid` is atomic only for `set_pid_for_test`; pid `0` and
  `leader: None` exist for the `PaneRuntime::with_child_io` fixture seam, so
  production tests `pid != 0` and carries `None` fallbacks.
- `ClientShellEndpoint::snapshot_generation: Option<u64>` is `None` only in
  tests, and `endpoint_snapshot_matches` accepts it with `is_none_or`;
  `type SurfaceGeneration = Option<u64>` and the cfg(test)
  `receive_pane_surface` shims follow from it.
- `PtyIoActorRunner` carries test seams in production fields
  (`poll_pty_and_wake: fn(..)`, `drain_wake_fd: fn(..)`, `resize_pty: Box<dyn
  FnMut>`, `poll_observer`); a small trait would keep them out.
- `TerminalState::set_hook_authority_at` is a public production method
  documented as a fixture seam taking the source as a string.
- `AgentResumePlan::argv` is public and `shepr-server/src/test_support.rs`
  overwrites it.
- `SshControlDir::unchecked` exists only so tests can make the control and
  config directories differ.

Reported by server-serving, mux-panes, client-core, client-shell, foundation,
agents and edges.

## CLN-018 - Public surface that exists for another crate's test

- `shepr_remote::BridgeUpload` and `BridgeUploadEnd` (with `pub` fields) are
  exported only so `shepr-client/src/transport.rs`'s test
  `upload_cancellation_preserves_pending_endpoint_download` can drive them.
  Move the test into `shepr-remote` and make the types crate-private.
- `shepr-server` dev-depends on `shepr-client` for one test
  (`server/netside_tests.rs`), which is why `pub mod endpoint`, `pub use
  view::{..}`, `pub use shell::{ClientShellConfig, ClientShellState}`,
  `EndpointRegistry::new_at`, `insert`, `viewed` and the choice methods are
  `pub`; the binary uses only `run_client`, `ClientExit` and
  `ClientRunError`. A dedicated integration-test member would let the client's
  surface shrink.
- `codec::Decoder::finish`, `from_slice` and `Decoder` are public but used only
  by tests; `validated_sidebar_bounds` is public but used only in config;
  `encode_public_number`/`decode_public_number` are public only for mux tests.

Reported by edges, client-core and contracts.

## Unused dependencies and edges

## Stale documentation

## CLN-021 - The shepr-remote module tree is named for an older layout

`shepr-remote`'s `remote/` directory name reflects an older module tree (its
`lib.rs` mounts the files with `#[path]`; see the structure entry on the client
shell and remote module trees). (edges)

