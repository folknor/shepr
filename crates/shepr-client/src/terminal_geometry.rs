use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tracing::{debug, warn};

use super::ClientLoopEvent;
use crate::limits::{DEFAULT_CELL_HEIGHT_PX, DEFAULT_CELL_WIDTH_PX, TERMINAL_RESIZE_POLL_INTERVAL};

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

/// One host-terminal observation. The extent is kept separately from the rounded cell pitch:
/// the terminal may have pixel padding that a cell size alone cannot reconstruct.
#[derive(Clone, Copy)]
pub(super) struct HostGeometrySnapshot {
    pub(super) geometry: TerminalGeometry,
    pub(super) pixel_extent: Option<shepr_termio::input::mouse::HostPixelExtent>,
}

#[derive(Clone)]
pub(super) struct SharedHostGeometry(Arc<RwLock<HostGeometrySnapshot>>);

impl SharedHostGeometry {
    pub(super) fn new(initial: HostGeometrySnapshot) -> Self {
        Self(Arc::new(RwLock::new(initial)))
    }

    pub(super) fn publish(&self, snapshot: HostGeometrySnapshot) {
        match self.0.write() {
            Ok(mut current) => *current = snapshot,
            Err(_) => warn!("host geometry snapshot lock is poisoned"),
        }
    }

    pub(super) fn pixel_extent(&self) -> Option<shepr_termio::input::mouse::HostPixelExtent> {
        self.0
            .read()
            .ok()
            .and_then(|snapshot| snapshot.pixel_extent)
    }
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

pub(super) type TerminalGeometry = shepr_core::geometry::HostGeometry;

/// Host grid size as reported by the client. The client-owned shell must keep
/// its full grid within one surface frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClientHostSize {
    pub(super) cols: u16,
    pub(super) rows: u16,
}

impl ClientHostSize {
    pub(super) fn new(cols: u16, rows: u16) -> Self {
        let size = shepr_protocol::ClientSurfaceSize { cols, rows }.clamped();
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

fn ioctl_host_geometry() -> Option<HostGeometrySnapshot> {
    let size = crossterm::terminal::window_size().ok()?;
    let width_px = u32::from(size.width);
    let height_px = u32::from(size.height);
    let (cell_width_px, cell_height_px) =
        ioctl_cell_size(size.columns, size.rows, width_px, height_px)?;
    let geometry =
        TerminalGeometry::new(size.columns, size.rows, cell_width_px, cell_height_px, true);
    let pixel_extent = shepr_termio::input::mouse::HostPixelExtent::new(
        size.columns,
        size.rows,
        width_px,
        height_px,
    );
    Some(HostGeometrySnapshot {
        geometry,
        pixel_extent,
    })
}

fn current_host_geometry(
    reported_cell_size: &AtomicCellSize,
    last_cell_size: Option<(u32, u32)>,
) -> io::Result<HostGeometrySnapshot> {
    if let Some(snapshot) = ioctl_host_geometry() {
        return Ok(snapshot);
    }
    let (cols, rows) = shepr_platform::terminal_grid_size()?;
    let (cell_width_px, cell_height_px) = reported_cell_size
        .load()
        .or(last_cell_size
            .filter(|(width, height)| shepr_core::geometry::CellPx::new(*width, *height).is_some()))
        .unwrap_or((DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX));
    Ok(HostGeometrySnapshot {
        geometry: TerminalGeometry::new(cols, rows, cell_width_px, cell_height_px, false),
        pixel_extent: None,
    })
}

/// Reads terminal geometry before the handshake. Pixel input is eligible only
/// when one ioctl supplied a coherent exact geometry snapshot.
pub(super) fn initial_terminal_geometry() -> io::Result<HostGeometrySnapshot> {
    current_host_geometry(&AtomicCellSize::new(), None)
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
    initial: HostGeometrySnapshot,
    reported_cell_size: &AtomicCellSize,
    host_geometry: &SharedHostGeometry,
    should_quit: &Arc<AtomicBool>,
) {
    shepr_platform::watch_terminal_resize_signal();
    let mut last_size = initial.geometry;
    while !should_quit.load(Ordering::Acquire) {
        std::thread::sleep(TERMINAL_RESIZE_POLL_INTERVAL);
        // Finish the probe after a quit arrives during sleep so terminal loss can still enter
        // the event queue; a successful unchanged probe is silent and quit already wakes the
        // client loop.
        let signalled = shepr_platform::take_terminal_resize_signal();
        let snapshot = match current_host_geometry(
            reported_cell_size,
            Some((last_size.cell_width(), last_size.cell_height())),
        ) {
            Ok(snapshot) => snapshot,
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
        host_geometry.publish(snapshot);
        let new_size = snapshot.geometry;
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
/// Returns whether the query was written. On focus changes, the blocking
/// reader has already opened its bounded one-flush reply window before the
/// client loop sends this query, so this wrapper cannot close it on failure.
pub(super) fn query_host_terminal_appearance(writer: &mut impl io::Write) -> bool {
    match write_host_terminal_appearance_query(writer) {
        Ok(()) => true,
        Err(error) => {
            warn!(
                error = %error,
                "failed to send host terminal color scheme query; keeping default appearance"
            );
            false
        }
    }
}

pub(super) fn write_host_terminal_appearance_query(mut writer: impl io::Write) -> io::Result<()> {
    writer
        .write_all(shepr_termio::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.as_bytes())?;
    writer.flush()
}

/// Asks the host terminal for its palette. Logged on failure for the same
/// reason as [`query_host_terminal_appearance`]. Startup uses the result to
/// arm reply tracking only after this large query was written successfully.
pub(super) fn query_host_terminal_theme(writer: &mut impl io::Write) -> bool {
    match write_host_terminal_theme_query(writer) {
        Ok(()) => true,
        Err(error) => {
            warn!(
                error = %error,
                "failed to send host terminal theme query; keeping default theme"
            );
            false
        }
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
/// is logged. Startup uses the result to arm reply tracking only after the
/// query was written successfully.
pub(super) fn query_host_cell_size(writer: &mut impl io::Write) -> bool {
    match write_host_cell_size_query(writer) {
        Ok(()) => true,
        Err(error) => {
            warn!(
                error = %error,
                default_width_px = DEFAULT_CELL_WIDTH_PX,
                default_height_px = DEFAULT_CELL_HEIGHT_PX,
                "failed to send host cell size query; pixel geometry falls back to a guessed cell size"
            );
            false
        }
    }
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

#[cfg(test)]
pub(super) fn current_terminal_geometry_with(
    reported_cell_size: &AtomicCellSize,
    last_cell_size: Option<(u32, u32)>,
    exact_geometry: Option<(u16, u16, u32, u32)>,
    terminal_grid_size: impl FnOnce() -> io::Result<(u16, u16)>,
) -> io::Result<TerminalGeometry> {
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

#[cfg(test)]
pub(super) fn cell_size_fallback(reported: u64, last: Option<(u32, u32)>) -> (u32, u32) {
    unpack_cell_size(reported)
        .or(last
            .filter(|(width, height)| shepr_core::geometry::CellPx::new(*width, *height).is_some()))
        .unwrap_or((DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX))
}
