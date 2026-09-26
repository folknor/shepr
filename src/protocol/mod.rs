//! Shared wire protocol and presentation encoding code.

pub mod codec;
pub mod endpoint;
pub mod preamble;
pub(crate) mod render_ansi;
pub(crate) mod surface_delta;
pub(crate) mod surface_reuse;
mod wire;

pub use wire::*;
