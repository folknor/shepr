//! Direct terminal attach input parsing and semantic actions.

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use crate::protocol::{AttachScrollDirection, AttachScrollSource, ClientMessage};

pub(super) fn forward_input(write_stream: &mut super::endpoint::EndpointRegistry, data: &[u8]) {
    use super::endpoint::EndpointSendOutcome;

    let oversized = data.len() > crate::protocol::MAX_INPUT_PAYLOAD;
    let flush = |stream: &mut super::endpoint::EndpointRegistry| {
        stream.flush_active(std::time::Instant::now() + std::time::Duration::from_secs(5))
    };
    // The endpoint writer has a bounded queue. Stream a large paste through
    // it one frame at a time so splitting the frame does not simply overflow
    // that queue a few chunks later.
    if oversized && flush(write_stream) == EndpointSendOutcome::NotSent {
        return;
    }
    for message in input_messages(data) {
        if write_stream.send(&message) == EndpointSendOutcome::NotSent {
            break;
        }
        if oversized && flush(write_stream) == EndpointSendOutcome::NotSent {
            break;
        }
    }
}

fn input_messages(data: &[u8]) -> impl Iterator<Item = ClientMessage> + '_ {
    data.chunks(crate::protocol::MAX_INPUT_PAYLOAD)
        .map(|chunk| ClientMessage::Input {
            data: chunk.to_vec(),
        })
}

#[derive(Debug, Default)]
pub(super) struct AttachEscapeState {
    pending_prefix: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(super) enum AttachInputAction {
    Forward(Vec<u8>),
    ForwardPair(Vec<u8>, Vec<u8>),
    Semantic(AttachSemanticAction),
    ForwardThenSemantic(Vec<u8>, AttachSemanticAction),
    Detach,
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
        kind: crate::protocol::ClientMouseKind,
        position: crate::protocol::ClientMousePosition,
        modifiers: u8,
    },
    Ignore,
}

impl AttachEscapeState {
    pub(super) fn filter_input(
        &mut self,
        data: Vec<u8>,
        viewport_rows: u16,
        mouse_scroll_lines: usize,
    ) -> AttachInputAction {
        const PREFIX: u8 = 0x02; // Ctrl+B

        if crate::raw_input::is_complete_text_bracketed_paste(&data) {
            return if let Some(prefix) = self.pending_prefix.take() {
                AttachInputAction::ForwardPair(prefix, data)
            } else {
                AttachInputAction::Forward(data)
            };
        }

        if let Some(key) = single_attach_key(&data) {
            let is_prefix = key.code == crossterm::event::KeyCode::Char('b')
                && key.modifiers == crossterm::event::KeyModifiers::CONTROL;
            let is_quit = key.code == crossterm::event::KeyCode::Char('q')
                && key.modifiers.is_empty()
                && key.kind == crossterm::event::KeyEventKind::Press;

            if let Some(mut prefix) = self.pending_prefix.take() {
                if is_prefix && key.kind != crossterm::event::KeyEventKind::Press {
                    prefix.extend(data);
                    self.pending_prefix = Some(prefix);
                    return AttachInputAction::None;
                }
                if is_quit {
                    return AttachInputAction::Detach;
                }
                if is_prefix {
                    return AttachInputAction::Forward(data);
                }
                if let Some(action) = attach_scroll_action(&data, viewport_rows, mouse_scroll_lines)
                {
                    return AttachInputAction::ForwardThenSemantic(prefix, action);
                }
                prefix.extend(data);
                return AttachInputAction::Forward(prefix);
            }

            if is_prefix && key.kind == crossterm::event::KeyEventKind::Press {
                self.pending_prefix = Some(data);
                return AttachInputAction::None;
            }
        }

        if let Some(action) = attach_scroll_action(&data, viewport_rows, mouse_scroll_lines) {
            return if let Some(prefix) = self.pending_prefix.take() {
                AttachInputAction::ForwardThenSemantic(prefix, action)
            } else {
                AttachInputAction::Semantic(action)
            };
        }

        // The host framer normally supplies one complete event. Preserve the legacy
        // byte path for coalesced plain input used by older terminals.
        let mut output = Vec::with_capacity(data.len());
        for byte in data {
            if let Some(mut prefix) = self.pending_prefix.take() {
                match byte {
                    b'q' => return AttachInputAction::Detach,
                    PREFIX => output.extend(prefix),
                    other => {
                        prefix.push(other);
                        output.extend(prefix);
                    }
                }
                continue;
            }

            if byte == PREFIX {
                self.pending_prefix = Some(vec![PREFIX]);
            } else {
                output.push(byte);
            }
        }

        if output.is_empty() {
            AttachInputAction::None
        } else if let Some(action) =
            attach_scroll_action(&output, viewport_rows, mouse_scroll_lines)
        {
            AttachInputAction::Semantic(action)
        } else {
            AttachInputAction::Forward(output)
        }
    }

    pub(super) fn take_pending_prefix(&mut self) -> Option<Vec<u8>> {
        self.pending_prefix.take()
    }
}

fn single_attach_key(data: &[u8]) -> Option<crate::input::TerminalKey> {
    let mut events = crate::raw_input::parse_raw_input_bytes_sync(data);
    if events.len() != 1 {
        return None;
    }
    match events.pop()? {
        crate::raw_input::RawInputEvent::Key(key) => Some(key),
        _ => None,
    }
}

pub(super) fn direct_attach_pixel_mouse(
    data: &[u8],
    geometry: crate::input::mouse::HostGeometry,
) -> Option<(
    crate::protocol::ClientMouseKind,
    crate::protocol::ClientMousePosition,
    u8,
)> {
    let (x, y) = crate::input::mouse::parse_report(data)?;
    let (column, row) = geometry.cell(x, y)?;
    let cell_report = crate::input::mouse::report_at_cell(data, column, row)?;
    let mut events = crate::raw_input::parse_raw_input_bytes_sync(&cell_report);
    if events.len() != 1 {
        return None;
    }
    let crate::raw_input::RawInputEvent::Mouse(mouse) = events.pop()? else {
        return None;
    };
    Some((
        crate::protocol::ClientMouseKind::from_crossterm(mouse.kind)?,
        crate::protocol::ClientMousePosition::Pixels { x, y, column, row },
        mouse.modifiers.bits(),
    ))
}

fn attach_scroll_action(
    data: &[u8],
    viewport_rows: u16,
    mouse_scroll_lines: usize,
) -> Option<AttachSemanticAction> {
    let mut events = crate::raw_input::parse_raw_input_bytes_sync(data);
    if events.len() != 1 {
        return None;
    }

    match events.pop()? {
        crate::raw_input::RawInputEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let direction = if mouse.kind == MouseEventKind::ScrollUp {
                    AttachScrollDirection::Up
                } else {
                    AttachScrollDirection::Down
                };
                Some(AttachSemanticAction::Scroll {
                    source: AttachScrollSource::Wheel,
                    direction,
                    lines: u16::try_from(mouse_scroll_lines.max(1)).unwrap_or(u16::MAX),
                    column: Some(mouse.column),
                    row: Some(mouse.row),
                    modifiers: mouse.modifiers.bits(),
                })
            }
            kind => Some(AttachSemanticAction::Mouse {
                kind: crate::protocol::ClientMouseKind::from_crossterm(kind)?,
                position: crate::protocol::ClientMousePosition::Cell {
                    column: mouse.column,
                    row: mouse.row,
                },
                modifiers: mouse.modifiers.bits(),
            }),
        },
        crate::raw_input::RawInputEvent::Key(key)
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
        crate::raw_input::RawInputEvent::Key(key)
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
            modifiers,
        },
        AttachSemanticAction::Mouse {
            kind,
            position,
            modifiers,
        } => ClientMessage::AttachMouse {
            kind,
            position,
            geometry: None,
            modifiers,
            lines: 1,
        },
        AttachSemanticAction::Ignore => return None,
    };
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AttachScrollDirection, AttachScrollSource};

    #[test]
    fn oversized_attach_input_is_split_into_valid_frames() {
        let data = vec![b'x'; crate::protocol::MAX_FRAME_SIZE + 17];
        let mut reconstructed = Vec::new();
        let mut frames = 0;
        for message in input_messages(&data) {
            let ClientMessage::Input { data: chunk } = &message else {
                panic!("attach input must be sent as input");
            };
            assert!(chunk.len() <= crate::protocol::MAX_INPUT_PAYLOAD);
            reconstructed.extend_from_slice(chunk);
            let mut frame = Vec::new();
            crate::protocol::write_message(&mut frame, &message).expect("chunk must fit a frame");
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
            fn flush(&mut self, _deadline: std::time::Instant) -> std::io::Result<()> {
                if let Ok(mut sent) = self.0.lock() {
                    sent.flushes += 1;
                }
                Ok(())
            }
        }

        let sent = Arc::new(Mutex::new(Sent::default()));
        let mut registry = super::super::endpoint::EndpointRegistry::new(
            Capture(Arc::clone(&sent)),
            1,
            super::super::endpoint::EndpointNegotiation::default(),
        );
        forward_input(
            &mut registry,
            &vec![b'x'; crate::protocol::MAX_FRAME_SIZE + 17],
        );
        let sent = sent.lock().expect("test precondition");
        assert_eq!(
            sent.chunks.iter().sum::<usize>(),
            crate::protocol::MAX_FRAME_SIZE + 17
        );
        assert_eq!(sent.flushes, sent.chunks.len() + 1);
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
                kind: crate::protocol::ClientMouseKind::Down(
                    crate::protocol::ClientMouseButton::Left
                ),
                position: crate::protocol::ClientMousePosition::Cell { column: 10, row: 5 },
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
                    kind: crate::protocol::ClientMouseKind::Down(
                        crate::protocol::ClientMouseButton::Left
                    ),
                    position: crate::protocol::ClientMousePosition::Cell {
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
        let geometry =
            crate::input::mouse::HostGeometry::new(80, 24, 800, 480).expect("test precondition");
        let (kind, position, modifiers) =
            direct_attach_pixel_mouse(b"\x1b[<0;21;22M", geometry).expect("pixel mouse");

        assert_eq!(
            kind,
            crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left)
        );
        assert_eq!(
            position,
            crate::protocol::ClientMousePosition::Pixels {
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
                kind: crate::protocol::ClientMouseKind::Down(
                    crate::protocol::ClientMouseButton::Left
                ),
                position: crate::protocol::ClientMousePosition::Cell { column: 1, row: 2 },
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
