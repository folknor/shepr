# Defects: agent detection, integrations and agent state arbitration

Filed from the defect hunt over `crates/shepr-agent/src/detect/`,
`crates/shepr-agent/src/agent/`, `crates/shepr-mux/src/pane/agent_detection.rs`,
`crates/shepr-agent/src/integration/` (with its assets), and
`crates/shepr-mux/src/terminal` (the `TerminalState` arbitration between
detection and hook reports).

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## AGENT-011 - The per-pane detection state machine is spread over three files

Scope: agent-detection (structural note).

The state machine lives in pure deciders with single-use input structs in
`agent_detection.rs`, in `DetectorState` in `process_probe.rs`, and in the
orchestration loop in `runtime.rs`, which interleaves `spawn_blocking` probes,
OSC clearing, theme restore and publishing. AGENT-001, AGENT-002 and AGENT-003
are all interaction bugs between these pieces: an initial state in one file, a
skip rule in another, an early `continue` in the third. A single pure
`DetectorState::tick(Observations) -> TickOutput` (observations: foreground pgid,
probe result, content seq, screen and OSC, authority flag, `now`; output: events
to publish and the next wake) would put every transition in one testable
function and leave the runtime as I/O only.

## AGENT-024 - Nothing checks the integration assets against the server's acceptance contract

Scope: agent-integration (the hunter's main structural finding).

The TS tests assert which requests an asset emits, and the Rust state tests
anchor sessions by calling `set_persisted_agent_session` directly, so no test
feeds an asset's real request sequence into `TerminalState`. AGENT-012 through
AGENT-015 all fall through that gap.

Also, 14 assets each re-implement the envelope, socket, timeout and JSON code,
and the regex test `hook_assets_share_one_envelope` exists only to keep them in
line.

**Direction.** One cross-boundary harness that runs each asset (sh via a fake
socket, JS/TS via the existing bun tests) through a scripted agent session and
replays the captured JSON requests into a `TerminalState`, asserting on the
resulting hook authority and persisted session rather than request shapes. Pair
it with a server-side or explicit asset-side rule for a state report with no
session ref (AGENT-014). Generating the sh assets from one template at build time
would remove most of the duplicated surface and keep install byte-comparable
(AGENT-016). A hidden `shepr` reporter subcommand would do it more simply but
widens the CLI, which AGENTS.md keeps small on purpose; the hunter leaves that
choice to the owner.

The hunter lists as not verified, and not reported as defects: Copilot's hook
event spelling (`SessionStart` here, camelCase in Copilot's own hooks format) and
settings file name; whether Codex has an `Interrupt` hook event; OpenCode's
global plugin directory name (`plugins/` for OpenCode vs `plugin/` for Kilo).

## AGENT-009 - Seeded history in a restored plain shell reads as live agent chrome

Scope: agent-detection, pane runtime, restore.

Restore does not seed saved history for a pane that has an agent resume plan.
A restored plain shell still gets its history seeded onto the live screen, and if
an agent is started there later, broad detection regions (Claude's permission
and no-prompt blockers, Amp's approval footer, Cursor's approval prompt, Grok's
option dialog, Codex's timer fallback) can read the saved frame as live chrome
until it scrolls off. The screen snapshot cannot tell seeded rows from new
output, so no manifest rule can distinguish them. A capture test that seeds a
Claude dialog and writes a working frame is in
`crates/shepr-mux/src/pane/agent_detection.rs`. Fix at the restore and pane
runtime boundary: record which rows were seeded (or the seeded row count) so the
detector can exclude them, or clear the seeded rows' eligibility once new output
arrives.

## AGENT-034 - Typed report sources stop at the API edge

Scope: mux-terminal, server-app.

The arbitration is now one per-source ledger with explicit states, validation
precedes mutation, and per-agent quirks are descriptor policies. The API parses
official sources once for reference-policy validation, but internal events
(`StateEvent` and its reducer in `crates/shepr-server/src/app/`) still carry the
source and agent label as strings, so `TerminalState` re-parses them. Carry the
typed source through the event and the reducer. `crates/shepr-mux/src/limits.rs`
still documents the old silence-based re-anchor rule, which the ledger replaced
with server wall-clock versus monotonic-clock evidence; reword it.

## AGENT-032 - Dead data left on the hook and read paths

Scope: mux-terminal, server-app, api.

The unused authority message storage, the runtime serde derives and the unused
effective-change projections are gone. Left:

- The report's `message` still crosses the API and the internal event envelope
  though nothing reads it; drop it from the parameters, the event and the
  reducer together.
- `TerminalReadSnapshot`, its `truncated` flag
  (`crates/shepr-mux/src/pane/terminal/read_snapshot.rs`) and the four
  `recent_*_snapshot` methods have no production reader apart from history
  persistence, which ignores `truncated`. Remove what history does not use.
  (`recent_ansi_snapshot` and `recent_unwrapped_ansi_snapshot` in
  `pane/terminal/backend.rs` are now identical, and the `format.rs` module doc
  still names the unwrapped one.)

## AGENT-036 - Hook ordering reads the host clocks inside TerminalState

Scope: mux-terminal (from the review of the hook ledger).

The per-source hook ledger now accepts a non-increasing sequence as a clock step
only when the host wall clock has fallen behind its monotonic clock by the
threshold since the last acceptance. `HookSourceState::record_sequence` and
`hook_seq_superseded` read `Instant::now()` and `SystemTime::now()` themselves,
so the `now` parameters of `hook_seq_superseded` and `record_hook_seq` are dead
and the ordering tests' `t0 + delay` arguments no longer mean anything. Inject a
wall-clock sample next to the monotonic `now`, as the rest of the arbitration
does. Separately, a backward wall-clock step smaller than the 5 s threshold still
drops reports until the clock catches up, which can lose a final idle report;
decide whether a smaller step should be tolerated.

## AGENT-037 - A Claude session replaced by /clear may stay pinned to the old id

Scope: mux-terminal (lateral from review, unverified).

For an agent without full-lifecycle authority (Claude), a recognized replacement
start sets the persisted session, but the hook authority keeps the old session
ref, because only full-lifecycle authorities are released on replacement.
`current_session_identity_for_persistence` prefers the authority, and
`conflicting_same_owner_session_ref` rewrites the next state report's new id back
to the old one when it carries no start source. That could pin the pre-clear
session for Claude until something clears the authority, so restore would resume
the wrong conversation. Write a targeted test (Claude session A, SessionStart
with source clear and session B, then a state report for B) before changing
anything.
