use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::GitReadError;
use super::discovery::{GitWorktreeInfo, canonicalize_best_effort_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BranchConfig {
    pub(super) remote: String,
    pub(super) merge_ref: String,
    fetch_refspecs: Vec<(String, String)>,
    remote_urls: Vec<(String, String)>,
}

type FileStamp = Option<(Option<SystemTime>, u64)>;
pub(super) type FileDep = (PathBuf, FileStamp, bool, Option<PathBuf>);

pub(super) fn stamp(path: PathBuf, target: Option<PathBuf>) -> FileDep {
    let meta = std::fs::metadata(&path);
    let reusable = meta.is_ok() || matches!(&meta, Err(e) if e.kind() == ErrorKind::NotFound);
    let stamp = meta.ok().map(|m| (m.modified().ok(), m.len()));
    (path, stamp, reusable, target)
}

pub(super) fn deps_current(deps: &[FileDep]) -> bool {
    deps.iter().all(|dep| {
        let target = dep
            .3
            .as_ref()
            .map(|_| canonicalize_best_effort_path(&dep.0));
        dep.2 && stamp(dep.0.clone(), target) == *dep
    })
}

pub(super) type ConfigCtx = (String, Option<BranchConfig>, Vec<FileDep>);

#[derive(Clone, Copy)]
struct ConfigValueQuery<'a> {
    section: &'a str,
    key: &'a str,
}

#[derive(Default)]
struct ConfigReader {
    files: HashMap<PathBuf, Option<String>>,
    deps: Vec<FileDep>,
    failure: Option<(PathBuf, ErrorKind, String)>,
}

impl ConfigReader {
    fn read(&mut self, path: &Path) -> (PathBuf, Option<String>) {
        let logical = path.to_path_buf();
        let path = canonicalize_best_effort_path(path);
        self.deps
            .extend((logical != path).then(|| stamp(logical, Some(path.clone()))));
        let Self {
            files,
            deps,
            failure,
        } = self;
        let contents = files
            .entry(path.clone())
            .or_insert_with(|| {
                let mut dep = stamp(path.clone(), None);
                let r = std::fs::read_to_string(&path);
                dep.2 &= r.is_ok() || matches!(&r, Err(e) if e.kind() == ErrorKind::NotFound);
                deps.push(dep);
                match r {
                    Ok(contents) => Some(contents),
                    Err(error) if error.kind() == ErrorKind::NotFound => None,
                    Err(error) => {
                        if failure.is_none() {
                            *failure = Some((path.clone(), error.kind(), error.to_string()));
                        }
                        None
                    }
                }
            })
            .clone();
        (path, contents)
    }

    fn read_error(&self) -> Option<std::io::Error> {
        self.failure.as_ref().map(|(path, kind, message)| {
            std::io::Error::new(*kind, format!("{}: {message}", path.display()))
        })
    }
}

#[cfg(test)]
pub(super) fn read_config(info: &GitWorktreeInfo, branch: &str) -> ConfigCtx {
    read_config_with_user_paths(
        info,
        branch,
        git_user_config_paths_at(&info.repo_root).expect("test environment config paths"),
    )
}

pub(super) fn read_config_for_status(
    info: &GitWorktreeInfo,
    branch: &str,
    errors: &mut Vec<GitReadError>,
) -> ConfigCtx {
    let user_config_paths = match git_user_config_paths_at(&info.repo_root) {
        Ok(paths) => paths,
        Err(error) => {
            errors.push(GitReadError::ConfigEnvironment {
                message: error.to_string(),
            });
            return (branch.to_string(), None, Vec::new());
        }
    };
    read_config_with_user_paths_and_errors(info, branch, user_config_paths, errors)
}

#[cfg(test)]
pub(super) fn read_config_with_user_paths(
    info: &GitWorktreeInfo,
    branch: &str,
    user_config_paths: Vec<PathBuf>,
) -> ConfigCtx {
    let mut errors = Vec::new();
    read_config_with_user_paths_and_errors(info, branch, user_config_paths, &mut errors)
}

fn read_config_with_user_paths_and_errors(
    info: &GitWorktreeInfo,
    branch: &str,
    user_config_paths: Vec<PathBuf>,
    errors: &mut Vec<GitReadError>,
) -> ConfigCtx {
    let command_parameters = match shepr_core::env::read_git_config_parameters() {
        Ok(parameters) => parameters,
        Err(error) => {
            errors.push(GitReadError::ConfigEnvironment {
                message: error.to_string(),
            });
            Vec::new()
        }
    };
    let mut reader = ConfigReader::default();
    let worktree_config_enabled =
        worktree_config_enabled(&info.git_common_dir.join("config"), info, &mut reader);
    let config_paths = user_config_paths
        .into_iter()
        .chain(std::iter::once(info.git_common_dir.join("config")))
        .collect::<Vec<_>>();
    let mut remote_urls = Vec::new();
    for path in &config_paths {
        let mut include_stack = Vec::new();
        collect_remote_urls(
            path,
            info,
            branch,
            &mut remote_urls,
            &mut include_stack,
            &mut reader,
        );
    }
    let mut config = BranchConfig {
        remote: String::new(),
        merge_ref: String::new(),
        fetch_refspecs: Vec::new(),
        remote_urls,
    };
    let mut ignored_value = None;
    for path in config_paths {
        let mut include_stack = Vec::new();
        merge_git_config(
            &mut config,
            &path,
            branch,
            info,
            true,
            None,
            &mut ignored_value,
            &mut include_stack,
            &mut reader,
        );
    }
    if worktree_config_enabled {
        let mut include_stack = Vec::new();
        merge_git_config(
            &mut config,
            &info.git_dir.join("config.worktree"),
            branch,
            info,
            false,
            None,
            &mut ignored_value,
            &mut include_stack,
            &mut reader,
        );
    }
    apply_git_config_parameters(&mut config, branch, &command_parameters);
    if let Some((path, kind, message)) = &reader.failure {
        errors.push(GitReadError::FileRead {
            path: path.clone(),
            message: format!("{kind:?}: {message}"),
        });
    }
    (
        branch.to_string(),
        (!config.remote.is_empty() && !config.merge_ref.is_empty()).then_some(config),
        reader.deps,
    )
}

/// The system and global config files in Git's read order. The two location
/// variables replace their corresponding defaults, `GIT_CONFIG_GLOBAL`
/// replaces both default global files, and `GIT_CONFIG_NOSYSTEM` omits the
/// system file when Git's boolean grammar reads it as true. `EnvKind::Path`
/// accepts relative overrides, which Git resolves against its working
/// directory. shepr runs every Git command from the repository root, so the
/// callers pass that root as `cwd` and this reader opens the same files those
/// commands do, not files relative to the server's own directory.
pub(super) fn git_user_config_paths_at(cwd: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let no_system = shepr_core::env::read_text(shepr_core::env::EnvVar::GitConfigNoSystem)?
        .and_then(|value| git_config_bool(&value))
        .unwrap_or(false);
    if !no_system {
        let system_path = git_config_override_path(cwd, shepr_core::env::EnvVar::GitConfigSystem)?
            .unwrap_or_else(|| PathBuf::from("/etc/gitconfig"));
        paths.push(system_path);
    }
    let global_path = git_config_override_path(cwd, shepr_core::env::EnvVar::GitConfigGlobal)?;
    if let Some(global_path) = global_path {
        paths.push(global_path);
        return Ok(paths);
    }

    let home = shepr_core::pathutil::home_dir().ok();
    // This reads Git's user config, not a Shepr location. A refused
    // `XDG_CONFIG_HOME` (relative, padded, non-UTF-8) already failed shepr's
    // own launch, since shepr's config directory resolves from it; here it
    // reads as unset, the spec's answer for an invalid value.
    let xdg_config_home = shepr_core::env::read_path(shepr_core::env::EnvVar::XdgConfigHome)
        .ok()
        .flatten();
    if let Some(xdg_config_home) = xdg_config_home {
        paths.push(xdg_config_home.join("git/config"));
    } else if let Some(home) = &home {
        paths.push(home.join(".config/git/config"));
    }
    if let Some(home) = home {
        paths.push(home.join(".gitconfig"));
    }
    Ok(paths)
}

fn git_config_override_path(
    cwd: &Path,
    var: shepr_core::env::EnvVar,
) -> io::Result<Option<PathBuf>> {
    Ok(shepr_core::env::read_path(var)?.map(|path| {
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    }))
}

/// Reads the final value for one key across `config_paths`, using the same
/// include handling and file dependency tracking as branch config. This is
/// for keys Git resolves through the whole config chain; repository format
/// keys go through [`read_repository_format_value`] instead. A missing file
/// or key is distinct from an unreadable config file so discovery can fail
/// closed when it cannot tell what Git would see.
pub(super) fn read_config_value(
    info: &GitWorktreeInfo,
    branch: &str,
    config_paths: &[PathBuf],
    target_section: &str,
    target_key: &str,
) -> std::io::Result<(Option<String>, Vec<FileDep>)> {
    let command_parameters = shepr_core::env::read_git_config_parameters()?;
    let mut reader = ConfigReader::default();
    let mut remote_urls = Vec::new();
    for path in config_paths {
        let mut include_stack = Vec::new();
        collect_remote_urls(
            path,
            info,
            branch,
            &mut remote_urls,
            &mut include_stack,
            &mut reader,
        );
    }
    if let Some(error) = reader.read_error() {
        return Err(error);
    }

    let mut config = BranchConfig {
        remote: String::new(),
        merge_ref: String::new(),
        fetch_refspecs: Vec::new(),
        remote_urls,
    };
    let mut value = None;
    let query = ConfigValueQuery {
        section: target_section,
        key: target_key,
    };
    for path in config_paths {
        let mut include_stack = Vec::new();
        merge_git_config(
            &mut config,
            path,
            branch,
            info,
            true,
            Some(query),
            &mut value,
            &mut include_stack,
            &mut reader,
        );
    }
    if let Some(command_value) =
        git_config_parameter_value(&command_parameters, target_section, None, target_key)
    {
        value = Some(command_value.to_owned());
    }
    if let Some(error) = reader.read_error() {
        return Err(error);
    }
    Ok((value, reader.deps))
}

fn git_config_parameter_value<'a>(
    parameters: &'a [(String, String)],
    section: &str,
    subsection: Option<&str>,
    key: &str,
) -> Option<&'a str> {
    parameters
        .iter()
        .rev()
        .find(|(name, _)| git_config_parameter_matches(name, section, subsection, key))
        .map(|(_, value)| value.as_str())
}

fn git_config_parameter_matches(
    name: &str,
    section: &str,
    subsection: Option<&str>,
    key: &str,
) -> bool {
    let Some((parameter_section, remainder)) = name.split_once('.') else {
        return false;
    };
    if !parameter_section.eq_ignore_ascii_case(section) {
        return false;
    }
    match (subsection, remainder.rsplit_once('.')) {
        (None, None) => remainder.eq_ignore_ascii_case(key),
        (Some(expected), Some((actual, actual_key))) => {
            actual == expected && actual_key.eq_ignore_ascii_case(key)
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn git_config_parameter_subsection<'a>(name: &'a str, section: &str, key: &str) -> Option<&'a str> {
    let (parameter_section, remainder) = name.split_once('.')?;
    let (subsection, actual_key) = remainder.rsplit_once('.')?;
    (parameter_section.eq_ignore_ascii_case(section) && actual_key.eq_ignore_ascii_case(key))
        .then_some(subsection)
}

fn apply_git_config_parameters(
    config: &mut BranchConfig,
    branch: &str,
    parameters: &[(String, String)],
) {
    for (name, value) in parameters {
        if git_config_parameter_matches(name, "branch", Some(branch), "remote") {
            config.remote.clone_from(value);
        } else if git_config_parameter_matches(name, "branch", Some(branch), "merge") {
            config.merge_ref.clone_from(value);
        } else if let Some(remote) = git_config_parameter_subsection(name, "remote", "fetch") {
            config
                .fetch_refspecs
                .push((remote.to_owned(), value.clone()));
        } else if let Some(remote) = git_config_parameter_subsection(name, "remote", "url") {
            config.remote_urls.push((remote.to_owned(), value.clone()));
        }
    }
}

/// The last value of `[section] key` in the one config file at `path`, with
/// no includes and no other config level. Git reads its repository format
/// (`core.repositoryformatversion` and every `extensions.*` key) this way:
/// the format check parses the repository's own config file alone before
/// any include is resolved, so `extensions.refstorage = reftable` reached
/// only through `include.path` or `~/.gitconfig` leaves a files ref store,
/// as `git rev-parse --show-ref-format` confirms. A key written without `=`
/// reads as `true`, Git's implicit boolean. A missing file or key is `None`;
/// an unreadable file is an error, so the caller can fail closed rather than
/// guess the format.
pub(super) fn read_repository_format_value(
    path: &Path,
    target_section: &str,
    target_key: &str,
) -> std::io::Result<(Option<String>, Vec<FileDep>)> {
    let mut reader = ConfigReader::default();
    let contents = reader.read(path).1;
    if let Some(error) = reader.read_error() {
        return Err(error);
    }
    let mut value = None;
    let mut in_section = false;
    for raw_line in contents.as_deref().unwrap_or_default().lines() {
        let line = raw_line.trim();
        if let Some(section_name) = extract_config_section(line) {
            in_section = section_name.trim().eq_ignore_ascii_case(target_section);
            continue;
        }
        if !in_section {
            continue;
        }
        match line.split_once('=') {
            Some((key, raw_value)) if key.trim().eq_ignore_ascii_case(target_key) => {
                value = Some(normalize_config_value(raw_value));
            }
            None if line.eq_ignore_ascii_case(target_key) => value = Some("true".to_string()),
            _ => {}
        }
    }
    Ok((value, reader.deps))
}

/// A config value read with Git's boolean grammar: `true`, `yes`, `on` and a
/// non-zero integer (with an optional `k`, `m` or `g` unit) are true;
/// `false`, `no`, `off`, `0` and the empty value (`key =`) are false, all
/// case-insensitively. A key with no `=` at all arrives here as `true`.
/// Anything else is `None`: Git refuses to run on such a value.
pub(super) fn git_config_bool(value: &str) -> Option<bool> {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "true" | "yes" | "on" => return Some(true),
        "false" | "no" | "off" | "" => return Some(false),
        _ => {}
    }
    let digits = lower
        .strip_suffix(['k', 'm', 'g'])
        .unwrap_or(lower.as_str());
    digits.parse::<i64>().ok().map(|number| number != 0)
}

#[cfg(test)]
mod xdg_path_tests {
    use super::*;
    use shepr_test_support::IsolatedEnv;

    #[test]
    fn git_user_config_ignores_empty_and_relative_xdg_home() {
        // The isolated environment turns the system level off.
        let env = IsolatedEnv::new();
        let expected = vec![
            env.home().join(".config/git/config"),
            env.home().join(".gitconfig"),
        ];
        for invalid in ["", "relative/config"] {
            env.set("XDG_CONFIG_HOME", invalid);
            assert_eq!(
                git_user_config_paths_at(Path::new(".")).expect("paths"),
                expected
            );
        }

        let xdg = env.path().join("xdg-config");
        env.set("XDG_CONFIG_HOME", &xdg);
        assert_eq!(
            git_user_config_paths_at(Path::new(".")).expect("paths"),
            vec![xdg.join("git/config"), env.home().join(".gitconfig")]
        );
    }

    #[test]
    fn git_user_config_skips_home_fallback_without_absolute_home() {
        let env = IsolatedEnv::new();
        env.remove("XDG_CONFIG_HOME");
        env.set("HOME", "relative/home");
        assert!(
            git_user_config_paths_at(Path::new("."))
                .expect("paths")
                .is_empty()
        );
    }

    #[test]
    fn git_config_environment_paths_follow_git_scope_order() {
        let env = IsolatedEnv::new();
        env.remove(shepr_core::env::EnvVar::GitConfigNoSystem);
        let system = env.path().join("system.gitconfig");
        let global = env.path().join("global.gitconfig");
        assert_eq!(
            git_user_config_paths_at(Path::new(".")).expect("paths"),
            vec![
                PathBuf::from("/etc/gitconfig"),
                env.home().join(".config/git/config"),
                env.home().join(".gitconfig"),
            ]
        );

        env.set(shepr_core::env::EnvVar::GitConfigSystem, &system);
        env.set(shepr_core::env::EnvVar::GitConfigGlobal, &global);
        assert_eq!(
            git_user_config_paths_at(Path::new(".")).expect("paths"),
            vec![system.clone(), global.clone()]
        );

        env.set(shepr_core::env::EnvVar::GitConfigNoSystem, "yes");
        assert_eq!(
            git_user_config_paths_at(Path::new(".")).expect("paths"),
            vec![global.clone()]
        );

        env.set(shepr_core::env::EnvVar::GitConfigNoSystem, "false");
        assert_eq!(
            git_user_config_paths_at(Path::new(".")).expect("paths"),
            vec![system, global]
        );
    }
}

fn worktree_config_enabled(path: &Path, info: &GitWorktreeInfo, reader: &mut ConfigReader) -> bool {
    let Some(contents) = reader.read(path).1 else {
        return false;
    };
    let mut section = ConfigSection::Other;
    let mut enabled = false;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if let Some(section_name) = extract_config_section(line) {
            let is_extensions = section_name.eq_ignore_ascii_case("extensions");
            section = if is_extensions {
                ConfigSection::Extensions
            } else {
                parse_config_section(
                    section_name,
                    "",
                    info,
                    path,
                    &BranchConfig {
                        remote: String::new(),
                        merge_ref: String::new(),
                        fetch_refspecs: Vec::new(),
                        remote_urls: Vec::new(),
                    },
                )
            };
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = normalize_config_value(value);
            match &section {
                ConfigSection::Extensions if key.eq_ignore_ascii_case("worktreeConfig") => {
                    enabled = git_config_bool(&value) == Some(true);
                }
                _ => {}
            }
            continue;
        }
        if matches!(section, ConfigSection::Extensions)
            && line.eq_ignore_ascii_case("worktreeConfig")
        {
            enabled = true;
        }
    }
    enabled
}

fn collect_remote_urls(
    path: &Path,
    info: &GitWorktreeInfo,
    branch: &str,
    remote_urls: &mut Vec<(String, String)>,
    include_stack: &mut Vec<PathBuf>,
    reader: &mut ConfigReader,
) {
    let (path, contents) = reader.read(path);
    if include_stack.contains(&path) {
        return;
    }
    include_stack.push(path.clone());
    let Some(contents) = contents else {
        include_stack.pop();
        return;
    };
    let mut section = ConfigSection::Other;
    let dummy_config = BranchConfig {
        remote: String::new(),
        merge_ref: String::new(),
        fetch_refspecs: Vec::new(),
        remote_urls: remote_urls.clone(),
    };
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if let Some(section_name) = extract_config_section(line) {
            section = parse_config_section(section_name, branch, info, &path, &dummy_config);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = normalize_config_value(value);
        match &section {
            ConfigSection::Remote(remote) if key.eq_ignore_ascii_case("url") => {
                remote_urls.push((remote.clone(), value));
            }
            ConfigSection::Include | ConfigSection::IncludeIf(IncludeIfMode::Enabled)
                if key.eq_ignore_ascii_case("path") =>
            {
                let Some(include_path) = resolve_include_path(&path, &value) else {
                    continue;
                };
                collect_remote_urls(
                    &include_path,
                    info,
                    branch,
                    remote_urls,
                    include_stack,
                    reader,
                );
            }
            _ => {}
        }
    }
    include_stack.pop();
}

fn merge_git_config(
    config: &mut BranchConfig,
    path: &Path,
    branch: &str,
    info: &GitWorktreeInfo,
    collect_hasconfig_urls: bool,
    query: Option<ConfigValueQuery<'_>>,
    query_value: &mut Option<String>,
    include_stack: &mut Vec<PathBuf>,
    reader: &mut ConfigReader,
) {
    let (path, contents) = reader.read(path);
    if include_stack.contains(&path) {
        return;
    }
    include_stack.push(path.clone());
    let Some(contents) = contents else {
        include_stack.pop();
        return;
    };
    let mut section = ConfigSection::Other;
    let mut current_section_name = String::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if let Some(section_name) = extract_config_section(line) {
            current_section_name.clear();
            current_section_name.push_str(section_name);
            section = parse_config_section(section_name, branch, info, &path, config);
            continue;
        }
        let (key, raw_value) = match line.split_once('=') {
            Some((key, value)) => (key, value),
            None if query.is_some_and(|query| {
                current_section_name.eq_ignore_ascii_case(query.section)
                    && line.eq_ignore_ascii_case(query.key)
            }) =>
            {
                (line, "true")
            }
            None => continue,
        };
        let key = key.trim();
        let value = normalize_config_value(raw_value);
        if let Some(query) = query
            && current_section_name.eq_ignore_ascii_case(query.section)
            && key.eq_ignore_ascii_case(query.key)
        {
            *query_value = Some(value.clone());
        }
        match &section {
            ConfigSection::Branch if key.eq_ignore_ascii_case("remote") => config.remote = value,
            ConfigSection::Branch if key.eq_ignore_ascii_case("merge") => config.merge_ref = value,
            ConfigSection::Remote(remote) if key.eq_ignore_ascii_case("fetch") => {
                config.fetch_refspecs.push((remote.clone(), value));
            }
            ConfigSection::Remote(remote)
                if collect_hasconfig_urls && key.eq_ignore_ascii_case("url") =>
            {
                config.remote_urls.push((remote.clone(), value));
            }
            ConfigSection::Include | ConfigSection::IncludeIf(IncludeIfMode::Enabled)
                if key.eq_ignore_ascii_case("path") =>
            {
                let Some(include_path) = resolve_include_path(&path, &value) else {
                    continue;
                };
                merge_git_config(
                    config,
                    &include_path,
                    branch,
                    info,
                    collect_hasconfig_urls,
                    query,
                    query_value,
                    include_stack,
                    reader,
                );
            }
            ConfigSection::IncludeIf(IncludeIfMode::HasConfig)
                if key.eq_ignore_ascii_case("path") =>
            {
                let Some(include_path) = resolve_include_path(&path, &value) else {
                    continue;
                };
                if !included_config_defines_remote_url(
                    &include_path,
                    branch,
                    info,
                    config,
                    include_stack,
                    reader,
                ) {
                    merge_git_config(
                        config,
                        &include_path,
                        branch,
                        info,
                        collect_hasconfig_urls,
                        query,
                        query_value,
                        include_stack,
                        reader,
                    );
                }
            }
            _ => {}
        }
    }
    include_stack.pop();
}

enum ConfigSection {
    Branch,
    Extensions,
    Include,
    IncludeIf(IncludeIfMode),
    Remote(String),
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncludeIfMode {
    Disabled,
    Enabled,
    HasConfig,
}

fn extract_config_section(line: &str) -> Option<&str> {
    if !line.starts_with('[') {
        return None;
    }
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, ch) in line.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ']' if !in_quotes => {
                let rest = line[index + 1..].trim();
                if rest.is_empty() || rest.starts_with('#') || rest.starts_with(';') {
                    return Some(&line[1..index]);
                }
                return None;
            }
            _ => {}
        }
    }
    None
}

fn parse_config_section(
    section: &str,
    branch: &str,
    info: &GitWorktreeInfo,
    config_path: &Path,
    config: &BranchConfig,
) -> ConfigSection {
    if let Some(name) = quoted_config_subsection(section, "branch") {
        return if name == branch {
            ConfigSection::Branch
        } else {
            ConfigSection::Other
        };
    }
    if let Some(name) = quoted_config_subsection(section, "remote") {
        return ConfigSection::Remote(name.to_string());
    }
    if section.eq_ignore_ascii_case("include") {
        return ConfigSection::Include;
    }
    if let Some(condition) = quoted_config_subsection(section, "includeIf") {
        return ConfigSection::IncludeIf(include_if_mode(
            condition,
            info,
            config_path,
            branch,
            config,
        ));
    }
    ConfigSection::Other
}

fn include_if_mode(
    condition: &str,
    info: &GitWorktreeInfo,
    config_path: &Path,
    branch: &str,
    config: &BranchConfig,
) -> IncludeIfMode {
    let (case_insensitive, pattern) = if let Some(pattern) = condition.strip_prefix("gitdir/i:") {
        (true, pattern)
    } else if let Some(pattern) = condition.strip_prefix("gitdir:") {
        (false, pattern)
    } else if let Some(pattern) = condition.strip_prefix("onbranch:") {
        let pattern = normalize_branch_include_pattern(pattern);
        return if wildcard_match(&pattern, branch, false) {
            IncludeIfMode::Enabled
        } else {
            IncludeIfMode::Disabled
        };
    } else if let Some(pattern) = condition.strip_prefix("hasconfig:remote.*.url:") {
        return if config
            .remote_urls
            .iter()
            .any(|(_, url)| wildcard_match(pattern, url, false))
        {
            IncludeIfMode::HasConfig
        } else {
            IncludeIfMode::Disabled
        };
    } else {
        return IncludeIfMode::Disabled;
    };
    let Some(pattern) = normalize_gitdir_include_pattern(pattern, config_path) else {
        return IncludeIfMode::Disabled;
    };
    let candidates = [
        info.git_dir.display().to_string(),
        info.git_common_dir.display().to_string(),
        info.repo_root.join(".git").display().to_string(),
    ];
    if candidates
        .iter()
        .any(|candidate| wildcard_match(&pattern, candidate, case_insensitive))
    {
        IncludeIfMode::Enabled
    } else {
        IncludeIfMode::Disabled
    }
}

fn included_config_defines_remote_url(
    path: &Path,
    branch: &str,
    info: &GitWorktreeInfo,
    config: &BranchConfig,
    include_stack: &mut Vec<PathBuf>,
    reader: &mut ConfigReader,
) -> bool {
    let (path, contents) = reader.read(path);
    if include_stack.contains(&path) {
        return false;
    }
    include_stack.push(path.clone());
    let Some(contents) = contents else {
        include_stack.pop();
        return false;
    };
    let mut section = ConfigSection::Other;
    let mut defines_remote_url = false;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if let Some(section_name) = extract_config_section(line) {
            section = parse_config_section(section_name, branch, info, &path, config);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if matches!(section, ConfigSection::Remote(_)) && key.eq_ignore_ascii_case("url") {
            defines_remote_url = true;
            break;
        }
        let value = normalize_config_value(value);
        match &section {
            ConfigSection::Include
            | ConfigSection::IncludeIf(IncludeIfMode::Enabled | IncludeIfMode::HasConfig)
                if key.eq_ignore_ascii_case("path") =>
            {
                let Some(include_path) = resolve_include_path(&path, &value) else {
                    continue;
                };
                if !included_config_defines_remote_url(
                    &include_path,
                    branch,
                    info,
                    config,
                    include_stack,
                    reader,
                ) {
                    continue;
                }
                defines_remote_url = true;
                break;
            }
            _ => {}
        }
    }
    include_stack.pop();
    defines_remote_url
}

fn normalize_branch_include_pattern(pattern: &str) -> String {
    if pattern.ends_with('/') {
        format!("{pattern}**")
    } else {
        pattern.to_string()
    }
}

/// The `gitdir:` pattern as an absolute glob, or `None` when it starts with
/// `~/` and `HOME` is unusable: the condition then names no directory, rather
/// than one relative to wherever the server runs.
pub(super) fn normalize_gitdir_include_pattern(
    pattern: &str,
    config_path: &Path,
) -> Option<String> {
    let mut pattern = if let Some(rest) = pattern.strip_prefix("~/") {
        // `home_dir` rejects unusable HOME values instead of resolving this
        // Git config path relative to the server's current directory.
        shepr_core::pathutil::home_dir()
            .ok()?
            .join(rest)
            .display()
            .to_string()
    } else if let Some(rest) = pattern.strip_prefix("./") {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(rest)
            .display()
            .to_string()
    } else if Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    };
    if pattern.ends_with('/') {
        pattern.push_str("**");
    }
    Some(pattern)
}

fn wildcard_match(pattern: &str, value: &str, case_insensitive: bool) -> bool {
    let pattern = if case_insensitive {
        pattern.to_ascii_lowercase()
    } else {
        pattern.to_string()
    };
    let value = if case_insensitive {
        value.to_ascii_lowercase()
    } else {
        value.to_string()
    };
    wildcard_match_bytes(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], value: &[u8]) -> bool {
    match pattern.split_first() {
        None => value.is_empty(),
        Some((&b'*', rest)) => {
            wildcard_match_bytes(rest, value)
                || (!value.is_empty() && wildcard_match_bytes(pattern, &value[1..]))
        }
        Some((&expected, rest)) => value.split_first().is_some_and(|(&actual, value_rest)| {
            actual == expected && wildcard_match_bytes(rest, value_rest)
        }),
    }
}

fn quoted_config_subsection<'a>(section: &'a str, name: &str) -> Option<&'a str> {
    let prefix_len = name.len() + 2;
    if section.len() <= prefix_len {
        return None;
    }
    let prefix = &section[..prefix_len];
    if !prefix.eq_ignore_ascii_case(&format!("{name} \"")) {
        return None;
    }
    section[prefix_len..].strip_suffix('"')
}

/// The file an `include.path` names, or `None` when it starts with `~/` and
/// `HOME` is unusable: the include is then skipped, rather than read from a
/// path relative to the including file.
pub(super) fn resolve_include_path(config_path: &Path, include_path: &str) -> Option<PathBuf> {
    let include_path = match include_path.strip_prefix("~/") {
        // Keep tilde includes under the shared absolute HOME policy.
        Some(rest) => shepr_core::pathutil::home_dir().ok()?.join(rest),
        None => PathBuf::from(include_path),
    };
    Some(if include_path.is_absolute() {
        include_path
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(include_path)
    })
}

fn normalize_config_value(value: &str) -> String {
    let value = value.trim();
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            '#' | ';'
                if !in_quotes
                    && value[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace) =>
            {
                return unquote_config_value(value[..index].trim());
            }
            _ => {}
        }
    }
    unquote_config_value(value)
}

fn unquote_config_value(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

pub(super) fn upstream_full_ref(config: &BranchConfig) -> Option<String> {
    if config.remote == "." {
        return Some(config.merge_ref.clone());
    }
    let default_refspec = format!("+refs/heads/*:refs/remotes/{}/*", config.remote);
    let remote_refspecs = config
        .fetch_refspecs
        .iter()
        .filter(|(remote, _)| remote == &config.remote)
        .map(|(_, refspec)| refspec);
    let refspecs = remote_refspecs.collect::<Vec<_>>();
    if refspecs.is_empty() {
        return map_fetch_refspec(&default_refspec, &config.merge_ref).into_ref();
    }
    refspecs
        .into_iter()
        .find_map(|refspec| map_fetch_refspec(refspec, &config.merge_ref).into_ref())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchRefspecMatch {
    Ref(String),
    NoMatch,
}

impl FetchRefspecMatch {
    fn into_ref(self) -> Option<String> {
        match self {
            FetchRefspecMatch::Ref(value) => Some(value),
            FetchRefspecMatch::NoMatch => None,
        }
    }
}

fn map_fetch_refspec(refspec: &str, merge_ref: &str) -> FetchRefspecMatch {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    if refspec.starts_with('^') {
        return FetchRefspecMatch::NoMatch;
    }
    let Some((source, destination)) = refspec.split_once(':') else {
        return FetchRefspecMatch::NoMatch;
    };
    match (source.split_once('*'), destination.split_once('*')) {
        (None, None) => {
            if source == merge_ref {
                FetchRefspecMatch::Ref(destination.to_string())
            } else {
                FetchRefspecMatch::NoMatch
            }
        }
        (Some((source_prefix, source_suffix)), Some((destination_prefix, destination_suffix))) => {
            let Some(matched) = merge_ref
                .strip_prefix(source_prefix)
                .and_then(|matched| matched.strip_suffix(source_suffix))
            else {
                return FetchRefspecMatch::NoMatch;
            };
            FetchRefspecMatch::Ref(format!("{destination_prefix}{matched}{destination_suffix}"))
        }
        _ => FetchRefspecMatch::NoMatch,
    }
}
