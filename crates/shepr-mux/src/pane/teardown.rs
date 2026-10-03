use crate::limits::{PANE_TEARDOWN_BUDGET, PANE_TEARDOWN_STEPS};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::Duration;
use tracing::{info, warn};

use shepr_core::layout::PaneId;

/// The pane's child identity and the observations used to decide whether it
/// has exited or has been reaped. Keeping the process handle with the pid and
/// wait result prevents each lifecycle path from choosing its own authority.
pub(super) struct ChildLiveness {
    pid: AtomicU32,
    wait_completed: AtomicBool,
    /// Whether the child's exec committed. Until then the process is the
    /// server's own image (in chdir, or exec'ing) and nothing about it, cwd or
    /// foreground job, describes the pane's shell.
    launched: AtomicBool,
    /// The child's shared pidfd identity, acquired by PTY before returning.
    /// Teardown signals through it so a reused pid is never hit.
    leader: Option<Arc<shepr_platform::ProcessHandle>>,
}

impl ChildLiveness {
    /// A child known to run its program already.
    fn from_parts(
        pid: Option<shepr_platform::Pid>,
        leader: Option<Arc<shepr_platform::ProcessHandle>>,
    ) -> Self {
        Self {
            // Zero encodes None in the atomic storage, never a process id.
            pid: AtomicU32::new(pid.map_or(0, shepr_platform::Pid::get)),
            wait_completed: AtomicBool::new(false),
            launched: AtomicBool::new(true),
            leader,
        }
    }

    pub(super) fn absent() -> Self {
        Self::from_parts(None, None)
    }

    /// A child just forked, not yet past its exec.
    pub(super) fn launching(
        pid: shepr_platform::Pid,
        leader: Arc<shepr_platform::ProcessHandle>,
    ) -> Self {
        Self {
            launched: AtomicBool::new(false),
            ..Self::from_parts(Some(pid), Some(leader))
        }
    }

    pub(super) fn mark_launched(&self) {
        self.launched.store(true, Ordering::Release);
    }

    pub(super) fn is_launched(&self) -> bool {
        self.launched.load(Ordering::Acquire)
    }

    /// The pid the pane owns, launched or not: teardown signals it.
    pub(super) fn process_id(&self) -> Option<shepr_platform::Pid> {
        self.leader
            .as_ref()
            .map(|leader| leader.process_id())
            .or_else(|| shepr_platform::Pid::new(self.pid.load(Ordering::Acquire)))
    }

    /// The child pid while it names this unreaped child running the pane's
    /// program. Before exec it is still the server image, and after reaping
    /// its pid may name another process. All observations use this gate.
    pub(super) fn live_process_id(&self) -> Option<shepr_platform::Pid> {
        (self.is_launched() && !self.wait_completed() && !self.is_reaped())
            .then(|| self.process_id())
            .flatten()
    }

    pub(super) fn live_pid(&self) -> Option<u32> {
        self.live_process_id().map(shepr_platform::Pid::get)
    }

    pub(super) fn mark_wait_completed(&self) {
        self.wait_completed.store(true, Ordering::Release);
    }

    pub(super) fn wait_completed(&self) -> bool {
        self.wait_completed.load(Ordering::Acquire)
    }

    /// Whether the child has exited; a zombie counts as exited.
    pub(super) fn has_exited(&self) -> bool {
        self.leader
            .as_ref()
            .map_or_else(|| self.wait_completed(), |leader| leader.has_exited())
    }

    /// Whether the child has been reaped and its pid can be reused.
    pub(super) fn is_reaped(&self) -> bool {
        self.leader
            .as_ref()
            .map_or_else(|| self.wait_completed(), |leader| !leader.is_unreaped())
    }

    pub(super) fn leader(&self) -> Option<&shepr_platform::ProcessHandle> {
        self.leader.as_deref()
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
        *shepr_vt::lock_auxiliary(&self.in_flight) += 1;
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
        let guard = shepr_vt::lock_auxiliary(&self.in_flight);
        match self
            .done
            .wait_timeout_while(guard, timeout, |in_flight| *in_flight > 0)
        {
            Ok((guard, _)) => *guard == 0,
            Err(poisoned) => *shepr_vt::recover_auxiliary_poison(poisoned).0 == 0,
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
        let mut in_flight = shepr_vt::lock_auxiliary(&self.tracker.in_flight);
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
        .name(format!("shepr-pane-{}-teardown", pane_id.raw()))
        .spawn(move || run_pane_teardown(pane_id, &thread_work));
    if let Err(err) = spawned {
        warn!(
            pane = pane_id.raw(),
            error = %err,
            "could not start pane teardown thread; tearing down inline"
        );
        run_pane_teardown(pane_id, &work);
    }
}

type PaneTeardownWork = Mutex<Option<(PaneTeardownInFlight, Arc<ChildLiveness>)>>;

fn run_pane_teardown(pane_id: PaneId, work: &PaneTeardownWork) {
    let taken = shepr_vt::lock_auxiliary(work).take();
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
    for (signal, grace) in PANE_TEARDOWN_STEPS {
        // Rescan every round: a process that forked while being hung up is
        // still in the session and must not escape the next signal.
        members = shepr_platform::session_members(session_id, leader_reaped);
        let handles: Vec<&shepr_platform::ProcessHandle> = child_liveness
            .leader()
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
                    pane = pane_id.raw(),
                    session = session_id.get(),
                    ?signal,
                    "pane session terminated"
                );
                return;
            }
        }
    }

    let survivors: Vec<u32> = child_liveness
        .leader()
        .into_iter()
        .chain(members.iter())
        .filter(|handle| !handle.has_exited())
        .map(shepr_platform::ProcessHandle::pid)
        .collect();
    warn!(
        pane = pane_id.raw(),
        session = session_id.get(),
        ?survivors,
        "pane session still alive after forced shutdown"
    );
}

#[cfg(test)]
impl ChildLiveness {
    pub(super) fn new(pid: u32, leader: Option<Arc<shepr_platform::ProcessHandle>>) -> Self {
        Self::from_parts(shepr_platform::Pid::new(pid), leader)
    }

    pub(super) fn set_pid_for_test(&self, pid: u32) {
        self.pid.store(pid, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
