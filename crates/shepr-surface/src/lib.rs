//! What a pane surface means, above the wire that carries it: the server's
//! delta planner (`delta`), the client's decoder and its baseline (`decode`),
//! conversion from what ratatui renderers draw (`ratatui_conversion`), the
//! wide-glyph rule for pane rows (`pane_row`), the repair of glyphs split by
//! chrome laid over a frame (`glyph_repair`), and the client's frame
//! composition over a shape-checked grid (`compose`). `shepr-protocol` keeps the wire types,
//! codec, framing and preamble; this crate holds the policy and state built on
//! them, so the server and the client share one rule for each.

pub mod compose;
pub mod decode;
pub mod delta;
pub mod glyph_repair;
mod limits;
pub mod pane_row;
pub mod ratatui_conversion;
