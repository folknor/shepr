use std::path::{Path, PathBuf};

/// A fresh scratch directory, cleared by the next run rather than by the test.
pub(super) fn temp_test_dir(name: &str) -> PathBuf {
    shepr_test_support::ScratchDir::new(name).to_path_buf()
}

/// The commit every plain-file fixture's branches point at. No object backs
/// it: discovery and the files-backend status read refs, never objects.
const FIXTURE_OID: &str = "1111111111111111111111111111111111111111";

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("test precondition");
    }
    std::fs::write(path, contents).expect("test precondition");
}

/// A repository directory as Git lays one out, in plain files: `HEAD` on
/// `branch`, which points at [`FIXTURE_OID`], empty `objects`, and a config
/// saying whether it is bare.
pub(super) fn write_git_dir(git_dir: &Path, branch: &str, bare: bool) {
    std::fs::create_dir_all(git_dir.join("objects")).expect("test precondition");
    write(
        &git_dir.join("HEAD"),
        &format!("ref: refs/heads/{branch}\n"),
    );
    write(
        &git_dir.join("refs/heads").join(branch),
        &format!("{FIXTURE_OID}\n"),
    );
    write(
        &git_dir.join("config"),
        &format!("[core]\n\trepositoryformatversion = 0\n\tbare = {bare}\n"),
    );
}

/// A linked worktree of the repository whose common directory is
/// `common_dir`, checked out at `checkout` on a new branch `name`, laid out
/// as `git worktree add` lays it out.
pub(super) fn add_linked_worktree(common_dir: &Path, name: &str, checkout: &Path) {
    let admin = common_dir.join("worktrees").join(name);
    write(&admin.join("HEAD"), &format!("ref: refs/heads/{name}\n"));
    write(&admin.join("commondir"), "../..\n");
    write(
        &admin.join("gitdir"),
        &format!("{}\n", checkout.join(".git").display()),
    );
    write(
        &common_dir.join("refs/heads").join(name),
        &format!("{FIXTURE_OID}\n"),
    );
    write(
        &checkout.join(".git"),
        &format!("gitdir: {}\n", admin.display()),
    );
}

pub fn create_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let repo = base.join("shepr");
    let checkout = base.join("testr56");
    write_git_dir(&repo.join(".git"), "main", false);
    add_linked_worktree(&repo.join(".git"), "testr56", &checkout);
    (base, repo, checkout)
}

pub(crate) fn create_bare_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let bare = base.join(".bare");
    let checkout = base.join("feature");
    write_git_dir(&bare, "main", true);
    add_linked_worktree(&bare, "feature", &checkout);
    (base, bare, checkout)
}

pub(super) fn write_fake_tracked_repo(root: &Path) {
    let head_oid = "1111111111111111111111111111111111111111";
    let upstream_oid = "2222222222222222222222222222222222222222";
    std::fs::create_dir_all(root.join(".git/refs/heads")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("test precondition");
    std::fs::write(root.join(".git/refs/heads/main"), format!("{head_oid}\n"))
        .expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/origin/main"),
        format!("{upstream_oid}\n"),
    )
    .expect("test precondition");
    std::fs::write(
        root.join(".git/config"),
        "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    )
    .expect("test precondition");
}

/// The Git space production derives for `cwd`: the `space` a branch-only
/// status refresh reports, the same call the background refresh makes.
pub(super) fn live_git_space(cwd: &Path) -> Option<crate::git::GitSpaceMetadata> {
    super::status::git_status_snapshot_for_cwd_with_demand(
        cwd,
        None,
        super::status::GitStatusRefreshDemand {
            branch: true,
            ahead_behind: false,
        },
    )
    .0
    .space
}

/// Runs the host's Git to build a fixture only Git can write: a reftable ref
/// store, which is a binary format, or real commit objects for production's
/// own `git rev-list` to walk. Only tests of production code that itself
/// spawns Git use it; every other repository fixture is plain files.
pub(super) fn git_written_fixture(cwd: &Path, args: &[&str]) {
    // host-program-ok: the fixture feeds production code that spawns Git itself
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("test precondition");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
