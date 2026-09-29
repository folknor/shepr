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
layer staying silent on it. (The server's "already running" text now has one
owner, `RunServerError`'s `Display`.)

Recorded absence, so it is not re-hunted: the protocol/config hunter verified by
grep that neither `crates/shepr-protocol/src` nor `crates/shepr-config/src`
contains `println!`, `eprintln!` or `print!`.

Enforcement named: one `operator_message(...)` (or a returned `Hint`/typed
value the binary renders) plus a `clippy.toml disallowed_methods` entry for
`eprintln!`/`io::stderr` outside that one module and `src/main.rs`. The wording
and phrasing of the messages themselves cannot be held mechanically.

## HYGC-053 - Two log facts that can be wrong after a disconnect or a failed probe

- `shepr-api/src/server.rs::finish_api_response` writes through
  `write_text_line_allow_disconnect`, which turns a disconnect into `Ok`, and
  then logs `api.request.complete` with the response's own outcome (say "ok")
  for a response that was never delivered. Match
  `is_connection_closed_error` there and log a "client_disconnected" outcome.
- `shepr-mux/src/pane/launch.rs`: `SHEPR_BIN_PATH` is allowed through and only
  overwritten when `launch_executable()` succeeds, so if that fails a nested
  server's pane keeps the outer server's stale value. Scrubbing it instead
  would drop it in that case; the success case writes after the scrub and is
  unaffected.

## HYGC-054 - Handshake failure text may repeat the machine name

Client handshake errors now read "endpoint local (session X) handshake failed:
...". Nobody checked whether the endpoint status UI already prefixes the
machine name, so an SSH endpoint may show it twice. Look at the rendered
status line for a failing remote handshake and trim whichever side repeats.

## HYGC-013 - Structured field names for the same thing differ across sites

`shepr-client` keys every failure field `error`. Every other crate mixes `err`
and `error` for the same thing in `tracing` fields: a count across the Rust
sources found both spellings in `shepr-agent`, `shepr-api`, `shepr-mux`,
`shepr-platform`, `shepr-remote` and `shepr-server`, and only `err` in
`shepr-pty`. Pick one name and convert the other crates.

Enforcement named: a text rule on the field name, or funnelling failures through
one helper.

## HYGC-022 - Errors that reach an operator naming no subject

Client-side decode, reader, config and handshake errors now carry their
subject. Open: `shepr-server/src/server/client_transport.rs` framing read
failures log the `client_id` but no session. The worker has no validated
session (reading `SHEPR_SESSION` would be wrong when `--session` chose it), so
the acceptor must pass it in.

Enforcement named: partly. A typed error per module carrying the subject makes the
subject impossible to omit; a lint cannot.

## HYGC-050 - Pane restore failure wording is owned by the UI module and reached from the app layer

The pane restore failure is now a typed `RestoreFailure`, worded by
`shepr-server/src/ui/panes.rs::restore_failure_message`. Two costs came with
that: `app/creation.rs` calls `crate::ui::restore_failure_message` to fill the
API response, so the app layer reaches into the UI module for API text; and
`render_panes` builds that `String` on every render for each pane with a
restore failure, where it used to borrow a `&str`. Give the wording a neutral
home both can use (a `Display` on the failure, or a presentation module outside
`ui`), and let the render path borrow or cache it.
