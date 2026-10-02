use crate::limits::GIT_STATUS_RETRY_DELAY;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{AheadBehind, GitReadError, WorkspaceGitStatusSnapshot};

use super::{
    config::{ConfigCtx, FileDep, deps_current, read_config_for_status, stamp, upstream_full_ref},
    discovery::{
        GitWorktreeInfo, canonicalize_best_effort_path, git_ref_storage_is_reftable,
        git_rev_parse_verify_with_errors, git_symbolic_head_full, git_trimmed_stdout,
        git_worktree_info, git_worktree_info_with_errors, read_git_ref_file,
        read_ref_oid_with_errors, valid_full_ref, valid_oid,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusCacheEntry {
    pub fingerprint: Option<GitStatusFingerprint>,
    pub retry_after: Option<Instant>,
    pub snapshot: WorkspaceGitStatusSnapshot,
    pub read_errors: Vec<GitReadError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusFingerprint {
    pub head: GitHeadIdentity,
    pub upstream: Option<GitUpstreamIdentity>,
    repository_context: RepoContext,
}

type RepoContext = (GitWorktreeInfo, bool, Vec<FileDep>, Option<ConfigCtx>);

fn repo_context(cwd: &Path, read_errors: &mut Vec<GitReadError>) -> Option<RepoContext> {
    let info = git_worktree_info_with_errors(cwd, read_errors)?;
    let (reftable, config_deps) = match git_ref_storage_is_reftable(&info) {
        Ok(result) => result,
        Err(error) => {
            read_errors.push(GitReadError::FileRead {
                path: info.git_common_dir.join("config"),
                message: error.to_string(),
            });
            return None;
        }
    };
    let mut paths = vec![info.repo_root.join(".git"), info.git_dir.join("commondir")];
    paths.push(info.git_dir.join("HEAD"));
    paths.push(info.git_common_dir.join("config"));
    paths.extend((info.git_dir != info.git_common_dir).then(|| info.git_dir.join("config")));
    let mut deps: Vec<_> = paths.into_iter().map(|path| stamp(path, None)).collect();
    deps.extend(config_deps);
    let current_info = git_worktree_info_with_errors(cwd, read_errors);
    deps[0].2 &= current_info.as_ref() == Some(&info) && deps_current(&deps);
    Some((info, reftable, deps, None))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHeadIdentity {
    Branch {
        full_ref: String,
        short_name: String,
        oid: Option<String>,
    },
    Detached {
        oid: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitUpstreamIdentity {
    pub remote: String,
    pub merge_ref: String,
    pub full_ref: String,
    pub oid: Option<String>,
}

pub fn git_status_cache_key(cwd: &Path) -> Option<PathBuf> {
    git_worktree_info(cwd).map(|info| canonicalize_best_effort_path(&info.repo_root))
}

pub fn git_status_snapshot_for_cwd(
    cwd: &Path,
    cached: Option<&GitStatusCacheEntry>,
) -> (WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>) {
    // One sample anchors both retry comparisons and deadlines recorded below;
    // a subprocess must not move the cache decision partway through a snapshot.
    let now = Instant::now();
    let mut read_errors = Vec::new();
    if let Some(cached) = cached.filter(|entry| {
        entry.fingerprint.is_none()
            && entry
                .retry_after
                .is_some_and(|retry_after| retry_after > now)
    }) {
        return (cached.snapshot.clone(), Some(cached.clone()));
    }

    let repository_context = cached
        .and_then(|entry| entry.fingerprint.as_ref())
        .map(|fingerprint| fingerprint.repository_context.clone())
        .filter(|context| deps_current(&context.2))
        .or_else(|| repo_context(cwd, &mut read_errors));
    let Some(repository_context) = repository_context else {
        let snapshot = WorkspaceGitStatusSnapshot {
            repo_root: None,
            branch: None,
            ahead_behind: None,
        };
        return (
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint: None,
                retry_after: Some(now + GIT_STATUS_RETRY_DELAY),
                snapshot,
                read_errors,
            }),
        );
    };
    let repo_root = repository_context.0.repo_root.clone();
    let Some(fingerprint) = fingerprint(repository_context, &mut read_errors) else {
        let snapshot = WorkspaceGitStatusSnapshot {
            repo_root: Some(repo_root),
            branch: None,
            ahead_behind: None,
        };
        return (
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint: None,
                retry_after: Some(now + GIT_STATUS_RETRY_DELAY),
                snapshot,
                read_errors,
            }),
        );
    };
    let branch = fingerprint.branch_name().map(str::to_string);

    if let Some(cached) = cached.filter(|entry| {
        entry.fingerprint.as_ref() == Some(&fingerprint)
            && entry
                .retry_after
                .is_none_or(|retry_after| retry_after > now)
    }) {
        let snapshot = WorkspaceGitStatusSnapshot {
            repo_root: Some(repo_root),
            branch,
            ahead_behind: cached.snapshot.ahead_behind,
        };
        return (
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint: Some(fingerprint),
                retry_after: cached.retry_after,
                snapshot,
                read_errors: cached.read_errors.clone(),
            }),
        );
    }

    let revision_pair = fingerprint.head_oid().zip(fingerprint.upstream_oid());
    let (ahead_behind, retry_after) = match revision_pair {
        Some((head_oid, upstream_oid)) => {
            let repo_root = &fingerprint.repository_context.0.repo_root;
            let ahead_behind =
                git_ahead_behind_between(repo_root, head_oid, upstream_oid, &mut read_errors);
            let retry_after = ahead_behind
                .is_none()
                .then_some(now + GIT_STATUS_RETRY_DELAY);
            (ahead_behind, retry_after)
        }
        None => (None, None),
    };
    let snapshot = WorkspaceGitStatusSnapshot {
        repo_root: Some(repo_root),
        branch,
        ahead_behind,
    };
    (
        snapshot.clone(),
        Some(GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after,
            snapshot,
            read_errors,
        }),
    )
}

fn fingerprint(
    mut repo: RepoContext,
    read_errors: &mut Vec<GitReadError>,
) -> Option<GitStatusFingerprint> {
    let head = read_head_identity(&repo.0, repo.1, read_errors)?;
    let upstream = match &head {
        GitHeadIdentity::Branch { short_name, .. } => {
            read_upstream(&mut repo, short_name, read_errors)
        }
        _ => None,
    };

    Some(GitStatusFingerprint {
        head,
        upstream,
        repository_context: repo,
    })
}

impl GitStatusFingerprint {
    fn branch_name(&self) -> Option<&str> {
        match &self.head {
            GitHeadIdentity::Branch { short_name, .. } => Some(short_name.as_str()),
            GitHeadIdentity::Detached { .. } => None,
        }
    }

    fn head_oid(&self) -> Option<&str> {
        match &self.head {
            GitHeadIdentity::Branch { oid, .. } => oid.as_deref(),
            GitHeadIdentity::Detached { oid } => Some(oid.as_str()),
        }
    }

    fn upstream_oid(&self) -> Option<&str> {
        self.upstream
            .as_ref()
            .and_then(|upstream| upstream.oid.as_deref())
    }
}

fn read_head_identity(
    info: &GitWorktreeInfo,
    reftable: bool,
    read_errors: &mut Vec<GitReadError>,
) -> Option<GitHeadIdentity> {
    if reftable {
        return read_head_identity_from_git(info, read_errors);
    }

    read_head_identity_from_files(info, read_errors)
}

fn read_head_identity_from_git(
    info: &GitWorktreeInfo,
    read_errors: &mut Vec<GitReadError>,
) -> Option<GitHeadIdentity> {
    if let Some(full_ref) = git_symbolic_head_full(&info.repo_root, read_errors) {
        if !valid_full_ref(&full_ref) {
            return None;
        }
        let short_name = full_ref.strip_prefix("refs/heads/")?.to_string();
        let oid = git_rev_parse_verify_with_errors(&info.repo_root, &full_ref, read_errors);
        return Some(GitHeadIdentity::Branch {
            full_ref,
            short_name,
            oid,
        });
    }

    git_rev_parse_verify_with_errors(&info.repo_root, "HEAD", read_errors)
        .map(|oid| GitHeadIdentity::Detached { oid })
}

fn read_head_identity_from_files(
    info: &GitWorktreeInfo,
    read_errors: &mut Vec<GitReadError>,
) -> Option<GitHeadIdentity> {
    let head = read_git_ref_file(&info.git_dir.join("HEAD"), read_errors)?;
    let head = head.trim();
    if let Some(full_ref) = head.strip_prefix("ref: ") {
        if !valid_full_ref(full_ref) {
            read_errors.push(GitReadError::FileRead {
                path: info.git_dir.join("HEAD"),
                message: "HEAD contains an invalid ref name".into(),
            });
            return None;
        }
        let short_name = full_ref.strip_prefix("refs/heads/")?.to_string();
        let oid = read_ref_oid_with_errors(&info.git_common_dir, full_ref, read_errors);
        return Some(GitHeadIdentity::Branch {
            full_ref: full_ref.to_string(),
            short_name,
            oid,
        });
    }

    if !valid_oid(head) {
        read_errors.push(GitReadError::FileRead {
            path: info.git_dir.join("HEAD"),
            message: "detached HEAD is not a complete object ID".into(),
        });
        return None;
    }
    Some(GitHeadIdentity::Detached {
        oid: head.to_string(),
    })
}

fn read_upstream(
    repo: &mut RepoContext,
    branch: &str,
    read_errors: &mut Vec<GitReadError>,
) -> Option<GitUpstreamIdentity> {
    if repo
        .3
        .as_ref()
        .is_none_or(|context| context.0 != branch || !deps_current(&context.2))
    {
        repo.3 = Some(read_config_for_status(&repo.0, branch, read_errors));
    }
    let config = repo.3.as_ref()?.1.clone()?;
    let full_ref = upstream_full_ref(&config)?;
    let oid = if repo.1 {
        git_rev_parse_verify_with_errors(&repo.0.repo_root, &full_ref, read_errors)
    } else {
        read_ref_oid_with_errors(&repo.0.git_common_dir, &full_ref, read_errors)
    };
    Some(GitUpstreamIdentity {
        remote: config.remote,
        merge_ref: config.merge_ref,
        full_ref,
        oid,
    })
}

fn git_ahead_behind_between(
    repo_root: &Path,
    head_oid: &str,
    upstream_oid: &str,
    read_errors: &mut Vec<GitReadError>,
) -> Option<AheadBehind> {
    if !valid_oid(head_oid) || !valid_oid(upstream_oid) {
        return None;
    }
    let range = format!("{head_oid}...{upstream_oid}");
    let stdout = git_trimmed_stdout(
        repo_root,
        &[
            "rev-list",
            "--left-right",
            "--count",
            "--end-of-options",
            &range,
        ],
        read_errors,
    )?;
    match parse_git_ahead_behind_output(&stdout) {
        Some(ahead_behind) => Some(ahead_behind),
        None => {
            read_errors.push(GitReadError::InvalidOutput {
                cwd: repo_root.to_path_buf(),
                arguments: "rev-list --left-right --count".into(),
                output: stdout,
            });
            None
        }
    }
}

fn parse_git_ahead_behind_output(stdout: &str) -> Option<AheadBehind> {
    let mut parts = stdout.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some(AheadBehind { ahead, behind })
}

#[cfg(test)]
pub(super) fn git_status_fingerprint(cwd: &Path) -> Option<GitStatusFingerprint> {
    let mut read_errors = Vec::new();
    fingerprint(repo_context(cwd, &mut read_errors)?, &mut read_errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{git_written_fixture, temp_test_dir, write_fake_tracked_repo};
    use std::time::Duration;

    #[test]
    fn cache_key_preserves_non_utf8_checkout_path() {
        use std::os::unix::ffi::OsStringExt;

        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("non-utf8-key");
        let root = base.join(std::ffi::OsString::from_vec(vec![
            b'r', b'e', b'p', b'o', 0x80,
        ]));
        write_fake_tracked_repo(&root);

        assert_eq!(
            git_status_cache_key(&root),
            Some(std::fs::canonicalize(&root).expect("test precondition"))
        );
    }

    // HEAD edge cases, read through the status refresh the sidebar uses.

    #[test]
    fn branch_reads_head_from_standard_repo() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("standard-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test precondition");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, None);

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
    }

    #[test]
    fn unavailable_loose_ref_is_carried_as_a_status_read_error() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("unavailable-status-ref");
        write_fake_tracked_repo(&root);
        let ref_path = root.join(".git/refs/heads/main");
        std::fs::write(
            &ref_path,
            "a".repeat(super::super::discovery::MAX_GIT_REF_FILE_BYTES + 1),
        )
        .expect("test precondition");

        let (snapshot, entry) = git_status_snapshot_for_cwd(&root, None);

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert!(entry.is_some_and(|entry| {
            entry.read_errors.iter().any(
                |error| matches!(error, GitReadError::FileRead { path, .. } if path == &ref_path),
            )
        }));
    }

    #[test]
    fn oversized_head_reports_no_branch() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("oversized-head");
        let git_dir = root.join(".git");
        std::fs::create_dir_all(&git_dir).expect("test precondition");
        let head = git_dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").expect("test precondition");
        std::fs::OpenOptions::new()
            .write(true)
            .open(head)
            .expect("test precondition")
            .set_len(60 * 1024 * 1024)
            .expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, None);

        let branch_len = snapshot.branch.as_ref().map(String::len);
        assert!(
            snapshot.branch.is_none(),
            "oversized Git HEAD produced branch with {branch_len:?} bytes"
        );
    }

    #[test]
    fn branch_reads_head_from_worktree_gitdir_file() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("worktree");
        let worktree_git_dir = root.join(".bare/worktrees/feature");
        std::fs::create_dir_all(&worktree_git_dir).expect("test precondition");
        std::fs::write(root.join(".git"), "gitdir: .bare/worktrees/feature\n")
            .expect("test precondition");
        std::fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n")
            .expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, None);

        assert_eq!(snapshot.branch.as_deref(), Some("feature"));
    }

    #[test]
    fn detached_head_reports_no_branch() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("detached-head");
        std::fs::create_dir_all(root.join(".git")).expect("test precondition");
        std::fs::write(
            root.join(".git/HEAD"),
            "3e1b9a8d3e1b9a8d3e1b9a8d3e1b9a8d3e1b9a8d\n",
        )
        .expect("test precondition");

        let (snapshot, update) = git_status_snapshot_for_cwd(&root, None);

        assert_eq!(snapshot.branch, None);
        assert!(
            snapshot.repo_root.is_some(),
            "a detached HEAD is still a repo"
        );
        assert!(
            update
                .and_then(|entry| entry.fingerprint)
                .is_some_and(|fingerprint| fingerprint.head
                    == GitHeadIdentity::Detached {
                        oid: "3e1b9a8d3e1b9a8d3e1b9a8d3e1b9a8d3e1b9a8d".into()
                    })
        );
    }

    /// Production reads a reftable store through Git, and the store is a
    /// binary format only Git writes, so Git makes this fixture.
    #[test]
    fn branch_reads_unborn_symbolic_head_from_reftable_repo() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("reftable-branch");
        // host-program-ok: a reftable store is written by Git; production reads it through Git
        let output = super::super::discovery::run_git_output(
            &root,
            &["init", "--ref-format=reftable", "-b", "main"],
        )
        .expect("test precondition");
        assert!(
            output.status.success(),
            "this test needs a host Git with reftable support (2.45 or later): {output:?}"
        );

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, None);

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_status_cache_key_ignores_invalid_git_marker() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("invalid-git-root");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(base.join(".git")).expect("test precondition");
        std::fs::create_dir_all(&cwd).expect("test precondition");

        assert_eq!(git_status_cache_key(&cwd), None);
    }

    #[test]
    fn non_git_refresh_reuses_cached_miss_without_rechecking_filesystem() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("cached-miss");
        let cwd = root.join("deep/nested");
        std::fs::create_dir_all(&cwd).expect("test precondition");

        let (initial, cache_entry) = git_status_snapshot_for_cwd(&cwd, None);
        let cache_entry = cache_entry.expect("non-Git result should be cached");
        std::fs::remove_dir_all(&root).expect("test precondition");

        let (cached, update) = git_status_snapshot_for_cwd(&cwd, Some(&cache_entry));

        assert_eq!(cached, initial);
        assert_eq!(update, Some(cache_entry));
    }

    #[test]
    fn expired_non_git_cache_detects_repository_created_in_place() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("expired-miss");
        let (_, cache_entry) = git_status_snapshot_for_cwd(&root, None);
        let mut cache_entry = cache_entry.expect("non-Git result should be cached");
        cache_entry.retry_after = Some(Instant::now() - Duration::from_secs(1));
        write_fake_tracked_repo(&root);

        let (snapshot, update) = git_status_snapshot_for_cwd(&root, Some(&cache_entry));

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert!(update.is_some_and(|entry| entry.fingerprint.is_some()));
    }

    #[test]
    fn cached_repo_identity_clears_when_head_disappears() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("missing-head");
        write_fake_tracked_repo(&root);
        let (_, cached) = git_status_snapshot_for_cwd(&root, None);
        std::fs::remove_file(root.join(".git/HEAD")).expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, cached.as_ref());

        assert_eq!(snapshot.repo_root, None);
    }

    #[test]
    fn refresh_attempts_ahead_behind_with_the_branch() {
        // The fixture's objects are fake, so the count fails and is retried:
        // the retry deadline shows the refresh tried it alongside the branch.
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("full-status");
        write_fake_tracked_repo(&root);

        let (snapshot, update) = git_status_snapshot_for_cwd(&root, None);

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead_behind, None);
        assert!(update.is_some_and(|entry| {
            entry.fingerprint.is_some()
                && entry
                    .retry_after
                    .is_some_and(|retry_after| retry_after > Instant::now())
        }));
    }

    #[test]
    fn git_status_reuses_cached_ahead_behind_when_fingerprint_matches() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("cache-hit");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root).expect("test precondition");
        let cached = GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after: None,
            snapshot: WorkspaceGitStatusSnapshot {
                repo_root: Some(root.clone()),
                branch: Some("main".into()),
                ahead_behind: Some(crate::git::AheadBehind {
                    ahead: 2,
                    behind: 1,
                }),
            },
            read_errors: Vec::new(),
        };

        let (snapshot, update) = git_status_snapshot_for_cwd(&root, Some(&cached));

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(
            snapshot.ahead_behind,
            Some(AheadBehind {
                ahead: 2,
                behind: 1
            })
        );
        assert_eq!(
            update.expect("test precondition").snapshot.ahead_behind,
            Some(AheadBehind {
                ahead: 2,
                behind: 1
            })
        );
    }

    #[test]
    fn git_status_does_not_reuse_cache_when_branch_changes_at_same_oid() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("branch-switch");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root).expect("test precondition");
        let cached = GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after: None,
            snapshot: WorkspaceGitStatusSnapshot {
                repo_root: Some(root.clone()),
                branch: Some("main".into()),
                ahead_behind: Some(crate::git::AheadBehind {
                    ahead: 4,
                    behind: 0,
                }),
            },
            read_errors: Vec::new(),
        };
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature\n")
            .expect("test precondition");
        std::fs::write(
            root.join(".git/refs/heads/feature"),
            "1111111111111111111111111111111111111111\n",
        )
        .expect("test precondition");
        std::fs::write(
            root.join(".git/config"),
            "[branch \"feature\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, Some(&cached));

        assert_eq!(snapshot.branch.as_deref(), Some("feature"));
        assert_eq!(snapshot.ahead_behind, None);
    }

    #[test]
    fn git_status_clears_ahead_behind_when_upstream_is_unset() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("upstream-unset");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root).expect("test precondition");
        let cached = GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after: None,
            snapshot: WorkspaceGitStatusSnapshot {
                repo_root: Some(root.clone()),
                branch: Some("main".into()),
                ahead_behind: Some(crate::git::AheadBehind {
                    ahead: 0,
                    behind: 3,
                }),
            },
            read_errors: Vec::new(),
        };
        std::fs::write(root.join(".git/config"), "").expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, Some(&cached));

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead_behind, None);
    }

    #[test]
    fn git_status_rebuilds_config_when_missing_include_appears() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("include-appears");
        write_fake_tracked_repo(&root);
        std::fs::write(
            root.join(".git/config"),
            "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n[include]\n\tpath = branch.cfg\n",
        )
        .expect("test precondition");
        let (_, cached) = git_status_snapshot_for_cwd(&root, None);
        std::fs::write(
            root.join(".git/branch.cfg"),
            "[branch \"main\"]\n\tremote = fork\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

        let (_, updated) = git_status_snapshot_for_cwd(&root, cached.as_ref());

        let upstream = updated
            .expect("test precondition")
            .fingerprint
            .expect("test precondition")
            .upstream
            .expect("test precondition");
        assert_eq!(upstream.remote, "fork");
    }

    #[test]
    fn git_status_fingerprint_reads_packed_refs() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("packed-refs");
        write_fake_tracked_repo(&root);
        std::fs::remove_file(root.join(".git/refs/remotes/origin/main"))
            .expect("test precondition");
        std::fs::write(
            root.join(".git/packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\n2222222222222222222222222222222222222222 refs/remotes/origin/main\n",
        )
        .expect("test precondition");

        let fingerprint = git_status_fingerprint(&root).expect("test precondition");

        assert_eq!(
            fingerprint
                .upstream
                .expect("test precondition")
                .oid
                .as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn linked_worktree_refresh_keeps_checkout_name_as_auto_label() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let (_, _, checkout) =
            crate::git::test_support::create_repo_with_linked_worktree("linked-refresh-label");

        let (snapshot, _) = git_status_snapshot_for_cwd(&checkout, None);
        let status =
            snapshot.into_workspace_status("workspace".into(), checkout.clone(), PathBuf::new());

        assert_eq!(
            status.auto_label,
            checkout
                .file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );
    }

    #[test]
    fn git_status_cache_key_is_per_linked_worktree_checkout() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("linked-worktree-keys");
        let common_dir = base.join("repo/.git");
        let worktree_one = base.join("one");
        let worktree_two = base.join("two");
        let git_dir_one = common_dir.join("worktrees/one");
        let git_dir_two = common_dir.join("worktrees/two");
        std::fs::create_dir_all(&git_dir_one).expect("test precondition");
        std::fs::create_dir_all(&git_dir_two).expect("test precondition");
        std::fs::create_dir_all(&worktree_one).expect("test precondition");
        std::fs::create_dir_all(&worktree_two).expect("test precondition");
        std::fs::write(
            worktree_one.join(".git"),
            format!("gitdir: {}\n", git_dir_one.display()),
        )
        .expect("test precondition");
        std::fs::write(
            worktree_two.join(".git"),
            format!("gitdir: {}\n", git_dir_two.display()),
        )
        .expect("test precondition");
        std::fs::write(git_dir_one.join("HEAD"), "ref: refs/heads/one\n")
            .expect("test precondition");
        std::fs::write(git_dir_two.join("HEAD"), "ref: refs/heads/two\n")
            .expect("test precondition");

        assert_ne!(
            git_status_cache_key(&worktree_one),
            git_status_cache_key(&worktree_two)
        );
    }

    /// As the reftable branch test: Git writes the store production reads
    /// through Git.
    #[test]
    fn git_status_fingerprint_reads_reftable_branch_identity() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = temp_test_dir("reftable-fingerprint");
        // host-program-ok: a reftable store is written by Git; production reads it through Git
        let output = super::super::discovery::run_git_output(
            &root,
            &["init", "--ref-format=reftable", "-b", "main"],
        )
        .expect("test precondition");
        assert!(
            output.status.success(),
            "this test needs a host Git with reftable support (2.45 or later): {output:?}"
        );
        git_written_fixture(&root, &["config", "user.email", "shepr@example.invalid"]);
        git_written_fixture(&root, &["config", "user.name", "Shepr Test"]);
        git_written_fixture(&root, &["commit", "--allow-empty", "-m", "initial"]);

        let fingerprint = git_status_fingerprint(&root).expect("test precondition");

        assert_eq!(
            fingerprint.head,
            GitHeadIdentity::Branch {
                full_ref: "refs/heads/main".into(),
                short_name: "main".into(),
                oid: super::super::discovery::git_rev_parse_verify(&root, "HEAD"),
            }
        );
    }

    /// Production counts ahead and behind with `git rev-list`, which walks
    /// real commit objects, so Git makes this fixture.
    #[test]
    fn git_status_recomputes_ahead_behind_when_head_moves() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let base = temp_test_dir("head-moves");
        let remote = base.join("remote.git");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).expect("test precondition");
        let remote_arg = remote.to_string_lossy().to_string();
        git_written_fixture(&base, &["init", "--bare", &remote_arg]);
        git_written_fixture(&repo, &["init"]);
        git_written_fixture(&repo, &["config", "user.email", "shepr@example.invalid"]);
        git_written_fixture(&repo, &["config", "user.name", "Shepr Test"]);
        git_written_fixture(&repo, &["commit", "--allow-empty", "-m", "initial"]);
        git_written_fixture(&repo, &["branch", "-M", "main"]);
        git_written_fixture(&repo, &["remote", "add", "origin", &remote_arg]);
        git_written_fixture(&repo, &["push", "-u", "origin", "main"]);

        let (initial, cache_entry) = git_status_snapshot_for_cwd(&repo, None);
        assert_eq!(
            initial.ahead_behind,
            Some(AheadBehind {
                ahead: 0,
                behind: 0
            })
        );
        git_written_fixture(&repo, &["commit", "--allow-empty", "-m", "ahead"]);

        let (updated, _) = git_status_snapshot_for_cwd(&repo, cache_entry.as_ref());

        assert_eq!(updated.branch.as_deref(), Some("main"));
        assert_eq!(
            updated.ahead_behind,
            Some(AheadBehind {
                ahead: 1,
                behind: 0
            })
        );
    }
}
