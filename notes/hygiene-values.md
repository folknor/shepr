# Hygiene findings: values and their owners

This file collects the findings from the nine-scope hygiene hunt that answer the
hunt's first two questions: (1) values spelled at more than one site instead of
being defined once and read - environment variables and their resolution rules,
tunable constants and thresholds, ports, endpoints and addresses, filesystem
paths and roots, timeouts and limits, exit codes and status strings; and (2)
values nobody can find, change or trust - a knob defined once but where nobody
tuning the system would look, a value with no injection point, configuration read
at the moment of use rather than validated once at startup. Entries gather every
site and every hunter that reported the same value. It is a working document
assembled from nine independent readings, none of them verified by running the
build, so individual entries may be wrong; a later fix pass is expected to find
phantoms here.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGV-001 - `SHEPR_ENV`, its value and its resolution rule are defined at four sites

**Decision (partial):** piece 1 of the test-isolation work adopted from
broadarrow (one environment reader and registry in `shepr-core`, after
broadarrow's `core::env`) gives `SHEPR_ENV` one registry entry with a declared
kind, and the reader's single policy replaces `should_block_nested_for_env`'s
private exact-`"1"` rule. The asset spellings of the name are held by that
piece's test that every `SHEPR_*` literal in the shipped assets is a registry
member. Open: the assets' own value rule (`!== "1"`, `== "1"`) is not checkable
from Rust, and a flag kind would accept `true` where the assets do not.

Reported independently by the vt/pty, agent, api/cli and mux hunters.

- `src/main.rs` defines `SHEPR_ENV_VAR` and `SHEPR_ENV_VALUE` privately for the
  nested-launch refusal (the reader).
- `crates/shepr-mux/src/pane/launch.rs` defines the same two consts privately
  and writes them into panes (the writer).
- `crates/shepr-pty/src/backend.rs` spells `cmd.env("SHEPR_ENV", "1")` as a bare
  literal pair.
- The shipped hook assets restate the rule: the opencode
  `shepr-tui-session.js` / `.test.ts` compare `!== "1"`.

The resolution rule lives only in `main.rs::should_block_nested_for_env`: exact
equality with `"1"`, so `SHEPR_ENV=true` and `SHEPR_ENV=` do not block nesting.
Nothing states that, and the setter may change the value without the reader
noticing. The copies agree today.

Fix named by several hunters: one `pub const` pair in `shepr-config` beside
`SOCKET_PATH_ENV_VAR` (both `shepr-mux` and the binary already depend on it),
plus a shared predicate for the resolution rule. Enforcement: a `brokkr.toml`
text rule forbidding the literal `"SHEPR_ENV"` outside that module. The asset
copies are forced (they run inside other agents' runtimes and cannot link Rust);
what could keep them in step is the asset-grep test in HYGV-006.

## HYGV-002 - `SHEPR_BIN_PATH` is a bare literal in two crates and about twenty assets

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes `SHEPR_BIN_PATH` a registry entry, and its
asset-membership test holds the twenty asset spellings to it. Open: the two Rust
writers still spell the name bare (no text rule against `SHEPR_` literals was
decided), and the per-asset "unset means `shepr`" fallback is not checkable.

Reported by the vt/pty, agent, mux and server hunters.

- `crates/shepr-mux/src/pane/launch.rs` writes it as a bare literal, in the same
  function where it imports `SOCKET_PATH_ENV_VAR` as a constant and exports
  `SHEPR_PANE_ID_ENV_VAR`.
- `crates/shepr-server/src/app/tab_bar_status.rs` writes it as a bare literal
  too, so there are two independent writers with no shared definition.
- The qwen, letta, qodercli and other hook assets spell it, with the
  fallback rule "unset means the bare name `shepr`" restated per asset.

Same fix and enforcement as HYGV-001.

## HYGV-004 - The pane and tab-bar identity variables have no single owner

Reported by the mux and server hunters.

- `crates/shepr-server/src/app/tab_bar_status.rs` spells
  `"SHEPR_ACTIVE_WORKSPACE_ID"`, `"SHEPR_ACTIVE_TAB_ID"`, `"SHEPR_ACTIVE_PANE_ID"`
  and `"SHEPR_ACTIVE_PANE_CWD"` inline.
- `SHEPR_ROLE` is a bare literal in `crates/shepr-server/src/app/api/layouts.rs`
  and in `src/cli.rs`.

The server hunter notes the repo already has the convention this family breaks
(`shepr-config::SOCKET_PATH_ENV_VAR`, `shepr-mux`'s `SHEPR_PANE_ID_ENV_VAR`,
`shepr-remote`'s `STARTUP_CWD_ENV_VAR`).

## HYGV-005 - The `SHEPR_*` namespace has no registry, and nothing answers "what environment variables does shepr read"

**Decision (partial):** piece 1 adopts broadarrow's `core::env` shape, which is
the core/platform hunter's fix: one registry in `shepr-core` where every variable
is an entry with a declared kind, read under one policy (empty is unset,
surrounding whitespace and non-UTF-8 refuse naming the variable, flags are
exactly `1`/`0`/`true`/`false`, secrets never echoed), with a pure `resolve`
beside `read`. `std::env::var`/`var_os`/`vars`/`vars_os` and
`set_var`/`remove_var` are banned in `clippy.toml` with scoped `#[expect]`
escapes (so the `unsafe remove_var` in `bootstrap.rs` must go), `IsolatedEnv`
isolates from the registry rather than a `SHEPR_` prefix, `SHEPR_LOG` becomes an
entry, and a test holds every `SHEPR_*` literal in the shipped assets to
registry membership. Open: rendering the registry into `shepr --help`, and the
pane-inheritance question (which entries a pane may inherit, HYGP-018).

Reported by the core/platform, api/cli and mux hunters.

Distinct `SHEPR_*` names are spread across many files in several crates.
`crates/shepr-config/src/address.rs` owns two of them as constants
(`SOCKET_PATH_ENV_VAR`, `CLIENT_SOCKET_PATH_ENV_VAR`), and then:

- `crates/shepr-remote/src/remote/local_server.rs` spells
  `"SHEPR_CLIENT_SOCKET_PATH"` as a literal in the same test that uses
  `shepr_config::SOCKET_PATH_ENV_VAR` as a constant on the adjacent line.
- `crates/shepr-api/src/session.rs` bakes both names into operator guidance
  strings as literals.
- `crates/shepr-platform/src/logging.rs` owns `SHEPR_LOG` as a bare literal with
  no constant at all, and it appears in no registry, no `--help` text and no doc.
- `shepr-test-support` scrubs the namespace by the prefix `"SHEPR_"` rather than
  by a list of names.
- `crates/shepr-server/src/server/headless/bootstrap.rs` removes
  `SHEPR_STARTUP_CWD` with an `unsafe remove_var`, ad hoc.
- The api/cli hunter adds that the CLI's behaviour is changed by
  `SHEPR_CONFIG_PATH`, `SHEPR_SESSION`, `SHEPR_SOCKET_PATH`,
  `SHEPR_CLIENT_SOCKET_PATH` and `SHEPR_PANE_ID`, and `shepr --help` documents
  exactly one of the five - and documents it by spelling
  `"SHEPR_CONFIG_PATH"` as a literal rather than interpolating
  `shepr_config::CONFIG_PATH_ENV_VAR`.
- The vt/pty hunter adds that nothing lists which `SHEPR_*` variables a pane may
  inherit: `base_env` is `std::env::vars_os()` wholesale, scrubbed by a denylist
  split between `launch.rs` and `shepr-agent`.

Fix named by the core/platform hunter: since `shepr-platform` sits below
`shepr-config`, one `env` module in `shepr-core` (which depends on nothing)
holding every name as a `pub const`, read by `shepr-platform`, `shepr-config`,
the binary and `shepr-test-support`. Enforcement: a `brokkr.toml` text rule
forbidding the literal substring `SHEPR_` outside that module, with test-only
probe variables listed there too; plus a `(const, description)` slice rendered by
`print_help` and a test asserting it covers every `*_ENV_VAR` const the binary's
crates export; plus a test that every `*_ENV_VAR` constant is either scrubbed
from pane env or explicitly allowed.

## HYGV-006 - The hook environment contract is restated in about twenty shipped assets (forced duplication)

**Decision:** piece 1 (the `shepr-core` environment registry) includes exactly
the mux hunter's proposal: a test that every `SHEPR_*` literal in
`crates/shepr-agent/src/integration/assets/**` is a registry member. The
semantics stay uncheckable from Rust, as the entry already says.

Reported by the agent, mux, api/cli and vt/pty hunters.

`SHEPR_ENV` must equal `"1"`, `SHEPR_SOCKET_PATH` and `SHEPR_PANE_ID` must be
non-empty, `SHEPR_BIN_PATH` falls back to the bare name `shepr`. The producing
side is `crates/shepr-mux/src/pane/launch.rs::apply_pane_launch_env`. Every asset
under `crates/shepr-agent/src/integration/assets/` restates the whole resolution
rule field by field, in four languages.

This is the forced case: the scripts are shipped into other agents' config trees
and run inside those agents' runtimes, so they cannot link Rust constants. What
keeps them in step today: nothing.

What the hunters proposed: a test that walks
`crates/shepr-agent/src/integration/assets/**`, extracts every `SHEPR_[A-Z_]+`
literal, and asserts each is a member of the exported constant set (per the mux
hunter, this is the only mechanical answer available); and per the agent hunter, a
test that greps every asset in `INTEGRATION_SPECS` for each contract variable
name. The names are the part that can drift silently; the semantics are not
checkable from Rust.

## HYGV-007 - Three different rules for resolving `$HOME`

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes `HOME` a registry entry and bans the raw
`std::env::var_os` reads in `clippy.toml`, so the two ad-hoc `git/config.rs`
expansions and `shepr-pty`'s `home_dir` must read through the one reader, which
supplies one empty and non-UTF-8 rule. Open: which caller-side rule (the
`pathutil.rs` error or `home_dir`'s passwd-then-`/` fallback) governs the pane
cwd; the relative-path half of the policy is not the reader's.

Reported by the core/platform hunter.

`crates/shepr-core/src/pathutil.rs` is the declared owner and its doc comment
states the policy: unset, empty or relative `HOME` is an error, never a fallback,
because every caller builds a path under it and an invalid home would make that
path relative to the current directory. Two other rules exist:

- `crates/shepr-mux/src/git/config.rs::normalize_gitdir_include_pattern` and
  `::resolve_include_path` both do
  `std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest)`
  for a `~/` prefix. `expand_tilde_path` is already public and returns the right
  error. (The hunter also filed the behaviour as a live defect; that half is in
  `notes/bugs.md`. The duplication stays here.)
- `crates/shepr-pty/src/command.rs::home_dir` requires `HOME` to be absolute
  and an existing directory, then falls back to the passwd entry, then to `/`:
  a third policy with two silent fallbacks, used as the pane cwd fallback.

Enforcement: delete the ad-hoc expansions, then a `brokkr.toml` text rule
forbidding `"HOME"` as a literal outside `shepr-core/src/pathutil.rs` and
`shepr-test-support`. A `clippy.toml disallowed_methods` entry is not enough,
since the offence is the argument rather than the method.

## HYGV-008 - The XDG variable set and its empty/relative rule have several owners

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) resolves the name ownership and the forced copy:
every XDG and `SHEPR_*` name is a registry entry, `IsolatedEnv` isolates from the
registry (so `shepr-test-support` takes `shepr-core` instead of restating the
list), and the reader applies one rule for empty (unset) and non-UTF-8
(refused). Open: the relative-path column of the table - what a relative
`XDG_CONFIG_HOME` or `SHEPR_CONFIG_PATH` means stays with each owning site, as it
does in broadarrow.

Reported by the protocol/config, core/platform, mux and agent hunters.

Owners of the names:

- `crates/shepr-config/src/io.rs` reads `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
  `XDG_RUNTIME_DIR`, `HOME`, `SHEPR_CONFIG_PATH`, `SHEPR_SOCKET_PATH`,
  `SHEPR_CLIENT_SOCKET_PATH`, `SHEPR_SESSION`.
- `crates/shepr-core/src/pathutil.rs` owns the `HOME` rule (HYGV-007).
- `crates/shepr-test-support/src/lib.rs` holds an independent list
  (`XDG_BASE_DIR_VARS`, four names, plus `HOME`/`XDG_RUNTIME_DIR` and the
  `SHEPR_` prefix), and that list is what keeps tests out of the real
  `~/.config`. This copy is forced: `shepr-test-support`'s dependency allowlist
  in `brokkr.toml` is `["libc"]`, so it cannot import the names from anywhere,
  and that restriction is right.
- `crates/shepr-agent/src/integration/env.rs::absolute_xdg_home` is a third
  implementation.
- `crates/shepr-mux/src/git/config.rs::git_user_config_paths` is a fourth: it
  reads `XDG_CONFIG_HOME` directly, filters on `is_absolute()`, and falls back to
  `~/.config/git/config`. The mux hunter notes this duplication has a legitimate
  answer (these are Git's paths, not shepr's, and the file says so) but that the
  answer is incomplete - see HYGV-071.

Owners of the rule (protocol/config hunter, inside `resolve_paths_from_env` /
`platform_xdg_dir` / `socket_path_override` alone - five variables, four rules):

| variable | empty | relative | reported? |
|---|---|---|---|
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | treated as unset, falls back to `HOME` | same | silently, and provenance says `Default` |
| `XDG_RUNTIME_DIR` | hard error | hard error | yes |
| `SHEPR_SOCKET_PATH`, `SHEPR_CLIENT_SOCKET_PATH` | diagnostic | diagnostic | yes |
| `SHEPR_CONFIG_PATH` | diagnostic | joined to the cwd | n/a |
| `SHEPR_SESSION` | parsed (empty name is an error) | n/a | yes |

The one that fails open is also the one that silently sends config reads
somewhere other than where the user pointed them, which the protocol/config
hunter reads as a counterexample to the project's "any config problem fails the
launch; no fallbacks" sentence.

Enforcement offered: one `env_path(variable, policy)` helper with an explicit
policy enum used for all of them, plus a test table over the five variables
pinning the matrix; and either widening the `shepr-test-support` allowlist to
`shepr-core` so it can read one exported name set, or a test in `shepr-config`
asserting every variable it reads is in the isolation list.

## HYGV-009 - `SSH_AUTH_SOCK` is resolved by three sites with three rules, already divergent

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes `SSH_AUTH_SOCK` a registry entry read under the
one policy, so empty means unset at every reader and the `Some("")` divergence in
`shepr-api/src/server.rs` goes; the raw reads are banned in `clippy.toml`. Open:
the inline read in `start_server_inner` (no injection point) and a single owning
function in `shepr_platform::ssh_agent`.

Reported by the remote hunter (and the api/cli hunter, as an ambient read).

- `crates/shepr-remote/src/remote/ssh_agent.rs`:
  `env::var("SSH_AUTH_SOCK").ok().filter(|p| !p.is_empty())` - empty means
  absent, and absent means no registration worker at all.
- `crates/shepr-api/src/server.rs`: `env::var_os("SSH_AUTH_SOCK").map(PathBuf::from)`
  with no empty check, so an empty variable becomes `Some("")` and is handed to
  `SshAgentRegistry` as a real agent path. The api/cli hunter adds that this read
  is inline inside `start_server_inner`, so the registry cannot be constructed in
  a test without mutating the process environment, even though
  `SshAgentRegistry::new` already takes the socket as an argument.
- `crates/shepr-mux/src/pane/launch.rs` writes the name back into pane children.

Fix: `shepr_platform::ssh_agent` owns `inherited_agent_socket() -> Option<PathBuf>`
and all three call it (mux already calls `ssh_agent::pane_agent_socket`, so the
module is the natural owner). Enforcement: a `brokkr.toml` text rule forbidding
the literal `"SSH_AUTH_SOCK"` outside `shepr-platform/src/ssh_agent.rs`, the same
shape as the existing `alacritty-terminal-only-in-shepr-vt` rule.

## HYGV-011 - `SHEPR_LOG` is read at the moment of use and an invalid value degrades silently

Merged into BUG-044 (`notes/bugs.md`), which carries the full finding.

Fix: move it into the config file (or validate it during launch and pass the mode
down); `parse_process_detection_mode` is already the right shape for that. See
also HYGV-085 for the mode's missing injection point.

## HYGV-013 - `SHEPR_DEBUG_OSC_EVIDENCE` is resolved at pane creation, not at launch, and is documented nowhere

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes the variable a registry entry of flag kind, so a
value outside `1`/`0`/`true`/`false` is refused naming the variable rather than
read by `osc.rs`'s own `"1" | "true" | "yes" | "on"` list, and the registry is
where it is named. Open: the read still happens per pane; resolving it once at
launch and carrying it down is not part of the decision.

Reported by the mux hunter.

`crates/shepr-mux/src/pane/osc.rs`: `impl Default for OscDebugTracker` calls
`Self::from_env()`, and `OscDebugTracker::default()` is reached from pane core
construction, so the process environment is read once per pane rather than once at
startup. A typo (`SHEPR_DEBUG_OSC_EVIDENCEE=1`, or `=yes please`) is not a launch
failure but a silent no-op discovered hours later. The flag appears in no doc and
no `brokkr man` page, and it is the flag that puts pane content (OSC payloads)
into the log.

Fix: resolve it in the same pass as the rest of the config and carry it in the
validated config down to pane construction.

## HYGV-014 - `host_modify_other_keys_mode()` reads three environment variables at the moment of use, with three resolution rules

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) makes `TMUX`, `TERM_PROGRAM` and `WEZTERM_PANE`
registry entries read under one policy, which removes the three divergent
empty-value rules, and bans the raw reads in `clippy.toml` as the entry asks.
Open: resolving them at launch into `ClientSettings` rather than when
`setup_terminal_with_capabilities` runs.

Reported by the termio/client hunter.

`crates/shepr-termio/src/input/model.rs` splits the rule correctly
(`host_modify_other_keys_mode_for_env` is pure and tested), but the wrapper reads
`TMUX`, `TERM_PROGRAM` and `WEZTERM_PANE` when
`setup_terminal_with_capabilities` happens to run rather than at launch
resolution. The values never reach `ClientSettings`, so nothing in the client can
report what host protocol it decided on, and the decision is invisible to every
test of terminal setup.

Within that one function: `WEZTERM_PANE` uses `var_os(...).is_some()` (empty
counts as set), `TMUX` uses `var(...).is_ok()` (empty also counts), and
`TERM_PROGRAM` compares case-insensitively. Three resolution rules for three
variables, none stated.

The named model to follow is `ClientProcessRole::from_env`, which enumerates the
accepted values, treats absent as `Local`, and refuses startup on anything else
including non-UTF-8. Enforcement: resolve into `ClientSettings` at launch
alongside it, and forbid `env::var` outside the launch module by text rule (the
two crates have only four production `env::var` call sites, so the rule is cheap
today).

## HYGV-015 - `prefers_osc52_clipboard` re-reads three environment values on every call

**Decision (partial):** WSL support is removed entirely, so the `WSLInterop`
stat and the `running_inside_wsl()` combination this entry named are gone. For
the remaining SSH and VS Code reads, piece 1 (the `shepr-core` environment
registry, after broadarrow's `core::env`) adopts the entry's enforcement:
`SSH_CONNECTION`, `SSH_TTY` and `VSCODE_IPC_HOOK_CLI` become registry entries
and raw `std::env::var_os` is banned in `clippy.toml`. Open: the per-call
re-read; resolving "does this host have a local clipboard" once at startup.

Reported by the core/platform hunter.

`crates/shepr-platform/src/terminal_environment.rs` reads `SSH_CONNECTION`,
`SSH_TTY` and `VSCODE_IPC_HOOK_CLI` per call. It is called from
`crates/shepr-termio/src/host_term/title.rs` on every clipboard write. The pure
inner function exists and is testable; what is missing is the caching, and a
single startup-time resolution of "does this host have a local clipboard".

Enforcement: resolve once into a value the client carries, then a
`clippy.toml disallowed_methods` entry for `std::env::var_os` outside a
designated env module.

## HYGV-016 - `AgentIntegrationPaths::resolve()` captures the environment but the resolvers still read it directly

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) bans the resolvers' raw `std::env::var_os` in
`clippy.toml` and makes every agent directory variable a registry entry, and
broadarrow's pure `resolve(var, raw)` beside `read(var)` is the seam that lets
path tests hand values in instead of mutating the process through `IsolatedEnv`.
Open: the resolvers' signature change so `resolve()` is the crate's only read,
and `GROK_CONFIG_DIR` as a production test seam (HYGP-033).

Reported by the agent hunter.

`crates/shepr-agent/src/integration/env.rs` documents the boundary ("install and
status code receives this value and never consults the process environment") and
holds it for install and status. But every `*_dir()` function reads
`std::env::var_os` itself and is `pub(crate)`, so each one is a second
environment read behind `resolve`, and tests must manipulate real environment
variables through `IsolatedEnv` to steer paths. About twenty resolvers do this.

Fix by signature: give the resolvers an explicit environment argument (a
`&dyn Fn(&str) -> Option<OsString>` or a captured map) so the only environment
read in the crate is at `resolve()`. The agent hunter adds that this also removes
`GROK_CONFIG_DIR`, a production environment variable whose own comment says it
exists primarily as a test seam.

## HYGV-018 - The remote exit codes `255` and `254` are literals in a shell string with no decoder

Reported by the remote hunter, as fact.

`SSH_OWN_FAILURE_EXIT_CODE: i32 = 255` exists in
`crates/shepr-remote/src/remote/bridge.rs`, but
`posix_remote_output_command` writes `255` and `254` as literal text inside the
remote shell script rather than interpolating the constant. Nothing anywhere maps
`254` back: a remote `shepr` that exits 255 reaches the operator as "remote
command failed (exit status 254)", a number documented in no user-facing text.
The encode side has no decode partner.

Fix: interpolate `SSH_OWN_FAILURE_EXIT_CODE`, add
`REMAPPED_REMOTE_255_EXIT_CODE`, and have `ssh_bridge_exit_error` say "remote
command failed with 255 (reported as 254)". Enforcement: a test asserting the
generated script contains the constants and that the error text names 255; the
existing test only checks the shell behaviour, not the decode.

## HYGV-019 - Nine API error codes are minted as string literals outside `ApiErrorCode`, and two in-enum codes are re-spelled

Reported by the api/cli hunter.

`crates/shepr-api/src/error.rs`'s `ApiErrorCode` is a macro-generated single
owner of variant and wire spelling. The CLI bypasses it by hand-building
`serde_json::json!` error bodies: `agent_explain_file_read_failed`,
`agent_start_failed`, `agent_start_transport_failed`, `agent_kind_mismatch`,
`agent_name_not_found` (`src/cli/agent.rs`), `build_mismatch` (`src/cli.rs`),
`server_not_running` (`src/cli/server_not_running.rs`), and
`invalid_session_name` / `session_stop_failed` / `session_delete_failed`
(`src/cli/error.rs`). A consumer of `shepr <cmd>` JSON therefore sees codes that
do not exist in the schema, and `ApiError::from_body` classifies every one as
`External(..)`.

Already diverged: `src/cli/agent.rs` emits the literal `"timeout"` and
`agent_not_ready`, duplicating `ApiErrorCode::Timeout` and
`ApiErrorCode::AgentNotReady` with a second spelling site each.

Inside `shepr-api` itself, `server.rs` passes `"invalid_ssh_agent"` and
`"ssh_agent_unavailable"` to `error_response_json`, whose signature is
`code: impl Into<ApiErrorCode>`, so a typo silently becomes
`External("invald_ssh_agent")` with no compile error and no log, even though
`ApiErrorCode::InvalidSshAgent` and `SshAgentUnavailable` exist. A third spelling
of the timeout code appears in `api_response_outcome`, which re-parses the JSON it
just serialized in order to match `"timeout"` literally.

Enforcement: give the CLI-minted codes their own variants (or a `CliErrorCode`
enum) and make `ErrorBody` construction take the enum rather than `&str`;
restrict `error_response_json` to `ApiErrorCode` (the `From<&str>` impl is needed
for wire parsing but need not be reachable from an emit site); thread the
`ApiResult`'s outcome to `finish_api_response` instead of the encoded string,
which removes both the third literal and a full JSON parse per response. A weaker
fallback: a text rule forbidding a string literal assigned to an `ErrorBody.code`
field outside `error.rs`.

## HYGV-020 - "pane not found" is spelled about thirty times in three wordings, and one condition gets two error codes

Reported by the server hunter.

- Bare `"pane not found"` with no identifier: six sites in
  `crates/shepr-server/src/app/api/panes.rs`, fourteen in
  `app/api/panes/geometry.rs`.
- `format!("pane not found: {}", params.pane_id)`: two sites in
  `app/api/panes/copy.rs`.
- `format!("agent target pane {target} not found")` and
  `format!("agent target {target} not found")` in `app/agents.rs`.
- `"source pane not found"` / `"target pane {raw} not found"` /
  `"source tab not found"`: about ten sites in `app/api/panes/geometry.rs`.

The same pattern for workspaces: `format!("workspace {id} not found")` in
`app/creation.rs`, `app/api/tabs.rs`, `app/api/workspaces.rs`,
`app/api/layouts.rs` and `app/api/panes/geometry.rs`, versus bare
`"workspace not found"` twice in `app/api/layouts.rs`.

Divergence today, not a prediction: `app/api/layouts.rs` answers a missing
workspace with `ApiErrorCode::WorkspaceNotFound` in one place and with
`ApiErrorCode::LayoutApplyFailed` plus the message `"workspace not found"` in the
apply path, so a client switching on the code sees two classes for one fact.

Enforcement: `api_helpers::pane_not_found(&pane_id) -> ApiError` and siblings, a
single `resolve_workspace(...) -> Result<usize, ApiError>` that owns the code, a
table test over the layout handlers asserting the code per resolution failure,
and a text rule banning the bare literals.

## HYGV-021 - "Unknown presents as Idle" has four statements and two implementations

**Decision (partial):** the `--state-label STATUS=TEXT` feature is deleted end
to end (CLI flag, API params, storage in `shepr-mux` metadata, projection,
sidebar rendering), which removed this entry's `state_label_assignment` /
`normalize_state_labels` half and the vocabulary-ownership question with it.
The "Unknown presents as Idle" half remains open.

Reported by the mux hunter: the rule has four statements and two
implementations - stated in `AGENTS.md`, implemented in
`crates/shepr-agent/src/detect/mod.rs::attention_rank`, implemented again in
`crates/shepr-server/src/app/api_helpers.rs::pane_agent_status`, and documented
in `crates/shepr-mux/src/workspace/aggregate.rs` as happening "at the API edge",
which is a claim about a different crate. Consistent today.

Enforcement: one mapping function in `shepr-agent` used by both
`attention_rank` and `pane_agent_status`, with the `aggregate.rs` doc comment
deleted rather than restated.

## HYGV-022 - The session-start-source vocabulary is spelled three times and the copies disagree

Merged into BUG-018 (`notes/bugs.md`), which carries the full finding.

## HYGV-023 - The `shepr:<agent>` source string and the agent label are hard-coded in every asset

Merged into HYGG-102 (`notes/hygiene-guards.md`), which carries the full finding.

## HYGV-024 - `/etc/ssh/ssh_config` is hardcoded, in an `Option` that is never `None`

Reported by the core/platform hunter, as fact.

`crates/shepr-platform/src/ssh_paths.rs` sets
`system_config: Some(PathBuf::from("/etc/ssh/ssh_config"))`. The `Option` shape
claims a case the code cannot produce, and the sole consumer
(`crates/shepr-remote/src/remote/ssh.rs`) must handle it anyway.

Fix: make the field a `PathBuf`; the type system does the rest.

## HYGV-025 - The Unix socket path limit is restated in prose, in a test literal, and in another crate's doc comment

**Decision (partial):** piece 2 (scratch directories under the project's
`target/` tree, adopting broadarrow's `test-scratch`/`test-support` scheme)
replaces the `shepr-test-support` prose copy: broadarrow names scratch roots with
fixed-width digests and proves a socket-leaf budget at the deepest handed-out
path through the production `sun_path` check (`check_unix_socket_path`), rather
than restating the number. Decided: the `sun_path` limit and its check move
from `shepr-platform` down to `shepr-core`, so the scratch code can prove the
budget without depending on the platform crate. Also decided: the managed SSH
config writer (`crates/shepr-remote/src/remote/ssh.rs::write_managed_ssh_config`)
takes its control directory as an input rather than computing one internally,
so its tests can pass a short path instead of one that cannot fit `sun_path`
under a deep checkout. Open: the `ipc.rs` prose, the `platform/src/tests.rs`
literals and the `bridge.rs` shim.

Reported by the core/platform and remote hunters.

`crates/shepr-platform/src/ssh_paths.rs` owns `UNIX_SOCKET_PATH_MAX = 107`.
Restatements: `ipc.rs` ("without using any of `sun_path`'s 107 bytes"),
`platform/src/tests.rs` (`"x".repeat(107)`, `"x".repeat(108)`), and
`crates/shepr-test-support/src/lib.rs` ("`sun_path` (108 bytes)").

The test-support copy is a forced duplication: that crate's dependency allowlist
is `["libc"]`, so it cannot read the platform constant, and the restriction is
right. What keeps them in step: nothing. Since it is prose, the cheapest answer
is prose that cannot drift ("must fit in `sun_path`", without the number).

Related, from the remote hunter as fact: `crates/shepr-remote/src/remote/bridge.rs`
defines a private `fits_unix_socket_path` shim over
`shepr_platform::fits_unix_socket_path`, a public function from a crate
`shepr-remote` already depends on directly, used by `attach.rs` at three sites -
which makes the socket limit look like it has two owners.

## HYGV-026 - Socket basenames are single-owned but the test fixture spells them relatively

Merged into HYGG-024 (`notes/hygiene-guards.md`), which carries the full finding.

## HYGV-027 - The client state subdirectory is spelled at three sites, two ways

Reported by the remote hunter.

`crates/shepr-remote/src/machine/catalog.rs::catalog_path` builds
`state_dir/client/endpoints.json`; `::selection_path` builds
`state_dir/client/endpoint-selection.json`; `ssh_metadata.rs::new` builds
`state_dir.join("client/ssh-metadata")`, a different join style for the same
directory. Nothing owns "the client's state directory".

Fix: a `shepr_config::AppPaths::client_state_dir()` accessor, like the existing
`server_address()` / `session_id()`. Enforcement: a text rule forbidding the
literal `"client"` as a path component outside that accessor, or the accessor
plus review.

## HYGV-028 - The persisted session filenames have two owners each

Reported by the mux hunter.

`crates/shepr-mux/src/persist/io.rs` defines a private
`session_history_path(data_dir)`. `persist/writer.rs` never calls it: it derives
the path itself at three sites with
`self.path.with_file_name("session-history.json")`. So the writer and the loader
each spell the filename.

Worse for `"session-snapshots"`: five production spellings in `writer.rs`, and
one of them is load-bearing as a behavioural guard - `preserve_existing_in`
selects between "pruning failure is fatal" and "pruning failure is a warning" by
comparing `directory_name == "session-snapshots"`. Rename the directory at the
other four sites and forget this one and snapshot pruning failures silently
become warnings, with the directory growing without bound and nothing saying so.

Fix: consts in `io.rs`, `writer.rs` calling `io::` accessors, and the guard
replaced by a `PrunePolicy { Fatal, Warn }` passed alongside `keep`, which makes
the string comparison disappear entirely. Holdable by a text rule once the
literals are gone.

## HYGV-029 - The recovery-filename timestamp width is spelled twice, ninety lines apart

Reported by the mux hunter.

`crates/shepr-mux/src/persist/writer.rs` writes
`format!("session-{timestamp:039}-{}-{sequence}.json", std::process::id())` and
later checks `fields[0].len() == 39`. Change one and every existing recovery copy
becomes invisible to pruning and to `prepare_snapshot_history`, silently, because
`recovery_files` just skips names it cannot parse.

Fix: a `const RECOVERY_TIMESTAMP_DIGITS: usize = 39` used by the format string
(`{timestamp:0width$}`) and the check, plus a round-trip test.

## HYGV-030 - Two pending-file naming conventions in one module

Merged into BUG-069 (`notes/bugs.md`), which carries the full finding.

## HYGV-031 - The temp-file prefix in `store_private_json` is wrong for two of its three users

Reported by the remote hunter, as fact.

`crates/shepr-remote/src/machine/catalog.rs::store_private_json` names its
staging file `.endpoints-<pid>-<seq>.tmp` regardless of the `description` it was
given, and it is used for the endpoint catalog, the endpoint selection, and the
SSH metadata cache. A leftover `.endpoints-*.tmp` in the `ssh-metadata` directory
names the wrong subject.

Fix: derive the prefix from the destination file name. Enforcement: a test
asserting the staged name derives from the target path.

## HYGV-032 - `"shepr"` as a program name has two resolution rules, plus an independent remote-install location list

Reported by the remote hunter, as fact.

- `crates/shepr-remote/src/remote/launch.rs::run_remote` resolves the local
  program from `std::env::args().next()` with `"shepr"` as fallback.
- `machine/saved.rs::saved_ssh_bootstrap_command` hardcodes `"shepr"`
  unconditionally, and that string is printed to the operator as a command to
  run.
- `remote/discovery.rs` hardcodes `shepr` as the remote binary name in
  `command -v shepr` and in the known-locations script.

This one legitimately needs two values (local argv0 versus remote install name);
the finding is that neither is named. Fix: one `PROGRAM_NAME` constant plus one
`local_invocation_name()` helper, and a separate constant for the remote install
name with a comment saying so. Enforcement: a text rule against the bare
`"shepr"` literal outside the owning module.

Related and not mechanically enforceable: `discovery.rs::known_remote_binary_candidate_script`
hardcodes `$HOME/.cargo/bin/shepr` and `$HOME/.local/bin/shepr`, while `brokkr
install` decides where the binary actually lands. Where we install and where we
look are independent lists across the brokkr/shepr boundary. Best available: a
comment at each site naming the other, and a test that the script's paths are a
superset of `brokkr install`'s destination if brokkr exposes it.

## HYGV-033 - The profile-id truncation length is duplicated, beside a different constant of the same value

Reported by the remote hunter.

`crates/shepr-remote/src/machine/saved.rs` slices
`&profile_id.as_str()[..16]` in `saved_bridge_path` and again in
`SavedSshApiBridge::start` for the short socket name, while `profile_id.rs` owns
`PROFILE_ID_BYTES = 16` - a different 16 (bytes, not hex chars), so the two are
easy to confuse. Both slicing sites also panic on a shorter id; `parse`
guarantees 32 today, so the invariant is maintained by a constructor and relied
on by slicing two modules away.

Fix: `ProfileId::short() -> &str` on the type that owns the invariant, which
removes the indexing and the panic together.

## HYGV-034 - `BRIDGE_SOCKET_PERMISSION_MODE` duplicates `PRIVATE_SOCKET_MODE`, and a test uses it for an unrelated file

Reported by the remote hunter, as fact.

`crates/shepr-remote/src/remote/bridge.rs` defines
`BRIDGE_SOCKET_PERMISSION_MODE: u32 = 0o600`;
`crates/shepr-platform/src/ipc.rs` defines a private
`PRIVATE_SOCKET_MODE: u32 = 0o600`. Worse, the `attach.rs` test
`managed_ssh_config_includes_user_config_then_fallback` asserts the ssh config
file's mode against `BRIDGE_SOCKET_PERMISSION_MODE` with the message "keepalive
config must be user-only", so two unrelated policies now share one constant and
changing the bridge socket mode breaks an ssh-config test. The bridge's extra
`restrict_socket_permissions(0o600)` is itself redundant:
`bind_private_local_listener` already ends owner-only.

Fix: export `PRIVATE_SOCKET_MODE` from `shepr-platform`, delete the bridge copy,
and have the config test assert `0o600` or a `PRIVATE_FILE_MODE` owned by the
private-file module.

## HYGV-035 - `/bin/sh` and the shell-resolution rules are spelled across several sites

Reported by the vt/pty and server hunters.

- `crates/shepr-pty/src/command.rs` spells `/bin/sh` five times across two
  fallback chains: `passwd_shell` falls back to `/bin/sh`, then `resolve_shell`
  falls back to `/bin/sh` again.
- The shell-env trim rule is applied twice in the same crate: `interactive_shell`
  trims `default_shell`, then `trimmed_shell` trims `$SHELL` again.
- `crates/shepr-server/src/app/tab_bar_status.rs` runs every tab-bar status
  command as `Command::new("/bin/sh")` with `args(["-lc", &command])`. That is
  not configurable, not derived from `terminal.default_shell`, not from `$SHELL`,
  and `-l` is a behavioural choice: each status command pays a full login-shell
  startup every interval and picks up the user's rc files. The choice is
  invisible from config and from the config documentation. The server hunter
  files this under question 2 - a knob nobody tuning the system would find.

Fix: one const for the fallback shell; for the tab bar, a named constant or a
config key, which moves it into HYGV-036.

## HYGV-036 - Nothing answers "what are this crate's tunables", in any crate

**Decision (partial):** per-crate `limits` modules are adopted from broadarrow,
incrementally as part of the hygiene work rather than wholesale: each crate's
constants move into its `limits` module (the `shepr-protocol` model below), and
the crate is then held by scoped textlints in the shape of broadarrow's
`numeric-consts-live-in-limits` and `duration-and-capacity-literals-live-in-limits`
(B8 in `notes/broadarrow-ports.md`). That settles the enforcement the hunters
converge on, with the name `limits`. Open: every crate, one at a time, and which
knobs should become config keys.

Every hunter reported this independently, for their own scope. The shape is
always the same: each constant is individually defined once, which is why none
reads as a finding on its own, and the finding is that a person tuning any
subsystem has many files to read and no index. There is no `reference/` or
`docs/` folder in the repository, so nothing enumerates any of these sets.

The one counterexample all hunters cite: `crates/shepr-protocol/src/limits.rs`.
Every consumer across `shepr-client`, `shepr-server` and `shepr-termio` reads the
constant, there are no magic restatements anywhere, and derived values
(`MAX_SURFACE_CELLS`, `MAX_TERMINAL_FRAME_BYTES`) are computed from their bases.
That is the model.

The inventories, by scope:

- **shepr-platform** (fifteen tunables in eleven files):
  `CLIPBOARD_HELPER_TIMEOUT` (2 s), `STARTUP_WAIT` (100 ms, wl-copy), two
  separate `POLL_INTERVAL`s (5 ms each), `MAX_CLIPBOARD_TEXT_BYTES` (1 MiB),
  `DEFAULT_MAX_LOG_BYTES` (5 MiB), `DEFAULT_RETAINED_LOG_FILES` (1),
  `LOG_FILE_MODE`, `PRIVATE_SOCKET_MODE`, `STAGING_ATTEMPTS`,
  `UNIX_SOCKET_PATH_MAX`, `IDLE_TIMEOUT` (60 s), `PROBE_INTERVAL` (1 s, ssh
  agent), `START_TIME_EXIT_RECHECK` (10 ms), the logind backoff ceiling (60 s),
  and an unnamed 100 ms in `wait_client_stream_readable`.
- **shepr-vt / shepr-pty**: `ACTOR_IDLE_POLL_MS`, the inbox byte and item caps
  and the resize retry constants at the top of `actor.rs`; `MAX_DRAIN_CHUNKS =
  1024` inside `handle_write_failure`; the read buffer `8192` and wake-drain
  buffer `64`; `MAX_OSC_BYTES` and the other scanner limits in `scan.rs`; the
  scrollback floor and cap and `MAX_CLIPBOARD_BYTES` (192 KiB) in vt `lib.rs`;
  `SYNCHRONIZED_OUTPUT_FLUSH_MARGIN` and `DEFAULT_DETECTION_ROWS` in mux
  `terminal.rs`; `MIN_PANE_ROWS`/`MIN_PANE_COLS` and the detection task's
  initial 50 ms sleep in `runtime.rs`.
- **shepr-agent**: detection limits (`MAX_RULES_PER_MANIFEST`, `MAX_GATE_DEPTH`,
  `MAX_TOTAL_GATES`, `MAX_MATCHERS_PER_GATE`, `MAX_REGIONS_PER_MANIFEST`,
  `MAX_TOTAL_MATCHERS`, `MAX_MATCHER_CHARS`) in `manifest.rs`; probe budgets
  (`CHILD_GROUPS_SCAN_LIMIT`, `FOREGROUND_TREE_SCAN_LIMIT`,
  `FOREGROUND_TASK_ENTRY_LIMIT`, `FOREGROUND_CHILD_BYTE_LIMIT`,
  `FOREGROUND_CHILD_PID_LIMIT`) in `proc_tree.rs`; version-probe budgets
  (`VERSION_PROBE_TIMEOUT`, `VERSION_PROBE_POLL_INTERVAL`,
  `MAX_VERSION_PROBE_OUTPUT`) in `version.rs`; session-ref caps
  (`MAX_SESSION_ID_LEN`, `MAX_SESSION_PATH_LEN`) in `resume.rs`; hook timeouts in
  `integration/mod.rs`; retry pacing (`SHEPR_OMP_IDLE_DEBOUNCE_MS`,
  `SHEPR_OMP_RETRY_GRACE_MS`) only inside the OMP TypeScript asset; a 12-line
  lookback and a 32-char needle cap in `contains_recent_non_whitespace`; and
  `128` temp-name attempts in both `file_ops.rs` and `config_file.rs`.
- **shepr-config**: `lib.rs` holds four (`DEFAULT_SCROLLBACK_LIMIT_BYTES`,
  `DEFAULT_MOUSE_SCROLL_LINES`, `DEFAULT_HEADLESS_COLS`,
  `DEFAULT_HEADLESS_ROWS`); the rest are wherever first needed:
  `MAX_SESSION_NAME_LEN = 64`, `MAX_SIDEBAR_ROWS = 16`,
  `MAX_SIDEBAR_TOKENS_PER_ROW = 16`, `DEFAULT_SIDEBAR_ROW_GAP = 0`,
  `MAX_TAB_BAR_RIGHT_ENTRIES = 16`,
  `MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS = 31_536_000`,
  `MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS = 3_600`,
  `DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS = 5`,
  `DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS = 2`, `KEY_BINDING_COUNT = 51`, plus
  roughly forty values buried in `Default` impls in `model.rs`. Several caps (the
  one-year interval, `MAX_SESSION_NAME_LEN`, the sidebar 16s) are documented
  nowhere a user would look, including `default.toml`.
- **shepr-api and the CLI**: `APP_RESPONSE_TIMEOUT` 5 s,
  `ORDINARY_REQUEST_TIMEOUT` 60 s, `INITIAL_REQUEST_TIMEOUT` 5 s,
  `STREAM_WRITE_TIMEOUT` 5 s, `CONNECTION_POLL_INTERVAL` 100 ms,
  `MAX_INITIAL_REQUEST_BYTES` 1 MiB, `ACCEPT_BACKOFF_MIN`/`MAX` (all in
  `shepr-api/src/server.rs`); `EventHub::MAX_EVENTS` 512;
  `ORDINARY_RESPONSE_TIMEOUT` and `WAIT_RESPONSE_GRACE` 30 s (`client.rs`);
  `AGENT_PROMPT_EFFECT_TIMEOUT_MS` 5 s and `AGENT_PROMPT_RESPONSE_GRACE` 1 s
  (`wait.rs`); `STOP_WAIT_TIMEOUT` 15 s, `STOP_WAIT_POLL` 25 ms,
  `MIN_SOCKET_TIMEOUT` 1 ms (`session.rs`); `SERVER_READY_TIMEOUT` 15 s
  (`src/autodetect.rs`); an inline `Duration::from_secs(15)` in
  `src/cli/target.rs`; `AGENT_START_POLL_INTERVAL` 100 ms,
  `PANE_SHELL_READINESS_RETRY_TIMEOUT` 2 s and
  `DEFAULT_AGENT_START_TIMEOUT_MS` 30 s (`src/cli/agent.rs`). The hunter names
  `ORDINARY_REQUEST_TIMEOUT` and `WAIT_RESPONSE_GRACE` as the two with real
  documentation and the standard the rest should meet.
- **shepr-remote** (seven files): `NONINTERACTIVE_SSH_COMMAND_TIMEOUT`;
  `PIPE_DRAIN_GRACE`, `POLL_INTERVAL`, `SSH_STDOUT_CAPTURE_LIMIT`,
  `SSH_STDERR_CAPTURE_LIMIT`; `BRIDGE_ACCEPT_POLL`, `BRIDGE_IO_POLL`,
  `BRIDGE_FAILURE_REPORT_TIMEOUT`, `BRIDGE_FAILURE_REPORT_POLL_INTERVAL`,
  `BRIDGE_SOCKET_PERMISSION_MODE`; `REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT`,
  `REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL`; `SOCKET_POLL_INTERVAL`,
  `STATUS_REQUEST_TIMEOUT`; `CATALOG_POLL_INTERVAL`, `MAX_CATALOG_BYTES`,
  `MAX_PROFILES`, `MAX_LABEL_BYTES`; `MAX_METADATA_BYTES`;
  `MAX_SSH_TARGET_BYTES`; `MAX_REMOTE_EXECUTABLE_BYTES`; plus an unnamed
  `Duration::from_millis(250)` in `bridge_connection`'s wait loop and
  100 ms / 500 ms / 10 ms inside `ssh_agent.rs`.
- **shepr-mux** (thirty-plus across seven files): the nine acquisition-timing
  knobs in `pane/process_probe.rs` (`RELEASE_REACQUIRE_SUPPRESSION`,
  `AGENT_MISS_CONFIRMATION_ATTEMPTS`, `PROCESS_RECHECK_IDENTIFIED`,
  `PROCESS_RECHECK_MISSING_FOREGROUND_GROUP`, `PROCESS_ACQUISITION_WINDOW`,
  `PROCESS_ACQUISITION_FAST_WINDOW`, `PROCESS_ACQUISITION_FAST_RECHECK`,
  `PROCESS_ACQUISITION_SLOW_RECHECK`, `PROCESS_ACQUISITION_IDLE_RESET`); six in
  `pane/agent_detection.rs` including `AGENT_ABSENCE_STARTUP_HOLD` aliased to
  `MANAGED_AGENT_RESUME_TIMEOUT`; `MIN_ALIGNMENT_RATIO_PERCENT = 30` and
  `SIMILAR_VIEWPORT_RATIO_PERCENT = 70` in `terminal/history_read.rs`, two
  similarity thresholds with no stated basis; `MAX_METADATA_SOURCES = 64`,
  `MAX_STATE_LABELS_PER_SOURCE = 16`, `MAX_SEQUENCE_SOURCES = 32`;
  `MAX_BODY_BYTES = 4096`, `AGENT_OSC_MAX_CHARS = 256` and a second
  `MAX_CHARS = 512` inside `sanitized_osc_debug_payload`;
  `DEFAULT_DETECTION_ROWS`, `SYNCHRONIZED_OUTPUT_FLUSH_MARGIN`,
  `SCAN_CHUNK_ROWS`, `COPY_MODE_WORD_SEPARATORS`; `SNAPSHOT_INTERVAL` (15 min)
  and `SNAPSHOT_LIMIT` (48, twelve hours of recovery, a number nobody wrote down)
  next to a bare inline `3` for the other recovery directory's limit;
  `GIT_STATUS_RETRY_DELAY` (30 s); `MAX_GIT_REF_FILE_BYTES`;
  `HOOK_SEQUENCE_REANCHOR_AFTER`; and unnamed bounds `for _ in 0..16` (symlink
  hops), `for sequence in 0..128` (recovery name attempts), `.take(256)`
  (palette).
- **shepr-server** (about forty): `MIN_RENDER_INTERVAL` 16 ms; git refresh
  1500 ms, 5 min, and a 30 s retry buried in a test fixture; session save
  debounce 5 s and backoff 250 ms to 30 s over 3 failures; agent start 30 s
  default / 300 s max / 3 s settle; agent resume retry 1 s and the managed-resume
  timeout; agent prompt submit delay 300 ms; alt-screen read quiet/step/max
  windows (10/10/120 ms, 15 s, 5 s, 3 wheel events); handshake timeout 4 s;
  shutdown flush timeout 1 s; pane teardown wait 3 s; shell cwd refresh 1 s;
  datetime refresh 1 s; read line cap 1000; seven metadata TTL and token caps in
  `api_helpers.rs`; layout pane and depth caps 24/16; copy query and match caps
  4096/1024; status text caps 4096/80; input batch cap 4096; handshake frame cap
  64 KiB; endpoint byte caps 1 MiB/128/128. `[advanced]` in the config model
  holds exactly one key (`scrollback_limit_bytes`). A person tuning alt-screen
  reads has no way to discover that `INITIAL_QUIET`, `OUTPUT_QUIET`,
  `STEP_TIMEOUT`, `MAX_DURATION`, `MAX_RESTORE_DURATION` and `WHEEL_STEP_EVENTS`
  are the six dials.
- **shepr-termio and shepr-client** (at least thirty in fourteen files):
  `HOST_KEYBOARD_QUERY_TIMEOUT`, `MAX_BUFFERED_HOST_INPUT`;
  `INITIAL_RETRY_DELAY`, `MAX_RETRY_DELAY`, `STABLE_CONNECTION_PERIOD`,
  `ATTENTION_RETRY_DELAY`, `ATTEMPT_BUDGET`; `MAX_QUEUED_BATCHES`,
  `MAX_BATCH_BYTES`, `MAX_QUEUED_BYTES`, `WRITE_TIMEOUT`, `IO_POLL_INTERVAL`;
  `ENDPOINT_COMMAND_TIMEOUT`, `MAX_RETIRED_REQUESTS_PER_ENDPOINT`,
  `MAX_ENDPOINT_RESPONSE_BYTES`; `HEARTBEAT_INTERVAL`, `HEARTBEAT_TIMEOUT`;
  `ACTIVATION_TIMEOUT`; `LOCAL_HANDSHAKE_READ_TIMEOUT`,
  `REMOTE_HANDSHAKE_READ_TIMEOUT`; `SELECTION_AUTOSCROLL_INTERVAL`,
  `SELECTION_REPAINT_INTERVAL`, `MODAL_PASTE_CLIPBOARD_TIMEOUT`;
  `MIN_TAB_WIDTH`, `NEW_TAB_WIDTH`, `WORKSPACE_HEADER_ROWS`;
  `TAB_SCROLL_BUTTON_WIDTH`, `MIN_TAB_STRIP_WIDTH`; `DEFAULT_CELL_WIDTH_PX`,
  `DEFAULT_CELL_HEIGHT_PX`; `MAX_PENDING_PASTE_BYTES`, `PASTE_STALL_TIMEOUT`,
  `RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS`,
  `MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS`,
  `MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES`, `MAX_DISCARDED_CONTROL_TAIL_BYTES`;
  `MAX_NOTICES` declared inside a function body in `lib.rs`, so it is invisible
  to anyone auditing the crate's limits; plus the 100 ms resize-poll sleep in
  `terminal_geometry.rs::resize_poll_loop` and the unnamed inline durations in
  HYGV-046.

Enforcement the hunters converge on: one `tunables.rs` (or `limits.rs` plus
`timing.rs`) per crate as the file of record, held by a `brokkr.toml` text rule
that `Duration::from_*`, `const MAX_*` and octal mode literals may appear only
there and in tests. The vt/pty hunter adds that a text rule forbidding numeric
`const` inside function bodies is feasible. Whether any individual knob should
become a config key is a judgement the code cannot reveal.

## HYGV-037 - The bridge idle timeout and the client heartbeat interval are coupled across crates with nothing linking them

Reported by the core/platform and remote hunters.

`shepr_platform::remote_bridge::IDLE_TIMEOUT` (60 s) must exceed
`shepr_client::endpoint::health::HEARTBEAT_INTERVAL` (5 s) or a healthy idle
remote bridge is torn down under a live client. Neither constant mentions the
other, and neither crate can see the other (`shepr-platform` is below
`shepr-client`); `shepr-remote` sits between them and passes only
`idle_timeout: bool` through. The `remote_bridge.rs` module doc states the
relationship in prose ("The client endpoint sends HealthPing after five seconds
without received data ... Those protocol frames renew this byte-level watchdog"),
which is exactly the claim nothing checks: halve the timeout or double the ping
interval and healthy idle bridges start dying.

Fix: both constants in `shepr-core` (which both crates may depend on) with the
relation stated at the definition. Enforcement: a test in whichever crate can see
both asserting `IDLE_TIMEOUT >= HEARTBEAT_INTERVAL * k`.

## HYGV-038 - Four independent fifteen-second SSH budgets, and the two `wait_for_server_socket` callers disagree in the wrong direction

**Decision (partial):** the two inline `Duration::from_secs` literals
(`src/cli/target.rs`'s 15 and `host.rs`'s 5) are what the duration-literal
textlint of the per-crate `limits` modules forbids, adopted incrementally with
the hygiene work (HYGV-036), so they get names when their crate's turn comes.
Open: the shared owner for the 15-second budget, the `wait_for_server_socket`
parameter, and deriving `ATTEMPT_BUDGET` from the values it cites.

Reported by the remote hunter, as fact, with the api/cli hunter's timeout table
naming two of the same values.

- `crates/shepr-remote/src/remote/ssh.rs`:
  `NONINTERACTIVE_SSH_COMMAND_TIMEOUT` is 15 s for every noninteractive command.
- `src/cli/target.rs::server_status` uses an inline `Duration::from_secs(15)` for
  the remote API probe. Same physical quantity ("one cold SSH round trip"), two
  unnamed literals in two crates.
- `src/autodetect.rs` uses `SERVER_READY_TIMEOUT = 15 s` for the local server;
  `crates/shepr-remote/src/remote/host.rs` passes an inline
  `Duration::from_secs(5)` for the server on the remote host reached over SSH, so
  the slower case gets the shorter budget and the 5 is not even named.
- `crates/shepr-client/src/endpoint/supervisor.rs` reasons at length in prose
  about "15 seconds" per discovery command, "the handshake 60", and
  "ControlMaster, persisting ten minutes" (`ControlPersist=600` in `ssh.rs`), and
  derives `ATTEMPT_BUDGET = 25 s` from them. None of those numbers is read; all
  are restated. Changing `NONINTERACTIVE_SSH_COMMAND_TIMEOUT` to 30 s silently
  invalidates the 25 s budget and the documented argument for it, and no test
  fails.

Fix: `local_server::SERVER_READY_TIMEOUT` owned next to
`wait_for_server_socket`, with the parameter removed unless a caller has a stated
reason to differ (removal makes divergence unrepresentable); export
`NONINTERACTIVE_SSH_COMMAND_TIMEOUT` and the handshake timeout and define
`ATTEMPT_BUDGET` in terms of them. Enforcement: a test asserting
`ATTEMPT_BUDGET >= NONINTERACTIVE_SSH_COMMAND_TIMEOUT + slack` and
`ATTEMPT_BUDGET < MAX_RETRY_DELAY` - the second half already exists in
`supervisor.rs`, so the pattern is known there and only half applied.

## HYGV-039 - SSH option values are inline literals, and the keepalive settings are spelled twice in two syntaxes

Reported by the remote hunter, as fact (agreeing today).

`crates/shepr-remote/src/remote/ssh.rs::apply_noninteractive_ssh_options` emits
`-o ServerAliveInterval=15 -o ServerAliveCountMax=4`;
`::write_managed_ssh_config` writes `ServerAliveInterval 15` /
`ServerAliveCountMax 4` into the config text. Same tunable, two syntaxes, both
literal, and a third copy of the values sits in the `attach.rs` test assertions.

In the same file and also unnamed: `ConnectTimeout=10`, `ConnectionAttempts=1`,
`NumberOfPasswordPrompts=0` and `=3`, `StrictHostKeyChecking=yes`,
`ControlPersist=600`, `ControlMaster=auto`. `StrictHostKeyChecking=yes` appears
in both `apply_noninteractive_ssh_options` and
`authentication_command_with_config`: a security-relevant value with two sites.
`ControlPersist=600` is additionally cited in prose in
`crates/shepr-client/src/endpoint/supervisor.rs` (see HYGV-038).

Fix: a `struct SshKeepalive { interval_secs, count_max }` that renders itself as
`-o` args or as config lines, and one `ssh_options` module with named constants
and one builder. Not enforceable while the values are inline string literals.

## HYGV-040 - The API and CLI timing values have no injection point, so the CLI's most intricate loop is untested

Reported by the api/cli hunter.

`stop_session_with_timeout` is the model to copy: the timeout is a parameter and
the public `stop_session` supplies `STOP_WAIT_TIMEOUT`, so its tests run in
75 ms. Nothing else in scope does this.

- `SERVER_READY_TIMEOUT` (15 s) is baked into `auto_detect_launch`, which
  otherwise takes an injected `run_client` closure - the function was designed
  for testability and then hardcoded the one value a test would need.
- `AGENT_START_POLL_INTERVAL`, `PANE_SHELL_READINESS_RETRY_TIMEOUT` and
  `DEFAULT_AGENT_START_TIMEOUT_MS` are module-private constants in
  `src/cli/agent.rs`, which is why `agent_start`'s retry loop - the most
  intricate control flow in the CLI, with a pinned-terminal check, a busy-retry
  deadline and a five-branch readiness decision - has no unit tests at all. Only
  its argument parsing is tested.

Fix by signature: take the timing as a parameter (a small `AgentStartTiming`
struct) and the loop becomes testable without a server. The hunter calls this the
single highest-value structural change in the CLI half of that scope.

Related, and recorded by the same hunter as a coupling that deserves the comment
it does not have: `src/cli/agent.rs` reads
`shepr_server::app::AGENT_START_SETTLE_DELAY` and
`shepr_server::app::MAX_AGENT_START_TIMEOUT` at CLI-argument-validation time, so
the binary couples its retry policy to the server's internals and a `--machine`
invocation compares the local build's constants against a remote server's
behaviour. It is defensible because client and server are always the same build,
which is what the `build_mismatch` check polices. Enforcement available: the root
`shepr` package has no `dependency_rule` in `brokkr.toml` while every library
crate does, so nothing constrains what the binary reaches into; adding a rule for
the root package would make this coupling a deliberate allowlist entry.

## HYGV-041 - Unnamed durations and capacities in the server, beside named siblings

**Decision:** per-crate `limits` modules are adopted incrementally as part of
the hygiene work (HYGV-036); when `shepr-server` gets its module, these three
become named constants there and the duration-and-capacity textlint holds them.

Reported by the server hunter.

- `crates/shepr-server/src/app/session.rs` writes
  `Some(now + Duration::from_millis(250))` at two sites for the "save still
  running, check back" interval. It coincidentally equals
  `SESSION_SAVE_RETRY_MIN` in the same file but is a different knob (poll
  cadence, not backoff floor), so neither reuse nor a shared constant is right:
  it needs its own name.
- A third unnamed literal in the same file, `.min(Duration::from_secs(1))`, caps
  the host-shutdown checkpoint backoff, sitting next to the two named
  `SESSION_SAVE_RETRY_*` constants.
- `crates/shepr-server/src/server/headless.rs` creates the server event channel
  with `mpsc::channel(64)`, next to `APP_EVENT_CHANNEL_CAPACITY = 256` and
  `APP_EVENT_DRAIN_LIMIT = 64`, which are named. Same class of tunable, two
  conventions.

Naming them removes the sites; see HYGV-036 for where they should live.

## HYGV-042 - One megabyte is the cap on "one client request" in three unrelated places

**Decision (partial):** per-crate `limits` modules are adopted incrementally with
the hygiene work (HYGV-036), which gives each of the three constants a findable
home. Open: whether the three are one knob owned by `shepr-protocol::limits`, as
the enforcement below proposes, or three knobs in three crates' modules.

Reported by the server hunter.

- `shepr_protocol::MAX_INPUT_PAYLOAD = 1024 * 1024`.
- `MAX_ENDPOINT_COMMAND_BYTES = 1024 * 1024` in
  `crates/shepr-server/src/server/client_commands.rs`.
- `MAX_INITIAL_REQUEST_BYTES = 1024 * 1024` in `crates/shepr-api/src/server.rs`.

Three independent spellings of one magnitude for three doors into the same
process. None cites the others, and all three crates depend on `shepr-protocol`,
which already owns `MAX_FRAME_SIZE` and `MAX_INPUT_PAYLOAD`, so no boundary
forces the copies.

Enforcement: move all three into `shepr-protocol::limits` and add a text rule
forbidding `1024 * 1024` outside that module.

## HYGV-043 - The clipboard byte caps have two unrelated owners, and one is restated as a magic number in its own test

**Decision (partial):** per-crate `limits` modules are adopted incrementally with
the hygiene work (HYGV-036); a numeric `const` declared inside a function body is
a violation of the `numeric-consts-live-in-limits` textlint, so
`MAX_CLIPBOARD_TEXT_BYTES` leaves the function when `shepr-platform` gets its
module. Open: the test's magic number and one owner for the pair.

Reported by the vt/pty and core/platform hunters.

`shepr-vt`'s `MAX_CLIPBOARD_BYTES` silently drops OSC 52 payloads over 192 KiB,
with no log. `crates/shepr-platform/src/clipboard.rs` uses a separate 1 MiB cap
for reads. The two caps have unrelated owners for one user-visible behaviour.

The platform cap is additionally declared inside a function body
(`const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024`) while its test asserts
the limit with `yes x | head -c 1048578`. Change the constant and the test still
passes while testing nothing in particular.

Enforcement: hoist the const to module scope and have the test compute
`MAX_CLIPBOARD_TEXT_BYTES + 2`; decide one owner for the pair, or state why two.

## HYGV-044 - Retry counts and poll intervals are invented per site inside one crate

**Decision (partial):** the enforcement named below is adopted, incrementally as
part of the hygiene work: `shepr-platform` gets a `limits` module held by the
duration-and-capacity textlint (HYGV-036). Open: collapsing the duplicated
values (the two `POLL_INTERVAL`s, 4 versus 16 attempts, the two copy buffers)
when they move.

Reported by the core/platform hunter as a question 1 finding (the hunters also
filed the broader "retry policy per call site" pattern under per-call-site
policy, which is a sibling document; what is here is the duplicated values).

Within `shepr-platform` alone: `STAGING_ATTEMPTS = 4` in `ipc.rs` versus a bare
`for _ in 0..16` in `ssh_paths.rs` for the same "random name collided, try again"
policy; `POLL_INTERVAL = 5ms` declared twice in `clipboard.rs`;
`START_TIME_EXIT_RECHECK = 10ms` in `process.rs`; a hardcoded `100` ms in
`client_stream.rs::wait_client_stream_readable`; and a `16 * 1024` copy buffer in
`remote_bridge_io.rs` against `8192` in `child_io.rs::read_limited_reader`.

Enforcement: the per-crate tunables module of HYGV-036 plus a text rule
forbidding bare integer millisecond literals in `Duration::from_millis` outside
it.

## HYGV-045 - The pane teardown budget and the server's wait for it are unrelated numbers in different crates

**Decision (partial):** per-crate `limits` modules are adopted incrementally with
the hygiene work (HYGV-036), so the three `250` ms spellings and the server's
`3` s wait become named constants in their crates' modules. Open: deriving the
server's wait from an exported teardown budget and asserting the relation.

Reported by the mux hunter.

`crates/shepr-mux/src/pane/teardown.rs` spells `Duration::from_millis(250)` three
times inside `PANE_TEARDOWN_STEPS` (one value, three spellings) and the 750 ms
total is nowhere named. `crates/shepr-server/src/server/headless.rs` waits
`Duration::from_secs(3)` for those teardowns to finish. The 3 s must exceed the
750 ms plus however long two `/proc` session scans take; that relationship is
stated nowhere and the two numbers cannot see each other.

Fix: export the total from `teardown.rs` as `pub const PANE_TEARDOWN_BUDGET` and
have the server derive its wait from it, with a compile-time or test assertion
that the wait is the larger.

## HYGV-046 - Client-side throttles and timeouts are unnamed, duplicated, or typed differently from their siblings

**Decision (partial):** per-crate `limits` modules are adopted incrementally with
the hygiene work (HYGV-036); the inline `Duration::from_millis(350)`, `(33)`,
`(100)`, `Duration::from_secs(1)` and `(5)` become named constants in
`shepr-client`'s module, and the two `_SECS: u64` constants move there as
`Duration`s. The `attach.rs` deadline read also falls under the clock seam
(HYGP-001). Open: the `Throttle` type and the `finish_client` teardown.

Reported by the termio/client hunter.

- The 350 ms double-click window is written inline as
  `Duration::from_millis(350)` in both
  `crates/shepr-client/src/shell/state.rs::ClientPaneClick::is_double_click_for`
  and `shell/input/mouse.rs` (sidebar-divider double click). One user-facing
  gesture, two unnamed copies, different files.
- `shell/input/mouse.rs` names `SELECTION_AUTOSCROLL_INTERVAL` (30 ms) and
  `SELECTION_REPAINT_INTERVAL` (16 ms) at the top of the file, then writes
  `Duration::from_millis(33)` inline twice for the scrollbar-drag and split-drag
  send throttle. Three throttles for the same class of thing at 16/30/33 ms, two
  findable and one not. The hunter's proposed `Throttle { interval, last }` type
  makes the interval a named construction argument.
- `crates/shepr-client/src/lib.rs` ends `run_client_with_mode` through two paths,
  each writing the same three teardown lines
  (`rt.shutdown_timeout(Duration::from_millis(100))`,
  `shepr_remote::release_ssh_resources_before_exit(Duration::from_secs(1))`,
  `shepr_platform::logging::shutdown("client")`), with both timeouts unnamed and
  the string `"client"` passed to `logging::startup` and `logging::shutdown` at
  three sites. A `finish_client(rt)` function, or a guard type whose `Drop` runs
  it, makes the omission unrepresentable.
- `shell/state.rs` declares `ENDPOINT_ERROR_TIMEOUT_SECS: u64 = 5` and
  `ENDPOINT_NOTICE_TIMEOUT_SECS: u64 = 10`, each wrapped in
  `Duration::from_secs(...)` at the use site, while every sibling of the same
  class is a `const ...: Duration`. They share values with
  `HEARTBEAT_INTERVAL` (5 s) and `HEARTBEAT_TIMEOUT` (10 s), a coincidence a
  reader has to check.
- `attach.rs` writes `Instant::now() + Duration::from_secs(5)` inline for a flush
  budget that happens to equal `WRITE_TIMEOUT`.

## HYGV-047 - Interlocks between tunables exist only in prose, including one user-visible promise

Reported by the termio/client hunter, with the remote hunter's R10 as the same
shape across crates (see HYGV-038).

`crates/shepr-client/src/endpoint/supervisor.rs`'s doc comment for
`MAX_RETRY_DELAY` states that `shepr machine reconnect` tells the user open
clients retry within 30 seconds, and that `ATTEMPT_BUDGET` (25 s) plus the retry
accounting keep that promise. Three constants in that file, a fourth in
`shepr-remote` (the fifteen-second per-command discovery budget the comment
cites), and the CLI's user-facing wording all have to agree, and nothing in the
build would notice any of them drifting. The hunter's point is that this is a
careful, correct comment about an unenforced invariant.

Enforcement, cheap and absent: `const _: () = assert!(...)` for
`ATTEMPT_BUDGET < MAX_RETRY_DELAY`, `HEARTBEAT_INTERVAL < HEARTBEAT_TIMEOUT` and
`IO_POLL_INTERVAL < WRITE_TIMEOUT`, plus a test asserting the CLI's reconnect
message quotes `MAX_RETRY_DELAY` rather than a literal `30`.

## HYGV-048 - The split-ratio bounds and defaults are magic numbers at five sites, and the policy has two public names

Reported by the core/platform and termio/client hunters.

`crates/shepr-core/src/layout.rs` exports both `SplitRatio::clamped(f32)` and the
free function `valid_split_ratio(f32) -> SplitRatio`, whose entire body is
`SplitRatio::clamped(ratio)`. Both are used externally (`clamped` from about
thirty-six sites, `valid_split_ratio` from
`crates/shepr-mux/src/persist/restore.rs` and three internal layout sites), so a
future change to the policy has two doors. Fix: delete `valid_split_ratio`; the
compiler enforces the rest.

The bounds themselves are unnamed at five sites in the same file: the
`(0.1..=0.9)` range check, the `clamp(0.1, 0.9)`, the NaN default `0.5`,
`split_focused`'s hardcoded `0.5`, and the missing-ratio default `0.5`. A test
re-spells `0.1`, `0.9` and `0.05` again. Nothing names "minimum pane share" or
"even split".

The same shape recurs in the client:
`crates/shepr-client/src/shell/sidebar/sidebar_tokens.rs`'s `SectionSplit` has
`DEFAULT: Self(0.5)`, `new()` validating `(0.1..=0.9)`, `from_drag()` clamping to
`0.1, 0.9` with a second literal `0.5` for a non-finite value, and a
`Deserialize` error message restating the range in prose - five copies of four
numbers.

Enforcement: `const MIN_SPLIT_RATIO` / `MAX_SPLIT_RATIO` / `EVEN_SPLIT` (and
`MIN`/`MAX` on `SectionSplit`) referenced by every check, clamp, fallback and
error message, plus a test asserting `SplitRatio::clamped(MIN - eps).get() == MIN`
so the const and the clamp stay in step.

## HYGV-049 - `SplitRatio`'s validating constructor exists only in test builds

Reported by the core/platform hunter.

`crates/shepr-core/src/layout.rs` gates `fn new(value: f32) -> Option<Self>`
behind `#[cfg(test)]`. Production has only `clamped`, which never refuses, so the
test `split_ratio_rejects_values_outside_layout_bounds` exercises a constructor
no shipped code path can call, and the restore path in
`crates/shepr-mux/src/persist/restore.rs` silently clamps a corrupt saved ratio
rather than refusing the layout. Given "no wire compatibility obligations" and
"validated once at launch", the hunter reads a stored ratio outside `0.1..=0.9`
as a refusal of the session file rather than a clamp.

Fix: make `new` non-test and have the restore path return `InvalidSavedLayout`,
which `from_saved` already models.

The mux hunter's related observation: `parse_snapshot` is `pub` and returns a
`SessionSnapshot` whose types encode none of the validation
(`ratio: f32`, `active: Option<usize>`, `selected: usize`, `active_tab: usize`,
`focused: Option<u32>`, `root_pane: Option<u32>`, `cwd: PathBuf`), and two
consumers already parse it without going through restore. Carrying
`shepr_core::layout::SplitRatio` and validated cwd and index types in the
snapshot struct itself would make restore's sanitizing unnecessary.

## HYGV-050 - Minimum grid size is clamped at three layers with three different minimums

Reported by the vt/pty hunter.

`shepr-vt` clamps to 2 columns and 1 row, `shepr-mux` to 4 columns and 2 rows,
and `shepr-core`'s `GridSize::clamped` to 1 and 1. Three owners of "how small may
a terminal be", none referencing the others.

## HYGV-051 - Pixel geometry (`cols * cell_width`) is computed four times with three overflow policies

Reported by the vt/pty hunter.

| site | arithmetic |
|---|---|
| `crates/shepr-pty/src/fd.rs::resize_pty_fd` (TIOCSWINSZ) | clamped to `u16` |
| `shepr-vt`'s `handler.rs::in_band_size_report` and `text_area_pixels_report` | `u64` |
| `Terminal::width_px` / `height_px` | saturating `u32` |

For a pane over 65535 px the child's TIOCGWINSZ and its `CSI 14 t` answer already
disagree. Fix: a `PaneGeometry::text_area_px()` in `shepr-core` that every site
calls.

## HYGV-052 - DEC mode numbers have three owners

Reported by the vt/pty hunter.

- `pub const MODE_*` in `crates/shepr-vt/src/lib.rs`.
- Literal numbers in the `MODES` table in `shepr-vt`'s `modes.rs`: 1004, 1005,
  1006, 1007, 1016, 2004 and 2031 are literals even though constants exist.
- Private `MODE_MOUSE_X10` / `PRESS_RELEASE` / `BUTTON_MOTION` / `ANY_MOTION`
  (9, 1000, 1002, 1003) in `crates/shepr-mux/src/pane/terminal.rs`.

The mux also ORs those four modes itself (in `backend.rs` and `wheel_routing`)
although `Terminal::mouse_tracking_enabled()` already computes the same thing and
is used two methods earlier in the same file.

Fix by type: a `DecMode` enum with `number()`, used for both the table and the
API, and `mode_get(DecMode)` instead of `u16`.

## HYGV-053 - The halfwidth katakana voiced-mark rule has three owners

Reported by the vt/pty hunter.

- `crates/shepr-vt/src/cell.rs::is_halfwidth_voiced_mark`.
- `crates/shepr-mux/src/pane/terminal/helpers.rs`:
  `is_halfwidth_katakana_voiced_mark` and `..._grapheme`.
- A verbatim copy of `is_halfwidth_katakana_voiced_grapheme` in
  `crates/shepr-termio/src/blit.rs`.

The mux copy exists because `ghostty_buffer_symbol_into` measures with raw
`unicode_width` instead of `shepr_vt::unicode_text_width` and then patches around
the difference. Fix: export the predicate from `shepr-vt`. Enforcement: a text
rule banning the U+FF9E / U+FF9F spelling outside `shepr-vt` is feasible.

## HYGV-054 - The kitty placeholder filter is spelled five times, and four of the copies cannot fire

Reported by the vt/pty hunter.

`shepr-vt`'s `cell_text` already classifies U+10EEEE as `Empty`, so the mux
checks are unreachable: `helpers.rs::ghostty_cell_symbol`,
`helpers.rs::ghostty_buffer_symbol_into`, `pane/terminal/text.rs` and
`terminal/history_read.rs`.

Fix: stop making `KITTY_UNICODE_PLACEHOLDER` public, which removes the four
downstream spellings.

## HYGV-055 - Underline flags, named-colour thresholds and one string terminator are duplicated inside `shepr-vt`

Reported by the vt/pty hunter.

The underline ladder appears in both `cell.rs::cell_style` and
`format.rs::push_sgr`. The "named index >= 16 means default" rule appears in both
`cell_color` and `push_color`. `"\x1b]8;;\x1b\\"` appears twice in `format.rs`.

Fix: one `UnderlineStyle::from_flags` and one SGR table.

## HYGV-056 - `ScreenTextRow` carries the wrap fields flat beside a type that already holds them

Reported by the vt/pty hunter.

`ScreenTextRow` carries `soft_wrapped` and `wrap_continuation` as flat fields
next to a `RowWrap` type with the same two fields; the comment admits it. Fix by
type: embed `RowWrap`.

## HYGV-057 - The seqlock protocol on `content_seq` is hand-copied at six sites, and the sync epoch bump at four

Merged into HYGP-020 (`notes/hygiene-policy.md`), which carries the full finding.

## HYGV-058 - `PANE_TERM` has one owner but `PANE_COLORTERM` lives in another crate, and a test re-spells both

**Decision (partial):** `pane_terminal_identity_overrides_outer_terminal_env`
no longer runs `printf` through the host shell and now reads `PANE_TERM` and the
colorterm constant directly, so the re-spelling half is resolved. Open:
`PANE_COLORTERM`'s owner and XTGETTCAP's independent `Tc`/`RGB` claim.

Reported by the vt/pty hunter.

`PANE_TERM` is single-owned (good), while its sibling
`PANE_COLORTERM = "truecolor"` lives in `shepr-mux`, and XTGETTCAP in
`shepr-vt`'s `scan.rs` advertises `Tc` and `RGB` independently of it. The
terminal-identity claims should sit together in `shepr-vt`.

## HYGV-059 - The modifyOtherKeys level has two enums and the client recovers the number by sniffing a byte string

Reported by the termio/client hunter.

`shepr_termio::input::model::ModifyOtherKeysMode` (`Mode1` / `Mode2`) emits
`b"\x1b[>4;1m"` / `b"\x1b[>4;2m"` from `set_sequence()`.
`shepr_vt::ModifyOtherKeysLevel` is a second enum over the same concept, and
`host_term::modes::set_direct_host_keyboard_protocol` writes
`\x1b[>4;{level}m` from it. `terminal_setup::setup_terminal_with_capabilities`
bridges them with
`let parameter = if mode.set_sequence().ends_with(b";1m") { 1 } else { 2 };`.

So the mode-to-parameter mapping is owned three times (the termio enum's byte
literal, the vt enum's rendering, and this string sniff), and `set_sequence()`'s
only remaining consumer is the sniff - the bytes it builds are never written. A
`Mode3`, or a spelling change in `set_sequence`, silently yields `2`.

Fix by type: have the detector return `shepr_vt::ModifyOtherKeysLevel` directly,
delete `ModifyOtherKeysMode`, and the `;1m` / `;2m` literals disappear. The
hunter notes this copy is not forced: `shepr-vt` sits below `shepr-termio` in the
documented layering and `shepr-termio` already depends on it.

## HYGV-060 - Host-terminal control sequences are owned partly by `shepr-termio` and partly by the client

Reported by the termio/client hunter.

`crates/shepr-termio/src/host_term/modes.rs` is the stated owner, but
`crates/shepr-client/src/terminal_setup.rs` writes its own raw copies:
`\x1b[>4;0m` and `\x1b[<1u` in `HostModes::restore` (both also spelled in
`modes.rs`), `\x1b[?1016h` in `set_mouse_capture_with_writer` (whose disable
counterpart is `modes.rs`'s `DISABLE_HOST_MOUSE_REPORTING_SEQUENCE`),
`\x1b[?25h\x1b[0 q` in the restore postlude, and `PUSH_WINDOW_TITLE` /
`POP_WINDOW_TITLE` (`\x1b[22;0t` / `\x1b[23;0t`) while OSC 0 itself lives in
`host_term::title`. `HOST_CELL_SIZE_QUERY = b"\x1b[16t"` is defined in
`terminal_geometry.rs` while the module doc for `host_term::cell_size` names the
same sequence in prose and `shepr-vt`'s `scan.rs` parses the reply. Enable and
disable of one mode therefore sit in different crates.

The hunter records explicitly that nothing forces this: both halves could live in
`shepr-termio::host_term::modes`. Enforcement: a brokkr-style text rule ("no
`\x1b[` byte literal outside `shepr-termio/src/host_term/` and `shepr-vt`"),
comparable to the existing gremlin scan, with test assertions excluded the way
the gremlin rule already does.

Related, from the same hunter: `DISABLE_HOST_MOUSE_REPORTING_SEQUENCE` lists
eight modes and its test `clears_all_known_host_mouse_modes` loops over the same
eight spelled again as strings, so adding a ninth to the constant and not the
test passes. Deriving both from a single `const MODES: [&str; N]` makes the pair
structural - and the `\x1b[?1016h` enable in the other crate is the copy the test
cannot see at all.

## HYGV-061 - The kitty keyboard flag bits have five spellings, two of them raw

Reported by the termio/client hunter.

`crates/shepr-termio/src/input/model.rs` exports
`pub const KITTY_FLAG_REPORT_ALL_KEYS` (not re-exported from `input/mod.rs`),
while `encode.rs` declares four private `KITTY_FLAG_*` constants over the same
bitflags type, including its own `KITTY_FLAG_DISAMBIGUATE` far from the other
three. `KeyboardProtocol::reports_event_types` writes the bit as a raw literal
`0b0000_0010` while `reports_all_keys` uses the named constant: two spellings in
adjacent methods of one impl.

Fix: delete all five constants and call `KittyKeyboardFlags::contains`, which
makes the raw literal unrepresentable.

## HYGV-062 - The synchronized-output clock has no injection point

**Decision (partial):** the clock seam is adopted incrementally as part of the
hygiene work (HYGP-001 in `notes/hygiene-policy.md`), and the fix below is this
subsystem's instance of it. Open: the shepr-owned `Timeout` impl.

Reported by the vt/pty hunter.

`shepr-vt` uses vte's default `StdSyncHandler` rather than its generic
`Processor<T: Timeout>`, and `flush_expired_synchronized_output` reads
`Instant::now()` itself. Consequently
`crates/shepr-vt/src/tests.rs::synchronized_output_buffers_until_end_or_timeout`
sleeps through vte's 150 ms timeout, and the same test asserts
`!flush_expired...` immediately after a write, which will flake under load.

Fix by type: a shepr-owned `Timeout` impl driven by an injected clock. The hunter
names this as one of two structural suggestions for that scope, because it also
gives the render and timer paths one `now`.

## HYGV-063 - The keybinding action list is spelled at eight sites; three of the eight are compiler-checked

**Decision:** either keys are rebindable or they are not, and they are. The six
hard-coded help entries (`esc`, `tab / shift+tab`, `enter`, `1..9`) become
ordinary configurable bindings with defaults. Do this together with the single
declarative keybinding table so the new keys are not added to eight sites by
hand.

Reported by the protocol/config hunter, who calls it the largest single finding
in that scope.

| site | checked against anything? |
|---|---|
| `crates/shepr-config/src/model.rs` `KeysConfig`, 51 `BindingConfig` fields | source of truth |
| `model.rs` `impl Default for KeysConfig` | yes, struct literal |
| `keybinds.rs` `Keybinds` + `NavigateKeybinds` structs | no |
| `keybinds.rs` `Keybinds { ... empty_action!() ... }` literal | yes, against `Keybinds` |
| `keybinds.rs` the ~51 `apply_action!` / `apply_indexed!` / `apply_navigate!` lines | no |
| `wire.rs` `key_binding_fields!` macro list | yes, `take_bindings!` builds `KeysConfig` |
| `default.toml` comment block | partly, see HYGV-065 |
| `crates/shepr-termio/src/input/keybindings.rs` dispatch and `keybind_help.rs` entries | no |

The uncovered one that bites: add a field to `KeysConfig` and forget its
`apply_action!` line and it compiles, the user's binding parses, and the action
never fires with no diagnostic. Same for a missing `keybindings.rs` dispatch row
or `keybind_help.rs` entry.

The termio/client hunter reached the same list from the other end:
`keybind_help::keybind_help_groups` enumerates `keybinds.<field>` by hand for all
47 `Keybinds` fields plus `NavigateKeybinds`' 6, and every field does appear
today, so it is a true claim with nothing holding it. That function also
hard-codes six entries that come from nowhere (`entry("esc", "back")`,
`entry("tab / shift+tab", "cycle pane")`, `entry("enter", "open workspace")`,
`entry("1..9", "switch workspace")`); if any is rebindable the help is lying, and
if none is, they are undocumented fixed keys the config cannot reach.

Enforcement, and both hunters recommend paying for it: collapse the list into one
declarative table naming each action once with its kind (action / indexed /
navigate), its default binding and its help text, generating `KeysConfig`, its
`Default`, `Keybinds`, the apply loop, the wire mapping and the help entries, so
every omission is a compile error. Cheaper intermediate step named by the
termio/client hunter: destructure `Keybinds` exhaustively with no `..` at the top
of `keybind_help_groups`, so adding a field fails to compile until it is placed.

## HYGV-064 - `KEY_BINDING_COUNT = 51` is a hand-maintained count of a compile-time-known list

Reported by the protocol/config hunter.

In `crates/shepr-config/src/wire.rs`. Correct today (verified: 51
`BindingConfig` fields). The macro list it guards is already tied to
`KeysConfig` by the compiler, so the constant's only job is a runtime
`Err("resolved config has N keybindings; expected 51")` that cannot trigger.

Enforcement: derive it from the macro (`[$(stringify!($field)),*].len()`), or
delete it and the runtime check with it. The hunter adds that `into_config`'s
`Result<Config, String>` error type exists for this case that cannot occur.

## HYGV-065 - Every default appears twice, once in a `Default` impl and once as a `default.toml` comment, and only the keybindings are checked

Reported by the protocol/config hunter.

`src/main.rs::default_config_documents_every_keybinding_with_its_default` covers
`[keys]` only, and only string-valued entries (it `continue`s past anything that
is not a TOML string). Unchecked: `sidebar_width = 26`, `sidebar_min_width = 18`,
`sidebar_max_width = 36`, `mouse_scroll_lines = 3`, `headless_cols = 120`,
`headless_rows = 40`, `scrollback_limit_bytes = 10000000`,
`startup_per_agent_delay_ms = 100`, `row_gap = 0`,
`window_title = "{hostname}: {workspace}"`, every enum default, and the whole
`[theme]` and `[remote]` blocks.

`default.toml` also restates several lists the code owns: the theme list
(diverged, HYGV-066), the `cjk_ime_agents` accepted-name list (22 agent names,
hand-written, generated by `ConfigAgent::all()`, unchecked), the sidebar built-in
token lists (unchecked) and the `right_click_passthrough_modifier` alias list
(unchecked, HYGV-067).

Enforcement: generalise the existing test - serialise `Config::default()` to a
TOML table, walk every leaf, assert each appears as `# key = <value>` in
`DEFAULT_CONFIG`. The mechanism already exists in `main.rs` and is merely scoped
to one table. The hunter also notes `default.toml` ships exactly one active
(uncommented) setting, `pane_history = false` under `[experimental]`, almost
certainly an editing slip, holdable by a test that every non-blank,
non-`[section]` line starts with `#`.

## HYGV-066 - The built-in theme list has diverged, and theme names are spelled three times in code

Reported by the protocol/config hunter, as fact.

`crates/shepr-config/src/theme.rs`'s `THEME_NAMES` holds 18 themes;
`default.toml`'s comment lists 11. Every light variant
(`catppuccin-latte`, `tokyo-night-day`, `gruvbox-light`, `one-light`,
`solarized-light`, `kanagawa-lotus`, `rose-pine-dawn`) is implemented, accepted
by `canonical_theme_name`, named in the error message for an unknown theme, and
absent from the printed default config.

In code the names are spelled three times (`THEME_NAMES`,
`canonical_theme_name`'s match, `Palette::from_name`'s match) plus 18 constructor
functions. `built_in_theme_names_resolve` covers
`THEME_NAMES -> canonical -> from_name` in one direction only, so a palette
implemented but missing from `THEME_NAMES` is undetected - which is exactly the
failure mode that produced the divergence.

The default theme name `"catppuccin"` is a bare literal in
`theme_config.rs::resolve_palette`, restated in `default.toml`'s comment and in
`THEME_NAMES[0]`; a `DEFAULT_THEME` const referenced from all three fixes that.

Enforcement: assert every `THEME_NAMES` entry appears in `DEFAULT_CONFIG` (same
shape as the keybinding test), and one table used in both directions for the
name-to-palette mapping.

## HYGV-067 - `right_click_passthrough_modifier`'s accepted-value set is spelled five times, and one copy is narrower

Reported by the protocol/config hunter.

The parser in `crates/shepr-config/src/model.rs`; the hand-written error-message
constant `RIGHT_CLICK_PASSTHROUGH_MODIFIER_VALUES`, which restates the alias list
by hand; the `Serialize` impl, which emits a narrower set
(`off` / `ctrl` / `alt` / `ctrl+alt`); `WireRightClickModifier` in `wire.rs`; and
`default.toml`'s comment.

Enforcement: a table of `(&str alias, Option<KeyModifiers>)` that the parser, the
serialiser and the error message all read, with a round-trip test over the table
as the whole check.

## HYGV-068 - What an empty string means is invented per config key, and one default is a sentinel that is never applied

Reported by the protocol/config hunter.

Five keys, four rules: `window_title = ""` means "leave the title alone";
`right_click_passthrough_modifier = ""` means "disabled";
`terminal.default_shell = ""` means "$SHELL, then /bin/sh";
`terminal.new_cwd = ""` is a documented error; `ui.accent = ""` is a hard launch
failure (`parse_configured_color` on a non-optional `String`) even though
`default.toml` says "Unset uses the theme accent", so a user who reads that and
writes `accent = ""` gets a refused launch.

`ui.accent`'s `Default` is `"cyan"`, which is never applied: `resolve_palette`
only uses `ui.accent` when `provenance.is_explicit(Accent)`. So the Rust default
is a sentinel whose only live requirement is that it parses as a colour, and
`default.toml` documents a different value (`#89b4fa`) with a different meaning.

Not mechanically enforceable. The fix is a type: `Option<NonEmpty<String>>` or a
small `ConfigOverride<T>` that spells "unset" once, at which point the rule is in
one place and the sentinel becomes unrepresentable.

## HYGV-069 - A bare `16` beside two named twins, twice on adjacent lines

Reported by the protocol/config hunter.

`crates/shepr-config/src/sidebar.rs::RawSidebarToken::parts` has
`if token.rules.len() > 16 { return Err("sidebar tokens may contain at most 16
rules") }` - the number appears twice on adjacent lines, in a file that already
has two named `= 16` constants for neighbouring limits. Fix: a named const plus
`{MAX}` interpolation in the message, after which drift is impossible.

## HYGV-070 - Server-local aliases for protocol constants read as independent knobs

Reported by the protocol/config hunter.

`crates/shepr-server/src/server/client_transport.rs` defines
`MAX_CLIENT_SHELL_DIMENSION`, `MAX_CLIENT_SHELL_CELLS` and
`MAX_CLIENT_CELL_SIZE_PX`, and `client_commands.rs` defines
`ENDPOINT_RESPONSE_CHUNK_BYTES`, each a direct `= shepr_protocol::MAX_*`. They
are correct, but someone tuning one will edit the alias and find it does nothing
independent. Fix: delete the aliases and use the protocol constants directly.

## HYGV-071 - The Git configuration rule has four owners, and two of the parsers already disagree

Merged into BUG-063 (`notes/bugs.md`) and BUG-064 (`notes/bugs.md`), which carry the full finding.

## HYGV-072 - Three boolean-from-string parsers, no owner, and one is an incomplete implementation of an external grammar

**Decision (partial):** the `env_bool()` half is piece 1 (the `shepr-core`
environment registry, after broadarrow's `core::env`): shepr env flags have one
kind and one rule, exactly `1`/`0`/`true`/`false`, so `osc.rs`'s
`"1" | "true" | "yes" | "on"` goes (and `yes`/`on` become refusals). Open: the
`git_bool()` half against Git's grammar.

Reported by the mux hunter.

- `crates/shepr-mux/src/git/config.rs`: `"true" | "1" | "yes" | "on"` (Git's
  boolean syntax).
- `crates/shepr-mux/src/pane/osc.rs`: `"1" | "true" | "yes" | "on"` (a shepr env
  var).
- `crates/shepr-remote/src/remote/server_lifecycle.rs`: `"y" | "yes"` (a
  prompt).

The first two are the same list in a different order for two different domains.
Git's actual boolean syntax also accepts the empty string as true and
`off` / `no` / `false` as false, so the git copy is an incomplete implementation
of a documented external grammar while the osc copy is shepr's own invention that
happens to look identical.

This is two owners rather than one: a `git_bool()` in the git module completed
against Git's grammar, and one `env_bool()` wherever shepr env flags are
resolved. Holdable by a text rule forbidding the bare list elsewhere.

## HYGV-073 - Each agent's own config file name is spelled two to four times

Reported by the agent hunter.

For every target the same file name appears in `check_config_targets`, in the
install body, in the uninstall body, and again in
`registry.rs::hook_registration_is_current`: `"settings.json"` at nine sites,
`"hooks.json"` at eight, `"config.toml"` at five, `"config.yaml"` at four,
`"config.json"` at three, `"cli.json"` at five (four of them in
`opencode_config.rs`), `"tui.json"` at four. None of it is a constant, and
`TUI_CONFIG_NAME` is the lone counterexample.

Fix: put the config file name (and the ancestor depth, HYGV-075) on the
`IntegrationSpec` row and have install, uninstall and status read that row. The
hunter's overall recommendation for this crate is to make `INTEGRATION_SPECS` the
only table - carrying the config file name, the config path depth, the hooks
root, the registration check strategy, the directory key as an enum, the asset,
the version, the events and the timeout - which subsumes this entry and
HYGV-074, HYGV-075, HYGV-076, HYGV-078 and HYGV-079.

## HYGV-074 - The agent config directory registry is keyed by free-form strings

Reported by the agent hunter.

`crates/shepr-agent/src/integration/env.rs::AgentIntegrationPaths` holds a
`HashMap<&'static str, CapturedDirectory>` populated from a literal list of twenty
keys (`"pi_extension"`, `"claude"`, `"opencode_state"`, and so on), read back by
`paths.directory("claude")` at forty call sites in `targets.rs` and by
`spec.directory` in `registry.rs`. A typo or a rename produces a runtime
`NotFound` error at install time only, on the one target exercised.

Fix by type: make the key an enum (or index the array by `IntegrationTarget` plus
a small `DirectoryRole`), and the bad spelling stops compiling.

## HYGV-075 - Ancestor depths in `hook_registration_is_current` mirror the install paths by hand

Reported by the agent hunter.

`json_in(2, "settings.json", ...)` for Claude because the hook lives at
`<dir>/hooks/<name>`, `json_in(1, ...)` for Codex because it lives at
`<dir>/<name>`, `ancestor(hook_path, 2)` for Kimi. Those numbers are
`spec.path.len() + 1` and nothing says so. Change a spec path and status quietly
reports Outdated forever - the hook is fine, the check is looking in the wrong
directory - with no log line.

Fix: derive the depth from `spec.path.len()`, or better, keep the config path on
the spec row and stop walking upward from the hook path.

## HYGV-076 - One ten-second hook timeout, four spellings, two units, and four bare literals

Reported by the agent hunter.

`LETTA_HOOK_TIMEOUT_MS = 10_000`, `MASTRACODE_HOOK_TIMEOUT_MS = 10_000`,
`ANTIGRAVITY_CLI_HOOK_TIMEOUT_SEC = 10`, and a bare `10` in
`claude_settings.rs::canonical_hook_value`, in `canonical_hook_input`, in
`claude_settings.rs::install`'s `ensure_command_hook(..., 10, ...)`, in
`config_edit.rs::kimi_hook_table` (`timeout = 10` inside a format string) and in
`targets.rs::grok_hook_config`.

The agents genuinely disagree about the unit, which is a real reason for separate
values but not for anonymous ones. Fix: one `HOOK_TIMEOUT: Duration` with
per-agent unit conversion at the edit site; the mechanical part is the type,
which cannot be written as a bare integer into JSON.

## HYGV-077 - The remote `shepr` CLI's argument spellings are re-spelled in `shepr-remote` with no shared constant

**Decision (partial):** `--idle-timeout-v1` is deleted, which removes one of the
listed spellings. The shared-constant and round-trip-test proposal remains open
for the rest.

Reported by the remote hunter, as fact.

`shepr-remote` builds command lines for a remote `shepr` binary out of bare
literals: `"remote-client-bridge"`, `"--idle-timeout-v1"`, `"remote-api-bridge"`,
`"--check"`, `"status"`, `"client"`, `"server"`, `"--json"`, `"server stop"`,
`"--session"`. Every one is defined independently in `src/cli/spec.rs` and
`src/cli.rs`; `--idle-timeout-v1` alone is spelled in `launch.rs`, `src/cli.rs`
and `src/cli/spec.rs`. The root binary depends on `shepr-remote`, so a shared
constant module there could be the single owner for both the producer and the
parser.

Enforcement: a test that round-trips each generated remote command string through
`cli::spec::command().try_get_matches_from`, so the parser proves the producer.
The only current check is byte-for-byte golden strings in `attach.rs`, which pin
the producer to itself and say nothing about the parser.

## HYGV-078 - The asset version parity test carries a hand-written list

**Decision (partial):** Hermes support is removed entirely, so the
`plugin.yaml` version and name findings go with it. The hand-written parity
list (the two opencode TUI assets) remains open.

Reported by the agent hunter.

`bundled_integration_asset_versions_match_expected_versions` enumerates its
`(name, asset, version)` triples by hand and omits `OPENCODE_TUI_PLUGIN_ASSET`
and `OPENCODE_V2_TUI_PLUGIN_ASSET`. A new target added without extending the
list is silently uncovered, and `registry::integration_asset(target)` already
exists, so iterating `INTEGRATION_SPECS` would make the test exhaustive by
construction.

## HYGV-079 - Claude's hook event and action are re-spelled six times, beside a descriptor that already declares them

Reported by the agent hunter.

`claude_settings.rs` hard-codes `"SessionStart"` in `HOOK_REMOVALS`, in
`install`, in `canonical_hook_value`, in `canonical_hook_input`, and in the
`installing && event == "SessionStart"` guard, plus the action `"session"` four
times, while `CLAUDE_HOOK_EVENTS` in `agent/mod.rs` already declares exactly that
pair. Kimi, Copilot, Devin, Droid, Qodercli, Qwen, Cursor and Mastracode all
drive their edits off `integration_hook_events`; Claude and Grok do not.

Grok compounds it in two ways the hunter reports as fact:
`command.rs::hook_command` produces `bash '<path>' <action>` while
`targets.rs::grok_hook_command` produces `sh '<path>' session` - the grok asset
is `#!/bin/sh`, so `sh` is probably intentional, but the choice is invisible at
the one place that owns how shepr invokes hook scripts, and the action string
`session` is spelled here rather than taken from
`IntegrationHookAction::as_str`. Fix: give `hook_command` an interpreter
parameter (or read it off the spec row) so every call site goes through one
function, and make `claude_settings` take the event list so the descriptor really
is the domain source its module doc claims.

## HYGV-080 - Shell-name lists have already diverged three ways, and panes running some shells get no detection

Merged into BUG-017 (`notes/bugs.md`), which carries the full finding.

## HYGV-081 - Two walkers over one argv grammar, each with its own flag list

Reported by the agent hunter.

`script_arg_agent_name` takes `eval_flags` / `module_flags` as arguments, and
`letta_entrypoint_index` re-implements the same walk inline with its own copy of
`["-e", "--eval", "-p", "--print"]` and its own
`+= if option_takes_value(arg) { 2 } else { 1 }`.

Fix: have `letta_entrypoint_index` call the shared walker, which needs to return
the index.

## HYGV-082 - `"--conversation"` is spelled three times

Reported by the agent hunter.

`AGENTS[Antigravity].resume_args = FlagValue("--conversation")` and twice
literally in `resume.rs::plan`'s `LettaConversation` arm. Enforceable only by
restructuring `ResumeArgs::LettaConversation` to carry the flag, or by accepting
it as a documented one-off.

## HYGV-083 - Two owners for "does this agent have a screen manifest", with the existing test as the enforcement

Reported by the agent hunter, who files it as a non-finding with an answer, and
who asked for it to be recorded so it is not hunted again.

`AgentDescriptor::screen_manifest` (the flag) and
`manifest::has_screen_manifest(agent)` (whether the registry actually loaded one)
agree today because
`manifest/tests.rs::all_bundled_manifests_parse_validate_and_compile` pins it,
which is exactly the right kind of enforcement. Worth knowing that
`BUNDLED_MANIFESTS` is a third list keyed by label string
(`("agy", include_str!("manifests/antigravity.toml"))`), so a label rename breaks
the join - and the existing test catches that. The recommendation is: keep the
test, it is the enforcement.

## HYGV-085 - Version-probe timing has no injection point at the outer entry

**Decision (partial):** the clock seam (HYGP-001 in `notes/hygiene-policy.md`)
and per-crate `limits` modules (HYGV-036) are both adopted incrementally as part
of the hygiene work; `run_version_probe`'s `Instant::now()` and the probe budgets
fall under them when `shepr-agent`'s turn comes. Open: the outer entry taking
the timeout.

Reported by the agent hunter.

`enforce_agent_version` hard-codes `VERSION_PROBE_TIMEOUT` (5 s) at the call,
while `run_version_probe` takes a timeout parameter, so a test can only exercise
the inner function. `version_probe_deadline_includes_inherited_stdout`
consequently sleeps 300 ms plus 50 ms of real wall clock to let a grandchild die.

Fix: have `enforce_agent_version` take the timeout (or a small `ProbeBudget`),
and the test stops needing the wall clock.

## HYGV-086 - Log rotation size and retention have no injection point and never reach the config crate

Reported by the core/platform hunter.

`crates/shepr-platform/src/logging.rs::init_file_logging(dir, file_name)`
hardcodes `DEFAULT_MAX_LOG_BYTES` and `DEFAULT_RETAINED_LOG_FILES` into the
`RotatingFileMakeWriter::new` call. The struct already takes both as parameters
and the tests use that, so production cannot set them. A 5 MiB cap with one
generation is a policy decision that never reaches `shepr-config`, the crate that
owns "read and validate once at launch".

Fix: move both into `shepr-config` as validated keys and pass them to
`init_file_logging`, after which the launch-time validation rule covers them.

## HYGV-087 - Identifier allocation reaches process-global counters and clocks directly, with no injection point and no owner of the format

Reported by the core/platform, protocol/config, remote and server hunters.

- `crates/shepr-core/src/layout.rs`: `static NEXT_PANE_ID`. `PaneId::alloc()`
  reads it, and `alloc_from(&counter)` exists purely so the exhaustion test can
  inject one. Any test wanting deterministic pane ids must use `from_raw`, which
  bypasses validation entirely: it accepts `0`, the documented placeholder, while
  `collect_validated_ids` rejects `0`. Fix: `PaneId::from_raw -> Option<PaneId>`
  is a compiler-enforced signature change; removing the global needs an allocator
  value threaded through `Workspace`, which is the larger and better fix.
- `crates/shepr-protocol/src/ids.rs`: `TerminalId::alloc()` reads
  `SystemTime::now()` and a `static AtomicU64` (`Ordering::Relaxed`) directly, so
  a test cannot pin either and any test asserting on terminal ids must accept
  whatever it gets. Uniqueness rests on the clock being monotonic across the
  process or the counter never wrapping, and `duration_since(UNIX_EPOCH)` falls
  back to `.unwrap_or(0)` on a before-epoch clock, at which point ids become
  `term_<counter>` only.
- `crates/shepr-remote/src/machine/profile_id.rs::ProfileId::generate` reads
  `SystemTime::now()`, `std::process::id()` and a private `AtomicU64`, hashing
  `"{pid}:{nanos}:{seq}"` with `sha2` and truncating to 16 bytes, while
  `shepr-platform::unpredictable_token` already exists and is unpredictable
  (getrandom). Two independent id schemes. The comment says "not secrets;
  practical uniqueness is enough", which was written for the catalog row rather
  than for the socket name in the shared XDG runtime directory that the id later
  became. Fix: generate from `unpredictable_token`; enforcement afterwards is
  dropping `sha2` from the `shepr-remote-layer` allowlist in `brokkr.toml`.
- `crates/shepr-server/src/server/headless.rs` builds the client-shell boot id
  with `format!("{}-{}", std::process::id(), SystemTime::now()...as_nanos())`
  inside a struct literal. `shepr_protocol::BootId` is a newtype over `String`
  with `From<String>` and no constructor that owns the format, so the identity
  scheme for the whole boot-generation mechanism - compared in
  `client_commands.rs`, `client_transport.rs`, `surface_reuse.rs` and four places
  in `shepr-client` - is decided by a `format!` at one call site, and
  `unwrap_or_default()` means a clock before the epoch collapses every boot id to
  `pid-0`, silently defeating the stale-boot rejection it exists for. Fix:
  `BootId::for_this_process()` in `shepr-protocol`, with `From<String>`
  restricted to deserialization.
- `crates/shepr-protocol/src/ids.rs`'s doc claims `TerminalId` is an "opaque
  identity for a server-owned terminal ... callers must not derive it from a pane
  id or layout position", while `TerminalId` has a public `From<String>` and a
  non-`cfg`-gated `pub fn test_new`, so deriving one from anything is a one-liner.
  Removing `From<String>` and gating `test_new` makes the claim structural.

## HYGV-088 - The clock has no injection point in several subsystems, and tests wait or fabricate mtimes as a result

Merged into HYGP-001 (`notes/hygiene-policy.md`), which carries the full finding.

## HYGV-089 - Remote configuration is validated at the moment of use, on the client, at attach time

Reported by the protocol/config hunter, as the one real instance in that scope
and as a qualification of the project's "read and validated once at launch"
sentence.

The local path holds: `Config::load_validated` returns `Err` if `diagnostics` is
non-empty, `main.rs::load_validated_config_or_exit` exits, and
`bootstrap.rs::encode_resolved_config` propagates an encode failure with `?` so
the server refuses to boot. But `ValidatedConfig`'s `Deserialize` re-runs
`from_resolution(..., CwdCheck::Received)`, so a remote endpoint's config is
validated during snapshot decode on the client rather than at that client's
launch, and a config the server accepted can be rejected by the client.

This duplication is forced (two hosts, two binaries, one config travelling
between them). What keeps the two validations in step is the exact-build preamble
plus the shared crate, and the hunter's recommendation is to say that out loud in
the `AGENTS.md` sentence, which currently reads as absolute.

On the same path, and reported as hygiene rather than as the defect the hunter
filed separately: the config is decoded twice per new snapshot on the fanout path
in `crates/shepr-client/src/shell/endpoints.rs` - `cache_endpoint_snapshot`
decodes and discards the error with `.ok()`, then `resolve_snapshot_config`
decodes the same bytes again. Decoding once and keeping the `Result` removes the
duplication.

## HYGV-090 - `TerminalState::revision` is bumped at four sites under two different overflow policies

Reported by the mux hunter, as an existing divergence.

- `crates/shepr-mux/src/terminal/state/detection.rs`: `wrapping_add(1)`.
- `crates/shepr-mux/src/terminal/state/detection.rs`, forty lines away:
  `saturating_add(1)`.
- `crates/shepr-server/src/app/actions/workspace.rs`: `saturating_add(1)`.
- `crates/shepr-server/src/app/api/panes/reports.rs`: `saturating_add(1)`.

Two spellings of one counter's increment, disagreeing at `u64::MAX`. Neither is
obviously right, which is the point: nobody chose.

Fix: make the field private behind a single `fn bump_revision(&mut self)`, after
which the overflow behaviour has one answer by construction.

The protocol/config hunter reports the same shape one layer down:
`crates/shepr-protocol/src/revision.rs`'s `counter!` macro gives every counter
both `next()` (saturating) and `checked_next()` (returns `None`), plus saturating
`Add` / `AddAssign`. `surface_reuse::Baseline::accepts` relies on
`checked_next()`; other callers use `next()`. A saturated `SurfaceRevision` at
`u64::MAX` would silently stop advancing and every subsequent delta would be
rejected as a baseline mismatch, forever, with nothing logged. Not reachable in
practice, but the type offers two answers and lets the call site pick. Fix: keep
one; if saturation is never acceptable, delete `next()`.

## HYGV-091 - Braille spinner glyphs are hard-coded next to a manifest-owned glyph set

Reported by the mux hunter.

`crates/shepr-mux/src/terminal/title.rs` computes
`matches!(first, '\u{2800}'..='\u{28ff}') || Agent::all().any(|a| a.activity_glyphs().contains(first))`.
"What counts as an activity glyph" therefore has two owners: the detection
manifests (`activity_glyphs`) and this literal range. A manifest that lists a
braille glyph is silently redundant; a spinner style outside braille that nobody
adds to a manifest is silently not stripped.

Fix: move the braille range into the manifest schema (or a shared `shepr-agent`
const) so `activity_glyphs` is the only answer, and assert in a test that no
manifest glyph falls inside a range the code also hard-codes.

Related, and recorded by the agent hunter as data wearing a general mechanism:
`title_activity_glyphs` is non-empty for Claude alone
(`CLAUDE_ACTIVITY_GLYPHS`); every other agent has `""`.

## HYGV-092 - `AppPolicy` has two spellings for two of its three variants, and the `persist_session` mapping is implemented twice

Reported by the server hunter.

`crates/shepr-server/src/app/mod.rs` defines `AppPolicy::PRODUCTION` and
`AppPolicy::TEST` as associated consts that are literally `Self::Production` and
`Self::Test`; the third variant, `Suspended`, has no const. The result is that
`server/headless/lifecycle.rs` writes two naming conventions inside one
expression:

```rust
self.app.policy = if freeze.persist_session {
    crate::app::AppPolicy::PRODUCTION
} else {
    crate::app::AppPolicy::Suspended
};
```

Repo-wide there are about sixty `AppPolicy::TEST` sites and two
`AppPolicy::Test` sites. Deleting the consts makes the second spelling
unrepresentable.

The three lines above appear twice in `lifecycle.rs`, identically, and a third
site writes `Suspended` directly. They agree today, so the duplication is a
prediction rather than a fact, but the rule belongs on `HostShutdownFreeze` as
`fn restored_policy(&self) -> AppPolicy`, after which the caller cannot spell the
mapping.

## HYGV-093 - `headless_size` has two owners and its `Rect` derivation is spelled three times

Reported by the server hunter.

`AppState::settings.headless_size` (from `config.headless_size()`) and
`HeadlessServer::headless_size` (copied from the former) are two owners of one
value. The "headless size as a `Rect`" derivation appears three times:
`app/state.rs::pane_geometry`, `app/mod.rs`'s restore path, and
`server/headless/client_views.rs::resize_tabs_to_headless_size`.

`app/mod.rs` is the clearest case: it constructs by hand, field for field, the
same `shepr_mux::workspace::PaneGeometry { area, pane_borders, pane_gaps,
pane_outer_borders, pane_scrollbars }` that `AppState::pane_geometry_in` already
owns. If a chrome field is added to `PaneGeometry`, the restore path is the site
that will be missed.

Fix: drop `HeadlessServer::headless_size` and route through `app.state`; make
`AppSettings::headless_rect()` the only constructor.

## HYGV-094 - `hostname()` is resolved twice in one `App`, with two empty-value rules

Reported by the server hunter.

`app/window_title.rs` uses `shepr_platform::hostname().unwrap_or_default()`;
`app/tab_bar_status.rs` uses
`shepr_platform::hostname().as_deref().unwrap_or_default()`. Both cache at
configure time, neither knows about the other, so the `{hostname}` in a window
title and the `hostname` tab-bar segment can disagree only by accident of when
each was configured. Fix: resolve once into an `App` field.

The core/platform hunter's lateral note on the same function: the `hostname()`
buffer is 256 bytes against a `HOST_NAME_MAX` of 64, which is harmless and
correctly handles a non-NUL-terminated truncation, and is recorded only because
the 256 is another unnamed number.

## HYGV-095 - `client_socket_path(paths)` is recomputed four times in one function

Reported by the server hunter.

`crates/shepr-server/src/server/headless/bootstrap.rs` computes it at three sites
plus the API socket at a fourth. Pure and cheap, so this is tidiness rather than
risk, but it is four sites that will each be read as "where the client socket
comes from".

## HYGV-096 - `normalize_api_key_alias` is a three-entry alias table living away from the parser that owns key names

Reported by the server hunter.

`crates/shepr-server/src/app/api_helpers.rs` maps `"C-c" | "c-c" => "ctrl+c"` and
`"+" => "plus"`. Key-name parsing otherwise belongs entirely to
`shepr-config::parse_key_combo`, so a fourth alias will be added here rather than
there and the two will drift. Fix: move the aliases into `shepr-config` next to
the parser.

## HYGV-097 - The remote `status --json` contract is two independent structs with no shared type

Merged into HYGG-080 (`notes/hygiene-guards.md`), which carries the full finding.

## HYGV-098 - The `local` / `server` keybinding-role round trip is spelled twice, in two crates

**Decision (partial):** piece 1 (the `shepr-core` environment registry, after
broadarrow's `core::env`) takes the absent and non-UTF-8 handling out of
`ClientProcessRole::from_env`: the variable becomes a registry entry and the
reader answers those cases once. Open: the `"local"`/`"server"` value mapping,
which stays with the owning site as broadarrow's text kinds do.

Reported by the remote hunter, as fact.

`crates/shepr-remote/src/remote/args.rs::RemoteKeybindings::parse` / `as_str`
owns the mapping and the env var name, but `parse` is `pub(super)` and therefore
not exported, so
`crates/shepr-client/src/handshake.rs::ClientProcessRole::from_env`
re-implements it: its own `"server"` / `"local"` literals, its own error text
(`"{var} must be 'local' or 'server', got {value:?}"` versus
`"--remote-keybindings must be 'local' or 'server'"`), and its own handling of
the absent and non-UTF-8 cases.

Fix: `RemoteKeybindings` owns `to_env_value` / `from_env` and is exported;
`shepr-client` already depends on `shepr-remote`. Enforcement: a round-trip test
`from_env(to_env_value(x)) == x`, impossible to write today because the two
halves live in crates that do not share the type.

The termio/client hunter recorded the adjacent case as the answer to "what keeps
forced copies in step", and as the pattern the rest of that crate should follow:
`REMOTE_KEYBINDINGS_ENV_VAR` and `REATTACH_COMMAND_ENV_VAR` are `pub const` in
`shepr-remote` and the client reads them rather than spelling the strings, and the
build-identity preamble means a drifted pair fails loudly at connect.

## HYGV-099 - The shell-safe character set is duplicated verbatim in two modules

Reported by the remote hunter, as fact.

`crates/shepr-remote/src/remote/launch.rs::shell_quote` and
`machine/executable.rs::has_only_shell_safe_characters` contain the identical
predicate
`ch.is_ascii_alphanumeric() || matches!(ch, '@'|'%'|'_'|'+'|'='|':'|','|'.'|'/'|'-')`.
They agree today and serve different purposes (one decides whether to quote, the
other whether to reject), which is why the duplication was easy to introduce:
`executable.rs` rejecting a character `shell_quote` would have quoted safely is a
silent discovery failure.

Fix: one `fn is_shell_plain_word(s: &str) -> bool` called by both. Enforcement: a
test asserting `shell_quote(s) == s` exactly when
`has_only_shell_safe_characters(s)`, writeable today, which pins the two together
without merging them.

## HYGV-100 - An untrusted remote version string is rendered by two policies, and the looser one is the interactive prompt

Merged into BUG-038 (`notes/bugs.md`), which carries the full finding.

## HYGV-101 - `input_wire` is one conversion layer written twice, with the accounting rule byte-identical and the unknown-bit policy opposite

Merged into HYGP-023 (`notes/hygiene-policy.md`), which carries the full finding.

## HYGV-102 - The local endpoint's name exists in three spellings

Reported by the termio/client hunter.

- `crates/shepr-client/src/endpoint.rs::ClientEndpointId::storage_key()` yields
  `"local"` (the persistence key).
- `shell/endpoints.rs` constructs `label: "Local".into()` inline (the display
  label).
- `shell/sidebar/endpoint_sidebar.rs` falls back with
  `.map_or("Local", |e| e.label.as_str())`.

The sidebar fallback restates the display label that `shell/endpoints.rs` builds,
so a rename there leaves the fallback showing the old name for exactly the case
where the entry is missing, which is the case nobody tests. Fix: one
`ClientEndpointId::display_label()` next to `storage_key()`, read by the
fallback.

## HYGV-103 - `pixel_geometry_*` has two owners: a constructor that returns placeholders and a caller that patches them

Reported by the termio/client hunter.

`ClientSettings::from_config` sets `pixel_geometry_enabled: false` and
`pixel_geometry_fallback: false` unconditionally, then `run_client_with_mode`
computes the real values from `client_rendered_shell` / `attach_escape` and
assigns them into the struct. Between the two, `ClientSettings` holds values that
are not the resolved configuration, and a later reader of `from_config` cannot
tell that its answer for those two fields is a placeholder. `lib.rs` then reads
one of the pair from `state.settings` and the other from `config.settings` in the
same expression.

Fix by signature: `ClientSettings::resolve(config, launch_mode)` returning a
fully initialised value, with the fields private and no setters.

## HYGV-104 - Sidebar chrome preferences are validated at the moment of use rather than at startup

Reported by the termio/client hunter.

`crates/shepr-client/src/shell/presentation/config.rs::persist_chrome_preferences`
writes preferences and, on failure, calls `self.set_endpoint_error(error)` - a UI
banner, hours into a session, on whatever gesture happened to trigger a persist.
The preferences path is an `Option` and a `None` silently skips persistence
entirely. Nothing at launch checks that the path is writable, so the first sidebar
drag of the session is where an unwritable state directory is discovered, against
the project's "any config problem fails the launch; no fallbacks".

Fix: a startup probe on the preferences path, which turns this into a launch
refusal. Checkable by a test that launches with a read-only state directory.

## HYGV-105 - Two distinct types named `DeadlineReader` in one crate

Merged into HYGP-007 (`notes/hygiene-policy.md`), which carries the full finding.

## HYGV-106 - Nine `serde` bounded-vec annotations restate the codec's own default cap

Merged into HYGP-056 (`notes/hygiene-policy.md`), which carries the full finding.

## HYGV-107 - `read_message`'s `max_frame_size` parameter has had one value at every production call site

Reported by the protocol/config hunter.

Roughly fifteen call sites across `shepr-client` and `shepr-server` all pass
`shepr_protocol::MAX_FRAME_SIZE`; only one test passes anything else. A parameter
nobody varies is both dead weight and a hazard, since a call site can weaken the
cap and nothing notices.

Fix: drop the parameter from the public function and keep a
`#[cfg(any(test, feature = "test-support"))]` variant for the one test; the
signature then makes the bad spelling unrepresentable.

## HYGV-108 - `git` is invoked from four production sites with four policies, and the client's is the least careful

Merged into BUG-066 (`notes/bugs.md`), which carries the full finding.

