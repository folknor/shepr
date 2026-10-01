use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use super::teardown::ChildLiveness;
use crate::events::{AppEvent, EventSender};
use shepr_core::layout::PaneId;
use tracing::error;

/// Watches an owned child independently of detection and PTY parsing. The
/// watcher must keep reaping after the pane runtime has been dropped.
pub(super) fn spawn(
    pane_id: PaneId,
    child: std::process::Child,
    child_liveness: Arc<ChildLiveness>,
    events: EventSender,
) {
    let pidfd = child_liveness
        .leader()
        .and_then(|leader| match leader.try_clone_pidfd() {
            Ok(pidfd) => Some(pidfd),
            Err(err) => {
                tracing::debug!(
                    pane = pane_id.raw(),
                    pid = child.id(),
                    error = %err,
                    "could not duplicate child pidfd; falling back to child wait"
                );
                None
            }
        });
    // Wrapped before the task exists, so a watcher dropped before its first
    // poll still hands the child to a reaper thread.
    let child = UnreapedChild(Some(child));
    // Await the owned pidfd so each live pane uses no blocking-pool
    // thread; waitid reaps it while Child::wait remains the fallback.
    tokio::spawn(async move {
        let exit_reason = match wait_for_child_exit(child, pidfd).await {
            Ok(status) => {
                let exit_reason = shepr_platform::classify_child_exit(&status);
                crate::logging::pane_exited(pane_id.raw(), &status);
                exit_reason
            }
            Err(e) => {
                crate::logging::pane_exit_failed(pane_id.raw(), &e.to_string());
                shepr_platform::ChildExitReason::WaitFailed
            }
        };
        child_liveness.mark_wait_completed();
        // Wait for channel capacity so this critical pane exit is not dropped.
        if let Err(e) = events
            .send(AppEvent::PaneDied {
                pane_id,
                exit_reason,
            })
            .await
        {
            error!(pane = pane_id.raw(), error = %e, "failed to send PaneDied event");
        }
    });
}

/// Owns the pane child while its watcher awaits the pidfd. If the watcher is
/// dropped before it reaps (the runtime shutting down while the child still
/// runs), the child is handed to a detached thread that waits for it, so it
/// never stays a zombie for the rest of the process.
struct UnreapedChild(Option<std::process::Child>);

impl UnreapedChild {
    fn take(&mut self) -> Option<std::process::Child> {
        self.0.take()
    }
}

impl Drop for UnreapedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if let Ok(None) = child.try_wait() {
            let spawned = std::thread::Builder::new()
                .name("shepr-pane-reaper".into())
                .spawn(move || {
                    // The pane is gone, so its exit status has no reader; only
                    // a failed reap (a possible zombie) is worth a line.
                    if let Err(err) = child.wait() {
                        tracing::warn!(
                            pid = child.id(),
                            error = %err,
                            "could not reap an abandoned pane child"
                        );
                    }
                });
            if let Err(err) = spawned {
                tracing::warn!(error = %err, "could not start a reaper for an abandoned pane child");
            }
        }
    }
}

async fn wait_for_child_exit(
    mut child: UnreapedChild,
    pidfd: Option<OwnedFd>,
) -> std::io::Result<std::process::ExitStatus> {
    let Some(pidfd) = pidfd else {
        return wait_for_child_exit_blocking(child).await;
    };
    let async_pidfd = match tokio::io::unix::AsyncFd::new(pidfd) {
        Ok(async_pidfd) => async_pidfd,
        Err(err) => {
            tracing::debug!(error = %err, "could not register child pidfd; falling back to child wait");
            return wait_for_child_exit_blocking(child).await;
        }
    };
    if let Err(err) = async_pidfd.readable().await {
        tracing::debug!(error = %err, "child pidfd readiness failed; falling back to child wait");
        return wait_for_child_exit_blocking(child).await;
    }

    match shepr_platform::reap_pidfd(async_pidfd.get_ref().as_fd()) {
        Ok(status) => {
            // waitid(P_PIDFD, WEXITED) reaps the child, so dropping its
            // std::process::Child wrapper cannot leave a zombie behind.
            drop(child.take());
            Ok(status)
        }
        Err(err) => {
            // Kernels may expose pidfd_open before waitid(P_PIDFD); the child
            // is ready by now, so Child::wait is only a short fallback reap.
            tracing::debug!(error = %err, "waitid on child pidfd failed; falling back to child wait");
            wait_for_child_exit_blocking(child).await
        }
    }
}

async fn wait_for_child_exit_blocking(
    mut child: UnreapedChild,
) -> std::io::Result<std::process::ExitStatus> {
    let Some(mut child) = child.take() else {
        return Err(std::io::Error::other("pane child was already reaped"));
    };
    // A blocking task keeps running once started even if this await is
    // dropped, so the fallback reaps on runtime shutdown too.
    tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(std::io::Error::other)?
}
