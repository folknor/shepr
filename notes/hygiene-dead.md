# Hygiene: dead code

Modules, functions, flags, fields and files nothing uses any more: switches with
one value, compatibility paths for versions nobody runs, leftovers of removed
features (pane history and the recently removed settings among them). Each entry
says what tells the hunter it is dead. Filed from the nine-scope hunt; each entry
names the hunts that reported it.

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

---

## DEAD-001 - History-era files are orphaned on every host that ran with `pane_history`

Reported by: persistence.

Before the pane-history removal, recovery copies came in pairs
(`session-<ts>-<seq>.json` and `session-history-<ts>-<seq>.json`) plus a live
`session-history.json`. History copies could be up to 256 MiB each, across up to 48
snapshots and 3 backups. `RecoveryKey::parse` now rejects `session-history-...` names,
so pruning never sees them, and nothing removes `session-history.json`. Any host that
ran with `pane_history` keeps potentially gigabytes of screen contents in its data
directory indefinitely. The owner never enabled the setting, and the rule is no
migration code, so this is an operator action, not a code change: remove
`session-history.json`, `session-snapshots/session-history-*` and
`session-backups/session-history-*` by hand where present. Filed because nothing else
will ever do it.

## DEAD-002 - `sha2` in shepr-mux exists only for an in-memory equality

Reported by: persistence.

The layout fingerprint is SHA-256 over a hand-built encoding, compared only in process
and never stored. Its original reason (keeping digests apart from history pairing) went
with pane history. Comparing the encodings, or deriving `PartialEq` on a
`Shape<PanePublicNumber>` projection, does the same; then `sha2` leaves
`shepr-mux/Cargo.toml` and the `shepr-mux-layer` allow list in `brokkr.toml` (whose
dependency rule then catches a reintroduction). The `Option` plumbing through
`layout_fingerprint` and `append_fingerprint_count` (`u64::try_from(usize)`, which
cannot fail on 64-bit Linux) and the `SavedLayout::Unknown` result of a failed
fingerprint go with it.

## DEAD-003 - The machine-readable restore failure taxonomy has no reader

Reported by: persistence. See BUG-003.

`SessionRestoreFailure::{Unreadable.kind, NotRegularFile.kind, Unparseable.{line,
column, category}}`, `SessionIoErrorKind` (21 variants, including `ConnectionRefused`,
`AddrInUse`, `NotConnected` and others that cannot come from reading a file),
`SessionFileKind`, `SessionParseCategory`, and `files::session_io_error_kind`,
`session_file_kind` and `session_parse_failure`'s mapping exist so "the variant and parse
coordinates keep the outcome machine-readable". Nothing reads them: the client renders
only `Display`, which uses `detail`, and `Unreadable` and `NotRegularFile` display
identically; the one consumer is an assertion in `open.rs`'s tests. Collapse to
`{ detail, path }` or a two-variant enum. Also, `load`'s `NotRegularFile` arm is
reachable only if the path changes type between `check_session_target` (which refuses a
non-regular path at startup) and `load`, a window of microseconds, yet it costs a wire
variant and a mapping table.

## DEAD-004 - Recovery copy sequence numbers and a defensive publish branch are almost never used

Reported by: persistence, restore-resume.

`preserve_existing_in` forces the new timestamp past the newest parsed regular-file key
(`max(now, previous + 1)`), so sequence `0` is always free unless something that is not
a regular file sits at the exact future name. `RECOVERY_SEQUENCE_LIMIT` (128 attempts),
`RECOVERY_SEQUENCE_DIGITS`, the compile-time width assertion and the `AlreadyExists`
backstop loop exist for that case; dropping the sequence (or the loop) changes only
recovery copy names. `copy_recovery`'s `NotDurable` branch is dead by its own comment (a
create-only publish cannot return it); the parent-directory sync after it is not dead (it
covers a freshly created recovery directory) but runs on every copy, where
`create_private_directory_all` could report whether it created anything, as
`missing_directory_chain` in `files.rs` does the other way.

## DEAD-005 - Small persistence leftovers

Reported by: persistence.

- `SessionWriter::retire(self) { drop(self) }` and `DataDirLease::release(self)` are
  names for `drop`.
- `actor::abandoned()` and `actor::lease_only()` are one-line wrappers around enum
  constructors (the second goes with POL-001).

## DEAD-006 - Restore re-validates an already validated session through an alias

Reported by: restore-resume.

`schema.rs` `pub type PaneAgentSessionSnapshot = shepr_agent::resume::PersistedAgentSession;`
is the remnant of a once-separate snapshot type. `restore.rs`
`persisted_agent_session_from_snapshot` rebuilds a `PersistedAgentSession` from the
fields of a `PersistedAgentSession` through the validating constructor, which cannot
fail for a decoded value, and `restored_terminal_agent_session` /
`restore_plan_for_snapshot` wrap it in `Option` plumbing that is never `None` for a
present session. Fold to `.cloned()`.

## DEAD-007 - The pane-gone resume abandonment path cannot run

Reported by: restore-resume.

`agent_resume.rs` `start_pending_agent_resume`'s `let Some(public_id) = ... else` branch,
`App::abandon_resume` and `ResumeUnavailableReason::PaneGone` ("the pane no longer
exists"). Candidates are collected from the same state moments before in the same pass,
with no mutation between, so the branch cannot run; if it did, the reason would be
recorded through `update_terminal_state`, which does nothing for a missing pane. Delete
all three.

## DEAD-008 - Small restore and resume leftovers

Reported by: restore-resume.

- `AGENT_RESUME_DETECTION_HOLD`, a private alias whose only reader is
  `AGENT_ABSENCE_STARTUP_HOLD`.
- `agent_resume.rs` `derived_pending_agent_resume_pane_infos`, a one-line function with
  one caller; `resume_candidate` returns a `&TerminalState` both callers discard.
- `restore.rs` `AgentRestoreState` / `PaneRestoreStartup` / `RestorePlanContext` thread
  one bool (`resume_agents_on_restore`) through four layers; an
  `Option<&mut HashSet<..>>` (none when disabled) says the same.
- `terminal/state/sessions.rs` holds only a test seam (`seed_hook_authority_for_test`);
  the name suggests session logic.
- `ResumeOutcome::replaced_runtimes`: a candidate has no runtime by definition, so the
  launch installs one rather than replacing it.
- `restore_error` is the field for every start failure, fresh launches included
  (`PaneStartFailure`'s own doc says so); the name misleads.
- `RestoreLoss::Panes` and `panes_pruned` have no honest producer (BUG-006).

## DEAD-009 - Save and shutdown values and guards nothing reads

Reported by: save-shutdown.

- `finish_final_session_save`'s returned bool is read by neither caller, and its
  `autosave.clear()` on success duplicates `retire_session_writer`'s unconditional clear
  a few lines later; on failure, `record_failure` arms a deadline nothing will service.
- `cancel_host_shutdown()` and `restart_host_shutdown_warning()` return
  `Option<HostShutdownFreeze>`, but both callers use only `.is_some()`; the struct's doc
  ("held ... until shutdown completes") describes a token, not the generation record it
  is.
- Unreachable phase guards: `RunServerError::Shutdown(UnexpectedPhase)` and
  `ShutdownStep::CompleteShutdown` exist for `complete_shutdown`'s `require_phase`, which
  the loop calls only after checking `phase() == Stopping`; `freeze_for_host_shutdown`'s
  `require_phase` and `finish_host_shutdown_freeze`'s second one are likewise
  unreachable. With the phase fold (CLAIM-023) and a `Stopping` token (DIAG-009),
  transitions can take the phase by value and these go.
- `Autosave::is_due` is used only by `next_save` and its tests; `SessionSaver::is_due`
  is never true for a due checkpoint with no retry, which works only because
  `service_session_saves` also starts on `save_reaped` (CLAIM-023).

## DEAD-010 - Small pane lifecycle leftovers

Reported by: pane-lifecycle.

- `shepr_platform::session_member_handles` is a one-line exported alias of
  `session_members` with one caller (a mux test).
- `PaneLaunchEnv::pane_id: Option`: production always calls `with_pane_id`; the `None`
  branch ("stays unset rather than inheriting") is exercised only by tests. Make the id
  a constructor argument.
- `PaneRuntimeRegistry`: `new()` duplicates `Default`; `From<HashMap<..>>` has only test
  callers; `IntoIterator` has no production caller. Restore builds and returns its own
  `HashMap<PaneId, PaneRuntime>` (`OpenedSession::terminal_runtimes`) instead of the
  registry, so the newtype is bypassed for exactly the runtimes created before the app
  exists.
- `fd::set_cloexec` on the PTY master in `PtyIoActor::spawn_inner`: every production
  master is opened `O_CLOEXEC`; the call only matters for test sockets.
- `PaneTeardownInFlight::drop`'s "completion had no matching start" branch cannot occur
  (the guard is minted only by `start`).
- `ChildLiveness::launched_without_child` carries two doc paragraphs for one
  constructor, the first describing "the public ChildIo constructor" in terms that
  predate `with_child_io`'s doc.
- `ChildBacking` (VAL-024) and `SHEPR_BIN_PATH` (BUG-022).

## DEAD-011 - Agent detection code with no production reader

Reported by: agent-state, workspace-model.

- `visible_working` end to end (BUG-031).
- `manifest::explain(agent, screen)`: used only by a manifest test.
- `Runtime::Tmux`: classified only so `wrapped_agent_from_runtime_argv` can return
  `None` for it, which an unclassified name gets anyway.
- `identify_agent` and `agent_from_basename` are both `parse_agent_label` under another
  name.
- `TitleActivityGlyphs`: a unit struct plus a const instance for one `contains` function
  with one caller.
- `api_helpers.rs`: `detect_state_from_api` and `presented_agent_status` return their
  argument (`AgentStatus` is a re-export alias of `PresentedAgentState`), and
  `pane_not_found` wraps `ApiError::pane_not_found` unchanged: indirection from when the
  API and internal types differed.
- `SnapshotAgent.agent: Option<Agent>` is always `Some` (`agent_info` filters on
  `effective_agent().is_some()`).
- OSC 21337 ("agent status") in `osc_debug`: captured as manifest evidence, but no
  region, manifest or detector reads it; a herdr protocol leftover.
- `set_detected_state_with_visible_blocker`'s `_ignored_screen_idle` parameter, a
  leftover of a removed screen-idle signal.
- `is_unsequenced_opencode_selection` is driven by
  `HookSessionPolicy::unsequenced_selection`, not OpenCode-specific; the name is stale.

## DEAD-012 - Pane-history read helpers survived the feature

Reported by: agent-state.

`pane/terminal/backend.rs` and `pane/runtime.rs`: `recent_text`, `recent_ansi`,
`recent_unwrapped_text`, `visible_ansi` and `PaneRuntime::recent_unwrapped_text`. The
comment calls them "Test-only reads", yet they are `pub(crate)` / `pub` production items
with no production caller. `cfg(test)` them or delete them with their tests;
`check_dead_test_helpers.py` misses them because they are not under a test cfg. The
`shepr-vt` VT formatter (`Format::Vt`, `AnsiCarry`, `read_ansi_screen_carrying` and its
open-ended reads) was also left with no production caller by the removal (reported by
the agent that removed pane history), and `PaneRuntime::launched()` has only a restore
test caller.

## DEAD-013 - Integration code, keys and assets nothing needs

Reported by: integrations.

- The `Devin | Droid` arms of `expected_events`' exception: both descriptors give every
  event an action, so only Copilot takes it (goes with POL-019).
- The update-in-place branch of `ensure_direct_command_hook`: its only caller starts
  from an empty map and Copilot has one event, so the `find(..)` never matches;
  `direct_command_field()` returns the constant `"bash"`.
- `targets::grok_hook_command`, identical to `hook_command` (its doc remembers when it
  differed); `registry::install_operation`, a pass-through to `targets::install`.
- `missing_agent_directory`'s per-agent prose table: reachable in production only if an
  agent directory vanishes between the presence check and the install, and it reads like
  CLI install guidance for a command that no longer exists.
- `SHEPR_INTEGRATION_ID` (read by nothing), and `SHEPR_INTEGRATION_VERSION` with
  `installed_version`, `parse_integration_version`, `IntegrationOutdatedReason` and the
  `NotInstalled` / `Outdated` split: all exist only to be logged, since currentness is
  exact bytes; the hand-bumped `version` in `SPECS` feeds only these (VAL-043).
- `SHEPR_OMP_RETRY_GRACE_MS`, set by nothing (VAL-045).
- The `codex_hooks` deletion in `build_codex_config_with_hooks` and the test
  `install_codex_only_migrates_top_level_feature_flags`: migration code for Codex's old
  flag name (BUG-037).
- The `pi.events.on("shepr:blocked")` listener in `decoders/pi.ts` and
  `decoders/omp.ts`: an inbound event nothing in shepr emits and nothing documents (its
  payload's `label` field is a remnant). Document it as a feature or delete it.
- `process_owned_integration_assets_do_not_report_release` (CLAIM-013).
- `PermissionPolicy`'s two match arms both yield `0o666` (`atomic_replace.rs`).
- `case "session.deleted": break; default: break;` in both OpenCode-family decoders.
- Qoder and Qwen config-dir overrides are captured by `IntegrationEnvironment::capture`
  (it takes every descriptor's `config_dir_override`) although neither has an
  integration target.
- Fifteen `#[cfg(test)] install_<agent>` wrappers in `targets.rs` and
  `registry::integration_hook_events` are one-line forwards of `install(paths,
  Target::X)` and `target.hook_events()`.

## DEAD-014 - Workspace model leftovers: misplaced agent types, stale module names, small duplicates

Reported by: workspace-model.

- `shepr-core`'s `agent_state.rs` (`PresentedAgentState`) and `agent_session.rs`
  (`AgentSessionRefKind`) are used by nothing in core; `shepr-agent` re-exports them and
  `shepr-protocol` imports one directly although it may depend on `shepr-agent`.
  AGENTS.md places agent identity in `shepr-agent`. Move them; the `shepr-core-layer`
  rule then keeps core free of agent types.
- `terminal/state/` module names no longer describe their contents: `sessions.rs`
  holds only a `cfg(test)` seed; `detection.rs` holds title handling and resume
  abandonment; `hooks.rs` is three one-line forwarders to `AgentOwnership` (two used only
  by tests); `names.rs` holds the workspace name type. `Label` (a workspace name and a
  pane label) living under `terminal::state` is why `Workspace` imports
  `crate::terminal::Label`.
- `SpawnGeometry::cell_px()` duplicates the public `cell` field; `spawn_geometry(grid,
  cell)` wraps `PaneGeometry::with_cell`; `Workspace::matches_identity_cwd` compares the
  Git status cwd, not `identity_cwd`; `responses::success` is `Ok`;
  `EndpointOutcome::view_changed` and the `App` test adapters `handle_endpoint_command`,
  `handle_endpoint_command_in` and `handle_endpoint_command_with_render` are three names
  for one call.

## DEAD-015 - Server lifecycle types, payloads and arms with one value or no reader

Reported by: server-lifecycle.

- `restart.rs` `RestartFailure` has one variant, `Local`; the remote variant is gone.
  Use `ServerStopError` directly.
- `LaunchError` payloads never read after construction: `DifferentBuild.status`,
  `SiblingBuildMismatch.status`, `TransitionTimeout.timeout`, `BootTimeout.timeout` and
  `.occupant_only`, `DaemonFailed.status`. Only the messages and `DaemonFailed.class` are
  consumed.
- `wait_for_overridden_server`'s `Starting | Stopping` arm: the comment admits the wait
  it calls returns only settled states.
- `app_paths.rs` `resolve_paths_from_env_with_marker`: the "paths could not be resolved;
  no path-specific error was reported" arm is unreachable (every `None` pushes a
  problem).
- `ServerAddress::apply_to_child_command` takes `&self` and ignores it.
- `ServerHandle::remove_socket_file_if_owned` is `pub` but only `Drop` calls it;
  `ApiClient::request_value_with_timeout` is `pub` with no caller outside the crate's
  tests.
- `connection_health.rs` exists only to re-export `HEARTBEAT_INTERVAL` from `limits`.
- `stop.rs`: the `label` parameter of `stop_socket_with_timeout` and of every
  `ServerStopError` variant has one value, `"server"`.
- The removed-method and removed-command test lists (`schema/tests.rs`
  `removed_methods_are_rejected`, `removed_uncalled_methods_are_rejected`, `cli.rs`
  `unknown_commands_and_launch_flags_are_rejected` with `--session`, `machine`,
  `integration`, `config`, `remote-api-bridge`, ...) assert that unknown strings are
  unknown. They protect against resurrection, which the owner may value, but grow with
  every removal and fail only on a deliberate re-add.
- Not dead, listed so they are not mistaken for leftovers: the `#[serde(default)]` on
  `Pong.stopping` / `starting` and `StatusOverviewJson.summary`, which AGENTS.md keeps for
  `status --all` against older hosts.

## DEAD-016 - The remote bridge's filesystem socket and multi-stream accept loop

Reported by: remote.

Each connection attempt binds a fresh, randomly named, single-use Unix socket in the
runtime directory (`SshStdioBridge::start_command`), and the same thread immediately
connects to it (`MachineSshConnector::attempt` -> `connect_trusted_local_stream_within`).
Nothing else ever connects: the path is random, 0600, and handed to no other process. Yet
the bridge carries an accept loop that serves stream after stream, a failure channel with
"discard unclaimed failures of an earlier stream" logic
(`discard_unclaimed_bridge_failure`, the generation-slot comment),
`PeerAdmission::OwnerOrRoot`, and a comment ("Each local API request has its own stream
and SSH stdio process") inherited from upstream, where the bridge carried API requests.

A `UnixStream::pair()` given to `bridge_connection` directly deletes: the socket file,
its lock sidecar and `release_single_use_socket_lock`, the per-attempt dead-owner sweep
and random token, `remote_bridge_endpoint_path`, `validate_remote_bridge_endpoint_path`
and its export, `validate_machine_bridge_path`, the "bridge socket path does not fit"
launch-fatal setup error (a whole class of `is_launch_fatal_setup_error`),
`BRIDGE_NAME_LABEL_CHARS` and `bridge_name_fragment`, `TeardownResource::Socket`,
`BridgeSocketStartupCleanup`, `BRIDGE_ACCEPT_POLL`'s accept role, the failure-channel
generation problem and the prefix-collision claim (CLAIM-019), and the only producer of
`AddrInUse` as a link failure (DIAG-026). It also replaces the 1 s `reported_failure`
wait with joining the one connection's worker.

## DEAD-017 - Remote options with one production value, a namespace that distinguishes nothing, and dead exports

Reported by: remote.

- `SshFailureDiagnostic`'s `failed_before_remote_result`, `is_ssh_process_failure`,
  `remote_exit_code`, `is_transient_network_failure`, `needs_attention`, `from_message`
  and `from_local_setup_error` have no production caller (POL-010); the production
  interface is `from_error`, `from_ssh_output`, `with_context`, `evidence` and
  `disposition`.
- `ManagedSshOptions::control_path: Option<PathBuf>` is always `Some` in production;
  `apply_managed_ssh_options(_, None)` and `SshStdioBridge::start(.., ssh_options: None)`
  are `None` only from tests; `RemoteSsh::attempt_deadline: Option<Instant>` is always
  `Some` before a production command, its `None` branch exercised only by
  `an_attempt_deadline_shortens_and_then_refuses_commands`. Make the deadline a
  constructor argument.
- `ssh_control_path_under` hashes `client_config_file()` into the control socket name,
  justified as "User ControlPaths may be shared across isolated Shepr configs". There is
  no config path override, and the socket already lives in the per-profile runtime
  directory, so the namespace distinguishes nothing in production except two
  `XDG_CONFIG_HOME` values sharing one `XDG_RUNTIME_DIR` (a test setup). Likely a
  leftover of the removed config override; hash the target alone, or say what it is for.
- `pub use shell_command::shell_quote` and `pub use preflight::classify_check` have no
  user outside the crate; `pub mod machine` exports `RemoteExecutable`,
  `RemoteExecutableError` and `SshMetadataCache`, none used outside; `SshRuntimeError`
  and `UnsafeSshRuntimeDirectory` are `pub` (with a `pub fn new`) only because the
  test-only export returns them.
- `EndpointSupervisorEvent::Status { message: EndpointFailure }`: the field is a failure
  named `message`, a remnant of a string-typed status.
