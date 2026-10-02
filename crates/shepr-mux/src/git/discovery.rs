pub(super) use crate::limits::MAX_GIT_REF_FILE_BYTES;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use super::GitReadError;
use super::identity::{FullRefName, Oid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitWorktreeInfo {
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub git_common_dir: PathBuf,
}

pub(super) enum Discovery {
    Checkout(GitWorktreeInfo),
    Outside,
    Unreadable(GitReadError),
}

struct LocatedGitDir {
    path: PathBuf,
    from_gitfile: bool,
}

/// The label for a cwd outside any Git checkout: `~` for the home directory,
/// the directory name otherwise. This runs when a workspace's identity cwd
/// changes or its Git status is refreshed, never per frame, so `$HOME` is
/// read here. Shared fallback naming stays in core; this wrapper only supplies
/// the resolved home. An unusable `HOME` only means the `~` label is not offered.
pub fn fallback_label_from_cwd(cwd: &Path) -> String {
    let home = shepr_core::pathutil::home_dir().ok();
    shepr_core::workspace_label::workspace_label_from_cwd(cwd, None, home.as_deref())
}

pub(crate) fn git_worktree_info(cwd: &Path) -> Option<GitWorktreeInfo> {
    git_worktree_info_with_errors(cwd, &mut Vec::new())
}

/// Existing mux callers keep their `Option` plus accumulated-error boundary;
/// this adapter delegates repository classification to the shared discovery.
pub(super) fn git_worktree_info_with_errors(
    cwd: &Path,
    errors: &mut Vec<GitReadError>,
) -> Option<GitWorktreeInfo> {
    match discover(cwd) {
        Discovery::Checkout(info) => Some(info),
        Discovery::Outside => None,
        Discovery::Unreadable(error) => {
            errors.push(error);
            None
        }
    }
}

pub(super) fn discover(cwd: &Path) -> Discovery {
    discover_below(cwd, &GitCeilings::from_env())
}

pub fn discover_checkout_root(cwd: &Path) -> Result<Option<PathBuf>, GitReadError> {
    match discover(cwd) {
        Discovery::Checkout(info) => Ok(Some(canonicalize_best_effort_path(&info.repo_root))),
        Discovery::Outside => Ok(None),
        Discovery::Unreadable(error) => Err(error),
    }
}

fn discover_below(cwd: &Path, ceilings: &GitCeilings) -> Discovery {
    let location = match git_worktree_location_below(cwd, ceilings) {
        Ok(Some(location)) => location,
        Ok(None) => return Discovery::Outside,
        Err(error) => return Discovery::Unreadable(error),
    };
    let (repo_root, located) = location;
    match git_config_info(&repo_root, &located.path) {
        Ok(info) => Discovery::Checkout(info),
        Err(error) => Discovery::Unreadable(GitReadError::FileRead {
            path: located.path.join("commondir"),
            message: error.to_string(),
        }),
    }
}

/// Inside a Git checkout the label is the checkout root's name; the home
/// directory is never consulted, so none is resolved.
pub(crate) fn automatic_workspace_label(cwd: &Path, repo_root: &Path) -> String {
    shepr_core::workspace_label::workspace_label_from_cwd(cwd, Some(repo_root), None)
}

pub(super) fn canonicalize_best_effort_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The common directory a Git directory shares its refs with: itself unless
/// a `commondir` file names another. `None` when `commondir` exists but
/// cannot be read: taking the Git directory as its own common directory then
/// would read linked-worktree refs from the wrong directory.
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

/// [`locate_git_dir`] with filesystem and Git probe errors kept apart from
/// "not a checkout root", so the discovery walk can stop instead of ascending.
fn locate_git_dir(repo_root: &Path) -> Result<Option<LocatedGitDir>, GitReadError> {
    let git_path = repo_root.join(".git");
    match entry_type(&git_path).map_err(|error| file_read_error(&git_path, &error))? {
        Some(kind) if kind.is_dir() => {
            return Ok(Some(LocatedGitDir {
                path: git_path,
                from_gitfile: false,
            }));
        }
        Some(kind) if kind.is_file() => {
            // A regular `.git` file claims to be a gitfile. Any failure to
            // read its target is an invalid marker, not a reason to ascend.
            let gitdir = std::fs::read_to_string(&git_path)
                .map_err(|error| file_read_error(&git_path, &error))?;
            let Some(relative) = gitdir
                .trim()
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|relative| !relative.is_empty())
            else {
                return Err(GitReadError::FileRead {
                    path: git_path,
                    message: "gitfile has no gitdir target".into(),
                });
            };
            let resolved = Path::new(relative);
            return Ok(Some(LocatedGitDir {
                path: if resolved.is_absolute() {
                    resolved.to_path_buf()
                } else {
                    repo_root.join(resolved)
                },
                from_gitfile: true,
            }));
        }
        Some(_) | None => {}
    }

    if path_is_git_dir_layout(repo_root).map_err(|error| file_read_error(repo_root, &error))? {
        let info = git_config_info(repo_root, repo_root)
            .map_err(|error| file_read_error(&repo_root.join("commondir"), &error))?;
        if git_dir_is_bare(&info)? {
            return Ok(Some(LocatedGitDir {
                path: repo_root.to_path_buf(),
                from_gitfile: false,
            }));
        }
    }

    Ok(None)
}

fn file_read_error(path: &Path, error: &std::io::Error) -> GitReadError {
    GitReadError::FileRead {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn git_head_file_is_readable(git_dir: &LocatedGitDir) -> std::io::Result<bool> {
    let head = git_dir.path.join("HEAD");
    let is_file = is_file_entry(&head)?;
    // Git can skip a `.git` directory without HEAD, but a gitfile target must
    // identify a usable repository and cannot be treated as absent.
    if !is_file && git_dir.from_gitfile {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "gitfile target has no regular HEAD file",
        ));
    }
    if is_file && git_dir.from_gitfile {
        drop(std::fs::File::open(head)?);
    }
    Ok(is_file)
}

fn path_is_git_dir_layout(path: &Path) -> std::io::Result<bool> {
    Ok(is_file_entry(&path.join("HEAD"))?
        && is_dir_entry(&path.join("objects"))?
        && is_dir_entry(&path.join("refs"))?)
}

pub(super) enum SymbolicHeadProbe {
    Output(FullRefName),
    NoOutput,
    InvalidOutput,
}

pub(super) fn git_symbolic_head_full(
    repo_root: &Path,
    errors: &mut Vec<GitReadError>,
) -> SymbolicHeadProbe {
    let args = ["symbolic-ref", "--quiet", "HEAD"];
    let Some(output) = git_trimmed_stdout(
        repo_root,
        &args,
        |output| output.status.code() == Some(1),
        errors,
    ) else {
        return SymbolicHeadProbe::NoOutput;
    };
    match FullRefName::parse(&output) {
        Some(full_ref) => SymbolicHeadProbe::Output(full_ref),
        None => {
            errors.push(GitReadError::InvalidOutput {
                cwd: repo_root.to_path_buf(),
                arguments: args.join(" "),
                output,
            });
            SymbolicHeadProbe::InvalidOutput
        }
    }
}

pub(super) fn git_rev_parse_verify_with_errors(
    repo_root: &Path,
    revision: &str,
    errors: &mut Vec<GitReadError>,
) -> Option<Oid> {
    let args = ["rev-parse", "--verify", "--end-of-options", revision];
    let output = git_trimmed_stdout(
        repo_root,
        &args,
        |output| String::from_utf8_lossy(&output.stderr).contains("Needed a single revision"),
        errors,
    )?;
    match Oid::parse(&output) {
        Some(oid) => Some(oid),
        None => {
            errors.push(GitReadError::InvalidOutput {
                cwd: repo_root.to_path_buf(),
                arguments: args.join(" "),
                output,
            });
            None
        }
    }
}

/// Whether the repository keeps its refs in a reftable store. Git takes
/// `extensions.refstorage` from the common directory's `config` file alone,
/// as part of its repository format check: an include or the user's global
/// config cannot switch the ref backend, so neither is read here.
pub(super) fn git_ref_storage_is_reftable(
    info: &GitWorktreeInfo,
) -> Result<(super::RefBackend, super::config::Dependencies), GitReadError> {
    let config_path = info.git_common_dir.join("config");
    let result =
        super::config::read_repository_format_value(&config_path, "extensions", "refstorage");
    let (value, deps) = result?;
    Ok((
        if value.is_some_and(|value| value.eq_ignore_ascii_case("reftable")) {
            super::RefBackend::Reftable
        } else {
            super::RefBackend::Files
        },
        deps,
    ))
}

/// Git resolves effective core.bare, including its own config grammar and
/// conditional includes. Require explicit true rather than treating an
/// unconfigured directory with a Git-like layout as a bare repository.
fn git_dir_is_bare(info: &GitWorktreeInfo) -> Result<bool, GitReadError> {
    super::config::read_bare(info)
}

pub(super) fn git_trimmed_stdout(
    repo_root: &Path,
    args: &[&str],
    is_absent: impl FnOnce(&Output) -> bool,
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
        if !is_absent(&output) {
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

/// Runs one Git probe through the mux runner, typing its failure for the
/// status refresh.
pub(super) fn run_git_output(cwd: &Path, args: &[&str]) -> Result<Output, GitReadError> {
    use super::GitCommandError;
    super::run_git(cwd, args).map_err(|error| match error {
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

pub(super) fn command_failed(cwd: &Path, args: &[&str], output: &Output) -> GitReadError {
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
    /// This process's ceilings, parsed from Git's colon-separated OS bytes.
    fn from_env() -> Self {
        match shepr_core::env::read_os(shepr_core::env::EnvVar::GitCeilingDirectories) {
            Ok(Some(value)) => Self::parse(&value),
            Ok(None) => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "failed to read Git ceiling directories");
                Self::default()
            }
        }
    }

    fn parse(value: &OsStr) -> Self {
        let mut resolve = true;
        let mut dirs = Vec::new();
        for entry in value.as_bytes().split(|byte| *byte == b':') {
            if entry.is_empty() {
                resolve = false;
                continue;
            }
            let entry = OsStr::from_bytes(entry);
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

/// The checkout root and its located Git directory come from the same walk,
/// so discovery consumers do not locate the final marker a second time.
fn git_worktree_location_below(
    start: &Path,
    ceilings: &GitCeilings,
) -> Result<Option<(PathBuf, LocatedGitDir)>, GitReadError> {
    let mut current = match is_dir_entry(start) {
        Ok(true) => start.to_path_buf(),
        Ok(false) => match start.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Ok(None),
        },
        Err(error) => return Err(file_read_error(start, &error)),
    };

    loop {
        let found = match locate_git_dir(&current)? {
            Some(git_dir) => {
                if git_head_file_is_readable(&git_dir)
                    .map_err(|error| file_read_error(&git_dir.path.join("HEAD"), &error))?
                {
                    Some(git_dir)
                } else {
                    None
                }
            }
            None => None,
        };
        if let Some(git_dir) = found {
            return Ok(Some((current, git_dir)));
        }
        if !current.pop() || ceilings.contains(&current) {
            return Ok(None);
        }
    }
}

/// Reads a ref only after its repository-controlled name has been validated
/// and kept as a full-ref type.
pub(super) fn read_ref_oid_for_full_ref(
    common_dir: &Path,
    full_ref: &FullRefName,
    errors: &mut Vec<GitReadError>,
) -> Option<Oid> {
    let loose_ref = common_dir.join(full_ref.as_str());
    match read_git_ref_file_state(&loose_ref) {
        RefFileRead::Content(contents) => {
            let Some(oid) = Oid::parse(contents.trim()) else {
                errors.push(GitReadError::FileRead {
                    path: loose_ref,
                    message: "loose ref is not a complete object ID".into(),
                });
                return None;
            };
            return Some(oid);
        }
        // An existing but unavailable loose ref must not resurrect a stale
        // packed OID. Symbolic loose refs are reported unavailable too.
        RefFileRead::Unavailable(message) => {
            errors.push(GitReadError::FileRead {
                path: loose_ref,
                message,
            });
            return None;
        }
        RefFileRead::Absent => {}
    }

    // Packed refs can exceed the small loose-ref cap. Stream bounded lines
    // instead of allocating the entire file or an unbounded malformed line.
    use std::io::BufRead;
    let packed_path = common_dir.join("packed-refs");
    let file = match std::fs::File::open(&packed_path) {
        Ok(file) => file,
        Err(error) if is_absence(&error) => return None,
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: packed_path,
                message: error.to_string(),
            });
            return None;
        }
    };
    let mut reader = std::io::BufReader::new(file);
    loop {
        let mut bytes = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_GIT_REF_FILE_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes);
        match read {
            Ok(0) => return None,
            Ok(_) if bytes.len() <= MAX_GIT_REF_FILE_BYTES => {}
            Ok(_) => {
                errors.push(GitReadError::FileRead {
                    path: packed_path,
                    message: "packed ref line is too large".into(),
                });
                return None;
            }
            Err(error) => {
                errors.push(GitReadError::FileRead {
                    path: packed_path,
                    message: error.to_string(),
                });
                return None;
            }
        }
        let Ok(line) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let mut parts = line.split_whitespace();
        let (Some(oid), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name == full_ref.as_str() {
            if let Some(oid) = Oid::parse(oid)
                && parts.next().is_none()
            {
                return Some(oid);
            }
            errors.push(GitReadError::FileRead {
                path: packed_path,
                message: "packed ref is not a complete object ID".into(),
            });
            return None;
        }
    }
}

#[cfg(test)]
fn git_worktree_location_below_with_errors(
    start: &Path,
    ceilings: &GitCeilings,
    errors: &mut Vec<GitReadError>,
) -> Option<(PathBuf, LocatedGitDir)> {
    match git_worktree_location_below(start, ceilings) {
        Ok(location) => location,
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

#[cfg(test)]
pub(super) fn read_ref_oid_with_errors(
    common_dir: &Path,
    full_ref: &str,
    errors: &mut Vec<GitReadError>,
) -> Option<String> {
    let Some(full_ref) = FullRefName::parse(full_ref) else {
        errors.push(GitReadError::FileRead {
            path: common_dir.to_path_buf(),
            message: "invalid ref name".into(),
        });
        return None;
    };
    read_ref_oid_for_full_ref(common_dir, &full_ref, errors).map(|oid| oid.as_str().to_owned())
}

#[cfg(test)]
fn derive_label_from_cwd(cwd: &Path) -> String {
    match git_repo_root(cwd) {
        Some(repo_root) => automatic_workspace_label(cwd, &repo_root),
        None => fallback_label_from_cwd(cwd),
    }
}

#[cfg(test)]
pub(super) fn git_rev_parse_verify(repo_root: &Path, revision: &str) -> Option<String> {
    git_rev_parse_verify_with_errors(repo_root, revision, &mut Vec::new())
        .map(|oid| oid.as_str().to_owned())
}

#[cfg(test)]
pub(super) fn git_repo_root(start: &Path) -> Option<PathBuf> {
    git_repo_root_below(start, &GitCeilings::from_env())
}

#[cfg(test)]
fn git_repo_root_below(start: &Path, ceilings: &GitCeilings) -> Option<PathBuf> {
    git_repo_root_below_with_errors(start, ceilings, &mut Vec::new())
}

#[cfg(test)]
pub(super) fn read_ref_oid(common_dir: &Path, full_ref: &str) -> Option<String> {
    read_ref_oid_with_errors(common_dir, full_ref, &mut Vec::new())
}

/// The checkout root for `start`, with the ceilings handed in: the walk
/// examines `start` (or its parent, for a file) and each ancestor up to, not
/// including, the nearest ceiling. A directory whose Git state cannot be read
/// (a stat or read error other than absence) ends the walk with `None` and an
/// entry in `errors` rather than being passed over: ascending past it could
/// attribute `start` to an enclosing checkout it is not part of.
#[cfg(test)]
fn git_repo_root_below_with_errors(
    start: &Path,
    ceilings: &GitCeilings,
    errors: &mut Vec<GitReadError>,
) -> Option<PathBuf> {
    git_worktree_location_below_with_errors(start, ceilings, errors).map(|(repo_root, _)| repo_root)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::git::test_support::{
        add_linked_worktree, git_written_fixture, temp_test_dir, write_git_dir,
    };

    #[test]
    fn ref_reader_rejects_paths_and_revision_expressions() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("invalid-ref-inputs");
        std::fs::create_dir_all(root.join("refs/heads")).expect("refs");
        for name in [
            "/etc/passwd",
            "refs/heads/../../config",
            "refs/heads//main",
            "HEAD",
        ] {
            let mut errors = Vec::new();
            assert!(read_ref_oid_with_errors(&root, name, &mut errors).is_none());
            assert!(!errors.is_empty());
        }
        for value in ["--output=owned", "HEAD", "aabbcc", "ref: refs/heads/other"] {
            std::fs::write(root.join("refs/heads/main"), value).expect("loose ref");
            let mut errors = Vec::new();
            assert!(read_ref_oid_with_errors(&root, "refs/heads/main", &mut errors).is_none());
            assert!(!errors.is_empty());
        }
        assert!(Oid::parse(&"a".repeat(40)).is_some());
        assert!(Oid::parse(&"0".repeat(64)).is_some());
        assert!(Oid::parse(&"A".repeat(40)).is_none());
    }

    #[test]
    fn packed_reader_skips_incomplete_lines_and_rejects_invalid_oids() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("packed-ref-validation");
        let packed = root.join("packed-refs");
        std::fs::write(
            &packed,
            format!("incomplete\n{} refs/heads/main\n", "a".repeat(40)),
        )
        .expect("packed refs");
        assert_eq!(read_ref_oid(&root, "refs/heads/main"), Some("a".repeat(40)));
        std::fs::write(&packed, "--output=owned refs/heads/main\n").expect("packed refs");
        assert!(read_ref_oid(&root, "refs/heads/main").is_none());
        std::fs::write(&packed, "a".repeat(MAX_GIT_REF_FILE_BYTES + 1)).expect("oversized line");
        assert!(read_ref_oid(&root, "refs/heads/main").is_none());
    }

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

    #[test]
    fn invalid_gitfiles_stop_before_an_enclosing_checkout() {
        let outer = temp_test_dir("invalid-gitfile-enclosing-repo");
        mark_checkout(&outer);

        let cases: [(&str, &[u8]); 4] = [
            ("missing-target", b"gitdir: absent-admin\n"),
            ("missing-directive", b"not a gitfile\n"),
            ("non-utf8", b"\xff"),
            ("non-file-head", b"gitdir: admin\n"),
        ];
        for (name, contents) in cases {
            let checkout = outer.join(".worktrees").join(name);
            std::fs::create_dir_all(&checkout).expect("test precondition");
            std::fs::write(checkout.join(".git"), contents).expect("test precondition");
            if name == "non-file-head" {
                std::fs::create_dir_all(checkout.join("admin/HEAD")).expect("test precondition");
            }

            let mut errors = Vec::new();
            assert_eq!(
                git_repo_root_below_with_errors(&checkout, &GitCeilings::default(), &mut errors),
                None,
                "invalid gitfile in {name} must stop discovery"
            );
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, GitReadError::FileRead { .. })),
                "invalid gitfile in {name} must be reported: {errors:?}"
            );
        }
    }

    #[test]
    fn git_directory_without_head_can_be_skipped_for_an_enclosing_checkout() {
        let outer = temp_test_dir("git-directory-without-head");
        mark_checkout(&outer);
        let nested = outer.join(".worktrees/no-head");
        std::fs::create_dir_all(nested.join(".git")).expect("test precondition");

        assert_eq!(
            git_repo_root_below(&nested, &GitCeilings::default()),
            Some(outer)
        );
    }

    /// A directory discovery recognises as a checkout root.
    fn mark_checkout(root: &Path) {
        std::fs::create_dir_all(root.join(".git")).expect("test precondition");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test precondition");
    }

    fn ceilings(value: &str) -> GitCeilings {
        GitCeilings::parse(OsStr::new(value))
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
    fn git_worktree_info_finds_standalone_bare_repo_root() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let bare = temp_test_dir("bare-space");
        write_git_dir(&bare, "main", true);
        let nested = bare.join("refs");

        let info = git_worktree_info(&nested).expect("bare repo should be discovered");
        assert_eq!(git_repo_root(&nested), Some(bare.clone()));
        assert_eq!(info.git_dir, canonicalize_best_effort_path(&bare));
        assert_eq!(info.repo_root, bare);
    }

    #[test]
    fn bare_source_and_linked_checkout_labels_use_each_checkout_root() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, bare, checkout) =
            crate::git::test_support::create_bare_repo_with_linked_worktree("bare-linked-labels");

        let bare_info = git_worktree_info(&bare).expect("test precondition");
        let checkout_info = git_worktree_info(&checkout).expect("test precondition");
        let bare_auto_label = automatic_workspace_label(&bare, &bare_info.repo_root);
        let checkout_auto_label = automatic_workspace_label(&checkout, &checkout_info.repo_root);

        assert_eq!(bare_info.repo_root, bare);
        assert_eq!(checkout_info.repo_root, checkout);
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
    fn embedded_dot_bare_source_and_checkout_labels_use_each_checkout_root() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("embedded-dot-bare");
        let repo = base.join("reported-repo");
        let bare = repo.join(".bare");
        let checkout = repo.join("develop");
        write_git_dir(&bare, "main", true);
        std::fs::write(repo.join(".git"), "gitdir: ./.bare\n").expect("test precondition");
        add_linked_worktree(&bare, "develop", &checkout);

        let source = git_worktree_info(&repo).expect("test precondition");
        let linked = git_worktree_info(&checkout).expect("test precondition");

        assert_eq!(source.repo_root, repo);
        assert_eq!(linked.repo_root, checkout);
        assert_eq!(
            automatic_workspace_label(&repo, &source.repo_root),
            "reported-repo"
        );
        assert_eq!(
            automatic_workspace_label(&checkout, &linked.repo_root),
            "develop"
        );
    }

    #[test]
    fn git_worktree_info_finds_bare_dot_git_repo_root() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("bare-dot-git");
        write_git_dir(&root.join(".git"), "main", true);

        let info = git_worktree_info(&root).expect("bare .git repo should be discovered");
        assert_eq!(git_repo_root(&root), Some(root.clone()));
        assert_eq!(
            info.git_dir,
            canonicalize_best_effort_path(&root.join(".git"))
        );
        assert_eq!(info.repo_root, root);
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
