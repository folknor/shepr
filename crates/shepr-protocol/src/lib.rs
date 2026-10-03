//! The wire between shepr processes: its types, the positional codec, frame
//! framing and the connection preamble. What a surface means (delta planning,
//! the client's decoder baseline, ratatui conversion, wide-glyph repair and
//! frame composition) lives in `shepr-surface`, above this crate.

mod build;
pub use build::{BuildIdentity, BuildIdentityParseError, BuildVersion, PACKAGE_VERSION};
pub mod codec;
pub mod command;
pub mod endpoint;
mod frame;
mod framing;
mod geometry;
mod identity;
mod ids;
mod input;
mod limit;
mod limits;
mod message;
pub mod preamble;
mod projection;
mod remote_path;
mod revision;
pub use remote_path::RemotePath;
mod status;
mod style;
mod surface;
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
pub use ids::{
    PanePublicNumber, PublicIdParseError, PublicPaneId, decode_public_number, encode_public_number,
};
pub use ids::{TerminalId, TerminalIdParseError, WorkspaceId, WorkspaceIdParseError};
pub use input::*;
pub use limit::{Limit, LimitExceeded, LimitKind};
pub use message::*;
pub use projection::*;
pub use revision::*;
pub use status::*;
pub use style::*;
pub use surface::*;

/// Human-readable version advertised by the JSON API and executable.
/// It includes the build ID for display; `ping` also carries `build_id` as its
/// separate machine-comparison field.
pub fn build_version() -> String {
    BuildVersion {
        version: PACKAGE_VERSION.to_owned(),
        build_id: BUILD_ID.parse().unwrap_or(BuildIdentity::Unidentifiable),
    }
    .to_string()
}

/// Whether `id` states a build identity: exactly the sixteen lowercase hex
/// digits the build script mints. The build script's marker for a build whose
/// inputs could not be established is not hex, and neither is an empty,
/// truncated or garbled field, so none of them is an identity.
pub fn is_identifiable_build_id(id: &str) -> bool {
    matches!(id.parse(), Ok(BuildIdentity::Known(_)))
}

/// Whether two builds are provably the same build: `ours` states an identity
/// and `peer` states the same one. Not reflexive on purpose: a build that
/// cannot establish its own identity matches nothing, itself included, so it
/// is refused everywhere rather than attaching to whatever answers.
pub fn builds_match(ours: &str, peer: &str) -> bool {
    match (ours.parse::<BuildIdentity>(), peer.parse::<BuildIdentity>()) {
        (Ok(ours), Ok(peer)) => ours.matches(peer),
        _ => false,
    }
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
        assert_eq!(build_version(), format!("{PACKAGE_VERSION}+{BUILD_ID}"));
        assert!(is_identifiable_build_id(BUILD_ID), "{BUILD_ID}");
        assert!(BuildIdentity::for_this_build().is_this_build());
    }
}
