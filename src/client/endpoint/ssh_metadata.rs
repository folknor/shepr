use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ProfileId;

const MAX_METADATA_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SshMachineMetadata {
    pub(crate) os: String,
    pub(crate) executable: String,
}

impl SshMachineMetadata {
    pub(crate) fn is_valid(&self) -> bool {
        let path = &self.executable;
        if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
            return false;
        }
        match self.os.as_str() {
            "linux" => path.starts_with('/') && !path.ends_with("/mise/shims/shepr"),
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredMetadata {
    version: u32,
    target: String,
    session: String,
    metadata: SshMachineMetadata,
}

pub(crate) struct SshMetadataCache {
    path: PathBuf,
    target: String,
    session: String,
}

impl SshMetadataCache {
    pub(crate) fn new(
        paths: &crate::config::AppPaths,
        profile_id: &ProfileId,
        target: &str,
        session: &str,
    ) -> Self {
        Self {
            path: paths
                .state_dir()
                .join("client/ssh-metadata")
                .join(format!("{profile_id}.json")),
            target: target.to_owned(),
            session: session.to_owned(),
        }
    }

    pub(crate) fn load(&self) -> Option<SshMachineMetadata> {
        load_metadata(&self.path, &self.target, &self.session)
    }

    pub(crate) fn store(&self, metadata: &SshMachineMetadata) {
        if !metadata.is_valid() {
            return;
        }
        let stored = StoredMetadata {
            version: 1,
            target: self.target.clone(),
            session: self.session.clone(),
            metadata: metadata.clone(),
        };
        let result = serde_json::to_vec(&stored)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                super::catalog::store_private_json(&self.path, &bytes, "SSH metadata")
            });
        if let Err(error) = result {
            tracing::debug!(%error, "could not cache SSH machine metadata");
        }
    }

    pub(crate) fn invalidate(&self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(%error, "could not invalidate SSH machine metadata");
        }
    }
}

fn load_metadata(path: &Path, target: &str, session: &str) -> Option<SshMachineMetadata> {
    let file_type = std::fs::symlink_metadata(path).ok()?.file_type();
    if !file_type.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return None;
    }
    let stored: StoredMetadata = serde_json::from_slice(&bytes).ok()?;
    (stored.version == 1
        && stored.target == target
        && stored.session == session
        && stored.metadata.is_valid())
    .then_some(stored.metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_accepts_only_supported_platforms_and_absolute_paths() {
        for (os, path, valid) in [
            ("linux", "/home/a b/shepr", true),
            ("linux", "$HOME/.local/bin/shepr", false),
            ("linux", "/home/user/.local/share/mise/shims/shepr", false),
            ("linux", "/bin/shepr\nmalformed", false),
            ("macos", "/opt/homebrew/bin/shepr", false),
            ("unknown", "/bin/shepr", false),
        ] {
            assert_eq!(
                SshMachineMetadata {
                    os: os.into(),
                    executable: path.into()
                }
                .is_valid(),
                valid,
                "{os}: {path}"
            );
        }
    }

    #[test]
    fn metadata_is_disposable_fingerprinted_and_independent_per_profile() {
        // Not created yet: storing creates the cache directory.
        let root = crate::test_support::ScratchDir::new("ssh-metadata")
            .keep_until_exit()
            .join("cache");
        let first = SshMetadataCache {
            path: root.join("first.json"),
            target: "mac".into(),
            session: "fleet".into(),
        };
        let second = SshMetadataCache {
            path: root.join("second.json"),
            target: "mac".into(),
            session: "fleet".into(),
        };
        let metadata = SshMachineMetadata {
            os: "linux".into(),
            executable: "/some path/shepr".into(),
        };
        assert!(first.load().is_none());
        first.store(&metadata);
        second.store(&metadata);
        assert_eq!(first.load(), Some(metadata.clone()));
        assert!(load_metadata(&first.path, "different-host", "fleet").is_none());
        assert!(load_metadata(&first.path, "mac", "different-session").is_none());
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&first.path)
                    .expect("test precondition")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let mut stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&first.path).expect("test precondition"))
                .expect("test precondition");
        stored["future_field"] = true.into();
        std::fs::write(
            &first.path,
            serde_json::to_vec(&stored).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(first.load(), Some(metadata.clone()));
        stored["version"] = 2.into();
        std::fs::write(
            &first.path,
            serde_json::to_vec(&stored).expect("test precondition"),
        )
        .expect("test precondition");
        assert!(first.load().is_none());
        for bytes in [
            b"broken".to_vec(),
            vec![b' '; usize::try_from(MAX_METADATA_BYTES).unwrap_or(usize::MAX) + 1],
        ] {
            std::fs::write(&first.path, bytes).expect("test precondition");
            assert!(first.load().is_none());
        }
        first.invalidate();
        assert_eq!(second.load(), Some(metadata));
        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn metadata_does_not_follow_symlinks() {
        let root = crate::test_support::ScratchDir::new("ssh-metadata-link").keep_until_exit();
        let cache = SshMetadataCache {
            path: root.join("cache.json"),
            target: "mac".into(),
            session: "fleet".into(),
        };
        let other = root.join("other");
        std::fs::write(&other, "untouched").expect("test precondition");
        std::os::unix::fs::symlink(&other, &cache.path).expect("test precondition");
        assert!(cache.load().is_none());
        cache.store(&SshMachineMetadata {
            os: "linux".into(),
            executable: "/bin/shepr".into(),
        });
        assert_eq!(
            std::fs::read_to_string(&other).expect("test precondition"),
            "untouched"
        );
        std::fs::remove_dir_all(root).expect("test precondition");
    }
}
