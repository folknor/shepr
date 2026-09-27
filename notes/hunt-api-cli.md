# Hygiene hunt: `crates/shepr-api` + root binary `src/` (JSON API and the CLI that drives it)

Scope read in full: `crates/shepr-api/src/{lib,error,status,client,event_hub,server,subscriptions,session,wait,schema,schema/*}.rs`
and `src/{main,autodetect,netside_tests,test_support}.rs` plus the whole `src/cli/` tree.

Every finding below is small. The count is the point. For each one I say what could hold the
fixed version mechanically, since this repo already pays for that kind of rule in `brokkr.toml`.

Two structural claims were in scope. Both are *mostly* true and both leak in named places:

- **"Every CLI subcommand acting on a running server goes through the JSON API; local-state
  commands run in the CLI process and cannot be sent with `--machine`."** The `--machine` gate is
  real and tested (`validate_machine_command` + `machine_commands_reject_real_local_commands`).
  The API-only claim leaks: `session stop`, `session delete` and local `server stop` reach a
  *running server* without going through `ApiClient` at all - `shepr-api/src/session.rs` hand-rolls
  a second client transport (F19). `status` (overview) and `status client` also act on a running
  server through the API while declaring `is_api_command() == false` (F21).
- **"API error codes, response shapes and operator guidance each have one owner."** False in
  several places. Nine wire error codes are minted as string literals outside `ApiErrorCode`
  (F1), two codes that *are* in the enum are spelled as literals at the site that emits them
  (F2), the response-encoding path has three independent implementations with different failure
  behaviour (F13), and operator guidance is assembled at three sites (F16).

---

## 1. One value, one owner

**F1. Nine API error codes are invented at CLI call sites and never enter `ApiErrorCode`.**
`ApiErrorCode` in `crates/shepr-api/src/error.rs` is a macro-generated single owner of variant and
wire spelling - a good rule somebody already paid for. The CLI bypasses it entirely by hand-building
`serde_json::json!` error bodies: `agent_explain_file_read_failed`, `agent_start_failed`,
`agent_start_transport_failed`, `agent_kind_mismatch`, `agent_name_not_found` (`src/cli/agent.rs`),
`build_mismatch` (`src/cli.rs`), `server_not_running` (`src/cli/server_not_running.rs`), and
`invalid_session_name` / `session_stop_failed` / `session_delete_failed` (`src/cli/error.rs`).
A consumer of `shepr <cmd>` JSON therefore sees codes that do not exist in the schema, and
`ApiError::from_body` classifies every one of them as `External(..)`.
*Diverged already?* Yes in one case: `src/cli/agent.rs` emits the literal `"timeout"` and
`agent_not_ready`, which duplicate `ApiErrorCode::Timeout` and `ApiErrorCode::AgentNotReady` with a
second spelling site each.
*Enforceable:* yes, and cheaply. Give the CLI-minted codes their own variants (or a
`CliErrorCode` enum) and make `cli_agent_error` / `ErrorBody` construction take the enum rather
than `&str`; a type then makes the bad spelling unrepresentable. A text rule ("no string literal
assigned to an `ErrorBody.code` field outside `error.rs`") is a weaker but workable fallback.

**F2. Two in-enum codes are re-spelled as literals inside `shepr-api` itself.**
`crates/shepr-api/src/server.rs` (SSH-agent registration) passes `"invalid_ssh_agent"` /
`"ssh_agent_unavailable"` to `error_response_json`, whose signature is
`code: impl Into<ApiErrorCode>` - so a typo silently becomes `External("invald_ssh_agent")` with
no compile error and no log. `ApiErrorCode::InvalidSshAgent` and `SshAgentUnavailable` exist.
*Enforceable:* yes - change `error_response_json` to take `ApiErrorCode` directly and delete the
`From<&str>`-driven coercion from that path. `From<&str>` is needed for wire parsing; it does not
need to be reachable from an emit site.

**F3. `api_response_outcome` re-parses the JSON it just serialized and matches `"timeout"`
literally.** `crates/shepr-api/src/server.rs`. Every single API response is serialized, then
parsed back into a `serde_json::Value` purely to classify the log outcome, then discarded. Two
problems in one: a third spelling of the timeout code, and a full JSON parse per response on the
request path. The classification is already known upstream (the `ApiResult` that `encode_result`
consumed).
*Enforceable:* yes, structurally - thread the `ApiResult`'s outcome to `finish_api_response`
instead of the encoded string, and the literal and the reparse both disappear. This is the one
finding in the hunt where the structural fix also removes measurable work from a hot path.

**F4. `SHEPR_ENV` and its value `"1"` are defined twice, once by the reader and once by the
writer.** `src/main.rs` (`SHEPR_ENV_VAR` / `SHEPR_ENV_VALUE`, private to the binary) and
`crates/shepr-mux/src/pane/launch.rs` (identical private consts, used to *set* it). A third
copy is a bare `"1"` in `crates/shepr-agent/src/integration/assets/opencode/shepr-tui-session.test.ts`.
The resolution rule lives only in `main.rs`: exact equality with `"1"`, so `SHEPR_ENV=true` or
`SHEPR_ENV=` does not block nesting. Nothing states that, and the setter is free to change its
value without the reader noticing.
*Enforceable:* yes - one `pub const` in `shepr-config` (which both `shepr-mux` and the binary
already depend on), plus a shared predicate for the resolution rule. The dependency allowlists
in `brokkr.toml` already permit it. The TypeScript asset is a genuinely-forced copy (it ships into
another agent's config and cannot link Rust); what keeps it in step is nothing today - see F30.

**F5. `SHEPR_CONFIG_PATH` is spelled as a literal in the help text.** `src/cli.rs`
(`print_help`) prints `"Env:    SHEPR_CONFIG_PATH overrides config file path"` while
`shepr_config::CONFIG_PATH_ENV_VAR` owns the name.
*Enforceable:* yes - interpolate the const. Trivial, and it is the same edit as F6.

**F6. The help text's environment list is one of five variables, with no mechanism keeping it
complete.** The CLI's behaviour is changed by `SHEPR_CONFIG_PATH`, `SHEPR_SESSION`
(`shepr_config::SESSION_ENV_VAR`), `SHEPR_SOCKET_PATH` and `SHEPR_CLIENT_SOCKET_PATH`
(`shepr_config::address`), and `SHEPR_PANE_ID` (`shepr_mux::pane`, read by
`CliContext::local` to resolve `--current`). `shepr --help` documents exactly one. A user
debugging why `--current` says "belongs to a different server" has no way to discover from the
CLI that two socket variables are involved, even though the error text names one of them.
*Enforceable:* yes - a slice of `(const, description)` in one place, rendered by `print_help`, with
a test asserting the slice covers every `*_ENV_VAR` const the binary's crates export. Without the
test it is a list that drifts, which is exactly the shape of F26.

**F7. The agent-state name list has two owners, and they already disagree.** `src/cli/spec.rs`
owns `AGENT_STATUSES` and `PANE_AGENT_STATES` as typed `(&str, T)` tables (the good pattern), then
`state_label_assignment` in the same file re-implements the same list as bare strings:
`matches!(status.as_str(), "idle" | "working" | "blocked")`. `PANE_AGENT_STATES` has a fourth
entry, `unknown`, which the string list rejects. Whether `--state-label unknown=...` should be
accepted is a product question, but today the answer is set by an independently-maintained
`matches!` rather than by the table.
*Enforceable:* yes - make `state_label_assignment` parse its key through `Choice(AGENT_STATUSES)`
(or `PANE_AGENT_STATES`), so the accepted set is the table by construction.

**F8. Every method's wire name is spelled twice: `#[serde(rename)]` and `traits().name`.**
`crates/shepr-api/src/schema.rs`, 72 methods, 144 string literals. I mechanically compared all 72
pairs: **they agree today**, so this is a prediction rather than a present fact - but a single
mismatch would silently mislabel every log line, every `api_method_name` caller, and the
`api_response_outcome` classification for that method, with nothing failing.
*Enforceable:* yes, by a test - serialize each `Method` variant and assert `json["method"] ==
traits().name`. That needs an enumeration of variants; a `Method::all_for_test()` or a
`strum`-free hand-written sample list with an exhaustiveness check in `traits()` (which already
exists, because `traits()` matches exhaustively) is enough.

**F9. `MethodTraits` is built from a positional 7-tuple of five unlabeled bools.**
`crates/shepr-api/src/schema.rs`: `("pane.swap", true, false, true, true, false, false)` x 72.
Two adjacent bools swapped is a behaviour change (`runs_on_socket_thread` decides whether a
request is handled on the socket thread or dispatched to the app loop) that no reader can spot and
no test covers per-method.
*Enforceable:* yes, by a type - return a `MethodTraits { name, mutates_ui: .., .. }` struct
literal per arm. Verbose, mechanically checked by the compiler, and the bad spelling becomes
unrepresentable. Given "pre-1.0, rewrite internals aggressively", this is worth doing outright.

**F10. Request-id strings are per-call-site literals with no owner, and one id is shared by eight
different methods.** `"cli:pane:list"`, `"cli:agent:start"`, `"cli:workspace:create"` and ~40 more
across `src/cli/*`. Worse: `send_ok_request` in `src/cli.rs` uses the single id `"cli:request"` for
`pane.send_text`, `pane.send_keys`, `pane.send_input`, `pane.report_agent`,
`pane.report_agent_session`, `pane.release_agent`, `pane.report_metadata` and `pane.close`. The
server logs request id and method together, so the id is not load-bearing for correlation - but
it *is* what appears in the error response the user sees, and `"cli:request"` names nothing
actionable. Meanwhile `shepr-api/src/session.rs` hardcodes `server_stop_request("cli:session:stop")`
and `stop_active_server` (reached from `shepr server stop`) reuses it, so `server stop` reports
itself as `session:stop`.
*Enforceable:* partly - derive the id from the command path (`CliCommand::name()` +
`subcommand_name()` already exist and are exhaustive) instead of writing it at each site. Then a
mismatch is impossible rather than merely unlikely. The `cli:session:stop` misnaming is a present
fact, not a prediction.

**F11. Table column widths are magic numbers at three sites.** `src/cli.rs`
(`print_session_table`: `{:<20} {:<8} {:<48}`), `src/cli/server.rs`
(`print_agent_manifest_status`: `{agent:<11}`), `src/cli/machine.rs` (tab-separated, no widths at
all). Three different table conventions in one CLI.
*Enforceable:* by a shared table writer; not by a lint.

**F12. `machine list --json` pretty-prints; every other `--json` in the CLI is compact.**
`src/cli/machine.rs` uses `serde_json::to_string_pretty`; `src/cli.rs`, `src/cli/status.rs` and
every API response path use `to_string`. A consumer doing line-oriented parsing of `--json`
output gets one command that breaks the pattern.
*Enforceable:* yes - route every `--json` through the one `print_json` helper (there are currently
two, `src/cli.rs` and `src/cli/status.rs`, with different signatures) and delete the local
serializer calls. A text rule forbidding `to_string_pretty` outside that helper would hold it.

---

## 2. Values nobody can find, change, or trust

**F13. Nothing answers "what are the tunables of this subsystem".** The timeouts that decide
whether a CLI call, a wait, or a stop succeeds are scattered across six files, each defined
"wherever it was first needed":

| value | where | what it bounds |
|---|---|---|
| `APP_RESPONSE_TIMEOUT` 5 s | `shepr-api/src/server.rs` | app loop answer for subscription/wait probes |
| `ORDINARY_REQUEST_TIMEOUT` 60 s | `shepr-api/src/server.rs` | app loop answer for ordinary requests |
| `INITIAL_REQUEST_TIMEOUT` 5 s | `shepr-api/src/server.rs` | reading the request line |
| `STREAM_WRITE_TIMEOUT` 5 s | `shepr-api/src/server.rs` | socket write |
| `CONNECTION_POLL_INTERVAL` 100 ms | `shepr-api/src/server.rs` | every wait and stream poll |
| `MAX_INITIAL_REQUEST_BYTES` 1 MiB | `shepr-api/src/server.rs` | request line size |
| `ACCEPT_BACKOFF_MIN/MAX` | `shepr-api/src/server.rs` | accept retry |
| `EventHub::MAX_EVENTS` 512 | `shepr-api/src/event_hub.rs` | retained event history |
| `ORDINARY_RESPONSE_TIMEOUT` (server + 5 s) | `shepr-api/src/client.rs` | client-side ordinary bound |
| `WAIT_RESPONSE_GRACE` 30 s | `shepr-api/src/client.rs` | client slack past a wait's own timeout |
| `AGENT_PROMPT_EFFECT_TIMEOUT_MS` 5 s, `AGENT_PROMPT_RESPONSE_GRACE` 1 s | `shepr-api/src/wait.rs` | prompt effect gate |
| `STOP_WAIT_TIMEOUT` 15 s, `STOP_WAIT_POLL` 25 ms, `MIN_SOCKET_TIMEOUT` 1 ms | `shepr-api/src/session.rs` | stop handshake |
| `SERVER_READY_TIMEOUT` 15 s | `src/autodetect.rs` | freshly spawned server's socket |
| remote status probe 15 s | `src/cli/target.rs`, inline `Duration::from_secs(15)` | `--machine` status probe |
| `AGENT_START_POLL_INTERVAL` 100 ms, `PANE_SHELL_READINESS_RETRY_TIMEOUT` 2 s, `DEFAULT_AGENT_START_TIMEOUT_MS` 30 s | `src/cli/agent.rs` | `agent start` polling |

The ones with real documentation (`ORDINARY_REQUEST_TIMEOUT`, `WAIT_RESPONSE_GRACE`) are excellent
and show what the rest should look like. But there is no `timeouts.rs`, no config surface, and no
document; the `15` in `src/cli/target.rs` is not even a named constant. Three separate 100 ms poll
intervals (`CONNECTION_POLL_INTERVAL`, `AGENT_START_POLL_INTERVAL`, and
`dispatch_to_app_until_stopped_result`'s reuse of the former as a recv slice) happen to agree.
*Enforceable:* a single module owning the API/CLI timing budget, with the relationships that are
currently prose (`ORDINARY_RESPONSE_TIMEOUT = ORDINARY_REQUEST_TIMEOUT + 5 s`, already
expressed in code - good) made explicit for the rest. A text rule banning bare
`Duration::from_secs` outside that module is enforceable and would have caught the inline 15.

**F14. Values with no injection point force tests to wait them out.** `stop_session_with_timeout`
is the model to copy: the timeout is a parameter and the public `stop_session` supplies
`STOP_WAIT_TIMEOUT`, so its tests run in 75 ms. Nothing else in scope does this.
`SERVER_READY_TIMEOUT` (15 s) is baked into `auto_detect_launch`, which otherwise takes an
injected `run_client` closure - so the function was *designed* for testability and then hardcoded
the one value a test would need. `AGENT_START_POLL_INTERVAL` / `PANE_SHELL_READINESS_RETRY_TIMEOUT`
/ `DEFAULT_AGENT_START_TIMEOUT_MS` in `src/cli/agent.rs` are all module-private constants, which is
why `agent_start`'s retry loop - by far the most intricate control flow in the CLI, with a
pinned-terminal check, a busy-retry deadline and a five-branch readiness decision - has **no unit
tests at all**. Only its argument parsing is tested.
*Enforceable:* by signature - take the timing as a parameter (a small `AgentStartTiming` struct)
and the loop becomes testable without a server. This is the single highest-value structural change
in the CLI half of the scope.

**F15. `agent start` reaches into `shepr-server` for two constants at CLI-argument-validation
time.** `src/cli/agent.rs` reads `shepr_server::app::AGENT_START_SETTLE_DELAY` and
`shepr_server::app::MAX_AGENT_START_TIMEOUT` to decide `retryable_timeout`. The binary therefore
couples its retry policy to the server's internals, and a `--machine` invocation compares the
*local* build's constants against a *remote* server's behaviour. It works only because client and
server are always the same build - which is true for `--session`, and is exactly what the
`build_mismatch` check exists to police for `--machine`, so it is defensible; it deserves the
comment it does not have.
*Enforceable:* the root `shepr` package has **no `dependency_rule` in `brokkr.toml`** (every
library crate has one). So nothing constrains what the binary reaches into. Adding a rule for the
root package is the mechanical fix, and it would make this coupling a deliberate allowlist entry
rather than an accident.

**F16. Operator guidance for the same situation is assembled at three sites.**
`shepr_api::session::restart_after_update_guidance` / `..._for` own the local "stop the server to
use this build" text. `src/cli/target.rs::restart_guidance` re-authors the whole paragraph for the
`--machine` case in a single `format!`. `src/cli/server_not_running.rs` authors a third variant
("no shepr server is running at ...; run `X` to start or attach it"). All three answer "what should
the operator type next", and the two-sentence structure ("Stopping exits pane processes") appears
in two of them with different wording.
*Enforceable:* partly - one guidance builder taking a target descriptor. Not lintable.

**F17. `restart_after_update_guidance` is `pub` but has exactly one caller, itself.** Only
`restart_after_update_guidance_for` (same file) uses it. Pre-1.0 this is just surface.

---

## 3. One channel, one implementation

**F18. The CLI process has no tracing subscriber, so everything it logs is discarded - including
the only report of a swallowed failure.** `init_file_logging` is called from exactly two places:
`shepr-client/src/lib.rs` and `shepr-server/src/server/headless/bootstrap.rs`. Nothing in
`src/main.rs`, `src/cli.rs` or any subcommand initialises one. Consequences:

- `src/autodetect.rs` emits `tracing::info!("auto-detect launch starting")`,
  `"server already running, attaching as client"`, `"no server running, spawning server daemon"`,
  and critically `tracing::warn!(%error, "Local startup failed; keeping saved machines available")`
  - all **before** `run_client` installs the subscriber. Every one is dropped. The warn is the
  *only* trace of a swallowed local-server startup failure (F22); the user sees nothing at all.
  This is a live defect, not a style point.
- `shepr_api::serialize_response_or_error`'s `tracing::error!("failed to serialize API response")`
  and `send_api_response`'s debug line are dead in the CLI process (they are live in the server,
  which is the main caller).
*Enforceable:* by a test that asserts a subscriber exists before `auto_detect_launch` runs, or
structurally by moving logging init into `main` ahead of the launch dispatch. The general rule
("no `tracing::` call in a process with no subscriber") is not mechanically checkable; the
specific ordering is.

**F19. `shepr-api/src/session.rs` is a second, hand-rolled API client transport.**
`send_stop_request` / `send_stop_request_inner` connect the socket, `serde_json::to_vec` the
request, write `\n`, `BufReader::read_line`, and deserialize - duplicating `ApiClient::connect`,
`write_request` and `read_json_line` from `crates/shepr-api/src/client.rs`, with its own send/recv
timeout policy (`socket_timeout_until`, `MIN_SOCKET_TIMEOUT`) and its own error tolerance
(`stop_request_error_allows_wait`). The documented reason - a build-mismatched server must still
be stoppable - justifies skipping the *build check* in `src/cli.rs::ensure_server_build_matches`.
It does not justify a second transport: `send_request_unchecked` already exists for exactly this,
and `src/cli/server.rs::server_stop` uses it for the `--machine` path while the local path goes
through `session.rs`. So the same command has two implementations depending on the target.
*Enforceable:* structurally - make `session.rs` use `ApiClient` with an explicit timeout and drop
`send_stop_request*`. Then the "one channel" claim holds by construction.

**F20. Three response-encoding implementations, with different behaviour on encoder failure.**
`crate::serialize_response_or_error` is the owner: on a serde failure it logs and emits a valid
`serialization_error` JSON body preserving the request id. `crate::error::encode_result` and
`error_response_json` go through it - correct. But `crates/shepr-api/src/wait.rs` bypasses it four
times with `serde_json::to_string(&ErrorResponse{..}).map_err(std::io::Error::other)?`, and
`crates/shepr-api/src/subscriptions.rs` / `server.rs` use `write_json_line`, which maps an encode
failure to `io::Error::other("failed to encode json: ..")`. So the same class of failure either
produces a well-formed error response to the client (owner path) or kills the connection with an
io error and no response at all (the other two). The fallback machinery in
`serialize_response_or_error` - which has a dedicated test - is defeated on those paths.
*Enforceable:* yes - have `write_json_line` and the `wait.rs` sites call
`serialize_response_or_error`; then delete the raw `serde_json::to_string(&ErrorResponse...)`
spellings. A text rule ("no `to_string` of an `ErrorResponse`/`SuccessResponse` outside
`lib.rs`") would hold it.

**F21. Two error channels for CLI failures, chosen per site.** `CliError` (printed as JSON on
stderr with an exit code, `src/cli/error.rs`) is the owner. But roughly a dozen sites print with
`eprintln!` and return an exit code instead: `src/cli/machine.rs` (eight sites, plain
`error: {error}` text), `src/cli/server.rs::server_stop` (local path), `src/cli/integration.rs`
(`report_outcome`), `src/cli/agent.rs::agent_attach`. A script that parses stderr as JSON - which
is what the API-backed commands train it to do - gets plain prose from these. Within
`machine.rs` alone the prefix is inconsistent: `eprintln!("{error}")` in some arms,
`eprintln!("error: {error}")` in others, `eprintln!("error: {error}; machine was not saved")` in a
third.
*Enforceable:* partly - make these paths return `CliError` (they already run in
`CliResult<i32>` functions, so this is a mechanical change) and a text rule banning `eprintln!`
outside `error.rs`. That rule is checkable.

**F22. `status`'s `is_api_command()` is false for a command that makes an API request, and the
refusal message it produces is wrong and malformed.** `status` (overview) calls
`read_server_runtime_status`, which sends `ping` over the API. It reports
`is_api_command() == false` because part of its output is local. With `--machine`, the user gets
"`status ` is not an API-backed machine command" - note the dangling space, because
`Command::Overview.name()` returns `""`, the same value `Command::Invalid` returns. Two distinct
states share one name string, and the message asserts something untrue about the command.
*Enforceable:* by a type - the classification should be "may this run against a remote machine",
not "is this API-backed", and `name()` should not be able to return `""` (make it `Option<&str>`,
or give `Overview` the name `"status"` and drop the `Invalid` variant in favour of
`Result`/`Option` at parse time). The `""`-for-two-states pattern repeats in every
`src/cli/*.rs` `name()` (agent, pane, tab, workspace, machine, integration, server, session,
terminal, config) - ten copies of the same smell.

**F23. `is_api_command` classification has no single owner and two modules do not implement it.**
`src/cli.rs::CliCommand::is_api_command` hardcodes `false` for `Config`, `Machine`, `Session` and
`Integration`, while `Status`, `Server`, `Workspace`, `Tab`, `Pane`, `Agent` and `Terminal`
delegate to per-module methods. So the rule for four command groups lives in `cli.rs` and for
seven in their own files. If an `integration` or `machine` subcommand ever becomes API-backed,
the blanket `false` silently blocks it with no test failing.
*Enforceable:* yes - require every group's `Command` to implement one trait method (a trait makes
the missing implementation a compile error). `machine_commands_reject_real_local_commands` is a
good test but it enumerates commands by hand, so a new subcommand is not covered by it.

---

## 4. Errors

**F24. `auto_detect_launch` swallows a local-server startup failure and its only report goes
nowhere.** `src/autodetect.rs`: when saved machines are configured, a failed
`spawn_server_daemon` / `wait_for_server_socket` / `validate_running_server_compatibility` is
downgraded to `tracing::warn!` and the client starts anyway. The comment says the Local endpoint's
handshake will report the problem - plausible - but per F18 the warn line reaches no subscriber, so
if the handshake's report is ever incomplete there is no record whatsoever that the server failed
to start. This is the highest-severity item in the report: a swallowed failure whose only
diagnostic is provably discarded.

**F25. `EventHub::push` silently drops an event when the mutex is poisoned.**
`crates/shepr-api/src/event_hub.rs`: `let Ok(mut state) = self.inner.lock() else { return; }`. The
read path was deliberately hardened - `events_after_checked` returns
`EventHistoryError::Unavailable` and has a test for the poisoned case - but the write path just
returns. After a poison, subscribers see a silent, permanent gap rather than the `server_unavailable`
they were designed to receive, because `current_sequence` also stops advancing (so
`events_after_checked` sees a consistent-looking empty tail rather than `Lost`).
*Enforceable:* yes - `push` should be infallible by construction (a lock-free ring, or
`PoisonError::into_inner`, both defensible given the state is a plain Vec of values) or report.
A test can cover it the same way `checked_history_reports_unavailable_instead_of_empty_after_poison`
covers the read side.

**F26. `std::process::exit` on operator-controlled input, from eight sites in `main`.**
`src/main.rs`: `usage_exit` (invalid UTF-8 argv, bad `--session`, bad `--remote`,
`--remote` with a subcommand), `exit_if_nested_disabled`, `load_validated_config_or_exit` (twice),
the remote-launch failure, and the autodetect failure. `main` returns `io::Result<()>`, and
`finish_cli` exists to turn a `CliResult` into an exit code, so the machinery for "refuse with a
code" is already there - these sites just do not use it. Consequence: none of these paths is
reachable from a test, which is why `should_block_nested_for_env` was extracted (good) while the
config-error and remote-launch paths have no tests at all.
*Enforceable:* yes - have `main` build a `CliResult` and exit in exactly one place. A text rule
("`process::exit` only in `main`") is checkable and would hold it.

**F27. A `machine` selector failure and a missing-command failure are both "usage errors", but
one of them is reported without naming the subject.** `src/cli/target.rs::run_on_machine` returns
`usage_error("usage: shepr --machine <label-or-id> <command>")` when no command was given - the
message does not repeat the selector the user typed, so with several shells open it names no
subject. `resolve_machine`'s errors do name it ("unknown machine 'x'; use `shepr machine list`") -
that is the standard to match.

**F28. `write_request` failures inside `ApiClient::request_value` (the unbounded path) have no
send timeout.** `crates/shepr-api/src/client.rs`: the `response_timeout(request) == None` branch
(`agent.prompt` without `wait`, and waits sent without `timeout_ms`) connects and writes with no
`set_send_timeout`, unlike `request_value_with_timeout`. A server that accepts the connection and
never drains the socket buffer blocks the CLI in `write_all` forever with no diagnostic. The
unbounded *read* is deliberate and well argued in the comment; the unbounded *write* looks
incidental to it.
*Enforceable:* by a test using a listener that never reads (the file already has three tests of
exactly this shape).

---

## 5. Tests that prove nothing

**F29. `src/netside_tests.rs` has four unbounded `recv()` loops that hang instead of failing.**
Lines around the source-release ack, the presentation-sync ack, the sync snapshot, the
presentation-effects fence, and the final returning-activation loop all do
`loop { ... control.recv().expect(..) ... }` with `continue` arms and no deadline. If the expected
message never arrives the test blocks forever rather than failing - and `brokkr check` has no
per-test timeout to rescue it. Contrast
`crates/shepr-api/src/server/subscription_socket_tests.rs`, which defines `RESPONSE_TIMEOUT` and
threads a deadline through every read; that is the pattern to copy.
*Enforceable:* yes - a deadline helper, plus a text rule banning bare `.recv()` in tests in favour
of `recv_timeout`.

**F30. One assertion in `src/netside_tests.rs` discards the result it exists to check.**
`returning.receive_response(&target_id, 7, &request_id, &data, &mut endpoints);` - every other
`receive_response` call in the test is wrapped in `assert_eq!` against a
`SurfaceActivationProgress`. This one is a bare statement, so the returning-activation half of the
test asserts nothing about the response it just fed in. It reads as coverage.

**F31. The `--machine` allow/deny test enumerates commands by hand, so new subcommands are
uncovered by construction.** `src/cli/target.rs::machine_commands_reject_real_local_commands`
lists 12 denied and 8 allowed invocations. It is a good test of the ones listed. Nothing makes a
newly added subcommand appear in either list, and `every_cli_spec_root_has_typed_parser` (which
*does* cross-check the spec against the sample list, and is an excellent example of the enforceable
kind) only covers command *groups*, not subcommands.
*Enforceable:* yes - walk the spec's full subcommand tree (`collect_subcommand_paths` already
exists in `src/cli/spec.rs`'s tests) and assert every leaf has an explicit machine-allowed
classification. That turns an enumeration into a rule.

**F32. `EventHub::events_after` (test-support) cannot report what its production sibling
reports.** `crates/shepr-api/src/event_hub.rs`: it returns `Vec::new()` on a poisoned lock and has
no `Lost` signal, while `events_after_checked` distinguishes both. Eleven call sites in
`shepr-server` tests use it (e.g. `app/api/panes/tests.rs`,
`assert!(app.event_hub.events_after(0).is_empty())`) - and an assertion that a history is empty
cannot distinguish "no events were emitted" from "the lock is poisoned", i.e. it is a test that can
pass for the wrong reason.
*Enforceable:* delete `events_after` and have tests use `events_after_checked(..).expect(..)`.

**F33. `status_exposes_only_dynamic_server_capabilities` asserts the absence of fields that no
type has.** `src/cli/status.rs`: `assert!(value["capabilities"].get("surface_interest").is_none())`
and `..get("health_check").is_none()`. `ServerCapabilitiesJson` has exactly two fields, so both
assertions are true for any possible value of the struct - they cannot fail. They read as a guard
against re-adding removed capabilities, but nothing connects them to that intent; the real guard is
the struct definition.

**F34. `random_nested_message_comes_from_known_set` asserts a tautology.** `src/main.rs`:
`random_nested_message()` returns `NESTED_SHEPR_MESSAGES[index]` where `index` is `% len`, and the
test asserts the result is in `NESTED_SHEPR_MESSAGES`. It cannot fail. (The neighbouring
`nested_message_strings_no_longer_repeat_shepr_prefix` *can* fail and is a real, if tiny, guard.)

**F35. `machine_session_attach_is_rejected_as_a_tui_launch` names a situation it does not
exercise.** `src/cli/target.rs`: the test parses `--machine mac session attach work`, asserts it
became a `Tui` launch, then calls `run_on_machine("mac", None, ..)` - constructing the `None`
by hand rather than deriving it from the invocation it just parsed. The assertion that a TUI launch
yields no command for `run_on_machine` is therefore made by the test, not by the code. It is the
same setup as `machine_prefix_rejects_missing_target_and_conflicting_global_options`, duplicated.

**F36. `terminal_and_agent_attach_reject_invalid_config_before_connecting` binds the env guard to
`_env` and then uses it.** `src/cli.rs`: `let _env = ...::IsolatedEnv::new();` followed by
`_env.set(..)`. It works, but the underscore prefix conventionally means "held only for Drop", and
a reader deleting the apparently-unused binding breaks the test's isolation. Cosmetic, but it is
the kind of thing that turns into a silently-non-isolating test later.

---

## 6. Guards and claims that have stopped holding

**F37. `error_response_json`'s `impl Into<ApiErrorCode>` is a check that fails open on a name.**
See F2. `From<&str> for ApiErrorCode` maps an unknown string to `External(s)` by design (correct
for parsing responses off the wire), which means every *emit* site that passes a string gets a
silent pass-through instead of a rejection. Checkable: yes - restrict the emit path to the enum.

**F38. `server_not_running::was_reported` / `reported_response` key on the string
`"server_not_running"`.** `src/cli/server_not_running.rs`. These are `#[cfg(test)]` helpers whose
doc comment in `src/cli.rs`'s test says "The typed error preserves the response without string
matching" - while the helper it calls does exactly string matching on `response.error.code`. If the
code is renamed, both helpers become silent no-ops (`matches!` just returns false) and the test
`maps_dead_server_connect_failure_to_friendly_error` fails loudly - so this one fails *closed*,
which is fine. The claim in the comment is what is false.

**F39. `startup_command` keys on path equality and falls back to a bare `"shepr"`.**
`src/cli/server_not_running.rs`: if the socket path is not exactly
`paths.server_address().api_socket()`, the guidance degrades to "run `shepr`" - which for a
`--session work` invocation or a `SHEPR_SOCKET_PATH` override is the wrong command and will attach
the wrong server. Nothing reports that the fallback was taken. The `--machine` case never reaches
here (it is routed to `target::remote_error`), so the reachable wrong-advice cases are socket
overrides.
*Enforceable:* by making the address the source of the command (it already knows how:
`ServerAddress::attach_command` takes the session) rather than comparing paths.

**F40. `matches::required` returns `String::default()` when the spec and the handler disagree.**
`src/cli/matches.rs` - deliberate and documented ("clap has already rejected argv without it"), and
the non-panicking choice is right. But the failure mode is an empty-string pane id or agent target
sent to the server, which surfaces as `pane_not_found: pane  not found` rather than as a CLI bug.
`src/cli/pane.rs` compounds it: `selected_pane(..)?.unwrap_or_default()`.
*Enforceable:* partly - the spec/handler agreement is what `every_cli_spec_root_has_typed_parser`
checks at group level; extending it to required arguments per subcommand (the spec tree walk of
F31) would make the fallback unreachable in fact as well as in intent.

**F41. `run_on_machine`'s doc comment claims something the function no longer decides alone.**
`src/cli/target.rs::validate_machine_command`'s comment says `--machine` excludes "no local file
evaluation (`agent explain --file`)". That is enforced by `agent::Command::is_api_command`
(`Self::Explain(args) => args.file.is_none()`) in a different file, and nothing ties the comment to
it. It is true today. Checkable: the existing test covers the `--file` case, so this one is held.

**F42. Comment in `src/cli/matches.rs` says a spec/handler mismatch "shows up as a missing value
in tests".** No test asserts that. The claim is unenforced; see F40.

---

## 7. Policy invented per call site

**F43. Retry-and-rediscover policy exists in exactly one place with a comment forbidding its
spread - which is the right design and worth recording as the standard.**
`src/cli/target.rs::server_status`: "Only this read-only probe may rediscover and retry. Requests
that follow the probe must never be replayed after an ambiguous SSH failure." Nothing enforces it;
a second retry loop added elsewhere would violate it silently. Checkable only by review today.

**F44. Two independent busy/poll retry policies in the CLI.** `src/cli/agent.rs` has one loop for
`agent_pane_busy` (deadline `PANE_SHELL_READINESS_RETRY_TIMEOUT`, interval
`AGENT_START_POLL_INTERVAL`, plus a pinned-terminal invariant check each turn) and a second in
`wait_for_named_agent` (deadline from `timeout`, same interval, five-way outcome decision). Neither
shares anything with `shepr-api`'s wait machinery, which already implements deadline-bounded
polling against the same server (`wait_for_agent` with `until` statuses). `agent start` is
effectively a client-side reimplementation of `agent.wait` with extra identity checks.
*Structural suggestion:* this belongs on the server as a parameter of `agent.start`, not as a
polling loop in the CLI. The CLI loop cannot be tested (F14), duplicates the server's wait
semantics, and issues 3+ API requests per poll iteration (`PaneGet`, `PaneProcessInfo`, `AgentGet`)
at 100 ms. Given "willing to rewrite internals aggressively", moving the readiness wait behind
`agent.start` is the right call.

**F45. Ambient dependencies reached directly from logic.** `src/cli/target.rs::CliContext::local`
reads `SHEPR_PANE_ID` and `SHEPR_SOCKET_PATH` from the process environment in the constructor -
which is actually the good pattern (captured once at the edge, and `caller_pane_from` is a pure
function with tests). The rest is less disciplined: `src/main.rs::should_block_nested` reads
`SHEPR_ENV` inline (mitigated by `should_block_nested_for_env`);
`crates/shepr-api/src/server.rs` reads `SSH_AUTH_SOCK` inline inside `start_server_inner`, so the
SSH-agent registry cannot be constructed in a test without mutating the process environment;
`src/main.rs::random_nested_message` reaches for `SystemTime::now()` *and* `process::id()` as an
entropy source inside the function.
*Enforceable:* by signature - pass `SSH_AUTH_SOCK` in as a parameter (`SshAgentRegistry::new`
already takes it as an argument; only the caller hardcodes the lookup).

**F46. User-supplied regexes are compiled with no size or complexity limit, from the wire.**
`crates/shepr-api/src/subscriptions.rs` (`Subscription::PaneOutputMatched`) and
`crates/shepr-api/src/wait.rs` (`wait_for_output`) both call `Regex::new(value)` on a pattern that
arrives in an API request. `regex` is not backtracking so there is no catastrophic-backtracking
risk, but `RegexBuilder::size_limit` defaults to 10 MiB of compiled program per pattern and
`events.subscribe` accepts a *list* of subscriptions on one connection, each with its own pattern,
and one connection thread per subscription. A client (or a misbehaving agent hook, which is a local
untrusted-ish caller) can allocate a large multiple of that per connection.
*Enforceable:* yes, by one shared constructor (`fn compile_match_regex(&str) -> Result<Regex,
ApiError>`) with `size_limit`/`dfa_size_limit` set, used by both sites. A text rule banning
`Regex::new` outside it is checkable. This is also the only place in the scope where a value
coming off the wire is validated by two independently written call sites.

**F47. Two identical request-and-print helpers, plus a third variant.**
`src/cli/runtime.rs::print_method_response` and `src/cli/pane.rs::print_request` are the same
function with different names (both `(&CliContext, &'static str, Method) -> CliResult<i32>`,
both `print_response(send_request(..))`). `src/cli.rs::send_ok_request` is the same again with the
id fixed and the success body dropped. Three spellings of one policy.

**F48. `#[cfg(test)]` poll wrappers discard the errors the production path exists to surface.**
`crates/shepr-api/src/subscriptions.rs`: `ActiveSubscription::poll` and
`ActiveAgentStatusChangedSubscription::poll` are `#[cfg(test)]`-only and implemented as
`self.poll_for_wait(..).ok().flatten()`. The module's own doc comment stresses that errors are
*final* and must not be silently dropped ("reports that instead of going silent"). The test-only
shims do exactly the thing the design forbids, so tests written against them cannot observe a
`pane_not_found` or `events_lost` at all. Two of the module's tests were clearly written to
compensate (`sampling_subscriptions_report_a_vanished_pane_instead_of_going_silent` uses the
checked path).
*Enforceable:* delete the shims; tests call `poll_for_wait` and `.expect(..)`.

**F49. Shared mutable state whose safety rests on call order: `CliContext::build_checked`.**
`src/cli.rs::ensure_server_build_matches` short-circuits on `context.build_checked()` and
`src/cli/target.rs` stores it in a `Cell<bool>`. The invariant is "one build check per target per
process", but the flag is set on the *first successful* status probe and never invalidated - and
`src/cli/target.rs::api_client` can silently rebuild the SSH bridge (`target.bridge.take()`, then
`start(..)` with `use_cached_metadata = false`) *after* the flag was set, which for a machine whose
remote binary was replaced between the two means subsequent requests skip the build check. In
practice the rediscover path only runs inside `server_status`, before `mark_build_checked`, so it
is safe today - by ordering, not by structure.
*Enforceable:* by structure - make the checked state part of the bridge/target value it describes,
so replacing the bridge necessarily clears it. Then the ordering does not matter.

**F50. No secret or personal-data leak found, with one thing to watch.**
`src/cli/machine.rs` prints SSH targets (`user@host`) and `error.escape_debug()` from
connection failures; `crates/shepr-api/src/server.rs` logs socket paths, not credentials;
`shepr-api/src/server.rs` passes the `SSH_AUTH_SOCK` path around but never logs its contents.
`pane.send_text` / `agent.prompt` payloads reach the app but I found no site logging request
*params* - `api_request_started` takes only id, name and two bools, which is a deliberate and
correct choice. Worth stating explicitly so a future change does not casually add `%params`.

---

## 8. Code that is no longer load-bearing

**F51. `ConnectionTarget` is a one-variant enum - a switch that has had one value since it was
added.** `crates/shepr-api/src/client.rs`: `enum ConnectionTarget { SocketPath(PathBuf) }`, with a
`socket_path()` method that matches one arm. `ApiClient` wraps it and `ApiClient::socket_path`
forwards. Three consumers (`src/cli/target.rs`, `crates/shepr-api/src/status.rs`, and `client.rs`
itself) all construct `SocketPath`. Nothing selects between variants.
*Evidence of deadness:* the enum has no second variant anywhere in the workspace and no test
constructs one. *Fix:* `ApiClient { socket_path: PathBuf }`. Low risk; the type exists purely to
name a concept the code no longer distinguishes.

**F52. `ApiClientError::EmptyResponse` and `UnexpectedResult` are produced but never
distinguished.** `crates/shepr-api/src/client.rs` produces both; every consumer in scope funnels
them through `api_client_error_to_io` (`src/cli.rs`) or `io::Error::other(err)`
(`shepr-api/src/status.rs`), i.e. straight to a string. Only `ApiClientError::Io` is ever matched.
*Evidence:* grep for `EmptyResponse` / `UnexpectedResult` outside their definition and `Display`
impl returns nothing. They are not dead (they carry message text) but they carry no decision, so
the enum's shape overstates what callers can do.

**F53. `RenderDemand::join` exists for a single caller pattern.** `crates/shepr-api/src/lib.rs`
defines it with a dedicated test; `shepr-server` is the only user. Not dead - flagged only because
`RenderDemand` lives in `shepr-api` while every consumer is in `shepr-server`, so it is in the
wrong crate for its one client. Moving it would tighten `shepr-server`'s use of the API crate to
actual wire concerns.

**F54. `SessionCliError::InvalidName` never reaches `CliError::source()`.**
`src/cli/error.rs`: `source()` matches `Stop(error) | Delete(error)` and falls through to `_ =>
None` for `InvalidName`, though `InvalidName` wraps the same `SessionError` type. Either an
oversight or an intentional distinction with no comment; either way the asymmetry is invisible.

**F55. `shepr-api`'s test-support surface is broad and partly single-use.**
`error.rs` exports `TestResponseJson`, `TestReply`, `test_json`, `test_success`, `test_error`
behind `cfg(any(test, feature = "test-support"))`. `TestResponseJson` (the trait, as a named
bound) appears only in its own file; the free functions are used from `shepr-server` tests. Two
traits and three free functions to say "get the JSON of a response" is more surface than the use
justifies, and it is production-reachable whenever the `test-support` feature is on.

**F56. `session::data_dir_for` / `client_socket_path_for` / `api_socket_path_for` are `pub`
one-line forwarders to `SessionId` methods.** `crates/shepr-api/src/session.rs`.
`client_socket_path_for` has one in-crate caller, `data_dir_for` one, `api_socket_path_for` two
(one external). They are a compatibility shim for callers that could hold a `SessionId` directly.
*Evidence of deadness:* none are dead; all are redundant indirection. Flagged because they make
`shepr-api::session` look like the owner of path layout when `shepr-config::SessionId` is.

**F57. `ConfigCommand::Invalid`, `TerminalCommand::Invalid`, and the eight other `Invalid`
variants are unreachable by construction, per their own comments.** Each group's spec
`.subcommand_required(true)`, so `missing_subcommand()` ("The spec makes every command group
require a subcommand, so this only runs if a handler and the spec disagree") is documented as
unreachable. Ten `Invalid` variants, ten `""` names (F22), ten dispatch arms, and one
`missing_subcommand` exist to model a state the parser prevents.
*Fix:* have each `parse` return `Option<Command>` / `Result` and let `CliCommand::from_matches`
propagate; the `Invalid` variants and the `""` names disappear together. This is the single
largest mechanical simplification available in `src/cli/`.

---

## Forced duplication, with its reason

- **`shepr-agent/src/integration/assets/.../shepr-tui-session.test.ts` spelling `SHEPR_ENV = "1"`.**
  Forced: the hook ships into another agent's config and runs under that agent's runtime, so it
  cannot link the Rust const. *What keeps it in step today:* nothing. A test that reads the asset
  file and asserts it contains the const's value would hold it, and the repo already has
  manifest-adjacent tests of that shape.
- **`shepr-api/src/client.rs`'s `ORDINARY_RESPONSE_TIMEOUT` deriving from
  `server::ORDINARY_REQUEST_TIMEOUT`.** Not duplication - this is the *good* case, a derived
  constant with the relationship in code and a test (`response_timeout_bounds_ordinary_requests..`)
  asserting the inequality. Cited as the standard the values in F13 should meet.
- **Local and remote hosts each running their own binary.** The `build_mismatch` check
  (`src/cli.rs::ensure_server_build_matches` against `shepr_protocol::BUILD_ID`) is what keeps the
  two copies in step, and it is enforced at runtime rather than claimed. This is the best-handled
  duplication in the scope. The exception carved out for `server stop` / `session stop` is
  documented at both sites and is correct; only its *implementation* is duplicated (F19).

---

## Lateral findings (outside the eight questions)

- **`api_response_outcome` costs a full `serde_json` parse of every API response** solely to pick
  one of three log strings (F3). Per request, on the socket thread.
- **`agent start` issues at least three API requests per 100 ms poll turn** (`PaneGet`,
  `PaneProcessInfo`, `AgentGet`), each of which round-trips to the app loop, for up to 30 s by
  default (F44).
- **`subscriptions.rs::poll_into` allocates a `vec![false; n]` per poll turn**, i.e. every 100 ms
  per subscription stream. Trivial, but it is in the fanout loop the AGENTS.md hot-path principle
  calls out.
- **`ApiClient::request_value`'s unbounded branch has no send timeout** (F28) - closest thing to a
  live hang in the scope.
- **`crates/shepr-api/src/schema/integrations.rs` is 47 bytes**, a single re-export. Not a
  problem, just noting the module boundary buys nothing there.
- **`crates/shepr-api/src/subscriptions.rs` at ~52 KB and `server.rs` at ~69 KB** are the two
  largest files in the crate and each mixes transport, policy and tests. `server.rs` in particular
  holds the listener, the accept backoff, the per-connection state machine, the app-dispatch
  helpers, the shutdown policy and ~700 lines of tests. Splitting it along the lines the comments
  already draw (listen / read / dispatch / shutdown) would make several findings above local
  rather than cross-cutting.
