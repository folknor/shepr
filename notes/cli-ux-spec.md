# CLI reduction: green landing specification

This specifies commits, not work already done. Each numbered landing is one commit that must pass `brokkr check` before it is committed. No state migration is needed. The decisions and target surface are in [cli-ux.md](cli-ux.md); this document treats them as settled. "Remove an API method" below includes its `Method` variant and traits, request and response schema that become unused, dispatcher arm, handler, error codes and tests, unless a retained internal path is named. Do not remove domain operations merely because a CLI caller disappears.

## Dependencies

```text
1 detect command -> 4 old command groups -> 6 machine API bridge
2 integrations ---------------------------> 4 old command groups
3 metadata ------------------------------> 4 old command groups
5 direct remote attach -------------------> 7 session identity
6 machine API bridge ---------------------> 7 session identity
7 session identity -----------------------> 8 configured machines -> 9 startup authentication
```

Landings 1, 2, 3 and 5 can be ordered freely before their dependents. Landing 6 follows 4 because that leaves `--machine` serving only the reviewed status and server commands. Landing 8 can be prepared earlier, but the specified single-session endpoint shape assumes 7. The removals in 9 require 8's launch-time config source and authentication replacement. The `status` and `server` command reviews, automatic integration installation, and workspace/tab flattening remain deferred; none is a prerequisite for these commits.

## 1. Give detection its own CLI

**Decision.** Keep manifest maintenance commands as `detect capture` and `detect explain`, with pane IDs as live targets.

**Change together.** Add the `detect` clap group and typed handler in `src/cli/spec.rs`, `src/cli.rs` and a focused CLI module. Move `agent explain` file evaluation and text/JSON rendering from `src/cli/agent.rs`; keep `--file` plus `--agent`, `--json`, and `-v`, and remove `--format`. Implement capture with `Method::AgentRead` initially, explicitly setting `ReadSource::Detection`, `ReadFormat::Text`, `strip_ansi = true`, and no line limit. Live explain can initially use `Method::AgentExplain`. Validate a live argument as a pane ID rather than permitting `resolve_agent_target` to fall back to a unique agent name. Update the AGENTS.md capture instruction in this landing's eventual implementation commit and any command examples or parser tests that mention the old spelling. The new command should print the capture text directly and preserve file-mode operation without a server.

**Leave.** Keep old `agent read` and `agent explain` as temporary aliases until landing 4, and retain the detection manifests, `PaneReadResult`, and the detector's `PaneTerminal::detection_text` path. Do not use the scrolled viewport or change manifest matching.

**Done when.** CLI parser tests cover the exact new grammar, reject removed capture switches, and reject agent-name targets. Server/API tests prove capture reads the detector snapshot and explain uses the same pane's live detection input. A file-mode test covers evaluation without a running server. Risk: existing `agent read` defaults to `ReadSource::Recent`, so simply renaming it would capture the wrong text. Also, live `handle_agent_explain` can return a hook-authority skip instead of rule evidence; retain that behavior and explain it in the command's help.

## 2. Remove the CLI-calling integrations

**Decision.** Remove letta, qodercli and qwen hook integrations, while keeping their compiled detection manifests.

**Change together.** Delete their `IntegrationTarget` variants in `crates/shepr-agent/src/agent/mod.rs`, registry entries, install/uninstall/status functions and constants in `crates/shepr-agent/src/integration/`, their hook assets, and their integration tests. Update the `integration` CLI target parser, status output and relevant tests. Keep the group for the other integrations. Search all target enumeration and version/status tests for exhaustive matches. Delete any environment enum members only if no detection or agent-launch path uses them.

**Leave.** Keep `detect/manifests/{letta,qodercli,qwen}.toml`, normal detection labels, agent resume definitions where supported, `pane.report_agent` and `pane.report_agent_session`, and all other hooks.

**Done when.** `integration install/status/uninstall` no longer list or accept those targets; their screen detection still works. Tests for surviving integrations still assert direct socket JSON reports. Risk: installed old hook files can remain on a user's machine, but this repository has never been run and no migration is required.

## 3. Remove metadata reporting and release-agent

**Decision.** Remove `workspace report-metadata`, `pane report-metadata`, `pane release-agent`, and the metadata feature.

**Change together.** Delete the CLI parsing and formatting in `src/cli/{workspace,pane}.rs`, the metadata API variants and parameter types in `crates/shepr-api/src/schema{,/workspaces,/panes}.rs`, and their `App` handlers and dispatcher arms in `crates/shepr-server/src/app/api{,/workspaces,/panes/reports}.rs`. Remove the metadata normalization helpers and limits when their callers disappear. In `shepr-mux`, remove workspace and terminal metadata token stores, TTL/sequence tracking, agent title/display overrides, expiry scheduling and related events. Follow the fields through `shepr-api` snapshot/event types, `shepr-protocol` projection fields, server view creation, and client sidebar rendering/configured custom `$token` resolution. Remove metadata-only tests and replace any snapshot fixtures that require those fields. Preserve built-in sidebar tokens and actual agent state, agent session references, OSC title, and Git data. Delete `release_agent_with_mutation` only after checking its lifecycle tests and all production callers; remove the API release handler and its response/error code in the same commit.

**Leave.** `pane.clear_agent_authority` and its hook/detection authority logic need an independent audit; do not infer that they are metadata. Keep `pane.report_agent`, `pane.report_agent_session`, their validation and source sequencing needed by hooks and restore. Keep generic `PaneReadResult` and session persistence.

**Done when.** Removed methods fail as unknown API methods, metadata cannot appear in snapshots or the sidebar, and hook state/session reports and resume still work. Retain tests for screen detection after hook authority ends. Risk: custom sidebar token syntax is supported by metadata reporting today; config validation and default examples must stop advertising it, while normal built-ins remain valid.

## 4. Remove the old workspace/tab/pane/agent/terminal CLI surface

**Decision.** Remove the scripting and direct terminal attach command groups. `detect` is now the only inspection command from the former agent group.

**Change together.** Delete group builders in `src/cli/spec.rs`; typed variants, parsers and dispatch in `src/cli.rs`; `src/cli/{workspace,tab,pane,agent,runtime}.rs` and their helpers when unreferenced; the terminal attach/title set/clear and agent attach takeover paths. Remove selector, `--env`, focus and token options that only these groups used, and the `--current` cross-target guards in `src/cli/target.rs`. Retire `Method` variants whose only production caller was this CLI: `workspace.list/get`; `tab.list/get`; `agent.list/get/rename/focus`; `pane.layout/process_info/neighbor/edges/list/current/get/read/move`; and their dedicated handlers, parameter/result types and API tests. `workspace.move_block`, `layout.export/apply`, and `pane.clear_agent_authority` have no CLI or TUI request caller found, but are outside the listed command removal: audit their internal handler/test uses and remove only if the chosen scope explicitly includes this extra API pruning. `client.window_title.set/clear` are issued by the terminal title CLI in `src/cli.rs`, so remove their server-side API path if no other production caller remains. Remove `agent read/explain` aliases now; either retain the two API method names behind `detect` or rename both wire names and handlers in this commit. Update `crates/shepr-api/src/schema/tests.rs` and exhaustive `Method`/`ResponseResult` matches. Remove `run_terminal_attach`, `SessionMode::DirectAttach`, direct terminal setup, takeover/escape handling, the direct `ClientMessage::AttachTerminal` route and its server/client protocol replies if no hidden client path uses them. Keep the TUI input forwarding and paste notice helpers that are currently housed in `crates/shepr-client/src/attach.rs`, moving them to a fitting module if needed.

**Leave.** Every method in the keep column of the audit below, server-owned workspace/tab/pane operations, `session.snapshot` for client attach, `client_shell.surface.set`, and both hook report methods. Keep `terminal` domain types and the headless server's PTY hosting. Do not delete pane `--session` launch arguments belonging to external agents.

**Done when.** Help rejects the removed groups, while TUI workspace creation, focus, rename, movement, splitting, resizing, zoom, input flags and copy mode still work. Delete CLI parser tests for the old groups; retain or adapt server/TUI tests for shared operations. Risk: deleting API methods by name similarity will break TUI actions. The table below is the required deletion boundary.

## API method audit for landing 4

This audit is against production `Method::...` call sites in `crates/shepr-client/src/`, `src/cli/`, and other server/remote request senders, plus the JSON hook assets. A server handler or test alone does not establish a live caller. "Keep" also means keep its handler and schema after the CLI group is gone.

| Method(s) | Result and concrete dependent |
|---|---|
| `workspace.create/focus/rename/close` | Keep. Client navigation, overlay input, context menu and endpoint activation send these. |
| `workspace.move` | Keep. Client mouse drag sends it. |
| `workspace.get` | Remove. CLI only. |
| `workspace.list` | Remove. The observed client endpoint-command and server-app references are tests; the CLI is the live request sender. |
| `workspace.move_block` | No production request sender found. Server handler exists; optional separate API pruning, not a TUI dependency. |
| `workspace.report_metadata` | Remove in landing 3. |
| `tab.create/focus/rename/move/close` | Keep. Client navigation, mouse, overlay input or context menu sends each. |
| `tab.list/get` | Remove. CLI only. |
| `agent.read/explain` | Keep as live `detect` transport for now; file explain stays local. |
| `agent.list/get/rename/focus` | Remove. CLI only; TUI agent rows use snapshots and pane focus. |
| `pane.split/swap/zoom/focus_direction/resize/clear/focus/rename/close` | Keep. Client navigation or context menu sends each. |
| `pane.scroll/selection.read/copy_motion/copy_search` | Keep. Client mouse, selection and copy mode send them. |
| `pane.input.set` | Keep. Client context menu sends it. |
| `pane.layout/process_info/neighbor/edges/list/current/get/read/move` | Remove. CLI is their only production request sender; `pane.move` has server test coverage but no client sender. Keep generic read formatting needed by `detect capture`. |
| `pane.report_agent/report_agent_session` | Keep. Bundled surviving hooks send socket JSON, and server handlers update state/session references. |
| `pane.report_metadata/release_agent` | Remove in landing 3. |
| `pane.clear_agent_authority` | No CLI/TUI sender found; server path exists. Audit independently from hook state before pruning. |
| `layout.set_split_ratio` | Keep. Client mouse drag sends it. |
| `layout.export/apply` | No CLI/TUI sender found. Server handler and tests exist; optional separate API pruning. |
| `client.window_title.set/clear` | CLI terminal title only; remove with that CLI path after confirming no client request sender. |
| `ping`, `server.stop`, `server.ssh_agent.register` | Keep for status/stop/remote SSH setup. |
| `client_shell.surface.set`, `session.snapshot` | Keep for client attach and surface ownership. |
| `events.subscribe/wait` | Keep until the API subscription/wait surface is separately reviewed; neither is an old CLI group command. |

## 5. Remove direct remote attach, default-config and config check

**Decision.** Remove `--remote`, `--remote-keybindings`, `--default-config` and `config check`; saved machines remain.

**Change together.** Remove clap options, `Invocation` fields and launch branches in `src/{main,cli,cli/spec}.rs`. Delete the direct-remote `RemoteCliCommand` variant, `remote::args` construction, foreground direct bridge and client role/attach mode code only where no saved-machine or hidden `remote-client-bridge` path uses it. Rework `crates/shepr-client/src/endpoint` Local transport assumptions and `LocalEndpointLink::SshBridge` if they are now direct-remote-only. Retain `RemoteSsh`, managed SSH config, bridge IO, build preamble and executable discovery used by saved machines. Remove the print-default-config branch, not the bundled `default.toml` used for normal config defaults. Delete the `config check` clap group, `ConfigCommand`, `config_check` and `config_check_from_paths` display path, its provenance-only formatting and tests; keep normal launch validation and the config diagnostics it uses. Update direct-remote reattach/bootstrap messages in `shepr-remote`, client tests and stale comments in config defaults.

**Leave.** `--machine`, session selection, catalog and remote client bridge for later landings. `remote.manage_ssh_config` still controls saved-machine SSH. Do not change host key policy.

**Done when.** Removed flags and `config check` are usage errors; configured machines still connect, and invalid config still fails a normal launch. Tests cover saved bridge discovery and local attach after the direct path is deleted. Risk: `shepr --remote` and saved SSH share low-level bridge utilities, so deleting the whole bridge module would remove a live path.

## 6. Remove `--machine` and its API bridge

**Decision.** Drop remote CLI API forwarding. Running `ssh <host> shepr ...` covers the remaining remote status/stop cases.

**Change together.** Delete root `--machine`, `can_run_on_machine`, `run_on_machine`, the remote branch of `CliContext` and stale-metadata retry in `src/cli/target.rs`. Remove `Launch::ApiBridge`, hidden `remote-api-bridge --check`, `SavedSshApiBridge`, `cached_remote_api_command`, `CandidateVerification::ApiForwarding`, its discovery check and API-only bridge socket machinery. Keep status/client and server/stop local dispatch. Remove `--machine`-specific option diagnostics, command guidance in `crates/shepr-api/src/guidance.rs`, and relevant parser/bridge tests. Adapt discovery to `StatusProbe` only.

**Leave.** Hidden `remote-client-bridge`, saved-machine connector/cache, status client probe, `server.ssh_agent.register`, and direct local JSON API. The remote executable location cache remains keyed by SSH target even after its API-bridge sharing comments are removed.

**Done when.** `--machine` and hidden API bridge are rejected, saved-machine TUI discovery still probes `status client`, and remote client bridge still starts a daemon. Risk: a build mismatch message that suggests `shepr --machine ... server stop` must be rewritten in this commit; do not leave an impossible remedy.

## 7. Remove named shepr sessions and split runtime by build profile

**Decision.** Eliminate `--session`, the `session` group, named sockets/layout/history and `SessionId`; dev builds get separate runtime paths automatically.

**Change together.** In `build.rs`, expose the build profile as a compile-time constant independent of the build ID, then use it in `shepr-config::AppPaths` path resolution so release retains default paths and dev uses its own runtime and saved layout/history namespace. Keep config and saved-machine configuration shared. Remove `SHEPR_SESSION` from `shepr-core::env`, pane child export/strip rules and server launch inheritance. Simplify `AppPaths`, `ServerAddress`, path provenance, socket path creation, `shepr-api::session`, `src/main.rs`, `src/cli.rs`, local server startup, remote bridge arguments and saved connector to a single session per build. Remove the `session` clap group (`attach/list/stop/delete`) and named-session management types; keep the actual mux snapshot, history persistence and agent resume, whose "session" means saved layout or agent conversation, not a shepr server name. Rewrite `SessionId::attach_command` and all guidance in `shepr-api`, `shepr-client`, `shepr-server`, `shepr-remote`, `src/autodetect.rs` to plain `shepr` and `shepr server stop [--force]` guidance. Replace AGENTS.md's dev-run instructions in the eventual implementation commit. Since machines still use the catalog at this point, make its `session` member and `--remote-session` option disappear here or, if retained internally for one commit, force the default and remove them in 8.

**Leave.** `status` and `server stop` commands, socket environment overrides if still needed for an explicit socket target, `remote-client-bridge`, config shared across profiles, persistence of layout/history and agent conversation IDs. Never remove external agents' own `--session` resume flags.

**Done when.** A dev and a release build resolve distinct sockets and saved layout paths without flags, while sharing config; `--session`, `SHEPR_SESSION` targeting and `session *` fail or are ignored only where explicitly documented. Tests cover profile path choice, override precedence and build mismatch guidance. Risk: build identity already includes `PROFILE`, but that fact alone does not change paths. Also, release defaults must stay in their current XDG locations.

## 8. Use launch-time `[[machines]]` config

**Decision.** Saved machines become `[[machines]]` entries with `label` and `ssh`; labels become IDs; catalog watching and remembered selection end.

**Change together.** Add machine entries to `shepr-config::Config`, `ValidatedConfig` and its positional wire form, with validation of nonblank unique labels and `SshTarget` shape. Preserve server/client config snapshot validation on the sending host and receiving client; skip only the existing host-local cwd/shell checks, not machine syntax. At client launch, take profiles from the validated local config, construct endpoint identities from labels, and initialize selection to Local. Do not replace this local machine set with a remote server's `resolved_config` when changing the active endpoint; that snapshot still supplies remote UI settings. Refactor `ClientEndpointId`, `SavedSshConnector`, sidebar/topology/diagnostics, supervisor maps and tests away from generated `ProfileId`. Remove catalog/selection files, `EndpointCatalogWatch`, poll/reconcile logic, `EndpointSelectionTracker` disk writes, and catalog error types; retain in-memory selection/rollback protection for failed handoffs. Keep `SshMetadataCache` keyed by target. Replace machine add-time preparation with the existing connector discovery and bridge daemon startup path. Remove `machine list/status/add/remove` and their add-time SSH preparation here. Keep `machine reconnect` temporarily as the interactive authentication route until landing 9; narrow its selector from label-or-ID to label. Update config help/default comments and tests.

**Leave.** Noninteractive `BatchMode=yes`, strict host-key checking, SSH connection retry, detached daemon checks, diagnostics overlay and manual `machine reconnect` until 9. Keep local failure policy: with configured machines, local-server loss must not end the client.

**Done when.** Launch fails on duplicate labels or malformed SSH targets; unreachable hosts still fail soft; client starts on Local and switches by label; edits to config do not mutate an open client's machine set; no catalog or selection file is read or written. Tests cover config round trip and receiving-host snapshot validation, multi-machine local failure, selection rollback, and remote executable cache reuse. Risk: `shepr-config` is below `shepr-remote`, so validation cannot depend on `shepr_remote::SshTarget`; move or reproduce the pure target parser in a lower crate without reversing the dependency rule. Another risk is old tests constructing generated IDs across many client modules.

## 9. Authenticate at startup, then remove the machine group

**Decision.** Replace interactive `machine reconnect` with pre-TUI authentication for configured hosts.

**Change together.** Before taking over the terminal in the local client startup path, run bounded noninteractive checks for all configured targets concurrently using the same managed SSH config/control socket strategy as `SavedSshConnector`. Classify authentication failures separately from offline, missing host key, incompatible build and daemon errors. For only authentication-needed targets, print the label and run interactive SSH sequentially on shepr's control socket, with `BatchMode=no` and `StrictHostKeyChecking=yes`; then start the TUI and its normal noninteractive supervisors. Do not accept host keys automatically. Remove `machine reconnect`, the remaining `machine` clap group, `src/cli/machine.rs` and its add/status/reconnect preparation helpers, and diagnostic suggestions to use those commands. Give overlay text a restart-the-client remedy for authentication loss mid-session; update reconnect timing comments that promised a CLI action. Keep `ControlPersist` behavior and cache keyed by SSH target.

**Leave.** Soft-failure remote endpoints, local reconnect while machines are configured, hidden remote client bridge and discovery/status probe. Do not add a TUI suspend/auth action yet.

**Done when.** A host requiring password/passphrase/2FA/FIDO interaction prompts before TUI entry on the managed socket, already-authenticated and offline hosts do not prompt, unknown host keys remain failures, and mid-session auth expiry is reported with restart guidance. Tests use a fake SSH process/transport seam and assert check parallelism, prompt serialization, timeout and classification without a real host. Risk: checking hosts concurrently must not let their prompts interleave; the precheck must be noninteractive and bounded by the existing connect timeout. The managed control socket must outlive the preflight through the client session.

## Corrections to the agreed plan from code inspection

- `agent read` defaults to recent output, not detection output (`src/cli/agent.rs::read_params`). `detect capture` must set the source explicitly. `handle_agent_read` resolves agent names, so a pane-only command also needs target validation or a pane-specific handler.
- Metadata is broader than the two API reports: `shepr-mux::TerminalState`, workspace token storage, protocol projections, client sidebar custom tokens, and expiry events use it. `pane.release_agent` also clears matching persisted agent session references in `TerminalState::release_agent_with_mutation_at`; removal must preserve the separate normal session-report/restore path.
- The client directly sends many methods behind the removed CLI groups. The audit table records each. In particular `workspace.move`, `tab.move`, `layout.set_split_ratio`, `pane.scroll`, selection/copy methods and `pane.input.set` survive.
- `layout.apply`, `layout.export`, `workspace.move_block`, and `pane.clear_agent_authority` are exposed API methods despite having no matching CLI command or observed TUI sender. The plan's command-based pruning criterion does not by itself decide their removal.
- The build ID includes profile inputs (`build.rs`) but `AppPaths::app_dir_name` currently fixes the same paths for every build. Session removal therefore needs an explicit new path selector; build identity cannot isolate the processes.
- Machine catalog identifiers are `ProfileId` values throughout the client, supervisor, selection file, bridge sockets and tests. A label-only catalog change is an endpoint identity change, not just a TOML parser change. `shepr-config` cannot directly import the SSH target parser from the higher `shepr-remote` layer.
- There is no `docs/` or `reference/` directory in this checkout. The command guidance to update currently lives in root convention files, `crates/shepr-config/src/default.toml`, CLI help and source messages/tests. Do not create empty document directories for the reduction.

## Lateral findings

- `src/cli/agent.rs::print_agent_explain_text` uses `unwrap_or` on JSON values and prints Unicode match symbols. The requested specification uses ASCII, but the later CLI implementation can decide whether those symbols belong in a terminal UI; this is outside the command reduction.
- The API has `events.subscribe` and `events.wait` plus layout methods with no matching user command in the target. Their retention is an API-scope decision after this reduction, not evidence that a removed CLI group is still needed.
- `crates/shepr-agent/src/agent/mod.rs` contains `--session` flags for external agents' resume commands. A repository-wide string deletion would break resume on restore.
