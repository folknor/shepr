//! Row coordinates used at terminal and selection boundaries.

/// A row offset from the top of the currently displayed viewport.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct ViewportRow(pub u16);

impl ViewportRow {
    /// The viewport row that screen row `row` is, for a viewport whose top row
    /// is on screen row `top`; `None` for a row above the viewport.
    pub fn on_screen(row: u16, top: u16) -> Option<Self> {
        row.checked_sub(top).map(Self)
    }
}

/// The position of a stable row relative to a viewport's top row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportPosition {
    Above,
    At(ViewportRow),
    Below,
}

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
///
/// The field stays public and the arithmetic saturating on purpose: callers
/// clamp to the retained rows, and viewport conversion already reports
/// out-of-range rows through `checked_sub`. Making the field private would
/// force a constructor on every test literal without removing a failure mode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct AbsRow(pub u64);

/// A terminal cell position whose row coordinate space is carried by `R`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
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
    pub fn saturating_add(self, rows: usize) -> Self {
        Self(
            self.0
                .saturating_add(u64::try_from(rows).unwrap_or(u64::MAX)),
        )
    }

    pub fn saturating_sub(self, rows: usize) -> Self {
        Self(
            self.0
                .saturating_sub(u64::try_from(rows).unwrap_or(u64::MAX)),
        )
    }

    /// Convert a viewport-relative offset to its stable row identity.
    pub fn from_viewport_top(top: Self, row: ViewportRow) -> Self {
        Self(top.0.saturating_add(u64::from(row.0)))
    }

    /// Convert this stable row identity to a viewport-relative position.
    pub fn viewport_row(self, top: Self) -> ViewportPosition {
        let Some(offset) = self.0.checked_sub(top.0) else {
            return ViewportPosition::Above;
        };
        u16::try_from(offset).map_or(ViewportPosition::Below, |row| {
            ViewportPosition::At(ViewportRow(row))
        })
    }
}
