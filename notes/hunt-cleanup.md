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

## CLN-023 - Small leftovers from the first fixes

- `crates/shepr-server/src/app/`: `AppPolicy::Test` is kept as a const equal
  to `Suspended`, with a comment calling Suspended a different state, while
  every test teardown now spells `Suspended`. One spelling should win.
- `crates/shepr-client/src/shell/`: most items are `pub(in crate::shell)` where
  only their parent module uses them, and `OverlayRender` and
  `render_client_overlay` in `overlays/mod.rs` are `pub(crate)`; an item-level
  visibility pass would make the new module tree mean something.
- `crates/shepr-server/src/app/`: `AppPolicy` and the saver's `SavePolicy` are
  now separate state, and about twenty tests still end with
  `app.policy = Suspended`, which no longer touches the saver.
- `crates/shepr-mux/src/pane/runtime.rs`: every pane input encoder exists both
  mode-free and `_with_modes`; the mode-free public ones look used only by tests.

(wave-1 review and gate, wave-3 review, wave-5 fixer and review, wave-7 and wave-8 reviews)

## CLN-026 - Publish helpers take the pane id twice

`publish_state_changed_event` and `publish_agent_process_detected_event` in
`crates/shepr-mux/src/pane/process_probe.rs` take an `EventSender` (which
carries the pane id) and a separate `pane_id`; if the two disagree, admission
drops the event. The event's pane id should come from the sender. (wave-3
review)

## CLN-027 - Leftovers from the fourth light-loop wave

- `ServerMessage::Clipboard { data: Vec<u8> }` goes through `serialize_seq` and
  decodes one visitor call per byte; the codec has `serialize_bytes`, and a
  bytes wrapper would make a large clipboard one copy.
- `crates/shepr-protocol/src/framing.rs` `read_frames` picks the oversized-frame
  `LimitKind` by comparing `max_frame == MAX_FRAME_SIZE`; a reader whose cap is
  neither reports `MessageBytes` with the wrong maximum. The caller should pass
  the kind.
- `crates/shepr-protocol/src/codec.rs` carries bounded-vec limit errors through
  serde's string-only `custom` channel with a private prefix and parses them
  back.
- `ServerStatusJson` validates `boot_id` on decode but stores a `String`; it
  could hold a `BootId` with the same JSON spelling.
- `parse_client_config` and `parse_server_config` report an unreachable
  "could not produce validated values" fallback as a path diagnostic; it should
  be unrepresentable or its own internal kind.
- The no-shell diagnostic says "Configure a shell shepr recognizes" where
  "Set it to a shell shepr recognizes" would read with the key path prefix.

(wave-4 review)

## Test-only twins and test seams in production

## CLN-016 - Production rules with a test-only twin that the tests exercise instead

`handle_internal_event_after_checkpoint` (cfg(test), in
`crates/shepr-server/src/app/session.rs`) reimplements the server's
hold-and-replay of checkpointed exits with a fixed `for _ in 0..4` loop; five
tests in `app/session.rs` and seven in `app/mod.rs` run against it rather than
the `HeadlessServer` loop's own queue (whose ordering has four tests in
`server/headless/tests/pane_exit.rs`). Moving those assertions onto the loop
would let the copy go. Reported by mux-panes, mux-state and server-app.

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
