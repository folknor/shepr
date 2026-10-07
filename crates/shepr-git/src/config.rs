use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::discovery::{
    GitWorktreeInfo, canonicalize_best_effort_path, command_failed, run_git_output,
};
use super::identity::{BranchName, FullRefName};
use super::{FileReadReason, GitReadError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BranchConfig {
    pub(super) remote: String,
    pub(super) merge_ref: FullRefName,
    full_ref: FullRefName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DependencyStamp {
    Present { file: shepr_platform::FileStamp },
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileDependency {
    path: PathBuf,
    stamp: DependencyStamp,
    canonical_target: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Dependencies {
    Tracked(Vec<FileDependency>),
    Uncacheable,
}

impl Dependencies {
    pub(super) fn tracked(dependencies: Vec<FileDependency>) -> Self {
        if dependencies
            .iter()
            .any(|dependency| dependency.stamp == DependencyStamp::Unavailable)
        {
            Self::Uncacheable
        } else {
            Self::Tracked(dependencies)
        }
    }

    fn push(&mut self, dependency: FileDependency) {
        match self {
            Self::Tracked(dependencies) if dependency.stamp != DependencyStamp::Unavailable => {
                dependencies.push(dependency);
            }
            Self::Tracked(_) => *self = Self::Uncacheable,
            Self::Uncacheable => {}
        }
    }

    pub(super) fn mark_uncacheable(&mut self) {
        *self = Self::Uncacheable;
    }

    pub(super) fn extend(&mut self, other: Self) {
        match (self, other) {
            (Self::Tracked(current), Self::Tracked(mut added)) => current.append(&mut added),
            (current, _) => *current = Self::Uncacheable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConfigCtx {
    pub(super) branch: BranchName,
    pub(super) config: Option<BranchConfig>,
    pub(super) dependencies: Dependencies,
}

pub(super) fn stamp(path: PathBuf, canonical_target: Option<PathBuf>) -> FileDependency {
    // This may block on a shared user config or include outside the checkout
    // paths the worker records. Mount isolation needs this actual dependency
    // tracked before metadata, not just the refresh job's cwd and key.
    let stamp = match std::fs::metadata(&path) {
        Ok(metadata) => DependencyStamp::Present {
            file: shepr_platform::FileStamp::from_metadata(&metadata),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => DependencyStamp::Missing,
        Err(_) => DependencyStamp::Unavailable,
    };
    FileDependency {
        path,
        stamp,
        canonical_target,
    }
}

pub(super) fn deps_current(dependencies: &Dependencies) -> bool {
    match dependencies {
        Dependencies::Tracked(dependencies) => dependencies.iter().all(|dependency| {
            let canonical_target = dependency
                .canonical_target
                .as_ref()
                .map(|_| canonicalize_best_effort_path(&dependency.path));
            stamp(dependency.path.clone(), canonical_target) == *dependency
        }),
        Dependencies::Uncacheable => false,
    }
}

fn config_output(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, GitReadError> {
    let output = run_git_output(cwd, args)?;
    if !output.status.success() {
        return Err(command_failed(cwd, args, &output));
    }
    Ok(output.stdout)
}

// Git owns config grammar, conditional includes and refspec mapping. The
// origin listing is only a cache dependency list, never a second config parser.
// Stamp absent default files and include targets too: creating an empty or
// previously missing config must invalidate the same cache as editing one.
fn config_deps(
    info: &GitWorktreeInfo,
    mut paths: Vec<PathBuf>,
) -> Result<Dependencies, GitReadError> {
    paths.extend([
        info.git_common_dir.join("config"),
        info.git_dir.join("config.worktree"),
    ]);
    let mut deps = Dependencies::tracked(paths.into_iter().map(|path| stamp(path, None)).collect());
    let query = ["config", "--includes", "--null", "--show-origin", "--list"];
    let output = config_output(&info.repo_root, &query)?;
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(origin) = fields.next().filter(|field| !field.is_empty()) {
        let Some(entry) = fields.next() else {
            return Err(GitReadError::InvalidOutput {
                cwd: info.repo_root.clone(),
                arguments: ["config", "--includes", "--null", "--show-origin", "--list"]
                    .map(str::to_owned)
                    .to_vec(),
                output: String::from_utf8_lossy(&output).into_owned(),
            });
        };
        let path = origin.strip_prefix(b"file:").map(|origin| {
            let path = PathBuf::from(std::ffi::OsString::from_vec(origin.to_vec()));
            if path.is_absolute() {
                path
            } else {
                info.repo_root.join(path)
            }
        });
        if let Some(path) = &path {
            let target = canonicalize_best_effort_path(path);
            deps.push(stamp(path.clone(), Some(target)));
        }
        let Some(index) = entry.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let (key, value) = (&entry[..index], &entry[index + 1..]);
        if key != b"include.path" && !(key.starts_with(b"includeif.") && key.ends_with(b".path")) {
            continue;
        }
        let value = PathBuf::from(std::ffi::OsString::from_vec(value.to_vec()));
        let included =
            if let Some(suffix) = value.as_os_str().as_encoded_bytes().strip_prefix(b"~/") {
                shepr_core::pathutil::home_dir().ok().map(|home| {
                    let suffix = suffix
                        .iter()
                        .copied()
                        .skip_while(|byte| *byte == b'/')
                        .collect();
                    home.join(std::ffi::OsString::from_vec(suffix))
                })
            } else if value.as_os_str().as_encoded_bytes().starts_with(b"~") {
                // Git also expands ~user using the host's account database. An
                // absent target has no file origin to stamp; conservatively query
                // again rather than duplicate that resolution or cache a miss.
                deps.mark_uncacheable();
                None
            } else if value.is_absolute() {
                Some(value)
            } else {
                path.as_ref()
                    .and_then(|path| path.parent())
                    .map(|parent| parent.join(value))
            };
        if let Some(included) = included {
            let target = canonicalize_best_effort_path(&included);
            deps.push(stamp(included, Some(target)));
        }
    }
    // Includes can only be discovered by asking Git. Query again after their
    // stamps are captured: if a config changed between the first query and
    // those stamps, the changed output keeps this answer out of the cache.
    if config_output(&info.repo_root, &query)? != output {
        deps.mark_uncacheable();
    }
    Ok(deps)
}

fn branch_config(
    info: &GitWorktreeInfo,
    branch: &BranchName,
) -> Result<Option<BranchConfig>, GitReadError> {
    let full_ref = branch.full_ref();
    // A ref query cannot enumerate the config origins used to derive its
    // upstream. Keep a separate config-origin probe on config changes so
    // conditional includes and Git's refspec rules have one owner: Git.
    let args = [
        "for-each-ref",
        "--format=%(refname)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)",
        full_ref.as_str(),
    ];
    let output = config_output(&info.repo_root, &args)?;
    let output = String::from_utf8(output).map_err(|_| GitReadError::InvalidUtf8 {
        cwd: info.repo_root.clone(),
        arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
    })?;
    for line in output.lines() {
        let mut fields = line.split('\0');
        if fields.next() != Some(full_ref.as_str()) {
            continue;
        }
        let Some(upstream) = fields.next() else {
            return Err(GitReadError::InvalidOutput {
                cwd: info.repo_root.clone(),
                arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
                output: line.to_owned(),
            });
        };
        if upstream.is_empty() {
            return Ok(None);
        }
        let (Some(remote), Some(merge_ref)) = (fields.next(), fields.next()) else {
            return Err(GitReadError::InvalidOutput {
                cwd: info.repo_root.clone(),
                arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
                output: line.to_owned(),
            });
        };
        let (Some(full_ref), Some(merge_ref)) =
            (FullRefName::parse(upstream), FullRefName::parse(merge_ref))
        else {
            return Err(GitReadError::InvalidOutput {
                cwd: info.repo_root.clone(),
                arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
                output: line.to_owned(),
            });
        };
        return Ok(Some(BranchConfig {
            remote: remote.to_owned(),
            merge_ref,
            full_ref,
        }));
    }
    Ok(None)
}

pub(super) fn read_config_for_status(
    info: &GitWorktreeInfo,
    branch: &BranchName,
    errors: &mut Vec<GitReadError>,
) -> ConfigCtx {
    // Keep refused environment values typed apart from errors reading config files.
    let user_config_paths = match git_user_config_paths_at(&info.repo_root) {
        Ok(paths) => paths,
        Err(error) => {
            errors.push(GitReadError::ConfigEnvironment {
                error: super::GitConfigEnvironmentError(error),
            });
            return ConfigCtx {
                branch: branch.clone(),
                config: None,
                dependencies: Dependencies::Uncacheable,
            };
        }
    };
    let mut deps = match config_deps(info, user_config_paths) {
        Ok(deps) => deps,
        Err(error) => {
            errors.push(error);
            return ConfigCtx {
                branch: branch.clone(),
                config: None,
                dependencies: Dependencies::Uncacheable,
            };
        }
    };
    let config = match branch_config(info, branch) {
        Ok(config) => config,
        Err(error) => {
            errors.push(error);
            deps.mark_uncacheable();
            None
        }
    };
    ConfigCtx {
        branch: branch.clone(),
        config,
        dependencies: deps,
    }
}

pub(super) fn upstream_full_ref(config: &BranchConfig) -> FullRefName {
    config.full_ref.clone()
}

pub(super) fn read_repository_format_value(
    path: &Path,
    section: &str,
    key: &str,
) -> Result<(Option<String>, Dependencies), GitReadError> {
    let cwd = path.parent().ok_or_else(|| GitReadError::FileRead {
        path: path.to_path_buf(),
        reason: FileReadReason::ConfigWithoutParent,
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| GitReadError::FileRead {
            path: path.to_path_buf(),
            reason: FileReadReason::InvalidConfigName,
        })?;
    let dep = stamp(path.to_path_buf(), None);
    match dep.stamp.clone() {
        DependencyStamp::Unavailable => {
            return Err(GitReadError::FileRead {
                path: path.to_path_buf(),
                reason: FileReadReason::ConfigMetadataUnavailable,
            });
        }
        DependencyStamp::Missing => return Ok((None, Dependencies::tracked(vec![dep]))),
        DependencyStamp::Present { .. } => {}
    }
    let query = format!("{section}.{key}");
    let args = ["config", "--file", name, "--no-includes", "--get", &query];
    let output = run_git_output(cwd, &args)?;
    // For `config --get`, status 1 means this key is absent; keep that meaning
    // beside the probe instead of inferring it from a shared argv matcher.
    if output.status.code() == Some(1) {
        return Ok((None, Dependencies::tracked(vec![dep])));
    }
    if !output.status.success() {
        return Err(command_failed(cwd, &args, &output));
    }
    let value = String::from_utf8(output.stdout).map_err(|_| GitReadError::InvalidUtf8 {
        cwd: cwd.to_path_buf(),
        arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
    })?;
    Ok((
        Some(value.trim().to_owned()),
        Dependencies::tracked(vec![dep]),
    ))
}

pub(super) fn read_bare(info: &GitWorktreeInfo) -> Result<bool, GitReadError> {
    let args = ["config", "--includes", "--bool", "--get", "core.bare"];
    let output = run_git_output(&info.git_dir, &args)?;
    // This `config --get` probe uses the same key-absent exit status, declared
    // next to its own argv so unrelated Git commands keep their own policy.
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    if !output.status.success() {
        return Err(command_failed(&info.git_dir, &args, &output));
    }
    Ok(output.stdout == b"true\n")
}

pub(super) fn git_user_config_paths_at(
    cwd: &Path,
) -> Result<Vec<PathBuf>, shepr_core::env::EnvError> {
    let mut paths = Vec::new();
    let no_system = shepr_core::env::read_os(shepr_core::env::EnvVar::GitConfigNoSystem)?
        .as_deref()
        .and_then(|value| git_config_bool(value.as_bytes()))
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

    // These are Git's user config files, not shepr locations, and Git itself
    // skips the HOME-relative ones when HOME is unusable rather than failing.
    // A server always computes a workspace's Git branch and ahead/behind, so
    // a server started without HOME must not lose Git status over files Git
    // would not read either; the HOME and XDG errors are deliberately not
    // propagated here. A refused `XDG_CONFIG_HOME` (relative, padded,
    // non-UTF-8) already failed shepr's own launch, since shepr's config
    // directory resolves from it; here it reads as unset, the spec's answer
    // for an invalid value.
    let home = shepr_core::pathutil::home_dir().ok();
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
) -> Result<Option<PathBuf>, shepr_core::env::EnvError> {
    Ok(shepr_core::env::read_os(var)?.map(|path| {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    }))
}

/// Git's maybe-bool grammar: boolean words or a base-0 integer with an
/// optional scaling suffix.
pub(super) fn git_config_bool(value: &[u8]) -> Option<bool> {
    if value.eq_ignore_ascii_case(b"true")
        || value.eq_ignore_ascii_case(b"yes")
        || value.eq_ignore_ascii_case(b"on")
    {
        return Some(true);
    }
    if value.eq_ignore_ascii_case(b"false")
        || value.eq_ignore_ascii_case(b"no")
        || value.eq_ignore_ascii_case(b"off")
        || value.is_empty()
    {
        return Some(false);
    }

    let value = std::str::from_utf8(value).ok()?;
    let value = value.trim_start_matches([' ', '\t', '\n', '\r', '\u{b}', '\u{c}']);
    let (negative, value) = if let Some(value) = value.strip_prefix('-') {
        (true, value)
    } else if let Some(value) = value.strip_prefix('+') {
        (false, value)
    } else {
        (false, value)
    };
    let (radix, digits_start) = if value.len() > 2
        && value.as_bytes()[0] == b'0'
        && matches!(value.as_bytes()[1], b'x' | b'X')
        && value.as_bytes()[2].is_ascii_hexdigit()
    {
        (16, 2)
    } else if value.starts_with('0') {
        (8, 0)
    } else {
        (10, 0)
    };
    let digits_end = digits_start
        + value.as_bytes()[digits_start..]
            .iter()
            .take_while(|&&byte| match radix {
                8 => matches!(byte, b'0'..=b'7'),
                10 => byte.is_ascii_digit(),
                16 => byte.is_ascii_hexdigit(),
                _ => false,
            })
            .count();
    if digits_end == digits_start {
        return None;
    }
    let magnitude = i128::from_str_radix(&value[digits_start..digits_end], radix).ok()?;
    let number = if negative { -magnitude } else { magnitude };
    let suffix = &value[digits_end..];
    let factor = if suffix.is_empty() {
        1
    } else if suffix.eq_ignore_ascii_case("k") {
        1024
    } else if suffix.eq_ignore_ascii_case("m") {
        1024 * 1024
    } else if suffix.eq_ignore_ascii_case("g") {
        1024 * 1024 * 1024
    } else {
        return None;
    };
    i32::try_from(number.checked_mul(factor)?)
        .ok()
        .map(|number| number != 0)
}
