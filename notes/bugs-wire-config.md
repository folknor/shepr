# Defects: wire and config

Filed from the defect hunt over `crates/shepr-protocol/src/`,
`crates/shepr-api/src/`, `crates/shepr-config/src/` and the build identity
script.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## WIRECFG-001 - The TUI client validates server-owned settings in its own context and refuses to launch on them

Files: `crates/shepr-config/src/validated.rs` (`ConfigResolution::parse` always
runs `ValidatedTerminalConfig::parse`: `resolve_default_shell`,
`parse_new_cwd`), `crates/shepr-config/src/io.rs` (`Config::load_validated`).

Claim: AGENTS.md: "A setting belongs to whoever draws or interprets it ... Each
server applies its own config to what it runs ...: shell and working directory".

The client launch validates `terminal.default_shell` against the client's
`PATH`/`SHELL` and `terminal.new_cwd` against the client's current directory. It
checks the directory exists and that `SHELL` names a shell shepr recognises.
Nothing in `shepr-client` or `src/` reads `config.terminal()` (checked with
grep). So running `shepr` with an unrecognised `SHELL` (a dev environment that
sets `SHELL` to a wrapper, or an uncommon shell) fails the TUI launch even when
it only attaches to a running local server or to remote machines; and with a
relative `terminal.new_cwd`, launching from a directory without that
subdirectory fails, though the running server resolved the setting at its own
launch and never re-resolves it for this client.

Structural fix: split `ValidatedConfig` into per-role resolutions (client-drawn,
server-run) so each process validates only what it applies. Each role still
parses the whole file, so unknown-key and syntax errors still fail every launch.

## WIRECFG-002 - A pane's own socket variables make a nested process treat the runtime address as an override

Surfaced by the wire and config hunt and, as a related false negative, by the
remote and launch hunt. Files: `crates/shepr-config/src/address.rs`
(`ServerAddress::resolve_paths`, `is_runtime_address`, `command`,
`apply_to_child_command`); consumer
`crates/shepr-remote/src/remote/local_server.rs` (`require_own_runtime_address`).

Claim: `is_runtime_address`: "Whether this is the build profile's own runtime
address, as opposed to one a socket override picked. Only the runtime address is
one a client may start a server for." AGENTS.md: guidance names "the selected
socket override".

Every pane exports `SHEPR_SOCKET_PATH` and `SHEPR_CLIENT_SOCKET_PATH`, for the
default server exactly `<runtime>/shepr.sock` and `<runtime>/shepr-client.sock`.
In any process inheriting that environment with a matching profile marker,
`resolve_paths` sees "client override set, API override equal to runtime_api"
and classifies the address `AddressSource::ClientOverride`, though both paths are
the runtime defaults. Only `SHEPR_SOCKET_PATH` set and equal to the runtime path
is classified `ApiOverride`. Results:

- `require_own_runtime_address` refuses to start a server ("no shepr server is
  running at ..., which SHEPR_CLIENT_SOCKET_PATH selects"). This bites any
  process that outlives the server's pane while keeping its environment: a
  terminal window or tmux session started from a pane, or a script.
- Build-mismatch and stop guidance comes out as
  `SHEPR_CLIENT_SOCKET_PATH=/run/user/N/shepr/shepr-client.sock shepr server
  stop` rather than `shepr server stop`.
- `apply_to_child_command` passes the variable on to children.

The remote hunter rated this low (reachable only with inherited pane
environment).

Fix: in `resolve_paths`, treat an override equal to the runtime path it would
replace as no override. Classify by the resulting paths, not by which variables
were present.

## WIRECFG-003 - Every `SurfaceUpdate` carries the full metadata, and the fanout clones and re-sends the hyperlink table on every patch

Files: `crates/shepr-protocol/src/surface.rs` (`SurfaceUpdate`: "An absent
metadata value retains the previous projection"),
`crates/shepr-protocol/src/surface_reuse.rs` (`Baseline::update` always sets
`meta: Some(...)`; `Decoder::decode` patch branch),
`crates/shepr-server/src/server/render_stream.rs` (`prepare_pane_surface_patch`
builds `SurfaceMeta::from(last)` and sends `meta: Some(meta)`).

Claim: AGENTS.md "Hot paths multiply"; the wire type documents a metadata-free
update.

No production code sends `meta: None`; only tests construct it. Each dirty-row
patch (the per-keystroke path) clones `last.frame.hyperlinks` (up to
`MAX_SURFACE_HYPERLINKS = 65_536` strings), `panes` and `splits` once per client,
encodes and sends all of them; the client compares `meta.splits ==
previous.splits` and `meta.frame.hyperlinks == previous.frame.hyperlinks`
string-by-string to decide whether it is a patch. Dirty patches never introduce
hyperlinks (`terminal_collect_dirty_patch` falls back on `hyperlink_present`), so
for the patch path the table is the baseline's by construction. Sending
`meta: None`, with only the cursor and changed panes in a small delta-meta type,
would make a patch proportional to what changed and let the decoder skip the
comparison.

## WIRECFG-004 - `surface_delta::message` encodes the entire full surface on every render just to learn its size

File: `crates/shepr-protocol/src/surface_delta.rs` (`message`:
`encoded_size(full)`, plus `encoded_size` per span in `changed_rows` and again
`encoded_size(&message)`).

Claim: the framing code avoids exactly this ("Calling `encoded_len` first would
traverse every field again on the client fanout path", `framing.rs`), and the
same hot-path rule.

For every full-surface render with a baseline, per client, the server serializes
the whole new surface (up to `MAX_SURFACE_CELLS = 4 Mi` cells) through the
counting sink, then each changed span, then the whole update, and the transport
serializes the chosen message again. The full-size figure only acts as a
threshold; a cheap bound would do (cell count times a per-cell lower bound, or
the previous full frame's size cached on the baseline). With the
`prepare_pane_surface` equality check (`last.frame == surface.frame`) and
`surface.clone()` for the committed copy, one changed cell costs several
full-grid passes per client. Client side, the non-patch branch of
`surface_reuse::Decoder::decode` clones the baseline grid into the new surface
and then `clone_from`s it back: two full-grid copies per projection-changing
update.

## WIRECFG-005 - `FrameData::intern_hyperlink` is a linear scan per hyperlinked cell on the render path

Files: `crates/shepr-protocol/src/frame.rs` (`intern_hyperlink`), called per
cell from `crates/shepr-mux/src/pane/terminal/backend.rs` in the full-frame
render. Every cell with `has_hyperlink` triggers `hyperlinks.iter().position(...)`
over the table so far. A screen of distinct links (`ls --hyperlink`, a file tree)
costs O(linked cells x distinct links) string comparisons per full render per
pane, worst case millions of cells against 65,536 links; consecutive cells of one
link repeat the lookup. A `HashMap<String, u32>` beside the `Vec` (as
`from_ratatui_buffer_with_hyperlinks` already does), or a last-URI fast path,
makes this linear.

## WIRECFG-006 - The codec doc claims a textlint covers config

`crates/shepr-protocol/src/codec.rs` module doc: "A brokkr textlint rule rejects
these serde shapes in protocol, config, core and VT source". The rule in
`brokkr.toml` (`wire-types-use-positional-serde-shapes`) covers protocol, core
and VT only, and says "config is not checked". Config does use
`#[serde(untagged)]` (`keybinds.rs`, `BindingConfig`), which is fine because
config never crosses the wire.

## WIRECFG-007 - `build.rs` writes dead output and has an unreachable branch

`build.rs` writes `OUT_DIR/build_id.rs` (a `pub(crate) const BUILD_ID`) next to
`build_identity.rs`. Nothing includes `build_id.rs`; only `build_identity.rs` is
included, by `shepr-protocol/src/limits.rs`. The `crate_dir_name` branch that
roots the tree at `manifest_dir` for any crate other than
`shepr-protocol`/`shepr-config` is unreachable: the root package has
`build = false`.

## WIRECFG-008 - `read_runtime_status_at` maps two kinds of "no usable status" differently

A stalled server (`TimedOut`) maps to `Ok(None)`, but a server that accepts and
closes without a line (connection thread spawn failure, peer-credential
refusal, oversized request line) maps to `Err("empty api response")`. Both mean
"no usable status".

## WIRECFG-009 - `ValidatedUiConfig::sidebar_width` says "clamped"

The doc says "already clamped to `sidebar_bounds`". It is not clamped:
validation rejects an out-of-range width. The invariant holds; the word is wrong.

## WIRECFG-010 - `format_key_combo` labels for `+` do not read back

`format_key_combo` says labels should "read back as the same binding". For
`KeyCode::Char('+')` (configured as `plus`) it prints `+`, so the label is
`ctrl++` / `prefix++`, which `parse_key_combo` rejects (it splits on `+`).
Display only today.

## WIRECFG-011 - `TerminalGeometry` serializes and deserializes through two hand-synced structs

`TerminalGeometry` serializes with its own derive and deserializes through the
separate `ReceivedTerminalGeometry` (`try_from`). The positional layouts must
stay identical by hand; reordering either struct's fields silently breaks the
wire with no compile error, caught only by a round-trip test. A `TryFrom`
validation on the deserialized value of the same type (or a shared private
struct) removes the duplication.

## WIRECFG-012 - The build identity varies with the host CPU and possibly the rustc path

Filed by the hunter as surprising rather than a contract break. The build
identity folds in `CARGO_CFG_TARGET_FEATURE` (through the `CARGO_CFG_*` family).
The owner's global `~/.cargo/config.toml` sets `-Ctarget-cpu=native`, so two
hosts with different CPUs that each build the same commit get different
`BUILD_ID`s and each refuses the other as "a different build". Copying one
binary to every host instead gives matching identities but a binary tuned for the
build host's CPU, which can fault with SIGILL elsewhere. `build.rs`'s "the same
inputs give the same identity on every host" is true but hides this. If per-host
builds are the install method, remote machines with different CPUs can never
connect. `RUSTC` and `HOST` are also identity inputs, and `RUSTC` may be an
absolute path under the builder's home; the hunter did not verify what cargo
hands the script under rustup here, and if it is absolute it would make
identities differ across users even on identical hardware.

## WIRECFG-013 - The stop's lease check can make a concurrently starting server lose the lease race

Filed by the hunter as surprising; the window is tiny. `wait_for_lease_release`
checks the data-directory lease by briefly taking its `flock`. A server starting
at that instant (another terminal's autostart) can lose the race and exit with
`ALREADY_RUNNING`.
