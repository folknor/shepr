//! Shared wire protocol and presentation encoding code.

pub mod codec;
pub mod command;
pub mod endpoint;
mod frame;
mod framing;
mod geometry;
mod identity;
mod ids;
mod input;
mod limits;
mod message;
mod pane_row;
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
pub use limits::{
    BUILD_ID, InputBatchCharge, MAX_CELL_SIZE_PX, MAX_CLIENT_MESSAGE_SIZE,
    MAX_CLIENT_REQUEST_BYTES, MAX_FRAME_SIZE, MAX_INITIAL_REQUEST_BYTES, MAX_INPUT_EVENT_BATCH,
    MAX_INPUT_PAYLOAD, MAX_MESSAGE_SIZE, MAX_SURFACE_CELLS, MAX_SURFACE_DIMENSION,
    MAX_SURFACE_HYPERLINKS, MAX_SURFACE_PANES, MAX_SURFACE_PATCH_SPANS, MAX_SURFACE_SPLIT_PATH,
    MAX_SURFACE_SPLITS, MIN_SURFACE_DIMENSION, surface_grid_size,
};

pub use frame::*;
pub use framing::*;
pub use geometry::*;
pub use identity::*;
pub use ids::{PublicIdParseError, PublicPaneId, decode_public_number, encode_public_number};
pub use ids::{TerminalId, TerminalIdParseError, WorkspaceId, WorkspaceIdParseError};
pub use input::*;
pub use message::*;
pub use pane_row::{blank_pane_cell, normalize_pane_row, pane_row_is_normalized};
pub use projection::*;
pub use revision::*;
pub use status::*;
pub use style::*;
pub use surface::*;

/// Human-readable version advertised by the JSON API and executable.
/// It includes the build ID for display; `ping` also carries `build_id` as its
/// separate machine-comparison field.
pub fn build_version() -> String {
    format!("{}+{}", env!("CARGO_PKG_VERSION"), limits::BUILD_ID)
}

/// Whether `id` states a build identity: exactly the sixteen lowercase hex
/// digits the build script mints. The build script's marker for a build whose
/// inputs could not be established is not hex, and neither is an empty,
/// truncated or garbled field, so none of them is an identity.
pub fn is_identifiable_build_id(id: &str) -> bool {
    id.len() == preamble::BUILD_ID_BYTES
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether two builds are provably the same build: `ours` states an identity
/// and `peer` states the same one. Not reflexive on purpose: a build that
/// cannot establish its own identity matches nothing, itself included, so it
/// is refused everywhere rather than attaching to whatever answers.
pub fn builds_match(ours: &str, peer: &str) -> bool {
    is_identifiable_build_id(ours) && ours == peer
}

/// Whether a peer that announced `peer` is this exact build. Every build
/// comparison (the preamble, `ping`, `status`, the CLI's per-command check and
/// the remote checks) goes through here.
pub fn is_this_build(peer: &str) -> bool {
    builds_match(BUILD_ID, peer)
}

#[cfg(test)]
mod wire_tests;

/// The workspace build script, compiled as a module so its identity recipe is
/// tested against the same code that stamps `BUILD_ID`.
#[cfg(test)]
#[path = "../../../build.rs"]
mod build_script;

#[cfg(test)]
mod build_identity_tests;

#[cfg(test)]
mod build_version_tests {
    use super::*;

    #[test]
    fn version_carries_the_build_fingerprint() {
        assert_eq!(
            build_version(),
            format!("{}+{}", env!("CARGO_PKG_VERSION"), BUILD_ID)
        );
        assert!(is_identifiable_build_id(BUILD_ID), "{BUILD_ID}");
        assert!(is_this_build(BUILD_ID));
    }
}
