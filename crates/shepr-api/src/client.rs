use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use serde::de::DeserializeOwned;

use crate::limits::{
    ORDINARY_RESPONSE_TIMEOUT, UNBOUNDED_RESPONSE_SEND_TIMEOUT, WAIT_RESPONSE_GRACE,
};
use crate::schema::{ErrorResponse, Method, PingParams, Request, ResponseResult, SuccessResponse};
use shepr_platform::ipc::LocalStream;

/// Reusable client for Shepr's newline-delimited JSON API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    socket_path: PathBuf,
}

impl ApiClient {
    pub fn local(paths: &shepr_config::AppPaths) -> Self {
        Self::for_socket(crate::socket_path(paths))
    }

    /// A client for the API socket at `socket_path`, resolved by the caller
    /// at the process edge.
    pub fn for_socket(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.socket_path.clone()
    }

    pub fn request(&self, request: &Request) -> Result<SuccessResponse, ApiClientError> {
        let value = self.request_value(request)?;
        parse_response_value(value)
    }

    /// Sends one request and reads its single-line response.
    ///
    /// The response wait is bounded by [`response_timeout`]: ordinary requests
    /// get a little longer than the server's own bound, and wait methods get
    /// their own `timeout_ms` plus a grace period, or no bound when they were
    /// sent without one. A timeout surfaces as `ErrorKind::TimedOut`.
    /// The initial request write stays bounded by the server's request-line deadline.
    pub fn request_value(&self, request: &Request) -> Result<serde_json::Value, ApiClientError> {
        if let Some(timeout) = response_timeout(request) {
            return self.request_value_with_timeout(request, timeout);
        }
        let mut stream = self.connect()?;
        stream.set_send_timeout(Some(UNBOUNDED_RESPONSE_SEND_TIMEOUT))?;
        write_request(&mut stream, request).map_err(normalize_socket_timeout)?;

        let mut reader = BufReader::new(stream);
        read_json_line(&mut reader)
    }

    /// Like [`Self::request_value`] with an explicit bound. The bound is an
    /// send timeout for writing and one overall deadline for reading. A timeout
    /// surfaces as `ErrorKind::TimedOut`, even if the server trickles out a
    /// partial response.
    pub fn request_value_with_timeout(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiClientError> {
        let mut stream = self.connect()?;
        stream.set_send_timeout(Some(timeout))?;
        write_request(&mut stream, request).map_err(normalize_socket_timeout)?;

        let deadline = deadline_after(timeout)?;
        let mut reader = BufReader::new(shepr_platform::ipc::DeadlineReader::new(
            &mut stream,
            deadline,
        ));
        read_json_line(&mut reader).map_err(normalize_socket_timeout)
    }

    pub(crate) fn request_value_until(
        &self,
        request: &Request,
        deadline: Instant,
    ) -> Result<serde_json::Value, ApiClientDeadlineError> {
        let mut stream = self.connect().map_err(ApiClientDeadlineError::Connect)?;
        let send_timeout = deadline.saturating_duration_since(Instant::now());
        if send_timeout.is_zero() {
            return Err(ApiClientDeadlineError::Request(ApiClientError::Io(
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the shepr server to respond",
                ),
            )));
        }
        // Some local socket wrappers reject SO_SNDTIMEO; the deadline reader
        // still bounds response reads in that case.
        if let Err(error) = stream.set_send_timeout(Some(send_timeout))
            && error.kind() != io::ErrorKind::InvalidInput
        {
            return Err(ApiClientDeadlineError::Request(error.into()));
        }
        write_request(&mut stream, request)
            .map_err(normalize_socket_timeout)
            .map_err(ApiClientDeadlineError::Request)?;

        let mut reader = BufReader::new(shepr_platform::ipc::DeadlineReader::new(
            &mut stream,
            deadline,
        ));
        read_json_line(&mut reader)
            .map_err(normalize_socket_timeout)
            .map_err(ApiClientDeadlineError::Request)
    }

    pub fn status(&self) -> Result<crate::RuntimeStatus, ApiClientError> {
        self.read_status(None)
    }

    pub fn status_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<crate::RuntimeStatus, ApiClientError> {
        self.read_status(Some(timeout))
    }

    fn read_status(
        &self,
        timeout: Option<Duration>,
    ) -> Result<crate::RuntimeStatus, ApiClientError> {
        let request = Request {
            id: "api-client:status".into(),
            method: Method::Ping(PingParams::default()),
        };
        let response = match timeout {
            Some(timeout) => {
                parse_response_value(self.request_value_with_timeout(&request, timeout)?)?
            }
            None => self.request(&request)?,
        };
        match response.result {
            ResponseResult::Pong {
                version,
                build_id,
                capabilities,
            } => Ok(crate::RuntimeStatus {
                version: Some(version),
                build_id,
                capabilities,
            }),
            result => Err(ApiClientError::UnexpectedResult(format!("{result:?}"))),
        }
    }

    fn connect(&self) -> io::Result<LocalStream> {
        shepr_platform::ipc::connect_local_stream(&self.socket_path)
    }
}

fn deadline_after(timeout: Duration) -> io::Result<Instant> {
    Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "api response timeout is too large",
        )
    })
}

/// How long [`ApiClient::request_value`] waits for a response, or `None` for
/// no bound.
///
/// Wait methods run as long as the caller asked: their own timeout plus
/// the wait response grace, or unbounded when sent without one.
/// Everything else, including the acknowledgement of events.subscribe, is ordinary.
pub(crate) fn response_timeout(request: &Request) -> Option<Duration> {
    let wait_bound = |timeout_ms: Option<u64>| {
        timeout_ms.map(|ms| Duration::from_millis(ms).saturating_add(WAIT_RESPONSE_GRACE))
    };
    match &request.method {
        Method::EventsWait(params) => wait_bound(params.timeout_ms),
        Method::PaneWaitForOutput(params) => wait_bound(params.timeout_ms),
        _ => Some(ORDINARY_RESPONSE_TIMEOUT),
    }
}

/// `SO_RCVTIMEO`/`SO_SNDTIMEO` expiry is reported as `EAGAIN`, which std maps
/// to `WouldBlock`; callers decide "stalled server" on `TimedOut` alone.
fn normalize_socket_timeout(error: ApiClientError) -> ApiClientError {
    match error {
        ApiClientError::Io(error) if error.kind() == io::ErrorKind::WouldBlock => {
            ApiClientError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for the shepr server to respond",
            ))
        }
        error => error,
    }
}

#[derive(Debug)]
pub enum ApiClientError {
    Io(io::Error),
    Json(serde_json::Error),
    ErrorResponse(ErrorResponse),
    /// No response bytes; session shutdown treats a server that closes after
    /// receiving its stop request as having completed the request.
    EmptyResponse,
    /// A successful response with a result variant other than the one asked for.
    /// Keep it distinct from transport and JSON errors so callers do not
    /// classify a decoded protocol mismatch as server unavailability.
    UnexpectedResult(String),
}

pub(crate) enum ApiClientDeadlineError {
    Connect(io::Error),
    Request(ApiClientError),
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::ErrorResponse(response) => write!(f, "{}", response.error.message),
            Self::EmptyResponse => write!(f, "empty api response"),
            Self::UnexpectedResult(result) => write!(f, "unexpected api result: {result}"),
        }
    }
}

impl std::error::Error for ApiClientError {}

impl From<io::Error> for ApiClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for ApiClientError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn write_request(stream: &mut LocalStream, request: &Request) -> Result<(), ApiClientError> {
    stream.write_all(serde_json::to_string(request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_json_line<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<T, ApiClientError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 || line.trim().is_empty() {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_str(&line).map_err(ApiClientError::Json)
}

pub fn parse_response_value(value: serde_json::Value) -> Result<SuccessResponse, ApiClientError> {
    if value.get("error").is_some() {
        let response: ErrorResponse = serde_json::from_value(value)?;
        Err(ApiClientError::ErrorResponse(response))
    } else {
        Ok(serde_json::from_value(value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_session_target_resolves_named_session_socket() {
        let env = shepr_test_support::IsolatedEnv::new();
        env.set(shepr_core::env::EnvVar::SheprSession, "work");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
        let client = ApiClient::local(&paths);
        let socket = client.socket_path();
        assert!(socket.ends_with("sessions/work/shepr.sock"), "{socket:?}");
        assert!(socket.starts_with(paths.runtime_dir()), "{socket:?}");
    }

    #[test]
    fn status_timeout_closes_a_stalled_probe() {
        use interprocess::local_socket::traits::Listener as _;
        let scratch = shepr_test_support::ScratchDir::new("status-timeout");
        let path = scratch.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("test precondition");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).expect("test precondition")["method"],
                "ping"
            );
            std::thread::sleep(Duration::from_millis(300));
        });
        let client = ApiClient::for_socket(path.clone());
        let error = client
            .status_with_timeout(Duration::from_millis(100))
            .expect_err("test precondition");
        assert!(matches!(
            error,
            ApiClientError::Io(error) if error.kind() == io::ErrorKind::TimedOut
        ));
        server.join().expect("test precondition");
        std::fs::remove_file(path).expect("test precondition");
    }

    #[test]
    fn request_timeout_on_a_stalled_server_is_reported_as_timed_out() {
        use interprocess::local_socket::traits::Listener as _;
        let scratch = shepr_test_support::ScratchDir::new("request-timeout");
        let path = scratch.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("test precondition");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the test releases the stalled connection");
        });
        let client = ApiClient::for_socket(path.clone());
        let request = Request {
            id: "stalled".into(),
            method: Method::WorkspaceList(crate::schema::EmptyParams::default()),
        };
        let error = client
            .request_value_with_timeout(&request, Duration::from_millis(100))
            .expect_err("test precondition");
        release_tx
            .send(())
            .expect("the stalled server is still holding the connection");
        server.join().expect("test precondition");
        std::fs::remove_file(path).expect("test precondition");
        assert!(
            matches!(&error, ApiClientError::Io(error) if error.kind() == io::ErrorKind::TimedOut),
            "{error:?}"
        );
    }

    #[test]
    fn partial_responses_cannot_extend_the_response_deadline() {
        use interprocess::local_socket::traits::Listener as _;
        let scratch = shepr_test_support::ScratchDir::new("partial-response");
        let path = scratch.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("test precondition");
            let mut stream = reader.into_inner();
            for _ in 0..4 {
                if stream.write_all(b"{").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(70));
            }
        });
        let client = ApiClient::for_socket(path.clone());
        let request = Request {
            id: "partial".into(),
            method: Method::WorkspaceList(crate::schema::EmptyParams::default()),
        };
        let error = client
            .request_value_with_timeout(&request, Duration::from_millis(120))
            .expect_err("partial JSON must time out");
        server.join().expect("test precondition");
        std::fs::remove_file(path).expect("test precondition");
        assert!(
            matches!(&error, ApiClientError::Io(error) if error.kind() == io::ErrorKind::TimedOut),
            "{error:?}"
        );
    }

    #[test]
    fn response_timeout_bounds_ordinary_requests_and_follows_wait_timeouts() {
        let request = |method| Request {
            id: "timeout".into(),
            method,
        };
        let ordinary = response_timeout(&request(Method::WorkspaceList(
            crate::schema::EmptyParams::default(),
        )))
        .expect("ordinary requests are bounded");
        assert!(ordinary > crate::limits::ORDINARY_REQUEST_TIMEOUT);

        let wait = |timeout_ms| {
            request(Method::EventsWait(crate::schema::EventsWaitParams {
                match_event: crate::schema::EventMatch::PaneAgentStatusChanged {
                    pane_id: "pane".into(),
                    agent_status: crate::schema::AgentStatus::Idle,
                },
                timeout_ms,
            }))
        };
        assert_eq!(response_timeout(&wait(None)), None);
        let bounded = response_timeout(&wait(Some(600_000))).expect("bounded wait");
        assert!(bounded > Duration::from_secs(600));
    }

    #[test]
    fn an_unbounded_request_times_out_sending_to_a_server_that_never_reads() {
        use interprocess::local_socket::traits::Listener as _;
        let scratch = shepr_test_support::ScratchDir::new("send-timeout");
        let path = scratch.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            // Holds the connection open without reading a byte.
            let _stream = listener.accept().expect("test precondition");
            release_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("the test releases the stalled connection");
        });
        let client = ApiClient::for_socket(path.clone());
        // A wait without a timeout has no response bound, and a body far
        // larger than the socket buffers blocks the write once they fill.
        let request = Request {
            id: "unread".into(),
            method: Method::EventsWait(crate::schema::EventsWaitParams {
                match_event: crate::schema::EventMatch::PaneAgentStatusChanged {
                    pane_id: "p".repeat(8 * 1024 * 1024),
                    agent_status: crate::schema::AgentStatus::Idle,
                },
                timeout_ms: None,
            }),
        };
        assert_eq!(response_timeout(&request), None);
        let started = Instant::now();
        let error = client
            .request_value(&request)
            .expect_err("a server that never reads must not hang the client");
        let elapsed = started.elapsed();
        release_tx
            .send(())
            .expect("the stalled server is still holding the connection");
        server.join().expect("test precondition");
        std::fs::remove_file(path).expect("test precondition");
        assert!(
            matches!(&error, ApiClientError::Io(error) if error.kind() == io::ErrorKind::TimedOut),
            "{error:?}"
        );
        // The send timeout bounds each blocked write, and a stalled peer
        // leaves at most one partial write before the final one times out.
        assert!(elapsed < UNBOUNDED_RESPONSE_SEND_TIMEOUT * 3, "{elapsed:?}");
    }

    #[test]
    fn socket_path_target_uses_explicit_path() {
        let path = PathBuf::from("/tmp/shepr-test.sock");
        let client = ApiClient::for_socket(path.clone());
        assert_eq!(client.socket_path(), path);
    }
}
