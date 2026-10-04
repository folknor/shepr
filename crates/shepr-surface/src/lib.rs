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

/// Counters at small positions for this crate's tests, reached the way an
/// owner reaches them: by stepping from zero.
#[cfg(test)]
mod test_counters {
    use shepr_protocol::{ContentRevision, ProjectionRevision, SurfaceRevision};

    pub(crate) fn projection(steps: u64) -> ProjectionRevision {
        (0..steps).fold(ProjectionRevision::ZERO, |revision, _| {
            revision.checked_next().expect("a small test position")
        })
    }

    pub(crate) fn surface(steps: u64) -> SurfaceRevision {
        (0..steps).fold(SurfaceRevision::ZERO, |revision, _| {
            revision.checked_next().expect("a small test position")
        })
    }

    pub(crate) fn content(mutations: u64) -> ContentRevision {
        let mut revision = ContentRevision::default();
        for _ in 0..mutations {
            revision.advance();
        }
        revision
    }
}
