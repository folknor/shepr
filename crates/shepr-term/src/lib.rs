//! Terminal vocabulary and pure encoding, shared by the emulator, the server
//! and the client without linking the emulator.
//!
//! This crate holds the values both sides of a pane speak about (row and
//! point coordinates, selections, scroll metrics, colours, underline shapes,
//! DEC modes, keyboard and mouse protocol modes, display widths, the host's
//! observed theme and cell size), the VT spellings shepr writes (`seq`), key
//! identity and chord matching (`key`), and the child-facing key and mouse
//! encoders. It holds no emulator state and no host terminal I/O: the
//! emulator adapter lives in `shepr-vt`, and reading or writing the host
//! terminal lives in `shepr-termio`.

mod color;
mod coords;
pub mod host;
pub mod key;
mod limits;
mod modes;
pub mod mouse;
pub mod scroll;
pub mod selection;
pub mod seq;
mod style;
pub mod width;

pub use color::{
    ColorQueryTarget, ColorScheme, DefaultColor, NAMED_COLOR_COUNT, RgbColor, default_palette,
    default_palette_color,
};
pub use coords::{AbsRow, Point, ScreenRow, ViewportPosition, ViewportRow};
pub use modes::{DecMode, FocusEvent, KittyKeyboardFlags, ModifyOtherKeysLevel, encode_focus};
pub use mouse::{MouseEncoding, MouseProtocol, MouseProtocolMode};
pub use scroll::{ScrollMetrics, ScrollMetricsFields};
pub use style::UnderlineStyle;
