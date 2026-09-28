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
registry membership. Open: the pane-inheritance question (which entries a pane
may inherit, HYGP-018). Rendering the registry into `shepr --help` is a new
claim, moved to `notes/todo.md`.

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
  answer (these are Git's paths, not shepr's, and the file says so); it now also
  follows git for `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`
  and command-scope config.

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
under a deep checkout. Open: the `ipc.rs` prose and the `platform/src/tests.rs`
literals.

Reported by the core/platform and remote hunters.

`crates/shepr-platform/src/ssh_paths.rs` owns `UNIX_SOCKET_PATH_MAX = 107`.
Restatements: `ipc.rs` ("without using any of `sun_path`'s 107 bytes"),
`platform/src/tests.rs` (`"x".repeat(107)`, `"x".repeat(108)`), and
`crates/shepr-test-support/src/lib.rs` ("`sun_path` (108 bytes)").

The test-support copy is a forced duplication: that crate's dependency allowlist
is `["libc"]`, so it cannot read the platform constant, and the restriction is
right. What keeps them in step: nothing. Since it is prose, the cheapest answer
is prose that cannot drift ("must fit in `sun_path`", without the number).

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
  agent), the logind backoff ceiling (60 s),
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

The platform half is resolved: `MAX_CLIPBOARD_TEXT_BYTES` is at module scope in
`crates/shepr-platform/src/clipboard.rs` and its test derives the oversize input
from it. The two caps stay separate on purpose: the 1 MiB platform cap bounds
host clipboard reads, the 192 KiB `shepr-vt` cap bounds terminal-originated OSC
52 stores, opposite directions.

Open: `shepr-vt`'s `MAX_CLIPBOARD_BYTES` drops an OSC 52 payload over 192 KiB
with no log line, so a copy from a pane that silently does nothing cannot be
diagnosed. A rate-limited log with the byte count (never the content) is the
fix.

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
policy; `POLL_INTERVAL = 5ms` declared twice in `clipboard.rs`; a hardcoded `100` ms in
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

## HYGV-072 - Three boolean-from-string parsers, no owner, and one is an incomplete implementation of an external grammar

**Decision (partial):** the `env_bool()` half is piece 1 (the `shepr-core`
environment registry, after broadarrow's `core::env`): shepr env flags have one
kind and one rule, exactly `1`/`0`/`true`/`false`, so `osc.rs`'s
`"1" | "true" | "yes" | "on"` goes (and `yes`/`on` become refusals). The
`git_bool()` half is resolved: `git/config.rs::git_config_bool` follows git's
grammar (checked against git 2.53) and serves `core.bare` and
`worktree_config_enabled`. Open: the env half, until piece 1 lands at `osc.rs`.

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

## HYGV-087 - Identifier allocation reaches process-global counters and clocks directly, with no injection point and no owner of the format

Reported by the core/platform, protocol/config, remote and server hunters.

- `crates/shepr-core/src/layout.rs`: `static NEXT_PANE_ID`. `PaneId::alloc()`
  reads it, and `alloc_from(&counter)` exists purely so the exhaustion test can
  inject one. Any test wanting deterministic pane ids must use `from_raw`, which
  bypasses validation entirely: it accepts `0`, the documented placeholder, while
  `collect_validated_ids` rejects `0`. Fix: `PaneId::from_raw -> Option<PaneId>`
  is a compiler-enforced signature change; removing the global needs an allocator
  value threaded through `Workspace`, which is the larger and better fix.
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

## HYGV-109 - Installers read hook events two ways

`crates/shepr-agent/src/integration/`: some installers now take their hook
events from `integration_hook_events(Target)` (the spec row), while others still
read per-agent `*_HOOK_EVENTS` constants directly. One path for every
installer, then delete the per-agent constants that only feed it.

## HYGV-107 - `read_message`'s `max_frame_size` parameter has had one value at every production call site

Reported by the protocol/config hunter.

Roughly fifteen call sites across `shepr-client` and `shepr-server` all pass
`shepr_protocol::MAX_FRAME_SIZE`; only one test passes anything else. A parameter
nobody varies is both dead weight and a hazard, since a call site can weaken the
cap and nothing notices.

Fix: drop the parameter from the public function and keep a
`#[cfg(any(test, feature = "test-support"))]` variant for the one test; the
signature then makes the bad spelling unrepresentable.
