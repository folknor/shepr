# Bugs: wire and surface (shepr-core, shepr-protocol, shepr-surface and their consumers)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the wire and surface hunt. The raw report, including its list of
areas checked and found sound, is in commit 6dc81572
(`notes/hunt-wire-surface.md`). That hunter only outlined
`shepr-core/src/env.rs`; it did not audit it.

## SURF-002 - The retained patch path does not respect `MAX_SURFACE_PATCH_SPANS`, and treats hitting it as an encode failure

Claim broken: the protocol caps a `SurfaceUpdate` at `MAX_SURFACE_PATCH_SPANS`
(4096) spans. The delta planner treats that cap as a planning bound:
`changed_rows` in `crates/shepr-surface/src/delta.rs` returns `None`, and the
planner sends the full surface. The retained path in `render_patches`
(`retained_surface.rs`) has no such bound. It extends `patch_rows` across every
collected pane, plus one scrollbar row per changed track cell. It sorts them,
builds the update in `prepare_pane_surface_patch`, and only learns about the
cap when `shepr_protocol::encode_message` fails in `serialize_bounded_vec`.

What happens then: a WARN `surface.patch_encode` "failed to serialize retained
pane surface patch", `client.request_repaint()` (the baseline is dropped), and
the client is promoted to the full step.

How it gets there: 4096 spans is not exotic. Every span is a run of changed
cells plus one trailing cell. A 200x60 pane redrawn with alternating changed
cells (a TUI table, a colour ramp, htop-like output), or several panes changing
at once, passes the cap. The result is correct, but:

- an ordinary outcome is logged as a serialization error;
- the client loses its baseline. It gets a full `PaneSurface` rather than a
  compact delta against its baseline, which the full step would otherwise plan;
- the encode work for the oversized patch is wasted, once per recipient.

Fix direction: give `changed_rows` and `render_patches` the same bound
`delta::changed_rows` uses. Count spans as they are pushed, and promote
(`RetainedSurfaceFallback::...`) without logging an error or dropping the
baseline. Better still, have both producers build spans through one
span-collector type in `shepr-surface` that owns the cap. Then the rule "spans
past the cap mean a full surface" lives in one place, like `PatchSpanCheck`
does for ordering.

## SURF-003 - `SurfaceUpdate { meta: None }` decodes into a patch the client is guaranteed to refuse

Claim broken: the `SurfaceUpdate` doc in `crates/shepr-protocol/src/surface.rs`
says "An absent metadata value retains the previous projection".

What happens: in `Decoder::decode`, the compact branch maps `meta: None` to
`SurfacePatchMeta { cursor: previous cursor, panes: Vec::new() }`. The client's
`PaneSurfaces::validate`
(`crates/shepr-client/src/shell/presentation/surfaces.rs`) requires every pane
a row touches to be listed (`RowOutsideFrame`), and `dispatch.rs` fails the
connection on any rejection. So a `meta: None` update with any span on a pane
decodes fine in the reader, advances the reader's baseline, and then tears the
connection down.

Today the server never sends `None` (`Baseline::update` and
`prepare_pane_surface_patch` always set `Some`), so this is latent. Still, the
wire type offers a state that the receiving half cannot honour.

Fix direction: drop the `Option` (meta is always sent), or have the decoder
derive the touched panes from the baseline (`row_touches_rect` over
`previous.panes`), as the projection branch already does.

## SURF-004 - The decoder and the shell disagree on the patch rule, and a comment says the decoder checks less than it does

`dispatch.rs` (the `ClientPaneSurfacePatchOutcome::Rejected` arm) says a patch
can be accepted by the decoder and refused by the shell, naming "a pane
geometry change, or a row on a pane the patch does not list; the decoder checks
neither".

The decoder does check pane geometry: `SurfaceBaseline::check` in `decode.rs`
runs `pane_geometry_matches` for every listed pane. What it does not check is
the unlisted-row rule. That rule is enforced only in `PaneSurfaces::validate`.

The comment is half wrong. More to the point, two baselines (the reader's
`CellBaseline` and the shell's `PaneSurfaces`) each apply their own subset of
one "patch admission rule", which `decode.rs` describes as shared: "the
admission rule both the decoder and patch producers apply". The server's own
admission (`prepare_pane_surface_patch` through `SurfaceBaseline::admits`) does
not check the listing rule either. A producer bug that breaks it is caught only
at the last stage, as a connection failure.

Fix direction: move the listing rule into `SurfaceBaseline::check`. Producer,
reader and shell would then refuse the same patches, and the server would catch
its own bug before sending. Fix the dispatch comment either way.
