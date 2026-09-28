# Hygiene: channels and errors

This file collects the findings for two of the eight questions put to the nine
hygiene hunters who each read one scope of the workspace: question 3, "one
channel, one implementation" (output and diagnostics written directly where the
project has a channel for them, operator text assembled at the call site, and
the quality of what goes through the channel - levels, missing identifiers,
unreadable lines, and events that are logged nowhere), and question 4, "errors"
(failures swallowed where they should travel to a caller that can decide,
failures that travel but shed the context that made them actionable, and code
that aborts the process where a refusal was owed). Entries gather every scope
that reported the same thing. This is a working document produced by reading,
not by running anything; individual claims may be wrong, and a later fix pass is
expected to find phantoms among them.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGC-001 - No owner for "how shepr addresses an operator"

**Decision (partial):** the text rule named below is adopted: a `brokkr.toml`
textlint forbidding `print!` / `println!` / `eprint!` / `eprintln!` in the
library crates outside test code, leaving `src/` alone (B1 in
`notes/broadarrow-ports.md`). Every library-crate site this entry originally
named is gone: `shepr-platform/src/logging.rs`'s two stderr writes,
`shepr-client/src/lib.rs`'s stderr writes, `shepr-agent`'s
`print_outdated_update_notice`, `shepr-remote`'s thirteen `eprintln!`/`eprint!`
sites (the interactive prompt moved behind an `Operator` trait, see the former
HYGC-002), and `shepr-server/src/server/headless/bootstrap.rs`'s six lines
(`run_server` now returns a typed `RunServerError` and the caller decides how
to report it). The workspace clippy seal on the print macros is still not
adopted, so a CLI print site is not held by this rule, but the CLI's own
divergent channels are resolved (the former HYGC-004). Open: no function yet
owns operator output as a concept (the destination, the `shepr: ` prefix and
the capitalisation are still decided per remaining site), `writeln!(io::stderr(),
..)` is not a print macro so the rule does not catch it, and
`shepr-platform/src/ipc.rs::prepare_socket_path`'s `busy_message` closure still
has the platform layer's caller format operator text rather than the platform
layer staying silent on it. Also still open: `shepr-server/src/server/headless/bootstrap.rs`
and `crates/shepr-server/src/server/socket_paths.rs` spell "shepr server is
already running" independently (reduced from three copies to two).

Recorded absence, so it is not re-hunted: the protocol/config hunter verified by
grep that neither `crates/shepr-protocol/src` nor `crates/shepr-config/src`
contains `println!`, `eprintln!` or `print!`.

Enforcement named: one `operator_message(...)` (or a returned `Hint`/typed
value the binary renders) plus a `clippy.toml disallowed_methods` entry for
`eprintln!`/`io::stderr` outside that one module and `src/main.rs`. The wording
and phrasing of the messages themselves cannot be held mechanically.

## HYGC-003 - `io::stdout()` is acquired fresh at eighteen production sites; nothing owns host-terminal output

`shepr-client`: `terminal_setup.rs` (11), `terminal_geometry.rs` (3),
`state.rs` (2), `lib.rs` (5), `shell_runtime.rs` (3), plus `shepr-termio`'s
`host_term::title::write_clipboard_bytes` (1, and the only one that takes
`.lock()`). Every write is separately unbuffered and separately unlocked, and
the client writes frames, mode changes, window titles, queries and OSC 52
clipboard payloads through them from more than one thread - the clipboard write
happens on the main loop, terminal restore can happen from the panic hook and
from `Drop`. There is no `HostTerminalOut` type. The functions that take
`&mut impl io::Write` are the good half of this and are testable; their callers
all resolve to a fresh `io::stdout()`.

The termio/client hunter also noted that `shepr-termio` writes to `io::stdout()`
in exactly one place (`write_clipboard_bytes`) - the only place in the lower
crate that owns terminal output rather than taking a writer, and the only one
that locks. Both facts point at giving it a writer parameter.

Enforcement named: one owned writer handed to the client loop, plus a text rule
"no `io::stdout()` outside `host_out.rs`", the same shape as the existing
dependency allowlists.

## HYGC-005 - Operator-facing sentences assembled at the call site, with the phrasing drifting between sites

Reported from five scopes.

- `shepr-agent/src/integration/actions.rs` assembles operator text ad hoc in
  eighteen match arms: "installed claude integration hook to {}", "ensured
  claude settings at {}", "requires kimi code {KIMI_MIN_VERSION} or newer" -
  about 700 lines of near-identical formatting, with the phrasing drifting
  between arms ("installed X integration to", "installed X integration hook
  to", "ensured X settings at", "ensured X config at").
- `shepr-api::session::restart_after_update_guidance` / `..._for` own the local
  "stop the server to use this build" text; `src/cli/target.rs::restart_guidance`
  re-authors the whole paragraph for the `--machine` case in a single `format!`;
  `src/cli/server_not_running.rs` authors a third variant ("no shepr server is
  running at ...; run `X` to start or attach it"). All three answer "what should
  the operator type next", and the two-sentence structure ("Stopping exits pane
  processes") appears in two of them with different wording.
- `shepr-config`'s `ConfigDiagnostic::Display` is `f.write_str(self.message())`;
  the strings are built at about 40 call sites with ad-hoc prefixes ("config
  read error: {err}", "config parse error: {err}", "config provenance error:
  {error}", "config path error: {error}", "session selection error: {error}",
  "application paths could not be resolved", "state directory error: {error}").
  The prefix is the variant name restated in prose, at every site, by hand.
- `shepr-mux/src/persist/restore.rs` builds two operator-facing restore failure
  strings at the failure site ("Saved directory is unavailable. Restore the
  directory and restart this session." and "Could not start the saved shell:
  {e}. Fix the shell configuration and restart this session."), both landing in
  `TerminalState::restore_error: Option<String>` and rendered by
  `shepr-server/src/ui/panes.rs`. The second interpolates a raw `io::Error`
  `Display` into a sentence, and the crate has no other user-facing text. The
  test in `ui/panes.rs` invents a third wording ("Saved directory is
  unavailable. Restart to retry.") and asserts against its own invention, so it
  would not notice either production string changing.
- The paste-rejection sentence is spelled twice in `shepr-client`:
  `shell/input/input.rs::push_focused_paste` writes "Paste is {size} bytes;
  Shepr's limit is {} bytes", and `attach.rs::ForwardOutcome::notice` writes the
  same sentence under a comment reading "worded like the client shell's".
  Nothing keeps them in step; the threshold itself is correctly single-owned as
  `shepr_protocol::MAX_INPUT_PAYLOAD`.
- Launch-env validation exists twice with different messages: see HYGP-017.

Enforcement named: have each install return a
`Vec<(InstalledArtifact, PathBuf)>` and format once (agent); one guidance
builder taking a target descriptor (CLI - "not lintable"); move the prefix into
`Display` keyed on the variant (config), which also makes the variant
load-bearing; make `restore_error` a typed enum
(`RestoreFailure::DirectoryUnavailable { path }` / `ShellStartFailed { err }`)
and put the wording in whatever owns presentation - a `String` field invites
ad-hoc text, an enum does not; one `fn paste_rejected_notice(size, max)`; one
validator in `shepr-api` returning one `ApiError` (HYGP-017). The wording
itself is not mechanically holdable.

## HYGC-006 - Severity encoded as a text prefix instead of a level

`shepr-agent/src/integration/version.rs` builds three warnings by prepending the
constant `INSTALL_WARNING_PREFIX` (`"warning:"`) to a formatted string, which
the CLI then prints verbatim. The project has `tracing` and a CLI output path; a
prefix constant is a severity encoded in text.

Enforcement named: return a typed `InstallWarning` and let the printer decide
the prefix.

## HYGC-009 - The domain event catalogue, and one API level policy, live in the bottom platform crate

`shepr-platform/src/logging.rs` holds 25 functions named after concepts the
crate knows nothing about: `workspace_created`, `tab_renamed`, `pane_spawned`,
`session_saved`, `api_request_started`, `integration_action`. Each has one to
three call sites in `shepr-mux`, `shepr-api`, `shepr-server` or `shepr-agent`.
Adding a workspace event therefore requires editing the crate at the bottom of
the layering, and `api_request_started`'s level policy (info when
`mutates_ui && !routine`, debug otherwise) - an API-semantics decision - is
encoded three layers below the API. Related: `pane_exited(pane_id, status: &str)`
takes a pre-formatted status string built at the call site, so the format of the
most-read pane line is decided outside the module that owns the channel.

The channel itself (rotating writer, filter, mode, file names) genuinely belongs
in `shepr-platform`; the catalogue belongs beside each subject.

Enforcement named: a `brokkr.toml` text rule forbidding the identifiers
`workspace`/`tab`/`pane`/`api`/`session` in `shepr-platform` public item names,
or simply moving the functions so the existing dependency allowlists do the
work.

## HYGC-010 - The same class of event is logged at two or three different levels

Six scopes reported this.

- `shepr-platform`: `api_request_failed` is `warn!` while `pane_exit_failed` and
  `session_save_failed`/`session_clear_failed` are `error!`, though all four are
  "an operation the user asked for did not complete".
  `bind_private_local_listener`'s fallback to an insecure-window bind is `warn!`
  while `ProcessHandle::open`'s fallback to the racy start-time identity is
  `debug!` - the core/platform hunter's reading is that the second is the more
  consequential degradation and the quieter line. `shutdown.rs` logs the loss of
  the logind connection while a shutdown is pending (`err`, `retry_seconds`) at
  `debug!`, below the default `shepr=info` filter, in the case where the session
  may not be saved.
- `shepr-pty`/`shepr-mux`: actor read failure, poll failure and wake-drain
  failure are `debug!`; write failure is `warn!`. All of them end the pane's IO
  loop.
- `shepr-remote`: "a remote thing we depend on is unavailable" is logged at
  three levels - `debug!` for "SSH agent refresh unavailable"
  (`remote/ssh_agent.rs`), "could not cache SSH machine metadata" and "could not
  invalidate SSH machine metadata" (`remote/ssh_metadata.rs`), "saved SSH setup
  failed transiently" (`remote/saved.rs`); `warn!` for "SSH agent refresh
  unavailable" (`shepr-api/src/server.rs` - the *same message* as the debug one),
  "failed to check server socket" (`remote/local_server.rs`), "saved SSH
  endpoint bridge failed" (`remote/bridge.rs`); `error!` for "remote bridge
  failed to prepare client socket" (`remote/bridge.rs`).
- `shepr-server/src/server/headless/render.rs`: a closed client writer channel is
  logged at `debug!` at two sites and logged not at all at a third, all three
  being the same fact (this client's writer died mid-push) with three treatments
  in one file.
- `shepr-client`: a protocol violation is `ClientError::UnexpectedWelcome` from
  `handshake.rs` and a `debug!("received unexpected Welcome in main loop")` in
  `lib.rs` - same class of event (a peer sending a message it must not send at
  that point), two severities, the second below the default log level. The
  termio/client hunter noted the project's posture (same build both sides, no
  wire compatibility) argues for treating it as an error.

Enforcement named: none mechanical for level choice. A documented level policy in
the logging module, or in each crate's module comment, plus review is the only
lever; the remote hunter suggested a text rule could at least require every
`tracing::` call in that crate to carry the endpoint identifier (HYGC-012).

## HYGC-011 - One failure, two channels, chosen by which file or which function it landed in

**Decision (partial):** the third bullet is settled by the library-crate
print-macro textlint (HYGC-001): the `eprintln!` branch in `bridge.rs` is a
violation, leaving `tracing` as the one channel in the transport. Open: the
first two bullets.

- `shepr-mux/src/persist/writer.rs::finish_save_with_snapshot_plan` uses the
  project's owned channel for save outcomes
  (`shepr_platform::logging::session_save_failed` / `session_saved`) and raw
  `tracing` for the snapshot-preservation outcomes in the same call path
  (`tracing::warn!(event = "persist.snapshot", ...)`,
  `tracing::info!(event = "persist.backup", ...)`). A failed save is an operator
  event and a failed recovery copy is a log line, for no stated reason.
- `shepr-protocol`: `surface_reuse::message` logs a `warn` and returns `None`
  when `codec::encoded_len` fails; `surface_delta::message` wraps the identical
  failure as `SurfaceDeltaError::Encoding` and returns `Err`. Two policies for
  one class of event, chosen by which file the code landed in.
- `shepr-remote/src/remote/bridge.rs` picks between `tracing::warn!` and
  `eprintln!` for the same event depending on the `noninteractive` flag (see
  HYGC-001).

Enforcement named: add `session_snapshot_failed` / `session_snapshot_preserved`
to `shepr_platform::logging` and hold it with a text rule banning `tracing::` in
`shepr-mux/src/persist/`; make both `surface_*::message` functions return
`Result` and let one caller decide to log-and-fall-back; one
`fn writer_gone(client_id)` helper for the render branches so they cannot
disagree.

## HYGC-012 - Log lines that omit the identifiers someone would need to act

Reported from six scopes.

- `shepr-platform`: `session_restored(workspaces, outcome)` logs no session id
  and no path, while its siblings `session_saved`/`session_cleared` both log
  `path`. `shutdown.rs`'s "host shutdown requested; preserving session before
  pane termination" carries no generation number, though the generation is the
  whole correctness mechanism of that module and does appear in the `debug!`
  release line.
- `shepr-protocol/src/surface_reuse.rs::message`: `tracing::warn!(%error,
  "failed to size surface reuse")` omits the boot id, both revisions and the
  surface dimensions - everything an operator would need.
- `shepr-remote/src/remote/bridge.rs` logs "saved SSH endpoint bridge failed",
  "saved SSH endpoint listener failed", "rejected remote bridge socket peer with
  different credentials" and "remote bridge failed to prepare client socket" with
  no profile id, label, target or socket path, and the bridge thread owns all of
  them (`target` is captured in the closure). With several saved machines
  configured, these lines do not say which machine. `remote/saved.rs` logs
  "remembered remote Shepr did not connect; rediscovering" and "SSH discovery
  stopped; the next attempt resumes it" without the profile id or target, though
  `self.profile_id` and `self.target` are in hand.
- `shepr-mux/src/persist/io.rs`, twenty lines apart: `load()` logs
  `warn!(event = "persist.restore", subsystem = "persist", outcome =
  "read_error", path = %path.display(), err = %err, "failed to read session
  file")` while `load_history()` logs `warn!(err = %err, "failed to read session
  history file")` - no path, no event, no subsystem, no outcome. An operator
  cannot tell which session directory failed, which matters precisely because
  named sessions put the file somewhere non-obvious. The parse-error pair has the
  same asymmetry.
- `shepr-client`, in a client that serves several endpoints at once:
  `transport.rs` `warn!(err = %err, "server read error")` (no endpoint, no
  generation - which machine dropped?); `state.rs` `warn!(%error, "failed to
  present client frame")` (no endpoint, no frame or surface revision);
  `clipboard_forwarding.rs` `warn!("received invalid clipboard payload from
  server")` (no endpoint, no payload length - "from server" names no server);
  `lib.rs` `warn!(%error, "failed to present retained pane surface patch")` (no
  pane id, no endpoint); `lib.rs` `debug!("received unexpected Welcome in main
  loop")` (no endpoint). Two other sites in `lib.rs` do carry
  `endpoint = %...storage_key()` and `generation`, so the crate knows what a
  good line looks like.
- `shepr-agent`, for contrast, was reported as mostly fine on field content:
  `installed_integration_statuses` logs `integration` and `error`,
  `process_detection_mode` logs `variable` and `value`, `config_file.rs::Drop`
  logs the temp path. The agent hunter's finding there is coverage, not quality
  (HYGC-014, HYGC-016).

Enforcement named: partly holdable. A test can assert required fields per event
if the events become structs rather than free functions with positional
arguments; one helper taking `(path, err, outcome)` with required parameters for
the persist pair; and a `tracing::Span` built once at construction (per endpoint
in the client loop, per connector and per bridge in `shepr-remote`) attaches the
identifier structurally, which is the closest thing to enforcement available.

## HYGC-013 - Structured field names for the same thing differ across sites

`shepr-client` uses `%err` at some sites, `err = %err` at others, `%error` and
`error = %message` elsewhere; `lib.rs` alone uses three of the four. Fields keyed
`err` and `error` for the same thing mean no single query finds client failures.

Same shape in `shepr-mux/src/persist/writer.rs`: the `persist.snapshot` line
omits `subsystem = "persist"` while `persist.backup` includes it.

Enforcement named: a text rule on the field name, or funnelling failures through
one helper.

## HYGC-014 - Significant events with no log at all

Gathered from six scopes. The core/platform hunter's note that these are
judgement calls only review catches applies throughout.

`shepr-platform`:

- `SocketStartupLock` acquisition and release: nothing. Losing the race for it is
  turned into an `AddrInUse` error message but never logged.
- `bind_private_local_listener` succeeding via staging versus via the insecure
  in-place path: only the failure warns, so there is no record of which path a
  running server actually took.
- `remote_bridge.rs`'s `std::process::exit(1)` on idle expiry: the bridge dies
  with no line saying why - the one place a log would explain a mysterious
  disconnect.
- `SshAgentRegistry::publish` swapping the published agent symlink to
  `.unavailable`: no log. The user sees agent forwarding stop working silently.

`shepr-remote`:

- `SshStdioBridge::start` logs nothing: no line says a bridge came up, for which
  machine, at which socket, with which remote executable. `shepr-api`'s server
  logs `info!("api server listening")` and `shepr-server` logs
  `info!("client protocol socket listening")` for the equivalent event, so the
  pattern exists and this crate skips it.
- A successful reconnect after N failures logs nothing.
- `SshMetadataCache::store` on success logs nothing, so there is no record of
  which remote path we decided to remember - the exact fact you want when a
  machine starts failing.
- `EndpointCatalogWatch` reloading the catalog logs nothing (the failure warns).
- `ssh_config_include` silently drops an include when the path is absent or when
  OpenSSH reads its system config from somewhere else entirely; the hunter's
  suggested fix is a `debug` line naming which includes were emitted and which
  paths were skipped.

`shepr-agent`: nothing is logged when an override manifest is rejected (the
warning is stored on the `LoadedManifest` and only surfaces through
`explain`/`reload` summaries, so a bad override at server boot is only visible if
someone asks), and nothing is logged when `hook_registration_is_current` returns
false - which is the single most likely "why is my agent not reporting"
question.

`shepr-mux`: `PaneTerminal::seed_history_ansi` returns `()` and silently does
nothing when the core lock is poisoned, so restored scrollback is lost with no
line anywhere (see also HYGC-029).

`shepr-client`: `terminal_geometry.rs`'s three query wrappers
(`query_host_terminal_appearance`, `query_host_terminal_theme`,
`query_host_cell_size`) each do `let _ = write_...(io::stdout())`. When a query
fails to go out, the client waits for a reply that can never come and falls back
to `DEFAULT_CELL_WIDTH_PX`/`DEFAULT_CELL_HEIGHT_PX` (8x16) with no line logged;
pixel-accurate mouse and resize reporting silently degrade to a guess.
`restore_terminal_state` discards `host_modes.restore`'s error with `let _ =` and
only reports `ratatui::try_restore`'s, so a failure to pop the kitty keyboard
stack - the single most user-visible restore failure - is invisible.

`shepr-protocol`/`shepr-config`: the swallowed remote `ValidatedConfig` decode in
`shepr-client/src/shell/endpoints.rs` leaves a `None` as its only trace. (The
protocol/config hunter filed that swallow itself as a live defect.)
Separately, `config check` classifies every diagnostic into a `ConfigDiagnostic`
variant and then nothing ever reads the variant, so the classification never
reaches any channel (the dead-classification half of that belongs to the
dead-code sibling document).

## HYGC-015 - A total clipboard failure is logged nowhere, at either end

`shepr-platform/src/clipboard.rs::write_clipboard` returns `bool`. Every helper
failing produces no log line at any level, from either `shepr-platform` or the
`shepr-termio/src/host_term/title.rs` caller, which just falls through to OSC 52
and then `let _ = stdout.write_all(...)`. `read_clipboard_text` returns
`Option<String>` with the same silence. So "copy did nothing" is undiagnosable;
the two `tracing::warn!`s in the module are about reaping the wl-copy child, not
about the user's copy failing.

Related note kept from the same scope: `remote_bridge.rs` and
`remote_bridge_io.rs` both carry an explicit module-level rule ("input content
must stay out of logs and error messages here; byte counts and error kinds
only") and honour it. No equivalent note exists on the clipboard path, which
handles the same class of content (the user's selection, potentially a token
pasted between panes) and spawns it through an argv-visible helper process.
Nothing leaks today because no clipboard log line exists at all - which means
the first person to add one is the person who will leak it.

Also in scope: `shepr-vt`'s `MAX_CLIPBOARD_BYTES` silently drops OSC 52 payloads
over 192 KiB with no log.

Enforcement named: a capturing subscriber can assert a log line once the module
emits one; the shape change (`bool` -> `Result<(), ClipboardError>`) is
compiler-enforced at the call site. A module-level comment on `clipboard.rs`
matching the bridge's is the whole fix for the content-in-logs half.

## HYGC-016 - An unrecognised hook source or agent label silently downgrades a pane, and nothing is logged

Reported by the agent and mux hunters as the same fact from both ends.

When a hook reports a source shepr does not recognise, `AgentSource::parse`
yields `Custom`, and `shepr-mux`'s `terminal/state/hooks.rs::set_hook_authority_at`
takes a different branch: no authority, no session identity, no log. The same
for an `agent_label` that is not a canonical label
(`PersistedAgentSession::from_report` returns `None`). `AgentSource::from_pair`
returning `None` falls through silently at five mux sites
(`terminal/state/hooks.rs` x3, `sessions.rs`, `persist/snapshot.rs`), and
`full_lifecycle_hook_authority` / `session_identity_only_integration` are
consulted at ten further sites across `terminal/state/hooks.rs`, `lifecycle.rs`
and `sessions.rs`, each yielding `false` for an unrecognised pair.

These are exactly the failures an asset typo produces, and they are invisible:
the failure mode is "the sidebar became less accurate", which is the kind of
regression nobody bisects. A hook reporting an unrecognised source is
indistinguishable from no hook at all, in the logs and on screen.

Enforcement named: a `tracing::warn!` with pane id, source and label at each
site costs nothing on this path (once per session report, not per byte). The
complementary mechanical check - a test extracting every source and agent-label
literal from the shipped assets and asserting `AgentSource::from_pair` accepts
each one - belongs to the guards sibling document, where the asset-literal
finding is filed.

## HYGC-018 - Drops on the terminal-reply and dirty-patch paths with no counter and no log

`shepr-pty`/`shepr-mux`:

- Terminal replies that overflow the inbox (`push_terminal_response`, and a
  `let _ =` in `read_chunk`). Documented as deliberate, but a counter or a
  rate-limited log would let an operator see it happen.
- Resize replies refused by `reserve` in `replace_resize`.
- Replies from the timer before the actor handle is set (`timer_writer.get()`
  returns `None`).
- `enable_utf8_input` failures.
- `ghostty_collect_dirty_patch` takes a `fallback!($reason:literal)` and throws
  the reason away. The reasons are never logged or counted, so a fallback storm
  is invisible. The same macro pattern appears in `retained_surface.rs`.

Also recorded from the same scope: a poisoned core is logged twice under
different names - `"ghostty core lock poisoned in reader"` in mux
`pane/terminal/backend.rs`, and again by the actor as "terminal core is broken
... closing the pane". The ghostty backend no longer exists, so the first line
names a component that is not present (the naming half of that is in the stale
claims sibling document).

Enforcement named: make the `fallback!` macro record the reason; a counter or
rate-limited log for the inbox drops.

## HYGC-019 - `api_response_outcome` reparses every API response to pick one of three log strings

`shepr-api/src/server.rs`. Every API response is serialized, then parsed back
into a `serde_json::Value` purely to classify the log outcome, then discarded.
Two problems in one: a third spelling of the `"timeout"` error code, and a full
JSON parse per response on the request path. The classification is already known
upstream (the `ApiResult` that `encode_result` consumed).

Enforcement named: thread the `ApiResult`'s outcome to `finish_api_response`
instead of the encoded string; the literal and the reparse both disappear. The
api/cli hunter noted this is the one finding in that hunt where the structural
fix also removes measurable work from a hot path.

## HYGC-021 - A second, hand-rolled API client transport

`shepr-api/src/session.rs`'s `send_stop_request` / `send_stop_request_inner`
connect the socket, `serde_json::to_vec` the request, write `\n`,
`BufReader::read_line`, and deserialize - duplicating `ApiClient::connect`,
`write_request` and `read_json_line` from `shepr-api/src/client.rs`, with its own
send/recv timeout policy (`socket_timeout_until`, `MIN_SOCKET_TIMEOUT`) and its
own error tolerance (`stop_request_error_allows_wait`). The documented reason - a
build-mismatched server must still be stoppable - justifies skipping the build
check in `src/cli.rs::ensure_server_build_matches`; it does not justify a second
transport, since `send_request_unchecked` already exists for exactly this, and
`src/cli/server.rs::server_stop` uses it for the `--machine` path while the local
path goes through `session.rs`. The same command has two implementations
depending on the target.

Enforcement named: make `session.rs` use `ApiClient` with an explicit timeout and
drop `send_stop_request*`; the "one channel" claim then holds by construction.

## HYGC-022 - Errors that reach an operator naming no subject

Gathered from six scopes.

`shepr-platform`:

- `ssh_paths.rs` "SSH bridge socket path exceeds the Unix socket length limit" -
  no path, no length, no limit. The operator's only fix is to shorten
  `XDG_RUNTIME_DIR`, which the message never mentions. The control-socket site
  has the same shape.
- `UnsafeSshRuntimeDirectory`'s `Display` names the three requirements but not
  the directory that failed them, and `validate_shared_ssh_dir` has the path in
  hand.
- `ipc.rs` "lock must be a regular file owned by this user" - no path, no uid.
- `ssh_agent.rs` "SSH agent must be an absolute, user-owned socket" - no path,
  and it covers three distinct rejections (relative, self-referential, not a
  user-owned socket) with one string.

`shepr-config`/`shepr-protocol`:

- "invalid endpoint configuration: unexpected end of input: needed 1 bytes, 0
  remaining" - no host, no endpoint, no session.
- `FramingError::SurfaceDecode(String)` and `CodecError::Message(String)` flow to
  the client with no pane, boot id or revision attached.
- `ConfigDiagnostic::Validation("configuration values could not be resolved")`
  in `model.rs::into_validated` - the final refusal on the launch path, naming
  nothing at all. It fires when `resolution.values` is `None` while
  `diagnostics` is empty, i.e. exactly the case where nothing else explained the
  failure. `resolve_paths_from_env`'s `Err(vec!["application paths could not be
  resolved".to_string()])` is the same shape on the same path.

`shepr-server`: "pane not found" is spelled about 30 times in three wordings and
most omit the pane id - bare `"pane not found"` with no identifier at 6 sites in
`app/api/panes.rs` and 14 in `app/api/panes/geometry.rs`;
`format!("pane not found: {}", params.pane_id)` in `app/api/panes/copy.rs`;
`format!("agent target pane {target} not found")` and
`format!("agent target {target} not found")` in `app/agents.rs`; plus "source
pane not found" / "target pane {raw} not found" / "source tab not found" across
`app/api/panes/geometry.rs`. A `pane.resize` that fails tells you a pane was not
found but not which one, even though the handler holds the id it just failed to
resolve. The same for workspaces: `format!("workspace {id} not found")` at five
sites versus bare `"workspace not found"` at two.

`src/cli`: `target.rs::run_on_machine` returns
`usage_error("usage: shepr --machine <label-or-id> <command>")` when no command
was given - the message does not repeat the selector the user typed, so with
several shells open it names no subject. `resolve_machine`'s errors do name it
("unknown machine 'x'; use `shepr machine list`"), which is the standard to
match.

`shepr-client`: `set_handshake_recv_timeout(stream, timeout, _context: &'static
str)` never reads `_context`. Its one caller passes "failed to clear client
handshake read timeout", so the string is dead and the resulting
`ClientError::ConnectionFailed` carries the bare socket error with no indication
that it came from clearing the handshake timeout. The intent to attach context is
visible in the source and does nothing.

Enforcement named: partly. A typed error per module carrying the subject makes the
subject impossible to omit; a lint cannot.
`api_helpers::pane_not_found(&pane_id) -> ApiError` and siblings plus a text rule
banning the bare literals; either use or delete the `_context` parameter.

## HYGC-023 - Stringly-typed errors, and classification by `ErrorKind`, shed the category

- `shepr-remote`: every validation and IO failure in `machine/catalog.rs`,
  `target.rs`, `profile_id.rs` and `executable.rs` is a `String`.
  `src/cli/machine.rs` then wraps them with `std::io::Error::other(error)` and
  sometimes prefixes them ("remote prepared, but machine was not saved:
  {error}"). Nothing downstream can branch on why the catalog was rejected -
  compare `SshFailureDiagnostic`, which exists in the same crate and does this
  properly.
- `shepr-remote`: `print_saved_ssh_error_hint` reclassifies an error by
  re-parsing it - `is_remote_host_key_error` / `is_remote_auth_error` call
  `SshFailureDiagnostic::from_error`, which for an error that is not already a
  diagnostic falls back to classifying by `ErrorKind`. So a hint is chosen from a
  downgraded classification whenever the typed value did not survive the
  journey. The crate's own comment says "Text classification is reserved for the
  SSH process boundary", and this is the one place that re-derives it afterwards.
- `shepr-agent`: `load_manifest_uncached` collapses three distinct failures
  (unreadable or unparseable, id mismatch, compile failure) into one
  `warning: String` attached to the fallback manifest. The subject is in the
  text, but the caller cannot act on the category and `build_manifest_cache` has
  no way to refuse. Given the project's "any config problem fails the launch"
  posture, an override that does not compile arguably ought to fail
  `shepr config check` rather than warn at runtime.
- `shepr-agent`: `RemoteExecutable::needs_shell_quoting` exists only to produce a
  diagnostic for a path `parse` already rejected, re-running the same predicate
  from outside, so the rejection reason is computed twice by two functions that
  must agree. (Reported in the remote scope; same shape.)
- `shepr-mux`: `AppEvent::TabBarCommandFinished` carries
  `Result<Option<String>, String>` across an internal channel. The subject
  (which command, which segment's configured argv) is not in the error, only in
  the sibling `segment_index` field, and by the time an operator sees the text
  neither is attached. The rest of `AppEvent` is fully typed.
  `TerminalState::restore_error: Option<String>` is the same shape (see
  HYGC-005).
- `shepr-config`: `into_config`'s error type is `Result<Config, String>` for a
  case that cannot occur - a stringly-typed error on the config decode path,
  which is where the context loss in HYGC-022 comes from.

Enforcement named: typed errors - `CatalogError` mirroring
`SshFailureDiagnostic`'s design; `print_saved_ssh_error_hint` taking `&SshFailureDiagnostic` rather than
`&io::Error`, so the typed value must be threaded; a typed
`ManifestOverrideError` plus a `config check` path that loads overrides; a typed
error carrying the segment's configured command; structured error types instead
of `String` payloads in `shepr-config`.

## HYGC-024 - Multi-line error strings, and a renderer that mangles them

`shepr-remote/src/remote/local_server.rs::validate_running_server_compatibility`
builds a five-line multi-paragraph error inside an `io::Error` via `format!`
with embedded `\n\n`. `src/cli/machine.rs::status` then `escape_debug`s an
`io::Error` message into one line - so that carefully formatted multi-line text
reaches the operator as one very long line with `\n` escapes in it. The two
sites disagree about whether error strings may contain newlines, and one of them
mangles the other.

Enforcement named: errors carry structure (subject, cause, guidance as fields)
and the binary formats; then a test that no error message produced by the crate
contains `\n`.

## HYGC-026 - Failures discarded with `let _ =` at cleanup, permission and signal sites

`shepr-platform`, the list the core/platform hunter gave:
`ipc.rs` `remove_file`/`remove_dir` of the staging directory (a leaked 0700
directory per failure in the XDG runtime directory, never logged); `ipc.rs`
`remove_file` after a failed restrict; `ssh_agent.rs` `remove_file` of the
temporary symlink and of the published path on drop; `logging.rs`
`set_permissions` tightening a world-readable log - the one place where failing
quietly means the log stays readable by others; `clipboard.rs::kill_and_reap`
(both calls, by design); `terminal_setup`/`title.rs` `let _ =
stdout.write_all(...)`.

Elsewhere:

- `shepr-pty`'s `prepare_pty_child` ignores the return codes of `sigemptyset`
  and `sigprocmask`.
- `shepr-server/src/app/api/workspaces.rs`: `let _ =
  std::fs::remove_dir_all(&source_cwd)` - a recursive delete whose failure is
  discarded. The hunter noted it is fixture teardown in test code, but a
  recursive delete of a path derived from workspace state is the one operation
  you want logged either way, and suggested a text rule banning
  `remove_dir_all` outside `shepr-test-support`.
- `shepr-remote`: `SshMetadataCache::store` and `invalidate` both return `()` and
  swallow every failure at `debug`. A metadata cache that can never be written
  means every reconnect pays full discovery forever, reported only at `debug`.
  `store` is also called from `src/cli/machine.rs::add` *after* "Saved SSH
  machine {id}. Remote server is ready." is printed, so the user is told setup
  succeeded even when the cache seeding silently failed. Enforcement named:
  return `io::Result<()>` and let `add` decide whether to mention it; `#[must_use]`
  or the signature itself holds it.

Enforcement named for the class: `clippy::let_underscore_must_use` in the
workspace lint table would flag all of them and force an explicit
`if let Err(e) = ... { tracing::debug!(...) }` decision at each site. It is not
currently in the lint table, and adding it is a finding somebody could pay for
once. The termio/client hunter's dissenting view on the same lint: an allow-list
for it would be "too noisy to be worth it", and those sites are individual
fixes.

## HYGC-029 - Poisoned locks answered with success, a fabricated value, or a silent drop

- `shepr-vt`/`shepr-mux`: `synchronized_output_state` returns `(true, 0)` on a
  poisoned core - a made-up value rather than an error.
- `shepr-mux`: `GhosttyPaneTerminal::resize`, `scroll_up`, `scroll_down`,
  `scroll_reset` and `set_scroll_offset_from_bottom` all use
  `if let Ok(mut core) = lock_terminal_core(...)` and drop the operation on a
  poisoned lock. The doc comment on `GhosttyPaneTerminal::core` justifies this
  policy for *readers* ("readers answer empty or default values rather than
  error"); it says nothing about writers, and a dropped resize is not the same as
  a stale read. `PaneTerminal::seed_history_ansi` is the same shape and loses
  restored scrollback (HYGC-014). Enforcement named: a `#[must_use]` result, or a
  helper that logs once per pane per poisoning.
- `shepr-agent/src/detect/manifest.rs` unwraps poisoned locks into inner values
  at five sites (`unwrap_or_else(PoisonError::into_inner)`,
  `Err(poisoned) => poisoned.into_inner()`). The agent hunter's reading: that is
  the right call for a cache, and it is consistent, but the choice is re-made at
  each site; a small `fn read_cache(&self)` / `write_cache(&self)` pair would
  make it one decision.

Note on disagreement across scopes: `shepr-platform`'s log writer recovers a poisoned
mutex and records the gap, `shepr-vt::lock_terminal_core` treats
poisoning as terminal for the pane, `shepr-mux/src/render_signal.rs` continues on
poisoned state at eight sites, and `shepr-server/src/app/session.rs` both
recovers and refuses on the *same* mutex twenty lines apart. The hunters did not
agree on which is right; the per-call-site-policy half of this is filed in the
policy sibling document, and only the swallowing is here.

## HYGC-030 - Hook assets swallow everything, by design, and report nowhere

Every shipped hook asset under `shepr-agent/src/integration/assets/` swallows
failures deliberately: `except Exception: pass` in the Python bodies, `|| true`
on the heredocs, `2>/dev/null`, `client.recv` wrapped in a bare `try`. The reason
is sound and documented in the claude asset - a traceback would be shown to the
user by the agent. The consequence is that a hook that cannot reach the socket,
or that shepr rejects, is indistinguishable from no hook at all, forever.

A related case: the antigravity asset's `emit_and_exit` path prints a JSON
document to stdout on every early return (missing `SHEPR_ENV`, missing socket,
missing pane id), because Antigravity CLI expects a hook response. So the "not
running under shepr" case and the "running under shepr and reported" case produce
the same visible artifact, and a genuinely broken install cannot be
distinguished from a hook running outside shepr.

Mitigation named: have the receiving side own the observability - log
unrecognised or malformed reports (HYGC-016) - since the sending side
structurally cannot.

## HYGC-032 - Failures answered with a valid-looking sentinel instead of a refusal

- `shepr-server/src/app/ids.rs::public_workspace_id` answers an invalid index
  with an empty string. This is documented as deliberate ("a stale one is a
  caller bug, reported and answered with an empty id rather than a panic") and it
  warns, which is right - but the empty `String` then flows into public ids and
  API responses as a valid-looking value, and a `""` workspace id in a response
  is indistinguishable from a real one to the client. The sibling functions
  `public_tab_id` and `public_pane_id` return `Option<String>`.
- `src/cli/matches.rs::required` returns `String::default()` when the spec and
  the handler disagree. Deliberate and documented ("clap has already rejected
  argv without it"), and the non-panicking choice is right, but the failure mode
  is an empty-string pane id or agent target sent to the server, which surfaces
  as `pane_not_found: pane  not found` rather than as a CLI bug.
  `src/cli/pane.rs` compounds it with `selected_pane(..)?.unwrap_or_default()`.
- `shepr-server/src/server/headless.rs`'s client-shell boot id is
  `format!("{}-{}", std::process::id(), SystemTime::now()...as_nanos())` with
  `unwrap_or_default()`, so a clock before the epoch collapses every boot id to
  `pid-0`, silently defeating the stale-boot rejection it exists for.
- `shepr-vt`'s `synchronized_output_state` returning `(true, 0)` on a poisoned
  core (HYGC-029) is the same shape.

Enforcement named: return `Option<String>` like the siblings; extending
`every_cli_spec_root_has_typed_parser` to required arguments per subcommand would
make the `matches::required` fallback unreachable in fact as well as in intent;
`BootId::for_this_process()` in `shepr-protocol` with `From<String>` restricted
to deserialization.

## HYGC-033 - Aborts and panics on state a caller or operator can reach

**Decision (partial):** the `bridge_upload_cancellation_for_test` bullet goes
with piece 4 of the test-isolation work adopted from broadarrow (test-only code
leaves production crates' `test-support` features for dev-only crates, held by
`never-ships` dependency rules). Open: every other bullet.

- `shepr-mux/src/workspace.rs`: `impl Deref for Workspace` resolves to
  `self.tabs.get(self.active_tab).expect("workspace must have a tab when
  implicitly dereferenced")`. Every `Tab` method is silently available on
  `Workspace`, and the one-tab invariant is enforced by an `expect` in a `Deref`
  impl - the least visible possible place for an abort. `active_tab` is also
  `pub`, and `tabs_mut()` hands out `&mut [Tab]` to any crate, so a server-side
  caller can put `active_tab` out of range and the next `ws.panes` (which reads
  as a field access) aborts the server. Enforcement named: delete the
  `Deref`/`DerefMut` impls, make `active_tab` private, and require
  `active_tab()` / `active_tab_mut()`, which already exist and return `Option` -
  that turns an abort into a refusal and makes the invariant structural.
- `shepr-remote`: `&profile_id.as_str()[..16]` at two sites
  (`saved_bridge_path` and `SavedSshApiBridge::start`) panics if a `ProfileId` is
  ever shorter than 16 bytes. `ProfileId::parse` guarantees 32 today, so this is
  reachable only through the struct-literal path used in tests - but it is an
  invariant maintained by a constructor and relied on by slicing two modules
  away. Enforcement named: `ProfileId::short()`.
- `shepr-config/src/sidebar/rules.rs` has an `unreachable!("validated condition
  count")` in production code. It is genuinely unreachable (the `count != 1`
  check above it guarantees `gt` or `lt` is `Some`), but the guarantee is a
  counted boolean array five lines up rather than a type. Enforcement named:
  build the `Condition` in the same match that counts, so the impossible case is
  not representable.
- `shepr-agent`: `Agent::descriptor` indexes an array with `&AGENTS[self as
  usize]` in a `const fn`, relying on `#[repr(usize)]` and on declaration order
  matching the array. This is a panic on mis-ordering, not on caller input, and
  `descriptors_are_the_domain_source_for_agent_views` pins it - recorded only
  because the enforcement is a test rather than a type; a `match` or a build-time
  `const` assertion per variant would make the mis-ordering unrepresentable.
- `shepr-remote`: `bridge_upload_cancellation_for_test` is `pub` under
  `#[cfg(any(test, feature = "test-support"))]` and `expect()`s four times and
  `assert!`s once, so if anything ever enables `test-support` in a non-test build
  a panicking API is exported from a library that otherwise bans `unwrap`. (The
  feature-unification half of that is filed with the test findings.)

## HYGC-035 - Presentation and restore results discarded in the client

- `shepr-client/src/state.rs`: `pub fn present_frame(&mut self, ...) { let _ =
  self.try_present_frame(...); }`. `try_present_frame`'s doc says callers who
  care about presentation-sensitive work use the return value, and
  `present_frame` is the wrapper that makes not caring the default.
  `present_frozen_chrome` and `present_chrome` both go through it, so every
  chrome path (machine statuses, diagnostics, overlays, the machine list) drops
  presentation failure. `repaint_pending` is set inside on failure, so it is not
  lost entirely, but no caller learns. Enforcement named: partly - deleting
  `present_frame` and making callers handle the `bool` is a type-level fix;
  whether each caller then does the right thing is review.
- `shepr-client/src/terminal_setup.rs::restore_terminal_state` discards
  `host_modes.restore`'s error with `let _ =` and reports only
  `ratatui::try_restore`'s (also listed under HYGC-014).
