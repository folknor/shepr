//! Where a shepr process keeps its files and which server socket it targets.
//!
//! [`AppPaths`] resolves the XDG directories, the build profile's runtime and
//! data directories, the launch working directory and the [`ServerAddress`]
//! once at the process boundary. [`BuildProfile`] decides which of those
//! directories a build uses, and the pane markers (`SHEPR_BUILD_PROFILE`,
//! `SHEPR_ENV`) decide whether an inherited `SHEPR_SOCKET_PATH` applies.
//! Nothing here reads a config file, so crates that only need the layout
//! (the API client, the SSH machinery, the CLI) do not link the settings.

mod address;
mod app_paths;
mod error;
mod profile;

pub use self::address::ServerAddress;
pub use self::app_paths::{AppPaths, DATA_DIR_LEASE_FILE_NAME};
pub use self::error::PathsError;
pub use self::profile::BuildProfile;

/// The integration installer lock directory beneath the XDG state home.
/// Agent config is shared by build profiles, so this root is shared too.
pub fn integration_lock_dir(xdg_state_home: &std::path::Path) -> std::path::PathBuf {
    xdg_state_home
        .join(shepr_core::env::SHARED_APP_DIR_NAME)
        .join("integration-locks")
}
