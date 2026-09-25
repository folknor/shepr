//! Stdin input reading for the thin client.
//!
//! Reads stdin bytes and forwards framed input to the main event loop.
//! The server handles semantic parsing.
//!
//! This is simpler and more reliable because:
//! - The server has the same input parsing code
//! - We avoid duplicating parsing logic in the client
//! - Host terminal control replies can be buffered or discarded before they leak

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use super::ClientLoopEvent;

// ---------------------------------------------------------------------------
// Stdin reader thread
// ---------------------------------------------------------------------------

/// Reads raw bytes from stdin and sends them to the main event loop.
///
/// This runs on a dedicated thread because stdin reading is blocking.
/// The main loop receives the raw bytes and forwards them as
/// `ClientMessage::Input` to the server.
pub fn stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
    host_mouse_capture_active: Arc<AtomicBool>,
    host_sgr_pixels_active: Arc<AtomicBool>,
    host_escape_disambiguation_active: bool,
    initial_host_input: Vec<u8>,
) {
    unix_stdin_reader_loop(
        event_tx,
        should_quit,
        host_color_query_sent,
        host_cell_size_query_sent,
        host_mouse_capture_active,
        host_sgr_pixels_active,
        host_escape_disambiguation_active,
        initial_host_input,
    );
}

fn unix_stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
    host_mouse_capture_active: Arc<AtomicBool>,
    host_sgr_pixels_active: Arc<AtomicBool>,
    host_escape_disambiguation_active: bool,
    initial_host_input: Vec<u8>,
) {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut scratch = [0u8; 4096];
    let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
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
        let chunks = framer.push(&initial_host_input);
        if !send_unix_input_chunks(
            chunks,
            &event_tx,
            &mut pending_palette,
            sgr_pixels,
            last_geometry,
        ) {
            return;
        }
        if (framer.has_pending_input() || !pending_palette.is_empty())
            && stdin_read_ready(
                &reader,
                idle_flush_timeout_ms(&framer, host_mouse_capture_active.load(Ordering::Acquire)),
            ) == Some(false)
        {
            let had_pending = framer.has_pending_input();
            let chunks = framer.flush_timeout();
            let held_escape = had_pending && chunks.is_empty();
            if !send_unix_input_chunks(
                chunks,
                &event_tx,
                &mut pending_palette,
                sgr_pixels,
                last_geometry,
            ) || !flush_unix_palette_input(&event_tx, &mut pending_palette)
            {
                return;
            }
            if held_escape
                && stdin_read_ready(&reader, crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS)
                    == Some(false)
                && !send_unix_input_chunks(
                    framer.flush_timeout(),
                    &event_tx,
                    &mut pending_palette,
                    sgr_pixels,
                    last_geometry,
                )
            {
                return;
            }
        }
        pending_mode = framer.has_pending_input().then_some(sgr_pixels);
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
                let chunks = framer.push(&scratch[..n]);
                if !framer.has_pending_input() {
                    pending_mode = None;
                }
                if !send_unix_input_chunks(
                    chunks,
                    &event_tx,
                    &mut pending_palette,
                    sgr_pixels,
                    last_geometry,
                ) {
                    return;
                }

                let timeout_ms = idle_flush_timeout_ms(
                    &framer,
                    host_mouse_capture_active.load(Ordering::Acquire),
                );
                if stdin_read_ready(&reader, timeout_ms) == Some(false) {
                    let had_pending = framer.has_pending_input();
                    let chunks = framer.flush_timeout();
                    let held_escape = had_pending && chunks.is_empty();
                    let sgr_pixels = pending_mode
                        .unwrap_or_else(|| host_sgr_pixels_active.load(Ordering::Acquire));
                    if !framer.has_pending_input() {
                        pending_mode = None;
                    }
                    if !send_unix_input_chunks(
                        chunks,
                        &event_tx,
                        &mut pending_palette,
                        sgr_pixels,
                        last_geometry,
                    ) || !flush_unix_palette_input(&event_tx, &mut pending_palette)
                    {
                        return;
                    }
                    if held_escape
                        && stdin_read_ready(
                            &reader,
                            crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS,
                        ) == Some(false)
                    {
                        let chunks = framer.flush_timeout();
                        if !framer.has_pending_input() {
                            pending_mode = None;
                        }
                        if !send_unix_input_chunks(
                            chunks,
                            &event_tx,
                            &mut pending_palette,
                            sgr_pixels,
                            last_geometry,
                        ) {
                            return;
                        }
                    }
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

fn send_unix_input_chunks(
    chunks: Vec<Vec<u8>>,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<Vec<u8>>,
    sgr_pixels: bool,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> bool {
    for data in chunks {
        let palette_response = std::str::from_utf8(&data)
            .ok()
            .and_then(crate::terminal_theme::parse_palette_color_response)
            .is_some();
        if palette_response {
            pending_palette.push(data);
            if pending_palette.len() == 256 && !flush_unix_palette_input(event_tx, pending_palette)
            {
                return false;
            }
            continue;
        }
        let default_color_response = std::str::from_utf8(&data)
            .ok()
            .and_then(crate::terminal_theme::parse_default_color_response)
            .is_some();
        if !default_color_response && !flush_unix_palette_input(event_tx, pending_palette) {
            return false;
        }
        let Some(event) = classify_unix_input(data, sgr_pixels, geometry) else {
            continue;
        };
        if event_tx.blocking_send(event).is_err() {
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
    data: Vec<u8>,
    sgr_pixels: bool,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> Option<ClientLoopEvent> {
    if sgr_pixels && crate::input::mouse::parse_report(&data).is_some() {
        return geometry.map(|geometry| ClientLoopEvent::PixelMouse(data, geometry));
    }
    Some(ClientLoopEvent::StdinInput(data))
}

fn flush_unix_palette_input(
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<Vec<u8>>,
) -> bool {
    if pending_palette.is_empty() {
        return true;
    }
    let data = std::mem::take(pending_palette).concat();
    event_tx
        .blocking_send(ClientLoopEvent::StdinInput(data))
        .is_ok()
}

fn idle_flush_timeout_ms(
    framer: &crate::raw_input::RawInputByteFramer,
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

        #[test]
    fn stdin_input_event_carries_raw_bytes() {
        let data = vec![0x1b, b'[', b'A']; // Up arrow escape sequence
        let event = ClientLoopEvent::StdinInput(data.clone());
        match event {
            ClientLoopEvent::StdinInput(d) => assert_eq!(d, data),
            _ => panic!("expected StdinInput event"),
        }
    }

    #[test]
    fn pixel_mouse_classification_is_narrow_and_uses_read_geometry() {
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).expect("test precondition");
        let report = b"\x1b[<35;321;241M".to_vec();
        let Some(ClientLoopEvent::PixelMouse(data, captured)) =
            classify_unix_input(report.clone(), true, Some(geometry))
        else {
            panic!("expected dedicated pixel mouse event");
        };
        assert_eq!(data, report);
        assert_eq!(captured, geometry);
        assert!(classify_unix_input(report, true, None).is_none());

        for raw in [
            b"key".as_slice(),
            b"\x1b[200~paste\x1b[201~".as_slice(),
            b"\x1b_Gi=7;unrelated\x1b\\".as_slice(),
            b"\x1b[<35;2;3Mtail".as_slice(),
        ] {
            let Some(ClientLoopEvent::StdinInput(data)) =
                classify_unix_input(raw.to_vec(), true, Some(geometry))
            else {
                panic!("unrelated input must remain raw");
            };
            assert_eq!(data, raw);
        }
    }

    #[test]
    fn transient_geometry_failure_keeps_last_real_value() {
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).expect("test precondition");
        assert_eq!(retain_geometry(Some(geometry), None), Some(geometry));
    }

    #[test]
    fn palette_replies_are_forwarded_as_one_input_batch() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut pending = Vec::new();
        assert!(send_unix_input_chunks(
            vec![
                b"\x1b]4;0;rgb:1111/2222/3333\x1b\\".to_vec(),
                b"\x1b]4;1;rgb:4444/5555/6666\x1b\\".to_vec(),
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
            data.windows(4)
                .filter(|window| *window == b"\x1b]4;")
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
        let mut escape = crate::raw_input::RawInputByteFramer::default();
        assert!(escape.push(b"\x1b").is_empty());
        let mut sgr_mouse = crate::raw_input::RawInputByteFramer::default();
        assert!(sgr_mouse.push(b"\x1b[<3").is_empty());
        let mut default_mouse = crate::raw_input::RawInputByteFramer::default();
        assert!(default_mouse.push(b"\x1b[MC").is_empty());
        let mut unrelated = crate::raw_input::RawInputByteFramer::default();
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