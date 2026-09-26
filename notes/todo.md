# Later

Things to do when the situation comes up or when there is time for a larger
change, not defects to hunt. Each redesign below touches several modules at
once, so it needs a wave of its own rather than parallel fixers.

## Resolve typescript question

What to do about src/integration/assets/shepr-agent-state.test.ts.

## Monitor upstream changes to integrations

We need to create a script we can run periodically that checks upstream
for changes and additions to src/integration/assets/* and
src/detect/manifests/* and anything else relevant.

Find out which commit we forked from first. Was it 21d0ce6?
https://github.com/herdrdev/herdr/commit/21d0ce60267ad947c081d3d3fba401c859f06dd2

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr agent read <pane> --source detection --format text`, and compare the dialog's labels with the gate. Fix the manifests if they differ.

## Wire absolute terminal rows through server, client and API

Selections and copy-mode positions drift once scrollback is full, because they name screen rows and every new line evicts the oldest.

- The terminal side is done: `RowOrigin` (`src/ghostty/rows.rs`) tracks evictions exactly; `Terminal::history_origin()`, `screen_row_for_absolute`, `absolute_row_for_screen`; `ScrollPosition` / `viewport_top_row()`; `_absolute` readers on `PaneTerminal` (in an `#[allow(dead_code)]` block); `src/selection.rs` stores `u64` rows with `_at` methods. Column changes, alt-screen resizes, RIS and over-long writes retire old ids.
- Server: add `history_origin: u64` to `PaneSurfaceScrollMetrics`, filled from `scroll_position()`; re-export `ScrollPosition` from `pane.rs`.
- Client: store selections and the copy-mode cursor/anchor with the `_at` methods, viewport top = `history_origin + max_offset - offset`. Until then the claim "Ordinary selections are live buffer ranges" (`src/client/shell/state.rs`) is untrue.
- API: `PaneTextPoint.row` is `u32` and names a screen row; widen it to `u64` and call the `_absolute` readers.
- Mouse copy: once rows are absolute, `content_revision: None` is safe because evicted rows are refused. Then drop the `allow(dead_code)`.

## Replace the JSON/base64 tunnel with typed wire messages

The stated contract is "payloads use the positional codec", and wire types must not use `skip_serializing_if`.

- The shell handshake, snapshot, agent completions, health ping, presentation sync and every endpoint request/response are JSON strings inside `EndpointControl { kind: String, data: String }`, with `.v1` kind strings (`protocol/endpoint.rs`). Surface reuse sends `PaneSurfaceFrame` metadata as JSON.
- Surface delta encodes positionally, base64s it (+33%), and wraps it in a `String` that is encoded again; on the render path that is several whole-frame passes per client per render (`surface_delta::message`).
- Composition rebuilds every cell's `String` plus a hyperlink HashMap whenever it round-trips a frame through a ratatui buffer (`client/shell/composition.rs`).
- Make SurfaceDelta, SurfaceReuse, Snapshot, Hello/Welcome and so on typed `ServerMessage`/`ClientMessage` variants and drop the `.v1` strings.

## Split large surfaces across frames

- Surface geometry is bounded to what one frame can carry (`MAX_SURFACE_CELLS = MAX_FRAME_SIZE / 16`, `MAX_SURFACE_DIMENSION`, `MAX_CELL_SIZE_PX` in `src/protocol/wire.rs`). Cells with long graphemes or many hyperlinks can still exceed the 16-byte budget (reported once to clients). Splitting a full surface across frames, and chunking OSC 52 clipboard data, would remove the limit.

## Per-client presentation state on the server

- The event loop mixes per-client presentation state (`foreground_client_id`, `effective_size`, global `app.state.active`, `outer_terminal_focus`, the diagnostic copies) with session state. Make each client's view the only source of presentation truth and drop the global "foreground client" projection into `AppState`.
- `terminal_id_by_string` is a linear scan; `TerminalId` (`src/terminal/`) has no `Borrow<str>` to key a map by `&str`.
- `app.session_snapshot()` always builds `layouts`, which the shell projection never reads.

## Event-driven API connection loop

- The API server is thread-per-connection with 100 ms polling. Streams carry the hub sequence (`SubscriptionStream`, `src/api/subscriptions.rs`), but sampled subscriptions (output match, scroll, agent-status fallback) still poll the app 10 times a second; each `pane.output_matched` subscription runs a full recent-text `PaneRead` on the main thread each time. One event-driven loop and a complete, sequenced model diff from the event hub would remove the polling.

## One owner for persistence, with history formatted off the loop

- Capture, writing, the history pairing and the resume schedule sit in separate places with no single owner. The data directory is locked (`src/persist/lock.rs`). One persistence actor could own the lock, take cheap snapshots on the loop, format history off it, and write layout plus history as one bundle. It would also own the carried history (`HistoryCarry`, `src/persist/snapshot.rs`, `App.pane_history_carry`).
- `live_history_read` (`src/persist/snapshot.rs`) still formats each pane's whole scrollback eagerly on the loop, because a `TerminalRuntime` can't leave the loop and there is no `Send` handle to the terminal core. With absolute rows, a `Send` reader could remember the last absolute row it saved and resume from the later of that and `history_origin()` (full re-read if the origin passed it), re-reading the screen rows each time, in bounded chunks under short lock holds.
- `persist::restore` takes one size for every pane in the session; restored panes start at that size, not their own layout size, until the first resize.
