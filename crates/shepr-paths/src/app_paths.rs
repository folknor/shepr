use std::io;
use std::path::{Path, PathBuf};

use shepr_core::env::{EnvVar, SHARED_APP_DIR_NAME};

use crate::profile::{PaneMarker, PaneOwner};
use crate::{BuildProfile, PathsError, ServerAddress};

/// The lease file inside the data directory. The server locks it for as long as
/// it owns the directory (`shepr-mux`'s `DataDirLease`), and a stop waits for
/// its release. One name for both.
pub const DATA_DIR_LEASE_FILE_NAME: &str = "session.lock";

/// Paths and the local target resolved once at the process boundary and
/// passed to consumers. Production constructors reject unresolved path inputs
/// that would put files relative to the working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    config_dir: PathBuf,
    state_dir: PathBuf,
    data_dir: PathBuf,
    xdg_runtime_dir: PathBuf,
    runtime_dir: PathBuf,
    home_dir: Option<PathBuf>,
    current_dir: Option<PathBuf>,
    startup_cwd: Option<PathBuf>,
    server_address: ServerAddress,
}

impl AppPaths {
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// The state directory shared by every build profile. It holds the
    /// client-owned state; the saved layout and
    /// history live in [`data_dir`](Self::data_dir).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The directory of the saved layout, pane history, server log and the
    /// lease that keeps one server per directory. For a release build it is
    /// [`state_dir`](Self::state_dir) itself; a dev build gets a `shepr-dev`
    /// sibling of it.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The server log in this build profile's data directory.
    pub fn server_log(&self) -> PathBuf {
        shepr_platform::logging::server_log_path(self.data_dir())
    }

    /// The lease file inside [`data_dir`](Self::data_dir): the server that holds
    /// an exclusive lock on it owns the directory. It is never removed, so every
    /// contender locks the same inode.
    pub fn data_dir_lease_path(&self) -> PathBuf {
        self.data_dir.join(DATA_DIR_LEASE_FILE_NAME)
    }

    /// The client-owned state directory beneath the shared application state
    /// directory. Shared by every build profile.
    pub fn client_state_dir(&self) -> PathBuf {
        self.state_dir.join("client")
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

    pub fn home_dir(&self) -> Option<&Path> {
        self.home_dir.as_deref()
    }

    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// Last-resort server cwd, captured at launch without later filesystem IO.
    pub fn fallback_cwd(&self) -> &Path {
        self.current_dir().unwrap_or_else(|| Path::new("/"))
    }

    /// The absolute `SHEPR_STARTUP_CWD` handed to the server, when present.
    /// The server's own working directory is not a startup handoff.
    pub fn startup_cwd(&self) -> Option<&Path> {
        self.startup_cwd.as_deref()
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
    /// environment is not read and no directory is checked or created, so the
    /// caller passes absolute paths; this is how a caller that is not a
    /// launch, which resolves from the environment, places a config somewhere
    /// it chose. The one check is the server socket: a root too long for
    /// `runtime/shepr.sock` to name a Unix socket is refused.
    pub fn rooted_at(
        root: &Path,
        home_dir: Option<&Path>,
        current_dir: Option<&Path>,
    ) -> io::Result<Self> {
        Ok(Self {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            data_dir: root.join("state"),
            xdg_runtime_dir: root.to_path_buf(),
            runtime_dir: root.join("runtime"),
            home_dir: home_dir.map(Path::to_path_buf),
            current_dir: current_dir.map(Path::to_path_buf),
            startup_cwd: None,
            server_address: ServerAddress::for_runtime_dir(&root.join("runtime"), None)?,
        })
    }
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

fn resolve_current_dir(
    origin: CurrentDirOrigin,
) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    let process = || std::env::current_dir().ok();
    match origin {
        CurrentDirOrigin::Process => Ok((process(), None)),
        CurrentDirOrigin::StartupHandoff => {
            match shepr_core::env::read_path(EnvVar::SheprStartupCwd) {
                Ok(Some(path)) if path.is_absolute() => Ok((Some(path.clone()), Some(path))),
                Ok(Some(path)) => Err(format!(
                    "{} must be an absolute path, got {}",
                    EnvVar::SheprStartupCwd,
                    path.display()
                )),
                Ok(None) => Ok((process(), None)),
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
    let (current_dir, startup_cwd) =
        resolve_current_dir(current_dir_origin).map_err(PathsError::one)?;
    let read_base = |variable| {
        if variable == EnvVar::Home {
            Ok(Some(home_dir.clone()))
        } else {
            shepr_core::env::read_path(variable).map_err(io::Error::from)
        }
    };
    let config_dir =
        shepr_core::env::xdg_config_home_with(read_base).map(|path| path.join(SHARED_APP_DIR_NAME));
    let state_dir =
        shepr_core::env::xdg_state_home_with(read_base).map(|path| path.join(SHARED_APP_DIR_NAME));
    // XDG_RUNTIME_DIR has no base-directory fallback in the XDG spec. Unset
    // and empty are an error for shepr because its runtime sockets need a
    // user-private runtime directory; a relative value is refused by the
    // environment policy.
    let xdg_runtime_dir = shepr_core::env::read_path(EnvVar::XdgRuntimeDir);
    let runtime_dir = match &xdg_runtime_dir {
        Ok(Some(path)) => Ok(path.join(profile.app_dir_name())),
        Ok(None) => Err(io::Error::other(
            "XDG_RUNTIME_DIR must be set to an absolute path",
        )),
        Err(error) => Err(io::Error::other(error.to_string())),
    };
    let xdg_runtime_dir = xdg_runtime_dir.ok().flatten();

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
            Ok(AppPaths {
                config_dir,
                state_dir,
                data_dir,
                xdg_runtime_dir,
                runtime_dir,
                home_dir: Some(home_dir),
                current_dir,
                startup_cwd,
                server_address,
            })
        }
        _ if problems.is_empty() => Err(PathsError::one(
            "paths could not be resolved; no path-specific error was reported",
        )),
        _ => Err(PathsError::new(problems)),
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
        assert_eq!(paths.current_dir(), process_dir.as_deref());
        assert_eq!(paths.startup_cwd(), None);

        env.set(EnvVar::SheprStartupCwd, &launch);
        let paths = AppPaths::resolve_for_server().expect("server paths resolve");
        assert_eq!(paths.current_dir(), Some(launch.as_path()));
        assert_eq!(paths.startup_cwd(), Some(launch.as_path()));
        // Only the server reads the handoff; any other process keeps its own.
        let cli = AppPaths::resolve().expect("CLI paths resolve");
        assert_eq!(cli.current_dir(), process_dir.as_deref());
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
        assert_eq!(release.runtime_dir(), runtime.join("shepr"));
        assert_eq!(
            release.server_address().socket(),
            runtime.join("shepr/shepr.sock")
        );

        // Dev: its own runtime and saved layout, distinct sockets.
        assert_eq!(dev.data_dir(), state.join("shepr-dev"));
        assert_eq!(dev.runtime_dir(), runtime.join("shepr-dev"));
        assert_eq!(
            dev.server_address().socket(),
            runtime.join("shepr-dev/shepr.sock")
        );

        // Both config files, the shared state directory (with the client state
        // below it) and the XDG runtime root are the same in both profiles.
        assert_eq!(release.config_dir(), dev.config_dir());
        assert_eq!(release.client_config_file(), dev.client_config_file());
        assert_eq!(release.server_config_file(), dev.server_config_file());
        assert_eq!(release.state_dir(), dev.state_dir());
        assert_eq!(release.client_state_dir(), dev.client_state_dir());
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
            assert_eq!(paths.server_address().socket(), runtime.join("shepr.sock"));
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

        for (invalid, expected) in [
            ("", "XDG_RUNTIME_DIR must be set"),
            ("relative/path", "relative path"),
        ] {
            env.set("XDG_RUNTIME_DIR", invalid);
            let errors = AppPaths::resolve().expect_err("runtime dir has no XDG default");
            assert!(
                errors
                    .messages()
                    .iter()
                    .any(|error| error.contains("XDG_RUNTIME_DIR") && error.contains(expected)),
                "XDG_RUNTIME_DIR={invalid:?}: {errors:?}"
            );
        }
        env.remove("XDG_RUNTIME_DIR");
        assert!(AppPaths::resolve().is_err());
        for invalid in ["", "relative/home"] {
            env.set("HOME", invalid);
            assert!(AppPaths::resolve().is_err());
        }
        env.remove("HOME");
        assert!(AppPaths::resolve().is_err());
    }
}
