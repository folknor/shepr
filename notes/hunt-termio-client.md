# Hygiene hunt: `shepr-termio` + `shepr-client`

Scope read: all of `crates/shepr-termio/src` and `crates/shepr-client/src`
(~51k lines including tests), following values out into `shepr-config`,
`shepr-vt`, `shepr-protocol`, `shepr-mux`, `shepr-server` and `src/main.rs`
where a copy lived there.

Findings are grouped by the eight questions, not ranked. Each carries an
**Enforce:** line saying whether the fixed version can be held mechanically and
by what. Where a copy is forced, that is stated instead.

---

## 1. One value, one owner

### 1.1 The modifyOtherKeys level has two enums and the client recovers the number by sniffing a byte string

`shepr_termio::input::model::ModifyOtherKeysMode` (`Mode1`/`Mode2`) emits
`b"\x1b[>4;1m"` / `b"\x1b[>4;2m"` from `set_sequence()`. `shepr_vt::ModifyOtherKeysLevel`
is a second enum over the same concept, and `host_term::modes::set_direct_host_keyboard_protocol`
writes `\x1b[>4;{level}m` from it. `terminal_setup::setup_terminal_with_capabilities`
bridges the two like this:

```rust
let parameter = if mode.set_sequence().ends_with(b";1m") { 1 } else { 2 };
let level = shepr_vt::ModifyOtherKeysLevel::from_parameter(parameter);
```

So the mode-to-parameter mapping is owned three times (the termio enum's byte
literal, the vt enum's `Display`, and this string sniff), and `set_sequence()`'s
only remaining consumer is the sniff - the bytes it builds are never written.
A `Mode3` or a spelling change in `set_sequence` silently yields `2` here.
`ModifyOtherKeysMode` should be deleted and `host_modify_other_keys_mode()`
should return `shepr_vt::ModifyOtherKeysLevel` directly.

**Enforce:** yes, by type. Returning `ModifyOtherKeysLevel` from the detector
makes the second enum and the sniff unrepresentable. The `;1m`/`;2m` literals
in `model.rs` then disappear.

### 1.2 Host-terminal control sequences are owned partly by `shepr-termio`, partly by the client

`crates/shepr-termio/src/host_term/modes.rs` is the stated owner of host mode
sequences, but `crates/shepr-client/src/terminal_setup.rs` writes its own raw
copies: `\x1b[>4;0m` and `\x1b[<1u` in `HostModes::restore` (both also spelled
in `modes.rs`), `\x1b[?1016h` in `set_mouse_capture_with_writer` (whose *disable*
counterpart lives in `modes.rs`'s `DISABLE_HOST_MOUSE_REPORTING_SEQUENCE`),
`\x1b[?25h\x1b[0 q` in the restore postlude, and `PUSH_WINDOW_TITLE`/
`POP_WINDOW_TITLE` (`\x1b[22;0t`/`\x1b[23;0t`) while OSC 0 itself lives in
`host_term::title`. `HOST_CELL_SIZE_QUERY = b"\x1b[16t"` is defined in
`terminal_geometry.rs` while the module doc for `host_term::cell_size` names the
same sequence in prose and `shepr-vt`'s `scan.rs` parses the reply. Enable and
disable of the same mode therefore sit in different crates.

**Enforce:** partly. A text/lint rule ("no `\x1b[` byte literal outside
`shepr-termio/src/host_term/` and `shepr-vt`") is expressible as a brokkr-style
scan, comparable to the existing gremlin scan and the
`alacritty-terminal-only-in-shepr-vt` dependency rule. The literals inside test
assertions are the legitimate exception and would need the same exclusion shape
the gremlin rule already uses.

### 1.3 The paste-rejection sentence is spelled twice, and one copy says so

`shell/input/input.rs::push_focused_paste`:
`"Paste is {size} bytes; Shepr's limit is {} bytes"`.
`attach.rs::ForwardOutcome::notice`: the same sentence, under a comment reading
"The notice for a rejected paste, **worded like the client shell's**". Nothing
keeps them in step - no shared formatter, no test comparing the two strings.
The threshold itself is correctly single-owned (`shepr_protocol::MAX_INPUT_PAYLOAD`);
only the operator-facing text is duplicated.

**Enforce:** yes, by extraction - one `fn paste_rejected_notice(size, max) -> String`
that both call. A test comparing the two strings would also work but is the
weaker form.

### 1.4 The local endpoint's name exists in three spellings

- `endpoint.rs::ClientEndpointId::storage_key()` → `"local"` (persistence key)
- `shell/endpoints.rs:484` → `label: "Local".into()` (display label, constructed inline)
- `shell/sidebar/endpoint_sidebar.rs:580` → `.map_or("Local", |e| e.label.as_str())` (fallback when no entry exists)

The sidebar fallback re-states the display label that `shell/endpoints.rs`
builds, so a rename there leaves the fallback showing the old name for exactly
the case where the entry is missing - the case nobody tests.

**Enforce:** yes. One `ClientEndpointId::display_label()` next to `storage_key()`,
and the fallback reads it. Not mechanically checkable beyond that; the type
makes the inline literal unnecessary.

### 1.5 The 350 ms double-click window is spelled twice

`shell/state.rs::ClientPaneClick::is_double_click_for` and
`shell/input/mouse.rs:1519` (sidebar-divider double click) each write
`std::time::Duration::from_millis(350)` inline. Same user-facing gesture,
two unnamed copies, different files.

**Enforce:** yes - one named constant; a clippy-style "no magic duration"
rule is not available, but a single `const DOUBLE_CLICK_WINDOW` referenced
twice is checkable by review and by grep.

### 1.6 The 33 ms drag-send throttle is spelled twice, inline, beside named siblings

`shell/input/mouse.rs` names `SELECTION_AUTOSCROLL_INTERVAL` (30 ms) and
`SELECTION_REPAINT_INTERVAL` (16 ms) at the top of the file, then writes
`Duration::from_millis(33)` inline at lines 803 and 840 for the scrollbar-drag
and split-drag send throttle. Three throttles for the same class of thing
(how often a drag produces network traffic or a repaint) at 16/30/33 ms, two of
them findable and one not.

**Enforce:** yes, by naming the third and putting all three together (see 2.1).

### 1.7 `SectionSplit`'s bounds and default are spelled three times

`shell/sidebar/sidebar_tokens.rs`: `DEFAULT: Self(0.5)`, `new()` validates
`(0.1..=0.9)`, `from_drag()` clamps to `0.1, 0.9` and falls back to a second
literal `0.5` for a non-finite value. Four numbers, three sites, and the
`Deserialize` error message ("sidebar split must be between 0.1 and 0.9")
restates the range in prose - a fifth copy.

**Enforce:** yes. `const MIN`/`MAX` used by `new`, `from_drag` and the error
message via `format!`, and `from_drag`'s fallback reading `DEFAULT.get()`.

### 1.8 Client teardown policy is spelled twice, verbatim

`lib.rs` ends `run_client_with_mode` through two paths, each writing the same
three lines: `rt.shutdown_timeout(Duration::from_millis(100))`,
`shepr_remote::release_ssh_resources_before_exit(Duration::from_secs(1))`,
`shepr_platform::logging::shutdown("client")`. The two timeouts have no names
and the string `"client"` is passed to both `logging::startup` and
`logging::shutdown` at three sites.

**Enforce:** yes - one `fn finish_client(rt)`; a guard type whose `Drop` runs it
makes the omission unrepresentable.

### 1.9 `pixel_geometry_*` has two owners: a constructor that lies and a caller that patches it

`ClientSettings::from_config` sets `pixel_geometry_enabled: false` and
`pixel_geometry_fallback: false` unconditionally, then `run_client_with_mode`
computes the real values from `client_rendered_shell` / `attach_escape` and
assigns them into the struct. Between the two, `ClientSettings` holds values
that are not the resolved configuration. A later reader of
`from_config` cannot tell that its answer for those two fields is a placeholder.
Then `lib.rs:451-452` reads one of them from `state.settings` and the other from
`config.settings` - two copies of the same pair in the same expression.

**Enforce:** yes, by signature: `ClientSettings::resolve(config, launch_mode)`
returning a fully initialised value, and the fields private with no setters.

### 1.10 Two timeout tunables in the shell are `u64` seconds while every sibling is a `Duration`

`shell/state.rs`: `ENDPOINT_ERROR_TIMEOUT_SECS: u64 = 5` and
`ENDPOINT_NOTICE_TIMEOUT_SECS: u64 = 10`, each wrapped in
`Duration::from_secs(...)` at the use site. Everywhere else in the crate the
same class of value is a `const ...: Duration`. Same numbers as
`HEARTBEAT_INTERVAL` (5 s) and `HEARTBEAT_TIMEOUT` (10 s), which is a
coincidence a reader has to check.

**Enforce:** yes, by type - declare them as `Duration`.

### 1.11 `git` is invoked from four production sites with four policies

`shepr-client/src/workspace_label.rs`, `shepr-mux/src/git/status.rs` (three
sites), `shepr-mux/src/git/discovery.rs` (two sites), `shepr-server/src/app/git_refresh.rs`.
Each spells `Command::new("git")` itself. The client's copy has no timeout, no
environment scrubbing, and drops every failure with `.ok()`. Whatever policy
`shepr-mux/src/git` has arrived at for env, timeouts and error reporting, the
client does not share it. Out of scope proper, but the client's copy is the one
in scope and it is the least careful of the four.

**Enforce:** yes, structurally: one `git` runner (it belongs below `shepr-mux`,
e.g. in `shepr-platform`), plus a text rule forbidding `Command::new("git")`
outside it.

---

## 2. Values nobody can find, change, or trust

### 2.1 Nothing answers "what are this crate's tunables?"

Counting only production code in scope, the input/presentation side has at
least 30 timing, size and geometry knobs. They are defined in 14 files, wherever
they were first needed: `HOST_KEYBOARD_QUERY_TIMEOUT` and
`MAX_BUFFERED_HOST_INPUT` in `terminal_setup.rs`; `INITIAL_RETRY_DELAY`,
`MAX_RETRY_DELAY`, `STABLE_CONNECTION_PERIOD`, `ATTENTION_RETRY_DELAY`,
`ATTEMPT_BUDGET` in `endpoint/supervisor.rs`; `MAX_QUEUED_BATCHES`,
`MAX_BATCH_BYTES`, `MAX_QUEUED_BYTES`, `WRITE_TIMEOUT`, `IO_POLL_INTERVAL` in
`endpoint/writer.rs`; `ENDPOINT_COMMAND_TIMEOUT`,
`MAX_RETIRED_REQUESTS_PER_ENDPOINT`, `MAX_ENDPOINT_RESPONSE_BYTES` in
`endpoint/commands.rs`; `HEARTBEAT_INTERVAL`/`HEARTBEAT_TIMEOUT` in
`endpoint/health.rs`; `ACTIVATION_TIMEOUT` in `endpoint/activation.rs`;
`LOCAL_`/`REMOTE_HANDSHAKE_READ_TIMEOUT` in `handshake.rs`;
`SELECTION_AUTOSCROLL_INTERVAL`/`SELECTION_REPAINT_INTERVAL`/
`MODAL_PASTE_CLIPBOARD_TIMEOUT` in `shell/input/`;
`MIN_TAB_WIDTH`/`NEW_TAB_WIDTH`/`WORKSPACE_HEADER_ROWS`/the two `_SECS` in
`shell/state.rs`; `TAB_SCROLL_BUTTON_WIDTH`/`MIN_TAB_STRIP_WIDTH` in
`presentation/tabs.rs`; `DEFAULT_CELL_WIDTH_PX`/`DEFAULT_CELL_HEIGHT_PX` in
`terminal_geometry.rs`; `MAX_PENDING_PASTE_BYTES`, `PASTE_STALL_TIMEOUT`,
`RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS`,
`MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS`,
`MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES`, `MAX_DISCARDED_CONTROL_TAIL_BYTES` in
`input/raw_input.rs`; `MAX_NOTICES` inside a function body in `lib.rs`; plus the
un-named inline ones from 1.5, 1.6 and 1.8 and the 100 ms resize-poll sleep in
`terminal_geometry.rs::resize_poll_loop`.

Several of these interlock and the interlock is documented only in prose on one
of them (`ATTEMPT_BUDGET`'s doc comment explains its relationship to
`MAX_RETRY_DELAY` and to the 15-second-per-command discovery budget that lives
in `shepr-remote`). Nothing lists the set, and nothing checks the interlock.

**Enforce:** partially. Collecting the timing knobs into one `timing.rs` per
crate is holdable by a text rule ("no `const ...: Duration` outside
`timing.rs`"), which is exactly the shape of rule `brokkr.toml` already runs.
The *interlocks* (`ATTEMPT_BUDGET < MAX_RETRY_DELAY`, `HEARTBEAT_INTERVAL <
HEARTBEAT_TIMEOUT`, `IO_POLL_INTERVAL < WRITE_TIMEOUT`) are holdable by
`const` assertions, which cost nothing and currently do not exist.

### 2.2 `EndpointRegistry::insert` reaches the clock, one level below the injection point

`endpoint/health.rs` is the model of an injectable value: every method takes
`now: Instant`, and its tests need no sleeps. `endpoint/registry.rs:99` then does
`EndpointHealth::new(Instant::now())` inside `insert`, so the one moment the
health machine measures against (`connected_at`, which drives
`initial_snapshot_expired`) cannot be injected. `registry.rs:430`'s test
consequently has to write `Instant::now() + Duration::from_secs(300)` to get
past a 10-second timeout, and no test can exercise the initial-snapshot
expiry boundary.

**Enforce:** yes, by signature: `insert(..., now: Instant)`.

### 2.3 `ActivationState` sets its deadline from the ambient clock at five sites

`endpoint/activation.rs` lines 1004, 1048, 1068, 1085, 1107 each write
`self.deadline = Instant::now() + ACTIVATION_TIMEOUT;`. The activation state
machine is otherwise pure and heavily tested (`activation_tests.rs`, 1622
lines), but its one time-dependent transition is not injectable, so no test can
drive an activation to its 5-second timeout without waiting 5 seconds - and none
does.

**Enforce:** yes, by signature - thread `now` through the five callers, same as
`health.rs` and `supervisor.rs` already do. That also collapses five copies of
the deadline rule into one.

### 2.4 The mouse/selection layer reaches the clock from inside the logic

`shell/input/mouse.rs` calls `std::time::Instant::now()` at six sites inside
event handling (autoscroll arming, two drag throttles, drag repaint, the
double-click check, click recording), and `shell/input/word_selection.rs` at
one more. This is the same defect as 2.3 with more sites, and it is why the
click and drag behaviours are tested only for their non-timing aspects:
`shell/tests/mouse_selection.rs` (1312 lines) never exercises a throttle or a
double-click window.

**Enforce:** yes, by signature. The client loop already has a `now` at
`lib.rs:652`; passing it into the mouse handlers makes the ambient reads
unnecessary, and a text rule forbidding `Instant::now()` under `shell/input/`
holds it.

### 2.5 `host_modify_other_keys_mode()` reads three environment variables at the moment of use

`input/model.rs` splits the rule correctly (`host_modify_other_keys_mode_for_env`
is pure and tested) but the wrapper reads `TMUX`, `TERM_PROGRAM` and
`WEZTERM_PANE` when `setup_terminal_with_capabilities` happens to run, not at
launch resolution. The values do not end up in `ClientSettings`, so nothing in
the client can report what host protocol it decided on, and the decision is
invisible to every test of terminal setup. Compare
`ClientProcessRole::from_env()`, which resolves its one variable at launch and
*refuses* on an unrecognised value - that is the pattern this one should follow.

Note also that `WEZTERM_PANE` uses `var_os(...).is_some()` (empty value counts
as set) while `TMUX` uses `var(...).is_ok()` (empty also counts) and
`TERM_PROGRAM` compares case-insensitively - three different resolution rules
for three variables in one function, none stated.

**Enforce:** yes - resolve into `ClientSettings` at launch alongside
`ClientProcessRole::from_env()`, and forbid `env::var` outside the launch module
by text rule (the crate has only four production `env::var` call sites, so the
rule is cheap today).

### 2.6 Sidebar chrome preferences are validated at the moment of use, not at startup

`shell/presentation/config.rs::persist_chrome_preferences` writes preferences and,
on failure, calls `self.set_endpoint_error(error)` - a UI banner, hours into a
session, on whatever gesture happened to trigger a persist. The preferences
*path* is `Option` and a `None` silently skips persistence entirely. Nothing at
launch checks that the preferences path is writable, so the first sidebar drag of
the session is where an unwritable state directory is discovered. AGENTS.md's
stated posture is "Any config problem fails the launch; no fallbacks".

**Enforce:** yes, by a startup probe on the preferences path, which turns this
into a launch refusal. Checkable by a test that launches with a read-only state
dir.

---

## 3. One channel, one implementation

### 3.1 `io::stdout()` is acquired fresh at eighteen production sites; there is no owner of host-terminal output

`terminal_setup.rs` (11), `terminal_geometry.rs` (3), `state.rs` (2),
`lib.rs` (5), `shell_runtime.rs` (3), and `shepr-termio`'s
`host_term::title::write_clipboard_bytes` (1, the only one that takes
`.lock()`). Every write is separately unbuffered and separately unlocked, and
the client writes frames, mode changes, window titles, queries and OSC 52
clipboard payloads through them from more than one thread (the clipboard write
happens on the main loop, terminal restore can happen from the panic hook and
from `Drop`). There is no `HostTerminalOut` type. The functions that take
`&mut impl io::Write` are the good half of this - they are testable - but their
callers all resolve to a fresh `io::stdout()`.

**Enforce:** yes. One owned writer handed to the client loop, and a text rule
"no `io::stdout()` outside `host_out.rs`" - the same shape as the existing
dependency allowlists. It also fixes 3.2 and 5.1 below.

### 3.2 Frame bytes reach the terminal through two implementations, and they have already diverged

`state.rs::try_present_frame` routes through
`frame_output::write_composed_frame` and picks its sink by `cfg`:

```rust
#[cfg(not(test))]
let mut stdout = io::stdout();
#[cfg(test)]
let mut stdout = io::sink();
```

with a comment explaining that a full-screen frame written to the test runner's
real stdout would scribble on the developer's terminal.
`state.rs::present_surface_patch`, forty lines above, writes
`io::stdout().write_all(&encoded.bytes)` unconditionally - no `cfg`, and
bypassing `frame_output` entirely. **This is a live defect, not a prediction:**
any unit test that drives the patch path writes escape sequences to the test
runner's terminal, which is precisely what the sibling comment forbids.
`frame_output::write_composed_frame` is a one-line `write_all` wrapper whose
only caller is the other path, so the "channel" it represents is not a channel.

**Enforce:** yes, and the `cfg`-selected sink is the wrong mechanism - an
injected writer on `ClientState` makes the second path's omission
unrepresentable and removes the `#[cfg(test)]` divergence between what tests
exercise and what production runs.

### 3.3 Operator-facing text is assembled at three sites in two styles

`lib.rs` writes to stderr three times: `eprintln!("shepr: failed to set up terminal: {err}")`,
`writeln!(io::stderr(), "shepr: {notice}")`, `writeln!(io::stderr(), "shepr: {error_message}")`.
Two macros, one prefix spelled three times, one path checking its result and
two discarding it. `errors.rs::ClientError::display_with_context` is the
crate's actual owner of operator-facing failure text and it is good - but the
terminal-setup failure does not go through it.

**Enforce:** partly - one `fn report_to_operator(msg)` collapses the three, and
a text rule banning `eprintln!` outside it is expressible. That the *content*
belongs in `ClientError` is a review matter.

### 3.4 The same field is spelled two ways in tracing calls

`%err` at some sites, `err = %err` at others, `%error` and `error = %message`
elsewhere - `lib.rs` alone uses three of the four. Structured log fields keyed
`err` and `error` for the same thing means no single query finds client
failures.

**Enforce:** yes, by a text rule on the field name, or by funnelling failures
through one helper.

### 3.5 Log lines that omit the identifier an operator needs

In a client that serves several endpoints at once:

- `transport.rs:113` `warn!(err = %err, "server read error")` - no endpoint, no generation. Which machine dropped?
- `state.rs:269` `warn!(%error, "failed to present client frame")` - no endpoint, no frame or surface revision.
- `clipboard_forwarding.rs:11` `warn!("received invalid clipboard payload from server")` - no endpoint, no payload length. "From server" names no server.
- `lib.rs:1197` `warn!(%error, "failed to present retained pane surface patch")` - no pane id, no endpoint.
- `lib.rs:1509` `debug!("received unexpected Welcome in main loop")` - no endpoint.

By contrast `lib.rs:1005` and `lib.rs:1559` do carry
`endpoint = %…storage_key()` and `generation`, so the crate knows what a good
line looks like; five sites do not follow it.

**Enforce:** no, not mechanically. A tracing span per endpoint established once
in the client loop would attach the identifier structurally, which is the
closest thing to enforcement available here and is worth doing.

### 3.6 A protocol violation is a hard error in one place and a `debug!` in another

`handshake.rs` returns `ClientError::UnexpectedWelcome` when a welcome arrives
with the wrong shape. `lib.rs:1509` logs `debug!("received unexpected Welcome
in main loop")` and continues. Same class of event - a peer sending a message
it must not send at that point - two severities, and the second one is below the
default log level.

**Enforce:** no, not mechanically. It is a decision to make once (this project's
posture - same build both sides, no wire compatibility - argues for treating it
as an error) and then hold by review.

### 3.7 Silent on significant events

`terminal_geometry.rs` has three query wrappers -
`query_host_terminal_appearance`, `query_host_terminal_theme`,
`query_host_cell_size` - each `let _ = write_…(io::stdout())`. When a query
fails to go out, the client then waits for a reply that can never come and
falls back to `DEFAULT_CELL_WIDTH_PX`/`DEFAULT_CELL_HEIGHT_PX` (8x16) with no
line logged. Pixel-accurate mouse and resize reporting silently degrade to a
guess. Likewise `restore_terminal_state` discards `host_modes.restore`'s error
(`let _ =`) and only reports `ratatui::try_restore`'s - so a failure to pop the
kitty keyboard stack, the single most user-visible restore failure, is
invisible.

**Enforce:** no, not mechanically; a clippy `let_underscore_must_use` allow-list
is too noisy to be worth it. These are individual fixes.

---

## 4. Errors

### 4.1 `forward_clipboard` reports success when nothing was written

```rust
shepr_termio::host_term::title::write_clipboard_bytes(&bytes);
true
```

`write_clipboard_bytes` returns `()`. Inside, `shepr_platform::write_clipboard`
returns a `bool` that is consumed, and the OSC 52 fallback does
`let _ = stdout.write_all(...)`. So a failed native clipboard tool followed by a
failed terminal write produces `true` from `forward_clipboard`, which the client
reports to the server as a completed clipboard forward. The user's copy silently
did not happen.

**Enforce:** yes, by signature: `write_clipboard_bytes(...) -> io::Result<()>`
(or `-> bool`) makes ignoring the outcome a visible `let _ =` at one site
instead of an invisible `()`.

### 4.2 `present_frame` throws away the result that `try_present_frame` exists to return

`state.rs`: `pub fn present_frame(&mut self, …) { let _ = self.try_present_frame(…); }`.
`try_present_frame`'s doc says callers who care about presentation-sensitive
work use the return value - and `present_frame` is the wrapper that makes not
caring the default. `present_frozen_chrome` and `present_chrome` both go through
it, so every chrome path (machine statuses, diagnostics, overlays, the machine
list) drops presentation failure. `repaint_pending` is set inside on failure, so
it is not lost entirely, but no caller learns.

**Enforce:** partly - deleting `present_frame` and making callers handle the
`bool` is a type-level fix; whether each caller then does the right thing is
review.

### 4.3 `derive_label_from_cwd` swallows every failure mode into one

```rust
.output().ok()
  .filter(|o| o.status.success())
  .and_then(|o| String::from_utf8(o.stdout).ok())
```

`git` missing, `git` failing, non-UTF-8 output, and "this cwd is genuinely not a
repo" all become `None`, which is then read as "not a repo" and changes the
label offered to the user. Nothing is logged.

**Enforce:** no. Individual fix: distinguish "not a repo" (exit 128) from "could
not run git" and log the latter.

### 4.4 `std::process::exit(1)` from inside a library crate, bypassing the CLI's exit-code owner

`shepr-client/src/lib.rs:311`. `src/main.rs` has an `error.exit_code()` owner
used at three sites, and the client library does not use it - it hard-exits
with a literal `1` from library code, skipping every remaining destructor in the
process. The client is also the crate with the most to lose from skipped
destructors (terminal restore), and it works today only because restore is
explicitly run a few lines earlier.

**Enforce:** yes, structurally: returning the failure to `main.rs` and letting
the existing `exit_code()` owner act. A text rule "no `process::exit` outside
`src/main.rs`" is expressible and would have caught this.

### 4.5 Dead `_context` parameter means a failure names no subject

```rust
fn set_handshake_recv_timeout(stream: &LocalStream, timeout: Option<Duration>,
                              _context: &'static str) -> Result<(), ClientError>
```

The one caller passes `"failed to clear client handshake read timeout"`. The
parameter is unused, so the string is dead and the resulting
`ClientError::ConnectionFailed` carries the bare socket error with no indication
that it came from clearing the handshake timeout. The intent to attach context
is visible in the source and does nothing.

**Enforce:** yes - clippy's unused-variable lint is silenced only by the leading
underscore, so a `#![warn]` on underscore-prefixed *parameters* (or simply
deleting the parameter) is the fix. Either use it or drop it.

---

## 5. Tests that prove nothing

### 5.1 `#[cfg(test)]` changes where frames go, so no test covers the production writer

See 3.2. The presentation test surface (`shell/tests/copy.rs` 2492 lines,
`mouse_selection.rs` 1312, `endpoints.rs` 1934) all runs against `io::sink()`
for full frames and against the developer's real terminal for patches. No test
asserts anything about what the production sink receives, and the two paths are
not even the same code.

**Enforce:** yes - an injected writer means the tests assert on a `Vec<u8>` and
run the same code production runs.

### 5.2 Clipboard tests depend on the wall clock and sleep 400 ms

`shell/input/input.rs:1043-1075`: one test sleeps 400 ms inside a fake clipboard
reader and asserts `started.elapsed() < Duration::from_millis(300)`; another
sleeps 10 ms. On a loaded machine the 300 ms assertion is a coin flip, and the
400 ms is paid on every run of the suite. The bounded-read helper takes a
`Duration` and a closure - it is *almost* injectable; only the clock it measures
against is not.

**Enforce:** yes, by injecting the clock into `read_clipboard_text_bounded_with`,
which removes both the sleep and the wall-clock assertion.

### 5.3 The handshake deadline test asserts against a quarter of a 60-second constant

`handshake.rs::an_attempt_deadline_caps_a_silent_peer_below_the_read_timeout`
sets a 200 ms deadline and asserts `elapsed < REMOTE_HANDSHAKE_READ_TIMEOUT / 4`
- i.e. under 15 seconds. A regression that made the deadline 10 seconds late
passes. The assertion should be against the deadline it set, not a fraction of
the value it is trying to prove is not used.

**Enforce:** yes - a test; this one just needs a tighter bound. It also binds two
real sockets and a thread, so it is environment-coupled in the mild sense.

### 5.4 Tests that construct real sockets in the scratch dir and leak them

`handshake.rs::socket_pair` builds a `ScratchDir` with `.keep_until_exit()`
and each test removes the socket file by hand with `let _ = std::fs::remove_file(path)`
after `peer.join()`. A test that panics before that line leaves the socket
behind. The scratch-dir convention exists precisely so cleanup is not
hand-rolled per test.

**Enforce:** yes - let `ScratchDir`'s `Drop` own it instead of `keep_until_exit`
plus manual removal.

### 5.5 `should_enable_host_color_scheme_reports` is an identity function re-exported for tests

```rust
pub(super) fn should_enable_host_color_scheme_reports(enable_client_protocols: bool) -> bool {
    enable_client_protocols
}
```

It is `#[cfg(test)]`-imported in `lib.rs` alongside real helpers. Any test
asserting on it asserts `x == x`: both sides come from the same place. The
function exists so a rule *could* live there; today it holds no rule.

**Enforce:** yes - delete it and inline the boolean, or give it the rule it was
created to hold. A test cannot enforce this; only removal can.

### 5.6 The keybind help test names five entries out of roughly seventy

`keybind_help.rs::help_lists_every_default_pane_binding` - the name claims
"every default pane binding" and the body checks `copy mode` plus the four
`swap pane` directions. It reads as coverage of the whole help screen and is
coverage of five rows. See 6.1 for the enforceable version.

**Enforce:** yes, and not by widening this test - see 6.1.

### 5.7 An unrelated-project issue number and a wall-clock timestamp cited in a test comment

`input/raw_input.rs:2524`: `// Issue #3911, 2026-09-13 07:02:14 UTC: this prefix
timed out, then its tail arrived 33 ms later.` Nobody working in this repository
can look up issue #3911 (shepr is a personal fork with no tracker), and the
timestamp is precise to the second for no purpose. The *behavioural* content of
the comment (a prefix, a 33 ms gap, two idle flushes) is the valuable part and
survives without either citation. Per the documentation rule in AGENTS.md, the
drifting specific should be reworded away rather than updated.

**Enforce:** no - a text rule against `#\d+` in comments would be over-broad.

---

## 6. Guards and claims that have stopped holding

### 6.1 The keybinding help screen is a hand-maintained restatement of the `Keybinds` struct

`keybind_help::keybind_help_groups` enumerates `keybinds.<field>` by hand for
every one of `Keybinds`' 47 fields plus `NavigateKeybinds`' 6. I checked: today
every field does appear, so this is a checkable claim that is true right now and
has nothing holding it. Adding a config key and forgetting the help line
compiles, ships, and is invisible - the key simply has no help entry.

The same function also hard-codes six entries that come from nowhere:
`entry("esc", "back")`, `entry("tab / shift+tab", "cycle pane")`,
`entry("enter", "open workspace")`, `entry("1..9", "switch workspace")`. If any
of those is rebindable, the help is lying; if none is, they are undocumented
fixed keys that the config cannot reach. Worth deciding which.

**Enforce:** yes, at compile time. Destructure `Keybinds { navigate, help,
new_workspace, .. }` exhaustively (no `..`) at the top of
`keybind_help_groups` and build the groups from the bindings. Adding a field
then fails to compile until it is placed. This is the single highest-value
mechanical fix in the scope.

### 6.2 `EndpointTransport`'s default method bodies fail open

```rust
fn disconnect(&mut self) {}
fn flush(&mut self, _deadline: Instant) -> io::Result<()> { Ok(()) }
fn take_error(&mut self) -> Option<io::Error> { None }
```

A transport that forgets `flush` reports every flush as succeeding; one that
forgets `take_error` reports itself permanently healthy to the registry, which
is exactly the signal `local_failure_policy` and the supervisor act on. Three
defaults, three silent no-ops keyed on a name the implementor did not write.
There are few implementors, so the defaults save almost nothing.

**Enforce:** yes - remove the defaults. The compiler then requires each
implementor to state its answer.

### 6.3 `ClientProcessRole::from_env` is the good pattern; nothing holds other env reads to it

`from_env` enumerates the accepted values, treats absent as `Local`, and
*refuses startup* on anything else, including non-UTF-8. That is a rule worth
generalising, and it is the only env read in the two crates that follows it (see
2.5 for the three that do not). Recording it here as the enforceable model
rather than as a defect.

**Enforce:** yes, by moving all env resolution into one launch module and
forbidding `env::var` elsewhere by text rule.

### 6.4 The `modes.rs` mouse-clear list and its test are maintained by hand, together

`DISABLE_HOST_MOUSE_REPORTING_SEQUENCE` lists eight modes; the test
`clears_all_known_host_mouse_modes` loops over the same eight spelled again as
strings. The test restates the constant rather than deriving anything from it,
so adding a ninth mode to the constant and not to the test passes, and adding it
to the test and not the constant fails with a clear message - half a guard.
Meanwhile `\x1b[?1016h` (the *enable* for one of those eight) lives in another
crate (1.2), which is the copy the test cannot see at all.

**Enforce:** partly - deriving the list from a single `const MODES: [&str; N]`
used by both the sequence builder and the test makes the pair structural.

### 6.5 The `MAX_RETRY_DELAY` doc asserts a user-visible promise nothing checks

`supervisor.rs`'s doc comment states that `shepr machine reconnect` tells the
user open clients retry within 30 seconds, and that `ATTEMPT_BUDGET` (25 s) plus
the retry accounting keep that promise. Three separate constants, a fourth in
`shepr-remote` (the 15-second per-command discovery budget the comment cites),
and the CLI's user-facing wording all have to agree. Nothing in the build would
notice any of them drifting. This is a careful, correct comment about an
unenforced invariant - which is the finding.

**Enforce:** yes, partially and cheaply: `const _: () = assert!(ATTEMPT_BUDGET.as_secs() < MAX_RETRY_DELAY.as_secs());`
and a test asserting the CLI's reconnect message quotes `MAX_RETRY_DELAY` rather
than a literal `30`.

### 6.6 `shepr-client`'s `test-support` feature is enabled in the same build as its production code

Root `Cargo.toml` depends on `shepr-client` normally (line 100) and as a
dev-dependency with `features = ["test-support"]` (line 115). Under `cargo test`
/ `cargo build --tests`, feature unification turns `test-support` on for the
library that the binary links, so items gated
`#[cfg(any(test, feature = "test-support"))]` - including
`ClientState::test_new()`, the activation test hooks at `activation.rs:162,323`
and the shell hooks at `endpoints.rs:280,299,387` - are reachable from
`shepr-client`'s own production modules in that build. Nothing prevents a
production code path from calling them; only the fact that none does today.

**Enforce:** partly. Real isolation means moving the helpers into a separate
crate (the `shepr-test-support` pattern the workspace already uses) so the
production module cannot name them. Keeping the feature but forbidding
production callers is not mechanically checkable.

---

## 7. Policy invented per call site

### 7.1 Timeout budgets are computed per site, with five different shapes

- `handshake.rs`: `Instant::now() + read_timeout`, then `min` with an optional caller deadline. This one is right, and it is the only one that composes.
- `endpoint/writer.rs:227`: `Instant::now() + WRITE_TIMEOUT`, then a poll loop with `thread::sleep(IO_POLL_INTERVAL)` checking `Instant::now() >= deadline`.
- `attach.rs:58`: `Instant::now() + Duration::from_secs(5)` inline, an un-named flush budget that happens to equal `WRITE_TIMEOUT`.
- `terminal_setup.rs:125`: `Instant::now() + HOST_KEYBOARD_QUERY_TIMEOUT` with its own `checked_duration_since` remaining-time computation and its own `i32::try_from(...).max(1)` millisecond conversion.
- `activation.rs`: five copies of `Instant::now() + ACTIVATION_TIMEOUT` (2.3).

Five implementations of "a deadline and the remaining time until it", one of
which (the 5 s in `attach.rs`) is an unnamed duplicate of a named constant.

**Enforce:** yes - one small `Deadline` type with `remaining()`,
`remaining_millis_i32()` and a `min` combinator. The type makes the
hand-rolled arithmetic unnecessary; a text rule against
`Instant::now() + Duration::` outside it holds it.

### 7.2 Retry and backoff exist once; the throttle-with-last-sent-timestamp pattern exists three times

Reconnect backoff is properly single-owned in `supervisor.rs` - good. But the
"only send if enough time has passed since `last_sent_at`" rule is
reimplemented three times in `shell/input/mouse.rs` (scrollbar drag at 33 ms,
split drag at 33 ms, selection repaint at `SELECTION_REPAINT_INTERVAL`), each
with its own `last_sent_at` field, its own `is_none_or` comparison, and its own
awkward re-borrow of `self.chrome_drag` to write the timestamp back.

**Enforce:** yes, by a small `Throttle { interval, last: Option<Instant> }`
type with `fn admit(&mut self, now) -> bool`. Three call sites collapse to
three fields of one type, the interval becomes a named construction argument
(fixing 1.6), and the re-borrow dance disappears.

### 7.3 Cleanup on the error path is per-site in `HostModes::restore`

`restore` runs seven independent restores plus title reset plus a title-stack
pop, each in the shape

```rust
if restore_state & FLAG != 0 {
    let next = <write>;
    if result.is_ok() { result = next; }
}
```

- nine hand-written copies of "keep going, remember the first error". The
`restore_state` bitfield is itself maintained by four different recorder methods
(`record_keyboard_restore_state`, `record_keyboard_entry`, `record_restore_flag`,
plus `apply_mouse`'s conditional `record_restore_flag(RESTORE_MOUSE_CAPTURE)`
which only records when `reassert` is true - so a mouse capture enabled without
`reassert` is not recorded for restore).

**Enforce:** yes - a `Vec<(flag, fn(&mut W) -> io::Result<()>)>` table iterated
once, with `first_error`. The nine copies become one loop, and the flag-to-
action pairing becomes data a reader can check at a glance.

### 7.4 Two writers to the host terminal's mode state, never introduced

`HostModes` guards `HostModesState` behind a `Mutex` and `restore_state` behind
an `AtomicU8`, with a comment explaining the split: the panic hook must restore
without taking a lock that may be held by the panicking thread. That is
deliberate and sound for the panic case. But it means the restore *intent* and
the mode *state* are two pieces of shared mutable state kept consistent only by
each setter remembering to call a recorder before and after its write - and
`set_keyboard_enhancement_flags`, `set_direct_keyboard_protocol` and
`set_modify_other_keys` each do that pairing slightly differently (the first
records `false` for modify-other-keys on success unconditionally; the second and
third record the computed value). Whether those three agree is not checkable
from the types.

**Enforce:** no, not mechanically, and the lock-free restore path is worth
keeping. The honest statement is that the recorder pairing is a convention
maintained by three call sites, and a single `set_keyboard(…)` entry point that
computes the flags itself would reduce it to one.

### 7.5 `ClientInputTarget` is a one-variant enum matched at nine sites

`shell/state.rs:493`: `enum ClientInputTarget { Pane(PublicPaneId) }`. Nine
construct/match sites carry a `match` with one arm. Every `match target { … }`
in `shell/input/events.rs` is a rename of a field. It reads as a policy point
("where does input go?") and holds no policy.

**Enforce:** yes, by deletion - replace with `PublicPaneId`. If the enum is
anticipating a second target, nothing says so.

### 7.6 Resources bounded in some places, unbounded in others

Bounded and named: `MAX_NOTICES` (64), `MAX_PENDING_PASTE_BYTES` (16 MiB),
`MAX_QUEUED_BATCHES`/`MAX_QUEUED_BYTES`, `MAX_RETIRED_REQUESTS_PER_ENDPOINT`,
`MAX_ENDPOINT_RESPONSE_BYTES`, `MAX_BUFFERED_HOST_INPUT`,
`MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES`, `MAX_DISCARDED_CONTROL_TAIL_BYTES`. This
crate is unusually good here. Two gaps:

- `EndpointRegistry::failures: Vec<EndpointTransportFailure>` has no cap. It is drained each loop iteration by `take_failures()`, so it is bounded by loop cadence rather than structurally - a transport that produces failures faster than the loop drains grows it.
- `remember_direct_notice` caps at 64 but `MAX_NOTICES` is declared *inside the function body*, so it is invisible to anyone auditing the crate's limits (2.1).

**Enforce:** partly - moving every cap into one place makes the absent ones
visible, which is the enforceable part.

### 7.7 Personal data in operator output

`host_term::title::write_window_title` is careful about control characters and
says so: titles carry cwd and branch text. Two related paths are less careful.
`terminal_geometry.rs:278` logs `width_px`/`height_px` (harmless).
`workspace_label.rs` passes the cwd and `$HOME` into a label, and
`shell/input/word_bounds.rs` tests use `$HOME`-shaped fixtures. No secret
reaches a log in this scope that I found - recording the absence so it is not
re-hunted. The one thing worth noting: `clipboard_forwarding.rs` logs on invalid
payload and correctly does *not* log the payload; if 3.5 is fixed by adding
context there, the fix must add the *length*, not the data.

**Enforce:** no. A note for whoever fixes 3.5.

---

## 8. Code that is no longer load-bearing

### 8.1 `REPEAT_IME_ANCHOR_AFTER_SYNC: bool = true` - a switch with one value, and it says so

`blit.rs:596`, with a doc comment reading "Production always repeats the IME
anchor after the synchronized block; the parameter threaded through the blit
functions exists so tests can check both output shapes." So the constant is
`true` at all three production call sites, and a `bool` parameter is threaded
through the blit call chain to let tests pass `false` - i.e. to let tests
exercise a shape production never produces. The tests asserting the `false`
shape assert nothing about the shipped behaviour.

**Evidence it is dead:** the constant has one value, is never computed, and its
own comment states the parameter exists only for tests.

**Enforce:** deletion. Remove the parameter and the `false`-shape tests; the
`true` behaviour is then unconditional and the hot blit path loses a branch.

### 8.2 `ModifyOtherKeysMode::set_sequence()` - output never written

See 1.1. Its only caller uses `ends_with` on the returned bytes to recover an
integer. Nothing writes the sequence it builds.

**Evidence it is dead:** one caller, which discards the bytes.

### 8.3 `should_enable_host_color_scheme_reports` - identity function

See 5.5. Returns its argument.

### 8.4 `set_handshake_recv_timeout`'s `_context` parameter and the string passed to it

See 4.5. Unused parameter; the string literal at the call site is dead.

### 8.5 `frame_output::write_composed_frame` and `ComposedFrame`

`ComposedFrame` is a newtype over `FrameData` with a `From` and a `Deref`, and
its doc says "Shepr no longer forwards pane images to the outer terminal, so
this is a thin wrapper around the plain text frame" - i.e. it is the residue of
a removed feature. `write_composed_frame` is `writer.write_all(encoded)`. Both
have one caller (`try_present_frame`), and the sibling patch path bypasses them
(3.2). A wrapper whose stated purpose no longer exists, read by everyone after
as a composition boundary.

**Evidence it is dead:** the doc comment names the removed reason for its
existence; the type adds no field and no invariant; the function adds no
behaviour over `write_all`.

**Enforce:** deletion. `shepr-client`'s history has no wire-compat obligation.

### 8.6 `ClientInputTarget` - see 7.5

### 8.7 `shell/presentation/blit.rs` alongside `shepr-termio/src/blit.rs`

Two modules named `blit` doing different things: the termio one encodes a frame
to terminal bytes, the client one copies cells between `FrameData` buffers
(`blit_pane_surface`). Not dead, but the name collision means "the blit code" is
ambiguous in every conversation and every grep, and the client's copy is
pane-surface composition, not blitting. Worth a rename
(`compose_pane_surface`), which is free pre-1.0.

**Enforce:** no - a naming matter, holdable only by review.

### 8.8 `KITTY_FLAG_REPORT_ALL_KEYS` is `pub` in `model.rs` and re-derived in `encode.rs`

`model.rs:110` exports `pub const KITTY_FLAG_REPORT_ALL_KEYS` (not re-exported
from `input/mod.rs`), while `encode.rs:12-14,366` declare four private
`KITTY_FLAG_*` constants of its own from the same bitflags type, including its
own `KITTY_FLAG_DISAMBIGUATE` at line 366, far from the other three at the top
of the file. Five copies of "read a bit out of `KittyKeyboardFlags`" that the
bitflags type already provides via `.contains()`. `KeyboardProtocol::reports_event_types`
even writes the bit as a raw literal `0b0000_0010` while
`reports_all_keys` uses the named constant - two spellings in adjacent methods
of the same impl.

**Enforce:** yes - delete all five constants and call
`KittyKeyboardFlags::contains`. The raw `0b0000_0010` becomes
unrepresentable once the flag type is used directly.

---

## Forced duplication, with the reason

- **Enable and disable of a host mode across a crate boundary.** Nothing forces this today (1.2): both halves could live in `shepr-termio::host_term::modes`. Reporting it as *not* forced, because it looks forced and is not.
- **Keybinding help vs the config struct.** The help text ("copy mode", "swap pane left") is genuinely a second thing - `shepr-config` owns the binding, the help owns the human label. What is not forced is the *list of actions*, which 6.1 makes structural. The labels can then live beside the fields in `shepr-config` or be required by an exhaustive match; either way the *set* is enforced and only the wording stays duplicated by design.
- **`ModifyOtherKeysLevel` in `shepr-vt` vs a client-side notion.** Not forced. `shepr-vt` sits below `shepr-termio` in the documented layering and `shepr-termio` already depends on it, so the termio enum is a free-standing copy, not a layering workaround (1.1).
- **Local and remote hosts each running their own binary.** This does force the build-identity preamble and the `REMOTE_KEYBINDINGS_ENV_VAR` / `REATTACH_COMMAND_ENV_VAR` contract to be spelled in `shepr-remote` and read in `shepr-client`. That is handled correctly: both names are `pub const` in `shepr-remote` and the client reads them rather than spelling the strings (`lib.rs:135`, `handshake.rs:36`, and the five test sites). This is the pattern the rest of the crate should follow, and the preamble check means a drifted pair fails loudly at connect rather than silently. No finding - recorded as the answer to "what keeps forced copies in step".

---

## Lateral findings (outside the eight questions)

1. **Live defect, `state.rs`:** `present_surface_patch` writes to the real
   `io::stdout()` under `cfg(test)` while its sibling deliberately diverts to
   `io::sink()` to avoid scribbling on the test runner's terminal. Any unit test
   reaching the patch path corrupts the developer's terminal. (3.2)

2. **Live defect, clipboard:** `forward_clipboard` returns `true` after a
   `write_clipboard_bytes` that cannot report failure and internally discards
   both of its failure modes. A failed copy is reported as a successful one. (4.1)

3. **Possible restore gap:** `HostModes::apply_mouse` records
   `RESTORE_MOUSE_CAPTURE` only when called with `reassert == true`. Both setup
   paths do pass `true`, so it holds today - but the flag that decides whether
   mouse capture gets turned off at exit is set by a parameter that means
   something else ("re-send even if unchanged"). A future caller with
   `reassert: false` that enables capture leaves the user's terminal in mouse
   mode after shepr exits. (7.3)

4. **`ClientError::display_with_context` is good and under-used.** It is the one
   place in the crate that composes an operator-facing failure with its
   remediation ("Run `{command}` to reattach"), and only two variants use the
   context. The terminal-setup failure (3.3) and the endpoint-attention warning
   both assemble their own text. Worth extending rather than replacing.

5. **`shepr-termio` writes to `io::stdout()` once**, in
   `host_term::title::write_clipboard_bytes`. That is the only place in the
   lower crate that owns terminal output rather than taking a writer, and it is
   also the only one that takes `.lock()`. Both facts point the same way: give
   it a writer parameter and the crate becomes uniformly testable.

6. **`errors.rs`'s `ClientErrorContext` fields are ordered differently in the
   struct and the constructor** (`remote_reattach, local_reattach` in the
   literal; `local_reattach, remote_reattach` in `new`'s signature). Both are
   `String`/`Option<String>` so the compiler catches a swap today, but only
   because the types differ. Cosmetic, listed because the fix is free.

7. **`copy_mode_page_lines(height, half_page)` subtracts a bare `2`** for
   chrome, while `shell/state.rs` names the same quantity
   `WORKSPACE_HEADER_ROWS: u16 = 2`. I could not establish that they are the
   same two rows, so this is flagged as a question rather than a finding: if they
   are the same, it is another 1.x; if not, the `2` in `copy_mode.rs` deserves a
   name saying what it is.
