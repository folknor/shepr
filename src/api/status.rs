use std::io;
use std::path::Path;
use std::time::Duration;

use crate::api::schema::{Method, Request, ResponseResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub version: Option<String>,
    pub protocol: Option<u32>,
    pub capabilities: Option<crate::api::schema::ServerCapabilities>,
}

pub fn read_runtime_status_at(
    socket_path: &Path,
    timeout: Duration,
) -> io::Result<Option<RuntimeStatus>> {
    if !socket_path.exists() {
        return Ok(None);
    }

    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(socket_path.to_path_buf()),
    );
    let request = Request {
        id: "runtime:status".into(),
        method: Method::Ping(crate::api::schema::PingParams::default()),
    };
    let response = client
        .request_value_with_timeout(&request, timeout)
        .and_then(crate::api::client::parse_response_value);
    let response = match response {
        Ok(response) => response,
        // A stalled server (one that accepts but never answers) reads as "no
        // usable status", the same as nothing listening: callers turn `None`
        // into "status API unavailable" guidance. A receive timeout on this
        // socket is `EAGAIN`, i.e. `WouldBlock`; the client normalizes that to
        // `TimedOut`, and both are accepted here so the mapping never depends
        // on which layer reported it.
        Err(crate::api::client::ApiClientError::Io(err))
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(io::Error::other(err)),
    };
    match response.result {
        ResponseResult::Pong {
            version,
            protocol,
            capabilities,
        } => Ok(Some(RuntimeStatus {
            version: Some(version),
            protocol: Some(protocol),
            capabilities,
        })),
        result => Err(io::Error::other(format!(
            "server status request returned unexpected result: {result:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::{BufRead as _, BufReader};

    #[test]
    fn stalled_server_reports_no_status_instead_of_an_error() {
        let path =
            std::env::temp_dir().join(format!("shepr-status-stalled-{}.sock", std::process::id()));
        let listener = crate::ipc::bind_private_local_listener(&path).expect("test precondition");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("test precondition");
            // Hold the connection open without answering.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });

        let status = read_runtime_status_at(&path, Duration::from_millis(100));
        let _ = release_tx.send(());
        server.join().expect("test precondition");
        std::fs::remove_file(&path).expect("test precondition");
        assert!(
            matches!(status, Ok(None)),
            "a stalled server must read as no status: {status:?}"
        );
    }
}
