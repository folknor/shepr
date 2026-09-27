## Terminal-core hygiene reading: shepr-vt, shepr-pty and their consumers in shepr-mux

I made no edits and ran no builds or tests. All findings come from reading the code, the pinned vte 0.15.0 source in the cargo registry, and grep. Where a finding is my inference rather than something I observed, I say so. For each finding I give a verdict on whether the fix could be held by the build ("Enforce").

### Defects found while reading (also listed under their questions below)

- **D1. Reply ordering is broken by the sync-timeout timer.** In `/home/folk/Programs/shepr/crates/shepr-mux/src/pane/runtime.rs` (`rt.spawn` -> `spawn_blocking`), the timer calls `flush_expired_synchronized_output`, which produces replies *outside* the actor's `response_order` lock. It then queues each reply with its own `write_terminal_response(|| Some(response))` call.
  - A PTY read on the actor thread can parse and queue newer replies in between, so older replies land behind newer ones.
  - `PtyIoActorHandle::response_order` documents exactly this guarantee ("replies enter the inbox in the order the terminal produced them"). Nothing enforces it: the closure-based API accepts an already-computed value.
  - Enforce: yes, by type. Make the only way to queue a reply `write_terminal_responses(|| -> Vec<Bytes>)`, run the flush inside that closure, and remove the single-value form.
- **D2. Render and read paths mutate the terminal.** In `/home/folk/Programs/shepr/crates/shepr-mux/src/pane/terminal/backend.rs`, `render()`, `collect_dirty_patch()`, `synchronized_output_active()` and `synchronized_output_state()` all call `flush_expired_synchronized_output`. That feeds the buffered frame through the parser, changes the grid, and queues replies and clipboard writes.
  - This violates AGENTS.md's "Render is pure".
  - These flushes don't bump `content_seq` or `detection_content_seq`. The timer path bumps `detection_content_seq` deliberately, so a frame flushed by a render is invisible to detection's change counter.
  - The replies wait until the next read or timer.
  - Enforce: structurally. Give `Terminal` one `tick(now)` entry point owned by the runtime, and give render paths `&Terminal` only. Today they take `&self` on a type that locks a `Mutex`, so the signature can't express it.
- **D3. The event loop can wait on `/proc` scans.** `read_chunk` in `/home/folk/Programs/shepr/crates/shepr-pty/src/actor.rs` holds `response_order` across the whole `on_read` callback. That callback runs `apply_process_result`, which does:
  - `resolve_default_color_owner`, a `/proc` scan;
  - `publish_reported_cwd`, a readlink via `process_cwd`.

  `PaneRuntime::resize` and `apply_host_terminal_appearance` take the same lock from the app side, so they block behind another thread's `/proc` walk. The comment in `backend.rs` says the caller releases the terminal and content locks before the scan; the reply-order lock is still held. Enforce: no lint can see this. The fix is structural: return the effects from `on_read`, run them after the lock is released, and put the lock order (response_order > content_write_lock > core) in one documented place.
- **D4. The pane can freeze silently with the child still alive.** `ReaderExit::Closed` covers EOF, but also poll failure and wake-pipe drain failure. Both of those are logged at `debug!`. The mux ignores `Closed` because it expects the child watcher to report. When the loop ends for one of those reasons, nobody reads the PTY: the child blocks on a full PTY, nothing is reported, and the only trace is a debug line. Enforce: by type. Give the actor a third `ReaderExit` variant (for example `Failed(io::Error)`) that the owner must handle.
- **D5. A flaky test with a lock that guards nothing.** `pty_spawn_leaves_one_parent_pty_fd` in `/home/folk/Programs/shepr/crates/shepr-pty/src/backend.rs` counts every `/dev/pts` fd in the process. The two sibling tests open PTYs in parallel without taking `pty_fd_test_lock`, which only that one test takes. Enforce: count only the fds this test created, or give every PTY test the same guard.
- **D6. A race that can leak PTY master fds.** Both `openpty` in `backend.rs` and `create_wake_pipe` in `fd.rs` set `FD_CLOEXEC` *after* creating the fds.
  - Pane children are covered by `mark_inherited_fds_cloexec`.
  - Any other concurrent `std::process::Command` spawn is not: git status, and especially ssh or a ControlMaster. If one of those runs in the gap, it can inherit a pane's master fd, and the PTY never hangs up.
  - Enforce: yes. Use `pipe2(O_CLOEXEC|O_NONBLOCK)` for the wake pipe, and `posix_openpt(O_CLOEXEC)` plus opening the slave with `O_CLOEXEC`.
  - The later `set_cloexec` on the master in `PtyIoActor::spawn_inner` is redundant.
- **D7. `default_shell` is never validated at launch.** It is stored as a raw `String` in `/home/folk/Programs/shepr/crates/shepr-config/src/validated.rs`.
  - PATH lookup and the executability check happen in `PtyCommand::to_std_command` at every spawn, so a typo fails each pane rather than the launch. That contradicts "Any config problem fails the launch".
  - `default.toml` says "Empty means $SHELL, then /bin/sh", but a `$SHELL` that is set and invalid is an error in pane mode, not a fall-through to `/bin/sh`.
  - Enforce: by type. A `ValidatedShell` produced at config load.

### 1. One value, one owner

- **`SHEPR_ENV` name and value defined twice.** They appear in `/home/folk/Programs/shepr/src/main.rs` (read by the nested-launch check) and in `/home/folk/Programs/shepr/crates/shepr-mux/src/pane/launch.rs` (written into panes). The hermes hook asset checks `== "1"` as well. They agree today. Enforce: define them once in shepr-config next to `SOCKET_PATH_ENV_VAR`; the hook asset can't share it (deployment constraint), and a test that greps the assets for the constant's spelling would keep it in step.
- **`SHEPR_BIN_PATH` is a literal in two crates:** `launch.rs` and `/home/folk/Programs/shepr/crates/shepr-server/src/app/tab_bar_status.rs`. The qwen, letta, hermes and qodercli assets also spell it. It needs the same fix as above.
- **DEC mode numbers have three owners:**
  - `pub const MODE_*` in `/home/folk/Programs/shepr/crates/shepr-vt/src/lib.rs`;
  - literal numbers in the `MODES` table in `modes.rs` (1004, 1005, 1006, 1007, 1016, 2004 and 2031 are literals even though constants exist);
  - private `MODE_MOUSE_X10/PRESS_RELEASE/BUTTON_MOTION/ANY_MOTION` (9, 1000, 1002, 1003) in `/home/folk/Programs/shepr/crates/shepr-mux/src/pane/terminal.rs`.

  The mux also ORs those four modes itself (`backend.rs`, `wheel_routing`), although `Terminal::mouse_tracking_enabled()` already computes the same thing and is used two methods earlier in the same file. Enforce: by type. A `DecMode` enum with `number()`, used for both the table and the API, and `mode_get(DecMode)` instead of `u16`.
- **Pixel geometry (`cols * cell_width`) is computed four times with three overflow policies:**

  | Site | Arithmetic |
  |---|---|
  | `fd.rs::resize_pty_fd` (TIOCSWINSZ) | clamped to u16 |
  | `handler.rs::in_band_size_report` and `text_area_pixels_report` | u64 |
  | `Terminal::width_px` / `height_px` | saturating u32 |

  For a large pane (over 65535 px) the child's TIOCGWINSZ and its `CSI 14 t` answer already disagree. Enforce: a `PaneGeometry::text_area_px()` in shepr-core that every site calls.
- **The halfwidth voiced-mark rule has three owners:**
  - `cell.rs::is_halfwidth_voiced_mark` in vt;
  - `is_halfwidth_katakana_voiced_mark` and `..._grapheme` in `/home/folk/Programs/shepr/crates/shepr-mux/src/pane/terminal/helpers.rs`;
  - a verbatim copy of `is_halfwidth_katakana_voiced_grapheme` in `/home/folk/Programs/shepr/crates/shepr-termio/src/blit.rs`.

  The mux copy exists because `ghostty_buffer_symbol_into` measures with raw `unicode_width` instead of `shepr_vt::unicode_text_width`, then patches around the difference. Enforce: export the predicate from vt. A text rule banning the U+FF9E/U+FF9F spelling outside shepr-vt is feasible.
- **The kitty placeholder filter is spelled five times.** vt's `cell_text` already classifies U+10EEEE as `Empty`, so the mux checks can never fire:
  - `helpers.rs` in `ghostty_cell_symbol` and `ghostty_buffer_symbol_into`;
  - `/home/folk/Programs/shepr/crates/shepr-mux/src/pane/terminal/text.rs`;
  - `/home/folk/Programs/shepr/crates/shepr-mux/src/terminal/history_read.rs`.

  See also Q8. Enforce: stop making `KITTY_UNICODE_PLACEHOLDER` public.
- **Underline flag mapping and named-colour thresholds are duplicated.** The underline ladder appears in both `cell.rs::cell_style` and `format.rs::push_sgr`. The "named index >= 16 means default" rule appears in both `cell_color` and `push_color`. `"\x1b]8;;\x1b\\"` appears twice in `format.rs`. Enforce: one `UnderlineStyle::from_flags` and one SGR table.
- **`ScreenTextRow` carries `soft_wrapped` and `wrap_continuation` flat, next to a `RowWrap` type with the same two fields.** The comment admits it. Enforce: by type (embed `RowWrap`).
- **`/bin/sh` appears five times in `command.rs`,** across two fallback chains: `passwd_shell` falls back to `/bin/sh`, then `resolve_shell` falls back to `/bin/sh` again. Enforce: one const.
- **The seqlock protocol on `content_seq` is hand-copied at six sites:** `on_read`, the timer, `resize`, `clear_screen`, `test_process_pty_bytes`, and `test_contend_during_dirty_collection`. Each copy locks `content_write_lock`, does `fetch_add(AcqRel)`, the mutation, then `fetch_add(Release)`. Enforce: by type. A `ContentWriteGuard` whose constructor and `Drop` do the two increments.
- **The synchronized-output epoch bump (`if before != after { epoch += 1 }`) is repeated four times** in `backend.rs` and `helpers.rs`.
- **The shell-env trim rule is applied twice:** `interactive_shell` trims `default_shell`, then `trimmed_shell` trims `$SHELL` again.
- **`PANE_TERM` has one owner (good), but its sibling `PANE_COLORTERM = "truecolor"` lives in mux.** XTGETTCAP in `scan.rs` advertises `Tc` and `RGB` independently of it. Enforce: keep the terminal-identity claims together in vt.
- **A test re-spells both values:** `pane_terminal_identity_overrides_outer_terminal_env` in `runtime.rs` hard-codes `"xterm-256color\ntruecolor\n"` instead of reading `PANE_TERM` and the colorterm constant.

### 2. Values nobody can find, change or trust

- **Nothing lists this scope's tunables.** They are scattered:
  - top of `actor.rs`: `ACTOR_IDLE_POLL_MS`, inbox byte and item caps, the resize retry constants;
  - inside functions: `MAX_DRAIN_CHUNKS = 1024` in `handle_write_failure`, the read buffer `8192` and the wake-drain buffer `64`;
  - `scan.rs`: `MAX_OSC_BYTES` and the other scanner limits;
  - vt `lib.rs`: scrollback floor and cap, `MAX_CLIPBOARD_BYTES = 192 KiB`;
  - mux `terminal.rs`: `SYNCHRONIZED_OUTPUT_FLUSH_MARGIN`, `DEFAULT_DETECTION_ROWS`;
  - `runtime.rs`: `MIN_PANE_ROWS/COLS` and the detection task's initial `sleep(50ms)`.

  Enforce: a text rule forbidding numeric `const` inside function bodies is feasible; one `limits` module per crate is a convention.
- **Minimum size is clamped at three layers with different minimums:** vt (2 columns, 1 row), mux (4 columns, 2 rows), and core `GridSize::clamped` (1, 1).
- **The sync-update clock has no injection point.** vt's `Processor<T: Timeout>` is generic (vte-0.15.0 `ansi.rs`), but shepr uses the default `StdSyncHandler`, and `flush_expired_synchronized_output` reads `Instant::now()` itself. As a result:
  - `/home/folk/Programs/shepr/crates/shepr-vt/src/tests.rs` (`synchronized_output_buffers_until_end_or_timeout`) sleeps through vte's 150 ms timeout;
  - that same test asserts `!flush_expired...` right after a write, which will flake under load.

  Enforce: by type. A shepr-owned `Timeout` impl driven by an injected clock.
- **The oversized clipboard drop has two different caps.** `MAX_CLIPBOARD_BYTES` silently drops OSC 52 payloads over 192 KiB, with no log. `shepr-platform/src/clipboard.rs` uses a separate 1 MiB cap for reads. The two caps have unrelated owners.

### 3. One channel, one implementation

- **The same event is logged twice under different names.** A poisoned core is logged as `"ghostty core lock poisoned in reader"` in mux `backend.rs`, and again by the actor as "terminal core is broken ... closing the pane". The ghostty backend no longer exists.
- **Level inconsistency.** Actor read failure, poll failure and wake-drain failure are `debug!`. Write failure is `warn!`. All of them end the pane's IO loop (see D4).
- **Silent drops with no log at all:**
  - terminal replies that overflow the inbox (`push_terminal_response`, `let _ =` in `read_chunk`);
  - resize replies refused by `reserve` in `replace_resize`;
  - replies from the timer before the actor handle is set (`timer_writer.get()` returns `None`);
  - `enable_utf8_input` failures.

  The first is documented as deliberate, but a counter or a rate-limited log would let an operator see it happen.
- **`ghostty_collect_dirty_patch` takes a `fallback!($reason:literal)` and throws the reason away.** The reasons are never logged or counted, so a fallback storm is invisible. The same macro pattern appears in `retained_surface.rs`. Enforce: make the macro record the reason.

### 4. Errors

- **D4 and D7 above.**
- **`to_std_command` quietly substitutes home for a bad cwd** (warn only), while the API validates `new_cwd` upstream. Two policies for the same value.
- **`prepare_pty_child` ignores the return codes of `sigemptyset` and `sigprocmask`.**
- **`synchronized_output_state` returns `(true, 0)` on a poisoned core,** a made-up value rather than an error.
- **The pty `Error` for the argv program keeps its context** ("unable to spawn X: ..."). That part is good.

### 5. Tests that prove nothing

- **`primary_screen_replay_honors_ed3_for_droid_at_chunk_boundaries`** (`/home/folk/Programs/shepr/crates/shepr-mux/src/pane/terminal/migration_tests.rs`) spawns host `bash` and polls `/proc` for a process named "droid" so it can pass a real pid. But `GhosttyPaneTerminal::process_pty_bytes` ignores `_shell_pid`. The setup does nothing: the test is pure environment dependence, as its own comment about "the former process-specific filter" admits.
- **`capture_bounded_migration_observations`** asserts `observations.last() == terminal.observe()` on unchanged state, so it cannot fail. Its real purpose, dumping to `SHEPR_MIGRATION_OBSERVATIONS`, belongs to a finished migration.
- **`child_sees_resolved_shell_not_a_non_executable_shell_env`** (`command.rs`) compares against `cmd.resolve_shell(...)`, which is the same function under test. Only its `assert_ne` does any work.
- **`login_shell_execs_shell_env_without_arguments`,** and the mux twin `login_shell_builder_uses_one_resolved_path...`, never check the `-sh` argv0 that makes the shell a login shell. `std::process::Command` doesn't expose `arg0`, so only a spawn test can check it.
- **`pane_terminal_identity_allows_explicit_override`** applies the override with a raw `cmd.env` after `apply_pane_terminal_env`. The production override path is `PaneLaunchEnv::extra` in `apply_pane_launch_env`, so the test can't fail.
- **Actor tests use `UnixStream` socket pairs, not PTYs.** PTY-specific behaviour is never exercised: EIO on slave close, POLLHUP semantics, TIOCSWINSZ. They accept `BrokenPipe | ConnectionReset | WriteZero`, but a real PTY master reports EIO. One end-to-end actor-on-openpty test would close the gap.
- **Environment dependence:**
  - `/bin/sh`, `/bin/cat`, `sleep` and `printf` in the pty tests;
  - `bash` in the mux migration tests and the shepr-agent `detect` tests;
  - wall-clock assertions: `< 500ms`, `>= delay/2`, `< 200ms` in `runtime.rs` teardown.
- **`process_cwd_does_not_require_traversing_the_directory_path` passes silently when run as root** (it `eprintln!`s "skipping").
- **`pane_terminal_identity_removes_outer_terminal_identity` restates the production scrub list verbatim.** A key added to production won't be tested. Enforce: export the list and iterate it.

### 6. Guards and claims that have stopped holding

Stale today:

- **AGENTS.md says `scan.rs` scans "modes 9/1016/2031/2048 ... and the halfwidth katakana voiced marks".** Those live in `handler.rs`. `scan.rs` explicitly forbids them and has a test for it (`modes_and_reset_are_left_to_the_parser_handler`). AGENTS.md also omits OSC 9;4, 9;9, 1337 and `CSI ? 3 J`. Its module list for shepr-vt omits `handler.rs`, `modes.rs`, `rows.rs`, `selection.rs`, `coords.rs` and `locks.rs`. This is a doc restating a list the code owns.
- **AGENTS.md says the child is "reaped by a blocking `wait()` in the pane runtime".** It is reaped through a pidfd `waitid`; blocking wait is only the fallback (commit b7c14da).
- **The `format.rs` module doc says the VT output is replayed "after some resizes".** `backend.rs::resize` says that replay was removed.
- **The `lib.rs` doc for `DEFAULT_FOREGROUND` says it matches "what the libghostty-vt render state reported".** That backend is gone.
- **The root `Cargo.toml` comment cites a "reference checkout in research/alacritty".** No `research/` directory exists.

Unenforced but holding today:

- **`handler.rs` claims "Every `Handler` method is listed explicitly".** I checked the impl against vte-0.15.0: it is complete today. Two things could break it:
  - vte is not pinned (only `alacritty_terminal` is `=0.26.0`, and vte arrives transitively at `^0.15`), so a `cargo update` can move it;
  - a new defaulted trait method would be silently a no-op.

  Enforce: `#[warn(clippy::missing_trait_methods)]` on that impl makes the claim mechanical. Separately, pinning vte with `=` would need `vte` added to the shepr-vt allowlist in `brokkr.toml`.
- **"Alacritty types never leak out".** The dependency rule enforces this at crate level, and I found no alacritty types in public signatures. Two soft spots:
  - the public `impl From<Rgb> for RgbColor` and `From<RgbColor> for Rgb`;
  - `CellStyle` is `pub` in a private module and reachable through `CellBasicData.style`, but not re-exported, so callers can't name it.
- **`PtyIoInbox` assumes entry `order` identifies one entry.** `insert_resize_replies` inserts several entries with the *same* order, and `next_entry_index(current_order)` uses `position(order == order)`. It is correct only because those replies are contiguous and written front to back. Enforce: give each reply its own order.
- **A mutation rule nothing checks.** "Every parser-driven mutation must use `with_handler`", and every mutation must end with `collect_damage()` (or `bump_full_damage`). Callers do this by hand: `write`, `flush`, `mode_set`, `resize`, the scroll methods. Enforce: structurally, by calling `collect_damage` inside `with_handler`.
- **`SHEPR_AGENT` is scrubbed from pane env** (`LAUNCH_ENV_TO_SCRUB` in shepr-agent), but nothing in the repo sets it. The guard is keyed on a name that nothing produces; it looks like a herdr leftover.

### 7. Policy invented per call site

- **The sync-timeout flush runs from six call sites,** each re-implementing "flush if expired, bump epoch": `Terminal::write`, `render`, `collect_dirty_patch`, `synchronized_output_active`, `synchronized_output_state`, and the runtime timer. See D1 and D2.
- **The clock is ambient** in `flush_expired_synchronized_output`, `apply_due_resize`, `poll_timeout_ms` and `SubmissionState` (see Q2).
- **Lock order has no single documented home:** response_order > content_write_lock > terminal core, with the inbox lock never held across a syscall. It is spread across comments in `actor.rs`, `runtime.rs` and `backend.rs`. D3 is the resulting lock held across blocking work.
- **Test-only shortcuts reachable from production:**
  - `PaneRuntimeIo::TestChannel`, compiled under `feature = "test-api"`, reimplements the actor: submissions as a thread with `sleep`, `try_send`, no ordering, `SubmissionCancel::untracked()`, resize replies discarded.
  - The root `Cargo.toml` enables `shepr-server/test-api` as a dev-dependency. My inference, not verified: with resolver 3 this means the binary built for `cargo test` carries it.
  - `SubmissionCancel::untracked()` and `never_started()`, and `backend::open_pty` / `spawn_in_pty`, are public solely for tests in other crates.
- **shepr-pty depends on tokio for one import:** `tokio::sync::mpsc::error::TrySendError`. The actor is a std thread; the error type exists to match the test double's `mpsc::Sender`. Enforce: remove `tokio` from the `shepr-pty-layer` allowlist in `brokkr.toml` and use a shepr-owned error type.
- **The environment passed to panes is inherited wholesale** (`base_env` is `std::env::vars_os()`), then scrubbed by a denylist split across `launch.rs` (host terminal keys) and shepr-agent (agent keys). Server-only variables are removed ad hoc elsewhere; for example `SHEPR_STARTUP_CWD` is removed with `unsafe remove_var` in `headless/bootstrap.rs`. Nothing lists which `SHEPR_*` variables a pane may inherit. Enforce: one list in shepr-config, and a test that every `*_ENV_VAR` constant is either scrubbed or explicitly allowed.

### 8. Code that is no longer load-bearing

- **The Ghostty naming layer:**
  - `GhosttyPaneTerminal`, `GhosttyPaneCore`, `PaneTerminal { ghostty }` and around 50 `ghostty_*` helpers wrap the alacritty-backed `shepr_vt::Terminal`;
  - `PaneTerminal` is almost entirely one-line forwards to `GhosttyPaneTerminal` (`/home/folk/Programs/shepr/crates/shepr-mux/src/pane/terminal.rs`);
  - `PaneRuntime` forwards again, so there are three hops to reach vt;
  - the mux terminal files contain several hundred ghostty references.

  Dead as a concept: only one backend exists. I'd collapse `PaneTerminal` and `GhosttyPaneTerminal` into one type and rename the helpers.
- **Parameters nothing reads:** `_shell_pid` in `process_pty_bytes`, and `_pane_id` / `_shell_pid` in `flush_expired_synchronized_output`. They are threaded from the runtime and ignored.
- **Switches with one value:**
  - `hide_kitty_placeholders = true` (twice), and the parameter it feeds;
  - `PtyIoActorConfig.on_reader_exit` and `core_broken` are `Option`, but production always passes `Some`;
  - `fail_active_submission` and `read_once` are single-line aliases.
- **The kitty placeholder checks in mux are unreachable** (see Q1): vt's `cell_text` already blanks those cells.
- **`migration_tests.rs`** is scaffolding from the finished ghostty-to-alacritty migration: the "old/candidate captures" harness, the observation dump and the droid pid gate.
- **The resize retry and backoff machinery** in `actor.rs` covers `RESIZE_RETRY_*`, `RESIZE_HOLD_ATTEMPTS`, the reply-holding logic and three tests. My inference, not verified: TIOCSWINSZ on a live PTY master has no transient failure mode (only EBADF, EFAULT or ENOTTY), so the retry loop protects against an error that retrying can't fix. It is worth confirming before deleting; logging once and moving on would do the same job.
- **`PtyReadResult::core_broken` and `PtyIoActorConfig::core_broken` report the same fact through two channels.** The per-loop check alone would do if it also ran after each read.
- **`Setter::Vte(NamedPrivateMode)` and `ModeSpec::name`** are `#[allow(dead_code)]` "documents the table". They are read only by tests.

### Two structural suggestions

- **One terminal-core owner type in mux.** Merge `PaneTerminal` and `GhosttyPaneTerminal` into one type that owns:
  - the seqlock guard (Q1);
  - the flush/tick entry point (D2);
  - the reply-producing closure contract (D1);
  - the effect application outside the reply lock (D3).

  Most findings here are about there being no one place where those rules live.
- **A shepr-owned vte `Timeout` with an injected clock.** It removes the ambient clock from vt, makes the sync tests deterministic, and gives the render and timer paths one `now`.
