use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use serde::de::DeserializeOwned;

use crate::schema::{ErrorResponse, Method, PingParams, Request, ResponseResult, SuccessResponse};
use shepr_platform::ipc::LocalStream;

/// API connection target resolved by clients at the process edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTarget {
    SocketPath(PathBuf),
}

impl ConnectionTarget {
    fn socket_path(&self) -> PathBuf {
        match self {
            Self::SocketPath(path) => path.clone(),
        }
    }
}

/// Reusable client for Shepr's newline-delimited JSON API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    target: ConnectionTarget,
}

impl ApiClient {
    pub fn local(paths: &shepr_config::AppPaths) -> Self {
        Self::for_target(ConnectionTarget::SocketPath(crate::socket_path(paths)))
    }

    pub fn for_target(target: ConnectionTarget) -> Self {
        Self { target }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.target.socket_path()
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
    pub fn request_value(&self, request: &Request) -> Result<serde_json::Value, ApiClientError> {
        if let Some(timeout) = response_timeout(request) {
            return self.request_value_with_timeout(request, timeout);
        }
        let mut stream = self.connect()?;
        write_request(&mut stream, request)?;

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
        shepr_platform::ipc::connect_local_stream(&self.socket_path())
    }
}

/// Client-side bound for ordinary requests. It trails the server's own bound
/// so the server can report that the request timed out with an unknown outcome
/// before the client gives up on the socket.
const ORDINARY_RESPONSE_TIMEOUT: Duration =
    Duration::from_secs(crate::server::ORDINARY_REQUEST_TIMEOUT.as_secs() + 5);

/// Slack past a wait's own `timeout_ms`. At its deadline a wait still makes a
/// final app probe (bounded by the server's 5 s app-response timeout), and
/// `agent.prompt --wait` chains a submission step and two status waits that
/// can each overrun by one such probe. This only has to exceed those
/// overruns; it is not what normally ends a wait.
const WAIT_RESPONSE_GRACE: Duration = Duration::from_secs(30);

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
/// Wait methods run as long as the caller asked: their own `timeout_ms` plus
/// [`WAIT_RESPONSE_GRACE`], or unbounded when sent without one. A plain
/// `agent.prompt` is unbounded because the server answers only once the
/// prompt is written to the agent, which a busy agent may delay for minutes
/// (see `prompt_agent` in `src/api/wait.rs`). Everything else, including the
/// acknowledgement of `events.subscribe`, is ordinary.
pub(crate) fn response_timeout(request: &Request) -> Option<Duration> {
    let wait_bound = |timeout_ms: Option<u64>| {
        timeout_ms.map(|ms| Duration::from_millis(ms).saturating_add(WAIT_RESPONSE_GRACE))
    };
    match &request.method {
        Method::EventsWait(params) => wait_bound(params.timeout_ms),
        Method::AgentWait(params) => wait_bound(params.timeout_ms),
        Method::PaneWaitForOutput(params) => wait_bound(params.timeout_ms),
        Method::AgentPrompt(params) => {
            // Omitting `timeout_ms` also leaves the app's submission deadline
            // empty. A hidden client cap could report failure while the queued
            // prompt remains live and is typed later.
            params
                .wait
                .as_ref()
                .and_then(|wait| wait_bound(wait.timeout_ms))
        }
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
    EmptyResponse,
    UnexpectedResult(String),
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
        env.set(shepr_config::SESSION_ENV_VAR, "work");
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
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
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
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let request = Request {
            id: "stalled".into(),
            method: Method::WorkspaceList(crate::schema::EmptyParams::default()),
        };
        let error = client
            .request_value_with_timeout(&request, Duration::from_millis(100))
            .expect_err("test precondition");
        let _ = release_tx.send(());
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
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
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
        assert!(ordinary > crate::server::ORDINARY_REQUEST_TIMEOUT);

        let wait = |timeout_ms| {
            request(Method::AgentWait(crate::schema::AgentWaitParams {
                target: "reviewer".into(),
                until: Vec::new(),
                timeout_ms,
            }))
        };
        assert_eq!(response_timeout(&wait(None)), None);
        let bounded = response_timeout(&wait(Some(600_000))).expect("bounded wait");
        assert!(bounded > Duration::from_secs(600));

        let prompt = |wait| {
            request(Method::AgentPrompt(crate::schema::AgentPromptParams {
                target: "reviewer".into(),
                text: "hi".into(),
                wait,
            }))
        };
        assert_eq!(response_timeout(&prompt(None)), None);
        let prompt_wait: crate::schema::AgentPromptWaitOptions =
            serde_json::from_value(serde_json::json!({ "timeout_ms": 1000 }))
                .expect("test precondition");
        assert!(response_timeout(&prompt(Some(prompt_wait))).is_some());
    }

    #[test]
    fn socket_path_target_uses_explicit_path() {
        let path = PathBuf::from("/tmp/shepr-test.sock");
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        assert_eq!(client.socket_path(), path);
    }
}
