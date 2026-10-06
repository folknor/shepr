# Hygiene: claims

Things that look like checks and are not: tests that cannot fail or depend on the
host rather than the repository, guards that fail open when a name stops matching,
and invariants or documentation that nothing enforces (several of them false
today). Filed from the nine-scope hunt; each entry names the hunts that reported it
and says how the fixed form could be enforced.

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

## Tests

## CLAIM-010 - Most bundled detection manifests have no behaviour test

Reported by: agent-state.

Only Claude (title stand-down, one blocker), OpenCode and Kilo (permission), Codex
(one server explain test) and the stable-Unknown flags of Gemini and Letta are
exercised. The rules of Amp, Antigravity, Cline, Copilot, Cursor, Devin, Droid,
Grok, Kimi, Kiro, Letta, Maki, Muse, Pi, Qodercli and Qwen are pinned by nothing,
and their comments' evidence claims ("Grok 1.0.34 live pane reads", "Muse Code 0.2.1
captures") are unverifiable. There is no capture corpus, though `detect capture`
produces exactly the JSON `detect explain --file` reads. Fix: commit captures per
agent state under the detect crate and a test running each through
`explain_with_input` against its expected rule. This is what makes BUG-034 and
VAL-027 safe to change.

## CLAIM-011 - Detection tests that reimplement the path, cannot fail, or name a configuration they do not run

Reported by: agent-state.

- `visible_working_does_not_override_hook_idle_for_same_agent` and
  `visible_working_does_not_override_full_lifecycle_hook_idle` pass no visible-working
  flag; ownership has no such input (BUG-031). They wait on that decision, and so does
  the clock-reading `set_detected_state` helper they use (CLAIM-012).

## CLAIM-012 - Detection tests that depend on wall-clock timing

Reported by: agent-state.

Every ownership seam now takes its instant except the no-time `set_detected_state`
helper, which still calls `Instant::now()` because the two pending visible-working
tests (CLAIM-011) use it; the exception is commented at the seam.

## CLAIM-016 - Server lifecycle tests that assert the absence of removed names

Reported by: server-lifecycle, remote.

Banned-word assertions for removed features (`"--session"`, `"--force"`,
`"SHEPR_SESSION"` in `guidance.rs` and `tui.rs`; `"capabilities"`, `"status"`,
`"running"` keys in `server_status_json_reports_the_running_boot`) assert the absence
of strings no code produces. Waits on the owner decision about removed-name tests
(DEAD-015).

## Guards that fail open

## Claims nothing enforces
