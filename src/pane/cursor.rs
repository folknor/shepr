//! Cursor shape reporting. Whether the child chose a shape (DECSCUSR 1-6,
//! OSC 50) or wants the terminal default (DECSCUSR 0, RIS) is tracked by the
//! terminal core's handler, in parser order; this only maps the core's shape
//! back to a DECSCUSR number.

/// The DECSCUSR parameter (1-6) for a cursor shape and blink state.
pub(crate) fn decscusr_cursor_shape(
    style: crate::ghostty::CursorVisualStyle,
    blinking: bool,
) -> u8 {
    match (style, blinking) {
        (crate::ghostty::CursorVisualStyle::Block, true)
        | (crate::ghostty::CursorVisualStyle::BlockHollow, true) => 1,
        (crate::ghostty::CursorVisualStyle::Block, false)
        | (crate::ghostty::CursorVisualStyle::BlockHollow, false) => 2,
        (crate::ghostty::CursorVisualStyle::Underline, true) => 3,
        (crate::ghostty::CursorVisualStyle::Underline, false) => 4,
        (crate::ghostty::CursorVisualStyle::Bar, true) => 5,
        (crate::ghostty::CursorVisualStyle::Bar, false) => 6,
    }
}
