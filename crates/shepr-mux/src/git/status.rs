use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{AheadBehind, WorkspaceGitStatusSnapshot};

use super::{
    config::{ConfigCtx, FileDep, deps_current, read_config, stamp, upstream_full_ref},
    discovery::{
        GitWorktreeInfo, automatic_workspace_label, canonicalize_best_effort_path,
        fallback_label_from_cwd, git_ref_storage_is_reftable, git_rev_parse_verify,
        git_space_metadata_from_info, git_symbolic_head_full, git_worktree_info, read_git_ref_file,
        read_ref_oid,
    },
};

const GIT_STATUS_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitStatusRefreshDemand {
    pub branch: bool,
    pub ahead_behind: bool,
}

impl GitStatusRefreshDemand {
    #[cfg(any(test, feature = "test-api"))]
    pub const ALL: Self = Self {
        branch: true,
        ahead_behind: true,
    };

    pub fn is_empty(self) -> bool {
        !self.branch && !self.ahead_behind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusCacheEntry {
    pub fingerprint: Option<GitStatusFingerprint>,
    pub retry_after: Option<Instant>,
    pub snapshot: WorkspaceGitStatusSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusFingerprint {
    pub head: GitHeadIdentity,
    pub upstream: Option<GitUpstreamIdentity>,
    repository_context: RepoContext,
}

type RepoContext = (GitWorktreeInfo, bool, Vec<FileDep>, Option<ConfigCtx>);

fn repo_context(cwd: &Path) -> Option<RepoContext> {
    let info = git_worktree_info(cwd)?;
    let reftable = git_ref_storage_is_reftable(&info.git_common_dir);
    let mut paths = vec![info.repo_root.join(".git"), info.git_dir.join("commondir")];
    paths.push(info.git_dir.join("HEAD"));
    paths.push(info.git_common_dir.join("config"));
    paths.extend((info.git_dir != info.git_common_dir).then(|| info.git_dir.join("config")));
    let mut deps: Vec<_> = paths.into_iter().map(|path| stamp(path, None)).collect();
    deps[0].2 &= git_worktree_info(cwd).as_ref() == Some(&info)
        && git_ref_storage_is_reftable(&info.git_common_dir) == reftable
        && deps_current(&deps);
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

#[cfg(test)]
pub fn git_status_snapshot_for_cwd(
    cwd: &Path,
    cached: Option<&GitStatusCacheEntry>,
) -> (WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>) {
    git_status_snapshot_for_cwd_with_demand(cwd, cached, GitStatusRefreshDemand::ALL)
}

pub fn git_status_snapshot_for_cwd_with_demand(
    cwd: &Path,
    cached: Option<&GitStatusCacheEntry>,
    demand: GitStatusRefreshDemand,
) -> (WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>) {
    let now = Instant::now();
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
        .or_else(|| repo_context(cwd));
    let Some(repository_context) = repository_context else {
        let snapshot = WorkspaceGitStatusSnapshot {
            auto_label: fallback_label_from_cwd(cwd),
            branch: None,
            ahead_behind: None,
            space: None,
        };
        return (
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint: None,
                retry_after: Some(now + GIT_STATUS_RETRY_DELAY),
                snapshot,
            }),
        );
    };
    let auto_label = automatic_workspace_label(cwd, &repository_context.0.repo_root);
    let space = git_space_metadata_from_info(&repository_context.0);

    if !demand.ahead_behind {
        let fingerprint = fingerprint(repository_context, false);
        let branch = demand
            .branch
            .then(|| fingerprint.as_ref()?.branch_name())
            .flatten()
            .map(str::to_string);
        let snapshot = WorkspaceGitStatusSnapshot {
            auto_label,
            branch,
            ahead_behind: None,
            space: Some(space),
        };
        let cache_entry = if let Some(fingerprint) = fingerprint {
            let prior = cached.filter(|entry| {
                entry
                    .fingerprint
                    .as_ref()
                    .is_some_and(|cached| cached.same_head_and_repository_context(&fingerprint))
            });
            let prior_status = prior.and_then(|entry| {
                entry.fingerprint.as_ref().map(|fingerprint| {
                    (
                        fingerprint.clone(),
                        entry.retry_after,
                        entry.snapshot.ahead_behind,
                    )
                })
            });
            let (fingerprint, retry_after, ahead_behind) =
                prior_status.unwrap_or((fingerprint, Some(now), None));
            GitStatusCacheEntry {
                fingerprint: Some(fingerprint),
                retry_after,
                snapshot: WorkspaceGitStatusSnapshot {
                    ahead_behind,
                    ..snapshot.clone()
                },
            }
        } else {
            GitStatusCacheEntry {
                fingerprint: None,
                retry_after: Some(now + GIT_STATUS_RETRY_DELAY),
                snapshot: snapshot.clone(),
            }
        };
        return (snapshot, Some(cache_entry));
    }

    let Some(fingerprint) = fingerprint(repository_context, true) else {
        let snapshot = WorkspaceGitStatusSnapshot {
            auto_label,
            branch: None,
            ahead_behind: None,
            space: Some(space),
        };
        return (
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint: None,
                retry_after: Some(now + GIT_STATUS_RETRY_DELAY),
                snapshot,
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
            auto_label,
            branch,
            ahead_behind: cached.snapshot.ahead_behind,
            space: Some(space),
        };
        return (
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint: Some(fingerprint),
                retry_after: cached.retry_after,
                snapshot,
            }),
        );
    }

    let revision_pair = fingerprint.head_oid().zip(fingerprint.upstream_oid());
    let (ahead_behind, retry_after) = match revision_pair {
        Some((head_oid, upstream_oid)) => {
            let ahead_behind = git_ahead_behind_between(cwd, head_oid, upstream_oid);
            let retry_after = ahead_behind
                .is_none()
                .then(|| Instant::now() + GIT_STATUS_RETRY_DELAY);
            (ahead_behind, retry_after)
        }
        None => (None, None),
    };
    let snapshot = WorkspaceGitStatusSnapshot {
        auto_label,
        branch,
        ahead_behind,
        space: Some(space),
    };
    (
        snapshot.clone(),
        Some(GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after,
            snapshot,
        }),
    )
}

#[cfg(test)]
pub(super) fn git_status_fingerprint(cwd: &Path) -> Option<GitStatusFingerprint> {
    fingerprint(repo_context(cwd)?, true)
}

fn fingerprint(mut repo: RepoContext, include_upstream: bool) -> Option<GitStatusFingerprint> {
    let head = read_head_identity(&repo.0, repo.1)?;
    let upstream = match &head {
        GitHeadIdentity::Branch { short_name, .. } if include_upstream => {
            read_upstream(&mut repo, short_name)
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
    fn same_head_and_repository_context(&self, other: &Self) -> bool {
        self.head == other.head
            && self.repository_context.0 == other.repository_context.0
            && self.repository_context.1 == other.repository_context.1
            && self.repository_context.2 == other.repository_context.2
    }

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

fn read_head_identity(info: &GitWorktreeInfo, reftable: bool) -> Option<GitHeadIdentity> {
    if reftable {
        return read_head_identity_from_git(info);
    }

    read_head_identity_from_files(info)
}

fn read_head_identity_from_git(info: &GitWorktreeInfo) -> Option<GitHeadIdentity> {
    if let Some(full_ref) = git_symbolic_head_full(&info.repo_root) {
        let short_name = full_ref.strip_prefix("refs/heads/")?.to_string();
        let oid = git_rev_parse_verify(&info.repo_root, &full_ref);
        return Some(GitHeadIdentity::Branch {
            full_ref,
            short_name,
            oid,
        });
    }

    git_rev_parse_verify(&info.repo_root, "HEAD").map(|oid| GitHeadIdentity::Detached { oid })
}

fn read_head_identity_from_files(info: &GitWorktreeInfo) -> Option<GitHeadIdentity> {
    let head = read_git_ref_file(&info.git_dir.join("HEAD"))?;
    let head = head.trim();
    if let Some(full_ref) = head.strip_prefix("ref: ") {
        let short_name = full_ref.strip_prefix("refs/heads/")?.to_string();
        let oid = read_ref_oid(&info.git_common_dir, full_ref);
        return Some(GitHeadIdentity::Branch {
            full_ref: full_ref.to_string(),
            short_name,
            oid,
        });
    }

    (!head.is_empty()).then(|| GitHeadIdentity::Detached {
        oid: head.to_string(),
    })
}

fn read_upstream(repo: &mut RepoContext, branch: &str) -> Option<GitUpstreamIdentity> {
    if repo
        .3
        .as_ref()
        .is_none_or(|context| context.0 != branch || !deps_current(&context.2))
    {
        repo.3 = Some(read_config(&repo.0, branch));
    }
    let config = repo.3.as_ref()?.1.clone()?;
    let full_ref = upstream_full_ref(&config)?;
    let oid = if repo.1 {
        git_rev_parse_verify(&repo.0.repo_root, &full_ref)
    } else {
        read_ref_oid(&repo.0.git_common_dir, &full_ref)
    };
    Some(GitUpstreamIdentity {
        remote: config.remote,
        merge_ref: config.merge_ref,
        full_ref,
        oid,
    })
}

fn git_ahead_behind_between(cwd: &Path, head_oid: &str, upstream_oid: &str) -> Option<AheadBehind> {
    let range = format!("{head_oid}...{upstream_oid}");
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-list", "--left-right", "--count", &range])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_git_ahead_behind_output(&stdout)
}

fn parse_git_ahead_behind_output(stdout: &str) -> Option<AheadBehind> {
    let mut parts = stdout.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some(AheadBehind { ahead, behind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{
        live_git_space, run_git, temp_test_dir, write_fake_tracked_repo,
    };

    #[test]
    fn cache_key_preserves_non_utf8_checkout_path() {
        use std::os::unix::ffi::OsStringExt;

        let base = temp_test_dir("non-utf8-key");
        let root = base.join(std::ffi::OsString::from_vec(vec![
            b'r', b'e', b'p', b'o', 0x80,
        ]));
        write_fake_tracked_repo(&root);

        assert_eq!(
            git_status_cache_key(&root),
            Some(std::fs::canonicalize(&root).expect("test precondition"))
        );

        std::fs::remove_dir_all(base).expect("test precondition");
    }

    // HEAD edge cases, read through the status refresh the sidebar uses.

    #[test]
    fn branch_reads_head_from_standard_repo() {
        let root = temp_test_dir("standard-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test precondition");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, None);
        std::fs::remove_dir_all(root).expect("test precondition");

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
    }

    #[test]
    fn oversized_head_reports_no_branch() {
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
        std::fs::remove_dir_all(root).expect("test precondition");

        let branch_len = snapshot.branch.as_ref().map(String::len);
        assert!(
            snapshot.branch.is_none(),
            "oversized Git HEAD produced branch with {branch_len:?} bytes"
        );
    }

    #[test]
    fn branch_reads_head_from_worktree_gitdir_file() {
        let root = temp_test_dir("worktree");
        let worktree_git_dir = root.join(".bare/worktrees/feature");
        std::fs::create_dir_all(&worktree_git_dir).expect("test precondition");
        std::fs::write(root.join(".git"), "gitdir: .bare/worktrees/feature\n")
            .expect("test precondition");
        std::fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n")
            .expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, None);
        std::fs::remove_dir_all(root).expect("test precondition");

        assert_eq!(snapshot.branch.as_deref(), Some("feature"));
    }

    #[test]
    fn detached_head_reports_no_branch() {
        let root = temp_test_dir("detached-head");
        std::fs::create_dir_all(root.join(".git")).expect("test precondition");
        std::fs::write(root.join(".git/HEAD"), "3e1b9a8d\n").expect("test precondition");

        let (snapshot, update) = git_status_snapshot_for_cwd(&root, None);
        std::fs::remove_dir_all(root).expect("test precondition");

        assert_eq!(snapshot.branch, None);
        assert!(snapshot.space.is_some(), "a detached HEAD is still a repo");
        assert!(
            update
                .and_then(|entry| entry.fingerprint)
                .is_some_and(|fingerprint| fingerprint.head
                    == GitHeadIdentity::Detached {
                        oid: "3e1b9a8d".into()
                    })
        );
    }

    #[test]
    fn branch_reads_unborn_symbolic_head_from_reftable_repo() {
        let root = temp_test_dir("reftable-branch");
        let root_arg = root.to_string_lossy().to_string();
        let output = std::process::Command::new("git")
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .expect("test precondition");
        if !output.status.success() {
            std::fs::remove_dir_all(root).expect("test precondition");
            return;
        }

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, None);
        std::fs::remove_dir_all(root).expect("test precondition");

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_status_cache_key_ignores_invalid_git_marker() {
        let base = temp_test_dir("invalid-git-root");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(base.join(".git")).expect("test precondition");
        std::fs::create_dir_all(&cwd).expect("test precondition");

        assert_eq!(git_status_cache_key(&cwd), None);

        std::fs::remove_dir_all(base).expect("test precondition");
    }

    #[test]
    fn non_git_refresh_reuses_cached_miss_without_rechecking_filesystem() {
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
        let root = temp_test_dir("expired-miss");
        let (_, cache_entry) = git_status_snapshot_for_cwd(&root, None);
        let mut cache_entry = cache_entry.expect("non-Git result should be cached");
        cache_entry.retry_after = Some(Instant::now() - Duration::from_secs(1));
        write_fake_tracked_repo(&root);

        let (snapshot, update) = git_status_snapshot_for_cwd(&root, Some(&cache_entry));

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert!(update.is_some_and(|entry| entry.fingerprint.is_some()));
        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn cached_repo_identity_clears_when_head_disappears() {
        let root = temp_test_dir("missing-head");
        write_fake_tracked_repo(&root);
        let (_, cached) = git_status_snapshot_for_cwd(&root, None);
        std::fs::remove_file(root.join(".git/HEAD")).expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, cached.as_ref());

        assert_eq!(snapshot.space, None);
        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn branch_only_refresh_skips_ahead_behind_cache_work() {
        let root = temp_test_dir("branch-only");
        write_fake_tracked_repo(&root);

        let (snapshot, update) = git_status_snapshot_for_cwd_with_demand(
            &root,
            None,
            GitStatusRefreshDemand {
                branch: true,
                ahead_behind: false,
            },
        );

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead_behind, None);
        assert!(update.is_some_and(|entry| entry.fingerprint.is_some()));

        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn git_status_reuses_cached_ahead_behind_when_fingerprint_matches() {
        let root = temp_test_dir("cache-hit");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root).expect("test precondition");
        let cached = GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after: None,
            snapshot: WorkspaceGitStatusSnapshot {
                auto_label: "repo".into(),
                branch: Some("main".into()),
                ahead_behind: Some(crate::git::AheadBehind {
                    ahead: 2,
                    behind: 1,
                }),
                space: live_git_space(&root),
            },
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

        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn git_status_does_not_reuse_cache_when_branch_changes_at_same_oid() {
        let root = temp_test_dir("branch-switch");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root).expect("test precondition");
        let cached = GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after: None,
            snapshot: WorkspaceGitStatusSnapshot {
                auto_label: "repo".into(),
                branch: Some("main".into()),
                ahead_behind: Some(crate::git::AheadBehind {
                    ahead: 4,
                    behind: 0,
                }),
                space: live_git_space(&root),
            },
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

        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn git_status_clears_ahead_behind_when_upstream_is_unset() {
        let root = temp_test_dir("upstream-unset");
        write_fake_tracked_repo(&root);
        let fingerprint = git_status_fingerprint(&root).expect("test precondition");
        let cached = GitStatusCacheEntry {
            fingerprint: Some(fingerprint),
            retry_after: None,
            snapshot: WorkspaceGitStatusSnapshot {
                auto_label: "repo".into(),
                branch: Some("main".into()),
                ahead_behind: Some(crate::git::AheadBehind {
                    ahead: 0,
                    behind: 3,
                }),
                space: live_git_space(&root),
            },
        };
        std::fs::write(root.join(".git/config"), "").expect("test precondition");

        let (snapshot, _) = git_status_snapshot_for_cwd(&root, Some(&cached));

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead_behind, None);

        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn git_status_rebuilds_config_when_missing_include_appears() {
        let root = temp_test_dir("include-appears");
        write_fake_tracked_repo(&root);
        std::fs::write(
            root.join(".git/config"),
            "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n[include]\n\tpath = branch.cfg\n",
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
        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn git_status_fingerprint_reads_packed_refs() {
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

        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn linked_worktree_refresh_keeps_checkout_name_as_auto_label() {
        let (base, _, checkout) =
            crate::git::test_support::create_repo_with_linked_worktree("linked-refresh-label");

        let (snapshot, _) = git_status_snapshot_for_cwd(&checkout, None);

        assert_eq!(
            snapshot.auto_label,
            checkout
                .file_name()
                .expect("test precondition")
                .to_str()
                .expect("test precondition")
        );

        std::fs::remove_dir_all(base).expect("test precondition");
    }

    #[test]
    fn git_status_cache_key_is_per_linked_worktree_checkout() {
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

        std::fs::remove_dir_all(base).expect("test precondition");
    }

    #[test]
    fn git_status_fingerprint_reads_reftable_branch_identity() {
        let root = temp_test_dir("reftable-fingerprint");
        let root_arg = root.to_string_lossy().to_string();
        let output = std::process::Command::new("git")
            .args(["init", "--ref-format=reftable", "-b", "main", &root_arg])
            .output()
            .expect("test precondition");
        if !output.status.success() {
            std::fs::remove_dir_all(root).expect("test precondition");
            return;
        }
        run_git(&root, &["config", "user.email", "shepr@example.invalid"]);
        run_git(&root, &["config", "user.name", "Shepr Test"]);
        run_git(&root, &["commit", "--allow-empty", "-m", "initial"]);

        let fingerprint = git_status_fingerprint(&root).expect("test precondition");

        assert_eq!(
            fingerprint.head,
            GitHeadIdentity::Branch {
                full_ref: "refs/heads/main".into(),
                short_name: "main".into(),
                oid: git_rev_parse_verify(&root, "HEAD"),
            }
        );

        std::fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn git_status_recomputes_ahead_behind_when_head_moves() {
        let base = temp_test_dir("head-moves");
        let remote = base.join("remote.git");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).expect("test precondition");
        let remote_arg = remote.to_string_lossy().to_string();
        run_git(&base, &["init", "--bare", &remote_arg]);
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.email", "shepr@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Shepr Test"]);
        run_git(&repo, &["commit", "--allow-empty", "-m", "initial"]);
        run_git(&repo, &["branch", "-M", "main"]);
        run_git(&repo, &["remote", "add", "origin", &remote_arg]);
        run_git(&repo, &["push", "-u", "origin", "main"]);

        let (initial, cache_entry) = git_status_snapshot_for_cwd(&repo, None);
        assert_eq!(
            initial.ahead_behind,
            Some(AheadBehind {
                ahead: 0,
                behind: 0
            })
        );
        run_git(&repo, &["commit", "--allow-empty", "-m", "ahead"]);

        let (updated, _) = git_status_snapshot_for_cwd(&repo, cache_entry.as_ref());

        assert_eq!(updated.branch.as_deref(), Some("main"));
        assert_eq!(
            updated.ahead_behind,
            Some(AheadBehind {
                ahead: 1,
                behind: 0
            })
        );

        std::fs::remove_dir_all(base).expect("test precondition");
    }
}
