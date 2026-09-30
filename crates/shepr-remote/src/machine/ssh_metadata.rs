use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{RemoteExecutable, SshTarget};
use crate::limits::MAX_METADATA_BYTES;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize, Deserialize)]
struct StoredMetadata {
    target: String,
    executable: String,
}

/// The remembered remote executable for one SSH target. Machines that share a
/// target share the hint, since the executable belongs to the host. The cache
/// is kept per build profile inside the shared client state directory, so a dev
/// and a release client never overwrite each other's hint for a target.
pub struct SshMetadataCache {
    path: PathBuf,
    target: String,
}

impl SshMetadataCache {
    pub fn new(paths: &shepr_config::AppPaths, target: &SshTarget) -> Self {
        Self::for_profile(paths, target, shepr_config::BuildProfile::current())
    }

    fn for_profile(
        paths: &shepr_config::AppPaths,
        target: &SshTarget,
        profile: shepr_config::BuildProfile,
    ) -> Self {
        Self {
            path: paths
                .client_state_dir()
                .join(format!("ssh-metadata-{}", profile.marker()))
                .join(format!("{:016x}.json", target_file_key(target.as_str()))),
            target: target.as_str().to_owned(),
        }
    }

    pub fn load(&self) -> Option<RemoteExecutable> {
        load_metadata(&self.path, &self.target)
    }

    /// The cache file, for callers naming it when a store or invalidate fails.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remembers where the remote shepr lives. Successful stores log the cache path
    /// and executable path at debug. A failure before rename leaves the old cache
    /// untouched. Since this is a disposable hint, a directory-sync failure after
    /// rename keeps the new entry available and does not fail the store; a crash may
    /// still lose that entry, in which case discovery can rebuild it.
    pub fn store(&self, executable: &RemoteExecutable) -> io::Result<()> {
        let stored = StoredMetadata {
            target: self.target.clone(),
            executable: executable.as_str().to_owned(),
        };
        let bytes = serde_json::to_vec(&stored).map_err(io::Error::other)?;
        store_private_json(&self.path, &bytes)?;
        tracing::debug!(
            path = %self.path.display(),
            target = %self.target,
            executable = %executable.as_str(),
            "cached SSH machine metadata"
        );
        Ok(())
    }

    /// Forgets the remembered executable. An absent cache is already forgotten. A
    /// failure leaves a stale hint that later connections try first.
    pub fn invalidate(&self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// A stable 64-bit FNV-1a hash of the target, naming its cache file. Targets can
/// hold characters and lengths a file name cannot, and a collision only costs
/// a cache miss because the stored target is compared on load.
fn target_file_key(target: &str) -> u64 {
    // limits-exempt: the FNV-1a 64-bit offset basis and prime are fixed by the algorithm.
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    target.bytes().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// Writes `content` to `path` through a private temporary file and a rename,
/// refusing to replace a symlink or a non-file.
fn store_private_json(path: &Path, content: &[u8]) -> io::Result<()> {
    store_private_json_with_directory_sync(path, content, shepr_platform::sync_directory)
}

fn store_private_json_with_directory_sync(
    path: &Path,
    content: &[u8],
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid SSH metadata path: {}", path.display()),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid SSH metadata path: {}", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to replace SSH metadata through a non-file path",
        ));
    }

    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let token = shepr_platform::unpredictable_token()?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(file_name);
    // Keep the random token at its full fixed width hexadecimal representation.
    temp_name.push(format!("-{token:016x}-{sequence}.tmp"));
    let temp_path = parent.join(temp_name);
    let mut temp = shepr_platform::create_private_file(&temp_path)?;
    let mut cleanup = AbandonedTempFile(Some(temp_path.clone()));
    temp.write_all(content).and_then(|()| temp.sync_all())?;
    drop(temp);
    std::fs::rename(&temp_path, path)?;
    cleanup.0 = None;
    if let Err(error) = sync_directory(parent) {
        // The cache is only a discovery hint. The atomic rename has published
        // the entry for live readers, so uncertain crash durability must not be
        // reported as if the cache were still absent.
        tracing::debug!(
            %error,
            directory = %parent.display(),
            "SSH metadata cache was published but its directory sync failed"
        );
    }
    Ok(())
}

/// Removes a failed store's temporary file. A failed removal is logged with its
/// path and does not replace the store error the caller returns.
struct AbandonedTempFile(Option<PathBuf>);

impl Drop for AbandonedTempFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0
            && let Err(error) = std::fs::remove_file(path)
        {
            tracing::warn!(
                %error,
                path = %path.display(),
                "could not remove temporary SSH metadata file after a failed store"
            );
        }
    }
}

fn load_metadata(path: &Path, target: &str) -> Option<RemoteExecutable> {
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
    if stored.target != target {
        return None;
    }
    RemoteExecutable::parse(stored.executable).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is keyed by SSH target alone: one target always finds one file
    /// however many machines name it, and distinct targets get distinct files.
    #[test]
    fn metadata_path_is_keyed_by_ssh_target() {
        let scratch = shepr_test_support::ScratchDir::new("ssh-metadata-key");
        let paths = shepr_config::AppPaths::rooted_at(&scratch, None, None);
        let target = |value: &str| SshTarget::parse(value).expect("test precondition");
        let build = SshMetadataCache::new(&paths, &target("dev@build.example"));
        let again = SshMetadataCache::new(&paths, &target("dev@build.example"));
        let other = SshMetadataCache::new(&paths, &target("dev@other.example"));
        assert_eq!(build.path(), again.path());
        assert_ne!(build.path(), other.path());
        assert!(build.path().starts_with(paths.client_state_dir()));
    }

    #[test]
    fn metadata_path_differs_per_build_profile() {
        let scratch = shepr_test_support::ScratchDir::new("ssh-metadata-profile");
        let paths = shepr_config::AppPaths::rooted_at(&scratch, None, None);
        let target = SshTarget::parse("dev@build.example").expect("test precondition");
        let release =
            SshMetadataCache::for_profile(&paths, &target, shepr_config::BuildProfile::Release);
        let dev = SshMetadataCache::for_profile(&paths, &target, shepr_config::BuildProfile::Dev);
        assert_ne!(release.path(), dev.path());
        assert!(dev.path().starts_with(paths.client_state_dir()));
    }

    #[test]
    fn metadata_is_disposable_fingerprinted_and_independent_per_target() {
        // Not created yet: storing creates the cache directory.
        let root = shepr_test_support::ScratchDir::new("ssh-metadata").join("cache");
        let first = SshMetadataCache {
            path: root.join("first.json"),
            target: "mac".into(),
        };
        let second = SshMetadataCache {
            path: root.join("second.json"),
            target: "mac".into(),
        };
        let metadata = RemoteExecutable::parse("/some-path/shepr").expect("test precondition");
        assert!(first.load().is_none());
        first.store(&metadata).expect("first store");
        second.store(&metadata).expect("second store");
        assert_eq!(first.load(), Some(metadata.clone()));
        assert!(load_metadata(&first.path, "different-host").is_none());
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
        assert_eq!(stored["executable"], "/some-path/shepr");
        assert!(stored.get("version").is_none());
        assert!(stored.get("os").is_none());
        stored["future_field"] = true.into();
        std::fs::write(
            &first.path,
            serde_json::to_vec(&stored).expect("test precondition"),
        )
        .expect("test precondition");
        assert_eq!(first.load(), Some(metadata.clone()));
        for bytes in [
            b"broken".to_vec(),
            vec![b' '; usize::try_from(MAX_METADATA_BYTES).unwrap_or(usize::MAX) + 1],
        ] {
            std::fs::write(&first.path, bytes).expect("test precondition");
            assert!(first.load().is_none());
        }
        first.invalidate().expect("invalidate");
        assert!(first.load().is_none());
        // Invalidating an absent cache is not a failure.
        first.invalidate().expect("invalidate absent");
        assert_eq!(second.load(), Some(metadata));
        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn metadata_does_not_follow_symlinks() {
        let root = shepr_test_support::ScratchDir::new("ssh-metadata-link");
        let cache = SshMetadataCache {
            path: root.join("cache.json"),
            target: "mac".into(),
        };
        let other = root.join("other");
        std::fs::write(&other, "untouched").expect("test precondition");
        std::os::unix::fs::symlink(&other, &cache.path).expect("test precondition");
        assert!(cache.load().is_none());
        assert!(
            cache
                .store(&RemoteExecutable::parse("/bin/shepr").expect("test precondition"))
                .is_err(),
            "storing through a symlink must be refused"
        );
        assert_eq!(
            std::fs::read_to_string(&other).expect("test precondition"),
            "untouched"
        );
        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn ssh_metadata_directory_sync_failure_keeps_the_published_hint() {
        let root = shepr_test_support::ScratchDir::new("ssh-metadata-unsynced");
        let path = root.join("cache.json");
        let executable = RemoteExecutable::parse("/some-path/shepr").expect("test precondition");
        let content = serde_json::to_vec(&StoredMetadata {
            target: "dev@build.example".into(),
            executable: executable.as_str().to_owned(),
        })
        .expect("serialize metadata");

        store_private_json_with_directory_sync(&path, &content, |_| {
            Err(io::Error::other("directory sync failed"))
        })
        .expect("the published disposable cache is still a successful store");

        assert_eq!(load_metadata(&path, "dev@build.example"), Some(executable));
    }
}
