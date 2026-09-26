# Agent detection and integration defects

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

## DET-025 - Workspace metadata seqs have the clock-step problem

- Workspace metadata reports (`src/app/api/workspaces.rs` via `metadata_tokens::accept_sequence` / `sequence_is_fresh`) still require strictly increasing seqs with no re-anchoring, so a wall clock stepping backwards drops them until it catches up. Use the shared `report_seq_superseded` rule (`src/terminal/state.rs`) as pane state, session and metadata reports now do.

## DET-026 - Restored agent panes probably flicker in the sidebar

- `restored_terminal` seeds the resumed agent as detected and Idle. The pane detector's first "no agent" tick replaces that seed even when the resume will succeed, so the sidebar likely drops the agent briefly until its process is detected, on every restored agent pane. Unverified.
