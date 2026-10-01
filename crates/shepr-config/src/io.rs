use std::io;
use std::path::{Path, PathBuf};

use shepr_core::env::EnvVar;

use super::validated::{
    ClientConfigResolution, ServerConfigResolution, ValidatedClientValues, ValidatedServerValues,
};
use super::{
    ClientConfig, ConfigDiagnostic, ConfigProvenance, ServerConfig, ValidatedClientConfig,
    ValidatedServerConfig,
};

include!(concat!(env!("OUT_DIR"), "/build_profile.rs"));

/// The directory name shepr uses under an XDG base directory for state that
/// every build shares: the config and the client-owned state in `client/`
/// below the state directory of this name.
const SHARED_APP_DIR_NAME: &str = "shepr";

/// The lease file inside the data directory. The server locks it for as long as
/// it owns the directory (`shepr-mux`'s `DataDirLease`), and a stop waits for
/// its release. One name for both.
pub const DATA_DIR_LEASE_FILE_NAME: &str = "session.lock";

/// The build profile a binary was compiled with, which decides where it keeps
/// its runtime sockets and its saved layout and history.
///
/// A release build uses the default XDG locations. Every other build (the
/// cargo dev profile) uses `shepr-dev` in place of `shepr` for the runtime
/// directory and the saved-layout directory, so a dev server and the installed
/// release server hold different sockets, locks and saved layouts without any
/// flag. Both config files and the client-owned state stay shared by every
/// profile.
/// A server of another build is still refused by the build-identity checks,
/// which is what tells the two apart once they can no longer collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProfile {
    Release,
    Dev,
}

impl BuildProfile {
    /// The profile this crate was built with, from cargo's `PROFILE`.
    pub const fn current() -> Self {
        Self::from_cargo_profile(BUILD_PROFILE)
    }

    /// `release` (and any profile inheriting from it) is [`Release`](Self::Release);
    /// everything else is [`Dev`](Self::Dev).
    pub(crate) const fn from_cargo_profile(profile: &str) -> Self {
        // A const fn cannot compare strs with `==`.
        match profile.as_bytes() {
            b"release" => Self::Release,
            _ => Self::Dev,
        }
    }

    /// The value a pane exports as `SHEPR_BUILD_PROFILE` to name the profile of
    /// the server that owns it.
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Dev => "dev",
        }
    }

    fn from_marker(value: &str) -> Option<Self> {
        match value {
            "release" => Some(Self::Release),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }

    /// The directory name this profile uses under the XDG runtime directory
    /// and beside the shared state directory.
    pub const fn app_dir_name(self) -> &'static str {
        match self {
            Self::Release => SHARED_APP_DIR_NAME,
            Self::Dev => "shepr-dev",
        }
    }
}

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
    server_address: super::ServerAddress,
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

    pub fn server_address(&self) -> &super::ServerAddress {
        &self.server_address
    }

    /// Resolve XDG directories and the local socket target once from the
    /// inherited process environment, for this build's profile.
    pub fn resolve() -> Result<Self, Vec<String>> {
        resolve_paths_from_env(BuildProfile::current(), CurrentDirOrigin::Process)
    }

    /// Resolve paths for the headless server process. The server daemon runs
    /// in the home directory so it never pins the directory it was launched
    /// from, but its current directory is still the one the user launched
    /// `shepr` from: the spawning client hands that over as
    /// `SHEPR_STARTUP_CWD`, and it is what `new_terminal_cwd = "current"`, a
    /// relative `new_terminal_cwd` and the new-terminal fallback resolve
    /// against. A server started without the handoff (by hand, from a shell)
    /// uses its own working directory.
    pub fn resolve_for_server() -> Result<Self, Vec<String>> {
        resolve_paths_from_env(BuildProfile::current(), CurrentDirOrigin::StartupHandoff)
    }

    /// Paths laid out under one directory: `config`, `state` and `runtime`
    /// below `root`, with `root` as the XDG runtime directory, the saved
    /// layout in the state directory (the release profile's layout, whatever
    /// profile built the caller) and every value's source the default. Nothing
    /// is resolved or checked, so the caller passes absolute paths; this is
    /// how a caller that is not a launch, which resolves from the
    /// environment, places a config somewhere it chose.
    pub fn rooted_at(root: &Path, home_dir: Option<&Path>, current_dir: Option<&Path>) -> Self {
        Self {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            data_dir: root.join("state"),
            xdg_runtime_dir: root.to_path_buf(),
            runtime_dir: root.join("runtime"),
            home_dir: home_dir.map(Path::to_path_buf),
            current_dir: current_dir.map(Path::to_path_buf),
            server_address: super::ServerAddress::resolve_paths(&root.join("runtime"), None, None),
        }
    }
}

/// An XDG base directory for shepr, with `app_dir` appended. Unset or empty falls back under `HOME`;
/// a relative, padded or non-UTF-8 value is refused rather than ignored, so a
/// mistyped variable fails the launch instead of silently moving shepr's
/// config or state back under `HOME`.
fn platform_xdg_dir(
    variable: EnvVar,
    home_suffix: &str,
    home_dir: Option<&Path>,
    app_dir: &str,
) -> io::Result<PathBuf> {
    // Empty follows the XDG Base Directory spec, which treats it as unset
    // (the registry reads empty as unset). Relative is refused, which the
    // spec also calls invalid.
    if let Some(directory) = shepr_core::env::read_path(variable)? {
        return Ok(directory.join(app_dir));
    }

    let home_dir = home_dir.ok_or_else(shepr_core::pathutil::missing_home_error)?;
    Ok(home_dir.join(home_suffix).join(app_dir))
}

fn socket_path_override(variable: EnvVar, diagnostics: &mut Vec<String>) -> Option<PathBuf> {
    shepr_core::env::read_path(variable).unwrap_or_else(|error| {
        diagnostics.push(error.to_string());
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

fn resolve_current_dir(origin: CurrentDirOrigin) -> Result<Option<PathBuf>, String> {
    let process = || std::env::current_dir().ok();
    match origin {
        CurrentDirOrigin::Process => Ok(process()),
        CurrentDirOrigin::StartupHandoff => {
            match shepr_core::env::read_path(EnvVar::SheprStartupCwd) {
                Ok(Some(path)) if path.is_absolute() => Ok(Some(path)),
                Ok(Some(path)) => Err(format!(
                    "{} must be an absolute path, got {}",
                    EnvVar::SheprStartupCwd,
                    path.display()
                )),
                Ok(None) => Ok(process()),
                Err(error) => Err(error.to_string()),
            }
        }
    }
}

fn resolve_paths_from_env(
    profile: BuildProfile,
    current_dir_origin: CurrentDirOrigin,
) -> Result<AppPaths, Vec<String>> {
    let mut target_env_diagnostics = Vec::new();
    let mut api_socket_override =
        socket_path_override(EnvVar::SheprSocketPath, &mut target_env_diagnostics);
    let mut client_socket_override =
        socket_path_override(EnvVar::SheprClientSocketPath, &mut target_env_diagnostics);
    // A pane names the profile of the server that owns it next to the socket
    // variables it exports. A process of another profile started in that pane
    // would otherwise follow them to the wrong server, so it drops them. With
    // no marker the variables came from a user or a script and apply as given.
    match shepr_core::env::read_text(EnvVar::SheprBuildProfile) {
        Ok(Some(marker)) => match BuildProfile::from_marker(&marker) {
            Some(owner) if owner != profile => {
                api_socket_override = None;
                client_socket_override = None;
            }
            Some(_) => {}
            None => target_env_diagnostics.push(format!(
                "{} must be `release` or `dev`, got `{marker}`",
                EnvVar::SheprBuildProfile
            )),
        },
        Ok(None) => {}
        Err(error) => target_env_diagnostics.push(error.to_string()),
    }
    if !target_env_diagnostics.is_empty() {
        return Err(target_env_diagnostics);
    }

    let home_dir = shepr_core::pathutil::home_dir().map_err(|error| vec![error.to_string()])?;
    let current_dir = resolve_current_dir(current_dir_origin).map_err(|error| vec![error])?;
    let config_dir = platform_xdg_dir(
        EnvVar::XdgConfigHome,
        ".config",
        Some(&home_dir),
        SHARED_APP_DIR_NAME,
    );
    let state_dir = platform_xdg_dir(
        EnvVar::XdgStateHome,
        ".local/state",
        Some(&home_dir),
        SHARED_APP_DIR_NAME,
    );
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

    let mut diagnostics = Vec::new();
    let config_dir = match config_dir {
        Ok(path) => Some(path),
        Err(error) => {
            diagnostics.push(format!("config directory error: {error}"));
            None
        }
    };

    let state_dir = match state_dir {
        Ok(path) => Some(path),
        Err(error) => {
            diagnostics.push(format!("state directory error: {error}"));
            None
        }
    };
    let runtime_dir = match runtime_dir {
        Ok(path) => Some(path),
        Err(error) => {
            diagnostics.push(format!("runtime directory error: {error}"));
            None
        }
    };

    match (config_dir, state_dir, xdg_runtime_dir, runtime_dir) {
        (Some(config_dir), Some(state_dir), Some(xdg_runtime_dir), Some(runtime_dir))
            if diagnostics.is_empty() =>
        {
            let server_address = super::ServerAddress::resolve_paths(
                &runtime_dir,
                api_socket_override.as_deref(),
                client_socket_override.as_deref(),
            );
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
                server_address,
            })
        }
        _ if diagnostics.is_empty() => Err(vec![
            "paths could not be resolved; no path-specific error was reported".to_owned(),
        ]),
        _ => Err(diagnostics),
    }
}

/// Normalize UTF-8 byte-order marks in config text.
///
/// TOML tolerates a single BOM at the very start of the document, but a BOM at
/// the start of a later line makes the parser reject the whole file. A
/// line-oriented edit can displace a leading BOM into the middle of the file,
/// so drop line-start BOMs that the TOML parser actually rejects. A U+FEFF that
/// is valid string data is kept, because its parse error would not point at it.
fn normalize_utf8_bom(content: &str) -> String {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    if !content.contains('\u{feff}') {
        return content.to_owned();
    }

    let mut normalized = content.to_owned();
    // `toml::Table`, not `toml::Value`: since toml 0.9, `Value::from_str`
    // parses a single value expression rather than a document.
    while let Err(error) = normalized.parse::<toml::Table>() {
        let Some(span) = error.span() else {
            break;
        };
        // toml reads a line-start BOM as the start of a bare key and reports
        // the error just past it ("key with no value"), so look for a BOM at
        // the start of the error's line rather than under the span.
        let bom_len = '\u{feff}'.len_utf8();
        let Some(before) = normalized.get(..span.start) else {
            break;
        };
        let bom_start = before.rfind('\n').map_or(0, |newline| newline + 1);
        if span.start > bom_start + bom_len
            || !normalized
                .get(bom_start..)
                .is_some_and(|line| line.starts_with('\u{feff}'))
        {
            break;
        }
        normalized.replace_range(bom_start..bom_start + bom_len, "");
    }
    normalized
}

fn read_optional_config(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(normalize_utf8_bom(&content))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

trait ConfigResolution {
    type Values;

    fn append_diagnostics(&self, diagnostics: &mut Vec<ConfigDiagnostic>);
    fn into_values(self) -> Option<Self::Values>;
}

impl ConfigResolution for ClientConfigResolution {
    type Values = ValidatedClientValues;

    fn append_diagnostics(&self, diagnostics: &mut Vec<ConfigDiagnostic>) {
        diagnostics.extend(
            self.diagnostics
                .iter()
                .cloned()
                .map(ConfigDiagnostic::Validation),
        );
    }

    fn into_values(self) -> Option<Self::Values> {
        self.values
    }
}

impl ConfigResolution for ServerConfigResolution {
    type Values = ValidatedServerValues;

    fn append_diagnostics(&self, diagnostics: &mut Vec<ConfigDiagnostic>) {
        diagnostics.extend(
            self.diagnostics
                .iter()
                .cloned()
                .map(ConfigDiagnostic::Validation),
        );
        diagnostics.extend(
            self.path_diagnostics
                .iter()
                .cloned()
                .map(ConfigDiagnostic::Path),
        );
    }

    fn into_values(self) -> Option<Self::Values> {
        self.values
    }
}

#[derive(Debug)]
struct LoadedConfig<C, R> {
    config: C,
    provenance: ConfigProvenance,
    resolution: Option<R>,
    diagnostics: Vec<ConfigDiagnostic>,
}

impl<C: Default, R: ConfigResolution> LoadedConfig<C, R> {
    fn failed(diagnostics: Vec<ConfigDiagnostic>) -> Self {
        Self {
            config: C::default(),
            provenance: ConfigProvenance::defaults(),
            resolution: None,
            diagnostics,
        }
    }

    fn into_validated_with<V>(
        self,
        paths: AppPaths,
        construct: impl FnOnce(C, ConfigProvenance, R::Values, AppPaths) -> V,
    ) -> Result<V, Vec<ConfigDiagnostic>> {
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }
        match self.resolution.and_then(ConfigResolution::into_values) {
            Some(values) => Ok(construct(self.config, self.provenance, values, paths)),
            None => Err(vec![ConfigDiagnostic::Validation(
                "configuration resolution produced no values and no diagnostic".to_owned(),
            )]),
        }
    }
}

impl LoadedConfig<ClientConfig, ClientConfigResolution> {
    fn into_validated(
        self,
        paths: AppPaths,
    ) -> Result<ValidatedClientConfig, Vec<ConfigDiagnostic>> {
        self.into_validated_with(paths, ValidatedClientConfig::from_loaded)
    }
}

impl LoadedConfig<ServerConfig, ServerConfigResolution> {
    fn into_validated(
        self,
        paths: AppPaths,
    ) -> Result<ValidatedServerConfig, Vec<ConfigDiagnostic>> {
        self.into_validated_with(paths, |config, _provenance, values, paths| {
            ValidatedServerConfig::from_loaded(config, values, paths)
        })
    }
}

fn resolve_client_config(
    config: &ClientConfig,
    provenance: &ConfigProvenance,
    _paths: &AppPaths,
) -> ClientConfigResolution {
    ClientConfigResolution::parse(config, provenance)
}

fn resolve_server_config(
    config: &ServerConfig,
    _provenance: &ConfigProvenance,
    paths: &AppPaths,
) -> ServerConfigResolution {
    ServerConfigResolution::parse(config, paths)
}

fn load_config_from_path<C, R>(
    path: &Path,
    paths: &AppPaths,
    resolve: impl Fn(&C, &ConfigProvenance, &AppPaths) -> R,
) -> LoadedConfig<C, R>
where
    C: Default + serde::de::DeserializeOwned,
    R: ConfigResolution,
{
    match read_optional_config(path) {
        Ok(Some(content)) => load_config_from_str(&content, paths, resolve),
        Ok(None) => {
            let config = C::default();
            let provenance = ConfigProvenance::from_document(None);
            let resolution = resolve(&config, &provenance, paths);
            let mut diagnostics = Vec::new();
            resolution.append_diagnostics(&mut diagnostics);
            LoadedConfig {
                config,
                provenance,
                resolution: Some(resolution),
                diagnostics,
            }
        }
        Err(error) => LoadedConfig::failed(vec![ConfigDiagnostic::Read(error.to_string())]),
    }
}

fn load_config_from_str<C, R>(
    content: &str,
    paths: &AppPaths,
    resolve: impl Fn(&C, &ConfigProvenance, &AppPaths) -> R,
) -> LoadedConfig<C, R>
where
    C: Default + serde::de::DeserializeOwned,
    R: ConfigResolution,
{
    let table = match content.parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => {
            return LoadedConfig::failed(vec![ConfigDiagnostic::Parse(error.to_string())]);
        }
    };
    let document = toml::Value::Table(table);
    let (config, ignored_keys) = match deserialize_with_ignored::<C, _>(document.clone()) {
        Ok(config) => config,
        Err(error) => {
            return LoadedConfig::failed(vec![ConfigDiagnostic::Parse(error.to_string())]);
        }
    };
    let provenance = ConfigProvenance::from_document(Some(&document));
    let resolution = resolve(&config, &provenance, paths);
    let (unknown_sections, unknown_diagnostics) =
        unknown_top_level_sections(&document, &ignored_keys);
    let mut diagnostics = unknown_diagnostics
        .into_iter()
        .map(ConfigDiagnostic::Unknown)
        .collect::<Vec<_>>();
    diagnostics.extend(
        unknown_config_key_diagnostics(
            ignored_keys
                .into_iter()
                .filter(|path| {
                    !matches!(path.as_slice(), [ConfigKeyPathSegment::Key(key)] if unknown_sections.contains(key))
                })
                .collect(),
        )
        .into_iter()
        .map(ConfigDiagnostic::Unknown),
    );
    resolution.append_diagnostics(&mut diagnostics);
    LoadedConfig {
        config,
        provenance,
        resolution: Some(resolution),
        diagnostics,
    }
}

pub fn load_client_validated(
    paths: &AppPaths,
) -> Result<ValidatedClientConfig, Vec<ConfigDiagnostic>> {
    let path = paths.client_config_file();
    load_config_from_path(&path, paths, resolve_client_config)
        .into_validated(paths.clone())
        .map_err(|diagnostics| {
            diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.with_file(&path))
                .collect()
        })
}

pub fn load_server_validated(
    paths: &AppPaths,
) -> Result<ValidatedServerConfig, Vec<ConfigDiagnostic>> {
    let path = paths.server_config_file();
    load_config_from_path(&path, paths, resolve_server_config)
        .into_validated(paths.clone())
        .map_err(|diagnostics| {
            diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.with_file(&path))
                .collect()
        })
}

fn unknown_top_level_sections(
    document: &toml::Value,
    ignored_paths: &[Vec<ConfigKeyPathSegment>],
) -> (std::collections::BTreeSet<String>, Vec<String>) {
    let Some(table) = document.as_table() else {
        return (std::collections::BTreeSet::new(), Vec::new());
    };
    let mut keys = Vec::new();
    let mut diagnostics = Vec::new();
    for path in ignored_paths {
        let [ConfigKeyPathSegment::Key(key)] = path.as_slice() else {
            continue;
        };
        let Some(value) = table.get(key) else {
            continue;
        };
        if let Some(diagnostic) = unknown_top_level_section_diagnostic(key, value) {
            keys.push(key.clone());
            diagnostics.push(diagnostic);
        }
    }
    (keys.into_iter().collect(), diagnostics)
}

fn unknown_top_level_section_diagnostic(key: &str, value: &toml::Value) -> Option<String> {
    let header = if value.is_table() {
        format!("[{key}]")
    } else if value
        .as_array()
        .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
    {
        format!("[[{key}]]")
    } else {
        return None;
    };

    Some(format!("section {header}"))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ConfigKeyPathSegment {
    Key(String),
    Index(usize),
}

fn config_key_path(path: &serde_ignored::Path<'_>) -> Vec<ConfigKeyPathSegment> {
    fn visit(path: &serde_ignored::Path<'_>, segments: &mut Vec<ConfigKeyPathSegment>) {
        match path {
            serde_ignored::Path::Root => {}
            serde_ignored::Path::Seq { parent, index } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Index(*index));
            }
            serde_ignored::Path::Map { parent, key } => {
                visit(parent, segments);
                segments.push(ConfigKeyPathSegment::Key(key.clone()));
            }
            serde_ignored::Path::Some { parent }
            | serde_ignored::Path::NewtypeStruct { parent }
            | serde_ignored::Path::NewtypeVariant { parent } => visit(parent, segments),
        }
    }

    let mut segments = Vec::new();
    visit(path, &mut segments);
    segments
}

fn format_config_key_path(path: &[ConfigKeyPathSegment]) -> String {
    path.iter()
        .map(|segment| match segment {
            ConfigKeyPathSegment::Key(key)
                if !key.is_empty()
                    && key.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                    }) =>
            {
                key.clone()
            }
            ConfigKeyPathSegment::Key(key) => toml::Value::String(key.clone()).to_string(),
            ConfigKeyPathSegment::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn unknown_config_key_diagnostics(mut paths: Vec<Vec<ConfigKeyPathSegment>>) -> Vec<String> {
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| format!("key {}", format_config_key_path(&path)))
        .collect()
}

fn deserialize_with_ignored<'de, T, D>(
    deserializer: D,
) -> Result<(T, Vec<Vec<ConfigKeyPathSegment>>), D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let mut ignored = Vec::new();
    let value = serde_ignored::deserialize(deserializer, |path| {
        ignored.push(config_key_path(&path));
    })?;
    Ok((value, ignored))
}

#[cfg(test)]
type LoadedClientConfig = LoadedConfig<ClientConfig, ClientConfigResolution>;
#[cfg(test)]
type LoadedServerConfig = LoadedConfig<ServerConfig, ServerConfigResolution>;

#[cfg(test)]
impl ClientConfig {
    fn load_from_path(path: &Path) -> LoadedClientConfig {
        load_config_from_path(path, &AppPaths::default(), resolve_client_config)
    }

    fn load_from_str(content: &str) -> LoadedClientConfig {
        load_config_from_str(content, &AppPaths::default(), resolve_client_config)
    }
}

#[cfg(test)]
impl ServerConfig {
    fn load_from_path(path: &Path) -> LoadedServerConfig {
        load_config_from_path(path, &AppPaths::default(), resolve_server_config)
    }

    fn load_from_str(content: &str) -> LoadedServerConfig {
        load_config_from_str(content, &AppPaths::default(), resolve_server_config)
    }
}

/// Absolute like resolved launch paths, and identical across calls, so two
/// test configs compare equal.
/// The root cannot be created by an unprivileged user: a test that writes
/// through these paths fails instead of leaving files in a shared location.
#[cfg(test)]
impl Default for AppPaths {
    fn default() -> Self {
        let root = Path::new("/nonexistent/shepr-test-config");
        Self::rooted_at(root, Some(root), None)
    }
}

#[cfg(test)]
impl AppPaths {
    pub fn test_at(root: &Path) -> Self {
        Self::rooted_at(root, None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misplaced_settings_and_retired_settings_fail_only_the_owning_launch() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for source in [
            "[terminal]\ndefault_shell = '/bin/sh'\n",
            "[session]\nresume_agents_on_restore = true\n",
            "[server]\nheadless_cols = 120\n",
            "[advanced]\nscrollback_limit_bytes = 1000\n",
            "[experimental]\npane_history = true\n",
            "[experimental]\nreveal_hidden_cursor_for_cjk_ime = true\n",
            "[experimental]\ncjk_ime_agents = ['codex']\n",
            "[experimental]\ncjk_ime_cursor_shape = 'bar'\n",
            "[ui]\npane_borders = 'always'\n",
            "[ui]\npane_outer_borders = true\n",
            "[ui]\npane_scrollbars = true\n",
            "[ui]\npane_gaps = true\n",
            "[ui]\nshow_agent_labels_on_pane_borders = true\n",
            "[ui]\nwindow_title = '{hostname}'\n",
        ] {
            assert!(
                ServerConfig::load_from_str(source)
                    .into_validated(AppPaths::default())
                    .is_ok(),
                "{source}"
            );
            let errors = ClientConfig::load_from_str(source)
                .into_validated(AppPaths::default())
                .expect_err("server setting in client file");
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, ConfigDiagnostic::Unknown(_))),
                "{source}: {errors:?}"
            );
        }
        for source in [
            "[[machines]]\nlabel = 'build'\nssh = 'build'\n",
            "[keys]\nprefix = 'ctrl+b'\n",
            "[ui]\nmouse_capture = false\n",
            "[ui]\nsidebar_width = 26\n",
            "[ui.sidebar.spaces]\nrows = [['workspace']]\n",
        ] {
            assert!(
                ClientConfig::load_from_str(source)
                    .into_validated(AppPaths::default())
                    .is_ok(),
                "{source}"
            );
            let errors = ServerConfig::load_from_str(source)
                .into_validated(AppPaths::default())
                .expect_err("client setting in server file");
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, ConfigDiagnostic::Unknown(_))),
                "{source}: {errors:?}"
            );
        }
        for source in [
            "[experimental]\nallow_nested = true\n",
            "[ui]\naccent = 'cyan'\n",
        ] {
            assert!(
                ClientConfig::load_from_str(source)
                    .into_validated(AppPaths::default())
                    .is_err()
            );
            assert!(
                ServerConfig::load_from_str(source)
                    .into_validated(AppPaths::default())
                    .is_err()
            );
        }
    }

    #[test]
    fn each_program_reads_only_its_file_and_never_the_retired_file() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("role-config-load");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()));
        std::fs::create_dir_all(paths.config_dir()).expect("create config directory");
        std::fs::write(paths.config_dir().join("config.toml"), "broken = [")
            .expect("retired file fixture");
        std::fs::write(paths.server_config_file(), "broken = [").expect("broken server fixture");
        env.set(EnvVar::Shell, scratch.join("missing/wrapper"));
        assert!(
            load_client_validated(&paths).is_ok(),
            "client must not read server config or SHELL"
        );
        let errors = load_server_validated(&paths).expect_err("server parses its file");
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("server.toml"))
        );
        assert!(errors.iter().all(|error| {
            error
                .to_string()
                .lines()
                .next()
                .is_some_and(|line| line.contains("server.toml"))
        }));
        std::fs::write(
            paths.server_config_file(),
            "[terminal]\ndefault_shell = '/bin/sh'\n",
        )
        .expect("valid server fixture");
        std::fs::write(paths.client_config_file(), "broken = [").expect("broken client fixture");
        assert!(
            load_server_validated(&paths).is_ok(),
            "server must not read client config"
        );
        let errors = load_client_validated(&paths).expect_err("client parses its file");
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("client.toml"))
        );
        assert!(errors.iter().all(|error| {
            error
                .to_string()
                .lines()
                .next()
                .is_some_and(|line| line.contains("client.toml"))
        }));
        std::fs::remove_file(paths.client_config_file()).expect("remove client fixture");
        assert!(load_client_validated(&paths).is_ok());
    }

    #[test]
    fn server_launch_collects_chrome_grid_and_terminal_errors() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let errors = ServerConfig::load_from_str("[server]\nheadless_cols = 0\n[ui]\nwindow_title = '{unknown}'\n[terminal]\ndefault_shell = '/missing/zsh'\nnew_cwd = 'missing'\n")
            .into_validated(AppPaths::default()).expect_err("invalid server settings");
        for setting in [
            "server.headless_cols",
            "ui.window_title",
            "terminal.default_shell",
            "terminal.new_cwd",
        ] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.to_string().contains(setting)),
                "{setting}: {errors:?}"
            );
        }
    }

    #[test]
    fn load_diagnostics_keep_their_kind() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let parse = ClientConfig::load_from_str("[keys\nprefix = 'ctrl+a'");
        assert!(matches!(
            parse.diagnostics.as_slice(),
            [ConfigDiagnostic::Parse(_)]
        ));

        let unknown = ClientConfig::load_from_str("[keys]\nunknown_binding = 'ctrl+a'");
        assert!(matches!(
            unknown.diagnostics.as_slice(),
            [ConfigDiagnostic::Unknown(_)]
        ));

        let invalid = ClientConfig::load_from_str("[keys]\nprefix = 'ctrl+'");
        assert!(
            invalid
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic, ConfigDiagnostic::Validation(_)))
        );
    }

    #[test]
    fn failed_load_does_not_resolve_the_placeholder_config() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let loaded = ClientConfig::load_from_str("[broken");

        assert!(loaded.resolution.is_none());
        assert!(!loaded.diagnostics.is_empty());
    }

    #[test]
    fn config_load_reports_unreadable_path() {
        let _env = shepr_test_support::IsolatedEnv::new();
        // A directory where the config file should be cannot be read.
        let scratch = shepr_test_support::ScratchDir::new("config");
        let startup = ClientConfig::load_from_path(scratch.path());
        let server = ServerConfig::load_from_path(scratch.path());
        assert!(server.into_validated(AppPaths::default()).is_err());
        assert!(
            startup
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.to_string().contains("config read error"))
        );
    }

    #[test]
    fn validated_config_rejects_parse_and_semantic_errors() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for (content, message) in [
            ("[keys]\nprefix = \"ctrl+\"\n", "keys.prefix"),
            ("[keys]\nzoom = \"prefix+nonsense\"\n", "keys.zoom"),
            ("[theme]\nname = \"not-a-theme\"\n", "theme.name"),
            (
                "[theme.custom]\nred = \"not-a-color\"\n",
                "theme.custom.red",
            ),
            ("[theme]\naccent = \"not-a-color\"\n", "theme.accent"),
            ("[ui]\nwindow_title = \"{unknown}\"\n", "ui.window_title"),
            (
                "[ui]\nsidebar_min_width = 50\nsidebar_max_width = 30\n",
                "sidebar_min_width",
            ),
            ("[ui]\nsidebar_width = 17\n", "ui.sidebar_width"),
            ("[ui]\nsidebar_width = 37\n", "ui.sidebar_width"),
            ("[server]\nheadless_cols = 0\n", "headless_cols"),
            (
                "[server]\nheadless_cols = 4097\nheadless_rows = 1\n",
                "server.headless_cols",
            ),
            (
                "[server]\nheadless_cols = 4096\nheadless_rows = 1025\n",
                "server.headless_cols",
            ),
            (
                "[ui]\ntab_bar_right = []\n",
                "unknown config key ui.tab_bar_right",
            ),
            (
                "[keys]\nnext_tab = \"prefix+n\"\n",
                "unknown config key keys.next_tab",
            ),
            ("[ui]\nwindow_title = \"{tab}\"\n", "unknown token '{tab}'"),
            (
                "[ui]\nmouse_captur = true\n",
                "unknown config key ui.mouse_captur",
            ),
        ] {
            let errors = if content.contains("[server]") || content.contains("window_title") {
                ServerConfig::load_from_str(content)
                    .into_validated(AppPaths::default())
                    .expect_err("invalid server config")
            } else {
                ClientConfig::load_from_str(content)
                    .into_validated(AppPaths::default())
                    .expect_err("invalid client config")
            };
            assert!(
                errors
                    .iter()
                    .any(|diagnostic| diagnostic.to_string().contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }

        let parse_error = ServerConfig::load_from_str("[server]\nheadless_cols = \"wide\"\n");
        assert!(parse_error.into_validated(AppPaths::default()).is_err());
    }

    #[test]
    fn machines_load_in_config_order() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let loaded = ClientConfig::load_from_str(
            r#"
[[machines]]
label = "build"
ssh = "dev@build"

[[machines]]
label = "gpu"
ssh = "ssh://gpu.example"
"#,
        );
        let validated = loaded
            .into_validated(AppPaths::default())
            .expect("valid machines load");
        let machines: Vec<_> = validated
            .machines()
            .iter()
            .map(|machine| (machine.label.as_str(), machine.ssh.as_str()))
            .collect();
        assert_eq!(
            machines,
            [("build", "dev@build"), ("gpu", "ssh://gpu.example")]
        );

        let none = ClientConfig::load_from_str("")
            .into_validated(AppPaths::default())
            .expect("no machines is valid");
        assert!(none.machines().is_empty());
    }

    #[test]
    fn invalid_machines_fail_the_launch() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for (content, message) in [
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"h1\"\n[[machines]]\nlabel = \"a\"\nssh = \"h2\"\n",
                "duplicates machines[0]",
            ),
            (
                "[[machines]]\nlabel = \"  \"\nssh = \"h\"\n",
                "machine label must not be blank",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"-oProxyCommand=x\"\n",
                "must not start with",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"\"\n",
                "SSH target must not be empty",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"u:p@h\"\n",
                "must not contain a password",
            ),
            ("[[machines]]\nlabel = \"a\"\n", "ssh"),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"h\"\nhost = \"x\"\n",
                "unknown config key machines.0.host",
            ),
        ] {
            let errors = ClientConfig::load_from_str(content)
                .into_validated(AppPaths::default())
                .expect_err("invalid machines must not launch");
            assert!(
                errors
                    .iter()
                    .any(|diagnostic| diagnostic.to_string().contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }
    }

    #[test]
    fn config_check_collects_all_semantic_diagnostics() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-diagnostics");
        let paths = AppPaths::test_at(scratch.path());
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(
            paths.client_config_file(),
            r#"
[theme]
name = "not-a-theme"
[theme.custom]
red = "not-a-color"
[keys]
prefix = "ctrl+"
zoom = "prefix+not-a-key"
[ui]
sidebar_width = 80
sidebar_min_width = 18
sidebar_max_width = 36
"#,
        )
        .expect("write invalid config fixture");

        let diagnostics = load_client_validated(&paths).expect_err("invalid fixture is refused");
        let messages = diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for expected in [
            "theme.name",
            "theme.custom.red",
            "keys.prefix",
            "keys.zoom",
            "ui.sidebar_width",
        ] {
            assert!(
                messages.iter().any(|message| message.contains(expected)),
                "missing {expected:?} from {messages:?}"
            );
        }
    }

    #[test]
    fn load_validated_rejects_bad_config_file_and_accepts_missing_file() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-load");
        let paths = AppPaths::test_at(scratch.path());
        let path = paths.client_config_file();
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");

        std::fs::write(&path, "[ui]\nsidebar_width = 0\n").expect("write bad config fixture");
        assert!(load_client_validated(&paths).is_err());

        std::fs::remove_file(&path).expect("remove config fixture");
        let defaults = load_client_validated(&paths).expect("missing config uses defaults");
        assert!(defaults.validated_live_keybinds().is_ok());
        assert_eq!(defaults.palette(), &crate::theme::Palette::catppuccin());

        std::fs::write(
            &path,
            "[theme]\nname = \"nord\"\n[theme.custom]\naccent = \"#010203\"\n",
        )
        .expect("write valid themed config");
        let themed = load_client_validated(&paths).expect("valid theme loads");
        assert_eq!(themed.palette().accent, ratatui::style::Color::Rgb(1, 2, 3));
    }

    #[test]
    fn load_validated_rejects_home_cwd_without_absolute_home() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-cwd");
        // Launch paths without a home directory: what resolution captures
        // when HOME is missing or relative.
        let paths = AppPaths::test_at(scratch.path());
        assert!(paths.home_dir().is_none());
        let path = paths.server_config_file();
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(&path, "[terminal]\nnew_cwd = \"home\"\n").expect("write config fixture");

        let errors = load_server_validated(&paths).expect_err("home cwd needs absolute HOME");
        assert!(
            errors
                .iter()
                .any(|error| error.message().contains("terminal.new_cwd")),
            "{errors:?}"
        );
    }

    #[test]
    fn role_config_paths_use_the_xdg_config_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        let paths = AppPaths::resolve().expect("default paths resolve");
        let directory = env.home().join(".config").join(SHARED_APP_DIR_NAME);
        assert_eq!(paths.client_config_file(), directory.join("client.toml"));
        assert_eq!(paths.server_config_file(), directory.join("server.toml"));
    }

    #[test]
    fn server_current_dir_is_the_handed_over_launch_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        let launch = env.path().join("launch");
        std::fs::create_dir_all(launch.join("project")).expect("create launch directory");
        let process_dir = std::env::current_dir().ok();
        assert_ne!(process_dir.as_deref(), Some(launch.as_path()));

        // Without the handoff the server uses its own working directory.
        let paths = AppPaths::resolve_for_server().expect("server paths resolve");
        assert_eq!(paths.current_dir(), process_dir.as_deref());

        env.set(EnvVar::SheprStartupCwd, &launch);
        let paths = AppPaths::resolve_for_server().expect("server paths resolve");
        assert_eq!(paths.current_dir(), Some(launch.as_path()));
        // Only the server reads the handoff; any other process keeps its own.
        let cli = AppPaths::resolve().expect("CLI paths resolve");
        assert_eq!(cli.current_dir(), process_dir.as_deref());

        // `current` and a relative new_cwd resolve against the launch directory.
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(
            paths.server_config_file(),
            "[terminal]\nnew_cwd = \"project\"\n",
        )
        .expect("write config fixture");
        let config = load_server_validated(&paths).expect("relative new_cwd validates");
        assert_eq!(
            config.terminal().new_cwd,
            crate::NewTerminalCwd::Path(launch.join("project"))
        );
        std::fs::write(
            paths.server_config_file(),
            "[terminal]\nnew_cwd = \"current\"\n",
        )
        .expect("write config fixture");
        let config = load_server_validated(&paths).expect("current new_cwd validates");
        assert_eq!(config.terminal().new_cwd, crate::NewTerminalCwd::Current);
        assert_eq!(config.paths().current_dir(), Some(launch.as_path()));

        env.set(EnvVar::SheprStartupCwd, "relative/launch");
        let errors = AppPaths::resolve_for_server().expect_err("a relative handoff is refused");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("SHEPR_STARTUP_CWD") && error.contains("absolute")),
            "{errors:?}"
        );
    }

    #[test]
    fn socket_path_overrides_resolve_independently() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.remove(EnvVar::SheprSocketPath);
        env.remove(EnvVar::SheprClientSocketPath);

        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        let paths = AppPaths::resolve().expect("API socket override resolves");
        assert_eq!(
            paths.server_address().api_socket(),
            env.path().join("api.sock")
        );
        assert_eq!(
            paths.server_address().client_socket(),
            crate::derive_client_socket_from_api_socket(&env.path().join("api.sock"))
        );

        env.set(
            EnvVar::SheprClientSocketPath,
            env.path().join("ignored-client.sock"),
        );
        let paths = AppPaths::resolve().expect("API override keeps precedence when both are set");
        let expected_client =
            crate::derive_client_socket_from_api_socket(&env.path().join("api.sock"));
        assert_eq!(
            paths.server_address().client_socket(),
            expected_client.as_path()
        );

        env.remove(EnvVar::SheprSocketPath);
        env.set(
            EnvVar::SheprClientSocketPath,
            env.path().join("client.sock"),
        );
        let paths = AppPaths::resolve().expect("client socket override resolves");
        assert_eq!(
            paths.server_address().api_socket(),
            paths.runtime_dir().join("shepr.sock")
        );
        assert_eq!(
            paths.server_address().client_socket(),
            env.path().join("client.sock")
        );
    }

    #[test]
    fn invalid_socket_environment_fails_resolution() {
        let env = shepr_test_support::IsolatedEnv::new();
        for variable in [EnvVar::SheprSocketPath, EnvVar::SheprClientSocketPath] {
            for (value, expected) in [
                ("", "set but empty"),
                ("rel.sock", "absolute path"),
                (" /abs.sock", "whitespace"),
            ] {
                env.set(variable, value);
                let errors = AppPaths::resolve().expect_err("invalid socket override");
                assert!(
                    errors
                        .iter()
                        .any(|error| error.contains(variable.name()) && error.contains(expected)),
                    "{variable}={value:?}: {errors:?}"
                );
            }
            env.remove(variable);
        }
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
            release.server_address().api_socket(),
            runtime.join("shepr/shepr.sock")
        );
        assert_eq!(
            release.server_address().client_socket(),
            runtime.join("shepr/shepr-client.sock")
        );

        // Dev: its own runtime and saved layout, distinct sockets.
        assert_eq!(dev.data_dir(), state.join("shepr-dev"));
        assert_eq!(dev.runtime_dir(), runtime.join("shepr-dev"));
        assert_eq!(
            dev.server_address().api_socket(),
            runtime.join("shepr-dev/shepr.sock")
        );
        assert_eq!(
            dev.server_address().client_socket(),
            runtime.join("shepr-dev/shepr-client.sock")
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
    fn socket_overrides_beat_the_profile_runtime_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        for profile in [BuildProfile::Release, BuildProfile::Dev] {
            let paths = resolve_paths_from_env(profile, CurrentDirOrigin::Process)
                .expect("override resolves");
            assert_eq!(
                paths.server_address().api_socket(),
                env.path().join("api.sock")
            );
            assert_eq!(
                paths.runtime_dir().file_name(),
                Some(std::ffi::OsStr::new(profile.app_dir_name()))
            );
        }
    }

    #[test]
    fn socket_overrides_with_a_matching_marker_win() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        for profile in [BuildProfile::Release, BuildProfile::Dev] {
            env.set(EnvVar::SheprBuildProfile, profile.marker());
            let paths = resolve_paths_from_env(profile, CurrentDirOrigin::Process)
                .expect("override resolves");
            assert_eq!(
                paths.server_address().api_socket(),
                env.path().join("api.sock")
            );
        }
    }

    #[test]
    fn socket_overrides_with_another_profiles_marker_are_ignored() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(EnvVar::SheprSocketPath, env.path().join("api.sock"));
        env.set(
            EnvVar::SheprClientSocketPath,
            env.path().join("client.sock"),
        );
        for (profile, owner) in [
            (BuildProfile::Dev, BuildProfile::Release),
            (BuildProfile::Release, BuildProfile::Dev),
        ] {
            env.set(EnvVar::SheprBuildProfile, owner.marker());
            let paths =
                resolve_paths_from_env(profile, CurrentDirOrigin::Process).expect("paths resolve");
            let runtime = env.path().join("runtime").join(profile.app_dir_name());
            assert_eq!(
                paths.server_address().api_socket(),
                runtime.join("shepr.sock")
            );
            assert_eq!(
                paths.server_address().client_socket(),
                runtime.join("shepr-client.sock")
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
                .iter()
                .any(|error| error.contains("SHEPR_BUILD_PROFILE") && error.contains("staging")),
            "{errors:?}"
        );
    }

    #[test]
    fn cargo_profile_names_map_to_build_profiles() {
        assert_eq!(
            BuildProfile::from_cargo_profile("release"),
            BuildProfile::Release
        );
        assert_eq!(BuildProfile::from_cargo_profile("debug"), BuildProfile::Dev);
        assert_eq!(BuildProfile::from_cargo_profile(""), BuildProfile::Dev);
        assert_eq!(BuildProfile::Release.app_dir_name(), "shepr");
        assert_eq!(BuildProfile::Dev.app_dir_name(), "shepr-dev");
    }

    #[test]
    fn config_load_reports_unknown_keys_and_parses_known_siblings() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let loaded = ClientConfig::load_from_str(
            r##"
plugin = []

[theme.custom]
accentt = "#ffffff"

[keys]
zoom = "prefix+z"
new_workspacee = "prefix+t"

[ui]
mouse_capture = false
mouse_captur = true
"foo.bar" = true
"##,
        );

        assert_eq!(
            loaded
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec![
                "unknown config key keys.new_workspacee",
                "unknown config key plugin",
                "unknown config key theme.custom.accentt",
                "unknown config key ui.\"foo.bar\"",
                "unknown config key ui.mouse_captur",
            ]
        );
        assert!(!loaded.config.ui.mouse_capture);
    }

    #[test]
    fn config_load_records_provenance_for_ui_values() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let loaded = ClientConfig::load_from_str(
            r#"
[ui]
sidebar_width = 26
agent_panel_sort = "priority"
"#,
        );
        assert!(
            loaded
                .resolution
                .as_ref()
                .is_some_and(|resolution| resolution.values.is_some())
        );
        assert!(
            loaded
                .provenance
                .is_explicit(super::super::UiPreferenceKey::SidebarWidth)
        );
        assert!(
            loaded
                .provenance
                .is_explicit(super::super::UiPreferenceKey::AgentPanelSort)
        );
        assert!(
            !loaded
                .provenance
                .is_explicit(super::super::UiPreferenceKey::SidebarStartCollapsed)
        );

        let empty = ClientConfig::load_from_str("");
        assert!(
            !empty
                .provenance
                .is_explicit(super::super::UiPreferenceKey::SidebarWidth)
        );
    }

    #[test]
    fn config_provenance_queries_array_fields_by_their_parent_key() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let configured =
            ClientConfig::load_from_str("[keys]\nfocus_agent = [\"prefix+1\", \"prefix+2\"]\n");
        assert!(configured.provenance.key_is_configured("keys.focus_agent"));

        let defaults = ClientConfig::load_from_str("[keys]\n");
        assert!(!defaults.provenance.key_is_configured("keys.focus_agent"));
    }

    #[test]
    fn config_load_reports_unknown_top_level_sections() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let loaded = ClientConfig::load_from_str(
            r#"
[[plugin]]
id = "example"
"#,
        );

        assert_eq!(
            loaded
                .diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["unknown config section [[plugin]]"]
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
                    errors.iter().any(|error| error.contains(key)),
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

    #[test]
    fn normalize_utf8_bom_removes_a_leading_bom() {
        let content = "\u{feff}[terminal]\n";
        assert_eq!(normalize_utf8_bom(content), "[terminal]\n");
    }

    #[test]
    fn normalize_utf8_bom_recovers_from_a_displaced_mid_file_bom() {
        let content = "[ui]\n\u{feff}[terminal]\ndefault_shell = \"zsh\"\n";
        let normalized = normalize_utf8_bom(content);
        assert_eq!(normalized, "[ui]\n[terminal]\ndefault_shell = \"zsh\"\n");
        assert!(normalized.parse::<toml::Table>().is_ok());
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_basic_strings() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\n";
        assert!(content.parse::<toml::Table>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_literal_strings() {
        let content = "[theme]\nname = '''\nfirst\n\u{feff}second\n'''\n";
        assert!(content.parse::<toml::Table>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_string_boms_despite_other_errors() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\nbroken = \n";
        assert!(content.parse::<toml::Table>().is_err());
        assert_eq!(normalize_utf8_bom(content), content);
    }
}
