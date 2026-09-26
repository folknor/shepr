Scope: config, persistence, CLI and platform. I read these files in full: `src/main.rs`, `src/config.rs`, `src/config/io.rs`, `src/session.rs`, `src/persist.rs`, `src/persist/io.rs`, `src/persist/lock.rs`, `src/cli.rs`, `src/cli/target.rs`, `src/cli/status.rs`, `src/pathutil.rs` and `src/server/socket_paths.rs`. I read part of `src/config/model.rs` (1-400), `src/platform/mod.rs` (1-1436) and `src/logging.rs` (1-150). I did not open `persist/{restore,snapshot,writer}.rs`, `cli/{spec,pane,agent,...}.rs`, `config/{keybinds,theme,sidebar,...}.rs`, `platform/shutdown.rs`, `build_info.rs` or `test_support.rs`, so nothing below speaks for them. I made one shell call, a grep, which failed; after that I only read files. Nothing was edited.

## Bugs and surprises (outside the three questions)

1. **An empty or relative `HOME` / `XDG_*_HOME` produces relative data paths.** `config/io.rs` `config_dir()`/`state_dir()` take `XDG_CONFIG_HOME`/`XDG_STATE_HOME` whenever they are set, even if empty or relative. The XDG spec says to ignore both. `platform_config_dir()` does the same with `HOME`: an empty `HOME` gives `.config/shepr` relative to the cwd, and the socket and session files go there. `pathutil::home_dir()` exists precisely to reject an empty `HOME`, and `config/io.rs` does not use it. `platform::remote_ssh_config_paths()` also reads `HOME` directly. That is three answers to "where is home". The config dir falls back to `temp_dir()` when `HOME` is unset, which puts a shared-tmp session dir and socket in play.
2. **`session::stop_socket_with_timeout` builds `{"method":"server.stop"}` with `json!` by hand.** It does this on purpose, to skip the protocol check. The method name is still a string literal outside `api::schema::Method`, and the test copies the same literal. If `Method` is renamed, nothing catches it.
3. **`logging::is_routine_api_method` string-matches API method names** (`"pane.get"`, `"pane.report_agent"`, ...). This is a classification of `Method` that lives in the logging module. A method that is renamed or added falls out of it silently. It should be a `Method::is_routine()` next to the enum.
4. **`KeysConfigOverlay.clear_pane`** is the only field missing `skip_serializing_if`. That looks like an accident: it either emits an explicit value or relies on toml dropping `None`. Either way it breaks the "only what the user chose" pattern of the other fields.
5. **`session::configure` mutates process env** (`set_var` / `remove_var` of `SHEPR_SESSION`) together with a global `AtomicBool`. That is how "which session am I" gets passed to the rest of the process: `active_name()` re-reads the env and re-validates on every call. See T1.
6. **`delete_session` and `list_sessions` repeat the `config_dir().join("sessions")` path construction** instead of calling `data_dir_for`. Same answer, spelled three times.
7. **`persist::io::load_history`** uses `path.exists()` and then reads, while `load` handles `NotFound` from the read. The difference is minor, but it is also inconsistent: `load` claims the lock and `load_history` does not. It only happens to work because `load` runs first.

## 1. Axes that should be types

- **T1: the session identity is `Option<&str>` / `Option<String>`, with `None` meaning "default".** It passes through `data_dir_for`, `api_socket_path_for`, `client_socket_path_for`, `stop_session`, `session_info` and `SessionInfo.name`. Validation happens in `validate_name` / `normalize_name` / `parse_target_name` and is re-applied in `active_name()` and `list_sessions`. What to do:
  - Introduce `enum SessionId { Default, Named(SessionName) }`, where `SessionName` can only be built through validation.
  - Resolve it once, in `main`, together with the socket override (T2), into a `SessionTarget` value that is passed down.
  - Delete the env mutation and `EXPLICIT_SESSION_REQUESTED`. `SHEPR_SESSION` should be written only into child env.
  - `delete_session(&str)` re-validates today; with the type it cannot receive a bad name.
- **T2: which socket this process talks to is decided from ambient state.** The inputs are an explicit flag, `SHEPR_SOCKET_PATH`, `SHEPR_CLIENT_SOCKET_PATH` and the session. Both `session::active_api_socket_path()` and `server::socket_paths::client_socket_path()` apply the same precedence independently (see D1). What to do:
  - Model it as `enum ServerAddress { Session(SessionId), Override { api: PathBuf, client: PathBuf } }` with `api_socket()`, `client_socket()`, `data_dir()`, `stop_command()` and `attach_command()`.
  - `restart_after_update_guidance`, `local_stop_command` and `local_attach_command` then become methods instead of re-deriving from env.
- **T3: config diagnostics are `Vec<String>`, and callers classify them by substring.**
  - `is_keybinding_config_diagnostic` looks for `"keybinding"` / `"keys."` and excludes the `"config parse error:"` / `"config read error:"` prefixes.
  - `config_diagnostic_summary` looks for `"using defaults"` and `"unknown config key "`.
  - `collect_diagnostics` post-edits a message with `.replace("using cyan", ...)`.
  - What to do: `struct ConfigDiagnostic { key: ConfigKeyPath, kind: DiagnosticKind /* ParseError, ReadError, UnknownKey, UnknownSection, InvalidValue{fallback} */, message }`. The summary and keybinding filters then match on `kind`/`key`. As it stands, a validator that rewords a message silently changes the startup banner and the keybinding filter.
- **T4: which config fields the user set are recorded as strings.** `ui.user_fields: BTreeSet<String>` with `is_user_configured("sidebar_width")`, and `keys.user_fields: BTreeSet<&'static str>`. A typo in a field-name literal compiles and returns false. A better shape is `Option<T>` for each overridable field, with the defaulting done in an accessor, or at least a field enum. Note that `ui_user_fields` re-parses the whole document a second time to get this.
- **T5: the machine allow-list is decided by strings.** `validate_machine_command(command: &str, …)` matches on command and subcommand strings (`"agent"` / `"attach"` / `"explain"` + `--file`). Dispatch in `cli.rs` matches the same strings again, and the clap spec in `spec.rs` owns the names. Whether a command is API-backed belongs on the spec: a clap `Command` extension/tag, or a typed `enum CliCommand` produced by parsing, with `fn locality(&self) -> Local | Api`. AGENTS.md states this rule in prose; today it is enforced by a hand-written string match that a new subcommand bypasses. A new API subcommand fails closed, which is safe but surprising. A new local subcommand under `workspace`/`tab`/`pane` passes, because those groups are allowed wholesale.
- **T6: `Invocation` exposes `command_name() -> Option<&str>`.** `main` then string-matches `"remote-api-bridge"`, `"remote-client-bridge"`, `"server"` and `"client"`, and `bridge_args()` round-trips flags back into `Vec<String>` so the bridge runners can re-parse them. Parse once into `enum Launch { Tui{remote,..}, Server, Client, ApiBridge{check}, ClientBridge{idle_timeout}, Cli(Command) }`. `CommandOutcome::NotCli` and the two-pass `cli::run` → `main` match then disappear.
- **T7: CLI failures to machine callers are ad hoc JSON with string codes.** Examples are `print_session_error("session_stop_failed", ...)` and `server_not_running` / `protocol_mismatch` carried as markers inside `io::Error` and recovered in `main::finish_cli` via `was_reported` downcasts. Use one `enum CliError { Usage(String), ServerNotRunning(ErrorResponse), ProtocolMismatch(..), Session(SessionError), Transport(io::Error) }` with one exit-code mapping and one printer. The downcast-a-marker-out-of-`io::Error` pattern is a type system routed through `io::Error::other`.
- **T8: `session.rs` returns `Result<_, String>` for every failure.** That includes "not running", "timed out with sockets still reachable" (paths only in prose), "name mismatch on a case-insensitive FS" and "is running, stop first". The CLI cannot branch on any of these. Replace with a `SessionError` enum.
- **T9: `headless_size() -> (u16, u16)`** is a bare tuple of cols and rows, and zero is validated by a separate diagnostic function (see D5). Use a `GridSize` of non-zero values. Likewise `validated_sidebar_bounds(min, max) -> Option<(u16, u16)>` should return a `SidebarBounds` type that the clamp sites consume.
- **T10: `SessionInfo` stores `socket_path: String` and `session_dir: String`**, already formatted with `display()`. The domain type is serialized directly to CLI JSON. Keep `PathBuf` in the domain type and format at the edge.

## 2. Decisions made in more than one place

- **D1: which socket this process targets, and how the client socket derives from it.**
  - `session::active_api_socket_path` applies: explicit session, else `SHEPR_SOCKET_PATH`, else session.
  - `server::socket_paths::client_socket_path` applies the same order, plus the legacy `SHEPR_CLIENT_SOCKET_PATH`.
  - `session::active_restart_after_update_guidance` makes the same explicit-vs-override decision a third time.
  - `session::stop_active_server` combines one path from each of those modules.
  - `api::socket_path()`, used in `cli/status.rs` and `cli/target.rs`, is presumably a fourth; I did not verify it.
  - The client-socket naming disagrees on edge cases. `client_socket_path_for` hard-codes `shepr-client.sock`, while `derive_client_socket_from_api_socket` produces `{stem}-client.sock`. They agree only because the API socket is named `shepr.sock`.
  - Owner: the `ServerAddress` from T2, resolved once.
- **D2: is a server alive?**
  - `session::is_running_at` answers `path.exists() && connect().is_ok()`.
  - `cli::server_not_running_error` answers `ErrorKind::NotFound | ConnectionRefused` on the error.
  - `cli/status.rs` goes through a status probe plus that classifier.
  - `server_not_running` / `map_server_not_running_or_io` classify again.
  - `ipc::prepare_socket_path` (stale versus live) is another answer; I did not read it.
  - They already disagree. A socket file that exists but whose connect fails with `PermissionDenied` counts as not running in `session list` and `session delete`. The CLI treats the same error as a transport error rather than "not running".
  - Owner: one `ipc::probe(path) -> Liveness { Absent, Stale, Live, Unreachable(io::Error) }`.
- **D3: is the server compatible, and does it need a restart?**
  - `cli/status.rs` computes `protocol == PROTOCOL_VERSION` three times: `compatibility_label`, the `compatible` field and `restart_needed_bool`.
  - `cli/protocol_guard::mismatch_response` decides it again.
  - `server_binary_stale_bool` compares version strings.
  - A `protocol::Compatibility::of(status)` should own this.
- **D4: which command groups exist and which are local.**
  - The clap spec, the `dispatch` string match, `validate_machine_command`, `COMMON_COMMANDS` in `main.rs` (held in step by a test) and `print_help`'s hand-written usage lines all answer it.
  - `print_help` is not tested: it lists `shepr session attach`, `machine` and similar lines that nothing verifies.
  - The fix is the typed `Launch`/`CliCommand` from T5 and T6, with help generated from the spec.
- **D5: whether the headless size is valid.** `headless_size()` calls `invalid_headless_size_diagnostic().is_some()` to decide the fallback, so validity and fallback share one predicate here, which is good. Sidebar bounds, however, are decided by `validated_sidebar_bounds` at config time and presumably again at each clamp site. The general pattern is that validators run on every accessor call (`keybinds()` re-runs `validated_keybinds()` on each call; the `io.rs` comment admits this). Validate once at load into a `ValidatedConfig`, since config never reloads.
- **D6: the list of config keys and their defaults** has five copies:
  - `Config` and its structs (the serde source);
  - `KNOWN_TOP_LEVEL_CONFIG_KEYS` (hand list; drifts when a section is added);
  - `KeysConfig` and `KeysConfigOverlay` (a full parallel struct of every binding);
  - the `DEFAULT_CONFIG` text in `main.rs` (a test checks keybindings only; no other section is covered);
  - the defaults in `Default` impls.

  `KNOWN_TOP_LEVEL_CONFIG_KEYS` is unnecessary: `serde_ignored` already reports unknown top-level keys. The only reason it exists is the section-versus-key wording. The overlay should be generated by a macro, or `KeysConfig` should hold `Option<BindingConfig>` directly. `DEFAULT_CONFIG` should live in `config/`, next to what it documents, not in `main.rs`.
- **D7: whether this process owns the data dir.** The lock is claimed in `persist::io::load`, and per the comments also in `SessionWriter`. So ownership is established lazily by the first reader or writer. `load_history` does not claim it. The right place is once at server start, producing an owned `DataDirLease` value that is the only way to get a writable session path. That makes "write without the lock" impossible to construct, instead of relying on the global `HELD` list with its idempotent re-claim.
- **D8: validation of session names in directory listings.** `list_sessions` filters with `validate_name` and excludes `"default"`. `exact_session_dir_for_delete` does its own exact-match scan. `active_name` re-validates. These are one answer (`SessionName::parse`) applied three times; with T1 they collapse.

## 3. Structure

- **`session.rs` does four unrelated jobs.**
  - Session identity and selection: env plus global flag.
  - Path layout: data dir, sockets.
  - A client for the stop RPC: raw socket IO with deadline and timeout arithmetic.
  - User-facing command text: `stop_command_for` and the restart guidance.

  Split it three ways. Identity and layout go into a `session::Target`/`ServerAddress` module that also absorbs `server/socket_paths.rs`: the client-socket derivation belongs with the rest of the layout, and today it lives in `server/` while `session` depends on it (`session::stop_active_server` calls `crate::server::socket_paths`). That is an edge from a CLI-side module into the server layer. The stop RPC goes into `api::client` as an `ApiClient::stop_unchecked(deadline)`; it duplicates connection and timeout handling that `ApiClient` surely already has. The guidance text goes to `cli`.
- **`config_dir()` is also the data root.** `session::data_dir_for(None)` equals `config_dir()`, so sockets, `session.json`, history, locks and logs all live in `~/.config/shepr`. Config and data are different axes: sockets arguably belong in `$XDG_RUNTIME_DIR`, session files in the state dir. `state_dir()` exists and is exported, yet session data does not use it. Moving the data root is a breaking layout change, which fits "no compatibility obligations", and it would separate `config::io` from session layout entirely.
- **`config.rs` mixes validation, TOML profile publishing** (`local_keybindings_profile_toml`, `keybindings_from_profile_toml`, which is a remote-keybinding wire concern) **and diagnostic filtering.** The profile code belongs with the remote keybindings feature, or with `keybinds.rs`.
- **`platform/mod.rs` is 2182 lines.** Its section headers (`// ---- Status commands`, `// ---- Foreground job detection`, `// ---- SSH paths`, `// ---- Config file replacement`, `// ---- Remote bridge stdio`, `// ---- Local client streams`) are submodules waiting to be split out.
  - "Flat platform layer" was an explicit decision (commit f3b5436). It should mean "no per-OS tree", not "one file".
  - The foreground-job `/proc` walker is a detection-side subsystem with its own budgets. `StatusCommandGuard` belongs to the tab-bar command runner. The xattr/ACL config temporary belongs to whoever writes config files. `is_pane_shell_process_name` and `shell_quote` are domain rules, not libc plumbing.
  - Keep `platform/` as a flat directory with `proc_tree.rs`, `ssh_paths.rs`, `client_stream.rs`, `private_file.rs` and so on, and move the domain rules out.
- **`persist::io::publish_private_file` calls `platform::create_config_temporary(pending, true)`.** Persistence depends on a "config" helper, and the `private: bool` parameter hides two policies. Name it `create_private_temporary` and drop the bool, or give it two functions.
- **`main.rs` has four jobs:** a 290-line `DEFAULT_CONFIG` literal, the nested-shepr joke messages, a hand-written help printer and dispatch. Move `DEFAULT_CONFIG` to `config/default.toml` (via `include_str!`, beside the model) and generate help from clap. `main` then becomes parse → resolve target → dispatch `Launch`.
- **`cli/target.rs` routes with thread-locals.** It keeps the `--machine` target and `PROTOCOL_CHECKED` in `thread_local!` and swaps them with a Drop guard. Every `send_request` reaches for this ambient state (`is_remote()`, `api_client()`, `remote_error()`, `restart_guidance()`). Pass a `&mut CliContext { client, target, protocol_checked }` into `dispatch` and the handlers instead. The comment ("scope routing to this command") shows the state really belongs to one call.
- **Tests track the accidental structure.** Examples: `stop_wait_timeout_allows_slow_graceful_shutdown` asserts a constant equals itself, and `nested_message_strings_no_longer_repeat_shepr_prefix` tests joke strings. Pairwise-agreement tests (`help_advertises_only_commands_the_parser_accepts`, `default_config_documents_every_keybinding_with_its_default`, `live_keybinds_matches_the_separate_accessor`) mark the D4/D6 duplications: each catches only the drift it happens to exercise.

**Recommended big moves, in order of payoff:**
1. A resolved `SessionTarget`/`ServerAddress` value built once in `main`. This replaces env mutation and the global flag, and folds in `socket_paths.rs`, fixing D1, D2 and T1/T2.
2. A typed `Launch`/`CliCommand` parsed from clap, with command locality on the spec, plus `CliContext` instead of thread-locals (D4, T5–T7).
3. A `ValidatedConfig` built once at load, with typed `ConfigDiagnostic`s and `Option` fields for user-set values (T3, T4, D5, D6).
4. Split `platform/mod.rs` by the seams its own section headers already mark.
