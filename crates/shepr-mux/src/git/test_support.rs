use std::path::{Path, PathBuf};

/// A fresh scratch directory, cleared by the next run rather than by the test.
pub(super) fn temp_test_dir(name: &str) -> PathBuf {
    shepr_test_support::ScratchDir::new(name).to_path_buf()
}

fn init_repo_with_commit(repo: &Path) {
    std::fs::create_dir_all(repo).expect("test precondition");
    run_git(repo, &["init", "--quiet"]);
    run_git(repo, &["config", "user.email", "shepr@example.invalid"]);
    run_git(repo, &["config", "user.name", "Shepr Test"]);
    run_git(
        repo,
        &["commit", "--quiet", "--allow-empty", "-m", "initial"],
    );
}

pub fn create_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let repo = base.join("shepr");
    let checkout = base.join("testr56");
    init_repo_with_commit(&repo);
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "testr56",
            checkout.to_str().expect("test precondition"),
            "HEAD",
        ],
    );
    (base, repo, checkout)
}

pub(crate) fn create_bare_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let seed = base.join("seed");
    let bare = base.join(".bare");
    let checkout = base.join("feature");
    init_repo_with_commit(&seed);
    run_git(
        &base,
        &[
            "clone",
            "--quiet",
            "--bare",
            seed.to_str().expect("test precondition"),
            bare.to_str().expect("test precondition"),
        ],
    );
    run_git(
        &bare,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            checkout.to_str().expect("test precondition"),
            "HEAD",
        ],
    );
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

pub(super) fn run_git(cwd: &Path, args: &[&str]) {
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
