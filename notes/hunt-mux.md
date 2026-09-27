# Hygiene hunt: `crates/shepr-mux`

Scope read: `src/pane/**`, `src/terminal/**`, `src/workspace/**`, `src/git/**`,
`src/persist/**`, `events.rs`, `render_signal.rs`, `lib.rs`. Values, channels and
rules were followed out of the crate into `shepr-agent`, `shepr-config`,
`shepr-server`, `shepr-platform`, the root binary, `brokkr.toml` and the hook
assets where they led.

Each finding says whether the fixed version can be held mechanically.

---

## 0. Real defects tripped over on the way

These are not hygiene. They are wrong now.

### 0.1 An unparseable hook source silently discards a good persisted agent session

`src/persist/snapshot.rs`, `capture_tab`'s `agent_session` closure:

```rust
let agent_session = terminal.and_then(|terminal| {
    if let Some(authority) = terminal.hook_authority.as_ref()
        && let Some(session_ref) = authority.session_ref.as_ref()
    {
        return Some(PaneAgentSessionSnapshot {
            source: shepr_agent::agent::AgentSource::from_pair(
                &authority.source, &authority.agent_label)?,
            agent: shepr_agent::agent::Agent::parse_canonical_label(
                &authority.agent_label)?,
            session_ref: session_ref.clone(),
        });
    }
    terminal.persisted_agent_session.as_ref().map(...)   // <- unreachable when ? fires
});
```

The two `?` operators return from the **whole closure**, not from the `if` block.
So when a live hook authority carries a source or label that `from_pair` /
`parse_canonical_label` does not recognise, the fallback to
`terminal.persisted_agent_session` is never reached and the pane is saved with
no agent session at all. The pane then restores as a plain shell with no resume.
Nothing is logged. The intended shape is to fall through to the fallback, which
needs the inner expression lifted into its own `Option` and an `.or_else(...)`.

Enforceable: yes, by a test that captures a snapshot from a `TerminalState`
holding both an unrecognised `hook_authority` and a valid
`persisted_agent_session`, and asserts the session survives. The class is also
catchable by a lint against `?` inside a closure whose other arm is a fallback,
but a test is the honest answer here.

### 0.2 `WorkspaceSnapshot::identity_cwd` / `PaneSnapshot::cwd` are `PathBuf` with no validation, and `TerminalState::cwd` is `pub`

`src/pane/cwd.rs` defines exactly the right type:

```rust
pub(super) struct UsableCwd(PathBuf);   // absolute && is_dir
```

and then `src/pane/runtime.rs`'s `usable_reported_cwd` throws the type away one
line later (`UsableCwd::new(cwd).map(UsableCwd::into_path_buf)`), so the
guarantee never propagates anywhere. Meanwhile `TerminalState::cwd` is a bare
`pub PathBuf` written directly from four `shepr-server` sites that do no
checking at all:

- `app/api/workspaces.rs` (two sites)
- `app/api/layouts.rs`
- `app/api/tabs.rs`
- `app/actions/events.rs`

`UsableCwd` is `pub(super)` inside `pane`, so those callers *cannot* use it even
if they wanted to. The one type in the crate that encodes the cwd rule is
unreachable from every place that needs it.

Enforceable: yes, structurally. Promote `UsableCwd` (rename it something like
`PaneCwd`) to the crate root, make `TerminalState::cwd` private behind
`fn cwd(&self) -> &Path` / `fn set_cwd(&mut self, PaneCwd)`, and make
`PaneSnapshot::cwd` deserialize through it. That makes the bad spelling
unrepresentable rather than merely discouraged.

### 0.3 `save_history`'s digest short-circuit trusts a claim the lease does not make

`src/persist/writer.rs::save_history` skips the write when the SHA-256 of the
new history JSON equals `self.written_history`, the digest of what *this writer*
last wrote. The justification is the `DataDirLease` ("only one server writes a
data directory"). The lease excludes another *server*; it does not stop a user or
a cleanup script from deleting `session-history.json`. After such a delete the
writer never rewrites history again for the life of the process, and every
subsequent restore loses scrollback silently.

Enforceable: yes, by a test that deletes the file between two saves with
unchanged history and asserts it comes back - or structurally, by keying the
skip on the file's own (mtime, len, digest) rather than on the writer's memory.

---

## 1. One value, one owner

### 1.1 `SHEPR_ENV` and its resolution rule are spelled three times

| site | spelling |
|---|---|
| `src/main.rs:3-4` | `pub(crate) const SHEPR_ENV_VAR = "SHEPR_ENV"`, `SHEPR_ENV_VALUE = "1"` |
| `crates/shepr-mux/src/pane/launch.rs:1-2` | private `const SHEPR_ENV_VAR = "SHEPR_ENV"`, `SHEPR_ENV_VALUE = "1"` |
| `crates/shepr-pty/src/backend.rs:341` | bare literal `cmd.env("SHEPR_ENV", "1")` |
| `crates/shepr-agent/src/integration/assets/opencode/shepr-tui-session.js` | `process.env.SHEPR_ENV !== "1"` |

mux is the writer, the binary is the reader, and the *rule* (`== Some("1")`
exactly, in `should_block_nested_for_env`) is restated by the JS hook as
`!== "1"`. Neither copy of the name nor of the value can see the other. The
value happens to agree today; nothing keeps it agreeing. The name belongs in
`shepr-config` beside `SOCKET_PATH_ENV_VAR` and `SESSION_ENV_VAR`, which mux
already imports in the same function.

Enforceable: for the Rust copies, yes - a single `pub const` in `shepr-config`
plus a text rule in `brokkr.toml` forbidding the literal `"SHEPR_ENV"` outside
that file. For the hook assets, no: they run inside another agent's process and
must carry the literal. What keeps them in step is a test that greps the shipped
assets for `SHEPR_*` literals and asserts each is one of the exported constants
(see 1.2).

### 1.2 `SHEPR_BIN_PATH`, `SHEPR_PANE_ID`, `SHEPR_TAB_ID`, `SHEPR_WORKSPACE_ID` have no single owner

`src/pane/launch.rs` exports `SHEPR_PANE_ID_ENV_VAR` publicly (and the CLI's
`cli/target.rs` correctly reads it through that), keeps `SHEPR_TAB_ID_ENV_VAR`
and `SHEPR_WORKSPACE_ID_ENV_VAR` private, and writes `"SHEPR_BIN_PATH"` as a
bare literal. `shepr-server/src/app/tab_bar_status.rs` writes `"SHEPR_BIN_PATH"`
as a bare literal too, so there are two independent writers of one name with no
shared definition. Roughly twenty shipped hook assets read both names as
literals. `SHEPR_TAB_ID` and `SHEPR_WORKSPACE_ID` have **no reader anywhere** in
the repo or the assets - see 8.3.

Enforceable: yes. One module (`shepr-config`) exporting every `SHEPR_*` name, a
`brokkr.toml` text rule forbidding `"SHEPR_` string literals elsewhere, and one
test that walks `crates/shepr-agent/src/integration/assets/**` extracting
`SHEPR_[A-Z_]+` and asserts set membership. That last test is the only thing
that can ever keep the deployment-forced copies honest, and it does not exist.

### 1.3 `TerminalState::revision` is bumped at four sites under two different overflow policies

- `src/terminal/state/detection.rs:40` - `self.revision.wrapping_add(1)`
- `src/terminal/state/detection.rs:81` - `self.revision.saturating_add(1)`
- `shepr-server/src/app/actions/workspace.rs:109` - `saturating_add(1)`
- `shepr-server/src/app/api/panes/reports.rs:229` - `saturating_add(1)`

Two spellings, forty lines apart, of one counter's increment. Already diverged:
`wrapping_add` and `saturating_add` disagree at `u64::MAX`. Neither is obviously
right, which is the point - nobody chose.

Enforceable: yes, by making the field private with a single
`fn bump_revision(&mut self)`. A `u64` counter's overflow behaviour then has one
answer by construction.

### 1.4 `"session-history.json"` has two owners

`src/persist/io.rs:13` defines `session_history_path(data_dir)`. It is private,
and `src/persist/writer.rs` never calls it - it derives the path itself at three
sites with `self.path.with_file_name("session-history.json")` (lines 134, 226,
plus the test at 707+). So the writer and the loader each spell the filename.

Same shape, worse, for `"session-snapshots"`: five production spellings in
`writer.rs` (lines 176, 240, 276, 286, 330, 383), and one of them is a
**behavioural guard** - see 6.1.

Enforceable: yes. Consts in `io.rs`, and `writer.rs` calling `io::` accessors.
Holdable by a text rule once the literals are gone.

### 1.5 The recovery-filename timestamp width is spelled twice

`src/persist/writer.rs`:

```rust
format!("session-{timestamp:039}-{}-{sequence}.json", std::process::id())  // line ~366
...
fields[0].len() == 39                                                       // line ~459
```

The writer's zero-padding width and the reader's length check are the same
number, 90 lines apart, with no shared name. Change one and every existing
recovery copy becomes invisible to pruning and to `prepare_snapshot_history`,
silently - `recovery_files` just skips names it does not parse.

Enforceable: yes, a `const RECOVERY_TIMESTAMP_DIGITS: usize = 39;` used in both
the format string (`{timestamp:0width$}`) and the check, plus a round-trip test.

### 1.6 Three boolean-from-string parsers, no owner

- `src/git/config.rs:219` - `"true" | "1" | "yes" | "on"` (Git's boolean syntax)
- `src/pane/osc.rs:248` - `"1" | "true" | "yes" | "on"` (a shepr env var)
- `shepr-remote/src/remote/server_lifecycle.rs:220` - `"y" | "yes"` (a prompt)

The first two are the same list in a different order for two different domains.
Git's actual boolean syntax also accepts the empty string as true and
`off`/`no`/`false` as false, so the `git/config.rs` copy is an incomplete
implementation of a documented external grammar while the `osc.rs` copy is
shepr's own invention that happens to look identical.

Enforceable: partly. The two domains genuinely differ, so this is two owners not
one: a `git_bool()` in the git module (completed against Git's grammar) and one
`env_bool()` wherever shepr env flags are resolved. Holdable by a text rule
forbidding the bare list elsewhere.

### 1.7 Braille spinners are hard-coded next to a manifest-owned glyph set

`src/terminal/title.rs`:

```rust
let recognized = matches!(first, '\u{2800}'..='\u{28ff}')
    || shepr_agent::agent::Agent::all().any(|a| a.activity_glyphs().contains(first));
```

"What counts as an activity glyph" has two owners: the detection manifests
(`activity_glyphs`) and this literal range. A manifest that lists a braille
glyph is silently redundant; a spinner style outside braille that nobody adds to
a manifest is silently not stripped.

Enforceable: yes - move the braille range into the manifest schema (or into a
shared `shepr-agent` const) so `activity_glyphs` is the only answer, and assert
in a test that no manifest glyph falls inside a range the code also hard-codes.

### 1.8 The teardown escalation budget and the process-exit wait budget are unrelated numbers in different crates

`src/pane/teardown.rs` spells `Duration::from_millis(250)` three times inside
`PANE_TEARDOWN_STEPS` (one value, three spellings, and the 750 ms total is
nowhere named). `shepr-server/src/server/headless.rs:590` waits
`Duration::from_secs(3)` for those teardowns to finish. The 3 s must exceed the
750 ms plus however long two `/proc` session scans take; that relationship is
stated nowhere and the two numbers cannot see each other.

Enforceable: yes - export the total from `teardown.rs` (`pub const
PANE_TEARDOWN_BUDGET`) and have the server derive its wait from it, with a
compile-time or test assertion that the wait is the larger.

### 1.9 The `Unknown` presents as `Idle` rule has four statements and two implementations

`AGENTS.md` states it. `shepr-agent/src/detect/mod.rs:35` implements it
(`attention_rank`). `shepr-server/src/app/api_helpers.rs:92` implements it again
(`pane_agent_status`). `src/workspace/aggregate.rs:35` documents it as happening
"at the API edge" - a claim about a different crate. Consistent today; two
implementations of a three-case mapping with nothing tying them.

Enforceable: yes, one function in `shepr-agent` used by both, and the doc comment
in `aggregate.rs` deleted rather than restated.

### 1.10 Two pending-file naming conventions in one module

`src/persist/io.rs` publishes through `target.with_extension("json.tmp")`;
`writer.rs::copy_recovery` publishes through
`backup.with_extension("pending")`. Both go through the same
`publish_private_file`, so the crash-recovery reasoning in
`remove_stale_temporary` (which only knows about `json.tmp`) covers one of them
and not the other: a crash between create and rename in `copy_recovery` leaves a
`.pending` file that nothing ever cleans up, and the next attempt at that exact
name fails `AlreadyExists` and burns one of the 128 sequence slots.

Enforceable: yes, one suffix const passed into `publish_private_file`, plus
extending `remove_stale_temporary` to both. Testable.

---

## 2. Values nobody can find, change, or trust

### 2.1 There is no answer to "what are this crate's tunables"

Thirty-plus tuning constants, each defined where it was first needed, across
seven files. Nothing in `reference/` or `docs/` enumerates them, and none is a
config key:

- `src/pane/process_probe.rs`: `RELEASE_REACQUIRE_SUPPRESSION`,
  `AGENT_MISS_CONFIRMATION_ATTEMPTS`, `PROCESS_RECHECK_IDENTIFIED`,
  `PROCESS_RECHECK_MISSING_FOREGROUND_GROUP`, `PROCESS_ACQUISITION_WINDOW`,
  `PROCESS_ACQUISITION_FAST_WINDOW`, `PROCESS_ACQUISITION_FAST_RECHECK`,
  `PROCESS_ACQUISITION_SLOW_RECHECK`, `PROCESS_ACQUISITION_IDLE_RESET` - nine
  timing knobs governing one state machine, none referenced outside the file.
- `src/pane/agent_detection.rs`: `AGENT_PENDING_IDLE_RECHECK`,
  `AGENT_PENDING_IDLE_CONFIRMATIONS`, `AGENT_PENDING_IDLE_CAP`,
  `STABLE_VISIBLE_SIGNAL_REFRESH`, `AGENT_STARTUP_GRACE_WINDOW`,
  `AGENT_ABSENCE_STARTUP_HOLD` (aliased to `MANAGED_AGENT_RESUME_TIMEOUT`).
- `src/terminal/history_read.rs`: `MIN_ALIGNMENT_RATIO_PERCENT = 30`,
  `SIMILAR_VIEWPORT_RATIO_PERCENT = 70` - two similarity thresholds governing
  whether restored history is stitched onto live output, with no stated basis.
- `src/terminal/metadata.rs` / `metadata_tokens.rs`: `MAX_METADATA_SOURCES = 64`,
  `MAX_STATE_LABELS_PER_SOURCE = 16`, `MAX_SEQUENCE_SOURCES = 32` - three
  unrelated caps on the same family of untrusted input.
- `src/pane/osc.rs`: `MAX_BODY_BYTES = 4096`, `AGENT_OSC_MAX_CHARS = 256`, a
  second `MAX_CHARS = 512` inside `sanitized_osc_debug_payload`.
- `src/pane/terminal.rs`: `DEFAULT_DETECTION_ROWS`,
  `SYNCHRONIZED_OUTPUT_FLUSH_MARGIN`, `SCAN_CHUNK_ROWS`,
  `COPY_MODE_WORD_SEPARATORS`.
- `src/persist/writer.rs`: `SNAPSHOT_INTERVAL` (15 min), `SNAPSHOT_LIMIT` (48)
  - 12 hours of recovery, a number nobody wrote down - plus a bare `3` inline in
  `preserve_existing` for the *other* recovery directory's limit. Same concept,
  one named and one not.
- `src/git/status.rs`: `GIT_STATUS_RETRY_DELAY` (30 s),
  `src/git/discovery.rs`: `MAX_GIT_REF_FILE_BYTES`.
- `src/terminal/state/mod.rs`: `HOOK_SEQUENCE_REANCHOR_AFTER`.
- Unnamed bounds: `for _ in 0..16` (symlink hops, `io.rs`), `for sequence in
  0..128` (recovery name attempts, `writer.rs`), `.take(256)` (palette,
  `snapshot.rs`).

None of this is a duplication finding - each is defined once. The finding is
that a person trying to make agent detection less twitchy has nine files to read
and no index.

Enforceable: partly. A `src/tuning.rs` (or a `reference/tuning.md` generated
from it) collecting every constant with its unit and its reason is holdable by a
text rule forbidding bare `Duration::from_*` and numeric caps outside that
module in production code. Whether any of them should become config keys is a
judgement call the code cannot reveal.

### 2.2 No clock injection anywhere in `persist`, so the tests fabricate mtimes

`src/persist/writer.rs` reaches `SystemTime::now()` directly at four sites
(`preserve_snapshot_history`, `prepare_snapshot_history`, `preserve_existing_in`
twice). There is no injection point, so the 15-minute snapshot gate cannot be
tested except by lying about file mtimes - which the test at `writer.rs:624`
does: `.set_modified(SystemTime::now() + Duration::from_secs(86400))`. A test
that sets a file's mtime a day in the future to walk past a gate is a report
that the gate has no seam.

By contrast `src/terminal/state/**` and `src/pane/process_probe.rs` thread
`now: Instant` through every entry point and are cleanly testable. So the crate
already knows how to do this; `persist` just does not.

Enforceable: yes - take `now: SystemTime` as a parameter the way the state layer
takes `now: Instant`, and delete the mtime fabrication. Holdable by a text rule
against `SystemTime::now()` / `Instant::now()` in `src/persist/`.

### 2.3 `SHEPR_DEBUG_OSC_EVIDENCE` is resolved at pane creation, not at launch

`src/pane/osc.rs`:

```rust
impl Default for OscDebugTracker { fn default() -> Self { Self::from_env() } }
```

`OscDebugTracker::default()` is reached from `GhosttyPaneCore` construction, so
the process environment is read once per pane rather than once at startup. The
project's stated rule is "Config is read and validated once at launch. There is
no reload." This knob reloads on every new pane, and a typo
(`SHEPR_DEBUG_OSC_EVIDENCEE=1`, or `=yes please`) is not a launch failure but a
silent no-op discovered hours later. It is also undocumented: it appears in no
`docs/`, no `reference/`, and no `brokkr man` page.

Enforceable: yes. Resolve it in the same pass as the rest of the config, carry it
in the validated config down to pane construction, and hold the rule with a text
rule against `std::env::var` outside `shepr-config` (the crate already has one
such convention in spirit - `shepr-config/src/io.rs` owns XDG resolution - but
nothing enforces it; see 2.4).

### 2.4 `src/git/config.rs` re-resolves `XDG_CONFIG_HOME` and `HOME` behind `shepr-config`'s back

`git_user_config_paths()` reads `XDG_CONFIG_HOME` directly, filters on
`is_absolute()`, and falls back to `~/.config/git/config`. That is a fourth
independent implementation of the XDG resolution rule, alongside
`shepr-config/src/io.rs::platform_xdg_dir`, `shepr-agent/src/integration/env.rs::absolute_xdg_home`,
and `shepr-core/src/pathutil.rs::home_dir`.

Here the duplication has an answer: these are *Git's* paths, not shepr's, and
the comment in the file says so. But the answer is incomplete in a way that has
already produced divergence:

- `git_user_config_paths` does not honour `GIT_CONFIG_GLOBAL`,
  `GIT_CONFIG_SYSTEM` or `GIT_CONFIG_NOSYSTEM`, and never reads
  `/etc/gitconfig`.
- The `git` subprocess invoked from `src/git/discovery.rs` and
  `src/git/status.rs` *does* honour all of them.

So for one repository the file-reading path and the subprocess path can report
different upstreams and different `core.bare`. This is a divergence in fact, not
a prediction.

Enforceable: partly. The set of Git config sources is Git's contract, so the
only mechanical hold is a test comparing `read_config`'s answer against
`git config --get` output for a repo with a global config and a `GIT_CONFIG_GLOBAL`
override. Worth writing; it will fail today.

### 2.5 Two Git config parsers in one module, and the naive one decides two real questions

`src/git/discovery.rs` has `read_git_config_value` / `simple_git_config_section`
/ `strip_git_config_comment`: ~40 lines that read one key from one file, skip
any `[section "subsection"]` header, and know nothing about `include.path` or
`includeIf`. `src/git/config.rs` is 784 lines implementing the real thing -
includes, conditional includes, `config.worktree`, user and repo precedence,
dependency stamping.

Two questions are answered by the naive parser:

- `git_dir_is_bare` → `core.bare`
- `git_ref_storage_is_reftable` → `extensions.refstorage`

Both are read from exactly one file, so `core.bare = true` set via an
`include.path` or in `~/.gitconfig` is missed, and a reftable repo is then read
with the loose-file path (`read_head_identity_from_files`) and reports no branch.
That is a behavioural divergence between two parsers in the same directory.

Enforceable: yes. Delete `read_git_config_value` and route both questions through
`config.rs`'s reader; hold it with a test using an `include.path` and a
`[extensions]` block in an included file.

---

## 3. One channel, one implementation

### 3.1 `src/persist` writes operator diagnostics through two channels in one function

`src/persist/writer.rs::finish_save_with_snapshot_plan` uses the project's owned
channel for save outcomes:

```rust
shepr_platform::logging::session_save_failed(&self.path, &err.to_string());
shepr_platform::logging::session_saved(&self.path, snapshot.workspaces.len());
```

and raw `tracing` for the snapshot-preservation outcomes in the same call path:

```rust
tracing::warn!(event = "persist.snapshot", outcome = "error", path = %..., err = %err, ...)
tracing::info!(event = "persist.backup", subsystem = "persist", outcome = "ok", ...)
```

So a failed save is an operator event and a failed recovery copy is a log line,
for no stated reason. The `tracing` calls are also inconsistent among themselves:
`persist.snapshot` omits `subsystem = "persist"`, `persist.backup` includes it.

Enforceable: yes - add `session_snapshot_failed` / `session_snapshot_preserved`
to `shepr_platform::logging` and hold it with a text rule banning `tracing::` in
`src/persist/`.

### 3.2 The same class of event is logged with full fields in one function and bare in the next

`src/persist/io.rs`, twenty lines apart:

```rust
// load()
warn!(event = "persist.restore", subsystem = "persist", outcome = "read_error",
      path = %path.display(), err = %err, "failed to read session file");
// load_history()
warn!(err = %err, "failed to read session history file");
```

The history line names no path, no event, no subsystem, and no outcome. An
operator seeing it cannot tell which session directory failed - which matters
precisely because named sessions put the file somewhere non-obvious. Same for the
parse-error pair.

Enforceable: yes, and cheaply: one helper taking `(path, err, outcome)`. A lint
cannot check field completeness, but a helper with required parameters can.

### 3.3 Two operator-facing restore failure messages, assembled at the site

`src/persist/restore.rs`:

- line ~480: `"Saved directory is unavailable. Restore the directory and restart this session."`
- line ~591: `"Could not start the saved shell: {e}. Fix the shell configuration and restart this session."`

Both land in `TerminalState::restore_error: Option<String>` and are rendered by
`shepr-server/src/ui/panes.rs`. Nothing owns how shepr talks to an operator here:
the strings are built at the failure site, in a crate that has no other
user-facing text, and the second interpolates a raw `io::Error` `Display` into a
sentence. The test in `ui/panes.rs:523` invents a *third* wording
(`"Saved directory is unavailable. Restart to retry."`) and asserts against its
own invention, so it would not notice either production string changing.

Enforceable: yes, by making `restore_error` a typed enum
(`RestoreFailure::DirectoryUnavailable { path }` / `ShellStartFailed { err }`)
and putting the wording in whatever owns presentation. Holdable by the type: a
`String` field invites ad-hoc text, an enum does not.

### 3.4 Significant things that log nothing

- `AgentSource::from_pair` returning `None` - five sites in mux
  (`terminal/state/hooks.rs` ×3, `sessions.rs`, `persist/snapshot.rs`) all fall
  through silently. A hook reporting an unrecognised source is indistinguishable
  from no hook at all, in the logs and on screen. See 6.2.
- `PaneTerminal::seed_history_ansi` returns `()` and silently does nothing when
  the core lock is poisoned - restored scrollback is lost with no line anywhere.
- `GhosttyPaneTerminal::resize`, `scroll_up`, `scroll_down`, `scroll_reset`,
  `set_scroll_offset_from_bottom` all use `if let Ok(mut core) = lock_terminal_core(...)`
  and drop the operation on a poisoned lock. The doc comment on
  `GhosttyPaneTerminal::core` justifies this policy for *readers* ("readers
  answer empty or default values rather than error"); it says nothing about
  writers, and a dropped resize is not the same as a stale read.
- `src/git/discovery.rs::git_trimmed_stdout` and
  `src/git/status.rs::git_ahead_behind_between` swallow spawn failure, non-zero
  exit and non-UTF-8 output into `None`. If `git` is not on the server's `PATH`
  the sidebar simply shows no branch, forever, with no diagnostic. See 4.1.

Enforceable: the lock cases, yes - a `#[must_use]` result or a helper that logs
once per pane per poisoning. The `from_pair` case needs a `warn!` at each site
plus the asset test from 1.2.

---

## 4. Errors

### 4.1 Git failure is uniformly `Option::None`, so nothing ever reaches an operator

Every Git read in the crate returns `Option`. Spawn failure, non-zero exit,
non-UTF-8 output, a permission error on `.git/config`, a 64 KiB-exceeding ref
file - all become `None`, and `None` means "this repo has no branch". The
`RefFileRead` enum in `discovery.rs` is a striking exception: it goes to real
trouble to distinguish `Absent` from `Unavailable` (with a careful comment about
`Path::exists()` lying on metadata errors), and then `read_git_ref_file`
immediately collapses both to `None`:

```rust
match read_git_ref_file_state(path) {
    RefFileRead::Content(c) => Some(c),
    RefFileRead::Absent | RefFileRead::Unavailable => None,
}
```

The one place in the module that models the distinction has exactly one caller,
which throws it away. `git_status_snapshot_for_cwd_with_demand` then applies the
same 30-second retry to "not a repo" and to "git is broken", so a missing `git`
binary is retried forever, quietly, once per workspace per 30 s.

Enforceable: yes. A `GitReadError` travelling to the refresh task, logged once
per distinct cause rather than per attempt, with `Absent` staying `None`. The
absence of an operator-visible signal is not lintable; the enum is.

### 4.2 `impl Deref for Workspace` panics on state the API can reach

`src/workspace.rs`:

```rust
impl Deref for Workspace {
    fn deref(&self) -> &Tab {
        self.tabs.get(self.active_tab)
            .expect("workspace must have a tab when implicitly dereferenced")
    }
}
```

Every `Tab` method is silently available on `Workspace`, and the one-tab
invariant is enforced by an `expect` in a `Deref` impl - the least visible
possible place for an abort. `active_tab` is also `pub`, and `tabs_mut()` handns
out `&mut [Tab]` to any crate, so a server-side caller can put `active_tab` out
of range and the next `ws.panes` (which reads as a field access) aborts the
server.

Enforceable: yes, and the fix is deletion. Remove the `Deref`/`DerefMut` impls,
make `active_tab` private, and require `active_tab()` / `active_tab_mut()`
(which already exist and return `Option`). That turns an abort into a refusal
and makes the invariant structural rather than asserted. Pre-1.0, breaking every
`ws.<tab method>` call site is the correct price.

### 4.3 `TabBarCommandFinished` carries `Result<Option<String>, String>` across an internal channel

`src/events.rs`. A stringly-typed error crossing a channel inside one process:
the subject (which command, which segment's configured argv) is not in the error,
only in the sibling `segment_index` field, and by the time an operator sees the
text neither is attached. Compare the rest of `AppEvent`, which is fully typed.

Enforceable: yes, a typed error carrying the segment's configured command.

### 4.4 `restore_error: Option<String>` (same shape, see 3.3)

A public `String` field is the crate's way of reporting a restore failure. Two
producers, one renderer, three wordings, no subject in the type.

---

## 5. Tests that prove nothing

### 5.1 `capture_bounded_migration_observations` cannot fail

`src/pane/terminal/migration_tests.rs`, last test in the file:

```rust
let mut observations = Vec::new();
terminal.write(MIXED.as_bytes());
observations.push(terminal.observe());
for (width, height) in [(8, 4), (17, 6), (12, 5)] { terminal.resize(...); observations.push(terminal.observe()); }
terminal.write(b"\x1b[6n\x1b[?2004h\x1b]52;c;aGk=\x07\x07");
observations.push(terminal.observe());
terminal.pane.scroll_up(2);  observations.push(terminal.observe());
terminal.pane.scroll_reset(); observations.push(terminal.observe());
if let Some(path) = std::env::var_os("SHEPR_MIGRATION_OBSERVATIONS") { std::fs::write(path, ...); }
assert_eq!(observations.last().expect(...), &terminal.observe());
```

The only assertion compares the last observation against observing the same
unchanged terminal again. Both sides come from the same place; it passes for any
behaviour the emulator could possibly have. It reads as coverage of eleven
semantic dimensions across four geometries and asserts none of them. The real
oracle - a human setting `SHEPR_MIGRATION_OBSERVATIONS`, running two builds and
diffing - has no "old" build to compare against any more (see 8.1), so the file's
header comment ("Keep the same runner for old/candidate captures") describes a
workflow that cannot be performed.

Enforceable: yes, and the honest fix is either a committed golden fixture
compared by the test, or deletion of the test and the env var. The file's other
six tests are real and should stay.

### 5.2 Tests depend on the host's `git`, `bash` and `/bin/sh`

- `src/git/test_support.rs::run_git` shells out to `git` and `.expect("test
  precondition")`s the spawn. `init_repo_with_commit`, `create_repo_with_linked_worktree`
  and `create_bare_repo_with_linked_worktree` all need `git worktree` and
  `git clone --bare`, i.e. a reasonably modern `git` installed on the machine
  running the suite. `live_git_space` exercises production code that shells out
  again.
- `src/pane/terminal/migration_tests.rs:128` - `PtyCommand::new("bash")`.
- `src/pane/runtime.rs:2131, 2218, 2279` - `/bin/sh`.
- `src/pane/runtime.rs:2282` and eight sites in `persist/restore.rs` and
  `workspace.rs:1061` use `std::env::current_dir()` as a test cwd, so the tests
  depend on where the runner was invoked.

The `git` dependency is the sharpest: nothing this repository builds provides it,
its version governs reftable and worktree behaviour (which is exactly what the
tests check), and `brokkr check` is the gate. A machine without `git` - or with
one old enough to lack `extensions.refstorage` - fails or silently passes
differently.

Enforceable: partly. `/bin/sh` on a Linux-only project is a defensible
dependency. The `git` one is not: the fixture repos can be written as files (the
crate already has `write_fake_tracked_repo` doing exactly that), and the handful
that genuinely need a real `git` should assert the binary's presence and version
up front so a missing one is a failure with a subject rather than a spawn panic.
Holdable by a test-support helper that all of them must go through.

### 5.3 A test that skips itself and reports success

`src/pane/runtime.rs:2153`:

```rust
eprintln!("skipping untraversable cwd assertion for privileged test process");
```

inside `process_cwd_does_not_require_traversing_the_directory_path`. Run as root
(containers, CI images) the assertion does not run and the test passes green. The
notice goes to stderr, which the test harness hides on success, so nothing
reports it. This is also the crate's only `eprintln!` in a source file - the
project has no channel for "test was skipped", so one was invented.

Enforceable: partly. Rust's harness has no skip state, so the mechanical answer
is to make the test not need privilege separation (run the assertion in a
subprocess that drops privileges, or assert the platform behaviour rather than
the effect), or to fail when running privileged so the condition is loud.

### 5.4 Tests that clean up by hand, so a failure leaks the directory

`src/git/test_support.rs::temp_test_dir` returns
`ScratchDir::new(name).keep_until_exit()` and the doc says "callers that clean up
remove it themselves". Callers then do
`std::fs::remove_dir_all(base).expect("test precondition")` *after* their
assertions (`git/status.rs:412`, `workspace.rs:1487`, `workspace.rs:1503`). Any
failing assertion skips the cleanup. `keep_until_exit()` is being used to opt out
of the crate's own RAII scratch directory for no stated reason.

Enforceable: yes - hold the `ScratchDir` guard in a binding and delete
`keep_until_exit()` and every manual `remove_dir_all`. Holdable by a text rule
against `remove_dir_all` in test modules.

### 5.5 The gate may never compile the feature set that ships

Root `Cargo.toml` has `shepr-server` in `[dependencies]` without features and in
`[dev-dependencies]` with `features = ["test-api"]`, and
`shepr-server/Cargo.toml`'s `test-api` pulls in `shepr-mux/test-api`. Because
both edges are on the same package, any `--all-targets` build (which is what a
clippy-plus-tests gate does) unifies the features, so `brokkr check` builds
`shepr-mux` and `shepr-server` with `test-api` **on**. The configuration that
`brokkr install` ships - `test-api` off - is compiled by no gate step. A
`#[cfg(not(feature = "test-api"))]` path, or code that accidentally depends on a
test-only item, would not be caught.

This matters more than usual here because `test-api` is not cosmetic in
`shepr-mux`: it adds a whole variant to a production enum (`PaneRuntimeIo::TestChannel`,
see 7.5), plus `Workspace::clear_tabs_for_test`, `PaneRuntimeRegistry::drain`,
`TerminalState::set_detected_state`, and nine `PaneRuntime::test_*` constructors.

Enforceable: yes - add a `[[check]]` entry to `brokkr.toml` that builds the
workspace with default features and no `--all-targets`, so the shipped feature
set is compiled by the gate.

### 5.6 Fixed `/tmp` paths inside test data

`src/persist/writer.rs`'s `snapshot()` helper builds JSON containing
`"identity_cwd": "/tmp/shepr-writer-test"`, and `src/workspace/aggregate.rs`'s
tests use `"/tmp".into()` as a `TerminalState` cwd. Nothing is written there, so
the project's "never read or write from /tmp" rule is not broken in effect - but
these are fixed shared paths standing in for a scratch directory, and
`restore.rs` tests do build real cwds. If validation is ever added at the
snapshot boundary (see 0.2) these fixtures become load-bearing on `/tmp`
existing and being a directory.

Enforceable: yes, a text rule against `/tmp` literals in the crate, with the
scratch helpers as the replacement.

---

## 6. Guards and claims that have stopped holding

### 6.1 A guard keyed on a directory-name string literal

`src/persist/writer.rs::preserve_existing_in`:

```rust
if let Err(err) = prune_backups(&older, keep) {
    if directory_name == "session-snapshots" {
        std::fs::remove_file(&backup)?;   // snapshot copies: pruning failure is fatal
        return Err(err);
    }
    tracing::warn!(...);                  // session-backups: pruning failure is a warning
}
```

Two different error policies selected by string comparison against one of the
five spellings of `"session-snapshots"` (1.4). Rename the directory at the four
call sites and forget this one, and snapshot pruning failures silently become
warnings - the directory grows without bound and nothing says so.

Enforceable: yes, and the fix removes the string entirely: pass a
`PrunePolicy { Fatal, Warn }` alongside `keep`. The bad spelling becomes
unrepresentable.

### 6.2 Ten hook-authority guards keyed on strings that arrive from shipped shell scripts

`shepr_agent::detect::full_lifecycle_hook_authority(source, agent_label)` is
`AgentSource::from_pair(source, agent_label).and_then(|s| s.agent()).is_some_and(...)`.
An unrecognised `(source, agent_label)` pair yields `false`. mux calls it, or the
sibling `session_identity_only_integration`, at ten sites across
`terminal/state/hooks.rs`, `lifecycle.rs` and `sessions.rs`, and calls
`from_pair` directly at five more.

Both strings originate in the hook assets shipped into other agents' own config
directories (`crates/shepr-agent/src/integration/assets/*/shepr-agent-state.sh`
and friends). Those files must carry the literals - the deployment constraint is
real. What is missing is anything keeping them in step: rename or typo a
`source` in one asset and every one of the fifteen guards downgrades that agent
from hook-authoritative to screen-detected, silently, with no log line at any
site (3.4). The failure mode is "the sidebar became less accurate", which is
exactly the kind of regression nobody bisects.

Enforceable: yes, by a test that extracts every `source` / agent-label literal
from the shipped assets and asserts `AgentSource::from_pair` accepts each one.
Combined with the `SHEPR_*` asset test from 1.2, that is the whole mechanical
answer to "the hook scripts are a forced second copy". Neither test exists.

### 6.3 `unregister_moved_pane` is a guard that vanishes in release

`src/workspace.rs:797`:

```rust
pub fn unregister_moved_pane(&mut self, _pane_id: PaneId) {
    // `take_pane_for_move` removes the pane record and its public number
    // together; the API still calls this to acknowledge that removal.
    debug_assert!(self.pane_state(_pane_id).is_none());
}
```

Called from production at `shepr-server/src/app/api/panes/geometry.rs:832`. In a
release build (`brokkr install`) `debug_assert!` compiles away and this is a
`&mut self` method that does nothing - so the shipped binary takes a mutable
borrow of the workspace to acknowledge something. The comment describes the
function as a courtesy call. It is either an invariant check that should be a
real `assert!` (or should return `Result`), or it is dead.

Enforceable: yes either way - promote to `assert!`/`Result`, or delete the
method and the call site.

### 6.4 `io::load` and `load_history` claim a lease they do not require

`src/persist/io.rs:226`: *"Reads the saved layout for restore. The server
acquires a DataDirLease before calling this, so native agent sessions cannot be
restored twice."* And `io.rs:146`: *"Removing it is safe because only one server
writes a data directory: `SessionWriter` owns the directory lease before any
write."*

`SessionWriter::new(lease, ...)` does take the lease by value - that half is
enforced by the type. `load(data_dir: &Path)` and `load_history(data_dir: &Path)`
are `pub`, take a bare path, and enforce nothing. The claim is true today only
because the one caller happens to do it.

Enforceable: yes, trivially - `load(lease: &DataDirLease)`. The claim then holds
by signature and the comment can be deleted. `lock::LOCK_FILE_NAME` would also
stop needing to be reachable.

### 6.5 The one-tab workspace invariant is enforced by opt-in test calls and one `Deref` panic

`Workspace::assert_invariants_for_test` (150 lines, `#[cfg(any(test, feature =
"test-api"))]`) is the real statement of the invariant: non-empty tabs,
`active_tab` in range, unique public tab and pane numbers, layout pane set
exactly equal to the pane record set, focused pane in layout, root pane present.
It is called from 25 places, all of them individual tests that chose to call it.
Nothing calls it after a mutation in production, and no mutating method calls it.
The recent commit "Enforce the one-tab workspace invariant" gets its enforcement
from the `Deref` panic (4.2) plus these opt-in calls.

Which are checkable: all of them, and they are already written down as
assertions - the gap is only *when* they run. Which are false today: none that I
could find by reading; the adversarial constructor
`test_adversarial_identity_state` exists precisely to exercise the divergences.

Enforceable: yes, and better than it is. Either (a) call the checker in
`debug_assert`-style from every mutating method's exit, or (b) restructure so the
checks are unnecessary: `tabs: Vec<Tab>` plus `active_tab: usize` is the whole
problem, and a non-empty-vec type with a focused index is the structural answer.
Given the posture, (b).

### 6.6 `TerminalState`'s doc claims a migration that is over

`src/terminal/state/mod.rs`:

> Pure state for a server-owned terminal.
> **During the migration** this is still one-to-one with a pane-backed PTY, but
> pane/view state no longer owns terminal identity, cwd, labels, or agent
> metadata.

and the module header:

> Effective state arbitration is intentionally centralized here. Full lifecycle
> Shepr hook integrations are hook-authoritative while live; ...

and `src/pane/state.rs`:

> Viewport state for a pane.
> Terminal identity, cwd, labels, and agent metadata live in TerminalState.

The migration described is complete: `PaneState` now holds two fields
(`attached_terminal_id`, `right_click_passthrough`). Three doc comments and a
whole test module named `migration_tests` describe a transition nobody can still
observe, and nothing in the build would notice them becoming false - they were
already false when `PaneState` shrank.

Enforceable: no, not mechanically. Stale prose is not lintable. It is a deletion.

### 6.7 ~200 production references to an emulator this project does not use

The terminal core is `alacritty_terminal`, pinned with `=`, and `brokkr.toml`
has a dependency rule (`alacritty-terminal-only-in-shepr-vt`) making that
boundary structural. Yet the pane terminal layer is named after Ghostty
throughout:

| file | matches |
|---|---|
| `src/pane/terminal.rs` | 65 |
| `src/pane/terminal/helpers.rs` | 76 |
| `src/pane/terminal/backend.rs` | 27 |
| `src/pane/runtime.rs`, `pane.rs`, `pane/osc.rs`, `migration_tests.rs` | 19 |
| `src/pane/terminal/tests.rs` | 187 |
| `shepr-vt/src/lib.rs`, `shepr-termio/src/input/*` | 12 |

`GhosttyPaneTerminal`, `GhosttyPaneCore`, `PaneTerminal::ghostty`, and about
forty `ghostty_*` free functions (`ghostty_visible_text`, `ghostty_cell_style`,
`ghostty_collect_dirty_patch`, `ghostty_default_bg`, ...).

Two of them are not naming but false claims:

- `src/pane/terminal/backend.rs:312`: *"a workaround for the libghostty core
  losing rows on resize"* - the stated reason for a workaround in live code cites
  a dependency that is not present. Nobody can now check whether
  `alacritty_terminal` has that behaviour, so the workaround is unfalsifiable.
- `shepr-vt/src/lib.rs:141`: *"match what the libghostty-vt render state
  reported"* - same.
- `backend.rs:143`: `error!(pane = ..., "ghostty core lock poisoned in reader")`
  - an operator-facing log line naming a component that does not exist.

Enforceable: yes, and this is the cheapest enforcement in the report: a
`brokkr.toml` text rule forbidding `ghostty`/`Ghostty` outside a comment that
explains a historical decision (or forbidding it outright after the rename).
That is the same mechanism the gremlin scan already uses. Renaming is mechanical;
the two workaround comments need a human to decide whether the workaround is
still needed against the real emulator, and that question cannot be answered by
reading - it needs the `alacritty_terminal` source in the registry, which
`AGENTS.md` already directs us to.

---

## 7. Policy invented per call site

### 7.1 Snapshot-preservation policy implemented twice, ~40 lines each

`src/persist/writer.rs` has `preserve_snapshot_history(path)` and
`prepare_snapshot_history(path)`. Both: list `recovery_files`, tolerate
`NotFound`, read the newest copy's mtime, compare its age against
`SNAPSHOT_INTERVAL`, read and parse `session.json`, check
`version != SNAPSHOT_VERSION || workspaces.is_empty()`, compute
`layout_fingerprint`, compare against the newest copy's fingerprint, and call
`preserve_existing_in(path, "session-snapshots", SNAPSHOT_LIMIT)`. They differ
only in what they return and in whether the fingerprint is handed back to the
caller - and the second one's outcome is *also* re-checked a third time in
`finish_snapshot_history`, which repeats the version and emptiness checks and
the fingerprint comparison inline.

One policy, three partial implementations, each with its own error handling and
its own `tracing::warn!` wording.

Enforceable: yes, by collapsing to one function returning a decision value that
the caller acts on. Holdable only by a reviewer, but the duplication is large
enough to be obvious once named.

### 7.2 Mutex poison policy differs within the crate, and is spelled at every site

`src/render_signal.rs` repeats
`.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` at eight sites:
`request_generic`, `request_pty`, `set_immediate_pty_sources`,
`has_immediate_work`, `request_terminal_title`,
`pending_terminal_title_sources`, `take` (and the wait path in `teardown.rs`
goes through `shepr_vt::recover_auxiliary_poison`). That is *continue on
poisoned state* - the `RenderRequest` a panicking thread left half-built is used
as-is.

Meanwhile `shepr_vt::lock_terminal_core` treats poisoning as terminal for the
pane, and `teardown.rs` routes through `shepr_vt::lock_auxiliary` /
`recover_auxiliary_poison`, which are shared helpers. So the crate has three
poison policies and two of them have owners while `render_signal.rs` writes its
own eight times.

Enforceable: yes - route `render_signal` through `shepr_vt::lock_auxiliary` like
`teardown.rs` does, and hold it with a text rule against
`PoisonError::into_inner` outside `shepr-vt`.

### 7.3 `git` is spawned three times with three ad-hoc argument builds, no timeout, and an inherited environment

Production sites: `src/git/discovery.rs::git_trimmed_stdout`,
`src/git/status.rs::git_ahead_behind_between`. Test site:
`src/git/test_support.rs::run_git`. All three build
`Command::new("git").arg("-C").arg(dir).args(...)` independently.

No shared helper, and consequently:

- **No timeout budget.** `git rev-list --left-right --count` on a repository
  whose objects live on a stalled network filesystem blocks the calling thread
  indefinitely. There is a 30-second *retry* delay and no *deadline*.
- **The environment is inherited whole.** `GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_INDEX_FILE`, `GIT_CONFIG_GLOBAL`, `GIT_CEILING_DIRECTORIES`,
  `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_ASKPASS` and `GIT_TERMINAL_PROMPT`
  all reach the child. The launching shell may well have set several of them
  (the owner is a heavy Git user running shepr from inside a repo). Compare
  `src/pane/launch.rs`, which goes to real trouble to scrub inherited host and
  agent variables for pane children - that care is not applied to the crate's
  own subprocesses. `GIT_TERMINAL_PROMPT` unset means a credential prompt can
  block the spawn.
- **stdin is inherited**, so an interactive credential helper has a terminal.

Enforceable: yes, and it consolidates several findings at once - one
`fn run_git(dir, args) -> Result<String, GitReadError>` with a scrubbed
environment (`GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`, `-c
core.fsmonitor=false`, null stdin), a deadline, and typed errors. Holdable by a
text rule banning `Command::new("git")` outside that function.

### 7.4 `SystemTime::now()`, `Instant::now()` and `std::process::id()` reached from logic

Ambient dependencies reached directly:

- `SystemTime::now()` ×4 in `persist/writer.rs` (2.2).
- `std::process::id()` inside the recovery filename format, so filenames are not
  reproducible in a test.
- `Instant::now()` in `git/status.rs::git_status_snapshot_for_cwd_with_demand`
  (twice - `now` at the top and a second `Instant::now()` at the retry-after
  computation, which is a subtle inconsistency: the retry deadline is measured
  from after the subprocess ran, the cache checks from before).
- `shepr_platform::hostname()` per OSC 7 report in `pane/osc.rs` - documented as
  deliberate ("a renamed host keeps matching"), which is a reasonable answer, but
  it is still an ambient read from parsing logic with no seam.
- `std::env::current_dir()` in `workspace.rs:1061` inside
  `test_adversarial_identity_state`.

`src/terminal/state/**` and `src/pane/process_probe.rs` show the crate's own
better pattern: `now: Instant` as a parameter everywhere.

Enforceable: yes - extend the existing `now` parameter convention to `persist`
and `git`, and hold it with a text rule against `now()` in those directories.

### 7.5 A test-only variant inside a production enum

```rust
enum PaneRuntimeIo {
    Actor(PtyIoActorHandle),
    #[cfg(any(test, feature = "test-api"))]
    TestChannel { sender: mpsc::Sender<Bytes>, resize_tx: watch::Sender<(u16,u16,u32,u32)> },
}
```

Five `match` arms across `shutdown`, `owns_child_process`, `resize`,
`try_send_bytes`, `write_terminal_response`, `queue_user_input_submission` carry
`#[cfg]` attributes, and the `TestChannel` arms contain real behaviour (one even
spawns a thread that sleeps and sends). Because the gate builds with `test-api`
on (5.5), this variant is in the enum every time the project is checked and out
of it every time the project is installed - the reverse of what you want.

Enforceable: yes, structurally. `PaneRuntimeIo` is a four-method interface;
making it a trait object (or generic) puts the test double in the test module and
deletes all six `#[cfg]` arms. Same for `PaneRuntimeRegistry::drain` and
`Workspace::clear_tabs_for_test`, which exist only because the real types have no
seam.

### 7.6 A process-global teardown counter

`src/pane/teardown.rs`:

```rust
static PANE_TEARDOWNS_IN_FLIGHT: Mutex<usize> = Mutex::new(0);
static PANE_TEARDOWNS_DONE: std::sync::Condvar = std::sync::Condvar::new();
```

`wait_for_pane_session_teardowns` waits on a count that is global to the
process, not scoped to a server. Two servers in one process (which is exactly
what the test suite does) share it: one server's shutdown wait blocks on the
other's pane teardowns, and a leaked count from a panicking teardown thread makes
every later wait time out. The `Drop` impl uses `saturating_sub`, which means an
unbalanced decrement is silently absorbed rather than caught.

Enforceable: yes - hang the counter off the thing that owns the panes (an
`Arc<TeardownTracker>` handed to `shutdown_pane_processes`). Holdable by a text
rule against `static.*Mutex` in the crate.

### 7.7 Unbounded reads and unbounded growth

- `io::load_history` does `std::fs::read_to_string` on `session-history.json`
  with no size cap, then `serde_json` on the whole thing. That file holds every
  pane's full scrollback; one long-lived session with a chatty agent produces a
  large file, and restore reads it entirely into memory twice (string, then
  parsed tree). `git/discovery.rs` caps ref files at 64 KiB, so the crate knows
  the pattern.
- `session-snapshots` is pruned to `SNAPSHOT_LIMIT` only when pruning succeeds
  (6.1). `session-backups` is pruned to `3`, warn-on-failure, forever.
- `OscDebugTracker::pending` grows until `drain_pending` is called. The only
  caller is `process_pty_bytes`, which drains immediately, so it is bounded in
  practice - worth noting because nothing structural says so.
- `metadata_report_sequences`, `metadata_report_agents`,
  `metadata_token_sequence_sources`, `hook_report_sequences`,
  `hook_report_accepted_at`, `suppressed_full_lifecycle_hook_reports`,
  `stale_full_lifecycle_hook_sessions` - seven per-source maps on
  `TerminalState`, keyed by strings from hook reports. Three of them are capped
  (`MAX_METADATA_SOURCES`, `MAX_SEQUENCE_SOURCES`, `MAX_STATE_LABELS_PER_SOURCE`);
  I did not find caps on `hook_report_sequences`, `hook_report_accepted_at`,
  `suppressed_full_lifecycle_hook_reports` or
  `stale_full_lifecycle_hook_sessions`. A misbehaving hook that reports a fresh
  `source` string per invocation grows those four without bound, per pane, for
  the life of the server.

Enforceable: the caps, yes - one `BoundedSourceMap<V>` type used for all seven,
with the cap as a construction parameter, makes the uncapped spelling
unrepresentable. The history read cap, yes, with a test.

### 7.8 Child output reaches diagnostics when a debug flag is set

`src/pane/terminal/backend.rs:152`:

```rust
for event in core.osc_debug_tracker.drain_pending() {
    debug!(pane = ..., osc_command = %event.command, osc_payload = ?event.payload,
           "agent OSC evidence observed");
}
```

OSC 0/2/9/21337 payloads are arbitrary child-controlled text - window titles and
progress strings that routinely carry branch names, file paths, ticket numbers,
and whatever an agent chose to print. Truncated to 512 chars but not otherwise
filtered. This has an answer: it is off unless `SHEPR_DEBUG_OSC_EVIDENCE` is set,
and its entire purpose is to capture that text for manifest authoring. Recording
it here so the decision is visible rather than implicit; what is missing is a
line in the docs saying the flag puts pane content in the log (the flag is
documented nowhere at all - 2.3).

---

## 8. Code that is no longer load-bearing

### 8.1 `src/pane/terminal/migration_tests.rs` gates a migration that finished

461 lines, header comment *"Bounded semantic migration gates ... Keep the same
runner for old/candidate captures."* The "old" side is the pre-fork upstream
terminal layer; there is no build of it in this repository, and `AGENTS.md`
states there is no compatibility with upstream. The `SHEPR_MIGRATION_OBSERVATIONS`
env var exists solely to dump captures for a hand diff against that absent
build, and the test that uses it cannot fail (5.1).

What tells me it is dead: no in-repo producer of the "old" captures, no committed
fixture to compare against, an assertion that is a tautology, and an env var with
one writer and no reader.

What is *not* dead: the file's other six tests
(`incremental_rows_reconstruct_full_render`,
`sparse_dirty_patches_preserve_coordinates_and_clipped_rows`,
`dirty_patch_fallback_keeps_previously_collected_rows_dirty`,
`complete_history_replay_supports_plain_append`, and the read-purity checks)
assert real invariants and should be kept - under a name that says what they
check rather than what they were once migrated from.

Enforceable: deletion of the tautological test and the env var; rename of the
module. Holdable by a text rule against `SHEPR_MIGRATION_OBSERVATIONS`.

### 8.2 `PaneTerminal` is a one-field newtype whose whole body is delegation

```rust
pub(crate) struct PaneTerminal { pub(crate) ghostty: GhosttyPaneTerminal }
impl PaneTerminal {
    pub fn new(ghostty: GhosttyPaneTerminal) -> Self { Self { ghostty } }
    pub fn core_poisoned(&self) -> bool { shepr_vt::terminal_core_is_poisoned(&self.ghostty.core) }
    pub fn process_pty_bytes(&self, ...) -> ProcessBytesResult { self.ghostty.process_pty_bytes(...) }
    ...
}
```

And the layer above it, `PaneRuntime`, is ~90 public methods of which roughly
forty are one-line delegations to `PaneTerminal` (`visible_text`, `visible_ansi`,
`detection_text`, `terminal_title`, `agent_osc_title`, `agent_osc_progress`,
`bracketed_paste_enabled`, `focus_reporting_enabled`, `mouse_reporting_enabled`,
`sgr_pixel_mouse_enabled`, `alternate_screen_active`,
`synchronized_output_active`, `recent_*_snapshot` ×4, `encode_mouse_*` ×3,
`scroll_*` ×4, `search_text_window`, `word_motion_target`,
`paragraph_motion_target`, ...). So a read from the API traverses
`PaneRuntime` → `PaneTerminal` → `GhosttyPaneTerminal` → `shepr_vt::Terminal`,
with two of those three hops adding nothing but a name.

What tells me the middle layer is dead: `PaneTerminal` has one field, one
constructor, no state of its own, no invariant, and no method that does anything
but forward. Its only non-trivial member is `core_poisoned`, which forwards to a
`shepr-vt` free function.

Enforceable: partly. Collapsing `PaneTerminal` into `GhosttyPaneTerminal` (under
a non-Ghostty name, 6.7) is mechanical. Thinning `PaneRuntime`'s forty
pass-throughs is not lintable - but `PaneRuntime` at 2960 lines with ~90 public
methods is the crate's god object, and `AGENTS.md`'s "No god objects" principle
names only `shepr-server/src/app/` and so does not reach it. If the principle is
meant generally, the enforcement should be a per-file or per-impl size rule in
`brokkr.toml`, which would catch `pane/runtime.rs`, `terminal/metadata.rs` (1438
lines) and `persist/restore.rs` (2365 lines) as well.

### 8.3 `SHEPR_TAB_ID` and `SHEPR_WORKSPACE_ID` have no reader

`src/pane/launch.rs` sets all three identity variables for a managed pane. Across
the whole repository - Rust, the shipped hook assets (`.sh`, `.js`, `.py`), the
docs - `SHEPR_PANE_ID` has many readers and `SHEPR_TAB_ID` / `SHEPR_WORKSPACE_ID`
have none. They are also the only two of the three that are kept private, so no
external reader could exist.

What tells me they are dead: a full-text search over every file type that could
read an environment variable, including the assets that run inside other agents'
processes, finds only the two `cmd.env(...)` writes and the two `const`
declarations.

The cost of being wrong: low but nonzero - a user's own shell profile or tab-bar
command could read them, and nothing in the repo would show it. Worth checking
with the owner before deleting, or documenting them as an intentional public
contract (in which case they should be `pub` consts alongside
`SHEPR_PANE_ID_ENV_VAR` and listed in `docs/`).

### 8.4 `SNAPSHOT_VERSION` has had one value, and `shepr` has never been run

`src/persist/snapshot.rs:13`: `pub const SNAPSHOT_VERSION: u32 = 1`, with
`parse_snapshot` and `parse_history_snapshot` rejecting anything else, and
`preserve_snapshot_history`/`prepare_snapshot_history`/`finish_snapshot_history`
each re-checking it. `AGENTS.md` states no on-disk state exists anywhere and
that migration code should be removed freely.

Unlike the other items here this is a *forward* guard, not a backward
compatibility path: its job is to refuse a file from a future build. That is
worth keeping. What is not worth keeping is the same check written four times
(7.1) and the `version` field being a bare `u32` in the struct so every consumer
has to remember to check it.

Enforceable: yes - a newtype whose `Deserialize` impl rejects the wrong version,
so `SessionSnapshot` cannot exist with a bad version and the four checks collapse
to zero.

### 8.5 `RefFileRead::Unavailable` is distinguished and then discarded

See 4.1. The enum's three-way distinction has one consumer, which maps two of
the three variants to the same answer. Either the distinction should reach the
caller (it should - it is the difference between "no branch" and "Git is
broken"), or forty lines of careful `symlink_metadata` reasoning are dead.

### 8.6 `GitStatusRefreshDemand::ALL` and `git_status_snapshot_for_cwd` are test-only wrappers in production files

`src/git/status.rs` carries `#[cfg(any(test, feature = "test-api"))] pub const ALL`,
`#[cfg(test)] pub fn git_status_snapshot_for_cwd`, and
`#[cfg(test)] pub(super) fn git_status_fingerprint`. Fine as such, but combined
with 5.5 (the gate always builds with `test-api`) it means the production shape
of this module is never compiled by the gate.

---

## Cross-cutting notes

**What `brokkr.toml` shows this build can already enforce**, and which findings
map onto each existing mechanism:

| existing mechanism | findings it could hold |
|---|---|
| gremlin text scan | 1.1 (`"SHEPR_ENV"` literal), 1.4 (filename literals), 6.7 (`ghostty`), 7.2 (`PoisonError::into_inner`), 7.3 (`Command::new("git")`), 8.1 (`SHEPR_MIGRATION_OBSERVATIONS`), 2.2/7.4 (`now()` in `persist`/`git`), 5.6 (`/tmp` literals) |
| `[[dependency_rule]]` | nothing new; the layering is clean and `alacritty_terminal` containment holds |
| `[[check]]` packages | 5.5 (add a default-features, non-`--all-targets` entry) |
| `clippy.toml` / workspace lints | 0.1 partly (no existing lint catches `?`-in-closure past a fallback) |
| a new per-file/per-impl size rule | 8.2 (`pane/runtime.rs`, `terminal/metadata.rs`, `persist/restore.rs`) |
| tests that do not exist yet | 1.2 and 6.2 (the hook-asset literal tests - the only mechanical answer to the forced duplication), 0.1, 0.3, 2.4, 2.5, 1.5 |

**Values that must be duplicated, and what keeps them in step:**

1. `SHEPR_ENV`, `SHEPR_PANE_ID`, `SHEPR_SOCKET_PATH`, `SHEPR_BIN_PATH` in the
   shipped hook assets - those files execute inside other agents' processes and
   cannot link against shepr. *Nothing currently keeps them in step.* The answer
   is a test that extracts the literals from the assets and checks them against
   the exported constants (1.2).
2. The `(source, agent_label)` pairs the assets report - same constraint, same
   missing test, higher stakes because fifteen guards fail open on a mismatch
   (6.2).
3. Local and remote hosts each run their own binary, so nothing in
   `shepr-mux`'s scope is duplicated for that reason - the crate is entirely
   server-side. No findings of this kind.

**On the two things asked about specifically:**

*Is `PaneState` really separate from `PaneRuntime`?* Technically yes, vacuously.
`PaneState` is two fields; everything that was pane state now lives in
`TerminalState`, which is itself a 30-field struct with 14 public mutable fields
including two derived ones (`state`, `revision`) whose invariants rest on callers
remembering to call `recompute_effective_state` and to bump the counter. The
separation that matters - pure data testable without PTYs - holds for
`TerminalState` (it is tested extensively and takes `now` as a parameter, which
is the real evidence) and does not hold for `PaneRuntime`, which mixes a PTY
actor handle, a tokio abort handle, four `Arc`-shared atomics, three mutexes, a
`Cell`, and forty pure-read delegations in one 2960-line type. The claim in
`AGENTS.md` is true about the name and misleading about the shape.

*Do the hidden-pane early exits and narrow accessors hold?* Yes, and
`render_signal.rs` is the cleanest module in the crate on this axis: the
`immediate_pty_sources` classification, the "first title source wakes queued
hidden work" rule, and the coalescing are each covered by a test that would fail
if the early exit were removed (`hidden_pty_sources_coalesce_to_one_wake` asserts
exactly one wake for fifty requests). `collect_dirty_patch_snapshot`'s
even-`content_seq` gate and the `SCAN_CHUNK_ROWS` lock release are likewise real
and commented with their reason. The only hygiene issue in this area is the
eight-times-repeated poison policy (7.2).

*Does the persisted snapshot format validate what it reads back?* It validates at
restore, not at parse. `restore.rs` sanitizes properly - `valid_split_ratio`
clamps saved ratios exactly as live splits do,
`remap_saved_index`/`resolve_restored_pane` handle indices and pane ids that no
longer exist, and there is a test
(`restored_split_ratios_are_clamped_like_live_splits`) asserting the clamp is the
same one. But `parse_snapshot` is `pub` and returns a `SessionSnapshot` whose
types encode none of it: `ratio: f32`, `active: Option<usize>`, `selected: usize`,
`active_tab: usize`, `focused: Option<u32>`, `root_pane: Option<u32>`,
`cwd: PathBuf`. Two other consumers already parse it without going through
restore - `preserve_snapshot_history` and `prepare_snapshot_history` both do
`serde_json::from_slice::<SessionSnapshot>` directly. They only read
`version`/`workspaces.len()`/`layout_fingerprint`, so no harm today; the harm is
that the next consumer gets unvalidated data by default. Carrying
`shepr_core::layout::SplitRatio` and validated cwd/index types in the snapshot
struct itself would make restore's sanitizing unnecessary and the next
consumer's mistake unrepresentable (0.2, 8.4).

---

## Scope not covered in depth

`src/pane/process_probe.rs` (1435 lines), `src/terminal/metadata.rs` (1438),
`src/persist/restore.rs` (2365) and `src/workspace/geometry.rs` (385) were read
for the eight questions' patterns (constants, env reads, subprocesses, swallowed
errors, name-keyed guards, logging) but not line by line for logic. The nine
acquisition-timing constants in `process_probe.rs` and the interaction between
`AGENT_ABSENCE_STARTUP_HOLD` and `MANAGED_AGENT_RESUME_TIMEOUT` (aliased, so one
value with two names governing two different state machines) deserve a closer
read than I gave them - an alias across subsystems is either a deliberate
coupling worth documenting or a coincidence worth splitting, and the code does
not say which. `src/pane/terminal/tests.rs` (3913 lines) and
`src/terminal/state/tests.rs` (4075) were sampled, not read; the question-5
findings above come from the smaller test modules and may under-count.
