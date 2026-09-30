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

## AGENT-038 - The pi and omp reporters still send a message the server ignores

Scope: agent-integration (lateral from review).

The server no longer reads a state report's `message`, and a test pins that an
extra field is still accepted. The pi and omp assets
(`crates/shepr-agent/src/integration/assets/pi/shepr-agent-state.ts` and the omp
one) still build and send it. Drop it from the assets, and the fixture seam
`set_hook_authority_at` in `crates/shepr-mux/src/terminal/state/hooks.rs` still
takes an ignored message argument at about 112 test call sites; remove it with
the assets so nothing on the path carries a message any more.
