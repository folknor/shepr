use super::random::unpredictable_token;
use shepr_core::socket_path::{UNIX_SOCKET_PATH_MAX, fits_unix_socket_path};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RemoteSshConfigPaths {
    pub user_config: Option<PathBuf>,
    pub system_config: PathBuf,
}

pub fn remote_ssh_config_paths(home_dir: Option<&Path>) -> RemoteSshConfigPaths {
    RemoteSshConfigPaths {
        user_config: home_dir.map(|home| home.join(".ssh").join("config")),
        system_config: PathBuf::from("/etc/ssh/ssh_config"),
    }
}

/// Create an ephemeral SSH config directory under the validated private runtime
/// directory. Each directory gets a random name so concurrent configured-machine
/// bridges do not share a small per-process allocation limit. Callers remove
/// it when done; the managed SSH owner also registers normal process-exit
/// cleanup. Each creation first sweeps leftovers of hard-killed owners: only
/// current-uid directories whose locked marker records a process `/proc` proves
/// has exited. Unmarked directories (created when the owner identity could not be
/// read) are retained because their owner cannot be established.
pub fn create_remote_ssh_config_dir(runtime_dir: &Path) -> std::io::Result<PathBuf> {
    validate_ssh_runtime_dir(runtime_dir)?;
    super::owned_runtime::OwnedRuntimeEntry::create_directory(
        runtime_dir,
        super::owned_runtime::DirectoryKind::SshConfig,
    )
    .map(super::owned_runtime::OwnedRuntimeEntry::into_path)
    .map_err(super::owned_runtime::RuntimeCreateError::into_io)
}

/// Resolves the config file owned by a directory from
/// [`create_remote_ssh_config_dir`]. The directory kind owns this filename so
/// creation, cleanup, and dead-owner sweeping use one layout rule.
pub fn remote_ssh_config_file_path(directory: &Path) -> PathBuf {
    super::owned_runtime::DirectoryKind::SshConfig.content_path(directory)
}

/// Choose an endpoint socket path in shepr's private runtime directory. The
/// token avoids collisions between concurrent bridges; the shorter name is
/// used when the readable one would exceed Linux's socket path limit. The
/// path is single-use, so bind it with
/// [`crate::ipc::bind_single_use_private_socket`]. After confirming a path
/// fits, allocation sweeps sockets and locks such binds left behind in
/// `runtime_dir` when their owner was killed; the owner is recorded in the lock
/// sidecar rather than the name, so the name spends none of the socket path
/// limit on it.
pub fn remote_bridge_endpoint_path(
    runtime_dir: &Path,
    readable_name: &str,
    short_name: &str,
) -> std::io::Result<PathBuf> {
    validate_ssh_runtime_dir(runtime_dir)?;
    bridge_endpoint_path_with_token(runtime_dir, readable_name, short_name, 0)?;
    super::ipc::sweep_abandoned_single_use_sockets(runtime_dir);
    let token = unpredictable_token()?;
    bridge_endpoint_path_with_token(runtime_dir, readable_name, short_name, token)
}

/// Checks that a fresh endpoint socket path can fit without sweeping the
/// runtime directory or consuming a random token.
pub fn validate_remote_bridge_endpoint_path(
    runtime_dir: &Path,
    readable_name: &str,
    short_name: &str,
) -> std::io::Result<()> {
    validate_ssh_runtime_dir(runtime_dir)?;
    bridge_endpoint_path_with_token(runtime_dir, readable_name, short_name, 0).map(|_| ())
}

fn bridge_endpoint_path_with_token(
    runtime_dir: &Path,
    readable_name: &str,
    short_name: &str,
    token: u64,
) -> std::io::Result<PathBuf> {
    let readable_name = with_name_token(readable_name, token);
    let short_name = with_name_token(short_name, token);
    let readable = runtime_dir.join(&readable_name);
    if fits_unix_socket_path(&readable) {
        return Ok(readable);
    }
    let short = runtime_dir.join(&short_name);
    if fits_unix_socket_path(&short) {
        return Ok(short);
    }
    let readable_len = unix_socket_path_len(&readable);
    let short_len = unix_socket_path_len(&short);
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "SSH bridge socket paths do not fit Linux's Unix socket limit of {UNIX_SOCKET_PATH_MAX} bytes: readable path {} is {readable_len} bytes and compact path {} is {short_len} bytes; shorten XDG_RUNTIME_DIR",
            readable.display(),
            short.display(),
        ),
    ))
}

fn unix_socket_path_len(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len()
}

/// `name` with `.{token:016x}` inserted before its extension, or appended
/// when it has none.
pub(super) fn with_name_token(name: &str, token: u64) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}.{token:016x}.{extension}"),
        _ => format!("{name}.{token:016x}"),
    }
}

/// Shared OpenSSH sockets outlive individual helpers. Keep them in the
/// private runtime directory so isolated environments cannot reach a user's live
/// master, and reject a directory belonging to another uid, a symlink, or a
/// directory accessible by others.
pub fn shared_ssh_control_path(
    runtime_dir: &Path,
    namespace: &Path,
    target: &str,
) -> std::io::Result<PathBuf> {
    validate_ssh_runtime_dir(runtime_dir)?;
    ssh_control_path_under(runtime_dir, namespace, target)
}

/// [`shared_ssh_control_path`] without the runtime directory check: the name,
/// and the refusal when OpenSSH's staging path would not fit a socket address.
/// The caller vouches for `runtime_dir`, having passed it through
/// [`validate_ssh_runtime_dir`] or, in a test that only renders the name,
/// chosen a directory nothing binds in. Split out so the naming and length
/// arithmetic can be exercised against a directory as short as a real
/// `/run/user/<uid>`, which no test scratch directory is.
pub fn ssh_control_path_under(
    runtime_dir: &Path,
    namespace: &Path,
    target: &str,
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
    if !namespace.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH control namespace must be an absolute path",
        ));
    }
    let namespace = namespace.to_owned();
    let mut hash = Sha256::new();
    hash.update(namespace.as_os_str().as_bytes());
    hash.update([0]);
    hash.update(target.as_bytes());
    // %C additionally scopes the socket to OpenSSH's resolved destination,
    // port and jump host, rather than merely the spelling of an alias.
    // Keep 64 bits of namespace/target hash plus OpenSSH's 160-bit %C. What
    // is left of the socket address for the runtime directory is small, but a
    // real `/run/user/<uid>` fits with room to spare.
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
pub fn validate_ssh_runtime_dir(runtime_dir: &Path) -> std::io::Result<()> {
    if !runtime_dir.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH runtime directory must be an absolute path",
        ));
    }
    validate_shared_ssh_dir(runtime_dir)
}

pub(super) fn validate_shared_ssh_dir(dir: &Path) -> std::io::Result<()> {
    match super::private_file::PrivateDir::require(dir) {
        Ok(()) => Ok(()),
        Err(error) if super::private_file::PrivateDir::is_policy_refusal(&error) => {
            // Keep this typed error as io::Error's direct payload: shepr-remote
            // downcasts it to classify launch failures. Carry the rejected path
            // so the operator can identify which runtime directory failed.
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                UnsafeSshRuntimeDirectory::new(dir),
            ))
        }
        Err(error) => Err(error),
    }
}

/// A deterministic policy failure, distinct from filesystem permission errors.
#[derive(Debug)]
pub struct UnsafeSshRuntimeDirectory {
    path: PathBuf,
}

impl UnsafeSshRuntimeDirectory {
    /// Builds the typed policy error while preserving the path that failed validation.
    pub fn new(path: &Path) -> Self {
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
            super::limits::PRIVATE_DIRECTORY_MODE,
        )
    }
}

impl std::error::Error for UnsafeSshRuntimeDirectory {}

#[cfg(test)]
mod tests {
    use super::ssh_control_path_under;
    use std::path::Path;

    #[test]
    fn ssh_control_path_rejects_percent_tokens_in_runtime_directory() {
        for runtime_dir in [Path::new("/run/%h"), Path::new("/run/%%")] {
            let error = ssh_control_path_under(runtime_dir, Path::new("/config/one"), "host")
                .expect_err("OpenSSH would reinterpret percent sequences in ControlPath");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("percent sequences"));
        }
    }
}
