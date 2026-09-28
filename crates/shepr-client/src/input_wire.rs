//! Adapters between terminal-core input and the shared wire input types.
//!
//! Crossterm value mappings and payload accounting live on the wire types in
//! `shepr-protocol`. These adapters remain at the client edge because
//! `TerminalKey` belongs to `shepr-termio`, which the protocol must not depend on.

pub(crate) fn wire_modifiers(
    modifiers: crossterm::event::KeyModifiers,
) -> shepr_protocol::WireModifiers {
    shepr_protocol::WireModifiers::from_host(modifiers)
}

pub(crate) trait WireMouseButton: Sized {
    fn from_crossterm(button: crossterm::event::MouseButton) -> Self;
}

impl WireMouseButton for shepr_protocol::ClientMouseButton {
    fn from_crossterm(button: crossterm::event::MouseButton) -> Self {
        Self::from_host(button)
    }
}

pub(crate) trait WireMouseKind: Sized {
    fn from_crossterm(kind: crossterm::event::MouseEventKind) -> Option<Self>;
}

impl WireMouseKind for shepr_protocol::ClientMouseKind {
    fn from_crossterm(kind: crossterm::event::MouseEventKind) -> Option<Self> {
        Some(Self::from_host(kind))
    }
}

pub(crate) trait WirePaneInput: Sized {
    fn from_terminal_key(key: shepr_termio::input::TerminalKey) -> Option<Self>;
}

impl WirePaneInput for shepr_protocol::ClientPaneInputEvent {
    fn from_terminal_key(key: shepr_termio::input::TerminalKey) -> Option<Self> {
        Some(Self::Key {
            code: shepr_protocol::ClientKeyCode::from_host(key.code)?,
            modifiers: shepr_protocol::WireModifiers::from_host(key.modifiers),
            kind: shepr_protocol::ClientKeyKind::from_host(key.kind),
            repeat_count: key.repeat_count,
            shifted_codepoint: key.shifted_codepoint,
            generated_text: key.generated_text,
        })
    }
}
