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
mod guidance;
mod profile;

pub use self::address::ServerAddress;
pub use self::app_paths::{AppPaths, DATA_DIR_LEASE_FILE_NAME};
pub use self::error::PathsError;
pub use self::guidance::operator_entrypoint;
pub use self::profile::BuildProfile;
