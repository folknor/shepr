# Defect hunt: wire and config

Scope: `crates/shepr-protocol/src/`, `crates/shepr-api/src/`, `crates/shepr-config/src/`,
plus the root `build.rs` that stamps the build identity. I followed values out of
scope where a finding depended on it (server render stream, client, remote launcher,
mux patch collection) and say so where I did.

Findings are ordered roughly by impact. Each one names the claim it breaks.

---

## 1. The API's control path (ping, stop) shares the 64-slot admission pool with app-bound requests, so a stalled app loop makes `server stop` fail

Files: `crates/shepr-api/src/server.rs` (`start_server`, `ConnectionAdmission`,
`handle_request`, `reject_busy_connection`), `crates/shepr-api/src/limits.rs`.

Claim: the API answers "ping and the stop methods on the API thread and everything
else [is] handed to the app loop". The stop path exists so a stop works when the app
loop does not answer (`server_stop_control_bypasses_app_channel`; `ServerStopSignal`
explains that the stop has to reach a loop that is waiting).

What happens: admission (`ConnectionAdmission::try_acquire`, cap
`MAX_ACTIVE_CONNECTIONS = 64`) runs on the listener thread before the request line is
read, so it applies to every method. An app-bound request (`pane.report_agent`,
`pane.report_agent_session`, `detect.*`) holds its slot until the app answers or
`ORDINARY_REQUEST_TIMEOUT` (15 s) runs out. Agent hooks fire on every agent state
change across all panes. When the app loop stalls, which is exactly when an operator
reaches for `shepr server stop`, the hook traffic fills all 64 slots within seconds.
After that:

- `server.stop` / `server.stop_if_boot` get `endpoint_busy`. `send_stop_request` maps
  that to `ServerStopError::Protocol`, and the stop fails without being delivered. The
  startup restart flow (`--expect-boot`) fails the same way.
- `ping` gets `endpoint_busy`, so `read_runtime_status_at` returns `Err` instead of a
  status, and `status`, preflight and the remote checks report a failure rather than a
  stalled server.

The busy refuser already reads the refused connection's request line
(`reject_busy_connection` parses it to echo the id). It could answer `ping` and both
stop methods there instead of refusing them. A structural fix: admit by method class,
either with a separate small reserve for the control methods or by reading the line
before admission and admitting only app-bound methods against the cap.

## 2. A pane's own socket variables make every nested process treat the profile's runtime address as an override, so it refuses to start a server and prints override-prefixed guidance

Files: `crates/shepr-config/src/address.rs` (`ServerAddress::resolve_paths`,
`is_runtime_address`, `command`, `apply_to_child_command`); the consumer is
`crates/shepr-remote/src/remote/local_server.rs` (`require_own_runtime_address`).

Claim: `is_runtime_address` is documented as "Whether this is the build profile's own
runtime address, as opposed to one a socket override picked. Only the runtime address
is one a client may start a server for." AGENTS.md says guidance names "the selected
socket override".

What happens: every pane exports both `SHEPR_SOCKET_PATH` and
`SHEPR_CLIENT_SOCKET_PATH`, set to the paths the server resolved. For the default
server those are exactly `<runtime>/shepr.sock` and `<runtime>/shepr-client.sock`. In
any process that inherits that environment with a matching profile marker,
`resolve_paths` sees "client override set, API override equal to runtime_api" and
classifies the address as `AddressSource::ClientOverride`, even though both paths are
the runtime defaults. The same happens when only `SHEPR_SOCKET_PATH` is set and equals
the runtime path: that is classified `ApiOverride`. As a result:

- `require_own_runtime_address` refuses to start a server: "no shepr server is running
  at ..., which SHEPR_CLIENT_SOCKET_PATH selects". This bites any process that outlives
  the server's pane while keeping its environment: a terminal window or tmux session
  started from a pane, or a script.
- Build-mismatch and stop guidance comes out as
  `SHEPR_CLIENT_SOCKET_PATH=/run/user/N/shepr/shepr-client.sock shepr server stop`
  rather than `shepr server stop`.
- `apply_to_child_command` passes the variable on to children.

Fix: in `resolve_paths`, treat an override that equals the runtime path it would
replace as no override at all. Classify by the resulting paths, not by which variables
were present.

## 3. The TUI client validates server-owned settings in its own context and refuses to launch on them

Files: `crates/shepr-config/src/validated.rs` (`ConfigResolution::parse` always runs
`ValidatedTerminalConfig::parse`: `resolve_default_shell`, `parse_new_cwd`),
`crates/shepr-config/src/io.rs` (`Config::load_validated`).

Claim: AGENTS.md: "A setting belongs to whoever draws or interprets it ... Each server
applies its own config to what it runs ...: shell and working directory".

What happens: the client launch (`AppPaths::resolve` plus `load_validated`) validates
`terminal.default_shell` against the client's `PATH`/`SHELL` and `terminal.new_cwd`
against the client's current directory. It checks that the directory exists and that
`SHELL` names a shell shepr recognises. Nothing in `shepr-client` or `src/` reads
`config.terminal()` (I checked with grep). So:

- Running `shepr` with an unrecognised `SHELL` (for example from a dev environment
  that sets `SHELL` to a wrapper, or an uncommon shell) fails the TUI launch, even when
  the TUI only attaches to an already-running local server, or only to remote machines.
- With a relative `terminal.new_cwd`, launching the TUI from a directory without that
  subdirectory fails the launch, even though the running server resolved the setting at
  its own launch and will never re-resolve it for this client.

The server's own load already checks these settings with the right inputs (the
`SHEPR_STARTUP_CWD` handoff and its own environment). Structural fix: split
`ValidatedConfig` into per-role resolutions (client-drawn, server-run), so that each
process validates only what it applies. Each role can still parse the whole file, so
unknown-key and syntax errors still fail every launch.

## 4. Every `SurfaceUpdate` carries the full metadata; the "absent metadata keeps the previous projection" path is never produced, and the fanout clones and re-sends the hyperlink table on every patch

Files: `crates/shepr-protocol/src/surface.rs` (`SurfaceUpdate`: "An absent metadata
value retains the previous projection"), `crates/shepr-protocol/src/surface_reuse.rs`
(`Baseline::update` always sets `meta: Some(...)`; `Decoder::decode` patch branch),
`crates/shepr-server/src/server/render_stream.rs` (`prepare_pane_surface_patch` builds
`SurfaceMeta::from(last)` and sends `meta: Some(meta)`).

Claim: AGENTS.md "Hot paths multiply. Work reachable from ... client frame fanout runs
per byte or event, times panes, times clients." The wire type documents a
metadata-free update.

What happens: no production code sends `meta: None`. Only tests construct it. Each
dirty-row patch, which is the per-keystroke path, therefore:

- clones `last.frame.hyperlinks` (up to `MAX_SURFACE_HYPERLINKS = 65_536` strings),
  `panes` and `splits` on the server, once per client;
- encodes and sends all of them;
- on the client, compares `meta.splits == previous.splits` and
  `meta.frame.hyperlinks == previous.frame.hyperlinks` (string-by-string) to decide
  whether this is a patch.

Dirty patches never introduce hyperlinks: `terminal_collect_dirty_patch` falls back on
`hyperlink_present`. So for the patch path the table is by construction the
baseline's. Sending `meta: None`, with only the cursor and the changed panes carried in
a small delta-meta type, would make a patch proportional to what changed. The client
decoder would then also skip the table comparison.

## 5. `surface_delta::message` encodes the entire full surface on every render just to learn its size

File: `crates/shepr-protocol/src/surface_delta.rs` (`message`: `encoded_size(full)`,
plus `encoded_size` per span in `changed_rows` and again `encoded_size(&message)`).

Claim: the framing code avoids exactly this ("Calling `encoded_len` first would
traverse every field again on the client fanout path", `framing.rs`). The same hot-path
rule as finding 4 applies.

What happens: for every full-surface render with a baseline, per client, the server
serializes the whole new surface (up to `MAX_SURFACE_CELLS = 4 Mi` cells) through the
counting sink. It then serializes each changed span, then the whole update, and the
transport finally serializes the chosen message again. The full-size figure only acts
as a threshold. A cheap bound would work as well: the cell count times a per-cell
lower bound, or the size of the previous full frame cached on the baseline. Combined
with the `prepare_pane_surface` equality check (`last.frame == surface.frame`, a full
grid comparison) and `surface.clone()` for the committed copy, one changed cell costs
several full-grid passes per client.

On the client side, the non-patch branch of `surface_reuse::Decoder::decode` clones
the baseline grid into the new surface and then `clone_from`s it back into the
baseline: two full-grid copies per projection-changing update.

## 6. `FrameData::intern_hyperlink` is a linear scan per hyperlinked cell on the render path

Files: `crates/shepr-protocol/src/frame.rs` (`intern_hyperlink`), called per cell from
`crates/shepr-mux/src/pane/terminal/backend.rs` in the full-frame render.

Claim: the same hot-path rule. The cap comment says the table bounds "the URI table".

What happens: every cell with `has_hyperlink` triggers
`hyperlinks.iter().position(...)` over the table built so far. A screen full of
distinct links, such as `ls --hyperlink` output or a file tree, costs
O(linked cells x distinct links) string comparisons per full render, per pane. The
worst case is millions of cells against 65,536 links. Consecutive cells of one link
also repeat the lookup (fetching the URI, then the scan). A `HashMap<String, u32>`
beside the `Vec` (as `from_ratatui_buffer_with_hyperlinks` already does), or a
last-URI fast path, makes this linear.

## 7. Client-to-server collections are not bounded at decode by the caps the protocol states for them

Files: `crates/shepr-protocol/src/input.rs` (`ClientMessage::ClientShellPaneInput
{ events: Vec<ClientPaneInputEvent> }`), `crates/shepr-protocol/src/command.rs`
(`LayoutSetSplitRatioParams.path: Vec<SplitBranch>`, `PaneCopySearchParams.query`),
`crates/shepr-protocol/src/limits.rs`.

Claim: `MAX_COLLECTION_ITEMS`: "Fields with tighter protocol caps apply those through
`serialize_bounded_vec` / `deserialize_bounded_vec` as well."
`MAX_INPUT_EVENT_BATCH`: "the server refuses a message past it".
`MAX_SURFACE_SPLIT_PATH` exists to bound "traversal work even for a malformed split
path".

What happens: `events` and `path` use plain `Vec` serde, so the only decode-time bound
is the general 4 Mi item cap plus the 2 MiB frame. A same-user client can make the
server allocate and decode several hundred thousand `ClientPaneInputEvent`s (each tens
of bytes in memory) before the batch check refuses the message. An over-long split
path is decoded and then compared against every split. Since every event counts at
least once against `MAX_INPUT_EVENT_BATCH`, it is a valid `Vec` length cap. The fix is
to apply the bounded-vec adapters with `MAX_INPUT_EVENT_BATCH` and
`MAX_SURFACE_SPLIT_PATH`. The impact is small because the peer is the same user and
passes the build preamble, but the stated contract is not met.

## 8. `read_frames` allocates the claimed frame before its bytes arrive

File: `crates/shepr-protocol/src/framing.rs` (`read_message` doc,
`read_frames`: `payload.resize(total, 0)` before `read_exact_or_eof`).

Claim: `read_message` says it "Rejects a frame over `MAX_FRAME_SIZE` or a message over
the cap without panicking or allocating ahead of the bytes that actually arrive."

What happens: after the length check, the whole claimed frame (up to 2 MiB, or 64 KiB
for the handshake) is zero-filled before any payload byte is read. The allocation is
bounded per frame, but it is ahead of the bytes. Either reword the doc ("at most one
frame ahead") or read with `take(claimed).read_to_end` into a growing buffer.

## 9. `server.stop` accepts a boot guard placed outside `params` and silently ignores it

Files: `crates/shepr-api/src/schema.rs` (`Request` with `#[serde(flatten)] method`, no
`deny_unknown_fields`), `crates/shepr-api/src/schema/server.rs`.

Claim: `server.stop_if_boot` has its own method name so that a guard is never ignored
("an older server rejects the request as invalid instead of ignoring the guard";
`ServerStopParams` has `deny_unknown_fields` for the same reason).

What happens: `{"id":"x","method":"server.stop","params":{},"expected_boot_id":"..."}`
decodes as an unconditional stop. The stray top-level key is dropped by the flattened
`Request`. Shepr's own client never sends this, so this is a hardening gap on the
cross-build control surface, not a live bug. Rejecting unknown top-level keys
(`Request` cannot take `deny_unknown_fields` together with `flatten`, so this needs a
hand-written `Deserialize`, or an explicit `method`/`params` pair) closes it.

## 10. Smaller contract and doc mismatches

- `crates/shepr-protocol/src/codec.rs` module doc: "A brokkr textlint rule rejects
  these serde shapes in protocol, config, core and VT source". The rule in
  `brokkr.toml` (`wire-types-use-positional-serde-shapes`) covers protocol, core and
  VT only, and says "config is not checked". Config does use `#[serde(untagged)]`
  (`keybinds.rs`, `BindingConfig`), which is fine because config never crosses the
  wire. The codec doc is stale.
- `build.rs` writes `OUT_DIR/build_id.rs` (a `pub(crate) const BUILD_ID`) next to
  `build_identity.rs`. Nothing includes `build_id.rs`. Only `build_identity.rs` is
  included, by `shepr-protocol/src/limits.rs`. This is dead output. The
  `crate_dir_name` branch that roots the tree at `manifest_dir` for any crate other
  than `shepr-protocol`/`shepr-config` is also unreachable: the root package has
  `build = false`.
- `send_stop_request` treats `EmptyResponse` (connection closed with no answer) as an
  accepted stop. The server also closes without answering when the connection thread
  fails to spawn, when the peer-credential check refuses, or when the request line is
  oversized. In those cases a stop that was never delivered waits out the full 15 s
  and reports `TimedOut` with "sockets are still reachable", which misnames the
  failure.
- `read_runtime_status_at` maps a stalled server (`TimedOut`) to `Ok(None)`, but a
  server that accepts and then closes without a line (the same cases as above) to
  `Err("empty api response")`. Both mean "no usable status".
- `NoticeKind::SessionRestoreIncomplete` admits a state that renders broken text:
  `unusable: None`, `dropped_workspaces: 0`, `panes_pruned: false` displays "restored
  in part:  could not be restored." The server never builds it today
  (`app/mod.rs` guards with `restore_was_partial`), but the type should not represent
  it. Split it into `Unusable { reason }` and `Partial { ... }`.
- `ValidatedUiConfig::sidebar_width` doc says "already clamped to `sidebar_bounds`".
  It is not clamped: validation rejects an out-of-range width. The invariant holds;
  the word "clamped" is wrong.
- `format_key_combo` says labels should "read back as the same binding". This does not
  hold for `KeyCode::Char('+')` (configured as `plus`): it prints `+`, so the label is
  `ctrl++` / `prefix++`, which `parse_key_combo` rejects (it splits on `+`). This only
  affects display today.
- `TerminalGeometry` serializes with its own derive and deserializes through the
  separate `ReceivedTerminalGeometry` (`try_from`). The positional layouts of the two
  structs must stay identical by hand. Reordering the fields of either one silently
  breaks the wire with no compile error, and only a round-trip test would catch it. A
  `TryFrom` validation on the deserialized value of the same type (or a shared private
  struct) removes the duplication.

## 11. Surprising, not a contract break

- The build identity folds in `CARGO_CFG_TARGET_FEATURE` (through the `CARGO_CFG_*`
  family). The owner's global `~/.cargo/config.toml` sets `-Ctarget-cpu=native`. So
  two hosts with different CPUs that each build the same commit get different
  `BUILD_ID`s, and each refuses the other as "a different build". Copying one binary to
  every host instead gives matching identities but a binary tuned for the build host's
  CPU, which can fault with SIGILL elsewhere. `build.rs`'s "the same inputs give the
  same identity on every host" is true but hides this. If per-host builds are the
  install method, remote machines with different CPUs can never connect. `RUSTC` and
  `HOST` are also identity inputs, and `RUSTC` may be an absolute path under the
  builder's home. I did not verify what cargo hands the script under rustup here. If it
  is an absolute path, it would make identities differ across users even on identical
  hardware.
- `wait_for_lease_release` checks the data-directory lease by briefly taking its
  `flock`. A server starting at that instant (another terminal's autostart) can lose
  the lease race and exit with `ALREADY_RUNNING`. The window is tiny.
- `BootId::for_this_process`, `TerminalId` and `ConnectionGeneration` are fine as they
  are. I found no defects in the varint, zigzag, depth-limit, trailing-byte, preamble
  or multi-frame split/reassembly logic: the frame arithmetic in
  `FramedPayloadBuffer::finish` is correct at exact multiples of `MAX_FRAME_SIZE` and
  at zero length. The id parsers' canonical round-trip checks are sound.
