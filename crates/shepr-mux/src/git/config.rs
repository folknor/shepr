use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::GitReadError;
use super::discovery::{GitWorktreeInfo, canonicalize_best_effort_path, run_git_output};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BranchConfig {
    pub(super) remote: String,
    pub(super) merge_ref: String,
    full_ref: String,
}

type FileStamp = Option<(Option<SystemTime>, u64)>;
pub(super) type FileDep = (PathBuf, FileStamp, bool, Option<PathBuf>);
pub(super) type ConfigCtx = (String, Option<BranchConfig>, Vec<FileDep>);

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

fn config_output(cwd: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = run_git_output(cwd, args).map_err(io::Error::other)?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output.stdout)
}

// Git owns config grammar, conditional includes and refspec mapping. The
// origin listing is only a cache dependency list, never a second config parser.
// Stamp absent default files and include targets too: creating an empty or
// previously missing config must invalidate the same cache as editing one.
fn config_deps(info: &GitWorktreeInfo) -> io::Result<Vec<FileDep>> {
    let mut paths = git_user_config_paths_at(&info.repo_root)?;
    paths.extend([
        info.git_common_dir.join("config"),
        info.git_dir.join("config.worktree"),
    ]);
    let mut deps: Vec<_> = paths.into_iter().map(|path| stamp(path, None)).collect();
    let query = ["config", "--includes", "--null", "--show-origin", "--list"];
    let output = config_output(&info.repo_root, &query)?;
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(origin) = fields.next().filter(|field| !field.is_empty()) {
        let Some(entry) = fields.next() else {
            return Err(io::Error::other("incomplete Git config origin output"));
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
                if let Some(dep) = deps.first_mut() {
                    dep.2 = false;
                }
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
        for dep in &mut deps {
            dep.2 = false;
        }
    }
    Ok(deps)
}

fn branch_config(info: &GitWorktreeInfo, branch: &str) -> io::Result<Option<BranchConfig>> {
    let full_ref = format!("refs/heads/{branch}");
    // A ref query cannot enumerate the config origins used to derive its
    // upstream. Keep a separate config-origin probe on config changes so
    // conditional includes and Git's refspec rules have one owner: Git.
    let args = [
        "for-each-ref",
        "--format=%(refname)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)",
        &full_ref,
    ];
    let output = config_output(&info.repo_root, &args)?;
    let output = String::from_utf8(output).map_err(io::Error::other)?;
    Ok(output.lines().find_map(|line| {
        let mut fields = line.split('\0');
        if fields.next()? != full_ref {
            return None;
        }
        let upstream = fields.next()?;
        let remote = fields.next()?;
        let merge_ref = fields.next()?;
        (!upstream.is_empty()).then(|| BranchConfig {
            remote: remote.to_owned(),
            merge_ref: merge_ref.to_owned(),
            full_ref: upstream.to_owned(),
        })
    }))
}

pub(super) fn read_config_for_status(
    info: &GitWorktreeInfo,
    branch: &str,
    errors: &mut Vec<GitReadError>,
) -> ConfigCtx {
    let mut deps = match config_deps(info) {
        Ok(deps) => deps,
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: info.git_common_dir.join("config"),
                message: error.to_string(),
            });
            let mut dep = stamp(info.git_common_dir.join("config"), None);
            dep.2 = false;
            return (branch.to_owned(), None, vec![dep]);
        }
    };
    let config = match branch_config(info, branch) {
        Ok(config) => config,
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: info.git_common_dir.join("config"),
                message: error.to_string(),
            });
            if let Some(dep) = deps.first_mut() {
                dep.2 = false;
            }
            None
        }
    };
    (branch.to_owned(), config, deps)
}

pub(super) fn upstream_full_ref(config: &BranchConfig) -> Option<String> {
    Some(config.full_ref.clone())
}

pub(super) fn read_repository_format_value(
    path: &Path,
    section: &str,
    key: &str,
) -> io::Result<(Option<String>, Vec<FileDep>)> {
    let cwd = path
        .parent()
        .ok_or_else(|| io::Error::other("config has no parent"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid config name"))?;
    let dep = stamp(path.to_path_buf(), None);
    if !dep.2 {
        return Err(io::Error::other(
            "repository config metadata is unavailable",
        ));
    }
    if dep.1.is_none() {
        return Ok((None, vec![dep]));
    }
    let query = format!("{section}.{key}");
    let output = run_git_output(
        cwd,
        &["config", "--file", name, "--no-includes", "--get", &query],
    )
    .map_err(io::Error::other)?;
    if output.status.code() == Some(1) {
        return Ok((None, vec![dep]));
    }
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let value = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    Ok((Some(value.trim().to_owned()), vec![dep]))
}

pub(super) fn read_bare(info: &GitWorktreeInfo) -> io::Result<bool> {
    let output = run_git_output(
        &info.git_dir,
        &["config", "--includes", "--bool", "--get", "core.bare"],
    )
    .map_err(io::Error::other)?;
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output.stdout == b"true\n")
}

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
    Ok(shepr_core::env::read_os(var)?.map(|path| {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    }))
}

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
