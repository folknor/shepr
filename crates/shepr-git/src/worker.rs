//! The long-lived thread that owns a [`GitRefresher`] and with it the status
//! cache. The handle sends it targets and invalidation; it answers each
//! refresh through the caller's publish function. The cache never leaves the
//! thread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use crate::refresh::{GitRefresher, RefreshOutcome, RefreshTarget};

enum Command<T> {
    Refresh(Vec<RefreshTarget<T>>),
    Invalidate,
    Clear,
}

type Publish<T> = Arc<dyn Fn(RefreshOutcome<T>) + Send + Sync>;

/// One started worker thread.
struct WorkerThread<T> {
    commands: mpsc::Sender<Command<T>>,
    handle: JoinHandle<()>,
    /// Refreshes sent to this thread whose `publish` call has not returned.
    /// Raised before the send and lowered by the thread after `publish`, so
    /// once the thread has finished it counts exactly the accepted refreshes
    /// it never published.
    unpublished: Arc<AtomicUsize>,
}

/// A thread that no longer takes commands and has not been joined yet.
struct RetiringThread {
    handle: JoinHandle<()>,
    unpublished: Arc<AtomicUsize>,
}

/// The handle to the Git status worker. The thread starts with the first
/// refresh and is started again, with an empty cache, once it stops taking
/// commands. It runs commands in the order they were sent, so an
/// invalidation or clear sent while a refresh runs applies to the cache that
/// refresh committed.
pub struct GitStatusWorker<T> {
    publish: Publish<T>,
    thread: Option<WorkerThread<T>>,
    /// Threads that stopped taking commands, kept until they have finished
    /// and can be joined without blocking. A thread that dropped its
    /// receiver may still be dropping queued commands or running
    /// thread-local destructors.
    retiring: Vec<RetiringThread>,
    /// Whether a joined thread left an accepted refresh unpublished and
    /// [`Self::take_lost_refresh`] has not reported it yet.
    lost: bool,
    /// Whether a refresh was accepted since the last clear. Invalidating or
    /// clearing a cache no refresh has filled is skipped.
    cache_may_hold_entries: bool,
}

impl<T: Send + 'static> GitStatusWorker<T> {
    /// A worker that calls `publish` on its own thread with the outcome of
    /// every refresh it accepts, exactly once per refresh, including one that
    /// panicked (whose outcome is empty). If the thread stops before a
    /// `publish` call returns, a panic in `publish` included, that refresh is
    /// reported by [`Self::take_lost_refresh`] instead, so `publish` must not
    /// deliver an outcome and then panic.
    pub fn new(publish: impl Fn(RefreshOutcome<T>) + Send + Sync + 'static) -> Self {
        Self {
            publish: Arc::new(publish),
            thread: None,
            retiring: Vec::new(),
            lost: false,
            cache_may_hold_entries: false,
        }
    }

    /// Queues a refresh of `targets`. `Ok` means its outcome will be
    /// published, or reported lost by [`Self::take_lost_refresh`]; an error
    /// means the worker thread could not be started and neither will happen.
    pub fn refresh(&mut self, targets: Vec<RefreshTarget<T>>) -> std::io::Result<()> {
        let mut command = Command::Refresh(targets);
        if let Some(thread) = &self.thread {
            thread.unpublished.fetch_add(1, Ordering::SeqCst);
            match thread.commands.send(command) {
                Ok(()) => {
                    self.cache_may_hold_entries = true;
                    return Ok(());
                }
                Err(mpsc::SendError(returned)) => {
                    thread.unpublished.fetch_sub(1, Ordering::SeqCst);
                    tracing::warn!("git status worker stopped; starting a new one");
                    command = returned;
                    self.retire_current();
                }
            }
        }
        let thread = self.spawn()?;
        thread.unpublished.fetch_add(1, Ordering::SeqCst);
        thread
            .commands
            .send(command)
            .map_err(|_| std::io::Error::other("git status worker stopped as it started"))?;
        self.thread = Some(thread);
        self.cache_may_hold_entries = true;
        Ok(())
    }

    /// True once when worker threads that finished since the last call left
    /// an accepted refresh unpublished. Never true for a thread that is
    /// still running, and never for a refresh whose `publish` call returned.
    /// A refresh reported here will not be published; the next
    /// [`Self::refresh`] starts a new thread with an empty cache if the
    /// current one has stopped. Only finished threads are joined, so this
    /// never blocks.
    pub fn take_lost_refresh(&mut self) -> bool {
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.handle.is_finished())
        {
            self.retire_current();
        }
        let (finished, running) = std::mem::take(&mut self.retiring)
            .into_iter()
            .partition::<Vec<_>, _>(|thread| thread.handle.is_finished());
        self.retiring = running;
        for thread in finished {
            // A panic payload carries nothing the lost report does not; the
            // panic hook has already logged it.
            thread.handle.join().ok();
            if thread.unpublished.load(Ordering::SeqCst) > 0 {
                self.lost = true;
            }
        }
        std::mem::take(&mut self.lost)
    }

    /// Drops every cached miss once the commands sent before it have run.
    pub fn invalidate(&mut self) {
        if self.cache_may_hold_entries {
            self.send(Command::Invalidate);
        }
    }

    /// Forgets every cached entry and reported read error once the commands
    /// sent before it have run.
    pub fn clear(&mut self) {
        if self.cache_may_hold_entries {
            self.send(Command::Clear);
            self.cache_may_hold_entries = false;
        }
    }

    fn send(&mut self, command: Command<T>) {
        // A stopped worker took its cache with it, which leaves nothing to
        // invalidate or clear. It retires so any refresh it lost is reported
        // once it finishes; the next refresh starts a new one.
        if let Some(thread) = &self.thread
            && thread.commands.send(command).is_err()
        {
            self.retire_current();
        }
    }

    /// Stops sending to the current thread and keeps it until it has
    /// finished, when [`Self::take_lost_refresh`] joins it and reports
    /// whether it lost an accepted refresh.
    fn retire_current(&mut self) {
        if let Some(thread) = self.thread.take() {
            let WorkerThread {
                commands,
                handle,
                unpublished,
            } = thread;
            drop(commands);
            self.retiring.push(RetiringThread {
                handle,
                unpublished,
            });
        }
        self.cache_may_hold_entries = false;
    }

    fn spawn(&self) -> std::io::Result<WorkerThread<T>> {
        let (commands, receiver) = mpsc::channel();
        let publish = Arc::clone(&self.publish);
        let unpublished = Arc::new(AtomicUsize::new(0));
        let thread_unpublished = Arc::clone(&unpublished);
        let handle = std::thread::Builder::new()
            .name("shepr-git-refresh".into())
            .spawn(move || run(&receiver, publish.as_ref(), &thread_unpublished))?;
        Ok(WorkerThread {
            commands,
            handle,
            unpublished,
        })
    }
}

/// Runs commands until the handle is dropped.
fn run<T>(
    commands: &mpsc::Receiver<Command<T>>,
    publish: &(dyn Fn(RefreshOutcome<T>) + Send + Sync),
    unpublished: &AtomicUsize,
) {
    let mut refresher = GitRefresher::default();
    while let Ok(command) = commands.recv() {
        match command {
            Command::Refresh(targets) => {
                publish(refresher.refresh(targets));
                unpublished.fetch_sub(1, Ordering::SeqCst);
            }
            Command::Invalidate => refresher.invalidate(),
            Command::Clear => refresher.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{GitBranch, GitStatusKey};

    const WAIT: Duration = Duration::from_secs(30);

    fn target<T>(owner: T, cwd: &std::path::Path) -> RefreshTarget<T> {
        RefreshTarget {
            owner,
            cwd: cwd.to_path_buf(),
            known_key: Some(GitStatusKey::Outside(cwd.to_path_buf())),
        }
    }

    fn wait_for_lost_refresh<T: Send + 'static>(worker: &mut GitStatusWorker<T>) {
        let deadline = Instant::now() + WAIT;
        while !worker.take_lost_refresh() {
            assert!(Instant::now() < deadline, "lost refresh was not reported");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// An owner whose drop, when gated, signals that it started and then
    /// blocks until released.
    struct GatedOwner {
        id: usize,
        drop_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }

    impl Drop for GatedOwner {
        fn drop(&mut self) {
            if let Some((dropping, release)) = self.drop_gate.take() {
                dropping.send(()).ok();
                release.recv_timeout(WAIT).ok();
            }
        }
    }

    fn plain_owner(id: usize) -> GatedOwner {
        GatedOwner {
            id,
            drop_gate: None,
        }
    }

    #[test]
    fn every_accepted_refresh_publishes_one_outcome_in_order() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let cwd = crate::test_support::temp_test_dir("git-worker-order");
        let (sender, outcomes) = mpsc::channel();
        let mut worker = GitStatusWorker::new(move |outcome: RefreshOutcome<usize>| {
            sender.send(outcome).ok();
        });

        worker.clear();
        worker.invalidate();
        for owner in 0..2 {
            worker
                .refresh(vec![RefreshTarget {
                    owner,
                    cwd: cwd.clone(),
                    known_key: (owner == 1).then(|| GitStatusKey::Outside(cwd.clone())),
                }])
                .expect("worker starts");
            worker.invalidate();
        }
        worker.clear();

        for owner in 0..2 {
            let outcome = outcomes.recv_timeout(WAIT).expect("refresh outcome");
            assert_eq!(outcome.statuses.len(), 1);
            assert_eq!(outcome.statuses[0].owner, owner);
            assert_eq!(
                outcome.statuses[0].status.branch,
                GitBranch::OutsideRepository
            );
        }
        assert!(!worker.take_lost_refresh());
    }

    #[test]
    fn panicking_publish_reports_one_lost_refresh_and_the_next_refresh_works() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let cwd = crate::test_support::temp_test_dir("git-worker-lost");
        let (sender, outcomes) = mpsc::channel();
        let panicked = AtomicBool::new(false);
        let mut worker = GitStatusWorker::new(move |outcome: RefreshOutcome<usize>| {
            if !panicked.swap(true, Ordering::SeqCst) {
                panic!("publish failed");
            }
            sender.send(outcome).ok();
        });

        worker
            .refresh(vec![target(0, &cwd)])
            .expect("worker starts");
        wait_for_lost_refresh(&mut worker);
        assert!(!worker.take_lost_refresh());

        worker
            .refresh(vec![target(1, &cwd)])
            .expect("worker restarts");
        let outcome = outcomes.recv_timeout(WAIT).expect("refresh outcome");
        assert_eq!(outcome.statuses.len(), 1);
        assert_eq!(outcome.statuses[0].owner, 1);
        assert!(outcomes.try_recv().is_err());
        assert!(!worker.take_lost_refresh());
    }

    #[test]
    fn published_refresh_is_not_reported_lost_when_the_thread_stops() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let cwd = crate::test_support::temp_test_dir("git-worker-published");
        let (sender, outcomes) = mpsc::channel();
        let mut worker = GitStatusWorker::new(move |outcome: RefreshOutcome<usize>| {
            sender.send(outcome).ok();
        });

        worker
            .refresh(vec![target(0, &cwd)])
            .expect("worker starts");
        outcomes.recv_timeout(WAIT).expect("refresh outcome");
        worker.retire_current();

        let deadline = Instant::now() + WAIT;
        while !worker.retiring.is_empty() {
            assert!(!worker.take_lost_refresh());
            assert!(Instant::now() < deadline, "retired thread was not joined");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!worker.take_lost_refresh());
    }

    #[test]
    fn disconnected_thread_is_not_joined_until_it_finishes() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let cwd = crate::test_support::temp_test_dir("git-worker-retiring");
        let (sender, outcomes) = mpsc::channel();
        let (go, go_receiver) = mpsc::channel::<()>();
        let go_receiver = std::sync::Mutex::new(go_receiver);
        let panicked = AtomicBool::new(false);
        let mut worker = GitStatusWorker::new(move |outcome: RefreshOutcome<GatedOwner>| {
            if !panicked.swap(true, Ordering::SeqCst) {
                let gate = go_receiver.lock().expect("go gate");
                gate.recv_timeout(WAIT).ok();
                drop(gate);
                panic!("publish failed");
            }
            let ids: Vec<usize> = outcome.statuses.iter().map(|s| s.owner.id).collect();
            sender.send(ids).ok();
        });
        let (dropping, dropping_receiver) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();

        // The first refresh holds the thread in publish while the second is
        // queued behind it; the panic then drops the receiver, whose queued
        // command blocks in its owner's drop.
        worker
            .refresh(vec![target(plain_owner(0), &cwd)])
            .expect("worker starts");
        worker
            .refresh(vec![target(
                GatedOwner {
                    id: 1,
                    drop_gate: Some((dropping, release_receiver)),
                },
                &cwd,
            )])
            .expect("refresh queued");
        go.send(()).expect("release publish");
        dropping_receiver
            .recv_timeout(WAIT)
            .expect("queued refresh is being dropped");

        assert!(!worker.take_lost_refresh());
        worker
            .refresh(vec![target(plain_owner(2), &cwd)])
            .expect("a new worker starts");
        assert_eq!(outcomes.recv_timeout(WAIT).expect("refresh outcome"), [2]);
        worker.invalidate();
        worker.clear();
        assert_eq!(worker.retiring.len(), 1);
        assert!(!worker.retiring[0].handle.is_finished());
        assert!(!worker.take_lost_refresh());

        release.send(()).expect("release queued drop");
        wait_for_lost_refresh(&mut worker);
        assert!(worker.retiring.is_empty());
        assert!(!worker.take_lost_refresh());
        assert!(worker.thread.is_some());
        assert!(outcomes.try_recv().is_err());
    }
}
