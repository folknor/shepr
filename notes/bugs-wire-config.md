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

## WIRECFG-016 - An identical surface is re-sent in full when its metadata does not fit a delta

Lateral, `crates/shepr-protocol/src/surface_delta.rs`. The separate whole-grid
equality check was folded into the delta planner, which detects an unchanged
surface during its cell scan. But the planner returns `Full` early when
`metadata_fits` fails (too many hyperlinks, panes, splits or split-path entries)
or the grid size is invalid, before that scan, so in those cases an identical
surface is now re-sent in full on every prepare instead of being skipped. Detect
the unchanged case before the early return.
