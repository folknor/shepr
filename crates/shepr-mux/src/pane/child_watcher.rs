use std::os::fd::OwnedFd;
use std::sync::Arc;

use shepr_pty::backend::PaneChild;

use super::exit_arbiter::{PaneEndReason, PaneEnding, PaneExitArbiter, RecordedEnding};
use super::teardown::ChildLiveness;
use shepr_core::layout::PaneId;

/// Watches an owned child independently of detection and PTY parsing. The
/// watcher must keep reaping after the pane runtime has been dropped. Once
/// the child is reaped it records the exit with `arbiter` at once, without
/// waiting on the launch or the app: the launch coordinator publishes it after
/// the launch's settlement.
pub(super) fn spawn(
    pane_id: PaneId,
    child: PaneChild,
    child_liveness: Arc<ChildLiveness>,
    arbiter: Arc<PaneExitArbiter>,
) {
    let pidfd = child_liveness
        .leader()
        .and_then(|leader| match leader.try_clone_pidfd() {
            Ok(pidfd) => Some(pidfd),
            Err(err) => {
                tracing::debug!(
                    pane = %pane_id,
                    pid = %child.process_id(),
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
    // thread; waitid reaps it while a blocking wait remains the fallback.
    tokio::spawn(async move {
        let result = wait_for_child_exit(child, pidfd).await;
        // clock-io-ok: the time the child's death was observed, which a
        // checkpoint compares with an agent's exit just before it.
        let ended_at = std::time::Instant::now();
        child_liveness.mark_wait_completed();
        // Recorded before logging, so nothing delays the decision. A failed
        // wait proves nothing about the child, which may still be alive.
        let (ending, logged) = match result {
            Ok(status) => (
                RecordedEnding::Observed {
                    ending: PaneEnding::new(shepr_platform::classify_child_exit(&status).into()),
                    child_exit_confirmed: true,
                    ended_at,
                },
                Ok(status),
            ),
            Err(error) => (
                RecordedEnding::Observed {
                    ending: PaneEnding::new(PaneEndReason::WaitFailed),
                    child_exit_confirmed: false,
                    ended_at,
                },
                Err(error),
            ),
        };
        arbiter.decide(ending);
        match logged {
            Ok(status) => super::logging::pane_exited(pane_id, &status),
            Err(error) => super::logging::pane_exit_failed(pane_id, &error.to_string()),
        }
    });
}

/// Reap a child whose pane runtime could not be assembled. There is no pane
/// runtime for the normal watcher to own, but the failed launch still needs
/// its child collected without waiting on the synchronous startup caller.
pub(super) fn reap_after_startup_failure(
    pane_id: PaneId,
    child: PaneChild,
    child_liveness: Option<Arc<ChildLiveness>>,
) {
    child.reap_detached(move |result| {
        match result {
            Ok(status) => super::logging::pane_exited(pane_id, &status),
            Err(err) => super::logging::pane_exit_failed(pane_id, &err.to_string()),
        }
        if let Some(child_liveness) = child_liveness {
            child_liveness.mark_wait_completed();
        }
    });
}

/// Owns the pane child while its watcher awaits the pidfd. If the watcher is
/// dropped before it reaps (the runtime shutting down while the child still
/// runs), the child is handed to a detached thread that waits for it, so it
/// can still be collected unless the system cannot create a reaper thread.
struct UnreapedChild(Option<PaneChild>);

impl UnreapedChild {
    fn take(&mut self) -> Option<PaneChild> {
        self.0.take()
    }
}

impl Drop for UnreapedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    pid = %child.process_id(),
                    error = %err,
                    "could not check an abandoned pane child before reaping"
                );
            }
        }
        // The pane is gone, so its exit status has no reader; only a failed
        // reap (a possible zombie) is worth a line.
        let pid = child.process_id();
        child.reap_detached(move |result| {
            if let Err(err) = result {
                tracing::error!(
                    %pid,
                    error = %err,
                    "could not reap an abandoned pane child"
                );
            }
        });
    }
}

async fn wait_for_child_exit(
    mut child: UnreapedChild,
    pidfd: Option<OwnedFd>,
) -> std::io::Result<std::process::ExitStatus> {
    let Some(pidfd) = pidfd else {
        return wait_for_child_exit_by_polling(child).await;
    };
    let async_pidfd = match tokio::io::unix::AsyncFd::new(pidfd) {
        Ok(async_pidfd) => async_pidfd,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "could not register child pidfd; falling back to child polling"
            );
            return wait_for_child_exit_by_polling(child).await;
        }
    };
    if let Err(err) = async_pidfd.readable().await {
        tracing::debug!(
            error = %err,
            "child pidfd readiness failed; falling back to child polling"
        );
        return wait_for_child_exit_by_polling(child).await;
    }

    let Some(mut owned_child) = child.take() else {
        return Err(std::io::Error::other("pane child was already reaped"));
    };
    match owned_child.wait_pidfd() {
        Ok(status) => Ok(status),
        Err(err) => {
            tracing::debug!(
                error = %err,
                "waitid on child pidfd failed; falling back to child polling"
            );
            wait_for_child_exit_by_polling(UnreapedChild(Some(owned_child))).await
        }
    }
}

async fn wait_for_child_exit_by_polling(
    mut child: UnreapedChild,
) -> std::io::Result<std::process::ExitStatus> {
    // A failed pidfd path must not park one shared blocking-pool worker for
    // each affected pane. `try_wait` polls waitpid without blocking, and the async
    // delay yields between probes while this guard retains reaping ownership.
    loop {
        let Some(owned_child) = child.0.as_mut() else {
            return Err(std::io::Error::other("pane child was already reaped"));
        };
        if let Some(status) = owned_child.try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(crate::limits::CHILD_WAIT_FALLBACK_POLL_INTERVAL).await;
    }
}
