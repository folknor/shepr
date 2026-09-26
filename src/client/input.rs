//! Stdin input reading for the thin client.
//!
//! Reads and classifies stdin on a dedicated blocking thread, then sends parsed
//! events with their original bytes to the main loop. The client shell consumes
//! typed events; direct attach forwards the retained bytes.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use super::{ClientLoopEvent, ParsedHostInput};

// ---------------------------------------------------------------------------
// Stdin reader thread
// ---------------------------------------------------------------------------

/// Reads host input, frames and parses it once, then sends it to the main loop.
///
/// This runs on a dedicated thread because stdin reading is blocking.
/// The raw bytes stay attached for direct attach; the client shell uses the
/// parsed event and pixel hit-test metadata without reparsing those bytes.
///
/// These bytes are keystrokes and paste contents (passwords included). Neither this loop
/// nor the client loop that consumes them logs them; keep it that way, and log sizes or
/// errors only (the oversized-paste warning in `attach::forward_input` logs the length).
pub fn stdin_reader_loop(
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
    host_mouse_capture_active: &Arc<AtomicBool>,
    host_sgr_pixels_active: &Arc<AtomicBool>,
    host_escape_disambiguation_active: bool,
    initial_host_input: &[u8],
) {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut scratch = [0u8; 4096];
    let mut framer = crate::raw_input::RawInputFramer::for_host_input();
    framer.set_host_escape_disambiguation_active(host_escape_disambiguation_active);
    if host_color_query_sent {
        framer.host_color_query_sent();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();
    }
    if host_cell_size_query_sent {
        framer.host_cell_size_query_sent();
    }
    let mut pending_palette = Vec::new();
    let mut pending_mode = None;
    let mut last_geometry = None;

    if !initial_host_input.is_empty() {
        let sgr_pixels = host_sgr_pixels_active.load(Ordering::Acquire);
        if sgr_pixels {
            last_geometry = crate::input::mouse::HostGeometry::current();
        }
        let chunks = framer.push_framed(initial_host_input);
        if !send_unix_input_chunks(
            chunks,
            event_tx,
            &mut pending_palette,
            sgr_pixels,
            last_geometry,
        ) {
            return;
        }
        pending_mode = framer.has_pending_input().then_some(sgr_pixels);
        if !flush_idle_input(
            &reader,
            &mut framer,
            event_tx,
            &mut pending_palette,
            &mut pending_mode,
            host_mouse_capture_active,
            host_sgr_pixels_active,
            last_geometry,
        ) {
            return;
        }
    }

    while !should_quit.load(Ordering::Acquire) {
        match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => {
                let sgr_pixels = *pending_mode
                    .get_or_insert_with(|| host_sgr_pixels_active.load(Ordering::Acquire));
                if sgr_pixels {
                    last_geometry = retain_geometry(
                        last_geometry,
                        crate::input::mouse::HostGeometry::current(),
                    );
                }
                let chunks = framer.push_framed(&scratch[..n]);
                if !framer.has_pending_input() {
                    pending_mode = None;
                }
                if !send_unix_input_chunks(
                    chunks,
                    event_tx,
                    &mut pending_palette,
                    sgr_pixels,
                    last_geometry,
                ) {
                    return;
                }

                if !flush_idle_input(
                    &reader,
                    &mut framer,
                    event_tx,
                    &mut pending_palette,
                    &mut pending_mode,
                    host_mouse_capture_active,
                    host_sgr_pixels_active,
                    last_geometry,
                ) {
                    return;
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // The reader owns these independent input states.
fn flush_idle_input<R: AsRawFd>(
    reader: &R,
    framer: &mut crate::raw_input::RawInputFramer,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<ParsedHostInput>,
    pending_mode: &mut Option<bool>,
    host_mouse_capture_active: &AtomicBool,
    host_sgr_pixels_active: &AtomicBool,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> bool {
    if !framer.has_pending_input() && pending_palette.is_empty() {
        return true;
    }
    let timeout_ms =
        idle_flush_timeout_ms(framer, host_mouse_capture_active.load(Ordering::Acquire));
    if stdin_read_ready(reader, timeout_ms) != Some(false) {
        return true;
    }
    let had_pending = framer.has_pending_input();
    let chunks = framer.flush_timeout_framed();
    let held_escape = had_pending && chunks.is_empty();
    let sgr_pixels = pending_mode.unwrap_or_else(|| host_sgr_pixels_active.load(Ordering::Acquire));
    if !framer.has_pending_input() {
        *pending_mode = None;
    }
    if !send_unix_input_chunks(chunks, event_tx, pending_palette, sgr_pixels, geometry)
        || !flush_unix_palette_input(event_tx, pending_palette)
    {
        return false;
    }
    if held_escape
        && stdin_read_ready(reader, crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS)
            == Some(false)
    {
        let chunks = framer.flush_timeout_framed();
        if !framer.has_pending_input() {
            *pending_mode = None;
        }
        return send_unix_input_chunks(chunks, event_tx, pending_palette, sgr_pixels, geometry);
    }
    true
}

fn send_unix_input_chunks(
    chunks: Vec<crate::raw_input::FramedRawInputEvent>,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<ParsedHostInput>,
    sgr_pixels: bool,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> bool {
    for chunk in chunks {
        let palette_response = matches!(
            &chunk.event,
            crate::raw_input::RawInputEvent::HostPaletteColors { .. }
        );
        if palette_response {
            if let Some(input) = classify_unix_input(chunk, sgr_pixels, geometry) {
                pending_palette.push(input);
            }
            if pending_palette.len() == 256 && !flush_unix_palette_input(event_tx, pending_palette)
            {
                return false;
            }
            continue;
        }
        let default_color_response = matches!(
            &chunk.event,
            crate::raw_input::RawInputEvent::HostDefaultColor { .. }
        );
        if !default_color_response && !flush_unix_palette_input(event_tx, pending_palette) {
            return false;
        }
        let Some(input) = classify_unix_input(chunk, sgr_pixels, geometry) else {
            continue;
        };
        if event_tx
            .blocking_send(ClientLoopEvent::StdinInput(vec![input]))
            .is_err()
        {
            return false;
        }
    }
    true
}

fn retain_geometry(
    last: Option<crate::input::mouse::HostGeometry>,
    observed: Option<crate::input::mouse::HostGeometry>,
) -> Option<crate::input::mouse::HostGeometry> {
    observed.or(last)
}

fn classify_unix_input(
    input: crate::raw_input::FramedRawInputEvent,
    sgr_pixels: bool,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> Option<ParsedHostInput> {
    let pixel_mouse = if sgr_pixels && input.raw.starts_with(b"\x1b[<") {
        let geometry = geometry?;
        let crate::raw_input::RawInputEvent::Mouse(mouse) = &input.event else {
            return None;
        };
        Some(crate::input::mouse::HostPixels {
            x: u32::from(mouse.column) + 1,
            y: u32::from(mouse.row) + 1,
            geometry,
        })
    } else {
        None
    };
    Some(ParsedHostInput {
        raw: input.raw,
        event: input.event,
        pixel_mouse,
    })
}

fn flush_unix_palette_input(
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<ParsedHostInput>,
) -> bool {
    if pending_palette.is_empty() {
        return true;
    }
    event_tx
        .blocking_send(ClientLoopEvent::StdinInput(std::mem::take(pending_palette)))
        .is_ok()
}

fn idle_flush_timeout_ms(
    framer: &crate::raw_input::RawInputFramer,
    host_mouse_capture_active: bool,
) -> i32 {
    if host_mouse_capture_active
        && (framer.has_pending_lone_escape() || framer.has_pending_incomplete_mouse_sequence())
    {
        crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
    } else {
        crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
    }
}

fn stdin_read_ready<R: AsRawFd>(reader: &R, timeout_ms: i32) -> Option<bool> {
    poll_read_ready(reader.as_raw_fd(), timeout_ms)
}

fn poll_read_ready(fd: i32, timeout_ms: i32) -> Option<bool> {
    crate::platform::poll_fd_readable(fd, timeout_ms).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // The stdin reader thread is hard to unit test since it reads from actual stdin.
    // Integration tests will verify the full client→server input flow.
    // Here we test the event type construction.

    use super::*;

    fn framed(raw: &[u8]) -> Vec<crate::raw_input::FramedRawInputEvent> {
        let mut framer = crate::raw_input::RawInputFramer::default();
        let mut inputs = framer.push_framed(raw);
        inputs.extend(framer.flush_timeout_framed());
        inputs
    }

    #[test]
    fn stdin_input_event_carries_raw_bytes() {
        let raw = vec![0x1b, b'[', b'A']; // Up arrow escape sequence
        let inputs = framed(&raw);
        let [input] = inputs.as_slice() else {
            panic!("expected one framed input event");
        };
        assert_eq!(input.raw, raw);
        assert!(matches!(
            &input.event,
            crate::raw_input::RawInputEvent::Key(_)
        ));
    }

    #[test]
    fn pixel_mouse_classification_is_narrow_and_uses_read_geometry() {
        let geometry =
            crate::input::mouse::HostGeometry::new(80, 24, 800, 480).expect("test precondition");
        let report = b"\x1b[<35;321;241M".to_vec();
        let mut report_events = framed(&report);
        assert_eq!(report_events.len(), 1);
        let report_event = report_events.pop().expect("one framed mouse event");
        let input =
            classify_unix_input(report_event, true, Some(geometry)).expect("pixel mouse event");
        assert_eq!(input.raw, report);
        assert_eq!(
            input.pixel_mouse,
            Some(crate::input::mouse::HostPixels {
                x: 321,
                y: 241,
                geometry
            })
        );
        assert!(classify_unix_input(framed(&report).remove(0), true, None).is_none());

        for raw in [
            b"key".as_slice(),
            b"\x1b[200~paste\x1b[201~".as_slice(),
            b"\x1b_Gi=7;unrelated\x1b\\".as_slice(),
        ] {
            let inputs = framed(raw)
                .into_iter()
                .map(|event| {
                    classify_unix_input(event, true, Some(geometry))
                        .expect("unrelated input must remain available")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                inputs
                    .iter()
                    .flat_map(|input| input.raw.iter().copied())
                    .collect::<Vec<_>>(),
                raw
            );
            assert!(inputs.iter().all(|input| input.pixel_mouse.is_none()));
        }
    }

    #[test]
    fn transient_geometry_failure_keeps_last_real_value() {
        let geometry =
            crate::input::mouse::HostGeometry::new(80, 24, 800, 480).expect("test precondition");
        assert_eq!(retain_geometry(Some(geometry), None), Some(geometry));
    }

    #[test]
    fn palette_replies_are_forwarded_as_one_input_batch() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut pending = Vec::new();
        assert!(send_unix_input_chunks(
            vec![
                framed(b"\x1b]4;0;rgb:1111/2222/3333\x1b\\").remove(0),
                framed(b"\x1b]4;1;rgb:4444/5555/6666\x1b\\").remove(0),
            ],
            &tx,
            &mut pending,
            false,
            None,
        ));
        assert!(rx.try_recv().is_err());

        assert!(flush_unix_palette_input(&tx, &mut pending));
        let ClientLoopEvent::StdinInput(data) = rx.try_recv().expect("test precondition") else {
            panic!("expected palette input batch");
        };
        assert_eq!(
            data.iter()
                .filter(|input| matches!(
                    &input.event,
                    crate::raw_input::RawInputEvent::HostPaletteColors { .. }
                ))
                .count(),
            2
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn raw_input_idle_flush_timeout_keeps_escape_responsive() {
        let timeout_ms = std::hint::black_box(crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS);
        assert!(timeout_ms <= 20);
    }

    #[test]
    fn mouse_active_escape_sequences_get_longer_reassembly_window() {
        let mut escape = crate::raw_input::RawInputFramer::default();
        assert!(escape.push(b"\x1b").is_empty());
        let mut sgr_mouse = crate::raw_input::RawInputFramer::default();
        assert!(sgr_mouse.push(b"\x1b[<3").is_empty());
        let mut default_mouse = crate::raw_input::RawInputFramer::default();
        assert!(default_mouse.push(b"\x1b[MC").is_empty());
        let mut unrelated = crate::raw_input::RawInputFramer::default();
        assert!(unrelated.push(b"\x1b[49:33;2:").is_empty());

        for framer in [&escape, &sgr_mouse, &default_mouse, &unrelated] {
            assert_eq!(
                idle_flush_timeout_ms(framer, false),
                crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
            );
        }
        for framer in [&escape, &sgr_mouse, &default_mouse] {
            assert_eq!(
                idle_flush_timeout_ms(framer, true),
                crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
            );
        }
        assert_eq!(
            idle_flush_timeout_ms(&unrelated, true),
            crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );

        let mouse_timeout_ms =
            std::hint::black_box(crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS);
        assert!(mouse_timeout_ms > 100);
    }
}
