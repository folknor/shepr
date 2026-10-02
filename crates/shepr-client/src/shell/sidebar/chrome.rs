//! Sidebar choices keep their value and persistence origin together.

use super::sidebar_tokens::SectionSplit;
use crate::shell::overlays::preferences::ClientChromePreferences;
use crate::shell::state::ClientShellConfig;

/// The sidebar width, collapse and section split the user sees, each with
/// whether the user chose it (only chosen values are persisted). Every width
/// it holds is within the configured bounds: a remembered or dragged width is
/// clamped on entry, and a reset returns to the configured width.
pub(in crate::shell) struct ChromeLayout {
    width: u16,
    width_manual: bool,
    collapsed: bool,
    collapsed_manual: bool,
    split: SectionSplit,
    split_manual: bool,
    bounds: shepr_config::SidebarBounds,
    configured_width: u16,
}

impl ChromeLayout {
    pub(in crate::shell) fn new(config: &ClientShellConfig) -> Self {
        let preferences = &config.preferences;
        Self {
            width: preferences
                .sidebar_width
                .map_or(config.sidebar_width, |width| {
                    config.sidebar_bounds.clamp_width(width)
                }),
            width_manual: preferences.sidebar_width.is_some(),
            collapsed: preferences
                .sidebar_collapsed
                .unwrap_or(config.sidebar_start_collapsed),
            collapsed_manual: preferences.sidebar_collapsed.is_some(),
            split: preferences
                .sidebar_section_split
                .unwrap_or(SectionSplit::DEFAULT),
            split_manual: preferences.sidebar_section_split.is_some(),
            bounds: config.sidebar_bounds,
            configured_width: config.sidebar_width,
        }
    }

    pub(in crate::shell) fn width(&self) -> u16 {
        self.width
    }

    pub(in crate::shell) fn collapsed(&self) -> bool {
        self.collapsed
    }

    pub(in crate::shell) fn split(&self) -> SectionSplit {
        self.split
    }

    /// Sets a user-chosen width, clamped to the configured bounds. Returns
    /// whether the width changed.
    pub(in crate::shell) fn set_width(&mut self, width: u16) -> bool {
        let width = self.bounds.clamp_width(width);
        if self.width == width {
            return false;
        }
        self.width = width;
        self.width_manual = true;
        true
    }

    /// Returns to the configured width, which is then no longer a user choice.
    pub(in crate::shell) fn reset_width(&mut self) {
        self.width = self.configured_width;
        self.width_manual = false;
    }

    pub(in crate::shell) fn set_collapsed(&mut self, collapsed: bool) {
        self.collapsed = collapsed;
        self.collapsed_manual = true;
    }

    pub(in crate::shell) fn toggle_collapsed(&mut self) {
        self.set_collapsed(!self.collapsed);
    }

    /// Sets a user-chosen section split. Returns whether it changed.
    pub(in crate::shell) fn set_split(&mut self, split: SectionSplit) -> bool {
        if self.split == split {
            return false;
        }
        self.split = split;
        self.split_manual = true;
        true
    }

    /// The values the user chose; the rest stay unset.
    pub(in crate::shell) fn preferences(&self) -> ClientChromePreferences {
        ClientChromePreferences {
            sidebar_width: self.width_manual.then_some(self.width),
            sidebar_collapsed: self.collapsed_manual.then_some(self.collapsed),
            sidebar_section_split: self.split_manual.then_some(self.split),
            ..ClientChromePreferences::default()
        }
    }
}
