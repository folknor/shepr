use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tracing::{debug, warn};

use super::ClientLoopEvent;

const DEFAULT_CELL_WIDTH_PX: u32 = 8;
const DEFAULT_CELL_HEIGHT_PX: u32 = 16;

/// Average cell size derived from a terminal ioctl pixel extent.
///
/// The extent need not divide evenly by the grid because terminals may include
/// padding; mouse mapping uses the resulting integer cell pitch.
pub(super) fn ioctl_cell_size(
    columns: u16,
    rows: u16,
    width_px: u32,
    height_px: u32,
) -> Option<(u32, u32)> {
    if columns == 0 || rows == 0 || width_px == 0 || height_px == 0 {
        return None;
    }
    Some((
        (width_px / u32::from(columns)).max(1),
        (height_px / u32::from(rows)).max(1),
    ))
}

fn ioctl_terminal_geometry() -> Option<(u16, u16, u32, u32)> {
    let size = crossterm::terminal::window_size().ok()?;
    let (cell_width_px, cell_height_px) = ioctl_cell_size(
        size.columns,
        size.rows,
        u32::from(size.width),
        u32::from(size.height),
    )?;
    Some((size.columns, size.rows, cell_width_px, cell_height_px))
}

#[cfg(test)]
pub(super) fn cell_size_fallback(reported: u64, last: Option<(u32, u32)>) -> (u32, u32) {
    unpack_cell_size(reported)
        .or(last
            .filter(|(width, height)| shepr_core::geometry::CellPx::new(*width, *height).is_some()))
        .unwrap_or((DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX))
}

/// A coherent cell pitch snapshot shared by the stdin and resize threads.
#[derive(Debug, Default)]
pub(super) struct AtomicCellSize(AtomicU64);

impl AtomicCellSize {
    pub(super) fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(super) fn load(&self) -> Option<(u32, u32)> {
        unpack_cell_size(self.0.load(Ordering::Acquire))
    }

    pub(super) fn store(&self, width_px: u32, height_px: u32) -> bool {
        let packed = pack_cell_size(width_px, height_px);
        self.0.swap(packed, Ordering::AcqRel) != packed
    }
}

pub(super) fn pack_cell_size(width_px: u32, height_px: u32) -> u64 {
    (u64::from(width_px) << 32) | u64::from(height_px)
}

fn unpack_cell_size(packed: u64) -> Option<(u32, u32)> {
    let width_px = (packed >> 32) as u32;
    let height_px = u32::try_from(packed & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    shepr_core::geometry::CellPx::new(width_px, height_px)
        .map(|cell| (cell.width.get(), cell.height.get()))
}

type TerminalGeometry = shepr_core::geometry::HostGeometry;

/// Host grid size as reported by the client. A client-owned shell must keep
/// its full grid within one surface frame; a direct terminal client reports
/// the host grid unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClientHostSize {
    pub(super) cols: u16,
    pub(super) rows: u16,
}

impl ClientHostSize {
    pub(super) fn new(cols: u16, rows: u16, client_shell: bool) -> Self {
        let size = shepr_protocol::ClientSurfaceSize { cols, rows };
        let size = if client_shell { size.clamped() } else { size };
        Self {
            cols: size.cols,
            rows: size.rows,
        }
    }
}

pub(super) fn bounded_cell_geometry(
    cell_width_px: u32,
    cell_height_px: u32,
    pixel_geometry_exact: bool,
) -> (u32, u32, bool) {
    let size = shepr_protocol::ProtocolCellSize::from_host(
        cell_width_px,
        cell_height_px,
        pixel_geometry_exact,
    );
    (size.width(), size.height(), size.exact)
}

pub(super) fn current_terminal_geometry_with(
    pixel_geometry_enabled: bool,
    pixel_geometry_fallback: bool,
    reported_cell_size: &AtomicCellSize,
    last_cell_size: Option<(u32, u32)>,
    exact_geometry: Option<(u16, u16, u32, u32)>,
    terminal_grid_size: impl FnOnce() -> io::Result<(u16, u16)>,
) -> io::Result<TerminalGeometry> {
    if !pixel_geometry_enabled {
        let (cols, rows) = terminal_grid_size()?;
        return Ok(TerminalGeometry::new(cols, rows, 0, 0, false));
    }
    if let Some((cols, rows, cell_width_px, cell_height_px)) = exact_geometry {
        return Ok(TerminalGeometry::new(
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            true,
        ));
    }
    let (cols, rows) = terminal_grid_size()?;
    if !pixel_geometry_fallback {
        return Ok(TerminalGeometry::new(cols, rows, 0, 0, false));
    }
    let (cell_width_px, cell_height_px) = reported_cell_size
        .load()
        .or(last_cell_size
            .filter(|(width, height)| shepr_core::geometry::CellPx::new(*width, *height).is_some()))
        .unwrap_or((DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX));
    Ok(TerminalGeometry::new(
        cols,
        rows,
        cell_width_px,
        cell_height_px,
        false,
    ))
}

fn current_terminal_geometry(
    pixel_geometry_enabled: bool,
    pixel_geometry_fallback: bool,
    reported_cell_size: &AtomicCellSize,
    last_cell_size: Option<(u32, u32)>,
) -> io::Result<TerminalGeometry> {
    current_terminal_geometry_with(
        pixel_geometry_enabled,
        pixel_geometry_fallback,
        reported_cell_size,
        last_cell_size,
        ioctl_terminal_geometry(),
        shepr_platform::terminal_grid_size,
    )
}

/// Reads terminal geometry before the handshake. Pixel input and direct graphics
/// are eligible only when one ioctl supplied a coherent exact geometry snapshot.
pub(super) fn initial_terminal_geometry(
    pixel_geometry_enabled: bool,
    pixel_geometry_fallback: bool,
) -> io::Result<TerminalGeometry> {
    current_terminal_geometry(
        pixel_geometry_enabled,
        pixel_geometry_fallback,
        &AtomicCellSize::new(),
        None,
    )
}

pub(super) fn resize_report_required(
    signalled: bool,
    new_size: TerminalGeometry,
    last_size: TerminalGeometry,
) -> bool {
    signalled || new_size != last_size
}

/// Watches the terminal size and sends resize events when it changes.
///
/// The baseline cell size must match what the handshake sent to the server:
/// reading a fresh one here would race the host cell size reply and could
/// swallow the first change.
pub(super) fn resize_poll_loop(
    resize_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    initial: TerminalGeometry,
    pixel_geometry_enabled: bool,
    pixel_geometry_fallback: bool,
    reported_cell_size: &AtomicCellSize,
    should_quit: &Arc<AtomicBool>,
) {
    shepr_platform::watch_terminal_resize_signal();
    let mut last_size = initial;
    while !should_quit.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(100));
        let signalled = shepr_platform::take_terminal_resize_signal();
        let new_size = match current_terminal_geometry(
            pixel_geometry_enabled,
            pixel_geometry_fallback,
            reported_cell_size,
            Some((last_size.cell_width(), last_size.cell_height())),
        ) {
            Ok(size) => size,
            Err(err) => {
                if let Err(send_error) =
                    resize_tx.blocking_send(ClientLoopEvent::TerminalUnavailable(err))
                {
                    // The client loop has already gone, so nothing is left to
                    // react to the lost terminal; record why this thread stopped.
                    if let ClientLoopEvent::TerminalUnavailable(err) = send_error.0 {
                        debug!(error = %err, "host terminal unavailable after client loop exit");
                    }
                }
                break;
            }
        };
        if resize_report_required(signalled, new_size, last_size) {
            last_size = new_size;
            if resize_tx
                .blocking_send(ClientLoopEvent::Resize(new_size))
                .is_err()
            {
                break;
            }
        }
    }
}

/// Asks the host terminal for its color scheme. A query that fails to go out
/// gets no reply, so the failure is logged here: without it the client just
/// keeps its default appearance with nothing saying why.
pub(super) fn query_host_terminal_appearance() {
    if let Err(error) = write_host_terminal_appearance_query(io::stdout()) {
        warn!(
            error = %error,
            "failed to send host terminal color scheme query; keeping default appearance"
        );
    }
}

pub(super) fn write_host_terminal_appearance_query(mut writer: impl io::Write) -> io::Result<()> {
    writer
        .write_all(shepr_termio::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.as_bytes())?;
    writer.flush()
}

/// Asks the host terminal for its palette. Logged on failure for the same
/// reason as [`query_host_terminal_appearance`].
pub(super) fn query_host_terminal_theme() {
    if let Err(error) = write_host_terminal_theme_query(io::stdout()) {
        warn!(
            error = %error,
            "failed to send host terminal theme query; keeping default theme"
        );
    }
}

pub(super) fn write_host_terminal_theme_query(mut writer: impl io::Write) -> io::Result<()> {
    let query = shepr_termio::host_term::theme::host_terminal_theme_query_sequence();
    writer.write_all(query.as_bytes())?;
    writer.flush()
}

/// Asks the host terminal for its cell size in pixels. Without a reply the
/// client falls back to the last or the default cell size, which degrades
/// pixel mouse and resize reporting to a guess, so a query that never went out
/// is logged.
pub(super) fn query_host_cell_size() {
    if let Err(error) = write_host_cell_size_query(io::stdout()) {
        warn!(
            error = %error,
            default_width_px = DEFAULT_CELL_WIDTH_PX,
            default_height_px = DEFAULT_CELL_HEIGHT_PX,
            "failed to send host cell size query; pixel geometry falls back to a guessed cell size"
        );
    }
}

pub(super) fn host_cell_size_query_required(pixel_geometry_enabled: bool) -> bool {
    pixel_geometry_enabled && ioctl_terminal_geometry().is_none()
}

pub(super) fn write_host_cell_size_query(mut writer: impl io::Write) -> io::Result<()> {
    writer.write_all(shepr_termio::host_term::modes::HOST_CELL_SIZE_QUERY_SEQUENCE)?;
    writer.flush()
}

pub(super) fn store_reported_cell_size(
    reported_cell_size: &AtomicCellSize,
    width_px: u32,
    height_px: u32,
) {
    if reported_cell_size.store(width_px, height_px) {
        debug!(width_px, height_px, "host terminal reported cell size");
    }
}

pub(super) fn reported_cell_size_from_events<'a>(
    events: impl IntoIterator<Item = &'a shepr_termio::input::raw_input::RawInputEvent>,
) -> Option<(u32, u32)> {
    events
        .into_iter()
        .filter_map(|event| match event {
            shepr_termio::input::raw_input::RawInputEvent::HostCellSizeReport {
                width_px,
                height_px,
            } => Some((*width_px, *height_px)),
            _ => None,
        })
        .last()
}
