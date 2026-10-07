# Bugs: terminal core (shepr-term, shepr-vt, shepr-pty)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the terminal core hunt. The raw report, including its list of areas
checked and found sound, is in commit 6dc81572 (`notes/hunt-terminal-core.md`).

## TERM-004 - UiPalette does not check text contrast against `surface1`, a surface the client draws text on

Where: `crates/shepr-term/src/host_tint.rs`, `UiPalette::derive_toward`: the
`surfaces` list that `on_surfaces` pushes text and hues against is
`[background, panel_bg, active_row_bg, selection_bg, surface0]`. `surface1`
(contrast target 1.8, the furthest neutral from the background) and
`surface_dim` are computed afterwards and never enter it.

Claim broken: the `UiPalette::derive` doc, "Text and the hues then keep their
contrast against every surface they are drawn on."

Evidence that text is drawn on it: copy-mode search matches use
`Style::default().fg(palette.text).bg(palette.surface1)` in
`crates/shepr-client/src/shell/view/draw.rs`. On a low-contrast theme where
`text` was pushed only just to 4.5:1 against the listed surfaces, its contrast
against `surface1` can fall well below that, and the `readable` flag that picks
the surface direction never sees it. Direction: include `surface1` in the
surface set (and anything else that is ever a text background), or narrow the
doc to the surfaces actually checked.

## TERM-008 - An OSC 52 store with an empty target, cut at the parser bound, still vanishes silently

Raised as a lateral by the wave reviewer.

The scanner (`crates/shepr-vt/src/scan.rs`) now reports a minimum dropped size
for an OSC 52 store it cuts at `MAX_OSC_RAW_BYTES`, for the `c`, `p` and `s`
targets. `52;;<payload>` (an empty target, which alacritty treats as a
selection store) is not among the recognised prefixes. A cut store of that
form is 2 mod 4 long, so it never decodes either, and it vanishes with no
diagnostic. (The `|| byte == b'='` beside `!is_base64_byte(byte)` there is
redundant.)

## TERM-009 - The scanner dispatches an OSC cancelled by CAN or SUB

Raised as a lateral by the wave reviewer.

The CAN and SUB arm of the scanner's OSC state calls `dispatch_osc` on the body
rather than discarding it, and a comment in `crates/shepr-vt/src/lib.rs`
assumes vte does the same with CAN. xterm discards an OSC cancelled by CAN.
Confirm against the pinned vte what it does, and make the scanner match it, so
shepr does not act on (or report the size of) an OSC the emulator dropped.
