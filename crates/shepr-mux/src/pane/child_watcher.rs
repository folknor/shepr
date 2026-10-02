use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use shepr_pty::backend::PaneChild;

use super::exit_arbiter::{PaneEnding, PaneExitArbiter};
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
                PaneEnding::Observed {
                    reason: shepr_platform::classify_child_exit(&status),
                    child_exit_confirmed: true,
                    ended_at,
                },
                Ok(status),
            ),
            Err(error) => (
                PaneEnding::Observed {
                    reason: shepr_platform::ChildExitReason::WaitFailed,
                    child_exit_confirmed: false,
                    ended_at,
                },
                Err(error),
            ),
        };
        arbiter.decide(ending);
        match logged {
            Ok(status) => crate::logging::pane_exited(pane_id.raw(), &status),
            Err(error) => crate::logging::pane_exit_failed(pane_id.raw(), &error.to_string()),
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
    reap_on_detached_thread(child, move |result| {
        match result {
            Ok(status) => crate::logging::pane_exited(pane_id.raw(), &status),
            Err(err) => crate::logging::pane_exit_failed(pane_id.raw(), &err.to_string()),
        }
        if let Some(child_liveness) = child_liveness {
            child_liveness.mark_wait_completed();
        }
    });
}

type ReaperCompletion = Box<dyn FnOnce(std::io::Result<std::process::ExitStatus>) + Send + 'static>;

/// Wait on a child away from the task or synchronous caller that gives it up.
/// If the system cannot create the reaper thread, the child is left for the
/// process to collect at exit rather than waited on inline: the caller may be
/// the event loop, and the child may be stuck in a hung chdir.
fn reap_on_detached_thread(
    child: PaneChild,
    on_wait: impl FnOnce(std::io::Result<std::process::ExitStatus>) + Send + 'static,
) {
    let pid = child.id();
    let on_wait: ReaperCompletion = Box::new(on_wait);
    let spawned = std::thread::Builder::new()
        .name("shepr-pane-reaper".into())
        .spawn(move || {
            let mut child = child;
            on_wait(child.wait());
        });
    if let Err(err) = spawned {
        tracing::warn!(
            pid,
            error = %err,
            "could not start a reaper for a pane child; it stays a zombie until the server exits"
        );
    }
}

/// Owns the pane child while its watcher awaits the pidfd. If the watcher is
/// dropped before it reaps (the runtime shutting down while the child still
/// runs), the child is handed to a detached thread that waits for it, so it
/// never stays a zombie for the rest of the process.
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
                    pid = child.id(),
                    error = %err,
                    "could not check an abandoned pane child before reaping"
                );
            }
        }
        // The pane is gone, so its exit status has no reader; only a failed
        // reap (a possible zombie) is worth a line.
        let pid = child.id();
        reap_on_detached_thread(child, move |result| {
            if let Err(err) = result {
                tracing::warn!(
                    pid,
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
            // waitid(P_PIDFD, WEXITED) reaped the child; record it so the
            // handle never waits on a pid that may be reused.
            if let Some(mut child) = child.take() {
                child.mark_reaped(status);
            }
            Ok(status)
        }
        Err(err) => {
            // Kernels may expose pidfd_open before waitid(P_PIDFD); the child
            // is ready by now, so a blocking wait is only a short fallback.
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
