//! Adapters between wire input and the pane terminal-core input model.
//!
//! Crossterm value mappings and payload accounting live on the wire types in
//! `shepr-protocol`. Conversion to `RawInputEvent` remains here because
//! that type belongs to `shepr-termio`, which the protocol must not depend on.

pub(crate) trait WirePaneInput: Sized {
    fn to_raw_input_event(&self) -> shepr_termio::input::raw_input::RawInputEvent;
}

impl WirePaneInput for shepr_protocol::ClientPaneInputEvent {
    fn to_raw_input_event(&self) -> shepr_termio::input::raw_input::RawInputEvent {
        match self {
            Self::Key {
                code,
                modifiers,
                kind,
                repeat_count,
                shifted_codepoint,
                generated_text,
            } => {
                let mut key =
                    shepr_termio::input::TerminalKey::new(code.to_host(), modifiers.to_host())
                        .with_kind(kind.to_host())
                        .with_repeat_count(*repeat_count)
                        .with_generated_text(generated_text.clone());
                if let Some(shifted_codepoint) = shifted_codepoint {
                    key = key.with_shifted_codepoint(*shifted_codepoint);
                }
                shepr_termio::input::raw_input::RawInputEvent::Key(key)
            }
            // Text commits are handled directly by pane input before this conversion.
            Self::TextCommit(_) => shepr_termio::input::raw_input::RawInputEvent::Unsupported,
            Self::Mouse {
                kind,
                position,
                modifiers,
                ..
            } => {
                let (column, row) = match position {
                    shepr_protocol::ClientMousePosition::Cell { column, row }
                    | shepr_protocol::ClientMousePosition::Pixels { column, row, .. } => {
                        (*column, *row)
                    }
                };
                shepr_termio::input::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: kind.to_host(),
                    column,
                    row,
                    modifiers: modifiers.to_host(),
                })
            }
            Self::Paste(text) => shepr_termio::input::raw_input::RawInputEvent::Paste(text.clone()),
        }
    }
}
