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

## HYGC-005 - Operator-facing sentences assembled at the call site, with the phrasing drifting between sites

Reported from five scopes. (The agent install phrasing is resolved: installs
return one outcome shape formatted in one place.)

- `shepr-api::session::restart_after_update_guidance` / `..._for` own the local
  "stop the server to use this build" text; `src/cli/target.rs::restart_guidance`
  re-authors the whole paragraph for the `--machine` case in a single `format!`;
  `src/cli/server_not_running.rs` authors a third variant ("no shepr server is
  running at ...; run `X` to start or attach it"). All three answer "what should
  the operator type next", and the two-sentence structure ("Stopping exits pane
  processes") appears in two of them with different wording.
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
- Launch-env validation exists twice with different messages: see HYGP-017.

Enforcement named: have each install return a
`Vec<(InstalledArtifact, PathBuf)>` and format once (agent); one guidance
builder taking a target descriptor (CLI - "not lintable"); make `restore_error` a typed enum
(`RestoreFailure::DirectoryUnavailable { path }` / `ShellStartFailed { err }`)
and put the wording in whatever owns presentation - a `String` field invites
ad-hoc text, an enum does not; one `fn paste_rejected_notice(size, max)`; one
validator in `shepr-api` returning one `ApiError` (HYGP-017). The wording
itself is not mechanically holdable.

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

Enforcement named: none mechanical for level choice. `shepr-platform`'s logging
module now states a level policy; the open bullets are the sites that do not
follow it yet. The remote hunter suggested a text rule could at least require every
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

- `shepr-mux/src/persist/io.rs`, twenty lines apart: `load()` logs
  `warn!(event = "persist.restore", subsystem = "persist", outcome =
  "read_error", path = %path.display(), err = %err, "failed to read session
  file")` while `load_history()` logs `warn!(err = %err, "failed to read session
  history file")` - no path, no event, no subsystem, no outcome. An operator
  cannot tell which session directory failed, which matters precisely because
  named sessions put the file somewhere non-obvious. The parse-error pair has the
  same asymmetry.
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

`shepr-client` now keys every failure field `error`. Open: the same audit across
the other crates, which was never done, and in `shepr-mux/src/persist/writer.rs`
the `persist.snapshot` line
omits `subsystem = "persist"` while `persist.backup` includes it.

Enforcement named: a text rule on the field name, or funnelling failures through
one helper.

## HYGC-014 - Significant events with no log at all

Gathered from six scopes. The core/platform hunter's note that these are
judgement calls only review catches applies throughout.

`shepr-client`: startup host queries now track replies only for writes that
succeeded. Open: a reactive query that fails to go out still leaves the input
reader's focus reply window open for its one-flush hold; the framer state is in
`shepr-termio/src/input/raw_input.rs`.

`shepr-protocol`/`shepr-config`: the swallowed remote `ValidatedConfig` decode in
`shepr-client/src/shell/endpoints.rs` leaves a `None` as its only trace. (The
protocol/config hunter filed that swallow itself as a live defect.)

## HYGC-037 - Once-only drop and failure reports that lose their subject or their total

- `shepr-pty`: the terminal-reply drop counter is reported at the first drop
  (count 1) and never again, so the total is not visible, for example at pane
  shutdown.
- `shepr-mux`: the terminal mutation-failure `error!` names no pane, because
  `PaneTerminal` does not know its id.
- `shepr-platform`: `api_request_failed` is now `error!`, including
  response-write IO failures from clients that disconnect abruptly; watch it for
  noise and split the disconnect case if it is.

## HYGC-038 - An internal client invariant breach is answered by reconnecting

`crates/shepr-client/src/lib.rs`: a `SurfaceUpdate` reaching presentation
undecoded can only mean a client bug, since the reader thread always decodes it
first; it now fails the endpoint, which reconnects and hides the bug. Log it at
error with the endpoint and generation so it is seen, whatever the recovery.

## HYGC-018 - Drops on the terminal-reply and dirty-patch paths with no counter and no log

Residue. The pty and mux drops are now counted and reported once per actor or
pane, and the mux dirty-patch fallback carries its reason. Open:
`crates/shepr-server/src/server/headless/retained_surface.rs` has its own
`fallback!` macro (seventeen call sites) that still throws the reason away, so
a fallback storm there is invisible. Carry the reason and log it once, off the
render loop's hot path.

## HYGC-022 - Errors that reach an operator naming no subject

Gathered from six scopes.

`shepr-platform`:

- `UnsafeSshRuntimeDirectory`'s `Display` names the three requirements but not
  the directory that failed them, and `validate_shared_ssh_dir` has the path in
  hand. Adding the path means changing the unit value that
  `shepr-remote/src/remote/saved.rs` downcasts and that its tests construct, so
  it needs one fixer holding both crates.

`shepr-config`/`shepr-protocol`:

- "invalid endpoint configuration: unexpected end of input: needed 1 bytes, 0
  remaining" - no host, no endpoint, no session.
- `FramingError::SurfaceDecode(String)` and `CodecError::Message(String)` flow to
  the client with no pane, boot id or revision attached.

`src/cli`: `target.rs::run_on_machine` returns
`usage_error("usage: shepr --machine <label-or-id> <command>")` when no command
was given - the message does not repeat the selector the user typed, so with
several shells open it names no subject. `resolve_machine`'s errors do name it
("unknown machine 'x'; use `shepr machine list`"), which is the standard to
match.

Enforcement named: partly. A typed error per module carrying the subject makes the
subject impossible to omit; a lint cannot.

## HYGC-023 - Stringly-typed errors, and classification by `ErrorKind`, shed the category

- `shepr-mux`: `AppEvent::TabBarCommandFinished` carries
  `Result<Option<String>, String>` across an internal channel. The subject
  (which command, which segment's configured argv) is not in the error, only in
  the sibling `segment_index` field, and by the time an operator sees the text
  neither is attached. The rest of `AppEvent` is fully typed.
  `TerminalState::restore_error: Option<String>` is the same shape (see
  HYGC-005).

Enforcement named: a typed error carrying the segment's configured command, and
a typed restore failure (HYGC-005). Also open: `shepr-client/src/lib.rs`'s
fallback catalog load formats the now-typed `CatalogError` into an `io::Error`,
losing the category on that route.

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

Enforcement named for the class: `clippy::let_underscore_must_use` in the
workspace lint table would flag all of them and force an explicit
`if let Err(e) = ... { tracing::debug!(...) }` decision at each site. It is not
currently in the lint table, and adding it is a finding somebody could pay for
once. The termio/client hunter's dissenting view on the same lint: an allow-list
for it would be "too noisy to be worth it", and those sites are individual
fixes.

## HYGC-029 - Poisoned locks answered with success, a fabricated value, or a silent drop


Note on disagreement across scopes: `shepr-platform`'s log writer recovers a poisoned
mutex and records the gap, `shepr-vt::lock_terminal_core` treats
poisoning as terminal for the pane, `shepr-mux/src/render_signal.rs` continues on
poisoned state at eight sites, and `shepr-server/src/app/session.rs` both
recovers and refuses on the *same* mutex twenty lines apart. The hunters did not
agree on which is right; the per-call-site-policy half of this is filed in the
policy sibling document, and only the swallowing is here.

## HYGC-032 - Failures answered with a valid-looking sentinel instead of a refusal


Enforcement named: return `Option<String>` like the siblings;
`BootId::for_this_process()` in `shepr-protocol` with `From<String>` restricted
to deserialization.

## HYGC-033 - Aborts and panics on state a caller or operator can reach

**Decision (partial):** the `bridge_upload_cancellation_for_test` bullet goes
with piece 4 of the test-isolation work adopted from broadarrow (test-only code
leaves production crates' `test-support` features for dev-only crates, held by
`never-ships` dependency rules). Open: every other bullet.

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
