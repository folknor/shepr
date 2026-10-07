use std::io;
use std::path::{Path, PathBuf};

use shepr_core::absolute_path::AbsolutePath;
use shepr_core::env::{EnvVar, SHARED_APP_DIR_NAME};

use crate::profile::{PaneMarker, PaneOwner};
use crate::{
    BuildProfile, PathsError, ServerAddress, boot_log_path, client_log_path, data_dir_lease_path,
    launch_lock_path, server_log_path, session_backup_directory, session_file_path,
    session_snapshot_directory, ssh_metadata_directory,
};

/// Paths and the local target resolved once at the process boundary and
/// passed to consumers. Production constructors reject unresolved path inputs
/// that would put files relative to the working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    config_dir: PathBuf,
    state_dir: PathBuf,
    data_dir: PathBuf,
    client_state_dir: PathBuf,
    xdg_runtime_dir: PathBuf,
    runtime_dir: PathBuf,
    home_dir: Option<AbsolutePath>,
    current_dir: Option<AbsolutePath>,
    /// `current_dir`, or the root when there is none: kept so
    /// [`fallback_cwd`](Self::fallback_cwd) can lend it.
    fallback_cwd: AbsolutePath,
    startup_cwd: Option<AbsolutePath>,
    server_address: ServerAddress,
}

impl AppPaths {
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// The state directory shared by every build profile. It holds the
    /// client's sidebar preferences (keyed by server socket), which for a
    /// release build sit inside the server's leased
    /// [`data_dir`](Self::data_dir) because the two are the same directory. The
    /// client log and the SSH metadata cache are per profile and live elsewhere
    /// (see [`client_log`](Self::client_log)). The saved layout and its
    /// recovery files live in [`data_dir`](Self::data_dir).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The directory for the saved layout, its recovery files, the server log,
    /// and the lease that keeps one server per directory. For a
    /// release build it is [`state_dir`](Self::state_dir) itself; a dev build
    /// gets a `shepr-dev` sibling of it.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The client's remembered remote-executable cache for this profile.
    pub fn ssh_metadata_directory(&self) -> PathBuf {
        ssh_metadata_directory(&self.client_state_dir)
    }

    /// The server log in this build profile's data directory.
    pub fn server_log(&self) -> PathBuf {
        server_log_path(self.data_dir())
    }

    /// The client log in this build profile's client-owned state directory.
    pub fn client_log(&self) -> PathBuf {
        client_log_path(&self.client_state_dir)
    }

    /// The lease file inside [`data_dir`](Self::data_dir): the server that holds
    /// an exclusive lock on it owns the directory. It is never removed, so every
    /// contender locks the same inode.
    pub fn data_dir_lease_path(&self) -> PathBuf {
        data_dir_lease_path(self.data_dir())
    }

    /// The saved layout file in this build profile's data directory.
    pub fn session_file_path(&self) -> PathBuf {
        session_file_path(self.data_dir())
    }

    /// The directory for recovery snapshots of the saved layout.
    pub fn session_snapshot_directory(&self) -> PathBuf {
        session_snapshot_directory(self.data_dir())
    }

    /// The directory for preserving a saved layout that could not be fully
    /// restored.
    pub fn session_backup_directory(&self) -> PathBuf {
        session_backup_directory(self.data_dir())
    }

    /// The launch lock in this build profile's runtime directory.
    pub fn launch_lock_path(&self) -> PathBuf {
        launch_lock_path(self.runtime_dir())
    }

    /// The temporary stderr log in this build profile's runtime directory.
    pub fn boot_log_path(&self) -> PathBuf {
        boot_log_path(self.runtime_dir())
    }

    /// The startup lock sidecar for this process's selected server socket.
    pub fn server_socket_startup_lock_path(&self) -> PathBuf {
        self.server_address.startup_lock_path()
    }

    /// The XDG runtime root before the application-specific directory is added.
    pub fn xdg_runtime_dir(&self) -> &Path {
        &self.xdg_runtime_dir
    }

    /// The build profile's runtime directory: `shepr` under the XDG runtime
    /// directory for a release build, `shepr-dev` for a dev build.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn client_config_file(&self) -> PathBuf {
        self.config_dir.join("client.toml")
    }

    pub fn server_config_file(&self) -> PathBuf {
        self.config_dir.join("server.toml")
    }

    /// `HOME`, absolute: a launch with a relative one fails where these paths
    /// are resolved.
    pub fn home_dir(&self) -> Option<&AbsolutePath> {
        self.home_dir.as_ref()
    }

    /// The launch working directory, absolute, or `None` when it could not be
    /// read at launch.
    pub fn current_dir(&self) -> Option<&AbsolutePath> {
        self.current_dir.as_ref()
    }

    /// Last-resort server cwd, captured at launch without later filesystem IO:
    /// [`current_dir`](Self::current_dir), else the root.
    pub fn fallback_cwd(&self) -> &AbsolutePath {
        &self.fallback_cwd
    }

    /// The absolute `SHEPR_STARTUP_CWD` handed to the server, when present.
    /// The server's own working directory is not a startup handoff.
    pub fn startup_cwd(&self) -> Option<&AbsolutePath> {
        self.startup_cwd.as_ref()
    }

    pub fn server_address(&self) -> &ServerAddress {
        &self.server_address
    }

    /// Resolve XDG directories and the local socket target once from the
    /// inherited process environment, for this build's profile.
    pub fn resolve() -> Result<Self, PathsError> {
        resolve_paths_from_env(BuildProfile::current(), CurrentDirOrigin::Process)
    }

    /// Resolve paths for a TUI or its internal client launch. A client launched
    /// from a pane owned by this build profile is refused before path or config
    /// loading; `None` represents that refusal.
    pub fn resolve_for_client() -> Result<Option<Self>, PathsError> {
        let profile = BuildProfile::current();
        let mut problems = Vec::new();
        let marker = PaneMarker::read(&mut problems);
        if !problems.is_empty() {
            return Err(PathsError::new(problems));
        }
        if marker.owner(profile) == PaneOwner::SameProfile {
            return Ok(None);
        }
        resolve_paths_from_env_with_marker(profile, CurrentDirOrigin::Process, marker, Vec::new())
            .map(Some)
    }

    /// Resolve paths for the headless server process. The server daemon runs
    /// in the home directory so it never pins the directory it was launched
    /// from, but its current directory is still the one the user launched
    /// `shepr` from: the spawning client hands that over as
    /// `SHEPR_STARTUP_CWD`, and it is what `terminal.new_cwd = "current"`, a
    /// relative `terminal.new_cwd` and the new-terminal fallback resolve
    /// against. A server started without the handoff (by hand, from a shell)
    /// uses its own working directory.
    pub fn resolve_for_server() -> Result<Self, PathsError> {
        resolve_paths_from_env(BuildProfile::current(), CurrentDirOrigin::StartupHandoff)
    }

    /// Paths laid out under one directory: `config`, `state` and `runtime`
    /// below `root`, with `root` as the XDG runtime directory, the saved
    /// layout in the state directory (the release profile's layout, whatever
    /// profile built the caller) and every value's source the default. The
    /// environment is not read and no directory is touched on disk, so the
    /// caller passes absolute paths; this is how a caller that is not a
    /// launch, which resolves from the environment, places a config somewhere
    /// it chose. The checks are lexical: a root too long for
    /// `runtime/shepr.sock` to name a Unix socket is refused, and so is a home
    /// or current directory that is not absolute, each as `InvalidInput`.
    pub fn rooted_at(
        root: &Path,
        home_dir: Option<&Path>,
        current_dir: Option<&Path>,
    ) -> io::Result<Self> {
        let absolute = |path: &Path| {
            AbsolutePath::new(path)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        };
        let home_dir = home_dir.map(absolute).transpose()?;
        let current_dir = current_dir.map(absolute).transpose()?;
        Ok(Self {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            data_dir: root.join("state"),
            client_state_dir: root.join("state-client"),
            xdg_runtime_dir: root.to_path_buf(),
            runtime_dir: root.join("runtime"),
            home_dir,
            fallback_cwd: fallback_cwd(current_dir.as_ref()),
            current_dir,
            startup_cwd: None,
            server_address: ServerAddress::for_runtime_dir(&root.join("runtime"), None)?,
        })
    }
}

fn fallback_cwd(current_dir: Option<&AbsolutePath>) -> AbsolutePath {
    current_dir.cloned().unwrap_or_else(AbsolutePath::root)
}

fn socket_path_override(variable: EnvVar, problems: &mut Vec<String>) -> Option<PathBuf> {
    shepr_core::env::read_path(variable).unwrap_or_else(|error| {
        problems.push(error.to_string());
        None
    })
}

/// Where a process's resolved current directory comes from.
#[derive(Clone, Copy)]
enum CurrentDirOrigin {
    /// The process's own working directory.
    Process,
    /// The launch directory a spawning client handed the server as
    /// `SHEPR_STARTUP_CWD`, falling back to the process's own directory when
    /// the variable is unset.
    StartupHandoff,
}

/// The resolved current directory and, for a server, the startup handoff it
/// came from. A process directory that cannot be read is `None`; one that is
/// read but not absolute fails the launch, like a relative handoff.
fn resolve_current_dir(
    origin: CurrentDirOrigin,
) -> Result<(Option<AbsolutePath>, Option<AbsolutePath>), String> {
    let process = || match std::env::current_dir() {
        Ok(path) => AbsolutePath::new(path)
            .map(Some)
            .map_err(|error| format!("the current directory is not absolute: {error}")),
        Err(_) => Ok(None),
    };
    match origin {
        CurrentDirOrigin::Process => Ok((process()?, None)),
        CurrentDirOrigin::StartupHandoff => {
            match shepr_core::env::read_path(EnvVar::SheprStartupCwd) {
                Ok(Some(path)) => match AbsolutePath::new(path) {
                    Ok(path) => Ok((Some(path.clone()), Some(path))),
                    Err(error) => Err(format!(
                        "{} must be an absolute path, got {}",
                        EnvVar::SheprStartupCwd,
                        error.path().display()
                    )),
                },
                Ok(None) => Ok((process()?, None)),
                Err(error) => Err(error.to_string()),
            }
        }
    }
}

fn resolve_paths_from_env(
    profile: BuildProfile,
    current_dir_origin: CurrentDirOrigin,
) -> Result<AppPaths, PathsError> {
    let mut target_env_problems = Vec::new();
    let pane_marker = PaneMarker {
        in_pane: false,
        owner_profile: PaneMarker::read_profile(&mut target_env_problems),
    };
    resolve_paths_from_env_with_marker(
        profile,
        current_dir_origin,
        pane_marker,
        target_env_problems,
    )
}

fn resolve_paths_from_env_with_marker(
    profile: BuildProfile,
    current_dir_origin: CurrentDirOrigin,
    pane_marker: PaneMarker,
    mut target_env_problems: Vec<String>,
) -> Result<AppPaths, PathsError> {
    let mut socket_override =
        socket_path_override(EnvVar::SheprSocketPath, &mut target_env_problems);
    // A pane names the profile of the server that owns it next to the socket
    // variable it exports. A process of another profile started in that pane
    // would otherwise follow it to the wrong server, so it drops it. With
    // no marker the variable came from a user or a script and applies as given.
    if pane_marker
        .owner_profile
        .is_some_and(|owner| owner != profile)
    {
        socket_override = None;
    }
    if !target_env_problems.is_empty() {
        return Err(PathsError::new(target_env_problems));
    }

    let home_dir =
        shepr_core::pathutil::home_dir().map_err(|error| PathsError::one(error.to_string()))?;
    let home_dir = AbsolutePath::new(home_dir)
        .map_err(|error| PathsError::one(format!("HOME is not absolute: {error}")))?;
    let (current_dir, startup_cwd) =
        resolve_current_dir(current_dir_origin).map_err(PathsError::one)?;
    let read_base = |variable| {
        if variable == EnvVar::Home {
            Ok(Some(home_dir.as_path().to_path_buf()))
        } else {
            shepr_core::env::read_path(variable).map_err(io::Error::from)
        }
    };
    let config_dir =
        shepr_core::env::xdg_config_home_with(read_base).map(|path| path.join(SHARED_APP_DIR_NAME));
    let state_dir =
        shepr_core::env::xdg_state_home_with(read_base).map(|path| path.join(SHARED_APP_DIR_NAME));
    // A relative XDG_RUNTIME_DIR is refused by the environment policy. Unset
    // or empty falls back to the directory logind makes for the user.
    let xdg_runtime_dir = match shepr_core::env::read_path(EnvVar::XdgRuntimeDir) {
        Ok(Some(path)) => Ok(path),
        Ok(None) => logind_runtime_dir(Path::new(LOGIND_RUNTIME_ROOT)),
        Err(error) => Err(io::Error::other(error.to_string())),
    };
    let runtime_dir = xdg_runtime_dir
        .as_ref()
        .map(|path| path.join(profile.app_dir_name()))
        .map_err(|error| io::Error::other(error.to_string()));
    let xdg_runtime_dir = xdg_runtime_dir.ok();

    let mut problems = Vec::new();
    let config_dir = match config_dir {
        Ok(path) => Some(path),
        Err(error) => {
            problems.push(format!("config directory error: {error}"));
            None
        }
    };

    let state_dir = match state_dir {
        Ok(path) => Some(path),
        Err(error) => {
            problems.push(format!("state directory error: {error}"));
            None
        }
    };
    let runtime_dir = match runtime_dir {
        Ok(path) => Some(path),
        Err(error) => {
            problems.push(format!("runtime directory error: {error}"));
            None
        }
    };

    match (config_dir, state_dir, xdg_runtime_dir, runtime_dir) {
        (Some(config_dir), Some(state_dir), Some(xdg_runtime_dir), Some(runtime_dir))
            if problems.is_empty() =>
        {
            // Fail before socket setup when either selected endpoint is too long.
            let server_address =
                ServerAddress::for_runtime_dir(&runtime_dir, socket_override.as_deref()).map_err(
                    |error| PathsError::one(format!("server socket path error: {error}")),
                )?;
            // The saved layout sits beside the shared state directory under the
            // profile's directory name: the state directory itself for release.
            let data_dir = state_dir.with_file_name(profile.app_dir_name());
            let client_state_dir =
                state_dir.with_file_name(format!("{}-client", profile.app_dir_name()));
            Ok(AppPaths {
                config_dir,
                state_dir,
                data_dir,
                client_state_dir,
                xdg_runtime_dir,
                runtime_dir,
                home_dir: Some(home_dir),
                fallback_cwd: fallback_cwd(current_dir.as_ref()),
                current_dir,
                startup_cwd,
                server_address,
            })
        }
        _ => Err(PathsError::new(problems)),
    }
}

/// Where logind creates each user's runtime directory, named by uid.
const LOGIND_RUNTIME_ROOT: &str = "/run/user";

/// The runtime directory logind made for this user under `root`, for a
/// session that never exported XDG_RUNTIME_DIR: one that skipped
/// `pam_systemd`, such as Tailscale SSH. It is used only when it is a private
/// directory owned by this user, as logind makes it, since shepr's sockets go
/// below it.
fn logind_runtime_dir(root: &Path) -> io::Result<PathBuf> {
    let path = root.join(shepr_platform::effective_uid().to_string());
    let refusal = |reason: &dyn std::fmt::Display| {
        io::Error::other(format!(
            "XDG_RUNTIME_DIR is not set and {} is not usable instead: {reason}",
            path.display()
        ))
    };
    match shepr_platform::require_private_directory(&path) {
        Ok(()) => Ok(path),
        Err(shepr_platform::PrivateDirError::Io(error)) => Err(refusal(&error)),
        Err(shepr_platform::PrivateDirError::Policy) => Err(refusal(
            &"it is not a directory private to this user (owned by it, mode 0700, not a symlink)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_config_paths_use_the_xdg_config_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        let paths = AppPaths::resolve().expect("default paths resolve");
        let directory = env.home().join(".config").join(SHARED_APP_DIR_NAME);
        assert_eq!(paths.client_config_file(), directory.join("client.toml"));
        assert_eq!(paths.server_config_file(), directory.join("server.toml"));
    }

    #[test]
    fn profile_file_accessors_cover_the_data_and_runtime_layout() {
        let paths = AppPaths::rooted_at(Path::new("/r"), None, None)
            .expect("short root has a valid layout");

        assert_eq!(
            paths.data_dir_lease_path().as_path(),
            Path::new("/r/state/session.lock")
        );
        assert_eq!(
            paths.server_log().as_path(),
            Path::new("/r/state/shepr-server.log")
        );
        assert_eq!(
            paths.client_log().as_path(),
            Path::new("/r/state-client/shepr-client.log")
        );
        assert_eq!(
            paths.session_file_path().as_path(),
            Path::new("/r/state/session.json")
        );
        assert_eq!(
            paths.session_snapshot_directory().as_path(),
            Path::new("/r/state/session-snapshots")
        );
        assert_eq!(
            paths.session_backup_directory().as_path(),
            Path::new("/r/state/session-backups")
        );
        assert_eq!(
            paths.launch_lock_path().as_path(),
            Path::new("/r/runtime/launch.lock")
        );
        assert_eq!(
            paths.boot_log_path().as_path(),
            Path::new("/r/runtime/server-boot.log")
        );
        assert_eq!(
            paths.server_socket_startup_lock_path().as_path(),
            Path::new("/r/runtime/shepr.sock.lock")
        );
    }

    #[test]
    fn a_root_too_long_for_the_server_socket_is_refused() {
        let root =
            PathBuf::from("/").join("x".repeat(shepr_core::socket_path::UNIX_SOCKET_PATH_MAX));
        let error = AppPaths::rooted_at(&root, None, None)
            .expect_err("the runtime socket cannot fit below this root");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let paths = AppPaths::rooted_at(Path::new("/r"), None, None).expect("short root");
        assert_eq!(
            paths.server_address().socket(),
            Path::new("/r/runtime/shepr.sock")
        );
        assert!(paths.server_address().is_runtime_address());
        assert_eq!(paths.fallback_cwd(), Path::new("/"));
    }

    #[test]
    fn a_rooted_layout_refuses_a_relative_home_or_current_directory() {
        let root = Path::new("/r");
        for (home, current) in [
            (Some(Path::new("home")), None),
            (None, Some(Path::new("launch"))),
        ] {
            let error = AppPaths::rooted_at(root, home, current)
                .expect_err("a relative directory is refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        let paths = AppPaths::rooted_at(root, Some(Path::new("/h")), Some(Path::new("/c")))
            .expect("absolute directories are accepted");
        assert_eq!(paths.fallback_cwd(), Path::new("/c"));
    }

    #[test]
    fn server_current_dir_is_the_handed_over_launch_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        let launch = env.path().join("launch");
        std::fs::create_dir_all(&launch).expect("create launch directory");
        let process_dir = std::env::current_dir().ok();
        assert_ne!(process_dir.as_deref(), Some(launch.as_path()));

        // Without the handoff the server uses its own working directory.
        let paths = AppPaths::resolve_for_server().expect("server paths resolve");
        assert_eq!(
            paths.current_dir().map(AbsolutePath::as_path),
            process_dir.as_deref()
        );
        assert_eq!(paths.startup_cwd(), None);

        env.set(EnvVar::SheprStartupCwd, &launch);
        let paths = AppPaths::resolve_for_server().expect("server paths resolve");
        assert_eq!(
            paths.current_dir().map(AbsolutePath::as_path),
            Some(launch.as_path())
        );
        assert_eq!(
            paths.startup_cwd().map(AbsolutePath::as_path),
            Some(launch.as_path())
        );
        assert_eq!(paths.fallback_cwd(), launch.as_path());
        // Only the server reads the handoff; any other process keeps its own.
        let cli = AppPaths::resolve().expect("CLI paths resolve");
        assert_eq!(
            cli.current_dir().map(AbsolutePath::as_path),
            process_dir.as_deref()
        );
        assert_eq!(cli.startup_cwd(), None);

        env.set(EnvVar::SheprStartupCwd, "relative/launch");
        let errors = AppPaths::resolve_for_server().expect_err("a relative handoff is refused");
        assert!(
            errors
                .messages()
                .iter()
                .any(|error| error.contains("SHEPR_STARTUP_CWD") && error.contains("absolute")),
            "{errors:?}"
        );
    }

    #[test]
    fn invalid_socket_environment_fails_resolution() {
        let env = shepr_test_support::IsolatedEnv::new();
        let variable = EnvVar::SheprSocketPath;
        for (value, expected) in [
            ("", "set but empty"),
            ("rel.sock", "absolute path"),
            (" /abs.sock", "whitespace"),
        ] {
            env.set(variable, value);
            let errors = AppPaths::resolve().expect_err("invalid socket override");
            assert!(
                errors
                    .messages()
                    .iter()
                    .any(|error| error.contains(variable.name()) && error.contains(expected)),
                "{variable}={value:?}: {errors:?}"
            );
        }
        env.remove(variable);
    }

    #[test]
    fn release_profile_keeps_the_default_locations_and_dev_gets_its_own() {
        let env = shepr_test_support::IsolatedEnv::new();
        let release = resolve_paths_from_env(BuildProfile::Release, CurrentDirOrigin::Process)
            .expect("release paths resolve");
        let dev = resolve_paths_from_env(BuildProfile::Dev, CurrentDirOrigin::Process)
            .expect("dev paths resolve");
        let state = env.home().join(".local/state");
        let runtime = env.path().join("runtime");

        // Release: exactly the locations every release install has used.
        assert_eq!(release.data_dir(), state.join("shepr"));
        assert_eq!(release.data_dir(), release.state_dir());
        assert_eq!(
            release.ssh_metadata_directory(),
            state.join("shepr-client/ssh-metadata")
        );
        assert_eq!(
            release.client_log(),
            state.join("shepr-client/shepr-client.log")
        );
        assert_eq!(release.runtime_dir(), runtime.join("shepr"));
        assert_eq!(
            release.server_address().socket(),
            runtime.join("shepr/shepr.sock")
        );

        // Dev: its own runtime and saved layout, distinct sockets.
        assert_eq!(dev.data_dir(), state.join("shepr-dev"));
        assert_eq!(
            dev.ssh_metadata_directory(),
            state.join("shepr-dev-client/ssh-metadata")
        );
        assert_eq!(
            dev.client_log(),
            state.join("shepr-dev-client/shepr-client.log")
        );
        assert_eq!(dev.runtime_dir(), runtime.join("shepr-dev"));
        assert_eq!(
            dev.server_address().socket(),
            runtime.join("shepr-dev/shepr.sock")
        );

        // Both config files, the shared state directory and the XDG runtime
        // root are the same in both profiles.
        assert_eq!(release.config_dir(), dev.config_dir());
        assert_eq!(release.client_config_file(), dev.client_config_file());
        assert_eq!(release.server_config_file(), dev.server_config_file());
        assert_eq!(release.state_dir(), dev.state_dir());
        assert_eq!(release.xdg_runtime_dir(), dev.xdg_runtime_dir());
    }

    #[test]
    fn a_socket_override_beats_the_profile_runtime_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        for profile in [BuildProfile::Release, BuildProfile::Dev] {
            let paths = resolve_paths_from_env(profile, CurrentDirOrigin::Process)
                .expect("override resolves");
            assert_eq!(paths.server_address().socket(), env.path().join("api.sock"));
            assert_eq!(
                paths.runtime_dir().file_name(),
                Some(std::ffi::OsStr::new(profile.app_dir_name()))
            );
        }
    }

    #[test]
    fn a_socket_override_with_a_matching_marker_wins() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        for profile in [BuildProfile::Release, BuildProfile::Dev] {
            env.set(EnvVar::SheprBuildProfile, profile.marker());
            let paths = resolve_paths_from_env(profile, CurrentDirOrigin::Process)
                .expect("override resolves");
            assert_eq!(paths.server_address().socket(), env.path().join("api.sock"));
        }
    }

    #[test]
    fn a_socket_override_with_another_profiles_marker_is_ignored() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        for (profile, owner) in [
            (BuildProfile::Dev, BuildProfile::Release),
            (BuildProfile::Release, BuildProfile::Dev),
        ] {
            env.set(EnvVar::SheprBuildProfile, owner.marker());
            let paths =
                resolve_paths_from_env(profile, CurrentDirOrigin::Process).expect("paths resolve");
            let runtime = env.path().join("runtime").join(profile.app_dir_name());
            assert_eq!(
                paths.server_address().socket(),
                crate::server_socket_path(&runtime)
            );
        }
    }

    #[test]
    fn an_unknown_profile_marker_fails_resolution() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprBuildProfile, "staging");
        let errors = resolve_paths_from_env(BuildProfile::Dev, CurrentDirOrigin::Process)
            .expect_err("an unknown marker is refused");
        assert!(
            errors
                .messages()
                .iter()
                .any(|error| error.contains("SHEPR_BUILD_PROFILE") && error.contains("staging")),
            "{errors:?}"
        );
    }

    #[test]
    fn xdg_paths_use_separate_roots_ignore_empty_and_refuse_relative_base_dirs() {
        let env = shepr_test_support::IsolatedEnv::new();
        let paths = AppPaths::resolve().expect("default paths resolve");
        assert_eq!(
            paths.config_dir(),
            env.home().join(".config").join(SHARED_APP_DIR_NAME)
        );
        assert_eq!(
            paths.state_dir(),
            env.home().join(".local/state").join(SHARED_APP_DIR_NAME)
        );
        assert_eq!(paths.xdg_runtime_dir(), env.path().join("runtime"));
        assert_eq!(
            paths.runtime_dir(),
            env.path()
                .join("runtime")
                .join(BuildProfile::current().app_dir_name())
        );

        for (key, suffix) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            env.set(key, "");
            let paths = AppPaths::resolve().expect("an empty XDG base reads as unset");
            let expected = env.home().join(suffix).join(SHARED_APP_DIR_NAME);
            let actual = if key == "XDG_CONFIG_HOME" {
                paths.config_dir()
            } else {
                paths.state_dir()
            };
            assert_eq!(actual, expected, "{key} empty");
            for refused in ["relative/path", " /padded"] {
                env.set(key, refused);
                let errors = AppPaths::resolve().expect_err("an invalid XDG base is refused");
                assert!(
                    errors.messages().iter().any(|error| error.contains(key)),
                    "{key}={refused:?}: {errors:?}"
                );
            }
            env.set(key, env.path().join(key));
            let paths = AppPaths::resolve().expect("absolute XDG base is accepted");
            let expected = env.path().join(key).join(SHARED_APP_DIR_NAME);
            let actual = if key == "XDG_CONFIG_HOME" {
                paths.config_dir()
            } else {
                paths.state_dir()
            };
            assert_eq!(actual, expected);
            env.remove(key);
        }

        env.set("XDG_RUNTIME_DIR", "relative/path");
        let errors = AppPaths::resolve().expect_err("a relative runtime dir is refused");
        assert!(
            errors
                .messages()
                .iter()
                .any(|error| error.contains("XDG_RUNTIME_DIR") && error.contains("relative path")),
            "{errors:?}"
        );
        env.remove("XDG_RUNTIME_DIR");
        for invalid in ["", "relative/home"] {
            env.set("HOME", invalid);
            assert!(AppPaths::resolve().is_err());
        }
        env.remove("HOME");
        assert!(AppPaths::resolve().is_err());
    }

    #[test]
    fn an_unset_runtime_dir_falls_back_only_to_a_private_logind_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = shepr_test_support::ScratchDir::new("logind-runtime");
        let user_dir = root.join(shepr_platform::effective_uid().to_string());
        let error = logind_runtime_dir(&root).expect_err("a missing directory is refused");
        assert!(
            error.to_string().contains("XDG_RUNTIME_DIR is not set"),
            "{error}"
        );

        std::fs::create_dir(&user_dir).expect("create user dir");
        std::fs::set_permissions(&user_dir, std::fs::Permissions::from_mode(0o755))
            .expect("open mode");
        let error = logind_runtime_dir(&root).expect_err("a shared mode is refused");
        assert!(error.to_string().contains("mode 0700"), "{error}");

        std::fs::set_permissions(&user_dir, std::fs::Permissions::from_mode(0o700))
            .expect("private mode");
        assert_eq!(
            logind_runtime_dir(&root).expect("a private directory is used"),
            user_dir
        );

        std::fs::remove_dir(&user_dir).expect("remove user dir");
        std::os::unix::fs::symlink(root.join("elsewhere"), &user_dir).expect("symlink");
        std::fs::create_dir(root.join("elsewhere")).expect("create target");
        std::fs::set_permissions(
            root.join("elsewhere"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("private target");
        logind_runtime_dir(&root).expect_err("a symlink is refused");
    }
}
