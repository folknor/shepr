//! Distinct counters for shell topology, rendered surfaces, and endpoint connections.

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

            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }

            pub fn checked_next(self) -> Option<Self> {
                self.0.checked_add(1).map(Self)
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }

        impl PartialEq<u64> for $name {
            fn eq(&self, other: &u64) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<$name> for u64 {
            fn eq(&self, other: &$name) -> bool {
                *self == other.get()
            }
        }

        impl PartialOrd<u64> for $name {
            fn partial_cmp(&self, other: &u64) -> Option<std::cmp::Ordering> {
                self.0.partial_cmp(other)
            }
        }
    };
}

counter!(ProjectionRevision);
counter!(SurfaceRevision);
counter!(ConnectionGeneration);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_domains_advance_independently() {
        assert_eq!(
            ProjectionRevision::ZERO.checked_next(),
            Some(ProjectionRevision::new(1))
        );
        assert_eq!(
            SurfaceRevision::new(7)
                .checked_next()
                .map(SurfaceRevision::get),
            Some(8)
        );
        assert_eq!(ConnectionGeneration::new(u64::MAX).checked_next(), None);
    }
}
