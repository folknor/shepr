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
render. Consecutive cells of one link now hit a table-tail fast path. Residue:
a cell whose link differs from the last one still scans the table with
`hyperlinks.iter().position(...)`, so a screen of distinct links
(`ls --hyperlink`, a file tree) costs O(linked cells x distinct links) string
comparisons per full render per pane. A cache inside `FrameData` would go stale
because `hyperlinks` is a public `Vec` callers can edit (the reason is now
commented in `frame.rs`), so the fix belongs to the caller: the full-frame render
in `backend.rs` should own a URI-to-index map for the frame it builds, seeded
from the existing table and updated on each insertion.
