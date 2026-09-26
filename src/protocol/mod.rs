//! Shared wire protocol and presentation encoding code.

pub mod codec;
pub mod endpoint;
mod frame;
mod framing;
mod input;
mod limits;
mod message;
pub mod preamble;
mod projection;
mod style;
mod surface;
pub(crate) mod surface_delta;
pub(crate) mod surface_reuse;
#[cfg(test)]
mod wire_tests;
pub(crate) use limits::frame_payload_fits;

pub use frame::*;
pub use framing::*;
pub use input::*;
pub use limits::*;
pub use message::*;
pub use projection::*;
pub use style::*;
pub(crate) use style::{RATATUI_UNDERLINE_STYLE_MASK, RATATUI_UNDERLINE_STYLE_SHIFT};
pub use surface::*;
