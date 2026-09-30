use super::EndpointReplyTicket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

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
            let result = match std::fs::metadata(&cwd) {
                Ok(metadata) => Ok(metadata.is_dir()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.to_string()),
            };
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
