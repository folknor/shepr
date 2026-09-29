use crate::server::input_wire::WirePaneInput;
use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use shepr_protocol::ClientPaneInputEvent;

/// Why one piece of pane input did not reach the PTY.
///
/// Input is handed to the PTY actor with a non-blocking send. The actor's
/// inbox holds up to 256 KiB (and a thousand items) of unwritten input and
/// replies, so it is only full when the child has stopped reading its
/// terminal (suspended, wedged, or flooded). Waiting for room is not an option: the server event loop is
/// shared by every pane and client, and blocking it on one stuck child would
/// freeze all of them. Queuing elsewhere would only grow an unbounded backlog
/// for a process that is not consuming it, and would have to preserve order
/// against later sends. So a full queue drops the input and reports it as
/// `Backpressure`; shell callers surface that to the user rather than only
/// logging it.
///
/// Callers log these errors, so they must never carry what was typed or
/// pasted: every variant holds a static label or a message that names only the
/// kind of input. Keep it that way, and keep input text and bytes out of every
/// `tracing` call on the input path (log lengths and kinds instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PaneInputError {
    /// The PTY input queue is full; the input was dropped.
    Backpressure(&'static str),
    /// The PTY actor no longer accepts input (the pane is shutting down).
    Closed(&'static str),
    /// The input could not be encoded or is not pane input.
    Other(String),
}

impl std::fmt::Display for PaneInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backpressure(what) => write!(f, "{what} dropped: pane input queue is full"),
            Self::Closed(what) => write!(f, "{what} dropped: pane no longer accepts input"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// Every failure from one batch of pane input events. A batch is always
/// applied to the end, so one failed event never swallows the events after it
/// (a key or button release in particular).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PaneInputFailures(Vec<PaneInputError>);

impl PaneInputFailures {
    /// Number of events dropped because the PTY input queue was full.
    pub(super) fn dropped_for_backpressure(&self) -> usize {
        self.0
            .iter()
            .filter(|error| matches!(error, PaneInputError::Backpressure(_)))
            .count()
    }
}

impl std::fmt::Display for PaneInputFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

fn send_input(
    runtime: &shepr_mux::pane::PaneRuntime,
    bytes: Bytes,
    what: &'static str,
) -> Result<(), PaneInputError> {
    runtime
        .try_send_bytes(bytes)
        .map_err(|err| send_error(&err, what))
}

fn send_paste(
    runtime: &shepr_mux::pane::PaneRuntime,
    text: String,
    what: &'static str,
) -> Result<(), PaneInputError> {
    runtime
        .try_send_paste(text)
        .map_err(|err| send_error(&err, what))
}

fn send_error(err: &shepr_pty::ChildIoSendError, what: &'static str) -> PaneInputError {
    match err {
        shepr_pty::ChildIoSendError::Full(_) => PaneInputError::Backpressure(what),
        shepr_pty::ChildIoSendError::Closed(_) => PaneInputError::Closed(what),
    }
}

pub(super) fn downgrade_ineligible_pixel_mouse(
    events: &mut [ClientPaneInputEvent],
    pixel_mouse: bool,
    runtime_size: shepr_core::geometry::GridSize,
    runtime_pixels: Option<(u32, u32)>,
) {
    let (runtime_rows, runtime_cols) = (runtime_size.rows.get(), runtime_size.cols.get());
    for event in events {
        let ClientPaneInputEvent::Mouse {
            position, geometry, ..
        } = event
        else {
            continue;
        };
        let shepr_protocol::ClientMousePosition::Pixels { x, y, column, row } = *position else {
            continue;
        };
        let exact = pixel_mouse
            && geometry.is_some_and(|geometry| {
                (runtime_rows, runtime_cols) == (geometry.rows, geometry.cols)
                    && runtime_pixels == Some((geometry.width_px, geometry.height_px))
                    && column < geometry.cols
                    && row < geometry.rows
                    && x > 0
                    && y > 0
                    && x <= geometry.width_px
                    && y <= geometry.height_px
            });
        if !exact {
            *position = shepr_protocol::ClientMousePosition::Cell { column, row };
            *geometry = None;
        }
    }
}

/// Which way a wheel event scrolls.
#[derive(Clone, Copy)]
enum ScrollDirection {
    Up,
    Down,
}

fn apply_scroll(
    runtime: &shepr_mux::pane::PaneRuntime,
    direction: ScrollDirection,
    lines: u16,
    position: shepr_termio::input::mouse::Position,
    modifiers: u8,
) -> Result<(), PaneInputError> {
    let wheel_kind = match direction {
        ScrollDirection::Up => MouseEventKind::ScrollUp,
        ScrollDirection::Down => MouseEventKind::ScrollDown,
    };

    match runtime.wheel_routing() {
        Some(shepr_mux::pane::WheelRouting::MouseReport) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_mouse_wheel(
                wheel_kind,
                position,
                KeyModifiers::from_bits_truncate(modifiers),
            ) else {
                // Only the wheel direction: this ends up in the server log.
                return Err(PaneInputError::Other(format!(
                    "failed to encode mouse wheel event: {wheel_kind:?}"
                )));
            };
            send_input(runtime, Bytes::from(bytes), "mouse wheel input")?;
        }
        Some(shepr_mux::pane::WheelRouting::AlternateScroll) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_alternate_scroll(wheel_kind) else {
                return Ok(());
            };
            send_input(runtime, Bytes::from(bytes), "alternate scroll input")?;
        }
        Some(shepr_mux::pane::WheelRouting::HostScroll) | None => match direction {
            ScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
            ScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
        },
    }
    Ok(())
}

/// Applies a batch of client pane input events in order.
///
/// Every event is attempted even after one fails, so a dropped press never
/// takes the matching release (or any later input) down with it. The error
/// carries every failure from the batch.
pub(super) fn apply_client_pane_input_events(
    runtime: &shepr_mux::pane::PaneRuntime,
    events: &[ClientPaneInputEvent],
) -> Result<(), PaneInputFailures> {
    let mut failures = PaneInputFailures::default();
    for event in events {
        if let Err(error) = apply_client_pane_input_event(runtime, event) {
            failures.0.push(error);
        }
    }
    if failures.0.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

fn apply_client_pane_input_event(
    runtime: &shepr_mux::pane::PaneRuntime,
    event: &ClientPaneInputEvent,
) -> Result<(), PaneInputError> {
    if let ClientPaneInputEvent::Mouse {
        kind,
        position,
        modifiers,
        lines,
        ..
    } = event
    {
        let kind = kind.to_host();
        let modifiers = modifiers.to_host();
        let position = match position {
            shepr_protocol::ClientMousePosition::Cell { column, row } => {
                shepr_termio::input::mouse::Position::Cell {
                    column: *column,
                    row: *row,
                }
            }
            shepr_protocol::ClientMousePosition::Pixels { x, y, column, row } => {
                if runtime.sgr_pixel_mouse_enabled() {
                    shepr_termio::input::mouse::Position::Pixels { x: *x, y: *y }
                } else {
                    shepr_termio::input::mouse::Position::Cell {
                        column: *column,
                        row: *row,
                    }
                }
            }
        };
        let bytes = match kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let direction = if kind == MouseEventKind::ScrollUp {
                    ScrollDirection::Up
                } else {
                    ScrollDirection::Down
                };
                return apply_scroll(
                    runtime,
                    direction,
                    (*lines).max(1),
                    position,
                    modifiers.bits(),
                );
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => runtime
                .encode_mouse_wheel(kind, position, modifiers)
                .unwrap_or_default(),
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => runtime
                .encode_mouse_button(kind, position, modifiers)
                .unwrap_or_default(),
            MouseEventKind::Moved => runtime
                .encode_mouse_motion(kind, position, modifiers)
                .unwrap_or_default(),
        };
        if bytes.is_empty() {
            return Ok(());
        }
        if kind != MouseEventKind::Moved {
            runtime.scroll_reset();
        }
        return send_input(runtime, Bytes::from(bytes), "mouse input");
    }

    if let ClientPaneInputEvent::TextCommit(text) = event {
        runtime.scroll_reset();
        return send_input(
            runtime,
            Bytes::copy_from_slice(text.as_bytes()),
            "text input",
        );
    }

    match event.to_raw_input_event() {
        shepr_termio::input::raw_input::RawInputEvent::Key(key) => {
            let key_event = key.as_key_event();
            if matches!(key_event.code, KeyCode::PageUp | KeyCode::PageDown)
                && key_event.modifiers.is_empty()
                && runtime.plain_page_keys_use_host_scrollback() == Some(true)
            {
                match key_event.kind {
                    KeyEventKind::Release => {}
                    KeyEventKind::Press | KeyEventKind::Repeat => {
                        let lines = usize::from(runtime.grid_size().rows.get());
                        if key_event.code == KeyCode::PageUp {
                            runtime.scroll_up(lines);
                        } else {
                            runtime.scroll_down(lines);
                        }
                    }
                }
                return Ok(());
            }

            runtime.scroll_reset();
            let bytes = runtime.encode_terminal_key(key);
            if bytes.is_empty() {
                return Ok(());
            }
            send_input(runtime, Bytes::from(bytes), "key input")
        }
        shepr_termio::input::raw_input::RawInputEvent::Paste(text) => {
            runtime.scroll_reset();
            send_paste(runtime, text, "paste")
        }
        shepr_termio::input::raw_input::RawInputEvent::Mouse(_)
        | shepr_termio::input::raw_input::RawInputEvent::OuterFocusGained
        | shepr_termio::input::raw_input::RawInputEvent::OuterFocusLost
        | shepr_termio::input::raw_input::RawInputEvent::HostDefaultColor { .. }
        | shepr_termio::input::raw_input::RawInputEvent::HostPaletteColors { .. }
        | shepr_termio::input::raw_input::RawInputEvent::HostColorSchemeChanged(_)
        | shepr_termio::input::raw_input::RawInputEvent::HostCellSizeReport { .. }
        | shepr_termio::input::raw_input::RawInputEvent::Unsupported => Err(PaneInputError::Other(
            "non-pane input reached targeted pane input".to_owned(),
        )),
    }
}

#[cfg(test)]
impl PaneInputFailures {
    pub(super) fn errors(&self) -> &[PaneInputError] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn a_full_input_queue_reports_every_dropped_event_without_aborting_the_batch() {
        // Input queue capacity 4: the fifth and later sends find it full.
        let (runtime, mut input_rx) =
            shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(20, 5, 0, b"", 4);
        let events = ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .map(|text| ClientPaneInputEvent::TextCommit(text.to_owned()))
            .collect::<Vec<_>>();

        // Both overflowing events are attempted and reported: the batch is not
        // cut short at the first failure.
        let failures =
            apply_client_pane_input_events(&runtime, &events).expect_err("queue overflows");
        assert_eq!(failures.dropped_for_backpressure(), 2);
        assert_eq!(
            failures.errors(),
            [
                PaneInputError::Backpressure("text input"),
                PaneInputError::Backpressure("text input"),
            ]
        );
        for expected in ["a", "b", "c", "d"] {
            assert_eq!(
                input_rx.try_recv().expect("queued input"),
                Bytes::from(expected)
            );
        }
        assert!(input_rx.try_recv().is_err(), "dropped input was delivered");

        apply_client_pane_input_events(
            &runtime,
            &[ClientPaneInputEvent::TextCommit("g".to_owned())],
        )
        .expect("room again after the queue drained");
    }

    #[tokio::test]
    async fn dropped_input_errors_do_not_carry_the_typed_text() {
        let (runtime, _input_rx) =
            shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(20, 5, 0, b"", 1);
        let secret = "hunter2-secret";
        let events = [
            ClientPaneInputEvent::TextCommit("x".to_owned()),
            ClientPaneInputEvent::TextCommit(secret.to_owned()),
            ClientPaneInputEvent::Paste(secret.to_owned()),
            ClientPaneInputEvent::Key {
                code: shepr_protocol::ClientKeyCode::Char('h'),
                modifiers: shepr_protocol::WireModifiers::NONE,
                kind: shepr_protocol::ClientKeyKind::Press,
                repeat_count: 1,
                shifted_codepoint: None,
                generated_text: Some(secret.to_owned()),
            },
        ];

        // The server logs this text on failure.
        let failures =
            apply_client_pane_input_events(&runtime, &events).expect_err("queue overflows");
        assert_eq!(failures.dropped_for_backpressure(), 3);
        let logged = failures.to_string();
        assert!(!logged.contains(secret), "{logged}");
    }

    #[test]
    fn ineligible_shell_pixel_mouse_uses_its_canonical_cell_position() {
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Down(shepr_protocol::ClientMouseButton::Left),
            position: shepr_protocol::ClientMousePosition::Pixels {
                x: 121,
                y: 81,
                column: 12,
                row: 4,
            },
            geometry: Some(shepr_protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(
            &mut events,
            false,
            shepr_core::geometry::GridSize::clamped(20, 5),
            Some((200, 100)),
        );

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: shepr_protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                ..
            }]
        ));
    }

    #[test]
    fn eligible_shell_pixel_mouse_remains_exact() {
        let position = shepr_protocol::ClientMousePosition::Pixels {
            x: 121,
            y: 81,
            column: 12,
            row: 4,
        };
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position,
            geometry: Some(shepr_protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(
            &mut events,
            true,
            shepr_core::geometry::GridSize::clamped(20, 5),
            Some((200, 100)),
        );

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: current,
                ..
            }] if *current == position
        ));
    }

    #[test]
    fn stale_shell_pixel_geometry_downgrades_to_its_canonical_cell() {
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position: shepr_protocol::ClientMousePosition::Pixels {
                x: 121,
                y: 81,
                column: 12,
                row: 4,
            },
            geometry: Some(shepr_protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(
            &mut events,
            true,
            shepr_core::geometry::GridSize::clamped(20, 6),
            Some((200, 120)),
        );

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: shepr_protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                geometry: None,
                ..
            }]
        ));
    }
}
