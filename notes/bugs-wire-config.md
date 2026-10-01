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

## WIRECFG-004 - Full surface renders still build, compare and clone the whole grid

The full-surface size count is gone (cell deltas use a five-byte-per-cell lower
bound and count only the candidate update), metadata-only updates skip counting
and the grid comparison, compact sends move the rendered grid into the
committed baseline, and projection decoding makes one grid copy instead of two.
Residue: a full render still builds the grid and compares it with the last one
(`last.frame == surface.frame`), a full send still clones it for the committed
baseline, projection decoding keeps one copy for its separate consumer, and the
conservative lower bound can pass over a delta that would have paid.
