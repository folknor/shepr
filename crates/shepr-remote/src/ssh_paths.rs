//! OpenSSH path policy: where the managed SSH config directory and the shared
//! control socket live, and how their names are made.
//! The generic owned runtime directory, private directory check and
//! dead-owner sweeps are `shepr-platform`'s; the OpenSSH `%C` expansion, the
//! control socket naming and the socket path budgets are SSH policy and live
//! here.
use shepr_core::socket_path::UNIX_SOCKET_PATH_MAX;
use shepr_platform::{DirectoryKind, PrivateDirError, RuntimeCreateError};
use std::path::{Path, PathBuf};

/// The managed SSH config directories: `shepr-ssh-` and a token, each holding
/// one regular file named `config`.
const SSH_CONFIG_DIRECTORY: DirectoryKind = DirectoryKind::regular_file("shepr-ssh-", "config");

/// Crate-internal SSH setup error, separating policy refusals from operational
/// failures before callers adapt it to the public `io::Error` boundary.
#[derive(Debug)]
pub(crate) enum SshRuntimeError {
    UnsafeDirectory(UnsafeSshRuntimeDirectory),
    RandomSource(std::io::Error),
    Io(std::io::Error),
}

impl SshRuntimeError {
    pub(crate) fn kind(&self) -> std::io::ErrorKind {
        match self {
            Self::UnsafeDirectory(_) => std::io::ErrorKind::PermissionDenied,
            Self::RandomSource(error) | Self::Io(error) => error.kind(),
        }
    }
}

impl From<std::io::Error> for SshRuntimeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for SshRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafeDirectory(error) => error.fmt(f),
            Self::RandomSource(error) | Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SshRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnsafeDirectory(error) => Some(error),
            Self::RandomSource(error) | Self::Io(error) => Some(error),
        }
    }
}

/// Create an ephemeral SSH config directory under the validated private runtime
/// directory. Each directory gets a random name so concurrent configured-machine
/// bridges do not share a small per-process allocation limit. Callers remove
/// it with [`release_remote_ssh_config_dir`] when done; the managed SSH owner
/// also registers normal process-exit cleanup. Each creation first sweeps
/// leftovers of hard-killed owners (see `shepr_platform::create_owned_directory`).
pub(crate) fn create_remote_ssh_config_dir(runtime_dir: &Path) -> Result<PathBuf, SshRuntimeError> {
    validate_ssh_runtime_dir(runtime_dir)?;
    shepr_platform::create_owned_directory(runtime_dir, SSH_CONFIG_DIRECTORY).map_err(|error| {
        match error {
            RuntimeCreateError::RandomSource(error) => SshRuntimeError::RandomSource(error),
            RuntimeCreateError::Io(error) => SshRuntimeError::Io(error),
        }
    })
}

/// Release a config directory created by this process. Used by both normal
/// teardown and the process-exit registry; absence is already cleaned up.
pub(crate) fn release_remote_ssh_config_dir(path: &Path) {
    shepr_platform::release_owned_directory(path, SSH_CONFIG_DIRECTORY);
}

/// Resolves the config file owned by a directory from
/// [`create_remote_ssh_config_dir`]. The directory kind owns this filename so
/// creation, cleanup, and dead-owner sweeping use one layout rule.
pub(crate) fn remote_ssh_config_file_path(directory: &Path) -> PathBuf {
    SSH_CONFIG_DIRECTORY.content_path(directory)
}

/// An opaque destination identity used only to scope an SSH control socket.
/// It carries bytes rather than SSH syntax: destination validation belongs to
/// the caller, and this only hashes the identity.
#[derive(Clone, Copy)]
pub(crate) struct SshControlKey<'a>(&'a [u8]);

impl<'a> SshControlKey<'a> {
    /// The identity of a configured machine's checked destination.
    pub(crate) fn for_target(target: &'a shepr_config::SshTarget) -> Self {
        Self(target.as_str().as_bytes())
    }
}

/// The target bytes are an opaque identity for hashing, never command text.
/// Shared OpenSSH sockets outlive individual helpers. Keep them in the
/// private runtime directory so isolated environments cannot reach a user's live
/// master, and reject a directory belonging to another uid, a symlink, or a
/// directory accessible by others.
pub(crate) fn shared_ssh_control_path(
    runtime_dir: &Path,
    target: SshControlKey<'_>,
) -> Result<PathBuf, SshRuntimeError> {
    validate_ssh_runtime_dir(runtime_dir)?;
    ssh_control_path_under(runtime_dir, target).map_err(SshRuntimeError::Io)
}

/// [`shared_ssh_control_path`] without the runtime directory check: the name,
/// and the refusal when OpenSSH's staging path would not fit a socket address.
/// The caller vouches for `runtime_dir`, having passed it through
/// [`validate_ssh_runtime_dir`] or, in a test that only renders the name,
/// chosen a directory nothing binds in. Split out so the naming and length
/// arithmetic can be exercised against a directory as short as a real
/// `/run/user/<uid>`, which no test scratch directory is.
pub(crate) fn ssh_control_path_under(
    runtime_dir: &Path,
    target: SshControlKey<'_>,
) -> std::io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt;

    if runtime_dir.as_os_str().as_bytes().contains(&b'%') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH runtime directory must not contain '%' because OpenSSH interprets percent sequences in ControlPath",
        ));
    }
    let mut hash = Sha256::new();
    hash.update(target.0);
    // %C additionally scopes the socket to OpenSSH's resolved destination,
    // port and jump host, rather than merely the spelling of an alias.
    // Keep 64 bits of target hash plus OpenSSH's 160-bit %C. The profile's
    // runtime directory already isolates shepr's sockets; no config-path
    // namespace is needed here. What is left of the socket address for the
    // runtime directory is small, but a real `/run/user/<uid>` fits with room
    // to spare.
    let digest = hash.finalize();
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Formatting a byte into a String cannot fail.
        write!(hash, "{byte:02x}").ok();
    }
    let path = runtime_dir.join(format!("{}-%C", &hash[..16]));
    // OpenSSH first binds ControlPath + '.' + 16 random characters, then
    // renames it. Reserve those 17 bytes, not just the final socket's length.
    // OpenSSH expands each literal `%C` token to 40 ASCII bytes, then appends
    // a dot and 16 random bytes while staging the socket. Count the added
    // bytes directly so a non-UTF-8 runtime directory is measured as its
    // actual path bytes.
    let path_bytes = path.as_os_str().as_bytes();
    let code_expansions = path_bytes
        .windows(2)
        .filter(|token| token[0] == b'%' && token[1] == b'C')
        .count();
    let staging_path_len = path_bytes
        .len()
        .saturating_add(code_expansions.saturating_mul(40 - 2))
        .saturating_add(1 + 16);
    if staging_path_len > UNIX_SOCKET_PATH_MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "SSH control socket staging path for {} is {staging_path_len} bytes; Linux's Unix socket limit is {UNIX_SOCKET_PATH_MAX} bytes, so shorten XDG_RUNTIME_DIR",
                path.display(),
            ),
        ));
    }
    Ok(path)
}

/// Refuses a runtime directory that may not hold shared SSH sockets: a
/// relative path, a symlink, or a directory another uid owns or can reach.
pub(crate) fn validate_ssh_runtime_dir(runtime_dir: &Path) -> Result<(), SshRuntimeError> {
    if !runtime_dir.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH runtime directory must be an absolute path",
        )
        .into());
    }
    validate_shared_ssh_dir(runtime_dir)
}

fn validate_shared_ssh_dir(dir: &Path) -> Result<(), SshRuntimeError> {
    match shepr_platform::require_private_directory(dir) {
        Ok(()) => Ok(()),
        Err(PrivateDirError::Policy) => Err(SshRuntimeError::UnsafeDirectory(
            UnsafeSshRuntimeDirectory::new(dir),
        )),
        Err(PrivateDirError::Io(error)) => Err(SshRuntimeError::Io(error)),
    }
}

/// A deterministic policy failure, distinct from filesystem permission errors.
#[derive(Debug)]
pub(crate) struct UnsafeSshRuntimeDirectory {
    path: PathBuf,
}

impl UnsafeSshRuntimeDirectory {
    /// Builds the typed policy error while preserving the path that failed validation.
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }
}

impl std::fmt::Display for UnsafeSshRuntimeDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SSH runtime directory {} must be owned by the current user, mode {:04o}, and not a symlink",
            self.path.display(),
            shepr_platform::PRIVATE_DIRECTORY_MODE,
        )
    }
}

impl std::error::Error for UnsafeSshRuntimeDirectory {}

#[cfg(test)]
impl<'a> SshControlKey<'a> {
    pub(crate) fn from_identity_bytes(identity: &'a [u8]) -> Self {
        Self(identity)
    }
}

#[cfg(test)]
mod tests;
