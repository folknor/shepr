use std::io;
use std::path::Path;
use std::time::Duration;

use crate::schema::{Method, Request, ResponseResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub version: Option<String>,
    pub protocol: Option<u32>,
    pub build_id: String,
    pub capabilities: Option<crate::schema::ServerCapabilities>,
}

pub fn read_runtime_status_at(
    socket_path: &Path,
    timeout: Duration,
) -> io::Result<Option<RuntimeStatus>> {
    if !socket_path.exists() {
        return Ok(None);
    }

    let client = crate::client::ApiClient::for_target(crate::client::ConnectionTarget::SocketPath(
        socket_path.to_path_buf(),
    ));
    let request = Request {
        id: "runtime:status".into(),
        method: Method::Ping(crate::schema::PingParams::default()),
    };
    let response = client
        .request_value_with_timeout(&request, timeout)
        .and_then(crate::client::parse_response_value);
    let response = match response {
        Ok(response) => response,
        // A stalled server (one that accepts but never answers) reads as "no
        // usable status", the same as nothing listening: callers turn `None`
        // into "status API unavailable" guidance. A receive timeout on this
        // socket is `EAGAIN`, i.e. `WouldBlock`; the client normalizes that to
        // `TimedOut`, and both are accepted here so the mapping never depends
        // on which layer reported it.
        Err(crate::client::ApiClientError::Io(err))
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
            build_id,
            capabilities,
        } => Ok(Some(RuntimeStatus {
            version: Some(version),
            protocol: Some(protocol),
            build_id,
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
        let scratch = shepr_test_support::ScratchDir::new("status");
        let path = scratch.join("stalled.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
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
        assert!(
            matches!(status, Ok(None)),
            "a stalled server must read as no status: {status:?}"
        );
    }
}
