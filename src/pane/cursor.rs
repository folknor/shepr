//! Cursor shape reporting. Whether the child chose a shape (DECSCUSR 1-6,
//! OSC 50) or wants the terminal default (DECSCUSR 0, RIS) is tracked by the
//! terminal core's handler, in parser order; this only maps the core's shape
//! back to a DECSCUSR number.

/// The DECSCUSR parameter (1-6) for a cursor shape and blink state.
pub(crate) fn decscusr_cursor_shape(
    style: shepr_vt::CursorVisualStyle,
    blinking: bool,
) -> shepr_protocol::CursorShapeParam {
    use shepr_protocol::CursorShapeParam;
    match (style, blinking) {
        (shepr_vt::CursorVisualStyle::Block, true)
        | (shepr_vt::CursorVisualStyle::BlockHollow, true) => CursorShapeParam::BlinkingBlock,
        (shepr_vt::CursorVisualStyle::Block, false)
        | (shepr_vt::CursorVisualStyle::BlockHollow, false) => CursorShapeParam::SteadyBlock,
        (shepr_vt::CursorVisualStyle::Underline, true) => CursorShapeParam::BlinkingUnderline,
        (shepr_vt::CursorVisualStyle::Underline, false) => CursorShapeParam::SteadyUnderline,
        (shepr_vt::CursorVisualStyle::Bar, true) => CursorShapeParam::BlinkingBar,
        (shepr_vt::CursorVisualStyle::Bar, false) => CursorShapeParam::SteadyBar,
    }
}
