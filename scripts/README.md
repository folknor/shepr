# scripts/

The roster, and what each file's standing is.

This table makes each script's role discoverable. An unwired check reads as
coverage and is not; an unlisted manual tool is hard to find.
`check_scripts_roster.py` (wired at `pre-clippy`) refuses if a file in this
directory has no row below, or a row names a file that is not here, or a row
declared `gate` is not run by a `[[script_check]]` in `brokkr.toml` (and the
reverse).

Three standings:

- **gate** - run by `brokkr check` as a `[[script_check]]`. Changing it changes
  what the gate refuses.
- **tool** - run by hand, deliberately. It has a stated job and no gate wants it.
- **diagnostic** - run by hand while investigating one specific question.

| script | standing | what it is |
|---|---|---|
| `_brokkr_config.py` | gate | shared module: `repository_sources()`, the one definition of "a file this repository authored", and `workspace_member_manifests()`, the root package plus the globbed members |
| `check_agent_asset_tests.py` | gate | the `agent-asset-tests` check: runs the bun tests beside the integration assets and passes only on a clean run with passing tests, no failure and every test file run |
| `check_cited_paths.py` | gate | the `cited-paths` check: every backticked repository path in a comment or durable document names a file in the tree |
| `check_scripts_roster.py` | gate | the `scripts-roster` check: this table against this directory |
| `check_skip_after_scopes.py` | gate | the `skip-after-scopes` check: every textlint `skip_after` rule re-applied to the production items below a file's first test cfg, which the line-based exemption releases; and that shape itself refused, so a file in a skip_after rule's scope holds no production code below its first `#[cfg(test)]` |
| `check_textlint_regions.py` | gate | the `textlint-regions` check: refuses a `region = "code"` textlint whose pattern contains a double quote, since that region masks string literals quotes included and the rule can never match |
| `check_tree_debris.py` | gate | the `tree-debris` check: every `crates/*/` root holds only `Cargo.toml`, `README.md`, `build.rs`, `src`, `tests`, `benches` and `examples`; an untracked file inside a crate's sources carries an extension those sources already use; and every workspace-root entry is tracked or gitignored |
| `check_workspace_dependencies.py` | gate | the `workspace-dependencies` check: every name the root `[workspace.dependencies]` pins is taken with `workspace = true` and no restated version or path, and an external dependency two members share is pinned there |
| `check_seal_paths.py` | gate | the `seal-paths` check: every `clippy.toml` seal is a path with a reason, the root `clippy.toml` is the only one, and both `disallowed_*` lints are denied (clippy itself refuses a path that no longer resolves) |
| `notes_drop.py` | tool | removes whole findings entries by ID from a notes document |
| `notes_drop_bullet.py` | tool | removes single top-level bullets from a notes document by the start of their first line; each prefix must match exactly one bullet |
| `upstream_watch.py` | tool | reports what upstream herdr changed in the integration assets, detection manifests and related sources since the recorded baseline, mapped to our files; `--advance` moves the baseline, `--fork-point` checks it |
| `upstream_baseline.txt` | tool | data for `upstream_watch.py`: the upstream herdr commit shepr has caught up to |
| `fix_unwraps.py` | tool | one-off: replaced `.unwrap()` in test code for clippy's `unwrap_used` |