# Defects: shepr-core, shepr-platform, shepr-test-support

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: all of `shepr-core` and `shepr-test-support`, most of `shepr-platform`; `terminal_environment.rs` and `tests.rs` were not read.

## FND-018 - Workspace dependencies have not been audited since the crate split

The root `Cargo.toml` still depends on `unicode-segmentation`, which nothing in `src/` uses (shepr-vt dropped it too), and other root or crate dependencies may be leftovers from before the extraction into `crates/`. An audit of every manifest against actual `use` sites would trim them. `brokkr deps` also shows ratatui pulling in the termwiz backend (and with it second copies of sha2, digest, nix, thiserror, bitflags, base64, getrandom and syn 1); shepr drives the terminal through crossterm, so ratatui with default features off and only the backend in use would likely drop most duplicated versions.

## FND-019 - The SSH control-path length check measures a lossy string

`crates/shepr-platform/src/ssh_paths.rs`, `shared_ssh_control_path`, measures the staging path after `to_string_lossy()`. A non-UTF-8 byte becomes a three-byte replacement character, so a runtime path whose real byte length fits the socket limit can be rejected. Measure `as_os_str().len()` instead.
