//! Adapts terminal-core keys to the shared wire input types.
//!
//! Crossterm value mappings and payload accounting live on the wire types in
//! `shepr-protocol`. This adapter remains at the client edge because
//! `TerminalKey` belongs to `shepr-termio`, which the protocol must not depend on.

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
