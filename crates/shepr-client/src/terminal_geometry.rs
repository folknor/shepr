use shepr_core::geometry::{CellReport, GridSize, HostCell};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tracing::debug;

use crate::events::ClientLoopEvent;
use crate::input::ProbeAvailability;
use crate::limits::{DEFAULT_CELL_HEIGHT_PX, DEFAULT_CELL_WIDTH_PX, TERMINAL_RESIZE_POLL_INTERVAL};
use crate::state::{HostWriteAction, HostWritePurpose, host_write_failure_action};

/// Average cell size derived from a terminal ioctl pixel extent.
///
/// The extent need not divide evenly by the grid because terminals may include
/// padding; mouse mapping uses the resulting integer cell pitch.
pub(super) fn ioctl_cell_size(
    columns: u16,
    rows: u16,
    width_px: u32,
    height_px: u32,
) -> Option<CellReport> {
    if columns == 0 || rows == 0 || width_px == 0 || height_px == 0 {
        return None;
    }
    CellReport::new(
        (width_px / u32::from(columns)).max(1),
        (height_px / u32::from(rows)).max(1),
    )
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
            Err(_) => shepr_platform::structured_log!(
                WARN,
                event = terminal.geometry,
                outcome = "poisoned",
                "host geometry snapshot lock is poisoned"
            ),
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

    pub(super) fn load(&self) -> Option<CellReport> {
        unpack_cell_size(self.0.load(Ordering::Acquire))
    }

    pub(super) fn store(&self, cell: Option<CellReport>) -> CellSizeUpdate {
        let packed = cell.map_or(0, |cell| {
            pack_cell_size(cell.width.get(), cell.height.get())
        });
        if self.0.swap(packed, Ordering::AcqRel) == packed {
            CellSizeUpdate::Unchanged
        } else {
            CellSizeUpdate::Changed
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CellSizeUpdate {
    Unchanged,
    Changed,
}

pub(super) fn pack_cell_size(width_px: u32, height_px: u32) -> u64 {
    (u64::from(width_px) << 32) | u64::from(height_px)
}

fn unpack_cell_size(packed: u64) -> Option<CellReport> {
    let width_px = (packed >> 32) as u32;
    let height_px = u32::try_from(packed & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    CellReport::new(width_px, height_px)
}

pub(super) type TerminalGeometry = shepr_core::geometry::HostGeometry;

/// Bounds an observed host geometry's grid to the shared grid budgets, the same
/// `BoundedGridSize` rule `ClientSurfaceSize::clamped` applies, so the
/// client-owned shell keeps its full grid within one surface frame. Every host
/// size the client keeps or lays out against passes through here.
pub(super) fn bounded_cell_geometry(geometry: TerminalGeometry) -> TerminalGeometry {
    // HostGeometry already owns the pixel bound. This boundary only bounds
    // the retained shell grid, preserving its coherent cell observation.
    let grid = shepr_core::geometry::BoundedGridSize::clamped(geometry.cols(), geometry.rows());
    geometry.with_grid(grid.grid())
}

/// The exact geometry one window-size ioctl reading gives, when it carries a pixel extent and
/// a non-empty grid.
fn ioctl_host_geometry(size: &crossterm::terminal::WindowSize) -> Option<HostGeometrySnapshot> {
    let width_px = u32::from(size.width);
    let height_px = u32::from(size.height);
    let report = ioctl_cell_size(size.columns, size.rows, width_px, height_px)?;
    let grid = GridSize::new(size.columns, size.rows)?;
    let geometry = TerminalGeometry::new(grid, HostCell::from_report(report, true));
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
    last_cell_size: Option<CellReport>,
) -> io::Result<HostGeometrySnapshot> {
    current_host_geometry_with(
        reported_cell_size,
        last_cell_size,
        || crossterm::terminal::window_size().ok(),
        shepr_platform::terminal_grid_size,
    )
}

/// `current_host_geometry` over its two host probes: the window-size ioctl, and the grid
/// query used when the ioctl gives no exact geometry. Without an exact geometry the cell is
/// the host's reported cell size, else `last_cell_size`, else the default, never exact.
fn current_host_geometry_with(
    reported_cell_size: &AtomicCellSize,
    last_cell_size: Option<CellReport>,
    window_size: impl FnOnce() -> Option<crossterm::terminal::WindowSize>,
    terminal_grid_size: impl FnOnce() -> io::Result<GridSize>,
) -> io::Result<HostGeometrySnapshot> {
    if let Some(snapshot) = window_size().and_then(|size| ioctl_host_geometry(&size)) {
        return Ok(snapshot);
    }
    let grid = terminal_grid_size()?;
    let cell = reported_cell_size.load().or(last_cell_size);
    let host_cell = cell.map_or_else(
        || HostCell::from_host(DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX, false),
        |report| HostCell::from_report(report, false),
    );
    Ok(HostGeometrySnapshot {
        geometry: TerminalGeometry::new(grid, host_cell),
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
        let last_cell = last_size.cell().cell().map(CellReport::from);
        let snapshot = match current_host_geometry(reported_cell_size, last_cell) {
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
pub(super) fn query_host_terminal_appearance(writer: &mut impl io::Write) -> io::Result<bool> {
    match write_host_terminal_appearance_query(writer) {
        Ok(()) => Ok(true),
        Err(error) => {
            if host_write_failure_action(HostWritePurpose::Probe, error.kind())
                == HostWriteAction::Fatal
            {
                return Err(error);
            }
            shepr_platform::structured_log!(
                WARN, event = terminal.scheme_query, outcome = "error",
                error = %error,
                "failed to send host terminal color scheme query; keeping default appearance"
            );
            Ok(false)
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
pub(super) fn query_host_terminal_theme(
    writer: &mut impl io::Write,
) -> io::Result<ProbeAvailability> {
    match write_host_terminal_theme_query(writer) {
        Ok(()) => Ok(ProbeAvailability::Armed),
        Err(error) => {
            if host_write_failure_action(HostWritePurpose::Probe, error.kind())
                == HostWriteAction::Fatal
            {
                return Err(error);
            }
            shepr_platform::structured_log!(
                WARN, event = terminal.theme_query, outcome = "error",
                error = %error,
                "failed to send host terminal theme query; keeping default theme"
            );
            Ok(ProbeAvailability::NotArmed)
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
pub(super) fn query_host_cell_size(writer: &mut impl io::Write) -> io::Result<ProbeAvailability> {
    match write_host_cell_size_query(writer) {
        Ok(()) => Ok(ProbeAvailability::Armed),
        Err(error) => {
            if host_write_failure_action(HostWritePurpose::Probe, error.kind())
                == HostWriteAction::Fatal
            {
                return Err(error);
            }
            shepr_platform::structured_log!(
                WARN, event = terminal.cell_query, outcome = "error",
                error = %error,
                default_width_px = DEFAULT_CELL_WIDTH_PX,
                default_height_px = DEFAULT_CELL_HEIGHT_PX,
                "failed to send host cell size query; pixel geometry falls back to a guessed cell size"
            );
            Ok(ProbeAvailability::NotArmed)
        }
    }
}

pub(super) fn write_host_cell_size_query(mut writer: impl io::Write) -> io::Result<()> {
    writer.write_all(shepr_termio::host_term::modes::HOST_CELL_SIZE_QUERY_SEQUENCE)?;
    writer.flush()
}

pub(super) fn store_reported_cell_size(reported_cell_size: &AtomicCellSize, cell: CellReport) {
    if reported_cell_size.store(Some(cell)) == CellSizeUpdate::Changed {
        debug!(
            width_px = cell.width.get(),
            height_px = cell.height.get(),
            "host terminal reported cell size"
        );
    }
}

pub(super) fn reported_cell_size_from_events<'a>(
    events: impl IntoIterator<Item = &'a shepr_termio::input::raw_input::RawInputEvent>,
) -> Option<CellReport> {
    events
        .into_iter()
        .filter_map(|event| match event {
            shepr_termio::input::raw_input::RawInputEvent::HostCellSizeReport { cell } => {
                Some(*cell)
            }
            _ => None,
        })
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::geometry::{CellPx, HostGeometry};

    fn window(columns: u16, rows: u16, width: u16, height: u16) -> crossterm::terminal::WindowSize {
        crossterm::terminal::WindowSize {
            rows,
            columns,
            width,
            height,
        }
    }

    fn grid_80x24() -> io::Result<GridSize> {
        Ok(GridSize::clamped(80, 24))
    }

    /// The geometry `current_host_geometry` derives when the ioctl gives no exact geometry
    /// and the grid query reports 80x24.
    fn fallback_geometry(
        reported_cell_size: &AtomicCellSize,
        last_cell_size: Option<CellReport>,
    ) -> TerminalGeometry {
        current_host_geometry_with(reported_cell_size, last_cell_size, || None, grid_80x24)
            .expect("the grid query answered")
            .geometry
    }

    #[test]
    fn atomic_cell_size_keeps_width_and_height_in_one_snapshot() {
        let size = AtomicCellSize::new();
        assert_eq!(size.load(), None);
        assert_eq!(size.store(CellReport::new(9, 18)), CellSizeUpdate::Changed);
        assert_eq!(size.load(), CellReport::new(9, 18));
        assert_eq!(
            size.store(CellReport::new(9, 18)),
            CellSizeUpdate::Unchanged
        );
        assert_eq!(size.store(None), CellSizeUpdate::Changed);
        assert_eq!(size.load(), None);
    }

    #[test]
    fn resize_signal_reports_even_when_polled_size_is_unchanged() {
        let size = HostGeometry::new(GridSize::clamped(120, 40), HostCell::from_host(8, 16, true));
        assert!(resize_report_required(true, size, size));
        assert!(!resize_report_required(false, size, size));
        assert!(resize_report_required(
            false,
            HostGeometry::new(GridSize::clamped(120, 41), HostCell::from_host(8, 16, true)),
            size
        ));
        assert!(resize_report_required(
            false,
            HostGeometry::new(GridSize::clamped(120, 40), HostCell::from_host(9, 18, true)),
            size
        ));
        assert!(resize_report_required(
            false,
            HostGeometry::new(
                GridSize::clamped(120, 40),
                HostCell::from_host(8, 16, false)
            ),
            size
        ));
    }

    #[test]
    fn unavailable_terminal_grid_is_not_fabricated() {
        let reported_cell_size = AtomicCellSize::new();
        let err = current_host_geometry_with(
            &reported_cell_size,
            None,
            || None,
            || {
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "terminal is gone",
                ))
            },
        )
        .err()
        .expect("an unavailable terminal must not produce fallback geometry");

        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn missing_pixel_geometry_keeps_a_valid_terminal_grid() {
        let reported_cell_size = AtomicCellSize::new();
        // The ioctl answers with a grid but no pixel extent, as many terminals do.
        let snapshot = current_host_geometry_with(
            &reported_cell_size,
            CellReport::new(9, 18),
            || Some(window(80, 24, 0, 0)),
            grid_80x24,
        )
        .expect("grid geometry remains valid without pixel dimensions");

        assert_eq!(
            snapshot.geometry,
            HostGeometry::new(GridSize::clamped(80, 24), HostCell::from_host(9, 18, false))
        );
        assert!(snapshot.pixel_extent.is_none());
    }

    #[test]
    fn an_exact_ioctl_geometry_wins_over_reported_and_previous_cell_sizes() {
        let reported_cell_size = AtomicCellSize::new();
        reported_cell_size.store(CellReport::new(11, 22));
        let snapshot = current_host_geometry_with(
            &reported_cell_size,
            CellReport::new(12, 24),
            || Some(window(80, 24, 800, 480)),
            || panic!("an exact ioctl geometry needs no grid query"),
        )
        .expect("exact geometry");

        assert_eq!(
            snapshot.geometry,
            HostGeometry::new(GridSize::clamped(80, 24), HostCell::from_host(10, 20, true))
        );
        assert!(snapshot.pixel_extent.is_some());
    }

    #[test]
    fn bounded_host_geometry_fits_the_grid_into_one_surface() {
        let shell = bounded_cell_geometry(HostGeometry::new(
            GridSize::clamped(1, u16::MAX),
            HostCell::Unknown,
        ));
        assert_eq!(shell.cols(), 1);
        assert!((1..=shepr_protocol::MAX_SURFACE_DIMENSION).contains(&shell.rows()));
        assert!(
            usize::from(shell.cols()) * usize::from(shell.rows())
                <= shepr_protocol::MAX_SURFACE_CELLS
        );
    }

    #[test]
    fn cell_geometry_is_bounded_before_wire_use_and_disables_inexact_pixel_mouse() {
        let geometry = bounded_cell_geometry(HostGeometry::new(
            GridSize::clamped(80, 24),
            HostCell::from_host(
                shepr_protocol::MAX_CELL_SIZE_PX + 1,
                shepr_protocol::MAX_CELL_SIZE_PX + 2,
                true,
            ),
        ));
        assert_eq!(
            geometry.cell(),
            HostCell::Estimated(
                CellPx::new(
                    shepr_protocol::MAX_CELL_SIZE_PX,
                    shepr_protocol::MAX_CELL_SIZE_PX,
                )
                .expect("the bound is a usable cell")
            )
        );
        assert!(!geometry.cell().is_exact());
    }

    #[test]
    fn write_host_terminal_appearance_query_emits_mode_2031_query() {
        let mut output = Vec::new();
        write_host_terminal_appearance_query(&mut output).expect("test precondition");
        assert_eq!(output, b"\x1b[?996n");
    }

    #[test]
    fn write_host_terminal_theme_query_emits_osc_queries() {
        let mut output = Vec::new();
        write_host_terminal_theme_query(&mut output).expect("test precondition");
        assert_eq!(
            output,
            shepr_termio::host_term::theme::host_terminal_theme_query_sequence().as_bytes()
        );
        assert!(
            !output
                .windows(shepr_termio::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.len())
                .any(|window| window
                    == shepr_termio::host_term::theme::HOST_COLOR_SCHEME_QUERY_SEQUENCE.as_bytes())
        );
    }

    #[test]
    fn write_host_cell_size_query_emits_xtwinops_request() {
        let mut output = Vec::new();
        write_host_cell_size_query(&mut output).expect("test precondition");

        assert_eq!(output, b"\x1b[16t");
    }

    #[test]
    fn cell_size_fallback_prefers_reported_then_previous_size() {
        let estimated = |width, height| HostCell::from_host(width, height, false);
        let unreported = AtomicCellSize::new();
        assert_eq!(
            fallback_geometry(&unreported, None).cell(),
            estimated(8, 16)
        );
        assert_eq!(
            fallback_geometry(&unreported, CellReport::new(11, 22)).cell(),
            estimated(11, 22)
        );
        let reported = AtomicCellSize::new();
        reported.store(CellReport::new(10, 21));
        assert_eq!(
            fallback_geometry(&reported, CellReport::new(11, 22)).cell(),
            estimated(10, 21)
        );
        // A stored value with a zero axis unpacks to no report.
        let half_reported = AtomicCellSize(AtomicU64::new(pack_cell_size(10, 0)));
        assert_eq!(
            fallback_geometry(&half_reported, None).cell(),
            estimated(8, 16)
        );
        let half_reported = AtomicCellSize(AtomicU64::new(pack_cell_size(0, 21)));
        assert_eq!(
            fallback_geometry(&half_reported, None).cell(),
            estimated(8, 16)
        );
    }

    #[test]
    fn reported_cell_size_is_taken_from_host_cell_size_events() {
        let events = shepr_test_fixtures::parse_raw_input_bytes_sync(b"\x1b[?997;1n");
        assert_eq!(reported_cell_size_from_events(&events), None);

        let events = shepr_test_fixtures::parse_raw_input_bytes_sync(b"\x1b[6;21;10t\x1b[6;18;9t");
        assert_eq!(
            reported_cell_size_from_events(&events),
            CellReport::new(9, 18)
        );
    }

    #[test]
    fn ioctl_cell_size_accepts_fractional_terminal_geometry() {
        assert_eq!(ioctl_cell_size(80, 24, 800, 480), CellReport::new(10, 20));
        assert_eq!(ioctl_cell_size(80, 24, 805, 480), CellReport::new(10, 20));
        assert_eq!(ioctl_cell_size(80, 24, 800, 485), CellReport::new(10, 20));
        assert_eq!(ioctl_cell_size(80, 24, 0, 485), None);
    }
}
