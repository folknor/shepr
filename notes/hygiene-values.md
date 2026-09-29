# Hygiene findings: values and their owners

This file collects the findings from the nine-scope hygiene hunt that answer the
hunt's first two questions: (1) values spelled at more than one site instead of
being defined once and read - environment variables and their resolution rules,
tunable constants and thresholds, ports, endpoints and addresses, filesystem
paths and roots, timeouts and limits, exit codes and status strings; and (2)
values nobody can find, change or trust - a knob defined once but where nobody
tuning the system would look, a value with no injection point, configuration read
at the moment of use rather than validated once at startup. Entries gather every
site and every hunter that reported the same value. It is a working document
assembled from nine independent readings, none of them verified by running the
build, so individual entries may be wrong; a later fix pass is expected to find
phantoms here.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGV-007 - Three different rules for resolving `$HOME`

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes `HOME` a registry entry and bans the raw
`std::env::var_os` reads in `clippy.toml`, so the two ad-hoc `git/config.rs`
expansions and `shepr-pty`'s `home_dir` must read through the one reader, which
supplies one empty and non-UTF-8 rule. Open: which caller-side rule (the
`pathutil.rs` error or `home_dir`'s passwd-then-`/` fallback) governs the pane
cwd; the relative-path half of the policy is not the reader's.

Reported by the core/platform hunter.

`crates/shepr-core/src/pathutil.rs` is the declared owner and its doc comment
states the policy: unset, empty or relative `HOME` is an error, never a fallback,
because every caller builds a path under it and an invalid home would make that
path relative to the current directory. Two other rules exist:

- `crates/shepr-mux/src/git/config.rs::normalize_gitdir_include_pattern` and
  `::resolve_include_path` both do
  `std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest)`
  for a `~/` prefix. `expand_tilde_path` is already public and returns the right
  error. (The hunter also filed the behaviour as a live defect; that half is in
  `notes/bugs.md`. The duplication stays here.)
- `crates/shepr-pty/src/command.rs::home_dir` requires `HOME` to be absolute
  and an existing directory, then falls back to the passwd entry, then to `/`:
  a third policy with two silent fallbacks, used as the pane cwd fallback.

Enforcement: delete the ad-hoc expansions, then a `brokkr.toml` text rule
forbidding `"HOME"` as a literal outside `shepr-core/src/pathutil.rs` and
`shepr-test-support`. A `clippy.toml disallowed_methods` entry is not enough,
since the offence is the argument rather than the method.

## HYGV-013 - `SHEPR_DEBUG_OSC_EVIDENCE` is resolved at pane creation, not at launch, and is documented nowhere

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes the variable a registry entry of flag kind, so a
value outside `1`/`0`/`true`/`false` is refused naming the variable rather than
read by `osc.rs`'s own `"1" | "true" | "yes" | "on"` list, and the registry is
where it is named. Open: the read still happens per pane; resolving it once at
launch and carrying it down is not part of the decision.

Reported by the mux hunter.

`crates/shepr-mux/src/pane/osc.rs`: `impl Default for OscDebugTracker` calls
`Self::from_env()`, and `OscDebugTracker::default()` is reached from pane core
construction, so the process environment is read once per pane rather than once at
startup. A typo (`SHEPR_DEBUG_OSC_EVIDENCEE=1`, or `=yes please`) is not a launch
failure but a silent no-op discovered hours later. The flag appears in no doc and
no `brokkr man` page, and it is the flag that puts pane content (OSC payloads)
into the log.

Fix: resolve it in the same pass as the rest of the config and carry it in the
validated config down to pane construction.

## HYGV-027 - The client state subdirectory is spelled at three sites, two ways

Reported by the remote hunter.

`crates/shepr-remote/src/machine/catalog.rs::catalog_path` builds
`state_dir/client/endpoints.json`; `::selection_path` builds
`state_dir/client/endpoint-selection.json`; `ssh_metadata.rs::new` builds
`state_dir.join("client/ssh-metadata")`, a different join style for the same
directory. Nothing owns "the client's state directory".

Fix: a `shepr_config::AppPaths::client_state_dir()` accessor, like the existing
`server_address()` / `session_id()`. Enforcement: a text rule forbidding the
literal `"client"` as a path component outside that accessor, or the accessor
plus review.

## HYGV-035 - `/bin/sh` and the shell-resolution rules are spelled across several sites

Reported by the vt/pty and server hunters.

- `crates/shepr-pty/src/command.rs` spells `/bin/sh` five times across two
  fallback chains: `passwd_shell` falls back to `/bin/sh`, then `resolve_shell`
  falls back to `/bin/sh` again.
- The shell-env trim rule is applied twice in the same crate: `interactive_shell`
  trims `default_shell`, then `trimmed_shell` trims `$SHELL` again.
- `crates/shepr-server/src/app/tab_bar_status.rs` runs every tab-bar status
  command as `Command::new("/bin/sh")` with `args(["-lc", &command])`. That is
  not configurable, not derived from `terminal.default_shell`, not from `$SHELL`,
  and `-l` is a behavioural choice: each status command pays a full login-shell
  startup every interval and picks up the user's rc files. The choice is
  invisible from config and from the config documentation. The server hunter
  files this under question 2 - a knob nobody tuning the system would find.

Fix: one const for the fallback shell; for the tab bar, a named constant or a
config key, which moves it into HYGV-036.

## HYGV-036 - Nothing answers "what are this crate's tunables", in any crate

**Decision (partial):** per-crate `limits` modules are adopted from broadarrow,
incrementally as part of the hygiene work rather than wholesale: each crate's
constants move into its `limits` module (the `shepr-protocol` model below), and
the crate is then held by scoped textlints in the shape of broadarrow's
`numeric-consts-live-in-limits` and `duration-and-capacity-literals-live-in-limits`
(B8 in `notes/broadarrow-ports.md`). That settles the enforcement the hunters
converge on, with the name `limits`. Open: every crate, one at a time, and which
knobs should become config keys.

Every hunter reported this independently, for their own scope. The shape is
always the same: each constant is individually defined once, which is why none
reads as a finding on its own, and the finding is that a person tuning any
subsystem has many files to read and no index. There is no `reference/` or
`docs/` folder in the repository, so nothing enumerates any of these sets.

The one counterexample all hunters cite: `crates/shepr-protocol/src/limits.rs`.
Every consumer across `shepr-client`, `shepr-server` and `shepr-termio` reads the
constant, there are no magic restatements anywhere, and derived values
(`MAX_SURFACE_CELLS`, `MAX_TERMINAL_FRAME_BYTES`) are computed from their bases.
That is the model.

The inventories, by scope:

- **shepr-platform** (fifteen tunables in eleven files):
  `CLIPBOARD_HELPER_TIMEOUT` (2 s), `STARTUP_WAIT` (100 ms, wl-copy), two
  separate `POLL_INTERVAL`s (5 ms each), `MAX_CLIPBOARD_TEXT_BYTES` (1 MiB),
  `DEFAULT_MAX_LOG_BYTES` (5 MiB), `DEFAULT_RETAINED_LOG_FILES` (1),
  `LOG_FILE_MODE`, `PRIVATE_SOCKET_MODE`, `STAGING_ATTEMPTS`,
  `UNIX_SOCKET_PATH_MAX`, `IDLE_TIMEOUT` (60 s), `PROBE_INTERVAL` (1 s, ssh
  agent), the logind backoff ceiling (60 s),
  and an unnamed 100 ms in `wait_client_stream_readable`.
- **shepr-vt / shepr-pty**: `ACTOR_IDLE_POLL_MS`, the inbox byte and item caps
  and the resize retry constants at the top of `actor.rs`; `MAX_DRAIN_CHUNKS =
  1024` inside `handle_write_failure`; the read buffer `8192` and wake-drain
  buffer `64`; `MAX_OSC_BYTES` and the other scanner limits in `scan.rs`; the
  scrollback floor and cap and `MAX_CLIPBOARD_BYTES` (192 KiB) in vt `lib.rs`;
  `SYNCHRONIZED_OUTPUT_FLUSH_MARGIN` and `DEFAULT_DETECTION_ROWS` in mux
  `terminal.rs`; `MIN_PANE_ROWS`/`MIN_PANE_COLS` and the detection task's
  initial 50 ms sleep in `runtime.rs`.
- **shepr-agent**: detection limits (`MAX_RULES_PER_MANIFEST`, `MAX_GATE_DEPTH`,
  `MAX_TOTAL_GATES`, `MAX_MATCHERS_PER_GATE`, `MAX_REGIONS_PER_MANIFEST`,
  `MAX_TOTAL_MATCHERS`, `MAX_MATCHER_CHARS`) in `manifest.rs`; probe budgets
  (`CHILD_GROUPS_SCAN_LIMIT`, `FOREGROUND_TREE_SCAN_LIMIT`,
  `FOREGROUND_TASK_ENTRY_LIMIT`, `FOREGROUND_CHILD_BYTE_LIMIT`,
  `FOREGROUND_CHILD_PID_LIMIT`) in `proc_tree.rs`; version-probe budgets
  (`VERSION_PROBE_TIMEOUT`, `VERSION_PROBE_POLL_INTERVAL`,
  `MAX_VERSION_PROBE_OUTPUT`) in `version.rs`; session-ref caps
  (`MAX_SESSION_ID_LEN`, `MAX_SESSION_PATH_LEN`) in `resume.rs`; hook timeouts in
  `integration/mod.rs`; retry pacing (`SHEPR_OMP_IDLE_DEBOUNCE_MS`,
  `SHEPR_OMP_RETRY_GRACE_MS`) only inside the OMP TypeScript asset; a 12-line
  lookback and a 32-char needle cap in `contains_recent_non_whitespace`; and
  `128` temp-name attempts in both `file_ops.rs` and `config_file.rs`.
- **shepr-config**: `lib.rs` holds four (`DEFAULT_SCROLLBACK_LIMIT_BYTES`,
  `DEFAULT_MOUSE_SCROLL_LINES`, `DEFAULT_HEADLESS_COLS`,
  `DEFAULT_HEADLESS_ROWS`); the rest are wherever first needed:
  `MAX_SESSION_NAME_LEN = 64`, `MAX_SIDEBAR_ROWS = 16`,
  `MAX_SIDEBAR_TOKENS_PER_ROW = 16`, `DEFAULT_SIDEBAR_ROW_GAP = 0`,
  `MAX_TAB_BAR_RIGHT_ENTRIES = 16`,
  `MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS = 31_536_000`,
  `MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS = 3_600`,
  `DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS = 5`,
  `DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS = 2`, `KEY_BINDING_COUNT = 51`, plus
  roughly forty values buried in `Default` impls in `model.rs`. Several caps (the
  one-year interval, `MAX_SESSION_NAME_LEN`, the sidebar 16s) are documented
  nowhere a user would look, including `default.toml`.
- **shepr-api and the CLI**: `APP_RESPONSE_TIMEOUT` 5 s,
  `ORDINARY_REQUEST_TIMEOUT` 60 s, `INITIAL_REQUEST_TIMEOUT` 5 s,
  `STREAM_WRITE_TIMEOUT` 5 s, `CONNECTION_POLL_INTERVAL` 100 ms,
  `MAX_INITIAL_REQUEST_BYTES` 1 MiB, `ACCEPT_BACKOFF_MIN`/`MAX` (all in
  `shepr-api/src/server.rs`); `EventHub::MAX_EVENTS` 512;
  `ORDINARY_RESPONSE_TIMEOUT` and `WAIT_RESPONSE_GRACE` 30 s (`client.rs`);
  `AGENT_PROMPT_EFFECT_TIMEOUT_MS` 5 s and `AGENT_PROMPT_RESPONSE_GRACE` 1 s
  (`wait.rs`); `STOP_WAIT_TIMEOUT` 15 s, `STOP_WAIT_POLL` 25 ms,
  `MIN_SOCKET_TIMEOUT` 1 ms (`session.rs`); `SERVER_READY_TIMEOUT` 15 s
  (`src/autodetect.rs`); an inline `Duration::from_secs(15)` in
  `src/cli/target.rs`; `AGENT_START_POLL_INTERVAL` 100 ms,
  `PANE_SHELL_READINESS_RETRY_TIMEOUT` 2 s and
  `DEFAULT_AGENT_START_TIMEOUT_MS` 30 s (`src/cli/agent.rs`). The hunter names
  `ORDINARY_REQUEST_TIMEOUT` and `WAIT_RESPONSE_GRACE` as the two with real
  documentation and the standard the rest should meet.
- **shepr-remote** (seven files): `NONINTERACTIVE_SSH_COMMAND_TIMEOUT`;
  `PIPE_DRAIN_GRACE`, `POLL_INTERVAL`, `SSH_STDOUT_CAPTURE_LIMIT`,
  `SSH_STDERR_CAPTURE_LIMIT`; `BRIDGE_ACCEPT_POLL`, `BRIDGE_IO_POLL`,
  `BRIDGE_FAILURE_REPORT_TIMEOUT`, `BRIDGE_FAILURE_REPORT_POLL_INTERVAL`,
  `BRIDGE_SOCKET_PERMISSION_MODE`; `REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT`,
  `REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL`; `SOCKET_POLL_INTERVAL`,
  `STATUS_REQUEST_TIMEOUT`; `CATALOG_POLL_INTERVAL`, `MAX_CATALOG_BYTES`,
  `MAX_PROFILES`, `MAX_LABEL_BYTES`; `MAX_METADATA_BYTES`;
  `MAX_SSH_TARGET_BYTES`; `MAX_REMOTE_EXECUTABLE_BYTES`; plus an unnamed
  `Duration::from_millis(250)` in `bridge_connection`'s wait loop and
  100 ms / 500 ms / 10 ms inside `ssh_agent.rs`.
- **shepr-mux** (thirty-plus across seven files): the nine acquisition-timing
  knobs in `pane/process_probe.rs` (`RELEASE_REACQUIRE_SUPPRESSION`,
  `AGENT_MISS_CONFIRMATION_ATTEMPTS`, `PROCESS_RECHECK_IDENTIFIED`,
  `PROCESS_RECHECK_MISSING_FOREGROUND_GROUP`, `PROCESS_ACQUISITION_WINDOW`,
  `PROCESS_ACQUISITION_FAST_WINDOW`, `PROCESS_ACQUISITION_FAST_RECHECK`,
  `PROCESS_ACQUISITION_SLOW_RECHECK`, `PROCESS_ACQUISITION_IDLE_RESET`); six in
  `pane/agent_detection.rs` including `AGENT_ABSENCE_STARTUP_HOLD` aliased to
  `MANAGED_AGENT_RESUME_TIMEOUT`; `MIN_ALIGNMENT_RATIO_PERCENT = 30` and
  `SIMILAR_VIEWPORT_RATIO_PERCENT = 70` in `terminal/history_read.rs`, two
  similarity thresholds with no stated basis; `MAX_METADATA_SOURCES = 64`,
  `MAX_STATE_LABELS_PER_SOURCE = 16`, `MAX_SEQUENCE_SOURCES = 32`;
  `MAX_BODY_BYTES = 4096`, `AGENT_OSC_MAX_CHARS = 256` and a second
  `MAX_CHARS = 512` inside `sanitized_osc_debug_payload`;
  `DEFAULT_DETECTION_ROWS`, `SYNCHRONIZED_OUTPUT_FLUSH_MARGIN`,
  `SCAN_CHUNK_ROWS`, `COPY_MODE_WORD_SEPARATORS`; `SNAPSHOT_INTERVAL` (15 min)
  and `SNAPSHOT_LIMIT` (48, twelve hours of recovery, a number nobody wrote down)
  next to a bare inline `3` for the other recovery directory's limit;
  `GIT_STATUS_RETRY_DELAY` (30 s); `MAX_GIT_REF_FILE_BYTES`;
  `HOOK_SEQUENCE_REANCHOR_AFTER`; and unnamed bounds `for _ in 0..16` (symlink
  hops), `for sequence in 0..128` (recovery name attempts), `.take(256)`
  (palette).
- **shepr-server** (about forty): `MIN_RENDER_INTERVAL` 16 ms; git refresh
  1500 ms, 5 min, and a 30 s retry buried in a test fixture; session save
  debounce 5 s and backoff 250 ms to 30 s over 3 failures; agent start 30 s
  default / 300 s max / 3 s settle; agent resume retry 1 s and the managed-resume
  timeout; agent prompt submit delay 300 ms; alt-screen read quiet/step/max
  windows (10/10/120 ms, 15 s, 5 s, 3 wheel events); handshake timeout 4 s;
  shutdown flush timeout 1 s; pane teardown wait 3 s; shell cwd refresh 1 s;
  datetime refresh 1 s; read line cap 1000; seven metadata TTL and token caps in
  `api_helpers.rs`; layout pane and depth caps 24/16; copy query and match caps
  4096/1024; status text caps 4096/80; input batch cap 4096; handshake frame cap
  64 KiB; endpoint byte caps 1 MiB/128/128. `[advanced]` in the config model
  holds exactly one key (`scrollback_limit_bytes`). A person tuning alt-screen
  reads has no way to discover that `INITIAL_QUIET`, `OUTPUT_QUIET`,
  `STEP_TIMEOUT`, `MAX_DURATION`, `MAX_RESTORE_DURATION` and `WHEEL_STEP_EVENTS`
  are the six dials.
- **shepr-termio and shepr-client** (at least thirty in fourteen files):
  `HOST_KEYBOARD_QUERY_TIMEOUT`, `MAX_BUFFERED_HOST_INPUT`;
  `INITIAL_RETRY_DELAY`, `MAX_RETRY_DELAY`, `STABLE_CONNECTION_PERIOD`,
  `ATTENTION_RETRY_DELAY`, `ATTEMPT_BUDGET`; `MAX_QUEUED_BATCHES`,
  `MAX_BATCH_BYTES`, `MAX_QUEUED_BYTES`, `WRITE_TIMEOUT`, `IO_POLL_INTERVAL`;
  `ENDPOINT_COMMAND_TIMEOUT`, `MAX_RETIRED_REQUESTS_PER_ENDPOINT`,
  `MAX_ENDPOINT_RESPONSE_BYTES`; `HEARTBEAT_INTERVAL`, `HEARTBEAT_TIMEOUT`;
  `ACTIVATION_TIMEOUT`; `LOCAL_HANDSHAKE_READ_TIMEOUT`,
  `REMOTE_HANDSHAKE_READ_TIMEOUT`; `SELECTION_AUTOSCROLL_INTERVAL`,
  `SELECTION_REPAINT_INTERVAL`, `MODAL_PASTE_CLIPBOARD_TIMEOUT`;
  `MIN_TAB_WIDTH`, `NEW_TAB_WIDTH`, `WORKSPACE_HEADER_ROWS`;
  `TAB_SCROLL_BUTTON_WIDTH`, `MIN_TAB_STRIP_WIDTH`; `DEFAULT_CELL_WIDTH_PX`,
  `DEFAULT_CELL_HEIGHT_PX`; `MAX_PENDING_PASTE_BYTES`, `PASTE_STALL_TIMEOUT`,
  `RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS`,
  `MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS`,
  `MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES`, `MAX_DISCARDED_CONTROL_TAIL_BYTES`;
  `MAX_NOTICES` declared inside a function body in `lib.rs`, so it is invisible
  to anyone auditing the crate's limits; plus the 100 ms resize-poll sleep in
  `terminal_geometry.rs::resize_poll_loop` and the unnamed inline durations in
  HYGV-046.

Enforcement the hunters converge on: one `tunables.rs` (or `limits.rs` plus
`timing.rs`) per crate as the file of record, held by a `brokkr.toml` text rule
that `Duration::from_*`, `const MAX_*` and octal mode literals may appear only
there and in tests. The vt/pty hunter adds that a text rule forbidding numeric
`const` inside function bodies is feasible. Whether any individual knob should
become a config key is a judgement the code cannot reveal.

## HYGV-043 - The clipboard byte caps have two unrelated owners, and one is restated as a magic number in its own test

The platform half is resolved: `MAX_CLIPBOARD_TEXT_BYTES` is at module scope in
`crates/shepr-platform/src/clipboard.rs` and its test derives the oversize input
from it. The two caps stay separate on purpose: the 1 MiB platform cap bounds
host clipboard reads, the 192 KiB `shepr-vt` cap bounds terminal-originated OSC
52 stores, opposite directions.

Open: `shepr-vt`'s `MAX_CLIPBOARD_BYTES` drops an OSC 52 payload over 192 KiB
with no log line, so a copy from a pane that silently does nothing cannot be
diagnosed. A rate-limited log with the byte count (never the content) is the
fix.

## HYGV-072 - Three boolean-from-string parsers, no owner, and one is an incomplete implementation of an external grammar

**Decision (partial):** the `env_bool()` half is piece 1 (the `shepr-core`
environment registry, after broadarrow's `core::env`): shepr env flags have one
kind and one rule, exactly `1`/`0`/`true`/`false`, so `osc.rs`'s
`"1" | "true" | "yes" | "on"` goes (and `yes`/`on` become refusals). The
`git_bool()` half is resolved: `git/config.rs::git_config_bool` follows git's
grammar (checked against git 2.53) and serves `core.bare` and
`worktree_config_enabled`. Open: the env half, until piece 1 lands at `osc.rs`.

Reported by the mux hunter.

- `crates/shepr-mux/src/git/config.rs`: `"true" | "1" | "yes" | "on"` (Git's
  boolean syntax).
- `crates/shepr-mux/src/pane/osc.rs`: `"1" | "true" | "yes" | "on"` (a shepr env
  var).
- `crates/shepr-remote/src/remote/server_lifecycle.rs`: `"y" | "yes"` (a
  prompt).

The first two are the same list in a different order for two different domains.
Git's actual boolean syntax also accepts the empty string as true and
`off` / `no` / `false` as false, so the git copy is an incomplete implementation
of a documented external grammar while the osc copy is shepr's own invention that
happens to look identical.

This is two owners rather than one: a `git_bool()` in the git module completed
against Git's grammar, and one `env_bool()` wherever shepr env flags are
resolved. Holdable by a text rule forbidding the bare list elsewhere.

## HYGV-087 - Identifier allocation reaches process-global counters and clocks directly, with no injection point and no owner of the format

Reported by the core/platform, protocol/config, remote and server hunters.

- `crates/shepr-core/src/layout.rs`: `static NEXT_PANE_ID`. `PaneId::alloc()`
  reads it, and `alloc_from(&counter)` exists purely so the exhaustion test can
  inject one. Any test wanting deterministic pane ids must use `from_raw`, which
  bypasses validation entirely: it accepts `0`, the documented placeholder, while
  `collect_validated_ids` rejects `0`. Fix: `PaneId::from_raw -> Option<PaneId>`
  is a compiler-enforced signature change; removing the global needs an allocator
  value threaded through `Workspace`, which is the larger and better fix.
- `crates/shepr-protocol/src/ids.rs`'s doc claims `TerminalId` is an "opaque
  identity for a server-owned terminal ... callers must not derive it from a pane
  id or layout position", while `TerminalId` has a public `From<String>` and a
  non-`cfg`-gated `pub fn test_new`, so deriving one from anything is a one-liner.
  Removing `From<String>` and gating `test_new` makes the claim structural.

## HYGV-089 - Remote configuration is validated at the moment of use, on the client, at attach time

Reported by the protocol/config hunter, as the one real instance in that scope
and as a qualification of the project's "read and validated once at launch"
sentence.

The local path holds: `Config::load_validated` returns `Err` if `diagnostics` is
non-empty, `main.rs::load_validated_config_or_exit` exits, and
`bootstrap.rs::encode_resolved_config` propagates an encode failure with `?` so
the server refuses to boot. But `ValidatedConfig`'s `Deserialize` re-runs
`from_resolution(..., CwdCheck::Received)`, so a remote endpoint's config is
validated during snapshot decode on the client rather than at that client's
launch, and a config the server accepted can be rejected by the client.

This duplication is forced (two hosts, two binaries, one config travelling
between them). What keeps the two validations in step is the exact-build preamble
plus the shared crate, and the hunter's recommendation is to say that out loud in
the `AGENTS.md` sentence, which currently reads as absolute.
