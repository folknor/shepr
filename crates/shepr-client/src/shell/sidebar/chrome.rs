//! Sidebar choices keep their value and persistence origin together.

use super::sidebar_tokens::SectionSplit;
use crate::shell::overlays::preferences::ClientChromePreferences;
use crate::shell::state::ClientShellConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ChromeOrigin {
    Default,
    Configured,
    Remembered,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) struct Chrome<T> {
    value: T,
    origin: ChromeOrigin,
}

impl<T: Copy> Chrome<T> {
    pub(in crate::shell) fn new(value: T, origin: ChromeOrigin) -> Self {
        Self { value, origin }
    }

    pub(in crate::shell) fn value(self) -> T {
        self.value
    }

    pub(in crate::shell) fn set_manual(&mut self, value: T) {
        self.value = value;
        self.origin = ChromeOrigin::Manual;
    }

    pub(in crate::shell) fn remembered_value(self) -> Option<T> {
        matches!(self.origin, ChromeOrigin::Remembered | ChromeOrigin::Manual).then_some(self.value)
    }
}

/// The resolved chrome used by initial sizing and the shell's first layout.
#[derive(Clone, Copy)]
pub(in crate::shell) struct InitialChrome {
    pub(in crate::shell) width: Chrome<shepr_config::SidebarWidth>,
    pub(in crate::shell) collapsed: Chrome<bool>,
    pub(in crate::shell) split: Chrome<SectionSplit>,
}

impl ClientShellConfig {
    /// One resolution for initial surface sizing and the shell's first
    /// layout, so the first surface request matches the first drawn chrome.
    pub(in crate::shell) fn initial_chrome(&self) -> InitialChrome {
        let preferences = &self.preferences;
        InitialChrome {
            width: if preferences.configured.sidebar_width {
                Chrome::new(self.sidebar_width, ChromeOrigin::Configured)
            } else {
                preferences.sidebar_width.map_or_else(
                    || Chrome::new(self.sidebar_width, ChromeOrigin::Default),
                    |width| {
                        Chrome::new(
                            self.sidebar_bounds.clamp_width(width),
                            ChromeOrigin::Remembered,
                        )
                    },
                )
            },
            collapsed: if preferences.configured.sidebar_collapsed {
                Chrome::new(self.sidebar_start_collapsed, ChromeOrigin::Configured)
            } else {
                preferences.sidebar_collapsed.map_or_else(
                    || Chrome::new(self.sidebar_start_collapsed, ChromeOrigin::Default),
                    |collapsed| Chrome::new(collapsed, ChromeOrigin::Remembered),
                )
            },
            split: preferences.sidebar_section_split.map_or_else(
                || Chrome::new(SectionSplit::DEFAULT, ChromeOrigin::Default),
                |split| Chrome::new(split, ChromeOrigin::Remembered),
            ),
        }
    }
}

/// Sidebar values and their origin. Remembered and manual values are persisted;
/// every width is within the configured bounds, and reset restores the
/// configured or default width.
pub(in crate::shell) struct ChromeLayout {
    width: Chrome<shepr_config::SidebarWidth>,
    collapsed: Chrome<bool>,
    split: Chrome<SectionSplit>,
    bounds: shepr_config::SidebarBounds,
    configured_width: Chrome<shepr_config::SidebarWidth>,
}

impl ChromeLayout {
    pub(in crate::shell) fn new(config: &ClientShellConfig) -> Self {
        let preferences = &config.preferences;
        let initial = config.initial_chrome();
        Self {
            width: initial.width,
            collapsed: initial.collapsed,
            split: initial.split,
            bounds: config.sidebar_bounds,
            configured_width: Chrome::new(
                config.sidebar_width,
                if preferences.configured.sidebar_width {
                    ChromeOrigin::Configured
                } else {
                    ChromeOrigin::Default
                },
            ),
        }
    }

    pub(in crate::shell) fn width(&self) -> u16 {
        self.width.value().value()
    }

    pub(in crate::shell) fn collapsed(&self) -> bool {
        self.collapsed.value()
    }

    pub(in crate::shell) fn split(&self) -> SectionSplit {
        self.split.value()
    }

    /// Sets a user-chosen width, clamped to the configured bounds. Returns
    /// whether the width changed.
    pub(in crate::shell) fn set_width(&mut self, width: u16) -> bool {
        let width = self.bounds.clamp_width(width);
        if self.width.value().value() == width.value() {
            return false;
        }
        self.width.set_manual(width);
        true
    }

    /// Returns to the configured width, which is then no longer a user choice.
    pub(in crate::shell) fn reset_width(&mut self) {
        self.width = self.configured_width;
    }

    pub(in crate::shell) fn set_collapsed(&mut self, collapsed: bool) {
        self.collapsed.set_manual(collapsed);
    }

    pub(in crate::shell) fn toggle_collapsed(&mut self) {
        self.set_collapsed(!self.collapsed.value());
    }

    /// Sets a user-chosen section split. Returns whether it changed.
    pub(in crate::shell) fn set_split(&mut self, split: SectionSplit) -> bool {
        if self.split.value() == split {
            return false;
        }
        self.split.set_manual(split);
        true
    }

    /// The values the user chose; the rest stay unset.
    pub(in crate::shell) fn preferences(&self) -> ClientChromePreferences {
        ClientChromePreferences {
            sidebar_width: self
                .width
                .remembered_value()
                .map(shepr_config::SidebarWidth::value),
            sidebar_collapsed: self.collapsed.remembered_value(),
            sidebar_section_split: self.split.remembered_value(),
            ..ClientChromePreferences::default()
        }
    }
}

#[cfg(test)]
impl<T: Copy> Chrome<T> {
    pub(in crate::shell) fn origin(self) -> ChromeOrigin {
        self.origin
    }
}

#[cfg(test)]
impl ChromeLayout {
    pub(in crate::shell) fn width_origin(&self) -> ChromeOrigin {
        self.width.origin()
    }

    pub(in crate::shell) fn collapsed_origin(&self) -> ChromeOrigin {
        self.collapsed.origin()
    }
}
