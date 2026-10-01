# Later

Recurring chores and checks that wait for the situation to come up, not
defects to hunt.

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

