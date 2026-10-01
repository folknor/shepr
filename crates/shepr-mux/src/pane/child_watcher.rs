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

/// Reap a child whose PTY actor could not be started. There is no pane runtime
/// for the normal watcher to own, but the failed launch still needs its child
/// collected without waiting on the synchronous startup caller.
pub(super) fn reap_after_actor_startup_failure(
    pane_id: PaneId,
    child: std::process::Child,
    child_liveness: Arc<ChildLiveness>,
) {
    reap_on_detached_thread(child, move |result| {
        match result {
            Ok(status) => crate::logging::pane_exited(pane_id.raw(), &status),
            Err(err) => crate::logging::pane_exit_failed(pane_id.raw(), &err.to_string()),
        }
        child_liveness.mark_wait_completed();
    });
}

type ReaperCompletion = Box<dyn FnOnce(std::io::Result<std::process::ExitStatus>) + Send + 'static>;

/// Wait on a child away from the task or synchronous caller that gives it up.
/// If the system cannot create the reaper thread, finish the wait inline so
/// the child is still collected.
fn reap_on_detached_thread(
    child: std::process::Child,
    on_wait: impl FnOnce(std::io::Result<std::process::ExitStatus>) + Send + 'static,
) {
    let pid = child.id();
    let on_wait: ReaperCompletion = Box::new(on_wait);
    let work = Arc::new(std::sync::Mutex::new(Some((child, on_wait))));
    let thread_work = Arc::clone(&work);
    let spawned = std::thread::Builder::new()
        .name("shepr-pane-reaper".into())
        .spawn(move || {
            let Some((mut child, on_wait)) = shepr_vt::lock_auxiliary(&thread_work).take() else {
                return;
            };
            on_wait(child.wait());
        });
    if let Err(err) = spawned {
        tracing::warn!(
            pid,
            error = %err,
            "could not start a reaper for a pane child; waiting inline"
        );
        if let Some((mut child, on_wait)) = shepr_vt::lock_auxiliary(&work).take() {
            on_wait(child.wait());
        }
    }
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
