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
pub use limits::{
    BUILD_ID, MAX_CELL_SIZE_PX, MAX_CLIENT_REQUEST_BYTES, MAX_ENDPOINT_COMMAND_BYTES,
    MAX_ENDPOINT_RESPONSE_CHUNK_BYTES, MAX_FRAME_SIZE, MAX_INITIAL_REQUEST_BYTES,
    MAX_INPUT_PAYLOAD, MAX_SURFACE_CELLS, MAX_SURFACE_DIMENSION, MAX_SURFACE_HYPERLINKS,
    MAX_SURFACE_PANES, MAX_SURFACE_PATCH_SPANS, MAX_SURFACE_SPLIT_PATH, MAX_SURFACE_SPLITS,
    MAX_TERMINAL_FRAME_BYTES, MIN_SURFACE_DIMENSION, SURFACE_BYTES_PER_CELL, frame_payload_fits,
    surface_grid_size,
};

pub use frame::*;
pub use framing::*;
pub use geometry::*;
pub use identity::*;
pub use ids::{
    PublicChildId, PublicIdParseError, PublicPaneId, PublicTabId, decode_public_number,
    encode_public_number,
};
pub use ids::{TerminalId, WorkspaceId};
pub use input::*;
pub use message::*;
pub use projection::*;
pub use revision::*;
pub use status::*;
pub use style::*;
pub use style::{RATATUI_UNDERLINE_STYLE_MASK, RATATUI_UNDERLINE_STYLE_SHIFT};
pub use surface::*;

/// Version advertised by the JSON API, using the same build ID as the wire preamble.
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
