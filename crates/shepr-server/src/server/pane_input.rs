use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use shepr_mux::workspace::SurfaceChange;
use shepr_protocol::ClientPaneInputEvent;
use shepr_term::key::TerminalKey;

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
    Backpressure(InputKind),
    /// The PTY actor no longer accepts input (the pane is shutting down).
    Closed(InputKind),
    /// The input could not be encoded.
    Other(String),
}

/// The kind of pane input a failure names, in place of what was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputKind {
    MouseWheel,
    AlternateScroll,
    Text,
    Paste,
    Key,
    Mouse,
}

impl InputKind {
    fn label(self) -> &'static str {
        match self {
            Self::MouseWheel => "mouse wheel input",
            Self::AlternateScroll => "alternate scroll input",
            Self::Text => "text input",
            Self::Paste => "paste",
            Self::Key => "key input",
            Self::Mouse => "mouse input",
        }
    }
}

impl std::fmt::Display for PaneInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backpressure(kind) => {
                write!(f, "{} dropped: pane input queue is full", kind.label())
            }
            Self::Closed(kind) => {
                write!(f, "{} dropped: pane no longer accepts input", kind.label())
            }
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// Every failure from one batch of pane input events. A batch is always
/// applied to the end, so one failed event never swallows the events after it
/// (a key or button release in particular).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PaneInputFailures {
    errors: Vec<PaneInputError>,
    changed: bool,
}

impl PaneInputFailures {
    pub(super) fn surface_change(&self) -> SurfaceChange {
        if self.changed {
            SurfaceChange::Changed
        } else {
            SurfaceChange::Unchanged
        }
    }
}

impl PaneInputFailures {
    /// Number of events dropped because the PTY input queue was full.
    pub(super) fn dropped_for_backpressure(&self) -> usize {
        self.errors
            .iter()
            .filter(|error| matches!(error, PaneInputError::Backpressure(_)))
            .count()
    }
}

impl std::fmt::Display for PaneInputFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
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
    kind: InputKind,
) -> Result<(), PaneInputError> {
    runtime
        .try_send_bytes(bytes)
        .map_err(|err| send_error(&err, kind))
}

fn send_paste(
    runtime: &shepr_mux::pane::PaneRuntime,
    text: String,
    kind: InputKind,
) -> Result<(), PaneInputError> {
    runtime
        .try_send_paste(text)
        .map_err(|err| send_error(&err, kind))
}

fn send_error(err: &shepr_pty::ChildIoSendError, kind: InputKind) -> PaneInputError {
    match err {
        shepr_pty::ChildIoSendError::Full(_) => PaneInputError::Backpressure(kind),
        shepr_pty::ChildIoSendError::Closed(_) => PaneInputError::Closed(kind),
    }
}

/// Rewrites every pixel report that may not reach the pane as pixels into its
/// cell position. The decision is `shepr_term::mouse::admit_pixel_report`.
pub(super) fn admit_pixel_reports(
    events: &mut [ClientPaneInputEvent],
    host: shepr_core::geometry::HostCell,
    pane: shepr_term::mouse::PanePixelMouse,
) {
    for event in events {
        let ClientPaneInputEvent::Mouse { position, .. } = event else {
            continue;
        };
        let shepr_protocol::ClientMousePosition::Pixels {
            column,
            row,
            report,
        } = *position
        else {
            continue;
        };
        if !shepr_term::mouse::admit_pixel_report(host, pane, report) {
            *position = shepr_protocol::ClientMousePosition::Cell { column, row };
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
    input_modes: Option<shepr_vt::InputModes>,
    direction: ScrollDirection,
    lines: u16,
    position: shepr_term::mouse::Position,
    modifiers: KeyModifiers,
    changed: &mut bool,
) -> Result<(), PaneInputError> {
    let wheel_kind = match direction {
        ScrollDirection::Up => MouseEventKind::ScrollUp,
        ScrollDirection::Down => MouseEventKind::ScrollDown,
    };

    match input_modes.map(|modes| {
        (
            modes,
            shepr_mux::pane::PaneRuntime::wheel_routing_for_modes(modes),
        )
    }) {
        Some((modes, shepr_mux::pane::WheelRouting::MouseReport)) => {
            *changed |= runtime.scroll_reset().is_changed();
            let Some(bytes) =
                runtime.encode_mouse_wheel_with_modes(modes, wheel_kind, position, modifiers)
            else {
                // Only the wheel direction: this ends up in the server log.
                return Err(PaneInputError::Other(format!(
                    "failed to encode mouse wheel event: {wheel_kind:?}"
                )));
            };
            send_input(runtime, Bytes::from(bytes), InputKind::MouseWheel)?;
        }
        Some((modes, shepr_mux::pane::WheelRouting::AlternateScroll)) => {
            *changed |= runtime.scroll_reset().is_changed();
            let Some(bytes) = runtime.encode_alternate_scroll_with_modes(modes, wheel_kind) else {
                return Ok(());
            };
            send_input(runtime, Bytes::from(bytes), InputKind::AlternateScroll)?;
        }
        _ => {
            let lines = usize::from(lines.max(1));
            *changed |= match direction {
                ScrollDirection::Up => runtime.scroll_up(lines),
                ScrollDirection::Down => runtime.scroll_down(lines),
            }
            .is_changed();
        }
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
) -> Result<SurfaceChange, PaneInputFailures> {
    let mut failures = PaneInputFailures::default();
    for event in events {
        if let Err(error) = apply_client_pane_input_event(runtime, event, &mut failures.changed) {
            failures.errors.push(error);
        }
    }
    if failures.errors.is_empty() {
        Ok(failures.surface_change())
    } else {
        Err(failures)
    }
}

fn apply_client_pane_input_event(
    runtime: &shepr_mux::pane::PaneRuntime,
    event: &ClientPaneInputEvent,
    changed: &mut bool,
) -> Result<(), PaneInputError> {
    match event {
        ClientPaneInputEvent::Key {
            code,
            modifiers,
            kind,
            shifted_codepoint,
            generated_text,
        } => {
            let mut key = TerminalKey::new(code.to_host(), modifiers.to_host())
                .with_kind(kind.to_host())
                .with_generated_text(generated_text.clone());
            if let Some(shifted_codepoint) = shifted_codepoint {
                key = key.with_shifted_codepoint(*shifted_codepoint);
            }
            apply_key(runtime, key, changed)
        }
        ClientPaneInputEvent::TextCommit(text) => {
            *changed |= runtime.scroll_reset().is_changed();
            send_input(
                runtime,
                Bytes::copy_from_slice(text.as_bytes()),
                InputKind::Text,
            )
        }
        ClientPaneInputEvent::Mouse {
            kind,
            position,
            modifiers,
            lines,
            ..
        } => apply_mouse(
            runtime,
            kind.to_host(),
            *position,
            modifiers.to_host(),
            *lines,
            changed,
        ),
        ClientPaneInputEvent::Paste(text) => {
            *changed |= runtime.scroll_reset().is_changed();
            send_paste(runtime, text.clone(), InputKind::Paste)
        }
    }
}

fn apply_key(
    runtime: &shepr_mux::pane::PaneRuntime,
    key: TerminalKey,
    changed: &mut bool,
) -> Result<(), PaneInputError> {
    let key_event = key.as_key_event();
    let input_modes = runtime.read().input_modes();
    if matches!(key_event.code, KeyCode::PageUp | KeyCode::PageDown)
        && key_event.modifiers.is_empty()
        && input_modes.is_some_and(shepr_vt::InputModes::plain_page_keys_use_host_scrollback)
    {
        match key_event.kind {
            KeyEventKind::Release => {}
            KeyEventKind::Press | KeyEventKind::Repeat => {
                let lines = usize::from(runtime.grid_size().rows.get());
                if key_event.code == KeyCode::PageUp {
                    *changed |= runtime.scroll_up(lines).is_changed();
                } else {
                    *changed |= runtime.scroll_down(lines).is_changed();
                }
            }
        }
        return Ok(());
    }

    *changed |= runtime.scroll_reset().is_changed();
    let bytes = if let Some(modes) = input_modes {
        runtime.encode_terminal_key_with_modes(key, modes)
    } else {
        runtime.encode_terminal_key(key)
    };
    if bytes.is_empty() {
        return Ok(());
    }
    send_input(runtime, Bytes::from(bytes), InputKind::Key)
}

fn apply_mouse(
    runtime: &shepr_mux::pane::PaneRuntime,
    kind: MouseEventKind,
    position: shepr_protocol::ClientMousePosition,
    modifiers: KeyModifiers,
    lines: u16,
    changed: &mut bool,
) -> Result<(), PaneInputError> {
    let input_modes = runtime.read().input_modes();
    let position = match position {
        shepr_protocol::ClientMousePosition::Cell { column, row } => {
            shepr_term::mouse::Position::Cell { column, row }
        }
        shepr_protocol::ClientMousePosition::Pixels {
            column,
            row,
            report,
        } => shepr_term::mouse::Position::Pixels {
            column,
            row,
            x: report.x(),
            y: report.y(),
        },
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
                input_modes,
                direction,
                lines,
                position,
                modifiers,
                changed,
            );
        }
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => input_modes
            .and_then(|modes| {
                runtime.encode_mouse_wheel_with_modes(modes, kind, position, modifiers)
            })
            .unwrap_or_default(),
        MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => input_modes
            .and_then(|modes| {
                runtime.encode_mouse_button_with_modes(modes, kind, position, modifiers)
            })
            .unwrap_or_default(),
        MouseEventKind::Moved => input_modes
            .and_then(|modes| {
                runtime.encode_mouse_motion_with_modes(modes, kind, position, modifiers)
            })
            .unwrap_or_default(),
    };
    if bytes.is_empty() {
        return Ok(());
    }
    if kind != MouseEventKind::Moved {
        *changed |= runtime.scroll_reset().is_changed();
    }
    send_input(runtime, Bytes::from(bytes), InputKind::Mouse)
}

#[cfg(test)]
impl PaneInputFailures {
    pub(super) fn errors(&self) -> &[PaneInputError] {
        &self.errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_term::mouse::PanePixelMouse;

    fn extent(
        width: u16,
        height: u16,
        cols: u16,
        rows: u16,
    ) -> Option<shepr_core::geometry::PanePixelExtent> {
        shepr_core::geometry::PanePixelExtent::new(
            shepr_core::geometry::GridSize::clamped(cols, rows),
            width,
            height,
        )
    }

    fn pixels_at(
        x: u32,
        y: u32,
        column: u16,
        row: u16,
        extent: Option<shepr_core::geometry::PanePixelExtent>,
    ) -> shepr_protocol::ClientMousePosition {
        shepr_protocol::ClientMousePosition::Pixels {
            column,
            row,
            report: shepr_term::mouse::PixelReport::new(x, y, extent.expect("test extent")),
        }
    }

    fn exact_host() -> shepr_core::geometry::HostCell {
        shepr_core::geometry::HostCell::Exact(
            shepr_core::geometry::CellPx::new(10, 20).expect("test cell"),
        )
    }

    #[tokio::test]
    async fn scroll_change_survives_a_failed_send_and_a_reset_in_the_same_batch() {
        let (runtime, _input_rx) =
            shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
                20,
                5,
                4096,
                b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n",
                1,
            );
        let events = [
            ClientPaneInputEvent::TextCommit("fill".to_owned()),
            ClientPaneInputEvent::Mouse {
                kind: shepr_protocol::ClientMouseKind::ScrollUp,
                position: shepr_protocol::ClientMousePosition::Cell { column: 0, row: 0 },
                modifiers: shepr_protocol::WireModifiers::NONE,
                lines: 1,
            },
            ClientPaneInputEvent::TextCommit("dropped".to_owned()),
        ];
        let failures = apply_client_pane_input_events(&runtime, &events)
            .expect_err("full queue rejects the final send");
        assert_eq!(failures.surface_change(), SurfaceChange::Changed);
        assert_eq!(failures.dropped_for_backpressure(), 1);
        assert_eq!(runtime.scroll_reset(), SurfaceChange::Unchanged);
    }

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
                PaneInputError::Backpressure(InputKind::Text),
                PaneInputError::Backpressure(InputKind::Text),
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
            position: pixels_at(121, 81, 12, 4, extent(200, 100, 20, 5)),
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 1,
        }];

        // The pane never asked for 1016.
        admit_pixel_reports(
            &mut events,
            exact_host(),
            PanePixelMouse::new(false, extent(200, 100, 20, 5)),
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
        let position = pixels_at(121, 81, 12, 4, extent(200, 100, 20, 5));
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::Moved,
            position,
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 1,
        }];

        admit_pixel_reports(
            &mut events,
            exact_host(),
            PanePixelMouse::new(true, extent(200, 100, 20, 5)),
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
            position: pixels_at(121, 81, 12, 4, extent(200, 100, 20, 5)),
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines: 1,
        }];

        // The pane was resized to 20x6 after the client mapped against 20x5.
        admit_pixel_reports(
            &mut events,
            exact_host(),
            PanePixelMouse::new(true, extent(200, 120, 20, 6)),
        );

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: shepr_protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                ..
            }]
        ));
    }
}
