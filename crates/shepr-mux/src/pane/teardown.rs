use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

use shepr_core::layout::PaneId;

use crate::limits::{PANE_TEARDOWN_BUDGET, PANE_TEARDOWN_STEPS};

/// The stable child identity and its coordinated lifecycle. A pending runtime
/// has no child; an owned child always has its pidfd-backed identity.
pub(super) struct ChildLiveness {
    state: Mutex<ChildState>,
}

struct ChildState {
    identity: ChildIdentity,
    phase: ChildPhase,
}

enum ChildIdentity {
    Absent,
    Process(Arc<shepr_platform::ProcessHandle>),
}

#[derive(Clone, Copy)]
enum LaunchPhase {
    Pending,
    Committed,
    Unconfirmed,
}

#[derive(Clone, Copy)]
enum ChildPhase {
    Launching,
    Running,
    Unconfirmed,
    /// Waiting ended, successfully or otherwise. The process handle remains
    /// the authority for exit and reaping; a failed wait proves neither.
    WaitEnded(LaunchPhase),
}

impl ChildPhase {
    fn launch(self) -> LaunchPhase {
        match self {
            Self::Launching => LaunchPhase::Pending,
            Self::Running => LaunchPhase::Committed,
            Self::Unconfirmed => LaunchPhase::Unconfirmed,
            Self::WaitEnded(launch) => launch,
        }
    }
}

impl ChildLiveness {
    /// The public ChildIo constructor admits externally hosted IO without a
    /// process owned by this runtime. Absence must carry no signalling or
    /// observation authority; substituting a numeric pid here could tear down
    /// an unrelated process when the runtime closes.
    ///
    /// A pane whose program is reached through a seam instead of a child
    /// process (`PaneRuntime::with_child_io`): it counts as launched, so its
    /// own screen is the pane's content, and it has no process to observe or
    /// signal.
    pub(super) fn launched_without_child() -> Self {
        Self {
            state: Mutex::new(ChildState {
                identity: ChildIdentity::Absent,
                phase: ChildPhase::Running,
            }),
        }
    }

    /// A child just forked, not yet past its exec. The handle supplies its
    /// identity; the caller cannot pair it with a different pid.
    pub(super) fn launching(leader: Arc<shepr_platform::ProcessHandle>) -> Self {
        Self {
            state: Mutex::new(ChildState {
                identity: ChildIdentity::Process(leader),
                phase: ChildPhase::Launching,
            }),
        }
    }

    pub(super) fn settle_launch(&self, committed: bool) {
        let mut state = shepr_core::locks::lock_auxiliary(&self.state);
        let launch = if committed {
            LaunchPhase::Committed
        } else {
            LaunchPhase::Unconfirmed
        };
        state.phase = match state.phase {
            ChildPhase::WaitEnded(_) => ChildPhase::WaitEnded(launch),
            _ if committed => ChildPhase::Running,
            _ => ChildPhase::Unconfirmed,
        };
    }

    pub(super) fn launch_committed(&self) -> Option<bool> {
        match shepr_core::locks::lock_auxiliary(&self.state)
            .phase
            .launch()
        {
            LaunchPhase::Pending => None,
            LaunchPhase::Committed => Some(true),
            LaunchPhase::Unconfirmed => Some(false),
        }
    }

    pub(super) fn is_launched(&self) -> bool {
        self.launch_committed() == Some(true)
    }

    /// The pid the pane owns, launched or not: teardown signals it.
    pub(super) fn process_id(&self) -> Option<shepr_platform::Pid> {
        match &shepr_core::locks::lock_auxiliary(&self.state).identity {
            ChildIdentity::Process(leader) => Some(leader.process_id()),
            ChildIdentity::Absent => None,
        }
    }

    /// The child pid while it names this unreaped child running the pane's
    /// program. Before exec it is still the server image, and after reaping
    /// its pid may name another process. Observation is allowed only after
    /// commitment and before waiting ends or the identity has been reaped.
    /// One lock hold: detection and cwd reads call this per probe.
    pub(super) fn live_process_id(&self) -> Option<shepr_platform::Pid> {
        let state = shepr_core::locks::lock_auxiliary(&self.state);
        if !matches!(state.phase, ChildPhase::Running) {
            return None;
        }
        match &state.identity {
            ChildIdentity::Process(leader) => leader.is_unreaped().then(|| leader.process_id()),
            ChildIdentity::Absent => None,
        }
    }

    /// Accept an observation only while the same child remains observable.
    /// Never hold the lifecycle lock across /proc I/O or terminal work.
    /// This brackets an observation; it does not lease a pid against reaping.
    pub(super) fn observe<T>(&self, read: impl FnOnce(shepr_platform::Pid) -> T) -> Option<T> {
        let pid = self.live_process_id()?;
        let observed = read(pid);
        self.is_live_process(pid).then_some(observed)
    }

    /// A checkpoint for work already bracketed by a sampled child identity.
    pub(super) fn is_live_process(&self, pid: shepr_platform::Pid) -> bool {
        self.live_process_id() == Some(pid)
    }

    pub(super) fn mark_wait_completed(&self) {
        let mut state = shepr_core::locks::lock_auxiliary(&self.state);
        state.phase = ChildPhase::WaitEnded(state.phase.launch());
    }

    pub(super) fn wait_completed(&self) -> bool {
        matches!(
            shepr_core::locks::lock_auxiliary(&self.state).phase,
            ChildPhase::WaitEnded(_)
        )
    }

    /// Whether the child exited, including zombies. Wait failure alone must
    /// never make an owned child read as exited.
    pub(super) fn has_exited(&self) -> bool {
        self.leader()
            .as_ref()
            .is_some_and(|leader| leader.has_exited())
    }

    pub(super) fn is_reaped(&self) -> bool {
        self.leader()
            .as_ref()
            .is_some_and(|leader| !leader.is_unreaped())
    }

    pub(super) fn leader(&self) -> Option<Arc<shepr_platform::ProcessHandle>> {
        match &shepr_core::locks::lock_auxiliary(&self.state).identity {
            ChildIdentity::Process(leader) => Some(Arc::clone(leader)),
            _ => None,
        }
    }
}

/// Pane session teardowns still running on their background threads, counted
/// per owner: the application that spawns panes creates one, hands it to every
/// pane it spawns (through `PaneSpawnHandles`), and waits on it at exit, so two
/// servers in one process never wait on each other's teardowns.
#[derive(Default)]
pub struct PaneTeardownTracker {
    in_flight: Mutex<usize>,
    done: std::sync::Condvar,
}

impl PaneTeardownTracker {
    /// Three signal grace periods; session scans add work outside this budget.
    pub const BUDGET: Duration = PANE_TEARDOWN_BUDGET;

    fn start(self: &Arc<Self>) -> PaneTeardownInFlight {
        *shepr_core::locks::lock_auxiliary(&self.in_flight) += 1;
        PaneTeardownInFlight {
            tracker: Arc::clone(self),
        }
    }

    /// Block until every teardown started through this tracker so far has
    /// finished, or `timeout` passes. Returns whether they all finished. For
    /// exit paths only: teardown runs off the caller's thread, and a process
    /// that exits right after dropping its panes would otherwise cut the
    /// SIGTERM/SIGKILL escalation short.
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = shepr_core::locks::lock_auxiliary(&self.in_flight);
        match self
            .done
            .wait_timeout_while(guard, timeout, |in_flight| *in_flight > 0)
        {
            Ok((guard, _)) => *guard == 0,
            Err(poisoned) => *shepr_core::locks::recover_auxiliary_poison(poisoned).0 == 0,
        }
    }
}

/// The queued work owns this guard through completion or unwind, so a
/// panicking teardown does not leak its in-flight count.
struct PaneTeardownInFlight {
    tracker: Arc<PaneTeardownTracker>,
}

impl Drop for PaneTeardownInFlight {
    fn drop(&mut self) {
        let mut in_flight = shepr_core::locks::lock_auxiliary(&self.tracker.in_flight);
        if *in_flight == 0 {
            warn!("pane teardown completion had no matching start");
            return;
        }
        *in_flight -= 1;
        if *in_flight == 0 {
            self.tracker.done.notify_all();
        }
    }
}

/// Tear down the pane's session: its leader (the shell or command the pane
/// started, spawned with `setsid`, so the session id is its pid) and every
/// process still in that session, such as background jobs and servers an
/// agent started. Those usually outlive the leader: by the time a pane is
/// closed because its child exited, the leader is long reaped.
///
/// Returns at once. The leader, if still running, is sent SIGHUP here through
/// its process handle; finding the other members (a `/proc` scan) and the
/// SIGHUP/SIGTERM/SIGKILL escalation with its grace periods run on a
/// background thread, so closing a workspace never stalls the caller for
/// them. Every signal goes through a pidfd-backed `platform::ProcessHandle`,
/// so a pid the kernel has handed to an unrelated process is not signalled.
pub(super) fn shutdown_pane_processes(
    pane_id: PaneId,
    child_liveness: Arc<ChildLiveness>,
    tracker: &Arc<PaneTeardownTracker>,
) {
    if child_liveness.process_id().is_none() {
        return;
    }
    if let Some(leader) = child_liveness.leader()
        && !child_liveness.has_exited()
    {
        leader.signal(shepr_platform::Signal::Hangup);
    }
    // `thread::Builder::spawn` drops its closure on failure, so the work is
    // parked in a shared slot that the inline fallback can still take back.
    let work = Arc::new(Mutex::new(Some((tracker.start(), child_liveness))));
    let thread_work = Arc::clone(&work);
    let spawned = std::thread::Builder::new()
        .name(format!("shepr-pane-{pane_id}-teardown"))
        .spawn(move || run_pane_teardown(pane_id, &thread_work));
    if let Err(err) = spawned {
        warn!(
            pane = %pane_id,
            error = %err,
            "could not start pane teardown thread; tearing down inline"
        );
        run_pane_teardown(pane_id, &work);
    }
}

type PaneTeardownWork = Mutex<Option<(PaneTeardownInFlight, Arc<ChildLiveness>)>>;

fn run_pane_teardown(pane_id: PaneId, work: &PaneTeardownWork) {
    let taken = shepr_core::locks::lock_auxiliary(work).take();
    if let Some((_in_flight, child_liveness)) = taken {
        terminate_pane_session(pane_id, &child_liveness);
    }
}

fn terminate_pane_session(pane_id: PaneId, child_liveness: &ChildLiveness) {
    let Some(leader_pid) = child_liveness.process_id() else {
        return;
    };
    let session_id = shepr_platform::SessionId::of_leader(leader_pid);
    let leader_reaped = || child_liveness.is_reaped();
    let mut members = Vec::new();
    let leader = child_liveness.leader();
    for (signal, grace) in PANE_TEARDOWN_STEPS {
        // Rescan every round: a process that forked while being hung up is
        // still in the session and must not escape the next signal.
        members = shepr_platform::session_members(session_id, leader_reaped);
        let handles: Vec<&shepr_platform::ProcessHandle> = leader
            .as_deref()
            .into_iter()
            .chain(members.iter())
            .collect();
        for handle in &handles {
            if !handle.has_exited() {
                handle.signal(signal);
            }
        }
        if shepr_platform::wait_for_process_exits(&handles, grace) {
            // A signalled member can fork before it exits. The newly forked
            // process was absent from `handles`, so confirm the whole session
            // is empty before ending the escalation.
            if shepr_platform::session_members(session_id, leader_reaped).is_empty() {
                info!(
                    pane = %pane_id,
                    session = session_id.get(),
                    ?signal,
                    "pane session terminated"
                );
                return;
            }
        }
    }

    let survivors: Vec<u32> = leader
        .as_deref()
        .into_iter()
        .chain(members.iter())
        .filter(|handle| !handle.has_exited())
        .map(|handle| handle.process_id().get())
        .collect();
    warn!(
        pane = %pane_id,
        session = session_id.get(),
        ?survivors,
        "pane session still alive after forced shutdown"
    );
}

#[cfg(test)]
impl ChildLiveness {
    /// A runtime that has not forked a child yet.
    pub(super) fn absent() -> Self {
        Self {
            state: Mutex::new(ChildState {
                identity: ChildIdentity::Absent,
                phase: ChildPhase::Launching,
            }),
        }
    }

    /// A launched child owned through its process handle.
    pub(super) fn running_with_handle(leader: Arc<shepr_platform::ProcessHandle>) -> Self {
        Self::running(ChildIdentity::Process(leader))
    }

    fn running(identity: ChildIdentity) -> Self {
        Self {
            state: Mutex::new(ChildState {
                identity,
                phase: ChildPhase::Running,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_is_discarded_when_wait_ends_during_read() {
        let pid = shepr_platform::Pid::new(std::process::id()).expect("test pid");
        let child = ChildLiveness::running_with_handle(Arc::new(
            shepr_platform::ProcessHandle::open(pid).expect("current process handle"),
        ));
        let observed = child.observe(|pid| {
            assert_eq!(pid.get(), std::process::id());
            child.mark_wait_completed();
            "stale"
        });
        assert_eq!(observed, None);
    }

    #[test]
    fn absent_child_never_runs_the_observation() {
        let child = ChildLiveness::absent();
        assert_eq!(
            child.observe(|_| panic!("must not read an absent child")),
            None::<()>
        );
    }

    #[test]
    fn settlement_after_wait_completion_never_reopens_observation() {
        let child = ChildLiveness::absent();
        child.mark_wait_completed();
        child.settle_launch(true);
        assert_eq!(child.launch_committed(), Some(true));
        assert!(child.wait_completed());
        assert!(child.live_process_id().is_none());
    }

    #[test]
    fn unsuccessful_settlement_is_shared_with_the_launch_watch() {
        let child = ChildLiveness::absent();
        assert_eq!(child.launch_committed(), None);
        child.settle_launch(false);
        assert_eq!(child.launch_committed(), Some(false));
        child.mark_wait_completed();
        assert_eq!(child.launch_committed(), Some(false));
    }

    #[test]
    fn teardown_trackers_wait_independently() {
        let first = Arc::new(PaneTeardownTracker::default());
        let second = Arc::new(PaneTeardownTracker::default());
        let first_ticket = first.start();

        assert!(!first.wait(Duration::ZERO));
        assert!(second.wait(Duration::ZERO));

        drop(first_ticket);
        assert!(first.wait(Duration::ZERO));
    }
}
