# Defects: platform, build and layering

Filed from the reviews of the waves that resolved the defect hunt. IDs continue
the original series.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PLAT-039 - The test-fixtures dev-edge rule is prose only

Scope: build layering (lateral from review).

`shepr-test-fixtures` sits above `shepr-config`, `shepr-protocol`, `shepr-pty`
and `shepr-termio`, so a crate it depends on (directly or through those) cannot
take it as a dev-dependency without linking a second copy of itself into its
tests. The rule lives only in the fixtures crate docs and AGENTS.md. The
`[[dependency_rule]]` entries in `brokkr.toml` check normal and build edges, so a
`shepr-agent` to `shepr-test-fixtures` dev edge passed the dependency-rules phase
and was caught only by review. Add a rule covering dev edges (if brokkr's rule
kinds support them) that forbids `shepr-test-fixtures` from `shepr-core`,
`shepr-platform`, `shepr-vt`, `shepr-pty`, `shepr-agent`, `shepr-config`,
`shepr-protocol` and `shepr-termio`; if brokkr cannot express it, a script check
over `cargo metadata` dev edges would.
