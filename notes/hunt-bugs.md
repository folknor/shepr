# Bugs from the design hunt

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

Defects the design hunters turned up on the way. Unverified: each is a
hunter's reading, with the hunter's own confidence where they gave one.

## Latent defects

## BUG-076 - An expired parked hook start reads as parked until the next process observation

When a parked start expires, the pane's last unapplied hook report is now
cleared, but expiry is only evaluated when process evidence arrives
(`clear_full_lifecycle_hook_suppression_for_detected_agent` in shepr-detect's
ownership `source.rs`). A pane with no further detector observation keeps
showing the report as parked, with a growing age, in detect explain. A
time-based check where the report is read (`last_unapplied_hook_report`, which
would need a `now`) closes it. (bug round)

## Hot-path costs

## BUG-078 - Server chrome glyph repair allocates a mask per run

Since server overlays repair split glyphs with the client compositor's rule,
`put_run` and `overlay_buffer` in `crates/shepr-surface/src/glyph_repair.rs`
build a covered-cell mask per call, and `ui/panes.rs` writes every border cell
through `put_run`, so each border cell allocates once per draw. A windowed or
reused mask avoids it. (bug round, glyph repair)
