//! Direct terminal attach input parsing and semantic actions.

use crate::input_wire::WireMouseKind;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use shepr_protocol::{AttachScrollDirection, AttachScrollSource, ClientMessage};

type KeyCombo = (KeyCode, KeyModifiers);

pub(super) fn paste_rejected_notice(size: usize, max: usize) -> String {
    format!("Paste is {size} bytes; Shepr's limit is {max} bytes")
}

/// What `forward_input` did with its bytes.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ForwardOutcome {
    /// Queued for the server (a failed write surfaces through the registry).
    Sent,
    /// A bracketed paste over the server's per-message input limit, dropped
    /// here. The caller owns telling the user.
    PasteRejected { size: usize, max: usize },
}

impl ForwardOutcome {
    /// The notice for a rejected paste.
    pub(super) fn notice(&self) -> Option<String> {
        match self {
            Self::Sent => None,
            Self::PasteRejected { size, max } => Some(paste_rejected_notice(*size, *max)),
        }
    }
}

pub(super) fn forward_input(
    write_stream: &mut super::endpoint::EndpointRegistry,
    data: &[u8],
    now: std::time::Instant,
) -> ForwardOutcome {
    use super::endpoint::EndpointSendOutcome;

    let max = shepr_protocol::MAX_INPUT_PAYLOAD;
    let oversized = data.len() > max;
    // A paste gets one limit whichever client it comes from. The server
    // handles a complete bracketed paste in one input message as a paste,
    // re-bracketed only if the pane enabled bracketed paste. Split across
    // messages, the host's raw markers would reach a pane that never asked
    // for them. The client shell rejects a paste over the same limit before
    // sending it.
    if oversized && shepr_termio::input::raw_input::is_complete_text_bracketed_paste(data) {
        tracing::warn!(
            size = data.len(),
            max,
            "paste over the input limit; not sending it"
        );
        return ForwardOutcome::PasteRejected {
            size: data.len(),
            max,
        };
    }
    let flush = |stream: &mut super::endpoint::EndpointRegistry| {
        stream.flush_active(now + crate::limits::ENDPOINT_WRITE_TIMEOUT)
    };
    // Other oversized input is plain bytes the server writes to the pane
    // as-is, so it can be split. The endpoint writer has a bounded queue:
    // stream it through one frame at a time so splitting does not simply
    // overflow that queue a few chunks later.
    if oversized && flush(write_stream) == EndpointSendOutcome::NotSent {
        return ForwardOutcome::Sent;
    }
    for message in input_messages(data) {
        if write_stream.send(&message) == EndpointSendOutcome::NotSent {
            break;
        }
        if oversized && flush(write_stream) == EndpointSendOutcome::NotSent {
            break;
        }
    }
    ForwardOutcome::Sent
}

fn input_messages(data: &[u8]) -> impl Iterator<Item = ClientMessage> + '_ {
    data.chunks(shepr_protocol::MAX_INPUT_PAYLOAD)
        .map(|chunk| ClientMessage::Input {
            data: chunk.to_vec(),
        })
}

/// The keys direct attach intercepts, from `keys.prefix` and `keys.detach`.
///
/// Prefix then a prefix-bound detach key (default `prefix+q`) detaches; a
/// directly bound detach key detaches on its own; the prefix twice sends one
/// literal prefix to the pane. Plain PageUp/PageDown and mouse input are
/// handled separately, by `attach_scroll_action`.
#[derive(Debug, Clone)]
pub(super) struct AttachKeys {
    prefix: KeyCombo,
    detach_after_prefix: Vec<KeyCombo>,
    detach_direct: Vec<KeyCombo>,
    /// Byte forms used by tests for coalesced legacy input.
    #[cfg(test)]
    legacy_prefix: Option<u8>,
    #[cfg(test)]
    legacy_detach_direct: Vec<u8>,
    #[cfg(test)]
    legacy_detach_after_prefix: Vec<Vec<u8>>,
}

impl AttachKeys {
    pub(super) fn from_config(config: &shepr_config::ValidatedConfig) -> Self {
        let live = config.live_keybinds();
        let mut detach_after_prefix = Vec::new();
        let mut detach_direct = Vec::new();
        for binding in &live.keybinds.detach.bindings {
            if binding.trigger.is_prefix() {
                detach_after_prefix.push(binding.trigger.combo());
            } else {
                detach_direct.push(binding.trigger.combo());
            }
        }
        // With `keys.detach` unset the client shell has no detach key, but it
        // still has its menus. Direct attach intercepts nothing else, so
        // without a key the only way out would be to kill the host terminal.
        if detach_after_prefix.is_empty() && detach_direct.is_empty() {
            detach_after_prefix.push((KeyCode::Char('q'), KeyModifiers::NONE));
        }
        Self {
            prefix: live.prefix,
            #[cfg(test)]
            legacy_prefix: legacy_control_byte(live.prefix),
            #[cfg(test)]
            legacy_detach_direct: detach_direct
                .iter()
                .filter_map(|combo| legacy_control_byte(*combo))
                .collect(),
            #[cfg(test)]
            legacy_detach_after_prefix: detach_after_prefix
                .iter()
                .filter_map(|combo| legacy_key_bytes(*combo))
                .collect(),
            detach_after_prefix,
            detach_direct,
        }
    }
}

#[cfg(test)]
impl Default for AttachKeys {
    fn default() -> Self {
        use shepr_test_fixtures::ValidatedConfigFixture as _;
        Self::from_config(&shepr_config::ValidatedConfig::test_default())
    }
}

/// The single C0 byte a legacy terminal sends for `combo`, if it has one.
/// Escape (ctrl+[) is left out: it starts every escape sequence.
#[cfg(test)]
fn legacy_control_byte(combo: KeyCombo) -> Option<u8> {
    let (KeyCode::Char(ch), modifiers) = shepr_config::normalize_key_combo(combo) else {
        return None;
    };
    if modifiers != KeyModifiers::CONTROL {
        return None;
    }
    match ch {
        'a'..='z' | '\\' | ']' | '^' | '_' => Some(ch as u8 & 0x1f),
        ' ' | '@' => Some(0x00),
        _ => None,
    }
}

/// The bytes a legacy terminal sends for `combo`: its control byte, or the
/// text of an unmodified or shifted character.
#[cfg(test)]
fn legacy_key_bytes(combo: KeyCombo) -> Option<Vec<u8>> {
    if let Some(byte) = legacy_control_byte(combo) {
        return Some(vec![byte]);
    }
    let (KeyCode::Char(ch), modifiers) = shepr_config::normalize_key_combo(combo) else {
        return None;
    };
    let ch = if modifiers.is_empty() {
        ch
    } else if modifiers == KeyModifiers::SHIFT && ch.is_ascii_lowercase() {
        ch.to_ascii_uppercase()
    } else {
        return None;
    };
    Some(ch.to_string().into_bytes())
}

fn matches_any(key: &shepr_termio::input::TerminalKey, combos: &[KeyCombo]) -> bool {
    combos
        .iter()
        .any(|combo| shepr_config::terminal_key_matches_combo(key, *combo))
}

#[derive(Debug)]
#[cfg_attr(test, derive(Default))]
pub(super) struct AttachEscapeState {
    keys: AttachKeys,
    pending_prefix: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(super) enum AttachInputAction {
    Forward(Vec<u8>),
    ForwardPair(Vec<u8>, Vec<u8>),
    Semantic(AttachSemanticAction),
    ForwardThenSemantic(Vec<u8>, AttachSemanticAction),
    Detach,
    /// Forward the input that came before the detach key (text coalesced into
    /// the same read, or a pending prefix a direct detach key cut short),
    /// then detach.
    ForwardThenDetach(Vec<u8>),
    None,
}

#[derive(Debug)]
pub(super) enum AttachSemanticAction {
    Scroll {
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    },
    Mouse {
        kind: shepr_protocol::ClientMouseKind,
        position: shepr_protocol::ClientMousePosition,
        modifiers: u8,
    },
    Ignore,
}

impl AttachEscapeState {
    /// An escape state intercepting the prefix and detach keys `config` sets.
    pub(super) fn from_config(config: &shepr_config::ValidatedConfig) -> Self {
        Self {
            keys: AttachKeys::from_config(config),
            pending_prefix: None,
        }
    }

    pub(super) fn filter_parsed_input(
        &mut self,
        data: Vec<u8>,
        event: &shepr_termio::input::raw_input::RawInputEvent,
        viewport_rows: u16,
        mouse_scroll_lines: u16,
    ) -> AttachInputAction {
        self.filter_parsed_input_inner(data, event, viewport_rows, mouse_scroll_lines)
    }

    #[cfg(test)]
    pub(super) fn filter_input(
        &mut self,
        data: Vec<u8>,
        viewport_rows: u16,
        mouse_scroll_lines: u16,
    ) -> AttachInputAction {
        let mut events = shepr_test_fixtures::parse_raw_input_bytes_sync(&data);
        if events.len() == 1 {
            let event = events.remove(0);
            return self.filter_parsed_input(data, &event, viewport_rows, mouse_scroll_lines);
        }
        self.filter_coalesced_test_input(&data)
    }

    #[cfg(test)]
    fn filter_coalesced_test_input(&mut self, data: &[u8]) -> AttachInputAction {
        let detach_with = |output: Vec<u8>| {
            if output.is_empty() {
                AttachInputAction::Detach
            } else {
                AttachInputAction::ForwardThenDetach(output)
            }
        };
        let mut output = Vec::with_capacity(data.len());
        let mut rest = data;
        while let Some(&byte) = rest.first() {
            if let Some(prefix) = self.pending_prefix.take() {
                if self
                    .keys
                    .legacy_detach_after_prefix
                    .iter()
                    .any(|detach| rest.starts_with(detach))
                {
                    return detach_with(output);
                }
                output.extend(prefix);
                if self.keys.legacy_prefix == Some(byte) {
                    rest = &rest[1..];
                }
                continue;
            }

            if self.keys.legacy_detach_direct.contains(&byte) {
                return detach_with(output);
            }
            if self.keys.legacy_prefix == Some(byte) {
                self.pending_prefix = Some(vec![byte]);
            } else {
                output.push(byte);
            }
            rest = &rest[1..];
        }

        if output.is_empty() {
            AttachInputAction::None
        } else {
            AttachInputAction::Forward(output)
        }
    }

    fn filter_parsed_input_inner(
        &mut self,
        data: Vec<u8>,
        event: &shepr_termio::input::raw_input::RawInputEvent,
        viewport_rows: u16,
        mouse_scroll_lines: u16,
    ) -> AttachInputAction {
        if matches!(
            event,
            shepr_termio::input::raw_input::RawInputEvent::Paste(_)
        ) {
            return if let Some(prefix) = self.pending_prefix.take() {
                AttachInputAction::ForwardPair(prefix, data)
            } else {
                AttachInputAction::Forward(data)
            };
        }

        if let shepr_termio::input::raw_input::RawInputEvent::Key(key) = event {
            let press = key.kind == KeyEventKind::Press;
            let is_prefix = shepr_config::terminal_key_matches_combo(key, self.keys.prefix);
            let is_direct_detach = press && matches_any(key, &self.keys.detach_direct);

            if let Some(mut prefix) = self.pending_prefix.take() {
                if is_prefix && !press {
                    prefix.extend(data);
                    self.pending_prefix = Some(prefix);
                    return AttachInputAction::None;
                }
                if press && matches_any(key, &self.keys.detach_after_prefix) {
                    return AttachInputAction::Detach;
                }
                if is_prefix {
                    return AttachInputAction::Forward(data);
                }
                if is_direct_detach {
                    return AttachInputAction::ForwardThenDetach(prefix);
                }
                if let Some(action) =
                    attach_scroll_action(event, &data, viewport_rows, mouse_scroll_lines)
                {
                    return AttachInputAction::ForwardThenSemantic(prefix, action);
                }
                prefix.extend(data);
                return AttachInputAction::Forward(prefix);
            }

            if is_direct_detach {
                return AttachInputAction::Detach;
            }
            if is_prefix && press {
                self.pending_prefix = Some(data);
                return AttachInputAction::None;
            }
        }

        if let Some(action) = attach_scroll_action(event, &data, viewport_rows, mouse_scroll_lines)
        {
            return if let Some(prefix) = self.pending_prefix.take() {
                AttachInputAction::ForwardThenSemantic(prefix, action)
            } else {
                AttachInputAction::Semantic(action)
            };
        }

        if let Some(mut prefix) = self.pending_prefix.take() {
            prefix.extend(data);
            AttachInputAction::Forward(prefix)
        } else {
            AttachInputAction::Forward(data)
        }
    }

    pub(super) fn take_pending_prefix(&mut self) -> Option<Vec<u8>> {
        self.pending_prefix.take()
    }
}

pub(super) fn direct_attach_pixel_mouse(
    event: &shepr_termio::input::raw_input::RawInputEvent,
    pixels: shepr_termio::input::mouse::HostPixels,
) -> Option<(
    shepr_protocol::ClientMouseKind,
    shepr_protocol::ClientMousePosition,
    u8,
)> {
    let shepr_termio::input::raw_input::RawInputEvent::Mouse(mouse) = event else {
        return None;
    };
    let (column, row) = pixels.geometry.cell(pixels.x, pixels.y)?;
    Some((
        shepr_protocol::ClientMouseKind::from_crossterm(mouse.kind)?,
        shepr_protocol::ClientMousePosition::Pixels {
            x: pixels.x,
            y: pixels.y,
            column,
            row,
        },
        mouse.modifiers.bits(),
    ))
}

fn attach_scroll_action(
    event: &shepr_termio::input::raw_input::RawInputEvent,
    data: &[u8],
    viewport_rows: u16,
    mouse_scroll_lines: u16,
) -> Option<AttachSemanticAction> {
    match event {
        shepr_termio::input::raw_input::RawInputEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let direction = if mouse.kind == MouseEventKind::ScrollUp {
                    AttachScrollDirection::Up
                } else {
                    AttachScrollDirection::Down
                };
                Some(AttachSemanticAction::Scroll {
                    source: AttachScrollSource::Wheel,
                    direction,
                    lines: mouse_scroll_lines,
                    column: Some(mouse.column),
                    row: Some(mouse.row),
                    modifiers: mouse.modifiers.bits(),
                })
            }
            kind => Some(AttachSemanticAction::Mouse {
                kind: shepr_protocol::ClientMouseKind::from_crossterm(kind)?,
                position: shepr_protocol::ClientMousePosition::Cell {
                    column: mouse.column,
                    row: mouse.row,
                },
                modifiers: mouse.modifiers.bits(),
            }),
        },
        shepr_termio::input::raw_input::RawInputEvent::Key(key)
            if key.modifiers.is_empty()
                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            let direction = match key.code {
                KeyCode::PageUp => AttachScrollDirection::Up,
                KeyCode::PageDown => AttachScrollDirection::Down,
                _ => return None,
            };
            Some(AttachSemanticAction::Scroll {
                source: AttachScrollSource::PageKey {
                    input: data.to_vec(),
                },
                direction,
                lines: viewport_rows.saturating_sub(1).max(1),
                column: None,
                row: None,
                modifiers: KeyModifiers::empty().bits(),
            })
        }
        shepr_termio::input::raw_input::RawInputEvent::Key(key)
            if key.modifiers.is_empty()
                && key.kind == KeyEventKind::Release
                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) =>
        {
            Some(AttachSemanticAction::Ignore)
        }
        _ => None,
    }
}

/// The server message for a semantic attach action, or `None` for one that sends nothing.
pub(super) fn attach_semantic_message(action: AttachSemanticAction) -> Option<ClientMessage> {
    let message = match action {
        AttachSemanticAction::Scroll {
            source,
            direction,
            lines,
            column,
            row,
            modifiers,
        } => ClientMessage::AttachScroll {
            source,
            direction,
            lines,
            column,
            row,
            modifiers: shepr_protocol::WireModifiers::from_bits_retain(modifiers),
        },
        AttachSemanticAction::Mouse {
            kind,
            position,
            modifiers,
        } => ClientMessage::AttachMouse {
            kind,
            position,
            geometry: None,
            modifiers: shepr_protocol::WireModifiers::from_bits_retain(modifiers),
            lines: 1,
        },
        AttachSemanticAction::Ignore => return None,
    };
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{AttachScrollDirection, AttachScrollSource};
    use shepr_test_fixtures::*;

    #[test]
    fn oversized_attach_input_is_split_into_valid_frames() {
        let data = vec![b'x'; shepr_protocol::MAX_FRAME_SIZE + 17];
        let mut reconstructed = Vec::new();
        let mut frames = 0;
        for message in input_messages(&data) {
            let ClientMessage::Input { data: chunk } = &message else {
                panic!("attach input must be sent as input");
            };
            assert!(chunk.len() <= shepr_protocol::MAX_INPUT_PAYLOAD);
            reconstructed.extend_from_slice(chunk);
            let mut frame = Vec::new();
            shepr_protocol::write_message(&mut frame, &message).expect("chunk must fit a frame");
            frames += 1;
        }
        assert!(frames > 1);
        assert_eq!(reconstructed, data);
    }

    #[test]
    fn oversized_attach_input_waits_for_each_chunk_to_drain() {
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Sent {
            chunks: Vec<usize>,
            flushes: usize,
        }
        struct Capture(Arc<Mutex<Sent>>);
        impl super::super::endpoint::EndpointTransport for Capture {
            fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
                if let ClientMessage::Input { data } = message
                    && let Ok(mut sent) = self.0.lock()
                {
                    sent.chunks.push(data.len());
                }
                Ok(())
            }

            fn disconnect(&mut self) {}

            fn flush(&mut self, _deadline: std::time::Instant) -> std::io::Result<()> {
                if let Ok(mut sent) = self.0.lock() {
                    sent.flushes += 1;
                }
                Ok(())
            }

            fn take_error(&mut self) -> Option<std::io::Error> {
                None
            }
        }

        let sent = Arc::new(Mutex::new(Sent::default()));
        let mut registry =
            super::super::endpoint::EndpointRegistry::new(Capture(Arc::clone(&sent)), 1);
        forward_input(
            &mut registry,
            &vec![b'x'; shepr_protocol::MAX_FRAME_SIZE + 17],
            std::time::Instant::now(),
        );
        let sent = sent.lock().expect("test precondition");
        assert_eq!(
            sent.chunks.iter().sum::<usize>(),
            shepr_protocol::MAX_FRAME_SIZE + 17
        );
        assert_eq!(sent.flushes, sent.chunks.len() + 1);
    }
    #[test]
    fn oversized_attach_paste_is_rejected_whole() {
        use std::sync::{Arc, Mutex};

        struct Capture(Arc<Mutex<usize>>);
        impl super::super::endpoint::EndpointTransport for Capture {
            fn send(&mut self, _message: &ClientMessage) -> std::io::Result<()> {
                if let Ok(mut sent) = self.0.lock() {
                    *sent += 1;
                }
                Ok(())
            }

            fn disconnect(&mut self) {}

            fn flush(&mut self, _deadline: std::time::Instant) -> std::io::Result<()> {
                Ok(())
            }

            fn take_error(&mut self) -> Option<std::io::Error> {
                None
            }
        }

        let sent = Arc::new(Mutex::new(0));
        let mut registry =
            super::super::endpoint::EndpointRegistry::new(Capture(Arc::clone(&sent)), 1);
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend(vec![b'x'; shepr_protocol::MAX_INPUT_PAYLOAD]);
        paste.extend_from_slice(b"\x1b[201~");

        let outcome = forward_input(&mut registry, &paste, std::time::Instant::now());
        assert_eq!(
            outcome,
            ForwardOutcome::PasteRejected {
                size: paste.len(),
                max: shepr_protocol::MAX_INPUT_PAYLOAD,
            }
        );
        assert!(
            outcome
                .notice()
                .is_some_and(|notice| notice.contains("limit"))
        );
        assert_eq!(*sent.lock().expect("test precondition"), 0);

        let small = b"\x1b[200~hello\x1b[201~";
        assert_eq!(
            forward_input(&mut registry, small, std::time::Instant::now()),
            ForwardOutcome::Sent
        );
        assert_eq!(*sent.lock().expect("test precondition"), 1);
    }

    fn escape_for(config: &str) -> AttachEscapeState {
        let values: shepr_config::Config = toml::from_str(config).expect("test precondition");
        let config = shepr_config::ValidatedConfig::test_from_config(values, Some(config));
        AttachEscapeState::from_config(&config)
    }

    #[test]
    fn attach_escape_uses_the_configured_prefix() {
        let mut escape = escape_for("[keys]\nprefix = \"ctrl+a\"\n");
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::Forward(bytes) if bytes == vec![0x02]
        ));
        assert!(matches!(
            escape.filter_input(vec![0x01], 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(vec![b'q'], 24, 3),
            AttachInputAction::Detach
        ));

        let mut escape = escape_for("[keys]\nprefix = \"ctrl+a\"\n");
        assert!(matches!(
            escape.filter_input(b"\x1b[97;5u".to_vec(), 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(b"\x1b[113u".to_vec(), 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[test]
    fn attach_escape_uses_the_configured_detach_key() {
        let mut escape = escape_for("[keys]\ndetach = \"prefix+d\"\n");
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(vec![b'q'], 24, 3),
            AttachInputAction::Forward(bytes) if bytes == vec![0x02, b'q']
        ));
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(vec![b'd'], 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[test]
    fn attach_escape_detaches_on_a_direct_detach_key() {
        let mut escape = escape_for("[keys]\ndetach = \"f10\"\n");
        assert!(matches!(
            escape.filter_input(b"\x1b[21~".to_vec(), 24, 3),
            AttachInputAction::Detach
        ));

        let mut escape = escape_for("[keys]\ndetach = \"ctrl+g\"\n");
        assert!(matches!(
            escape.filter_input(b"ab\x07cd".to_vec(), 24, 3),
            AttachInputAction::ForwardThenDetach(bytes) if bytes == b"ab"
        ));
    }

    #[test]
    fn attach_escape_keeps_a_way_out_when_detach_is_unset() {
        let mut escape = escape_for("[keys]\ndetach = \"\"\n");
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(vec![b'q'], 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[test]
    fn coalesced_input_before_prefix_q_is_forwarded_before_detaching() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"abc\x02q".to_vec(), 24, 3),
            AttachInputAction::ForwardThenDetach(bytes) if bytes == b"abc"
        ));

        let mut escape = escape_for("[keys]\nprefix = \"ctrl+a\"\n");
        assert!(matches!(
            escape.filter_input(b"ab\x02q".to_vec(), 24, 3),
            AttachInputAction::Forward(bytes) if bytes == b"ab\x02q"
        ));
        assert!(matches!(
            escape.filter_input(b"ab\x01q".to_vec(), 24, 3),
            AttachInputAction::ForwardThenDetach(bytes) if bytes == b"ab"
        ));
    }

    #[test]
    fn coalesced_double_prefix_sends_one_literal_prefix() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"a\x02\x02b".to_vec(), 24, 3),
            AttachInputAction::Forward(bytes) if bytes == b"a\x02b"
        ));
        assert!(matches!(
            escape.filter_input(b"a\x02xb".to_vec(), 24, 3),
            AttachInputAction::Forward(bytes) if bytes == b"a\x02xb"
        ));
    }

    #[test]
    fn attach_escape_detaches_on_prefix_q() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(vec![b'q'], 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[test]
    fn attach_escape_sends_literal_prefix_on_double_prefix() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        match escape.filter_input(vec![0x02], 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, vec![0x02]),
            other => panic!("expected forwarded prefix, got {other:?}"),
        }
    }

    #[test]
    fn attach_escape_detaches_on_kitty_encoded_prefix_q() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"\x1b[98;5u".to_vec(), 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(b"\x1b[98;5:3u".to_vec(), 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(b"\x1b[113u".to_vec(), 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[test]
    fn attach_escape_detaches_on_modify_other_keys_encoded_prefix() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"\x1b[27;5;98~".to_vec(), 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(b"q".to_vec(), 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[test]
    fn attach_escape_forwards_kitty_encoded_literal_prefix() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"\x1b[98;5u".to_vec(), 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(b"\x1b[98;5:3u".to_vec(), 24, 3),
            AttachInputAction::None
        ));
        match escape.filter_input(b"\x1b[98;5u".to_vec(), 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, b"\x1b[98;5u"),
            other => panic!("expected Kitty-encoded prefix, got {other:?}"),
        }
    }

    #[test]
    fn attach_escape_does_not_interpret_bracketed_paste_contents() {
        let mut escape = AttachEscapeState::default();
        let paste = b"\x1b[200~one\x02q\ntwo\x1b[201~".to_vec();

        match escape.filter_input(paste.clone(), 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, paste),
            other => panic!("expected opaque paste, got {other:?}"),
        }
    }

    #[test]
    fn attach_escape_flushes_pending_prefix_before_bracketed_paste() {
        let mut escape = AttachEscapeState::default();
        let paste = b"\x1b[200~one\ntwo\x1b[201~".to_vec();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));

        assert!(matches!(
            escape.filter_input(paste.clone(), 24, 3),
            AttachInputAction::ForwardPair(prefix, bytes)
                if prefix == vec![0x02] && bytes == paste
        ));
        assert!(matches!(
            escape.filter_input(vec![b'q'], 24, 3),
            AttachInputAction::Forward(bytes) if bytes == b"q"
        ));
    }

    #[test]
    fn attach_escape_forwards_prefix_before_non_escape_key() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![b'a', 0x02], 24, 3),
            AttachInputAction::Forward(bytes) if bytes == b"a"
        ));
        match escape.filter_input(vec![b'x'], 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, vec![0x02, b'x']),
            other => panic!("expected forwarded bytes, got {other:?}"),
        }
    }

    #[test]
    fn attach_escape_turns_wheel_into_scroll_action() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[<64;11;6M".to_vec(), 24, 7) {
            AttachInputAction::Semantic(AttachSemanticAction::Scroll {
                source,
                direction,
                lines,
                column,
                row,
                ..
            }) => {
                assert_eq!(source, AttachScrollSource::Wheel);
                assert_eq!(direction, AttachScrollDirection::Up);
                assert_eq!(lines, 7);
                assert_eq!(column, Some(10));
                assert_eq!(row, Some(5));
            }
            other => panic!("expected scroll action, got {other:?}"),
        }
    }

    #[test]
    fn attach_escape_routes_non_wheel_mouse_reports_semantically() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"\x1b[<0;11;6M".to_vec(), 24, 7),
            AttachInputAction::Semantic(AttachSemanticAction::Mouse {
                kind: shepr_protocol::ClientMouseKind::Down(
                    shepr_protocol::ClientMouseButton::Left
                ),
                position: shepr_protocol::ClientMousePosition::Cell { column: 10, row: 5 },
                modifiers: 0,
            })
        ));
    }

    #[test]
    fn attach_escape_flushes_pending_prefix_before_cell_mouse() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));

        assert!(matches!(
            escape.filter_input(b"\x1b[<0;11;6M".to_vec(), 24, 7),
            AttachInputAction::ForwardThenSemantic(
                prefix,
                AttachSemanticAction::Mouse {
                    kind: shepr_protocol::ClientMouseKind::Down(
                        shepr_protocol::ClientMouseButton::Left
                    ),
                    position: shepr_protocol::ClientMousePosition::Cell {
                        column: 10,
                        row: 5
                    },
                    modifiers: 0,
                }
            ) if prefix == vec![0x02]
        ));
    }

    #[test]
    fn direct_attach_pixel_mouse_keeps_pixels_and_semantic_kind() {
        let geometry = shepr_termio::input::mouse::HostPixelExtent::new(80, 24, 800, 480)
            .expect("test precondition");
        let mut events = shepr_test_fixtures::parse_raw_input_bytes_sync(b"\x1b[<0;21;22M");
        let Some(shepr_termio::input::raw_input::RawInputEvent::Mouse(mouse)) = events.pop() else {
            panic!("expected parsed pixel mouse");
        };
        let pixels = shepr_termio::input::mouse::HostPixels {
            x: 21,
            y: 22,
            geometry,
        };
        let (kind, position, modifiers) = direct_attach_pixel_mouse(
            &shepr_termio::input::raw_input::RawInputEvent::Mouse(mouse),
            pixels,
        )
        .expect("pixel mouse");

        assert_eq!(
            kind,
            shepr_protocol::ClientMouseKind::Down(shepr_protocol::ClientMouseButton::Left)
        );
        assert_eq!(
            position,
            shepr_protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            }
        );
        assert_eq!(modifiers, 0);
    }

    #[test]
    fn pixel_mouse_flushes_pending_attach_prefix() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));

        assert_eq!(escape.take_pending_prefix(), Some(vec![0x02]));
        assert_eq!(escape.take_pending_prefix(), None);
    }

    #[test]
    fn attach_escape_turns_plain_page_keys_into_scroll_actions() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[5~".to_vec(), 12, 3) {
            AttachInputAction::Semantic(AttachSemanticAction::Scroll {
                source,
                direction,
                lines,
                ..
            }) => {
                assert_eq!(
                    source,
                    AttachScrollSource::PageKey {
                        input: b"\x1b[5~".to_vec()
                    }
                );
                assert_eq!(direction, AttachScrollDirection::Up);
                assert_eq!(lines, 11);
            }
            other => panic!("expected page-up scroll action, got {other:?}"),
        }

        match escape.filter_input(b"\x1b[6~".to_vec(), 12, 3) {
            AttachInputAction::Semantic(AttachSemanticAction::Scroll {
                source,
                direction,
                lines,
                ..
            }) => {
                assert_eq!(
                    source,
                    AttachScrollSource::PageKey {
                        input: b"\x1b[6~".to_vec()
                    }
                );
                assert_eq!(direction, AttachScrollDirection::Down);
                assert_eq!(lines, 11);
            }
            other => panic!("expected page-down scroll action, got {other:?}"),
        }
    }

    #[test]
    fn ignored_semantic_action_sends_nothing() {
        assert!(attach_semantic_message(AttachSemanticAction::Ignore).is_none());
        assert!(matches!(
            attach_semantic_message(AttachSemanticAction::Mouse {
                kind: shepr_protocol::ClientMouseKind::Down(
                    shepr_protocol::ClientMouseButton::Left
                ),
                position: shepr_protocol::ClientMousePosition::Cell { column: 1, row: 2 },
                modifiers: 0,
            }),
            Some(ClientMessage::AttachMouse { lines: 1, .. })
        ));
    }

    #[test]
    fn attach_escape_forwards_modified_page_key() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[5;5~".to_vec(), 12, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, b"\x1b[5;5~"),
            other => panic!("expected modified page key to forward, got {other:?}"),
        }
    }
}
