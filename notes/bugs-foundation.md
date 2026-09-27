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

## FND-001 - Two servers can start on the same socket path

`crates/shepr-platform/src/ipc.rs`, `prepare_socket_path`. It checks whether the socket is live, then unlinks it if stale, with no lock around the two steps. Two servers starting together can both see "stale". A unlinks and binds; B then unlinks A's live socket and links its own. You end up with two servers, one of them unreachable. This breaks the promise in `bind_private_local_listener`'s doc that "a listener that raced us to the path is never replaced": the hard-link protection is undone by the unconditional unlink just before it. The fix is a startup lock (flock on a sibling lock file held for the server's lifetime), after which the stale-socket unlink is safe.

Related: the remote hunter notes that two bridges arriving at once on a remote host both start daemons and rely on the server refusing the second bind (see RMT-009).

## FND-002 - Private-socket binding often falls back to the less safe path

`ipc.rs`, `bind_via_private_staging`. The staged name `.shepr-<pid>-<nanos hex>-<n>/s` adds roughly 35–40 bytes to the socket path. So any path that fits the 107-byte limit but is within about 40 bytes of it takes the bind-then-chmod fallback, which is connectable with umask permissions for a moment. The code documents the fallback, but the staging name is what makes it common. Use a very short staging name, or bind through `/proc/self/fd/<dirfd>/s` on an O_PATH directory fd.

## FND-003 - Saved layouts are trusted without checks, and bad data panics

`crates/shepr-core/src/layout.rs`. `TileLayout::from_saved` does not check that `focus` is in the tree, that ids are unique, or that no id is 0 (0 is the placeholder id). `close_focused` then `expect`s the focused pane to be present, so a restored session with a stale focus id panics the server. `split_focused` also `expect`s (production `expect` on data that came from disk goes against "no unwrap in production"). `from_saved` should validate and return `Option`/`Result`, or fall back to the first leaf.

## FND-004 - SplitRatio does not leave room for both panes; unchecked u16 addition

`layout.rs`. The doc says `SplitRatio` is "constrained to leave room for both panes", but the clamp to 0.1–0.9 plus `round()` in `split_rect` gives a 0-width or 0-height child for small areas (width 3 × 0.1 rounds to 0). A rule in cells, not a fraction, is needed. Separately, `find_in_direction` and `ranges_overlap` use unchecked `u16` addition (`r.x + r.width`) while other helpers in the same file use saturating arithmetic.

## FND-005 - SSH control directory ignores XDG and escapes test isolation

`crates/shepr-platform/src/ssh_paths.rs`, `shared_ssh_control_path`. The fallback hard-codes `/run/user/<uid>` instead of `$XDG_RUNTIME_DIR`, against "Directories follow the XDG spec". The main path `/tmp/hssh-<uid>` (a leftover "h" prefix from herdr) is shared between a real shepr and any test. `IsolatedEnv` redirects `XDG_RUNTIME_DIR` precisely so tests cannot reach live state, but this function bypasses that, so a test could reuse the real OpenSSH ControlMaster sockets. `create_remote_ssh_config_dir` and `remote_bridge_endpoint_path` likewise use the temp dir or `/tmp`, and the last `/tmp/<short>` fallback in `remote_bridge_endpoint_path` is never checked with `fits_unix_socket_path`.

Related: the remote hunter reports that when `shared_ssh_control_path` finds neither a root-owned sticky `/tmp` nor a valid `/run/user/<uid>`, managed SSH config is silently dropped (RMT-001), and that `create_remote_ssh_config_dir`'s `ctl` length check is vacuous (RMT-010).

## FND-006 - Processes of other users are dropped from session teardown

`crates/shepr-platform/src/process.rs`. For pidfd handles, `ProcessHandle::is_unreaped` is `pidfd_send_signal(0)`, which returns EPERM for a process of another uid (for example setuid `sudo` inside a pane). Those members are silently left out of `session_member_handles` as if already reaped, which contradicts that function's doc ("every live process of session…"). EPERM should count as alive.

## FND-007 - HostShutdownMonitor: misplaced doc and missed cancellation during D-Bus outage

`crates/shepr-platform/src/shutdown.rs`.
- The doc comment describing `release_delay_lock` ("Tell the monitor that the session checkpoint…") sits on `warning_generation`, so `release_delay_lock` itself is undocumented and the getter carries the wrong contract.
- When the D-Bus connection errors while a shutdown is pending, `requested` stays true with no inhibitor held, and the reconnect loop backs off up to 60 s. That is fine for a shutdown, but a cancellation that happens during the outage is only noticed after reconnecting.

## FND-008 - IsolatedEnv creates less isolation than its doc says

`crates/shepr-test-support/src/lib.rs`.
- It sets `XDG_RUNTIME_DIR` to `<scratch>/runtime` but never creates that directory, so code under test gets a missing runtime dir and ends up creating it itself.
- The doc still says "one lock for the whole crate"; since the extraction it is one lock per test binary. It should be reworded without a count.
- `ScratchDir::new_in` directories are not under the scratch root, so `keep_until_exit` on one leaks it, contrary to that method's doc.

## FND-009 - Config writes fail on files carrying privileged xattrs or foreign ownership

`crates/shepr-platform/src/config_file.rs`, `write_config_temporary`. Every xattr on the source is copied, and any `fsetxattr` failure aborts the whole write, for example `security.*` labels that need privileges, or any xattr after `fchown` has handed the file to another uid. `fchown` to a different owner also fails with EPERM for non-root. So editing an agent config that carries such attributes fails outright. Only ACL entries (`system.posix_acl_*`) need copying; the others should be best-effort.

## FND-010 - One logging error disables logging for good

`crates/shepr-platform/src/logging.rs`. A single I/O error (transient ENOSPC, or a directory recreated) sets `disabled = true` forever. The process then logs nothing for the rest of its life and nothing records that it stopped.

## FND-011 - ChildExitReason variants misclassify or are never produced

`ChildExitReason::Interrupted` covers death by any signal, including SIGHUP/SIGKILL that shepr sends itself, and `WaitFailed` is never produced by `classify_child_exit`.

Related: TRM-011 (reader panic reported as `Exited`).

## FND-012 - workspace_label_from_cwd compares raw $HOME

`workspace_label_from_cwd` in `shepr-core` compares the raw `$HOME` with no normalisation, so a trailing slash defeats the `~` label. It also bypasses `pathutil::home_dir`'s validation and reads the environment from a crate that is otherwise pure.

## FND-013 - expand_tilde_path passes non-UTF-8 paths through unexpanded

`expand_tilde_path*` passes non-UTF-8 paths through unexpanded, even ones that start with `~/`.

## FND-014 - Small platform smells

- In `lib.rs`, `signal_processes` is documented as test-only but is exported `pub` unconditionally.
- `create_private_temporary` is a pure alias of `create_private_file`.
- `poll_local_stream_read` throws away the byte count it computes.
- `ssh_agent`: the server's inherited fallback agent always takes priority over every attached client's agent. This is intentional per the comment, but it means a stale inherited sshd socket that still accepts connections wins over a fresh one.
