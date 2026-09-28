//! A pane child channel with no child behind it.

use std::time::Duration;

use bytes::Bytes;
use shepr_pty::actor::{QueuedSubmission, SubmissionCancel};
use shepr_pty::{ChildIo, ChildIoSendError};
use tokio::sync::mpsc;

/// Stands in for the PTY actor in a pane runtime built with
/// `PaneRuntime::with_child_io`: whatever the pane writes to its child
/// (input, terminal replies) arrives on the receiver `new` returns. A resize
/// still resizes the terminal; its replies go nowhere. A submission's text and
/// Enter are sent from a thread `delay` apart, unordered against other writes,
/// and its cancel handle always answers that the submission finished.
pub struct ChannelChildIo {
    sender: mpsc::Sender<Bytes>,
}

impl ChannelChildIo {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<Bytes>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { sender }, receiver)
    }
}

impl ChildIo for ChannelChildIo {
    fn shutdown(&self) {}

    fn owns_child_process(&self) -> bool {
        false
    }

    fn resize(
        &self,
        _geometry: shepr_core::geometry::PaneGeometry,
        terminal_responses: &mut dyn FnMut() -> Vec<Bytes>,
    ) {
        let _ = terminal_responses();
    }

    fn try_write_user_input(&self, bytes: Bytes) -> Result<(), ChildIoSendError> {
        self.sender.try_send(bytes).map_err(|error| match error {
            mpsc::error::TrySendError::Full(bytes) => ChildIoSendError::Full(bytes),
            mpsc::error::TrySendError::Closed(bytes) => ChildIoSendError::Closed(bytes),
        })
    }

    fn write_terminal_response(&self, response: &mut dyn FnMut() -> Option<Bytes>) {
        if let Some(bytes) = response() {
            // Many tests drop the receiver or never drain it, so a closed or
            // full channel is the normal case for them; a test that checks
            // replies reads the receiver and sees any that went missing.
            self.sender.try_send(bytes).ok();
        }
    }

    fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
    ) -> std::io::Result<QueuedSubmission> {
        let sender = self.sender.clone();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = sender
                .try_send(text)
                .map_err(std::io::Error::other)
                .and_then(|()| {
                    std::thread::sleep(delay);
                    sender.try_send(enter).map_err(std::io::Error::other)
                });
            // The completion receiver is gone only when the caller dropped the
            // submission, and then nobody is waiting for the outcome.
            reply_tx.send(result).ok();
        });
        Ok(QueuedSubmission {
            completion: reply_rx,
            cancel: SubmissionCancel::untracked(),
        })
    }
}
