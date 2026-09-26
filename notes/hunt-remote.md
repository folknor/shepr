Structural review: remote machines and endpoints

What I read: `src/remote.rs`, `src/remote/{attach,saved,args,restart_policy}.rs`, `src/client/endpoint.rs`, `src/client/endpoint/{catalog,supervisor,ssh_metadata,health}.rs`, `src/client/endpoint_commands.rs`, `src/client/endpoint_selection.rs`, `src/cli/machine.rs` and `src/cli/target.rs`. I skimmed or skipped `registry.rs`, `activation.rs`, `control.rs`, `writer.rs`, `message_policy.rs`, `host.rs`, `process.rs` and `ssh_agent.rs`. I edited nothing and ran no commands.

The top three changes, in order:
1. A typed SSH failure that replaces the six string classifiers.
2. A neutral `machine` module holding the catalog, IDs and cache, which breaks the `remote` / `client::endpoint` import cycle and pulls selection out of the catalog.
3. Deleting the capability and restart-policy fossil, and making the handshake the only compatibility decision.

After those, splitting `attach.rs` is mechanical.

## 1. Axes that should be types

- **Profile ID travels as `&str` after being parsed.** `ProfileId` exists, but `SavedSshConnector::new`, `SavedSshApiBridge::start`, `SshMetadataCache::new` and `saved_bridge_path` all take `&str`, and each one validates again:
  - `saved.rs` `validate_profile_path_id` is a second, hand-copied 32-hex check that repeats `ProfileId::parse`.
  - `SshMetadataCache::new` parses again.
  - `&profile_id[..16]` in `saved_bridge_path` and `SavedSshApiBridge::start` is a panic-on-slice that is only safe because of that earlier check.
  - Fix: take `&ProfileId` everywhere and delete `validate_profile_path_id`. The "rejects invalid profiles" test in `saved.rs` then tests something the types make impossible.
- **Session name is a `String`, checked by `crate::session::validate_name` at five sites.** They are `SavedSshEndpoint::validate`, `SavedSshConnector::connect`, `validated_saved_ssh`, `check_saved_ssh` and `prepare_saved_ssh`. A `SessionName` newtype, minted once when the catalog is loaded or the CLI is parsed, would remove all of them.
- **SSH target is a `String` with three validators that disagree:**
  - `validate_remote_target` rejects only empty targets and a leading `-`.
  - `SavedSshEndpoint::validate` adds a length limit, no control characters and no password in the userinfo.
  - `ssh_authentication_command` (`attach.rs:439`) has its own inline empty / `-` / control-character check.
  - So `shepr --remote` accepts targets with control characters, and the saved catalog does not.
  - Fix: one `SshTarget` type, owning its validation, taken by `RemoteSsh`, the connector, the bridge and the catalog.
- **Remote executable path has three definitions of "valid":**
  - `remote_shepr_from_path`: absolute and not a mise shim.
  - `SshMachineMetadata::is_valid`: adds no control characters and at most 4096 bytes.
  - The shell `case` in `posix_remote_api_discovery_script`.
  - They disagree today. Discovery can accept a `RemoteShepr` whose `machine_metadata()` then returns `None`, so it is remembered but never cached.
  - Fix: a `RemoteExecutable` newtype with one parse, wrapped by `RemoteShepr`.
  - `SshMachineMetadata.os: String`, where only `"linux"` is valid, is a leftover from upstream in a Linux-only fork. Delete the field.
- **Connection generation is a bare `u64` everywhere** (supervisor, registry, commands, selection tracker `Option<u64>`). A `ConnectionGeneration` newtype would stop it being swapped with other counters.
- **Command correlation keys:**
  - `boot_id: String` and `request_id: String` should be newtypes.
  - `EndpointCommandLane.retired: VecDeque<(u64, String, String)>` is a bare tuple. It should be a `RequestKey { generation, boot_id, request_id }` struct, which `InFlightCommand` also has inline.
  - `ClientShellEndpointError.code: Option<String>` with the magic strings `"endpoint_timeout"` and `"endpoint_response_too_large"` should be an enum.
- **Failures reach callers only as prose.** This is the biggest item in this scope; see decision A in section 2.
- **`RemoteServerStatus::Running`'s capability bools and `remote_server_restart_reason(protocol, bool, bool, bool, bool)`** take five positional bools that are easy to swap. See section 3 for why most of them should be deleted rather than typed.
- **`EndpointSupervisorEvent::Status { message: String }`.** The status there is derived from that same text (next section), so the classification is lost by the time it reaches the UI.

## 2. Decisions made in more than one place

**A. What kind of SSH failure is this? (6 sites, 2 disagreements)**
- `saved::saved_ssh_failure_needs_attention` uses the `ErrorKind` plus substrings `"permission denied"`, `"protocol"`, `"not ready"`, `"install or update"`, `"handshake rejected"` and others.
- `attach::is_ssh_link_failure` uses a downcast to `SshBridgeExit` with code 255, else a set of kinds.
- `attach::ssh_error_requires_authentication` is lowercase and includes `"signing failed"`.
- `remote.rs::is_remote_auth_error` is case-sensitive `"Permission denied"` with no `"signing failed"`. It already disagrees with the previous classifier.
- `remote.rs::is_remote_host_key_error`.
- `SavedSshApiBridge::stale_metadata_failure` looks for the marker string `STALE_API_METADATA`, which travels through the remote's stderr and then into an error message.
- `handshake_error` in `supervisor.rs` carefully picks an `ErrorKind` so the substring classifier gives the right answer. That coupling exists only because the classifier reads text.
- Separately, `"protocol"` as a bare substring will match unrelated ssh stderr, such as `kex_exchange_identification`-style "Protocol mismatch" noise, and turn transient failures into Attention.
- `attach.rs:999` hard-codes `255` instead of `SSH_OWN_FAILURE_EXIT_CODE`: two spellings of the same decision.
- **Owner:** one `SshFailure` enum (Link / Timeout / Auth / HostKey / NotInstalled / Incompatible / StaleApiMetadata / RemoteCommand{code} / Local). Build it once, where ssh output is interpreted (`command_failed`, `ssh_bridge_exit_error`, `path_lookup_result`, `remote_client_status`, `handshake_error`). Carry it typed, not as `io::Error` text. Then `needs_attention`, `is_link_failure`, the CLI status label and the hints each become one `match`.

**B. Is this remote shepr compatible? (3 sites)**
- `RemoteClientStatusJson::supports_endpoint_requirement` (a protocol-number compare with an unused parameter).
- `remote_server_restart_reason` (protocol plus three capability flags).
- The handshake build-identity preamble.
- AGENTS.md says client and server are always the same build, so the handshake is the only authority. The other two are upstream federation fossils. For example, `_require_surface_interest` is ignored in `remote_server_status` and `supports_endpoint_requirement`, and yet `run_remote` loads the whole catalog just to compute it.

**C. Which remote executable is right for this machine? (2 discovery pipelines, 1 shared cache)**
- `SavedSshConnector` uses `DiscoveryProgress` plus a `status client --json` protocol probe.
- `SavedSshApiBridge` uses `posix_remote_api_discovery_script` plus a `remote-api-bridge --check` capability string.
- Both read and write the same `SshMetadataCache` file per profile.
- The connector calls `metadata_cache.invalidate()` on any non-link failure, which also throws away the API bridge's cache, and each pipeline can store a path the other would reject.
- **Owner:** one discovery, with one definition of "match". Either both paths use the protocol probe, or the executable check becomes a single `shepr remote-probe` that reports both. Then one cache.

**D. Is this profile selectable, and is the selection still valid? (about 7 sites)**
- In the catalog: `EndpointCatalog::is_selectable`, `select_ssh` (inline copy), `validate` (inline copy), `load_from_paths` (inline copy), `replace_profiles`, `set_enabled` and `remove_ssh` (each clears the selection).
- In the tracker: `EndpointSelectionTracker::settle` (filters with `is_selectable`).
- In the CLI: `cli/machine.rs` `remove` and `set_enabled`, which rewrite the selection file themselves.
- **Owner:** selection should not live in the catalog at all (see section 3). Store it as `Option<ProfileId>` and resolve it through one `effective_selection(&profiles)` wherever it is read.

**E. Is this machine enabled or live?**
- The catalog filters, `EndpointCatalogChanges::between` (its own "live" definition), `EndpointSupervisors::with_ssh_settings` (a filter), `resolve_machine` (the enabled check) and `contains_enabled_target_session`.
- **Owner:** mostly one-liners today, but "live" (enabled plus target plus session identity) belongs as a method on the profile, and `between` and the supervisor should both call it.

**F. The retry promise ("within 30 seconds")**
- It is written in `cli/machine.rs` `reconnect`'s output string, in `MAX_RETRY_DELAY` and `ATTENTION_RETRY_DELAY`, and in `ATTEMPT_BUDGET < MAX_RETRY_DELAY`.
- The supervisor has a test for it, but the CLI text is a literal. Export the constant and format the message from it.

**G. What counts as a machine selector?**
- `machine status` and `reconnect` accept a label or an ID through `resolve_machine`.
- `rename`, `remove`, `enable` and `disable` accept only a raw ID through `ProfileId::parse`.
- `resolve_machine` also rejects disabled machines, so `machine enable <label>` could never work through it anyway.
- **Owner:** one resolver with an "include disabled" flag.

## 3. Structure

- **Dependency cycle between `remote` and `client::endpoint`.**
  - `remote` imports `client::endpoint::{SshMachineMetadata, SshMetadataCache, EndpointCatalog}`.
  - `client::endpoint` imports `remote::{SavedSshConnector, SavedSshSettings, SavedSshBridge, saved_ssh_failure_needs_attention}`.
  - The machine catalog is not a client concern: the CLI (`machine`, `--machine`) and `remote::run_remote` use it too.
  - **Move** `ProfileId`, `SavedSshEndpoint`, `EndpointCatalog` (profiles only), `EndpointCatalogWatch`, `EndpointCatalogChanges`, `SshMetadataCache`, `SshMachineMetadata` and `store_private_json` into a neutral `src/machine/` module. That module depends on nothing above it.
  - `remote/` then becomes pure SSH transport, discovery and bridge, depending on `machine`. `client/endpoint/` becomes runtime supervision, depending on both.
  - `store_private_json` is a general atomic private-file writer that happens to live in `catalog.rs`, and its temp name says `.endpoints-`. It belongs in `platform`.
- **`EndpointCatalog` mixes persisted shared state with per-client runtime state.** `selected_profile` is `#[serde(skip_serializing)]` plus a `default` field, ignored on load, set from a second file, and validated inside `validate()`, so storing profiles fails if the in-memory selection is stale. Split it into `MachineCatalog` (persisted, shared) and a selection owned by `EndpointSelectionTracker`, which already holds `persisted` and the attempt. That split resolves decision D.
- **`attach.rs` (about 2000 lines) does roughly eight unrelated jobs:**
  - the interactive `--remote` launcher (`run_remote`, `reattach_command`)
  - one-shot saved checks (`check_saved_ssh`, `prepare_saved_ssh`)
  - remote command construction (`RemoteShepr`, POSIX wrappers)
  - the managed ssh config
  - the process-global `TeardownRegistry`
  - the `RemoteSsh` command runner
  - executable discovery plus the API discovery script
  - server status, the restart prompt and the stop/wait sequence
  - the stdio bridge pump
  - error types

  Suggested split: `remote/ssh.rs` (RemoteSsh, options, managed config, `SshFailure`), `remote/discovery.rs`, `remote/server_lifecycle.rs` (status, prompt, stop), `remote/bridge.rs` (SshStdioBridge, pumps, teardown), `remote/launch.rs` (`run_remote`). `saved.rs` then is only the saved-machine connector.
- **Delete the upstream compatibility fossils:**
  - `restart_policy.rs` (SurfaceInterest, HealthCheck and DaemonDetach reasons for "servers started by an older shepr build").
  - The `capabilities` JSON.
  - The `require_surface_interest` plumbing and the catalog load in `run_remote`.
  - The `remote-api-bridge --check` versioned string `shepr-api-bridge-v1`.
  - With a same-build rule, the only real outcome is "wrong build: update it there". The prompt text ("predates Shepr's stable endpoint protocol", "join saved SSH endpoint federation") is herdr-era wording.
  - Probably also the version fields in `StoredMetadata`, `CATALOG_VERSION` and `SELECTION_VERSION`, since there are no compatibility obligations. These are on-disk formats, though, so this is a judgment call.
- **`endpoint_commands.rs` and `endpoint_selection.rs` sit at the `client/` root** but are endpoint subsystems keyed by `ClientEndpointId`. They belong under `client/endpoint/` next to `registry` and `supervisor`.
- **Config access is inconsistent.**
  - `SavedSshSettings` exists so the long-lived connector never re-reads config.
  - `RemoteSsh::new_noninteractive`, `ssh_authentication_command`, `run_remote` and `prepare_saved_ssh` each call `Config::load()` themselves, deep inside the transport.
  - Fix: pass settings in from the CLI entry points and remove `Config::load` from `remote/`.
- **`cli/target.rs` uses a thread-local `TARGET` plus `PROTOCOL_CHECKED` for routing.** It works, but it is ambient state. An explicit `ApiTarget` value threaded through `dispatch` would make `is_remote()`, `caller_pane()` and `restart_guidance()` pure functions of an argument.

## Smells and possible bugs noticed along the way

- **Misleading error text:** `ssh_bridge_exit_error` labels any non-zero exit that has stderr as "remote SSH connection failed", including the remote script's own `exit 78` (stale metadata) and remote command failures. Because classification is textual, wording like this is load-bearing.
- **Wrong hint:** `print_remote_error_hint` tells saved-machine users to load keys "before running `shepr --remote`".
- **Non-random ID:** `ProfileId::generate` is SHA-256 of pid, time and a counter, not randomness. That is fine for uniqueness, but it is not the "opaque" the tests call it.
- **Race in `machine add`:** it runs `add_ssh` twice (validate, prepare, reload, add), so the profile saved has a different ID from the one validated. That is harmless, but a label added concurrently in between is not rechecked for ambiguity.
- **`ClientEndpointStatus::Disabled`:** I did not see it set anywhere in the files I read. It may be dead; worth checking in `registry.rs`.
