//! Row coordinates used at terminal and selection boundaries.

/// A row offset from the top of the currently displayed viewport.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct ViewportRow(pub u16);

/// A row index in the currently retained screen buffer, starting at its oldest
/// retained row. This index can move when the buffer evicts old history.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct ScreenRow(pub usize);

/// A stable row identity. Rows below the terminal's current history origin
/// have been evicted and are no longer readable.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct AbsRow(pub u64);

/// A terminal cell position whose row coordinate space is carried by `R`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Point<R> {
    pub row: R,
    pub col: u16,
}

impl<R> Point<R> {
    pub const fn new(row: R, col: u16) -> Self {
        Self { row, col }
    }
}

impl AbsRow {
    pub fn saturating_add(self, rows: u64) -> Self {
        Self(self.0.saturating_add(rows))
    }

    pub fn saturating_sub(self, rows: u64) -> Self {
        Self(self.0.saturating_sub(rows))
    }

    /// Convert a viewport-relative offset to its stable row identity.
    pub fn from_viewport_top(top: Self, row: ViewportRow) -> Self {
        Self(top.0.saturating_add(u64::from(row.0)))
    }

    /// Convert this stable row identity to its current retained-buffer index.
    pub fn screen_row(self, origin: Self) -> Option<ScreenRow> {
        usize::try_from(self.0.checked_sub(origin.0)?)
            .ok()
            .map(ScreenRow)
    }

    /// Convert this stable row identity to a viewport-relative offset.
    pub fn viewport_row(self, top: Self) -> ViewportRow {
        ViewportRow(self.0.saturating_sub(top.0).try_into().unwrap_or(u16::MAX))
    }
}

impl From<u64> for AbsRow {
    fn from(row: u64) -> Self {
        Self(row)
    }
}
