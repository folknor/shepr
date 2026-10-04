//! Settles a pane launch from its child's status channel, and publishes the
//! pane's ending after it.
//!
//! The pane child is forked without waiting for its chdir or exec (see
//! `shepr_pty::backend`), so a runtime exists from the start while its shell
//! may not. One coordinator task per launch reads the child's reports and
//! settles the launch exactly once:
//!
//! - an error record: the launch failed at that stage;
//! - `ChdirOk` then EOF while the child lives: exec committed (`Launched`);
//! - anything else (the child exited first, the channel broke, or the pane
//!   ended before the launch was confirmed): unconfirmed, handled as an
//!   ordinary pane death.
//!
//! The settled result is shared state (`ChildLiveness` opens observation of
//! the child, `LaunchProgress` wakes detection) and is published to the app
//! with an awaited send. The same task then publishes the pane's ending that
//! the exit arbiter recorded, so the app always learns how a launch ended
//! before it learns the pane died, whichever observer decided the ending.
//!
//! A recorded ending can arrive before the launch settles. A reaped child
//! leaves the settlement nothing to wait for (its end of the status channel is
//! closed), so it finishes on its own and keeps a failure report the child
//! sent. An ending recorded while the child may still be alive (a failed PTY
//! reader, a failed wait) gives the settlement `LAUNCH_SETTLE_AFTER_PANE_END`
//! to finish and then settles it as unconfirmed: a child stuck in its chdir on
//! a dead mount must not keep the pane from ending. A teardown ends the task
//! with nothing published. The task outlives pane removal; reaping never waits
//! on it, and shutdown does not wait for it.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use shepr_pty::launch::{LaunchStatusEvent, LaunchStatusReader, Registration};
use tokio::io::unix::AsyncFd;
use tokio::sync::{oneshot, watch};

use super::exit_arbiter::{PaneExitArbiter, RecordedEnding};
use super::teardown::ChildLiveness;
use crate::events::EventSender;
use crate::limits::{
    LAUNCH_EXIT_POLL_INTERVAL, LAUNCH_SETTLE_AFTER_PANE_END, LAUNCH_STATUS_AFTER_EXIT,
};
use crate::terminal::PaneStartFailure;
use shepr_core::layout::PaneId;

/// How a pane launch ended, as the app is told.
#[derive(Debug)]
pub enum LaunchOutcome {
    /// Exec committed in `cwd`, the candidate the child entered.
    Launched { cwd: crate::UsableCwd },
    /// The child reported why it could not start the shell.
    Failed(PaneStartFailure),
    /// Exec was not confirmed: the child is gone without a report, or the
    /// pane ended before the launch settled. The pane's death follows and is
    /// an ordinary one.
    Unconfirmed,
}

/// The runtime's launch kind and the result of that launch, published together.
#[derive(Debug)]
pub struct LaunchSettlement {
    pub kind: super::launch::LaunchKind,
    pub outcome: LaunchOutcome,
}

/// The watch is a wakeup only. ChildLiveness owns the commitment decision.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct LaunchProgress;

pub(super) struct LaunchWatch {
    wake: watch::Receiver<LaunchProgress>,
    child: Arc<ChildLiveness>,
}

impl LaunchWatch {
    pub(super) async fn launched(&mut self) -> bool {
        loop {
            if let Some(committed) = self.child.launch_committed() {
                return committed;
            }
            if self.wake.changed().await.is_err() {
                return false;
            }
        }
    }
}

/// Everything a launch's coordinator needs, taken from the spawn.
pub(super) struct LaunchStatus {
    pub(super) channel: oneshot::Receiver<OwnedFd>,
    pub(super) registration: Registration,
    pub(super) cwd_candidates: Vec<shepr_core::absolute_path::AbsolutePath>,
    pub(super) program: PathBuf,
}

/// Starts the coordinator. Returns the watch the detection task waits on.
pub(super) fn spawn(
    pane_id: PaneId,
    kind: super::launch::LaunchKind,
    status: LaunchStatus,
    child_liveness: Arc<ChildLiveness>,
    arbiter: Arc<PaneExitArbiter>,
    events: EventSender,
) -> LaunchWatch {
    let (progress, watch) = watch::channel(LaunchProgress);
    let watch_child = Arc::clone(&child_liveness);
    tokio::spawn(async move {
        let LaunchStatus {
            channel,
            registration,
            cwd_candidates,
            program,
        } = status;
        let settling = settle(channel, cwd_candidates, &program, &child_liveness);
        coordinate(
            Coordinator {
                pane_id,
                kind,
                child_liveness: &child_liveness,
                arbiter: &arbiter,
                events: &events,
                progress: &progress,
            },
            settling,
            registration,
        )
        .await;
    });
    LaunchWatch {
        wake: watch,
        child: watch_child,
    }
}

struct Coordinator<'a> {
    pane_id: PaneId,
    kind: super::launch::LaunchKind,
    child_liveness: &'a ChildLiveness,
    arbiter: &'a PaneExitArbiter,
    events: &'a EventSender,
    progress: &'a watch::Sender<LaunchProgress>,
}

/// Settles the launch through `settling`, publishes the settlement, then the
/// recorded ending. `claim` (the status channel registration) is released once
/// the launch settled.
///
/// This task is the pane's only publisher, so a panic in it would leave the
/// app never hearing that the pane ended. There is no recovery path for a
/// panic: nothing here indexes unchecked or unwraps, IO errors become
/// settlements, and failed sends return.
async fn coordinate<Claim>(
    coordinator: Coordinator<'_>,
    settling: impl std::future::Future<Output = LaunchOutcome>,
    claim: Claim,
) {
    let Coordinator {
        pane_id,
        kind,
        child_liveness,
        arbiter,
        events,
        progress,
    } = coordinator;
    // Pinned once and only ever polled, so whatever it already read (a
    // `ChdirOk`) is never lost to a restart.
    tokio::pin!(settling);
    // Biased so an ending already recorded is seen first: a teardown recorded
    // before this task ran must publish nothing even when the settlement is
    // ready too.
    let settlement = tokio::select! {
        biased;
        ending = arbiter.decided() => match ending {
            RecordedEnding::Silent => return,
            RecordedEnding::Observed { child_exit_confirmed: true, .. } => settling.await,
            RecordedEnding::Observed { child_exit_confirmed: false, .. } => {
                tokio::time::timeout(LAUNCH_SETTLE_AFTER_PANE_END, settling)
                    .await
                    .unwrap_or(LaunchOutcome::Unconfirmed)
            }
        },
        settlement = &mut settling => settlement,
    };
    drop(claim);
    // A teardown recorded while the settlement finished: the pane is gone,
    // so nothing about it is published.
    if arbiter.ending() == Some(RecordedEnding::Silent) {
        return;
    }
    let launched = matches!(settlement, LaunchOutcome::Launched { .. });
    child_liveness.settle_launch(launched);
    progress.send_replace(LaunchProgress);
    if let LaunchOutcome::Failed(failure) = &settlement {
        tracing::warn!(pane = %pane_id, %failure, "pane launch failed");
    }
    if let Err(error) = events
        .send(crate::events::RuntimeEvent::PaneLaunchSettled {
            settlement: LaunchSettlement {
                kind,
                outcome: settlement,
            },
        })
        .await
    {
        tracing::error!(pane = %pane_id, %error, "failed to send PaneLaunchSettled event");
        return;
    }
    let RecordedEnding::Observed {
        ending, ended_at, ..
    } = arbiter.decided().await
    else {
        return;
    };
    // Wait for channel capacity so this critical pane exit is not dropped.
    if let Err(error) = events
        .send(crate::events::RuntimeEvent::PaneDied { ending, ended_at })
        .await
    {
        tracing::error!(pane = %pane_id, %error, "failed to send PaneDied event");
    }
}

async fn settle(
    mut channel: oneshot::Receiver<OwnedFd>,
    cwd_candidates: Vec<shepr_core::absolute_path::AbsolutePath>,
    program: &Path,
    child_liveness: &ChildLiveness,
) -> LaunchOutcome {
    let exit = child_liveness
        .leader()
        .and_then(|leader| leader.try_clone_pidfd().ok())
        .and_then(|pidfd| AsyncFd::new(pidfd).ok());
    // Without a watchable pidfd, poll the liveness the watcher also records:
    // a child that dies before reporting must still settle its launch, or its
    // death would never be published.
    let child_exited = async {
        if let Some(exit) = &exit
            && exit.readable().await.is_ok()
        {
            return;
        }
        while !(child_liveness.has_exited() || child_liveness.wait_completed()) {
            tokio::time::sleep(LAUNCH_EXIT_POLL_INTERVAL).await;
        }
    };
    let channel = tokio::select! {
        channel = &mut channel => channel.ok(),
        () = child_exited => {
            // A child that connected before it exited is already queued at
            // the listener; give its routing a moment. A listener delayed
            // past this bound loses that child's failure report: the launch
            // settles unconfirmed and the pane dies as an ordinary death
            // instead of a placeholder that says why. Accepted, since the
            // bound is what keeps a child that never connected from holding
            // the settlement open.
            tokio::time::timeout(LAUNCH_STATUS_AFTER_EXIT, channel)
                .await
                .ok()
                .and_then(Result::ok)
        }
    };
    let Some(channel) = channel else {
        return LaunchOutcome::Unconfirmed;
    };
    let channel = match AsyncFd::new(channel) {
        Ok(channel) => channel,
        Err(error) => {
            tracing::warn!(%error, "could not watch a pane launch status channel");
            return LaunchOutcome::Unconfirmed;
        }
    };
    let mut reader = LaunchStatusReader::new(cwd_candidates);
    loop {
        let mut ready = match channel.readable().await {
            Ok(ready) => ready,
            Err(error) => {
                tracing::warn!(%error, "pane launch status channel failed");
                return LaunchOutcome::Unconfirmed;
            }
        };
        match reader.read(ready.get_inner()) {
            Ok(LaunchStatusEvent::WouldBlock) => ready.clear_ready(),
            Ok(LaunchStatusEvent::Entered(_)) => {}
            Ok(LaunchStatusEvent::DirectoryFailed { path, error }) => {
                return LaunchOutcome::Failed(directory_failure(path.into_path_buf(), error));
            }
            Ok(LaunchStatusEvent::ExecFailed(error)) => {
                return LaunchOutcome::Failed(PaneStartFailure::ShellStartFailed {
                    program: Some(program.to_path_buf()),
                    error,
                });
            }
            Ok(LaunchStatusEvent::CommitCandidate(path)) => {
                return if child_liveness.has_exited() {
                    LaunchOutcome::Unconfirmed
                } else {
                    LaunchOutcome::Launched {
                        cwd: crate::UsableCwd::entered(path),
                    }
                };
            }
            Ok(LaunchStatusEvent::Unconfirmed) => return LaunchOutcome::Unconfirmed,
            Err(error) => {
                tracing::warn!(%error, "pane launch status channel failed");
                return LaunchOutcome::Unconfirmed;
            }
        }
    }
}

fn directory_failure(path: PathBuf, error: std::io::Error) -> PaneStartFailure {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    ) {
        PaneStartFailure::DirectoryUnavailable { path, error }
    } else {
        PaneStartFailure::directory_unreadable(path, &error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AppEvent;
    use crate::pane::{PaneEndReason, PaneEnding};
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// What the app was told, in order: `Settled(launched)` or `Died(reason)`.
    #[derive(Debug, PartialEq, Eq)]
    enum Told {
        Settled(&'static str),
        Died(PaneEndReason),
    }

    fn told(rx: &mut mpsc::Receiver<AppEvent>) -> Vec<Told> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            let AppEvent::Runtime { event, .. } = event else {
                panic!("runtime events carry their producer");
            };
            out.push(match *event {
                crate::events::RuntimeEvent::PaneLaunchSettled { settlement } => {
                    Told::Settled(match settlement.outcome {
                        LaunchOutcome::Launched { .. } => "launched",
                        LaunchOutcome::Failed(_) => "failed",
                        LaunchOutcome::Unconfirmed => "unconfirmed",
                    })
                }
                crate::events::RuntimeEvent::PaneDied { ending, .. } => Told::Died(ending.reason()),
                _ => panic!("unexpected event"),
            });
        }
        out
    }

    async fn run(
        arbiter: &PaneExitArbiter,
        settling: impl std::future::Future<Output = LaunchOutcome>,
    ) -> Vec<Told> {
        let (tx, mut rx) = mpsc::channel(8);
        let events = EventSender::runtime(
            tx,
            shepr_test_fixtures::fixed_pane_id(1),
            crate::events::RuntimeGeneration::alloc(),
        );
        let (progress, _watch) = watch::channel(LaunchProgress);
        let child_liveness = ChildLiveness::absent();
        coordinate(
            Coordinator {
                pane_id: shepr_test_fixtures::fixed_pane_id(1),
                kind: super::super::launch::LaunchKind::Fresh,
                child_liveness: &child_liveness,
                arbiter,
                events: &events,
                progress: &progress,
            },
            settling,
            (),
        )
        .await;
        told(&mut rx)
    }

    #[tokio::test]
    async fn watch_reads_commitment_from_child_state() {
        let child = Arc::new(ChildLiveness::absent());
        let (wake, receiver) = watch::channel(LaunchProgress);
        let mut watch = LaunchWatch {
            wake: receiver,
            child: Arc::clone(&child),
        };
        child.settle_launch(true);
        wake.send_replace(LaunchProgress);
        assert!(watch.launched().await);
        child.mark_wait_completed();
        assert!(watch.launched().await);
        assert!(child.live_process_id().is_none());
    }

    #[tokio::test]
    async fn cancelled_coordinator_never_reports_commitment() {
        let child = Arc::new(ChildLiveness::absent());
        let (wake, receiver) = watch::channel(LaunchProgress);
        let mut watch = LaunchWatch {
            wake: receiver,
            child,
        };
        drop(wake);
        assert!(!watch.launched().await);
    }

    #[test]
    fn directory_failures_retain_errno_and_classification() {
        for (errno, unavailable) in [
            (libc::ENOENT, true),
            (libc::ENOTDIR, true),
            (libc::EACCES, false),
        ] {
            let failure = directory_failure(
                "/requested".into(),
                std::io::Error::from_raw_os_error(errno),
            );
            let error = match &failure {
                PaneStartFailure::DirectoryUnavailable { error, .. } if unavailable => error,
                PaneStartFailure::DirectoryUnreadable { error, .. } if !unavailable => error,
                _ => panic!("wrong directory failure classification"),
            };
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    fn failed() -> LaunchOutcome {
        LaunchOutcome::Failed(PaneStartFailure::ShellStartFailed {
            program: None,
            error: std::io::Error::from_raw_os_error(libc::ENOENT),
        })
    }

    #[tokio::test]
    async fn a_death_recorded_first_is_published_after_the_settlement() {
        let arbiter = PaneExitArbiter::default();
        arbiter.decide(RecordedEnding::Observed {
            ending: PaneEnding::new(PaneEndReason::Exited),
            child_exit_confirmed: true,
            ended_at: std::time::Instant::now(),
        });
        // A reaped child still lets the settlement finish, failure included.
        let settling = async {
            tokio::task::yield_now().await;
            failed()
        };
        assert_eq!(
            run(&arbiter, settling).await,
            [Told::Settled("failed"), Told::Died(PaneEndReason::Exited)]
        );
    }

    // Waits out the real `LAUNCH_SETTLE_AFTER_PANE_END`: this crate's tests do
    // not enable tokio's paused clock (`test-util`), and one second is cheap
    // enough not to add the feature for.
    #[tokio::test]
    async fn a_hung_launch_does_not_keep_a_failed_reader_from_ending_the_pane() {
        let arbiter = PaneExitArbiter::default();
        arbiter.decide(RecordedEnding::Observed {
            ending: PaneEnding::new(PaneEndReason::ReaderIoFailed),
            child_exit_confirmed: false,
            ended_at: std::time::Instant::now(),
        });
        // A status channel that never produces a record: a child stuck in its
        // chdir on a dead mount.
        let settling = std::future::pending();
        assert_eq!(
            run(&arbiter, settling).await,
            [
                Told::Settled("unconfirmed"),
                Told::Died(PaneEndReason::ReaderIoFailed)
            ]
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_ending_still_keeps_a_report_inside_the_window() {
        let arbiter = PaneExitArbiter::default();
        arbiter.decide(RecordedEnding::Observed {
            ending: PaneEnding::new(PaneEndReason::TerminalClosed),
            child_exit_confirmed: false,
            ended_at: std::time::Instant::now(),
        });
        let settling = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            failed()
        };
        assert_eq!(
            run(&arbiter, settling).await,
            [
                Told::Settled("failed"),
                Told::Died(PaneEndReason::TerminalClosed)
            ]
        );
    }

    #[tokio::test]
    async fn a_teardown_publishes_nothing() {
        let arbiter = PaneExitArbiter::default();
        arbiter.decide(RecordedEnding::Silent);
        assert_eq!(run(&arbiter, std::future::pending()).await, []);
        // Nor when the settlement is ready by the time the task first runs.
        assert_eq!(run(&arbiter, std::future::ready(failed())).await, []);
    }

    #[tokio::test]
    async fn a_teardown_while_the_launch_settles_publishes_nothing() {
        let arbiter = std::sync::Arc::new(PaneExitArbiter::default());
        let settling = {
            let arbiter = std::sync::Arc::clone(&arbiter);
            async move {
                // The pane is removed while its launch settles; the runtime's
                // drop records a silent ending.
                tokio::time::sleep(Duration::from_millis(1)).await;
                arbiter.decide(RecordedEnding::Silent);
                failed()
            }
        };
        assert_eq!(run(&arbiter, settling).await, []);
    }

    #[tokio::test]
    async fn a_teardown_after_the_settlement_was_published_publishes_no_death() {
        let arbiter = std::sync::Arc::new(PaneExitArbiter::default());
        let (tx, mut rx) = mpsc::channel(8);
        let events = EventSender::runtime(
            tx,
            shepr_test_fixtures::fixed_pane_id(1),
            crate::events::RuntimeGeneration::alloc(),
        );
        let (progress, _watch) = watch::channel(LaunchProgress);
        let child_liveness = ChildLiveness::absent();
        let coordinating = coordinate(
            Coordinator {
                pane_id: shepr_test_fixtures::fixed_pane_id(1),
                kind: super::super::launch::LaunchKind::Fresh,
                child_liveness: &child_liveness,
                arbiter: &arbiter,
                events: &events,
                progress: &progress,
            },
            std::future::ready(failed()),
            (),
        );
        tokio::pin!(coordinating);
        // Run until the settlement is out and the task waits for an ending.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut coordinating)
                .await
                .is_err()
        );
        // The app removes the pane once the launch failed.
        arbiter.decide(RecordedEnding::Silent);
        coordinating.await;
        assert_eq!(told(&mut rx), [Told::Settled("failed")]);
    }
}
