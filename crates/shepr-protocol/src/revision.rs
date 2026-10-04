//! Distinct counters for shell topology, rendered surfaces, pane content and
//! endpoint connections. None converts to or from a bare integer: a counter
//! starts at its zero, advances by its own rule and travels as its own wire
//! value.

use serde::{Deserialize, Serialize};

macro_rules! counter {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            Default,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);
            /// The position after [`Self::ZERO`], where a counter that
            /// names its first thing starts.
            pub const FIRST: Self = Self(1);

            pub fn checked_next(self) -> Option<Self> {
                self.0.checked_add(1).map(Self)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

counter!(ProjectionRevision);
counter!(SurfaceRevision);
counter!(ConnectionGeneration);

/// Advances on every render-visible mutation of a pane's terminal core, under
/// the core lock, and travels on the wire in a pane's surface geometry. An
/// owner-held value is always even. A full surface spans several core holds,
/// so a revision sent for one is certified by [`Self::certify`], which makes it
/// odd when the cells it describes may be torn; a retained patch reads its
/// cells and revision under one hold and needs no certificate. A receiver only
/// compares revisions for equality, and an odd one never names a stable
/// surface ([`Self::is_stable`]).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ContentRevision(u64);

impl ContentRevision {
    /// Records one render-visible mutation. Wraps: a pane never reaches the
    /// end of the sequence, and parity survives the wrap.
    pub fn advance(&mut self) {
        self.0 = self.0.wrapping_add(2);
    }

    /// Whether the content changed between `earlier` and this revision.
    pub fn changed_since(self, earlier: Self) -> bool {
        self != earlier
    }

    /// Whether the revision certifies the cells it was sent with.
    pub fn is_stable(self) -> bool {
        self.0.is_multiple_of(2)
    }

    /// The revision of a full draw spanning several core holds, read before
    /// (`None` when it was unreadable) and after. Only unchanged, available
    /// reads certify its cells (an even revision); anything else is odd, which
    /// never names a stable surface. An unreadable core after the draw is odd
    /// too.
    pub fn certify(before: Option<Self>, after: Option<Self>) -> Self {
        Self(match (before, after) {
            (Some(before), Some(after)) if before == after => after.0,
            (_, Some(after)) => after.0 | 1,
            (_, None) => 1,
        })
    }
}

/// A counter of this crate's tests at an arbitrary position, decoded from its
/// wire value: the counters have no constructor from an integer.
#[cfg(test)]
pub(crate) fn at<T: serde::de::DeserializeOwned>(value: u64) -> T {
    crate::codec::from_slice_exact(&crate::codec::to_vec(&value).expect("counter encoding"))
        .expect("counter decoding")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_domains_advance_independently() {
        let one = ProjectionRevision::ZERO.checked_next();
        assert_eq!(one.map(|revision| revision.to_string()), Some("1".into()));
        assert_eq!(
            SurfaceRevision::ZERO
                .checked_next()
                .and_then(SurfaceRevision::checked_next)
                .map(|revision| revision.to_string()),
            Some("2".into())
        );
    }

    #[test]
    fn a_counter_at_the_end_of_its_range_has_no_successor() {
        let max: ConnectionGeneration = at(u64::MAX);
        assert_eq!(max.checked_next(), None);
    }

    #[test]
    fn content_revisions_are_stable_until_certified_odd() {
        let mut revision = ContentRevision::default();
        assert!(revision.is_stable());
        revision.advance();
        assert!(revision.is_stable());
        assert!(revision.changed_since(ContentRevision::default()));
        assert_eq!(
            ContentRevision::certify(Some(revision), Some(revision)),
            revision
        );
        assert!(!ContentRevision::certify(None, Some(revision)).is_stable());
        let mut moved = revision;
        moved.advance();
        assert!(!ContentRevision::certify(Some(revision), Some(moved)).is_stable());
        assert!(!ContentRevision::certify(Some(revision), None).is_stable());
    }
}
