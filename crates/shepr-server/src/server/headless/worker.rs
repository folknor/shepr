use crate::server::outbox::ReplyTicket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use tokio::sync::mpsc;

/// Refuse new checkout-root work when this combined count of worker threads
/// and queued completions reaches the limit. Resume checks add at most one
/// completion per restored agent pane in a finite restore batch.
pub(super) const MAX_WORKER_COMPLETION_BACKLOG: usize = 8;

pub(super) type CheckoutRootRunner = Arc<
    dyn Fn(PathBuf) -> Result<Option<shepr_protocol::RemotePath>, shepr_git::GitReadError>
        + Send
        + Sync,
>;

pub(super) enum WorkerCompletion {
    CheckoutRoot {
        ticket: ReplyTicket,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        home: Option<shepr_protocol::RemotePath>,
        result: Result<Option<shepr_protocol::RemotePath>, shepr_git::GitReadError>,
    },
}

impl WorkerCompletion {
    pub(super) fn into_reply(self) -> (ReplyTicket, shepr_protocol::ServerMessage) {
        match self {
            Self::CheckoutRoot {
                ticket,
                boot_id,
                request_id,
                home,
                result,
            } => {
                let result = result
                    .map(
                        |root| shepr_protocol::command::EndpointReply::WorkspaceCheckoutRoot {
                            root,
                            home,
                        },
                    )
                    .map_err(|error| {
                        shepr_protocol::command::EndpointError::ResourceFailure(error.to_string())
                    });
                (
                    ticket,
                    crate::server::client_commands::response_message(boot_id, request_id, result),
                )
            }
        }
    }
}

/// Owns checkout worker admission and completion delivery. Reply tickets are
/// reserved by the coordinator before launch and remain ordered by its outbox.
pub(super) struct EndpointWorkers {
    sender: mpsc::UnboundedSender<WorkerCompletion>,
    receiver: mpsc::UnboundedReceiver<WorkerCompletion>,
    runner: CheckoutRootRunner,
    /// Worker threads that have started and not yet finished.
    running: Arc<AtomicUsize>,
}

/// One running worker thread's claim on the admission count, released when
/// the thread ends (or never starts).
struct RunningWorker(Arc<AtomicUsize>);

impl RunningWorker {
    fn start(running: &Arc<AtomicUsize>) -> Self {
        running.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(running))
    }
}

impl Drop for RunningWorker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl EndpointWorkers {
    pub(super) fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver,
            runner: default_checkout_root_runner(),
            running: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Counts running workers and queued completions. During a send the
    /// overlap counts twice, making admission conservative.
    pub(super) fn can_admit(&self) -> bool {
        self.running
            .load(Ordering::Acquire)
            .saturating_add(self.receiver.len())
            < MAX_WORKER_COMPLETION_BACKLOG
    }

    pub(super) async fn recv(&mut self) -> Option<WorkerCompletion> {
        self.receiver.recv().await
    }

    /// Starts a checkout root worker thread. [`Self::can_admit`] counts it as
    /// running until it ends, which is after its completion is sent.
    pub(super) fn checkout_root(
        &self,
        ticket: ReplyTicket,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        cwd: PathBuf,
        home: Option<shepr_protocol::RemotePath>,
    ) -> io::Result<()> {
        let completion_tx = self.sender.clone();
        let runner = Arc::clone(&self.runner);
        let running = RunningWorker::start(&self.running);
        thread::Builder::new()
            .name("shepr-checkout-root".to_owned())
            .spawn(move || {
                let _running = running;
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
}

impl super::HeadlessServer {
    /// Reserves and starts a checkout root request, producing every refusal
    /// associated with worker admission and launch from the server coordinator.
    pub(super) fn dispatch_checkout_root(
        &mut self,
        client_id: crate::server::ClientId,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        cwd: PathBuf,
        home: Option<shepr_protocol::RemotePath>,
    ) {
        if !self.workers.can_admit() {
            self.queue_endpoint_reply(
                client_id,
                &crate::server::client_commands::response_message(
                    boot_id,
                    request_id,
                    Err(shepr_protocol::command::EndpointError::Busy(
                        "checkout root worker limit reached; retry later".to_owned(),
                    )),
                ),
            );
            return;
        }

        let shutdown_message = crate::server::client_commands::error_message(
            boot_id.clone(),
            request_id.clone(),
            shepr_protocol::command::EndpointError::ShuttingDown,
        );
        let Some(ticket) = self.reserve_endpoint_reply(client_id, &shutdown_message) else {
            return;
        };
        if let Err(error) =
            self.workers
                .checkout_root(ticket, boot_id.clone(), request_id.clone(), cwd, home)
        {
            self.complete_endpoint_reply(
                ticket,
                &crate::server::client_commands::response_message(
                    boot_id,
                    request_id,
                    Err(shepr_protocol::command::EndpointError::ResourceFailure(
                        format!("failed to start checkout root worker: {error}"),
                    )),
                ),
            );
        }
    }
}

fn default_checkout_root_runner() -> CheckoutRootRunner {
    Arc::new(|cwd| crate::app::App::checkout_root_for_worker(&cwd))
}

#[cfg(test)]
impl EndpointWorkers {
    pub(super) fn set_runner(&mut self, runner: CheckoutRootRunner) {
        self.runner = runner;
    }

    pub(super) fn enqueue(&self, completion: WorkerCompletion) {
        self.sender.send(completion).expect("worker channel open");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admission_counts_running_workers_and_queued_completions() {
        let mut workers = EndpointWorkers::new();
        let running = (0..MAX_WORKER_COMPLETION_BACKLOG - 1)
            .map(|_| RunningWorker::start(&workers.running))
            .collect::<Vec<_>>();
        assert!(workers.can_admit());
        let last = RunningWorker::start(&workers.running);
        assert!(!workers.can_admit());
        drop(last);
        assert!(workers.can_admit());
        workers.enqueue(WorkerCompletion::CheckoutRoot {
            ticket: ReplyTicket {
                client_id: crate::server::ClientId::test_new(1),
                seq: crate::server::outbox::ReplySeq::test_new(0),
            },
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            request_id: shepr_protocol::RequestId::allocate(),
            home: None,
            result: Ok(None),
        });
        assert!(!workers.can_admit());
        assert!(workers.recv().await.is_some());
        assert!(workers.can_admit());
        drop(running);
    }
}
