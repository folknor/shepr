# Hygiene hunt: shepr-core, shepr-platform, shepr-test-support

Scope read in full: `crates/shepr-core/src/{lib,geometry,layout,pathutil,workspace_label}.rs`,
`crates/shepr-platform/src/*` (all 16 modules plus `tests.rs` and
`remote_bridge_tests.rs`), `crates/shepr-test-support/src/lib.rs`, the three
`Cargo.toml`s, and every consumer site these led to in `shepr-mux`,
`shepr-pty`, `shepr-remote`, `shepr-config`, `shepr-api`, `shepr-client`,
`shepr-termio`, `shepr-server` and the root binary.

Each finding says whether it is a **fact** (the copies already disagree, or the
code already does the thing) or a **prediction** (one owner today, no rule
keeping it that way), and what could hold the fixed version mechanically.
"brokkr rule" means a new `[[dependency_rule]]`, gremlin-style text scan, or
manifest rule in `brokkr.toml`; "clippy" means a lint already available or a
`disallowed_methods`/`disallowed_types` entry in `clippy.toml`.

---

## 1. One value, one owner

### 1.1 Two different rules for "am I running under WSL" - FACT, diverged

`shepr-platform/src/host.rs::detect_running_inside_wsl` is the owner: four
signals (`/proc/sys/kernel/osrelease`, `/proc/version`, `WSL_DISTRO_NAME` or
`WSL_INTEROP`, `/run/WSL`), memoised in a `OnceLock`.

`shepr-platform/src/terminal_environment.rs::prefers_osc52_clipboard` calls it
and then **adds a fifth signal of its own**:
`std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()`.

So on a host where only the binfmt marker is present, the process answers two
different ways about the same fact in the same run:
`should_draw_host_cursor_by_default()` and `should_query_host_terminal_palette()`
(both `running_inside_wsl()`) say "not WSL", while the clipboard path says
"WSL". Not hypothetical - it is two expressions of one predicate, 40 lines
apart, in the same crate. The extra probe belongs in
`detect_running_inside_wsl`'s list; `prefers_osc52_clipboard` should take
`running_inside_wsl()` unmodified.

Enforceable: partly. A unit test asserting `prefers_osc52_clipboard_for_env`'s
`wsl` argument is exactly `running_inside_wsl()` is not expressible while the
probe is inline; making `detect_running_inside_wsl` the only place that names a
WSL marker string, plus a text rule in `brokkr.toml` forbidding `WSL` outside
`host.rs`, is enforceable.

### 1.2 Three different rules for resolving `$HOME` - FACT, diverged, one is a live defect

`shepr-core/src/pathutil.rs` is the declared owner and its doc comment states
the policy explicitly: unset, empty or relative `HOME` is an **error, never a
fallback**, because "every caller builds a path under it, and an invalid home
would make that path relative to the current directory".

Two other rules exist:

- `crates/shepr-mux/src/git/config.rs::normalize_gitdir_include_pattern` and
  `::resolve_include_path` both do
  `std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest)`
  for a `~/` prefix. With `HOME` unset or empty that yields exactly the failure
  pathutil exists to prevent: `~/x` becomes the relative path `x`, resolved
  against whatever cwd the server happens to have. Two sites, same file, both
  reachable from git config parsing. **This is a real bug, not just a
  duplication**: `expand_tilde_path` is already public and returns the right
  error.
- `crates/shepr-pty/src/command.rs::home_dir` requires `HOME` to be absolute
  **and an existing directory**, then falls back to the passwd entry, then to
  `/`. A third policy with two silent fallbacks, used as the pane cwd fallback.

Enforceable: yes, mechanically. Delete the ad-hoc expansions, then add a
`brokkr.toml` text rule forbidding `"HOME"` as a literal outside
`shepr-core/src/pathutil.rs` and `shepr-test-support` (and a
`clippy.toml disallowed_methods` entry is not enough, since the offence is the
argument, not the method).

### 1.3 The `SHEPR_*` environment namespace has no owner - FACT

40-odd distinct `SHEPR_*` names across 20 files. `shepr-config/src/address.rs`
owns two of them as constants (`SOCKET_PATH_ENV_VAR`,
`CLIENT_SOCKET_PATH_ENV_VAR`), and then:

- `crates/shepr-remote/src/remote/local_server.rs:247,263` spells
  `"SHEPR_CLIENT_SOCKET_PATH"` as a literal in the same test that uses
  `shepr_config::SOCKET_PATH_ENV_VAR` as a constant on the adjacent line.
- `crates/shepr-api/src/session.rs` bakes both names into operator guidance
  strings as literals.
- `shepr-platform/src/logging.rs:32` owns `SHEPR_LOG` as a bare literal with no
  constant at all, and it appears in no registry, no `--help` text, and no doc.
- `shepr-test-support` scrubs the namespace by the prefix `"SHEPR_"` (see 6.2).

There is nothing that answers "what environment variables does shepr read".
Since `shepr-platform` sits below `shepr-config`, the honest fix is a single
`env` module in `shepr-core` (which depends on nothing) holding every name as a
`pub const`, with `shepr-platform`, `shepr-config` and the binary reading from
it.

Enforceable: yes - a `brokkr.toml` text rule forbidding the literal substring
`SHEPR_` outside that one module and `shepr-test-support`, with test-only probe
variables exempted by listing them there too.

### 1.4 Exit code `1` is spelled at six sites, and one of them is a library - FACT

`src/cli/error.rs::exit_code()` is the owner of shepr's exit statuses. Bypassing
it: `shepr-platform/src/remote_bridge.rs:56` (`std::process::exit(1)` from a
watchdog thread inside a library crate), `shepr-client/src/lib.rs:311`,
`shepr-server/src/server/headless/bootstrap.rs:45,75`, `src/main.rs:49,202,218,
233,243`. The bridge's `1` is then re-spelled a seventh time as an assertion in
`crates/shepr-platform/src/remote_bridge_tests.rs:139`
(`assert_eq!(bridge.wait().code(), Some(1))`).

Enforceable: yes - move every code into the `exit_code` owner as named
constants and add a `clippy.toml disallowed_methods` entry for
`std::process::exit` outside `src/main.rs`. See 4.3 for why the library exit is
worse than a duplicated literal.

### 1.5 Two public names for one split-ratio policy - FACT

`shepr-core/src/layout.rs` exports both `SplitRatio::clamped(f32)` and the free
function `valid_split_ratio(f32) -> SplitRatio`, whose entire body is
`SplitRatio::clamped(ratio)`. Both are used externally: `clamped` from 36
sites, `valid_split_ratio` from `shepr-mux/src/persist/restore.rs:827` and three
internal layout sites. Two names for one rule means a future change to the
policy has two doors.

Enforceable: yes - delete `valid_split_ratio`; the compiler enforces the rest.

### 1.6 The layout bounds `0.1`, `0.9`, `0.5` are magic numbers at five sites - PREDICTION

`layout.rs:17` (`(0.1..=0.9)`), `:22` (`clamp(0.1, 0.9)`), `:24` (NaN default
`0.5`), `:202` (`split_focused` hardcodes `0.5`), `:376` (missing-ratio default
`0.5`). Nothing names "minimum pane share" or "even split". The test at `:849`
re-spells `0.1`/`0.9`/`0.05` again.

Enforceable: yes - `const MIN_SPLIT_RATIO/MAX_SPLIT_RATIO/EVEN_SPLIT` plus
`clippy::disallowed_script_idents` does not cover it, but a test asserting
`SplitRatio::clamped(MIN - eps).get() == MIN` keeps the const and the clamp in
step.

### 1.7 Two distinct types named `DeadlineReader` in one crate - FACT

`shepr-platform/src/ipc.rs:419` (re-arms `SO_RCVTIMEO` per read on a
`LocalStream`) and `shepr-platform/src/clipboard.rs:118` (generic
`R: Read + AsRawFd`, polls with `poll_timeout_until`). Same name, same crate,
same concept - "a reader with an overall deadline" - two implementations. The
clipboard one is the general shape; the ipc one only exists because
`interprocess`'s `set_recv_timeout` is the only knob on that stream. They
should be one type with two backends, or at minimum two distinct names.

Enforceable: no mechanical rule catches duplicate private type names. A
signature that makes it unrepresentable (one `Deadline<R>` in `child_io.rs`
used by both) is the enforcement.

### 1.8 Retry counts and poll intervals invented per site - PREDICTION

Within `shepr-platform` alone: `STAGING_ATTEMPTS = 4` (`ipc.rs:255`) vs a bare
`for _ in 0..16` for the same "random name collided, try again" policy
(`ssh_paths.rs:34`); `POLL_INTERVAL = 5ms` declared twice in `clipboard.rs`
(`:141`, `:253`); `START_TIME_EXIT_RECHECK = 10ms` (`process.rs`); a hardcoded
`100` ms in `client_stream.rs::wait_client_stream_readable`; `16 * 1024` copy
buffer in `remote_bridge_io.rs` vs `8192` in `child_io.rs::read_limited_reader`.

Enforceable: partly - a `tunables` module per crate (see 2.1) plus a text rule
forbidding bare integer millisecond literals in `Duration::from_millis` outside
it.

### 1.9 `MAX_CLIPBOARD_TEXT_BYTES` is restated as a magic number in its own test - FACT

`clipboard.rs:186` declares `const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 *
1024` inside a function body; `tests.rs:830` asserts the limit with
`yes x | head -c 1048578`. Change the constant and the test still passes while
testing nothing in particular.

Enforceable: yes - hoist the const to module scope and have the test compute
`MAX_CLIPBOARD_TEXT_BYTES + 2`.

### 1.10 The Unix socket path limit is restated in prose and in a test literal - FACT (with a forced-copy reason)

`ssh_paths.rs:12` owns `UNIX_SOCKET_PATH_MAX = 107`. Restatements:
`ipc.rs:120` ("without using any of `sun_path`'s 107 bytes"),
`tests.rs:207-208` (`"x".repeat(107)`, `"x".repeat(108)`),
`shepr-test-support/src/lib.rs:73` ("`sun_path` (108 bytes)").

The test-support copy is a **forced duplication**: `brokkr.toml`'s
`shepr-test-support-layer` rule allows only `libc`, so it cannot read
`shepr-platform`'s constant, and that restriction is right (test-support is
below everything). What keeps them in step: nothing today. It is prose, so the
cheapest answer is to make it prose that cannot drift - "must fit in
`sun_path`" without the number.

Enforceable: the in-crate copies yes (export the const, use it in the test).
The test-support prose copy, no - reword it instead.

### 1.11 `/etc/ssh/ssh_config` is hardcoded, in an `Option` that is never `None` - FACT

`ssh_paths.rs:20-25`: `system_config: Some(PathBuf::from("/etc/ssh/ssh_config"))`.
The `Option` shape claims a case the code cannot produce, and the sole consumer
(`shepr-remote/src/remote/ssh.rs:505`) must handle it anyway.

Enforceable: yes - make the field a `PathBuf`; the type system does the rest.

---

## 2. Values nobody can find, change, or trust

### 2.1 There is no answer to "what are this layer's tunables" - FACT

Every knob in `shepr-platform` is a private const next to its first use:
`CLIPBOARD_HELPER_TIMEOUT` (2s), `STARTUP_WAIT` (100ms wl-copy), two
`POLL_INTERVAL`s (5ms), `MAX_CLIPBOARD_TEXT_BYTES` (1 MiB),
`DEFAULT_MAX_LOG_BYTES` (5 MiB), `DEFAULT_RETAINED_LOG_FILES` (1),
`LOG_FILE_MODE`, `PRIVATE_SOCKET_MODE`, `STAGING_ATTEMPTS`,
`UNIX_SOCKET_PATH_MAX`, `IDLE_TIMEOUT` (60s), `PROBE_INTERVAL` (1s, ssh agent),
`START_TIME_EXIT_RECHECK` (10ms), the logind backoff ceiling (60s), the
`wait_client_stream_readable` 100ms. Fifteen tunables in one crate, in eleven
files, none of them listed anywhere. Somebody asking "how long will shepr wait
for a clipboard helper" has to grep.

Enforceable: yes - one `tunables.rs` per crate holding every `Duration`,
byte-limit and mode, with a `brokkr.toml` text rule forbidding
`Duration::from_` and `0o6`/`0o7` mode literals elsewhere in the crate.

### 2.2 Log rotation size and retention have no injection point - FACT

`logging.rs::init_file_logging(dir, file_name)` hardcodes
`DEFAULT_MAX_LOG_BYTES` and `DEFAULT_RETAINED_LOG_FILES` into the
`RotatingFileMakeWriter::new` call. The struct already takes both as
parameters, and the tests use that. Production cannot set them; there is no
config key; a 5 MiB cap with one generation is a policy decision that never
reaches `shepr-config`, which is the crate that owns "read and validate once at
launch".

Enforceable: yes - move both into `shepr-config` as validated keys and pass them
to `init_file_logging`; then the launch-time validation rule covers them.

### 2.3 `SHEPR_LOG` is read at the moment of use, and an invalid value degrades silently - FACT

`logging.rs:31-32`:
`EnvFilter::try_from_env("SHEPR_LOG").unwrap_or_else(|_| EnvFilter::new("shepr=info"))`.
A typo in the filter (`shepr=inof`) produces no diagnostic of any kind - the
`Err` is discarded and the default installed. Given AGENTS.md's "Any config
problem fails the launch; no fallbacks", this is the one config input in the
project that contradicts the stated rule, and it is in the module that owns
diagnostics, so the failure cannot even be reported through itself.

The next line has the same shape: `let _ = tracing_subscriber::fmt()...
try_init();`. A second `init_file_logging` call (two roles in one process, a
test harness, a future embed) silently keeps the first subscriber and the caller
believes its writer is installed.

Enforceable: yes - return `Result` from `init_file_logging`, fail the launch on
a bad filter, and add `clippy::let_underscore_must_use` (or just make the
signature `-> io::Result<()>` so `unused_must_use` fires).

### 2.4 `prefers_osc52_clipboard` re-reads four environment variables on every call - FACT

`terminal_environment.rs:4-13` reads `SSH_CONNECTION`, `SSH_TTY`,
`VSCODE_IPC_HOOK_CLI` and stats `/proc/sys/fs/binfmt_misc/WSLInterop` per call,
while the WSL answer it combines them with is `OnceLock`-memoised in the same
crate. Called from `shepr-termio/src/host_term/title.rs:33` on every clipboard
write. The pure inner function is there and testable; the caching is what is
missing, and so is a single startup-time resolution of "does this host have a
local clipboard".

Enforceable: yes - resolve once into a value the client carries, then a
`clippy.toml disallowed_methods` entry for `std::env::var_os` outside a
designated env module makes the bad spelling unrepresentable.

### 2.5 `SplitRatio`'s validating constructor exists only in test builds - FACT

`layout.rs:15-18`: `#[cfg(test)] fn new(value: f32) -> Option<Self>`. Production
has only `clamped`, which never refuses. The test
`split_ratio_rejects_values_outside_layout_bounds` therefore tests a
constructor no shipped code path can call, and the restore path
(`shepr-mux/src/persist/restore.rs:827`) silently clamps a corrupt saved ratio
rather than refusing the layout. Given "no wire compatibility obligations" and
"validated once at launch", a stored ratio outside `0.1..=0.9` should be a
refusal of the session file, not a clamp.

Enforceable: yes - make `new` non-test and have the restore path return
`InvalidSavedLayout`, which `from_saved` already models.

### 2.6 `PaneId` allocation reaches a process-global counter directly - FACT

`layout.rs:37` `static NEXT_PANE_ID`. `alloc()` reads it; `alloc_from(&counter)`
exists purely so the exhaustion test can inject one. Any test that wants
deterministic pane ids has to use `from_raw`, which bypasses validation
entirely (it accepts `0`, the documented placeholder, while
`collect_validated_ids` rejects `0` - validation at a distance from the
constructor that can violate it).

Enforceable: partly - making `from_raw` return `Option<PaneId>` is a signature
change the compiler enforces at every site; removing the global needs an
allocator value threaded through `Workspace`, which is the larger and better fix.

---

## 3. One channel, one implementation

### 3.1 Operator text is assembled at the site, and the `shepr: ` prefix is spelled five times - FACT

`shepr-platform/src/logging.rs:25` writes
`"shepr: could not initialize file logging: {error}"` straight to stderr;
`:475` writes `"shepr: file logging resumed; log lines were lost after an I/O
error: {error}"` into the log stream itself; `shepr-client/src/lib.rs:237,289,
300` write `"shepr: ..."` to stderr; `shepr-agent/src/integration/registry.rs:280`
uses `eprintln!`. There is no function that owns "how shepr addresses an
operator", so the prefix, the capitalisation and the destination are re-decided
per site.

Worse, `logging.rs`'s two messages contradict each other about where operator
text goes: `write_with_recovery`'s comment says an outage is deliberately **not**
sent to stderr "which is the client's TUI terminal", yet `init_file_logging`'s
own failure - the strictly more serious event, in the same module, for the same
process - goes to exactly that stderr.

Enforceable: partly - one `operator_message(...)` helper plus a
`clippy.toml disallowed_methods` entry for `eprintln!`/`io::stderr` outside it
and `src/main.rs`.

### 3.2 A total clipboard failure is logged nowhere - FACT

`write_clipboard` returns `bool`. Every helper failing produces no log line at
any level, from either `shepr-platform` or the
`shepr-termio/src/host_term/title.rs:33` caller, which just falls through to
OSC 52 and then `let _ = stdout.write_all(...)`. `read_clipboard_text` returns
`Option<String>` with the same silence. So "copy did nothing" is
undiagnosable, and the two `tracing::warn!`s in the module are about reaping
the wl-copy child, not about the user's copy failing.

Enforceable: yes - a test can assert a log line via a capturing subscriber once
the module emits one; the shape change (`bool` -> `Result<(), ClipboardError>`)
is compiler-enforced at the call site.

### 3.3 The domain event catalogue lives in the bottom platform crate - FACT, structural

`shepr-platform/src/logging.rs` holds 25 functions named after concepts the
crate knows nothing about: `workspace_created`, `tab_renamed`, `pane_spawned`,
`session_saved`, `api_request_started`, `integration_action`. Each has 1-3 call
sites in `shepr-mux`, `shepr-api`, `shepr-server` or `shepr-agent`. The
consequence is that adding a workspace event requires editing the crate at the
bottom of the layering, and `api_request_started`'s **level policy** (info when
`mutates_ui && !routine`, debug otherwise) - an API-semantics decision - is
encoded three layers below the API.

The channel (rotating writer, filter, mode, file names) genuinely belongs here.
The catalogue belongs beside each subject.

Enforceable: yes, and this is the kind of rule `brokkr.toml` already expresses -
a text rule forbidding the identifiers `workspace`/`tab`/`pane`/`api`/`session`
in `shepr-platform` public item names, or simply moving the functions so the
existing dependency allowlists do the work.

### 3.4 The same class of event is logged at two levels - FACT

`api_request_failed` is `warn!`; `pane_exit_failed` and
`session_save_failed`/`session_clear_failed` are `error!`. All four are "an
operation the user asked for did not complete". Meanwhile
`bind_private_local_listener`'s fallback to an insecure-window bind is `warn!`
(`ipc.rs:282`) while `ProcessHandle::open`'s fallback to the racy start-time
identity is `debug!` (`process.rs:70`) - the second is the more consequential
degradation and the quieter line.

Enforceable: no mechanical rule. A documented level policy in the logging module
plus review is the only lever.

### 3.5 Lines that omit the identifier someone would need - FACT

- `session_restored(workspaces, outcome)` logs no session id and no path, while
  its siblings `session_saved`/`session_cleared` both log `path`.
- `shutdown.rs:89` `"host shutdown requested; preserving session before pane
  termination"` carries no generation number, though the generation is the whole
  correctness mechanism of that module and appears in the `debug!` release line.
- `shutdown.rs:142` logs `err` and `retry_seconds` at `debug!` - losing the
  logind connection while a shutdown is pending is the case where the session
  may not be saved, and it is below the default `shepr=info` filter.
- `pane_exited(pane_id, status: &str)` takes a pre-formatted status string built
  at the call site, so the format of the most-read pane line is decided outside
  the module that owns the channel.

Enforceable: partly - a test can assert required fields per event if the events
become structs rather than free functions with positional arguments.

### 3.6 Significant events with no log at all - FACT

- `SocketStartupLock` acquisition and release: nothing. Losing the race for it
  is turned into an `AddrInUse` error message but never logged.
- `bind_private_local_listener` succeeding via **staging** vs via the insecure
  in-place path: only the failure warns; there is no record of which path a
  running server actually took.
- `remote_bridge.rs:56` `std::process::exit(1)` on idle expiry: the bridge dies
  with no line saying why. The one place a log would explain a mysterious
  disconnect.
- `SshAgentRegistry::publish` swapping the published agent symlink to
  `.unavailable`: no log. The user sees agent forwarding stop working silently.

Enforceable: no. These are judgement calls; only review catches them.

---

## 4. Errors

### 4.1 The logging writer swallows every write error by contract - FACT (deliberate, but one branch is wrong)

`RotatingFileGuard::write` returns `Ok(buf.len())` on a poisoned mutex and on
any write failure, and `flush` returns `Ok(())` on a poisoned mutex. The
recovery design (remember the first error, report it into the log when writing
resumes) is sound and tested. The poisoned-mutex branch is not covered by it:
it returns success and records nothing in `lost_error`, so a panic inside the
rotation path silently turns the process into one that logs nothing, forever,
with no "lines were lost" note when it recovers - because it never recovers.

Enforceable: yes - use `PoisonError::into_inner` (as `shepr-test-support`
already does for its own mutexes) so the state is recovered rather than
abandoned, and the existing `writer_recovers_after_an_io_error_and_notes_the_gap`
test shape extends to it.

### 4.2 Errors that reach an operator naming no subject - FACT

- `ssh_paths.rs:75` `"SSH bridge socket path exceeds the Unix socket length
  limit"` - no path, no length, no limit. The operator's only fix is to shorten
  `XDG_RUNTIME_DIR`, which the message never mentions.
- `ssh_paths.rs:160` same shape for the control socket.
- `ssh_paths.rs:196` `UnsafeSshRuntimeDirectory`'s `Display` names the three
  requirements but not the directory that failed them, and
  `validate_shared_ssh_dir` has the path in hand.
- `ipc.rs:93` `"lock must be a regular file owned by this user"` - no path, no
  uid.
- `ssh_agent.rs:107` `"SSH agent must be an absolute, user-owned socket"` - no
  path, and it covers three distinct rejections (relative, self-referential,
  not a user-owned socket) with one string.

Enforceable: partly - a typed error per module carrying the subject makes the
subject impossible to omit; a lint cannot.

### 4.3 A library crate terminates the process on a timer - FACT

`shepr-platform/src/remote_bridge.rs:56`: a watchdog thread calls
`std::process::exit(1)` when the relay has been idle for 60 seconds. The
comment justifies why returning is insufficient, and for the dedicated bridge
process that reasoning holds - but the decision now lives in a crate that seven
other crates link, and nothing prevents a second caller of
`forward_remote_bridge_stdio(_, true)` from inheriting a hard exit it did not
ask for. The refusal that was owed is a signal back to the caller plus the
caller's own `exit`.

Enforceable: yes - return a `BridgeOutcome::IdleExpired` and let
`shepr-remote`/`src/main.rs` exit. A `clippy.toml disallowed_methods` entry for
`std::process::exit` outside `src/main.rs` then holds it.

### 4.4 `Activity::record` propagates a clock failure into the data path - PREDICTION

`remote_bridge.rs::TrackedIo::{read,write}` call `self.progressed(count)?`,
which calls `now()?`, which fails if `clock_gettime(CLOCK_BOOTTIME)` fails. A
clock error is then reported to the caller as an **IO error on the relayed
stream**, after the bytes have already been read or written - so the byte count
is lost and the stream desynchronises. A failed clock read should leave the
watchdog stale (the watchdog already treats a `now()` failure as expiry), not
corrupt the relay.

Enforceable: yes - make `record` infallible and the test is straightforward.

### 4.5 Failures discarded where the caller could act - FACT, list

`ipc.rs:334-335` (`remove_file`/`remove_dir` of the staging directory: a leaked
0700 directory per failure, never logged - see 7.5);
`ipc.rs:296` (`remove_file` after a failed restrict);
`ssh_agent.rs:160,174` (`remove_file` of the temporary symlink and of the
published path on drop);
`logging.rs:562` (`set_permissions` tightening a world-readable log - the one
place where failing quietly means the log stays readable by others);
`clipboard.rs::kill_and_reap` (both calls, by design);
`terminal_setup`/`title.rs` `let _ = stdout.write_all(...)`.

Enforceable: partly - `clippy::let_underscore_must_use` would flag all of them
and force an explicit `if let Err(e) = ... { tracing::debug!(...) }` decision at
each site. It is not currently in the workspace lint table, and adding it is a
finding somebody could pay for once.

---

## 5. Tests that prove nothing

### 5.1 A test whose assertion compares a value to itself on every normal run - FACT

`shepr-platform/src/tests.rs:355`
`config_metadata_preserves_ownership_and_acl_without_inheriting_extra_access`:

```rust
if effective_uid() == 0 {
    assert_eq!(unsafe { libc::fchown(input.as_raw_fd(), 1001, 1002) }, 0);
}
...
assert_eq!(
    (actual.uid(), actual.gid(), actual.mode()),
    (original.uid(), original.gid(), original.mode())
);
```

Unless the suite runs as root, the `fchown` never happens, so source and
destination were both created by the same uid/gid, and the ownership half of
the assertion cannot fail no matter what `write_config_temporary` does to
ownership. `brokkr check` does not run as root. The ownership-preservation
logic - `fchown` plus the `EPERM` tolerance in `config_file.rs` - is therefore
untested, and the test's name advertises it. (The ACL half of the test is real.)

Enforceable: yes - split the ownership case into a test that is `#[ignore]`d
with a stated reason when not root, so the suite stops reporting it as covered,
or drive `write_config_temporary` with injected metadata instead of real
`fchown`.

### 5.2 A `#[test]` that returns immediately and passes - FACT

`shepr-platform/src/remote_bridge_tests.rs:10-21` `bridge_child`: if
`SHEPR_BRIDGE_TEST_SOCKET` is unset it `return`s. It is a subprocess entry
point re-entered through `current_exe --exact remote_bridge_tests::bridge_child`,
not a test, and in every normal run it appears in the pass list having executed
four lines.

Enforceable: partly - the pattern is necessary (there is no other way to get a
child process running crate-private code), but it should be named so nobody
reads it as coverage, and the guard should `panic!` when the variable is absent
only if the harness cannot be told to skip it. Worth noting the same file
hardcodes libtest CLI flags (`--exact`, `--nocapture`), which couples the
crate's tests to the harness.

### 5.3 The only behavioural test of the logind protocol never runs - FACT

`shutdown.rs:394` `delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation`
is `#[ignore = "requires dbus-daemon; ..."]`. It is the sole test of
`watch_connection` - inhibitor acquisition, the hold-until-checkpoint
invariant, retake-after-cancellation, and the `already_preparing` reconnect
branch. Everything that `brokkr check` actually exercises in that module is the
three pure `Shared` tests. The mechanism AGENTS.md calls out as the reason
`zbus` is a dependency at all ("logind's delay inhibitor lets the server save
before host shutdown kills panes") is unverified by the gate.

Enforceable: yes, and worth doing: `zbus` can serve the `LoginManager`
interface over a `UnixStream` pair or a `p2p` connection with no
`dbus-daemon` at all, which removes the external-binary dependency and lets the
test run in the gate.

### 5.4 Tests that depend on programs that happen to be installed - FACT, list

Everything below is in `shepr-platform` and assumes a specific host layout
rather than anything this repository builds:

- `/bin/sleep` (`tests.rs:441,468`), `/bin/sh` (`tests.rs:485`,
  `process.rs:reap_tests:...`), `/bin/cat` (`tests.rs:714,719`).
- `sh`, `printf`, `yes`, `head` resolved from `PATH`
  (`tests.rs:747,769,816,829,843`). `printf` as an **executable** (`:816`)
  requires coreutils on `PATH`; on a host where it is only a shell builtin the
  test fails.
- `#!/bin/sh` shebangs in the generated fake clipboard helpers
  (`tests.rs:646,714,719`).
- `dbus-daemon` (see 5.3).
- `shutdown.rs` and `remote_bridge_tests.rs` re-exec `current_exe`, which is
  fine, but `remote_bridge_tests` inherits stderr into the parent's output.

Enforceable: yes for most of it - a tiny test helper binary built by the
workspace (a `[[bin]]` in `shepr-test-support` that can sleep, echo, exit with a
code, and hold a pipe open) replaces every one of these and makes the tests
depend only on what the repo builds. That is a structural fix worth the effort,
since the same shapes recur in `shepr-pty` and `shepr-agent` (`python3` is
skipped-if-absent there, which is the same finding with a nicer failure mode).

### 5.5 Tests that assert on the wall clock - FACT, list

`ipc.rs:691` `deadline_reader_cuts_off_a_trickling_peer` (300 ms deadline,
50 ms trickle, 2 s bound, real thread sleeps);
`tests.rs:767,840` (200 ms deadlines, 5 s bounds);
`tests.rs:616` wl-copy owner test (2 s polls on a marker file);
`remote_bridge_tests.rs:129,144,180` (300 ms `TIMEOUT`, 60 ms sleeps, 3 s
waits, and `legacy_bridge_has_no_idle_deadline` proves a negative by sleeping
`TIMEOUT * 2`);
`shutdown.rs` (5 s timeouts, 5 ms polls).

None of these can be made deterministic while the timeouts are `Instant`-based
constants with no injection point. `SshAgentLease::refresh_at(now)` shows the
pattern that works; `Activity`, `DeadlineReader`, `wait_child_until` and
`wait_for_process_exits` all take deadlines already and could take a clock.

Enforceable: yes - a `Clock` trait (or just `fn now() -> Instant` passed in) in
`shepr-core`, plus a `clippy.toml disallowed_methods` entry for
`std::time::Instant::now` outside designated modules. This is the root cause of
most of section 5.

### 5.6 Assertions that cannot fail - FACT, small

- `logging.rs:669` `assert!(!summary.contains("/shepr.log"))` - the asserted
  string is built by a `format!` that cannot produce it.
- `geometry.rs:120-126` `assert!(size_of::<PaneGeometry>() <= size_of::<(u16,
  u16, u32, u32)>())` - asserts a property of the compiler's layout choices,
  not of this code; it passes for any plausible field arrangement.
- `tests.rs:538` `session_members_are_withheld_...` calls
  `member.signal(Signal::Kill)` and drops the `bool` in cleanup, so a failure
  to clean up the background `sleep 30` is invisible and the process leaks past
  the test.
- `ssh_agent.rs:369-370` asserts both `!stable.exists()` and
  `symlink_metadata(&stable).is_ok()` - correct and deliberate (a dangling
  symlink), but reads as a contradiction without the comment it does not have.

Enforceable: partly - review only.

### 5.7 Scratch directories live under `/tmp`, against a project rule - FACT

`shepr-test-support/src/lib.rs:80`: `std::env::temp_dir().join(format!(
"shepr-test-{pid}"))`. AGENTS.md and CLAUDE.md both state "Never read or write
from `/tmp`. All data lives in the project." The doc comment gives the reason
(socket paths must fit `sun_path`, and a deep checkout eats the budget), which
is a real constraint - `target/` under this checkout is already ~30 bytes deep
before any scratch name.

This is a rule the build cannot enforce because the code deliberately breaks it,
and neither document records the exemption. Either AGENTS.md should name the
exemption, or the scratch root should move under a short symlink inside the
project. Related: `ScratchDir::keep_until_exit` relies on `atexit`, so a test
process killed with SIGKILL leaves the directory behind indefinitely, and the
only cleanup for a stale one is a later run reusing the same pid (`:83`) - an
unbounded `/tmp` growth path on a machine where tests get killed.

Enforceable: partly - a gremlin-style text rule forbidding `temp_dir` outside
`shepr-test-support` plus a documented exemption; the leak needs a
`shepr-test-*` sweep at scratch-root creation rather than an exact-pid match.

---

## 6. Guards and claims that have stopped holding

### 6.1 Checks keyed on names that become silent no-ops

- **`WSL_MARKER_ENV_VARS`** (`host.rs:...`): a two-entry list. When Microsoft
  renames or drops a variable the check quietly stops contributing, and there
  is no log and no test that the list is non-empty or current. Checkable only
  against a real WSL host; not false today as far as can be determined here.
- **`clipboard_program_name(command.program) == "wl-copy"`**
  (`clipboard.rs:...`): the entire "detach the clipboard owner instead of
  waiting for it" behaviour hangs on that string. Rename the helper, wrap it,
  or point at `wl-copy-wrapper` (a case the test at `tests.rs:610` explicitly
  demonstrates produces `"wl-copy-wrapper"`), and the write path silently
  reverts to waiting for a process that never exits until the 2 s timeout kills
  it - taking the user's clipboard content with it. **Checkable and worth
  fixing structurally**: the "owns the selection after exit" property belongs on
  `ClipboardCommand` as a field, not inferred from the program name.
- **`is_posix_acl_xattr`** (`config_file.rs`): keyed on the prefix
  `system.posix_acl_`. Correct today; a filesystem that expresses access
  control under another prefix (a security label, richacl) falls into the
  best-effort branch that ignores failures. The comment says as much, which is
  the honest version.
- **`XDG_BASE_DIR_VARS`** (`shepr-test-support`): four names. See 6.2.
- **`"SHEPR_"` prefix scrub** (`shepr-test-support:224`): filters
  `key.to_str().is_some_and(|k| k.starts_with("SHEPR_"))`, so a non-UTF-8 key
  is silently kept. Not reachable in practice, but it is the fail-open shape.

### 6.2 `IsolatedEnv` guarantees isolation from a list it does not own - FACT

The doc comment claims the guard means "nothing under test can reach the user's
real config, state or agent directories, or the live shepr server a test run
was started from". That guarantee rests on three name lists inside
`shepr-test-support`: `XDG_BASE_DIR_VARS` (four names), the `SHEPR_` prefix, and
`HOME`/`XDG_RUNTIME_DIR`. `shepr-config/src/io.rs` reads `XDG_CONFIG_HOME` and
`XDG_STATE_HOME`; `shepr-agent/src/integration/env.rs` reads a per-agent set of
`*_HOME`-style variables (`GROK_HOME` among them, per its own tests). The day a
variable is added on the reading side and not here, every test keeps passing
while reaching into the developer's real `$HOME`-adjacent state, and reports
nothing.

Enforceable: yes, and this is the highest-value mechanical fix in the report.
If every environment variable name lives in one `shepr-core::env` module
(finding 1.3), `IsolatedEnv` can iterate that module's full list instead of
restating a subset, and a `brokkr.toml` text rule forbidding env-name literals
elsewhere keeps the list complete by construction.

### 6.3 Claims nothing enforces

- **`shepr-platform/src/lib.rs:3-4`**: "domain rules live with their consumers
  in `detect`, `remote`, and the app". There is no `detect` or `remote` module
  in this crate or this workspace at that path - they are
  `crates/shepr-agent/src/detect` and `crates/shepr-remote`. Stale, and false
  in a second way: 25 domain event functions live in `logging.rs` (3.3). **False
  today.**
- **`remote_bridge.rs:12-14`**: "The client endpoint sends HealthPing after five
  seconds without received data, and the server answers HealthPong. Those
  protocol frames renew this byte-level watchdog." A 60-second timeout in
  `shepr-platform` whose safety depends on a five-second interval defined in
  `shepr-client`/`shepr-protocol`. Nothing relates the two numbers; halve the
  timeout or double the ping interval and healthy idle bridges start dying.
  **Checkable**: a test that asserts `IDLE_TIMEOUT > HEALTH_PING_INTERVAL * k`
  needs the two constants in one place, which the layering permits (put the
  interval in `shepr-core` or `shepr-protocol` and have both read it).
- **`process.rs:...`** `ProcessIdentity::StartTime` documents a pid-reuse race
  window ("a window of a few syscalls") that no test reaches and no assertion
  guards. See 8.1 - the right answer is deleting the branch, not testing it.
- **`ipc.rs:41-45`**: "Acquire this before `prepare_socket_path` and keep it
  until the listener has stopped." `SocketStartupLock` is a separate value from
  the listener; nothing in the type system requires the ordering or the
  lifetime. **Checkable structurally**: make `prepare_socket_path` take
  `&SocketStartupLock`, which makes the wrong order unrepresentable.
- **AGENTS.md's "Document folders"** section describes `reference/` and `docs/`
  as binding in-repo folders. Neither exists in the tree; only `notes/` does.
  **False today**, and it matters here because several of the durable claims
  above (the tunable inventory, the env-var registry, the logging level policy)
  have nowhere to live that the document says they must.
- **AGENTS.md's crate list** restates each crate's responsibility in prose
  ("`shepr-platform`: Linux process, filesystem, IPC and terminal plumbing").
  `brokkr.toml`'s `[[dependency_rule]]` entries already encode the layering
  mechanically; the prose adds the responsibilities, which nothing checks - and
  `logging.rs`'s domain catalogue is already outside its stated responsibility.

---

## 7. Policy invented per call site

### 7.1 Three unrelated backoff/retry policies inside one crate - FACT

`shutdown.rs:124-154`: 1 s initial, double, cap 60 s, reset to 1 s while a
shutdown is pending. `ssh_paths.rs:34`: 16 immediate retries, no delay.
`ipc.rs:255`: 4 immediate retries. `clipboard.rs::wait_child_until`: fixed 5 ms
poll to a deadline. `process.rs::wait_for_process_exits`: poll with a 10 ms
recheck floor and a 10 ms sleep on poll failure. Five policies, five shapes, no
shared vocabulary for "keep trying until".

Enforceable: partly - one `retry` helper in `shepr-core` taking a policy value
would let a test assert the policy, and the call sites become data.

### 7.2 Ambient dependencies reached from logic - FACT

- **Clock**: `Instant::now()` in `ipc.rs` (×2), `clipboard.rs` (×4),
  `client_stream.rs`, `process.rs`, `ssh_agent.rs` (×2, one injectable),
  `remote_bridge.rs` (`clock_gettime` directly). This is why section 5.5 exists.
- **Randomness**: `ssh_paths.rs::unpredictable_token` is the single owner
  (good), and `ipc.rs:320` reaches across module boundaries into
  `super::ssh_paths::unpredictable_token` for a socket staging name - a
  cross-cutting utility living in the SSH module because that is where it was
  first needed. It belongs in its own module.
- **Identifier generation**: `PaneId::alloc` (2.6).
- **Environment**: `std::env::var_os` in `pathutil.rs`, `host.rs`,
  `terminal_environment.rs`, `clipboard.rs::ClipboardSession::from_env`. Three
  of the four have a pure inner function taking the values, which is the right
  pattern; `host.rs::detect_running_inside_wsl` does not.
- **Process id**: `std::process::id()` in `logging.rs` (×2), `ssh_paths.rs`,
  `ssh_agent.rs:157`, `remote_bridge_tests.rs`, `shepr-test-support` (×4).
- **Working directory**: `ipc.rs:315` falls back to `Path::new(".")` for the
  staging parent when the socket path has no parent - a silent dependency on
  cwd for a security-relevant 0700 directory.

Enforceable: yes for the clock, the environment and `process::id` - a
`clippy.toml disallowed_methods` list plus designated wrapper modules.

### 7.3 A mutex held across blocking filesystem and socket work - FACT

`ssh_agent.rs::SshAgentRegistry::register` and `SshAgentLease::refresh_at` hold
`Arc<Mutex<State>>` across `State::publish`, which does
`symlink_metadata`, up to N `connect_sync()` calls through `live_socket`,
`symlink`, `rename` and another `symlink_metadata`. Each `connect_sync` uses
`ConnectWaitMode::Timeout(Duration::ZERO)`, so the window is short by
construction - but the structure, not the timeout, is what keeps it short, and
nothing records that. Every attachment's refresh serialises behind it.

`logging.rs::RotatingFileState` is the same shape and worse: the mutex is held
across `flock(LOCK_EX)` - a **blocking** syscall that waits for another
*process* - plus `rename`, `remove_file` and `write`. Every thread in the
process that emits a log line blocks behind a cross-process lock. The workspace
already denies `await_holding_lock`; this is the sync analogue and no lint
covers it.

Enforceable: partly - `clippy::await_holding_lock` is already on, which covers
the async case. The sync case needs the I/O moved out from under the guard
(take the file handle, drop the guard, then write).

### 7.4 Shared mutable state whose safety rests on call order

- `logging.rs::init_file_logging` installs a process-global subscriber and
  discards a second call's error (2.3).
- `host.rs::watch_terminal_resize_signal` installs a process-global SIGWINCH
  handler and `TERMINAL_RESIZE_SIGNALLED` is a process-global atomic. The test
  `terminal_resize_signal_is_recorded_once_per_delivery` installs the handler
  and `raise`s SIGWINCH while every other test in the binary runs concurrently;
  it survives only because nothing else in the suite touches SIGWINCH. The
  handler also persists for the rest of the process.
- `shepr-test-support`'s `SCRATCH_ROOT_OWNER`/`SCRATCH_ROOT`/
  `KEPT_SCRATCH_DIRS`/`NEXT_SCRATCH` are four separate statics whose
  consistency rests on `ensure_exit_cleanup` being called before any of them
  are read - which `ScratchDir::new` does via `scratch_root()` and
  `keep_until_exit` does explicitly. Two writers (the `atexit` handler and
  `Drop`) can both remove the same path; harmless because both ignore errors,
  which is exactly the "invariant maintained by two writers who have never been
  introduced" shape.
- `PaneId::NEXT_PANE_ID` (2.6).

Enforceable: partly. Bundling the test-support statics into one
`OnceLock<ScratchState>` makes the ordering structural.

### 7.5 Resources that can grow without bound when something upstream misbehaves

- `ipc.rs::bind_via_private_staging` leaks a 0700 staging directory per failed
  `remove_dir` (`let _ =`, no log). One per bind attempt, in the XDG runtime
  directory.
- `ssh_paths.rs::create_remote_ssh_config_dir` creates
  `shepr-ssh-<pid>-<token>` directories and never removes them; the doc says
  "ephemeral" and the caller is responsible. Nothing sweeps stale ones from a
  killed process.
- `ssh_agent.rs::publish` leaves `<path>.<pid>.new` behind if `rename`
  succeeds... it does not (rename consumes it), but if `symlink` succeeds and
  the process dies before `rename`, the temporary stays.
- `shepr-test-support` kept-scratch directories after SIGKILL (5.7).
- `read_limited_reader` is correctly bounded - the good counter-example.

Enforceable: partly - a startup sweep keyed on "own uid, no live pid" is
testable; the leaks themselves are caught by making the `let _ =` sites
explicit (4.5).

### 7.6 Secrets and personal data in diagnostics - mostly right, one asymmetry

`remote_bridge.rs` and `remote_bridge_io.rs` both carry an explicit module-level
rule ("input content must stay out of logs and error messages here; byte counts
and error kinds only") and honour it. No equivalent note exists on the clipboard
path, which handles the same class of content (the user's selection, potentially
a token pasted between panes) and spawns it through an argv-visible helper
process. Nothing leaks today: no clipboard log line exists at all (3.2), which
means the first person to add one is the person who will leak it.

Enforceable: no. A module-level comment on `clipboard.rs` matching the bridge's
is the whole fix.

### 7.7 A test-only shortcut production can reach - FACT

`shepr-platform/src/process.rs:408` `signal_processes` - documented as
"Test-only: production code signals through `ProcessHandle`, which cannot hit a
reused pid" - is gated `#[cfg(any(test, feature = "test-support"))]`. That
feature is enabled by `crates/shepr-server/Cargo.toml:47` as a
**dev-dependency** feature. Cargo unifies features across a single build graph,
so a `cargo test --workspace` (which `brokkr check` runs, and which builds the
root `shepr` binary too) compiles `shepr-platform` **once**, with
`test-support` on, for every consumer in that invocation - including the
production binary. Its single user is one line in
`shepr-server/src/app/snapshot_tests.rs:506`.

Enforceable: yes - move the helper into the test that needs it (it is nine
lines of `libc::kill`), and delete the `test-support` feature from
`shepr-platform` entirely. Then the shortcut is unrepresentable rather than
merely gated.

---

## 8. Code that is no longer load-bearing

### 8.1 The `ProcessIdentity::StartTime` fallback - a compatibility path for a kernel nobody runs

What tells me: `ProcessHandle::open` uses it only when `pidfd_open` fails with
something other than `ESRCH`/`EINVAL` - i.e. `ENOSYS` (kernel older than 5.3,
September 2019) or fd exhaustion. `ProcessHandle::open_by_start_time` is
`pub(super)` and has **no non-test caller**; the two tests that exercise the
branch assert `pidfd.pidfd().is_some(), "this kernel has pidfds"` in the same
breath, and `both_handle_kinds`/`openers` exist only to double every process
test. shepr is Linux-only, `rust-version = "1.99"`, and has never been
deployed.

What it costs: roughly a third of `process.rs` (a second identity enum arm,
`state_and_start_time_from_stat`, `state_by_start_time`, a `/proc` read before
every signal, a `has_exited` implementation with different semantics, and the
`START_TIME_EXIT_RECHECK` tunable and its poll-floor logic in
`wait_for_process_exits`), plus a documented pid-reuse race the pidfd path does
not have, plus 2× the test matrix in two tests.

Recommendation: delete it. `ProcessHandle::open` returns `None` (or an error
naming fd exhaustion) when `pidfd_open` fails. The fd-exhaustion case is real
but is a "refuse and say so" case, not a "silently degrade to a racy identity"
case. Cost of being wrong: shepr stops managing pane process groups on
pre-5.3 kernels - which the owner does not run.

Enforceable after deletion: yes - the `#[cfg]`-free code has one identity, and
`wait_for_process_exits` loses its "handles without a pidfd" branch entirely.

### 8.2 `--idle-timeout-v1` is a flag with one value - FACT

Every production spawner passes it: `shepr-remote/src/remote/attach.rs:798` and
`::launch.rs:149`. `src/cli/spec.rs:81` defines it, `src/cli.rs:280` parses it,
`src/main.rs:172` forwards it. The `false` path is reachable only by invoking
the hidden `remote-client-bridge` subcommand by hand, and its only exerciser is
`legacy_bridge_has_no_idle_deadline` plus the `SHEPR_BRIDGE_TEST_LEGACY`
variable that exists solely to reach it. The `v1` in the name is a version
negotiation for a deployment that does not exist - AGENTS.md: "no wire
compatibility obligations", "Client and server are always the same build".

(To be precise: the `idle_timeout: bool` **parameter** of
`forward_remote_bridge_stdio` is genuinely two-valued -
`shepr-remote/src/lib.rs:258`'s API bridge passes `false` legitimately. It is
the CLI flag, the `legacy` test path and the `SHEPR_BRIDGE_TEST_LEGACY`
variable that are dead.)

Recommendation: delete the flag, the parse, the plumbing, the test variable and
`legacy_bridge_has_no_idle_deadline`; the client bridge always gets the
watchdog.

Enforceable after deletion: yes - the flag cannot be passed if `spec.rs` does
not define it.

### 8.3 The world-readable-log tightening path is for a build nobody ran - FACT

`logging.rs:557-563` re-chmods an existing log "left behind by an older build
that created logs world-readable", with a dedicated assertion in
`log_files_are_private_to_the_user`. AGENTS.md: "shepr has never been run: no
config, catalog, session or other on-disk state exists anywhere". There is no
older build and no world-readable log anywhere. The `mode(0o600)` on the
`OpenOptions` covers every file this code creates.

Recommendation: delete the tightening branch and the legacy half of the test.
Cost of being wrong: nil - `mode` still applies to created files, and the owner
can `chmod` once if a stray file ever appears.

### 8.4 `impl From<bool> for SplitBranch` has no callers - FACT

`shepr-core/src/geometry.rs:12-18`. Grepped the whole workspace for
`SplitBranch::from`, `.into()` producing a `SplitBranch`, and any `bool`->
`SplitBranch` coercion: zero sites, including inside `shepr-core` itself.
Trait impls are invisible to `dead_code`, so nothing in the build notices.
It also encodes a claim (`true` means `Second`) that nothing depends on -
`shepr-client/src/shell/presentation/topology.rs:37` and
`::input/mouse.rs:862,1031` all write the comparison out by hand, in the
opposite direction.

Recommendation: delete.

### 8.5 `help_log_paths_summary` is a one-line alias - FACT

`logging.rs:48-50`: `pub fn help_log_paths_summary(dir) -> String {
log_paths_summary(dir) }`. One external caller (`src/cli.rs:401`). The private
`log_paths_summary` exists only so the test can call it under a different name.
Two names, one body, one caller.

Recommendation: make `log_paths_summary` public under one name.

### 8.6 `shepr-remote/src/remote/bridge.rs:688` re-wraps `fits_unix_socket_path` - FACT

```rust
pub(super) fn fits_unix_socket_path(path: &Path) -> bool {
    shepr_platform::fits_unix_socket_path(path)
}
```

A private shim over a public function from a crate `shepr-remote` already
depends on directly, used by `attach.rs` at three sites. It makes the socket
limit look like it has two owners.

Recommendation: delete; call through `shepr_platform::`.

### 8.7 `shepr-platform`'s `test-support` feature - FACT

Gates exactly one nine-line function with one caller (7.7). Delete the feature
and the function; remove the `features = ["test-support"]` from
`shepr-server/Cargo.toml:47`.

---

## Lateral findings (outside the eight questions, flagged as asked)

1. **Live defect, HOME expansion in git config** (1.2). With `HOME` unset or
   empty, `shepr-mux/src/git/config.rs` turns `~/x` into the relative path `x`
   and resolves it against the server's cwd. The fix is one call to
   `shepr_core::pathutil::expand_tilde_path`. This is the exact failure
   `pathutil`'s doc comment says it exists to prevent.
2. **Live defect, clipboard fallback keyed on a program name** (6.1). Pointing
   the wl-copy slot at a wrapper (or a renamed binary) silently reverts the
   write path from "detach the selection owner" to "wait 2 s then kill it",
   which loses the copy. The project's own test demonstrates the name it would
   see.
3. **Latent desync, `Activity::record`** (4.4). A `clock_gettime` failure is
   returned as an IO error from `TrackedIo::read`/`write` *after* the transfer,
   discarding the byte count on the SSH relay.
4. **`hostname()` buffer**: 256 bytes against `HOST_NAME_MAX` of 64. Harmless,
   but `gethostname` is not required to NUL-terminate on truncation; the code
   handles that (`position(|b| b == 0).unwrap_or(len)`), so this is fine - noted
   only because the 256 is another unnamed number.
5. **`unpredictable_token`'s fallback**: on `getrandom` failure it hashes only
   `std::process::id()` with a `RandomState` hasher. The randomness comes from
   `RandomState`'s OS-seeded keys, so the result is unpredictable - but the
   doc comment says "std's OS-seeded hasher keys", which is subtle enough that
   a future simplification ("why hash the pid at all?") could quietly turn it
   into a predictable value used for 0700 staging directory names and socket
   paths. Worth a sharper comment or a `getrandom`-or-fail policy.
6. **`shepr-core` depends on `ratatui`**: `layout.rs` uses
   `ratatui::layout::{Direction, Rect}` and `brokkr.toml` allows it. That puts a
   TUI rendering crate at the bottom of the layering, where `shepr-mux`,
   `shepr-protocol` and `shepr-config` all inherit it. `Rect` and `Direction`
   are four `u16`s and a two-variant enum. Owning them in `shepr-core`
   (alongside `GridSize`, which is already there) would drop `ratatui` from the
   bottom four crates' dependency closure and remove a re-export the wire types
   currently share with the renderer.
7. **`geometry.rs` and `layout.rs` disagree on where split geometry lives**:
   `SplitBranch` is in `geometry.rs`, `SplitRatio`/`SplitBorder`/`Node` in
   `layout.rs`, and `shepr-protocol/src/geometry.rs` has a third set of
   conversions. Not a duplication, but the reason `SplitBranch`'s dead `From`
   impl (8.4) went unnoticed.
8. **`prepare_socket_path` takes a `busy_message` closure** so the caller
   supplies the operator text for an `AddrInUse` - the one place in this scope
   where message ownership is inverted (the caller decides, the owner formats).
   It reads as the right idea implemented at the wrong seam: the platform layer
   should not be formatting operator text at all (3.1), so the closure is a
   symptom of the missing operator-message channel rather than a fix for it.

---

## What the build could enforce, collected

Existing enforcement (`brokkr.toml` gremlins + manifest + per-crate dependency
allowlists, `clippy.toml`, the workspace lint table) already covers layering,
dependency creep and a long list of idioms. The rules below are the ones this
hunt found worth adding, roughly in order of how much of the report each closes:

1. **One env-name module in `shepr-core` + a text rule forbidding `SHEPR_`,
   `"HOME"` and `XDG_` literals elsewhere.** Closes 1.2, 1.3, 6.2, most of 2.4.
2. **A clock passed in, plus `clippy.toml disallowed_methods` for
   `Instant::now`/`SystemTime::now` outside designated modules.** Closes most of
   5.5 and part of 7.2.
3. **A workspace-built test helper binary in `shepr-test-support`.** Closes 5.4
   across three crates.
4. **`clippy::let_underscore_must_use` in the workspace lint table.** Forces a
   decision at every site in 4.5.
5. **`clippy.toml disallowed_methods` for `std::process::exit` outside
   `src/main.rs`, and for `eprintln!`/`io::stderr` outside one operator-message
   module.** Closes 1.4, 4.3, 3.1.
6. **A `tunables` module per crate + a text rule on `Duration::from_*` and
   octal mode literals.** Closes 2.1, part of 1.8.
7. **Signature changes that make bad spellings unrepresentable**:
   `prepare_socket_path(&SocketStartupLock, ...)` (6.3),
   `PaneId::from_raw -> Option` (2.6), `RemoteSshConfigPaths::system_config:
   PathBuf` (1.11), `init_file_logging -> io::Result<()>` (2.3),
   `write_clipboard -> Result` (3.2).
8. **A `zbus` p2p-connection rewrite of the logind test** so it runs in the
   gate (5.3).
9. **Deletions** (section 8): they need no rule, and each one removes a site
   from every finding above.
