//! Sidebar choices keep their value and persistence origin together.

use super::sidebar_tokens::SectionSplit;
use crate::shell::config::ClientShellConfig;
use crate::shell::sidebar::preferences::ClientChromePreferences;

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

    pub(super) fn remembered_value(self) -> Option<T> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_config::ClientConfig;

    const MIN: u16 = 20;
    const MAX: u16 = 40;

    /// A shell config whose sidebar width lies within `MIN..=MAX`, set in client.toml when
    /// `configured_width` is given, with nothing remembered.
    fn config(configured_width: Option<u16>) -> ClientShellConfig {
        let mut values = ClientConfig::default();
        values.ui.sidebar_min_width = MIN;
        values.ui.sidebar_max_width = MAX;
        values.ui.sidebar_width = configured_width;
        ClientShellConfig::from_config(&values)
    }

    fn nothing_remembered(chrome: &ChromeLayout) -> bool {
        let preferences = chrome.preferences();
        preferences.sidebar_width.is_none()
            && preferences.sidebar_collapsed.is_none()
            && preferences.sidebar_section_split.is_none()
            && preferences.agent_panel_sort.is_none()
    }

    #[test]
    fn a_fresh_layout_takes_the_defaults_and_remembers_nothing() {
        let config = config(None);
        let chrome = ChromeLayout::new(&config);
        assert_eq!(chrome.width(), config.sidebar_width.value());
        assert_eq!(chrome.width_origin(), ChromeOrigin::Default);
        assert_eq!(chrome.collapsed(), config.sidebar_start_collapsed);
        assert_eq!(chrome.collapsed_origin(), ChromeOrigin::Default);
        assert_eq!(chrome.split(), SectionSplit::DEFAULT);
        assert!(nothing_remembered(&chrome));
    }

    #[test]
    fn set_width_clamps_to_the_bounds_and_reports_only_a_change() {
        let config = config(None);
        let mut chrome = ChromeLayout::new(&config);
        // The width already shown is no change and no choice.
        assert!(!chrome.set_width(config.sidebar_width.value()));
        assert_eq!(chrome.width_origin(), ChromeOrigin::Default);
        assert!(nothing_remembered(&chrome));

        assert!(chrome.set_width(u16::MAX));
        assert_eq!(chrome.width(), MAX);
        assert_eq!(chrome.width_origin(), ChromeOrigin::Manual);
        // Past the bound again clamps to the same width.
        assert!(!chrome.set_width(MAX + 1));
        assert!(chrome.set_width(0));
        assert_eq!(chrome.width(), MIN);
        assert_eq!(chrome.preferences().sidebar_width, Some(MIN));
    }

    #[test]
    fn a_remembered_width_is_clamped_kept_and_dropped_by_reset() {
        let mut config = config(None);
        config.preferences.sidebar_width = Some(MAX + 100);
        let mut chrome = ChromeLayout::new(&config);
        assert_eq!(chrome.width(), MAX);
        assert_eq!(chrome.width_origin(), ChromeOrigin::Remembered);
        assert_eq!(chrome.preferences().sidebar_width, Some(MAX));

        chrome.reset_width();
        assert_eq!(chrome.width(), config.sidebar_width.value());
        assert_eq!(chrome.width_origin(), ChromeOrigin::Default);
        assert_eq!(chrome.preferences().sidebar_width, None);
    }

    #[test]
    fn reset_returns_a_dragged_width_to_the_configured_one() {
        let configured = 30;
        let mut chrome = ChromeLayout::new(&config(Some(configured)));
        assert_eq!(chrome.width(), configured);
        assert_eq!(chrome.width_origin(), ChromeOrigin::Configured);
        assert!(nothing_remembered(&chrome));

        assert!(chrome.set_width(configured + 5));
        assert_eq!(chrome.width(), configured + 5);
        chrome.reset_width();
        assert_eq!(chrome.width(), configured);
        assert_eq!(chrome.width_origin(), ChromeOrigin::Configured);
        assert_eq!(chrome.preferences().sidebar_width, None);
    }

    #[test]
    fn collapsing_by_hand_is_remembered_whichever_way_it_goes() {
        let config = config(None);
        let start = config.sidebar_start_collapsed;
        let mut chrome = ChromeLayout::new(&config);
        chrome.toggle_collapsed();
        assert_eq!(chrome.collapsed(), !start);
        assert_eq!(chrome.collapsed_origin(), ChromeOrigin::Manual);
        assert_eq!(chrome.preferences().sidebar_collapsed, Some(!start));
        // Back to the starting state is still the user's choice.
        chrome.toggle_collapsed();
        assert_eq!(chrome.collapsed(), start);
        assert_eq!(chrome.preferences().sidebar_collapsed, Some(start));
        chrome.set_collapsed(true);
        assert!(chrome.collapsed());
        assert_eq!(chrome.preferences().sidebar_collapsed, Some(true));
    }

    #[test]
    fn a_remembered_collapse_and_split_start_the_layout_and_stay_remembered() {
        let split = SectionSplit::from_drag(shepr_core::layout::MIN_SPLIT_RATIO);
        let mut config = config(None);
        config.preferences.sidebar_collapsed = Some(!config.sidebar_start_collapsed);
        config.preferences.sidebar_section_split = Some(split);
        let chrome = ChromeLayout::new(&config);
        assert_eq!(chrome.collapsed(), !config.sidebar_start_collapsed);
        assert_eq!(chrome.collapsed_origin(), ChromeOrigin::Remembered);
        assert_eq!(chrome.split(), split);
        let preferences = chrome.preferences();
        assert_eq!(
            preferences.sidebar_collapsed,
            Some(!config.sidebar_start_collapsed)
        );
        assert_eq!(preferences.sidebar_section_split, Some(split));
        assert_eq!(preferences.sidebar_width, None);
    }

    #[test]
    fn set_split_reports_only_a_change_and_remembers_it() {
        let mut chrome = ChromeLayout::new(&config(None));
        assert!(!chrome.set_split(SectionSplit::DEFAULT));
        assert!(nothing_remembered(&chrome));

        let split = SectionSplit::from_drag(shepr_core::layout::MIN_SPLIT_RATIO);
        assert_ne!(split, SectionSplit::DEFAULT);
        assert!(chrome.set_split(split));
        assert_eq!(chrome.split(), split);
        assert_eq!(chrome.preferences().sidebar_section_split, Some(split));
        assert!(!chrome.set_split(split));
    }
}
