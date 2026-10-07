use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

static CLIPBOARD_ROUTE: OnceLock<shepr_platform::ClipboardRoute> = OnceLock::new();

pub(super) fn set_clipboard_route(route: shepr_platform::ClipboardRoute) {
    // One client loop owns the process. A repeated set is expected in launch
    // tests that build more than one loop; the first loop's route remains the
    // process route, matching the once-at-launch environment decision.
    if CLIPBOARD_ROUTE.set(route).is_err() {
        tracing::debug!("clipboard route already set; keeping the first");
    }
}

pub(super) fn clipboard_route() -> shepr_platform::ClipboardRoute {
    *CLIPBOARD_ROUTE.get_or_init(shepr_platform::ClipboardRoute::from_env)
}

/// A host writer shared by the event loop and the clipboard worker. Each write
/// is serialized as a whole so an OSC 52 sequence cannot be interleaved with a
/// frame or a host mode sequence.
#[derive(Clone)]
pub(super) struct SharedHostWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl SharedHostWriter {
    pub(super) fn new(writer: impl Write + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(writer))))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Box<dyn Write + Send>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Write for SharedHostWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.lock().write_all(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.lock().write_all(bytes)
    }

    fn write_fmt(&mut self, fmt: std::fmt::Arguments<'_>) -> io::Result<()> {
        self.lock().write_fmt(fmt)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock().flush()
    }
}

#[derive(Default)]
struct PendingClipboardWrite {
    bytes: Option<Vec<u8>>,
    stopping: bool,
    terminal_write_active: bool,
}

/// Server clipboard writes must not wait for clipboard owner processes on the
/// event loop. While a write is active, one pending copy is kept; a newer copy
/// replaces it so a burst cannot build an unbounded queue of stale selections.
pub(super) struct ClipboardWriteWorker {
    pending: Arc<(Mutex<PendingClipboardWrite>, Condvar)>,
}

impl ClipboardWriteWorker {
    pub(super) fn new(
        route: shepr_platform::ClipboardRoute,
        mut writer: SharedHostWriter,
    ) -> io::Result<Self> {
        let pending = Arc::new((Mutex::new(PendingClipboardWrite::default()), Condvar::new()));
        let worker_pending = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("clip-write".into())
            .spawn(move || {
                let mut last_error = None;
                loop {
                    let Some(bytes) = take_pending_clipboard_write(&worker_pending) else {
                        return;
                    };
                    match write_server_clipboard(&bytes, route, &mut writer, &worker_pending) {
                        Ok(()) => last_error = None,
                        Err(error) => {
                            let kind = error.kind();
                            if last_error != Some(kind) {
                                shepr_platform::structured_log!(
                                    WARN,
                                    event = clipboard.copy,
                                    outcome = Error,
                                    bytes = bytes.len(),
                                    error_kind = ?kind,
                                    "clipboard copy from the server did not reach the host clipboard"
                                );
                                last_error = Some(kind);
                            }
                        }
                    }
                }
            })?;
        Ok(Self { pending })
    }

    pub(super) fn submit(&self, bytes: Vec<u8>) {
        let (pending, wake) = &*self.pending;
        let mut pending = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.stopping {
            return;
        }
        pending.bytes = Some(bytes);
        wake.notify_one();
    }
}

impl Drop for ClipboardWriteWorker {
    fn drop(&mut self) {
        let (pending, wake) = &*self.pending;
        let mut pending = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.stopping = true;
        pending.bytes = None;
        wake.notify_one();
        while pending.terminal_write_active {
            pending = wake
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

fn take_pending_clipboard_write(
    pending: &Arc<(Mutex<PendingClipboardWrite>, Condvar)>,
) -> Option<Vec<u8>> {
    let (state, wake) = &**pending;
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if state.stopping {
            return None;
        }
        if let Some(bytes) = state.bytes.take() {
            return Some(bytes);
        }
        state = wake
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn write_server_clipboard(
    bytes: &[u8],
    route: shepr_platform::ClipboardRoute,
    writer: &mut impl Write,
    pending: &Arc<(Mutex<PendingClipboardWrite>, Condvar)>,
) -> io::Result<()> {
    if route.write_with_helpers(bytes) {
        return Ok(());
    }
    // A helper may have occupied its full deadline while the client was
    // shutting down. Do not write a late OSC 52 sequence after terminal restore.
    {
        let (state, _) = &**pending;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopping {
            return Ok(());
        }
        state.terminal_write_active = true;
    }
    let active_write = ActiveTerminalWrite(Arc::clone(pending));
    let result = forward_clipboard(bytes, shepr_platform::ClipboardRoute::Osc52, writer);
    drop(active_write);
    result
}

struct ActiveTerminalWrite(Arc<(Mutex<PendingClipboardWrite>, Condvar)>);

impl Drop for ActiveTerminalWrite {
    fn drop(&mut self) {
        let (state, wake) = &*self.0;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.terminal_write_active = false;
        wake.notify_all();
    }
}

/// Writes clipboard bytes from the server to the host clipboard.
/// The server sends bytes decoded from OSC 52 by its terminal parser.
pub(super) fn forward_clipboard(
    data: &[u8],
    route: shepr_platform::ClipboardRoute,
    writer: &mut impl io::Write,
) -> io::Result<()> {
    shepr_termio::host_term::title::write_clipboard_bytes(data, route, writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn forward_clipboard_writes_osc52_to_the_supplied_test_sink() {
        let mut output = Vec::new();
        forward_clipboard(b"test", shepr_platform::ClipboardRoute::Osc52, &mut output)
            .expect("clipboard bytes are written through OSC 52");
        assert_eq!(output, b"\x1b]52;c;dGVzdA==\x07\x1b]52;p;dGVzdA==\x07");
    }

    #[test]
    fn server_clipboard_worker_keeps_only_the_latest_pending_copy() {
        #[derive(Default)]
        struct GateState {
            output: Vec<u8>,
            started: bool,
            released: bool,
            writes: usize,
        }

        struct GateWriter(Arc<(Mutex<GateState>, Condvar)>);

        impl Write for GateWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let (state, wake) = &*self.0;
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.writes += 1;
                if state.writes == 1 {
                    state.started = true;
                    wake.notify_all();
                    while !state.released {
                        state = wake
                            .wait(state)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
                state.output.extend_from_slice(bytes);
                wake.notify_all();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let shared = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
        let worker = ClipboardWriteWorker::new(
            shepr_platform::ClipboardRoute::Osc52,
            SharedHostWriter::new(GateWriter(Arc::clone(&shared))),
        )
        .expect("clipboard writer thread starts");
        worker.submit(b"active".to_vec());

        let (state, wake) = &*shared;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.started {
            state = wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(state);

        worker.submit(b"superseded".to_vec());
        worker.submit(b"latest".to_vec());

        let mut state = shared
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.released = true;
        wake.notify_all();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while state.writes < 2 && std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let (next, _) = wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
        let output = String::from_utf8(state.output.clone()).expect("OSC 52 is ASCII");
        drop(state);
        drop(worker);

        assert!(output.contains("YWN0aXZl"));
        assert!(output.contains("bGF0ZXN0"));
        assert!(!output.contains("c3VwZXJzZWRlZA=="));
    }
}
