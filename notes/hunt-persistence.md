# Hunt: session persistence

Scope: `crates/shepr-mux/src/persist.rs`, `crates/shepr-mux/src/persist/`
(actor, capture, error, files, lock, open, recovery, schema, writer; not
restore), the persistence part of `crates/shepr-mux/src/limits.rs`, followed
into `shepr-platform` (publish_file, data_directory_lease, file_stamp,
daemon), `shepr-paths`, `shepr-protocol` (restore notice types),
`shepr-server` (bootstrap, `App::open`, `app/session.rs`) where the questions
led. Every in-scope file was read in full, tests included.

Findings are not ranked. Each says what enforcement the fixed version could
have.

## 1. Defects

### 1.1 A retried clear reports durable success without ever syncing the directory

`files::clear_path` removes the target, then syncs its directory. If the
remove succeeds and the sync fails, it returns `Err`, which `SessionWriter::clear`
maps to `SaveError::Io`. `SaveError::Io` is retryable, so the server retries.
The retry's `remove_file` gets `NotFound`, and `clear_path` returns `Ok(())`
without syncing anything. The retry reports success, so the server counts the
clear as durable when no directory sync ever succeeded.

The broken claim: `SaveError`'s own documentation. `Io` means "The write
failed before it was known to have been published", and `PublishedNotDurable`
is the variant for "published but could not be confirmed durable". A clear
whose unlink happened but whose sync failed is the second case. The writer
reports it as the first, and the retry then hides it.

Fix: on `NotFound`, `clear_path` still syncs the containing directory (cheap,
idempotent). A removal followed by a failed sync reports a
`PublishedNotDurable`-equivalent. Enforceable by a writer test that injects a
failing directory sync (the platform already has
`commit_with_directory_sync` as a seam; `clear_path` needs the same seam).

### 1.2 The version test cannot fail on the version

`schema::tests::snapshot_types_reject_wrong_version_during_deserialization`
parses `{"version":2,"workspaces":[],"active":null}` and
`{"version":2,"workspaces":[]}` and asserts an error. Neither input has
`host_theme`, a required key, so both fail with version 1 too. The test would
pass if `SnapshotVersion`'s check were deleted. (Listed here because it leaves
the "Deserialization rejects every other value" claim on `SNAPSHOT_VERSION`
unverified. It is also an item 6 finding.)

Fix: build a valid file (as the neighbouring test does), change only
`version`, assert the error, and assert the same file parses with
`SNAPSHOT_VERSION`.

### 1.3 `SnapshotVersion` was not bumped when the format changed

The history removal (7fdac186) changed the session file from an envelope
`{snapshot, history_digest}` to the bare layout object, and the version stayed
`1`. Old files still fail to parse, but through `deny_unknown_fields` /
missing-field errors, not through the version check. The doc claim on
`SessionSnapshot::version`, "Format version - used to detect incompatible
changes", is false today: the last incompatible change kept the same number.

Either bump it on every format change or delete the field. Given
`deny_unknown_fields` and required keys, the field adds nothing: any
structural change already fails the parse, and nothing migrates. Enforceable
by a golden fixture test: a checked-in `session.json` per version, where
changing the schema without adding a new fixture and bumping fails the test.
Not enforceable by a text rule.

### 1.4 Documented backup-policy semantics contradict the code (the code is the safer one)

`SessionBackupPolicy::NoBackupNeeded` is documented as "Restore used the file
in full, or there is no source file to preserve". But `open_and_summarize`
leaves a `Missing` load at `PreserveExisting`, so a file that appears later is
still backed up before the first replacement. A test pins this
(`first_clear_preserves_an_unloaded_file_even_after_an_earlier_missing_clear`).
`SessionPersister::spawn`'s doc likewise names only two reasons for the
backup ("it could not be loaded, or restore dropped part of it"). The
behaviour is right and the two doc comments are wrong. A doc-only fix.

### 1.5 Startup refusal of the session path can name no path

`check_session_target` runs `SessionPath::resolve`, and the server wraps any
error as `RunServerError::SessionTarget(io::Error)`, whose `Display` is the
bare io error. Only the not-regular case carries a path (`NotRegularFile`).
Two other cases do not:

- A stat error on any hop (`EACCES` on the data dir, for example) prints
  `Permission denied (os error 13)`.
- The hop-limit refusal prints "session path still resolves through a symlink
  after the hop limit".

Neither names the file, so the operator is told the server refused to start
without being told what to look at. The claim: the bootstrap comment says a
bad session path "refuses the start ... rather than running panes". It does
refuse, but the refusal gives the operator nothing to act on. Fix: `resolve`
wraps its errors with the path it was resolving (one wrapper type). A unit
test can assert that the path appears in every `resolve` error.

### 1.6 The restore notice never names the session file

`files::load` builds `SessionRestoreFailure` from `err.to_string()` or the
serde error. For `Unreadable`, `TooLarge` and `Unparseable`, the detail does
not contain the file path (serde gives "expected value at line 1 column 1";
io gives "Permission denied (os error 13)"). The notice the client shows
("The saved session was not restored: it could not be read: Permission denied
(os error 13). The original session file is copied to <backup_dir> ...") names
the backup directory but not the file that failed. The log line has the path;
the TUI notice, which is what the operator sees, does not. Fix: carry the
session path in `SessionRestoreNotice` beside `backup_dir`. A protocol test
can assert that the rendered notice contains it.

## 2. One value, one owner

### 2.1 The data directory's file layout is spread over three crates

- `shepr-paths` names the lease (`DATA_DIR_LEASE_FILE_NAME = "session.lock"`).
- `shepr-platform::logging` names the server log (`SERVER_LOG_FILE`), reached
  both through `AppPaths::server_log()` and directly in bootstrap's
  `init_file_logging(data_dir, SERVER_LOG_FILE)`.
- `shepr-mux::persist::files` names `session.json`, `session-snapshots` and
  `session-backups`.

Nothing answers "what lives in the data directory". Old files left by an
earlier build show the cost (see 9.1). Fix: one `DataDirLayout` in
`shepr-paths` (lease, session file, snapshot and backup directories, server
log) that mux and platform read. Enforceable by a textlint that forbids the
literal file names outside that module.

### 2.2 `session-backups` is spelled as a literal in an operator message

`open.rs` logs "the saved session is backed up to session-backups before the
first save". `files::BACKUP_DIRECTORY_NAME` is the owner. Rename the directory
and the log lies. Fix: log the `backup_dir` path field (which is already
computed for the notice) instead of a literal. Enforceable by the textlint in
2.1.

### 2.3 The too-large message and limit exist as three spellings

- `files::SessionFileTooLarge` displays "session file exceeds {} bytes".
- `files::save_to_path` builds its own `format!("session file exceeds
  {MAX_SESSION_FILE_BYTES} bytes")` as an untyped `InvalidData`, so an
  oversized save is not the typed error and nothing could classify it.
- `SessionRestoreFailure::TooLarge` renders "it exceeds the {limit}-byte
  session file limit".

Fix: `save_to_path` returns `SessionFileTooLarge` as well. Enforceable only by
review or a test that matches both paths on the typed error.

### 2.4 The not-regular error names the link on one path and the target on another

`open_regular(path)` passes the caller's path (the symlink) into
`not_regular`. `save_to_path`, `clear_path` and `check_session_target` pass
`resolved.target()`. The same condition on the same file therefore produces
two different messages depending on whether a read or a write met it. Pick
one (the target, and mention the link when they differ) and have
`SessionPath` own the error construction. A test can assert both paths give
equal messages.

### 2.5 `NotRegularFile` exists twice

`shepr-mux/src/persist/files.rs` and `shepr-integration/src/file_ops.rs` each
define a `NotRegularFile` error over `shepr_platform::open_regular_file`'s
`Err(FileType)`, with different texts and different detail. Platform's
`open_regular_file` should return the typed error itself. Enforceable by a
textlint on `struct NotRegularFile` outside platform.

### 2.6 Publication cleanup is two functions

`shepr_platform::publish_file::cleanup` (a warn with `path` and `error`, no
`event` or `subsystem`) and `persist::files::remove_after_failed_publish` (a
warn with `event = "persist.cleanup"`) implement the same "best-effort remove,
NotFound is fine, warn otherwise" policy. Not mechanically enforceable beyond
review.

### 2.7 The lease name is aliased again in mux, under a false comment

`lock.rs` has `pub(super) const LOCK_FILE_NAME = shepr_paths::DATA_DIR_LEASE_FILE_NAME`,
with the comment "Config owns the lease filename used to build API paths;
platform is below config". Config does not own it (`shepr-paths` does), so
the comment is false today (see 7.1). The alias exists only so a writer test
can join it. Use the `shepr_paths` name directly.

### 2.8 Event names: `writer.rs` says one thing, `recovery.rs` does another

`writer.rs` states that event literals "are the emitted log schema, so the
names stay visible at the event site". `recovery.rs` routes
`persist.snapshot` / `persist.backup` through `RecoveryKind::event()` in two
helpers, and also spells `"persist.snapshot"` inline three more times.
`persist.restore` is spelled in `files.rs` and `open.rs`. No list of
persistence events exists. Pick one convention. A textlint can require
`event = "persist.` literals (or forbid them outside one module).

## 3. Values nobody can find, change or trust

### 3.1 The snapshot cadence uses a clock the textlint does not see

`persist-clock-is-injected` forbids `SystemTime::now()` in `persist/`, and
`now` is injected. But `snapshot_history_decision` compares the injected `now`
with `std::fs::metadata(latest)?.modified()`, the filesystem's clock at copy
time. The interval therefore mixes two clocks. Tests reach the cadence only by
reading real mtimes back (`snapshot_interval_uses_supplied_clock` builds `now`
from the file's mtime) or by `set_times`. The test names claim "uses supplied
clock", which is half true.

The mtime is used deliberately, so a rolled-back clock recovers after one copy
(`snapshot_cadence_recovers_after_clock_rollback_and_restart`). That is a
real reason. The cleaner shape is for the writer to remember in memory when it
last made a copy (it is the only writer under the lease), and fall back to the
file's mtime only for the first decision after startup. The lint cannot catch
`modified()`, so the guard fails open for filesystem time. Either widen the
pattern (`\.modified\s*\(`) with an allow marker for the one sanctioned site,
or accept it and say so beside the rule.

### 3.2 The persistence tunables are well placed

`SNAPSHOT_INTERVAL`, `SNAPSHOT_RECOVERY_WINDOW`, `SNAPSHOT_LIMIT`,
`BACKUP_LIMIT`, `RECOVERY_SEQUENCE_LIMIT`, `MAX_SESSION_FILE_BYTES` and
`MAX_SESSION_PATH_SYMLINK_HOPS` are all in `limits.rs` with reasons, and the
two filename-width constants carry `limits-exempt`. Nothing here is read at
use from config. The one gap is that `SNAPSHOT_LIMIT`'s "covers an overnight
failure" holds only when a copy is actually made every interval. Copies are
made only on a save whose layout fingerprint changed, so in practice the 48
copies span much longer than 12 hours. A doc nuance, not a defect.

## 4. One channel, one implementation

### 4.1 Persistence log lines without `event` / `subsystem` and without identifiers

- `capture.rs`: two `tracing::error!`s ("workspace focus or root has no pane
  record; not saved", "workspace layout and pane records disagree") have
  `workspace` but no `event`/`subsystem`.
- `schema.rs` `deserialize_agent_session`: `tracing::warn!(%error, "ignoring
  invalid saved agent session")` has no event, no path, and no workspace or
  pane identity. Worse, it fires from every parse, including the fingerprint
  reads `SnapshotFingerprintCache::read_or_reuse` makes of the session file
  and of the newest snapshot copy. One bad agent session in a recovery copy
  therefore logs "ignoring invalid saved agent session" again whenever that
  file's stamp changes, out of any restore context.
- `open.rs`: the partial-restore warn has `dropped_workspaces` and
  `restore_damage` but no `event`, `subsystem` or `path`. The info `log_restore`
  right after it has all three.

Enforceable by a textlint over `crates/shepr-mux/src/persist/**` requiring
`event =` in every `tracing::(warn|error|info)!` (multi-line, so a script
check rather than a single-line regex).

### 4.2 A published-but-not-durable save logs as a failed save, twice

`finish_save` logs `NotDurable` through `session_save_failed` at error level
with the message "failed to save session". The file was saved; only the
directory sync failed. The server then logs `warn!("session save failed")`
again for the same event, without the path. Every save failure produces two
lines at two levels, one with the path and one without. Give `NotDurable` its
own outcome (`outcome = "not_durable"`, "session saved but not confirmed
durable") and let only one layer log.

### 4.3 A broken snapshot directory logs two warnings on every save

If `session-snapshots` is unreadable (a file in its place, as in
`snapshot_failure_does_not_block_primary_save_and_clear`),
`plan_snapshot_history` fails, returns `RetryAfterWrite`, and
`preserve_snapshot_history` fails again after the write. Each autosave
therefore emits two `persist.snapshot` warnings, indefinitely, at autosave
cadence. Fix: rate-limit, or log once per state change in the writer. Not
mechanically enforceable.

## 5. Errors

### 5.1 A capture inconsistency silently drops a workspace from disk

`capture_workspace` returns `None` (logged) when a tree's focus or root lacks
a record or its shape and records disagree. `capture_deferred` then skips that
workspace and the save proceeds, replacing the session file without it, and
with no backup, since the backup policy is only for load-time losses. If every
workspace failed, the job is a `Save` with zero workspaces, not a `Clear`. The
comment says this "constructors and mutators rule out", which is the case
where failing closed costs nothing. Fix: a capture inconsistency fails the
whole job (`SaveError` with a new variant, or `capture_job` returning
`Result`), so the last good file survives. Enforceable by a test that builds
an inconsistent tree through a seam and asserts the file is unchanged.

### 5.2 Failure detail is shed between load and notice

See 1.6. The machine-readable fields that were meant to carry the reason are
unused (9.3), and the human `detail` lacks the path.

### 5.3 A permanently unreadable session file retries forever with no operator signal beyond the log

When the session file is unreadable (`EACCES`), the load is `Unusable` and the
policy is `PreserveExisting`. Every save then fails in `preserve_existing`
(the source cannot be opened), with a retryable `SaveError::Io`, so the
autosave backoff retries for the life of the boot. Each failure logs an error
and a warning. The operator sees the restore notice ("copied ... before the
server first saves over it") and nothing telling them that no save will ever
land. `SaveError::is_retryable` is the classifier. A source that cannot be
backed up is a condition the operator must fix, which neither "retryable" nor
"refused" models. Fix: carry a distinct "blocked on backup" outcome that the
server projects to clients, the way `session_saves_stopped` is projected.
Testable at the writer and saver level.

## 6. Tests that prove nothing

- 1.2: the version test cannot fail on the version.
- `files::tests::resolve_write_target_returns_a_stat_error_other_than_not_found`
  and `writer::tests::repeated_failed_saves_do_not_replace_a_completed_recovery_copy`
  return early and pass when the runner can read a 0o000 directory or write a
  0o500 one, so under root they assert nothing and report success. They
  should fail loudly or be ignored under a privileged runner, not pass. The
  repo has `brokkr test`'s `--include-ignored` convention for root-only
  tests; an `#[ignore]`d privileged variant, or a check that refuses to pass
  vacuously, would keep the signal.
- `open::tests::refusing_launcher` hard-codes
  `socket_path: "/run/user/1000/shepr-test.sock"`, one developer's uid. It is
  harmless today only because the launcher refuses before any child sees it.
  Use a scratch path.
- `writer::tests::snapshot_survives_exit_bursts_clears_and_writer_restarts`
  varies only the workspace name across 100 saves. The name is not in the
  layout fingerprint, and every save is within the interval, so the test
  cannot tell interval suppression from fingerprint suppression. The local is
  still called `shrinking`, a leftover of the history-era version.
- `files::tests::an_unrelated_leftover_beside_the_session_is_not_touched_by_saves`
  guards a `session.json.tmp` staging name no code has used since publication
  moved to `.shepr-<token>-<seq>.tmp`. It is a regression test for a removed
  behaviour. Keep it or drop it, but its premise is history.
- The writer tests end with `std::fs::remove_dir_all(writer.path.parent())`,
  while `ScratchDir` is documented as "deliberately left in place afterwards".
  Half the tests in the file follow the convention and half fight it. (The
  `writer()` helper also drops its `ScratchDir` immediately. That is harmless
  only because `ScratchDir` has no `Drop`.)
- `persist/` spells scratch directories both `crate::test_support::ScratchDir`
  and `shepr_test_support::ScratchDir` (the former is a re-export of the
  latter). Cosmetic.

## 7. Guards and claims that have stopped holding

### 7.1 False comments today

- `lock.rs`: "Config owns the lease filename used to build API paths; platform
  is below config in the crate layers". False; `shepr-paths` owns it.
- `recovery.rs`, three sites (`plan_snapshot_history`,
  `preserve_snapshot_history`, `preserve_existing_in`): "the platform's session
  helpers emit through tracing too but label save outcomes" / "cover save,
  clear and restore outcomes only" / "only cover session mutations". False:
  `shepr-platform` has no session helpers and emits no persist events. The
  writer in this crate does. A stale explanation, three times.
- `AGENTS.md` ("the data directory (saved layout, history, server log, lease)",
  "its own sockets, saved layout and history"), `shepr-paths/src/app_paths.rs`
  (`state_dir` doc: "the saved layout and history live in data_dir") and
  `shepr-paths/src/profile.rs` ("its saved layout and history"). Pane history
  is gone.
- `persist.rs`'s module doc lists the files "by job" and omits `restore`,
  `error`, `actor` and `lock` from that list (two are mentioned in the
  paragraph above it). This is a hand-restated list of the module's files that
  nothing checks.
- `files.rs` `load`'s parse-error log says "failed to parse session file,
  ignoring". The file is not ignored: it is protected and backed up.

None of these is mechanically checkable as prose. The `persist.rs` list could
be dropped in favour of each file's own module doc.

### 7.2 The seal allowlist has fail-open entries

`brokkr.toml`'s `disallowed-escapes-are-allowlisted` excludes
`crates/shepr-mux/src/persist/writer.rs`, which has no
`clippy::disallowed_*` escape any more, and
`crates/shepr-mux/src/pane/terminal/migration_tests.rs`, which does not exist.
Each stale entry pre-approves a future escape in that file with no review,
which is the thing the rule exists to stop. Checkable: a script check that
every path in that rule's `exclude` exists and contains at least one escape.
The same check is worth having for every textlint `exclude` list.

### 7.3 `persist-clock-is-injected` misses the filesystem clock

See 3.1.

### 7.4 "Writes happen under one writer" is enforced only by the lease

`limits.rs` (`RECOVERY_SEQUENCE_LIMIT`) and `preserve_existing_in` both
reason that a taken recovery name "is a leftover, not a concurrent writer"
because of the lease. That holds, and the writer type requires a
`DataDirLease` to construct, so the type enforces it. This one does hold.

## 8. Policy invented per call site

### 8.1 Test-only modes reachable from production

`SessionOpenPolicy::Never`, `SessionPersister::lease_only`,
`Worker::LeaseOnly`, `SaveRefusal::LeaseOnly` and the server's
`SavePolicy::Never` / `SaveMode::Never` form one whole runtime mode: "hold the
lease, persist nothing". No config selects it and no production code path
uses it. Only `App::new` (test constructor) and `agent_report_test_support`
(`cfg(test)`) pass `Never`; bootstrap always passes `Persist`. It is a test
shortcut living in production types, adding a branch to every `SavePolicy`
match. Fix: tests open with `Persist` on a scratch data directory (they
already have one), or the server crate gets a test seam. Then delete the
variant, the lease-only worker and the refusal. Enforceable by the
`dead-test-helpers` script widened to flag non-`cfg(test)` public items whose
only callers are test code.

The same pattern at smaller scale:

- `persist::capture` (eager cwd read) is used only by
  `shepr-server/src/app/snapshot_tests.rs`.
- `SessionLoad::into_snapshot` and `CapturedLayout::snapshot()` are used only
  by server tests.
- `PendingSave::channel` is used only by server tests. That one is an
  acknowledged seam.

### 8.2 Unbounded leftovers

- A crash mid-publish leaves a `.shepr-<token>-<seq>.tmp` staging file (up to
  `MAX_SESSION_FILE_BYTES`, 64 MiB) beside the session file, which with a
  symlinked session file may be in the user's dotfiles tree, or in a recovery
  directory. `files::publish_private_file`'s doc says such a leftover "is
  never reused or removed here", and nothing removes it anywhere else.
  Across crashes they accumulate without bound. The lease makes a startup
  sweep of `.shepr-*.tmp` in the data and recovery directories safe: no other
  writer can be mid-publish while it is held.
- See 9.1 for the history-era leftovers.

### 8.3 Snapshot bookkeeping re-derives state the writer already has

`SnapshotFingerprintCache` stamps and re-parses files (full schema validation,
with logging side effects, 4.1) to learn the fingerprint of the newest copy
and of the current file. Under the lease the writer is the only process that
writes any of them. Between startup and retirement, it could know both from
what it wrote itself. The four-state `SnapshotHistoryPlan` (`Skip`,
`PreserveBeforeWrite`, `PreserveAfterWrite`, `RetryAfterWrite`), the
optional-replacement decision function and the stamp cache all exist to
recover facts the writer discarded.

Structural fix: the writer holds `{ on_disk: SavedLayout, latest_copy:
Option<(when, SavedLayout)> }`, initialized once at open from disk (the one
read), and updated on each publish and copy. The decision becomes a pure
function of that state plus `now`, testable without a filesystem or mtimes,
which also resolves 3.1. A large simplification for what is a small feature.

### 8.4 Two recovery directories are resolved from the link path while the file is the target's

`snapshot_directory` and `backup_directory` are `path.with_file_name(..)` of
the session path as configured (the symlink), while the staging file and
directory syncs are on the resolved target. So with a symlinked session file,
recovery copies live in the data directory and staging lives in the target's
directory. That is consistent with the notice (which names the data
directory) and may be the intent, but nothing states it, and
`missing_directory_chain` / `create_private_directory_all` create directories
on the target side. It should be written down where `SessionPath` is defined.

## 9. Code that is no longer load-bearing

### 9.1 History-era files on disk are orphaned forever

Before 7fdac186, recovery copies came in pairs: `session-<ts>-<seq>.json` and
`session-history-<ts>-<seq>.json`, plus a live `session-history.json`. The
history copies could be up to 256 MiB each (`MAX_SESSION_HISTORY_FILE_BYTES`),
up to 48 snapshots plus 3 backups. `RecoveryKey::parse` now rejects
`session-history-...` names, so pruning never sees them, and nothing removes
`session-history.json`. Every host that ran with `pane_history` keeps
potentially gigabytes of screen contents (the privacy cost the commit message
named) in its data directory indefinitely. The owner's rule is no migration
code, so this is an operator action, not a code change: remove
`session-history.json` and `session-snapshots/session-history-*`,
`session-backups/session-history-*` by hand on each host. Mentioned because
nothing else will ever do it.

### 9.2 `sha2` in shepr-mux exists only for an in-memory equality

The layout fingerprint is SHA-256 over a hand-built encoding, compared only in
process and never stored. Its original reason (keeping digests apart from
history pairing; the pre-removal comment said so) is gone. Comparing the
encodings, or deriving `PartialEq` on a `Shape<PanePublicNumber>` projection,
does the same. Then `sha2` leaves `shepr-mux/Cargo.toml` and the
`shepr-mux-layer` allow list in `brokkr.toml`. The `Option` plumbing through
`layout_fingerprint` and `append_fingerprint_count` (`u64::try_from(usize)`,
which cannot fail on 64-bit Linux, the only platform) and the
`SavedLayout::Unknown` result of a failed fingerprint are dead with it.
`brokkr check`'s dependency rule would catch the dependency's removal once
the allow entry is dropped.

### 9.3 The machine-readable restore failure taxonomy has no reader

`SessionRestoreFailure::{Unreadable.kind, NotRegularFile.kind,
Unparseable.{line, column, category}}`, `SessionIoErrorKind` (21 variants,
including `ConnectionRefused`, `AddrInUse`, `NotConnected` and others that
cannot come from reading a file), `SessionFileKind` and
`SessionParseCategory`, plus `files::session_io_error_kind`,
`session_file_kind` and `session_parse_failure`'s mapping, exist so "the
variant and parse coordinates keep the outcome machine-readable". Nothing
reads them: the client renders only `Display`, which uses `detail`.
`Unreadable` and `NotRegularFile` display identically. The only consumer is
one assertion in `open.rs`'s tests. Collapse to `{ detail, path }` (see 1.6)
or a two-variant enum. Removing them is checked by the compiler.

### 9.4 `NotRegularFile` in the restore failure is practically unreachable

`check_session_target` refuses a non-regular session path at startup, before
`load`. `load`'s `NotRegularFile` arm is reachable only if the path changes
type in the microseconds between the two calls. That is a tiny window, and
keeping the arm costs a wire variant and a mapping table.

### 9.5 Recovery name sequence numbers are almost never used

`preserve_existing_in` forces the new timestamp past the newest parsed
regular-file key (`max(now, previous + 1)`), so sequence `0` is always free
unless something that is not a regular file (a directory, a symlink) sits at
the exact future name. `RECOVERY_SEQUENCE_LIMIT` (128 attempts), the
`RECOVERY_SEQUENCE_DIGITS` field, the compile-time width assertion and the
`AlreadyExists` "backstop" loop exist for that case. Dropping the sequence
(or keeping the field but not the loop) is a format change for recovery copy
names only, which the parser already rejects or accepts by width.

### 9.6 `copy_recovery`'s `NotDurable` branch is dead by contract

The comment itself says a create-only publish cannot return `NotDurable`.
The branch, and the parent-directory sync after it, are defensive. The parent
sync is not dead: it covers a freshly created recovery directory, but it runs
on every copy, not only after creating the directory.
`create_private_directory_all` could report whether it created anything
(compare `missing_directory_chain` in `files.rs`, which solves the same
problem the other way).

### 9.7 Small dead items

- `SessionWriter::retire(self) { drop(self) }` and
  `DataDirLease::release(self)` are only names for `drop`. Harmless, but each
  is one more thing to read.
- `actor::abandoned()` and `actor::lease_only()` are one-line wrappers around
  enum constructors.

## Lateral notes (outside the nine questions or outside scope)

- `shepr-integration` has its own `AtomicReplace` wrapper over
  `shepr_platform::publish_file::PreparedFile` and its own `NotRegularFile`.
  `shepr-remote/src/machine/ssh_metadata.rs` builds `PublishOptions` inline.
  There are three call styles over one platform primitive. The persistence
  layer's `publish_private_file` is the cleanest of them; a platform-level
  `publish_private(target, bytes, mode, PublishTarget)` would serve all three.
- `App::retire_session_writer` calls `save.pending.wait()` (blocking) on the
  calling thread, while `save_session_before_teardown_async` uses
  `spawn_blocking`. Whether the first is reached from the async loop belongs
  to the server-side hunt.
- In the server, `wait_off_the_runtime` maps a `JoinError` to
  `SaveError::Abandoned`, which `finish_session_save` treats as non-retryable
  and logs "session persister cannot accept further saves; disabling session
  persistence for this boot". A cancelled blocking task at runtime shutdown is
  not a dead persister, so the message would mislead. That belongs to the
  server hunter as well.
- `SessionRestoreNotice` is assembled in `shepr-protocol`'s `Display`, which is
  the right channel. The server-side warn duplicating it (2.2) is the only
  ad-hoc copy.
