//! The byte channel between a pane and its child.

use std::time::Duration;

use bytes::Bytes;

use crate::actor::{PtyIoActorHandle, QueuedSubmission};

/// A non-blocking child write was rejected. The original bytes are returned
/// so the caller can retry or handle the input itself.
#[derive(Debug, PartialEq, Eq)]
pub enum ChildIoSendError {
    /// The child's input queue has no room for this write.
    Full(Bytes),
    /// The child channel has stopped accepting writes.
    Closed(Bytes),
}

impl std::fmt::Display for ChildIoSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(_) => f.write_str("child input queue is full"),
            Self::Closed(_) => f.write_str("child input channel is closed"),
        }
    }
}

impl std::error::Error for ChildIoSendError {}

/// What a pane runtime writes to its child through: user input, prompt
/// submissions, terminal replies and resizes. Every pane shepr runs talks to
/// its child through the PTY actor ([`PtyIoActorHandle`]); the trait is the
/// seam that lets a caller supply another channel, such as one with no child
/// behind it.
///
/// The closures run under the implementation's reply-order lock (they may take
/// the terminal core lock) and are called at most once. They are `FnMut` only
/// so the trait stays object-safe.
pub trait ChildIo: Send + Sync {
    /// Stop accepting writes; anything queued afterwards is dropped.
    fn shutdown(&self);

    /// Whether a child process stands behind the channel, so ending the pane
    /// has a process group to signal.
    fn owns_child_process(&self) -> bool;

    /// Apply `geometry`, queueing the replies `terminal_responses` produces at
    /// the resize's place in the reply order.
    fn resize(
        &self,
        geometry: shepr_core::geometry::PaneGeometry,
        terminal_responses: &mut dyn FnMut() -> Vec<Bytes>,
    );

    fn try_write_user_input(&self, bytes: Bytes) -> Result<(), ChildIoSendError>;

    /// Queue a terminal reply produced outside a read of the child's output.
    fn write_terminal_response(&self, response: &mut dyn FnMut() -> Option<Bytes>);

    /// Queue `text`, then `enter` once `delay` has passed.
    fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
    ) -> std::io::Result<QueuedSubmission>;
}

impl ChildIo for PtyIoActorHandle {
    fn shutdown(&self) {
        PtyIoActorHandle::shutdown(self);
    }

    fn owns_child_process(&self) -> bool {
        true
    }

    fn resize(
        &self,
        geometry: shepr_core::geometry::PaneGeometry,
        terminal_responses: &mut dyn FnMut() -> Vec<Bytes>,
    ) {
        PtyIoActorHandle::resize(self, geometry, terminal_responses);
    }

    fn try_write_user_input(&self, bytes: Bytes) -> Result<(), ChildIoSendError> {
        PtyIoActorHandle::try_write_user_input(self, bytes)
    }

    fn write_terminal_response(&self, response: &mut dyn FnMut() -> Option<Bytes>) {
        PtyIoActorHandle::write_terminal_response(self, response);
    }

    fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
    ) -> std::io::Result<QueuedSubmission> {
        PtyIoActorHandle::queue_user_input_submission(self, text, enter, delay)
    }
}
