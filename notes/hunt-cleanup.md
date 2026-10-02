# Cleanup from the design hunt

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

Dead code, dead state, test-only twins of production rules, public surface
that exists only for tests, unused dependencies and stale documentation. Each
entry is a deletion or a rewording rather than a redesign. Unverified: the
raw reports are in the commit that precedes this file's.

## Dead code and dead state

## CLN-001 - A Git config parser in `shepr-core` with no production caller

`shepr-core/src/env.rs` carries about 200 lines (`read_git_config_parameters`,
`parse_git_config_parameters`, `parse_git_single_quote`,
`parse_git_config_count`, `indexed_git_config_pair`) with no production caller;
only `is_registered_name` uses the indexed-name recognizer, for test isolation.
Git subprocesses spawned by mux inherit the variables and apply them
themselves. Delete the parser and keep the recognizer, or move it to
`shepr-mux/src/git/config.rs` and wire it if that was the intent. (foundation)

## CLN-002 - `GitSpaceMetadata` is computed, cached and diffed but never read

`Workspace::cached_git_space` and `WorkspaceGitStatus::space` are computed on
every refresh (`git_space_metadata_from_info`: two `canonicalize` calls plus
`embedded_bare_repo_container`, another `locate_git_dir` and possibly a
`git config` spawn), cached, and compared in `apply_workspace_git_statuses`,
where a difference triggers a projection rebuild and a render. Nothing in the
server, client or protocol reads `key`, `checkout_key`, `repo_name` or
`is_linked_worktree`; only `repo_root` is used, for the label. `key` and
`checkout_key` are also lossy `display().to_string()` renderings. Keep only
`repo_root`. (mux-state)

## CLN-003 - `GitStatusRefreshDemand` and the branch-only status path are unreachable

The server always passes `{ branch: true, ahead_behind: true }` (with a comment
saying to keep it full), yet mux implements a branch-only path in
`git_status_snapshot_for_cwd_with_demand` with its own cache-merging rules,
`WorkspaceGitStatus` carries the demand, and `apply_workspace_git_statuses`
checks `demand.branch` and `demand.ahead_behind`. The `retry_after =
Some(now)` "not computed" sentinel comes from that path. Delete the axis.
Reported by mux-state and server-app.

## CLN-004 - A second automatic-label algorithm in the Git status snapshot

`git_status_snapshot_for_cwd_with_demand` computes `auto_label` from the cache
key (so for a repo always the root's own name), and
`WorkspaceGitStatusSnapshot::into_workspace_status` overwrites it from the real
identity cwd. The first is read only as a cached value and in tests. Delete
`auto_label` from the snapshot and the cache entry. (mux-state)

## CLN-005 - `AppState::should_quit` duplicates the lifecycle phase

Only `HeadlessServer::initiate_shutdown` sets it, and that function also sets
the phase to `Stopping` and raises the stop signal. All nine
`stop_requested(self.app.state.should_quit)` calls pass a value that adds
nothing; remove the field and `ShutdownLifecycle::stop_requested`'s `app_quit`
parameter. The lifecycle doc calling it "the in-process input path" is herdr
residue. The stop signal is also called `stop_requested`, `stop_request`,
`stop_signal` and `should_quit` across `headless.rs`, `lifecycle.rs` and
`client_transport.rs`. Reported by server-serving and server-app.

## CLN-006 - Dead checks in the server serving path

- The fallback `ApiError` in `reject_api_request_for_shutdown`: every caller
  runs after `initiate_shutdown`, so `shutdown_error()` is always `Some`.
- `client_transport` bounds `boot_id.len()` against
  `MAX_ENDPOINT_BOOT_ID_BYTES`; a `BootId` only decodes from its canonical
  form, far below 128 bytes. `RequestId` is the one that needs a bound, in its
  decode.
- The read loop's host palette bound `colors.len() > 256` duplicates the
  decoder's bound, which already refuses 257 entries.
- `client_shell_geometry_error`'s empty-surface branch: `GridSize` is
  `NonZeroU16`, so zero fails decode.
- `ProtocolCellSize::from_wire` nulls oversize cells, but the transport's
  refusal runs first, so that branch is dead.
- `drain_internal_events_with_forwarding_up_to -> (bool, bool)`: the first
  element is never read.
- `ClientPasteRejected { size, max }` carries `max`, which is always
  `MAX_INPUT_PAYLOAD`.
- `frame_server_message` is a one-line alias of `encode_message` kept so
  `SurfaceBoundary` has a function pointer to replace.

(server-serving)

## CLN-007 - Dead pieces in the app

- `lookup_runtime` returns a `WorkspaceId` neither caller uses.
- `live_host_theme_reported()` is a private method returning a `pub(crate)`
  field.
- `create_workspace` is a pass-through to `create_workspace_without_save`; both
  mark the session dirty, so "without save" is a stale name.
- `AppPolicy::Test` is an alias of `Suspended` spelled as a const.
- `crossterm` is used in `app/state.rs` only for a test helper `key_matches`
  that exercises `shepr_config::terminal_key_matches_combo` (client key
  config).
- `handle_detect_capture` parses the pane id to `(ws_idx, pane)` and then
  rebuilds the same `PublicPaneId` with `public_pane_id`.

(server-app)

## CLN-008 - Dead pieces in the client core

- `ClientState::frames_frozen` checks in `present_frame` and
  `present_surface_patch` cannot fire at any call site.
- The `ClientMessage::ClientShellResize` arm in `finish_client_shell_input` is
  never reached; the shell does not emit it, so resize is requestable two ways
  of which one is dead.
- `ServerMessage::SurfaceUpdate` and `EndpointWelcome` reach the loop only as
  protocol violations; `DecodedServerMessage::Wire(ServerMessage)` admits every
  wire variant.
- `install_client_shell_snapshot` returns `Result` but never fails.
- `EndpointRegistry::received` exists only for transports without reader
  stamps, which in production means none that are health tracked; `insert`
  and `insert_native` are two insertion paths.
- `host_replies.rs`'s `HostInputFramer` is a `Deref`/`DerefMut` newtype over
  `RawInputFramer<HostReplies>` that buys nothing; its test exercises termio's
  `HostReplies` and belongs there.
- `ClientShellState::endpoint_label(&self, id)` ignores `self` and forwards to
  `display_label()`.

(client-core)

## CLN-009 - Dead pieces in the client shell

- `sidebar::workspace_entries` returns `(0..len).collect()`, the vestige of a
  removed filter, yet six callers index through it while two iterate
  directly.
- `ClientShellWorkspace.custom_label` is not read anywhere in shell production
  code.
- `aggregate_agent_rows(_active_endpoint_id)` has an unused parameter.
- `AgentRowIndex::new`'s sort is dead work: the panel reorders by
  `aggregate_agent_rows` afterwards.
- `online_agent_targets` is misnamed (its comment admits it).
- `hit_test::contains` duplicates `Rect::contains`.

(client-shell)

## CLN-010 - Dead pieces in the edges

- `DifferentBuildServer.version` is populated and stored but read only in
  tests.
- `DaemonExit::code()` is used only in its own test; `shepr-daemon`'s
  `report_server_error` and `config_error` map to the raw constants by hand.
- `MachineProbe::resolve`'s arm `Err(error) if failed_before_remote_result(..)`
  precedes `Err(error) if !is_remote_candidate_mismatch(..)`; the origins are
  disjoint, so the first adds nothing but reads as a distinct rule.
- `status::run_status_command` keeps a `Client` arm `dispatch` can no longer
  reach, and `detect::explain` re-checks `args.file.is_some()` after `cli::run`
  routed it.
- `main.rs`'s `ClientBridge | Cli(_) => Err("launch was already handled")` arm
  is unreachable.
- `cli/target.rs` `CliContext` is left over from the removed `--machine`
  targeting: one constructor (`test_local` is identical), a `Deref` to
  `AppPaths`, one-line wrappers, and `build_checked: Cell<bool>` serving only
  `send_request`.
- `cli/server_not_running.rs` is 47 lines whose production content is one
  response builder plus a `cli_error` identity wrapper.
- `cli/matches.rs` keeps fallible and infallible versions of every helper; the
  infallible ones serve only the root help and version flags.
- `ServerStatusJson.compatible` and `.restart_needed`, and
  `FullStatusJson.update.restart_needed`, are computed in `src/cli/status.rs`
  (`build_compatible_bool` and `restart_needed_bool` each call `is_this_build`
  on the same field) but the only machine consumer recomputes compatibility
  from `build_id` and ignores them.
- `forward_remote_bridge_stdio(stream, idle_timeout: bool)`: the only
  production caller passes `true`.
- `local_server::ensure_running(paths, timeout, ..)`: every caller passes the
  same `SERVER_READY_TIMEOUT`, which is re-exported and re-aliased on the way
  (`shepr-remote` limits, `local_server`, `src/limits.rs`).

Reported by edges; contracts also notes the doubled compatibility derivation.

## CLN-011 - Dead pieces in the contracts crates

- `CodecError::Invalid(&'static str)` has no producer.
- `theme_config::resolve_palette`'s "theme has no built-in palette" branch is
  unreachable.
- In `surface_reuse::Decoder::decode`, the `Some(SurfaceMeta::Projection(_))`
  arm inside the compact branch is unreachable (the enclosing `if` excludes
  it) and, unlike every other error, returns `MetadataMismatch` without a
  subject.
- `RuntimeStatus::version: Option<String>` is always `Some`.
- `Pong::version` is `"<pkg>+<BUILD_ID>"`, so a pong sends the build id twice.
- `PaneSurfacePatch` derives `Serialize`/`Deserialize` though it never crosses
  the wire.
- `command.rs` enums (`SplitDirection`, `PaneRightClickTarget`,
  `PaneDirection`, `PaneCopySearchDirection`) carry
  `#[serde(rename_all = "snake_case")]`, which the positional codec ignores;
  they read as JSON types.
- `format_key_combo` falls back to `format!("{code:?}").to_lowercase()` for key
  codes `parse_key_combo` cannot read back, so "a printed combo reads back as
  the same binding" holds only for the parsed set.
- `limits.rs`'s `INDEXED_BINDING_RANGE_SYNTAX = "1..9"` is derivable from the
  first and last indexed keys, kept in step by hand.

(contracts)

## CLN-012 - Dead pieces in shepr-agent

- `fallback_explain`'s `None` arms cannot run; both callers pass
  `Some(agent)`. The fallback state is also computed both by `fallback_state`
  and inside `fallback_explain`.
- `RegionSpec::extract` has a second-stage match whose arm returning `""` is
  unreachable.
- `is_cased` and `is_case_ignorable` in `manifest.rs` are the same function.
- `every_line_regex_matches` keeps a fallback for more than
  `MAX_MATCHERS_PER_GATE` regexes, which validation forbids.
- `loaded_manifest` `take()`s `AgentManifest.compiled` with a
  `None => compile_manifest(..)` arm production never reaches (the loader is
  filed among the structure findings).
- `ForegroundProcess.cmdline` is always `argv.join(" ")` when argv exists, so
  `cmdline_argv0_agent_name` and the Letta fallback re-split a string joined
  from the vector they could use; the fallback can never fire with a value.
- `IntegrationStatus.installed_version` reads a `SHEPR_INTEGRATION_VERSION=`
  marker that is diagnostic only, bumped by hand in 16 assets and compared by
  nothing. Drop it or derive it from the asset hash.
- The Claude and Antigravity hooks send `agent_session_path`; both agents have
  `SessionRefPolicy::Id`, so `session_ref_for_agent_report` drops it. Dead
  wire data that suggests those agents resume by path.
- `agent_label` and `parse_canonical_agent_label` in `detect` are one-line
  re-exports of `Agent` methods: two names per operation.
- `ManifestRule.state: Option<ManifestState>` where `None` and
  `ManifestState::Unknown` both mean Unknown and no bundled rule uses `None`.

(agents)

## CLN-013 - Dead pieces in the pane runtime

- `PaneLaunchEnv::extra: Vec<(String, String)>` is `Vec::new()` at every
  production construction (`workspace.rs` twice, `persist/restore.rs`,
  `shepr-server/src/app/ids.rs`); the "explicit launch env opts back into
  scrubbed variables" machinery and its test exist for an input nothing
  provides. Remove it, or key it by the env vocabulary if it returns.
- `From<PaneClearError> for String` appears unused; its one would-be caller
  (`copy.rs::handle_pane_clear`) writes its own message, duplicating the
  `Display` text for `AlternateScreenActive`.
- `PaneTerminalCore::initial_default_foreground` and `_background` are
  `Option<RgbColor>` but always `Some`, so `terminal_default_fg`/`_bg` carry a
  dead `None` branch.
- `TerminalDirtyPatchSnapshot`'s `patch` can only be `Clean` or `Patch`, but the
  type allows `Fallback`, and `retained_surface.rs` has a dead arm for it;
  `scroll_metrics: Option<ScrollMetrics>` is always `Some`.
- `PaneRuntime::detection_text` is used only by a server test;
  `primary_history_ansi` duplicates the cached history path for
  `agent_resume.rs`.
- `PaneRuntime::cursor_state(area, show_cursor: bool)` returns `None` when the
  bool is false; the caller can skip the call.
- `DetectorState` threads `Some(input.content_seq)` into three functions that
  accept `Option<u64>` but are never given `None`.
- `clear_osc_evidence_for_agent_transition` re-checks `previous_agent.is_some()`
  after `observe_process_probe` computed `should_clear_osc_evidence` from it.

(mux-panes)

## CLN-014 - Dead pieces in mux state and core layout

- `upstream_full_ref(&BranchConfig) -> Option<String>` always returns `Some`.
- `split_pane` returns `None` when the target is not in the workspace, then
  `split_pane_shell` returns `Err(NotFound)` for "target not in the layout", a
  condition the first check ruled out unless layout and records disagree.
- `cached_auto_label = String::new()` in `assemble` before
  `mark_identity_undiscovered` overwrites it.
- `shepr_core::layout`: `split_focused` is documented as "used by tests" yet
  `pub`; `TileLayout::new` returns `(Self, PaneId)` although the id is
  `focused()`; `InvalidSavedLayout::InvalidSplitRatio` is never produced by
  `from_saved` (a `Node` already holds a valid `SplitRatio`) but by mux
  restore.
- `PaneId::from_raw` has no production caller, and the `Serialize` and
  `Deserialize` derives on `PaneId` have no consumer found (the wire uses
  `PublicPaneId`, snapshots use `u32`), yet the doc lists deserialization as a
  way to mint one.
- `ProcessIdentity::tag(token)` is always called with `0`, and the sweep accepts
  only `Some((owner, 0))` from `parse_tag`: the token field is dead format.
  `ssh_paths.rs` `bridge_endpoint_path_with_token(.., 0)` uses token `0` to
  measure the name length.
- `fair_share` returns `usize::MAX` for "no pane needs trimming".
- Panic payload to message is written three times: pty `actor.rs`
  `panic_payload_message`, `shepr-client/src/fatal_panic.rs` and
  shepr-test-support (code duplication only).

Reported by mux-state and foundation.

## CLN-015 - Dead pieces in the terminal crates

- `PANE_TRUECOLOR_BITS_PER_CHANNEL: Option<&[u8]>` makes `PANE_COLORTERM` a
  `match` yielding `""` for "no truecolor": a compile-time switch with one
  value ever used. Make it, and the XTGETTCAP `Tc`/`RGB` answers, plain
  constants.
- `KeybindMatch` has a single variant `Action(KeybindAction)`.
- `input::raw_input::HostReplyPolicy` is a 13-method trait with default no-ops
  and two impls, one empty (`NoHostReplies`). A concrete `HostReplies` with an
  inactive state would do.
- `InputLeaseTable::normalize_press` returns its input unchanged; it only drops
  an old lease.
- `selection_render::selection_palette_background` and `panel_contrast_fg` have
  identical bodies (client-shell also lists four panel-contrast copies among
  its duplicated answers).
- `ModifyOtherKeysLevel::from_parameter` is used only by tests.
- `Terminal::drain_events` drops empty clipboard stores and any
  `ClipboardType::Selection` store with no counter, unlike the oversized case;
  probably fine, but it is the only effect with no trace.

(terminal)

## Test-only twins and test seams in production

## CLN-016 - Production rules with a test-only twin that the tests exercise instead

- `PaneTerminal::plain_page_keys_use_host_scrollback` (production) and
  `InputState::plain_page_keys_use_host_scrollback` (`#[cfg(test)]`); the test
  `plain_page_keys_host_scroll_for_shell_like_decckm_with_bracketed_paste`
  calls the twin.
- `terminal_buffer_symbol_into` (render path) and
  `terminal_normalize_buffer_symbol` (`#[cfg(test)]`); the grapheme-width tests
  in `terminal/tests.rs` call the twin. Suggested: a pure
  `normalized_symbol(&str, CellWide) -> &str` both call.
- `PaneTerminal::input_state()` and the `InputState`/`ScrollPosition` types
  exist only for tests, and `input_state` derives mouse mode and encoding with a
  third precedence ladder.
- `RenderSignal::request_pty` (test-only) restates `request_pty_coalesced`
  minus the flag; the coalescing tests test a copy.
- `save_session_before_teardown` (cfg(test)) duplicates the production
  `save_session_before_teardown_async` synchronously, and `save_session_now` is
  another test-only save path.
- `handle_internal_event_after_checkpoint` (cfg(test)) reimplements the
  server's hold-and-replay of checkpointed exits with a fixed `for _ in 0..4`
  loop; session tests run against it rather than the loop's.

There is no agreement test for any of these: the copies can drift with every
test green. Reported by mux-panes, mux-state and server-app.

## CLN-017 - Production types shaped by test fixtures

- `ClientOutbox.attached: bool` is false only for `detached()` fixtures, yet
  every presenting predicate in production checks it.
- `client_read_loop_with_endpoint_controls(.., Option<&ControlSender>)` is
  always `Some` in production; `HeadlessServer::new(.., api_server:
  Option<ServerHandle>, ..)` is `None` only in tests.
- `SurfaceBoundary` holds function pointers for encode and render so a test can
  inject an oversize encode failure.
- `ClientRegistry::get/get_mut/contains_key/Index` are generic over
  `K: Copy + Into<ClientId>` so tests can write `clients.get(&1)` through
  cfg(test) `From<u64>`/`From<i32>`; `ActivityStamp` likewise.
- `ChildLiveness`: `pid` is atomic only for `set_pid_for_test`; pid `0` and
  `leader: None` exist for the `PaneRuntime::with_child_io` fixture seam, so
  production tests `pid != 0` and carries `None` fallbacks.
- `ClientShellEndpoint::snapshot_generation: Option<u64>` is `None` only in
  tests, and `endpoint_snapshot_matches` accepts it with `is_none_or`;
  `type SurfaceGeneration = Option<u64>` and the cfg(test)
  `receive_pane_surface` shims follow from it.
- `PtyIoActorRunner` carries test seams in production fields
  (`poll_pty_and_wake: fn(..)`, `drain_wake_fd: fn(..)`, `resize_pty: Box<dyn
  FnMut>`, `poll_observer`); a small trait would keep them out.
- `TerminalState::set_hook_authority_at` is a public production method
  documented as a fixture seam taking the source as a string.
- `AgentResumePlan::argv` is public and `shepr-server/src/test_support.rs`
  overwrites it.
- `SshControlDir::unchecked` exists only so tests can make the control and
  config directories differ.

Reported by server-serving, mux-panes, client-core, client-shell, foundation,
agents and edges.

## CLN-018 - Public surface that exists for another crate's test

- `shepr_remote::BridgeUpload` and `BridgeUploadEnd` (with `pub` fields) are
  exported only so `shepr-client/src/transport.rs`'s test
  `upload_cancellation_preserves_pending_endpoint_download` can drive them.
  Move the test into `shepr-remote` and make the types crate-private.
- `shepr-server` dev-depends on `shepr-client` for one test
  (`server/netside_tests.rs`), which is why `pub mod endpoint`, `pub use
  view::{..}`, `pub use shell::{ClientShellConfig, ClientShellState}`,
  `EndpointRegistry::new_at`, `insert`, `viewed` and the choice methods are
  `pub`; the binary uses only `run_client`, `ClientExit` and
  `ClientRunError`. A dedicated integration-test member would let the client's
  surface shrink.
- `codec::Decoder::finish`, `from_slice` and `Decoder` are public but used only
  by tests; `validated_sidebar_bounds` is public but used only in config;
  `encode_public_number`/`decode_public_number` are public only for mux tests.

Reported by edges, client-core and contracts.

## Unused dependencies and edges

## CLN-019 - Dependencies with no use in the sources

`shepr-api`'s `Cargo.toml` lists `shepr-agent`, `shepr-core` and `shepr-vt`;
none is used in its sources. `shepr-protocol` lists `tracing`, unused. These
edges inflate the layering picture `brokkr.toml` checks. (contracts)

## Stale documentation

## CLN-020 - The content revision doc contradicts the server

`PaneTerminalCore::content_revision`'s doc says the parity scheme is no longer
used, but mux still advances by two (`wrapping_add(2)`) and
`shepr-server/src/server/client_shell.rs` marks a torn read with `after | 1` and
tests `after.is_multiple_of(2)`; the retained path sends the snapshot revision
without that rule. One side is stale; mux-panes reads the doc as the stale one.
The revision type is filed among the consolidations. Reported by mux-panes,
terminal and server-serving.

## CLN-021 - Stale comments and docs across crates

- `shepr-core/src/shell.rs` says the lookup is "shared between config
  validation and PTY launch"; PTY launch does not use it.
- `shepr-core/src/agent_session.rs` says `AgentSessionRefKind` lives there
  because protocol and API name it; neither does any more.
- `note_default_color_change` says "`shell_pid` 0 (no child yet) is handled
  there", but `resolve_default_color_owner` takes an `Option` from
  `live_pid()`.
- The doc on `AgentDetection.visible_working` says the flag is not forwarded in
  `StateChanged`; `DetectionPublishDecision::Publish` carries it.
- `ChildExitReason` hosts the checkpoint policy in platform while AGENTS.md
  places checkpoint policy in shepr-server.
- The `shepr-api` schema says an invalid official reference "fails validation
  before dispatch"; it fails inside the app handler.
- `crates/shepr-config/src/io.rs` `AppPaths::resolve_for_server` and
  `crates/shepr-protocol/src/command.rs` `WorkspaceCreateSource::Follow` speak
  of `new_terminal_cwd`; the key is `terminal.new_cwd`.
- `crates/shepr-config/src/default-client.toml`'s `[[machines]]` comment
  hard-codes path byte counts, a drifting specific.
- `crates/shepr-protocol/src/ids.rs`'s `TerminalId` doc speaks of "the
  pane-backed transition", which reads as a finished migration.
- AGENTS.md's crate list describes `shepr-termio` as "terminal input and copy
  mode", but `copy_mode.rs` is four helpers; copy mode lives in the client and
  mux.
- `shepr-remote`'s `remote/` directory name reflects an older module tree.

Reported by foundation, agents, mux-panes, server-app, contracts, terminal and
edges.

## CLN-022 - The saved schema carries compatibility optionality nothing needs

No on-disk state needs to stay compatible, yet `WorkspaceSnapshot::id:
Option<String>` with `#[serde(default)]`, `custom_name` default,
`next_public_pane_number` defaulting to `0`, `focused` and `root_pane` as
optional `u32`s, `PaneSnapshot::public_number` optional and tolerated at zero,
`SessionSnapshot::host_theme` default, `SavedHostTheme::palette` default and
`history_digest` default all have restore fallbacks (focus to first leaf, root
to first leaf, fresh IDs, fresh numbers). Each absence could be a parse error
handled by the existing drop-and-back-up path. The agent-session tolerance in
`deserialize_agent_session` is different (a newer build can lose an agent
kind) and should stay. The layout file is also written as `SavedSession`
(`#[serde(flatten)]` over the snapshot) and read twice, as `SessionSnapshot` and
as `SavedHistoryReference`; one `SessionFile { snapshot, history_digest }` would
serve both directions. The saved `identity_cwd` is derivable from `root_pane`
and `panes` except when the root pane is gone, which restore treats as damage
anyway. (mux-state)
