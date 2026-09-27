# Hygiene hunt: `crates/shepr-server`

Scope read: `src/app/` (state, actions, api handlers, agents, agent resume,
creation, events, git refresh, host theme, ids, runtime, session, tab bar
status, terminal targets/titles, window title), `src/server/` (headless +
bootstrap, dispatcher, client views, endpoint requests, internal events,
lifecycle, render, retained surface, surface interest, client accept, commands,
shell, transport, pane input, input wire, render stream, socket paths,
alt-screen reads), `src/ui.rs` and `src/ui/`. Followed values out into
`shepr-protocol`, `shepr-config`, `shepr-api`, `shepr-client` and the root
binary where the copy lives there.

Findings are grouped by the eight questions, not ranked. Each carries an
enforcement note: **[build]** = holdable by a rule wired into `brokkr check`,
**[type]** = the bad spelling can be made unrepresentable, **[test]** = holdable
by a test, **[none]** = no mechanical hold available, review only.

---

## 1. One value, one owner

### 1.1 `AppPolicy` has two spellings for two of its three variants, and both are in live use

`app/mod.rs:59-63` defines `AppPolicy::PRODUCTION` and `AppPolicy::TEST` as
associated consts that are literally `Self::Production` and `Self::Test`. The
third variant, `Suspended`, has no const. The result is that
`server/headless/lifecycle.rs:221-225` reads:

```rust
self.app.policy = if freeze.persist_session {
    crate::app::AppPolicy::PRODUCTION
} else {
    crate::app::AppPolicy::Suspended
};
```

Two naming conventions inside one expression. Repo-wide there are ~60
`AppPolicy::TEST` sites and 2 `AppPolicy::Test` sites (lifecycle.rs reaches for
the variant because no const exists for `Suspended`). Delete the consts; they
buy nothing.

**Enforcement:** [build] a clippy/grep text rule banning `AppPolicy::PRODUCTION|TEST`
once removed; or simply [type] deleting the consts makes the second spelling
unrepresentable.

### 1.2 The `persist_session: bool` → `AppPolicy` mapping is implemented twice, identically

`server/headless/lifecycle.rs:221-225` and `:264-268` are the same three lines.
Both restore a frozen policy from `HostShutdownFreeze::persist_session`. A third
site (`:249`) writes `Suspended` directly. This is the rule "which policy does a
thaw restore" spelled at two sites; the two are in agreement today, which means
the finding is a prediction, not yet a fact - but it belongs on
`HostShutdownFreeze` as `fn restored_policy(&self) -> AppPolicy`.

**Enforcement:** [type] a method on `HostShutdownFreeze`; the caller then cannot
spell the mapping.

### 1.3 `headless_size` has two owners

`AppState::settings.headless_size` (`app/state.rs:94`, from `config.headless_size()`)
and `HeadlessServer::headless_size` (`server/headless.rs:209`, copied from the
former at `:256`). Worse, the "headless size as a `Rect`" derivation is spelled
three times:

- `app/state.rs:199-204` (`pane_geometry`)
- `app/mod.rs:186-195` (restore path, builds a `PaneGeometry` by hand instead of
  calling `pane_geometry`)
- `server/headless/client_views.rs:568-569` (`resize_tabs_to_headless_size`)

`app/mod.rs:186` is the clearest case: it constructs the same
`shepr_mux::workspace::PaneGeometry { area, pane_borders, pane_gaps,
pane_outer_borders, pane_scrollbars }` that `AppState::pane_geometry_in` already
owns, field for field. If a chrome field is added to `PaneGeometry`, the restore
path is the site that will be missed.

**Enforcement:** [type] drop `HeadlessServer::headless_size` and route through
`app.state`; make `AppSettings::headless_rect()` the only constructor.

### 1.4 `"pane not found"` is spelled ~30 times, in three different wordings, and most omit the pane id

- bare `"pane not found"`, no identifier: `app/api/panes.rs` 6 sites,
  `app/api/panes/geometry.rs` 14 sites
- `format!("pane not found: {}", params.pane_id)`: `app/api/panes/copy.rs:54,63`
- `format!("agent target pane {target} not found")`: `app/agents.rs:291`, and
  `format!("agent target {target} not found")` at `:331`
- `"source pane not found"` / `"target pane {raw} not found"` /
  `"source tab not found"`: `app/api/panes/geometry.rs` (~10 sites)

The same failure, three vocabularies, and the majority of them reach the
operator naming no subject - a `pane.resize` that fails tells you a pane was not
found but not which one, even though the handler holds the id it just failed to
resolve. Same pattern for workspaces: `format!("workspace {id} not found")`
(`app/creation.rs:149`, `app/api/tabs.rs:273`, `app/api/workspaces.rs:352`,
`app/api/layouts.rs:67`, `app/api/panes/geometry.rs:785`) versus bare
`"workspace not found"` (`app/api/layouts.rs:120`, `:201`).

**Enforcement:** [type] `api_helpers::pane_not_found(&pane_id) -> ApiError` and
siblings, plus [build] a text rule banning the bare literals.

### 1.5 The same not-found condition is answered with two different error codes

`app/api/layouts.rs:120` answers a missing workspace with
`ApiErrorCode::WorkspaceNotFound`. `app/api/layouts.rs:201`, in the apply path,
answers the identical condition with `ApiErrorCode::LayoutApplyFailed` and the
message `"workspace not found"`. A client switching on the code sees two
classes for one fact. **This is a divergence today, not a prediction.**

**Enforcement:** [test] a table test over the layout handlers asserting the code
for each resolution failure; or [type] a single `resolve_workspace(...) ->
Result<usize, ApiError>` that owns the code.

### 1.6 The 250 ms "save still running, check back" interval has no name and is spelled twice

`app/session.rs:409` and `:465` both write `Some(now + Duration::from_millis(250))`.
It coincidentally equals `SESSION_SAVE_RETRY_MIN` (`:7`) but is a different knob
(poll cadence, not backoff floor), so neither reuse nor a shared constant is
right - it needs its own name. A third unnamed literal sits at `:253`:
`.min(Duration::from_secs(1))` caps the host-shutdown checkpoint backoff, next
to the two named `SESSION_SAVE_RETRY_*` constants.

**Enforcement:** [build] a clippy lint against bare `Duration::from_*` in
non-test code is too blunt; realistically [none] beyond review, but naming them
removes the site.

### 1.7 1 MiB is the cap on "one client request" in three unrelated places

- `shepr_protocol::MAX_INPUT_PAYLOAD = 1024 * 1024` (`limits.rs:36`)
- `MAX_ENDPOINT_COMMAND_BYTES = 1024 * 1024` (`server/client_commands.rs:11`)
- `MAX_INITIAL_REQUEST_BYTES = 1024 * 1024` (`shepr-api/src/server.rs:48`)

Three independent spellings of the same magnitude for three doors into the same
process. None cites the others. All three crates depend on `shepr-protocol`,
which already owns `MAX_FRAME_SIZE` and `MAX_INPUT_PAYLOAD` - there is no
boundary forcing the copies.

**Enforcement:** [build] move all three to `shepr-protocol::limits` and add a
text rule forbidding `1024 * 1024` outside that module.

### 1.8 `SHEPR_BIN_PATH` and the `SHEPR_ACTIVE_*` family are bare literals

`app/tab_bar_status.rs:30,38,44,48,54` spell `"SHEPR_BIN_PATH"`,
`"SHEPR_ACTIVE_WORKSPACE_ID"`, `"SHEPR_ACTIVE_TAB_ID"`, `"SHEPR_ACTIVE_PANE_ID"`,
`"SHEPR_ACTIVE_PANE_CWD"` inline. `SHEPR_BIN_PATH` is *also* spelled as a bare
literal in `shepr-mux/src/pane/launch.rs:126` and in nine hook assets under
`shepr-agent/src/integration/assets/*/`. The repo has an established convention
for this (`shepr-config::SOCKET_PATH_ENV_VAR`, `shepr-mux`'s
`SHEPR_PANE_ID_ENV_VAR`, `shepr-remote`'s `STARTUP_CWD_ENV_VAR`), so the
tab-bar family is the exception, not the rule. `SHEPR_ROLE` is likewise a bare
literal in `app/api/layouts.rs:798` and `src/cli.rs:1166`.

The hook-asset copies are the genuine forced duplication (shell/python shipped
into other agents' config trees), and nothing keeps them in step with the Rust
side today. The tab-bar and mux copies are not forced.

**Enforcement:** [build] constants in `shepr-mux::pane::launch` (already the
home for the pane env family) plus a text rule banning `"SHEPR_` string
literals outside that module and the asset directory. For the assets, [test] a
test asserting each asset's referenced variable names are a subset of the
declared constant set.

### 1.9 `hostname()` is resolved twice, with two empty-value rules

`app/window_title.rs:23` (`shepr_platform::hostname().unwrap_or_default()`) and
`app/tab_bar_status.rs:142`
(`shepr_platform::hostname().as_deref().unwrap_or_default()`). Both cache at
configure time, both in the same `App`, neither knows about the other. The
`{hostname}` in a window title and the `hostname` tab-bar segment can therefore
disagree only by accident of when each was configured - but they are two owners
of one fact.

**Enforcement:** [type] resolve once into an `App` field, remove the second call.

### 1.10 The server event channel capacity is an unnamed `64`

`server/headless.rs:253`: `mpsc::channel(64)`, next to
`APP_EVENT_CHANNEL_CAPACITY = 256` and `APP_EVENT_DRAIN_LIMIT = 64`
(`app/mod.rs:106-107`), which are named. Same class of tunable, two conventions.

**Enforcement:** [none] mechanically; naming it removes the finding.

### 1.11 `client_socket_path(paths)` is recomputed four times in one function

`server/headless/bootstrap.rs:74, 82, 86` plus `:44` for the API socket. Pure
and cheap, so this is tidiness rather than risk - but it is four sites that will
each be read as "where the client socket comes from."

**Enforcement:** [none].

---

## 2. Values nobody can find, change, or trust

### 2.1 Nothing answers "what are this scope's tunables"

There are ~40 hardcoded constants in `shepr-server` governing observable
behaviour: render cadence (`MIN_RENDER_INTERVAL` 16 ms), git refresh (1500 ms,
5 min, a 30 s retry buried in a test fixture at `app/git_refresh.rs:346`),
session save debounce (5 s) and backoff (250 ms-30 s, 3 failures),
agent start timeout (30 s default / 300 s max / 3 s settle), agent resume
retry (1 s) and managed-resume timeout, agent prompt submit delay (300 ms),
alt-screen read quiet/step/max windows (10/10/120 ms, 15 s, 5 s, 3 wheel
events), handshake timeout (4 s), shutdown flush timeout (1 s), pane teardown
wait (3 s), shell cwd refresh (1 s), datetime refresh (1 s), read line cap
(1000), metadata TTL/token caps (7 constants in `api_helpers.rs:313-319`),
layout pane/depth caps (24/16), copy query/match caps (4096/1024), status text
caps (4096/80), input batch cap (4096), handshake frame cap (64 KiB), endpoint
byte caps (1 MiB/128/128).

`[advanced]` in the config model holds exactly one key
(`scrollback_limit_bytes`). There is no `reference/` or `docs/` folder in the
repo at all, so nothing enumerates the set; each constant sits wherever it was
first needed. A person tuning alt-screen read behaviour has no way to discover
that `INITIAL_QUIET`, `OUTPUT_QUIET`, `STEP_TIMEOUT`, `MAX_DURATION`,
`MAX_RESTORE_DURATION` and `WHEEL_STEP_EVENTS` are the six dials, short of
reading `server/alt_screen_read.rs`.

**Enforcement:** [build] one `server/tunables.rs` (or `[advanced]` config keys
for the ones worth exposing) plus a text rule forbidding bare
`Duration::from_*` / `const MAX_*` outside it. This is the single highest-leverage
structural change in the scope: it converts ~40 review-only findings into one
rule the build holds.

### 2.2 `/bin/sh -lc` is the shell for every tab-bar status command, decided at the call site

`app/tab_bar_status.rs:538`: `std::process::Command::new("/bin/sh")` with
`args(["-lc", &command])`. Not configurable, not derived from
`terminal.default_shell`, not from `$SHELL`, and `-l` (login shell) is a
behavioural choice - each status command pays a full login-shell startup every
interval, and picks up the user's rc files. The choice is invisible from config
and from the config documentation.

**Enforcement:** [none] mechanically; the fix is a named constant or a config
key, which moves it into 2.1.

### 2.3 The shell session cache's clock has no injection point

`server/headless/render.rs:37` calls `Instant::now()` inside
`rebuild_shell_session_cache`, while every reader of the resulting `built_at`
takes an injected `now` (`shell_cwd_refresh_due(now)`, `:29`). A test of the
`SHELL_CWD_REFRESH_INTERVAL` cadence therefore cannot set the built-at time and
must sleep a real second. See 7.4 for the general form.

**Enforcement:** [type] `rebuild_shell_session_cache(&mut self, now: Instant)`;
the loop already has a `now` (`server/headless.rs:385`).

### 2.4 Nothing in this scope reads configuration at point of use

Checked: `AppSettings::from_config` (`app/state.rs:117`) copies everything once,
`configure_tab_bar_status` and `configure_validated_window_title` run once from
`App::with_paths`, and `resolved_config` is encoded before serving
(`bootstrap.rs:8`). This claim of the project's holds. No finding.

---

## 3. One channel, one implementation

### 3.1 Bootstrap writes six `eprintln!` lines the logging channel never sees

`server/headless/bootstrap.rs:43-44, 73-74, 152-161`. Two of these are the
*only* record of a fatal condition:

```rust
Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
    eprintln!("error: shepr server is already running");
    eprintln!("api socket: {}", shepr_api::socket_path(paths).display());
    std::process::exit(1);
}
```

Nothing is logged, so a server started from a unit file or a spawning client
leaves no trace in the server log of why it refused. The "already running"
message is assembled ad hoc at two sites with different second lines (api socket
vs client socket) and a third wording exists in
`server/socket_paths.rs:22-25`: `"shepr server is already running (socket busy
at {})"`. Three hand-built variants of one operator message.

`print_ready_message` (`:151-162`) is the defensible case - the user is looking
at the terminal - but it duplicates the `info!("shepr server started")` line at
`:80-84` with different fields, and the logs path is assembled by joining
`SERVER_LOG_FILE` at the call site rather than by whoever owns the log location.

**Enforcement:** [build] a text rule banning `eprintln!`/`println!` in
`crates/*/src` outside a designated operator-output module; [type] one
`ServerStartupError` type owning all three "already running" renderings.

### 3.2 The tab-bar status command failure logs the operator's command line at `warn`, every interval

`app/tab_bar_status.rs:266`:
`tracing::warn!(command = %runtime.command, error, "tab bar status command failed")`.
A status command that fails persistently emits a warn line every
`interval_seconds` (default as low as 1) for the server's whole life -
unbounded log growth on a permanent condition (see also 7.7). And the command
line is user-authored shell; a `tab_bar_right` entry that curls an endpoint with
a bearer token puts that token in the log at warn level. See 7.8.

**Enforcement:** [none] mechanically for the secret; [test] a test that the
second consecutive identical failure does not re-log, once a rate limit exists.

### 3.3 The status command's stderr is discarded

`app/tab_bar_status.rs:544`: `.stderr(Stdio::null())`. A failing status command
produces a segment that silently goes blank; the only diagnostic is the exit
status. The `Err` arm's message when it times out is
`format!("timed out after {}s", timeout.as_secs())` (`:535`) - it names no
command, and `as_secs()` renders any sub-second timeout as `"after 0s"`.

**Enforcement:** [none].

### 3.4 Levels are inconsistent for the same class of event: a client writer channel closing

`server/headless/render.rs:155` and `:271` log a closed writer channel at
`debug!` and then remove the client; `:214` takes the same branch and logs
nothing at all. All three are the same fact (this client's writer died mid-push)
with three treatments in one file.

**Enforcement:** [type] one `fn writer_gone(client_id)` helper that logs and
records; the branches then cannot disagree.

### 3.5 Nothing is logged when the signal handler fails to install

See 4.1 - listed there because the swallow is the primary defect.

---

## 4. Errors

### 4.1 `ctrlc::set_handler` failure is swallowed, silently, on the server's only signal path

`server/headless.rs:2179`:

```rust
let _ = ctrlc::set_handler(move || { ... });
```

If installation fails (a handler already registered, which `ctrlc` reports as
`MultipleHandlers`), the server runs with no SIGINT/SIGTERM/SIGHUP handling:
`systemctl stop`, a logout or a Ctrl-C will kill it without the shutdown
sequence that saves the session. Nothing is logged and nothing is returned.
**This is a live swallowed failure, not a style point.**

**Enforcement:** [type] return `io::Result` from `ctrlc_handler` and propagate to
`run_server`, which already returns `io::Result<()>`.

### 4.2 The final session save's result is discarded on shutdown

`app/session.rs:686-689`:

```rust
pub(crate) fn retire_session_writer(&mut self) {
    if let Some(thread) = self.session_saver.session_save_thread.take() {
        let _ = thread.join();
    }
```

The join drops both the thread's panic (`Err`) and the `io::Result<()>` the save
job returned. On the one path where losing a save matters most - server
shutdown - a failed write is invisible. Every other save result goes through
`record_session_save_result`, which logs and retries.

**Enforcement:** [type] `join()` into `record_session_save_result`; the `Result`
then cannot be dropped without `#[must_use]` firing.

### 4.3 `cleanup_sockets` returns `io::Result` but can never be `Err`

`server/headless/lifecycle.rs:361-373` logs a removal failure and returns
`Ok(())` unconditionally. `release_sockets_after_save` (`:354`) propagates that
always-`Ok`. `Drop for HeadlessServer` (`server/headless.rs:2163`) then writes
`let _ = self.cleanup_sockets();` - discarding a value that is provably `Ok`.
The signature claims a failure can travel; nothing can. Either it should return
`()` or the warn should become an `Err`.

**Enforcement:** [type] change the signature to `()`; the `let _ =` at the Drop
site then disappears.

### 4.4 Bootstrap aborts the process on a condition an operator controls

`server/headless/bootstrap.rs:45` and `:75` call `std::process::exit(1)` on
`AddrInUse` - from inside `run_server`, which returns `io::Result<()>` and is
called from `main`. "Another server is already running" is exactly the condition
a caller should be allowed to handle (the spawning client wants to attach to the
existing server, not die). `exit(1)` also skips `logging::shutdown("server")` at
`:95`, so the last log lines may not be flushed. The exit code `1` is a bare
literal with no named owner.

**Enforcement:** [type] a typed error variant returned to `main`, which already
owns process exit; [build] a text rule banning `std::process::exit` outside
`src/main.rs`.

### 4.5 A pane removal's result is discarded on the death path

`app/events.rs:153`: `let _ = self.state.commit_pane_removal(&plan);`. The
`AppEvent::PaneDied` handler drops whatever `commit_pane_removal` reports. If
the plan no longer matches state (the pane was closed by an API call between
plan and commit), the event is consumed with nothing recorded.

**Enforcement:** [type] `#[must_use]` on the return, handled explicitly.

### 4.6 `public_workspace_id` answers an invalid index with an empty string

`app/ids.rs:17-28` documents this as deliberate ("a stale one is a caller bug,
reported and answered with an empty id rather than a panic"). It warns, which is
right, but the empty `String` then flows into public ids and API responses as a
valid-looking value - a `""` workspace id in a response is indistinguishable
from a real one to the client. `Option<String>` is what the sibling functions
(`public_tab_id`, `public_pane_id`) return.

**Enforcement:** [type] return `Option<String>` like its siblings.

---

## 5. Tests that prove nothing

### 5.1 `advertised_client_shell_methods_all_exist` cannot fail for most breakages

`server/client_commands.rs:162-176`:

```rust
let method = serde_json::json!({ "method": name, "params": {} });
if let Err(error) = serde_json::from_value::<Method>(method) {
    assert!(!error.to_string().contains("unknown variant"), ...);
}
```

The test passes whenever the error message does not contain the exact substring
`"unknown variant"`. Serde changing its wording, a `#[serde(tag)]` change, or a
params error arriving before the tag is resolved all turn this into a no-op that
still reads as coverage of the 26-entry method list. It is the *only* thing
holding that list to the schema (see 6.1).

**Enforcement:** [test] have `shepr-api` expose the set of method names
(`Method::ALL_NAMES` or an iterator over the schema table) and assert
`CLIENT_SHELL_METHODS ⊆ ALL_NAMES` by set membership - no string matching.

### 5.2 `clamp_terminal_size` tests assert against the constant they are testing

`server/client_transport.rs:1526-1529`:

```rust
assert_eq!(
    clamp_terminal_size(MIN_CLIENT_COLS, MIN_CLIENT_ROWS),
    (MIN_CLIENT_COLS, MIN_CLIENT_ROWS)
);
```

Both sides come from the same constant, so the assertion holds for any value of
`MIN_CLIENT_COLS` as long as the clamp uses it as its lower bound. It cannot
detect a wrong minimum. `:1545`
(`assert!(cols >= MIN_CLIENT_COLS && rows >= MIN_CLIENT_ROWS)`) is near-vacuous
with `MIN_* = 1`: a `u16` fails it only at zero.

**Enforcement:** [test] assert the literal `(1, 1)`, and add a case asserting
`clamp_terminal_size(0, 0) == (1, 1)` - the behaviour actually at stake.

### 5.3 Tests depend on host utilities and shell behaviour

- `app/tab_bar_status.rs:598-599`: `"printf 'old\nfinal\n'"` and
  `"head -c 5000 /dev/zero | tr '\0' x; printf '\nREADY\n'"` - coreutils
  `head`/`tr`, `/dev/zero`, and a pipeline.
- `app/tab_bar_status.rs:696`: `sleep 0.3` - fractional sleep is a coreutils
  extension, not POSIX.
- `app/tab_bar_status.rs:932-933`, `app/snapshot_tests.rs:461`,
  `app/agent_resume.rs:731-738`, `server/headless/tests/mod.rs:5579,5634`:
  hardcoded `/bin/sh`.
- `app/git_refresh.rs:305`: shells out to `git init` via `Command::new("git")`,
  resolved from `PATH`. The test asserts cache-key deduplication, which has
  nothing to do with `git` being installed - the dependency is incidental.

**Enforcement:** [none] for `/bin/sh` (the project is Linux-only and a PTY test
needs a real shell); [test] the `git` dependency can be removed by writing a
`.git` directory by hand, since the code under test only canonicalises paths.

### 5.4 A test reads the real `$HOME` without an `IsolatedEnv`, and hardcodes `/tmp`

`app/snapshot_tests.rs:865-878`:

```rust
cwd: PathBuf::from("/tmp/this-directory-does-not-exist-for-shepr-test"),
...
cwd: std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp")),
```

This is the project's own rule broken twice over (`AGENTS.md`: tests touching
the environment hold an `IsolatedEnv`; never read or write from `/tmp`). The
test means "one pane with a missing cwd, one with an existing cwd"; it gets that
from the developer's `$HOME`. If `$HOME` is unset the fallback silently changes
the test's meaning. `ScratchDir` gives both cases deterministically.

**Enforcement:** [build] a text rule forbidding `"/tmp` literals and
`env::var("HOME")` in `crates/*/src` outside `shepr-test-support`. Note
`server/socket_paths.rs:39-124` also uses `/tmp/...` path strings, but only as
env values never touched on disk - harmless, though it defeats the same grep.

### 5.5 Two `#[cfg(test)]` shortcuts make agent-hosting assertions unfalsifiable

`app/agents.rs:446-463`:

```rust
fn available_shell_name(runtime: &PaneRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() { return Some("sh".into()); }
    ...
}
pub(super) fn runtime_hosts_agent(runtime: &PaneRuntime, expected: Agent) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() { return true; }
    ...
}
```

Any test using a `PaneRuntime` without a live child - which is most of them,
including every `PaneRuntime::test_with_screen_bytes` fixture - gets
`runtime_hosts_agent == true` for *every* agent. Assertions that a pane hosts
the expected agent cannot fail in those tests. The shortcut is also gated on
`cfg(test)` only, while the rest of the crate gates test affordances on
`any(test, feature = "test-api")`, so the root binary's integration tests see
the production path and unit tests see the shortcut - the two suites test
different code.

**Enforcement:** [type] inject the probe (a `ProcessProbe` trait or a
`Option<fn>` on the runtime) rather than branching on `cfg(test)`; then a test
that wants "hosts the agent" must say so.

---

## 6. Guards and claims that have stopped holding

### 6.1 `CLIENT_SHELL_METHODS` is a hand-maintained list of 26 method-name strings matched against generated names

`server/client_commands.rs:17-52`. `supports_client_shell_method` compares
`method.traits().name` - generated in `shepr-api`'s schema - against a literal
list in this crate. Rename or retire a method in the schema and the entry here
becomes a dead string: the method silently stops being reachable over the
client-shell lane (`client_transport.rs:130`, `endpoint_requests.rs:19` both
refuse it), and nothing reports the mismatch. The one test guarding it is 5.1,
which cannot fail reliably. **Checkable, and I cannot rule out that it is stale
today without enumerating the schema.**

**Enforcement:** [type] the right shape is a `client_shell: bool` in the schema's
own `MethodTraits`, which already carries `mutates_ui` and `name` - then the
list disappears and a new method must declare its lane. Failing that, [test] the
set-membership test from 5.1 in both directions.

### 6.2 `AGENTS.md`'s claim that `src/app/` is "split into state, actions and input" is false today

There is no `input` module under `src/app/` at all. `app/` holds 20 modules
(`actions`, `agent_resume`, `agents`, `api`, `api_helpers`, `creation`, `events`,
`git_refresh`, `host_theme`, `ids`, `runtime`, `session`, `state`,
`tab_bar_status`, `terminal_targets`, `terminal_titles`, `window_title`,
`snapshot_tests`, plus `api/` and `actions/` subtrees). Input lives in
`server/pane_input.rs` and `server/input_wire.rs`. The module doc at
`app/mod.rs:1-4` restates the same stale claim, listing only `state.rs` and
`actions.rs` - a doc comment enumerating a list the directory generates.

The no-god-object intent is honoured in substance (nothing is a 3000-line
monolith), but `impl App` is spread over 25 blocks in 20 files and `impl
AppState` over 6 - the "three parts" framing no longer describes anything.
Restate the claim as what is actually true and enforceable: `AppState` is pure
data; `App` owns runtime; no single file over N lines.

**Enforcement:** [build] a file-length rule; [none] for the prose, which should
be rewritten to stop naming a module count that drifts.

### 6.3 `HeadlessServer`'s module doc names socket files the config owns

`server/headless.rs:5-6` claims the server listens on `shepr.sock` and
`shepr-client.sock`. Those names live in `shepr-config::address` and are
session-dependent - a named session listens on
`sessions/<name>/shepr-client.sock` (see `socket_paths.rs:117`). Documentation
restating a list the code generates.

**Enforcement:** [none]; delete the names from the comment.

### 6.4 The "render is pure" claim is honoured by naming, not by signature

`compute_view` (`ui.rs:20`) does touch only `AppState::view` - that part holds.
But `resize_pane_infos` (`ui/panes.rs:79-110`) takes `app: &AppState` and a
`&PaneRuntimeRegistry` and calls `rt.resize(...)` through them. So do
`resize_tab_surface`, `resize_tab_surface_layout` and `resize_all_tab_surfaces`.
The draw path (`render_tab_surface`) and the resize path take *identical*
signatures: `(&AppState, &PaneRuntimeRegistry, ...)`. Nothing in the type system
distinguishes "only draws" from "resizes every PTY in the tab"; the invariant
rests entirely on which function name a caller types. In `render.rs:423-531` the
two are interleaved in one function, which is where a mistake would land.

**Enforcement:** [type] give the resize paths a distinct receiver - a
`PaneResizer<'a>` newtype over the registry, constructed only on the explicit
geometry paths - so a drawing function cannot reach `resize`. Also make
`compute_view` return `ViewState` instead of taking `&mut AppState`, which makes
"touches only `view`" true by signature.

### 6.5 `AppState`'s "pure data" claim leaks a work queue

`app/state.rs:87`: `terminal_runtime_shutdowns: Vec<TerminalId>` - "runtimes
that should be shut down by the app/runtime layer." That is a pending-effects
queue, not state. It is data-shaped so the claim survives literally, but it is
the seam through which pure state schedules side effects, and nothing stops the
next such field from being a channel.

**Enforcement:** [none] mechanically; returning the shutdown list from the
mutating call instead of parking it on state removes the field.

### 6.6 `#![cfg_attr(feature = "test-api", allow(dead_code))]` disables dead-code detection for the whole crate in the build that would run it

`crates/shepr-server/src/lib.rs:2`. The root package depends on `shepr-server`
normally (`Cargo.toml:107`) and with `features = ["test-api"]` as a
dev-dependency (`:118`). Under feature unification, any build that includes
dev-dependencies - `cargo test`, `cargo clippy --all-targets`, i.e. exactly what
`brokkr check` runs - turns `test-api` on, and with it silences `dead_code`
across all ~45 000 lines of the crate. The gate cannot report an unused
function, module, field or variant in `shepr-server`. This is a guard that fails
open, and it is the reason question 8 below is mostly hand-found.

It is also the only crate-wide `allow(dead_code)` in the repo; `shepr-vt` and
`shepr-mux` use narrow per-item allows with justifying comments, which is the
house style.

**Enforcement:** [build] delete the blanket allow and gate the test fixtures
properly (`#[cfg(any(test, feature = "test-api"))]` on the items themselves,
which the crate already does nearly everywhere), then let `dead_code` run.
Verify by checking that `brokkr check` reports nothing new.

### 6.7 Two `debug_assert_eq!` phase claims are not checked in the build that ships

`server/headless/lifecycle.rs:89` and `:300` assert the lifecycle phase.
`brokkr.toml` sets `[test] debug = true`, so these do run under `brokkr test` -
but `brokkr test <name>` defaults to release per AGENTS.md, and the shipped
binary is release. These are the only two invariant assertions in the serving
layer, and they cover the phase machine, which is the one piece of state written
by signal, API and logind threads. Worth promoting to real checks that return
the canonical rejection rather than assertions that vanish.

**Enforcement:** [type] make the phase transitions total functions on
`ShutdownPhase` returning `Result`, so an illegal transition is unrepresentable
rather than asserted.

---

## 7. Policy invented per call site

### 7.1 `server/input_wire.rs` and `shepr-client/src/input_wire.rs` are two halves of one conversion layer, with the shared half duplicated verbatim and the unshared half asymmetric

Same file name, same trait names (`WireKeyKind`, `WireKeyCode`,
`WireMouseButton`, `WireMouseKind`, `WirePaneInput`), mirror-image directions.
Two concrete problems:

1. **`text_bytes` is byte-identical in both copies**, including its doc comment,
   and it is the function that computes the `MAX_INPUT_PAYLOAD` budget. The
   client uses it to decide what to batch (`shell/input/input.rs:955,976`); the
   server uses it to decide what to reject (`client_transport.rs:591`). If the
   two copies ever drift the client will send batches the server refuses, or
   under-fill and lose throughput. Two writers of one accounting rule who have
   never been introduced.
2. **Opposite policies for unrecognised bits.** Client:
   `WireModifiers::from_bits_retain(modifiers.bits())`. Server:
   `KeyModifiers::from_bits_truncate(modifiers.bits())`. One preserves unknown
   bits, the other drops them silently. Equivalent today because both bitflag
   sets happen to cover the same bits; they diverge the moment either side gains
   one. Meanwhile `render.rs:259` uses `KittyKeyboardFlags::from_bits_retain`
   for a value going the other way - so the crate holds both policies for wire
   bitflags.

Neither crate depends on the other, which is what people usually cite here -
but both depend on `shepr-protocol`, which *owns* `ClientPaneInputEvent`,
`WireModifiers` and `MAX_INPUT_PAYLOAD`. There is no forced duplication: this is
one module in the wrong crate, twice.

**Enforcement:** [type] move both directions into `shepr-protocol` (or
`shepr-termio`, also a dependency of both) as inherent impls on the wire types.
`text_bytes` becomes a method on `ClientPaneInputEvent` next to the constant it
charges against, and the conversion direction is then chosen by which method you
call rather than by which crate you are in.

### 7.2 Launch-env validation exists twice with different rules and different messages

`app/api/env.rs:5-37` (server, validating a JSON map) versus
`src/cli.rs:49-60` (CLI, parsing `KEY=VALUE`). Divergences today:

| rule | API | CLI |
|---|---|---|
| empty key | `"env key must not be empty"` | `"env key must not be empty"` |
| key contains `=` | rejected, `"env key {key} must not contain '='"` | impossible by construction (splits on first `=`) |
| NUL in key | `"env key must not contain NUL bytes"` | `"env must not contain NUL bytes"` |
| NUL in value | `"env value for {key} must not contain NUL bytes"` | `"env must not contain NUL bytes"` |

The same rejected request yields different operator text depending on whether it
arrived via `--env` or the JSON API. Within `api/env.rs` alone, two of the four
messages name the key and two do not.

**Enforcement:** [type] one validator in `shepr-api` (both the CLI and the server
depend on it) returning one `ApiError`; the CLI's parser then only splits and
delegates.

### 7.3 The same mutex has two poison policies

`app/session.rs:695` recovers the poisoned `session_writer` with
`unwrap_or_else(PoisonError::into_inner)` and retires it anyway;
`app/session.rs:714` refuses, logs `"session writer is poisoned; refusing to
modify session"` and returns an `io::Error`. Same `Arc<Mutex<SessionWriter>>`,
opposite rules, ~20 lines apart. Elsewhere the crate is consistent
(`client_transport.rs:405`, `tab_bar_status.rs:467,478` all use `into_inner`).

**Enforcement:** [type] a `SessionWriterHandle` newtype owning the lock and the
poison rule; both call sites then get the same answer.

### 7.4 The clock is reached ambiently at ~25 production sites in a loop that already has a `now`

`server/headless.rs:385` takes `let now = Instant::now()` per iteration and
threads it into `handle_scheduled_tasks_headless`, `expire_due_metadata`,
`sync_host_shutdown_freeze`, `handle_tab_bar_status_tasks(now)` and others -
the pattern is clearly intended. Yet within one iteration these call
`Instant::now()` again independently:

`server/headless.rs:335, 361, 1528, 1995, 2072` · `render.rs:37` ·
`internal_events.rs:17` · `client_views.rs:458` · `app/session.rs:156, 395, 491,
504, 590, 616, 657, 680` · `app/events.rs:24, 173` · `app/agents.rs:57, 255` ·
`app/agent_resume.rs:421, 458, 489, 790` · `app/api.rs:710, 749` ·
`app/api/agents.rs:477` · `app/api/panes/reports.rs:227` ·
`app/api/workspaces.rs:274` · `app/git_refresh.rs` (6 sites) ·
`app/tab_bar_status.rs:132` · `app/runtime.rs:149` · `app/mod.rs:289`.

Two consequences. Within one loop iteration, deadlines set from
`Instant::now()` and deadlines compared against the threaded `now` disagree by
the iteration's duration - harmless at 16 ms, load-bearing for the 250 ms
save-poll and the 300 ms prompt delay. And every one of these is a value with no
injection point, which is why so many tests in this crate sleep
(`app/mod.rs:1658` sleeps 30 ms, `tab_bar_status.rs:649` sleeps 1100 ms,
`:726` sleeps 400 ms, `client_transport.rs:841,1497` sleep 5 ms in loops).

**Enforcement:** [build] a text rule forbidding `Instant::now()` outside a small
allowed set (the loop head, a `Clock` type, tests). The signatures that already
take `now` show the target shape.

### 7.5 `HostCellSize::default()` as the fallback for an unknown cell size is spelled at five sites

`server/headless/render.rs:439, 524-528, 570-573`, `client_views.rs`, and the
pattern `if cell_size.is_known() { cell_size } else { HostCellSize::default() }`
appears three times inside `render_and_stream` alone. One rule ("an unknown
reported cell size means the default"), five implementations, in the hottest
function in the scope.

**Enforcement:** [type] `HostCellSize::or_default(self) -> Self` in
`shepr-termio`; the branch becomes unspellable at call sites.

### 7.6 The client-shell boot id's format lives at its only call site

`server/headless.rs:157-168`:

```rust
client_shell_boot_id: format!("{}-{}", std::process::id(),
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()).into(),
```

`shepr_protocol::BootId` is a newtype over `String` with `From<String>` and no
constructor that owns the format - so the identity scheme for the whole
boot-generation mechanism (compared in `client_commands.rs`,
`client_transport.rs`, `surface_reuse.rs`, and four places in `shepr-client`) is
decided by a `format!` in a struct literal. `unwrap_or_default()` means a clock
before the epoch collapses every boot id to `pid-0`, silently defeating the
stale-boot rejection it exists for.

**Enforcement:** [type] `BootId::for_this_process()` in `shepr-protocol`, with
the `From<String>` impl restricted to deserialization.

### 7.7 Three sanitizers for text entering the same tab bar, with three different rules

`app/tab_bar_status.rs:291-313`:

| | trims | strips control | strips unicode format controls | caps length |
|---|---|---|---|---|
| `sanitize_separator` | no | yes | no | no |
| `sanitize_literal_text` | no | yes | no | no |
| `sanitize_status_text` | yes | yes | yes | 80 chars |

All three feed `AppState::tab_bar_right`, which is rendered into the same row. A
configured `text` entry can therefore carry a bidi override
(`U+202A`-`U+202E`) that an identical string from a `command` entry cannot, and
the separator is uncapped. Three owners of "what may appear in the tab bar."

**Enforcement:** [type] one `TabBarText` newtype whose only constructor
sanitizes; `AppState::tab_bar_right` and `tab_bar_right_separator` then cannot
hold anything else.

### 7.8 Secrets and user data reaching diagnostic output

- `app/tab_bar_status.rs:266` logs the full status command line at `warn` (see 3.2).
- `render.rs:693-695` puts a terminal id into a `ServerShutdown` message text
  sent to the client - benign, but it is operator text assembled at the site.
- `app/api/workspaces.rs:539`: `let _ = std::fs::remove_dir_all(&source_cwd);`
  - a recursive delete whose failure is discarded. Worth a second look: in test
  code it is a fixture teardown, but a recursive delete of a path derived from
  workspace state is the one operation you want logged either way.

**Enforcement:** [none] mechanically for the secret; [build] a text rule
banning `remove_dir_all` outside `shepr-test-support`.

### 7.9 Unbounded growth reachable from a misbehaving peer

- `pending_alt_screen_reads`, `deferred_alt_screen_reads`,
  `queued_agent_manifest_reloads` (`server/headless.rs:175-182`) are `Vec`s with
  no cap. `queued_agent_manifest_reloads` in particular accumulates one entry
  per `server.reload-agent-manifests` request that arrives while a reload runs;
  a client looping on that method grows it without bound.
- `shutdown_flushes` (`:222`) is bounded by client count, fine.
- `broken_clients.contains(&client_id)` (`render.rs:797`) is a linear scan
  inside the per-render loop; bounded by client count, so cosmetic.

**Enforcement:** [type] a bounded queue type that rejects with `EndpointBusy`
(the code already exists) past a cap.

### 7.10 `normalize_api_key_alias` is a three-entry alias table with no owner

`app/api_helpers.rs:9-15` maps `"C-c" | "c-c" => "ctrl+c"` and `"+" => "plus"`.
Key-name parsing otherwise belongs entirely to `shepr-config::parse_key_combo`.
A fourth alias will be added here rather than there, and the two will drift.

**Enforcement:** [type] move the aliases into `shepr-config` next to the parser.

---

## 8. Code that is no longer load-bearing

Caveat: 6.6 means the compiler is not helping here. Everything below is
hand-verified by grep.

### 8.1 `success()` and `failure()` discard their first parameter at ~220 call sites

`app/api/responses.rs:4-14`:

```rust
pub(crate) fn success(_id: String, result: ResponseResult) -> ApiResult { Ok(result) }
pub(crate) fn failure(_id: String, code: ..., message: ...) -> ApiResult { Err(...) }
```

Both ignore `_id`. Counted call sites: ~220 across `app/api/*` (geometry 61,
workspaces 33, panes 28, layouts 26, agents 21, tabs 19, reports 17, copy 14,
plus tests). Every one threads an id - usually a `String` that had to be cloned
or moved specifically to be dropped - and every one reads to a newcomer as
though response correlation happens here. It does not: correlation is
`shepr_api::error::encode_result(id, result)` at the dispatcher. This is the
single largest volume of dead plumbing in the scope.

**What tells me it is dead:** the parameters are `_`-prefixed and unread in
bodies that are one expression long; `ApiResult` has no id field
(`shepr_api::error::ApiResult = Result<ResponseResult, ApiError>`); the
dispatcher supplies the id separately.

**Enforcement:** [type] delete the parameter - the compiler then finds all 220
sites and the dead clones with them.

### 8.2 The `AppEvent::GitStatusRefreshed` arm of `AppState::handle_app_event` is unreachable

`app/actions/events.rs:240-246`:

```rust
AppEvent::GitStatusRefreshed { results, cache_updates } => {
    let _ = results;
    let _ = cache_updates;
    Vec::new()
}
```

`App::handle_internal_event_with_updates_and_render` (`app/events.rs:38-...`)
intercepts `GitStatusRefreshed` before `state.handle_app_event` is ever called,
routing it to `apply_workspace_git_statuses`. So in production this arm cannot
run; in a test that calls `state.handle_app_event(GitStatusRefreshed { .. })`
directly it silently does nothing, which reads as "git statuses were applied."

**What tells me it is dead:** the only producer path goes through
`App::handle_internal_event*`, which matches the variant first; the arm's body
explicitly discards both payload fields.

**Enforcement:** [type] split the event enum so state-level events and
app-level events are different types; the arm then cannot be written.

### 8.3 Two vestigial section banners and an orphaned import

`server/headless.rs:127-131` is a `// Constants` banner containing no constants
- only `struct ListenerFd`. `:80-82` is a `// Loop event enum` banner, fine.
`:79` `#[cfg(test)] use std::fs;` sits among the crate-level imports of a
2222-line file.

**Enforcement:** [none].

### 8.4 `AppPolicy::PRODUCTION` / `AppPolicy::TEST`

Covered in 1.1; they are dead abstractions (pure aliases) rather than dead code,
but they are counted as a site by every finding that touches policy.

### 8.5 Candidates I could not confirm

Because `dead_code` is silenced (6.6), I cannot responsibly call the following
dead without the lint enabled: `MIN_CLIENT_COLS`/`MIN_CLIENT_ROWS` (used, but
only to clamp to 1 - a constant that has had one value and whose only effect is
"not zero"), the `AttachInputDelivery::Failed` variant, and
`ShutdownLifecycle::set_frozen_session_policy_for_test`. The right move is to
fix 6.6 first and let the build answer, since the cost of a wrong deletion here
is the one nobody can undo by reading.

---

## Hot-path notes (weighed, per the brief)

These are not findings in the eight categories, but they sit on the paths the
brief asked me to weigh.

- **Hidden-pane early exits and narrow accessors are in good shape.**
  `sync_immediate_pty_sources` / `pty_sources_visible_to_any_render_target`
  (`render.rs:295-388`) do the right thing, and the comment at
  `server/headless.rs:186-196` explains the dirty-flag design that keeps the
  per-notify walk off the loop. `render_and_stream` exits early for the
  no-clients case (`:426`). No terminal-core lock is held across an await in
  this scope.
- **`shell_target_for_client(client_id)` is called twice in immediate
  succession** at `render.rs:465` and `:497` (`if changed && let Some(shell_target)
  = self.shell_target_for_client(client_id)`), inside the per-render-target
  loop. The first result is still in scope and unused after the guard.
- **`render_and_stream` recomputes `effective_size` cols/rows twice** (`:427`
  and `:818`), the second time only to pass to a `debug!`. The second pair is
  computed unconditionally even when the log level filters the line out.
- **`refresh_shell_projection_sources` (`render.rs:49-75`) clones the whole
  shared `SessionSnapshot` once per shell client, per second, purely to discover
  whether anything changed**, then throws every clone away. Idle cost is
  O(clients × snapshot size) per second. A revision/fingerprint comparison
  before the clone would make the idle path allocation-free; the comment at
  `:42-48` argues the current cost is acceptable, but it argues about the render,
  not the clone.

---

## The three project claims, verdicts

| claim | verdict |
|---|---|
| `AppState` is pure data, testable without PTYs or async | **Holds.** No channels, no runtime, no `Arc`; `test_new()` works. One leak: `terminal_runtime_shutdowns` is an effects queue parked on state (6.5). |
| Render is pure; `compute_view()` touches only `AppState::view`; surface drawing takes shared references and only draws | **Holds in behaviour, not in structure.** `compute_view` does touch only `view`, but nothing enforces it (it takes `&mut AppState`), and the resize paths are signature-identical to the draw paths and mutate PTYs through `&AppState` (6.4). |
| `src/app/` stays split into state, actions and input | **False as written.** There is no `input` module in `app/`; there are 20 modules and 25 `impl App` blocks. The no-god-object outcome is met; the claim describes a structure that no longer exists, and `app/mod.rs`'s own doc header repeats it (6.2). |

---

## If I had to name one structural move

Fix 6.6 first - delete the crate-wide `allow(dead_code)` - because it is the one
finding that hides other findings, and then do 2.1: one `server/tunables.rs`
holding every timeout, interval, cap and limit in the crate, with a text rule in
`brokkr.toml` forbidding bare `Duration::from_*` and `const MAX_*` elsewhere.
Those two turn roughly half the list above from "a tidy-up that decays" into
"a rule the build runs."

The second move is 7.1: `input_wire` belongs in `shepr-protocol`, once, in both
directions. It is the only place in this scope where two copies of one
accounting rule govern two ends of the same wire.
