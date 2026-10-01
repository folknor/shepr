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

## WIRECFG-015 - The per-role config loaders keep plumbing only for symmetry

Lateral from the client and server config split, `crates/shepr-config/src/`.
The `role_loader!` macro in `io.rs` generates `ClientConfig::load_validated` and
`ServerConfig::load_validated`, which duplicate the free `load_client_validated`
and `load_server_validated` (the only ones used outside the crate); its body is
unindented and rustfmt does not format macro bodies, so a small trait or two
plain functions would read better. `ServerConfigResolution::parse`,
`ValidatedServerConfig::from_loaded` and the test-only `ValidatedServerConfig::new`
take a provenance they ignore, and `ValidatedServerConfig::from_values` parses
one only to reject unparseable TOML; `ClientConfigResolution` takes an unused
`_paths` and always returns empty `path_diagnostics`. The test-only
`ValidatedClientConfig::test_from_config_with_paths` has no caller.
`ConfigDiagnostic::with_file` appends the file path after the whole message, so
for a multi-line TOML parse error it lands after the caret diagram; put it on
the first line. The `server.toml` template says the theme accent is for
"highlights, borders, and navigation UI", but on the server it colours only
pane chrome.
