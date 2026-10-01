use super::EndpointReplyTicket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::limits::RESUME_CWD_CHECK_TIMEOUT;

pub(super) type CheckoutRootRunner =
    Arc<dyn Fn(PathBuf) -> Result<Option<String>, String> + Send + Sync>;

pub(super) enum WorkerCompletion {
    CheckoutRoot {
        ticket: EndpointReplyTicket,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        home: Option<String>,
        result: Result<Option<String>, String>,
    },
    ResumeCwdChecked {
        terminal_id: shepr_protocol::TerminalId,
        cwd: PathBuf,
        result: Result<bool, String>,
    },
}

pub(super) fn channel() -> (
    mpsc::UnboundedSender<WorkerCompletion>,
    mpsc::UnboundedReceiver<WorkerCompletion>,
) {
    mpsc::unbounded_channel()
}

pub(super) fn default_checkout_root_runner() -> CheckoutRootRunner {
    Arc::new(|cwd| crate::app::App::checkout_root_for_worker(&cwd))
}

pub(super) fn checkout_root(
    sender: &mpsc::UnboundedSender<WorkerCompletion>,
    runner: CheckoutRootRunner,
    ticket: EndpointReplyTicket,
    boot_id: shepr_protocol::BootId,
    request_id: shepr_protocol::RequestId,
    cwd: PathBuf,
    home: Option<String>,
) -> io::Result<()> {
    let completion_tx = sender.clone();
    thread::Builder::new()
        .name("shepr-checkout-root".to_owned())
        .spawn(move || {
            let result = runner(cwd);
            let completion = WorkerCompletion::CheckoutRoot {
                ticket,
                boot_id,
                request_id,
                home,
                result,
            };
            if completion_tx.send(completion).is_err() {
                tracing::debug!("server loop ended before the checkout root result");
            }
        })
        .map(|_| ())
}

pub(super) fn resume_cwd_check(
    sender: &mpsc::UnboundedSender<WorkerCompletion>,
    terminal_id: shepr_protocol::TerminalId,
    cwd: PathBuf,
) -> io::Result<()> {
    let completion_tx = sender.clone();
    thread::Builder::new()
        .name("shepr-resume-cwd".to_owned())
        .spawn(move || {
            let check_cwd = cwd.clone();
            let result = resume_cwd_check_with_deadline(RESUME_CWD_CHECK_TIMEOUT, move || {
                match std::fs::metadata(check_cwd) {
                    Ok(metadata) => Ok(metadata.is_dir()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error),
                }
            })
            .map_err(|error| error.to_string());
            let completion = WorkerCompletion::ResumeCwdChecked {
                terminal_id,
                cwd,
                result,
            };
            if completion_tx.send(completion).is_err() {
                tracing::debug!("server loop ended before the resume directory check result");
            }
        })
        .map(|_| ())
}

fn resume_cwd_check_with_deadline<F>(timeout: Duration, check: F) -> io::Result<bool>
where
    F: FnOnce() -> io::Result<bool> + Send + 'static,
{
    // A filesystem stat can block in the kernel and cannot be cancelled. Isolate
    // it so the resume gets a bounded result even if that worker stays blocked.
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    thread::Builder::new()
        .name("shepr-resume-cwd-stat".to_owned())
        .spawn(move || {
            // The receiver is gone only once its deadline passed; the late
            // result has no reader.
            result_tx.send(check()).ok();
        })?;

    match result_rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                "saved agent resume directory check timed out; treating the directory as unavailable"
            );
            Ok(false)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
            "saved agent resume directory check worker stopped without a result",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timed_out_resume_cwd_check_counts_as_unavailable() {
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let result = resume_cwd_check_with_deadline(Duration::from_millis(5), move || {
            release_rx.recv_timeout(Duration::from_secs(1)).ok();
            Ok(true)
        })
        .expect("the deadline is an unavailable result, not a worker error");
        release_tx.send(()).ok();

        assert!(!result);
    }

    #[test]
    fn a_resume_cwd_check_result_before_the_deadline_is_kept() {
        let result = resume_cwd_check_with_deadline(Duration::from_secs(1), || Ok(true))
            .expect("the check worker returns a result");

        assert!(result);
    }
}
