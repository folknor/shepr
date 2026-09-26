use std::collections::HashSet;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::ProfileId;

const CATALOG_VERSION: u32 = 1;
const SELECTION_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 64 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_LABEL_BYTES: usize = 128;
const MAX_TARGET_BYTES: usize = 1024;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedSshEndpoint {
    pub(crate) id: ProfileId,
    pub(crate) label: String,
    pub(crate) target: String,
    pub(crate) session: String,
    pub(crate) enabled: bool,
}

impl SavedSshEndpoint {
    pub(crate) fn new(
        label: impl Into<String>,
        target: impl Into<String>,
        session: impl Into<String>,
    ) -> Result<Self, String> {
        let profile = Self {
            id: ProfileId::generate(),
            label: label.into(),
            target: target.into(),
            session: session.into(),
            enabled: true,
        };
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<(), String> {
        ProfileId::parse(self.id.to_string())?;
        let label = self.label.trim();
        if label.is_empty() {
            return Err("SSH endpoint label cannot be empty".into());
        }
        if label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control) {
            return Err(format!(
                "SSH endpoint label must be at most {MAX_LABEL_BYTES} bytes and contain no control characters"
            ));
        }
        if self.target.len() > MAX_TARGET_BYTES || self.target.chars().any(char::is_control) {
            return Err(format!(
                "SSH target must be at most {MAX_TARGET_BYTES} bytes and contain no control characters"
            ));
        }
        crate::remote::validate_remote_target(&self.target).map(|_| ())?;
        let authority = self.target.strip_prefix("ssh://").unwrap_or(&self.target);
        if authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
        {
            return Err("SSH target must not contain a password".into());
        }
        crate::session::validate_name(&self.session)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointCatalog {
    version: u32,
    /// The in-memory selection. It lives in `endpoint-selection.json`, never in the
    /// profile file: it is not written here, and a value left in an older profile file
    /// is accepted but discarded on load.
    #[serde(default, skip_serializing)]
    pub(crate) selected_profile: Option<ProfileId>,
    #[serde(default)]
    pub(crate) ssh: Vec<SavedSshEndpoint>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointSelection {
    version: u32,
    selected_profile: Option<ProfileId>,
}

impl Default for EndpointCatalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            selected_profile: None,
            ssh: Vec::new(),
        }
    }
}

impl EndpointCatalog {
    pub(crate) fn load() -> Result<Self, String> {
        Self::load_from_paths(&catalog_path(), &selection_path())
    }

    pub(crate) fn load_profiles() -> Result<Vec<SavedSshEndpoint>, String> {
        // Profiles only: each running client holds its selection in memory, and the
        // selection file only seeds the next launch (the last client to commit a
        // handoff wins).
        Self::load_from_path(&catalog_path()).map(|catalog| catalog.ssh)
    }

    fn load_from_paths(catalog_path: &Path, selection_path: &Path) -> Result<Self, String> {
        let mut catalog = Self::load_from_path(catalog_path)?;
        match load_selection_from_path(selection_path) {
            Ok(Some(selection)) => {
                let valid = selection.selected_profile.as_ref().is_none_or(|selected| {
                    catalog
                        .ssh
                        .iter()
                        .any(|profile| &profile.id == selected && profile.enabled)
                });
                if valid {
                    catalog.selected_profile = selection.selected_profile;
                } else {
                    tracing::warn!(
                        path = %selection_path.display(),
                        "saved endpoint selection is absent or disabled; using Local"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %selection_path.display(),
                    "saved endpoint selection is unavailable; using Local"
                );
            }
        }
        Ok(catalog)
    }

    pub(crate) fn store_profiles(&self) -> Result<(), String> {
        self.store_to_path(&catalog_path())
    }

    pub(crate) fn store_selection(&self) -> Result<(), String> {
        self.store_selection_to_path(&selection_path())
    }

    fn store_selection_to_path(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let content = serde_json::to_vec_pretty(&EndpointSelection {
            version: SELECTION_VERSION,
            selected_profile: self.selected_profile.clone(),
        })
        .map_err(|error| format!("failed to encode endpoint selection: {error}"))?;
        store_private_json(path, &content, "endpoint selection")
    }

    pub(crate) fn add_ssh(
        &mut self,
        label: impl Into<String>,
        target: impl Into<String>,
        session: impl Into<String>,
    ) -> Result<ProfileId, String> {
        if self.ssh.len() >= MAX_PROFILES {
            return Err(format!("at most {MAX_PROFILES} SSH endpoints can be saved"));
        }
        let profile = SavedSshEndpoint::new(label, target, session)?;
        let id = profile.id.clone();
        self.ssh.push(profile);
        Ok(id)
    }

    pub(crate) fn rename_ssh(
        &mut self,
        id: &ProfileId,
        label: impl Into<String>,
    ) -> Result<bool, String> {
        let Some(index) = self.ssh.iter().position(|profile| &profile.id == id) else {
            return Ok(false);
        };
        let mut renamed = self.ssh[index].clone();
        renamed.label = label.into();
        renamed.validate()?;
        self.ssh[index] = renamed;
        Ok(true)
    }

    pub(crate) fn remove_ssh(&mut self, id: &ProfileId) -> bool {
        let previous_len = self.ssh.len();
        self.ssh.retain(|profile| &profile.id != id);
        if self.selected_profile.as_ref() == Some(id) {
            self.selected_profile = None;
        }
        self.ssh.len() != previous_len
    }

    pub(crate) fn select_local(&mut self) {
        self.selected_profile = None;
    }

    pub(crate) fn select_endpoint(&mut self, endpoint_id: &super::ClientEndpointId) -> bool {
        match endpoint_id {
            super::ClientEndpointId::Local => {
                self.select_local();
                true
            }
            super::ClientEndpointId::Ssh(profile_id) => self.select_ssh(profile_id),
        }
    }

    pub(crate) fn select_ssh(&mut self, id: &ProfileId) -> bool {
        if !self
            .ssh
            .iter()
            .any(|profile| &profile.id == id && profile.enabled)
        {
            return false;
        }
        self.selected_profile = Some(id.clone());
        true
    }

    pub(crate) fn has_enabled_ssh(&self) -> bool {
        self.ssh.iter().any(|profile| profile.enabled)
    }

    pub(crate) fn contains_enabled_target_session(&self, target: &str, session: &str) -> bool {
        self.ssh.iter().any(|profile| {
            profile.enabled && profile.target == target && profile.session == session
        })
    }

    pub(crate) fn set_enabled(&mut self, id: &ProfileId, enabled: bool) -> bool {
        let Some(profile) = self.ssh.iter_mut().find(|profile| &profile.id == id) else {
            return false;
        };
        profile.enabled = enabled;
        if !enabled && self.selected_profile.as_ref() == Some(id) {
            self.selected_profile = None;
        }
        true
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != CATALOG_VERSION {
            return Err(format!(
                "unsupported endpoint catalog version {}; expected {CATALOG_VERSION}",
                self.version
            ));
        }
        if self.ssh.len() > MAX_PROFILES {
            return Err(format!(
                "endpoint catalog contains more than {MAX_PROFILES} SSH profiles"
            ));
        }
        let mut ids = HashSet::new();
        for profile in &self.ssh {
            profile.validate()?;
            if !ids.insert(profile.id.clone()) {
                return Err(format!("duplicate endpoint profile id {}", profile.id));
            }
        }
        if self.selected_profile.as_ref().is_some_and(|selected| {
            !self
                .ssh
                .iter()
                .any(|profile| &profile.id == selected && profile.enabled)
        }) {
            return Err("selected SSH endpoint is absent or disabled in the catalog".into());
        }
        Ok(())
    }

    fn load_from_path(path: &Path) -> Result<Self, String> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(format!(
                    "failed to open endpoint catalog {}: {error}",
                    path.display()
                ));
            }
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect endpoint catalog: {error}"))?;
        if metadata.len() > MAX_CATALOG_BYTES {
            return Err("endpoint catalog exceeds the storage limit".into());
        }
        let mut content = String::new();
        file.take(MAX_CATALOG_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|error| format!("failed to read endpoint catalog: {error}"))?;
        if content.len() as u64 > MAX_CATALOG_BYTES {
            return Err("endpoint catalog exceeds the storage limit".into());
        }
        let mut catalog: Self = serde_json::from_str(&content)
            .map_err(|error| format!("stored endpoint catalog is invalid: {error}"))?;
        catalog.selected_profile = None;
        catalog.validate()?;
        Ok(catalog)
    }

    fn store_to_path(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let content = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("failed to encode endpoint catalog: {error}"))?;
        store_private_json(path, &content, "endpoint catalog")
    }
}

/// How often an open client looks at the saved-machine catalog file for changes.
const CATALOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Identity of the catalog file as last seen. Writers replace the file by rename, so a
/// change shows up as a new inode even when size and mtime happen to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CatalogFingerprint {
    device: u64,
    inode: u64,
    len: u64,
    modified_sec: i64,
    modified_nsec: i64,
}

fn catalog_fingerprint(path: &Path) -> Option<CatalogFingerprint> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(path).ok()?;
    Some(CatalogFingerprint {
        device: metadata.dev(),
        inode: metadata.ino(),
        len: metadata.len(),
        modified_sec: metadata.mtime(),
        modified_nsec: metadata.mtime_nsec(),
    })
}

/// Watches the saved-machine catalog for an open client. The catalog is state, not config:
/// `shepr machine add/remove/enable/disable` rewrite it while clients run, and those clients
/// pick the change up here instead of at their next launch.
///
/// It polls a `stat` at most once per `CATALOG_POLL_INTERVAL` and reloads only when the
/// file's identity changed. The first poll always reloads, so a write that landed between
/// the client's launch-time load and the watcher's creation is not missed; applying an
/// unchanged catalog is a no-op.
pub(crate) struct EndpointCatalogWatch {
    path: PathBuf,
    /// `None` until the first poll; `Some(None)` when the file was absent.
    seen: Option<Option<CatalogFingerprint>>,
    next_poll: Instant,
}

impl EndpointCatalogWatch {
    pub(crate) fn new(now: Instant) -> Self {
        Self::for_path(catalog_path(), now)
    }

    fn for_path(path: PathBuf, now: Instant) -> Self {
        Self {
            path,
            seen: None,
            next_poll: now,
        }
    }

    /// The saved profiles when the file changed since the last poll, or `None` when it did
    /// not (or it is not time to look yet). An unreadable or invalid file is reported once
    /// per change; the caller keeps the profiles it has.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Result<Vec<SavedSshEndpoint>, String>> {
        if now < self.next_poll {
            return None;
        }
        self.next_poll = now + CATALOG_POLL_INTERVAL;
        let fingerprint = catalog_fingerprint(&self.path);
        if self.seen == Some(fingerprint) {
            return None;
        }
        self.seen = Some(fingerprint);
        Some(EndpointCatalog::load_from_path(&self.path).map(|catalog| catalog.ssh))
    }
}

/// What a catalog change means for connections: which saved machines stop being
/// supervised and which start. A machine counts as live when it is enabled; a live machine
/// whose target or session changed is both retired and started, because its connector,
/// bridge and remote server all belong to the old target. A label change is neither.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EndpointCatalogChanges {
    pub(crate) retired: Vec<ProfileId>,
    pub(crate) started: Vec<SavedSshEndpoint>,
}

impl EndpointCatalogChanges {
    pub(crate) fn between(previous: &[SavedSshEndpoint], next: &[SavedSshEndpoint]) -> Self {
        fn same_live_machine(a: &SavedSshEndpoint, b: &SavedSshEndpoint) -> bool {
            a.id == b.id && a.enabled && b.enabled && a.target == b.target && a.session == b.session
        }
        let retired = previous
            .iter()
            .filter(|old| old.enabled)
            .filter(|old| !next.iter().any(|new| same_live_machine(old, new)))
            .map(|old| old.id.clone())
            .collect();
        let started = next
            .iter()
            .filter(|new| new.enabled)
            .filter(|new| !previous.iter().any(|old| same_live_machine(old, new)))
            .cloned()
            .collect();
        Self { retired, started }
    }
}

impl EndpointCatalog {
    /// Replaces the saved profiles with a newer copy of the catalog file, keeping this
    /// client's in-memory selection only while it still names an enabled machine.
    pub(crate) fn replace_profiles(&mut self, profiles: Vec<SavedSshEndpoint>) {
        self.ssh = profiles;
        if let Some(selected) = self.selected_profile.as_ref()
            && !self.is_selectable(selected)
        {
            self.selected_profile = None;
        }
    }

    /// The endpoint this client wants to own the pane surface.
    pub(crate) fn selected_endpoint(&self) -> super::ClientEndpointId {
        self.selected_profile
            .as_ref()
            .map_or(super::ClientEndpointId::Local, |profile_id| {
                super::ClientEndpointId::Ssh(profile_id.clone())
            })
    }

    /// Whether `id` names an enabled saved machine, i.e. whether it may be selected.
    pub(crate) fn is_selectable(&self, id: &ProfileId) -> bool {
        self.ssh
            .iter()
            .any(|profile| &profile.id == id && profile.enabled)
    }
}

fn load_selection_from_path(path: &Path) -> Result<Option<EndpointSelection>, String> {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read endpoint selection: {error}")),
    };
    if content.len() as u64 > MAX_CATALOG_BYTES {
        return Err("endpoint selection exceeds the storage limit".into());
    }
    let selection: EndpointSelection = serde_json::from_slice(&content)
        .map_err(|error| format!("stored endpoint selection is invalid: {error}"))?;
    if selection.version != SELECTION_VERSION {
        return Err(format!(
            "unsupported endpoint selection version {}; expected {SELECTION_VERSION}",
            selection.version
        ));
    }
    Ok(Some(selection))
}

pub(super) fn store_private_json(
    path: &Path,
    content: &[u8],
    description: &str,
) -> Result<(), String> {
    if content.len() as u64 > MAX_CATALOG_BYTES {
        return Err(format!("{description} exceeds the storage limit"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid {description} path: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {description} directory: {error}"))?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!(
            "refusing to replace {description} through a non-file path"
        ));
    }

    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(".endpoints-{}-{sequence}.tmp", std::process::id()));
    let mut temp = crate::platform::create_private_file(&temp_path)
        .map_err(|error| format!("failed to create {description}: {error}"))?;
    if let Err(error) = temp.write_all(content).and_then(|()| temp.sync_all()) {
        drop(temp);
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("failed to write {description}: {error}"));
    }
    drop(temp);
    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("failed to activate {description}: {error}"));
    }
    crate::platform::sync_directory(parent)
        .map_err(|error| format!("failed to persist {description} directory: {error}"))
}

pub(crate) fn catalog_path() -> PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("endpoints.json")
}

fn selection_path() -> PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("endpoint-selection.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog path whose directory does not exist yet, in a scratch
    /// directory kept until the test process exits.
    fn path(name: &str) -> PathBuf {
        crate::test_support::ScratchDir::new(name)
            .keep_until_exit()
            .join("client")
            .join("endpoints.json")
    }

    #[test]
    fn catalog_roundtrip_persists_profiles_without_secret_fields() {
        let path = path("roundtrip");
        let _ = std::fs::remove_dir_all(path.parent().expect("test precondition"));
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "ssh://dev@build.example:2222", "agents")
            .expect("test precondition");
        assert!(catalog.select_ssh(&id));
        catalog.store_to_path(&path).expect("test precondition");

        let encoded = std::fs::read_to_string(&path).expect("test precondition");
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("private_key"));
        assert!(!encoded.contains("control_socket"));
        // The selection belongs to the selection file, not the shared profile list.
        assert!(!encoded.contains("selected_profile"));
        let loaded = EndpointCatalog::load_from_path(&path).expect("test precondition");
        assert_eq!(loaded.ssh, catalog.ssh);
        assert_eq!(loaded.selected_profile, None);
        assert_eq!(loaded.ssh[0].id, id);
        std::fs::remove_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn duplicate_target_and_session_profiles_keep_distinct_opaque_ids() {
        let mut catalog = EndpointCatalog::default();
        let first = catalog
            .add_ssh("One", "build", "default")
            .expect("test precondition");
        let second = catalog
            .add_ssh("Two", "build", "default")
            .expect("test precondition");
        assert_ne!(first, second);
    }

    #[test]
    fn catalog_rejects_passwords_embedded_in_ssh_targets() {
        let mut catalog = EndpointCatalog::default();
        assert!(
            catalog
                .add_ssh("Build", "ssh://dev:secret@build.example", "default")
                .expect_err("test precondition")
                .contains("must not contain a password")
        );
        assert!(
            catalog
                .add_ssh("Build", "dev:secret@build.example", "default")
                .is_err()
        );
        assert!(
            catalog
                .add_ssh("Build", "ssh://dev@[::1]:2222", "default")
                .is_ok()
        );
    }

    #[test]
    fn interactive_bootstrap_matches_only_enabled_target_and_session() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        assert!(catalog.contains_enabled_target_session("build", "agents"));
        assert!(!catalog.contains_enabled_target_session("build", "default"));
        assert!(catalog.set_enabled(&id, false));
        assert!(!catalog.contains_enabled_target_session("build", "agents"));
    }

    #[test]
    fn rename_changes_only_the_machine_label() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Old", "build", "agents")
            .expect("test precondition");
        let original = catalog.ssh[0].clone();

        assert!(catalog.rename_ssh(&id, "New").expect("test precondition"));
        assert_eq!(catalog.ssh[0].label, "New");
        assert_eq!(catalog.ssh[0].id, original.id);
        assert_eq!(catalog.ssh[0].target, original.target);
        assert_eq!(catalog.ssh[0].session, original.session);
        assert!(catalog.rename_ssh(&id, "\n").is_err());
        assert_eq!(catalog.ssh[0].label, "New");
    }

    #[test]
    fn removal_and_disable_return_selection_to_local() {
        let mut catalog = EndpointCatalog::default();
        let first = catalog
            .add_ssh("One", "one", "default")
            .expect("test precondition");
        assert!(catalog.select_ssh(&first));
        assert!(catalog.set_enabled(&first, false));
        assert_eq!(catalog.selected_profile, None);

        assert!(catalog.set_enabled(&first, true));
        assert!(catalog.select_ssh(&first));
        assert!(catalog.remove_ssh(&first));
        assert_eq!(catalog.selected_profile, None);
    }

    #[test]
    fn catalog_rejects_unknown_fields_instead_of_retaining_possible_secrets() {
        let path = path("unknown-field");
        let _ = std::fs::remove_dir_all(path.parent().expect("test precondition"));
        std::fs::create_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
        std::fs::write(
            &path,
            r#"{
              "version": 1,
              "ssh": [{
                "id": "0123456789abcdef0123456789abcdef",
                "label": "Build",
                "target": "build",
                "session": "default",
                "enabled": true,
                "password": "must-not-be-accepted"
              }]
            }"#,
        )
        .expect("test precondition");
        assert!(
            EndpointCatalog::load_from_path(&path)
                .expect_err("test precondition")
                .contains("unknown field")
        );
        std::fs::remove_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn storing_selection_does_not_rewrite_profile_membership() {
        let catalog_path = path("separate-selection");
        let selection_path = catalog_path.with_file_name("selection.json");
        let _ = std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"));
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        catalog
            .store_to_path(&catalog_path)
            .expect("test precondition");
        let profiles_before = std::fs::read(&catalog_path).expect("test precondition");

        assert!(catalog.select_ssh(&id));
        catalog
            .store_selection_to_path(&selection_path)
            .expect("test precondition");

        assert_eq!(
            std::fs::read(&catalog_path).expect("test precondition"),
            profiles_before
        );
        assert_eq!(
            load_selection_from_path(&selection_path)
                .expect("test precondition")
                .expect("test precondition")
                .selected_profile,
            Some(id)
        );
        std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn malformed_selection_does_not_discard_saved_profiles() {
        let catalog_path = path("malformed-selection");
        let selection_path = catalog_path.with_file_name("selection.json");
        let _ = std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"));
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        catalog
            .store_to_path(&catalog_path)
            .expect("test precondition");
        std::fs::write(&selection_path, b"not json").expect("test precondition");

        let loaded = EndpointCatalog::load_from_paths(&catalog_path, &selection_path)
            .expect("test precondition");
        assert_eq!(loaded.ssh.len(), 1);
        assert_eq!(loaded.ssh[0].id, id);
        assert_eq!(loaded.selected_profile, None);
        std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn absent_selected_profile_falls_back_without_discarding_catalog() {
        let catalog_path = path("absent-selection");
        let selection_path = catalog_path.with_file_name("selection.json");
        let _ = std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"));
        let mut catalog = EndpointCatalog::default();
        let saved = catalog
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        catalog
            .store_to_path(&catalog_path)
            .expect("test precondition");
        let missing =
            ProfileId::parse("fedcba9876543210fedcba9876543210").expect("test precondition");
        store_private_json(
            &selection_path,
            &serde_json::to_vec(&EndpointSelection {
                version: SELECTION_VERSION,
                selected_profile: Some(missing),
            })
            .expect("test precondition"),
            "endpoint selection",
        )
        .expect("test precondition");

        let loaded = EndpointCatalog::load_from_paths(&catalog_path, &selection_path)
            .expect("test precondition");
        assert_eq!(loaded.ssh[0].id, saved);
        assert_eq!(loaded.selected_profile, None);
        std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn selection_left_in_an_older_profile_file_is_not_a_fallback() {
        let catalog_path = path("legacy-selection");
        let selection_path = catalog_path.with_file_name("selection.json");
        let _ = std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"));
        std::fs::create_dir_all(catalog_path.parent().expect("test precondition"))
            .expect("test precondition");
        std::fs::write(
            &catalog_path,
            r#"{
              "version": 1,
              "selected_profile": "0123456789abcdef0123456789abcdef",
              "ssh": [{
                "id": "0123456789abcdef0123456789abcdef",
                "label": "Build",
                "target": "build",
                "session": "default",
                "enabled": true
              }]
            }"#,
        )
        .expect("test precondition");

        let loaded = EndpointCatalog::load_from_paths(&catalog_path, &selection_path)
            .expect("test precondition");
        assert_eq!(loaded.ssh.len(), 1);
        assert_eq!(loaded.selected_profile, None);
        std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    fn machine(id: &str, target: &str, enabled: bool) -> SavedSshEndpoint {
        SavedSshEndpoint {
            id: ProfileId::parse(id).expect("test precondition"),
            label: "Build".into(),
            target: target.into(),
            session: "agents".into(),
            enabled,
        }
    }

    #[test]
    fn catalog_changes_retire_and_start_only_what_changed() {
        let a = "0123456789abcdef0123456789abcdef";
        let b = "fedcba9876543210fedcba9876543210";
        let c = "00112233445566778899aabbccddeeff";
        let previous = vec![
            machine(a, "one", true),
            machine(b, "two", true),
            machine(c, "three", false),
        ];

        // A label change touches no connection.
        let mut renamed = previous.clone();
        renamed[0].label = "Renamed".into();
        assert_eq!(
            EndpointCatalogChanges::between(&previous, &renamed),
            EndpointCatalogChanges::default()
        );

        // Removed, disabled, enabled, added and re-pointed machines.
        let d = "ffeeddccbbaa99887766554433221100";
        let next = vec![
            machine(a, "one-moved", true),
            machine(c, "three", true),
            machine(d, "four", true),
        ];
        let changes = EndpointCatalogChanges::between(&previous, &next);
        assert_eq!(
            changes.retired,
            vec![
                ProfileId::parse(a).expect("test precondition"),
                ProfileId::parse(b).expect("test precondition"),
            ]
        );
        assert_eq!(
            changes
                .started
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>(),
            vec![a, c, d]
        );

        let disabled = vec![machine(a, "one", false), machine(b, "two", true)];
        let changes = EndpointCatalogChanges::between(&previous, &disabled);
        assert_eq!(
            changes.retired,
            vec![ProfileId::parse(a).expect("test precondition")]
        );
        assert!(changes.started.is_empty());
    }

    #[test]
    fn replacing_profiles_drops_a_selection_that_is_no_longer_selectable() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        assert!(catalog.select_ssh(&id));
        assert_eq!(
            catalog.selected_endpoint(),
            crate::client::endpoint::ClientEndpointId::Ssh(id.clone())
        );

        let kept = catalog.ssh.clone();
        catalog.replace_profiles(kept.clone());
        assert_eq!(catalog.selected_profile, Some(id));

        let mut disabled = kept;
        disabled[0].enabled = false;
        catalog.replace_profiles(disabled);
        assert_eq!(catalog.selected_profile, None);
        assert_eq!(
            catalog.selected_endpoint(),
            crate::client::endpoint::ClientEndpointId::Local
        );
    }

    #[test]
    fn catalog_watch_reloads_only_after_the_file_changes() {
        let path = path("watch");
        let _ = std::fs::remove_dir_all(path.parent().expect("test precondition"));
        let start = Instant::now();
        let mut watch = EndpointCatalogWatch::for_path(path.clone(), start);

        // The first poll always loads; an absent file is an empty catalog.
        assert_eq!(watch.poll(start), Some(Ok(Vec::new())));
        let later = start + CATALOG_POLL_INTERVAL;
        assert_eq!(watch.poll(later), None);

        let mut catalog = EndpointCatalog::default();
        catalog
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        catalog.store_to_path(&path).expect("test precondition");
        // Not due yet: at most one stat per interval.
        assert_eq!(watch.poll(later), None);
        let later = later + CATALOG_POLL_INTERVAL;
        assert_eq!(watch.poll(later), Some(Ok(catalog.ssh.clone())));
        let later = later + CATALOG_POLL_INTERVAL;
        assert_eq!(watch.poll(later), None);

        // Every store replaces the file, so even an identical rewrite is seen (and applying
        // it is a no-op for the client).
        catalog.store_to_path(&path).expect("test precondition");
        let later = later + CATALOG_POLL_INTERVAL;
        assert_eq!(watch.poll(later), Some(Ok(catalog.ssh.clone())));

        // An invalid file is reported once, not on every poll.
        std::fs::write(&path, b"not json").expect("test precondition");
        let later = later + CATALOG_POLL_INTERVAL;
        assert!(matches!(watch.poll(later), Some(Err(_))));
        let later = later + CATALOG_POLL_INTERVAL;
        assert_eq!(watch.poll(later), None);

        std::fs::remove_dir_all(path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    #[test]
    fn invalid_or_missing_selected_profile_is_rejected() {
        let catalog = EndpointCatalog {
            selected_profile: Some(
                ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition"),
            ),
            ..EndpointCatalog::default()
        };
        assert!(catalog.validate().is_err());
    }
}
