//! Where a shepr process keeps its files and which server socket it targets.
//!
//! [`AppPaths`] resolves the XDG directories, the build profile's runtime and
//! data directories, the launch working directory and the [`ServerAddress`]
//! once at the process boundary. [`BuildProfile`] decides which of those
//! directories a build uses. Of the pane markers, `SHEPR_BUILD_PROFILE`
//! decides whether an inherited `SHEPR_SOCKET_PATH` applies; `SHEPR_ENV` says
//! the process runs in a pane at all, which with the profile decides whether
//! the TUI is refused there.
//! Nothing here reads a config file, so crates that only need the layout
//! (the API client, the SSH machinery, the CLI) do not link the settings.

mod address;
mod app_paths;
mod error;
mod layout;
mod profile;

pub use self::address::ServerAddress;
pub use self::app_paths::AppPaths;
pub use self::error::PathsError;
pub use self::layout::{
    BACKUP_DIRECTORY_NAME, BOOT_LOG_FILE_NAME, CLIENT_LOG_FILE_NAME, DATA_DIR_LEASE_FILE_NAME,
    LAUNCH_LOCK_FILE_NAME, SERVER_LOG_FILE_NAME, SERVER_SOCKET_FILE_NAME, SESSION_FILE_NAME,
    SNAPSHOT_DIRECTORY_NAME, boot_log_path, client_log_path, data_dir_lease_path, launch_lock_path,
    server_log_path, server_socket_path, session_backup_directory, session_file_path,
    session_snapshot_directory, socket_startup_lock_path, ssh_metadata_directory,
};
pub use self::profile::BuildProfile;

/// The integration installer lock directory beneath the XDG state home.
/// Agent config is shared by build profiles, so this root is shared too.
pub fn integration_lock_dir(xdg_state_home: &std::path::Path) -> std::path::PathBuf {
    xdg_state_home
        .join(shepr_core::env::SHARED_APP_DIR_NAME)
        .join("integration-locks")
}
