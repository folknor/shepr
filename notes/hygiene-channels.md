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

## HYGC-013 - Structured field names for the same thing differ across sites

`shepr-client` keys every failure field `error`. Every other crate mixes `err`
and `error` for the same thing in `tracing` fields: a count across the Rust
sources found both spellings in `shepr-agent`, `shepr-api`, `shepr-mux`,
`shepr-platform`, `shepr-remote` and `shepr-server`, and only `err` in
`shepr-pty`. Pick one name and convert the other crates.

Enforcement named: a text rule on the field name, or funnelling failures through
one helper.

## HYGC-037 - Once-only drop and failure reports that lose their subject or their total

- `shepr-mux`: the terminal mutation-failure `error!` names no pane, because
  `PaneTerminal` does not know its id.
- `shepr-platform`: `api_request_failed` is now `error!`, including
  response-write IO failures from clients that disconnect abruptly; watch it for
  noise and split the disconnect case if it is.

## HYGC-022 - Errors that reach an operator naming no subject

`shepr-config`/`shepr-protocol`:

- "invalid endpoint configuration: unexpected end of input: needed 1 bytes, 0
  remaining" - no host, no endpoint, no session.
- `FramingError::SurfaceDecode(String)` and `CodecError::Message(String)` flow to
  the client with no pane, boot id or revision attached.

Enforcement named: partly. A typed error per module carrying the subject makes the
subject impossible to omit; a lint cannot.

## HYGC-026 - Failures discarded with `let _ =` at cleanup, permission and signal sites

The `shepr-platform` and `shepr-pty` sites are all checked, logged or returned
now. Open:

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

## HYGC-051 - A corrupt client preferences file is dropped silently

`shepr-client/src/shell/overlays/preferences.rs::load` discards an unreadable or
unparseable preferences file with `.ok()` and falls back to defaults with no log
line. Log a warning for any failure other than not-found.

## HYGC-050 - Pane restore failure wording is owned by the UI module and reached from the app layer

The pane restore failure is now a typed `RestoreFailure`, worded by
`shepr-server/src/ui/panes.rs::restore_failure_message`. Two costs came with
that: `app/creation.rs` calls `crate::ui::restore_failure_message` to fill the
API response, so the app layer reaches into the UI module for API text; and
`render_panes` builds that `String` on every render for each pane with a
restore failure, where it used to borrow a `&str`. Give the wording a neutral
home both can use (a `Display` on the failure, or a presentation module outside
`ui`), and let the render path borrow or cache it.
