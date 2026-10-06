# Hygiene: policy

One rule implemented separately wherever it was needed (retry, timeouts, cleanup,
validation, error classification), ambient dependencies reached from logic that
should have been handed them, shared state whose safety rests on call order,
growth without bound, personal data on disk, and test-only shortcuts production
can reach. Filed from the nine-scope hunt; each entry names the hunts that
reported it and says how the fixed form could be enforced.

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

## POL-021 - Three hand-written delivery retry policies in the plugin kits

Reported by: integrations.

`plugin_kit.js` makes one attempt; `extension_kit.ts` retries once in the same queue
slot; `tui_kit.js` and the TUI decoder retry every 500 ms indefinitely while the
selection is current. `bundle.rs` documents the split, but each policy is hand-written
per kit.

## POL-031 - Workspace model laterals

Reported by: workspace-model.

- `right_click_passthrough` is per pane, projected and not saved, so it resets on every
  server restart; nothing documents either way (consistent with `set_pane_input` not
  marking the session dirty).
- `mark_focused_pane_cells` with gaps off marks the cell one past the pane's right and
  bottom edge even for an outermost pane; harmless only because the grid ignores
  out-of-area cells.
- `WorkspaceSet` looks workspaces up by linear scan and `projection_input` calls
  `workspace_info(&ws.id())` per workspace, re-scanning: quadratic in workspaces on every
  projection rebuild. Small numbers today.
- `close_workspace` reports removed panes in hash order while `remove_pane` reports
  layout order; callers only shut runtimes down, so no effect yet.
- The global `LayoutEpoch` starts at 0 for every tree, restore included; fine because a
  client's epoch is always from the current boot's projection, but a client reconnecting
  to a new boot with a cached epoch could match a different tree by accident.

## POL-041 - The structured log vocabulary is not constrained

Reported by: the wave 11 reviewer.

Every info, warn and error call now goes through `structured_log!`, and
`scripts/check_structured_logs.py` refuses direct ones. The names those calls carry
are free text:

- Failure outcomes are spelled `error` almost everywhere, but the final save's info
  logs `failed` (documented so in `reference/session-save-shutdown.md`), so a failed
  final save logs two `persist.save` events with different failure outcomes. Success
  is spelled both `ok` and `completed`. `$outcome:expr` accepts any expression, so
  nothing holds outcomes to a small set; an outcome enum or a closed list of idents
  would.
- Subsystems that mean two things: `terminal.*` is the host terminal in the client
  and a pane's terminal in mux; `client.*` is the client process in shepr-client and a
  server-side connection in shepr-server and platform; `client.connection` in
  `shepr-client` `launch.rs` overlaps `endpoint.connection` in the same crate;
  `endpoint.response_encode` in the server's `client_commands.rs` uses the client-side
  name. `shutdown.pane_teardown` and `shutdown.client_flush` sit under `shutdown`,
  otherwise the logind host shutdown. `blit.frame_encode` is a one-event subsystem.
- Failure fields keyed other than `error`: `%failure` (`pane.launch` in
  `launch_status.rs`), `%reason` (`surface.patch` in `render_stream.rs`,
  `client.resize` in `client_transport.rs`). The textlint for the `error` key only
  catches `err`.

## POL-042 - Wave 12 laterals

Reported by: the wave 12 fixers.

- Waiting on the owner: install and status match a registered hook as a subset
  (`canonical_registration_entry_matches` in `json_edit.rs`), so a shepr hook the user
  disabled with an extra field such as `"disabled": true` reads as Current and install
  leaves it alone. If the agent honours that field, the user's disable is respected but
  status says Current while no reports arrive. The tests' comments claim none of these
  agents honours a `disabled` field; nothing verifies it.
- Hook commands now need a POSIX shell (`;`, `case`, `${VAR:-...}`, quoting). The new
  `sh -c` test proves they work there, but whether Codex, Kimi, Grok, MastraCode, Devin
  and Cursor run a registered command through a shell or split and exec it is not
  knowable from the repository; each agent's own source or docs decide it.
- The hook commands' shell resolves a relative `XDG_CONFIG_HOME` or agent config
  override against the agent's cwd, while Rust (`AgentIntegrationPaths`) refuses one, so
  the two can disagree on where the hook lives. Affects Devin, OpenCode and Kilo through
  XDG, and OMP through `PI_CONFIG_DIR`.
- `command.rs` `directory_setup` keeps Pi, OMP, OpenCode and Kilo arms only for
  exhaustiveness (a test now proves those targets have no hook events). Moving the shell
  directory onto the spec rows that register commands would drop them.
- `no-interpreter-stand-ins` matches only `Command::new("python3")`, so
  `command_in_scratch("python3", ..)` in the integration `tests.rs` escapes it.
- The boot log a ready server leaves behind is now kept and logged as a WARN by the
  launcher, but nothing shows it on the operator's screen; that needs `ensure_running`
  to return it to its callers.
- `src/preflight.rs` `restart_local` now fails for a final-save failure followed by a
  new occupant; no preflight test covers that combination.
