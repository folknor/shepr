//! Settles a pane launch from its child's status channel.
//!
//! The pane child is forked without waiting for its chdir or exec (see
//! `shepr_pty::backend`), so a runtime exists from the start while its shell
//! may not. One coordinator task per launch reads the child's reports and
//! settles the launch exactly once:
//!
//! - an error record: the launch failed at that stage;
//! - `ChdirOk` then EOF while the child lives: exec committed (`Launched`);
//! - anything else (the child exited first, or the channel broke): unconfirmed,
//!   handled as an ordinary pane death.
//!
//! The settled result is shared state (`ChildLiveness` opens observation of
//! the child, `LaunchProgress` wakes detection), published to the app with an
//! awaited send, and only then may the child watcher publish the pane's death,
//! so the app always learns how a launch ended before it learns the pane
//! died. The task outlives pane removal; reaping never waits on it.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

use shepr_pty::launch::{LaunchRecord, RecordRead, Registration};
use tokio::io::unix::AsyncFd;
use tokio::sync::{oneshot, watch};

use super::teardown::ChildLiveness;
use crate::events::{AppEvent, EventSender};
use crate::terminal::RestoreFailure;
use shepr_core::layout::PaneId;

/// How a pane launch ended, as the app is told.
#[derive(Debug)]
pub enum LaunchSettlement {
    /// Exec committed in `cwd`, the candidate the child entered.
    Launched { cwd: crate::UsableCwd },
    /// The child reported why it could not start the shell.
    Failed(RestoreFailure),
    /// The child is gone without a report; its death is an ordinary one.
    Unconfirmed,
}

/// What a launch has reached: settled (and how), then published to the app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LaunchProgress {
    /// `Some(true)` once exec committed, `Some(false)` for any other end.
    pub(super) launched: Option<bool>,
    pub(super) published: bool,
}

pub(super) struct LaunchWatch(watch::Receiver<LaunchProgress>);

impl LaunchWatch {
    /// Waits until the launch settled; whether exec committed.
    pub(super) async fn launched(&mut self) -> bool {
        self.0
            .wait_for(|progress| progress.launched.is_some())
            .await
            .is_ok_and(|progress| progress.launched == Some(true))
    }

    /// Waits until the app has been told how the launch ended. A coordinator
    /// that is gone has nothing left to publish.
    pub(super) async fn published(&mut self) {
        self.0.wait_for(|progress| progress.published).await.ok();
    }
}

impl Clone for LaunchWatch {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// Everything a launch's coordinator needs, taken from the spawn.
pub(super) struct LaunchStatus {
    pub(super) channel: oneshot::Receiver<OwnedFd>,
    pub(super) registration: Registration,
    pub(super) cwd_candidates: Vec<PathBuf>,
    pub(super) program: String,
}

/// Starts the coordinator. Returns the watch the detection task and the child
/// watcher wait on.
pub(super) fn spawn(
    pane_id: PaneId,
    status: LaunchStatus,
    child_liveness: Arc<ChildLiveness>,
    events: EventSender,
) -> LaunchWatch {
    let (progress, watch) = watch::channel(LaunchProgress::default());
    tokio::spawn(async move {
        let LaunchStatus {
            channel,
            registration,
            cwd_candidates,
            program,
        } = status;
        let settlement = settle(channel, &cwd_candidates, &program, &child_liveness).await;
        drop(registration);
        let launched = matches!(settlement, LaunchSettlement::Launched { .. });
        if launched {
            child_liveness.mark_launched();
        }
        progress.send_modify(|progress| progress.launched = Some(launched));
        if let LaunchSettlement::Failed(failure) = &settlement {
            tracing::warn!(pane = pane_id.raw(), %failure, "pane launch failed");
        }
        if let Err(error) = events
            .send(AppEvent::PaneLaunchSettled {
                pane_id,
                settlement,
            })
            .await
        {
            tracing::error!(pane = pane_id.raw(), %error, "failed to send PaneLaunchSettled event");
        }
        progress.send_modify(|progress| progress.published = true);
    });
    LaunchWatch(watch)
}

async fn settle(
    mut channel: oneshot::Receiver<OwnedFd>,
    cwd_candidates: &[PathBuf],
    program: &str,
    child_liveness: &ChildLiveness,
) -> LaunchSettlement {
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
            tokio::time::sleep(crate::limits::LAUNCH_EXIT_POLL_INTERVAL).await;
        }
    };
    let channel = tokio::select! {
        channel = &mut channel => channel.ok(),
        () = child_exited => {
            // A child that connected before it exited is already queued at
            // the listener; give its routing a moment.
            tokio::time::timeout(crate::limits::LAUNCH_STATUS_AFTER_EXIT, channel)
                .await
                .ok()
                .and_then(Result::ok)
        }
    };
    let Some(channel) = channel else {
        return LaunchSettlement::Unconfirmed;
    };
    let channel = match AsyncFd::new(channel) {
        Ok(channel) => channel,
        Err(error) => {
            tracing::warn!(%error, "could not watch a pane launch status channel");
            return LaunchSettlement::Unconfirmed;
        }
    };
    let mut selected: Option<usize> = None;
    loop {
        let mut ready = match channel.readable().await {
            Ok(ready) => ready,
            Err(error) => {
                tracing::warn!(%error, "pane launch status channel failed");
                return LaunchSettlement::Unconfirmed;
            }
        };
        let read = shepr_pty::launch::read_record(ready.get_inner());
        match read {
            Ok(RecordRead::WouldBlock) => ready.clear_ready(),
            Ok(RecordRead::Record(LaunchRecord::ChdirOk(index))) if selected.is_none() => {
                match usize::try_from(index)
                    .ok()
                    .filter(|index| *index < cwd_candidates.len())
                {
                    Some(index) => selected = Some(index),
                    None => {
                        tracing::warn!(index, "pane launch reported an unknown cwd candidate");
                        return LaunchSettlement::Unconfirmed;
                    }
                }
            }
            Ok(RecordRead::Record(LaunchRecord::ChdirFailed(errno))) if selected.is_none() => {
                let path = cwd_candidates.first().cloned().unwrap_or_default();
                return LaunchSettlement::Failed(directory_failure(path, errno));
            }
            Ok(RecordRead::Record(LaunchRecord::ExecFailed(errno))) if selected.is_some() => {
                let error = std::io::Error::from_raw_os_error(errno);
                return LaunchSettlement::Failed(RestoreFailure::ShellStartFailed {
                    error: format!("{program}: {error}"),
                });
            }
            Ok(RecordRead::Record(record)) => {
                tracing::warn!(?record, "pane launch reported out of order");
                return LaunchSettlement::Unconfirmed;
            }
            Ok(RecordRead::Eof) => {
                // The child's end is close-on-exec and nothing else closes it
                // before exec, so EOF while the child lives is exec committed.
                return match selected {
                    Some(index) if !child_liveness.has_exited() => LaunchSettlement::Launched {
                        cwd: crate::UsableCwd::entered(cwd_candidates[index].clone()),
                    },
                    _ => LaunchSettlement::Unconfirmed,
                };
            }
            Err(error) => {
                tracing::warn!(%error, "pane launch status channel failed");
                return LaunchSettlement::Unconfirmed;
            }
        }
    }
}

fn directory_failure(path: PathBuf, errno: i32) -> RestoreFailure {
    let error = std::io::Error::from_raw_os_error(errno);
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    ) {
        RestoreFailure::DirectoryUnavailable { path }
    } else {
        RestoreFailure::directory_unreadable(path, &error)
    }
}
