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
//! tests keep their `#[cfg(test)]` helpers. `brokkr.toml`'s
//! `test-fixtures-only-above-its-closure` rule forbids those dev edges.

mod child_io;
mod config;
mod termio;

pub use child_io::ChannelChildIo;
pub use config::{AppPathsFixture, ValidatedClientConfigFixture, ValidatedServerConfigFixture};
pub use termio::{parse_raw_input_bytes_sync, parse_sgr_mouse_report};

/// A fixed pane id for tests that key state by pane without a layout.
pub fn fixed_pane_id(raw: u32) -> shepr_core::layout::PaneId {
    shepr_core::layout::PaneId::from_raw(raw)
}

/// A typed id parsed from its canonical text, for tests that spell workspace,
/// pane or terminal ids as literals.
///
/// # Panics
///
/// Panics when `text` is not canonical for `T`.
pub fn id<T>(text: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    text.parse()
        .unwrap_or_else(|error| panic!("{text:?}: {error}"))
}

/// A canonical boot id for tests, one per `process_id`: the id a server with
/// that process id would take at the epoch. Distinct numbers give distinct
/// boot ids, so tests name servers by number.
pub fn fixed_boot_id(process_id: u32) -> shepr_protocol::BootId {
    shepr_protocol::BootId::from_process_clock(process_id, Ok(std::time::Duration::ZERO))
}

/// Encode `value` with the wire codec into a fresh buffer.
pub fn encode_to_vec<T: serde::Serialize + ?Sized>(
    value: &T,
) -> Result<Vec<u8>, shepr_protocol::codec::CodecError> {
    let mut encoded = Vec::new();
    shepr_protocol::codec::encode_into(&mut encoded, value)?;
    Ok(encoded)
}
