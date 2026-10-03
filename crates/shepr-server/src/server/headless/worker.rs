use crate::server::outbox::ReplyTicket;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

pub(super) type CheckoutRootRunner = Arc<
    dyn Fn(PathBuf) -> Result<Option<shepr_protocol::RemotePath>, shepr_mux::git::GitReadError>
        + Send
        + Sync,
>;

pub(super) enum WorkerCompletion {
    CheckoutRoot {
        ticket: ReplyTicket,
        boot_id: shepr_protocol::BootId,
        request_id: shepr_protocol::RequestId,
        home: Option<shepr_protocol::RemotePath>,
        result: Result<Option<shepr_protocol::RemotePath>, shepr_mux::git::GitReadError>,
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
}

impl EndpointWorkers {
    pub(super) fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver,
            runner: default_checkout_root_runner(),
        }
    }

    /// Counts running workers and queued completions. During a send the
    /// overlap counts twice, making admission conservative.
    pub(super) fn can_admit(&self) -> bool {
        self.sender
            .strong_count()
            .saturating_sub(1)
            .saturating_add(self.receiver.len())
            < crate::limits::MAX_WORKER_COMPLETION_BACKLOG
    }

    pub(super) async fn recv(&mut self) -> Option<WorkerCompletion> {
        self.receiver.recv().await
    }

    /// Starts a checkout root worker thread. Its sender clone is what
    /// [`Self::can_admit`] counts as running until the completion is sent.
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
        let running = (0..crate::limits::MAX_WORKER_COMPLETION_BACKLOG - 1)
            .map(|_| workers.sender.clone())
            .collect::<Vec<_>>();
        assert!(workers.can_admit());
        let last = workers.sender.clone();
        assert!(!workers.can_admit());
        drop(last);
        assert!(workers.can_admit());
        workers.enqueue(WorkerCompletion::CheckoutRoot {
            ticket: ReplyTicket {
                client_id: crate::server::ClientId::test_new(1),
                seq: crate::server::outbox::ReplySeq::test_new(0),
            },
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            request_id: "queued".into(),
            home: None,
            result: Ok(None),
        });
        assert!(!workers.can_admit());
        assert!(workers.recv().await.is_some());
        assert!(workers.can_admit());
        drop(running);
    }
}
