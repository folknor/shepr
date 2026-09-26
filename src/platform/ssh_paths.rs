use super::*;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct RemoteSshConfigPaths {
    pub(crate) user_config: Option<PathBuf>,
    pub(crate) system_config: Option<PathBuf>,
}

/// The longest socket path Linux accepts: `sun_path` is 108 bytes, one of
/// them the terminating NUL.
const UNIX_SOCKET_PATH_MAX: usize = 107;

pub(crate) fn fits_unix_socket_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX
}

pub(crate) fn remote_ssh_config_paths(home_dir: Option<&Path>) -> RemoteSshConfigPaths {
    RemoteSshConfigPaths {
        user_config: home_dir.map(|home| home.join(".ssh").join("config")),
        system_config: Some(PathBuf::from("/etc/ssh/ssh_config")),
    }
}

pub(crate) fn create_remote_ssh_config_dir(control_socket_name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let mut bases = vec![std::env::temp_dir()];
    let short_tmp = PathBuf::from("/tmp");
    if bases.first() != Some(&short_tmp) {
        bases.push(short_tmp);
    }

    let mut last_error = None;
    let mut path_fits = false;
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("shepr-ssh-{}-{attempt}", std::process::id()));
            if !fits_unix_socket_path(&dir.join(control_socket_name)) {
                continue;
            }
            path_fits = true;
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }

    if let Some(err) = last_error {
        return Err(err);
    }
    let message = if path_fits {
        "failed to create private shepr ssh config directory"
    } else {
        "SSH control socket path exceeds the Unix socket length limit"
    };
    Err(std::io::Error::new(
        if path_fits {
            std::io::ErrorKind::AlreadyExists
        } else {
            std::io::ErrorKind::InvalidInput
        },
        message,
    ))
}

/// A path in the shared temp directory for an SSH bridge socket. Callers
/// compute it once and carry it; it is not derivable again.
///
/// The names callers pass are pid-derived and so predictable, and the temp
/// directory is shared: another user could leave a socket at the exact path
/// first, and this user cannot remove it, so the bind would fail. A random
/// token goes into every name ("<stem>.<token>.sock") so such a squat has to
/// guess it. The bind is owner-only and the accept checks its peer either
/// way; this only keeps a connect attempt from being blocked.
pub(crate) fn remote_bridge_endpoint_path(readable_name: &str, short_name: &str) -> PathBuf {
    let token = unpredictable_token();
    let readable_name = with_name_token(readable_name, token);
    let short_name = with_name_token(short_name, token);
    let tmp = std::env::temp_dir();
    let readable = tmp.join(&readable_name);
    if fits_unix_socket_path(&readable) {
        return readable;
    }
    let short = tmp.join(&short_name);
    if fits_unix_socket_path(&short) {
        return short;
    }
    PathBuf::from("/tmp").join(short_name)
}

/// `name` with `.{token:016x}` inserted before its extension, or appended
/// when it has none.
pub(super) fn with_name_token(name: &str, token: u64) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}.{token:016x}.{extension}"),
        _ => format!("{name}.{token:016x}"),
    }
}

/// 64 bits another local user cannot predict: getrandom(2), or std's
/// OS-seeded hasher keys if that fails.
fn unpredictable_token() -> u64 {
    use std::hash::{BuildHasher, Hasher};

    let mut bytes = [0_u8; 8];
    // SAFETY: getrandom(2) writes at most `bytes.len()` bytes into a live
    // stack buffer and keeps no reference to it.
    let filled = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if usize::try_from(filled).is_ok_and(|filled| filled == bytes.len()) {
        return u64::from_ne_bytes(bytes);
    }
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// Shared OpenSSH sockets outlive individual helpers. Never adopt a directory
/// belonging to another uid, a symlink, or a directory accessible by others.
pub(crate) fn shared_ssh_control_path(namespace: &Path, target: &str) -> std::io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
    };

    // Prefer the system shared temp directory, but only when root owns its
    // sticky bit. Some Linux containers expose /tmp through an untrusted uid;
    // in that case the current user's private runtime directory is the safe
    // fallback and already provides the per-user namespace.
    let base = Path::new("/tmp");
    let trusted_shared_tmp = std::fs::canonicalize(base)
        .and_then(std::fs::symlink_metadata)
        .is_ok_and(|metadata| {
            metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o1000 != 0
        });
    let dir = if trusted_shared_tmp {
        let dir = base.join(format!("hssh-{}", effective_uid()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        validate_shared_ssh_dir(&dir)?;
        dir
    } else {
        // This is XDG_RUNTIME_DIR's standard Linux location. Keep sockets
        // directly in it so the maximum-width uid still fits sun_path.
        let runtime = PathBuf::from(format!("/run/user/{}", effective_uid()));
        validate_shared_ssh_dir(&runtime).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no safe SSH control directory parent",
            )
        })?;
        runtime
    };
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
    // Keep 96 bits of namespace/target hash plus OpenSSH's 160-bit %C.
    let digest = hash.finalize();
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hash, "{byte:02x}");
    }
    let path = dir.join(format!("{}-%C", &hash[..24]));
    // OpenSSH first binds ControlPath + '.' + 16 random characters, then
    // renames it. Reserve those 17 bytes, not just the final socket's length.
    let expanded = path.to_string_lossy().replace("%C", &"0".repeat(40));
    let staging = PathBuf::from(format!("{expanded}.{}", "0".repeat(16)));
    if !fits_unix_socket_path(&staging) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH control socket staging path exceeds the Unix socket length limit",
        ));
    }
    Ok(path)
}

pub(super) fn validate_shared_ssh_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o7777 != 0o700
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SSH control directory must be owned by the current user, mode 0700, and not a symlink",
        ));
    }
    Ok(())
}
