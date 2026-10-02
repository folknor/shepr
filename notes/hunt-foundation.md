# Design hunt: foundation (shepr-core, shepr-platform, shepr-pty)

Scope read in full: every non-test source file of `crates/shepr-core`,
`crates/shepr-platform` and `crates/shepr-pty`. Consumers in vt, agent, config,
protocol, api, remote, mux, server, client and the root binary were followed
wherever a foundation type or decision leaks out. File references name the
file and the function or type; line numbers are left out on purpose.

## Headline

The three crates are individually careful (the libc and fd work is good, and
`env.rs`, `ProcessHandle` and the launch status socket are solid). The design
debt sits at their edges:

1. **pty and platform are forbidden from knowing each other** (`brokkr.toml`,
   `shepr-pty-layer` allows only `bytes`, `libc`, `shepr-core`, `tracing`).
   So pty reimplements platform plumbing (nonblocking fds, poll deadlines,
   peer credentials, accept errno triage), and shepr-mux glues the two halves
   of one child together (`PaneChild` from pty, `ProcessHandle` from
   platform, opened by mux after the fork). Several of the scattered decisions
   below exist only because of this wall.
2. **Platform owns things that are not platform**: a pane exit vocabulary with
   session checkpoint policy (`ChildExitReason`), the SSH bridge stdio relay
   and its watchdog (which in turn drags client heartbeat timing into
   shepr-core), and half of an atomic-replace protocol whose other halves live
   in three consumer crates.
3. **Geometry and ids escape as primitives**: `HostGeometry` is routinely
   taken apart into five numbers (with `0` meaning "no cell size") and rebuilt;
   `PaneId::raw()` is called about forty times in production, nearly all to
   log; split ratios cross every API as bare `f32` and are clamped in four
   places.
4. **Classification questions answered per call site**: "is this IO error a
   gone peer", "what does this accept errno mean", "is this process dead per
   /proc", "is this file/dir private enough to trust" are each answered in
   three to six places, and in several cases the answers already disagree.

Two concrete defects surfaced along the way (details under Lateral findings):
the remote bridge listener and the pty launch status listener both stop
accepting forever on transient accept errors that the platform classifier
treats as retryable.

---

## 1. Axes that should be types

### 1.1 The resolved pane shell travels as `&str` and is re-derived three times

- `shepr-config/src/validated.rs` resolves the shell (`resolve_recognized_shell`:
  PATH search, access check, recognized basename) to an absolute path, then
  `shell_path_string` turns it into a `String`.
- `shepr-mux/src/pane/launch.rs` `PaneShellConfig { default_shell: &str, .. }`
  carries it on.
- `shepr-pty/src/command.rs` `PtyCommand::interactive_shell(default_shell: &str, login)`
  trims it again, writes `SHELL` from it, and `launch_spec` re-checks
  `is_absolute()` and overwrites `SHELL` a second time.
- `shepr-mux/src/pane/runtime.rs` `PtySetup::start` recovers the shell for
  error messages by reading `cmd.get_env(ChildEnv::Shell)` back out of the env
  map, with `String::new()` as the fallback.

Proposal: a `ResolvedShell` (absolute, executable, recognized, with
`login: bool` or a `ShellInvocation` enum) minted only by config validation.
`PtyCommand::new(shell: &ResolvedShell)` takes it, `launch_spec` stops
re-validating, and `SpawnedPty` hands back the program for messages. The
trims and the relative-path refusal in `launch_spec` disappear because the
wrong value cannot be built.

### 1.2 Host terminal geometry is decomposed into five primitives

`HostGeometry` (`shepr-core/src/geometry.rs`) has `cols()`, `rows()`,
`cell_width()`, `cell_height()` (both `0` when the cell is unknown) and a public
`exact: bool`. In shepr-client these are pulled apart and rebuilt in a loop:

- `client/src/state.rs` `set_host_size` rebuilds a `HostGeometry` from
  `ClientHostSize` plus the old geometry's `cell_width()`, `cell_height()`,
  `exact`.
- `client/src/lib.rs` `run_until_exit` feeds `cell_width()/cell_height()/exact`
  into `ProtocolCellSize::from_host`, then builds a new `HostGeometry` from
  `cols()/rows()` and `cell.width()/cell.height()/cell.exact`.
- `client/src/lib.rs` `handle_event` destructures `Resize(geometry)` into
  `handle_resize(cols, rows, cell_width, cell_height, exact)`.
- `client/src/handshake.rs` and `terminal_geometry.rs` `bounded_cell_geometry`
  return `(u32, u32, bool)`.
- `shepr-platform/src/host.rs` `terminal_grid_size()` returns `(u16, u16)`
  although core has `GridSize`.

Proposal: `HostGeometry` with private fields, `with_grid(GridSize)`,
`cell() -> Option<CellPx>`, and exactness as an enum carried only on a known
cell (`enum CellKnowledge { Unknown, Estimated(CellPx), Exact(CellPx) }`).
Pixel bounds (`MAX_CELL_SIZE_PX`, today only in protocol) belong to `CellPx`
construction. `terminal_grid_size` returns `GridSize`. Every
`(u32, u32, bool)` round trip goes away.

### 1.3 Process ids have no type

Pids appear as `u32` (`PaneChild::id`, `ProcessHandle::pid`,
`SpawnedDaemon::id`, `ProcessIdentity.pid`, launch `Waiting.pid`), as
`libc::pid_t` (`daemon.rs` `kill_process_group`, `PaneChild::raw_pid`), and
session ids as both `u32` (`session_member_handles(session_id: u32, ..)`) and
`i32` (`session_and_tty_from_stat`). Each boundary re-checks `> 0` and the
`pid_t` range by hand. In mux, `ChildLiveness::new(0, None)` uses `0` as "no
child".

Proposal: `Pid(NonZeroU32)` in shepr-platform with `as_pid_t()`, `SessionId`
and `ProcessGroupId` as distinct wrappers (a session id equals the leader's
pid by construction, so `SessionId::of_leader(Pid)`). `ChildLiveness` takes
`Option<Pid>`.

### 1.4 Launch status reports leak indices and errnos

`shepr-pty/src/launch.rs` `LaunchRecord::{ChdirOk(u32), ChdirFailed(i32),
ExecFailed(i32)}` and `backend.rs` `SpawnedPty { cwd_candidates: Vec<PathBuf>, .. }`
are handed to mux separately. `shepr-mux/src/pane/launch_status.rs` `settle`
then:

- bounds-checks the index against `cwd_candidates` it got from pty,
- enforces the record order (ChdirFailed only before ChdirOk, ExecFailed only
  after, EOF after ChdirOk means exec committed),
- turns errnos into `io::Error::from_raw_os_error`,
- uses `cwd_candidates.first().cloned().unwrap_or_default()` (an empty path
  sentinel) for the failed directory.

The launch protocol is pty's, but its state machine is in mux. Proposal: pty
exposes a `LaunchStatusReader` built from the registration and the candidates,
whose `poll()` yields
`LaunchOutcome::{Entered(PathBuf), DirectoryFailed { path, error }, ExecFailed(io::Error), Committed, ProtocolViolation}`.
Mux keeps only the async waiting and the settlement policy.

### 1.5 A verified server stream and an unverified one are the same type

`shepr-platform/src/ipc.rs` has `pub type LocalStream = UnixStream` and
`pub type LocalListener = UnixListener`. `connect_trusted_local_stream` (peer
uid checked) and `connect_local_stream` (not checked) both return
`LocalStream`, so nothing stops a client path from writing to an unverified
stream; today that is held by convention (production callers that send data
use the trusted variant; `shepr-api/src/server.rs` `wake_listener` and `probe`
use the untrusted one deliberately). Proposal: `TrustedServerStream` returned
only by the trusted connect, accepted by the handshake and API client; an
`AdmittedPeer` returned by an accept helper that ran `peer_is_same_user`
(see 2.2). The aliases go.

### 1.6 A bound socket is a bare triple

`bind_private_socket` and `bind_single_use_private_socket` return
`(LocalListener, SocketStartupLock, SocketFileIdentity)`, and
`remove_socket_file_if_owned(path, identity)` takes the path and the identity
separately, so a caller can pair an identity with the wrong path. Consumers
rebundle it themselves (`shepr-api/src/server.rs` `ServerHandle`,
`shepr-remote/src/remote/bridge.rs` with `TeardownResource::Socket { path, identity }`
and `BridgeSocketStartupCleanup`). Proposal: `BoundSocket { listener, lock, path, identity }`
with `remove_if_still_ours(self)` and a lock lifetime tied to the struct.

### 1.7 Typed failures smuggled through `io::Error` payloads

- `SocketBusy` is an `io::ErrorKind::AddrInUse` payload, recovered by
  downcast in `shepr-api/src/server.rs`, `shepr-server/.../bootstrap.rs`,
  `shepr-remote/src/remote/machine_ssh.rs`.
- `UnsafeSshRuntimeDirectory` is a `PermissionDenied` payload, downcast in
  `shepr-remote/src/lib.rs` (twice) and `machine_ssh.rs`.
- `RuntimeCreateError::into_io` erases the random-source distinction it just
  made.
- `FileLoggingUnavailable.reason: String` is prose.
- `EnvError` has a `From` into `io::Error`, and most readers immediately go
  through `io::Result`.

Each downcast is a caller re-deciding "was this the policy refusal" from an
erased type. Proposal: `bind_*` return `Result<BoundSocket, BindError>` with
`BindError::{Busy(PathBuf), Io(io::Error)}`; SSH path helpers return
`Result<_, SshRuntimeError>` with an `UnsafeDirectory` arm; consumers match
instead of downcasting.

### 1.8 Closed-set environment values typed as free text

In `shepr-core/src/env.rs`, `SHEPR_BUILD_PROFILE`, `SHEPR_ENV` and
`SHEPR_PANE_ID` are `EnvKind::Text`. Their consumers compare strings:
`src/main.rs` `should_block_nested_for_env` compares to `SHEPR_ENV_IN_PANE`
(`"1"`) and to `BuildProfile::current().marker()`; `shepr-config/src/io.rs`
parses the same marker with `BuildProfile::from_marker`. (This already
disagrees; see 2.6.) Proposal: a typed read `pane_marker() -> Result<PaneMarker, EnvError>`
where `PaneMarker { in_pane: bool, owner: Option<BuildProfile> }`, owned by
whichever crate owns `BuildProfile` (config), and read once.

Related: the typed readers `resolve_flag`, `resolve_text`, `resolve_path`,
`resolve_os` assert the kind at runtime and `unreachable!` on the
`EnvValue` arm. The kind is a compile-time fact of the variant. Splitting
`EnvVar` into per-kind enums (`FlagVar`, `TextVar`, `PathVar`, `PresenceVar`,
`RawVar`), each with exactly one reader, removes the panics and the
`EnvValue` enum, while `EnvVar::ALL` can stay as their union for the registry.

### 1.9 Flags and sentinels as parameters

- `ipc::acquire_flock_lock(path, blocking: bool)`.
- `remote_bridge_io::forward_remote_bridge_stdio(stream, idle_timeout: bool)`.
- Poll timeouts as `i32` milliseconds with `-1` meaning forever
  (`child_io::poll_fd_readable`, `poll_fd`, `fd::poll_pty_and_wake`), passed
  through from shepr-client. A `Wait::{Forever, Until(Instant), Now}` would
  also own the deadline conversion (2.11).
- `PtyIoInbox::push_terminal_response -> (bool, bool)` ("accepted",
  "first drop").
- `ChildIo::owns_child_process() -> bool`.

### 1.10 Split ratio is an `f32` at every API

`TileLayout::split_pane(.., ratio: f32)`, `set_ratio_at(path, f32)`,
`resize_focused(nav, delta: f32, ..)`, `SplitBorder.ratio: f32`, and the wire
`LayoutSetSplitRatioParams.ratio`. `SplitRatio` exists but only lives inside
`Node`. See 2.4 for the four clamps this causes. Proposal: `SplitRatio` (with
serde that refuses out-of-range values) on every API and on the wire, a
`RatioDelta` for resizes, and `set_ratio_at` returning
`Option<RatioChange::{Changed, Unchanged}>` so callers stop recomputing
`splits(area)` just to read the old ratio.

### 1.11 Saved pane ids and live pane ids share a primitive

`shepr-mux/src/persist/snapshot.rs` keys everything by `u32`
(`HashMap<u32, PaneSnapshot>`, `LayoutSnapshot::Pane(u32)`,
`type PaneKey = (usize, u32)`), and `TileLayout::from_saved` documents
"Callers must remap restored IDs through `PaneId::alloc` first". The remap is a
comment-enforced obligation. Proposal: a `SavedPaneId(u32)` in the snapshot
model, and `TileLayout::from_saved(root: SavedNode, focus: SavedPaneId) -> (TileLayout, Remap)`
that allocates live ids itself, so a live layout cannot be built from saved
ids at all.

### 1.12 Log file selection by string

`logging::init_file_logging(dir, file_name: &str)` with the constants
`SERVER_LOG_FILE`, `CLIENT_LOG_FILE`. A `LogFile::{Server, Client}` would also
give the one place to compute its path (2.16).

---

## 2. Decisions made in more than one place

### 2.1 "Does this IO error mean the peer is gone?" (six answers, disagreeing)

| Site | Kinds |
|---|---|
| `shepr-platform/src/ipc.rs` `is_connection_closed_error` | BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected, UnexpectedEof, WriteZero |
| `shepr-platform/src/remote_bridge_io.rs` `is_closed_socket` | BrokenPipe, ConnectionReset, NotConnected |
| `shepr-client/src/shell_runtime.rs` `endpoint_disconnect_notice` | UnexpectedEof, BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected (no WriteZero) |
| `shepr-api/src/server_stop.rs` `stop_request_error_allows_wait` | BrokenPipe, ConnectionReset, UnexpectedEof, NotConnected, TimedOut, WouldBlock |
| `shepr-api/src/status.rs` `status_probe_has_no_answer` | ConnectionRefused, NotFound, BrokenPipe, ConnectionReset, UnexpectedEof, NotConnected, TimedOut, WouldBlock |
| `shepr-remote/src/lib.rs` `is_ssh_link_error_kind` | TimedOut, ConnectionRefused, ConnectionReset, AddrInUse, Host/NetworkUnreachable, NetworkDown |

They already disagree (ConnectionAborted and WriteZero in one, not another).
`status.rs` has a pairwise test (`launch_and_stop_share_status_transport_failure_classification`)
holding two of these in step, which is exactly the kind of agreement test that
only catches the drift it exercises. Owner: shepr-platform, as
`classify_stream_error(&io::Error) -> StreamFailure::{PeerGone, NoListener, TimedOut, Other}`.
Each consumer then matches the arms it cares about.

### 2.2 "What does this accept failure mean?" and "may this peer in?" (three loops)

- `shepr-platform/src/ipc.rs` `accept_failed_for_one_connection`: EINTR,
  ECONNABORTED, EPROTO, EPERM retry at once; everything else backs off. Used by
  `shepr-api/src/server/listener.rs`.
- `shepr-pty/src/launch.rs` `Router::accept_loop`: EINTR, ECONNABORTED,
  EPROTO, EAGAIN retry; EMFILE, ENFILE, ENOBUFS, ENOMEM back off; anything
  else (EPERM included) logs an error and **returns, ending the listener for
  the life of the process**.
- `shepr-remote/src/remote/bridge.rs` accept thread: WouldBlock sleeps; every
  other error, EINTR, ECONNABORTED and EMFILE included, **breaks the loop and
  ends the bridge listener**.

Peer admission is likewise decided three times: `ipc::peer_is_same_user`
(api listener, remote bridge) and an inline `SO_PEERCRED` plus
`credentials.uid != geteuid()` in `pty/src/launch.rs` `accept_hello`.

Owner: shepr-platform, as one accept helper returning
`Accepted::{Peer(AdmittedPeer), RetryNow, Backoff, Fatal(io::Error)}` with the
credential check folded in (and `AdmittedPeer` exposing the peer pid that
pty's router needs). This requires pty to depend on platform (3.1).

### 2.3 Reading `/proc/<pid>/stat` and "is this process dead" (four parsers)

- `shepr-platform/src/process.rs` `session_and_tty_from_stat`: skip past the
  last `)`, then `skip(3)` to session and tty.
- `shepr-platform/src/process_identity.rs` `process_snapshot`: its own `)`
  split, state char, `nth(18)` for start time; dead means state `Z | X`.
- `shepr-agent/src/detect/proc_tree.rs` `foreground_process_group_id`: field 5
  after `)` for tpgid.
- `shepr-agent/src/detect/proc_tree.rs` `process_pgrp_comm_and_state_from_stat`:
  comm, state, pgrp; `process_state_allows_remote_memory_read` refuses
  `D | Z | X | x`.

The field arithmetic is repeated with different offsets, and the
"finished process" states differ (`x` only in agent). Agent already depends
on platform. Owner: shepr-platform `ProcStat::read(Pid) -> io::Result<Option<ProcStat>>`
with typed fields (`state: ProcState`, `ppid`, `pgrp`, `session`, `tty_nr`,
`tpgid`, `start_ticks`) and `ProcState::is_finished()`.

### 2.4 "What is a valid split ratio, and how is one clamped?" (four answers)

- `shepr-core/src/layout.rs` `SplitRatio::new` and `SplitRatio::clamped`
  (non-finite becomes `EVEN_SPLIT`).
- `shepr-client/src/shell/sidebar/sidebar_tokens.rs` `SectionSplit`: the same
  bounds, the same finite check, the same `EVEN_SPLIT` fallback and its own
  serde, rewritten against the core constants.
- `shepr-client/src/shell/input/mouse.rs` `pane_split_ratio` clamps to
  `MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO` before sending.
- `shepr-server/src/app/api/layouts.rs` `handle_layout_set_split_ratio`
  rejects non-finite itself, clamps through `SplitRatio::clamped(..).get()` to
  compare `to_bits()` with the stored ratio, then passes the raw `f32` to
  `set_split_ratio_at`, which clamps again.

They agree today because the constants are shared. Owner: `SplitRatio` in
core, used on the wire and by `SectionSplit` (which can become
`SplitRatio` or a newtype over it).

### 2.5 Cell exactness and the minimum host grid

"Pixel coordinates are exact only if a cell size is known" is decided in
`HostGeometry::new` (core), `TerminalGeometry::new` and its
`TryFrom<ReceivedTerminalGeometry>` (protocol), and
`ProtocolCellSize::from_host` (protocol), which adds a `MAX_CELL_SIZE_PX`
bound that the core type does not know. A host cell of 100000 px is "exact" to
core and not to protocol.

The minimum host grid is decided three ways, and two disagree:
`GridSize::clamped` documents "Preserve host and protocol grids down to one
cell", yet `HostGeometry` wraps a `PaneGeometry`, which clamps to the pane
minimum (4 by 2) through `GridSize::clamped_pane`. `ClientHostSize::new` in
`client/src/terminal_geometry.rs` clamps through `ClientSurfaceSize::clamped`
instead. So a 2-column host terminal reads as 4 columns through
`reported_geometry.cols()`. Owner: core `HostGeometry` with its own grid rule
(not `PaneGeometry`), and `CellPx` owning the pixel bound.

### 2.6 Build profile marker and startup cwd, each read twice

- `SHEPR_BUILD_PROFILE`: `src/main.rs` `should_block_nested_for_env` treats
  any marker other than the current one as "another profile" (so an invalid
  value such as `staging` is not refused there), while
  `shepr-config/src/io.rs` `resolve_paths_from_env` refuses it as a launch
  error. AGENTS.md says an invalid marker fails the launch; the nested check
  answers a different question about the same value. One typed read (1.8)
  owned by config.
- `SHEPR_STARTUP_CWD`: `shepr-config/src/io.rs` `resolve_current_dir`
  requires it to be absolute; `shepr-server/.../bootstrap.rs`
  `read_startup_cwd` reads it again with no absolute check. The env kind is
  `Handoff` (byte-preserving, no absolute rule), so the policy lives in one
  consumer only. Owner: read once into `AppPaths` and passed to bootstrap.

### 2.7 "Is this file or directory private enough to trust?"

Owner-and-type checks, each written out:

- `daemon.rs` `open_boot_log` (regular file, uid, then chmod 0600),
  `read_boot_log_tail` (regular, then uid, different errors and kinds).
- `ipc.rs` `acquire_flock_lock` (regular, uid, chmod 0600).
- `owned_runtime.rs` sweep (regular, uid, mode exactly `RUNTIME_MARKER_MODE`,
  size cap) and `private_directory` (dir, uid, mode exactly 0700).
- `ssh_paths.rs` `validate_shared_ssh_dir` (dir, uid, mode exactly 0700, via
  the literal `0o7777` while `limits::PERMISSION_BITS` exists).

The mode `0o600` is a literal in `open_boot_log`, `acquire_flock_lock`,
`private_file.rs` `create_private_file`, and as two constants
(`PRIVATE_SOCKET_MODE`, `LOG_FILE_MODE`) plus `RUNTIME_MARKER_MODE`. Also
inconsistent in scope: the SSH runtime directory must be exactly 0700 and
owned, but the server socket's directory is only created through
`create_private_directory_all`, which accepts an existing directory "whatever
its mode" (the server relies on `SO_PEERCRED` per connection). That may be
deliberate, but nothing records the decision in one place. Owner:
shepr-platform `PrivateFile::open_or_create(path, Policy)` and
`PrivateDir::require(path)` returning typed refusals.

### 2.8 The owned runtime artifact layout is restated in six places

Per `DirectoryKind`, `owned_runtime.rs` decides the name prefix in
`create_directory` (`.s`, `shepr-ssh-` with a 16-hex token), again in `sweep`
(prefix plus `RUNTIME_TOKEN_HEX_BYTES`), the allowed content in
`contents_owned` (`s` socket, `config` file), and the content again in
`release`. Outside the module, `ipc.rs` `bind_via_private_staging` joins
`"s"` and `shepr-remote/src/remote/ssh.rs` joins `"config"`. If remote renamed
its file, sweeps would refuse to reclaim and `release` would leave the
directory behind, silently. Owner: methods on `DirectoryKind`
(`prefix()`, `content_name()`, `content_type()`), and the entry hands out
`content_path()` so no consumer spells the name.

### 2.9 "Durably and atomically publish a private file" (three implementations)

- `shepr-mux/src/persist/io.rs` `publish_private_file`: create private temp,
  copy, fsync, rename, sync dir; `Published::{Durable, NotDurable}`; no symlink
  check at the target.
- `shepr-remote/src/machine/ssh_metadata.rs`
  `store_private_json_with_directory_sync`: refuses a symlink or non-file
  target, its own temp naming with `unpredictable_token` and a sequence, dir
  sync failure only logged at debug.
- `shepr-agent/src/integration/atomic_replace.rs` with platform's
  `config_file.rs` (`create_config_temporary`, `write_config_temporary`
  copying owner, ACL xattrs and mode).

Each answers temp naming, symlink policy and the meaning of a failed dir sync
differently. Owner: shepr-platform `publish_file(target, contents, Options { preserve_metadata_from, refuse_symlink_target, durability })`
returning a typed durability outcome. `config_file.rs` is the start of it but
is named for one consumer.

### 2.10 The Unix socket path limit

`shepr-core/src/socket_path.rs` says it is "owned once" and that every site
asks it. `shepr-platform/src/ipc.rs` `connect_local_stream_within` decides
independently with `bytes.len() >= address.sun_path.len()`. Same answer today
(108 > 107), different source. The main server socket path is never checked
up front at all; it only fails at bind. Owner: core, through a
`SocketPath` type constructed with the check, used by `AppPaths` and by the
connect.

### 2.11 Deadline to poll timeout

`shepr-platform/src/child_io.rs` `poll_timeout_until` (with
`MIN_POLL_TIMEOUT_MILLISECONDS`) and `shepr-pty/src/fd.rs`
`poll_pty_and_wake` (inline, with its own `MIN_POLL_TIMEOUT_MS`) each decide
how a remaining duration becomes a poll argument and what "at least 1 ms"
means. Also `set_nonblocking` exists in `clipboard.rs`, `fd.rs`, and inline
in `ipc.rs` (duplicated code, mentioned only because 3.1 removes it for free).

### 2.12 Pane exit vocabulary and the checkpoint decision

- pty `ReaderExit::{ShutdownRequested, Closed, IoFailed, Panicked}` (ordered
  by severity through derive order).
- platform `ChildExitReason::{Exited, Interrupted, WaitFailed, ReaderPanicked, ReaderIoFailed, TerminalClosed}`
  with `requires_session_checkpoint`.
- mux `pane/runtime.rs` `reader_exit_callback` maps the first onto the second.
- "Does this exit get a checkpoint?" is `requires_session_checkpoint` plus a
  second clause in `shepr-server/src/app/events.rs`
  `pane_exit_needs_checkpoint` ("and the core is not broken"), plus the same
  method again inside `shepr-mux/.../source/detection.rs`
  `transition_pane_exit` and `CheckpointCandidate::qualifies`.

AGENTS.md says the checkpoint policy lives in shepr-server; the method lives in
platform. Owner: one `PaneEnding` type in shepr-mux (next to the exit
arbiter) carrying the reason and whether the core is intact, with a single
`needs_checkpoint()`; platform keeps only `classify_child_exit` returning a
plain `ExitKind::{Exited(code), Signalled(sig)}`.

### 2.13 Whitespace in shell values

`env.rs`'s doctrine: interpreted values refuse surrounding whitespace,
because guessing is how a setting silently fails. `SHELL` is `Raw`, then
`shepr-core/src/shell.rs` `trim_shell_value` trims it (with a careful
non-UTF-8 path that is moot, since `shell_path_string` refuses non-UTF-8
afterwards); `terminal.default_shell` is `trim()`med in
`validated.rs`; `PtyCommand::interactive_shell` trims again. Owner: config
validation, once, under one stated rule (refuse or trim, not both).

### 2.14 "Is this HOME usable?"

`shepr-core/src/pathutil.rs` `home_dir_from_env_value` (absolute, no padding,
UTF-8) exists precisely for a HOME captured in a child environment, but
`shepr-pty/src/command.rs` `cwd_candidates` decides for itself
(`Path::is_absolute` only), so `"/home/me "` is a refused HOME to core and a
cwd candidate to pty.

### 2.15 What a child's environment contains

`ChildEnv` documents "every name shepr writes into a child in one place", but
mux writes `SHEPR_ENV`, `SHEPR_SOCKET_PATH`, `SHEPR_BUILD_PROFILE`,
`SHEPR_PANE_ID` and `TERM_PROGRAM` through `EnvVar`, and pty writes `PWD` and
removes `OLDPWD` by string literal outside both vocabularies.
`PtyCommand::env` accepts any `AsRef<OsStr>`, and `PaneLaunchEnv.extra` is
`Vec<(String, String)>`. `SHELL` is set in `interactive_shell` and overwritten
in `launch_spec`. The read/write split of the two enums does not match the
use. Owner: one child-env vocabulary (or `EnvVar` gaining a "written to
panes" attribute), and `PtyCommand` taking registered names for everything
shepr itself sets, with a separate `extra` channel for user-supplied pairs.

### 2.16 Where the server log is

`data_dir.join(SERVER_LOG_FILE)` is composed in
`shepr-server/.../bootstrap.rs` (twice), `shepr-remote/src/remote/local_server.rs`,
and implicitly in `logging::help_log_paths_summary`. Owner: `AppPaths::server_log()`.

### 2.17 Smaller repeats

- Hostname: `shepr-server/src/app/mod.rs` reads it once at startup
  (`unwrap_or_default`, empty string as "unknown"); `shepr-mux/src/pane/osc.rs`
  calls `shepr_platform::hostname()` (a syscall) on every OSC 7 parse. Two
  answers to "this machine's name" taken at different times, and a syscall in
  the PTY parse path.
- `XDG_CONFIG_HOME`: read by `shepr-config/src/io.rs` and again per call by
  `shepr-mux/src/git/config.rs` `git_user_config_paths_at` (errors swallowed).
- Panic payload to message: `pty/src/actor.rs` `panic_payload_message`,
  `shepr-client/src/fatal_panic.rs`, shepr-test-support (code duplication only).

---

## 3. Structure

### 3.1 Let shepr-pty depend on shepr-platform, and give the child one handle

The layering wall between pty and platform is the root of 2.2, 2.11 and part
of 2.14, and it forces mux to assemble a pane child from two crates:
`PtySetup::start` in `shepr-mux/src/pane/runtime.rs` calls `spawn_pty`, then
`ProcessHandle::open(pid)` itself, kills via `PaneChild::kill` (plain
`kill(pid)`) on failure, and builds `ChildLiveness::launching(pid, leader)`.
`PaneChild` (pty) reaps with `waitpid`; `ProcessHandle` (platform) probes and
reaps with the pidfd; `PaneChild::mark_reaped` keeps them in step by hand.

Proposal: pty depends on platform. `spawn_pty` opens the pidfd immediately
after the fork (or forks with `clone3(CLONE_PIDFD)`), and `PaneChild` owns the
`ProcessHandle`; `kill`, `try_wait`, `wait` all go through the pidfd, and the
"was it reaped" state has one owner. The launch status router uses platform's
accept helper and peer check. pty's `fd.rs` keeps only PTY-specific code
(wake pipe, winsize). This is a real simplification of mux too: `ChildLiveness`
can shrink to wrapping the child's handle.

### 3.2 Platform hosts non-platform policy

- `ChildExitReason` and its checkpoint policy (2.12) belong to mux.
- The SSH bridge relay (`remote_bridge.rs`, `remote_bridge_io.rs`) is
  shepr-remote's protocol: its idle timeout is defined by client heartbeats.
  Because it sits in platform, `shepr-core/src/limits.rs` has to own
  `BRIDGE_IDLE_TIMEOUT`, `HEARTBEAT_INTERVAL`, `SSH_ROUND_TRIP_TIMEOUT`,
  `SSH_ATTEMPT_SLACK` and `SSH_CONNECTION_ATTEMPT_BUDGET`. Move the relay into
  shepr-remote (its only caller is `shepr-remote/src/remote/host.rs`) and the
  timing to remote, which client already depends on. Core loses its only
  network-timing knowledge.
- `ssh_paths.rs` (OpenSSH `%C` expansion arithmetic, control path naming) is
  SSH policy; the generic piece is the owned runtime directory. Same move.
- `config_file.rs` is half of agent integration's atomic replace (2.9).

### 3.3 shepr-core holds a dead Git parser

`shepr-core/src/env.rs` carries about 200 lines (`read_git_config_parameters`,
`parse_git_config_parameters`, `parse_git_single_quote`,
`parse_git_config_count`, `indexed_git_config_pair`) with **no production
caller**; only `is_registered_name` uses the indexed-name recognizer, for test
isolation. Git subprocesses spawned by mux inherit the variables and apply
them themselves. Delete the parser (keep the name recognizer), or if it was
meant to feed `shepr-mux/src/git/config.rs`, move it there and wire it.

### 3.4 `shepr-core/src/shell.rs` is config's

Its doc says the lookup is "shared between config validation and PTY launch";
PTY launch does not use it (it requires an absolute path). Its only caller is
`shepr-config/src/validated.rs`, which also supplies the classifier closure
that calls platform's `has_execute_access`. Move `resolve_executable` and
`ExecutableStatus` to config, and offer `shepr_platform::classify_executable(path) -> ExecutableStatus`
so the access policy is not a closure in config.

### 3.5 `shepr-core/src/layout.rs`

- `PaneId` and its process-global allocator live in the layout module; pty
  imports `shepr_core::layout::PaneId` only to put it in log lines and thread
  names. An `ids` module (or `PaneId` next to its allocator) reads better.
- `TileLayout::resize_pane` swaps `self.focus` to the target, calls
  `resize_focused`, and swaps it back. The focus-centred method should take the
  pane, with `resize_focused` as the wrapper.
- `split_focused` is documented as "used by tests" yet `pub`; `TileLayout::new`
  returns `(Self, PaneId)` although the id is `focused()`.
- `Node` is fully public, so an invalid tree (duplicate leaves) is
  constructible anywhere; only `from_saved` validates. `InvalidSavedLayout::InvalidSplitRatio`
  is never produced by `from_saved` (a `Node` already holds a valid
  `SplitRatio`); mux produces it in `persist/restore.rs`. With `SavedNode`
  (1.11) the variant moves to the saved model.
- `SplitBranch` lives in `geometry.rs` but is a layout path element; split
  addresses are `Vec<SplitBranch>` compared with `==` in
  `api/layouts.rs`. A `SplitPath` type would own `split_path_for_children` and
  the ratio get/set by path.

### 3.6 Platform's flat module list

The flat layout mixes client-only pieces (clipboard, `terminal_grid_size`,
SIGWINCH watcher, OSC 52 preference, `begin_cli_output`), server-only pieces
(`redirect_stderr_to_null`, `SpawnedDaemon` is client-side but daemon-shaped),
filesystem primitives and IPC. After 3.2 moves the remote pieces out, group
the rest by seam (`fs`, `ipc`, `process`, `host_terminal`), and keep
`lib.rs` re-exports per group. The clipboard route decision is a separate
smell: `prefers_osc52_clipboard()` returns a `bool` that shepr-client threads
through five layers (`loop_config.rs`, `shell_runtime.rs`, `lib.rs`,
`clipboard_forwarding.rs`) to `shepr-termio/src/host_term/title.rs`, which
decides `!prefers_osc52 && write_clipboard(bytes)`. A `ClipboardRoute::{Osc52, Helpers(ClipboardSession)}`
from platform would carry the decision instead of the bit.

### 3.7 pty internals

- `launch.rs` hosts `c_string`, used by `command.rs`; `passwd_home` lives in
  the launch service. Both are launch-spec building and belong with
  `command.rs`.
- `SERVICE: OnceLock<Result<LaunchService, String>>` stringifies the bind
  error, so every later spawn reports `io::Error::other` with the kind lost.
- `PtyIoActorRunner` carries test seams in production fields
  (`poll_pty_and_wake: fn(..)`, `drain_wake_fd: fn(..)`, `resize_pty: Box<dyn FnMut>`,
  `poll_observer`). A small `ActorIo` trait would keep the production struct
  free of them.
- `ReaderExit`'s severity is its declaration order (`derive(Ord)` plus
  `raise_exit` using `max`); reordering variants changes behaviour. An
  explicit `severity()` would make it visible.

---

## 4. Types that resolve to primitives

### 4.1 `PaneId::raw()`

About forty production calls, almost all `pane = pane_id.raw()` in tracing
fields (`shepr-mux/src/pane/runtime.rs`, `teardown.rs`, `child_watcher.rs`,
`launch_status.rs`, `osc.rs`, `terminal.rs`, `detection_task.rs`,
`shepr-server/src/app/*`), thread names (`shepr-pty-{}` in `actor.rs`,
`shepr-pane-{}-teardown`), and snapshot keys (`persist/snapshot.rs`
`(workspace_index, id.raw())`, `LayoutSnapshot::Pane(id.raw())`,
`root_pane.raw()`). Offer `Display` and `tracing::Value` so logs take the id
itself, and `SavedPaneId::from(&PaneId)` for persistence. `PaneId::from_raw`
has no production caller (tests and shepr-test-fixtures only). The
`Serialize`/`Deserialize` derives on `PaneId` have no consumer found (the wire
uses `PublicPaneId`, snapshots use `u32`), yet the doc lists deserialization as
a legitimate way to mint one; drop the derives.

### 4.2 Public fields that bypass the constructor's invariant

- `HostGeometry { pub pane, pub exact }`: `new` enforces "exact only with a
  cell"; a struct literal or `geometry.exact = true` does not.
- `Rect { pub x, pub y, pub width, pub height }`: `new` clamps so the far edges
  fit `u16`; literals in `shepr-mux/src/workspace/geometry.rs`
  (`layout_rect`, `ratatui_rect`) and elsewhere skip it.
- `GridSize { pub cols: NonZeroU16, pub rows }`: callers write
  `.cols.get()` everywhere (`app/state.rs` `headless_rect`, protocol
  geometry); accessors `cols()`/`rows()` returning `u16` would do.
- `SplitBorder` has all-public fields including `ratio: f32` and the path.

### 4.3 Sentinels standing in for absence

- `PaneGeometry::cell_width()/cell_height()` and `HostGeometry` versions return
  `0` for "no cell"; protocol `TerminalGeometry::width()/height()` copy the
  pattern; all feed back into constructors that turn `0` into `None` again.
  `text_area_px().unwrap_or((0, 0))` in `fd.rs` is the one legitimate `0`
  (the winsize ABI).
- `ProcessIdentity::tag(token)` is always called with `0`, and the sweep
  accepts only `Some((owner, 0))` from `parse_tag`; the token field is dead
  format.
- `ssh_paths.rs` `bridge_endpoint_path_with_token(.., 0)` uses token `0` to
  measure the name length.
- `hostname().unwrap_or_default()` (empty string as unknown) in
  `shepr-server/src/app/mod.rs`.
- `ChildLiveness::new(0, None)` in mux, pid `0` as no child.
- `cwd_candidates.first().cloned().unwrap_or_default()` and
  `program: String::new()` in `launch_status.rs` / `runtime.rs`.
- Poll timeout `-1` (1.9).

### 4.4 String-typed closed sets

- `SHEPR_ENV == "1"` and `SHEPR_BUILD_PROFILE == marker()` (1.8).
- `owned_runtime.rs` name prefixes and content names as string literals
  matched in four functions (2.8).
- Logging `file_name: &str` with two constants (1.12).
- Tracing `outcome = "busy" | "acquired" | "released"` in `ipc.rs` is fine as
  log text, but `SocketStartupLock` reports outcome by hand in three places.

### 4.5 `SplitRatio::get()`

Callers unwrap and compare bits (`api/layouts.rs`
`current_ratio.to_bits() != next_ratio.to_bits()`), store the `f32` in
`SplitBorder`, and re-clamp. `SplitRatio` should implement `PartialEq` by
value (it already holds only finite values, so `Eq` is sound), be the type of
`SplitBorder.ratio`, and offer `nudged(delta)` for keyboard resizes.

### 4.6 Aliases posing as types

`ipc::LocalListener` and `ipc::LocalStream` (1.5); `remote_bridge.rs`
`BootClock = Arc<dyn Fn() -> io::Result<u64>>` with nanoseconds as raw `u64`
(an `Instant`-like boot-clock type would keep units straight).

---

## Lateral findings

1. **Remote bridge listener dies on a transient accept error.**
   `shepr-remote/src/remote/bridge.rs`: the accept thread breaks out of its
   loop on any error but `WouldBlock`, including `Interrupted`,
   `ConnectionAborted` and fd exhaustion (`EMFILE`). After that the bridge
   accepts nothing until it is rebuilt. Platform's
   `accept_failed_for_one_connection` exists for exactly this.
2. **The pty launch status listener dies on `EPERM` and any unlisted errno.**
   `shepr-pty/src/launch.rs` `Router::accept_loop` returns on errors outside
   its two lists. Its own doc says "a listener that stopped accepting would
   leave every later launch unsettled"; launches then settle `Unconfirmed`
   after `LAUNCH_STATUS_AFTER_EXIT` and lose their failure reasons. Platform
   classifies `EPERM` (a security module refusing one connection) as
   per-connection.
3. **Host grid clamped to the pane minimum** (2.5): `HostGeometry` built on
   `PaneGeometry` reports a 2-column host as 4 columns, contrary to
   `GridSize::clamped`'s documented intent.
4. **Dead Git parser in core** (3.3).
5. **`ChildExitReason` doc versus AGENTS.md**: AGENTS.md places checkpoint
   policy in shepr-server, the method is on a platform enum and consulted in
   mux too (2.12).
6. **Hot path syscall**: `shepr-mux/src/pane/osc.rs` calls
   `shepr_platform::hostname()` (gethostname) for every OSC 7 cwd report,
   inside PTY parsing.
7. **Rotating logger stats the log path on every record**
   (`logging.rs` `write_once` calls `fs::metadata` per write, under a shared
   flock). Correct for cross-process rotation, but every tracing line costs a
   stat and two flock calls; an inotify watch or a size counter with a
   periodic re-check would do.
8. **`shepr-core/src/shell.rs` module doc is stale** (3.4).
9. **`PaneChild::kill` uses `kill(pid)`** while a pidfd handle for the same
   child exists in mux. It is safe only because every call happens before the
   child watcher can reap; that ordering is not expressed in any type (3.1
   fixes it).
10. **`read_boot_log_tail` and `open_boot_log` refuse the same conditions with
    different `ErrorKind`s and order** (`InvalidInput` versus
    `PermissionDenied` for a non-regular file), so a caller branching on kind
    gets different answers for the same planted file.
11. **Two `Direction` enums and three rects**: core `Direction` versus protocol
    `PaneSurfaceSplitDirection`; core `Rect`, ratatui `Rect`, protocol
    `SurfaceRect`, with hand-written field copies in mux because no `From` can
    exist. Expected at a wire boundary, but the mux pair (`layout_rect`,
    `ratatui_rect`) is the kind of conversion that drifts if a clamp is added
    to one side.
