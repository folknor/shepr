//! Everything the server sends one client.
//!
//! A `ClientOutbox` owns the connection's queue, which the socket writer
//! thread drains: a bounded control lane (FIFO, items and bytes counted while
//! queued and in flight) and a one-frame surface slot that the writer takes
//! after control. Loop-side, it holds endpoint replies until the render pass
//! that follows their command has projected the change (`ReplyQueue`), and
//! remembers which presentation state the client was last told (`Told`).
//!
//! Closure is a state of the queue, not an event. Every way a connection ends
//! from the server side (control overflow, a failed socket write, a failed
//! health pong, an unencodable message, an explicit close) goes through
//! `OutboxQueue::close_connection`, which shuts the socket and wakes the
//! server loop; the loop's reap then removes the client.

use crate::limits::{
    CLIENT_CONTROL_QUEUE_MAX_BYTES, CLIENT_CONTROL_QUEUE_MAX_ITEMS, MAX_HELD_ENDPOINT_REPLIES,
    MAX_HELD_ENDPOINT_REPLY_BYTES,
};
use shepr_platform::ipc::LocalStream;
use shepr_protocol::ServerMessage;
use shepr_term::mouse::HostMouseCapture;
use std::collections::VecDeque;
use std::io;
use std::net::Shutdown;
use std::sync::mpsc::{SendError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Notify;
use tracing::{debug, warn};

fn encode_message_or_close<M: serde::Serialize>(
    queue: &OutboxQueue,
    message: &M,
) -> Option<Vec<u8>> {
    match shepr_protocol::encode_message(message) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            warn!(%error, "failed to encode client message; closing the client");
            queue.close_connection();
            None
        }
    }
}

/// What happened to a message offered to a client.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Queued,
    /// The outbox is closed (or never connected); nothing was queued.
    Closed,
}

/// What happened to a surface frame offered to a client.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SurfaceOffer {
    Queued,
    /// The writer has not taken the previous frame yet; nothing was queued.
    Occupied,
    Closed,
}

/// Everything the server sends one client. Owned by its `ClientConnection`
/// and deliberately not `Clone`: it owns the loop-side reply queue and told
/// state, and counts as one sender on the queue until it is dropped.
#[derive(Debug)]
pub(crate) struct ClientOutbox {
    queue: Arc<OutboxQueue>,
    replies: ReplyQueue,
    told: Told,
}

impl ClientOutbox {
    pub(crate) fn from_queue(queue: Arc<OutboxQueue>) -> Self {
        queue.add_sender();
        Self {
            queue,
            replies: ReplyQueue::default(),
            told: Told::default(),
        }
    }

    pub(crate) fn for_connection(stream: LocalStream, wake: Arc<Notify>) -> Self {
        Self::from_queue(OutboxQueue::new_for_connection(stream, wake))
    }

    /// The queue the socket writer thread drains.
    pub(crate) fn queue_handle(&self) -> Arc<OutboxQueue> {
        Arc::clone(&self.queue)
    }

    /// The reader thread's handle, for health pongs.
    pub(crate) fn control_sender(&self) -> ControlSender {
        ControlSender::new(Arc::clone(&self.queue))
    }

    /// Whether the writer has closed. The loop reaps closed connections.
    pub(crate) fn is_closed(&self) -> bool {
        !self.queue.lock_state().writer_alive
    }

    pub(crate) fn close(&self) {
        self.queue.close_connection();
    }

    /// Drops a surface frame the writer has not taken yet.
    pub(crate) fn discard_pending_surface(&self) {
        self.queue.discard_pending_render();
    }

    /// Appends a barrier to the control lane. The receiver resolves once the
    /// writer has flushed everything queued before it, or is dropped if the
    /// writer exits first.
    pub(crate) fn flush_barrier(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.queue.send_flush()
    }

    pub(crate) fn surface_slot_free(&self) -> bool {
        let state = self.queue.lock_state();
        state.writer_alive && state.render.is_none()
    }

    /// The only way a surface frame enters the slot.
    pub(crate) fn offer_surface(&self, framed: Vec<u8>) -> SurfaceOffer {
        match self.queue.try_send_render(framed) {
            Ok(()) => SurfaceOffer::Queued,
            Err(TrySendError::Full(_)) => SurfaceOffer::Occupied,
            Err(TrySendError::Disconnected(_)) => SurfaceOffer::Closed,
        }
    }

    /// Encodes and queues a message on the control lane. A message past the
    /// lane's remaining budget closes the outbox (the slow-reader policy), as
    /// does one that cannot be encoded: the client could never receive it.
    pub(crate) fn send(&self, message: &ServerMessage) -> Delivery {
        let Some(bytes) = self.frame(message) else {
            return Delivery::Closed;
        };
        if self.queue.send_control(bytes).is_ok() {
            Delivery::Queued
        } else {
            Delivery::Closed
        }
    }

    /// Encodes a message for this outbox, or `None` when the outbox cannot
    /// take it: it is closed, or the message cannot be encoded, which closes it.
    fn frame<M: serde::Serialize>(&self, message: &M) -> Option<Vec<u8>> {
        if !self.queue.lock_state().writer_alive {
            return None;
        }
        encode_message_or_close(&self.queue, message)
    }
}

impl Drop for ClientOutbox {
    fn drop(&mut self) {
        self.queue.remove_sender();
    }
}

/// The reader thread's handle on a connection's queue: control sends (the
/// health pong), which close the connection on overflow or an encode
/// failure. Each clone counts as a sender, so the writer
/// thread keeps draining while the reader holds one after the outbox is gone.
#[derive(Debug)]
pub(crate) struct ControlSender {
    queue: Arc<OutboxQueue>,
}

impl Clone for ControlSender {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.queue))
    }
}

impl Drop for ControlSender {
    fn drop(&mut self) {
        self.queue.remove_sender();
    }
}

impl ControlSender {
    fn new(queue: Arc<OutboxQueue>) -> Self {
        queue.add_sender();
        Self { queue }
    }

    /// Queues a message under the control lane's policy; overflow and an
    /// encode failure both close the connection.
    #[must_use = "a closed client connection is reaped by the server loop"]
    pub(crate) fn send(&self, message: &ServerMessage) -> Delivery {
        let Some(framed) = encode_message_or_close(&self.queue, message) else {
            return Delivery::Closed;
        };
        if self.queue.send_control(framed).is_ok() {
            Delivery::Queued
        } else {
            Delivery::Closed
        }
    }
}

/// One connection's queue, shared by its outbox, the reader's control sender
/// and the socket writer thread.
#[derive(Debug)]
pub(crate) struct OutboxQueue {
    state: Mutex<OutboxQueueState>,
    ready: Condvar,
    shutdown_stream: Mutex<Option<LocalStream>>,
    max_control_items: usize,
    max_control_bytes: usize,
    /// The server loop's outbox wake: raised on close, and when the writer
    /// frees control-lane room a held reply is waiting for.
    wake: Arc<Notify>,
}

#[derive(Debug, Default)]
struct OutboxQueueState {
    control: VecDeque<ClientControlItem>,
    /// Queued and in-flight control items share the same bound.
    control_items: usize,
    /// Queued and in-flight control bytes share the same bound.
    control_bytes: usize,
    render: Option<Vec<u8>>,
    senders: usize,
    writer_alive: bool,
    /// A held reply was handed back for lack of room; the writer wakes the
    /// loop when it next finishes a control item.
    room_wanted: bool,
}

#[derive(Debug)]
pub(crate) enum ClientWriteItem {
    Control(Vec<u8>),
    Render(Vec<u8>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

#[derive(Debug)]
enum ClientControlItem {
    Data(Vec<u8>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

impl OutboxQueue {
    pub(crate) fn new_for_connection(shutdown_stream: LocalStream, wake: Arc<Notify>) -> Arc<Self> {
        Self::with_limits(
            Some(shutdown_stream),
            wake,
            CLIENT_CONTROL_QUEUE_MAX_ITEMS,
            CLIENT_CONTROL_QUEUE_MAX_BYTES,
        )
    }

    pub(crate) fn with_limits(
        shutdown_stream: Option<LocalStream>,
        wake: Arc<Notify>,
        max_control_items: usize,
        max_control_bytes: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(OutboxQueueState {
                writer_alive: true,
                ..OutboxQueueState::default()
            }),
            ready: Condvar::new(),
            shutdown_stream: Mutex::new(shutdown_stream),
            max_control_items,
            max_control_bytes,
            wake,
        })
    }

    fn add_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_add(1);
    }

    fn remove_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_sub(1);
        self.ready.notify_one();
    }

    /// Whether `len` more control bytes fit the lane's remaining budget.
    fn control_fits(&self, state: &OutboxQueueState, len: usize) -> bool {
        state.control_items < self.max_control_items
            && len <= self.max_control_bytes.saturating_sub(state.control_bytes)
    }

    fn push_control(&self, state: &mut OutboxQueueState, data: Vec<u8>) {
        state.control_items += 1;
        state.control_bytes += data.len();
        state.control.push_back(ClientControlItem::Data(data));
        self.ready.notify_one();
    }

    /// Queues `data` only if it fits the remaining budget. Otherwise hands it
    /// back without closing anything and asks the writer to wake the loop
    /// once it has drained an item, so a held reply waits loop-side for room
    /// instead of exceeding the backlog bound or closing the client.
    fn try_send_control_within_budget(&self, data: Vec<u8>) -> Result<(), Vec<u8>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(data);
        }
        if !self.control_fits(&state, data.len()) {
            if state.control_items == 0 {
                // Nothing is queued or in flight, so no drain will ever make
                // room: the reply is past the lane's whole budget, which
                // `response_message` rules out. Close rather than hold it
                // forever with its client waiting out its command timeout.
                drop(state);
                self.close_connection();
                return Err(data);
            }
            state.room_wanted = true;
            return Err(data);
        }
        self.push_control(&mut state, data);
        Ok(())
    }

    pub(crate) fn send_control(&self, data: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(SendError(data));
        }
        // Control items share the byte budget while queued and in flight.
        // Immediate control traffic that exceeds the remaining budget closes
        // the connection. Held endpoint replies instead wait for lane room
        // (`try_send_control_within_budget`), without admitting any bytes
        // past this queued plus in-flight bound.
        if !self.control_fits(&state, data.len()) {
            drop(state);
            self.close_connection();
            return Err(SendError(data));
        }
        self.push_control(&mut state, data);
        Ok(())
    }

    fn send_flush(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (ack, receiver) = tokio::sync::oneshot::channel();
        let mut state = self.lock_state();
        if !state.writer_alive {
            return receiver;
        }
        if state.control_items >= self.max_control_items {
            drop(state);
            self.close_connection();
            return receiver;
        }
        state.control_items += 1;
        state.control.push_back(ClientControlItem::Flush(ack));
        self.ready.notify_one();
        receiver
    }

    pub(crate) fn try_send_render(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if !state.writer_alive {
            return Err(TrySendError::Disconnected(data));
        }
        if state.render.is_some() {
            return Err(TrySendError::Full(data));
        }
        state.render = Some(data);
        self.ready.notify_one();
        Ok(())
    }

    fn discard_pending_render(&self) {
        let mut state = self.lock_state();
        state.render = None;
        self.ready.notify_all();
    }

    /// The writer thread's next item: control first, then the surface slot.
    /// `None` once the queue is closed, or empty with no sender left.
    pub(crate) fn recv(&self) -> Option<ClientWriteItem> {
        let mut state = self.lock_state();
        loop {
            if let Some(item) = state.control.pop_front() {
                return Some(match item {
                    ClientControlItem::Data(data) => ClientWriteItem::Control(data),
                    ClientControlItem::Flush(ack) => ClientWriteItem::Flush(ack),
                });
            }
            if let Some(data) = state.render.take() {
                return Some(ClientWriteItem::Render(data));
            }
            if state.senders == 0 || !state.writer_alive {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Releases a written control item's share of the budget, and wakes the
    /// loop if a held reply was waiting for that room.
    pub(crate) fn finish_control_item(&self, bytes: usize) {
        let mut state = self.lock_state();
        state.control_items = state.control_items.saturating_sub(1);
        state.control_bytes = state.control_bytes.saturating_sub(bytes);
        if state.room_wanted {
            state.room_wanted = false;
            self.wake.notify_one();
        }
    }

    /// The one place a connection ends from the server side: drops whatever
    /// is queued, shuts both socket directions so the reader and writer
    /// threads stop, and wakes the loop to reap the client. Idempotent: a
    /// second call shuts nothing again and stores no second wake.
    pub(crate) fn close_connection(&self) {
        {
            let mut state = self.lock_state();
            if !state.writer_alive {
                return;
            }
            state.writer_alive = false;
            state.render = None;
            state.control.clear();
            state.control_items = 0;
            state.control_bytes = 0;
            self.ready.notify_all();
        }
        self.wake.notify_one();
        let stream = self
            .shutdown_stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(stream) = stream
            && let Err(error) = stream.shutdown(Shutdown::Both)
            && error.kind() != io::ErrorKind::NotConnected
        {
            debug!(error = %error, "failed to shut down client connection");
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, OutboxQueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// How held replies enter the control lane.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ReleaseMode {
    /// Only into room the lane has; a reply that does not fit waits
    /// loop-side, with everything behind it, until the writer drains.
    WithinBudget,
    /// Under the lane's ordinary policy (overflow closes): the connection is
    /// ending, and the shutdown notice must follow the replies.
    Shutdown,
}

/// Endpoint replies held loop-side, in command order. Entries hold framed
/// bytes, so their size is known while they are held.
#[derive(Debug, Default)]
struct ReplyQueue {
    entries: VecDeque<Vec<u8>>,
    /// Sum of every held buffer.
    held_bytes: usize,
}

impl ClientOutbox {
    /// Checks the held-reply bounds after an addition; past either, closes
    /// the outbox (a client that piles up replies past any legitimate
    /// backlog is dropped rather than buffered for).
    fn admission(&self, count: usize, bytes: usize) -> bool {
        if count > MAX_HELD_ENDPOINT_REPLIES || bytes > MAX_HELD_ENDPOINT_REPLY_BYTES {
            warn!(
                count,
                bytes, "client exceeded held endpoint reply budget; closing the client"
            );
            self.close();
            false
        } else {
            true
        }
    }

    /// Holds a reply behind any earlier one until the next release, within
    /// the held-reply bounds. Encoding or held-budget failure closes the queue
    /// for the loop to reap.
    pub(crate) fn hold_reply(&mut self, message: &ServerMessage) {
        let Some(bytes) = self.frame(message) else {
            return;
        };
        let total = self.replies.held_bytes.saturating_add(bytes.len());
        if !self.admission(self.replies.entries.len().saturating_add(1), total) {
            return;
        }
        self.replies.held_bytes = total;
        self.replies.entries.push_back(bytes);
    }

    /// Moves held replies onto the control lane, in order, under `mode`.
    /// Within budget, stops at the first reply the lane has no room for. A
    /// closed queue wakes the loop to reap the client; a reply waiting for
    /// room also wakes it when the writer drains the lane.
    pub(crate) fn release_replies(&mut self, mode: ReleaseMode) {
        while let Some(entry) = self.replies.entries.front_mut() {
            let bytes = std::mem::take(entry);
            let len = bytes.len();
            let result = match mode {
                ReleaseMode::WithinBudget => self.queue.try_send_control_within_budget(bytes),
                ReleaseMode::Shutdown => self.queue.send_control(bytes).map_err(|error| error.0),
            };
            match result {
                Ok(()) => {
                    self.replies.entries.pop_front();
                    self.replies.held_bytes = self.replies.held_bytes.saturating_sub(len);
                }
                Err(bytes) => {
                    if let Some(entry) = self.replies.entries.front_mut() {
                        *entry = bytes;
                    }
                    break;
                }
            }
        }
    }
}

/// What the control lane last told the client, so that sending a mode and
/// remembering it was sent are one operation. `None` is "not told since the
/// presentation was last reset".
#[derive(Debug, Default)]
struct Told {
    mouse_capture: Option<HostMouseCapture>,
    keyboard_report_all: Option<bool>,
}

impl ClientOutbox {
    /// Forgets everything told, so the next `tell_*` of each value sends it
    /// again (a surface activation or a host-effects replay requests them).
    pub(crate) fn forget_presentation(&mut self) {
        self.told = Told::default();
    }

    pub(crate) fn tell_mouse_capture(&mut self, mode: HostMouseCapture) {
        if self.told.mouse_capture == Some(mode) {
            return;
        }
        let result = self.send(&ServerMessage::MouseCapture { mode });
        if result == Delivery::Queued {
            self.told.mouse_capture = Some(mode);
        }
    }

    pub(crate) fn tell_keyboard_report_all(&mut self, enabled: bool) {
        if self.told.keyboard_report_all == Some(enabled) {
            return;
        }
        let result = self.send(&ServerMessage::ClientShellKeyboardReportAll { enabled });
        if result == Delivery::Queued {
            self.told.keyboard_report_all = Some(enabled);
        }
    }
}

#[cfg(test)]
pub(crate) use tests::RenderLaneReceiver;

#[cfg(test)]
impl ClientOutbox {
    /// A closed outbox for fixtures. It follows the same reap policy as a
    /// connection whose writer has exited.
    pub(crate) fn detached() -> Self {
        let queue = OutboxQueue::with_limits(
            None,
            Arc::new(Notify::new()),
            CLIENT_CONTROL_QUEUE_MAX_ITEMS,
            CLIENT_CONTROL_QUEUE_MAX_BYTES,
        );
        queue.close_connection();
        Self::from_queue(queue)
    }

    pub(crate) fn held_reply_count(&self) -> usize {
        self.replies.entries.len()
    }

    pub(crate) fn told_mouse_capture(&self) -> Option<HostMouseCapture> {
        self.told.mouse_capture
    }

    pub(crate) fn told_keyboard_report_all(&self) -> Option<bool> {
        self.told.keyboard_report_all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    /// How often a test reader re-checks the queue. The queue's condvar wakes one
    /// waiter, and a test can have two (the control drain and a render read), so
    /// test readers poll rather than rely on being the one woken.
    const TEST_LANE_POLL: Duration = Duration::from_millis(2);

    /// The test side of a writer's render slot, read the way the socket writer
    /// thread takes it. Mirrors the `std::sync::mpsc::Receiver` methods tests use.
    #[derive(Debug)]
    pub(crate) struct RenderLaneReceiver {
        queue: Arc<OutboxQueue>,
    }

    impl RenderLaneReceiver {
        pub(crate) fn try_recv(&self) -> Result<Vec<u8>, std::sync::mpsc::TryRecvError> {
            self.queue.take_render_for_test()
        }

        pub(crate) fn recv(&self) -> Result<Vec<u8>, std::sync::mpsc::RecvError> {
            loop {
                match self.queue.take_render_for_test() {
                    Ok(data) => return Ok(data),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(std::sync::mpsc::RecvError);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        self.queue.wait_for_test(TEST_LANE_POLL);
                    }
                }
            }
        }

        pub(crate) fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Result<Vec<u8>, std::sync::mpsc::RecvTimeoutError> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match self.queue.take_render_for_test() {
                    Ok(data) => return Ok(data),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(std::sync::mpsc::RecvTimeoutError::Timeout);
                }
                self.queue
                    .wait_for_test(TEST_LANE_POLL.min(deadline.saturating_duration_since(now)));
            }
        }
    }

    impl ClientOutbox {
        /// An attached outbox nobody drains, raising `wake` like a connection.
        pub(crate) fn test_buffered(wake: Arc<Notify>, max_items: usize, max_bytes: usize) -> Self {
            Self::from_queue(OutboxQueue::with_limits(None, wake, max_items, max_bytes))
        }

        /// A writer over the production queue whose far side a test reads in
        /// place of the socket writer thread: control items arrive on the
        /// returned channel (flush barriers are acknowledged as they are
        /// reached), and the render slot is read through [`RenderLaneReceiver`].
        /// The server side sends through exactly the code production uses, so the
        /// one-slot render backpressure a test sees is the real one: a render the
        /// test has not read keeps the slot full.
        pub(crate) fn test_pair() -> (Self, std::sync::mpsc::Receiver<Vec<u8>>, RenderLaneReceiver)
        {
            let queue = OutboxQueue::with_limits(
                None,
                Arc::new(Notify::new()),
                CLIENT_CONTROL_QUEUE_MAX_ITEMS,
                CLIENT_CONTROL_QUEUE_MAX_BYTES,
            );
            let writer = ClientOutbox::from_queue(Arc::clone(&queue));
            let (control_tx, control_rx) = std::sync::mpsc::channel();
            let drain = Arc::clone(&queue);
            std::thread::spawn(move || {
                while let Some(item) = drain.recv_control_for_test() {
                    match item {
                        ClientControlItem::Data(data) => {
                            let bytes = data.len();
                            if control_tx.send(data).is_err() {
                                // The test dropped its control receiver: the
                                // client is gone, as a failed socket write says.
                                drain.close_connection();
                                return;
                            }
                            drain.finish_control_item(bytes);
                        }
                        ClientControlItem::Flush(ack) => {
                            // Same contract as the socket writer: a waiter that
                            // stopped waiting dropped its receiver, and the
                            // barrier was reached either way.
                            ack.send(()).ok();
                            drain.finish_control_item(0);
                        }
                    }
                }
            });
            (writer, control_rx, RenderLaneReceiver { queue })
        }
    }

    /// The far side of the queue as a test reads it, standing in for
    /// `client_writer_loop`. Only the consumer side is replaced; every send goes
    /// through the production methods above.
    impl OutboxQueue {
        /// The next control item, or `None` once the lane can produce no more.
        fn recv_control_for_test(&self) -> Option<ClientControlItem> {
            let mut state = self.lock_state();
            loop {
                if let Some(item) = state.control.pop_front() {
                    return Some(item);
                }
                if state.senders == 0 || !state.writer_alive {
                    return None;
                }
                state = self
                    .ready
                    .wait_timeout(state, TEST_LANE_POLL)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        }

        /// Takes the render slot, as the writer thread does before writing it.
        fn take_render_for_test(&self) -> Result<Vec<u8>, std::sync::mpsc::TryRecvError> {
            let mut state = self.lock_state();
            if let Some(data) = state.render.take() {
                return Ok(data);
            }
            if state.senders == 0 || !state.writer_alive {
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            } else {
                Err(std::sync::mpsc::TryRecvError::Empty)
            }
        }

        fn wait_for_test(&self, timeout: Duration) {
            let state = self.lock_state();
            drop(
                self.ready
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }

    /// A queue with its own wake and no socket, as the tests drive it.
    fn test_queue(max_items: usize, max_bytes: usize) -> Arc<OutboxQueue> {
        OutboxQueue::with_limits(None, Arc::new(Notify::new()), max_items, max_bytes)
    }

    fn test_queue_writer() -> (ClientOutbox, Arc<OutboxQueue>) {
        let queue = test_queue(
            CLIENT_CONTROL_QUEUE_MAX_ITEMS,
            CLIENT_CONTROL_QUEUE_MAX_BYTES,
        );
        (ClientOutbox::from_queue(Arc::clone(&queue)), queue)
    }

    fn single_frame(message: &ServerMessage) -> Vec<u8> {
        shepr_protocol::encode_frame(message).expect("frame")
    }

    #[test]
    fn client_writer_queue_keeps_render_slot_bounded() {
        let (writer, _queue) = test_queue_writer();
        let first = single_frame(&payload("first"));
        let second = single_frame(&payload("second"));

        writer
            .queue_handle()
            .try_send_render(first)
            .expect("first render fits");
        assert!(matches!(
            writer.queue_handle().try_send_render(second),
            Err(TrySendError::Full(_))
        ));
    }

    #[test]
    fn client_control_queue_bounds_outstanding_bytes_and_items() {
        let queue = test_queue(2, 5);
        let writer = ClientOutbox::from_queue(Arc::clone(&queue));
        writer
            .queue_handle()
            .send_control(vec![b'x'; 5])
            .expect("message within the byte bound fits");
        let Some(ClientWriteItem::Control(data)) = queue.recv() else {
            panic!("expected the queued control message");
        };
        assert_eq!(data.len(), 5);
        assert!(matches!(
            writer.queue_handle().send_control(vec![b'y']),
            Err(SendError(_))
        ));
        assert!(matches!(
            writer.queue_handle().try_send_render(vec![b'z']),
            Err(TrySendError::Disconnected(_))
        ));

        let queue = test_queue(2, 10);
        let writer = ClientOutbox::from_queue(Arc::clone(&queue));
        writer
            .queue_handle()
            .send_control(vec![b'a'])
            .expect("first item fits");
        writer
            .queue_handle()
            .send_control(vec![b'b'])
            .expect("second item fits");
        assert!(matches!(queue.recv(), Some(ClientWriteItem::Control(_))));
        assert!(matches!(
            writer.queue_handle().send_control(vec![b'c']),
            Err(SendError(_))
        ));
    }

    #[test]
    fn client_control_queue_closes_when_endpoint_reply_exceeds_remaining_byte_budget() {
        let reply =
            shepr_protocol::encode_message(&crate::server::client_commands::response_message(
                shepr_test_fixtures::fixed_boot_id(1),
                shepr_protocol::RequestId::allocate(),
                Ok(shepr_protocol::command::EndpointReply::Done),
            ))
            .expect("endpoint response frames");
        let byte_cap = reply.len();
        let empty_queue = test_queue(4, byte_cap);
        let empty_writer = ClientOutbox::from_queue(Arc::clone(&empty_queue));
        empty_writer
            .queue_handle()
            .send_control(reply.clone())
            .expect("the endpoint reply fits an empty queue");

        let queue = test_queue(4, byte_cap);
        let writer = ClientOutbox::from_queue(Arc::clone(&queue));

        writer
            .queue_handle()
            .send_control(vec![b'x'])
            .expect("first control item fits");
        assert!(matches!(
            writer.queue_handle().send_control(reply),
            Err(SendError(_))
        ));
        assert!(matches!(
            writer.queue_handle().try_send_render(vec![b'z']),
            Err(TrySendError::Disconnected(_))
        ));
    }

    fn queued_outbox() -> ClientOutbox {
        ClientOutbox::from_queue(test_queue(
            CLIENT_CONTROL_QUEUE_MAX_ITEMS,
            CLIENT_CONTROL_QUEUE_MAX_BYTES,
        ))
    }

    /// A control message carrying `value`, for tests that need a distinct
    /// payload of a chosen size.
    fn payload(value: &str) -> ServerMessage {
        ServerMessage::Clipboard {
            data: value.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn control_overflow_closes_the_outbox_and_wakes_the_loop() {
        let queue = test_queue(1, 1);
        let outbox = ClientOutbox::from_queue(Arc::clone(&queue));
        assert_eq!(outbox.send(&payload("too large")), Delivery::Closed);
        assert!(outbox.is_closed());
        tokio::time::timeout(Duration::from_millis(100), queue.wake.notified())
            .await
            .expect("close wakes loop");
    }

    #[tokio::test]
    async fn a_closed_outbox_refuses_every_later_send_and_closing_twice_is_a_no_op() {
        let outbox = queued_outbox();
        outbox.close();
        outbox.queue.wake.notified().await;
        assert_eq!(outbox.send(&payload("late")), Delivery::Closed);
        assert_eq!(outbox.offer_surface(vec![1]), SurfaceOffer::Closed);
        outbox.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), outbox.queue.wake.notified())
                .await
                .is_err()
        );
    }

    #[test]
    fn a_closed_fixture_outbox_refuses_sends_and_is_reaped() {
        let outbox = ClientOutbox::detached();
        assert!(outbox.is_closed());
        assert_eq!(outbox.send(&payload("late")), Delivery::Closed);
        assert_eq!(outbox.offer_surface(vec![1]), SurfaceOffer::Closed);
    }

    #[test]
    fn held_replies_leave_in_command_order() {
        let mut outbox = queued_outbox();
        outbox.hold_reply(&payload("first"));
        outbox.hold_reply(&payload("second"));
        assert_eq!(outbox.held_reply_count(), 2);
        assert!(outbox.queue.lock_state().control.is_empty());
        outbox.release_replies(ReleaseMode::WithinBudget);
        assert_eq!(outbox.held_reply_count(), 0);
        for expected in ["first", "second"] {
            let Some(ClientWriteItem::Control(data)) = outbox.queue.recv() else {
                panic!("control reply");
            };
            let message: ServerMessage =
                shepr_protocol::read_message(&mut data.as_slice()).expect("decode");
            assert_eq!(message, payload(expected));
            outbox.queue.finish_control_item(data.len());
        }
    }

    #[test]
    fn held_reply_count_over_the_bound_closes_the_outbox() {
        let mut outbox = queued_outbox();
        for _ in 0..MAX_HELD_ENDPOINT_REPLIES {
            outbox.hold_reply(&payload("held"));
            assert!(!outbox.is_closed());
        }
        outbox.hold_reply(&payload("overflow"));
        assert!(outbox.is_closed());
    }

    #[test]
    fn held_reply_bytes_over_the_bound_closes_the_outbox() {
        let mut outbox = queued_outbox();
        let message = payload(&"x".repeat(MAX_HELD_ENDPOINT_REPLY_BYTES / 2));
        outbox.hold_reply(&message);
        assert!(!outbox.is_closed());
        outbox.hold_reply(&message);
        assert!(outbox.is_closed());
    }

    #[tokio::test]
    async fn a_reply_that_fits_the_lane_waits_for_room_instead_of_closing() {
        let bytes = shepr_protocol::encode_message(&payload("reply")).expect("frame");
        let queue = test_queue(2, bytes.len());
        let mut outbox = ClientOutbox::from_queue(Arc::clone(&queue));
        queue.send_control(vec![1]).expect("occupy part of budget");
        outbox.hold_reply(&payload("reply"));
        outbox.release_replies(ReleaseMode::WithinBudget);
        assert_eq!(outbox.held_reply_count(), 1);
        assert!(!outbox.is_closed());
        assert!(matches!(queue.recv(), Some(ClientWriteItem::Control(_))));
        queue.finish_control_item(1);
        tokio::time::timeout(Duration::from_millis(100), queue.wake.notified())
            .await
            .expect("lane room wakes loop");
        outbox.release_replies(ReleaseMode::WithinBudget);
        assert_eq!(outbox.held_reply_count(), 0);
        assert!(!outbox.is_closed());
    }

    #[test]
    fn a_reply_past_the_whole_lane_closes_instead_of_waiting_forever() {
        let bytes = shepr_protocol::encode_message(&payload("reply")).expect("frame");
        let mut outbox = ClientOutbox::from_queue(test_queue(2, bytes.len() - 1));
        outbox.hold_reply(&payload("reply"));
        assert!(!outbox.is_closed());
        outbox.release_replies(ReleaseMode::WithinBudget);
        assert!(outbox.is_closed());
    }

    #[test]
    fn told_values_send_only_changes_and_forget_on_presentation_reset() {
        let mut outbox = queued_outbox();
        outbox.tell_mouse_capture(HostMouseCapture::Pixels);
        outbox.tell_keyboard_report_all(false);
        outbox.tell_mouse_capture(HostMouseCapture::Pixels);
        outbox.tell_keyboard_report_all(false);
        assert_eq!(outbox.queue.lock_state().control_items, 2);
        outbox.forget_presentation();
        outbox.tell_mouse_capture(HostMouseCapture::Pixels);
        outbox.tell_keyboard_report_all(false);
        assert_eq!(outbox.queue.lock_state().control_items, 4);
    }

    #[test]
    fn the_surface_slot_holds_one_frame_and_frees_when_the_writer_takes_it() {
        let outbox = queued_outbox();
        assert!(outbox.surface_slot_free());
        assert_eq!(outbox.offer_surface(vec![1]), SurfaceOffer::Queued);
        assert!(!outbox.surface_slot_free());
        assert_eq!(outbox.offer_surface(vec![2]), SurfaceOffer::Occupied);
        assert!(
            matches!(outbox.queue.recv(), Some(ClientWriteItem::Render(data)) if data == vec![1])
        );
        assert!(outbox.surface_slot_free());
    }

    #[test]
    fn an_unencodable_message_closes_the_outbox() {
        struct Unencodable;
        impl serde::Serialize for Unencodable {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("injected encoding failure"))
            }
        }
        let outbox = queued_outbox();
        assert!(outbox.frame(&Unencodable).is_none());
        assert!(outbox.is_closed());
        assert_eq!(outbox.send(&payload("later")), Delivery::Closed);
    }

    #[test]
    fn shutdown_release_admits_replies_ahead_of_the_shutdown_notice() {
        let mut outbox = queued_outbox();
        outbox.hold_reply(&payload("reply"));
        outbox.release_replies(ReleaseMode::Shutdown);
        assert_eq!(outbox.send(&payload("shutdown")), Delivery::Queued);
        for expected in ["reply", "shutdown"] {
            let Some(ClientWriteItem::Control(bytes)) = outbox.queue.recv() else {
                panic!("control item");
            };
            let actual: ServerMessage =
                shepr_protocol::read_message(&mut bytes.as_slice()).expect("decode");
            assert_eq!(actual, payload(expected));
            outbox.queue.finish_control_item(bytes.len());
        }
    }
}
