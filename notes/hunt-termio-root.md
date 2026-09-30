# Defect hunt: shepr-termio and the root binary

Scope: `crates/shepr-termio` (key parsing and encoding, raw input framing,
copy mode, scroll, selection rendering, host terminal modes, `blit.rs`) and the
root `shepr` package (`src/main.rs`, `src/cli.rs`, `src/cli/`,
`src/preflight.rs`, `src/autodetect.rs`). Values were followed into
`shepr-mux` (pane key encoding), `shepr-client` (host keyboard setup),
`shepr-config` (paths, address guidance), `shepr-api` (stop, status) and
`shepr-remote` (preflight, discovery, remote stop).

Findings are ordered by severity. Each one names the claim it breaks.

---

## 1. Shift+Tab reaches legacy panes as a plain Tab when the host speaks kitty (high)

**Claim broken:** AGENTS.md "Key encoding to pane children covers ... legacy
encoding, kitty disambiguate and the keys crossterm's `KeyCode` models, and
modifyOtherKeys for Enter, Esc, Tab and Backspace". Shift+Tab (BackTab) is a
key crossterm models and the legacy encoding has a form for it (`CSI Z`).

**Path:**

- The client always pushes kitty flags 7 (disambiguate, event types,
  alternate keys) to the host at startup
  (`crates/shepr-client/src/terminal_setup.rs`, `set_keyboard_enhancement_flags`
  with `ime_compatible_keyboard_enhancement_flags()`).
- A kitty-capable host (kitty, Ghostty, foot, Alacritty, ...) then reports
  Shift+Tab as `CSI 9;2u`.
- `parse_kitty_key_sequence` (`crates/shepr-termio/src/input/parse.rs`) turns
  that into `KeyCode::Tab` with `SHIFT`. Only the legacy `CSI Z` is ever parsed
  as `KeyCode::BackTab`.
- Server side, `PaneTerminal::encode_terminal_key_once`
  (`crates/shepr-mux/src/pane/terminal/backend.rs`) sends non-Char keys through
  `encode_terminal_key_with_modes`. For a pane with no kitty flags and
  modifyOtherKeys off or at level 1:
  - `encode_modify_other_keys` returns `None` (Tab is level 2 only);
  - `encode_legacy` finds no `encode_modified_special` form for Tab;
  - `encode_legacy_inner` returns `\t`.

  The Shift is lost.
- The kitty-pane direction works: `BackTab` is rewritten to `Tab+SHIFT`, which
  gives `CSI 9;2u`. The reverse normalization (`Tab+SHIFT` to `CSI Z` for a
  legacy pane) is missing.

**Impact:** any legacy-mode child (bash/readline completion cycling, most TUI
agents that bind Shift+Tab, including mode toggles) gets Tab instead of
Shift+Tab whenever the outer terminal supports kitty.

**Coverage:** no test feeds a host `CSI 9;2u` to a legacy pane.
`terminal_backtab_preserves_shift_across_keyboard_protocols` in
`crates/shepr-mux/src/pane/terminal/tests.rs` starts from `KeyCode::BackTab`,
which a kitty host never produces.

**Fix direction:** treat BackTab and Tab+Shift as one key. Either the parser
normalizes one to the other, or `encode_legacy`/`encode_legacy_inner` emits
`CSI Z` for Tab with exactly Shift. The modifyOtherKeys level-1 path should do
the same. It should be one canonical form, not two spellings that each encoder
must remember.

---

## 2. Build-mismatch and not-running guidance is profile-blind and can stop the wrong server (high)

**Claims broken:**

- AGENTS.md "Dev and release builds use separate runtime and data
  directories ... a dev and a release build never talk to each other's
  server".
- "on refusal, the server is left running and unavailable and shepr says how
  to stop it".

**Path:** `ServerAddress::stop_command` / `attach_command`
(`crates/shepr-config/src/address.rs`) always spell the command as `shepr` /
`shepr server stop`, prefixed only with a socket override. The root binary
passes this text straight to the operator in several places:

- `autodetect.rs` through `local_server::ensure_running` and
  `running_build_mismatch`;
- `cli.rs` `ensure_server_build_matches` (`target::restart_guidance`);
- `cli/server_not_running.rs` (`attach_command`);
- `unresponsive_error` in `crates/shepr-remote/src/remote/local_server.rs`.

**Scenario:** a dev build (`brokkr run`) meets an older dev server. It is told
"Run `shepr server stop`, then run `shepr` again". On a host with shepr
installed, `shepr` on `PATH` is the release binary. It resolves the release
runtime directory and stops the release server, ending every pane and agent
the operator actually works in. The dev server is left untouched. The follow-up
`shepr` attaches the release TUI, not the dev one. The not-running message has
the same problem for a dev CLI command ("run `shepr`" starts a release server).

**Fix direction:** the guidance must name this profile's own entry point. For
release that is `shepr`. For dev it is `brokkr run -- server stop`, or the
absolute path of the running executable, which `launch_executable` already
resolves. The address type is the wrong owner for this text: it knows sockets,
not which binary resolves to them. Move the command rendering to the root
binary, which knows its profile and path.

---

## 3. The dev TUI cannot be launched from a release pane, contrary to AGENTS.md (medium)

**Claim broken:** AGENTS.md "Run it with plain `brokkr run -- [<command>]`,
including from inside a pane of the installed server." With no command that is
the TUI.

**Path:** `main.rs` `refuse_if_nested_disabled` blocks the TUI and `client`
launches whenever `SHEPR_ENV == SHEPR_ENV_IN_PANE` and
`experimental.allow_nested` is false (the default). It ignores
`SHEPR_BUILD_PROFILE`. A release pane exports both, so the dev TUI launched
from it fails with "nested shepr is disabled by default".

The profile marker already decides that a dev process in a release pane is not
talking to that pane's server (the socket overrides are dropped). The nesting
guard does not use the same fact. Either the guard should pass when the
marker's profile differs from `BuildProfile::current()`, or AGENTS.md must say
that the dev TUI needs `allow_nested`.

---

## 4. Remote "left running" notice gives a stop command that may not resolve (medium)

**Claim broken:** AGENTS.md "on refusal, the server is left running and
unavailable and shepr says how to stop it".

**Path:** `preflight.rs` `remote_stop_command` renders
`ssh <target> shepr server stop`. That command relies on `shepr` being on the
non-interactive ssh `PATH`. Discovery explicitly does not rely on this
(`crates/shepr-remote/src/remote/discovery.rs`: a login-shell `command -v`,
then `/bin/sh`, then known locations, "which misses them when a non-interactive
SSH shell has a minimal PATH").

The executable discovery found is in hand: every `restart_notice` branch that
prints `left_running` has `MachineCheck::DifferentBuild(server)` with
`server.executable`, and `stop_remote_server` uses exactly that path
(`server.executable.command(&args)`). The notice should render the same path,
shell-quoted, so the printed command is the one shepr itself would run.

---

## 5. Raw-input logging writes typed bytes it promises not to log (low)

**Claim broken:** `raw_input.rs` comments: "Length and kind only: the bytes and
the parsed key are what the user typed, passwords included, and the log file
outlives the session" and "Buffer contents are user keystrokes; log lengths,
never bytes."

**Path:** `extract_one_event` logs
`tracing::debug!(sequence = ?seq, "dropping unsupported escape sequence")` with
the full sequence. Unsupported sequences include key reports the parser
rejects. For example, a kitty report-all CSI u carrying associated text with a
codepoint the parser refuses (`reject_malformed_kitty_associated_text` cases),
or IME text in a form the parser does not accept. The codepoints of typed text
then land in the client log. `flush_timeout` also logs `bytes = ?self.buffer`
for timed-out SGR mouse prefixes, which is harmless, but it is the same pattern.

**Fix:** log length and a classification only, as the neighbouring code does.

---

## 6. `matches::flag`/`string` turn a spec/handler mismatch into a valid-looking value (low, latent)

**Claim broken:** the module doc of `src/cli/matches.rs`: "these helpers use
clap's non-panicking lookups so a spec/handler mismatch is rejected rather than
turned into a valid-looking empty value".

**What the helpers do:**

- `flag` maps a lookup error to `false`.
- `string` maps it to `None`.

Both are valid-looking values. For `server stop`, a misnamed id would turn
`--expect-boot X` into an unconditional stop (`expected_boot: None`). That is
the exact race the flag exists to close, and it would happen silently.

Today the ids are derived from the same constant (`option_name_from_flag`) and
`server_stop_parses_the_expected_boot` covers it, so this is latent. Still, the
helpers do the opposite of their stated contract. Either make them return
`Result` and fail the parse (exit 2), or fix the doc.

---

## 7. Preflight runs before the terminal usability check it depends on (low)

**Claim:** `autodetect.rs` "The client requires terminal geometry before it can
attach. Reject an unusable terminal before socket lookup creates directories or
starts a daemon."

**Path:** `main.rs` calls `preflight::run` before `auto_detect_launch`. So
before the terminal check, a launch can:

- probe the local socket;
- run the full SSH check round against every machine (up to
  `PREFLIGHT_CHECK_BUDGET`);
- prompt for authentication;
- with consent, stop the local server (`restart_local`, with `can_prompt` needing
  only stdin and stderr to be terminals).

Only then does `terminal_grid_size` reject the terminal. A launch that cannot
attach has by then done the costly and destructive work the check was meant to
prevent. Move the grid-size check ahead of `preflight::run`, or into it.

---

## 8. Restart offer does not say it can end the launching terminal (low)

**Claim:** the offer "says that the restart ends the server's pane processes".

With `allow_nested` and a same-profile server whose pane runs this `shepr`,
`restart_local` offers to stop the server that owns the launching shell.
Stopping it ends the process asking the question. The text ("ends every pane
process it hosts") is literally true, but it does not tell the operator that
this includes the terminal they are typing in. `SHEPR_ENV` and a matching
`SHEPR_BUILD_PROFILE` are enough to detect this and either refuse or say so.

---

## Structural notes (not defects in themselves)

- **The cross-build surface is implicit.** The preflight restart offer, `status`
  and `server stop --expect-boot` all exist to talk to a server of another
  build, over the JSON `ping` and `server.stop` requests:
  - `read_runtime_status_at` deserializes the full `SuccessResponse` /
    `ResponseResult` enum;
  - the boot guard lives only in the receiving server's handling of
    `expected_boot_id`.

  AGENTS.md says there are no wire compatibility obligations. These few shapes
  are the exception the whole restart flow depends on. If a future `Pong` shape
  changes, `local_server_status` logs a warning and returns `None`, and the
  offer silently disappears. The launch then fails with "did not give a usable
  status answer", with no stop guidance. Pinning a tiny, explicitly frozen
  identity/stop envelope (its own type, its own test fixture) would make that
  dependency visible.

- **`blit.rs` trusts a `FrameData` invariant it cannot see.**
  `write_all_cells`, `write_changed_cells` and `blit_patch_to` index
  `frame.cells[idx]` directly and panic if `cells.len() != width * height`.
  The client-side composers check this before building frames
  (`wire_cells.rs`, `compose_pane_surface.rs`), so nothing is known to break.
  But the invariant lives in several callers instead of in the type. A
  validated frame type (constructed once, cells length proven) would remove
  the panics and the duplicated checks.

- **Two "text char of a key" helpers disagree.** `copy_mode_command_char` maps
  an unshifted char with Shift through `shifted_ascii_char` (`/` + Shift to
  `?`). `keybind_help_text_char` returns the unshifted char. For the same key
  (no alternate codepoint), copy mode and the help filter see different
  characters. One shared helper would do.

- **Misleading module doc.** `crates/shepr-termio/src/host_term/title.rs`
  opens with a module doc about clipboard bytes. The module is named for
  titles and holds both.

- **CLI subcommands never load or validate config.** `status`, `server stop`
  and `detect` resolve paths only. This is probably intended (a broken config
  must not block `server stop`), but AGENTS.md says "Config is read and
  validated once at launch ... Any config problem fails the launch" without
  carving out the CLI. The doc or the behaviour should say which.
