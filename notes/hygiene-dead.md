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
- `actor::abandoned()` is a one-line wrapper around an enum constructor.

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
- `ClientShellAgent.agent` on the wire is still an `Option`, though the server now
  always sends `Some` (`SnapshotAgent.agent` became required).
- `SessionRestoreDamage.repaired_bookmarks` is a count that can only be 0 or 1 (there
  is one bookmark); a `bool` says that.
- `session_saves_stopped` and `session_saves_blocked_on_backup` are two wire booleans
  for mutually exclusive states; one unit enum says that (check the codec rule against
  tagged enums first).
- `bundle.rs` builds dummy params to read an API method name because `shepr-api`'s
  `MethodKind` and its `traits()` are private; a public name lookup by kind is cleaner.

## DEAD-013 - Integration code, keys and assets nothing needs

Reported by: integrations.

- `SHEPR_INTEGRATION_ID` (read by nothing), and `SHEPR_INTEGRATION_VERSION` with
  `installed_version`, `parse_integration_version`, `IntegrationOutdatedReason` and the
  `NotInstalled` / `Outdated` split: all exist only to be logged, since currentness is
  exact bytes; the hand-bumped `version` in `SPECS` feeds only these (VAL-043).
- The `pi.events.on("shepr:blocked")` listener in `decoders/pi.ts` and
  `decoders/omp.ts`: an inbound event nothing in shepr emits and nothing documents (its
  payload's `label` field is a remnant). Document it as a feature or delete it.
- `case "session.deleted": break; default: break;` in both OpenCode-family decoders.

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
- `ui/panes.rs` carries a duplicated doc comment above `split_hit_rect` (reported by
  the pane chrome fix).

## DEAD-015 - Server lifecycle types, payloads and arms with one value or no reader

Reported by: server-lifecycle.

- `connection_health.rs` exists only to re-export `HEARTBEAT_INTERVAL` from `limits`.
- `stop.rs`: the `label` parameter of `stop_socket_with_timeout` and of every
  `ServerStopError` variant has one value, `"server"`.

And:
- The removed-method and removed-command test lists (`schema/tests.rs`
  `removed_methods_are_rejected`, `removed_uncalled_methods_are_rejected`, `cli.rs`
  `unknown_commands_and_launch_flags_are_rejected` with `--session`, `machine`,
  `integration`, `config`, `remote-api-bridge`, ...) assert that unknown strings are
  unknown. They protect against resurrection, which the owner may value, but grow with
  every removal and fail only on a deliberate re-add.
- Not dead, listed so they are not mistaken for leftovers: the `#[serde(default)]` on
  `Pong.stopping` / `starting` and `StatusOverviewJson.summary`, which AGENTS.md keeps for
  `status --all` against older hosts.

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
