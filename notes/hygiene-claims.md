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

## CLAIM-008 - The D-Bus monitor test borrows the host's `dbus-daemon` unflagged

Reported by: save-shutdown.

`delay_lock_is_held_until_checkpoint_and_retaken_after_cancellation` spawns the
host's `dbus-daemon`. It is `#[ignore]`, so `brokkr check` never runs it, while
`brokkr test -p shepr-server host_shutdown` runs it (`--include-ignored`) and fails on
a host without `dbus-daemon`. `no-borrowed-process-stand-ins` lists only shells and
coreutils, so it is not flagged. Add `dbus-daemon` with a `host-program-ok:` marker
(real D-Bus behaviour is its subject), or say so in the rule's preset. It also uses
5 s wall-clock timeouts.

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

## CLAIM-016 - Server lifecycle tests that cannot fail, duplicate each other, or use a developer's paths

Reported by: server-lifecycle, remote.

- Wall-clock tests: `repeated_socket_transitions_share_one_wait_deadline` (350 ms
  sleep, `< 225 ms` assertion),
  `request_line_arriving_after_connect_is_read_without_a_poll_delay` (`< 100 ms`),
  `a_holder_that_never_leaves...` (2..=4 restarts),
  `classification_saturation_still_serves_a_peer_whose_kind_has_room` (a 50 ms sleep
  decides which path runs; under load the overflow path its name claims may not be
  exercised).
- Banned-word assertions for removed features (`"--session"`, `"--force"`,
  `"SHEPR_SESSION"` in `guidance.rs` and `tui.rs`; `"capabilities"`, `"status"`,
  `"running"` keys in `server_status_json_reports_the_running_boot`) assert the
  absence of strings no code produces (waits on the owner decision about removed-name
  tests).
- The host-path textlint for compared test paths covers only the launch and CLI status
  test files.

## Guards that fail open

## CLAIM-019 - Guards keyed on names that become silent no-ops

Reported by: integrations, server-lifecycle, remote.

- `brokkr.toml` `endpoint-moves-are-driven-from-the-endpoint-module` matches
  `choice\s*\.\s*(select|...)`, keyed on the binding name; `let c = &mut
  shell.endpoints.choice; c.commit()` or a direct field assignment (which test code
  already does) passes. Make the transition methods `pub(in crate::endpoint)` and the
  field private behind an accessor, and the compiler is the guard.

## Claims nothing enforces

## CLAIM-026 - Integration claims that are false today

Reported by: integrations.

- `shepr-agent/src/lib.rs` `IntegrationHookAction`: "The action word an installed
  hook passes to the shepr report command". There is no report command; hooks write
  to the socket.
- `shepr-core/src/env.rs` `SHEPR_ASSET_INTERNAL_NAMES`: "Header markers install and
  status code parse out of an asset's text". Only `SHEPR_INTEGRATION_VERSION=` is
  parsed; `SHEPR_INTEGRATION_ID` is read by nothing.
- `registration.rs` `JsonShape::expected_events`: "Copilot, Devin and Droid call their
  payload-decoding hooks for every event". Devin's and Droid's events all carry an
  action; only Copilot has an action-less event.
- `config_edit.rs`: "Copilot uses the flatter settings shape `{ type, matcher, bash }`"
  sits above `ensure_flat_command_hook`, which is MastraCode's; Copilot uses
  `ensure_direct_command_hook`.
- `registration.rs`: "Install merges these entries and status matches them; neither
  reconstructs a second interpretation of the descriptor." Claude's install is a
  second, CST implementation, and Cursor's install inserts `"version": 1` that status
  never checks.
- `bundle.rs` `EMPTY_OBJECT`: "every exit path emits an empty object" still fails for a
  `set -e` abort on an unguarded failing command, which exits without `finish`; the
  template guards most commands with `|| true`, not all.
- `lib.rs` and the `bundle.rs` module doc each describe the whole envelope including
  the 500 ms number: two copies of one paragraph no test reads.
- True and unenforced: `opencode.js`'s "it never runs alongside this server plugin"
  (rests on `ownsLocalLifecycle` and OpenCode's launch shapes).
