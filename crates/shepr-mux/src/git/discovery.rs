use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;

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
    pub is_bare: bool,
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
    let repo_root = git_repo_root(cwd)?;
    let git_dir = canonicalize_best_effort_path(&git_dir_for_repo_root(&repo_root)?);
    let git_common_dir = canonicalize_best_effort_path(&git_common_dir_for_git_dir(&git_dir)?);
    let is_linked_worktree = git_dir != git_common_dir;
    let is_bare = git_dir_is_bare(&git_dir);

    Some(GitWorktreeInfo {
        repo_root,
        git_dir,
        git_common_dir,
        is_bare,
        is_linked_worktree,
    })
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
    Unavailable,
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
                Ok(_) | Err(_) => RefFileRead::Unavailable,
            };
        }
        // Permission or I/O errors: the ref may exist, so its identity is
        // unavailable rather than absent.
        Err(_) => return RefFileRead::Unavailable,
    };
    let mut contents = String::new();
    if file
        .take((MAX_GIT_REF_FILE_BYTES + 1) as u64)
        .read_to_string(&mut contents)
        .is_err()
        || contents.len() > MAX_GIT_REF_FILE_BYTES
    {
        return RefFileRead::Unavailable;
    }
    RefFileRead::Content(contents)
}

pub(super) fn read_git_ref_file(path: &Path) -> Option<String> {
    match read_git_ref_file_state(path) {
        RefFileRead::Content(contents) => Some(contents),
        RefFileRead::Absent | RefFileRead::Unavailable => None,
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

    if path_is_git_dir_layout(repo_root)? && git_dir_is_bare(repo_root) {
        return Ok(Some(repo_root.to_path_buf()));
    }

    Ok(None)
}

fn path_is_git_dir_layout(path: &Path) -> std::io::Result<bool> {
    Ok(is_file_entry(&path.join("HEAD"))?
        && is_dir_entry(&path.join("objects"))?
        && is_dir_entry(&path.join("refs"))?)
}

pub(super) fn git_symbolic_head_full(repo_root: &Path) -> Option<String> {
    git_trimmed_stdout(repo_root, &["symbolic-ref", "--quiet", "HEAD"])
}

pub(super) fn git_rev_parse_verify(repo_root: &Path, revision: &str) -> Option<String> {
    git_trimmed_stdout(repo_root, &["rev-parse", "--verify", revision])
}

pub(super) fn git_ref_storage_is_reftable(git_common_dir: &Path) -> bool {
    read_git_config_value(&git_common_dir.join("config"), "extensions", "refstorage")
        .is_some_and(|value| value.eq_ignore_ascii_case("reftable"))
}

fn git_dir_is_bare(git_dir: &Path) -> bool {
    read_git_config_value(&git_dir.join("config"), "core", "bare")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

fn read_git_config_value(path: &Path, section: &str, key: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut in_section = false;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section_name) = simple_git_config_section(line) {
            in_section = section_name.eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(key) {
            return Some(strip_git_config_comment(value).trim().to_string());
        }
    }
    None
}

fn simple_git_config_section(line: &str) -> Option<&str> {
    let section = line.strip_prefix('[')?.split_once(']')?.0.trim();
    (!section.contains('"')).then_some(section)
}

fn strip_git_config_comment(value: &str) -> &str {
    let value = value.trim();
    for marker in ['#', ';'] {
        if let Some((prefix, _)) = value.split_once(marker)
            && prefix.chars().next_back().is_some_and(char::is_whitespace)
        {
            return prefix;
        }
    }
    value
}

fn git_trimmed_stdout(repo_root: &Path, args: &[&str]) -> Option<String> {
    // host-program-ok: production asks Git what a reftable store holds
    let output = shepr_platform::child_command("git", repo_root)
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim();
    (!stdout.is_empty()).then(|| stdout.to_string())
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

pub(super) fn git_repo_root(start: &Path) -> Option<PathBuf> {
    git_repo_root_below(start, &GitCeilings::from_env())
}

/// [`git_repo_root`] with the ceilings handed in: the walk examines `start`
/// (or its parent, for a file) and each ancestor up to, not including, the
/// nearest ceiling. A directory whose Git state cannot be read (a stat or
/// read error other than absence) ends the walk with `None` rather than being
/// passed over: ascending past it could attribute `start` to an enclosing
/// checkout it is not part of.
fn git_repo_root_below(start: &Path, ceilings: &GitCeilings) -> Option<PathBuf> {
    let mut current = match is_dir_entry(start) {
        Ok(true) => start.to_path_buf(),
        Ok(false) => start.parent()?.to_path_buf(),
        Err(error) => {
            tracing::debug!(path = %start.display(), %error, "git discovery start unreadable");
            return None;
        }
    };

    loop {
        let found = locate_git_dir(&current).and_then(|git_dir| match git_dir {
            Some(git_dir) => is_file_entry(&git_dir.join("HEAD")),
            None => Ok(false),
        });
        match found {
            Ok(true) => return Some(current),
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(
                    path = %current.display(),
                    %error,
                    "git discovery stopped at an unreadable directory"
                );
                return None;
            }
        }
        if !current.pop() || ceilings.contains(&current) {
            return None;
        }
    }
}

pub(super) fn read_ref_oid(common_dir: &Path, full_ref: &str) -> Option<String> {
    let loose_ref = common_dir.join(full_ref);
    match read_git_ref_file_state(&loose_ref) {
        RefFileRead::Content(contents) => {
            let oid = contents.trim();
            if oid.is_empty() {
                // An empty loose ref is present but broken. Git does not fall
                // back to an older same-name packed ref in this case.
                return None;
            }
            return Some(oid.to_string());
        }
        // A loose ref that exists - or whose existence cannot be ruled out
        // because of a metadata or I/O error - must not fall back to
        // packed-refs: that could resurrect a stale same-name OID into the
        // status fingerprint. Report the ref as unavailable instead.
        RefFileRead::Unavailable => return None,
        RefFileRead::Absent => {}
    }

    let packed_refs = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
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
        let bare = temp_test_dir("bare-space");
        write_git_dir(&bare, "main", true);
        let nested = bare.join("refs");

        let info = git_worktree_info(&nested).expect("bare repo should be discovered");
        assert!(info.is_bare);
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
        let root = temp_test_dir("bare-dot-git");
        write_git_dir(&root.join(".git"), "main", true);

        let info = git_worktree_info(&root).expect("bare .git repo should be discovered");
        assert!(info.is_bare);
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
        let root = temp_test_dir("reftable-ref-oid");
        let root_arg = root.to_string_lossy().to_string();
        // host-program-ok: a reftable store is written by Git; production reads it through Git
        let output = shepr_test_support::command_in_scratch("git", "reftable-ref-oid-init")
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .expect("test precondition");
        if !output.status.success() {
            return;
        }

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
