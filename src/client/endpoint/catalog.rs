use std::collections::HashSet;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::ProfileId;
use crate::remote::{IntoSshTarget, SshTarget};

const CATALOG_VERSION: u32 = 1;
const SELECTION_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 64 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_LABEL_BYTES: usize = 128;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedSshEndpoint {
    pub(crate) id: ProfileId,
    pub(crate) label: String,
    pub(crate) target: SshTarget,
    pub(crate) session: String,
}

impl SavedSshEndpoint {
    pub(crate) fn new(
        label: impl Into<String>,
        target: impl IntoSshTarget,
        session: impl Into<String>,
    ) -> Result<Self, String> {
        let profile = Self {
            id: ProfileId::generate(),
            label: label.into(),
            target: target.into_ssh_target()?,
            session: session.into(),
        };
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<(), String> {
        let label = self.label.trim();
        if label.is_empty() {
            return Err("SSH endpoint label cannot be empty".into());
        }
        if label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control) {
            return Err(format!(
                "SSH endpoint label must be at most {MAX_LABEL_BYTES} bytes and contain no control characters"
            ));
        }
        crate::session::validate_name(&self.session)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointCatalog {
    version: u32,
    #[serde(default)]
    pub(crate) ssh: Vec<SavedSshEndpoint>,
    #[serde(skip)]
    catalog_path: PathBuf,
    #[serde(skip)]
    selection_path: PathBuf,
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
            ssh: Vec::new(),
            catalog_path: PathBuf::new(),
            selection_path: PathBuf::new(),
        }
    }
}

impl EndpointCatalog {
    pub(crate) fn load(paths: &crate::config::AppPaths) -> Result<Self, String> {
        Self::load_from_paths(&catalog_path(paths), &selection_path(paths))
    }

    pub(crate) fn load_profiles(
        paths: &crate::config::AppPaths,
    ) -> Result<Vec<SavedSshEndpoint>, String> {
        // Profiles only: each running client holds its selection in memory, and the
        // selection file only seeds the next launch (the last client to commit a
        // handoff wins).
        Self::load_from_path(&catalog_path(paths)).map(|catalog| catalog.ssh)
    }

    fn load_from_paths(catalog_path: &Path, selection_path: &Path) -> Result<Self, String> {
        let mut catalog = Self::load_from_path(catalog_path)?;
        catalog.catalog_path = catalog_path.to_path_buf();
        catalog.selection_path = selection_path.to_path_buf();
        Ok(catalog)
    }

    /// The selection saved by the last client to commit a handoff, when it still names
    /// a saved machine. `None` means Local. The client's selection tracker owns the
    /// selection while it runs; this only seeds it.
    pub(crate) fn load_selection(&self) -> Option<ProfileId> {
        let path = &self.selection_path;
        match load_selection_from_path(path) {
            Ok(Some(selection)) => {
                let selected = selection.selected_profile?;
                if self.is_selectable(&selected) {
                    Some(selected)
                } else {
                    tracing::warn!(
                        path = %path.display(),
                        "saved endpoint selection is absent; using Local"
                    );
                    None
                }
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "saved endpoint selection is unavailable; using Local"
                );
                None
            }
        }
    }

    pub(crate) fn store_profiles(&self) -> Result<(), String> {
        self.store_to_path(&self.catalog_path)
    }

    /// Saves `selected` (`None` is Local) as the next launch's selection.
    pub(crate) fn store_selection(&self, selected: Option<&ProfileId>) -> Result<(), String> {
        self.store_selection_to_path(&self.selection_path, selected)
    }

    fn store_selection_to_path(
        &self,
        path: &Path,
        selected: Option<&ProfileId>,
    ) -> Result<(), String> {
        self.validate()?;
        if selected.is_some_and(|selected| !self.is_selectable(selected)) {
            return Err("selected SSH endpoint is absent from the catalog".into());
        }
        let content = serde_json::to_vec_pretty(&EndpointSelection {
            version: SELECTION_VERSION,
            selected_profile: selected.cloned(),
        })
        .map_err(|error| format!("failed to encode endpoint selection: {error}"))?;
        store_private_json(path, &content, "endpoint selection")
    }

    pub(crate) fn add_ssh(
        &mut self,
        label: impl Into<String>,
        target: impl IntoSshTarget,
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

    pub(crate) fn remove_ssh(&mut self, id: &ProfileId) -> bool {
        let previous_len = self.ssh.len();
        self.ssh.retain(|profile| &profile.id != id);
        self.ssh.len() != previous_len
    }

    pub(crate) fn has_ssh(&self) -> bool {
        !self.ssh.is_empty()
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
        let catalog: Self = serde_json::from_str(&content)
            .map_err(|error| format!("stored endpoint catalog is invalid: {error}"))?;
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
/// `shepr machine add/remove` rewrite it while clients run, and those clients pick the
/// change up here instead of at their next launch.
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
    pub(crate) fn new(paths: &crate::config::AppPaths, now: Instant) -> Self {
        Self::for_path(catalog_path(paths), now)
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
/// supervised and which start. A machine whose target or session changed is both retired
/// and started, because its connector, bridge and remote server belong to the old target.
/// A label change is neither.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EndpointCatalogChanges {
    pub(crate) retired: Vec<ProfileId>,
    pub(crate) started: Vec<SavedSshEndpoint>,
}

impl EndpointCatalogChanges {
    pub(crate) fn between(previous: &[SavedSshEndpoint], next: &[SavedSshEndpoint]) -> Self {
        fn same_machine(a: &SavedSshEndpoint, b: &SavedSshEndpoint) -> bool {
            a.id == b.id && a.target == b.target && a.session == b.session
        }
        let retired = previous
            .iter()
            .filter(|old| !next.iter().any(|new| same_machine(old, new)))
            .map(|old| old.id.clone())
            .collect();
        let started = next
            .iter()
            .filter(|new| !previous.iter().any(|old| same_machine(old, new)))
            .cloned()
            .collect();
        Self { retired, started }
    }
}

impl EndpointCatalog {
    /// Replaces the saved profiles with a newer copy of the catalog file. The client's
    /// selection tracker drops a selection this removed (`catalog_changed`).
    pub(crate) fn replace_profiles(&mut self, profiles: Vec<SavedSshEndpoint>) {
        self.ssh = profiles;
    }

    /// Whether `id` names a saved machine that may be selected.
    pub(crate) fn is_selectable(&self, id: &ProfileId) -> bool {
        self.ssh.iter().any(|profile| &profile.id == id)
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

pub(crate) fn catalog_path(paths: &crate::config::AppPaths) -> PathBuf {
    paths.state_dir().join("client").join("endpoints.json")
}

fn selection_path(paths: &crate::config::AppPaths) -> PathBuf {
    paths
        .state_dir()
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
        catalog.store_to_path(&path).expect("test precondition");

        let encoded = std::fs::read_to_string(&path).expect("test precondition");
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("private_key"));
        assert!(!encoded.contains("control_socket"));
        // The selection belongs to the selection file, not the shared profile list.
        assert!(!encoded.contains("selected_profile"));
        let loaded = EndpointCatalog::load_from_path(&path).expect("test precondition");
        assert_eq!(loaded.ssh, catalog.ssh);
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
    fn catalog_rejects_the_removed_enabled_field() {
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
                "enabled": true
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

        catalog
            .store_selection_to_path(&selection_path, Some(&id))
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
        assert_eq!(loaded.load_selection(), None);
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
        assert_eq!(loaded.load_selection(), None);
        std::fs::remove_dir_all(catalog_path.parent().expect("test precondition"))
            .expect("test precondition");
    }

    fn machine(id: &str, target: &str) -> SavedSshEndpoint {
        SavedSshEndpoint {
            id: ProfileId::parse(id).expect("test precondition"),
            label: "Build".into(),
            target: SshTarget::parse(target).expect("test precondition"),
            session: "agents".into(),
        }
    }

    #[test]
    fn catalog_changes_retire_and_start_only_what_changed() {
        let a = "0123456789abcdef0123456789abcdef";
        let b = "fedcba9876543210fedcba9876543210";
        let c = "00112233445566778899aabbccddeeff";
        let previous = vec![machine(a, "one"), machine(b, "two"), machine(c, "three")];

        // A label change touches no connection.
        let mut renamed = previous.clone();
        renamed[0].label = "Renamed".into();
        assert_eq!(
            EndpointCatalogChanges::between(&previous, &renamed),
            EndpointCatalogChanges::default()
        );

        // Removed, added and re-pointed machines.
        let d = "ffeeddccbbaa99887766554433221100";
        let next = vec![
            machine(a, "one-moved"),
            machine(c, "three"),
            machine(d, "four"),
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
            vec![a, d]
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
    fn storing_a_selection_absent_from_the_catalog_is_rejected() {
        let selection_path = path("missing-selection").with_file_name("selection.json");
        let catalog = EndpointCatalog::default();
        let missing =
            ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition");
        assert!(
            catalog
                .store_selection_to_path(&selection_path, Some(&missing))
                .is_err()
        );
        assert!(!selection_path.exists());
    }
}
