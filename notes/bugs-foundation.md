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

## FND-007 - HostShutdownMonitor: misplaced doc and missed cancellation during D-Bus outage

`crates/shepr-platform/src/shutdown.rs`.
- The doc comment describing `release_delay_lock` ("Tell the monitor that the session checkpoint...") sits on `warning_generation`, so `release_delay_lock` itself is undocumented and the getter carries the wrong contract.
- When the D-Bus connection errors while a shutdown is pending, `requested` stays true with no inhibitor held, and the reconnect loop backs off up to 60 s. That is fine for a shutdown, but a cancellation that happens during the outage is only noticed after reconnecting.

## FND-009 - Config writes fail on files carrying privileged xattrs or foreign ownership

`crates/shepr-platform/src/config_file.rs`, `write_config_temporary`. Every xattr on the source is copied, and any `fsetxattr` failure aborts the whole write, for example `security.*` labels that need privileges, or any xattr after `fchown` has handed the file to another uid. `fchown` to a different owner also fails with EPERM for non-root. So editing an agent config that carries such attributes fails outright. Only ACL entries (`system.posix_acl_*`) need copying; the others should be best-effort.

## FND-010 - One logging error disables logging for good

`crates/shepr-platform/src/logging.rs`. A single I/O error (transient ENOSPC, or a directory recreated) sets `disabled = true` forever. The process then logs nothing for the rest of its life and nothing records that it stopped.

## FND-011 - ChildExitReason variants misclassify or are never produced

`ChildExitReason::Interrupted` covers death by any signal, including SIGHUP/SIGKILL that shepr sends itself, and `WaitFailed` is never produced by `classify_child_exit`. (A PTY reader panic now has its own `ReaderPanicked` variant.)

## FND-012 - workspace_label_from_cwd compares raw $HOME

`workspace_label_from_cwd` in `shepr-core` compares the raw `$HOME` with no normalisation, so a trailing slash defeats the `~` label. It also bypasses `pathutil::home_dir`'s validation and reads the environment from a crate that is otherwise pure.

## FND-013 - expand_tilde_path passes non-UTF-8 paths through unexpanded

`expand_tilde_path*` passes non-UTF-8 paths through unexpanded, even ones that start with `~/`.
