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

The split needs role-specific config types threaded through consumers in
several crates (shepr-client `lib.rs` and its presentation config readers,
shepr-server `app/mod.rs`, `app/state.rs` and `headless/bootstrap.rs`), which is
why a footprint-limited attempt went no further than comments, which were then
removed. Run it as a single-fixer wave.

## WIRECFG-004 - Full surface renders still build, compare and clone the whole grid

The full-surface size count is gone (cell deltas use a five-byte-per-cell lower
bound and count only the candidate update), metadata-only updates skip counting
and the grid comparison, compact sends move the rendered grid into the
committed baseline, and projection decoding makes one grid copy instead of two.
Residue: a full render still builds the grid and compares it with the last one
(`last.frame == surface.frame`), a full send still clones it for the committed
baseline, projection decoding keeps one copy for its separate consumer, and the
conservative lower bound can pass over a delta that would have paid.

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
