pub(super) use crate::limits::MAX_GIT_REF_FILE_BYTES;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use super::identity::{FullRefName, Oid};
use super::{FileReadReason, GitReadError};

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

/// Status callers keep their `Option` plus accumulated-error boundary;
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
            reason: FileReadReason::from(&error),
        }),
    }
}

pub(super) fn canonicalize_best_effort_path(path: &Path) -> PathBuf {
    crate::access::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The common directory a Git directory shares its refs with: itself unless
/// a `commondir` file names another. An error when `commondir` exists but
/// cannot be read: taking the Git directory as its own common directory then
/// would read linked-worktree refs from the wrong directory.
fn git_common_dir_for_git_dir(git_dir: &Path) -> io::Result<PathBuf> {
    let commondir = git_dir.join("commondir");
    let contents = match crate::access::read_to_string(&commondir) {
        Ok(contents) => contents,
        Err(error) if is_absence(&error) => return Ok(git_dir.to_path_buf()),
        Err(error) => return Err(error),
    };
    // setup.c removes line endings from commondir, not surrounding path
    // whitespace. Leading or trailing spaces and tabs can be path bytes.
    let mut end = contents.len();
    while end > 0 && matches!(contents.as_bytes()[end - 1], b'\r' | b'\n') {
        end -= 1;
    }
    let path = Path::new(&contents[..end]);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    })
}

fn git_config_info(repo_root: &Path, git_dir: &Path) -> io::Result<GitWorktreeInfo> {
    let repo_root = repo_root.to_path_buf();
    let git_dir = canonicalize_best_effort_path(git_dir);
    let git_common_dir = git_common_dir_for_git_dir(&git_dir)?;
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
    Unavailable(FileReadReason),
}

pub(super) fn read_git_ref_file_state(path: &Path) -> RefFileRead {
    let file = match crate::access::open(path) {
        Ok(file) => file,
        Err(error) if is_absence(&error) => {
            return match crate::access::symlink_metadata(path) {
                Err(metadata_error) if is_absence(&metadata_error) => RefFileRead::Absent,
                // An entry that exists has a symlink target that is missing or
                // traverses a non-directory: Git treats the loose ref as broken
                // and does not fall back to an older packed ref. Any other stat
                // error leaves the ref's identity unknown.
                Ok(_) => RefFileRead::Unavailable(FileReadReason::from(&error)),
                Err(metadata_error) => {
                    RefFileRead::Unavailable(FileReadReason::from(&metadata_error))
                }
            };
        }
        // Permission or I/O errors: the ref may exist, so its identity is
        // unavailable rather than absent.
        Err(error) => return RefFileRead::Unavailable(FileReadReason::from(&error)),
    };
    let mut contents = String::new();
    if let Err(error) = file
        .take((MAX_GIT_REF_FILE_BYTES + 1) as u64)
        .read_to_string(&mut contents)
    {
        return RefFileRead::Unavailable(FileReadReason::from(&error));
    }
    if contents.len() > MAX_GIT_REF_FILE_BYTES {
        return RefFileRead::Unavailable(FileReadReason::ReadLimit {
            bytes: MAX_GIT_REF_FILE_BYTES,
        });
    }
    RefFileRead::Content(contents)
}

pub(super) fn read_git_ref_file(path: &Path, errors: &mut Vec<GitReadError>) -> Option<String> {
    match read_git_ref_file_state(path) {
        RefFileRead::Content(contents) => Some(contents),
        RefFileRead::Absent => None,
        RefFileRead::Unavailable(reason) => {
            errors.push(GitReadError::FileRead {
                path: path.to_path_buf(),
                reason,
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
    match crate::access::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(error) if is_absence(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_dir_entry(path: &Path) -> std::io::Result<bool> {
    Ok(matches!(entry_type(path)?, Some(kind) if kind.is_dir()))
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
            let gitdir = crate::access::read_to_string(&git_path)
                .map_err(|error| file_read_error(&git_path, &error))?;
            let Some(relative) = gitdir
                .trim()
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|relative| !relative.is_empty())
            else {
                return Err(GitReadError::FileRead {
                    path: git_path,
                    reason: FileReadReason::InvalidGitfile,
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
        reason: FileReadReason::from(error),
    }
}

fn git_directory_is_readable(git_dir: &LocatedGitDir) -> std::io::Result<bool> {
    let head = git_dir.path.join("HEAD");
    let head_is_valid = if git_dir.from_gitfile {
        // A gitfile names one specific target, so preserve its original I/O
        // error instead of changing it to synthetic invalid data.
        validate_git_head(&head)?
    } else {
        validate_plain_git_head(&head)?
    };
    // A plain .git directory with an invalid HEAD can be skipped in favour of
    // an enclosing checkout. A .git file claims a specific target, so a bad
    // target must stop discovery rather than silently changing repositories.
    if !head_is_valid {
        return if git_dir.from_gitfile {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gitfile target has no valid HEAD",
            ))
        } else {
            Ok(false)
        };
    }
    // Linked worktrees keep objects and refs in their common directory.
    let common = git_common_dir_for_git_dir(&git_dir.path)?;
    // setup.c uses access(X_OK), which checks search permission and follows
    // symlinks; it does not require these paths to be regular directories.
    let valid = crate::access::has_execute_access(&common.join("objects"))?
        && crate::access::has_execute_access(&common.join("refs"))?;
    if !valid && git_dir.from_gitfile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gitfile target has no searchable objects or refs path",
        ));
    }
    Ok(valid)
}

/// setup.c's `validate_headref`: accept a symbolic link whose target begins
/// with refs/, or else (following any link, as Git's open does) a `ref:` file
/// whose target begins with refs/, or a hexadecimal object id at the start of
/// the file. Missing and malformed HEADs are not a valid repository marker.
/// Other I/O failures retain their errno so a gitfile target does not turn an
/// unreadable HEAD into a synthetic invalid-data error; callers decide whether
/// an unreadable plain marker should be skipped.
fn validate_git_head(path: &Path) -> io::Result<bool> {
    let metadata = match crate::access::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if is_absence(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        let target = match crate::access::read_link(path) {
            Ok(target) => target,
            Err(error) if is_absence(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        if target.as_os_str().as_bytes().starts_with(b"refs/") {
            return Ok(true);
        }
    }
    let metadata = if metadata.file_type().is_symlink() {
        match crate::access::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if is_absence(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
    } else {
        metadata
    };
    // Git opens whatever HEAD names. Shepr reads only a regular file: opening
    // a FIFO or device in a malformed marker could block a refresh.
    if !metadata.file_type().is_file() {
        return Ok(false);
    }

    let file = match crate::access::open(path) {
        Ok(file) => file,
        Err(error) if is_absence(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut contents = Vec::new();
    file.take(crate::limits::MAX_GIT_HEAD_VALIDATION_BYTES)
        .read_to_end(&mut contents)?;

    let contents = contents.split(|byte| *byte == 0).next().unwrap_or_default();
    if let Some(reference) = contents.strip_prefix(b"ref:") {
        // Git's own isspace: space, tab, LF and CR.
        let start = reference
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
            .unwrap_or(reference.len());
        return Ok(reference[start..].starts_with(b"refs/"));
    }
    Ok([40, 64].into_iter().any(|length| {
        contents.len() >= length && contents[..length].iter().all(u8::is_ascii_hexdigit)
    }))
}

/// A plain marker does not claim a specific repository target. Keep Git's
/// discovery behavior by skipping it on ordinary I/O errors, while retaining
/// WouldBlock so the worker's quarantine refusal is not mistaken for absence.
fn validate_plain_git_head(path: &Path) -> io::Result<bool> {
    match validate_git_head(path) {
        Err(error) if error.kind() != io::ErrorKind::WouldBlock => Ok(false),
        result => result,
    }
}

fn path_is_git_dir_layout(path: &Path) -> std::io::Result<bool> {
    if !validate_plain_git_head(&path.join("HEAD"))? {
        return Ok(false);
    }
    let common = git_common_dir_for_git_dir(path)?;
    Ok(crate::access::has_execute_access(&common.join("objects"))?
        && crate::access::has_execute_access(&common.join("refs"))?)
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
                arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
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
                arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
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
                arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
            });
            return None;
        }
    };
    let stdout = stdout.trim();
    if stdout.is_empty() {
        errors.push(GitReadError::InvalidOutput {
            cwd: repo_root.to_path_buf(),
            arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
            output: String::new(),
        });
        None
    } else {
        Some(stdout.to_string())
    }
}

/// Runs one Git probe through the runner, typing its failure for the
/// status refresh.
pub(super) fn run_git_output(cwd: &Path, args: &[&str]) -> Result<Output, GitReadError> {
    use super::GitCommandError;
    super::run_git(cwd, args).map_err(|error| match error {
        GitCommandError::Spawn(error) => GitReadError::Spawn {
            cwd: cwd.to_path_buf(),
            reason: super::GitIoError::from(&error),
        },
        GitCommandError::TimedOut => GitReadError::TimedOut {
            cwd: cwd.to_path_buf(),
            arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
        },
        GitCommandError::Process(error) => GitReadError::Process {
            cwd: cwd.to_path_buf(),
            reason: super::GitIoError::from(&error),
        },
    })
}

pub(super) fn command_failed(cwd: &Path, args: &[&str], output: &Output) -> GitReadError {
    GitReadError::CommandFailed {
        cwd: cwd.to_path_buf(),
        arguments: args.iter().map(|arg| (*arg).to_owned()).collect(),
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
    /// Resolved entries also retain their spelling; entries after an empty
    /// field must match the physical discovery path exactly as written.
    dirs: Vec<PathBuf>,
}

impl GitCeilings {
    /// This process's ceilings, parsed from Git's colon-separated OS bytes.
    fn from_env() -> Self {
        match shepr_core::env::read_os(shepr_core::env::EnvVar::GitCeilingDirectories) {
            Ok(Some(value)) => Self::parse(&value),
            Ok(None) => Self::default(),
            Err(error) => {
                shepr_platform::structured_log!(WARN, event = git.ceiling_read, outcome = Error, %error, "failed to read Git ceiling directories");
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
            if resolve && let Ok(real) = crate::access::canonicalize(path) {
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
    let across_filesystems = discovery_across_filesystems_from_env();
    // An unreadable mountinfo yields the empty table, which knows no boundary:
    // discovery then ascends across filesystems rather than leaving every
    // workspace without Git status. `access::read_mount_table` logs that.
    let mounts = crate::access::mount_table();
    git_worktree_location_below_with(start, ceilings, across_filesystems, &mounts)
}

fn discovery_across_filesystems_from_env() -> bool {
    match shepr_core::env::read_os(shepr_core::env::EnvVar::GitDiscoveryAcrossFilesystem) {
        Ok(Some(value)) => super::config::git_config_bool(value.as_bytes()).unwrap_or(false),
        Ok(None) => false,
        Err(error) => {
            shepr_platform::structured_log!(WARN, event = git.discovery_across_filesystem, outcome = Error, %error, "failed to read Git filesystem discovery setting");
            false
        }
    }
}

fn git_worktree_location_below_with(
    start: &Path,
    ceilings: &GitCeilings,
    across_filesystems: bool,
    mounts: &shepr_platform::mounts::MountTable,
) -> Result<Option<(PathBuf, LocatedGitDir)>, GitReadError> {
    // OSC 7 may supply a logical symlink spelling. Git changes directory
    // before discovery and ascends physical parents, not that spelling. The
    // root returned is therefore physical too: it must not be compared as a
    // prefix of the logical cwd, and anything displaying it shows the
    // resolved path.
    let physical =
        crate::access::canonicalize(start).map_err(|error| file_read_error(start, &error))?;
    let mut current = match is_dir_entry(&physical) {
        Ok(true) => physical.clone(),
        Ok(false) => match physical.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Ok(None),
        },
        Err(error) => return Err(file_read_error(start, &error)),
    };

    loop {
        let found = match locate_git_dir(&current)? {
            Some(git_dir) => {
                if git_directory_is_readable(&git_dir)
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
        let mut parent = current.clone();
        if !parent.pop() {
            return Ok(None);
        }
        if !across_filesystems && mounts.is_filesystem_boundary(&current, &parent) == Some(true) {
            return Ok(None);
        }
        if ceilings.contains(&parent) {
            return Ok(None);
        }
        current = parent;
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
                    reason: FileReadReason::InvalidObjectId,
                });
                return None;
            };
            return Some(oid);
        }
        // An existing but unavailable loose ref must not resurrect a stale
        // packed OID. Symbolic loose refs are reported unavailable too.
        RefFileRead::Unavailable(reason) => {
            errors.push(GitReadError::FileRead {
                path: loose_ref,
                reason,
            });
            return None;
        }
        RefFileRead::Absent => {}
    }

    // Packed refs can exceed the small loose-ref cap. Stream bounded lines
    // instead of allocating the entire file or an unbounded malformed line.
    use std::io::BufRead;
    let packed_path = common_dir.join("packed-refs");
    let file = match crate::access::open(&packed_path) {
        Ok(file) => file,
        Err(error) if is_absence(&error) => return None,
        Err(error) => {
            errors.push(GitReadError::FileRead {
                path: packed_path,
                reason: FileReadReason::from(&error),
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
                    reason: FileReadReason::PackedRefLineTooLarge,
                });
                return None;
            }
            Err(error) => {
                errors.push(GitReadError::FileRead {
                    path: packed_path,
                    reason: FileReadReason::from(&error),
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
                reason: FileReadReason::InvalidObjectId,
            });
            return None;
        }
    }
}

#[cfg(test)]
pub(crate) fn git_worktree_info(cwd: &Path) -> Option<GitWorktreeInfo> {
    git_worktree_info_with_errors(cwd, &mut Vec::new())
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
            reason: FileReadReason::InvalidRefName,
        });
        return None;
    };
    read_ref_oid_for_full_ref(common_dir, &full_ref, errors).map(|oid| oid.as_str().to_owned())
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
fn git_repo_root_below_with_mounts(
    start: &Path,
    ceilings: &GitCeilings,
    across_filesystems: bool,
    mounts: &shepr_platform::mounts::MountTable,
) -> Option<PathBuf> {
    git_worktree_location_below_with(start, ceilings, across_filesystems, mounts)
        .ok()
        .flatten()
        .map(|(repo_root, _)| repo_root)
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
    use crate::test_support::{
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

        let cases: [(&str, &[u8]); 5] = [
            ("missing-target", b"gitdir: absent-admin\n"),
            ("missing-directive", b"not a gitfile\n"),
            ("non-utf8", b"\xff"),
            ("non-file-head", b"gitdir: admin\n"),
            ("head-only", b"gitdir: admin\n"),
        ];
        for (name, contents) in cases {
            let checkout = outer.join(".worktrees").join(name);
            std::fs::create_dir_all(&checkout).expect("test precondition");
            std::fs::write(checkout.join(".git"), contents).expect("test precondition");
            if name == "non-file-head" {
                std::fs::create_dir_all(checkout.join("admin/HEAD")).expect("test precondition");
            } else if name == "head-only" {
                std::fs::create_dir_all(checkout.join("admin")).expect("test precondition");
                std::fs::write(checkout.join("admin/HEAD"), "ref: refs/heads/main\n")
                    .expect("test precondition");
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
    fn unreadable_gitfile_head_preserves_errno_but_plain_marker_can_be_skipped() {
        use std::os::unix::fs::symlink;

        let _env = shepr_test_support::IsolatedEnv::new();
        let outer = temp_test_dir("unreadable-head-enclosing-repo");
        mark_checkout(&outer);

        let plain = outer.join("plain-marker");
        std::fs::create_dir_all(plain.join(".git")).expect("test precondition");
        // A symlink loop supplies a deterministic filesystem error even when
        // the test process can bypass mode-bit permission checks.
        symlink("HEAD", plain.join(".git/HEAD")).expect("test precondition");
        assert_eq!(
            git_repo_root_below_with_errors(&plain, &GitCeilings::default(), &mut Vec::new()),
            Some(outer.clone()),
            "an unreadable plain .git marker is skipped for an enclosing checkout"
        );

        let checkout = outer.join(".worktrees/gitfile-unreadable-head");
        let git_dir = checkout.join("admin");
        std::fs::create_dir_all(git_dir.join("objects")).expect("test precondition");
        std::fs::create_dir_all(git_dir.join("refs")).expect("test precondition");
        std::fs::create_dir_all(&checkout).expect("test precondition");
        std::fs::write(checkout.join(".git"), "gitdir: admin\n").expect("test precondition");
        symlink("HEAD", git_dir.join("HEAD")).expect("test precondition");

        let mut errors = Vec::new();
        assert_eq!(
            git_repo_root_below_with_errors(&checkout, &GitCeilings::default(), &mut errors),
            None,
            "a gitfile with an unreadable target HEAD must stop discovery"
        );
        assert!(matches!(
            errors.as_slice(),
            [GitReadError::FileRead { path, reason: FileReadReason::Io(reason) }]
                if path == &git_dir.join("HEAD")
                    && reason.to_string() == io::Error::from_raw_os_error(libc::ELOOP).to_string()
        ));
    }

    #[test]
    fn logical_symlink_uses_physical_repository_and_ceiling() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let logical = temp_test_dir("logical-repo");
        let physical = temp_test_dir("physical-repo");
        mark_checkout(&logical);
        mark_checkout(&physical);
        let work = physical.join("work");
        std::fs::create_dir_all(&work).expect("test precondition");
        let link = logical.join("link");
        std::os::unix::fs::symlink(&work, &link).expect("test precondition");
        assert_eq!(
            git_repo_root_below(&link, &GitCeilings::default()),
            Some(physical.clone())
        );
        assert_eq!(
            git_repo_root_below(
                &link,
                &ceilings(physical.to_str().expect("utf-8 scratch path"))
            ),
            None
        );
        std::fs::remove_dir_all(physical.join(".git")).expect("test precondition");
        assert_eq!(git_repo_root(&link), None);
    }

    #[test]
    fn incomplete_marker_is_skipped_for_an_enclosing_checkout() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let outer = temp_test_dir("head-only-marker");
        mark_checkout(&outer);
        for directory in ["", "objects", "refs"] {
            let nested = outer.join(format!("nested-{directory}"));
            std::fs::create_dir_all(nested.join(".git").join(directory))
                .expect("test precondition");
            std::fs::write(nested.join(".git/HEAD"), "ref: refs/heads/main\n")
                .expect("test precondition");
            assert_eq!(
                git_repo_root_below(&nested, &GitCeilings::default()),
                Some(outer.clone())
            );
        }
    }

    #[test]
    fn linked_marker_validates_the_common_directory() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("linked-marker-validation");
        let common = base.join("common");
        let checkout = base.join("checkout");
        write_git_dir(&common, "main", true);
        add_linked_worktree(&common, "main", &checkout);
        assert_eq!(
            git_repo_root_below(&checkout, &GitCeilings::default()),
            Some(checkout.clone())
        );
        std::fs::remove_dir_all(common.join("objects")).expect("test precondition");
        assert!(matches!(
            discover_below(&checkout, &GitCeilings::default()),
            Discovery::Unreadable(_)
        ));
    }

    #[test]
    fn common_directory_read_preserves_filesystem_errors() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("common-directory-error");
        std::os::unix::fs::symlink("commondir", base.join("commondir")).expect("test precondition");
        assert_eq!(
            git_common_dir_for_git_dir(&base)
                .expect_err("symlink loop")
                .raw_os_error(),
            Some(libc::ELOOP)
        );
    }

    #[test]
    fn commondir_strips_line_endings_but_preserves_path_whitespace() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("commondir-whitespace");
        let git_dir = base.join("admin");
        let common_dir = git_dir.join(" common \t ");
        std::fs::create_dir_all(&git_dir).expect("test precondition");
        std::fs::write(git_dir.join("commondir"), " common \t \r\n").expect("test precondition");

        assert_eq!(
            git_common_dir_for_git_dir(&git_dir).expect("commondir"),
            common_dir
        );
    }

    #[test]
    fn discovery_checks_head_contents_and_accepts_git_head_forms() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let outer = temp_test_dir("head-validation-outer");
        mark_checkout(&outer);
        let nested = outer.join("nested");
        let nested_git = nested.join(".git");
        std::fs::create_dir_all(nested_git.join("objects")).expect("test precondition");
        std::fs::create_dir_all(nested_git.join("refs")).expect("test precondition");

        for invalid in [
            "",
            "not a head\n",
            "ref: HEAD\n",
            "ref: refs-invalid/main\n",
        ] {
            std::fs::write(nested_git.join("HEAD"), invalid).expect("HEAD");
            assert_eq!(
                git_repo_root_below(&nested, &GitCeilings::default()),
                Some(outer.clone()),
                "invalid HEAD should not make a .git directory a repository: {invalid:?}"
            );
        }

        std::fs::write(nested_git.join("HEAD"), "ref:\trefs/heads/main\n").expect("symbolic HEAD");
        assert_eq!(
            git_repo_root_below(&nested, &GitCeilings::default()),
            Some(nested.clone())
        );

        for oid_length in [40, 64] {
            std::fs::write(
                nested_git.join("HEAD"),
                format!("{} detached suffix\n", "A".repeat(oid_length)),
            )
            .expect("detached HEAD");
            assert_eq!(
                git_repo_root_below(&nested, &GitCeilings::default()),
                Some(nested.clone())
            );
        }

        std::fs::remove_file(nested_git.join("HEAD")).expect("remove HEAD");
        std::os::unix::fs::symlink("refs/heads/missing", nested_git.join("HEAD"))
            .expect("symbolic link HEAD");
        assert_eq!(
            git_repo_root_below(&nested, &GitCeilings::default()),
            Some(nested)
        );
    }

    #[test]
    fn discovery_checks_search_permission_for_objects_and_refs() {
        use std::os::unix::fs::PermissionsExt as _;

        let _env = shepr_test_support::IsolatedEnv::new();
        let outer = temp_test_dir("search-permission-outer");
        mark_checkout(&outer);
        for inaccessible in ["objects", "refs"] {
            let nested = outer.join(inaccessible);
            let nested_git = nested.join(".git");
            let inaccessible_path = nested_git.join(inaccessible);
            std::fs::create_dir_all(nested_git.join("objects")).expect("test precondition");
            std::fs::create_dir_all(nested_git.join("refs")).expect("test precondition");
            std::fs::write(nested_git.join("HEAD"), "ref: refs/heads/main\n")
                .expect("test precondition");
            std::fs::set_permissions(&inaccessible_path, std::fs::Permissions::from_mode(0o600))
                .expect("remove search permission");

            assert_eq!(
                git_repo_root_below(&nested, &GitCeilings::default()),
                Some(outer.clone()),
                "Git rejects a repository whose {inaccessible} path is not searchable"
            );
        }
    }

    #[test]
    fn discovery_stops_at_filesystem_boundary_unless_enabled() {
        let env = shepr_test_support::IsolatedEnv::new();
        let outer = temp_test_dir("filesystem-boundary");
        mark_checkout(&outer);
        let mount = outer.join("mount");
        let work = mount.join("work");
        std::fs::create_dir_all(&work).expect("test precondition");
        let mounts = shepr_platform::mounts::MountTable::from_mountinfo(&format!(
            "1 0 8:1 / / rw - ext4 root rw\n2 1 8:2 / {} rw - ext4 other rw\n",
            mount.display()
        ));

        env.set(shepr_core::env::EnvVar::GitDiscoveryAcrossFilesystem, "OFF");
        assert_eq!(
            git_repo_root_below_with_mounts(
                &work,
                &GitCeilings::default(),
                discovery_across_filesystems_from_env(),
                &mounts,
            ),
            None
        );

        env.set(shepr_core::env::EnvVar::GitDiscoveryAcrossFilesystem, "yes");
        assert_eq!(
            git_repo_root_below_with_mounts(
                &work,
                &GitCeilings::default(),
                discovery_across_filesystems_from_env(),
                &mounts,
            ),
            Some(outer)
        );
    }

    #[test]
    fn unreadable_mount_table_still_finds_the_enclosing_checkout() {
        // A refresh whose mountinfo read failed runs with the empty table:
        // discovery degrades to ignoring filesystem boundaries instead of
        // reporting no checkout.
        let _env = shepr_test_support::IsolatedEnv::new();
        let outer = temp_test_dir("mount-table-unreadable");
        mark_checkout(&outer);
        let nested = outer.join("nested");
        std::fs::create_dir_all(&nested).expect("test precondition");
        let progress = crate::worker::RefreshProgress::default();
        let mut errors = Vec::new();

        let found = crate::access::scoped_with_mounts(
            &progress,
            shepr_platform::mounts::MountTable::default(),
            || git_repo_root_below_with_errors(&nested, &GitCeilings::default(), &mut errors),
        );

        assert_eq!(found, Some(outer));
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn deleted_cwd_remains_a_git_read_error() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let deleted = temp_test_dir("deleted-git-cwd");
        std::fs::remove_dir_all(&deleted).expect("remove test cwd");
        let mut errors = Vec::new();

        assert_eq!(
            git_repo_root_below_with_errors(&deleted, &GitCeilings::default(), &mut errors),
            None
        );
        assert!(
            errors.iter().any(|error| matches!(
                error,
                GitReadError::FileRead {
                    path,
                    reason: FileReadReason::Io(_),
                } if path == &deleted
            )),
            "deleted cwd should be reported as a Git read error: {errors:?}"
        );
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
        std::fs::create_dir_all(root.join(".git/objects")).expect("test precondition");
        std::fs::create_dir_all(root.join(".git/refs")).expect("test precondition");
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
    fn bare_source_and_linked_checkout_each_have_their_own_root() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, bare, checkout) =
            crate::test_support::create_bare_repo_with_linked_worktree("bare-linked-labels");

        let bare_info = git_worktree_info(&bare).expect("test precondition");
        let checkout_info = git_worktree_info(&checkout).expect("test precondition");

        assert_eq!(bare_info.repo_root, bare);
        assert_eq!(checkout_info.repo_root, checkout);
    }

    #[test]
    fn embedded_dot_bare_source_and_checkout_each_have_their_own_root() {
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
