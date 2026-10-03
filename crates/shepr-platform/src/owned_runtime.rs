//! Private, single-use runtime artifacts and conservative dead-owner cleanup.
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::process_identity::ProcessIdentity;

const OWNER_MARKER: &str = ".owner";

#[derive(Clone, Copy)]
pub(crate) enum DirectoryKind {
    Staging,
    SshConfig,
}

impl DirectoryKind {
    pub(crate) fn directory_name(self, token: u64) -> String {
        format!("{}{token:016x}", self.name_prefix())
    }

    fn has_valid_name(self, name: &str) -> bool {
        let Some(token) = name.strip_prefix(self.name_prefix()) else {
            return false;
        };
        token.len() == crate::limits::RUNTIME_TOKEN_HEX_BYTES
            && token.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    const fn name_prefix(self) -> &'static str {
        match self {
            Self::Staging => ".s",
            Self::SshConfig => "shepr-ssh-",
        }
    }

    const fn content_name(self) -> &'static str {
        match self {
            Self::Staging => "s",
            Self::SshConfig => "config",
        }
    }

    pub(crate) fn content_path(self, directory: &Path) -> PathBuf {
        directory.join(self.content_name())
    }

    fn content_is_owned(self, name: &OsStr, metadata: &fs::Metadata) -> bool {
        name == OsStr::new(self.content_name())
            && match self {
                Self::Staging => metadata.file_type().is_socket(),
                Self::SshConfig => metadata.is_file(),
            }
    }
}

#[derive(Clone, Copy)]
enum RuntimeKind {
    Directory(DirectoryKind),
    SocketSidecar,
}

#[derive(Debug)]
pub(crate) enum RuntimeCreateError {
    RandomSource(io::Error),
    Io(io::Error),
}

impl RuntimeCreateError {
    pub(crate) fn into_io(self) -> io::Error {
        match self {
            Self::RandomSource(error) | Self::Io(error) => error,
        }
    }
}

impl From<io::Error> for RuntimeCreateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Creation publishes an identity only after locking its marker. A missing or
/// incomplete marker never authorizes reclamation. Directory names carry only
/// collision tokens; every artifact uses the same identity and lock checks.
pub(crate) struct OwnedRuntimeEntry {
    path: PathBuf,
    kind: RuntimeKind,
    owner: Option<ProcessIdentity>,
    hold: File,
}

impl OwnedRuntimeEntry {
    pub(crate) fn create_directory(
        parent: &Path,
        kind: DirectoryKind,
    ) -> Result<Self, RuntimeCreateError> {
        Self::sweep_directory(parent, kind);
        for _ in 0..crate::limits::RANDOM_NAME_ATTEMPTS {
            let token = crate::unpredictable_token().map_err(RuntimeCreateError::RandomSource)?;
            let path = parent.join(kind.directory_name(token));
            match fs::DirBuilder::new()
                .mode(crate::limits::PRIVATE_DIRECTORY_MODE)
                .create(&path)
            {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
            match Self::create_marker(path.clone(), RuntimeKind::Directory(kind)) {
                Ok(entry) => return Ok(entry),
                Err(error) => {
                    remove_file(&path.join(OWNER_MARKER));
                    if let Err(cleanup) = fs::remove_dir(&path) {
                        tracing::warn!(%cleanup, path = %path.display(), "could not remove runtime artifact directory");
                    }
                    return Err(error.into());
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free runtime artifact name",
        )
        .into())
    }

    pub(crate) fn create_socket(path: &Path) -> io::Result<Self> {
        Self::create_marker(path.to_path_buf(), RuntimeKind::SocketSidecar)
    }

    fn create_marker(path: PathBuf, kind: RuntimeKind) -> io::Result<Self> {
        let marker = marker_path(&path, kind);
        let hold = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(crate::limits::RUNTIME_MARKER_MODE)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&marker)?;
        let result = (|| {
            crate::ipc::flock_exclusive(&hold, false)?;
            let owner = ProcessIdentity::current().inspect_err(|error| {
                tracing::debug!(%error, "could not mark runtime artifact; abandoned entry will be retained");
            }).ok();
            if let Some(owner) = owner {
                (&hold).write_all(owner.tag().as_bytes())?;
            }
            Ok(owner)
        })();
        match result {
            Ok(owner) => Ok(Self {
                path,
                kind,
                owner,
                hold,
            }),
            Err(error) => {
                remove_file(&marker);
                Err(error)
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Transfers the held marker to a socket lifetime guard.
    pub(crate) fn into_hold(self) -> File {
        self.hold
    }

    /// Transfers directory cleanup to an external lifetime or exit registry.
    pub(crate) fn into_path(self) -> PathBuf {
        self.path
    }

    pub(crate) fn release(self) {
        release(&self.path, self.kind, self.owner);
    }

    pub(crate) fn sweep_directory(parent: &Path, kind: DirectoryKind) {
        Self::sweep(parent, RuntimeKind::Directory(kind));
    }

    pub(crate) fn sweep_socket_sidecars(parent: &Path) {
        Self::sweep(parent, RuntimeKind::SocketSidecar);
    }

    fn sweep(parent: &Path, kind: RuntimeKind) {
        let uid = crate::effective_uid();
        if !private_directory(parent, uid) {
            return;
        }
        let Ok(entries) = fs::read_dir(parent) else {
            return;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let path = match kind {
                RuntimeKind::SocketSidecar => {
                    let Some(socket) = name.strip_suffix(".lock").filter(|name| !name.is_empty())
                    else {
                        continue;
                    };
                    parent.join(socket)
                }
                RuntimeKind::Directory(directory_kind) => {
                    if !directory_kind.has_valid_name(name) {
                        continue;
                    }
                    entry.path()
                }
            };
            if !matches!(kind, RuntimeKind::SocketSidecar) && !private_directory(&path, uid) {
                continue;
            }
            let marker = marker_path(&path, kind);
            let Ok(mut hold) = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&marker)
            else {
                continue;
            };
            let Ok(metadata) = hold.metadata() else {
                continue;
            };
            if !crate::private_file::PrivateFile::is_owned_regular_with_mode(
                &metadata,
                uid,
                crate::limits::RUNTIME_MARKER_MODE,
            ) || metadata.len() > crate::limits::RUNTIME_OWNER_MAX_BYTES
            {
                continue;
            }
            let mut contents = String::new();
            if (&mut hold)
                .take(crate::limits::RUNTIME_OWNER_MAX_BYTES + 1)
                .read_to_string(&mut contents)
                .is_err()
                || contents.len() as u64 > crate::limits::RUNTIME_OWNER_MAX_BYTES
            {
                continue;
            }
            let Some(owner) = ProcessIdentity::parse_tag(&contents) else {
                continue;
            };
            // Both proofs are required: ambiguous proc views and held locks
            // retain the entry. Keep the lock through validation and removal.
            if !owner.is_provably_gone() || crate::ipc::flock_exclusive(&hold, false).is_err() {
                continue;
            }
            let Ok(current) = fs::symlink_metadata(&marker) else {
                continue;
            };
            if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
                continue;
            }
            if contents_owned(&path, kind, uid) {
                release(&path, kind, Some(owner));
            }
        }
    }
}

fn marker_path(path: &Path, kind: RuntimeKind) -> PathBuf {
    match kind {
        RuntimeKind::SocketSidecar => crate::ipc::socket_startup_lock_path(path),
        RuntimeKind::Directory(_) => path.join(OWNER_MARKER),
    }
}

fn private_directory(path: &Path, uid: u32) -> bool {
    crate::private_file::PrivateDir::is_private(path, uid)
}

fn contents_owned(path: &Path, kind: RuntimeKind, uid: u32) -> bool {
    let directory_kind = match kind {
        RuntimeKind::SocketSidecar => {
            return match fs::symlink_metadata(path) {
                Ok(metadata) => metadata.uid() == uid && metadata.file_type().is_socket(),
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            };
        }
        RuntimeKind::Directory(directory_kind) => directory_kind,
    };
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    let mut marker_seen = false;
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if metadata.uid() != uid {
            return false;
        }
        let name = entry.file_name();
        if name == OWNER_MARKER && metadata.is_file() {
            marker_seen = true;
        } else {
            if !directory_kind.content_is_owned(&name, &metadata) {
                return false;
            }
        }
    }
    marker_seen
}

pub(crate) fn remove_file(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not remove runtime artifact file");
            false
        }
    }
}

fn release(path: &Path, kind: RuntimeKind, owner: Option<ProcessIdentity>) {
    let directory_kind = match kind {
        RuntimeKind::SocketSidecar => {
            if remove_file(path) {
                remove_file(&marker_path(path, kind));
            }
            return;
        }
        RuntimeKind::Directory(directory_kind) => directory_kind,
    };
    if !remove_file(&directory_kind.content_path(path)) {
        return;
    }
    let marker = marker_path(path, kind);
    if !remove_file(&marker) {
        return;
    }
    match fs::remove_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            // An unexpected entry or failed rmdir must not lose the proof used
            // by future sweeps. Never recurse into an unknown directory.
            if let Some(owner) = owner {
                let restore = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(crate::limits::RUNTIME_MARKER_MODE)
                    .open(&marker)
                    .and_then(|mut file| file.write_all(owner.tag().as_bytes()));
                if let Err(error) = restore {
                    tracing::warn!(%error, "could not restore runtime artifact marker");
                }
            }
            tracing::warn!(%error, path = %path.display(), "could not remove runtime artifact directory");
        }
    }
}

/// Release a config directory created by this process. Used by both normal
/// teardown and the process-exit registry; absence is already cleaned up.
pub fn release_remote_ssh_config_dir(path: &Path) {
    release(
        path,
        RuntimeKind::Directory(DirectoryKind::SshConfig),
        ProcessIdentity::current().ok(),
    );
}

/// Removes a single-use socket sidecar while its owner still holds the lock.
/// Shared socket sidecars must remain in place so binders lock one inode.
pub fn release_single_use_socket_lock(path: &Path) {
    remove_file(&marker_path(path, RuntimeKind::SocketSidecar));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn private_fixture(parent: &Path, name: &str, marker: Option<&str>) -> PathBuf {
        let path = parent.join(name);
        fs::DirBuilder::new()
            .mode(crate::limits::PRIVATE_DIRECTORY_MODE)
            .create(&path)
            .expect("create fixture directory");
        if let Some(marker) = marker {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(crate::limits::RUNTIME_MARKER_MODE)
                .open(path.join(OWNER_MARKER))
                .expect("create fixture marker");
            file.write_all(marker.as_bytes())
                .expect("write fixture marker");
        }
        path
    }

    #[test]
    fn directory_sweeps_share_dead_owner_content_and_lock_checks() {
        for kind in [DirectoryKind::Staging, DirectoryKind::SshConfig] {
            let scratch = shepr_test_support::ScratchDir::new("owned-runtime-sweep");
            fs::set_permissions(
                scratch.path(),
                fs::Permissions::from_mode(crate::limits::PRIVATE_DIRECTORY_MODE),
            )
            .expect("private parent");
            let live = ProcessIdentity::current().expect("current identity").tag();
            let (_, rest) = live.split_once('-').expect("identity fields");
            let dead = format!("{:08x}-{rest}", u32::MAX);
            let fixture = |token, marker| {
                private_fixture(scratch.path(), &kind.directory_name(token), marker)
            };
            let abandoned = fixture(1, Some(dead.as_str()));
            match kind {
                DirectoryKind::Staging => {
                    drop(
                        std::os::unix::net::UnixListener::bind(kind.content_path(&abandoned))
                            .expect("fixture socket"),
                    );
                }
                _ => fs::write(kind.content_path(&abandoned), "Host *\n").expect("fixture config"),
            }
            let unmarked = fixture(2, None);
            let live_path = fixture(3, Some(live.as_str()));
            let malformed = fixture(4, Some("invalid"));
            let held = fixture(5, Some(dead.as_str()));
            let _hold = crate::ipc::acquire_flock_lock(&held.join(OWNER_MARKER), false)
                .expect("hold dead-marked fixture");
            let unexpected = fixture(6, Some(dead.as_str()));
            fs::create_dir(unexpected.join("unexpected")).expect("unexpected directory");
            let oversized_owner = "x".repeat(
                usize::try_from(crate::limits::RUNTIME_OWNER_MAX_BYTES).expect("limit fits usize")
                    + 1,
            );
            let oversized = fixture(7, Some(oversized_owner.as_str()));
            let symlink = fixture(8, Some(dead.as_str()));
            std::os::unix::fs::symlink("absent", kind.content_path(&symlink))
                .expect("content symlink");
            let wrong_kind = fixture(9, Some(dead.as_str()));
            fs::create_dir(kind.content_path(&wrong_kind)).expect("wrong content kind");
            let wrong_mode = fixture(10, Some(dead.as_str()));
            fs::set_permissions(
                wrong_mode.join(OWNER_MARKER),
                fs::Permissions::from_mode(0o644),
            )
            .expect("non-private marker");
            let marker_symlink = fixture(11, None);
            std::os::unix::fs::symlink(held.join(OWNER_MARKER), marker_symlink.join(OWNER_MARKER))
                .expect("marker symlink");
            let fifo = fixture(12, None);
            use std::os::unix::ffi::OsStrExt as _;
            let fifo_path = std::ffi::CString::new(fifo.join(OWNER_MARKER).as_os_str().as_bytes())
                .expect("fixture path contains no nul");
            // SAFETY: fifo_path is a valid nul-terminated pathname.
            assert_eq!(
                unsafe { libc::mkfifo(fifo_path.as_ptr(), crate::limits::PRIVATE_FILE_MODE) },
                0
            );
            OwnedRuntimeEntry::sweep_directory(scratch.path(), kind);
            assert!(!abandoned.try_exists().expect("stat abandoned entry"));
            for retained in [
                unmarked,
                live_path,
                malformed,
                held,
                unexpected,
                oversized,
                symlink,
                wrong_kind,
                wrong_mode,
                marker_symlink,
                fifo,
            ] {
                assert!(
                    retained.try_exists().expect("stat retained entry"),
                    "{} retained",
                    retained.display()
                );
            }
        }
    }

    #[test]
    fn creation_marks_and_holds_until_release_or_transfer() {
        let scratch = shepr_test_support::ScratchDir::new("owned-runtime-lifetime");
        let entry = OwnedRuntimeEntry::create_directory(scratch.path(), DirectoryKind::SshConfig)
            .expect("create directory");
        let path = entry.path().to_path_buf();
        assert_eq!(
            fs::read_to_string(path.join(OWNER_MARKER)).expect("read marker"),
            ProcessIdentity::current().expect("current identity").tag()
        );
        assert!(crate::ipc::acquire_flock_lock(&path.join(OWNER_MARKER), false).is_err());
        fs::write(DirectoryKind::SshConfig.content_path(&path), "Host *\n").expect("write config");
        entry.release();
        assert!(!path.try_exists().expect("stat released directory"));

        let entry = OwnedRuntimeEntry::create_directory(scratch.path(), DirectoryKind::SshConfig)
            .expect("create transferred directory");
        let path = entry.into_path();
        release_remote_ssh_config_dir(&path);
        release_remote_ssh_config_dir(&path);
        assert!(!path.try_exists().expect("stat transferred directory"));
    }

    #[test]
    fn failed_directory_release_restores_marker_without_recursing() {
        let scratch = shepr_test_support::ScratchDir::new("owned-runtime-release");
        let entry = OwnedRuntimeEntry::create_directory(scratch.path(), DirectoryKind::Staging)
            .expect("create staging directory");
        let path = entry.path().to_path_buf();
        fs::create_dir(path.join("unexpected")).expect("unexpected directory");
        entry.release();
        assert!(
            path.join("unexpected")
                .try_exists()
                .expect("stat unexpected entry")
        );
        assert_eq!(
            fs::read_to_string(path.join(OWNER_MARKER)).expect("restored marker"),
            ProcessIdentity::current().expect("current identity").tag()
        );
    }
}
