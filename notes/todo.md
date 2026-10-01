# Later

Things to do when the situation comes up or when there is time for a larger
change, not defects to hunt. Each redesign below touches several modules at
once, so it needs a wave of its own rather than parallel fixers.

## Deferred

Parked until the situation comes up.

- **A visible notice for a partially restored session.** A workspace
  dropped during restore leaves only a server log line and a backup of the
  original `session.json`; pane-level restore errors draw inside the pane, but
  there is no session-level warning channel from server to client to say a
  workspace was dropped. Build it the first time one actually goes missing.

## Residuals from the CLI reduction

Surfaced while landing the CLI reduction and the client/daemon binary split;
none blocks anything.

- **Integration install reads the server's environment.** The launch-time
  install resolves agent config locations (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `XDG_*`) from the server process, which a client- or SSH-spawned server may
  not share with the user's interactive shells; an agent with a non-default
  config directory then reads as absent.
- **Re-prompting for SSH authentication.** A machine that still needs
  authentication after a failed or skipped startup prompt is not prompted again
  until the next launch, and there is no TUI action to suspend the screen and
  authenticate. Add one if losing the shared connection mid-session proves
  annoying.

## Review upstream changes

`scripts/upstream_watch.py` reports upstream herdr changes to the integration
assets and their tests, the detection manifests (bundled and published), the
manifest tooling and the detection and hook wiring since the baseline in
`scripts/upstream_baseline.txt` (the fork was 21d0ce6; the baseline advances
as upstream changes are ported or judged irrelevant). Run it periodically;
its docstring lists what it watches. Test coverage of the agent plugins grows by
porting upstream's tests, not by writing our own.

## Confirm the opencode/Kilo permission-dialog labels

Do this the next time opencode or Kilo is in use.

- The `permission_required` rules in `crates/shepr-agent/src/detect/manifests/opencode.toml` and `kilo.toml` match "△ Permission required" only when one of the dialog's control labels is also on screen: "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI, not captured.
- If they are wrong, opencode/Kilo panes never show as blocked on a permission prompt; they read as working or idle while waiting on you.
- To check: in a shepr pane, get the agent to ask for a permission, run `shepr detect capture <pane>`, and compare the dialog's labels with the gate. Fix the manifests if they differ.

