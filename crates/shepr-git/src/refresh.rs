//! The refresh algorithm: one pass over a set of targets, grouped by
//! checkout, read against the status cache and committed to it only once the
//! whole pass has been computed.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::status::{
    GitStatusCache, GitStatusCacheEntry, GitStatusDiscovery, git_status_discovery,
    git_status_snapshot_for_cwd, git_status_snapshot_for_discovery,
};
use crate::{GitReadError, GitStatus, GitStatusKey};

/// One cwd to refresh. `owner` is whatever the caller associates the answer
/// with; the refresh only carries it back. `known_key` is the checkout key the
/// caller last admitted for this cwd: with one, the refresh reuses that key's
/// cached entry; without one, it discovers the repository again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshTarget<T> {
    pub owner: T,
    pub cwd: PathBuf,
    pub known_key: Option<GitStatusKey>,
}

/// One target's answer, with the target's owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshedStatus<T> {
    pub owner: T,
    pub status: GitStatus,
}

/// What one refresh produced: an answer for every target, and the read
/// errors no answer still held in the cache carried before this refresh. A
/// refresh that panicked answers nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshOutcome<T> {
    pub statuses: Vec<RefreshedStatus<T>>,
    pub new_read_errors: Vec<GitReadError>,
}

impl<T> RefreshOutcome<T> {
    pub fn empty() -> Self {
        Self {
            statuses: Vec::new(),
            new_read_errors: Vec::new(),
        }
    }
}

struct RefreshJob<T> {
    key: GitStatusKey,
    cached: Option<GitStatusCacheEntry>,
    discovery: Option<GitStatusDiscovery>,
    targets: Vec<(T, PathBuf)>,
}

struct ComputedRefresh<T> {
    statuses: Vec<RefreshedStatus<T>>,
    cache_updates: Vec<(GitStatusKey, GitStatusCacheEntry)>,
}

/// The status cache and the refresh algorithm over it: explicit invalidation,
/// clearing, and refreshes whose results are committed whole or not at all.
#[derive(Debug, Default)]
pub struct GitRefresher {
    cache: GitStatusCache,
}

impl GitRefresher {
    /// Drops every cached miss, so the next refresh looks again at each cwd
    /// that had no readable repository instead of waiting out its retry delay.
    pub fn invalidate(&mut self) {
        self.cache.mark_due();
    }

    /// Forgets every cached entry and every reported read error.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Refreshes `targets`. The pass is computed against the cache without
    /// changing it, then committed: the visited entries replace the cache's,
    /// unvisited ones are released, and read errors are deduplicated against
    /// those the retained entries carry.
    ///
    /// This is where a panicking refresh is contained, and a caught panic
    /// becomes an empty outcome, so a caller waiting for the answer always
    /// gets one. A panic while computing leaves the cache as the last commit
    /// left it, since nothing was written. A panic while committing may have
    /// written part of the pass, so the cache is cleared and the next refresh
    /// rebuilds it.
    pub fn refresh<T>(&mut self, targets: Vec<RefreshTarget<T>>) -> RefreshOutcome<T> {
        self.refresh_with(targets, compute_refresh)
    }

    /// [`Self::refresh`] with the computing pass handed in, so a test can
    /// stand a panicking one in.
    fn refresh_with<T>(
        &mut self,
        targets: Vec<RefreshTarget<T>>,
        compute: impl FnOnce(Vec<RefreshTarget<T>>, &GitStatusCache) -> ComputedRefresh<T>,
    ) -> RefreshOutcome<T> {
        let cache = &mut self.cache;
        let mut committing = false;
        #[expect(
            clippy::disallowed_methods,
            reason = "a refresh must always produce an outcome: its caller holds a refresh \
                      in flight until one arrives, so an uncaught panic in a Git probe would \
                      stop Git status updates for the process's lifetime; the cache is \
                      cleared if the panic interrupted a commit"
        )]
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let computed = compute(targets, cache);
            committing = true;
            let new_read_errors = cache.apply_refresh(computed.cache_updates);
            RefreshOutcome {
                statuses: computed.statuses,
                new_read_errors,
            }
        }));
        caught.unwrap_or_else(|_| {
            if committing {
                cache.clear();
                tracing::warn!(
                    "git status refresh panicked while committing; cleared the status cache \
                     and reporting an empty refresh"
                );
            } else {
                tracing::warn!("git status refresh panicked; reporting an empty refresh");
            }
            RefreshOutcome::empty()
        })
    }
}

/// Groups targets by checkout key. A target with a known key joins that
/// key's cached entry; one without discovers its checkout, and a group any
/// such target joins drops the cached entry so the status is read afresh.
fn group_targets<T>(targets: Vec<RefreshTarget<T>>, cache: &GitStatusCache) -> Vec<RefreshJob<T>> {
    let mut indexes = HashMap::<GitStatusKey, usize>::new();
    let mut jobs = Vec::<RefreshJob<T>>::new();

    for target in targets {
        let reconcile = target.known_key.is_none();
        let (key, discovery) = match target.known_key {
            Some(key) => (key, None),
            None => {
                let discovery = git_status_discovery(&target.cwd);
                (discovery.cache_key().clone(), Some(discovery))
            }
        };
        if let Some(&index) = indexes.get(&key) {
            jobs[index].cached = jobs[index].cached.take().filter(|_| !reconcile);
            if jobs[index].discovery.is_none() {
                jobs[index].discovery = discovery;
            }
            jobs[index].targets.push((target.owner, target.cwd));
            continue;
        }

        let cached = cache.get(&key).filter(|_| !reconcile).cloned();
        indexes.insert(key.clone(), jobs.len());
        jobs.push(RefreshJob {
            key,
            cached,
            discovery,
            targets: vec![(target.owner, target.cwd)],
        });
    }

    jobs
}

fn compute_refresh<T>(
    targets: Vec<RefreshTarget<T>>,
    cache: &GitStatusCache,
) -> ComputedRefresh<T> {
    let mut statuses = Vec::new();
    let mut cache_updates = Vec::new();

    for job in group_targets(targets, cache) {
        let (snapshot, cache_entry) = match job.discovery {
            Some(discovery) => git_status_snapshot_for_discovery(discovery),
            None => git_status_snapshot_for_cwd(job.key.as_path(), job.cached.as_ref()),
        };
        if let Some(cache_entry) = cache_entry {
            cache_updates.push((job.key.clone(), cache_entry));
        }
        statuses.extend(job.targets.into_iter().map(|(owner, cwd)| RefreshedStatus {
            owner,
            status: snapshot.clone().into_status(cwd, job.key.clone()),
        }));
    }

    ComputedRefresh {
        statuses,
        cache_updates,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::test_support::temp_test_dir;

    fn target(owner: usize, cwd: PathBuf, known_key: Option<GitStatusKey>) -> RefreshTarget<usize> {
        RefreshTarget {
            owner,
            cwd,
            known_key,
        }
    }

    #[test]
    fn targets_in_one_checkout_share_one_cache_entry() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let repo = temp_test_dir("git-refresh-dedupe");
        let nested = repo.join("nested");
        let other = repo.join("other");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        // The repository as Git lays one out, in plain files: grouping only
        // needs the checkout discovered.
        std::fs::create_dir_all(repo.join(".git/objects")).expect("create git objects dir");
        std::fs::create_dir_all(repo.join(".git/refs/heads")).expect("create git refs dir");
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write git HEAD");

        let computed = compute_refresh(
            vec![
                target(1, nested.clone(), None),
                target(2, other.clone(), None),
            ],
            &GitStatusCache::default(),
        );

        assert_eq!(computed.cache_updates.len(), 1);
        assert_eq!(
            computed.cache_updates[0].0,
            GitStatusKey::Checkout(std::fs::canonicalize(&repo).expect("canonical repo path"))
        );
        assert_eq!(computed.statuses.len(), 2);
        assert_eq!(computed.statuses[0].owner, 1);
        assert_eq!(computed.statuses[0].status.cwd, nested);
        assert_eq!(computed.statuses[1].owner, 2);
        assert_eq!(computed.statuses[1].status.cwd, other);
    }

    #[test]
    fn shared_root_repo_refresh_keeps_target_specific_labels() {
        let cache_key = PathBuf::from("/");
        let cached = GitStatusCacheEntry::Miss {
            retry_after: Instant::now() + Duration::from_secs(30),
            repo_root: Some(cache_key.clone()),
            read_errors: Vec::new(),
        };
        let targets = ["alpha", "beta"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                target(
                    index,
                    cache_key.join(name),
                    Some(GitStatusKey::Checkout(cache_key.clone())),
                )
            })
            .collect();

        let mut cache = GitStatusCache::default();
        cache.apply_refresh(vec![(GitStatusKey::Checkout(cache_key), cached)]);
        let computed = compute_refresh(targets, &cache);

        assert_eq!(computed.cache_updates.len(), 1);
        assert_eq!(computed.statuses.len(), 2);
        assert_eq!(computed.statuses[0].status.label, "alpha");
        assert_eq!(computed.statuses[1].status.label, "beta");
        assert_eq!(
            computed.statuses[0].status.branch,
            crate::GitBranch::ReadFailed
        );
        assert_eq!(
            computed.statuses[1].status.branch,
            crate::GitBranch::ReadFailed
        );
    }

    #[test]
    fn rediscovery_ignores_the_cached_entry_of_the_discovered_key() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let cwd = temp_test_dir("git-refresh-rediscover");
        let cached = GitStatusCacheEntry::Miss {
            retry_after: Instant::now(),
            repo_root: None,
            read_errors: Vec::new(),
        };
        let mut cache = GitStatusCache::default();
        cache.apply_refresh(vec![(GitStatusKey::Outside(cwd.clone()), cached)]);

        let jobs = group_targets(vec![target(1, cwd, None)], &cache);

        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].cached.is_none());
    }

    #[test]
    fn refresh_commits_the_pass_and_invalidation_drops_its_misses() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let cwd = temp_test_dir("git-refresh-commit");
        let mut refresher = GitRefresher::default();

        let outcome = refresher.refresh(vec![target(7, cwd.clone(), None)]);

        assert_eq!(outcome.statuses.len(), 1);
        assert_eq!(outcome.statuses[0].owner, 7);
        assert_eq!(
            outcome.statuses[0].status.branch,
            crate::GitBranch::OutsideRepository
        );
        assert!(
            refresher
                .cache
                .get(&GitStatusKey::Outside(cwd.clone()))
                .is_some()
        );

        refresher.invalidate();
        assert!(refresher.cache.is_empty());
    }

    #[test]
    fn a_panicking_refresh_reports_an_empty_outcome_and_keeps_the_cache() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let cwd = temp_test_dir("git-refresh-panic");
        let mut refresher = GitRefresher::default();
        refresher.refresh(vec![target(1, cwd.clone(), None)]);
        let before = refresher.cache.clone();
        assert!(!before.is_empty());

        let outcome = refresher.refresh_with(vec![target(2, cwd, None)], |_, cache| {
            assert!(!cache.is_empty());
            panic!("simulated git refresh failure")
        });

        assert_eq!(outcome, RefreshOutcome::empty());
        assert_eq!(refresher.cache, before);
    }
}
