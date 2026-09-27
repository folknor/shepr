//! Shared wire protocol and presentation encoding code.

pub mod codec;
pub mod endpoint;
mod frame;
mod framing;
mod geometry;
mod identity;
mod ids;
mod input;
mod limits;
mod message;
pub mod preamble;
mod projection;
mod ratatui_conversion;
mod revision;
mod status;
mod style;
mod surface;
pub mod surface_delta;
pub mod surface_reuse;
mod theme_conversion;
#[cfg(test)]
mod wire_tests;
pub use limits::frame_payload_fits;

pub use frame::*;
pub use framing::*;
pub use geometry::*;
pub use identity::*;
pub use ids::{PublicPaneId, PublicTabId, decode_public_number, encode_public_number};
pub use ids::{TerminalId, WorkspaceId};
pub use input::*;
pub use limits::*;
pub use message::*;
pub use projection::*;
pub use revision::*;
pub use status::*;
pub use style::*;
pub use style::{RATATUI_UNDERLINE_STYLE_MASK, RATATUI_UNDERLINE_STYLE_SHIFT};
pub use surface::*;
