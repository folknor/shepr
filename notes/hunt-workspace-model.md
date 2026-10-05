# Hunt: the workspace and pane model

Scope read in full: `crates/shepr-core/src/` (every file), `crates/shepr-mux/src/workspace.rs` and
`workspace/`, `terminal/`, `events.rs`, `cwd.rs`, and in `crates/shepr-server/src/app/`: `state.rs`,
`actions.rs` and `actions/`, `api.rs` and `api/` (minus `api/panes/reports.rs`), `creation.rs`,
`pane_launch.rs`, `pane_resize.rs`, `events.rs`, `outputs.rs`, `terminal_titles.rs`,
`git_refresh.rs`, `mod.rs`. Followed into `server/headless/render.rs`, `ui/pane_surface.rs`,
`ui/panes.rs`, `retained_surface.rs`, `api_helpers.rs`, `logging.rs`, `limits.rs`,
`shepr-protocol/src/ids.rs`, `persist/restore.rs`, `persist/capture.rs`,
`pane/exit_arbiter.rs`, `shepr-test-fixtures/src/config.rs`, the client's new-workspace
prompt, `default-server.toml` and `docs/config.md`.

IDs are `WM-<n>`; they are for this note only.

## 1. Defects

**WM-1. A saved workspace id near the top of the number space panics the server on the next
workspace creation.** `WorkspaceId::from_str` (`shepr-protocol/src/ids.rs`) accepts any
bijective base-32 number up to `usize::MAX` (`"wZZZZZZZZZZZZZ"`, or 13 `0` digits). Restore
calls `workspace_ids.reserve(...)` (`persist/restore.rs`) and `WorkspaceSet::restored` reserves
again; `WorkspaceIdAllocator::reserve` saturates `next` to `usize::MAX`, `try_allocate` then
returns `None`, and `allocate` panics ("workspace id space exhausted"). The next
`prepare_workspace` (a `workspace.create` command, or `create_default_workspace` once the
last workspace closes) takes the server down. The claim broken: `WorkspaceId`'s own doc ("no
workspace ID exists that the server's allocator could not have issued": the allocator never
issues `usize::MAX`), and the hunt's "abort on storage-controlled input" rule. `prepare_split`
handles the same exhaustion for pane numbers gracefully (`checked_next()?`), so the two
counters already disagree on policy. Fix: make exhaustion a refusal (`try_allocate` out of
`prepare_workspace`, an `EndpointError::ResourceFailure`), and/or bound the number at parse.

**WM-2. Exhausted pane numbers are reported as "pane not found".** `handle_pane_split`
(`api/panes.rs`) maps `prepare_split` returning `None` to `pane_missing(&params.pane_id)`, but
`prepare_split` also returns `None` when `next_number.checked_next()` fails. The client is told
`PaneGone` for a pane that exists. Practically unreachable (needs a saved `next_public_pane_number`
of `usize::MAX`, which `admit_numbers` accepts), but it is a refusal naming the wrong cause.
Make `prepare_split` return a `Result` with a distinct refusal.

**WM-3. Two writers of a pane's cwd with two invalidation rules.** `StateEvent::TerminalCwdReported`
(`actions/events.rs`) sets the cwd only when it differs and marks both the session and the shell
projection dirty. `handle_pane_launch_settled` (`pane_launch.rs`, `LaunchOutcome::Launched`)
calls `set_cwd(cwd)` directly through `pub(super)` fields, unconditionally, marks the session
dirty, and does not advance the shell projection revision. The launched cwd differs from the
requested one whenever the child's chdir fell back to `HOME`/passwd home/`/`, so the projected
pane cwd (`SnapshotPane::cwd`) is stale until the 1 s `SHELL_CWD_REFRESH_INTERVAL` timer
rebuild (`render.rs`, "bounds how long any missed invalidation can leave a client stale"). The
comment in `apply_runtime_state_event` ("the projection revision is what says the cwd moved")
is true only for one of the two writers. Fix: route the launch's cwd through the same reducer
(`StateEvent::TerminalCwdReported`).

**WM-4. `process_cwd_is_deleted` is applied to paths that never came from `/proc`.** The
`" (deleted)"` suffix is a kernel marker on a `/proc/<pid>/cwd` readlink, but `terminal_cwd`
filters the stored terminal cwd (an OSC 7 report or a saved path) with it too, and
`resolved_identity_cwd_from_root_pane` filters whatever it is handed. A real directory named
`build (deleted)` reported by OSC 7 is discarded as unusable, and a workspace rooted there falls
back to its construction cwd. The guard is keyed on a spelling it was written for one source of;
it should run only on the `/proc` observation (in `PaneRuntime::cwd`/`follow_cwd`/
`remembered_cwd`), not on stored state.

**WM-5. Stale configuration documentation after the border removals.** Commits `0810c21e` and
`b3110f94` removed `pane_borders`/`pane_outer_borders`, but:
- `default-server.toml` header: "whether panes have borders, gaps and scrollbars" (borders is no
  longer a choice);
- `docs/config.md` line 8: "pane borders, gaps and scrollbars and their colours" (borders are
  not a setting, and the same file says every colour is the client's);
- `docs/config.md` "Config never crosses hosts": "a remote machine's panes use the shell,
  borders and scrollbars set on that machine".
These are documented claims that no longer hold. Enforceable only by review; a textlint for
`borders` near `server.toml` would be too blunt.

**WM-6. `PaneChrome`'s doc and `chrome.rs`'s module doc describe gap geometry that does not
exist.** `PaneChrome::rect` is documented as "the outer rect (gaps already taken off)" and the
module as computing "the gaps between panes", but `apply_pane_chrome` never shrinks a rect: with
`pane_gaps` on, every pane keeps all four borders so two adjacent boxes draw double lines with
no gap cell (the test `pane_gaps_keep_independent_bordered_panes` asserts the rects still
touch). Either the docs or the setting's description ("Keep split panes visually apart") should
be reworded to "each pane draws its own border instead of sharing the divider".

**WM-7. Lone-pane zoom has two contradicting policies.** `AppState::toggle_pane_zoom` documents
that a one-pane workspace's toggle "changes nothing, though the pane is still focused" and
implements that branch; `handle_pane_zoom` (`api/panes/geometry.rs`), its only production
caller, short-circuits a lone pane first and documents "does nothing at all (no focus, no
navigation)". The reducer's lone-pane branch and its `set_zoomed` refusal branch are
unreachable from production; only `snapshot_tests.rs` reaches the reducer. One rule, one owner:
put the lone-pane decision in the reducer and delete the endpoint's copy.

## 2. One value, one owner

**WM-8. "A workspace has one pane" is spelled six times.** `shepr-core/src/limits.rs` owns
`MIN_WORKSPACE_PANES = 1`, used only by `TileLayout::close_focused`/`close_pane`. The same rule
is re-spelled as literals in `PaneTree::remove` (`len() <= 1`), `PaneTree::set_zoomed`
(`len() < 2`), `PaneTree::plan` (`numbers.len() > 1`), `WorkspaceSet::remove_pane`
(`len() > 1`), `AppState::toggle_pane_zoom` (`len() <= 1`) and `handle_pane_zoom`
(`len() <= 1`). Not diverged yet. Fix: a `PaneTree::is_lone()` (or `can_zoom()`/`can_remove()`)
and delete the server-side copies. Enforceable by a textlint on `tree\(\)\.len\(\)\s*[<>]=?\s*[12]`
in `crates/shepr-server/src/app/**`.

**WM-9. The new split's ratio and zoom rule are spelled at prepare and again at commit.**
`Workspace::prepare_split` sizes the PTY against `planned.split_pane(target, direction,
SplitRatio::EVEN, pane)` with `zoomed = false`; `PaneTree::commit_split` independently installs
`SplitRatio::EVEN` and sets `zoomed = false`. If either site changed (a configurable split
ratio, a split that keeps the zoom), the child would be spawned at one size and laid out at
another, which is exactly what `PreparedSplit` exists to rule out ("nothing can pair a launched
child with another geometry"). Fix: `PreparedSplit` carries the planned `TileLayout` (or the
ratio and resulting zoom) and commit installs that. Enforceable by type (commit takes no ratio
argument).

**WM-10. The default workspace name rule has two halves in two crates, and the client uses only
one.** `shepr_core::workspace_label::default_workspace_name` (file name says "label", function
says "name") gives the directory name or the whole path; `shepr_mux::terminal::Label::for_directory`
adds the trim-and-fall-back-to-path step. Both docs claim to be "the name a workspace gets when
it is given none". The client's new-workspace prompt (`shell/overlays/mod.rs`) prefills with
the core half only, so for a directory named only with spaces the prompt shows the blank name
while the server names the workspace `/srv/`. Already diverged for that input. Fix: one
function returning a `Label`-shaped value, in core (the client cannot see `shepr_mux::Label`).

**WM-11. The "too narrow for a scrollbar gutter" width is a bare `4`, twice, coupled to an
unnamed minimum.** `shepr_core::chrome::content_rect` (`pane_inner.width <= 4`) and
`ui/panes.rs::render_pane_border_titles` (`info.rect.width <= 4`) both hard-code it. With
`PANE_MIN_COLS = 4` in core limits, the gutter cut-off is evidently "keep at least the pane
minimum", but nothing links them. The `numeric-consts-live-in-limits` textlint documents that it
does not catch comparison literals, so this is a known gap. Fix: a named limit
(`MIN_COLS_FOR_SCROLLBAR_GUTTER = PANE_MIN_COLS`).

**WM-12. `inner_rect` means two different rects.** In `shepr_core::chrome`, `inner_rect` is the
rect inside the borders, before the scrollbar gutter. In `ui::PaneSurface`, the wire
`PaneSurfacePane` and every client consumer, `inner_rect` is the content rect, after the gutter
(`inner_rect: ratatui_rect(content.content)`). `pane_resize::laid_out_pane_sizes` sizes PTYs
from `pane.inner_rect` and is right only because it reads the surface meaning. A reader moving
code between the two layers will get the gutter wrong by one column. Rename the surface/wire
field `content_rect`. Enforceable by the type change (rename) only.

**WM-13. `PanePublicNumber` has two text spellings.** Its `Display` is decimal (`10`), while
`PublicPaneId` spells the same number in bijective base 32 (`w1:pA`). Anything that logs a
`TreeRejection::RepeatedNumber(n)` or `NumberNotBelowNext(n)` with `{}` prints a number the user
has never seen. Make `PanePublicNumber`'s `Display` the base-32 form (or remove `Display`).

**WM-14. Two copies of the same title-change struct.** `shepr_mux::terminal::state::TerminalTitleChange`
and `shepr_server::app::terminal_titles::TerminalTitleChanges` have the same two fields; the
server folds one into the other field by field. Keep one (the mux one, with an `|=`).

**WM-15. Headless rect computed two ways.** `AppSettings::headless_rect` rebuilds
`Rect::new(0, 0, cols, rows)` by hand; `GridSize::rect()` and `SpawnGeometry::for_grid` already
do it. Likewise `AppSettings::pane_geometry_in` and `AppState::chrome_in` are the same function
under two names. Delete the copies.

**WM-16. Documentation restates numbers the code owns.** `docs/config.md` `[server]`: "each at
most 4096, and together at most 4194304 cells" (`MAX_TERMINAL_GRID_DIMENSION`,
`MAX_TERMINAL_GRID_CELLS`); `default-server.toml` `[advanced]`: "keep at least 1000 lines"
(`MIN_HISTORY_LINES`). True today, checked by nothing. Per the documentation rule, reword
("within the shared grid limits") or add a test that greps the doc for the constant's value.

**WM-17. The error text for a bad pane id spells the id grammar by hand.** `App::json_pane_with_id`
says `expected w<workspace>:p<pane>`; the grammar is owned by `shepr-protocol/src/ids.rs`. Have
`PublicIdParseError` carry the expected form.

## 3. Values nobody can find, change, or trust

**WM-18. Geometry sizing allocates global pane ids.** `WorkspaceChrome::sole_pane_size` calls
`TileLayout::new()`, which calls `PaneId::alloc()` on the process-wide counter, just to measure
a one-pane layout. Every workspace creation, every restore fallback (`restored_pane_size`) and
the `prepare_split` fallback burn an id. A pure sizing function reaching an ambient allocator is
the item-8 smell, and it makes id sequences in tests depend on how many sizes were computed.
Compute the one-pane content directly (`PaneChrome { rect: area, borders: ALL, .. }`), or use
`TileLayout::from_live_pane` with a placeholder id.

**WM-19. `DEFAULT_PANE_RESIZE_AMOUNT` is named as a default with nothing to override it.** It is
the only keyboard resize step; "default" suggests a setting that does not exist. Rename
`PANE_RESIZE_STEP`. The lost-refresh check cadence borrows `GIT_REMOTE_STATUS_REFRESH_INTERVAL`
(`refresh_deadline_after` used for both the next refresh and `lost_refresh_check_at`); two
meanings, one constant.

**WM-20. Tests restate tunables as literals.** `copy_search_bounds_returned_matches_but_keeps_exact_total`
asserts `1024` (`MAX_RETURNED_MATCHES`); `pane_resize_changes_target_ratio_without_changing_focus_or_navigating`
asserts `0.55` (`EVEN_SPLIT + DEFAULT_PANE_RESIZE_AMOUNT`); several core layout tests assert
`0.45`/`0.55`. Changing the tunable breaks tests that do not name it. Reference the constants.

## 4. One channel, one implementation

**WM-21. Pane lifecycle is not logged; workspace lifecycle logs the wrong identifier.**
`logging.rs` has structured `workspace.create/focus/close/rename` events, but a pane split, a
pane close, a pane exit removing a pane, a zoom, a swap and a workspace move log nothing.
`workspace_created` logs the root pane as the process-local raw `PaneId` (`pane_id = %root_pane_id`),
which the operator cannot match to anything (`shepr detect explain` takes `w1:p1`), and logs
neither the cwd nor the name; `workspace_renamed` does not log the new name.
`commit_workspace_creation` and `commit_pane_split` log refusals ad hoc with `tracing::error!`/
`warn!` outside the module that owns these events. Fix: pane events in `logging.rs`, keyed by
`PublicPaneId`.

**WM-22. `UsableCwd::new` traces a permission error at `trace`.** Its doc says the stat error
"is traced with its error, so an unreadable directory is not mistaken for a missing one", but
`trace` is off in every default filter, so in practice it is mistaken. Use `debug`/`warn`, or
drop the claim.

**WM-23. `create_default_workspace` logs a failure without the cwd it tried.** "failed to create
default workspace" with only the io error; the cwd decides most failures.

**WM-24. Operator guidance in `PaneStartFailure::guidance` is assembled at the site.** "Restart
this session" names no command; the code that owns how shepr talks about restarting a server is
`shepr-launch`'s guidance module. Minor, but the text is shown in pane placeholders and in
`detect` errors.

## 5. Errors

**WM-25. `WorkspaceSet::insert` refuses without saying why.** It returns `Err(Box<Workspace>)`
for both a repeated workspace id and a shared pane id; `commit_workspace_creation` logs
"refused to add a new workspace" and `create_workspace_outcome` turns it into "the new workspace
was refused by the session". Return a reason enum like `RemoveRefusal`.

**WM-26. `WorkspaceSet::remove_pane` swallows `RemoveRefusal` with `.ok()?`**, so an internal
disagreement between the records and the layout reads to callers as "no workspace holds the
pane". The `NotHere`-after-`contains` path is the only one, and it signals a broken invariant
that should be logged.

**WM-27. Silent fallbacks that would hide a broken invariant.** `TileLayout::resize_pane` reads
`get_ratio_at(...).unwrap_or(SplitRatio::EVEN)` although the `SplitBorder` it just found already
carries `ratio` (a bad path would silently jump to 0.5); `prepare_split` falls back to
`sole_pane_spawn_geometry` when the new pane is not visible in its own planned layout;
`sole_pane_size` falls back to `GridSize::clamped` (not `clamped_pane`, so a different minimum).
All three are unreachable by construction; if reached they produce a wrong size silently.
Prefer `split.ratio` and an internal error.

**WM-28. Shell projection revision saturates silently.** `mark_shell_projection_dirty` stops
advancing at `u64::MAX` "so bookkeeping never panics", after which no client ever sees a change.
Unreachable in practice; noted because the chosen failure is silent staleness.

## 6. Tests that prove nothing

**WM-29. Dead `SHELL` setup in split tests.** `pane_split_request_focuses_the_new_pane_and_navigates_the_requester_only`,
`pane_split_request_splits_in_half_and_keeps_default_input_routing` and
`a_split_sizes_against_the_recorded_geometry_and_only_then_the_requesters` (`api/panes/tests.rs`)
do `env.set("SHELL", exiting_test_command())`. The fixture config sets an explicit
`default_shell` (`FIXTURE_SHELL`), so validation never takes the shell from `SHELL`, and
`test_app()` then replaces the launcher's shell with `set_test_shell`. The setup does nothing.

**WM-30. Test fixtures depend on the host's `/bin/sh`.** `shepr-test-fixtures/src/config.rs`
sets `FIXTURE_SHELL = "/bin/sh"` as the default shell of every validated test server config;
validation checks it exists and is a recognized shell. Every `App::new` in these tests therefore
depends on the host shell, and tests that build an `App` without `set_test_shell` and spawn a
pane run it. This is the dependency `no-borrowed-process-stand-ins` exists to stop; the textlint
does not catch a `const` string. Use `shepr_test_support::fixture::resolved_shell`.

**WM-31. Git refresh tests whose names promise more than they check.**
`moved_cwd_without_osc7_rediscovers_the_label_identity` moves no cwd and asserts only that a
refresh went in flight; "label identity" is a leftover of when Git named workspaces.
`refreshed_status_is_applied_to_its_workspace` asserts `name() == "labelled"`, which comes from
`Workspace::test_at(None, cwd)` at construction, not from the refresh (only the `branch_state`
assertion tests the refresh). `cwd_identity_refresh_runs_once` never checks "once".

**WM-32. A state test that tests the runtime registry.** `runtime_lookup_is_by_pane`
(`state.rs`) builds an `AppState` with a workspace and never consults it; it exercises
`PaneRuntimeRegistry::get`/`insert` only.

**WM-33. An expectation computed by the function under test.**
`workspace_rename_trims_defaults_and_renders_what_it_changed` (`api.rs`) computes the expected
blank-rename name with `default_workspace_name`, the same function production calls through
`Label::for_directory`. The test cannot catch a change in the naming rule. Assert a literal.

**WM-34. Irrelevant bookmark seeding.** Many server tests seed `seed_bookmark_index(Some(n))` and
name workspaces "active"/"background" (`visible_blocker_overrides_hook_working`,
`pane_process_exit_publish_marks_agent_idle_before_pane_removal`, `terminal_titles` tests,
`git_refresh`'s fixtures) although no code they exercise reads the bookmark. Leftovers of the
"active workspace" model; harmless but misleading about what the code depends on.

## 7. Guards and claims that have stopped holding

**WM-35. Stale comment on the pane-history removal.** `App::decide_pane_exit` (`app/events.rs`):
"a prepared exit decides once and keeps it, since history capture leaves an unreadable
terminal's cached history as it was". There is no history capture any more. False today.

**WM-36. The checkpoint's core-intact gate is a vestige of saved history (lateral, owned by the
pane lifecycle hunter).** `PaneEnding::needs_checkpoint` requires `core_intact` because "a broken
core has nothing new to give the checkpoint", and `decide_pane_exit` reads
`terminal_core_broken()` to decide. The save no longer reads the terminal core at all
(`persist/capture.rs` reads layout, labels, cwd probes and agent ownership). `needs_checkpoint()`
is also passed to `set_pane_process_exit_at`, so a pane whose reader panicked and whose shell is
then signalled gets no exit checkpoint and its agent identity handling differs from an intact
pane's. If the identity checkpoint is wanted regardless of the core, the gate is now a defect,
not just dead weight.

**WM-37. A stale comment in `App::open`.** "Restored workspaces get their Git identity (label and
status) from the first background Git refresh" - a refresh never sets the name
(`an_unnamed_workspace_is_named_after_its_cwd_and_keeps_that_name`).

**WM-38. `EnvVar::SheprPaneId` is registered as interpreted, but no shepr process reads it.**
`EnvVar` is documented as "every environment variable a shepr process interprets" with a
declared `EnvKind::Text`; `SHEPR_PANE_ID` is only written (`pane/launch.rs`) and read by hook
assets. Its kind is a claim nothing exercises. It belongs in `ChildEnv`. Checkable: a test that
every `EnvVar` variant has a production `env::read*` call site would fail for it.

**WM-39. `RegisteredEnv` "contains each name once" is enforced by convention.** The variants are
public, so `RegisteredEnv::Child(ChildEnv::Shell)` is constructible, and `pane_policy` carries
match arms for that second spelling. Make the variants private behind the `From` impls.

**WM-40. Test-only seams production can reach.** `Workspace::test_from_pane` and
`PaneId::from_raw` are `pub` in production crates by design (their docs say why), and
`TerminalState::plan_agent_resume` and `TerminalState::set_hook_report_at` are `pub` production
methods whose only callers are tests (production uses `with_pending_agent_resume_plan` and
`ownership_mut().set_hook_report_at`). Nothing stops a production call. Enforceable: a textlint
banning `\btest_from_pane\b|\bPaneId::from_raw\b` outside test files and `cfg(test)` regions
(the same shape as the clock rules), and delete the two `TerminalState` forwarders.

**WM-41. `Borders` claims four independent sides; production only ever varies two.** Every pane
always has `TOP` and `LEFT`; `apply_pane_chrome` only removes `RIGHT`/`BOTTOM`. So
`render_pane_border_titles`' `!info.borders.contains(Borders::TOP)` and the `TOP`/`LEFT` arms of
`add_pane_border_cells` are guards that can no longer fire; `Borders::NONE`, `is_empty` and
`BitOr` are used only by tests. Since the border settings went, the type should be "shares its
right edge / shares its bottom edge". Enforceable by the type.

**WM-42. `quote_always` says it is "useful for fixtures"** but `shepr-integration/src/command.rs`
uses it in production. Reword.

## 8. Policy invented per call site

**WM-43. Shell-projection invalidation has three mechanisms and no owner.** Some `AppState`
reducers advance the projection revision themselves (`TerminalCwdReported`,
`update_terminal_state`, `sync_terminal_titles`); most do not (`focus_pane`, `commit_pane_split`,
`commit_workspace_creation`, `rename_*`, `move_workspace`, `remove_pane`, `close_workspace`,
`swap_panes`, `toggle_pane_zoom`, `set_pane_input`). For those, the endpoint path relies on
`ViewMutation`/`*Outcome` -> `EndpointEffects::shell_projection_changed` and
`handle_endpoint_app_command_with_render` marking afterwards; the non-endpoint callers remember
by hand (`apply_pane_removal`, `create_default_workspace`, `handle_git_status_refreshed`,
`handle_pane_launch_settled`'s failure arm) or forget (WM-3). Every caller then diffs the
revision (`observe_projection_change`) as well. The 1 s timer rebuild in `render.rs` masks any
miss, which also means no test notices one. Structural fix: every reducer that changes projected
data advances the revision itself (session-dirty marking already works that way), and
`EndpointEffects` keeps only the surface/topology facts.

**WM-44. Callers inside `app/` bypass the reducers through `pub(super)` fields.**
`handle_workspace_create` sets the name with `workspace.set_name` + `logging::workspace_renamed`
instead of `AppState::rename_workspace`; `pane_launch.rs` writes the terminal cwd and resume
state directly (WM-3); `close_workspace` reimplements `forget_removed_panes`. The struct doc
makes the fields `pub(super)` deliberately; the result is that each mutation's bookkeeping
(dirty marks, logs, authority drain) is re-decided per site. Making the fields private to
`state.rs` would force every mutation through a named reducer. Enforceable by visibility.

**WM-45. Label validation is decided at different layers per command.** Workspace rename takes a
validated `Label` at the endpoint; pane rename passes `normalized_user_label(...)` (a `Label`
turned back into `String`) to `AppState::rename_pane(Option<String>)`, which compares raw
strings and `TerminalState::set_manual_label` re-validates with `Label::new`. The reducer
should take `Option<Label>`. Enforceable by the type.

**WM-46. Ambient allocators reached from layout logic.** Besides WM-18, `PaneId::alloc` is called
inside `TreePlan::build`, `prepare_split`, `prepare_workspace` and even as a deliberate
error-forcing value (`ids.get(&self.focus).copied().unwrap_or_else(PaneId::alloc)` in
`TreePlan::build`, which burns an id to make `from_saved` fail). `RuntimeGeneration::alloc` is a
second global counter. The layout.rs comment argues why an owned allocator is awkward; the
`unwrap_or_else(PaneId::alloc)` trick at least should be an explicit `TreeRejection`.

**WM-47. The checkpoint/no-checkpoint and Git-refresh-on-launch rules ride on every launch.**
`handle_pane_launch_settled` requests a Git identity refresh (`request_git_identity_refresh`,
which also forces repository rediscovery) for every successful launch, including every pane of a
restore, so a restore of N panes queues N rediscovery requests (coalesced by the scheduler, but
each one invalidates the worker cache via `mark_due`). Minor; noted as policy decided at the
site rather than by the scheduler.

## 9. Code that is no longer load-bearing

**WM-48. Identity adapters in `api_helpers.rs`.** `detect_state_from_api` returns its argument;
`presented_agent_status` returns its argument (`AgentStatus` is a re-export alias of
`PresentedAgentState`); `pane_not_found` wraps `ApiError::pane_not_found` unchanged. Dead
indirection from when the API and internal types differed.

**WM-49. Agent vocabulary parked in `shepr-core`.** `agent_state.rs` (`PresentedAgentState`) and
`agent_session.rs` (`AgentSessionRefKind`) are used by nothing in core; `shepr-agent` re-exports
them and `shepr-protocol` imports one directly although it may depend on `shepr-agent`. AGENTS.md
places agent identity in `shepr-agent`. Move them; the `shepr-core-layer` rule would then keep
core free of agent types.

**WM-50. `terminal/state/` module names that no longer describe their contents.**
`sessions.rs` holds only a `cfg(test)` seed; `detection.rs` holds title handling and resume
abandonment; `hooks.rs` is three one-line forwarders to `AgentOwnership` (two used only by
tests, WM-40); `names.rs` holds the workspace name type. `Label` (a workspace name and a pane
label) living under `terminal::state` is the reason `Workspace` imports
`crate::terminal::Label`.

**WM-51. Small leftovers.** `SpawnGeometry::cell_px()` duplicates the public `cell` field;
`spawn_geometry(grid, cell)` is a one-line wrapper of `PaneGeometry::with_cell`;
`Workspace::matches_identity_cwd` compares the Git status cwd, not `identity_cwd`;
`responses::success` is `Ok`; the `api.rs` `EndpointOutcome::view_changed` and several `App`
test adapters (`handle_endpoint_command`, `handle_endpoint_command_in`,
`handle_endpoint_command_with_render`) are three names for one call.

## Lateral observations

- `right_click_passthrough` is per pane, projected, and not saved, so it resets on every server
  restart. Nothing documents either way; consistent with `set_pane_input` not marking the
  session dirty.
- `mark_focused_pane_cells` with gaps off marks the cell one past the pane's right and bottom
  edge even for a pane with no neighbour there (outermost panes); harmless only because the grid
  ignores out-of-area cells.
- `WorkspaceSet` looks workspaces up by linear scan and `projection_input` calls
  `workspace_info(&ws.id())` per workspace, re-scanning; quadratic in workspaces on every
  projection rebuild. Small numbers today.
- `close_workspace` reports removed panes in hash order while `remove_pane` reports layout
  order; callers only shut runtimes down, so no effect yet.
- The global `LayoutEpoch` starts at 0 for every tree, restore included; fine because a client's
  epoch is always from the current boot's projection, but a client that reconnects to a new boot
  with a cached epoch could match a different tree by accident.
