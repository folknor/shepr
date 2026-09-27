# Defects: shepr-agent

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Hunter coverage: detection, manifests, the manifest registry and reload, integrations and config editing, and the reload call sites in shepr-server.

## AGT-010 - Concurrent installs can lose settings edits

Every settings edit is read-modify-write with no lock, so two concurrent `shepr integration install` runs for the same target can lose one update. The rename is atomic; the edit is not.

## AGT-019 - The version probe can still hang on a grandchild holding stdout

`run_version_probe` (`integration/version.rs`) kills the probed agent after 5 s, but once the child has exited it calls `wait_with_output`, which blocks until stdout closes. A grandchild that inherited stdout keeps it open and the install hangs anyway. Unlikely for a `--version` command.

## AGT-016 - Detection hot path allocates on every tick, per pane

- The detection path allocates on every tick, per pane:
  - a `RegionTexts` Vec
  - a `lines` Vec
  - a full `to_lowercase()` copy of every region any `contains` gate reads, often `whole_recent`

  Case-insensitive search that doesn't allocate would remove the copy: aho-corasick over each region's needles, or an ASCII-folding search.
- **`line_regex` rescans lines per pattern.** It re-runs `text.lines()` for each pattern; compiling them as one `(?m)` regex or a RegexSet would avoid that.
- **`Agent::prompt_ready` rebuilds a String every call.** It collects the last 12 lines into a fresh String.

## AGT-017 - Manifest compile work is duplicated and blocks the event loop

Each regex is compiled twice per load (validation, then compilation). `reload_manifests` recompiles every bundled manifest synchronously on the app event loop, both at startup and on the reload API call. Also, a detection tick before the startup reload triggers `registry()` with no override dir, so all bundled manifests compile twice at boot.
