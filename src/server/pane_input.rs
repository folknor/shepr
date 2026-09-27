use crate::client::input_wire::{WireMouseKind, WirePaneInput};
use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use shepr_protocol::{AttachScrollDirection, AttachScrollSource, ClientPaneInputEvent};

/// Why one piece of pane input did not reach the PTY.
///
/// Input is handed to the PTY actor with a non-blocking send. The actor's
/// queue holds on the order of a thousand pending writes, so it is only full
/// when the child has stopped reading its terminal (suspended, wedged, or
/// flooded). Waiting for room is not an option: the server event loop is
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

    #[cfg(test)]
    pub(super) fn errors(&self) -> &[PaneInputError] {
        &self.0
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
    runtime: &crate::pane::PaneRuntime,
    bytes: Bytes,
    what: &'static str,
) -> Result<(), PaneInputError> {
    runtime
        .try_send_bytes(bytes)
        .map_err(|err| send_error(&err, what))
}

fn send_paste(
    runtime: &crate::pane::PaneRuntime,
    text: String,
    what: &'static str,
) -> Result<(), PaneInputError> {
    runtime
        .try_send_paste(text)
        .map_err(|err| send_error(&err, what))
}

fn send_error(
    err: &tokio::sync::mpsc::error::TrySendError<Bytes>,
    what: &'static str,
) -> PaneInputError {
    match err {
        tokio::sync::mpsc::error::TrySendError::Full(_) => PaneInputError::Backpressure(what),
        tokio::sync::mpsc::error::TrySendError::Closed(_) => PaneInputError::Closed(what),
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

pub(super) fn terminal_attach_mouse_position(
    runtime: &crate::pane::PaneRuntime,
    terminal_size: shepr_core::geometry::GridSize,
    cell_size: crate::host_term::cell_size::HostCellSize,
    pixel_mouse: bool,
    host_sgr_pixels_active: bool,
    position: shepr_protocol::ClientMousePosition,
    geometry: Option<shepr_protocol::ClientMouseGeometry>,
) -> Option<shepr_protocol::ClientMousePosition> {
    let runtime_size = runtime.grid_size();
    let cell_fallback = |column, row| {
        (column < runtime_size.cols.get() && row < runtime_size.rows.get())
            .then_some(shepr_protocol::ClientMousePosition::Cell { column, row })
    };
    let (x, y, column, row) = match position {
        shepr_protocol::ClientMousePosition::Cell { column, row } => {
            return cell_fallback(column, row);
        }
        shepr_protocol::ClientMousePosition::Pixels { x, y, column, row } => (x, y, column, row),
    };
    let Some(geometry) = geometry else {
        return cell_fallback(column, row);
    };
    let host_geometry = crate::input::mouse::HostPixelExtent::new(
        geometry.cols,
        geometry.rows,
        geometry.width_px,
        geometry.height_px,
    )?;
    if host_geometry.cell(x, y) != Some((column, row)) {
        return None;
    }
    let exact = (|| {
        let average_width = (geometry.width_px / u32::from(geometry.cols)).max(1);
        let average_height = (geometry.height_px / u32::from(geometry.rows)).max(1);
        let (child_width_px, child_height_px) = runtime.pixel_size()?;
        if !pixel_mouse
            || !host_sgr_pixels_active
            || !runtime.sgr_pixel_mouse_enabled()
            || terminal_size
                != shepr_core::geometry::GridSize::clamped(geometry.cols, geometry.rows)
            || runtime_size != shepr_core::geometry::GridSize::clamped(geometry.cols, geometry.rows)
            || !cell_size.is_known()
            || average_width != cell_size.width_px
            || average_height != cell_size.height_px
        {
            return None;
        }
        let crate::input::mouse::Position::Pixels { x, y } = (crate::input::mouse::HostPixels {
            x,
            y,
            geometry: host_geometry,
        })
        .pane_position(
            ratatui::layout::Rect::new(0, 0, geometry.cols, geometry.rows),
            child_width_px,
            child_height_px,
        )?
        else {
            return None;
        };
        Some(shepr_protocol::ClientMousePosition::Pixels { x, y, column, row })
    })();
    exact.or_else(|| cell_fallback(column, row))
}

pub(super) fn apply_terminal_attach_scroll(
    runtime: &crate::pane::PaneRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    column: Option<u16>,
    row: Option<u16>,
    modifiers: u8,
) -> Result<(), PaneInputError> {
    apply_scroll(
        runtime,
        source,
        direction,
        lines,
        crate::input::mouse::Position::Cell {
            column: column.unwrap_or(0),
            row: row.unwrap_or(0),
        },
        modifiers,
    )
}

fn apply_scroll(
    runtime: &crate::pane::PaneRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    position: crate::input::mouse::Position,
    modifiers: u8,
) -> Result<(), PaneInputError> {
    let wheel_kind = match direction {
        AttachScrollDirection::Up => MouseEventKind::ScrollUp,
        AttachScrollDirection::Down => MouseEventKind::ScrollDown,
    };
    if let AttachScrollSource::PageKey { input } = source {
        let host_scroll = runtime
            .plain_page_keys_use_host_scrollback()
            .unwrap_or(false);
        if host_scroll {
            match direction {
                AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
                AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
            }
            return Ok(());
        }
        return apply_terminal_attach_input(runtime, input);
    }

    match runtime.wheel_routing() {
        Some(crate::pane::WheelRouting::MouseReport) => {
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
        Some(crate::pane::WheelRouting::AlternateScroll) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_alternate_scroll(wheel_kind) else {
                return Ok(());
            };
            send_input(runtime, Bytes::from(bytes), "alternate scroll input")?;
        }
        Some(crate::pane::WheelRouting::HostScroll) | None => match direction {
            AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
            AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
        },
    }
    Ok(())
}

pub(super) fn apply_terminal_attach_input(
    runtime: &crate::pane::PaneRuntime,
    data: Vec<u8>,
) -> Result<(), PaneInputError> {
    runtime.scroll_reset();
    if let Some(text) = crate::raw_input::complete_text_bracketed_paste(&data) {
        send_paste(runtime, text.to_owned(), "terminal attach paste")
    } else {
        send_input(runtime, Bytes::from(data), "terminal attach input")
    }
}

/// Applies a batch of client pane input events in order.
///
/// Every event is attempted even after one fails, so a dropped press never
/// takes the matching release (or any later input) down with it. The error
/// carries every failure from the batch.
pub(super) fn apply_client_pane_input_events(
    runtime: &crate::pane::PaneRuntime,
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
    runtime: &crate::pane::PaneRuntime,
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
        let kind = kind.to_crossterm();
        let modifiers = crate::client::input_wire::host_modifiers(*modifiers);
        let position = match position {
            shepr_protocol::ClientMousePosition::Cell { column, row } => {
                crate::input::mouse::Position::Cell {
                    column: *column,
                    row: *row,
                }
            }
            shepr_protocol::ClientMousePosition::Pixels { x, y, column, row } => {
                if runtime.sgr_pixel_mouse_enabled() {
                    crate::input::mouse::Position::Pixels { x: *x, y: *y }
                } else {
                    crate::input::mouse::Position::Cell {
                        column: *column,
                        row: *row,
                    }
                }
            }
        };
        let bytes = match kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let direction = if kind == MouseEventKind::ScrollUp {
                    AttachScrollDirection::Up
                } else {
                    AttachScrollDirection::Down
                };
                return apply_scroll(
                    runtime,
                    AttachScrollSource::Wheel,
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
        crate::raw_input::RawInputEvent::Key(key) => {
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
        crate::raw_input::RawInputEvent::Paste(text) => {
            runtime.scroll_reset();
            send_paste(runtime, text, "paste")
        }
        crate::raw_input::RawInputEvent::Mouse(_)
        | crate::raw_input::RawInputEvent::OuterFocusGained
        | crate::raw_input::RawInputEvent::OuterFocusLost
        | crate::raw_input::RawInputEvent::HostDefaultColor { .. }
        | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
        | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
        | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
        | crate::raw_input::RawInputEvent::Unsupported => Err(PaneInputError::Other(
            "non-pane input reached targeted pane input".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_input_queue_reports_every_dropped_event_without_aborting_the_batch() {
        // Input queue capacity 4: the fifth and later sends find it full.
        let (runtime, mut input_rx) =
            crate::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(20, 5, 0, b"", 4);
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
            crate::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(20, 5, 0, b"", 1);
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

    #[tokio::test]
    async fn terminal_attach_stale_geometry_falls_back_to_the_canonical_cell() {
        let runtime = crate::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"");
        let position = shepr_protocol::ClientMousePosition::Pixels {
            x: 121,
            y: 81,
            column: 12,
            row: 4,
        };

        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                shepr_core::geometry::GridSize::clamped(20, 5),
                crate::host_term::cell_size::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                true,
                false,
                position,
                Some(shepr_protocol::ClientMouseGeometry {
                    cols: 20,
                    rows: 5,
                    width_px: 200,
                    height_px: 100,
                }),
            ),
            Some(shepr_protocol::ClientMousePosition::Cell { column: 12, row: 4 })
        );
        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                shepr_core::geometry::GridSize::clamped(20, 5),
                crate::host_term::cell_size::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                true,
                false,
                shepr_protocol::ClientMousePosition::Pixels {
                    x: 120,
                    y: 80,
                    column: 12,
                    row: 4,
                },
                Some(shepr_protocol::ClientMouseGeometry {
                    cols: 20,
                    rows: 5,
                    width_px: 200,
                    height_px: 100,
                }),
            ),
            None
        );
        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                shepr_core::geometry::GridSize::clamped(80, 24),
                crate::host_term::cell_size::HostCellSize::default(),
                false,
                false,
                shepr_protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                None,
            ),
            Some(shepr_protocol::ClientMousePosition::Cell { column: 12, row: 4 })
        );
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
