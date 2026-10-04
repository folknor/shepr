//! Mouse protocol vocabulary and child-facing mouse report encoding.

use crossterm::event::{KeyModifiers, MouseEventKind};
use shepr_core::geometry::{GridSize, HostCell, PanePixelExtent};
use shepr_core::limits::UTF8_MAX_BYTES_PER_CODEPOINT;

use crate::key::tables::{
    MOUSE_BUTTON_RELEASE, MOUSE_DRAG_OFFSET, mouse_button_code, mouse_modifier_bits,
    mouse_scroll_code,
};
use crate::limits::UTF8_MOUSE_REPORT_INITIAL_CAPACITY;

/// A pointer position delivered to a pane. A pixel position keeps the cell it
/// lies in, so a pane that is not in mode 1016 gets that cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    variant_size_differences,
    reason = "a Copy value of at most a dozen bytes; boxing the pixel form would allocate per mouse event"
)]
pub enum Position {
    Cell {
        column: u16,
        row: u16,
    },
    Pixels {
        column: u16,
        row: u16,
        x: u32,
        y: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProtocolMode {
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    Default,
    Utf8,
    Sgr,
}

/// The mouse protocol selected by the child. `encoding` is used for cell
/// coordinates; the 1016 bit lives in `PanePixelMouse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseProtocol {
    pub mode: MouseProtocolMode,
    pub encoding: MouseEncoding,
}

/// What one pane offers pixel mouse: whether its child set mode 1016 and the
/// extent the child was told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PanePixelMouse {
    requested: bool,
    extent: Option<PanePixelExtent>,
}

impl PanePixelMouse {
    pub const OFF: Self = Self {
        requested: false,
        extent: None,
    };

    pub const fn new(requested: bool, extent: Option<PanePixelExtent>) -> Self {
        Self { requested, extent }
    }

    pub const fn requested(self) -> bool {
        self.requested
    }

    pub const fn extent(self) -> Option<PanePixelExtent> {
        self.extent
    }
}

/// A pixel position a client mapped into a pane's extent, with the extent it
/// mapped against (so a report that crossed a resize is recognised).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PixelReport {
    x: u32,
    y: u32,
    extent: PanePixelExtent,
}

impl PixelReport {
    pub const fn new(x: u32, y: u32, extent: PanePixelExtent) -> Self {
        Self { x, y, extent }
    }

    pub const fn x(self) -> u32 {
        self.x
    }

    pub const fn y(self) -> u32 {
        self.y
    }

    pub const fn extent(self) -> PanePixelExtent {
        self.extent
    }
}

/// The one pixel mouse eligibility rule. A connection whose host measured its
/// cell exactly, looking at a pane whose child asked for 1016 and has a known
/// extent, presented at that extent's grid, may address the pane in pixels;
/// the returned extent is the one to map into.
pub fn pixel_mouse_eligible(
    host: HostCell,
    pane: PanePixelMouse,
    presented: GridSize,
) -> Option<PanePixelExtent> {
    let extent = pane.extent()?;
    (host.is_exact() && pane.requested() && extent.grid() == presented).then_some(extent)
}

/// Admission of one received pixel report: eligible at the report's own grid,
/// mapped against the pane's current extent, and inside it.
pub fn admit_pixel_report(host: HostCell, pane: PanePixelMouse, report: PixelReport) -> bool {
    pixel_mouse_eligible(host, pane, report.extent().grid())
        .is_some_and(|extent| extent == report.extent() && extent.contains(report.x(), report.y()))
}

/// Encodes a pointer event for a pane's child, reading the 1016 bit and the
/// extent from the same `PanePixelMouse`:
/// - 1016 and `Pixels`: SGR-pixels at `(x, y)`.
/// - 1016 and `Cell` with an extent: SGR-pixels at `extent.cell_origin`.
/// - 1016 and `Cell` without an extent: SGR at the cell (the child cannot
///   know a cell size either, and a report beats a dropped click).
/// - no 1016: the cell (a `Pixels` position reports its `column`, `row`), in
///   the child's cell encoding.
pub fn encode_pane_mouse_report(
    kind: MouseEventKind,
    position: Position,
    modifiers: KeyModifiers,
    protocol: MouseProtocol,
    pane: PanePixelMouse,
) -> Option<Vec<u8>> {
    let cell_encoding = match protocol.encoding {
        MouseEncoding::Default => MouseProtocolEncoding::Default,
        MouseEncoding::Utf8 => MouseProtocolEncoding::Utf8,
        MouseEncoding::Sgr => MouseProtocolEncoding::Sgr,
    };
    let cell = |column: u16, row: u16| (u32::from(column) + 1, u32::from(row) + 1);
    let (encoding, x, y) = match (pane.requested(), position) {
        (true, Position::Pixels { x, y, .. }) => (MouseProtocolEncoding::SgrPixels, x, y),
        (true, Position::Cell { column, row }) => match pane.extent() {
            Some(extent) => {
                let (x, y) = extent.cell_origin(column, row);
                (MouseProtocolEncoding::SgrPixels, x, y)
            }
            None => {
                let (x, y) = cell(column, row);
                (MouseProtocolEncoding::Sgr, x, y)
            }
        },
        (false, Position::Cell { column, row } | Position::Pixels { column, row, .. }) => {
            let (x, y) = cell(column, row);
            (cell_encoding, x, y)
        }
    };
    encode_mouse_event(kind, x, y, modifiers, protocol.mode, encoding)
}

/// The host mouse reporting a client is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HostMouseCapture {
    Off,
    Cells,
    Pixels,
}

impl HostMouseCapture {
    /// `Off` unless `capture`; `Pixels` when also `pixels`.
    pub const fn new(capture: bool, pixels: bool) -> Self {
        match (capture, pixels) {
            (false, _) => Self::Off,
            (true, false) => Self::Cells,
            (true, true) => Self::Pixels,
        }
    }

    pub const fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub const fn pixels(self) -> bool {
        matches!(self, Self::Pixels)
    }

    /// The client's race guard: a `Pixels` request is applied as `Cells` when
    /// the client's own newest host cell is not exact (the server's answer to
    /// an older geometry).
    pub fn effective(self, host: HostCell) -> Self {
        match self {
            Self::Pixels if !host.is_exact() => Self::Cells,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocolEncoding {
    Default,
    Utf8,
    Sgr,
    SgrPixels,
}

/// `column` and `row` are the final 1-based coordinates to report.
fn encode_mouse_cb(
    base_button: u16,
    release: bool,
    column: u32,
    row: u32,
    modifiers: KeyModifiers,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    // Mouse reports are not a bijection: legacy release loses the button,
    // and host decoding also accepts extended-button motion we cannot emit.
    // SGR reports which button was released; the legacy encodings report
    // every release as button 3.
    let sgr = matches!(
        encoding,
        MouseProtocolEncoding::Sgr | MouseProtocolEncoding::SgrPixels
    );
    let mut cb = if release && !sgr {
        u16::from(MOUSE_BUTTON_RELEASE)
    } else {
        base_button
    };
    cb += mouse_modifier_bits(modifiers);

    match encoding {
        MouseProtocolEncoding::Sgr | MouseProtocolEncoding::SgrPixels => Some(
            format!(
                "\x1b[<{cb};{column};{row}{}",
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        ),
        MouseProtocolEncoding::Default => {
            let cb = u8::try_from(cb + 32).ok()?;
            let column = u8::try_from(column + 32).ok()?;
            let row = u8::try_from(row + 32).ok()?;
            Some(vec![0x1b, b'[', b'M', cb, column, row])
        }
        MouseProtocolEncoding::Utf8 => {
            // UTF-8 mouse mode encodes coordinates as one or two UTF-8 bytes;
            // xterm's extended-coordinate range ends at position 2015.
            if column > 2015 || row > 2015 {
                return None;
            }
            let mut bytes = Vec::with_capacity(UTF8_MOUSE_REPORT_INITIAL_CAPACITY);
            bytes.extend_from_slice(b"\x1b[M");
            push_mouse_codepoint(&mut bytes, cb as u32 + 32)?;
            push_mouse_codepoint(&mut bytes, column + 32)?;
            push_mouse_codepoint(&mut bytes, row + 32)?;
            Some(bytes)
        }
    }
}

fn push_mouse_codepoint(bytes: &mut Vec<u8>, value: u32) -> Option<()> {
    let ch = char::from_u32(value)?;
    let mut buf = [0u8; UTF8_MAX_BYTES_PER_CODEPOINT];
    bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Some(())
}

/// Encode a mouse event as an xterm mouse report. `x` and `y` are 1-based
/// cell coordinates (or pixel coordinates for SGR-pixels). Returns `None`
/// when the protocol mode does not report this kind of event or the position
/// cannot be represented in the encoding.
pub fn encode_mouse_event(
    kind: MouseEventKind,
    x: u32,
    y: u32,
    modifiers: KeyModifiers,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let (base_button, release) = match kind {
        MouseEventKind::Down(button) => (mouse_button_code(button)?, false),
        MouseEventKind::Up(button) => (mouse_button_code(button)?, true),
        MouseEventKind::Drag(button) => (mouse_button_code(button)? + MOUSE_DRAG_OFFSET, false),
        MouseEventKind::Moved => (u16::from(MOUSE_BUTTON_RELEASE) + MOUSE_DRAG_OFFSET, false),
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => (mouse_scroll_code(kind)?, false),
    };
    let reported = match mode {
        // X10 reports button presses only.
        MouseProtocolMode::Press => (!release && base_button < 32) || base_button >= 64,
        MouseProtocolMode::PressRelease => !(32..64).contains(&base_button),
        MouseProtocolMode::ButtonMotion => kind != MouseEventKind::Moved,
        MouseProtocolMode::AnyMotion => true,
    };
    if !reported {
        return None;
    }
    let modifiers = if mode == MouseProtocolMode::Press {
        KeyModifiers::empty()
    } else {
        modifiers
    };
    encode_mouse_cb(base_button, release, x, y, modifiers, encoding)
}

/// Test-only: production mouse reports go through `encode_mouse_event`.
#[cfg(test)]
fn encode_mouse_scroll(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let button = match kind {
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => mouse_scroll_code(kind)?,
        _ => return None,
    };
    encode_mouse_cb(
        button,
        false,
        u32::from(column) + 1,
        u32::from(row) + 1,
        modifiers,
        encoding,
    )
}

/// Test-only: production mouse reports go through `encode_mouse_event`.
#[cfg(test)]
fn encode_mouse_button(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let (button, release) = match kind {
        MouseEventKind::Down(button) => (mouse_button_code(button)?, false),
        MouseEventKind::Up(button) => (mouse_button_code(button)?, true),
        MouseEventKind::Drag(button) => (mouse_button_code(button)? + MOUSE_DRAG_OFFSET, false),
        _ => return None,
    };
    encode_mouse_cb(
        button,
        release,
        u32::from(column) + 1,
        u32::from(row) + 1,
        modifiers,
        encoding,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_extent() -> PanePixelExtent {
        PanePixelExtent::new(GridSize::clamped(80, 24), 720, 432).expect("nonzero")
    }

    fn test_protocol(encoding: MouseEncoding) -> MouseProtocol {
        MouseProtocol {
            mode: MouseProtocolMode::AnyMotion,
            encoding,
        }
    }

    #[test]
    fn encoder_maps_cells_to_pitch_origins_under_1016_and_pixels_to_their_cell_without_it() {
        use crossterm::event::MouseButton;
        let press = MouseEventKind::Down(MouseButton::Left);
        let none = KeyModifiers::empty();
        let on = PanePixelMouse::new(true, Some(test_extent()));
        let off = PanePixelMouse::new(false, Some(test_extent()));
        let sgr = test_protocol(MouseEncoding::Sgr);

        // 1016 and a cell: the cell's top-left pixel, 1-based.
        assert_eq!(
            encode_pane_mouse_report(press, Position::Cell { column: 2, row: 3 }, none, sgr, on),
            Some(b"\x1b[<0;19;55M".to_vec())
        );
        // 1016 and pixels: the pixels as given.
        assert_eq!(
            encode_pane_mouse_report(
                press,
                Position::Pixels {
                    column: 2,
                    row: 3,
                    x: 48,
                    y: 139
                },
                none,
                sgr,
                on
            ),
            Some(b"\x1b[<0;48;139M".to_vec())
        );
        // No 1016: a cell in the child's cell encoding, and a pixel position
        // reports the cell it carries.
        assert_eq!(
            encode_pane_mouse_report(press, Position::Cell { column: 2, row: 3 }, none, sgr, off),
            Some(b"\x1b[<0;3;4M".to_vec())
        );
        assert_eq!(
            encode_pane_mouse_report(
                press,
                Position::Pixels {
                    column: 2,
                    row: 3,
                    x: 48,
                    y: 139
                },
                none,
                sgr,
                off
            ),
            Some(b"\x1b[<0;3;4M".to_vec())
        );
        assert_eq!(
            encode_pane_mouse_report(
                press,
                Position::Cell { column: 2, row: 3 },
                none,
                test_protocol(MouseEncoding::Default),
                off
            ),
            Some(vec![0x1b, b'[', b'M', 32, 35, 36])
        );
    }

    #[test]
    fn encoder_sends_sgr_cells_when_1016_has_no_extent() {
        use crossterm::event::MouseButton;
        let on = PanePixelMouse::new(true, None);
        assert_eq!(
            encode_pane_mouse_report(
                MouseEventKind::Down(MouseButton::Left),
                Position::Cell { column: 2, row: 3 },
                KeyModifiers::empty(),
                test_protocol(MouseEncoding::Default),
                on
            ),
            Some(b"\x1b[<0;3;4M".to_vec())
        );
    }

    #[test]
    fn pixel_mouse_needs_an_exact_host_a_requesting_pane_and_a_matching_presentation() {
        let extent = test_extent();
        let cell = shepr_core::geometry::CellPx::new(9, 18).expect("valid");
        let grid = extent.grid();
        let on = PanePixelMouse::new(true, Some(extent));
        assert_eq!(
            pixel_mouse_eligible(HostCell::Exact(cell), on, grid),
            Some(extent)
        );
        // Each condition alone fails.
        assert_eq!(
            pixel_mouse_eligible(HostCell::Estimated(cell), on, grid),
            None
        );
        assert_eq!(pixel_mouse_eligible(HostCell::Unknown, on, grid), None);
        assert_eq!(
            pixel_mouse_eligible(
                HostCell::Exact(cell),
                PanePixelMouse::new(false, Some(extent)),
                grid
            ),
            None
        );
        assert_eq!(
            pixel_mouse_eligible(HostCell::Exact(cell), PanePixelMouse::new(true, None), grid),
            None
        );
        assert_eq!(
            pixel_mouse_eligible(HostCell::Exact(cell), on, GridSize::clamped(79, 24)),
            None
        );
    }

    #[test]
    fn admission_refuses_a_stale_extent_and_out_of_range_pixels() {
        let extent = test_extent();
        let cell = shepr_core::geometry::CellPx::new(9, 18).expect("valid");
        let exact = HostCell::Exact(cell);
        let pane = PanePixelMouse::new(true, Some(extent));
        assert!(admit_pixel_report(
            exact,
            pane,
            PixelReport::new(1, 1, extent)
        ));
        // The same grid at other pixels: the pane was resized under the report.
        let same_grid = PanePixelExtent::new(extent.grid(), 800, 480).expect("nonzero");
        assert!(!admit_pixel_report(
            exact,
            pane,
            PixelReport::new(1, 1, same_grid)
        ));
        // Another grid.
        let other_grid =
            PanePixelExtent::new(GridSize::clamped(70, 24), 630, 432).expect("nonzero");
        assert!(!admit_pixel_report(
            exact,
            pane,
            PixelReport::new(1, 1, other_grid)
        ));
        assert!(!admit_pixel_report(
            exact,
            pane,
            PixelReport::new(0, 1, extent)
        ));
        assert!(!admit_pixel_report(
            exact,
            pane,
            PixelReport::new(721, 1, extent)
        ));
        assert!(!admit_pixel_report(
            HostCell::Estimated(cell),
            pane,
            PixelReport::new(1, 1, extent)
        ));
    }

    #[test]
    fn host_mouse_capture_downgrades_pixels_on_an_inexact_host() {
        let cell = shepr_core::geometry::CellPx::new(9, 18).expect("valid");
        assert_eq!(HostMouseCapture::new(false, true), HostMouseCapture::Off);
        assert_eq!(HostMouseCapture::new(true, false), HostMouseCapture::Cells);
        assert_eq!(HostMouseCapture::new(true, true), HostMouseCapture::Pixels);
        assert!(!HostMouseCapture::Off.enabled());
        assert!(HostMouseCapture::Cells.enabled() && !HostMouseCapture::Cells.pixels());
        assert!(HostMouseCapture::Pixels.enabled() && HostMouseCapture::Pixels.pixels());
        let pixels = HostMouseCapture::Pixels;
        assert_eq!(
            pixels.effective(HostCell::Exact(cell)),
            HostMouseCapture::Pixels
        );
        assert_eq!(
            pixels.effective(HostCell::Estimated(cell)),
            HostMouseCapture::Cells
        );
        assert_eq!(pixels.effective(HostCell::Unknown), HostMouseCapture::Cells);
        assert_eq!(
            HostMouseCapture::Off.effective(HostCell::Exact(cell)),
            HostMouseCapture::Off
        );
        assert_eq!(
            HostMouseCapture::Cells.effective(HostCell::Unknown),
            HostMouseCapture::Cells
        );
    }

    #[test]
    fn pixel_report_admission_needs_the_same_extent_and_an_exact_host() {
        let extent = test_extent();
        let cell = shepr_core::geometry::CellPx::new(9, 18).expect("valid");
        let exact = HostCell::Exact(cell);
        let pane = PanePixelMouse::new(true, Some(extent));
        assert!(admit_pixel_report(
            exact,
            pane,
            PixelReport::new(720, 432, extent)
        ));
        assert!(!admit_pixel_report(
            exact,
            pane,
            PixelReport::new(0, 1, extent)
        ));
        assert!(!admit_pixel_report(
            exact,
            pane,
            PixelReport::new(721, 1, extent)
        ));
        assert!(!admit_pixel_report(
            HostCell::Estimated(cell),
            pane,
            PixelReport::new(1, 1, extent)
        ));
        assert!(!admit_pixel_report(
            exact,
            PanePixelMouse::new(false, Some(extent)),
            PixelReport::new(1, 1, extent)
        ));
        let other = PanePixelExtent::new(GridSize::clamped(80, 24), 800, 480).expect("nonzero");
        assert!(!admit_pixel_report(
            exact,
            pane,
            PixelReport::new(1, 1, other)
        ));
        assert_eq!(
            pixel_mouse_eligible(exact, pane, GridSize::clamped(80, 24)),
            Some(extent)
        );
        assert_eq!(
            pixel_mouse_eligible(exact, pane, GridSize::clamped(81, 24)),
            None
        );
    }

    #[test]
    fn sgr_mouse_scroll_encodes_wheel_button_and_coordinates() {
        let encoded = encode_mouse_scroll(
            crossterm::event::MouseEventKind::ScrollDown,
            4,
            6,
            KeyModifiers::SHIFT,
            MouseProtocolEncoding::Sgr,
        )
        .expect("mouse scroll should encode");

        assert_eq!(encoded, b"\x1b[<69;5;7M");
    }

    #[test]
    fn sgr_mouse_release_keeps_button_code() {
        let encoded = encode_mouse_button(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            11,
            9,
            KeyModifiers::empty(),
            MouseProtocolEncoding::Sgr,
        )
        .expect("mouse release should encode");

        assert_eq!(encoded, b"\x1b[<0;12;10m");
    }

    #[test]
    fn utf8_mouse_encoding_caps_coordinates_at_xterms_limit() {
        let encoded = encode_mouse_cb(
            0,
            false,
            2015,
            2015,
            KeyModifiers::empty(),
            MouseProtocolEncoding::Utf8,
        );
        assert_eq!(encoded, Some(b"\x1b[M \xdf\xbf\xdf\xbf".to_vec()));

        assert_eq!(
            encode_mouse_cb(
                0,
                false,
                2016,
                1,
                KeyModifiers::empty(),
                MouseProtocolEncoding::Utf8,
            ),
            None
        );
        assert_eq!(
            encode_mouse_cb(
                0,
                false,
                1,
                2016,
                KeyModifiers::empty(),
                MouseProtocolEncoding::Utf8,
            ),
            None
        );
    }

    #[test]
    fn mouse_events_are_filtered_by_protocol_mode() {
        use crossterm::event::MouseButton;

        let press = MouseEventKind::Down(MouseButton::Left);
        let release = MouseEventKind::Up(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let sgr = MouseProtocolEncoding::Sgr;
        let none = KeyModifiers::empty();

        assert_eq!(
            encode_mouse_event(
                press,
                1,
                1,
                KeyModifiers::SHIFT,
                MouseProtocolMode::Press,
                sgr
            ),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(release, 1, 1, none, MouseProtocolMode::Press, sgr),
            None
        );
        assert_eq!(
            encode_mouse_event(release, 2, 3, none, MouseProtocolMode::PressRelease, sgr),
            Some(b"\x1b[<0;2;3m".to_vec())
        );
        assert_eq!(
            encode_mouse_event(drag, 2, 3, none, MouseProtocolMode::PressRelease, sgr),
            None
        );
        assert_eq!(
            encode_mouse_event(drag, 2, 3, none, MouseProtocolMode::ButtonMotion, sgr),
            Some(b"\x1b[<32;2;3M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                MouseEventKind::Moved,
                2,
                3,
                none,
                MouseProtocolMode::ButtonMotion,
                sgr
            ),
            None
        );
        assert_eq!(
            encode_mouse_event(
                MouseEventKind::ScrollUp,
                48,
                139,
                none,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::SgrPixels
            ),
            Some(b"\x1b[<64;48;139M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                release,
                1,
                1,
                none,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default
            ),
            Some(vec![0x1b, b'[', b'M', 3 + 32, 33, 33])
        );
    }
}
