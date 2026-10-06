//! Names and paths for files owned by one shepr build profile.

use std::path::{Path, PathBuf};

pub const DATA_DIR_LEASE_FILE_NAME: &str = "session.lock";
pub const SERVER_LOG_FILE_NAME: &str = "shepr-server.log";
pub const CLIENT_LOG_FILE_NAME: &str = "shepr-client.log";
pub const SESSION_FILE_NAME: &str = "session.json";
pub const SNAPSHOT_DIRECTORY_NAME: &str = "session-snapshots";
pub const BACKUP_DIRECTORY_NAME: &str = "session-backups";
pub const SERVER_SOCKET_FILE_NAME: &str = "shepr.sock";
pub const LAUNCH_LOCK_FILE_NAME: &str = "launch.lock";
pub const BOOT_LOG_FILE_NAME: &str = "server-boot.log";

/// The lease file inside the session data directory.
pub fn data_dir_lease_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DATA_DIR_LEASE_FILE_NAME)
}

/// The server log in the session data directory.
pub fn server_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SERVER_LOG_FILE_NAME)
}

/// The client log in the client's profile-owned state directory.
pub fn client_log_path(client_state_dir: &Path) -> PathBuf {
    client_state_dir.join(CLIENT_LOG_FILE_NAME)
}

/// The saved layout in the session data directory.
pub fn session_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SESSION_FILE_NAME)
}

/// Recovery snapshots beside the saved layout.
pub fn session_snapshot_directory(data_dir: &Path) -> PathBuf {
    data_dir.join(SNAPSHOT_DIRECTORY_NAME)
}

/// Preserved copies of a damaged saved layout, beside the saved layout.
pub fn session_backup_directory(data_dir: &Path) -> PathBuf {
    data_dir.join(BACKUP_DIRECTORY_NAME)
}

/// The client's remembered-remote-executable cache directory, under its
/// profile-owned state directory and outside the server's leased data tree.
pub fn ssh_metadata_directory(client_state_dir: &Path) -> PathBuf {
    client_state_dir.join("ssh-metadata")
}

/// The launch lock in the profile's runtime directory.
pub fn launch_lock_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(LAUNCH_LOCK_FILE_NAME)
}

/// The temporary stderr log in the profile's runtime directory.
pub fn boot_log_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(BOOT_LOG_FILE_NAME)
}

/// The profile's server socket in its runtime directory.
pub fn server_socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(SERVER_SOCKET_FILE_NAME)
}

/// The persistent startup-lock sidecar for a selected server socket.
pub fn socket_startup_lock_path(socket_path: &Path) -> PathBuf {
    let mut name = socket_path.as_os_str().to_os_string();
    name.push(".lock");
    name.into()
}
