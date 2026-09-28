use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Output;

use super::GitReadError;

pub(super) const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpaceMetadata {
    pub key: String,
    pub checkout_key: String,
    pub repo_name: String,
    pub repo_root: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitWorktreeInfo {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub git_common_dir: PathBuf,
    pub is_linked_worktree: bool,
}

#[cfg(test)]
fn derive_label_from_cwd(cwd: &Path) -> String {
    match git_repo_root(cwd) {
        Some(repo_root) => automatic_workspace_label(cwd, &repo_root),
        None => fallback_label_from_cwd(cwd),
    }
}

/// The label for a cwd outside any Git checkout: `~` for the home directory,
/// the directory name otherwise. This runs when a workspace's identity cwd
/// changes or its Git status is refreshed, never per frame, so `$HOME` is
/// read here. An unusable `HOME` only means the `~` label is not offered.
pub fn fallback_label_from_cwd(cwd: &Path) -> String {
    let home = shepr_core::pathutil::home_dir().ok();
    shepr_core::workspace_label::workspace_label_from_cwd(cwd, None, home.as_deref())
}

pub(crate) fn git_worktree_info(cwd: &Path) -> Option<GitWorktreeInfo> {
    git_worktree_info_with_errors(cwd, &mut Vec::new())
}

pub(super) fn git_worktree_info_with_errors(
    cwd: &Path,
    errors: &mut Vec<GitReadError>,
) -> Option<GitWorktreeInfo> {
    let repo_root = git_repo_root_with_errors(cwd, errors)?;
    let git_dir = match locate_git_dir(&repo_root) {
        Ok(Some(git_dir)) => git_dir,
        Ok(None) => return None,
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: repo_root.join(".git"),
                message: error.to_string(),
            });
            return None;
        }
    };
    match git_config_info(&repo_root, &git_dir) {
        Ok(info) => Some(info),
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: git_dir.join("commondir"),
                message: error.to_string(),
            });
            None
        }
    }
}

/// Inside a Git checkout the label is the checkout root's name; the home
/// directory is never consulted, so none is resolved.
pub(crate) fn automatic_workspace_label(cwd: &Path, repo_root: &Path) -> String {
    shepr_core::workspace_label::workspace_label_from_cwd(cwd, Some(repo_root), None)
}

pub(super) fn git_space_metadata_from_info(info: &GitWorktreeInfo) -> GitSpaceMetadata {
    let key = canonicalize_best_effort_path(&info.git_common_dir)
        .display()
        .to_string();
    let checkout_key = canonicalize_best_effort_path(&info.repo_root)
        .display()
        .to_string();
    let common_dir_name = info
        .git_common_dir
        .file_name()
        .and_then(|name| name.to_str());
    let label_path = match common_dir_name {
        Some(".git") => info.git_common_dir.parent().unwrap_or(&info.repo_root),
        Some(".bare") => embedded_bare_repo_container(info).unwrap_or(&info.git_common_dir),
        _ => &info.git_common_dir,
    };
    let repo_name = label_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string();
    GitSpaceMetadata {
        key,
        checkout_key,
        repo_name,
        repo_root: info.repo_root.clone(),
        is_linked_worktree: info.is_linked_worktree,
    }
}

fn embedded_bare_repo_container(info: &GitWorktreeInfo) -> Option<&Path> {
    let parent = info.git_common_dir.parent()?;
    let parent_git_dir = git_dir_for_repo_root(parent)?;
    (canonicalize_best_effort_path(&parent_git_dir) == info.git_common_dir).then_some(parent)
}

pub(super) fn canonicalize_best_effort_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The common directory a Git directory shares its refs with: itself unless
/// a `commondir` file names another. `None` when `commondir` exists but
/// cannot be read: taking the Git directory as its own common directory then
/// would give a linked worktree the wrong space key.
fn git_common_dir_for_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let commondir = git_dir.join("commondir");
    let contents = match std::fs::read_to_string(&commondir) {
        Ok(contents) => contents,
        Err(error) if is_absence(&error) => return Some(git_dir.to_path_buf()),
        Err(error) => {
            tracing::debug!(path = %commondir.display(), %error, "git commondir unreadable");
            return None;
        }
    };
    let path = Path::new(contents.trim());
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    })
}

fn git_config_info(repo_root: &Path, git_dir: &Path) -> io::Result<GitWorktreeInfo> {
    let repo_root = repo_root.to_path_buf();
    let git_dir = canonicalize_best_effort_path(git_dir);
    let git_common_dir = git_common_dir_for_git_dir(&git_dir).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has an unreadable commondir", git_dir.display()),
        )
    })?;
    let git_common_dir = canonicalize_best_effort_path(&git_common_dir);
    Ok(GitWorktreeInfo {
        repo_root,
        is_linked_worktree: git_dir != git_common_dir,
        git_dir,
        git_common_dir,
    })
}

/// Outcome of reading one Git ref file, classified without collapsing metadata
/// errors so callers can distinguish "this ref does not exist" from "this ref
/// exists (or cannot be ruled out) but its content must not be trusted".
/// `Path::exists()` cannot distinguish them because it returns `false` on
/// metadata errors. When opening reports `NotFound` or `NotADirectory`,
/// `symlink_metadata` distinguishes a genuinely absent path from a dangling
/// symlink without following the final link.
pub(super) enum RefFileRead {
    Content(String),
    Absent,
    Unavailable(String),
}

pub(super) fn read_git_ref_file_state(path: &Path) -> RefFileRead {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if is_absence(&error) => {
            return match std::fs::symlink_metadata(path) {
                Err(metadata_error) if is_absence(&metadata_error) => RefFileRead::Absent,
                // An entry that exists has a symlink target that is missing or
                // traverses a non-directory: Git treats the loose ref as broken
                // and does not fall back to an older packed ref. Any other stat
                // error leaves the ref's identity unknown.
                Ok(_) => RefFileRead::Unavailable(error.to_string()),
                Err(metadata_error) => RefFileRead::Unavailable(metadata_error.to_string()),
            };
        }
        // Permission or I/O errors: the ref may exist, so its identity is
        // unavailable rather than absent.
        Err(error) => return RefFileRead::Unavailable(error.to_string()),
    };
    let mut contents = String::new();
    if let Err(error) = file
        .take((MAX_GIT_REF_FILE_BYTES + 1) as u64)
        .read_to_string(&mut contents)
    {
        return RefFileRead::Unavailable(error.to_string());
    }
    if contents.len() > MAX_GIT_REF_FILE_BYTES {
        return RefFileRead::Unavailable(format!(
            "file exceeds the {MAX_GIT_REF_FILE_BYTES}-byte read limit"
        ));
    }
    RefFileRead::Content(contents)
}

pub(super) fn read_git_ref_file(path: &Path, errors: &mut Vec<GitReadError>) -> Option<String> {
    match read_git_ref_file_state(path) {
        RefFileRead::Content(contents) => Some(contents),
        RefFileRead::Absent => None,
        RefFileRead::Unavailable(message) => {
            errors.push(GitReadError::FileRead {
                path: path.to_path_buf(),
                message,
            });
            None
        }
    }
}

/// Whether an error from a stat or open means "nothing is there": `NotFound`,
/// or `NotADirectory` for a path through a file.
fn is_absence(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// The type of the entry at `path`, following symlinks as Git's discovery
/// does, or `None` when nothing is there. Any other stat error (`EACCES`,
/// `ELOOP`) is returned rather than read as absence: discovery that took an
/// unreadable `.git` for a missing one would ascend past the checkout it
/// cannot see and attribute the directory to an enclosing one.
fn entry_type(path: &Path) -> std::io::Result<Option<std::fs::FileType>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(error) if is_absence(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_dir_entry(path: &Path) -> std::io::Result<bool> {
    Ok(matches!(entry_type(path)?, Some(kind) if kind.is_dir()))
}

fn is_file_entry(path: &Path) -> std::io::Result<bool> {
    Ok(matches!(entry_type(path)?, Some(kind) if kind.is_file()))
}

/// The Git directory for a checkout root, or `None` when `repo_root` is not
/// one. Callers that only want an answer use this; an unreadable candidate is
/// logged and gives `None`, since none of them can do more with it.
pub(super) fn git_dir_for_repo_root(repo_root: &Path) -> Option<PathBuf> {
    match locate_git_dir(repo_root) {
        Ok(git_dir) => git_dir,
        Err(error) => {
            tracing::debug!(path = %repo_root.display(), %error, "git directory unreadable");
            None
        }
    }
}

/// [`git_dir_for_repo_root`] with a stat or read error kept apart from "not a
/// checkout root", so the discovery walk can stop instead of ascending.
fn locate_git_dir(repo_root: &Path) -> std::io::Result<Option<PathBuf>> {
    let git_path = repo_root.join(".git");
    match entry_type(&git_path)? {
        Some(kind) if kind.is_dir() => return Ok(Some(git_path)),
        Some(kind) if kind.is_file() => match std::fs::read_to_string(&git_path) {
            Ok(gitdir) => {
                if let Some(relative) = gitdir.trim().strip_prefix("gitdir:").map(str::trim) {
                    let resolved = Path::new(relative);
                    return Ok(Some(if resolved.is_absolute() {
                        resolved.to_path_buf()
                    } else {
                        repo_root.join(resolved)
                    }));
                }
            }
            // A `.git` file that is not UTF-8 is not a gitfile; one that
            // vanished since the stat is absent. Both fall through to the
            // bare-layout check, as a malformed `.git` file always has.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData || is_absence(&error) => {
            }
            Err(error) => return Err(error),
        },
        Some(_) | None => {}
    }

    if path_is_git_dir_layout(repo_root)? {
        let info = git_config_info(repo_root, repo_root)?;
        if git_dir_is_bare(&info)? {
            return Ok(Some(repo_root.to_path_buf()));
        }
    }

    Ok(None)
}

fn path_is_git_dir_layout(path: &Path) -> std::io::Result<bool> {
    Ok(is_file_entry(&path.join("HEAD"))?
        && is_dir_entry(&path.join("objects"))?
        && is_dir_entry(&path.join("refs"))?)
}

pub(super) fn git_symbolic_head_full(
    repo_root: &Path,
    errors: &mut Vec<GitReadError>,
) -> Option<String> {
    git_trimmed_stdout(repo_root, &["symbolic-ref", "--quiet", "HEAD"], errors)
}

#[cfg(test)]
pub(super) fn git_rev_parse_verify(repo_root: &Path, revision: &str) -> Option<String> {
    git_rev_parse_verify_with_errors(repo_root, revision, &mut Vec::new())
}

pub(super) fn git_rev_parse_verify_with_errors(
    repo_root: &Path,
    revision: &str,
    errors: &mut Vec<GitReadError>,
) -> Option<String> {
    git_trimmed_stdout(repo_root, &["rev-parse", "--verify", revision], errors)
}

/// Whether the repository keeps its refs in a reftable store. Git takes
/// `extensions.refstorage` from the common directory's `config` file alone,
/// as part of its repository format check: an include or the user's global
/// config cannot switch the ref backend, so neither is read here.
pub(super) fn git_ref_storage_is_reftable(
    info: &GitWorktreeInfo,
) -> io::Result<(bool, Vec<super::config::FileDep>)> {
    let config_path = info.git_common_dir.join("config");
    let result =
        super::config::read_repository_format_value(&config_path, "extensions", "refstorage");
    let (value, deps) = result?;
    Ok((
        value.is_some_and(|value| value.eq_ignore_ascii_case("reftable")),
        deps,
    ))
}

/// Whether `core.bare` resolves to true for this Git directory. Unlike the
/// repository format keys, Git's effective `core.bare` (what `git rev-parse
/// --is-bare-repository` reports from inside a Git directory) comes from the
/// whole config chain: the system config, the global config files, then the
/// repository's config, each with its includes, the last value winning.
/// `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM` and `GIT_CONFIG_GLOBAL` select
/// the same system and global sources Git uses. So a bare repository whose
/// `core.bare = true` sits in an included file or in a global config is still
/// bare here. The value takes Git's boolean grammar; a malformed one, which
/// Git refuses to run on, reads as not bare. One deliberate difference: Git
/// treats a Git directory it discovers as bare when `core.bare` is unset,
/// while this requires an explicit true, so a directory that merely looks
/// like a Git directory is walked past rather than taken as a repository
/// root.
fn git_dir_is_bare(info: &GitWorktreeInfo) -> io::Result<bool> {
    let branch = git_head_branch(&info.git_dir);
    let mut config_paths = super::config::git_user_config_paths_at(&info.repo_root);
    config_paths.push(info.git_dir.join("config"));
    let (value, _) =
        super::config::read_config_value(info, &branch, &config_paths, "core", "bare")?;
    Ok(value.as_deref().and_then(super::config::git_config_bool) == Some(true))
}

fn git_head_branch(git_dir: &Path) -> String {
    std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
        })
        .unwrap_or_default()
}

pub(super) fn git_trimmed_stdout(
    repo_root: &Path,
    args: &[&str],
    errors: &mut Vec<GitReadError>,
) -> Option<String> {
    let output = match run_git_output(repo_root, args) {
        Ok(output) => output,
        Err(error) => {
            errors.push(error);
            return None;
        }
    };
    if !output.status.success() {
        let expected_no_result = (args.first() == Some(&"symbolic-ref")
            && output.status.code() == Some(1))
            || (args.first() == Some(&"rev-parse")
                && String::from_utf8_lossy(&output.stderr).contains("Needed a single revision"));
        if !expected_no_result {
            errors.push(command_failed(repo_root, args, &output));
        }
        return None;
    }

    let stdout = match String::from_utf8(output.stdout) {
        Ok(stdout) => stdout,
        Err(_) => {
            errors.push(GitReadError::InvalidUtf8 {
                cwd: repo_root.to_path_buf(),
                arguments: args.join(" "),
            });
            return None;
        }
    };
    let stdout = stdout.trim();
    if stdout.is_empty() {
        errors.push(GitReadError::InvalidOutput {
            cwd: repo_root.to_path_buf(),
            arguments: args.join(" "),
            output: String::new(),
        });
        None
    } else {
        Some(stdout.to_string())
    }
}

/// Runs one Git probe through the shared platform runner, typing its failure
/// for the status refresh.
pub(super) fn run_git_output(cwd: &Path, args: &[&str]) -> Result<Output, GitReadError> {
    use shepr_platform::git::GitCommandError;
    shepr_platform::git::run_git(cwd, args).map_err(|error| match error {
        GitCommandError::Spawn(error) => GitReadError::Spawn {
            cwd: cwd.to_path_buf(),
            message: error.to_string(),
        },
        GitCommandError::TimedOut => GitReadError::TimedOut {
            cwd: cwd.to_path_buf(),
            arguments: args.join(" "),
        },
        GitCommandError::Process(error) => GitReadError::Process {
            cwd: cwd.to_path_buf(),
            message: error.to_string(),
        },
    })
}

fn command_failed(cwd: &Path, args: &[&str], output: &Output) -> GitReadError {
    GitReadError::CommandFailed {
        cwd: cwd.to_path_buf(),
        arguments: args.join(" "),
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The directories repository discovery does not ascend into, from
/// `GIT_CEILING_DIRECTORIES`, read with Git's semantics: a colon-separated
/// list in which relative entries are ignored, and entries after an empty one
/// are taken as spelled rather than resolved through symlinks (Git's escape
/// for slow network mounts). The starting directory is always examined, even
/// when it is a ceiling itself; only the walk upwards stops.
#[derive(Debug, Default)]
struct GitCeilings {
    /// Each entry as spelled and, where resolved, as its canonical path, so a
    /// walk from either spelling of a directory meets it.
    dirs: Vec<PathBuf>,
}

impl GitCeilings {
    /// This process's ceilings. A refused value (padded or not UTF-8) is
    /// logged and read as no ceiling, as Git ignores an entry it cannot use.
    fn from_env() -> Self {
        match shepr_core::env::read_text(shepr_core::env::EnvVar::GitCeilingDirectories) {
            Ok(value) => Self::parse(value.as_deref().unwrap_or_default()),
            Err(error) => {
                tracing::warn!(%error, "ignoring a refused environment value");
                Self::default()
            }
        }
    }

    fn parse(value: &str) -> Self {
        let mut resolve = true;
        let mut dirs = Vec::new();
        for entry in value.split(':') {
            if entry.is_empty() {
                resolve = false;
                continue;
            }
            let path = Path::new(entry);
            if !path.is_absolute() {
                continue;
            }
            if resolve && let Ok(real) = std::fs::canonicalize(path) {
                dirs.push(real);
            }
            dirs.push(path.to_path_buf());
        }
        Self { dirs }
    }

    fn contains(&self, dir: &Path) -> bool {
        self.dirs.iter().any(|ceiling| ceiling == dir)
    }
}

#[cfg(test)]
pub(super) fn git_repo_root(start: &Path) -> Option<PathBuf> {
    git_repo_root_below(start, &GitCeilings::from_env())
}

fn git_repo_root_with_errors(start: &Path, errors: &mut Vec<GitReadError>) -> Option<PathBuf> {
    git_repo_root_below_with_errors(start, &GitCeilings::from_env(), errors)
}

#[cfg(test)]
fn git_repo_root_below(start: &Path, ceilings: &GitCeilings) -> Option<PathBuf> {
    git_repo_root_below_with_errors(start, ceilings, &mut Vec::new())
}

/// The checkout root for `start`, with the ceilings handed in: the walk
/// examines `start` (or its parent, for a file) and each ancestor up to, not
/// including, the nearest ceiling. A directory whose Git state cannot be read
/// (a stat or read error other than absence) ends the walk with `None` and an
/// entry in `errors` rather than being passed over: ascending past it could
/// attribute `start` to an enclosing checkout it is not part of.
fn git_repo_root_below_with_errors(
    start: &Path,
    ceilings: &GitCeilings,
    errors: &mut Vec<GitReadError>,
) -> Option<PathBuf> {
    let mut current = match is_dir_entry(start) {
        Ok(true) => start.to_path_buf(),
        Ok(false) => start.parent()?.to_path_buf(),
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: start.to_path_buf(),
                message: error.to_string(),
            });
            return None;
        }
    };

    loop {
        let found = match locate_git_dir(&current) {
            Ok(Some(git_dir)) => match is_file_entry(&git_dir.join("HEAD")) {
                Ok(found) => found,
                Err(error) => {
                    errors.push(GitReadError::FileRead {
                        path: git_dir.join("HEAD"),
                        message: error.to_string(),
                    });
                    return None;
                }
            },
            Ok(None) => false,
            Err(error) => {
                errors.push(GitReadError::FileRead {
                    path: current.clone(),
                    message: error.to_string(),
                });
                return None;
            }
        };
        if found {
            return Some(current);
        }
        if !current.pop() || ceilings.contains(&current) {
            return None;
        }
    }
}

#[cfg(test)]
pub(super) fn read_ref_oid(common_dir: &Path, full_ref: &str) -> Option<String> {
    read_ref_oid_with_errors(common_dir, full_ref, &mut Vec::new())
}

pub(super) fn read_ref_oid_with_errors(
    common_dir: &Path,
    full_ref: &str,
    errors: &mut Vec<GitReadError>,
) -> Option<String> {
    let loose_ref = common_dir.join(full_ref);
    match read_git_ref_file_state(&loose_ref) {
        RefFileRead::Content(contents) => {
            let oid = contents.trim();
            if oid.is_empty() {
                errors.push(GitReadError::FileRead {
                    path: loose_ref.clone(),
                    message: "loose ref is empty".into(),
                });
                return None;
            }
            return Some(oid.to_string());
        }
        // A loose ref that exists - or whose existence cannot be ruled out
        // because of a metadata or I/O error - must not fall back to
        // packed-refs: that could resurrect a stale same-name OID into the
        // status fingerprint. Report the ref as unavailable instead.
        RefFileRead::Unavailable(message) => {
            errors.push(GitReadError::FileRead {
                path: loose_ref.clone(),
                message,
            });
            return None;
        }
        RefFileRead::Absent => {}
    }

    let packed_refs_path = common_dir.join("packed-refs");
    let packed_refs = match std::fs::read_to_string(&packed_refs_path) {
        Ok(contents) => contents,
        Err(error) if is_absence(&error) => return None,
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: packed_refs_path,
                message: error.to_string(),
            });
            return None;
        }
    };
    for line in packed_refs.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let oid = parts.next()?;
        let name = parts.next()?;
        if name == full_ref {
            return Some(oid.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::git::test_support::{
        add_linked_worktree, git_written_fixture, live_git_space, temp_test_dir, write_git_dir,
    };

    #[test]
    fn oversized_loose_ref_is_unavailable_not_absent() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("oversized-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).expect("test precondition");
        std::fs::write(
            root.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted \naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .expect("test precondition");
        let loose = refs_dir.join("main");
        std::fs::write(&loose, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n")
            .expect("test precondition");
        std::fs::OpenOptions::new()
            .write(true)
            .open(loose)
            .expect("test precondition")
            .set_len(8 * 1024 * 1024)
            .expect("test precondition");

        let oid = read_ref_oid(&root, "refs/heads/main");
        assert_eq!(
            oid, None,
            "an oversized loose ref must make the ref unavailable, not fall back to the stale packed OID"
        );
    }

    #[test]
    fn empty_or_whitespace_loose_ref_is_unavailable_not_absent() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("empty-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).expect("test precondition");
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .expect("test precondition");
        let loose = refs_dir.join("main");

        for contents in ["", " \n\t"] {
            std::fs::write(&loose, contents).expect("test precondition");
            assert_eq!(
                read_ref_oid(&root, "refs/heads/main"),
                None,
                "an empty or whitespace-only loose ref must not fall back to the stale packed OID"
            );
        }
    }

    #[test]
    fn dangling_symlink_loose_ref_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("dangling-symlink-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).expect("test precondition");
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .expect("test precondition");
        symlink("missing-target", refs_dir.join("main")).expect("test precondition");

        let oid = read_ref_oid(&root, "refs/heads/main");
        assert_eq!(
            oid, None,
            "a dangling loose ref must not fall back to the stale packed OID"
        );
    }

    #[test]
    fn dangling_symlink_through_file_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("dangling-symlink-through-file");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).expect("test precondition");
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .expect("test precondition");
        std::fs::write(refs_dir.join("target-parent"), "not a directory")
            .expect("test precondition");
        symlink("target-parent/nested", refs_dir.join("main")).expect("test precondition");

        let oid = read_ref_oid(&root, "refs/heads/main");
        assert_eq!(
            oid, None,
            "a dangling loose ref whose target traverses a file must not fall back to the stale packed OID"
        );
    }

    #[test]
    fn symlink_loop_loose_ref_is_unavailable_not_absent() {
        use std::os::unix::fs::symlink;

        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("symlink-loop-loose-ref");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).expect("test precondition");
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .expect("test precondition");
        // Unlike chmod(000), a symlink loop produces an open error even as root.
        // It exercises the same unavailable-ref branch as permission errors.
        let loose_ref = refs_dir.join("main");
        symlink("main", &loose_ref).expect("test precondition");
        let open_error = std::fs::File::open(&loose_ref).expect_err("test precondition");
        let oid = read_ref_oid(&root, "refs/heads/main");
        assert_eq!(open_error.raw_os_error(), Some(libc::ELOOP));
        assert_eq!(
            oid, None,
            "a loose ref behind an open error must be unavailable, not fall back to the stale packed OID"
        );
    }

    #[test]
    fn ref_path_through_a_file_still_reads_packed_refs() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("ref-path-through-file");
        let refs_dir = root.join("refs/heads");
        std::fs::create_dir_all(&refs_dir).expect("test precondition");
        // refs/heads/main is a file, so refs/heads/main/nested cannot exist as
        // a loose ref; the packed entry is the legitimate source.
        std::fs::write(
            refs_dir.join("main"),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        )
        .expect("test precondition");
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main/nested\n",
        )
        .expect("test precondition");

        let oid = read_ref_oid(&root, "refs/heads/main/nested");
        assert_eq!(
            oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn absent_loose_ref_still_reads_packed_refs() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("packed-only-ref");
        std::fs::create_dir_all(root.join("refs/heads")).expect("test precondition");
        std::fs::write(
            root.join("packed-refs"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa refs/heads/main\n",
        )
        .expect("test precondition");

        let oid = read_ref_oid(&root, "refs/heads/main");
        assert_eq!(
            oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn git_repo_root_ignores_invalid_git_marker() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("invalid-git-root");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(base.join(".git")).expect("test precondition");
        std::fs::create_dir_all(&cwd).expect("test precondition");

        assert_eq!(git_repo_root(&cwd), None);
    }

    /// A directory discovery recognises as a checkout root.
    fn mark_checkout(root: &Path) {
        std::fs::create_dir_all(root.join(".git")).expect("test precondition");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test precondition");
    }

    fn ceilings(value: &str) -> GitCeilings {
        GitCeilings::parse(value)
    }

    /// `outer` is a checkout; `outer/ceiling/work` sits below a ceiling.
    fn checkout_with_ceiling_below(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let outer = temp_test_dir(name);
        mark_checkout(&outer);
        let ceiling = outer.join("ceiling");
        let work = ceiling.join("work");
        std::fs::create_dir_all(&work).expect("test precondition");
        (outer, ceiling, work)
    }

    #[test]
    fn discovery_does_not_ascend_into_or_above_a_ceiling() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (outer, ceiling, work) = checkout_with_ceiling_below("ceiling-stops-walk");
        let spelled = ceiling.to_str().expect("test precondition");

        assert_eq!(
            git_repo_root_below(&work, &GitCeilings::default()),
            Some(outer.clone())
        );
        assert_eq!(git_repo_root_below(&work, &ceilings(spelled)), None);
        // A file's directory is where the walk starts.
        let file = work.join("file");
        std::fs::write(&file, "").expect("test precondition");
        assert_eq!(git_repo_root_below(&file, &ceilings(spelled)), None);
        // One ceiling in a list is enough, wherever it sits in it.
        assert_eq!(
            git_repo_root_below(&work, &ceilings(&format!("/nonexistent/a:{spelled}:/b"))),
            None
        );
        // A ceiling that is not an ancestor does not stop the walk.
        assert_eq!(
            git_repo_root_below(&work, &ceilings("/nonexistent/elsewhere")),
            Some(outer)
        );
    }

    #[test]
    fn a_checkout_below_a_ceiling_and_a_ceiling_start_are_still_found() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, ceiling, work) = checkout_with_ceiling_below("ceiling-below");
        let spelled = ceiling.to_str().expect("test precondition");
        let inner = work.join("inner");
        mark_checkout(&work);
        std::fs::create_dir_all(&inner).expect("test precondition");

        assert_eq!(
            git_repo_root_below(&inner, &ceilings(spelled)),
            Some(work.clone())
        );
        // Git never excludes the starting directory, even a ceiling itself.
        let work_spelled = work.to_str().expect("test precondition");
        assert_eq!(
            git_repo_root_below(&work, &ceilings(work_spelled)),
            Some(work)
        );
    }

    #[test]
    fn relative_ceiling_entries_are_ignored() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (outer, _, work) = checkout_with_ceiling_below("ceiling-relative");

        assert_eq!(
            git_repo_root_below(&work, &ceilings("ceiling:./ceiling::")),
            Some(outer)
        );
    }

    /// Entries are resolved through symlinks, so a ceiling spelled through a
    /// link stops a walk along the real path; after an empty entry they are
    /// taken as spelled, as Git takes them.
    #[test]
    fn ceilings_resolve_symlinks_until_an_empty_entry() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (outer, ceiling, work) = checkout_with_ceiling_below("ceiling-symlink");
        let link = outer.join("link");
        std::os::unix::fs::symlink(&ceiling, &link).expect("test precondition");
        let spelled = link.to_str().expect("test precondition");

        assert_eq!(git_repo_root_below(&work, &ceilings(spelled)), None);
        assert_eq!(
            git_repo_root_below(&work, &ceilings(&format!(":{spelled}"))),
            Some(outer)
        );
    }

    #[test]
    fn git_repo_root_reads_the_ceiling_from_the_environment() {
        let env = shepr_test_support::IsolatedEnv::new();
        let (outer, ceiling, work) = checkout_with_ceiling_below("ceiling-env");

        env.set(
            shepr_core::env::EnvVar::GitCeilingDirectories,
            format!("/nonexistent/a:{}", ceiling.display()),
        );
        assert_eq!(git_repo_root(&work), None);

        env.remove(shepr_core::env::EnvVar::GitCeilingDirectories);
        assert_eq!(git_repo_root(&work), Some(outer));
    }

    /// The isolation guard's ceiling keeps a scratch directory that is not a
    /// checkout from being discovered as part of the checkout the scratch
    /// base sits in.
    #[test]
    fn a_scratch_directory_is_not_inside_the_enclosing_checkout() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let plain = temp_test_dir("ceiling-scratch");

        assert_eq!(git_repo_root(&plain), None);
    }

    #[test]
    fn git_repo_root_ignores_standalone_non_bare_git_dir_layout() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("standalone-non-bare-git-dir");
        std::fs::write(root.join("HEAD"), "ref: refs/heads/main\n").expect("test precondition");
        std::fs::create_dir_all(root.join("objects")).expect("test precondition");
        std::fs::create_dir_all(root.join("refs")).expect("test precondition");
        std::fs::write(root.join("config"), "[core]\n\tbare = false\n").expect("test precondition");

        assert_eq!(git_repo_root(&root.join("refs")), None);
    }

    #[test]
    fn git_space_metadata_supports_standalone_bare_repo() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let bare = temp_test_dir("bare-space");
        write_git_dir(&bare, "main", true);
        let nested = bare.join("refs");

        let info = git_worktree_info(&nested).expect("bare repo should be discovered");
        assert_eq!(git_repo_root(&nested), Some(bare.clone()));
        assert!(!info.is_linked_worktree);
        assert_eq!(info.git_dir, canonicalize_best_effort_path(&bare));

        let metadata = live_git_space(&nested).expect("bare repo should map to a git space");
        assert_eq!(
            canonicalize_best_effort_path(&metadata.repo_root),
            canonicalize_best_effort_path(&bare)
        );
        assert!(!metadata.is_linked_worktree);
    }

    #[test]
    fn bare_source_and_linked_checkout_share_repo_name_but_not_auto_label() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, bare, checkout) =
            crate::git::test_support::create_bare_repo_with_linked_worktree("bare-linked-labels");

        let bare_space = live_git_space(&bare).expect("test precondition");
        let checkout_space = live_git_space(&checkout).expect("test precondition");
        let bare_auto_label = automatic_workspace_label(&bare, &bare_space.repo_root);
        let checkout_auto_label = automatic_workspace_label(&checkout, &checkout_space.repo_root);

        assert_eq!(bare_space.key, checkout_space.key);
        assert_eq!(bare_space.repo_name, ".bare");
        assert_eq!(checkout_space.repo_name, bare_space.repo_name);
        assert_eq!(
            bare_auto_label,
            bare.file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );
        assert_eq!(
            checkout_auto_label,
            checkout
                .file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );
    }

    #[test]
    fn embedded_dot_bare_source_and_checkout_use_container_repo_name() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("embedded-dot-bare");
        let repo = base.join("reported-repo");
        let bare = repo.join(".bare");
        let checkout = repo.join("develop");
        write_git_dir(&bare, "main", true);
        std::fs::write(repo.join(".git"), "gitdir: ./.bare\n").expect("test precondition");
        add_linked_worktree(&bare, "develop", &checkout);

        let source = live_git_space(&repo).expect("test precondition");
        let linked = live_git_space(&checkout).expect("test precondition");

        assert_eq!(source.repo_name, "reported-repo");
        assert_eq!(linked.repo_name, source.repo_name);
    }

    #[test]
    fn git_space_metadata_marks_bare_dot_git_repo() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("bare-dot-git");
        write_git_dir(&root.join(".git"), "main", true);

        let info = git_worktree_info(&root).expect("bare .git repo should be discovered");
        assert_eq!(git_repo_root(&root), Some(root.clone()));
        assert!(!info.is_linked_worktree);
        assert_eq!(
            info.git_dir,
            canonicalize_best_effort_path(&root.join(".git"))
        );

        let metadata = live_git_space(&root).expect("bare .git repo should map to a git space");
        assert_eq!(
            canonicalize_best_effort_path(&metadata.repo_root),
            canonicalize_best_effort_path(&root)
        );
    }

    #[test]
    fn derive_label_prefers_repo_root_name() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("label-repo");
        let nested = root.join("nested");
        std::fs::create_dir_all(root.join(".git")).expect("test precondition");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test precondition");
        std::fs::create_dir_all(&nested).expect("test precondition");

        assert_eq!(
            derive_label_from_cwd(&nested),
            root.file_name()
                .and_then(|name| name.to_str())
                .expect("test precondition")
        );
    }

    #[test]
    fn derive_label_uses_path_name_outside_git() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("label-plain");
        let label = root
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test precondition");

        assert_eq!(derive_label_from_cwd(Path::new(&root)), label);
    }

    /// Production reads a reftable store through Git, and the store is a
    /// binary format only Git writes, so Git makes this fixture.
    #[test]
    fn git_rev_parse_verify_reads_reftable_refs() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("reftable-ref-oid");
        // host-program-ok: a reftable store is written by Git; production reads it through Git
        let output = run_git_output(&root, &["init", "--ref-format=reftable", "-b", "main"])
            .expect("test precondition");
        assert!(
            output.status.success(),
            "this test needs a host Git with reftable support (2.45 or later): {output:?}"
        );

        git_written_fixture(&root, &["config", "user.email", "shepr@example.invalid"]);
        git_written_fixture(&root, &["config", "user.name", "Shepr Test"]);
        git_written_fixture(&root, &["commit", "--allow-empty", "-m", "initial"]);

        let head_oid = git_rev_parse_verify(&root, "HEAD").expect("test precondition");

        assert_eq!(
            git_rev_parse_verify(&root, "refs/heads/main").as_deref(),
            Some(head_oid.as_str())
        );
    }
}
