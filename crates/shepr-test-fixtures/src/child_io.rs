//! A pane child channel with no child behind it.

use std::time::Duration;

use bytes::Bytes;
use shepr_pty::ChildIo;
use shepr_pty::actor::{QueuedSubmission, SubmissionCancel};
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

    fn try_write_user_input(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.sender.try_send(bytes)
    }

    fn write_terminal_response(&self, response: &mut dyn FnMut() -> Option<Bytes>) {
        if let Some(bytes) = response() {
            let _ = self.sender.try_send(bytes);
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
            let _ = reply_tx.send(result);
        });
        Ok(QueuedSubmission {
            completion: reply_rx,
            cancel: SubmissionCancel::untracked(),
        })
    }
}
