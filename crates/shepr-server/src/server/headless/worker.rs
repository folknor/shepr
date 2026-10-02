use super::ReplyTicket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

pub(super) type CheckoutRootRunner =
    Arc<dyn Fn(PathBuf) -> Result<Option<String>, String> + Send + Sync>;

pub(super) enum WorkerCompletion {
    CheckoutRoot {
        ticket: ReplyTicket,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        home: Option<String>,
        result: Result<Option<String>, String>,
    },
}

pub(super) fn channel() -> (
    mpsc::UnboundedSender<WorkerCompletion>,
    mpsc::UnboundedReceiver<WorkerCompletion>,
) {
    mpsc::unbounded_channel()
}

/// Counts worker threads that can still send, plus completions waiting for the
/// event loop. The server owns one sender and each worker holds one clone until
/// after its send. The short overlap between those states is counted twice and
/// only makes admission conservative.
pub(super) fn completion_backlog(
    sender: &mpsc::UnboundedSender<WorkerCompletion>,
    receiver: &mpsc::UnboundedReceiver<WorkerCompletion>,
) -> usize {
    sender
        .strong_count()
        .saturating_sub(1)
        .saturating_add(receiver.len())
}

pub(super) fn default_checkout_root_runner() -> CheckoutRootRunner {
    Arc::new(|cwd| crate::app::App::checkout_root_for_worker(&cwd))
}

pub(super) fn checkout_root(
    sender: &mpsc::UnboundedSender<WorkerCompletion>,
    runner: CheckoutRootRunner,
    ticket: ReplyTicket,
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
