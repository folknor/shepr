use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::limits::{ORDINARY_CONNECT_TIMEOUT, ORDINARY_RESPONSE_TIMEOUT};
use crate::schema::{ErrorResponse, Request, ResponseResult, SuccessResponse};
use shepr_platform::ipc::{LocalStreamDeadlineReader, TrustedServerStream};

pub use crate::limits::{STATUS_REQUEST_TIMEOUT, STOP_REQUEST_TIMEOUT};

/// A decoded `ping` answer: the identity the server reports and its readiness
/// flags, as they crossed the wire. What they mean for a launch or a stop is
/// the caller's to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pong {
    pub version: String,
    pub build_id: shepr_protocol::BuildIdentity,
    /// The server process's boot identity.
    pub boot_id: shepr_protocol::BootId,
    /// The server has begun stopping.
    pub stopping: bool,
    /// The server has bound its socket but not yet opened its client protocol.
    pub starting: bool,
}

/// A decoded `server.summary` answer: how many workspaces, panes and agents
/// the server holds, and how many of those agents are blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerSummary {
    pub workspaces: usize,
    pub panes: usize,
    pub agents: usize,
    pub blocked_agents: usize,
}

/// Reusable client for Shepr's newline-delimited JSON API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    socket_path: PathBuf,
}

impl ApiClient {
    pub fn local(paths: &shepr_paths::AppPaths) -> Self {
        Self::for_socket(paths.server_address().socket().to_path_buf())
    }

    /// A JSON API client for the server socket at `socket_path`, resolved by the caller
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
    /// The bounded socket connect is separate from the response wait, which
    /// leaves the server's full request bound and the client's grace after it
    /// even when the listen backlog delays connection. A timeout surfaces as
    /// `ErrorKind::TimedOut`.
    pub fn request_value(&self, request: &Request) -> Result<serde_json::Value, ApiClientError> {
        self.request_value_with_timeout(request, ORDINARY_RESPONSE_TIMEOUT)
    }

    /// Like [`Self::request_value`] with a caller-selected write/read budget.
    fn request_value_with_timeout(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiClientError> {
        if timeout.is_zero() {
            return Err(ApiClientError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "api request deadline expired before connecting",
            )));
        }
        let stream = self
            .connect(timeout.min(ORDINARY_CONNECT_TIMEOUT))
            .map_err(ApiClientError::Io)?;
        let deadline = deadline_after(timeout).map_err(ApiClientError::Io)?;
        request_value_on_stream_until(request, stream, deadline)
            .map_err(ApiClientDeadlineError::into_client_error)
    }

    /// Sends one request and reads its single-line response, all of it bounded
    /// by one `deadline`: the connect, the write and the read share it, and
    /// connect also keeps the ordinary local-connect cap. A caller polling a
    /// server keeps one budget across requests. A failure to reach the socket
    /// is told apart from a failure of the request itself.
    pub fn request_value_until(
        &self,
        request: &Request,
        deadline: Instant,
    ) -> Result<serde_json::Value, ApiClientDeadlineError> {
        // clock-io-ok: the connect is bounded by what is left of the deadline.
        let connect_timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(ORDINARY_CONNECT_TIMEOUT);
        if connect_timeout.is_zero() {
            return Err(ApiClientDeadlineError::Connect(io::Error::new(
                io::ErrorKind::TimedOut,
                "api request deadline expired before connecting",
            )));
        }
        let stream = self
            .connect(connect_timeout)
            .map_err(ApiClientDeadlineError::Connect)?;
        request_value_on_stream_until(request, stream, deadline)
    }

    /// [`Self::request_value_until`], decoded into a success or the server's
    /// error response.
    pub fn request_until(
        &self,
        request: &Request,
        deadline: Instant,
    ) -> Result<SuccessResponse, ApiClientDeadlineError> {
        let value = self.request_value_until(request, deadline)?;
        parse_response_value(value).map_err(ApiClientDeadlineError::Request)
    }

    /// Asks the server for its identity and readiness within one status window.
    pub fn ping(&self) -> Result<Pong, ApiClientError> {
        self.ping_until(deadline_after(STATUS_REQUEST_TIMEOUT)?)
            .map_err(ApiClientDeadlineError::into_client_error)
    }

    /// [`Self::ping`] bounded by one `deadline`.
    pub fn ping_until(&self, deadline: Instant) -> Result<Pong, ApiClientDeadlineError> {
        let response = self.request_until(&Request::ping(), deadline)?;
        pong(response).map_err(ApiClientDeadlineError::Request)
    }

    /// Asks the app loop for the session's counts, bounded by one `deadline`.
    /// Only a server of this build knows the method.
    pub fn server_summary_until(
        &self,
        deadline: Instant,
    ) -> Result<ServerSummary, ApiClientDeadlineError> {
        let request = Request::server_summary();
        let response = self.request_until(&request, deadline)?;
        match response.result {
            ResponseResult::ServerSummary {
                workspaces,
                panes,
                agents,
                blocked_agents,
            } => Ok(ServerSummary {
                workspaces,
                panes,
                agents,
                blocked_agents,
            }),
            result => Err(ApiClientDeadlineError::Request(
                ApiClientError::UnexpectedResult(format!("{result:?}")),
            )),
        }
    }

    /// Every request (status, stop, detect) checks who serves the socket
    /// before the first byte is written to it, and waits at most `timeout` for
    /// a listener whose backlog is full (`ErrorKind::TimedOut`).
    fn connect(&self, timeout: Duration) -> io::Result<TrustedServerStream> {
        shepr_platform::ipc::connect_trusted_local_stream_within(&self.socket_path, timeout)
    }
}

fn request_value_on_stream_until(
    request: &Request,
    mut stream: TrustedServerStream,
    deadline: Instant,
) -> Result<serde_json::Value, ApiClientDeadlineError> {
    // clock-io-ok: bounds the request write by the same deadline as its read.
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
    if let Err(error) = stream.set_write_timeout(Some(send_timeout))
        && error.kind() != io::ErrorKind::InvalidInput
    {
        return Err(ApiClientDeadlineError::Request(error.into()));
    }
    write_request(&mut stream, request)
        .map_err(normalize_socket_timeout)
        .map_err(ApiClientDeadlineError::Request)?;

    let mut reader = BufReader::new(LocalStreamDeadlineReader::new(&mut stream, deadline));
    read_response_value(&mut reader, &request.id)
        .map_err(normalize_socket_timeout)
        .map_err(ApiClientDeadlineError::Request)
}

fn pong(response: SuccessResponse) -> Result<Pong, ApiClientError> {
    match response.result {
        ResponseResult::Pong {
            version,
            build_id,
            boot_id,
            stopping,
            starting,
        } => Ok(Pong {
            version,
            build_id,
            boot_id,
            stopping,
            starting,
        }),
        result => Err(ApiClientError::UnexpectedResult(format!("{result:?}"))),
    }
}

fn deadline_after(timeout: Duration) -> io::Result<Instant> {
    // clock-io-ok: the caller uses this deadline to bound real socket IO.
    Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "api request timeout is too large",
        )
    })
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

/// A deadline-bounded request's failure: the socket could not be reached, or
/// the request on a reached socket failed.
#[derive(Debug)]
pub enum ApiClientDeadlineError {
    /// No connection was made, including one whose deadline ran out first.
    Connect(io::Error),
    /// The connection was made and the request or its response failed.
    Request(ApiClientError),
}

impl ApiClientDeadlineError {
    fn into_client_error(self) -> ApiClientError {
        match self {
            Self::Connect(error) => ApiClientError::Io(error),
            Self::Request(error) => error,
        }
    }
}

impl fmt::Display for ApiClientDeadlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => write!(f, "{error}"),
            Self::Request(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ApiClientDeadlineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(error) => Some(error),
            Self::Request(error) => Some(error),
        }
    }
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

fn write_request(
    stream: &mut TrustedServerStream,
    request: &Request,
) -> Result<(), ApiClientError> {
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

fn read_response_value(
    reader: &mut impl BufRead,
    expected_id: &str,
) -> Result<serde_json::Value, ApiClientError> {
    let value = read_json_line::<serde_json::Value>(reader)?;
    let response_id = value.get("id").and_then(serde_json::Value::as_str);
    // Each stream carries one request, so an idless refusal on that stream is
    // still its answer; responses naming another request remain invalid.
    let idless_error =
        value.get("error").is_some() && value.get("id").is_none_or(serde_json::Value::is_null);
    if response_id != Some(expected_id) && !idless_error {
        return Err(ApiClientError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("API response id mismatch: expected {expected_id:?}, received {response_id:?}"),
        )));
    }
    Ok(value)
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
    use crate::schema::{Method, PingParams};

    #[test]
    fn an_expired_request_budget_cannot_connect() {
        let scratch = shepr_test_support::ScratchDir::new("expired-request-budget");
        let path = scratch.join("api.sock");
        let _listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let error = ApiClient::for_socket(path)
            .request_value_with_timeout(&Request::ping(), Duration::ZERO)
            .expect_err("an expired budget cannot send a request");
        assert!(
            matches!(error, ApiClientError::Io(error) if error.kind() == io::ErrorKind::TimedOut)
        );
    }

    #[test]
    fn local_client_targets_the_build_runtime_socket() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
        let client = ApiClient::local(&paths);
        assert_eq!(client.socket_path(), paths.server_address().socket());
    }

    #[test]
    fn request_timeout_on_a_stalled_server_is_reported_as_timed_out() {
        let scratch = shepr_test_support::ScratchDir::new("request-timeout");
        let path = scratch.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition").0;
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
            method: Method::Ping(PingParams::default()),
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
        let scratch = shepr_test_support::ScratchDir::new("partial-response");
        let path = scratch.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let server = std::thread::spawn(move || {
            let stream = listener.accept().expect("test precondition").0;
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
            method: Method::Ping(PingParams::default()),
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
    fn a_response_with_another_request_id_is_invalid_data() {
        let scratch = shepr_test_support::ScratchDir::new("response-id-mismatch");
        let path = scratch.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test precondition");
            let mut reader = BufReader::new(stream.try_clone().expect("test precondition"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("test precondition");
            stream
                .write_all(b"{\"id\":\"another-request\",\"result\":{\"type\":\"ok\"}}\n")
                .expect("test precondition");
        });

        let client = ApiClient::for_socket(path.clone());
        let error = client
            .request_value_with_timeout(&Request::ping(), Duration::from_secs(1))
            .expect_err("a response for another request must be refused");
        server.join().expect("test precondition");
        std::fs::remove_file(path).expect("test precondition");
        assert!(
            matches!(&error, ApiClientError::Io(error) if error.kind() == io::ErrorKind::InvalidData),
            "{error:?}"
        );
    }

    #[test]
    fn an_error_without_a_request_id_is_still_the_connection_answer() {
        let mut reader = BufReader::new(
            br#"{"id":null,"error":{"code":"endpoint_busy","message":"busy"}}"#.as_slice(),
        );
        let value = read_response_value(&mut reader, "ping").expect("idless refusal is answer");
        assert!(matches!(
            parse_response_value(value),
            Err(ApiClientError::ErrorResponse(ErrorResponse {
                id: None,
                ..
            }))
        ));
    }

    #[test]
    fn an_error_with_another_request_id_is_still_rejected() {
        let mut reader = BufReader::new(
            br#"{"id":"other","error":{"code":"endpoint_busy","message":"busy"}}"#.as_slice(),
        );
        assert!(matches!(
            read_response_value(&mut reader, "ping"),
            Err(ApiClientError::Io(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn socket_path_target_uses_explicit_path() {
        let path = PathBuf::from("/nonexistent/shepr-test.sock");
        let client = ApiClient::for_socket(path.clone());
        assert_eq!(client.socket_path(), path);
    }
}
