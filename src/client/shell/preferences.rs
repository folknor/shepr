use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

use serde::{Deserialize, Serialize};

/// Sidebar chrome the user changed by hand, remembered per endpoint across
/// launches.
///
/// Three of these values also have `[ui]` config keys (`sidebar_width`,
/// `sidebar_start_collapsed`, `agent_panel_sort`). The documented key wins:
/// a remembered value is used only while its key is absent from config.toml,
/// and is neither loaded nor stored once the key is set. Manual changes still
/// apply for the rest of the session either way.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct ClientChromePreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_section_split: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_collapsed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent_panel_sort: Option<crate::config::AgentPanelSortConfig>,
    /// Which of the values above config.toml sets. Taken from the config at
    /// launch, never from the file.
    #[serde(skip)]
    pub(super) configured: ConfiguredChrome,
}

/// Which remembered chrome values have a `[ui]` key set in config.toml.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ConfiguredChrome {
    pub(super) sidebar_width: bool,
    pub(super) sidebar_collapsed: bool,
    pub(super) agent_panel_sort: bool,
}

impl ConfiguredChrome {
    pub(super) fn from_config(config: &crate::config::Config) -> Self {
        Self {
            sidebar_width: config.ui.is_user_configured("sidebar_width"),
            sidebar_collapsed: config.ui.is_user_configured("sidebar_start_collapsed"),
            agent_panel_sort: config.ui.is_user_configured("agent_panel_sort"),
        }
    }
}

impl ClientChromePreferences {
    /// These preferences with every value config.toml owns dropped.
    pub(super) fn without_configured(mut self, configured: ConfiguredChrome) -> Self {
        if configured.sidebar_width {
            self.sidebar_width = None;
        }
        if configured.sidebar_collapsed {
            self.sidebar_collapsed = None;
        }
        if configured.agent_panel_sort {
            self.agent_panel_sort = None;
        }
        self.configured = configured;
        self
    }
}

pub(super) fn path_for_local_endpoint(state_dir: &Path, socket_path: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in socket_path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    state_dir
        .join("client-shell")
        .join(format!("local-{hash:016x}.json"))
}

pub(super) fn load(path: &Path) -> Option<ClientChromePreferences> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(super) fn store(path: &Path, preferences: &ClientChromePreferences) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid client shell state path: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create client shell state directory: {error}"))?;
    let content = serde_json::to_vec_pretty(preferences)
        .map_err(|error| format!("failed to encode client shell state: {error}"))?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = path
        .file_name()
        .ok_or_else(|| format!("invalid client shell state path: {}", path.display()))?
        .to_os_string();
    temp_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    let temp_path = parent.join(temp_name);
    std::fs::write(&temp_path, content)
        .map_err(|error| format!("failed to write client shell state: {error}"))?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        format!("failed to replace client shell state: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_paths_are_stable_and_distinct() {
        let scratch = crate::test_support::ScratchDir::new("preferences");
        let first = path_for_local_endpoint(scratch.path(), Path::new("/run/shepr/one.sock"));
        let again = path_for_local_endpoint(scratch.path(), Path::new("/run/shepr/one.sock"));
        let second = path_for_local_endpoint(scratch.path(), Path::new("/run/shepr/two.sock"));
        assert_eq!(first, again);
        assert_ne!(first, second);
    }

    #[test]
    fn legacy_preferences_ignore_unknown_fields() {
        let preferences: ClientChromePreferences =
            serde_json::from_str(r#"{"collapsed_groups":["/repo"],"sidebar_width":24}"#)
                .expect("legacy client chrome preferences");

        assert_eq!(preferences.sidebar_width, Some(24));
    }

    #[test]
    fn configured_chrome_drops_only_the_values_config_owns() {
        let remembered = || ClientChromePreferences {
            sidebar_width: Some(31),
            sidebar_section_split: Some(0.3),
            sidebar_collapsed: Some(true),
            agent_panel_sort: Some(crate::config::AgentPanelSortConfig::Priority),
            configured: ConfiguredChrome::default(),
        };

        let untouched = remembered().without_configured(ConfiguredChrome::default());
        assert_eq!(untouched.sidebar_width, Some(31));
        assert_eq!(untouched.sidebar_collapsed, Some(true));
        assert!(untouched.agent_panel_sort.is_some());

        let configured = ConfiguredChrome {
            sidebar_width: true,
            sidebar_collapsed: true,
            agent_panel_sort: true,
        };
        let owned = remembered().without_configured(configured);
        assert_eq!(owned.sidebar_width, None);
        assert_eq!(owned.sidebar_collapsed, None);
        assert_eq!(owned.agent_panel_sort, None);
        assert_eq!(owned.sidebar_section_split, Some(0.3));
        assert_eq!(owned.configured, configured);
    }

    #[test]
    fn configured_chrome_follows_the_keys_the_user_set() {
        let mut config = crate::config::Config::default();
        assert_eq!(
            ConfiguredChrome::from_config(&config),
            ConfiguredChrome::default()
        );
        config
            .ui
            .user_fields
            .insert("sidebar_start_collapsed".to_owned());
        assert_eq!(
            ConfiguredChrome::from_config(&config),
            ConfiguredChrome {
                sidebar_collapsed: true,
                ..ConfiguredChrome::default()
            }
        );
    }

    #[test]
    fn concurrent_stores_leave_complete_preferences() {
        let scratch = crate::test_support::ScratchDir::new("prefs-concurrent");
        let path = scratch.join("preferences.json");
        let writers = (20..28)
            .map(|width| {
                let path = path.clone();
                std::thread::spawn(move || {
                    store(
                        &path,
                        &ClientChromePreferences {
                            sidebar_width: Some(width),
                            ..ClientChromePreferences::default()
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().expect("preference writer").expect("store");
        }
        assert!(
            load(&path)
                .and_then(|saved| saved.sidebar_width)
                .is_some_and(|width| (20..28).contains(&width))
        );
        std::fs::remove_file(path).expect("remove preferences");
    }

    #[test]
    fn repeated_store_replaces_existing_preferences() {
        let scratch = crate::test_support::ScratchDir::new("prefs-replace");
        let path = scratch.join("preferences.json");
        store(
            &path,
            &ClientChromePreferences {
                sidebar_width: Some(24),
                ..ClientChromePreferences::default()
            },
        )
        .expect("first preference store");
        store(
            &path,
            &ClientChromePreferences {
                sidebar_width: Some(32),
                ..ClientChromePreferences::default()
            },
        )
        .expect("replacement preference store");
        assert_eq!(load(&path).and_then(|saved| saved.sidebar_width), Some(32));
        std::fs::remove_file(path).expect("remove preferences");
    }
}
