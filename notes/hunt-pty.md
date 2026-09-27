Review of crates/shepr-pty, plus the reap and teardown path in shepr-mux. I only read files and edited nothing. I did run two harmless shell commands by mistake before I remembered the read-only rule: an `ls` of the crate's src directory and a `grep` that failed.

## Defects, most important first

**1. Input backpressure does not work, and the actor's write queue has no size limit** (`/home/folk/Programs/shepr/crates/shepr-pty/src/actor.rs`)
- The API promises backpressure: `ACTOR_COMMAND_BUFFER = 1024`, and `try_write_user_input` returns `TrySendError::Full` with the bytes handed back.
- In practice, `drain_data_commands` moves every queued command into `pending_writes` (an unbounded `VecDeque`) on every loop iteration, whether or not the PTY can take writes. The only time it holds back is while a submission is active.
- So `Full` is almost never returned. When a child stops reading stdin, pastes and keystrokes keep piling up in memory.
- Terminal responses are worse. `read_chunk` pushes each `on_read` result into `pending_writes` with no limit. A child that prints queries such as DA1/DSR/XTGETTCAP in a loop and never reads stdin grows server memory for as long as it runs. That is a server-wide memory exhaustion caused by one pane.
- Fix: make `pending_writes` the bounded queue. Stop draining `data_rx` once the queued bytes pass a limit, and cap or coalesce terminal responses. When a child is not reading, dropping or coalescing replies is the correct terminal behaviour.

**2. One blocking-pool thread per pane for the child's whole life** (`/home/folk/Programs/shepr/crates/shepr-mux/src/pane/runtime.rs`, around lines 453-476)
- `tokio::task::spawn_blocking(move || child.wait())` holds a thread from Tokio's blocking pool (512 by default) until the child exits.
- The same pool runs detection's `foreground_process_group_id` and `probe_foreground_process`, the synchronized-output flush, and the theme probe. With enough panes those tasks queue forever.
- Also likely: `Runtime` drop waits for blocking tasks unless `shutdown_timeout` or `shutdown_background` is used. Any exit path that keeps pane processes alive (`preserve_processes_on_drop`) would then hang the server on `wait()`. That is exactly the "blocks waiting for the child" problem this crate claims to avoid, just moved elsewhere. I have not verified which runtime shutdown the server uses; please check.
- Fix: reap from a pidfd. `ProcessHandle` already opens one: register it with `AsyncFd`, then `waitid(P_PIDFD)`. That avoids a dedicated thread per child.

**3. If `PtyIoActor::spawn` fails, the child is orphaned** (runtime.rs, around lines 453 and 639-648)
- The child watcher is started before the actor. If `PtyIoActor::spawn(...)?` fails (wake pipe, fcntl, or thread creation), `spawn_command_builder` returns `Err`.
- `master_fd` is dropped, so the child gets SIGHUP, but `shutdown_pane_processes` never runs. A child or session member that ignores SIGHUP keeps running, and the watcher later sends `PaneDied` for a pane that never existed.
- The partial-failure path skips the teardown contract. Fix: create the actor, or at least the wake pipe and fds, before spawning, or run teardown on the error path.

**4. `POLLERR` throws away the child's last output** (`/home/folk/Programs/shepr/crates/shepr-pty/src/fd.rs`, `poll_pty_and_wake`)
- The actor itself claims that a child's last output is drained before the loop ends (`handle_write_failure` documents this).
- But `POLLERR` on the PTY fd returns `Err`, and `run()` then breaks with no drain.
- I believe Linux pty masters usually report `POLLHUP`/`EIO` rather than `POLLERR`, so this may be rare. Still, it is the one exit path that breaks the drain guarantee. Drain here the same way as on a write failure.

**5. A failed resize is swallowed, so the PTY and the emulator can disagree about size** (actor.rs `resize`, fd.rs `resize_pty_fd`)
- `clamp_pane_size` in runtime.rs says "the PTY and the emulator always agree on the size".
- `PaneRuntime::resize` resizes the emulator first, then asks the actor. If the ioctl fails, the actor logs at `debug!` and nothing else happens.
- Not exiting the process is correct. But the size contract then breaks silently, and `current_size` already holds the new value, so the next identical resize is skipped as a no-op and nothing retries.

**6. Resize replies can go out in the wrong order relative to earlier replies** (actor.rs `apply_pending_controls`)
- A resize request's responses are queued before any `controls.terminal_responses` that were pushed earlier, for example an appearance report queued before the resize.
- `write_terminal_response` has a `response_order` lock precisely to keep replies ordered. Resize replies skip that ordering.

**7. `PaneDied` from a reader panic says `ChildExitReason::Exited`** (runtime.rs, around line 629)
- The child did not exit. The type's own name is misused, and anything that branches on the exit reason (restore, UI) is told something false.

## Smaller items and smells

- **Pre-exec signal reset list is narrow** (`backend.rs` `prepare_pty_child`). It resets only SIGCHLD/HUP/INT/QUIT/TERM/ALRM/PIPE. An ignored SIGTSTP/SIGTTOU/SIGTTIN/SIGUSR* in the server (from a parent or a library) would leak into every pane. Resetting all signals 1..NSIG to `SIG_DFL` is cheap and matches the "clear inherited state" intent.
- **Doc wording on `PtyCommand::cwd`.** It says a missing or non-directory path "falls back to HOME". HOME itself is not checked: a relative or missing HOME makes `spawn()` fail with a bare chdir error. The check also runs in the parent (time-of-check/time-of-use gap, harmless).
- **Non-UTF-8 `SHELL` is silently ignored.** `resolve_shell` uses `OsStr::to_str`, so in pane mode a non-UTF-8 `SHELL` quietly becomes `/bin/sh`, which contradicts "reject an invalid selected shell".
- **Missing SAFETY comments in `command.rs`.** The three `unsafe` blocks in `passwd_field` and `access_ok` have none, although every other unsafe block in the crate is documented.
- **A burst of wakes delays PTY IO.** When `wake_ready` fires, `run()` does `continue` even if the PTY was also readable or writable, so a steady stream of wakes (keystrokes, timer responses) postpones PTY work. There is no correctness bug, but reads could be serviced in the same iteration.
- **Missed wakes are only caught by the 1 s idle poll.** The fallback is documented, but wake writes that hit `EAGAIN` are treated as success, which is correct only because the pipe is non-empty at that point. That holds as written.
- **Design note: the actor has four synchronisation channels** (tokio mpsc, std mpsc for control, a `Mutex<SharedPtyControls>`, a `response_order` mutex) plus a `UserWriteGate` mutex. A single mutex-protected inbox (bounded bytes, latest resize, shutdown flag) plus the wake pipe would remove the cross-channel ordering issues (items 1 and 6) and the unreachable `Full` case. That is a worthwhile rewrite.

## Checked and fine

- Close-on-exec and fd hygiene: both PTY ends are cloexec, the parent's slave is dropped, and `close_range(CLOEXEC)` covers inherited fds; its read_dir fallback is acceptable because no custom global allocator is set in `/home/folk/Programs/shepr/Cargo.toml`.
- setsid/TIOCSCTTY ordering, login argv0, and the environment being fully replaced by `env_clear` then `envs`.
- Opening the pidfd before the watcher starts.
- The submission cancel state machine, including the first-byte-under-lock race.
