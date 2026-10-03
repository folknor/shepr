//! The sidebar chrome remembered across launches, and its file: an atomic
//! replace for each store and a write probe at startup. Failures stay typed
//! until they are presented: a failed store becomes the endpoint error banner,
//! a failed probe the launch error.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

use serde::{Deserialize, Serialize};

/// Sidebar chrome changed by hand, remembered by the client across launches.
/// One file per local server socket, whichever endpoint is presented.
///
/// Three of these values also have `[ui]` config keys (`sidebar_width`,
/// `sidebar_start_collapsed`, `agent_panel_sort`). The documented key wins:
/// a remembered value is used only while its key is absent from client.toml,
/// and is neither loaded nor stored once the key is set. Manual changes still
/// apply for the rest of the session either way.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(in crate::shell) struct ClientChromePreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::shell) sidebar_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::shell) sidebar_section_split:
        Option<crate::shell::sidebar::sidebar_tokens::SectionSplit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::shell) sidebar_collapsed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::shell) agent_panel_sort: Option<shepr_config::AgentPanelSortConfig>,
    /// Which of the values above client.toml sets. Taken from the config at
    /// launch, never from the file.
    #[serde(skip)]
    pub(in crate::shell) configured: ConfiguredChrome,
}

/// Which remembered chrome values have a `[ui]` key set in client.toml.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::shell) struct ConfiguredChrome {
    pub(in crate::shell) sidebar_width: bool,
    pub(in crate::shell) sidebar_collapsed: bool,
    pub(in crate::shell) agent_panel_sort: bool,
}

impl ConfiguredChrome {
    pub(in crate::shell) fn from_validated_config(
        config: &shepr_config::ValidatedClientConfig,
    ) -> Self {
        let ui = config.ui();
        Self {
            sidebar_width: ui.sidebar_width_is_explicit(),
            sidebar_collapsed: ui.sidebar_start_collapsed_is_explicit(),
            agent_panel_sort: ui.agent_panel_sort_is_explicit(),
        }
    }
}

impl ClientChromePreferences {
    /// These preferences with every value client.toml owns dropped.
    pub(in crate::shell) fn without_configured(mut self, configured: ConfiguredChrome) -> Self {
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

/// Why the startup probe found the preferences location unusable.
#[derive(Debug)]
pub(crate) enum PreferencesProbeError {
    /// The path has no parent directory or no file name.
    InvalidPath {
        path: PathBuf,
    },
    CreateDirectory {
        directory: PathBuf,
        source: io::Error,
    },
    CreateProbe {
        probe: PathBuf,
        source: io::Error,
    },
    /// Writing the probe failed; `cleanup` is why it could not be removed
    /// afterwards, when it could not.
    WriteProbe {
        probe: PathBuf,
        source: io::Error,
        cleanup: Option<io::Error>,
    },
    RemoveProbe {
        probe: PathBuf,
        source: io::Error,
    },
}

impl PreferencesProbeError {
    pub(crate) fn kind(&self) -> io::ErrorKind {
        match self {
            Self::InvalidPath { .. } => io::ErrorKind::InvalidInput,
            Self::CreateDirectory { source, .. }
            | Self::CreateProbe { source, .. }
            | Self::WriteProbe { source, .. }
            | Self::RemoveProbe { source, .. } => source.kind(),
        }
    }
}

impl std::fmt::Display for PreferencesProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath { path } => {
                write!(f, "invalid client shell state path: {}", path.display())
            }
            Self::CreateDirectory { directory, source } => write!(
                f,
                "failed to create client shell state directory {}: {source}",
                directory.display()
            ),
            Self::CreateProbe { probe, source } => write!(
                f,
                "failed to create client shell state write probe {}: {source}",
                probe.display()
            ),
            Self::WriteProbe {
                probe,
                source,
                cleanup,
            } => {
                write!(
                    f,
                    "failed to write client shell state probe {}: {source}",
                    probe.display()
                )?;
                if let Some(cleanup) = cleanup {
                    write!(
                        f,
                        "; failed to remove client shell state write probe {}: {cleanup}",
                        probe.display()
                    )?;
                }
                Ok(())
            }
            Self::RemoveProbe { probe, source } => write!(
                f,
                "failed to remove client shell state write probe {}: {source}",
                probe.display()
            ),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for PreferencesProbeError {}

/// The launch reports a probe failure as an IO failure of its kind.
impl From<PreferencesProbeError> for io::Error {
    fn from(error: PreferencesProbeError) -> Self {
        io::Error::new(error.kind(), error)
    }
}

/// Why remembered chrome could not be stored.
#[derive(Debug)]
pub(in crate::shell) enum PreferencesStoreError {
    /// The path has no parent directory or no file name.
    InvalidPath {
        path: PathBuf,
    },
    CreateDirectory(io::Error),
    Encode(serde_json::Error),
    Write(io::Error),
    /// Renaming the written file over the state failed; `cleanup` is why the
    /// written file could not be removed, when it could not.
    Replace {
        source: io::Error,
        temporary: PathBuf,
        cleanup: Option<io::Error>,
    },
}

impl std::fmt::Display for PreferencesStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath { path } => {
                write!(f, "invalid client shell state path: {}", path.display())
            }
            Self::CreateDirectory(error) => {
                write!(f, "failed to create client shell state directory: {error}")
            }
            Self::Encode(error) => write!(f, "failed to encode client shell state: {error}"),
            Self::Write(error) => write!(f, "failed to write client shell state: {error}"),
            Self::Replace {
                source,
                temporary,
                cleanup,
            } => {
                write!(f, "failed to replace client shell state: {source}")?;
                // A failed cleanup leaves a stray file beside the state, so it is
                // named in the error the caller already reports rather than dropped.
                if let Some(cleanup) = cleanup {
                    write!(
                        f,
                        "; the temporary file {} was left behind: {cleanup}",
                        temporary.display()
                    )?;
                }
                Ok(())
            }
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for PreferencesStoreError {}

pub(in crate::shell) fn path_for_local_endpoint(state_dir: &Path, socket_path: &Path) -> PathBuf {
    let hash =
        crate::shell::presentation::topology::fnv1a64(socket_path.to_string_lossy().as_bytes());
    state_dir
        .join("client-shell")
        .join(format!("local-{hash:016x}.json"))
}

pub(in crate::shell) fn load(path: &Path) -> Option<ClientChromePreferences> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to read client chrome preferences; using defaults"
            );
            return None;
        }
    };
    match serde_json::from_str(&content) {
        Ok(preferences) => Some(preferences),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to parse client chrome preferences; using defaults"
            );
            None
        }
    }
}

pub(in crate::shell) fn probe_writable(path: &Path) -> Result<(), PreferencesProbeError> {
    let invalid_path = || PreferencesProbeError::InvalidPath {
        path: path.to_path_buf(),
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(invalid_path)?;
    std::fs::create_dir_all(parent).map_err(|source| PreferencesProbeError::CreateDirectory {
        directory: parent.to_path_buf(),
        source,
    })?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = path.file_name().ok_or_else(invalid_path)?.to_os_string();
    temp_name.push(format!(".write-probe-{}-{sequence}", std::process::id()));
    let temp_path = parent.join(temp_name);
    let mut probe = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|source| PreferencesProbeError::CreateProbe {
            probe: temp_path.clone(),
            source,
        })?;
    let write_error = probe.write_all(&[0]).err();
    drop(probe);
    if let Some(source) = write_error {
        let cleanup = std::fs::remove_file(&temp_path).err();
        return Err(PreferencesProbeError::WriteProbe {
            probe: temp_path,
            source,
            cleanup,
        });
    }
    std::fs::remove_file(&temp_path).map_err(|source| PreferencesProbeError::RemoveProbe {
        probe: temp_path.clone(),
        source,
    })
}

pub(in crate::shell) fn store(
    path: &Path,
    preferences: &ClientChromePreferences,
) -> Result<(), PreferencesStoreError> {
    let invalid_path = || PreferencesStoreError::InvalidPath {
        path: path.to_path_buf(),
    };
    let parent = path.parent().ok_or_else(invalid_path)?;
    std::fs::create_dir_all(parent).map_err(PreferencesStoreError::CreateDirectory)?;
    let content = serde_json::to_vec_pretty(preferences).map_err(PreferencesStoreError::Encode)?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = path.file_name().ok_or_else(invalid_path)?.to_os_string();
    temp_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    let temp_path = parent.join(temp_name);
    std::fs::write(&temp_path, content).map_err(PreferencesStoreError::Write)?;
    std::fs::rename(&temp_path, path).map_err(|source| {
        let cleanup = std::fs::remove_file(&temp_path).err();
        PreferencesStoreError::Replace {
            source,
            temporary: temp_path.clone(),
            cleanup,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Path;
    use super::{
        ClientChromePreferences, ConfiguredChrome, PreferencesProbeError, PreferencesStoreError,
        load, path_for_local_endpoint, probe_writable, store,
    };
    use shepr_test_fixtures::*;

    #[test]
    fn endpoint_paths_are_stable_and_distinct() {
        let scratch = shepr_test_support::ScratchDir::new("preferences");
        let first = path_for_local_endpoint(scratch.path(), Path::new("/run/shepr/one.sock"));
        let again = path_for_local_endpoint(scratch.path(), Path::new("/run/shepr/one.sock"));
        let second = path_for_local_endpoint(scratch.path(), Path::new("/run/shepr/two.sock"));
        assert_eq!(first, again);
        assert_ne!(first, second);
    }

    #[test]
    fn configured_chrome_drops_only_the_values_config_owns() {
        let remembered = || ClientChromePreferences {
            sidebar_width: Some(31),
            sidebar_section_split: crate::shell::sidebar::sidebar_tokens::SectionSplit::new(0.3),
            sidebar_collapsed: Some(true),
            agent_panel_sort: Some(shepr_config::AgentPanelSortConfig::Priority),
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
        assert_eq!(
            owned
                .sidebar_section_split
                .map(crate::shell::sidebar::sidebar_tokens::SectionSplit::get),
            Some(0.3)
        );
        assert_eq!(owned.configured, configured);
    }

    #[test]
    fn configured_chrome_follows_config_value_provenance() {
        let default_config = shepr_config::ValidatedClientConfig::test_default();
        assert_eq!(
            ConfiguredChrome::from_validated_config(&default_config),
            ConfiguredChrome::default()
        );
        // Explicitness comes from the config value being set, so an explicit
        // `false` still counts as configured.
        let mut values = shepr_config::ClientConfig::default();
        values.ui.sidebar_start_collapsed = Some(false);
        let config = shepr_config::ValidatedClientConfig::test_from_config(
            values,
            Some("[ui]\nsidebar_start_collapsed = false\n"),
        );
        assert_eq!(
            ConfiguredChrome::from_validated_config(&config),
            ConfiguredChrome {
                sidebar_collapsed: true,
                ..ConfiguredChrome::default()
            }
        );
    }

    #[test]
    fn concurrent_stores_leave_complete_preferences() {
        let scratch = shepr_test_support::ScratchDir::new("prefs-concurrent");
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
        let scratch = shepr_test_support::ScratchDir::new("prefs-replace");
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

    #[test]
    fn failures_stay_typed_and_format_only_when_presented() {
        let scratch = shepr_test_support::ScratchDir::new("prefs-typed-errors");
        let blocker = scratch.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("block the state directory");
        let path = blocker.join("preferences.json");

        let probe = probe_writable(&path).expect_err("a file stands where the directory goes");
        assert!(matches!(
            &probe,
            PreferencesProbeError::CreateDirectory { directory, .. } if *directory == blocker
        ));
        let launch_error = std::io::Error::from(probe);
        assert!(launch_error.to_string().starts_with(&format!(
            "failed to create client shell state directory {}: ",
            blocker.display()
        )));

        let stored = store(&path, &ClientChromePreferences::default())
            .expect_err("a file stands where the directory goes");
        assert!(matches!(stored, PreferencesStoreError::CreateDirectory(_)));
        assert!(
            stored
                .to_string()
                .starts_with("failed to create client shell state directory: ")
        );
    }
}
