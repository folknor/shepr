# Design hunt: mux-state

Scope: `crates/shepr-mux` minus `pane/`, `terminal/` and `pane.rs`. That is
`git/`, `persist.rs` and `persist/`, `workspace.rs` and `workspace/`, `lib.rs`,
`events.rs`, `cwd.rs`, `render_signal.rs`, `limits.rs`, `logging.rs`. Where a
question led into the server (`app/git_refresh.rs`, `app/session.rs`,
`app/mod.rs`, `app/actions/events.rs`, `app/agent_resume.rs`,
`app/api/checkout_root.rs`, `ui/panes.rs`, `retained_surface.rs`) or into
`shepr-core`/`shepr-protocol`, I followed it and say so.

The short version: the save state machines and the persister are in good shape,
but almost every identity this half of the crate handles travels as a bare
primitive (`u32` saved pane keys, `usize` public numbers with a zero sentinel,
SHA-256 digests as `String`, Git object IDs and ref names as `String`, workspace
IDs stringified back into `String`). Git status is the weakest area: tuples
indexed by `.0` to `.3`, a poison bool flipped on "the first element", a cache
entry whose `Option<Instant>` encodes four states, a whole demand-gated code path
that production never takes, and a `GitSpaceMetadata` that is computed, cached
and diffed on every refresh but read by nobody. The workspace is a bag of public
fields whose Git half is written field by field from the server. The biggest
structural moves I would make: a `PaneTree` that makes the layout/records
agreement structural, a `GitIdentity` value owned by the workspace, a
`shepr-git` crate that owns the status cache instead of handing it back and
forth through `AppEvent`, and one `persist::open_session` that owns the whole
load/restore/protect/persister sequence the server currently stitches together.

---

## 1. Axes that should be types

### 1.1 Public pane numbers: `usize` with zero as "none"

Sites: `WorkspacePane::public_number: usize` (pub, and `WorkspacePane::new`
sets it to `0`), `Workspace::next_public_pane_number: usize` (pub field and
accessor), `NewPane::public_number`, `commit_new_pane(.., public_number: usize,
..)`, `commit_prepared_split` (rejects `0`), `pane_id_for_public_number(usize)`,
`public_pane_number() -> Option<usize>`, `launch_env_for_new_pane(pane_number:
usize)`, `PaneSnapshot::public_number: Option<usize>` (where `Some(0)` also
means none), `WorkspaceSnapshot::next_public_pane_number: usize` with
`#[serde(default)]` (so a missing value is `0`), restore's
`numbers: HashMap<u32, usize>` and `next_number: usize`, and
`PublicPaneId::new(&WorkspaceId, usize)` in `shepr-protocol`, which panics on 0.

Every constructor has to remember to overwrite the zero:
`Workspace::spawn`, `test_from_pane`, `test_new` all write `pane.public_number
= 1` and pass `next = 2` by hand; `commit_prepared_split` and restore assign
after `WorkspacePane::new`. A record with number 0 is constructible and
`PublicPaneId::new` would panic on it.

Proposal: `PanePublicNumber(NonZeroUsize)` in `shepr-protocol` (owned next to
`PublicPaneId`, whose `number()` returns it), and a `PaneNumbering { next }`
allocator inside the workspace that hands out a `ReservedPaneNumber` at prepare
time and is consumed by commit (so "the number the child's `SHEPR` id was built
from is the one the commit registers" is a type fact, not a comment).
`WorkspacePane::new` takes the number. The snapshot field becomes required and
nonzero by deserialization.

### 1.2 Saved pane keys and workspace positions: bare `u32` and `usize`

The snapshot keys a pane by `PaneId::raw()`: `WorkspaceSnapshot::panes:
HashMap<u32, PaneSnapshot>`, `LayoutSnapshot::Pane(u32)`, `focused:
Option<u32>`, `root_pane: Option<u32>`, `WorkspaceHistorySnapshot::panes:
HashMap<u32, ..>`, `SessionHistory::workspaces: Vec<Vec<(u32, HistoryText)>>`,
`HistoryStamp::panes: Vec<Vec<(u32, PaneStamp)>>`. Workspace positions are
`usize`: `PaneKey = (usize, u32)`, `capture_pending_*_for_snapshot(..,
terminal_ids: &HashMap<(usize, u32), TerminalId>, ..)`, the server's
`PreservedLayout::terminal_ids`, `SessionSnapshot::active: Option<usize>`,
`RestoredSession::active`, `remap_saved_index`.

Restore is where this hurts: `plan_workspace` and `restore_workspace` juggle
`id_map: HashMap<u32, PaneId>`, `reverse_id_map: HashMap<PaneId, u32>`,
`numbers: HashMap<u32, usize>` and `public_pane_ids_by_old_raw: HashMap<u32,
PublicPaneId>`; three different `u32`-keyed maps in one function, where the
value of one is a public number (`usize`) that looks like any other count.

Proposal: `SavedPaneKey(u32)` (serde transparent, minted only by capture and by
deserialization) and `SavedWorkspaceIndex(usize)`, with `SavedPaneRef {
workspace, pane }` replacing `(usize, u32)`. `PaneId::raw` then has no caller
outside `shepr-core` (see 4.3).

### 1.3 Digests and fingerprints as `String`

Two different SHA-256 identities, both hex `String`:

- History digest: `io::history_digest -> String`,
  `SessionLoad::Loaded::history_digest: Option<String>`,
  `load_history(.., expected_digest: Option<&str>)`, `HistoryIntent::Write {
  digest: String }`, `HistoryIntent::Keep(String)`,
  `ResolvedHistory::Unchanged(String)`, `HistoryCarry::saved: Option<(HistoryStamp,
  String)>`, `WrittenHistory::digest`, `SavedSession::history_digest`,
  `SessionWriter::save -> io::Result<Option<String>>`,
  `save_keeping_history(.., digest: String, ..)`.
- Layout fingerprint: `snapshot::layout_fingerprint -> Option<String>`,
  `SnapshotLayoutFingerprint::fingerprint: Option<String>`,
  `layout_differs_from_latest(.., latest: Option<&String>)`.

They are interchangeable to the compiler, and they are produced by two separate
hex encoders (`history_digest` with a lookup table, `layout_fingerprint` with
`write!("{:02x}")`). Proposal: `HistoryDigest([u8; 32])` and
`LayoutFingerprint([u8; 32])` with one hex serde impl. `layout_fingerprint`
returning `Option` only because `serde_json::to_vec` of a plain struct "might
fail" also goes away if the fingerprint hashes a canonical byte encoding it
writes itself (it already sorts pane IDs by hand to be deterministic).

### 1.4 Git object IDs, ref names and branch names as `String`

`valid_oid` and `valid_full_ref` are validators whose result is thrown away:
the value stays a `String` and is re-validated or not at the next site.

- `GitHeadIdentity::Branch { full_ref: String, short_name: String, oid:
  Option<String> }`, `Detached { oid: String }`, `GitUpstreamIdentity {
  remote, merge_ref, full_ref, oid }` all `String`.
- `git_rev_parse_verify_with_errors` returns rev-parse stdout as an "oid"
  without `valid_oid`, so `GitHeadIdentity` can hold an unvalidated OID on the
  reftable path, which is why `git_ahead_behind_between` validates both OIDs
  again before building a range. The re-check is the symptom.
- `BranchConfig::full_ref` comes from `for-each-ref %(upstream)` and is only
  validated if it happens to go through `read_ref_oid_with_errors` (files
  backend); on the reftable path it goes to `rev-parse --verify
  --end-of-options` unvalidated (safe only because of `--end-of-options`).
- `short_name` is derived by `strip_prefix("refs/heads/")` at two sites
  (`read_head_identity_from_git`, `read_head_identity_from_files`).
- `upstream_full_ref(&BranchConfig) -> Option<String>` always returns `Some`.

Proposal: `Oid` (40 or 64 lowercase hex, parsed once), `FullRefName`
(validated), `BranchName` (only obtainable from a `FullRefName` under
`refs/heads/`). `WorkspaceGitStatus::branch` becomes `Option<BranchName>`.

### 1.5 Git cache dependencies: anonymous tuples with a poison flag

In `git/config.rs` and `git/status.rs`:

- `type FileStamp = Option<(Option<SystemTime>, u64)>`
- `type FileDep = (PathBuf, FileStamp, bool, Option<PathBuf>)`, where `.2` is
  "reusable" and `.3` the canonical target at capture time.
- `type ConfigCtx = (String, Option<BranchConfig>, Vec<FileDep>)` (branch name
  as `.0`).
- `type RepoContext = (GitWorktreeInfo, bool, Vec<FileDep>, Option<ConfigCtx>)`
  where `.1` is "reftable".

The whole-set "do not cache this" decision is made by flipping `.2` on one
element: `deps.first_mut().2 = false` (three sites in `config.rs`), `dep.2 =
false` on a single synthesized dep, `deps[0].2 &= ..` in `repo_context`, and a
loop setting every `.2 = false` when the second config query differs.
`deps.first_mut()` on an empty vector silently poisons nothing; today the vector
is never empty, by construction elsewhere. `same_head_and_repository_context`
compares `.0`, `.1`, `.2` by hand and leaves `.3` out.

Proposal: `struct FileDep { path, stamp: FileStamp, target: Option<PathBuf> }`,
`enum Dependencies { Tracked(Vec<FileDep>), Uncacheable }` (or a `cacheable`
field on the set, not on an element), `struct RepoContext { info, backend:
RefBackend, deps: Dependencies, branch_config: Option<BranchConfigCtx> }` with
`enum RefBackend { Files, Reftable }`, and `struct BranchConfigCtx { branch:
BranchName, config: Option<BranchConfig>, deps }`.

### 1.6 `GitStatusCacheEntry`: one `Option<Instant>` encoding four states

`GitStatusCacheEntry { fingerprint: Option<..>, retry_after: Option<Instant>,
snapshot, read_errors }`:

- `fingerprint: None, retry_after: Some(t)`: negative entry (not a repo, or
  discovery failed), retry at `t`.
- `fingerprint: Some, retry_after: None`: ahead/behind computed (or no upstream).
- `fingerprint: Some, retry_after: Some(now)`: ahead/behind never computed
  (the branch-only path writes `Some(now)` as "stale immediately").
- `fingerprint: Some, retry_after: Some(now + delay)`: ahead/behind failed.

The reader in `git_status_snapshot_for_cwd_with_demand` distinguishes these by
`retry_after.is_none_or(|r| r > now)`, and the server's
`GitRefreshScheduler::mark_due` decides which entries are negative by peeking at
`entry.fingerprint.is_some()`. Proposal: `enum GitStatusCacheEntry { Miss {
retry_after, snapshot, errors }, Hit { fingerprint, ahead_behind:
AheadBehindState, errors } }` with `enum AheadBehindState { NotComputed,
Known(Option<AheadBehind>), Failed { retry_after } }`, and an `is_miss()` the
scheduler calls instead of reading fields.

### 1.7 Git status cache key: one `PathBuf`, two meanings

The key is the canonical checkout root for a repo, or the raw resolved cwd for a
non-repo (`deduplicate_git_refresh_items`: `git_status_cache_key(..)
.unwrap_or_else(|| item.resolved_identity_cwd.clone())`), and
`Workspace::mark_identity_undiscovered` seeds `cached_git_status_key` with
`identity_cwd`, a third meaning (placeholder). The status snapshot is then
computed with the key passed as the cwd. Proposal: `enum GitStatusKey {
Checkout(PathBuf /* canonical */), Outside(PathBuf) }`, minted only by
discovery.

### 1.8 `GitReadError` carries prose and lies about the path

`GitReadError` is typed at the top but its payloads are `String`
(`arguments: args.join(" ")`, `message: error.to_string()`). Worse, `config.rs`
flattens typed failures: `config_output` and `read_repository_format_value` map
`run_git_output`'s `GitReadError` into `io::Error::other`, and
`read_config_for_status` maps every `config_deps`/`branch_config` error back to
`GitReadError::FileRead { path: <common_dir>/config, .. }`. A Git timeout or
spawn failure while reading config is therefore reported as "could not read
.git/config". `repo_context` does the same with `git_ref_storage_is_reftable`.
Proposal: config probes return `GitReadError` directly; `FileRead` gets a
`reason: FileReadReason { Io(io::ErrorKind), TooLarge, InvalidRefName,
IncompleteOid, UnreadableCommondir, InvalidGitfile }` instead of a message.

The error is also the dedup key (`reported_git_read_errors: HashSet<GitReadError>`
in the server), so embedding volatile `io::Error` text in it makes "the same
error" depend on message wording.

### 1.9 Persister outcomes as prose `io::Error`

`actor.rs` has three refusals, all `io::Error::other(<sentence>)`:
`abandoned()`, `lease_only()`, `stopped_after_panic()`. A retired persister
returns `Ok(())` for a job it never ran. The server's
`App::finish_session_save` cannot tell them apart and treats all of them as a
transient save failure: it re-arms the autosave backoff, increments checkpoint
failure counts, and logs `session save failed` every retry, even though after a
panic no save will ever run again. Proposal: `enum SaveError { Io(io::Error),
PublishedNotDurable(io::Error), Abandoned, Refused(Refusal) }` with `enum
Refusal { LeaseOnly, StoppedAfterPanic, Retired }`, so the loop can stop
scheduling (and say so once) on a refusal, and `Retired` stops being a fake
success.

### 1.10 `SessionLoad::Unusable(String)`

`load` returns `Unusable(format!("it could not be read: {err}"))` or
`Unusable(format!("it could not be parsed: {err}"))`; the server copies the
string into `SessionRestoreLoss::Unusable { reason: String }` on the wire.
`parse_snapshot` and `parse_history_snapshot` return `Result<_, String>`. A typed
`UnusableSession { Unreadable(io::ErrorKind), NotRegularFile(kind), TooLarge,
Unparseable { line, column, category } }` would let the client word it and
would let the server branch (a not-regular-file session is already refused at
startup by `check_session_target`, so `load` should never see it; with a type,
that is visible).

### 1.11 Restore outcome as `bool` + `usize`

`RestoredSession { restore_damage: bool, dropped_workspaces: usize }`; the
server folds those with the `SessionLoad` variant into `protect_unloaded` and
into `SessionRestoreLoss::partial(usize, bool)`. Proposal: restore returns
`RestoreLoss { None, Partial { dropped_workspaces: NonZeroUsize | 0, pruned:
bool } }` and the "protect before first overwrite" decision is made in mux (see
2.13).

### 1.12 The workspace's Git identity: six public fields and an empty-path sentinel

`Workspace` has `cached_identity_cwd`, `cached_auto_label`,
`cached_git_status_key`, `cached_git_branch`, `cached_git_ahead_behind`,
`cached_git_space`, all `pub`. "Undiscovered" is spelled `cached_identity_cwd =
PathBuf::new()` (an empty path that "never matches a resolved cwd"). The server's
`AppState::apply_workspace_git_statuses` writes them one by one with its own
change detection (label change counts only if `custom_name.is_none()`).
Proposal: `enum GitIdentity { Undiscovered { fallback_label }, Admitted {
cwd, key: GitStatusKey, label, branch, ahead_behind } }` owned by the workspace,
with `Workspace::admit_git_status(WorkspaceGitStatus) -> IdentityChange` in mux
and `matches_cwd(&Path)` replacing the empty-path trick in the server's
`cache_key_hint` test.

### 1.13 Workspace IDs stringified and re-parsed

`WorkspaceGitStatus::workspace_id: String` (filled from `ws.id.to_string()` in
`workspace_git_refresh_items`, compared via `PartialEq<String> for WorkspaceId`
in `apply_workspace_git_statuses` and `live_workspace_identity_cwd`),
`WorkspaceSnapshot::id: Option<String>` (written as `Some(ws.id.to_string())`,
read back with `.parse::<WorkspaceId>().ok()`). `WorkspaceId` already derives
`Deserialize` through its canonical parse. Restore silently replaces an
unparseable or duplicate ID with a fresh one and does not count it as damage,
so a damaged ID is lost without the backup the other defects get. Proposal:
`WorkspaceId` everywhere, required in the schema.

### 1.14 Grid sizes as `(u16, u16)` with flipping argument order

`PaneGeometry::pane_size -> Option<(u16, u16)>` is `(rows, cols)`;
`sole_pane_size`, restore's `restored_pane_size` return the same tuple;
`spawn_geometry(rows, cols, cell)` then calls
`shepr_core::geometry::PaneGeometry::with_cell(cols, rows, cell)`. One call
reverses the order of the next. Proposal: return
`shepr_core::geometry::PaneGeometry` (or a `GridSize { rows, cols }`) from the
geometry functions and delete `spawn_geometry`.

### 1.15 Modes and outcomes as bools

Smaller, but each is a two-valued domain fact passed positionally:
`publish_private_file(.., replace: bool)` (`enum Publish { Replace,
CreateNew }`; the doc already describes two different contracts),
`snapshot_history_decision(.., replacement: Option<&SessionSnapshot>, ..)` where
`None` means "after the write", `preserve_existing_in -> io::Result<bool>`,
`RestoredPaneStart::Running { duplicate_agent_session: bool }`,
`PendingHistory::resolve_for_save(.., allow_unchanged: bool)` (really "the
history file is known current"), `apply_pane_chrome(.., pane_gaps: bool,
pane_outer_borders: bool)` and `PaneGeometry`'s three chrome bools (one
`PaneChrome` config value), `Workspace::split_pane(.., focus_new_pane: bool,
..)`, `commit_new_pane -> Option<()>`.

### 1.16 Recovery copy names and kinds

Recovery copies are named by `recovery_filename(u128, usize)` and
`history_recovery_filename(u128, usize)`, parsed back by
`recovery_copy_key(name, prefix)` and `recovery_timestamp` (prefix
`"session-"`), and paired by `recovery_history_path`, which strips
`"session-"` from a file name and prepends `"session-history-"`. Parsing a
history copy with the layout prefix fails only because the digit-width check
rejects `"history"`; nothing in the type says these are different kinds.
Proposal: `struct RecoveryName { stamp: u128, sequence: u16, kind: Layout |
History }` with one `to_file_name`/`parse`, and `pair()` instead of string
surgery. Similarly `enum RecoveryKind { Snapshot, Backup }` carrying directory,
retention limit and log labels: today `log_recovery_preserved` and
`log_recovery_prune_failure` recover the kind by comparing the directory path
with `snapshot_directory(path)`, though every caller knows it.

### 1.17 Cwds: `UsableCwd` exists, then everything is `PathBuf`

`UsableCwd` is only used for the OSC 7 event. `Workspace::identity_cwd`,
`PaneSnapshot::cwd`, `WorkspaceSnapshot::identity_cwd`, `cwd_for_pane`,
`resolved_identity_cwd_from*`, `PendingCwds`, `WorkspaceGitStatus::
resolved_identity_cwd` are `PathBuf`. Restore's `plan_workspace` checks
`is_absolute()` by hand twice (pane cwds, identity cwd). Proposal: an
`AbsolutePath` checked at deserialization for saved cwds (no stat, as restore
requires), and `UsableCwd` (or an `ObservedCwd`) as the return type of the
runtime cwd reads.

### 1.18 `AppEvent::Runtime { event: Box<AppEvent> }`

The envelope can wrap any `AppEvent`, including another `Runtime` and
`GitStatusRefreshed`; inner events repeat `pane_id`, and the server's
`admit_runtime_event` recurses and never checks the inner `pane_id` against the
outer one. Nothing forces runtime-produced events to be tagged at all: an
untagged `PaneDied` is admitted unconditionally. Proposal: split
`RuntimeEvent` (launch settled, died, process detected, state changed,
clipboard, cwd reported) from `AppEvent`, with `AppEvent::Runtime { origin:
RuntimeOrigin { pane_id, generation }, event: RuntimeEvent }` and no `pane_id`
inside `RuntimeEvent`. The only constructor of runtime events is then
`EventSender::runtime`, and `From<Sender<AppEvent>> for EventSender` (the
untagged sender) cannot send them.

### 1.19 Lease liveness as `Option<File>`

`DataDirLease { file: Option<File> }` with `release(&mut self)` and
`is_active()`; `SessionWriter { lease: Option<DataDirLease> }` with
`may_write()`. "Possession of this value is required to construct a session
writer" is the doc's claim, but a released lease is still a `DataDirLease`, and
`load`/`load_history` check `is_active()` and return `Missing`/`None` for a
released one (a fresh start indistinguishable from "you have no lease").
Proposal: `release(self)` consumes; `load(&DataDirLease)` needs no runtime
check; the writer holds the lease by value and retirement drops the writer.

---

## 2. Decisions made in more than one place

### 2.1 What a workspace's identity cwd is

Question: the workspace's identity cwd is the root pane's cwd, else the stored
`identity_cwd`. Sites:

- `Workspace::resolved_identity_cwd_from` /
  `resolved_identity_cwd_from_root_pane` (live; root pane cwd via
  `cwd_for_pane`, which is `PaneRuntime::cwd()` (a raw `/proc` readlink, no
  usability check) else `TerminalState::cwd()`).
- `persist::snapshot::capture_workspace` (save; root pane's saved cwd, which is
  `PaneRuntime::remembered_cwd()` else `TerminalState::cwd()` else the server's
  fallback cwd).
- `PendingCwds::resolve` + `root_pane_cwd` (save, after the `/proc` probe:
  re-derives it for every touched workspace).
- `restore::plan_workspace` (a non-absolute saved `identity_cwd` is replaced by
  the root pane's cwd).

They disagree today: the live answer reads `/proc/<pid>/cwd` without the
usability filter the save path applies (`usable_process_cwd`), so a shell
sitting in a deleted directory gives the Git refresh and label a
`"/x (deleted)"` path while the save writes the last usable one. And the saved
`identity_cwd` field is fully derivable from `root_pane` + `panes` except when
the root pane is gone, which restore treats as damage anyway. Owner: drop
`identity_cwd` from the snapshot (derive at restore), and give the workspace one
`identity_cwd(&dyn PaneCwds)` that both the live path and capture call, over one
pane cwd policy in the pane runtime (the other hunter's scope; see 2.2).

### 2.2 What a pane's cwd is

Within this scope, `Workspace::cwd_for_pane` (runtime `cwd()` else terminal),
`capture_workspace` (runtime `remembered_cwd()` else terminal else fallback) and
`PendingCwds` (probe `read()`) each pick a source. In the pane runtime there are
further answers (`follow_cwd`, `foreground_cwd`). The three in scope answer the
same question ("the cwd that names this pane right now") with different
freshness and different validity. Owner: the pane runtime, returning one
`ObservedCwd` with an explicit freshness (`Live`, `Remembered`), so the
workspace and capture stop choosing.

### 2.3 What a workspace's automatic label is

Sites:

- `Workspace::mark_identity_undiscovered`: `fallback_label_from_cwd(identity_cwd)`.
- `git_status_snapshot_for_cwd_with_demand`: computes `auto_label` from the
  cwd it was called with, which is the cache key (the canonical repo root, or
  the raw cwd), so for a repo it is always the root's own name.
- `WorkspaceGitStatusSnapshot::into_workspace_status`: recomputes `auto_label`
  from the real `resolved_identity_cwd` and `space.repo_root`, overwriting the
  snapshot's.
- The client, from the server's `workspace.checkout_root` answer (2.4), derives
  a new workspace's default label from `git rev-parse --show-toplevel`.

The second is dead except as a cached value and in tests; it is a second label
algorithm kept in step by nothing. Owner: label is a pure function of `(cwd,
Option<checkout root>, home)` (already `shepr_core::workspace_label`); delete
`auto_label` from `WorkspaceGitStatusSnapshot` and the cache entry, compute it
once in `into_workspace_status` (or in the workspace's `admit`).

### 2.4 What the checkout root of a directory is

- mux `git/discovery.rs`: a filesystem walk honouring
  `GIT_CEILING_DIRECTORIES`, gitfiles, bare repositories (`core.bare` via a
  `git config` spawn), skipping `.git` directories without `HEAD`, and
  stopping on unreadable entries.
- server `app/api/checkout_root.rs`: `git rev-parse --show-toplevel`, with
  `stderr.contains("not a git repository")` meaning "outside".

They disagree: inside a bare repository mux says the bare directory is the
checkout root, while `--show-toplevel` fails ("must be run in a work tree")
and the server reports an error; Git's `safe.directory` ownership refusal makes
the server report an error where mux happily reads files; a `.git` directory
without `HEAD` is skipped by mux but makes Git error out. So a new workspace's
default label (client, from the server answer) and the same workspace's label
after the first refresh (mux discovery) can differ.

Within mux the question "is this directory a checkout root" is answered three
times with the same `locate_git_dir` then `git_head_file_is_readable` match:
`git_worktree_info_with_errors`, `git_dir_for_repo_root` (errors to debug log),
and the loop in `git_repo_root_below_with_errors`. The walk finds the
`LocatedGitDir` and throws it away, and `git_worktree_info_with_errors`
re-locates it at the root (a second stat round and possibly a second `git
config core.bare` spawn, with a TOCTOU window between the two answers). On top,
`repo_context` calls `git_worktree_info_with_errors` twice (once to build, once
to verify), and the server calls `git_status_cache_key` (another full
discovery) before that. An uncached refresh therefore runs three or four full
discovery walks per workspace. Owner: one `discover(cwd) -> Discovery {
Checkout(GitWorktreeInfo), Outside, Unreadable(GitReadError) }` returning the
info it found, and the server's `checkout_root` calls it instead of shelling
out.

### 2.5 Which Git exit means "no answer" rather than failure

- `git_trimmed_stdout` dispatches on `args.first() == Some(&"symbolic-ref")`
  (exit 1) and `args.first() == Some(&"rev-parse")` with stderr containing
  `"Needed a single revision"`.
- `read_repository_format_value` and `read_bare`: exit code 1 means unset.
- server `checkout_root`: stderr containing `"not a git repository"`.

Four sites classify Git results, two of them by matching argv[0] against
string literals. Owner: the probe that issues the command declares its
"absent" outcome (`enum Absent { ExitCode(i32), StderrContains(&'static str)
}` passed to one `run_probe`), so the classification sits with the argv, and
`git_trimmed_stdout` stops guessing which command it was given.

### 2.6 Whether a file changed since it was stamped

- `git/config.rs`: `FileStamp = Option<(Option<SystemTime>, u64)>`, mtime and
  length, plus canonical target.
- `persist/writer.rs`: `HistoryFileStamp` (device, inode, length, mtime ns,
  ctime ns), also reused, under its history name, for layout files in
  `SnapshotFingerprintCache`.

They disagree on strength: the Git stamp misses an atomic replace (rename over)
with equal size and mtime, which the persist stamp catches by inode and ctime.
Editors and `git config` both write by rename. Owner: one `FileStamp` in
`shepr-platform` with the persist semantics, used by both.

### 2.7 Whether a workspace may be zoomed

`Workspace::set_zoomed` (refuses with fewer than 2 panes),
`Workspace::from_restored` (`zoomed && panes.len() > 1`),
`restore::plan_workspace` (`snap.zoomed && pane_ids.len() > 1 &&
saved_focus_survived`), `detach_pane` and `commit_prepared_split` (force
`false`), `PaneGeometry::visible_panes` (assumes the zoomed pane is the focus),
and the test invariant checker. They agree today. Owner: zoom lives in the pane
tree (3.2) as a state that cannot be entered with one pane and is cleared by
the tree operations that change the pane set.

### 2.8 How public pane numbers are allocated and validated

`Workspace::valid_public_numbers` (nonzero, below `next`, unique),
`commit_prepared_split` (nonzero, `checked_add` succeeds),
`advance_next_public_pane_number` (saturating),
`restore::assign_public_pane_numbers` (zero filtered as missing, fresh numbers in
layout order), `plan_workspace` (`next = max(max + 1, saved next, 1)`, with its
own exhaustion check), the constructors that hard-code `1`/`2`, and
`PublicPaneId::new`'s assertion. Owner: `PaneNumbering` (1.1).

### 2.9 What size a pane starts at, and what its content rect is

- "A zoomed visible pane starts at the full area, hidden panes at their tiled
  size": `restore::restored_pane_size` (mux) and
  `derived_pending_agent_resume_pane_infos` (server `agent_resume.rs`).
- "Content rect = `visible_panes` then `pane_inner_rect` then
  `terminal_content_rect`": composed by hand in `PaneGeometry::pane_size`,
  `ui/panes.rs` (twice), `retained_surface.rs`, `agent_resume.rs`.
  `PaneChromeInfo::inner_rect` and `scrollbar_rect` are placeholders at
  construction (`inner_rect = rect`, `scrollbar_rect = None`, documented as
  "not settled here") and every caller overwrites them.
- The "narrow pane" threshold `<= 4` columns is in `terminal_content_rect` and
  again, independently, in `ui/panes.rs` (label and border-title rules).

Owner: `PaneGeometry::content_layout(layout, zoomed, |pane| alternate_screen)`
returning settled rects (no placeholder fields), plus `spawn_size(layout,
zoomed, pane)` implementing the hidden-pane rule once. The threshold becomes a
named constant beside it.

### 2.10 How a snapshot keys its panes

`capture_workspace` keys each pane by `(workspace index, PaneId::raw())`. The
server's `capture_preserved_layout` builds `HashMap<(usize, u32), TerminalId>`
by the same rule independently, then compares only the total count with the
snapshot's. A change to one keying rule passes that check. Owner: capture
returns the index it used (`capture_deferred` already returns `(snapshot,
PendingCwds)`; add `SavedPaneRef -> TerminalId`), and
`capture_pending_*_for_snapshot` take that value.

### 2.11 Which history revisions are restored ones

`next_restored_revision` sets the top bit of a `u64`; `PaneHistoryCache`'s own
counter (pane scope) "never reaches the top bit". Two files keep a partition of
one integer space. Owner: `enum HistoryRevision { Live(u64), Restored(u64) }`,
or the cache's allocator issues restored names too. `PaneStamp = Option<u64>`
becomes `Option<HistoryRevision>`.

### 2.12 Whether a save may run

`DataDirLease::file` (None after release), `SessionWriter::lease` (None after
retire, `may_write`), `PersistState::accepting_jobs` (false after a panic),
`Worker::LeaseOnly` (refuse with error), `Worker::Retired` (accept and report
success), and `load`'s `lease.is_active()`. Five flags across three types, with
three different answers to "this save will not happen" (error, error, success).
Owner: the persister's worker enum is the state machine; the writer and lease
should not carry their own retired flags (1.19), and refusals are typed (1.9).

### 2.13 Whether the on-disk session must be preserved before the first overwrite

The server computes `protect_unloaded = persists && snapshot.is_none()`, which
folds `SessionLoad::Missing` (including "lease inactive") in with `Unusable`,
then sets it again on `SessionRestoreLoss::partial(dropped, damage)` (a
protocol-crate function deciding what counts as loss). The writer then decides
when the protection is discharged (`preserve_existing` returns `false` on
`NotFound` and keeps it armed until a save lands). The rule is split across
three crates. Owner: a mux `persist::open_session(lease, policy, ..) ->
OpenedSession { restored, persister, notice: Option<RestoreNotice> }` that
decides protection from its own load and restore outcomes.

### 2.14 Whether a session path is a usable regular file

`check_session_target` (`fs::metadata`, kernel symlink resolution, up to 40
hops), `ensure_replaceable` after `resolve_write_target` (manual resolution,
`MAX_SESSION_PATH_SYMLINK_HOPS` = 16), `open_regular` (platform helper), and
`HistoryFileStamp::read` (a non-file stamps as absent). They disagree: a chain
of 17 to 40 symlinks passes the startup check, then every save fails with
`InvalidInput`, which is exactly the case the startup check exists to refuse;
and a directory at the history path stamps as "no file", same as absence.
Owner: one `SessionPath::resolve(path) -> Resolved { target, state: Absent |
Regular | NotRegular(kind) }` used by the check, saves, clears, reads and
stamps.

### 2.15 Whether Git status demand is partial

The server always passes `GitStatusRefreshDemand { branch: true, ahead_behind:
true }` (with a comment saying to keep it full), yet mux implements a separate
branch-only path in `git_status_snapshot_for_cwd_with_demand` (with its own
cache-merging rules), `WorkspaceGitStatus` carries the demand, and
`apply_workspace_git_statuses` checks `demand.branch` and
`demand.ahead_behind`. That path is unreachable in production and is where the
`retry_after = Some(now)` sentinel comes from. Owner: none; delete the demand.

### 2.16 Which Git cache entries are kept

`status.rs` decides retry timing; the server's `GitRefreshScheduler::mark_due`
drops negative entries by `fingerprint.is_some()`, and `finish` drops entries
not refreshed this round and prunes the error-dedup set. The cache's semantics
are split between its producer and a scheduler that reads its fields. Owner: a
`GitStatusCache` type next to the status code (3.5).

### 2.17 Layout tree operations and pane adjacency

- `restore::prune_restored_node` decides how a split collapses when one child
  goes; `shepr_core::layout::remove_pane` decides the same for live closes.
- `restore::collect_pane_ids`/`collect_ids_inner` duplicate
  `TileLayout::pane_ids`; `collect_snapshot_pane_ids` walks the snapshot tree.
- The `Direction` to `DirectionSnapshot` mapping is in `capture_node` and its
  inverse in `remap_inner`.
- `workspace/geometry.rs` decides adjacency (`ranges_overlap`, `pane_to_right`,
  `pane_below`, `u16` saturating) separately from `shepr-core`'s
  `find_in_direction`/`ranges_overlap` (`u32` ends). At the right or bottom edge
  of a `u16::MAX` area these differ.

Owner: `shepr-core::layout` (a `Node::prune(&surviving)` beside
`remove_pane`, `Node::pane_ids`, and one adjacency helper).

---

## 3. Structure

### 3.1 `Workspace` is a bag of public fields

`Workspace` exposes `id`, `custom_name`, `identity_cwd`, the six Git caches and
`next_public_pane_number` as `pub`, and the tree fields as `pub(crate)` so that
`persist` can read them for capture and fill them on restore. The server writes
Git fields directly; persist reaches into `ws.panes`, `ws.layout.root()`,
`ws.root_pane`, `ws.zoomed`. There is no place where a workspace's invariants
are enforced except `valid_panes` on restore. Proposal:

```
Workspace { id: WorkspaceId, name: Option<WorkspaceName>, tree: PaneTree, git: GitIdentity }
```

with capture going through a `Workspace::to_snapshot()`/`from_snapshot()` pair
that lives in `persist` but only uses public read methods of `PaneTree`. The
"display name" getter returning a cloned `String` each call (it is read by
sidebar, window title and API on every projection) would become `&str`.

### 3.2 The pane tree: two sets of pane IDs kept in agreement at runtime

`layout: TileLayout` (leaves), `panes: HashMap<PaneId, WorkspacePane>`, and
`root_pane` must name the same panes. `has_consistent_panes` recomputes this
(a `Vec` plus a `HashSet` allocation) and is called by `focus_pane`,
`swap_panes`, `resize_focused_pane`, `resize_pane`, `set_split_ratio_at`,
`commit_prepared_split`, `detach_pane`. That is a runtime re-proof, on every
drag-resize event, of an invariant that the representation should hold. The
split two-phase dance (`split_pane` prepares on a cloned `TileLayout`, the
caller later calls `commit_new_pane`, which re-diffs the two layouts' pane sets
to verify the clone is "this layout plus exactly one pane") exists because the
layout is edited separately from the records. Proposal: a `PaneTree` that owns
`Node`, focus, zoom and the records together; `prepare_split` returns a
`PreparedSplit` token (new id, reserved number, spawn size) that `commit`
consumes, so no diff is needed and the records cannot drift from the leaves.
Whether the tree stays in `shepr-core` (with records generic) or moves to mux
is a detail; the point is that one type owns the leaves and the records.

### 3.3 Workspaces launch processes

`Workspace::spawn` and `split_pane`/`split_pane_shell` call
`PaneRuntime::spawn` with a dozen arguments (theme, appearance, shell config,
scrollback, launch env, four channel handles), and `persist::restore` does the
same per pane. The documented principle is that state is plain data and the
runtime is held by `App` outside `AppState`; here the plain-data type constructs
runtimes. And `restore` re-bundles the very handles `PaneSpawnHandles` exists to
carry, as its own `RestoreRuntimeContext`, taking twelve parameters. Proposal:
workspace operations are pure plans (`PreparedSplit`, `WorkspaceRestorePlan`
already exists and is pure), and one `PaneLauncher` (holding
`PaneSpawnHandles` plus the launch settings) in the app executes them. Restore
becomes `plan(snapshot) -> Vec<WorkspaceRestorePlan>` (pure, testable without
PTYs) and `launch(plan, &PaneLauncher)`.

### 3.4 Pane chrome geometry is view code living in mux

`workspace/geometry.rs` is ratatui-based (`Block`, `Borders`, ratatui `Rect`)
border and gap logic. Its consumers are the server's UI and surface code; mux
itself needs only "what size does this pane spawn at". The file has two
hand-written `Rect` converters because `shepr_core::geometry::Rect` and ratatui's
`Rect` are both in play. Proposal: pure pane chrome math on
`shepr_core::geometry::Rect` (borders as a small bitset of its own) in
`shepr-core`, with the ratatui adapter in the server. mux then loses its ratatui
dependency except for whatever `pane/` needs.

### 3.5 Git status: a subsystem split across a crate boundary through its cache

mux `git/` has the runner, discovery, config dependency tracking and status; the
server's `app/git_refresh.rs` has the scheduler, the cache map
(`HashMap<PathBuf, GitStatusCacheEntry>`), deduplication by key, cache pruning
and read-error dedup. The cache travels: the app clones it into the worker
thread, the worker returns `cache_updates` inside
`AppEvent::GitStatusRefreshed`, and the app merges them back. `GitStatusCacheEntry`
fields are public because the server reads them. Proposal: a `shepr-git` crate
(runner, discovery, config, status, and a `GitStatusCache` that owns
`refresh(targets) -> Vec<WorkspaceGitStatus>` and its retention policy). The
cache can live on a long-lived worker so it never crosses the event channel;
`GitStatusRefreshed` then carries results only. `run_git` (used by the server's
checkout-root probe) comes from there, and per 2.4 that probe goes away anyway.
mux keeps only the `GitIdentity` value types the workspace holds.

### 3.6 Persistence orchestration lives in the server

`App::with_paths` sequences `persist::load`, the experimental-history gate,
`load_history`, `persist::restore`, the loss and protection decisions, the
restore log line, the empty-workspace fallback (re-deciding `active = None`
that restore already decides), and `SessionPersister::spawn`/`lease_only`. The
server's `session.rs` also builds the preserved-layout terminal map (2.10) and
decides "no workspaces means `PersistJob::Clear`" and the fallback cwd. All of
these are persistence policy. Proposal: `persist::open_session` (2.13) and a
`persist::capture_job(workspaces, terminals, runtimes, ..) -> PersistJob` that
owns the Clear-vs-Save decision and returns the pane index the checkpoint code
needs.

### 3.7 `persist/` files each do several jobs

- `snapshot.rs`: the on-disk schema (serde types, version, path-bytes codec),
  capture from live state (reads runtimes), and the history carry engine
  (`HistoryCarry`, `PendingHistory`, stamps, revision names).
- `io.rs`: path layout, the regular-file policy, atomic publish, the session
  file read/write, and the hand-written history JSON serializer with its
  fair-share trim algorithm.
- `writer.rs`: save orchestration (history intents, digests, fingerprint cache)
  and the recovery-copy subsystem (naming, cadence, pruning, orphan cleanup),
  roughly half each.

Proposal: `persist/schema.rs` (format types only), `persist/capture.rs`,
`persist/history/{carry.rs, serialize.rs}`, `persist/files.rs` (paths, publish,
`SessionPath`), `persist/recovery.rs` (snapshot and backup copies,
`RecoveryName`, `RecoveryKind`), `persist/writer.rs` (just the save sequence).

### 3.8 The schema carries compatibility optionality nothing needs

Per AGENTS.md there is no on-disk state to stay compatible with, yet:
`WorkspaceSnapshot::id: Option<String>` with `#[serde(default)]`,
`custom_name` default, `next_public_pane_number` default `0`, `focused` and
`root_pane` as `Option<u32>` with defaults, `PaneSnapshot::public_number`
optional and tolerated at zero, `SessionSnapshot::host_theme` default,
`SavedHostTheme::palette` default, `history_digest` default. Restore then has
fallback code for each (focus to first leaf, root to first leaf, fresh workspace
IDs, fresh numbers). Each absent value should be a parse error, which the
existing "drop the workspace and back up the file" path already handles. (The
agent-session tolerance in `deserialize_agent_session` is different: a newer
build reading an older build's file can lose an agent kind, so keep it.) Also,
the layout file is written by `SavedSession` (`#[serde(flatten)]` over the
snapshot) and read twice, once as `SessionSnapshot` and once as
`SavedHistoryReference`; one `SessionFile { snapshot, history_digest }` type
for both directions removes the asymmetry and the second parse.

### 3.9 `limits.rs` and `logging.rs` are crate-wide grab-bags

`limits.rs` mixes Git timeouts, detection cadences, history chunking, copy-mode
word separators, OSC bounds, snapshot cadence and pane teardown signals.
`FIRST_WORKSPACE_NUMBER` restates `WorkspaceId`'s "zero has none" rule. Each
constant belongs beside the policy it parameterises (the doc comments already
explain that policy). `logging.rs` is half pane (taking `pane_id: u32`, i.e.
`PaneId::raw()`) and half persist, while `writer.rs` also emits its own events
with hand-written `event = "persist.snapshot"` etc. literals; the event names
are a closed set spelled as strings at a dozen sites.

### 3.10 Workspace ID allocation

The process-global `NEXT_WORKSPACE_NUMBER` is justified in a comment, but the
workspace list that owns uniqueness is `AppState::workspaces`, and restore has
to call `reserve_workspace_ids` before any allocation to keep them disjoint (an
ordering rule enforced by a comment). An allocator owned by the app state and
passed to restore makes the ordering structural. Lower priority than the rest.

---

## 4. Types that resolve to primitives

- `WorkspaceId`: `Deref<Target = str>`, `From<WorkspaceId> for String`,
  `PartialEq<String>`/`PartialEq<str>`. Used to stringify into
  `WorkspaceGitStatus::workspace_id` and `WorkspaceSnapshot::id` and to compare
  against those strings (1.13). Remove the escape hatches; carry the type.
- `PublicPaneId`: `Deref<Target = str>` and string equality; `number() ->
  usize` (1.1).
- `PaneId::raw()` / `from_raw()`: the snapshot's key space (1.2), the pane
  logging functions (`logging::pane_spawned(pane_id.raw(), ..)` etc.), and a
  test that compares `raw() as usize` with a public number. With
  `SavedPaneKey` and a `tracing::Value` for `PaneId`, `raw` can become
  crate-private to `shepr-core`.
- `WorkspacePane`: `Deref`/`DerefMut` to `PaneState`, with `pane_state` and
  `public_number` also `pub`, so callers can mutate the number freely.
- `SplitRatio` exists, but `TileLayout::split_pane(.., ratio: f32)` (called
  with a literal `0.5` by `split_pane_shell` while core has `EVEN_SPLIT`),
  `set_ratio_at(f32)` and `LayoutSnapshot::Split::ratio: f32` take the
  primitive.
- `UsableCwd` (1.17): introduced, then unwrapped into `PathBuf` at the next
  boundary.
- Type aliases posing as types: `FileStamp`, `FileDep`, `ConfigCtx`,
  `RepoContext` (1.5), `PaneKey`, `PaneStamp`, `RestoredWorkspace` (a three
  tuple), `Drain`.
- Sentinels:
  - `cached_identity_cwd = PathBuf::new()` for "undiscovered" (1.12).
  - `public_number = 0` (1.1); snapshot `next_public_pane_number` defaulting to
    `0`.
  - `retry_after = Some(now)` for "ahead/behind not computed" (1.6).
  - `SnapshotLayoutFingerprint { has_workspaces: true, fingerprint: None }` for
    "unreadable, assume it matters"; this wants `enum SavedLayout { Empty,
    Known(LayoutFingerprint), Unknown }`.
  - `repo_name` falling back to the literal `"repo"`.
  - `cached_auto_label = String::new()` in `assemble` before
    `mark_identity_undiscovered` overwrites it.
  - `fair_share` returning `usize::MAX` for "no pane needs trimming".
  - `SessionWriter::save -> Ok(None)` meaning both "saved, no history" and
    "retired, wrote nothing".
  - `WorkspaceGitStatus::branch: None` meaning detached, not demanded (2.15),
    or read failed.
- String-typed classification: argv[0] matching in `git_trimmed_stdout`,
  stderr substring matching (`"Needed a single revision"`, `"not a git
  repository"`), recovery-name prefixes (1.16), the directory-path comparison
  that recovers `RecoveryKind`, and tracing event names.
- `GitSpaceMetadata::key` and `checkout_key` are canonical paths rendered with
  `display().to_string()`, which is lossy for non-UTF-8 paths, so two distinct
  paths can produce one key. (But see 5.1: nothing reads them.)

---

## 5. Lateral findings

### 5.1 `GitSpaceMetadata` is dead state that costs work and renders

`Workspace::cached_git_space` and `WorkspaceGitStatus::space` are computed on
every refresh (`git_space_metadata_from_info`: two `canonicalize` calls plus
`embedded_bare_repo_container`, which runs another `locate_git_dir` and possibly
a `git config` spawn), cached, and compared in
`apply_workspace_git_statuses`, where a difference sets `changed = true` and
triggers a shell-projection rebuild and a render. No consumer in the server,
client or protocol reads `key`, `checkout_key`, `repo_name` or
`is_linked_worktree`; only `repo_root` is used, for the label. Delete the type,
keep `Option<repo_root>` (or a `Checkout` marker) for the label.

### 5.2 Persister refusals are retried forever as ordinary failures

See 1.9. After a persister job panics, every autosave and checkpoint fails with
`stopped_after_panic`, and the loop keeps scheduling them with backoff and
warning each time, and pane-exit checkpoints run their failure budget before
releasing exits. A retired persister reports success for jobs it never ran,
which would mark a pane-exit checkpoint as durable if one were submitted after
retirement.

### 5.3 Saved workspace IDs are dropped without damage accounting

See 1.13. A saved `id` that fails to parse, or repeats an earlier workspace's,
silently gets a fresh ID. Other per-workspace defects set `restore_damage` so
the first save backs the file up; this one does not, so the original IDs (which
API clients may have recorded) are overwritten without a backup.

### 5.4 Symlink hop limits disagree between the startup check and saves

See 2.14: a session path behind 17 to 40 symlinks is accepted at startup and
then fails every save.

### 5.5 The live identity cwd can be an unusable `/proc` path

See 2.1: `PaneRuntime::cwd()` is a raw readlink, so a shell in a deleted
directory feeds `"<path> (deleted)"` to Git discovery and the label, and every
refresh retries discovery on a path that cannot exist.

### 5.6 Git config read errors misattributed

See 1.8: a Git spawn failure or timeout while listing config origins is logged
as "could not read <common dir>/config".

### 5.7 Repeated discovery per refresh

See 2.4: three or four full discovery walks per uncached workspace refresh,
each able to spawn `git config core.bare` on git-dir-shaped directories.

### 5.8 `has_consistent_panes` on the drag path

`set_split_ratio_at` and `resize_pane` run `has_consistent_panes` (a `Vec` and
a `HashSet` per call) for each mouse-drag resize event. Cheap per call, but it
is pure overhead that disappears with 3.2.

### 5.9 `display_name()` and `branch()` clone per read

Both return owned `String`s and are read on projection and title paths for
every workspace on every refresh; returning `&str` / `Option<&str>` is free.

### 5.10 `RenderSignal` test-only duplicate

`request_pty` (test-only) restates the logic of `request_pty_coalesced` minus
the flag; the tests exercising coalescing semantics therefore test a copy.
Out of the four questions, but a test of a copy is a drift hazard of the same
kind as a pairwise agreement test.

### 5.11 `split_pane` reports one condition through two channels

`Workspace::split_pane` returns `None` when the target is not in the workspace,
then `split_pane_shell` returns `Err(io::ErrorKind::NotFound)` for "target not
in the layout", a condition the first check already ruled out unless the layout
and records disagree. `Option<io::Result<NewPane>>` would become
`Result<NewPane, SplitError { UnknownPane, Spawn(io::Error) }>`.

---

## Suggested order

1. Delete dead weight first: `GitSpaceMetadata`, `GitStatusRefreshDemand` and
   the branch-only path, `auto_label` in the status snapshot, schema
   optionality (3.8). Each shrinks the surface the later moves touch.
2. Identity types: `SavedPaneKey`, `PanePublicNumber` with `PaneNumbering`,
   `HistoryDigest`/`LayoutFingerprint`, `Oid`/`FullRefName`/`BranchName`,
   `WorkspaceId` without string escape hatches.
3. `PaneTree` and `GitIdentity` inside a fieldless-public `Workspace`; pure
   split/restore plans executed by one `PaneLauncher`.
4. Typed persister refusals and `persist::open_session`; split `persist/` files
   along schema, capture, history, files, recovery.
5. `shepr-git` with its own cache and one discovery used by the server's
   checkout-root answer too.
