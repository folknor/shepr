# Defect hunt: wire and surface (shepr-core, shepr-protocol, shepr-surface)

Scope covered: `shepr-protocol` (codec, framing and the continuation bit,
preamble, ids, boot and build identity, revisions, frame, surface, input,
message, command, geometry), `shepr-surface` (delta planner, decoder and
baseline, glyph repair, pane row rule, compose, ratatui conversion), the
geometry, layout and chrome parts of `shepr-core`, and the consumers on both
ends: the server's `render_stream.rs` and `headless/retained_surface.rs`, the
client's `shell/presentation/surface_patch.rs`, `surfaces.rs`,
`client_loop/dispatch.rs`, `state.rs` and `shepr-termio/src/blit.rs`.

`shepr-core/src/env.rs` was only outlined, not audited.

Findings are ordered by how much they matter.

## F1. The client's patch fast path writes server chrome roles to the host uncoloured

The claim it breaks: AGENTS.md says the pane chrome a server draws "names each
cell's role (`shepr_protocol::ChromeRole`) instead of a colour, and the client
colours the roles as it composes the surface". `WireColor::Chrome` says "no role
reaches a host terminal". `blit.rs` `sgr_color` says "Composition resolves every
chrome role; one that slipped through draws in the terminal's default rather
than a guessed colour".

What happens: a `PaneSurfacePatch` that takes the fast path
(`ClientShellState::apply_pane_surface_patch_from` in
`crates/shepr-client/src/shell/presentation/surface_patch.rs`, which returns
`PatchPresentation::Rows`) copies `row.cells` unchanged into
`ClientComposedSurfacePatch`. `ClientState::present_surface_patch`
(`crates/shepr-client/src/state.rs`) hands them to `BlitEncoder::encode_patch`,
and from there they go straight to the host. `Canvas::compose_pane` is never
called, and neither is `ChromePalette::resolve`. Any cell carrying
`WireColor::Chrome(_)` is therefore written with SGR 39/49, the terminal's
default colour.

Patch rows carry chrome cells routinely:

- `retained_scrollbar_patch` (`crates/shepr-server/src/server/headless/retained_surface.rs`)
  emits scrollbar track and thumb cells built by `visit_scrollbar_track`
  (`crates/shepr-server/src/ui/scrollbar.rs`), all `fg: WireColor::Chrome(..)`.
  That happens whenever a scrolled pane's metrics move while output arrives.
  This is the common case for an agent pane with scrollback.
- The delta planner's Patch-meta updates (`Baseline::update` in
  `crates/shepr-surface/src/decode.rs`) can carry spans over border cells
  (border titles, strokes). The decoder forwards them as the same
  `PaneSurfacePatch`.

`fast_path_blocker` checks occlusion, overflow, selection, copy mode and
unknown panes, but nothing about chrome colours. `commit_patch` then stores the
unresolved cells in the encoder's `last_frame`. The next full composition differs
from them and repaints the cells correctly, so the wrong colour lasts until
something forces a full compose. With an idle sidebar that may be a long time.
While it lasts, the focused and unfocused scrollbar look the same, and so do the
focused border's host accent and the plain border.

Client tests never send `WireColor::Chrome` through a patch.

Fix direction: resolve the roles when building the fast-path rows. That means
the same `ChromePalette` that `draw_frame` builds (`chrome_palette` in
`shell/view/draw.rs`) applied per cell, or `compose_pane`'s per-cell step
factored out and shared. Do not count on full composition. A bigger, cleaner
version: make the fast path and `compose_pane` one function over a row span, so
there is one place that turns pane cells into canvas cells. Today there are two,
and they have drifted (colour roles, hyperlink remapping, cursor clipping).

## F2. The retained patch path does not respect `MAX_SURFACE_PATCH_SPANS`, and treats hitting it as an encode failure

The claim it breaks: the protocol caps a `SurfaceUpdate` at
`MAX_SURFACE_PATCH_SPANS` (4096) spans. The delta planner treats that cap as a
planning bound: `changed_rows` in `crates/shepr-surface/src/delta.rs` returns
`None`, and the planner sends the full surface. The retained path in
`render_patches` (`retained_surface.rs`) has no such bound. It extends
`patch_rows` across every collected pane, plus one scrollbar row per changed
track cell. It sorts them, builds the update in `prepare_pane_surface_patch`,
and only learns about the cap when `shepr_protocol::encode_message` fails in
`serialize_bounded_vec`.

What happens then: a WARN `surface.patch_encode` "failed to serialize retained
pane surface patch", `client.request_repaint()` (the baseline is dropped), and
the client is promoted to the full step.

How it gets there: 4096 spans is not exotic. Every span is a run of changed cells
plus one trailing cell. A 200x60 pane redrawn with alternating changed cells
(a TUI table, a colour ramp, htop-like output), or several panes changing at
once, passes the cap. The result is correct, but:

- an ordinary outcome is logged as a serialization error;
- the client loses its baseline. It gets a full `PaneSurface` rather than a
  compact delta against its baseline, which the full step would otherwise plan;
- the encode work for the oversized patch is wasted, once per recipient.

Fix direction: give `changed_rows` and `render_patches` the same bound
`delta::changed_rows` uses. Count spans as they are pushed, and promote
(`RetainedSurfaceFallback::...`) without logging an error or dropping the
baseline. Better still, have both producers build spans through one
span-collector type in `shepr-surface` that owns the cap. Then the rule
"spans past the cap mean a full surface" lives in one place, like
`PatchSpanCheck` does for ordering.

## F3. `SurfaceUpdate { meta: None }` decodes into a patch the client is guaranteed to refuse

The claim it breaks: the `SurfaceUpdate` doc in
`crates/shepr-protocol/src/surface.rs` says "An absent metadata value retains the
previous projection".

What happens: in `Decoder::decode`, the compact branch maps `meta: None` to
`SurfacePatchMeta { cursor: previous cursor, panes: Vec::new() }`. The client's
`PaneSurfaces::validate` (`crates/shepr-client/src/shell/presentation/surfaces.rs`)
requires every pane a row touches to be listed (`RowOutsideFrame`), and
`dispatch.rs` fails the connection on any rejection. So a `meta: None` update
with any span on a pane decodes fine in the reader, advances the reader's
baseline, and then tears the connection down.

Today the server never sends `None` (`Baseline::update` and
`prepare_pane_surface_patch` always set `Some`), so this is latent. Still, the
wire type offers a state that the receiving half cannot honour.

Fix direction: drop the `Option` (meta is always sent), or have the decoder
derive the touched panes from the baseline (`row_touches_rect` over
`previous.panes`), as the projection branch already does.

## F4. The decoder and the shell disagree on the patch rule, and a comment says the decoder checks less than it does

`dispatch.rs` (the `ClientPaneSurfacePatchOutcome::Rejected` arm) says a patch
can be accepted by the decoder and refused by the shell, naming "a pane geometry
change, or a row on a pane the patch does not list; the decoder checks neither".

The decoder does check pane geometry: `SurfaceBaseline::check` in `decode.rs`
runs `pane_geometry_matches` for every listed pane. What it does not check is
the unlisted-row rule. That rule is enforced only in `PaneSurfaces::validate`.

The comment is half wrong. More to the point, two baselines (the reader's
`CellBaseline` and the shell's `PaneSurfaces`) each apply their own subset of one
"patch admission rule", which `decode.rs` describes as shared: "the admission
rule both the decoder and patch producers apply". The server's own admission
(`prepare_pane_surface_patch` through `SurfaceBaseline::admits`) does not check
the listing rule either. A producer bug that breaks it is caught only at the
last stage, as a connection failure.

Fix direction: move the listing rule into `SurfaceBaseline::check`. Producer,
reader and shell would then refuse the same patches, and the server would catch
its own bug before sending. Fix the dispatch comment either way.

## F5. Odd `ContentRevision`s are compared for equality as if they certified content

The claim it breaks: the `ContentRevision` doc in
`crates/shepr-protocol/src/revision.rs` says "an odd one never names a stable
surface ([`Self::is_stable`])".

What happens: the only client reader, `surface_presented` in
`crates/shepr-client/src/shell/input/selection.rs`, decides whether a word
gesture's content moved with `previous.content_revision != next.content_revision`.
It never consults `is_stable()`. Two torn draws certified to the same odd value
compare equal, and the gesture survives. For example, a draw whose `before` read
failed (`certify(None, Some(r))`) followed by another torn draw ending at the
same `r`.

The window is narrow: revisions are monotone, so it needs an unreadable `before`.
But the type's documented meaning is that odd means "not certified", and the one
consumer treats odd values as certifying equality.

Fix direction: give `changed_since` the parity rule (`!self.is_stable() ||
self != earlier`) and use it at the call site. `changed_since` exists for this
and is currently unused there.

## F6. Smaller doc and contract drift

- `ClientMessage` (`crates/shepr-protocol/src/input.rs`) says "Not `Eq`: an
  endpoint command can carry a split ratio". `SplitRatio` implements `Eq`
  (`shepr-core/src/layout.rs`, with a comment saying why), and
  `LayoutSetSplitRatioParams` derives `Eq`. Nothing stops `EndpointCommand` or
  `ClientMessage` from deriving `Eq`, so the stated reason is stale.
- The `read_message` doc in `crates/shepr-protocol/src/framing.rs` says "at most
  one frame is allocated ahead of the bytes that actually arrive". `payload.resize`
  grows the `Vec` geometrically, so on a long multi-frame message the reserved
  capacity can reach about twice the bytes received, not "one frame ahead". The
  untouched pages are not committed on Linux, so this is a statement about
  allocation, not resident memory. Either reserve exactly (`reserve_exact` before
  `resize`) or reword the doc.
- `BootId::process_id` returns `Option<u32>` but is always `Some`: every
  `BootId` holds a pid. The signature suggests a case that does not exist.
- `MIN_ENCODED_CELL_BYTES = 5` (`crates/shepr-surface/src/limits.rs`) is a valid
  lower bound. The true minimum is 7, because `WireStyle` always encodes two
  bytes. Its comment says it ignores the style, which is true. This is only a
  note that the full-size estimate is about 30 percent low, so `delta::message`
  sends `Full` for some deltas that would have been smaller.

## Checked and found sound

These were followed and hold, so nobody needs to re-walk them:

- Framing: single and multi-frame encoding, the exact-multiple boundary (no
  empty trailing frame), and the continuation-bit checks: a continued frame must
  be full-sized, and single-frame readers refuse continuation. Message and frame
  caps are checked before allocation.
- Codec: canonical varints (overlong and overflow refused), zigzag, char and
  UTF-8 validation, length prefixes bounded by the remaining input,
  collection-item caps, depth bounding, trailing-byte refusal, and the
  bounded-vec marker round trip. `FrameData` deserializes through its validating
  constructor.
- Preamble: an unidentifiable build matches nothing, itself included, and
  malformed identity bytes read as a different build, not as a non-shepr peer.
- Public IDs: bijective base 32 has exactly one spelling, and zero is refused on
  both the text and wire paths. `BootId` parsing round-trips only canonical
  text.
- Delta planner: spans are capped and sorted, the dense-change fallback works,
  and the unchanged and metadata-only branches are right. A changed hyperlink
  table always ships with Projection metadata, so equal indices under a new table
  resolve correctly on the client.
- Decoder: every branch validates before it mutates. The full-path hyperlink
  validation over unchanged intervals is correct.
- Glyph repair, `put_run`/`overlay_buffer`, `Canvas::overwrite` and
  `compose_pane` clipping arithmetic: no overflow or out-of-range slicing
  found. The cursor is only carried when it lies in the copied part.
- `normalize_pane_row`: idempotent, and recutting it matches a single cut. The
  retained path normalizes only whole rows, never spans, as its comment requires.
- Layout `split_extent`, `remove_pane`, `set_ratio_at`, `PaneId::alloc`
  exhaustion, `BoundedGridSize::clamped`, and `chrome::border_inner_rect`
  saturation.
