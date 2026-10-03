//! Stdin input reading for the thin client.
//!
//! Reads and classifies stdin on a dedicated blocking thread, then sends parsed
//! events to the main loop. The client shell consumes typed events.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use shepr_termio::input::raw_input::RawInputFramer;
use tokio::sync::mpsc;

use crate::events::{ClientLoopEvent, ParsedHostInput};
use crate::limits::HOST_INPUT_READ_CHUNK_BYTES;
use crate::terminal_geometry::SharedHostGeometry;
use crate::terminal_setup::HostMouseInputProbe;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeAvailability {
    NotArmed,
    Armed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeDisambiguation {
    Inactive,
    Active,
}

pub(crate) struct HostInputProbe {
    color_scheme_query: ProbeAvailability,
    cell_size_query: ProbeAvailability,
    mouse: HostMouseInputProbe,
    escape_disambiguation: EscapeDisambiguation,
}

impl HostInputProbe {
    pub(crate) fn new(
        color_scheme_query_sent: bool,
        cell_size_query_sent: bool,
        mouse: HostMouseInputProbe,
        escape_disambiguation_active: bool,
    ) -> Self {
        Self {
            color_scheme_query: if color_scheme_query_sent {
                ProbeAvailability::Armed
            } else {
                ProbeAvailability::NotArmed
            },
            cell_size_query: if cell_size_query_sent {
                ProbeAvailability::Armed
            } else {
                ProbeAvailability::NotArmed
            },
            mouse,
            escape_disambiguation: if escape_disambiguation_active {
                EscapeDisambiguation::Active
            } else {
                EscapeDisambiguation::Inactive
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Stdin reader thread
// ---------------------------------------------------------------------------

/// Reads host input, frames and parses it once, then sends it to the main loop.
///
/// This runs on a dedicated thread because stdin reading is blocking.
/// The client shell uses the parsed event and pixel hit-test metadata without
/// reparsing the bytes.
///
/// These bytes are keystrokes and paste contents (passwords included). Neither this loop
/// nor the client loop that consumes them logs them; keep it that way, and log sizes or
/// errors only.
pub(crate) fn stdin_reader_loop(
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    probe: &HostInputProbe,
    host_geometry: &SharedHostGeometry,
    initial_host_input: &[u8],
) {
    let stdin = io::stdin();
    // Bypass StdinLock's shared buffer so polling and reading observe the same bytes.
    let stdin_fd = stdin.as_raw_fd();
    let mut scratch = [0u8; HOST_INPUT_READ_CHUNK_BYTES];
    let mut framer = RawInputFramer::for_host_input();
    framer.set_host_escape_disambiguation_active(
        probe.escape_disambiguation == EscapeDisambiguation::Active,
    );
    if probe.color_scheme_query == ProbeAvailability::Armed {
        framer.host_color_query_sent();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();
    }
    if probe.cell_size_query == ProbeAvailability::Armed {
        framer.host_cell_size_query_sent();
    }
    let mut pending_palette = Vec::new();
    let mut pending_mode = None;
    let mut last_geometry = None;

    if !initial_host_input.is_empty() {
        if !consume_input_bytes(
            initial_host_input,
            &mut framer,
            event_tx,
            &mut pending_palette,
            &mut pending_mode,
            &mut last_geometry,
            &probe.mouse,
            host_geometry,
        ) {
            return;
        }
        if !flush_idle_input(
            stdin_fd,
            &mut framer,
            event_tx,
            &mut pending_palette,
            &mut pending_mode,
            &probe.mouse,
            last_geometry,
        ) {
            return;
        }
    }

    while !should_quit.load(Ordering::Acquire) {
        match shepr_platform::read_fd(stdin_fd, &mut scratch) {
            Ok(0) => {
                report_terminal_unavailable(
                    event_tx,
                    io::Error::new(io::ErrorKind::UnexpectedEof, "host terminal input closed"),
                );
                break;
            }
            Ok(n) => {
                if !consume_input_bytes(
                    &scratch[..n],
                    &mut framer,
                    event_tx,
                    &mut pending_palette,
                    &mut pending_mode,
                    &mut last_geometry,
                    &probe.mouse,
                    host_geometry,
                ) {
                    return;
                }

                if !flush_idle_input(
                    stdin_fd,
                    &mut framer,
                    event_tx,
                    &mut pending_palette,
                    &mut pending_mode,
                    &probe.mouse,
                    last_geometry,
                ) {
                    return;
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                report_terminal_unavailable(event_tx, err);
                break;
            }
        }
    }
}

fn report_terminal_unavailable(event_tx: &mpsc::Sender<ClientLoopEvent>, error: io::Error) {
    event_tx
        .blocking_send(ClientLoopEvent::TerminalUnavailable(error))
        .ok();
}

fn consume_input_bytes(
    data: &[u8],
    framer: &mut RawInputFramer,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<ParsedHostInput>,
    pending_mode: &mut Option<bool>,
    last_geometry: &mut Option<shepr_termio::input::mouse::HostPixelExtent>,
    host_mouse_probe: &HostMouseInputProbe,
    host_geometry: &SharedHostGeometry,
) -> bool {
    let sgr_pixels = *pending_mode.get_or_insert_with(|| host_mouse_probe.sgr_pixels_active());
    if sgr_pixels {
        *last_geometry = retain_geometry(*last_geometry, host_geometry.pixel_extent());
    }
    let chunks = framer.push_framed(data);
    if !framer.has_pending_input() {
        *pending_mode = None;
    }
    send_unix_input_chunks(
        chunks,
        event_tx,
        pending_palette,
        sgr_pixels,
        *last_geometry,
    )
}

fn flush_idle_input(
    stdin_fd: RawFd,
    framer: &mut RawInputFramer,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<ParsedHostInput>,
    pending_mode: &mut Option<bool>,
    host_mouse_probe: &HostMouseInputProbe,
    geometry: Option<shepr_termio::input::mouse::HostPixelExtent>,
) -> bool {
    if !framer.has_pending_input() && pending_palette.is_empty() {
        return true;
    }
    let timeout_ms = idle_flush_timeout_ms(framer, host_mouse_probe.capture_active());
    if stdin_read_ready(stdin_fd, timeout_ms) != Some(false) {
        return true;
    }
    let chunks = framer.flush_timeout_framed();
    // A timeout flush can emit a prefix and still retain its remainder. Give
    // those bytes their follow-up flush too; emitted chunks do not mean the
    // framer is empty.
    let has_pending_after_flush = framer.has_pending_input();
    let sgr_pixels = pending_mode.unwrap_or_else(|| host_mouse_probe.sgr_pixels_active());
    if !framer.has_pending_input() {
        *pending_mode = None;
    }
    if !send_unix_input_chunks(chunks, event_tx, pending_palette, sgr_pixels, geometry)
        || !flush_unix_palette_input(event_tx, pending_palette)
    {
        return false;
    }
    if has_pending_after_flush
        && stdin_read_ready(stdin_fd, framer.held_input_flush_timeout_ms()) == Some(false)
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
    chunks: Vec<shepr_termio::input::raw_input::FramedRawInputEvent>,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    pending_palette: &mut Vec<ParsedHostInput>,
    sgr_pixels: bool,
    geometry: Option<shepr_termio::input::mouse::HostPixelExtent>,
) -> bool {
    for chunk in chunks {
        let palette_response = matches!(
            &chunk.event,
            shepr_termio::input::raw_input::RawInputEvent::HostPaletteColors { .. }
        );
        if palette_response {
            if let Some(input) = classify_unix_input(chunk, sgr_pixels, geometry) {
                pending_palette.push(input);
            }
            if pending_palette.len() == shepr_core::limits::PALETTE_COLOR_COUNT
                && !flush_unix_palette_input(event_tx, pending_palette)
            {
                return false;
            }
            continue;
        }
        let default_color_response = matches!(
            &chunk.event,
            shepr_termio::input::raw_input::RawInputEvent::HostDefaultColor { .. }
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
    last: Option<shepr_termio::input::mouse::HostPixelExtent>,
    observed: Option<shepr_termio::input::mouse::HostPixelExtent>,
) -> Option<shepr_termio::input::mouse::HostPixelExtent> {
    observed.or(last)
}

fn classify_unix_input(
    input: shepr_termio::input::raw_input::FramedRawInputEvent,
    sgr_pixels: bool,
    geometry: Option<shepr_termio::input::mouse::HostPixelExtent>,
) -> Option<ParsedHostInput> {
    let pixel_mouse = if sgr_pixels && input.raw.starts_with(b"\x1b[<") {
        let shepr_termio::input::raw_input::RawInputEvent::Mouse(mouse) = &input.event else {
            return None;
        };
        // In SGR pixel mode the report's coordinates are pixels, not cells.
        // Without a pixel extent there is no way to map them to a cell, and
        // passing them on as cells would aim the event at an unrelated cell
        // (a sidebar button, another pane), so the report is dropped.
        //
        // The window is tiny by construction: pixel mode is only enabled after
        // the client loop's window-size ioctl returned a full pixel extent,
        // this thread rereads that same ioctl before every pixel-mode batch,
        // and a later failed read keeps the last good extent. A drop needs the
        // ioctl to fail on this thread's first read after succeeding on the
        // client loop. Holding reports until an extent arrives would need a
        // queue and a give-up rule for that window, and mapping from the
        // reported cell pitch is not a substitute: the extent can include
        // padding the pitch does not describe, so it would misplace clicks.
        let geometry = geometry?;
        Some(shepr_termio::input::mouse::HostPixels {
            x: u32::from(mouse.column) + 1,
            y: u32::from(mouse.row) + 1,
            geometry,
        })
    } else {
        None
    };
    Some(ParsedHostInput {
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
    framer: &shepr_termio::input::raw_input::RawInputFramer,
    host_mouse_capture_active: bool,
) -> i32 {
    if !host_mouse_capture_active {
        return shepr_termio::limits::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS;
    }
    if framer.has_pending_lone_escape() || framer.has_pending_incomplete_mouse_sequence() {
        shepr_termio::limits::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
    } else if framer.has_pending_csi_introducer() {
        // A mouse report split after `ESC [` is still ambiguous with legacy Alt+[.
        shepr_termio::limits::MOUSE_ACTIVE_CSI_INTRODUCER_FLUSH_TIMEOUT_MS
    } else {
        shepr_termio::limits::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
    }
}

fn stdin_read_ready(stdin_fd: RawFd, timeout_ms: i32) -> Option<bool> {
    poll_read_ready(stdin_fd, timeout_ms)
}

fn poll_read_ready(fd: i32, timeout_ms: i32) -> Option<bool> {
    shepr_platform::poll_fd_readable(fd, timeout_ms).ok()
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

    fn framed(raw: &[u8]) -> Vec<shepr_termio::input::raw_input::FramedRawInputEvent> {
        let mut framer = shepr_termio::input::raw_input::RawInputFramer::default();
        let mut inputs = framer.push_framed(raw);
        inputs.extend(framer.flush_timeout_framed());
        inputs
    }

    #[test]
    fn stdin_input_event_is_classified_from_framed_bytes() {
        let raw = vec![0x1b, b'[', b'A']; // Up arrow escape sequence
        let inputs = framed(&raw);
        let [input] = inputs.as_slice() else {
            panic!("expected one framed input event");
        };
        assert!(matches!(
            &input.event,
            shepr_termio::input::raw_input::RawInputEvent::Key(_)
        ));
    }

    #[test]
    fn pixel_mouse_classification_is_narrow_and_uses_read_geometry() {
        let geometry = shepr_termio::input::mouse::HostPixelExtent::new(80, 24, 800, 480)
            .expect("test precondition");
        let report = b"\x1b[<35;321;241M".to_vec();
        let mut report_events = framed(&report);
        assert_eq!(report_events.len(), 1);
        let report_event = report_events.pop().expect("one framed mouse event");
        let input =
            classify_unix_input(report_event, true, Some(geometry)).expect("pixel mouse event");
        assert_eq!(
            input.pixel_mouse,
            Some(shepr_termio::input::mouse::HostPixels {
                x: 321,
                y: 241,
                geometry
            })
        );
        // Pixel coordinates without an extent cannot name a cell; read as
        // cells they would hit column 320, row 240.
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
            assert!(!inputs.is_empty());
            assert!(inputs.iter().all(|input| input.pixel_mouse.is_none()));
        }
    }

    #[test]
    fn transient_geometry_failure_keeps_last_real_value() {
        let geometry = shepr_termio::input::mouse::HostPixelExtent::new(80, 24, 800, 480)
            .expect("test precondition");
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
                    shepr_termio::input::raw_input::RawInputEvent::HostPaletteColors { .. }
                ))
                .count(),
            2
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn raw_input_idle_flush_timeout_keeps_escape_responsive() {
        let timeout_ms =
            std::hint::black_box(shepr_termio::limits::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS);
        assert!(timeout_ms <= 20);
    }

    #[test]
    fn mouse_active_escape_sequences_get_longer_reassembly_window() {
        let mut escape = shepr_termio::input::raw_input::RawInputFramer::default();
        assert!(escape.push_framed(b"\x1b").is_empty());
        let mut sgr_mouse = shepr_termio::input::raw_input::RawInputFramer::default();
        assert!(sgr_mouse.push_framed(b"\x1b[<3").is_empty());
        let mut default_mouse = shepr_termio::input::raw_input::RawInputFramer::default();
        assert!(default_mouse.push_framed(b"\x1b[MC").is_empty());
        let mut unrelated = shepr_termio::input::raw_input::RawInputFramer::default();
        assert!(unrelated.push_framed(b"\x1b[49:33;2:").is_empty());
        let mut csi = shepr_termio::input::raw_input::RawInputFramer::default();
        assert!(csi.push_framed(b"\x1b[").is_empty());

        assert_eq!(
            idle_flush_timeout_ms(&csi, true),
            shepr_termio::limits::MOUSE_ACTIVE_CSI_INTRODUCER_FLUSH_TIMEOUT_MS
        );
        for framer in [&escape, &sgr_mouse, &default_mouse, &unrelated, &csi] {
            assert_eq!(
                idle_flush_timeout_ms(framer, false),
                shepr_termio::limits::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
            );
        }
        for framer in [&escape, &sgr_mouse, &default_mouse] {
            assert_eq!(
                idle_flush_timeout_ms(framer, true),
                shepr_termio::limits::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
            );
        }
        assert_eq!(
            idle_flush_timeout_ms(&unrelated, true),
            shepr_termio::limits::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );

        let mouse_timeout_ms = std::hint::black_box(
            shepr_termio::limits::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS,
        );
        assert!(mouse_timeout_ms > 100);
    }

    type HostFramer = shepr_termio::input::raw_input::RawInputFramer;

    fn raw_bytes(events: Vec<shepr_termio::input::raw_input::FramedRawInputEvent>) -> Vec<Vec<u8>> {
        events.into_iter().map(|event| event.raw).collect()
    }

    /// Plays the stdin reader's idle waits across a `gap` between two host
    /// writes, without sleeping, using its production timeout selector, then
    /// pushes `next`. Returns the raw bytes of every event framed.
    fn reader_chunks_across_gap(
        framer: &mut HostFramer,
        mouse_capture: bool,
        gap: std::time::Duration,
        next: &[u8],
    ) -> Vec<Vec<u8>> {
        let first_wait = std::time::Duration::from_millis(
            u64::try_from(idle_flush_timeout_ms(framer, mouse_capture)).unwrap_or_default(),
        );
        let mut chunks = Vec::new();
        if gap >= first_wait {
            let flushed = framer.flush_timeout_framed();
            let has_pending_after_flush = framer.has_pending_input();
            chunks.extend(raw_bytes(flushed));
            let second_wait = std::time::Duration::from_millis(
                u64::try_from(framer.held_input_flush_timeout_ms()).unwrap_or_default(),
            );
            if has_pending_after_flush && gap >= first_wait + second_wait {
                chunks.extend(raw_bytes(framer.flush_timeout_framed()));
            }
        }
        chunks.extend(raw_bytes(framer.push_framed(next)));
        chunks
    }

    fn confirmed_disambiguation_framer() -> HostFramer {
        let mut framer = HostFramer::for_host_input();
        framer.set_host_escape_disambiguation_active(true);
        framer
    }

    #[test]
    fn confirmed_disambiguation_keeps_mouse_report_split_by_delayed_tail() {
        // The tail of a click has been seen to arrive 350 ms after its ESC.
        let gap = std::time::Duration::from_millis(350);
        for (prefix, tail) in [
            (b"\x1b".as_slice(), b"[<0;5;5M".as_slice()),
            (b"\x1b[", b"<0;5;5M"),
            (b"\x1b[<0;", b"5;5M"),
        ] {
            let mut framer = confirmed_disambiguation_framer();
            assert!(framer.push_framed(prefix).is_empty());

            assert_eq!(
                reader_chunks_across_gap(&mut framer, true, gap, tail),
                vec![b"\x1b[<0;5;5M".to_vec()],
                "prefix {prefix:?} must rejoin its mouse tail"
            );
        }
    }

    #[test]
    fn confirmed_disambiguation_keeps_escape_prefixed_bindings_intact() {
        // Terminal-side bindings that send ESC-prefixed bytes: Ghostty's macOS
        // Alt+Left/Right, Alacritty's Shift+Enter, iTerm2's Option+Backspace.
        for binding in [b"\x1bb".as_slice(), b"\x1bf", b"\x1b\r", b"\x1b\x7f"] {
            let mut framer = confirmed_disambiguation_framer();

            assert_eq!(
                raw_bytes(framer.push_framed(binding)),
                vec![binding.to_vec()],
                "binding {binding:?}"
            );
        }
    }

    #[test]
    fn confirmed_disambiguation_releases_escape_prefixed_bindings_after_bounded_wait() {
        // Terminal text bindings can send ESC, Alt+[ or Alt+O alone.
        let gap = std::time::Duration::from_secs(1);
        for binding in [b"\x1b".as_slice(), b"\x1b[", b"\x1bO"] {
            let mut framer = confirmed_disambiguation_framer();
            assert!(framer.push_framed(binding).is_empty());

            assert_eq!(
                reader_chunks_across_gap(&mut framer, true, gap, b"x"),
                vec![binding.to_vec(), b"x".to_vec()],
                "binding {binding:?} must not be held or dropped"
            );
        }
    }

    #[test]
    fn confirmed_disambiguation_separates_escape_prefixed_binding_from_fast_next_key() {
        // Typing right after an Alt+O, Alt+[ or ESC text binding must not glue
        // the next key onto it while the reader watches for a delayed mouse tail.
        for (binding, next, gap_ms) in [
            (b"\x1bO".as_slice(), b"q".as_slice(), 80),
            (b"\x1b[", b"A", 80),
            (b"\x1b", b"x", 200),
        ] {
            let mut framer = confirmed_disambiguation_framer();
            assert!(framer.push_framed(binding).is_empty());

            assert_eq!(
                reader_chunks_across_gap(
                    &mut framer,
                    true,
                    std::time::Duration::from_millis(gap_ms),
                    next
                ),
                vec![binding.to_vec(), next.to_vec()],
                "binding {binding:?} followed by {next:?} after {gap_ms} ms"
            );
        }
    }

    #[test]
    fn confirmed_disambiguation_releases_binding_while_host_reply_is_pending() {
        for (binding, next) in [(b"\x1b[".as_slice(), b"A".as_slice()), (b"\x1b", b"x")] {
            let mut framer = confirmed_disambiguation_framer();
            framer.host_cell_size_query_sent();
            framer.host_color_query_sent();
            assert!(framer.push_framed(binding).is_empty());

            assert_eq!(
                reader_chunks_across_gap(
                    &mut framer,
                    true,
                    std::time::Duration::from_secs(1),
                    next
                ),
                vec![binding.to_vec(), next.to_vec()],
                "binding {binding:?} must not stay held behind a pending host reply"
            );
        }
    }

    #[test]
    fn host_reply_idle_flush_reschedules_after_emitting_first_doubled_escape() {
        let mut framer = HostFramer::for_host_input();
        framer.host_color_query_sent();
        assert!(framer.push_framed(b"\x1b\x1b").is_empty());

        // The first idle flush emits one ESC while the host-reply hold retains
        // the second. The retained ESC must get another timeout before `x`.
        assert_eq!(
            reader_chunks_across_gap(&mut framer, false, std::time::Duration::from_secs(1), b"x"),
            vec![b"\x1b".to_vec(), b"\x1b".to_vec(), b"x".to_vec()]
        );
    }

    #[test]
    fn confirmed_disambiguation_keeps_slow_host_reply_and_paste_whole() {
        let paste = b"200~hello\n\x1b[201~";
        for (tail, gap_ms) in [(b"6;21;10t".as_slice(), 55), (paste.as_slice(), 80)] {
            let mut framer = confirmed_disambiguation_framer();
            framer.host_cell_size_query_sent();
            assert!(framer.push_framed(b"\x1b[").is_empty());

            let chunks = reader_chunks_across_gap(
                &mut framer,
                true,
                std::time::Duration::from_millis(gap_ms),
                tail,
            );
            let mut whole = b"\x1b[".to_vec();
            whole.extend_from_slice(tail);
            assert_eq!(
                chunks,
                vec![whole],
                "tail {tail:?} after {gap_ms} ms must stay attached to ESC["
            );
        }
    }

    #[test]
    fn confirmed_disambiguation_keeps_osc_reply_split_after_escape_whole() {
        let mut framer = confirmed_disambiguation_framer();
        framer.host_color_query_sent();
        assert!(framer.push_framed(b"\x1b").is_empty());

        let tail = b"]11;rgb:1111/2222/3333\x07";
        let mut whole = b"\x1b".to_vec();
        whole.extend_from_slice(tail);
        assert_eq!(
            reader_chunks_across_gap(
                &mut framer,
                true,
                std::time::Duration::from_millis(155),
                tail
            ),
            vec![whole]
        );
    }

    #[test]
    fn confirmed_disambiguation_mouse_wait_does_not_consume_later_reply_hold() {
        let mut framer = confirmed_disambiguation_framer();
        framer.enable_host_appearance_query_on_focus();
        assert_eq!(
            raw_bytes(framer.push_framed(b"\x1b[I")),
            vec![b"\x1b[I".to_vec()]
        );
        assert!(framer.push_framed(b"\x1b[<0;").is_empty());

        assert!(
            reader_chunks_across_gap(
                &mut framer,
                true,
                std::time::Duration::from_millis(650),
                b"\x1b[?997;"
            )
            .is_empty()
        );
        assert_eq!(
            reader_chunks_across_gap(
                &mut framer,
                true,
                std::time::Duration::from_millis(15),
                b"2n"
            ),
            vec![b"\x1b[?997;2n".to_vec()]
        );
    }

    #[test]
    fn confirmed_disambiguation_joins_escape_binding_split_before_wait_ends() {
        let mut framer = confirmed_disambiguation_framer();
        assert!(framer.push_framed(b"\x1b").is_empty());

        assert_eq!(
            reader_chunks_across_gap(
                &mut framer,
                true,
                std::time::Duration::from_millis(40),
                b"\r"
            ),
            vec![b"\x1b\r".to_vec()]
        );
    }

    #[test]
    fn legacy_alt_bracket_is_not_glued_to_following_key() {
        let mut framer = HostFramer::for_host_input();
        assert!(framer.push_framed(b"\x1b[").is_empty());

        assert_eq!(
            reader_chunks_across_gap(
                &mut framer,
                true,
                std::time::Duration::from_millis(80),
                b"a"
            ),
            vec![b"\x1b[".to_vec(), b"a".to_vec()]
        );
    }

    #[test]
    fn captured_mouse_report_split_after_csi_survives_idle_gap() {
        let mut framer = HostFramer::for_host_input();
        framer.enable_host_appearance_query_on_focus();
        assert_eq!(
            raw_bytes(framer.push_framed(b"\x1b[I")),
            vec![b"\x1b[I".to_vec()]
        );
        assert!(framer.push_framed(b"\x1b[").is_empty());

        // `ESC [` and its continuation have been seen 32.937 ms apart.
        let chunks = reader_chunks_across_gap(
            &mut framer,
            true,
            std::time::Duration::from_micros(32_937),
            b"<35;64;37M\x1b[<35;65;36M\x1b[<35;64;36M",
        );
        assert_eq!(
            chunks,
            vec![
                b"\x1b[<35;64;37M".to_vec(),
                b"\x1b[<35;65;36M".to_vec(),
                b"\x1b[<35;64;36M".to_vec(),
            ]
        );
    }
}
