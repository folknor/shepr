# CLI, config and keybinding defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
```

Findings raised in this scope but filed elsewhere: `--raw` / revision / `--lines` (API-004), build identity (WIRE-001), Windows key code (PLAT-003), scrollback limit not a maximum (TERM-005).

## CMD-001 - The clap spec only prints help; every subcommand is parsed a second time by hand and the two have drifted

- `src/cli/spec.rs` builds a complete clap model of every subcommand, but it is only used to print `--help`. Every subcommand is parsed again by a hand-rolled parser. Several entries below (CMD-002, CMD-003, CMD-012, CMD-014, CMD-018) come from that split.
- **Recommended by the hunter:** make `spec.rs`'s clap `Command` the real parser (global args only before the subcommand, `--` handling, `--flag=value`, value parsers), then map the results to `Method` params in one place. That removes the second parser for every command and makes flags that no handler reads (like `--group`) easy to spot.

## CMD-002 - Global flags are removed from anywhere in argv, not just before the subcommand

- `session::configure_from_args` (`src/session.rs:54-78`) removes `--session X` from anywhere up to `--`. `remote::extract_remote_args` (`src/remote/args.rs:45-94`) does the same for `--remote` and `--remote-keybindings`.
- `shepr pane run p1 foo --session work` sends the command to session `work`'s server instead of passing the text through.
- `pane run … --remote x` fails with "--remote can only be used with the default launch command".
- `pane run` has no `--` escape: the `--` itself is kept and becomes part of the command text.
- `--machine` parsing in `src/cli/target.rs:194` is correctly prefix-only and has a test for it. `--session` and `--remote` do not.

## CMD-003 - `--current` means different things on different pane commands

- Help shows `[--pane ID|--current]` everywhere.
- In `focus`, `resize`, `neighbor`, `swap` and `zoom`, `--current` sets `pane_id=None`, which the server treats as its focused pane (`src/cli/pane.rs:269,319,395,931`). An agent in a background pane running `pane resize --current` resizes some other pane.
- `layout`, `edges` and `process-info` fall back to the focused pane when `SHEPR_PANE_ID` is unset.
- `split` and `input` return an error instead ("--current requires SHEPR_PANE_ID").
- `split` defaults to the caller's pane even without `--current`; the others don't.
- `SHEPR_PANE_ID` is also used with `--session other` or a socket override, so it points at a pane on a different server.

## CMD-004 - `workspace close <id> --group` is parsed but ignored

- It is in the usage text and sets `close_group` (`src/cli/workspace.rs:233`). `handle_workspace_close` (`src/app/api/workspaces.rs:315`) never reads it.
- The flag is also missing from the clap spec.

## CMD-005 - `shepr --machine X api snapshot` silently exits 0

- `validate_machine_command` accepts it (`src/cli/target.rs:281`, and a test asserts it's valid), but `cli::maybe_run` has no `api` arm. It returns `NotCli`, `main.rs:393-394` treats that as `Ok(())`, and the process exits 0 with no output.

## CMD-006 - `shepr config reset-keys` is advertised but doesn't exist

Surfaced in two scopes: CLI/config, platform.

- It is listed under "Common commands" in `--help` (`src/main.rs:479-482`); `run_config_command` (`cli.rs:112`) only knows `check`. The command prints config help and exits 2.

## CMD-007 - A non-ASCII colour value in the config crashes startup, and bad colours are never reported

- `parse_color` (`src/config/theme.rs:121-128`) slices `&hex[0..2]` whenever the byte length is 6. `"#aééb"` (a·é·é·b = 6 bytes) slices through the middle of a character and panics.
- `parse_color` runs from `palette_from_config` in both `App::new` and `ClientShellConfig::from_config`, so both server and client crash at launch.
- Unknown colour names only produce a `warn!` and silently become cyan. No diagnostic is added, so `shepr config check` says "ok".

## CMD-008 - `ui.accent = "cyan"` is ignored, and "cyan" isn't really the default

- `palette_from_config` only applies the accent when `config.ui.accent != "cyan"` (`src/app/mod.rs:141`). Setting cyan explicitly keeps the theme's accent, and the unset default is the theme's accent, not cyan as `DEFAULT_CONFIG` documents.

## CMD-009 - A preferences file overrides three config keys permanently

- `src/client/shell/config.rs:182-193` and `preferences.rs` store `sidebar_width`, `sidebar_collapsed` and `agent_panel_sort` in `state_dir()/client-shell/local-*.json`.
- After the first manual toggle or resize, the stored value wins over `ui.sidebar_width`, `ui.sidebar_start_collapsed` ("Changes take effect on the next launch") and `ui.agent_panel_sort`. Editing config then has no visible effect.

## CMD-010 - The keybinding help screen leaves out four bound actions

- `keybind_help_groups` (`src/input/keybind_help.rs:141-183`) omits `swap_pane_left`, `swap_pane_down`, `swap_pane_up` and `swap_pane_right` (defaults `prefix+shift+h/j/k/l`).
- `copy_mode` and `swap_pane_*` are also missing from `DEFAULT_CONFIG`.

## CMD-011 - Direct attach ignores the configured prefix and detach keys

- `AttachEscapeState::filter_input` (`src/client/attach.rs:50-65`) hardcodes Ctrl+B and `q`. With `keys.prefix = "ctrl+a"`, `terminal attach` and `agent attach` still intercept Ctrl+B from the pane app.
- Separately, in the legacy byte path a coalesced chunk like `abc\x02q` detaches and drops the `abc` (`:104-107`).

## CMD-012 - `--flag=value` works on only some commands

- The doc comment on `expand_equals_args` (`src/cli.rs:560`) says hand-rolled parsers accept the form the clap help implies. It is only applied in 6 places.
- `pane split` expands only `--right-click`, so `--direction=right`, `--cwd=` and `--ratio=` are rejected. The workspace, tab and agent parsers don't expand at all.

## CMD-013 - Relative `--cwd` is resolved against the server's working directory

- The CLI sends the path unresolved, and `handle_workspace_create` does `PathBuf::from(cwd)` (`workspaces.rs:58`). So `shepr workspace create --cwd .` uses the server process's cwd, not the caller's.
- The hunter only verified this for `workspace create`.

## CMD-014 - Some bad values produce a Rust debug print instead of a usage error

- `parse_u64_flag`, `parse_pane_agent_state` and `parse_read_*` errors propagate with `?` in `workspace report-metadata`, `pane report-agent*`, `release-agent`, `report-metadata` and `agent read`.
- They reach `main` as `Err`, which prints `Error: Custom { kind: Other, … }` and exits 1, where the rest of the CLI prints a message and exits 2.

## CMD-015 - Two tests pass for the wrong reason

- `rejects_unknown_bare_and_malformed_custom_tokens` (`src/config/sidebar.rs:687`) builds TOML with a literal `\\n`. The input is invalid TOML regardless of the token, so the test can never fail.
- `add_parser_rejects_…` (`src/cli/machine.rs:491`): `--label --remote-session agents host` is rejected only because of the extra positional. `--label` actually swallows `--remote-session` as its value.

## CMD-016 - "Every CLI subcommand goes through the JSON API" is not true

- `config check`, `session list/delete`, `integration *`, `machine *` and `agent explain --file` (runs manifest evaluation in the CLI process) work locally.
- `session stop` hand-builds a raw `server.stop` request without the protocol check.
- The hunter's view: most of these are inherently local, so the documented contract needs rewording rather than the code needing change.

## CMD-017 - The meaning of "meta" is inconsistent

- In keybindings `parse_modifier_token` maps it to ALT (`src/config/keybinds.rs:928`). `format_key_combo` prints the META flag as "meta", and `right_click_passthrough_modifier` maps "meta" to META.

## CMD-018 - Help text and documentation drift

- `pane` help omits `--raw` and the `detection` source for `read`, and `--session-start-source` for `report-agent-session`.
- The `integration` usage string omits `antigravity-cli`, which is accepted.
- `server_not_running.rs:8` mentions a "plugin offline fallback", which doesn't exist.
- `#[allow(dead_code)]` comments in `input/parse.rs` and `input/encode.rs` say "reserved/unused", but the functions are used in production.
- `expect()` in production: `cli.rs:640,669` (the API-layer sites are in API-016).

## CMD-019 - Duplicate keybinding diagnostics, missing key names, and a status probe per request

- `validated_keybinds` re-parses and re-logs every diagnostic each time `prefix_key()`, `keybinds()` or `collect_diagnostics()` is called, producing duplicate warnings.
- `pane send-keys` cannot send Home, End, PageUp, PageDown, Delete or Insert: `parse_key_combo` has no names for them, and the only tmux-style alias is `C-c`.
- Every CLI request first does a status probe (up to 15 s timeout under `--machine`). `agent start` polling therefore makes two round trips per poll.
