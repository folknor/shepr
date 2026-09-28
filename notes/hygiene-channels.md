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

## HYGC-001 - No owner for "how shepr addresses an operator", and library crates write to stderr directly

Six scopes reported the same shape: there is no function that owns operator
output, so the destination (stderr vs the log vs a returned value), the `shepr: `
prefix and the capitalisation are re-decided per site, and several of the sites
are in library crates below the binary that owns operator output
(`src/cli/error.rs::CliError::print`).

Sites reported:

- `shepr-platform/src/logging.rs`: `"shepr: could not initialize file logging:
  {error}"` written straight to stderr, and `"shepr: file logging resumed; log
  lines were lost after an I/O error: {error}"` written into the log stream
  itself. The core/platform hunter noted these two contradict each other:
  `write_with_recovery`'s comment says an outage is deliberately *not* sent to
  stderr "which is the client's TUI terminal", yet `init_file_logging`'s own
  failure - the strictly more serious event, same module, same process - goes to
  exactly that stderr.
- `shepr-client/src/lib.rs`: three `"shepr: ..."` writes to stderr (see also
  HYGC-003 and HYGC-013).
- `shepr-agent/src/integration/registry.rs::print_outdated_update_notice`:
  `eprintln!` from inside a library crate, and it re-formats operator text with
  a string hack - it strips backticks out of `integration_update_instructions`
  by replacing every backtick character with the empty string, because that
  function was written for a different
  medium. One message, two renderings, one produced by deleting characters from
  the other.
- `shepr-remote`: 13 `eprintln!`/`eprint!` sites across `lib.rs` (3),
  `remote/bridge.rs` (2) and `remote/server_lifecycle.rs` (8). `bridge.rs`
  branches on `noninteractive` to pick between `tracing::warn!` and `eprintln!`
  for the same event ("saved SSH endpoint bridge failed" versus `"shepr: remote
  bridge failed: {err}"`) - a channel decision made inside the transport.
- `shepr-server/src/server/headless/bootstrap.rs`: six `eprintln!` lines the
  logging channel never sees, two of which are the only record of a fatal
  condition (`error: shepr server is already running` plus a socket path, then
  `std::process::exit(1)`), so a server started from a unit file or a spawning
  client leaves no trace in the server log of why it refused. `print_ready_message`
  is the defensible case (the user is looking at the terminal) but duplicates
  the `info!("shepr server started")` line with different fields, and assembles
  the logs path by joining `SERVER_LOG_FILE` at the call site rather than
  through whoever owns the log location.
- `src/cli/machine.rs`, `src/cli/server.rs::server_stop` (local path),
  `src/cli/integration.rs::report_outcome`, `src/cli/agent.rs::agent_attach`:
  see HYGC-004, which is the same sites read as an error-channel finding.
- `shepr-client`'s three stderr writes use two macros (`eprintln!` and
  `writeln!(io::stderr(), ...)`), spell the `shepr: ` prefix three times, and
  one path checks its result while two discard it.
- `shepr-mux/src/pane/runtime.rs` has the crate's only `eprintln!`, in a test,
  to say a test was skipped (see the tests sibling document).
- `shepr-platform/src/ipc.rs::prepare_socket_path` takes a `busy_message`
  closure so the *caller* supplies the operator text for an `AddrInUse`. The
  core/platform hunter read this as the right idea at the wrong seam: the
  platform layer should not be formatting operator text at all, so the closure
  is a symptom of the missing operator-message channel rather than a fix for it.

Recorded absence, so it is not re-hunted: the protocol/config hunter verified by
grep that neither `crates/shepr-protocol/src` nor `crates/shepr-config/src`
contains `println!`, `eprintln!` or `print!`.

Enforcement named: one `operator_message(...)` (or a returned `Hint`/typed
value the binary renders) plus a `brokkr.toml` text rule forbidding
`println!`/`eprintln!`/`print!` in `crates/**` outside test modules, and/or a
`clippy.toml disallowed_methods` entry for `eprintln!`/`io::stderr` outside that
one module and `src/main.rs`. The remote hunter called this "the single
highest-value rule this hunt found: it is trivially checkable, and the crate
violates it 13 times". The wording and phrasing of the messages themselves
cannot be held mechanically.

## HYGC-002 - An interactive terminal prompt lives in a library crate

`shepr-remote/src/remote/server_lifecycle.rs::confirm_remote_server_stop` checks
`io::stdin().is_terminal()`, prints five lines to stderr, prints a `[y/N]`
prompt, flushes, and reads from `stdin().lock()` - from a crate that also serves
a headless client's background reconnect worker. It is only reachable from
`run_remote`/`prepare_saved_ssh` today, but nothing structural keeps a
supervisor thread out of it.

Two further facts in the same function:

- The `[y/N]` default is stated twice and the two copies can disagree: the
  prompt text says `[y/N]` and the call is `read_remote_confirmation(&mut
  stdin, false)`. The prompt and the `default` argument are independent, and
  `default: true` is never passed.
- It is a six-line `eprintln!` wall including one 130-character sentence and one
  110-character sentence.

Enforcement named: `ensure_remote_server_ready` takes an `&mut dyn Confirm`, or
returns a `RemoteServerNeedsRestart` value the caller decides on -
`read_remote_confirmation` already takes a `&mut impl BufRead`, so the seam is
half-built and then bypassed by its caller. A `Confirmation { default }` type
that renders its own prompt makes the two-copies-of-the-default case
unrepresentable. The no-print rule from HYGC-001 catches the symptom; a
dependency rule cannot express "no stdin".

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

## HYGC-004 - CLI failures travel through two channels, chosen per site

`CliError` (printed as JSON on stderr with an exit code, `src/cli/error.rs`) is
the owner. Roughly a dozen sites print with `eprintln!` and return an exit code
instead: `src/cli/machine.rs` (eight sites, plain `error: {error}` text),
`src/cli/server.rs::server_stop` (local path), `src/cli/integration.rs::report_outcome`,
`src/cli/agent.rs::agent_attach`. A script that parses stderr as JSON - which is
what the API-backed commands train it to do - gets plain prose from these.
Within `machine.rs` alone the prefix is inconsistent: `eprintln!("{error}")` in
some arms, `eprintln!("error: {error}")` in others, and
`eprintln!("error: {error}; machine was not saved")` in a third.

`machine.rs` is also the only command family in the CLI that spells exit code 2
by hand, returning bare `Ok(2)` at five sites to mean "usage error" instead of
`CliError::Usage`, which is the type that owns exit code 2.

Enforcement named: these paths already run in `CliResult<i32>` functions, so
returning `CliError` is mechanical; then a text rule banning `eprintln!` outside
`error.rs`, and either a rule forbidding integer literals as `Ok(..)` exit codes
outside `error.rs` or changing `run_machine_command`'s return type to
`CliResult<()>` so the code cannot be spelled at all.

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
- An untrusted remote version string is rendered by two policies, and the
  unfiltered one reaches the terminal: see BUG-038.

Enforcement named: have each install return a
`Vec<(InstalledArtifact, PathBuf)>` and format once (agent); one guidance
builder taking a target descriptor (CLI - "not lintable"); move the prefix into
`Display` keyed on the variant (config), which also makes the variant
load-bearing; make `restore_error` a typed enum
(`RestoreFailure::DirectoryUnavailable { path }` / `ShellStartFailed { err }`)
and put the wording in whatever owns presentation - a `String` field invites
ad-hoc text, an enum does not; one `fn paste_rejected_notice(size, max)`; one
validator in `shepr-api` returning one `ApiError` (HYGP-017); one
`printable_remote_value` used everywhere (BUG-038). The wording itself is not
mechanically holdable.

## HYGC-006 - Severity encoded as a text prefix instead of a level

`shepr-agent/src/integration/version.rs` builds three warnings by prepending the
constant `INSTALL_WARNING_PREFIX` (`"warning:"`) to a formatted string, which
the CLI then prints verbatim. The project has `tracing` and a CLI output path; a
prefix constant is a severity encoded in text.

Enforcement named: return a typed `InstallWarning` and let the printer decide
the prefix.

## HYGC-007 - The CLI process installs no tracing subscriber, so everything it logs is discarded

Merged into BUG-025 (`notes/bugs.md`), which carries the full finding.

## HYGC-008 - `SHEPR_LOG` degrades silently on a bad filter, and a second subscriber install is discarded

Merged into BUG-044 (`notes/bugs.md`), which carries the full finding.

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

## HYGC-017 - A persistent tab-bar status failure re-logs every interval, at warn, carrying the user's command line

Merged into BUG-060 (`notes/bugs.md`), which carries the full finding.

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

## HYGC-020 - Three response-encoding implementations, with different behaviour on encoder failure

Merged into BUG-029 (`notes/bugs.md`), which carries the full finding.

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
- `shepr-remote`: `is_launch_fatal_setup_error` classifies launch-versus-retry
  by `io::ErrorKind` rather than by cause: see BUG-039.
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
`SshFailureDiagnostic`'s design; the `DeterministicSetupError` marker in
BUG-039; `print_saved_ssh_error_hint` taking `&SshFailureDiagnostic` rather than
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

## HYGC-025 - `std::process::exit` from library crates, and exit codes spelled outside their owner

`src/cli/error.rs::exit_code()` is the owner of shepr's exit statuses. Bypassing
it:

- `shepr-platform/src/remote_bridge.rs`: a watchdog thread calls
  `std::process::exit(1)` when the relay has been idle for 60 seconds. The
  comment justifies why returning is insufficient, and for the dedicated bridge
  process that reasoning holds - but the decision now lives in a crate that
  seven other crates link, and nothing prevents a second caller of
  `forward_remote_bridge_stdio(_, true)` from inheriting a hard exit it did not
  ask for. The refusal that was owed is a signal back to the caller plus the
  caller's own `exit`. The `1` is then re-spelled as an assertion in
  `remote_bridge_tests.rs` (`assert_eq!(bridge.wait().code(), Some(1))`).
- `shepr-client/src/lib.rs`: `std::process::exit(1)` from library code with a
  literal `1`, skipping every remaining destructor in the process. The client is
  the crate with the most to lose from skipped destructors (terminal restore),
  and it works today only because restore is explicitly run a few lines earlier.
- `shepr-server/src/server/headless/bootstrap.rs`: two `std::process::exit(1)`
  calls on `AddrInUse`, from inside `run_server`, which returns
  `io::Result<()>` and is called from `main`. "Another server is already
  running" is exactly the condition a caller should be allowed to handle (the
  spawning client wants to attach to the existing server, not die). `exit(1)`
  also skips `logging::shutdown("server")`, so the last log lines may not be
  flushed, and the `1` is a bare literal with no named owner.
- `src/main.rs`: eight sites - `usage_exit` (invalid UTF-8 argv, bad
  `--session`, bad `--remote`, `--remote` with a subcommand),
  `exit_if_nested_disabled`, `load_validated_config_or_exit` (twice), the
  remote-launch failure and the autodetect failure. `main` returns
  `io::Result<()>` and `finish_cli` exists to turn a `CliResult` into an exit
  code, so the machinery for "refuse with a code" is already there and these
  sites do not use it. Consequence: none of these paths is reachable from a
  test, which is why `should_block_nested_for_env` was extracted while the
  config-error and remote-launch paths have no tests at all. `main.rs` also
  exits on `run_remote` failure and during remote launch after printing,
  bypassing `CliError::exit_code`'s 1/2 distinction.

Recorded absences from the same question: `shepr-protocol` and `shepr-config`
abort only at the intended boundary (`main.rs` after printing diagnostics) and
have no `panic!`/`unwrap()`/`expect()` in production code; `shepr-remote` itself
has no `process::exit`/`panic!` on operator input - its callers do it on its
behalf.

Enforcement named: `shepr-platform` returns a `BridgeOutcome::IdleExpired` and
lets `shepr-remote`/`src/main.rs` exit; `shepr-client` returns the failure to
`main.rs`; bootstrap returns a typed error variant to `main`, which already owns
process exit; `main` builds a `CliResult` and exits in exactly one place. Then a
`clippy.toml disallowed_methods` entry or `brokkr.toml` text rule for
`std::process::exit` outside `src/main.rs` holds all of it, and every exit code
moves into `exit_code` as a named constant.

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
- `shepr-server/src/server/headless.rs`: `let _ = ctrlc::set_handler(...)` on the
  server's only signal path. If installation fails (a handler already
  registered, which `ctrlc` reports as `MultipleHandlers`), the server runs with
  no SIGINT/SIGTERM/SIGHUP handling, so `systemctl stop`, a logout or a Ctrl-C
  kills it without the shutdown sequence that saves the session; nothing is
  logged and nothing is returned. The server hunter called this a live swallowed
  failure rather than a style point. Enforcement named: return `io::Result` from
  `ctrlc_handler` and propagate to `run_server`, which already returns
  `io::Result<()>`.
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

## HYGC-027 - Results dropped on the server's shutdown and commit paths

- `shepr-server/src/app/session.rs::retire_session_writer` does
  `let _ = thread.join();`, dropping both the thread's panic (`Err`) and the
  `io::Result<()>` the save job returned. On the one path where losing a save
  matters most - server shutdown - a failed write is invisible, while every
  other save result goes through `record_session_save_result`, which logs and
  retries. Enforcement named: `join()` into `record_session_save_result`; the
  `Result` then cannot be dropped without `#[must_use]` firing.
- `shepr-server/src/server/headless/lifecycle.rs::cleanup_sockets` returns
  `io::Result` but logs a removal failure and returns `Ok(())` unconditionally;
  `release_sockets_after_save` propagates that always-`Ok`; `Drop for
  HeadlessServer` then writes `let _ = self.cleanup_sockets();`, discarding a
  value that is provably `Ok`. The signature claims a failure can travel and
  nothing can. Enforcement named: change the signature to `()`, at which point
  the `let _ =` at the Drop site disappears - or make the warn an `Err`.
- `shepr-server/src/app/events.rs`: `let _ = self.state.commit_pane_removal(&plan);`
  in the `AppEvent::PaneDied` handler. If the plan no longer matches state (the
  pane was closed by an API call between plan and commit), the event is consumed
  with nothing recorded. Enforcement named: `#[must_use]` on the return,
  handled explicitly.

## HYGC-028 - The rotating log writer swallows write errors by contract, and its poisoned-mutex branch never recovers

Merged into BUG-043 (`notes/bugs.md`), which carries the full finding.

## HYGC-029 - Poisoned locks answered with success, a fabricated value, or a silent drop

- `shepr-api/src/event_hub.rs::EventHub::push`: `let Ok(mut state) =
  self.inner.lock() else { return; }`. The read path was deliberately hardened -
  `events_after_checked` returns `EventHistoryError::Unavailable` and has a test
  for the poisoned case - but the write path just returns. After a poison,
  subscribers see a silent, permanent gap rather than the `server_unavailable`
  they were designed to receive, because `current_sequence` also stops
  advancing, so `events_after_checked` sees a consistent-looking empty tail
  rather than `Lost`. Enforcement named: make `push` infallible by construction
  (a lock-free ring, or `PoisonError::into_inner`, both defensible given the
  state is a plain `Vec` of values), or report; a test can cover it the same way
  `checked_history_reports_unavailable_instead_of_empty_after_poison` covers the
  read side.
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

Note on disagreement across scopes: `shepr-platform`'s writer treats a poisoned
mutex as a silent success (HYGC-028), `shepr-vt::lock_terminal_core` treats
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

## HYGC-031 - Git failure collapses to `None` everywhere, so nothing ever reaches an operator

Merged into BUG-065 (`notes/bugs.md`), which carries the full finding.

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

## HYGC-034 - An unbounded write with no send timeout

Merged into BUG-027 (`notes/bugs.md`), which carries the full finding.

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
