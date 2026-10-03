# Cleanup from the design hunt

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

Dead code, dead state, test-only twins of production rules, public surface
that exists only for tests, unused dependencies and stale documentation. Each
entry is a deletion or a rewording rather than a redesign. Unverified: the
raw reports are in the commit that precedes this file's.

## Dead code and dead state

## CLN-023 - The client shell has no item-level visibility pass

Most items in `crates/shepr-client/src/shell/` are `pub(in crate::shell)` where
only their parent module uses them, and `OverlayRender` and
`render_client_overlay` in `overlays/mod.rs` are `pub(crate)`; an item-level
visibility pass would make the module tree mean something. (wave-7 review)

## CLN-030 - The server tracks custom report origins the TUI no longer shows

The server still accepts and tracks free-label custom report origins
(`ReportedAgent::Custom`, non-`shepr:` sources in `ReportOrigin::parse`, the API
agent label in pane info), though the TUI projection carries only a known
`Agent` and shepr installs no third-party reporter. Rejecting non-official
sources at `app/api/panes/reports.rs` and deleting `ReportedAgent::Custom` would
make the server agree with the TUI; it cuts across shepr-agent ownership, mux
persistence and their tests. (wave-7 adjudication)

## CLN-031 - Leftovers from the eleventh light-loop wave

- `EndpointError::Rejected` (`crates/shepr-protocol/src/command.rs`) has no
  server producer now that every refusal has a typed category; only client tests
  build it. Remove it, or keep it as a documented policy-refusal category.
- `crates/shepr-platform/src/config_file.rs`: `create_config_temporary` and
  `write_config_temporary` are used only by platform tests now that the agent
  integration publishes through `PreparedFile`.
- `crates/shepr-platform/src/publish_file.rs`: `publish_file()` has no caller;
  the module has no tests of its own (hard-link no-clobber, withdraw on
  directory-sync failure, symlink refusal); the non-replace path uses
  `hard_link`, which fails on filesystems without hard links.
- `crates/shepr-remote/src/remote/machine_ssh.rs`: `MachineProbe::ensure_ssh`
  duplicates `ensure_managed_ssh_config` for the connector state.
- `crates/shepr-client/src/shell/ledger.rs`: the endpoint-response handler keeps
  an unused `_now: Instant` parameter.
- `crates/shepr-server/src/app/api/endpoint.rs`: `endpoint_rejected` returns
  `InvalidArgument`; its name no longer matches the category.
- `crates/shepr-server/src/app/api/detect.rs`: `handle_detect_capture` repeats
  `json_pane`'s parse and error because it needs the parsed id.
- `crates/shepr-protocol/src/identity.rs`: `RequestId::allocate` uses a plain
  `fetch_add` that would wrap at `u64::MAX` (unreachable, but undocumented now).
- `crates/shepr-protocol/src/ids.rs`: the comment above the public-number
  helpers states a plan (keep the exports until a mux test changes) rather than
  a fact; the mux workspace test could assert through the public id types and
  the helpers could narrow.

- `AppPaths::rooted_at` (`crates/shepr-config/src/io.rs`) is documented as
  unchecked, but `ServerAddress` now holds a checked `SocketPath`; for a root
  too long to host a socket it falls back to `/` as a placeholder runtime
  directory. A fallible `rooted_at` or a separate unchecked address would say
  what it does.

(wave-11 review and gate)

## Test-only twins and test seams in production

## CLN-018 - shepr-server dev-depends on shepr-client for one test

`shepr-server` dev-depends on `shepr-client` for `server/netside_tests.rs`,
which is why several client modules and methods are `pub`; a dedicated
integration-test workspace member would let the client surface shrink. This is
a workspace layout change, deferred with the crate splits. (edges, client-core)

## Unused dependencies and edges

## Stale documentation

## CLN-021 - The shepr-remote module tree is named for an older layout

`shepr-remote`'s `remote/` directory name reflects an older module tree (its
`lib.rs` mounts the files with `#[path]`; see the structure entry on the client
shell and remote module trees). (edges)
