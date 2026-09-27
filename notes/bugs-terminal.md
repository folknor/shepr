# Defects: shepr-vt, shepr-pty, shepr-termio

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## TRM-020 - Adjudicated against: kitty report-all keys shepr cannot re-encode (comments owed)

The owner decided not to build a shepr-owned key model: panes run agents and shells, which do not need full kitty report-all fidelity. AGENTS.md records the decision. Before this entry is removed, the code sites need comments saying the gap is deliberate and pointing at that scope rule's content inline:
- `crates/shepr-termio/src/input/encode.rs`: `encode_kitty_functional_key` and `encode_f_key` (F13 and above, lock, media and bare modifier keys are not encoded), and the doc on `encode_terminal_key` ("using the pane's negotiated keyboard protocol" overstates it).
- `crates/shepr-termio/src/input/parse.rs`: `kitty_codepoint_to_keycode` (keypad codepoints collapse into plain keys; lock bits dropped by `key_modifiers_from_u8`).

## TRM-021 - Adjudicated against: modifyOtherKeys only for Enter, Esc, Tab and Backspace (comments owed)

Same decision as TRM-020. Code sites that need a comment before removal: `crates/shepr-termio/src/input/encode.rs` `encode_modify_other_keys`, and the doc on `KeyEncodeModes::modify_other_keys` (it says level 0, 1 or 2 without saying levels 1 and 2 only affect those four keys). AGENTS.md's terminal-core section also says shepr tracks modifyOtherKeys; that stays true (the level is tracked), but the new scope paragraph bounds what is encoded.
