use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::deadline::Deadline;
use crate::endpoint::EndpointTransport;
use crate::limits::{
    ENDPOINT_IO_POLL_INTERVAL, ENDPOINT_WRITE_TIMEOUT, MAX_BATCH_BYTES, MAX_QUEUED_BATCHES,
    MAX_QUEUED_BYTES,
};
use shepr_platform::ipc::LocalStream;
use shepr_protocol::ClientMessage;

/// When the reader thread last took a complete frame off one connection, and whether one of
/// them was an endpoint snapshot. Endpoint health reads this instead of the time the client
/// loop processed the frame, so a stalled loop is not mistaken for a silent transport.
pub(crate) struct EndpointReadActivity {
    started_at: Instant,
    /// Nanoseconds after `started_at` of the last frame, shifted left one bit; the low bit is
    /// set once a snapshot has arrived. Zero means no frame yet. One word keeps both readable
    /// together without a lock.
    stamp: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EndpointReadObservation {
    pub(crate) last_frame_at: Option<Instant>,
    pub(crate) snapshot_seen: bool,
}

impl EndpointReadActivity {
    pub(crate) fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            stamp: AtomicU64::new(0),
        }
    }

    /// Records a frame that arrived at `now`.
    pub(crate) fn record(&self, now: Instant, snapshot: bool) {
        let elapsed = u64::try_from(now.saturating_duration_since(self.started_at).as_nanos())
            .unwrap_or(u64::MAX)
            .clamp(1, u64::MAX >> 1);
        let snapshot_seen = snapshot || self.stamp.load(Ordering::Acquire) & 1 == 1;
        self.stamp
            .store((elapsed << 1) | u64::from(snapshot_seen), Ordering::Release);
    }

    /// When the last frame arrived, if any has, and whether a snapshot has.
    pub(crate) fn observed(&self) -> EndpointReadObservation {
        let stamp = self.stamp.load(Ordering::Acquire);
        let received_at = (stamp >> 1 > 0)
            .then(|| {
                self.started_at
                    .checked_add(Duration::from_nanos(stamp >> 1))
            })
            .flatten();
        EndpointReadObservation {
            last_frame_at: received_at,
            snapshot_seen: stamp & 1 == 1,
        }
    }
}

#[derive(Default)]
struct FrameBatch {
    frames: Vec<Vec<u8>>,
    bytes: usize,
}

enum WriterCommand {
    Frames(Arc<Mutex<FrameBatch>>),
    Flush(mpsc::Sender<()>),
}

/// The UI batches complete frames until the worker claims them, so a short burst of tiny input
/// frames does not exhaust command slots. Frames retain their individual write boundaries.
/// A worker owns partial writes and cancellation. The transport closes its remote
/// connection explicitly on disconnect; socket backpressure and SSH teardown
/// never block other endpoints.
pub(crate) struct NativeEndpointTransport {
    sender: mpsc::SyncSender<WriterCommand>,
    pending_batch: Option<Arc<Mutex<FrameBatch>>>,
    queued_bytes: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    error: Arc<Mutex<Option<io::Error>>>,
    read_activity: Arc<EndpointReadActivity>,
    remote_connection: Option<Arc<shepr_remote::MachineSshConnection>>,
}

impl NativeEndpointTransport {
    pub(crate) fn with_remote_connection(
        stream: LocalStream,
        connection: Arc<shepr_remote::MachineSshConnection>,
    ) -> io::Result<Self> {
        let mut transport = Self::with_lifetime(stream, ())?;
        transport.remote_connection = Some(connection);
        Ok(transport)
    }

    pub(crate) fn with_lifetime(
        mut stream: LocalStream,
        lifetime: impl Send + 'static,
    ) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        let (sender, receiver) = mpsc::sync_channel::<WriterCommand>(MAX_QUEUED_BATCHES);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        // clock-io-ok: the origin the reader thread stamps frame arrivals against.
        let read_activity = Arc::new(EndpointReadActivity::new(Instant::now()));
        let worker_bytes = Arc::clone(&queued_bytes);
        let worker_stop = Arc::clone(&stopped);
        let worker_error = Arc::clone(&error);
        std::thread::Builder::new()
            .name("endpoint-writer".into())
            .spawn(move || {
                let _lifetime = lifetime;
                while let Ok(command) = receiver.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let batch = match command {
                        WriterCommand::Frames(batch) => batch,
                        WriterCommand::Flush(done) => {
                            // The receiver is gone only when `flush` already timed out and
                            // reported that itself, so there is no one left to tell.
                            done.send(()).ok();
                            continue;
                        }
                    };
                    let result = write_batch(&mut stream, &batch, &worker_stop, &worker_bytes);
                    if let Err(error) = result {
                        if let Ok(mut slot) = worker_error.lock() {
                            *slot = Some(error);
                        }
                        worker_stop.store(true, Ordering::Release);
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender,
            pending_batch: None,
            queued_bytes,
            stopped,
            error,
            read_activity,
            remote_connection: None,
        })
    }

    fn enqueue_frame(&mut self, frame: Vec<u8>) -> io::Result<()> {
        if let Some(batch) = &self.pending_batch {
            let mut batch = batch
                .lock()
                .map_err(|_| io::Error::other("endpoint batch lock poisoned"))?;
            // An empty batch has already been claimed by the worker. Never append to it.
            if !batch.frames.is_empty()
                && frame.len() <= MAX_BATCH_BYTES.saturating_sub(batch.bytes)
            {
                batch.bytes += frame.len();
                batch.frames.push(frame);
                return Ok(());
            }
        }
        let batch = Arc::new(Mutex::new(FrameBatch {
            bytes: frame.len(),
            frames: vec![frame],
        }));
        self.sender
            .try_send(WriterCommand::Frames(Arc::clone(&batch)))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => queue_full(),
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "endpoint writer stopped")
                }
            })?;
        self.pending_batch = Some(batch);
        Ok(())
    }

    pub(crate) fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stopped)
    }

    pub(crate) fn read_activity(&self) -> Arc<EndpointReadActivity> {
        Arc::clone(&self.read_activity)
    }
}

impl EndpointTransport for NativeEndpointTransport {
    fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "endpoint writer stopped",
            ));
        }
        // A client message crosses in one frame: `encode_frame` refuses one
        // over `MAX_FRAME_SIZE`, so an oversized message fails here with a size
        // error before anything is queued, rather than reaching the server,
        // which reads client messages with a one-frame cap and would drop the
        // connection without a word. Pastes are checked against the server's
        // input limit even earlier, in the shell's input handling, and never
        // get this far.
        let frame = shepr_protocol::encode_frame(message).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                shepr_launch::EndpointFailure::local_setup(format!(
                    "could not encode endpoint message: {error}"
                )),
            )
        })?;
        let len = frame.len();
        if self
            .queued_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
                bytes
                    .checked_add(len)
                    .filter(|total| *total <= MAX_QUEUED_BYTES)
            })
            .is_err()
        {
            return Err(queue_full());
        }
        self.enqueue_frame(frame).inspect_err(|_| {
            self.queued_bytes.fetch_sub(len, Ordering::AcqRel);
        })
    }

    fn disconnect(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(connection) = &self.remote_connection {
            connection.close();
        }
    }

    fn flush(&mut self, deadline: Instant) -> io::Result<()> {
        // Later frames must stay after the flush command, even if its wait times out.
        self.pending_batch = None;
        let (done, completion) = mpsc::channel();
        self.sender
            .try_send(WriterCommand::Flush(done))
            .map_err(|_| queue_full())?;
        // clock-io-ok: the flush wait must account for time spent enqueueing it.
        completion
            .recv_timeout(
                Deadline::at(deadline)
                    // clock-io-ok: compute the remaining time after the flush was queued.
                    .remaining(Instant::now())
                    .unwrap_or_default(),
            )
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, "endpoint flush timed out")
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "endpoint writer stopped")
                }
            })
    }

    fn take_error(&mut self) -> Option<io::Error> {
        self.error.lock().ok()?.take()
    }
}

impl Drop for NativeEndpointTransport {
    fn drop(&mut self) {
        self.disconnect();
    }
}

fn queue_full() -> io::Error {
    // A message may already be partially written. Retrying or dropping only this message would
    // lose input ordering; revoke the connection and recover through the normal lifecycle.
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        shepr_launch::EndpointFailure::backpressure("endpoint output queue is full"),
    )
}

fn write_batch(
    writer: &mut impl io::Write,
    batch: &Mutex<FrameBatch>,
    stopped: &AtomicBool,
    queued_bytes: &AtomicUsize,
) -> io::Result<()> {
    // Claim the frames before doing any I/O. The producer never waits for socket progress.
    let batch = std::mem::take(
        &mut *batch
            .lock()
            .map_err(|_| io::Error::other("endpoint batch lock poisoned"))?,
    );
    for frame in batch.frames {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        let result = write_frame(writer, &frame, stopped);
        queued_bytes.fetch_sub(frame.len(), Ordering::AcqRel);
        result?;
    }
    Ok(())
}

fn write_frame(
    writer: &mut impl io::Write,
    mut frame: &[u8],
    stopped: &AtomicBool,
) -> io::Result<()> {
    // clock-io-ok: measure elapsed time while the worker writes a frame.
    let deadline = Deadline::after(Instant::now(), ENDPOINT_WRITE_TIMEOUT);
    while !frame.is_empty() && !stopped.load(Ordering::Acquire) {
        let chunk = frame;
        match writer.write(chunk) {
            Ok(0) => {}
            Ok(written) => {
                frame = &frame[written..];
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        // clock-io-ok: check elapsed time after a blocked frame write.
        if deadline.is_expired(Instant::now()) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "endpoint write timed out",
            ));
        }
        std::thread::sleep(ENDPOINT_IO_POLL_INTERVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A client message carrying arbitrary text, for the queue and ordering tests.
    fn paste(text: String) -> ClientMessage {
        ClientMessage::ClientShellPaneInput {
            pane_id: "w1:p1".parse().expect("test pane id"),
            events: vec![shepr_protocol::ClientPaneInputEvent::Paste(text)],
        }
    }

    fn streams() -> (LocalStream, LocalStream) {
        let path = shepr_test_support::ScratchDir::new("writer").join("s.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let accepting = std::thread::spawn(move || listener.accept().expect("test precondition").0);
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        (client, accepting.join().expect("test precondition"))
    }

    #[test]
    fn native_endpoint_writer_delivers_ordered_protocol_frames() {
        let (stream, mut peer) = streams();
        let mut transport =
            NativeEndpointTransport::with_lifetime(stream, ()).expect("test precondition");
        let (done, received) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let first: ClientMessage =
                shepr_protocol::read_message(&mut peer).expect("test precondition");
            let second: ClientMessage =
                shepr_protocol::read_message(&mut peer).expect("test precondition");
            done.send((first, second)).expect("test precondition");
        });
        transport
            .send(&ClientMessage::ClientShellFocus { focused: true })
            .expect("test precondition");
        transport
            .send(&ClientMessage::ClientShellFocus { focused: false })
            .expect("test precondition");
        let messages = received
            .recv_timeout(Duration::from_secs(3))
            .expect("test precondition");
        assert_eq!(
            messages,
            (
                ClientMessage::ClientShellFocus { focused: true },
                ClientMessage::ClientShellFocus { focused: false }
            )
        );
        reader.join().expect("test precondition");
        drop(transport);
    }

    #[test]
    fn registry_exit_flushes_queued_input_and_a_complete_detach() {
        let (stream, mut peer) = streams();
        let transport =
            NativeEndpointTransport::with_lifetime(stream, ()).expect("test precondition");
        let (done, received) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let result = (|| {
                let first: ClientMessage = shepr_protocol::read_message(&mut peer)?;
                let second: ClientMessage = shepr_protocol::read_message(&mut peer)?;
                Ok::<_, shepr_protocol::FramingError>((first, second))
            })();
            done.send(result).expect("test precondition");
        });
        let mut registry =
            crate::endpoint::EndpointRegistry::new(transport, crate::tests::test_generation(1));
        let input = paste("queued input".to_owned());
        assert_eq!(
            registry.send_to(&crate::endpoint::ClientEndpointId::Local, &input),
            crate::endpoint::EndpointSendOutcome::Sent
        );
        drop(registry);
        let (first, second) = received
            .recv_timeout(Duration::from_secs(3))
            .expect("test precondition")
            .expect("clean exit must flush complete frames before closing");
        assert_eq!(first, input);
        assert_eq!(second, ClientMessage::Detach);
        reader.join().expect("test precondition");
    }

    #[test]
    fn native_endpoint_flush_drains_large_frames_before_detach() {
        // The SSH bridge polls for available bytes instead of posting a blocking read.
        struct PollingPeer(LocalStream);
        impl io::Read for PollingPeer {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                loop {
                    match shepr_platform::ipc::poll_local_stream_read_count(&mut self.0, buffer)? {
                        shepr_platform::ipc::LocalStreamReadCount::Data(count) => return Ok(count),
                        shepr_platform::ipc::LocalStreamReadCount::Closed => return Ok(0),
                        shepr_platform::ipc::LocalStreamReadCount::Pending => {
                            std::thread::sleep(ENDPOINT_IO_POLL_INTERVAL);
                        }
                    }
                }
            }
        }
        let (stream, peer) = streams();
        peer.set_nonblocking(true).expect("test precondition");
        let mut peer = PollingPeer(peer);
        let mut transport =
            NativeEndpointTransport::with_lifetime(stream, ()).expect("test precondition");
        let (done, received) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let first: ClientMessage =
                shepr_protocol::read_message(&mut peer).expect("test precondition");
            let second: ClientMessage =
                shepr_protocol::read_message(&mut peer).expect("test precondition");
            done.send((first, second)).expect("test precondition");
        });
        let input = paste(
            // Comfortably above MAX_BATCH_BYTES, but small enough that the peer drains it
            // well within the flush deadline under CI load.
            "x".repeat(256 * 1024),
        );
        transport.send(&input).expect("test precondition");
        transport
            .send(&ClientMessage::Detach)
            .expect("test precondition");
        // Large-frame correctness must not depend on the registry's short exit grace period.
        transport
            .flush(Instant::now() + Duration::from_secs(30))
            .expect("test precondition");
        drop(transport);
        let (first, second) = received
            .recv_timeout(Duration::from_secs(30))
            .expect("test precondition");
        assert_eq!(first, input);
        assert_eq!(second, ClientMessage::Detach);
        reader.join().expect("test precondition");
    }

    #[test]
    fn native_endpoint_teardown_does_not_wait_for_a_stalled_peer() {
        struct Lifetime(mpsc::Sender<()>);
        impl Drop for Lifetime {
            fn drop(&mut self) {
                // Drop must not panic; a gone receiver means the test already failed
                // its recv_timeout, which reports the failure.
                self.0.send(()).ok();
            }
        }
        let (stream, peer) = streams();
        let (done, dropped) = mpsc::channel();
        let mut transport = NativeEndpointTransport::with_lifetime(stream, Lifetime(done))
            .expect("test precondition");
        transport
            .send(&paste(
                // Large enough to overrun the socket buffer, small enough to fit one frame.
                "x".repeat(shepr_protocol::MAX_FRAME_SIZE - 64),
            ))
            .expect("test precondition");
        drop(transport);
        dropped
            .recv_timeout(Duration::from_secs(3))
            .expect("worker lifetime is released without peer reads");
        drop(peer);
    }

    #[test]
    fn partial_writes_preserve_frame_bytes() {
        struct PartialWriter(Vec<u8>);
        impl io::Write for PartialWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let count = bytes.len().min(3);
                self.0.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = PartialWriter(Vec::new());
        write_frame(&mut writer, b"first frame", &AtomicBool::new(false))
            .expect("test precondition");
        write_frame(&mut writer, b"second frame", &AtomicBool::new(false))
            .expect("test precondition");
        assert_eq!(writer.0, b"first framesecond frame");
    }

    #[test]
    fn a_stalled_write_is_cancellable_without_peer_progress() {
        struct StalledWriter(mpsc::Sender<()>);
        impl io::Write for StalledWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                // The writer retries until stopped and the test awaits only the first
                // attempt, so a retry that outlives the test's receiver tells no one.
                self.0.send(()).ok();
                Err(io::ErrorKind::WouldBlock.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (attempted, attempts) = mpsc::channel();
        let (done, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = write_frame(&mut StalledWriter(attempted), b"input", &worker_stop);
            done.send(result).expect("test precondition");
        });
        attempts
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition");
        stop.store(true, Ordering::Release);
        completion
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition")
            .expect("test precondition");
        worker.join().expect("test precondition");
    }

    fn queued_transport(
        capacity: usize,
    ) -> (NativeEndpointTransport, mpsc::Receiver<WriterCommand>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        (
            NativeEndpointTransport {
                sender,
                pending_batch: None,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
                stopped: Arc::new(AtomicBool::new(false)),
                error: Arc::new(Mutex::new(None)),
                read_activity: Arc::new(EndpointReadActivity::new(Instant::now())),
                remote_connection: None,
            },
            receiver,
        )
    }

    #[test]
    fn stdin_burst_is_queued_in_order_without_worker_progress() {
        let (mut transport, receiver) = queued_transport(MAX_QUEUED_BATCHES);
        let mut expected = Vec::new();
        for index in 0..128 {
            let message = paste(format!("{index:04}: ordered input burst\n"));
            shepr_protocol::write_message(&mut expected, &message).expect("test precondition");
            transport
                .send(&message)
                .expect("a small stdin burst must fit");
        }
        let queued_bytes = Arc::clone(&transport.queued_bytes);
        assert_eq!(queued_bytes.load(Ordering::Acquire), expected.len());
        let mut received = Vec::new();
        for command in receiver.try_iter() {
            let WriterCommand::Frames(batch) = command else {
                panic!("unexpected flush");
            };
            assert!(batch.lock().expect("test precondition").bytes <= MAX_BATCH_BYTES);
            write_batch(&mut received, &batch, &transport.stopped, &queued_bytes)
                .expect("test precondition");
        }
        assert_eq!(received, expected);
        assert_eq!(queued_bytes.load(Ordering::Acquire), 0);

        // The producer still holds the last claimed batch; new input must get a new command.
        transport
            .send(&ClientMessage::Detach)
            .expect("test precondition");
        let WriterCommand::Frames(batch) = receiver.try_recv().expect("test precondition") else {
            panic!("expected a new batch after the worker claimed the previous one");
        };
        write_batch(&mut received, &batch, &transport.stopped, &queued_bytes)
            .expect("test precondition");
        shepr_protocol::write_message(&mut expected, &ClientMessage::Detach)
            .expect("test precondition");
        assert_eq!(received, expected);
        assert_eq!(queued_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn later_frames_cannot_join_a_batch_before_a_flush() {
        let (mut transport, receiver) = queued_transport(3);
        let first = ClientMessage::ClientShellFocus { focused: true };
        let last = ClientMessage::Detach;
        transport.send(&first).expect("test precondition");
        assert_eq!(
            transport
                .flush(Instant::now())
                .expect_err("test precondition")
                .kind(),
            io::ErrorKind::TimedOut
        );
        transport.send(&last).expect("test precondition");

        let mut received = Vec::new();
        let WriterCommand::Frames(batch) = receiver.try_recv().expect("test precondition") else {
            panic!("expected first batch");
        };
        write_batch(
            &mut received,
            &batch,
            &transport.stopped,
            &transport.queued_bytes,
        )
        .expect("test precondition");
        let mut expected = Vec::new();
        shepr_protocol::write_message(&mut expected, &first).expect("test precondition");
        assert_eq!(received, expected);
        assert!(matches!(
            receiver.try_recv().expect("test precondition"),
            WriterCommand::Flush(_)
        ));
        let WriterCommand::Frames(batch) = receiver.try_recv().expect("test precondition") else {
            panic!("expected a separate batch after flush");
        };
        write_batch(
            &mut received,
            &batch,
            &transport.stopped,
            &transport.queued_bytes,
        )
        .expect("test precondition");
        shepr_protocol::write_message(&mut expected, &last).expect("test precondition");
        assert_eq!(received, expected);
        assert_eq!(transport.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_queued() {
        let (mut transport, receiver) = queued_transport(MAX_QUEUED_BATCHES);
        let error = transport
            .send(&paste("x".repeat(shepr_protocol::MAX_FRAME_SIZE)))
            .expect_err("a frame over the cap must not be sent");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(transport.queued_bytes.load(Ordering::Acquire), 0);
        assert!(receiver.try_recv().is_err(), "nothing reached the writer");
    }

    #[test]
    fn a_full_queue_is_a_connection_failure_not_silent_input_loss() {
        let (mut transport, _receiver) = queued_transport(1);
        transport
            .send(&paste("x".repeat(MAX_BATCH_BYTES)))
            .expect("test precondition");
        let queued = transport.queued_bytes.load(Ordering::Acquire);
        assert_eq!(
            transport
                .send(&ClientMessage::Detach)
                .expect_err("test precondition")
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(transport.queued_bytes.load(Ordering::Acquire), queued);
        transport
            .queued_bytes
            .store(MAX_QUEUED_BYTES, Ordering::Release);
        assert_eq!(
            transport
                .send(&ClientMessage::Detach)
                .expect_err("test precondition")
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
    }
}
