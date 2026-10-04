//! The long-lived thread that owns a [`GitRefresher`] and with it the status
//! cache. The handle sends it targets and invalidation; it answers each
//! refresh through the caller's publish function. The cache never leaves the
//! thread.
//!
//! A refresh can block for good: only the Git subprocess has a deadline, and
//! discovery and config tracking make filesystem calls that hang with a hung
//! mount. The handle therefore watches each thread's progress, and
//! [`GitStatusWorker::abandon_stalled`] gives up on one that owes a refresh and
//! has made none for [`GIT_REFRESH_STALL_BOUND`]. An abandoned thread is left
//! detached and joined only once it has finished; it publishes nothing more,
//! and the paths its stalled step reads are left out of later refreshes until
//! it finishes, so a mount that stays hung holds one thread, not one per
//! refresh. A thread stalled with no step running (blocked in a destructor,
//! say) is abandoned too, since the handle is wedged behind it, but it has no
//! paths to keep out; a recurring one holds an abandonment slot each time
//! until it finishes. At most [`MAX_ABANDONED_GIT_REFRESH_THREADS`] are left
//! alive at once, and past that a stall is waited out.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::limits::{GIT_REFRESH_STALL_BOUND, MAX_ABANDONED_GIT_REFRESH_THREADS};
use crate::refresh::{GitRefresher, RefreshOutcome, RefreshTarget};

enum Command<T> {
    Refresh(Vec<RefreshTarget<T>>),
    Invalidate,
    Clear,
}

type Publish<T> = Arc<dyn Fn(RefreshOutcome<T>) + Send + Sync>;

type RefreshFn<T> = dyn Fn(&mut GitRefresher, Vec<RefreshTarget<T>>, &RefreshProgress) -> RefreshOutcome<T>
    + Send
    + Sync;

fn lock<V>(mutex: &Mutex<V>) -> MutexGuard<'_, V> {
    // Every critical section here is an assignment or a clone, so a panic
    // elsewhere on the holding thread leaves the value whole.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a refresh is: when it last made progress, and the paths of the step
/// it is running, if any. The worker reads it to tell a refresh that is slow
/// from one that is stuck, and which paths a stuck one is stuck on.
pub struct RefreshProgress {
    activity: Mutex<Activity>,
}

struct Activity {
    since: Instant,
    paths: Vec<PathBuf>,
}

impl Default for RefreshProgress {
    fn default() -> Self {
        Self {
            activity: Mutex::new(Activity {
                since: Instant::now(),
                paths: Vec::new(),
            }),
        }
    }
}

impl RefreshProgress {
    /// Records that a step reading `paths` starts now. A refresh that blocks
    /// in that step is abandoned with these paths, which later refreshes
    /// leave out until it finishes.
    pub fn step(&self, paths: Vec<PathBuf>) {
        let mut activity = lock(&self.activity);
        activity.since = Instant::now();
        activity.paths = paths;
    }

    /// Records progress outside any step.
    fn settle(&self) {
        self.step(Vec::new());
    }

    /// The paths of the current step, if no progress was made in the stall
    /// bound before `now`.
    pub(crate) fn stalled_paths(&self, now: Instant) -> Option<Vec<PathBuf>> {
        let activity = lock(&self.activity);
        (now.saturating_duration_since(activity.since) >= GIT_REFRESH_STALL_BOUND)
            .then(|| activity.paths.clone())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Taking commands or computing a refresh.
    Running,
    /// Inside the caller's `publish`, which cannot be abandoned half way.
    Publishing,
    /// Given up on by the handle: the thread publishes nothing more and stops
    /// once it returns from what it was blocked in.
    Abandoned,
}

/// What the handle and one thread share.
struct ThreadShared {
    /// Refreshes sent to this thread whose `publish` call has not returned.
    /// Raised before the send and lowered by the thread after `publish`, so
    /// once the thread has finished it counts exactly the accepted refreshes
    /// it never published.
    unpublished: AtomicUsize,
    phase: Mutex<Phase>,
    progress: RefreshProgress,
}

impl ThreadShared {
    fn new() -> Self {
        Self {
            unpublished: AtomicUsize::new(0),
            phase: Mutex::new(Phase::Running),
            progress: RefreshProgress::default(),
        }
    }

    /// Moves a running thread to `phase`. False when it is publishing or
    /// already abandoned. Abandonment and publication both leave `Running`
    /// through here, so exactly one of them happens to an outcome.
    fn leave_running(&self, phase: Phase) -> bool {
        let mut current = lock(&self.phase);
        if *current != Phase::Running {
            return false;
        }
        *current = phase;
        true
    }

    fn finish_publishing(&self) {
        let mut current = lock(&self.phase);
        if *current == Phase::Publishing {
            *current = Phase::Running;
        }
    }

    fn is_abandoned(&self) -> bool {
        *lock(&self.phase) == Phase::Abandoned
    }

    /// The paths the thread is stuck on, if it owes a refresh and has made no
    /// progress in the stall bound before `now`.
    fn stall(&self, now: Instant) -> Option<Vec<PathBuf>> {
        if self.unpublished.load(Ordering::SeqCst) == 0 {
            return None;
        }
        self.progress.stalled_paths(now)
    }
}

/// One started worker thread.
struct WorkerThread<T> {
    commands: mpsc::Sender<Command<T>>,
    handle: JoinHandle<()>,
    shared: Arc<ThreadShared>,
}

/// A thread that no longer takes commands and has not been joined yet.
struct RetiringThread {
    handle: JoinHandle<()>,
    shared: Arc<ThreadShared>,
}

/// A thread given up on while it was stuck, kept until it has finished and
/// can be joined without blocking.
struct AbandonedThread {
    handle: JoinHandle<()>,
    /// The paths its stalled step reads, which refreshes leave out meanwhile.
    stuck: Vec<PathBuf>,
}

/// The handle to the Git status worker. The thread starts with the first
/// refresh and is started again, with an empty cache, once it stops taking
/// commands or is abandoned. It runs commands in the order they were sent, so
/// an invalidation or clear sent while a refresh runs applies to the cache
/// that refresh committed.
pub struct GitStatusWorker<T> {
    publish: Publish<T>,
    refresh: Arc<RefreshFn<T>>,
    thread: Option<WorkerThread<T>>,
    /// Threads that stopped taking commands, kept until they have finished
    /// and can be joined without blocking. A thread that dropped its
    /// receiver may still be dropping queued commands or running
    /// thread-local destructors.
    retiring: Vec<RetiringThread>,
    /// Threads given up on while stuck, at most
    /// [`MAX_ABANDONED_GIT_REFRESH_THREADS`] of them, each joined once it has
    /// finished.
    abandoned: Vec<AbandonedThread>,
    /// Whether a joined thread left an accepted refresh unpublished and
    /// [`Self::take_lost_refresh`] has not reported it yet.
    lost: bool,
    /// Whether a refresh was accepted since the last clear. Invalidating or
    /// clearing a cache no refresh has filled is skipped.
    cache_may_hold_entries: bool,
    /// Whether a stall found every abandonment slot taken and was logged; a
    /// slot freeing up rearms the log.
    abandon_limit_logged: bool,
}

impl<T: Send + 'static> GitStatusWorker<T> {
    /// A worker that calls `publish` on its own thread with the outcome of
    /// every refresh it accepts, exactly once per refresh, including one that
    /// panicked (whose outcome is empty). If the thread stops before a
    /// `publish` call returns, a panic in `publish` included, that refresh is
    /// reported by [`Self::take_lost_refresh`] instead, so `publish` must not
    /// deliver an outcome and then panic. A refresh whose thread is abandoned
    /// is reported by [`Self::abandon_stalled`] and never published.
    pub fn new(publish: impl Fn(RefreshOutcome<T>) + Send + Sync + 'static) -> Self {
        Self::with_refresher(publish, |refresher, targets, progress| {
            refresher.refresh(targets, progress)
        })
    }

    /// [`Self::new`] with the refresh itself handed in: the seam a test
    /// double stands a stalled or failing refresh in through, in place of the
    /// real refresh and its cache. `refresh` runs on the worker thread and
    /// reports each step it starts to the progress it is given.
    pub fn with_refresh(
        publish: impl Fn(RefreshOutcome<T>) + Send + Sync + 'static,
        refresh: impl Fn(Vec<RefreshTarget<T>>, &RefreshProgress) -> RefreshOutcome<T>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self::with_refresher(publish, move |_, targets, progress| {
            refresh(targets, progress)
        })
    }

    fn with_refresher(
        publish: impl Fn(RefreshOutcome<T>) + Send + Sync + 'static,
        refresh: impl Fn(
            &mut GitRefresher,
            Vec<RefreshTarget<T>>,
            &RefreshProgress,
        ) -> RefreshOutcome<T>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            publish: Arc::new(publish),
            refresh: Arc::new(refresh),
            thread: None,
            retiring: Vec::new(),
            abandoned: Vec::new(),
            lost: false,
            cache_may_hold_entries: false,
            abandon_limit_logged: false,
        }
    }

    /// Queues a refresh of `targets`. `Ok` means its outcome will be
    /// published, or reported lost by [`Self::take_lost_refresh`] or
    /// abandoned by [`Self::abandon_stalled`]; an error means the worker
    /// thread could not be started and none of those will happen.
    ///
    /// A target whose cwd or known checkout lies under a path an abandoned
    /// thread is still stuck on is left out, and the outcome carries no
    /// status for it.
    pub fn refresh(&mut self, mut targets: Vec<RefreshTarget<T>>) -> std::io::Result<()> {
        self.reap_abandoned();
        let requested = targets.len();
        targets.retain(|target| !self.is_stuck(target));
        if targets.len() < requested {
            tracing::debug!(
                skipped = requested - targets.len(),
                "left Git status targets on a stalled path out of this refresh"
            );
        }
        let mut command = Command::Refresh(targets);
        if let Some(thread) = &self.thread {
            Self::count_sent_refresh(&thread.shared);
            match thread.commands.send(command) {
                Ok(()) => {
                    self.cache_may_hold_entries = true;
                    return Ok(());
                }
                Err(mpsc::SendError(returned)) => {
                    thread.shared.unpublished.fetch_sub(1, Ordering::SeqCst);
                    tracing::warn!("git status worker stopped; starting a new one");
                    command = returned;
                    self.retire_current();
                }
            }
        }
        let thread = self.spawn()?;
        Self::count_sent_refresh(&thread.shared);
        thread
            .commands
            .send(command)
            .map_err(|_| std::io::Error::other("git status worker stopped as it started"))?;
        self.thread = Some(thread);
        self.cache_may_hold_entries = true;
        Ok(())
    }

    /// Counts a refresh about to be sent. A thread that owed nothing was idle,
    /// so the stall clock starts from the send rather than from whatever the
    /// thread last did.
    fn count_sent_refresh(shared: &ThreadShared) {
        if shared.unpublished.fetch_add(1, Ordering::SeqCst) == 0 {
            shared.progress.settle();
        }
    }

    /// True once when worker threads that finished since the last call left
    /// an accepted refresh unpublished. Never true for a thread that is
    /// still running or was abandoned, and never for a refresh whose
    /// `publish` call returned. A refresh reported here will not be
    /// published; the next [`Self::refresh`] starts a new thread with an
    /// empty cache if the current one has stopped. Only finished threads are
    /// joined, so this never blocks.
    pub fn take_lost_refresh(&mut self) -> bool {
        self.reap_abandoned();
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
            if thread.shared.unpublished.load(Ordering::SeqCst) > 0 {
                self.lost = true;
            }
        }
        std::mem::take(&mut self.lost)
    }

    /// Abandons every thread that owes a refresh and has made no progress in
    /// the stall bound before `now`, while fewer than the abandoned-thread
    /// limit are still alive. True when that gave up on an accepted refresh:
    /// it will never be published nor reported lost. The next
    /// [`Self::refresh`] starts a new thread with an empty cache, and leaves
    /// out the targets under the paths an abandoned thread's stalled step
    /// reads until that thread finishes. Never blocks.
    pub fn abandon_stalled(&mut self, now: Instant) -> bool {
        self.reap_abandoned();
        let mut gave_up = false;
        if let Some(thread) = self.thread.take() {
            match self.abandon_if_stalled(&thread.shared, &thread.handle, now) {
                Some(stuck) => {
                    let WorkerThread {
                        commands, handle, ..
                    } = thread;
                    drop(commands);
                    self.abandoned.push(AbandonedThread { handle, stuck });
                    self.cache_may_hold_entries = false;
                    gave_up = true;
                }
                None => self.thread = Some(thread),
            }
        }
        for thread in std::mem::take(&mut self.retiring) {
            match self.abandon_if_stalled(&thread.shared, &thread.handle, now) {
                Some(stuck) => {
                    self.abandoned.push(AbandonedThread {
                        handle: thread.handle,
                        stuck,
                    });
                    gave_up = true;
                }
                None => self.retiring.push(thread),
            }
        }
        gave_up
    }

    /// Marks a thread abandoned if it is alive, stalled and an abandonment
    /// slot is free, returning the paths it is stuck on.
    fn abandon_if_stalled(
        &mut self,
        shared: &ThreadShared,
        handle: &JoinHandle<()>,
        now: Instant,
    ) -> Option<Vec<PathBuf>> {
        // A finished thread is joined by `take_lost_refresh`, which reports
        // what it left unpublished.
        if handle.is_finished() {
            return None;
        }
        let stuck = shared.stall(now)?;
        if self.abandoned.len() >= MAX_ABANDONED_GIT_REFRESH_THREADS {
            if !self.abandon_limit_logged {
                self.abandon_limit_logged = true;
                tracing::warn!(
                    paths = ?stuck,
                    abandoned = self.abandoned.len(),
                    "git status refresh is stalled, but the abandoned-thread limit is reached; \
                     waiting for it"
                );
            }
            return None;
        }
        if !shared.leave_running(Phase::Abandoned) {
            return None;
        }
        if stuck.is_empty() {
            // Blocked with no step running (in a destructor, say): nothing
            // names what to keep out, but the handle is wedged behind the
            // thread, so it is abandoned all the same. It holds a slot until
            // it finishes, which bounds the threads a recurring one can hold.
            tracing::warn!(
                "git status worker made no progress within its bound with no step running; \
                 abandoned its thread, with no paths to leave out of refreshes"
            );
        } else {
            tracing::warn!(
                paths = ?stuck,
                "git status refresh made no progress within its bound; abandoned its worker \
                 thread and left these paths out of refreshes until it finishes"
            );
        }
        Some(stuck)
    }

    /// Joins the abandoned threads that have finished, which lifts their
    /// stuck paths.
    fn reap_abandoned(&mut self) {
        if self.abandoned.is_empty() {
            return;
        }
        let (finished, running) = std::mem::take(&mut self.abandoned)
            .into_iter()
            .partition::<Vec<_>, _>(|thread| thread.handle.is_finished());
        self.abandoned = running;
        for thread in finished {
            thread.handle.join().ok();
            self.abandon_limit_logged = false;
            tracing::info!(
                paths = ?thread.stuck,
                "abandoned git status worker thread finished; refreshing its paths again"
            );
        }
    }

    fn is_stuck(&self, target: &RefreshTarget<T>) -> bool {
        self.abandoned
            .iter()
            .flat_map(|thread| &thread.stuck)
            .any(|stuck| {
                target.cwd.starts_with(stuck)
                    || target
                        .known_key
                        .as_ref()
                        .is_some_and(|key| key.as_path().starts_with(stuck))
            })
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
                shared,
            } = thread;
            drop(commands);
            self.retiring.push(RetiringThread { handle, shared });
        }
        self.cache_may_hold_entries = false;
    }

    fn spawn(&self) -> std::io::Result<WorkerThread<T>> {
        let (commands, receiver) = mpsc::channel();
        let publish = Arc::clone(&self.publish);
        let refresh = Arc::clone(&self.refresh);
        let shared = Arc::new(ThreadShared::new());
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("shepr-git-refresh".into())
            .spawn(move || {
                run(
                    &receiver,
                    publish.as_ref(),
                    refresh.as_ref(),
                    &thread_shared,
                );
            })?;
        Ok(WorkerThread {
            commands,
            handle,
            shared,
        })
    }
}

/// Ends the publishing phase however `publish` leaves, unwinding included: a
/// thread whose `publish` panicked can still block afterwards, dropping its
/// queued commands, and must stay abandonable then.
struct PublishingGuard<'a>(&'a ThreadShared);

impl Drop for PublishingGuard<'_> {
    fn drop(&mut self) {
        self.0.finish_publishing();
    }
}

/// Runs commands until the handle is dropped or abandons the thread.
fn run<T>(
    commands: &mpsc::Receiver<Command<T>>,
    publish: &(dyn Fn(RefreshOutcome<T>) + Send + Sync),
    refresh: &RefreshFn<T>,
    shared: &ThreadShared,
) {
    let mut refresher = GitRefresher::default();
    while let Ok(command) = commands.recv() {
        // An abandoned thread's queued commands belong to a handle that has
        // moved on, and its cache to no one.
        if shared.is_abandoned() {
            return;
        }
        match command {
            Command::Refresh(targets) => {
                shared.progress.settle();
                let outcome = refresh(&mut refresher, targets, &shared.progress);
                shared.progress.settle();
                // The handle abandoned this refresh while it ran: the outcome
                // is late and is dropped unpublished.
                if !shared.leave_running(Phase::Publishing) {
                    return;
                }
                let publishing = PublishingGuard(shared);
                publish(outcome);
                shared.unpublished.fetch_sub(1, Ordering::SeqCst);
                shared.progress.settle();
                drop(publishing);
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
    use crate::{GitBranch, GitStatus, GitStatusKey, RefreshedStatus};

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

    fn wait_for_abandoned_threads<T: Send + 'static>(worker: &mut GitStatusWorker<T>) {
        let deadline = Instant::now() + WAIT;
        loop {
            worker.reap_abandoned();
            if worker.abandoned.is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "abandoned thread did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// An instant past the stall bound of any progress recorded so far.
    fn past_the_stall_bound() -> Instant {
        Instant::now() + GIT_REFRESH_STALL_BOUND
    }

    /// An outcome answering every target outside any repository, without
    /// touching the filesystem.
    fn answer_all<T>(targets: Vec<RefreshTarget<T>>) -> RefreshOutcome<T> {
        RefreshOutcome {
            statuses: targets
                .into_iter()
                .map(|target| RefreshedStatus {
                    owner: target.owner,
                    status: GitStatus {
                        key: GitStatusKey::Outside(target.cwd.clone()),
                        cwd: target.cwd,
                        branch: GitBranch::OutsideRepository,
                        ahead_behind: None,
                    },
                })
                .collect(),
            new_read_errors: Vec::new(),
        }
    }

    /// A refresh double that, for every target whose cwd is under `stuck`,
    /// starts a step on that cwd, reports it on `entered`, and blocks until
    /// `released` is set; every other target is answered at once.
    fn blocking_refresh(
        stuck: PathBuf,
        entered: mpsc::Sender<PathBuf>,
        released: Arc<AtomicBool>,
    ) -> impl Fn(Vec<RefreshTarget<usize>>, &RefreshProgress) -> RefreshOutcome<usize>
    + Send
    + Sync
    + 'static {
        move |targets, progress| {
            for target in &targets {
                if target.cwd.starts_with(&stuck) {
                    progress.step(vec![target.cwd.clone()]);
                    entered.send(target.cwd.clone()).ok();
                    let deadline = Instant::now() + WAIT;
                    while !released.load(Ordering::SeqCst) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            answer_all(targets)
        }
    }

    fn owners<T: Copy>(outcome: &RefreshOutcome<T>) -> Vec<T> {
        outcome.statuses.iter().map(|status| status.owner).collect()
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

    #[test]
    fn a_thread_blocked_after_a_panicking_publish_is_abandoned_not_lost() {
        let scratch = shepr_test_support::ScratchDir::new("git-worker-blocked-drop");
        let cwd = scratch.join("cwd");
        let (sender, outcomes) = mpsc::channel();
        let (go, go_receiver) = mpsc::channel::<()>();
        let go_receiver = std::sync::Mutex::new(go_receiver);
        let panicked = AtomicBool::new(false);
        let mut worker = GitStatusWorker::with_refresh(
            move |outcome: RefreshOutcome<GatedOwner>| {
                if !panicked.swap(true, Ordering::SeqCst) {
                    let gate = go_receiver.lock().expect("go gate");
                    gate.recv_timeout(WAIT).ok();
                    drop(gate);
                    panic!("publish failed");
                }
                let ids: Vec<usize> = outcome.statuses.iter().map(|s| s.owner.id).collect();
                sender.send(ids).ok();
            },
            |targets, _| answer_all(targets),
        );
        let (dropping, dropping_receiver) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();

        // The panic in the first publish drops the queued second refresh,
        // whose owner then blocks in its drop while the thread lives.
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
        assert!(worker.abandon_stalled(past_the_stall_bound()));
        assert!(worker.thread.is_none());

        worker
            .refresh(vec![target(plain_owner(2), &cwd)])
            .expect("a new worker starts");
        assert_eq!(outcomes.recv_timeout(WAIT).expect("refresh outcome"), [2]);

        release.send(()).expect("release queued drop");
        wait_for_abandoned_threads(&mut worker);
        assert!(!worker.take_lost_refresh());
        assert!(outcomes.try_recv().is_err());
    }

    #[test]
    fn a_worker_that_owes_nothing_is_never_abandoned() {
        let scratch = shepr_test_support::ScratchDir::new("git-worker-idle");
        let (sender, outcomes) = mpsc::channel();
        let mut worker = GitStatusWorker::with_refresh(
            move |outcome: RefreshOutcome<usize>| {
                sender.send(outcome).ok();
            },
            |targets, _| answer_all(targets),
        );

        worker
            .refresh(vec![target(0, &scratch.join("cwd"))])
            .expect("worker starts");
        outcomes.recv_timeout(WAIT).expect("refresh outcome");

        assert!(!worker.abandon_stalled(past_the_stall_bound()));
        assert!(worker.thread.is_some());
        assert!(worker.abandoned.is_empty());
    }

    #[test]
    fn a_stalled_refresh_is_abandoned_its_path_skipped_and_its_late_outcome_dropped() {
        let scratch = shepr_test_support::ScratchDir::new("git-worker-stalled");
        let stuck = scratch.join("stuck");
        let live = scratch.join("live");
        let (sender, outcomes) = mpsc::channel();
        let (entered, entered_receiver) = mpsc::channel();
        let released = Arc::new(AtomicBool::new(false));
        let mut worker = GitStatusWorker::with_refresh(
            move |outcome: RefreshOutcome<usize>| {
                sender.send(outcome).ok();
            },
            blocking_refresh(stuck.clone(), entered, Arc::clone(&released)),
        );

        worker
            .refresh(vec![target(0, &stuck.join("sub")), target(1, &live)])
            .expect("worker starts");
        assert_eq!(
            entered_receiver.recv_timeout(WAIT).expect("stuck step"),
            stuck.join("sub")
        );

        // Within the bound the refresh is only slow.
        assert!(!worker.abandon_stalled(Instant::now()));
        assert!(worker.abandon_stalled(past_the_stall_bound()));
        assert!(worker.thread.is_none());
        assert_eq!(worker.abandoned.len(), 1);
        assert!(!worker.abandon_stalled(past_the_stall_bound()));

        // The replacement leaves out the stuck path, including a known
        // checkout under it, and answers the rest.
        worker
            .refresh(vec![
                target(2, &stuck.join("sub")),
                RefreshTarget {
                    owner: 3,
                    cwd: scratch.join("elsewhere"),
                    known_key: Some(GitStatusKey::Checkout(stuck.join("sub"))),
                },
                target(4, &live),
            ])
            .expect("a new worker starts");
        let outcome = outcomes.recv_timeout(WAIT).expect("refresh outcome");
        assert_eq!(owners(&outcome), [4]);

        // The stuck thread returns: its outcome is dropped, and it is not a
        // lost refresh either.
        released.store(true, Ordering::SeqCst);
        wait_for_abandoned_threads(&mut worker);
        assert!(!worker.take_lost_refresh());
        assert!(outcomes.try_recv().is_err());

        // Once it has finished, its path is refreshed again.
        worker
            .refresh(vec![target(5, &stuck.join("sub"))])
            .expect("refresh accepted");
        let outcome = outcomes.recv_timeout(WAIT).expect("refresh outcome");
        assert_eq!(owners(&outcome), [5]);
        assert!(outcomes.try_recv().is_err());
    }

    #[test]
    fn a_stall_with_no_step_running_is_abandoned_and_keeps_no_path_out() {
        let scratch = shepr_test_support::ScratchDir::new("git-worker-no-step");
        let cwd = scratch.join("cwd");
        let (sender, outcomes) = mpsc::channel();
        let (entered, entered_receiver) = mpsc::channel();
        let released = Arc::new(AtomicBool::new(false));
        let double_released = Arc::clone(&released);
        let first = AtomicBool::new(true);
        let mut worker = GitStatusWorker::with_refresh(
            move |outcome: RefreshOutcome<usize>| {
                sender.send(outcome).ok();
            },
            move |targets, _| {
                // The first refresh blocks without starting a step.
                if first.swap(false, Ordering::SeqCst) {
                    entered.send(()).ok();
                    let deadline = Instant::now() + WAIT;
                    while !double_released.load(Ordering::SeqCst) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                answer_all(targets)
            },
        );

        worker
            .refresh(vec![target(0, &cwd)])
            .expect("worker starts");
        entered_receiver.recv_timeout(WAIT).expect("blocked");
        assert!(worker.abandon_stalled(past_the_stall_bound()));
        assert_eq!(worker.abandoned.len(), 1);
        assert!(worker.abandoned[0].stuck.is_empty());

        // Nothing is kept out: the same cwd is refreshed by the replacement.
        worker
            .refresh(vec![target(1, &cwd)])
            .expect("a new worker starts");
        let outcome = outcomes.recv_timeout(WAIT).expect("refresh outcome");
        assert_eq!(owners(&outcome), [1]);

        released.store(true, Ordering::SeqCst);
        wait_for_abandoned_threads(&mut worker);
        assert!(!worker.take_lost_refresh());
        assert!(outcomes.try_recv().is_err());
    }

    #[test]
    fn abandoned_threads_are_capped() {
        let scratch = shepr_test_support::ScratchDir::new("git-worker-abandon-cap");
        let stuck = scratch.join("stuck");
        let (sender, outcomes) = mpsc::channel();
        let (entered, entered_receiver) = mpsc::channel();
        let released = Arc::new(AtomicBool::new(false));
        let mut worker = GitStatusWorker::with_refresh(
            move |outcome: RefreshOutcome<usize>| {
                sender.send(outcome).ok();
            },
            blocking_refresh(stuck.clone(), entered, Arc::clone(&released)),
        );

        for owner in 0..MAX_ABANDONED_GIT_REFRESH_THREADS {
            worker
                .refresh(vec![target(owner, &stuck.join(owner.to_string()))])
                .expect("worker starts");
            entered_receiver.recv_timeout(WAIT).expect("stuck step");
            assert!(worker.abandon_stalled(past_the_stall_bound()));
        }
        assert_eq!(worker.abandoned.len(), MAX_ABANDONED_GIT_REFRESH_THREADS);

        // With every slot taken, a further stall is waited out on its thread.
        let last = MAX_ABANDONED_GIT_REFRESH_THREADS;
        worker
            .refresh(vec![target(last, &stuck.join(last.to_string()))])
            .expect("worker starts");
        entered_receiver.recv_timeout(WAIT).expect("stuck step");
        assert!(!worker.abandon_stalled(past_the_stall_bound()));
        assert!(worker.thread.is_some());
        assert_eq!(worker.abandoned.len(), MAX_ABANDONED_GIT_REFRESH_THREADS);

        // Only the thread that was waited out publishes.
        released.store(true, Ordering::SeqCst);
        let outcome = outcomes.recv_timeout(WAIT).expect("refresh outcome");
        assert_eq!(owners(&outcome), [last]);
        wait_for_abandoned_threads(&mut worker);
        assert!(!worker.take_lost_refresh());
        assert!(outcomes.try_recv().is_err());
    }
}
