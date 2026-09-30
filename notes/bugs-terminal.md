# Defects: terminal emulation, PTY and pane runtime

Filed from the defect hunt over `crates/shepr-vt`, `crates/shepr-pty` and
`crates/shepr-mux/src/pane` (except `agent_detection.rs`).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## TERM-007 - The scanner's ground-state ESC search is not memchr

Scope: vt-pty.

`Scanner::scan` (`crates/shepr-vt/src/scan.rs`) finds the next ESC with
`iter().position(|&b| b == 0x1b)`, while vte uses `memchr` on the same bytes. It
runs on every PTY chunk of every pane in ground state, just before vte scans the
same slice again. Switching to `memchr` was done once and reverted because the
`shepr-vt-layer` dependency rule in `brokkr.toml` did not allow a direct
`memchr` edge. The owner has approved the edge: add `memchr` to that layer's
allow list, add the dependency to `crates/shepr-vt/Cargo.toml` (it is already in
the build through vte, so no crate is added), and switch the search.
