//! Grid and cell pixel dimensions shared by the terminal, PTY, and client.

use std::num::{NonZeroU16, NonZeroU32};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SplitBranch {
    First,
    Second,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSize {
    pub cols: NonZeroU16,
    pub rows: NonZeroU16,
}

impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        Some(Self {
            cols: NonZeroU16::new(cols)?,
            rows: NonZeroU16::new(rows)?,
        })
    }

    pub fn clamped(cols: u16, rows: u16) -> Self {
        Self {
            cols: NonZeroU16::new(cols).unwrap_or(NonZeroU16::MIN),
            rows: NonZeroU16::new(rows).unwrap_or(NonZeroU16::MIN),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellPx {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl CellPx {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Some(Self {
            width: NonZeroU32::new(width)?,
            height: NonZeroU32::new(height)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneGeometry {
    pub grid: GridSize,
    pub cell: Option<CellPx>,
}

impl PaneGeometry {
    pub fn new(cols: u16, rows: u16, width: u32, height: u32) -> Self {
        Self {
            grid: GridSize::clamped(cols, rows),
            cell: CellPx::new(width, height),
        }
    }

    pub fn cols(self) -> u16 {
        self.grid.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.grid.rows.get()
    }

    pub fn cell_width(self) -> u32 {
        self.cell.map_or(0, |cell| cell.width.get())
    }

    pub fn cell_height(self) -> u32 {
        self.cell.map_or(0, |cell| cell.height.get())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostGeometry {
    pub pane: PaneGeometry,
    pub exact: bool,
}

impl HostGeometry {
    pub fn new(cols: u16, rows: u16, width: u32, height: u32, exact: bool) -> Self {
        let pane = PaneGeometry::new(cols, rows, width, height);
        Self {
            pane,
            exact: exact && pane.cell.is_some(),
        }
    }

    pub fn cols(self) -> u16 {
        self.pane.cols()
    }

    pub fn rows(self) -> u16 {
        self.pane.rows()
    }

    pub fn cell_width(self) -> u32 {
        self.pane.cell_width()
    }

    pub fn cell_height(self) -> u32 {
        self.pane.cell_height()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_rejects_zero_components() {
        assert!(GridSize::new(0, 24).is_none());
        assert!(CellPx::new(8, 0).is_none());
        assert_eq!(PaneGeometry::new(80, 24, 8, 0).cell, None);
        assert!(!HostGeometry::new(80, 24, 8, 0, true).exact);
    }
}
