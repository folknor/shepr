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
