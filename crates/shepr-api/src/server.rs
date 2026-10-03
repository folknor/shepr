use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, info, warn};

use crate::ApiRequestSender;
use shepr_platform::ipc::SocketStartupLock;

mod api_service;
mod client_protocol;
mod listener;
pub use client_protocol::{
    ClientGate, ClientHandshakeOutcome, ClientHandshakeSilence, ClientProtocolHandler,
    ConnectionSlot, read_client_handshake,
};

pub struct ServerHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    socket_file: shepr_platform::ipc::OwnedSocketFile,
    running: Arc<AtomicBool>,
    gate: ClientGate,
    // Declared last so it is released only after `drop` has removed the
    // socket file and joined the listener: a racing server cannot claim the
    // path while this one still owns it.
    _startup_lock: SocketStartupLock,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);

        // The listener thread only looks at `running` after an accept returns,
        // so without a wake-up it would sit in `accept` holding the listening
        // fd until the process exits. Connect to it once while the socket file
        // is still ours; the accept returns, the thread sees `running` false
        // and exits, dropping the listener. Only then is joining safe.
        let woke = self.wake_listener();

        if let Err(err) = self.remove_socket_file_if_owned()
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), error = %err, "failed to remove server socket on shutdown");
        }

        if let Some(thread) = self.thread.take() {
            if woke {
                // Bounded by one accept-failure backoff (at most a second).
                if thread.join().is_err() {
                    warn!("server listener thread panicked");
                }
            } else {
                debug!("server listener not woken; leaving its thread to process exit");
            }
        }
    }
}

impl ServerHandle {
    /// The gate through which the server installs its TUI protocol once its
    /// panes are restored; until then `ping` answers `starting`.
    pub fn client_gate(&self) -> ClientGate {
        self.gate.clone()
    }

    pub fn remove_socket_file_if_owned(&self) -> std::io::Result<()> {
        self.socket_file.remove_if_still_ours()
    }

    /// Unblocks the listener's `accept` with a throwaway connection. Skipped
    /// when the socket path no longer names this listener's socket (removed,
    /// or replaced by another server), since connecting would then reach
    /// somebody else. Returns whether the wake-up connection was made.
    fn wake_listener(&self) -> bool {
        let ours = self.socket_file.is_still_ours();
        if !ours {
            return false;
        }
        match shepr_platform::ipc::connect_local_stream(&self.path) {
            Ok(_stream) => true,
            Err(err) => {
                debug!(error = %err, "could not wake server listener for shutdown");
                false
            }
        }
    }
}

/// Binds the server socket and starts its listener. `boot_id` is the boot of
/// the server lifetime this socket belongs to: `ping` reports it and
/// `server.stop_if_boot` compares against it, answered on the connection
/// thread without reaching the server loop.
pub fn start_server(
    api_tx: ApiRequestSender,
    server_stop: Arc<crate::ServerStopSignal>,
    paths: &shepr_paths::AppPaths,
    boot_id: shepr_protocol::BootId,
) -> Result<ServerHandle, shepr_platform::ipc::BindError> {
    let path = paths.server_address().socket().to_path_buf();
    let (listener, socket_file, startup_lock) =
        shepr_platform::ipc::bind_owned_private_socket(paths.server_address().socket_path())?
            .into_parts();
    info!(path = %path.display(), "server socket listening");
    let running = Arc::new(AtomicBool::new(true));
    let gate = ClientGate::default();
    // Nothing restarts the listener, and a dead one leaves a server that
    // answers neither the CLI, agent hooks nor TUI attaches: it must outlive
    // every accept and spawn failure.
    let thread = listener::start_listener(
        listener,
        Arc::clone(&running),
        api_tx,
        server_stop,
        boot_id,
        gate.clone(),
    )?;
    Ok(ServerHandle {
        thread: Some(thread),
        path,
        socket_file,
        running,
        gate,
        _startup_lock: startup_lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_platform::ipc::{bind_private_socket, remove_socket_file_if_owned};
    use shepr_test_support::ScratchDir;
    use tokio::sync::mpsc;

    #[test]
    fn dropping_the_handle_stops_the_listener_thread() {
        let path = ScratchDir::new("listener-drop").join("s");
        let socket_path = shepr_platform::ipc::SocketPath::new(path.clone()).expect("socket path");
        let (listener, socket_file, startup_lock) =
            shepr_platform::ipc::bind_owned_private_socket(&socket_path)
                .expect("bind")
                .into_parts();
        let running = Arc::new(AtomicBool::new(true));
        let gate = ClientGate::default();
        let (tx, _rx) = mpsc::channel(1);
        let thread = listener::start_listener(
            listener,
            Arc::clone(&running),
            tx,
            Arc::default(),
            shepr_protocol::BootId::from_process_clock(1, Ok(std::time::Duration::ZERO)),
            gate.clone(),
        )
        .expect("listener thread");
        let alive = Arc::clone(&running);
        let handle = ServerHandle {
            thread: Some(thread),
            path: path.clone(),
            socket_file,
            running,
            gate,
            _startup_lock: startup_lock,
        };
        let refusal = bind_private_socket(&path).err().expect("path stays locked");
        let shepr_platform::ipc::BindError::Busy(busy) = refusal else {
            panic!("expected a busy socket")
        };
        assert_eq!(busy.path(), path);
        drop(handle);
        assert_eq!(Arc::strong_count(&alive), 1, "listener has exited");
        assert!(!path.try_exists().expect("socket removed"));
        let (_listener, _lock, identity) =
            bind_private_socket(&path).expect("released lock and listener");
        remove_socket_file_if_owned(&path, &identity).expect("cleanup");
    }
}
