# Defects: platform, remote machines and the root binary

Filed from the defect hunt over `crates/shepr-core`, `crates/shepr-platform`,
`crates/shepr-remote`, and the root `shepr` package (`src/main.rs`,
`src/cli.rs`, `src/cli/`, `src/preflight.rs`, `src/autodetect.rs`).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## PLAT-014 - `shepr-platform` holds higher-level policy its own doc says belongs elsewhere

Hunter's severity: not given (lateral). Scope: core-platform.

`lib.rs` says "higher-level rules belong to the crates that consume these
primitives". The crate nonetheless holds the logind inhibitor protocol and its
warning-generation policy, clipboard helper selection, Git environment policy,
and multi-process log rotation. It also makes the `shepr` client binary depend
on `zbus` and `tokio` for a monitor only the server runs. Moving the shutdown
monitor to `shepr-server` and the Git runner to `shepr-mux` would match the doc.

The owner has approved the move, including the `brokkr.toml` layer allow-list
changes it needs (the `zbus` edge moves with the monitor). The Git runner is
`run_git` in `crates/shepr-platform/src/git.rs` (the spawn with a scrubbed
environment and a deadline); its callers are the mux Git status code and
`workspace.checkout_root`. Clipboard helper selection and log rotation can stay
unless moving them pays.

## PLAT-031 - The cross-build status and stop surface is implicit

Scopes: termio-root and protocol-api (both filed it as a structural note, not a
defect in itself).

The preflight restart offer, `status` and `server stop --expect-boot` exist to
talk to a server of another build, over the JSON `ping` and `server.stop`
requests: `read_runtime_status_at` deserializes the full `SuccessResponse` /
`ResponseResult` enum, and the boot guard lives only in the receiving server's
handling of `expected_boot_id`. AGENTS.md says there are no wire compatibility
obligations, but these shapes are the exception the restart flow depends on. If
a future `Pong` shape changes, `local_server_status` logs a warning and returns
`None`, the offer silently disappears, and the launch fails with "did not give a
usable status answer" with no stop guidance. A tiny, explicitly frozen
identity/stop envelope (its own type, its own test fixture) would make the
dependency visible.

The protocol-api hunter adds the sharper half. The preamble has a fixed layout
so that "any two builds can read each other's preamble", but the restart flow
also relies on JSON between different builds: `ping`'s
`Pong { version, build_id, boot_id }` and `server.stop`'s `expected_boot_id`
param. The conditional stop is only safe if the other build understands the
parameter. `ServerStopParams` is a plain serde struct and serde ignores unknown
fields, so a build that did not know `expected_boot_id` (renamed, removed) would
stop unconditionally: exactly the "stop the replacement" outcome the flag exists
to prevent. Latent, since every build today has the field. That hunter proposes a
separate method name for the conditional stop (an unaware server answers
`invalid_request` and stops nothing), plus a frozen test fixture for `ping` and
`server.stop` as the one cross-build JSON surface. See also WIRE-024 on the
client status JSON.
