//! Private, single-use runtime artifacts and conservative dead-owner cleanup.
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::ipc::LockWait;
use crate::process_identity::ProcessIdentity;

const OWNER_MARKER: &str = ".owner";

/// What an owned directory holds besides its owner marker.
#[derive(Clone, Copy)]
enum DirectoryContent {
    Socket,
    RegularFile,
}

/// The layout of one family of owned runtime directories: the name prefix a
/// directory carries before its collision token, and the one entry it holds
/// besides the owner marker. Creation, release and dead-owner sweeping all
/// read this one description, so they cannot disagree about it.
#[derive(Clone, Copy)]
pub struct DirectoryKind {
    name_prefix: &'static str,
    content_name: &'static str,
    content: DirectoryContent,
}

impl DirectoryKind {
    /// Private staging directories that hold one socket named `s`.
    pub(crate) const STAGING: Self = Self {
        name_prefix: ".s",
        content_name: "s",
        content: DirectoryContent::Socket,
    };

    /// A family of directories named `name_prefix` plus a 16 digit hex token,
    /// each holding one regular file named `content_name`.
    pub const fn regular_file(name_prefix: &'static str, content_name: &'static str) -> Self {
        Self {
            name_prefix,
            content_name,
            content: DirectoryContent::RegularFile,
        }
    }

    pub(crate) fn directory_name(self, token: u64) -> String {
        format!("{}{token:016x}", self.name_prefix)
    }

    fn has_valid_name(self, name: &str) -> bool {
        let Some(token) = name.strip_prefix(self.name_prefix) else {
            return false;
        };
        // Exactly what `directory_name` writes: lowercase hex of the token
        // length, so a sweep never touches a name shepr could not have made.
        token.len() == crate::limits::RUNTIME_TOKEN_HEX_BYTES
            && token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    /// The path of the content entry inside a directory of this kind.
    pub fn content_path(self, directory: &Path) -> PathBuf {
        directory.join(self.content_name)
    }

    fn content_is_owned(self, name: &OsStr, metadata: &fs::Metadata) -> bool {
        name == OsStr::new(self.content_name)
            && match self.content {
                DirectoryContent::Socket => metadata.file_type().is_socket(),
                DirectoryContent::RegularFile => metadata.is_file(),
            }
    }
}

#[derive(Clone, Copy)]
enum RuntimeKind {
    Directory(DirectoryKind),
    SocketSidecar,
}

/// Why an owned directory could not be created.
#[derive(Debug)]
pub enum RuntimeCreateError {
    RandomSource(io::Error),
    Io(io::Error),
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
            crate::ipc::flock_exclusive(&hold, LockWait::FailIfHeld)?;
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
            if !owner.is_provably_gone()
                || crate::ipc::flock_exclusive(&hold, LockWait::FailIfHeld).is_err()
            {
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

/// Creates a directory of `kind` under the private directory `parent`, marked
/// with this process as its owner, and hands its cleanup to the caller. Each
/// creation first sweeps leftovers of hard-killed owners of the same kind: only
/// current-uid directories whose locked marker records a process `/proc` proves
/// has exited. Unmarked directories (created when the owner identity could not
/// be read) are retained because their owner cannot be established. Callers
/// remove the directory with [`release_owned_directory`], on normal teardown
/// and from any process-exit registry; absence is already cleaned up.
pub fn create_owned_directory(
    parent: &Path,
    kind: DirectoryKind,
) -> Result<PathBuf, RuntimeCreateError> {
    OwnedRuntimeEntry::create_directory(parent, kind).map(OwnedRuntimeEntry::into_path)
}

/// Releases a directory [`create_owned_directory`] created in this process.
pub fn release_owned_directory(path: &Path, kind: DirectoryKind) {
    release(
        path,
        RuntimeKind::Directory(kind),
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

    /// A regular-file family, standing in for a caller's own.
    const TEST_FILES: DirectoryKind = DirectoryKind::regular_file("shepr-test-", "config");

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

    /// A directory's on-disk name: the literal prefix and a 16 digit hex token.
    /// Written out here rather than through `DirectoryKind::directory_name`, so
    /// the fixtures pin the names leftovers of earlier processes carry.
    fn literal_directory_name(prefix: &str, token: u64) -> String {
        format!("{prefix}{token:016x}")
    }

    /// Asserts `path` is named `prefix` plus a 16 digit lowercase hex token.
    fn assert_literal_name(path: &Path, prefix: &str) {
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .expect("a UTF-8 file name");
        let token = name
            .strip_prefix(prefix)
            .unwrap_or_else(|| panic!("{name:?} starts with {prefix:?}"));
        assert_eq!(token.len(), 16, "{name:?}");
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "{name:?}"
        );
    }

    #[test]
    fn directory_kinds_name_their_entries_with_the_literal_on_disk_names() {
        let directory = Path::new("/runtime/dir");
        assert_eq!(
            DirectoryKind::STAGING.directory_name(0x0123_4567_89ab_cdef),
            ".s0123456789abcdef"
        );
        assert_eq!(
            DirectoryKind::STAGING.directory_name(1),
            ".s0000000000000001"
        );
        assert_eq!(
            DirectoryKind::STAGING.content_path(directory),
            Path::new("/runtime/dir/s")
        );
        assert_eq!(
            TEST_FILES.directory_name(0xff),
            "shepr-test-00000000000000ff"
        );
        assert_eq!(
            TEST_FILES.content_path(directory),
            Path::new("/runtime/dir/config")
        );
        assert_eq!(OWNER_MARKER, ".owner");
    }

    #[test]
    fn valid_names_are_exactly_what_directory_name_writes() {
        let kind = DirectoryKind::STAGING;
        assert!(kind.has_valid_name(&kind.directory_name(0x0123_4567_89ab_cdef)));
        assert!(kind.has_valid_name(".s0123456789abcdef"));
        for rejected in [
            ".s0123456789ABCDEF",
            ".s0123456789abcdeF",
            ".s0123456789abcde",
            ".s0123456789abcdef0",
            ".s0123456789abcdeg",
            ".t0123456789abcdef",
            "s0123456789abcdef",
        ] {
            assert!(!kind.has_valid_name(rejected), "{rejected:?}");
        }
    }

    #[test]
    fn directory_sweeps_share_dead_owner_content_and_lock_checks() {
        // Each kind with its literal on-disk prefix and content name, so a
        // change to either stops the sweep from finding these leftovers.
        for (kind, prefix, content, socket_content) in [
            (DirectoryKind::STAGING, ".s", "s", true),
            (TEST_FILES, "shepr-test-", "config", false),
        ] {
            let scratch = shepr_test_support::ScratchDir::new("owned-runtime-sweep");
            fs::set_permissions(
                scratch.path(),
                fs::Permissions::from_mode(crate::limits::PRIVATE_DIRECTORY_MODE),
            )
            .expect("private parent");
            let live = ProcessIdentity::current().expect("current identity").tag();
            let (_, rest) = live.split_once('-').expect("identity fields");
            let dead = format!("{:08x}-{rest}", i32::MAX);
            let fixture = |token, marker| {
                private_fixture(
                    scratch.path(),
                    &literal_directory_name(prefix, token),
                    marker,
                )
            };
            let content_path = |directory: &Path| directory.join(content);
            let abandoned = fixture(1, Some(dead.as_str()));
            if socket_content {
                drop(
                    std::os::unix::net::UnixListener::bind(content_path(&abandoned))
                        .expect("fixture socket"),
                );
            } else {
                fs::write(content_path(&abandoned), "Host *\n").expect("fixture config");
            }
            // Dead-marked with owned content, but its token is uppercase hex,
            // which `directory_name` never writes: not shepr's to sweep.
            let uppercase = private_fixture(
                scratch.path(),
                &format!("{prefix}{:016X}", 0xabc_u64),
                Some(dead.as_str()),
            );
            if socket_content {
                drop(
                    std::os::unix::net::UnixListener::bind(content_path(&uppercase))
                        .expect("fixture socket"),
                );
            } else {
                fs::write(content_path(&uppercase), "Host *\n").expect("fixture config");
            }
            let unmarked = fixture(2, None);
            let live_path = fixture(3, Some(live.as_str()));
            let malformed = fixture(4, Some("invalid"));
            let held = fixture(5, Some(dead.as_str()));
            let _hold =
                crate::ipc::acquire_flock_lock(&held.join(OWNER_MARKER), LockWait::FailIfHeld)
                    .expect("hold dead-marked fixture");
            let unexpected = fixture(6, Some(dead.as_str()));
            fs::create_dir(unexpected.join("unexpected")).expect("unexpected directory");
            let oversized_owner = "x".repeat(
                usize::try_from(crate::limits::RUNTIME_OWNER_MAX_BYTES).expect("limit fits usize")
                    + 1,
            );
            let oversized = fixture(7, Some(oversized_owner.as_str()));
            let symlink = fixture(8, Some(dead.as_str()));
            std::os::unix::fs::symlink("absent", content_path(&symlink)).expect("content symlink");
            let wrong_kind = fixture(9, Some(dead.as_str()));
            fs::create_dir(content_path(&wrong_kind)).expect("wrong content kind");
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
                uppercase,
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
        let entry = OwnedRuntimeEntry::create_directory(scratch.path(), TEST_FILES)
            .expect("create directory");
        let path = entry.path().to_path_buf();
        assert_literal_name(&path, "shepr-test-");
        assert_eq!(
            fs::read_to_string(path.join(".owner")).expect("read marker"),
            ProcessIdentity::current().expect("current identity").tag()
        );
        assert!(
            crate::ipc::acquire_flock_lock(&path.join(OWNER_MARKER), LockWait::FailIfHeld).is_err()
        );
        // The literal content name: release removes the directory only if it
        // recognises this entry as its content.
        fs::write(path.join("config"), "Host *\n").expect("write config");
        entry.release();
        assert!(!path.try_exists().expect("stat released directory"));

        let path = create_owned_directory(scratch.path(), TEST_FILES)
            .expect("create transferred directory");
        assert_literal_name(&path, "shepr-test-");
        release_owned_directory(&path, TEST_FILES);
        release_owned_directory(&path, TEST_FILES);
        assert!(!path.try_exists().expect("stat transferred directory"));
    }

    #[test]
    fn failed_directory_release_restores_marker_without_recursing() {
        let scratch = shepr_test_support::ScratchDir::new("owned-runtime-release");
        let entry = OwnedRuntimeEntry::create_directory(scratch.path(), DirectoryKind::STAGING)
            .expect("create staging directory");
        let path = entry.path().to_path_buf();
        assert_literal_name(&path, ".s");
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
