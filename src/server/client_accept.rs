use std::io;
use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::ipc::LocalListener;
use crate::server::client_transport::{self, ServerEvent};
use crate::server::clients::ClientRegistry;

/// Accepts pending thin-client connections and starts their handshake readers.
pub(crate) fn accept_pending_client_connections(
    listener: &LocalListener,
    clients: &mut ClientRegistry,
    should_quit: &Arc<AtomicBool>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }
        match listener.accept() {
            Ok(stream) => {
                // The socket file is owner-only; this is the second check,
                // for a socket whose mode was loosened or a path bound in a
                // shared directory. Dropping the stream closes it.
                match crate::ipc::peer_is_same_user(&stream) {
                    Ok(true) => {}
                    Ok(false) => {
                        warn!("client connection from another user refused");
                        continue;
                    }
                    Err(err) => {
                        warn!(err = %err, "client peer credentials unavailable; refused");
                        continue;
                    }
                }
                let client_id = clients.allocate_client_id();

                if let Err(err) = stream.set_nonblocking(true) {
                    warn!(err = %err, "failed to set client stream nonblocking");
                    continue;
                }

                let should_quit = Arc::clone(should_quit);
                let server_event_tx = server_event_tx.clone();
                std::thread::spawn(move || {
                    if let Err(err) = client_transport::handle_client_handshake(
                        stream,
                        client_id,
                        &server_event_tx,
                        &should_quit,
                    ) {
                        debug!(?client_id, err = %err, "client handshake failed");
                    }
                });
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener accept failed");
                break;
            }
        }
    }

    Ok(())
}
