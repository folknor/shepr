//! Test fixtures and doubles that several crates' tests share, built only on
//! the public API of the crates they stand in for.
//!
//! # Why this is a crate rather than a feature
//!
//! These helpers used to live in the production crates themselves, behind
//! `test-support` and `test-api` cargo features that each consumer's
//! dev-dependency switched on. A feature on a production crate is not a
//! boundary: cargo unifies features across one build graph, so a build that
//! compiles any consumer's tests turns the feature on for every consumer of
//! that crate, the shipped binary included, and the configuration `brokkr
//! install` ships was compiled by no gate step. A separate crate that nothing
//! production depends on needs no such care: the absence of a normal or build
//! dependency edge holds under every feature resolution, and `brokkr.toml`'s
//! `test-fixtures-never-ships` rule states it. Where a double has to reach
//! inside a production type, the production crate offers a seam (a trait the
//! double implements, or a public constructor) instead of test-only code.
//!
//! # Why this is not `shepr-test-support`
//!
//! `shepr-test-support` is the isolation leaf (scratch directories, the
//! environment guard, the fixture program) that every crate's tests take,
//! including the crates these fixtures are built on. A fixture that needs
//! `shepr-config` cannot live there: `shepr-config`'s own tests would then
//! link a second copy of `shepr-config` through it.
//!
//! # Which crates may take it
//!
//! Any crate above the ones listed in this crate's `[dependencies]`. A crate
//! this one depends on cannot, for the same second-copy reason; its own unit
//! tests keep their `#[cfg(test)]` helpers.

mod child_io;
mod config;
mod termio;

pub use child_io::ChannelChildIo;
pub use config::{AppPathsFixture, ValidatedConfigFixture};
pub use termio::{parse_raw_input_bytes_sync, parse_sgr_mouse_report};

/// Encode `value` with the wire codec into a fresh buffer.
pub fn encode_to_vec<T: serde::Serialize + ?Sized>(
    value: &T,
) -> Result<Vec<u8>, shepr_protocol::codec::CodecError> {
    let mut encoded = Vec::new();
    shepr_protocol::codec::encode_into(&mut encoded, value)?;
    Ok(encoded)
}
