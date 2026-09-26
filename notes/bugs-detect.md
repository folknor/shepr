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

## DET-012 - Hook seq units and Kilo's start source

- Hook seqs are now accepted as a new baseline after 5 s (`HOOK_SEQUENCE_REANCHOR_AFTER`), bounding the loss after a clock step. Remaining: seq units differ by integration (Kilo, opencode, pi, omp seed from `Date.now()*1000`, microseconds; shell/python hooks use nanoseconds; harmless while compared per source), and Kilo always sends `session_start_source: "startup"` because its events carry no start source (commented in the asset).

## DET-016 - opencode/Kilo permission-dialog control labels are unconfirmed

- The `permission_required` rules in the opencode and Kilo manifests also require one of "allow once", "allow always", "reject" or "enter confirm". Those labels were written from memory of opencode's TUI. Confirm them against a live dialog with `shepr agent read <pane> --source detection --format text`.

## DET-023 - Metadata report seqs have the clock-step problem

- `metadata_report_sequences` (`HookMetadataReported`) still orders strictly by seq per source, so a wall clock stepping backwards drops metadata reports until it catches up. Use the same `hook_seq_superseded` / `record_hook_seq` re-anchoring as state and session reports.

## DET-024 - A failed resume can leave a detected agent on a plain shell

- `restored_terminal` pre-marks the resumed agent as detected and Idle before any process exists. If the resume fails, the managed name is released but the pre-set detection stays until detection sends a no-agent update, so the sidebar can keep showing that agent on a plain shell.
