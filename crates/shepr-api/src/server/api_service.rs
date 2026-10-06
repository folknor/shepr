//! The JSON API connection service: one request line per connection, answered
//! on the connection thread for socket controls (`ping` and the stops) and by
//! the app loop for everything else. The listener classifies a connection and
//! hands it here with its admission slot; this module owns the request
//! semantics, including the protocol-correct refusal of a connection over the
//! limit.

use std::io::{self, Write};
use std::time::Instant;

use tracing::debug;

use super::client_protocol::{ClientGate, ConnectionAdmission, ConnectionSlot};
use crate::limits::{
    BUSY_REQUEST_ID_TIMEOUT, INITIAL_REQUEST_READ_CHUNK_BYTES, MAX_INITIAL_REQUEST_BYTES,
    ORDINARY_REQUEST_TIMEOUT, STREAM_WRITE_TIMEOUT,
};
use crate::schema::{
    AppRequest, ErrorResponse, MethodRoute, MethodTraits, Request, ResponseResult, SocketMethod,
    SuccessResponse,
};
use crate::{ApiRequestMessage, ApiRequestSender};
use shepr_platform::ipc::{
    LocalStream, LocalStreamDeadlineReader, StreamFailure, classify_stream_error,
};

const ORDINARY_REQUEST_TIMEOUT_MESSAGE: &str =
    "timed out waiting for app response; the request may still run, so its outcome is unknown";

/// Reads the bounded initial request line so server errors can preserve its ID.
fn request_id_from_line(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct RequestId {
        id: String,
    }

    if line.starts_with('{') {
        serde_json::from_str::<RequestId>(line)
            .ok()
            .map(|request| request.id)
    } else {
        None
    }
}

/// Refuses an API connection over the limit, echoing the caller's request ID
/// when its request line arrives within a short bound. Runs on the refuser
/// thread, or inline on a classification thread that found the limit full.
pub(super) fn reject_busy_connection(mut stream: LocalStream, admission: &ConnectionAdmission) {
    // clock-io-ok: the bound covers a real socket read of the request line.
    let deadline = Instant::now() + BUSY_REQUEST_ID_TIMEOUT;
    let request_id = match read_request_line_until(&mut stream, deadline) {
        Ok(Some(line)) => request_id_from_line(line.trim()),
        Ok(None) => None,
        Err(error) => {
            debug!(%error, "could not read api request id for connection limit refusal");
            None
        }
    };
    send_busy_refusal(stream, request_id.as_deref(), admission);
}

pub(super) fn send_busy_refusal(
    mut stream: LocalStream,
    request_id: Option<&str>,
    admission: &ConnectionAdmission,
) {
    // The refuser serves every refused peer in turn; an unbounded write to a
    // stalled one would hold all the others.
    if let Err(err) = stream.set_write_timeout(Some(STREAM_WRITE_TIMEOUT)) {
        debug!(error = %err, "api refusal write timeout unavailable; closing unanswered");
        return;
    }
    let response = ErrorResponse {
        id: request_id.map(str::to_owned),
        error: crate::error::ApiError::new(
            crate::error::ApiErrorCode::EndpointBusy,
            format!(
                "API server is at its limit of {} connections reading requests",
                admission.limit().max()
            ),
        )
        .into_body(),
    };
    let body = crate::serialize_response_or_error(request_id.unwrap_or_default(), &response);
    if let Err(err) = write_text_line_allow_disconnect(&mut stream, &body) {
        debug!(error = %err, "failed to send API connection limit refusal");
    }
}

/// Serves one API connection. `deadline` bounds the request line and is
/// counted from accept, so classifying the connection does not extend it.
///
/// Admission is in two parts, so app admission saturation does not prevent
/// control admission. `ingress` covers reading and parsing the request and
/// writing any answer given without the app: `ping`, both stops, parse errors
/// and refusals. A request for the app takes an app slot while still holding
/// `ingress`, then gives `ingress` up and holds the app slot through the app's
/// answer and its write, so every worker that can block is counted by one of
/// the two. A stalled app loop therefore fills only app slots, and a stop
/// still gets through. Its answer waits for the final save result.
pub(super) fn handle_connection(
    mut stream: LocalStream,
    deadline: Instant,
    ingress: ConnectionSlot,
    app_requests: &ConnectionAdmission,
    api_tx: &ApiRequestSender,
    server_stop: &crate::ServerStopSignal,
    boot_id: &shepr_protocol::BootId,
    gate: &ClientGate,
) -> std::io::Result<()> {
    // Every answer is a bounded write, so a stalled peer cannot hold a slot;
    // a connection that cannot have that bound is closed unanswered.
    stream.set_write_timeout(Some(STREAM_WRITE_TIMEOUT))?;

    let Some(line) = read_request_line_until(&mut stream, deadline)? else {
        return Ok(());
    };

    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    let request = match serde_json::from_str::<Request>(line) {
        Ok(request) => request,
        Err(request_error) => {
            // Recover correlation without relaxing typed request validation or accepting
            // ambiguous duplicate IDs. Invalid JSON and non-string IDs stay uncorrelated.
            let id = request_id_from_line(line);
            tracing::warn!(event = "api.request.invalid", request_id = ?id, error_category = ?request_error.classify(), line = request_error.line(), column = request_error.column(), "invalid API request");
            let response = ErrorResponse {
                id,
                error: crate::error::ApiError::new(
                    crate::error::ApiErrorCode::InvalidRequest,
                    format!("invalid request: {request_error}"),
                )
                .into_body(),
            };
            write_api_json_line_allow_disconnect(
                &mut stream,
                response.id.as_deref().unwrap_or_default(),
                &response,
            )?;
            return Ok(());
        }
    };

    let request_id = request.id.clone();
    let method_traits = request.method.traits();
    crate::logging::api_request_started(&request_id, method_traits);

    let response = match route_request(request, server_stop, boot_id, gate) {
        Route::Immediate(response) => response,
        Route::StopAnswer(response) => {
            let written = finish_api_response(&mut stream, &request_id, method_traits, &response);
            server_stop.stop_answered();
            return written;
        }
        Route::App(request) => {
            let Ok(app_slot) = app_requests.try_acquire() else {
                let busy = error_response_json(
                    &request_id,
                    crate::error::ApiErrorCode::EndpointBusy,
                    APP_REQUEST_BUSY_MESSAGE.to_owned(),
                );
                return finish_api_response(&mut stream, &request_id, method_traits, &busy);
            };
            drop(ingress);
            let dispatch = dispatch_to_app(request, api_tx);
            let written =
                finish_api_response(&mut stream, &request_id, method_traits, &dispatch.response);
            // A timeout response ends the socket request, but the app request
            // can still be queued or executing. Keep its admission slot until
            // the app replies so abandoned work remains included in the bound.
            drop(stream);
            if let Some(late_completion) = dispatch.late_completion {
                // A value and a dropped sender both mean the app is done with it.
                match late_completion.recv() {
                    Ok(_) | Err(_) => {}
                }
            }
            drop(app_slot);
            return written;
        }
    };
    finish_api_response(&mut stream, &request_id, method_traits, &response)
}

fn finish_api_response(
    stream: &mut LocalStream,
    request_id: &str,
    method: MethodTraits,
    response: &crate::error::EncodedApiResponse,
) -> std::io::Result<()> {
    // A client that hung up before its answer is not a server failure, but
    // the log must not claim the response's outcome for an answer nobody got.
    let outcome = match write_text_line(stream, &response.body) {
        Ok(()) => response.outcome,
        Err(err) if matches!(classify_stream_error(err.kind()), StreamFailure::PeerGone) => {
            crate::error::ApiLogOutcome::ClientDisconnected
        }
        Err(err) => {
            crate::logging::api_request_failed(request_id, method.name, &err.to_string());
            return Err(err);
        }
    };
    crate::logging::api_request_completed(request_id, method, outcome);
    Ok(())
}

/// Where a parsed request is answered.
enum Route {
    /// By the connection thread, without the app loop.
    Immediate(crate::error::EncodedApiResponse),
    /// An accepted stop's answer, carrying the final save result. The
    /// connection thread settles it with `ServerStopSignal::stop_answered`
    /// once written, so the server does not exit under it.
    StopAnswer(crate::error::EncodedApiResponse),
    /// By the app loop.
    App(AppRequest),
}

/// Answers socket-routed controls on the connection thread, including while
/// App is restoring or stalled. The schema generates the route split and the
/// app method subset; this function performs each socket method's action.
fn route_request(
    request: Request,
    server_stop: &crate::ServerStopSignal,
    boot_id: &shepr_protocol::BootId,
    gate: &ClientGate,
) -> Route {
    let Request { id, method } = request;
    let method = match method.into_route() {
        MethodRoute::Socket(SocketMethod::Ping(_)) => {
            let response = SuccessResponse {
                id: id.clone(),
                result: ResponseResult::Pong {
                    // Preserve the executable's display version; build_id is
                    // the separate machine-comparison field in this response.
                    version: shepr_protocol::build_version(),
                    build_id: shepr_protocol::BuildIdentity::for_this_build(),
                    boot_id: boot_id.clone(),
                    stopping: server_stop.is_requested(),
                    starting: !gate.is_open(),
                },
            };
            return Route::Immediate(crate::serialize_response_or_error_with_outcome(
                &id, &response,
            ));
        }
        MethodRoute::Socket(SocketMethod::ServerStop(_)) => {
            return stop_server(&id, None, boot_id, server_stop);
        }
        MethodRoute::Socket(SocketMethod::ServerStopIfBoot(params)) => {
            return stop_server(&id, Some(&params.expected_boot_id), boot_id, server_stop);
        }
        MethodRoute::App(method) => method,
    };

    if server_stop.is_requested() {
        return Route::Immediate(error_response_json(
            &id,
            crate::error::ApiErrorCode::ServerUnavailable,
            shepr_protocol::ShutdownReason::Stopping.to_string(),
        ));
    }

    Route::App(AppRequest { id, method })
}

fn stop_server(
    id: &str,
    expected_boot_id: Option<&shepr_protocol::BootId>,
    actual: &shepr_protocol::BootId,
    server_stop: &crate::ServerStopSignal,
) -> Route {
    // The conditional operation has its own method name because this request
    // crosses builds. A server that predates it rejects the method instead of
    // ignoring a guard and treating the request as an unconditional stop. A
    // guard sent anywhere but that method's params (for example beside
    // `server.stop`) never reaches this function: `Request` refuses unknown
    // top-level keys and repeated keys inside params, and `ServerStopIfBootParams`
    // refuses unknown params, so a stop is conditional only when its one
    // guard is where this method reads it.
    if let Some(expected) = expected_boot_id {
        // A stop aimed at one boot must not stop another: the caller observed
        // that instance, and the occupant may have been replaced since.
        if actual != expected {
            return Route::Immediate(error_response_json(
                id,
                crate::error::ApiErrorCode::ServerBootMismatch,
                format!(
                    "refusing to stop: this server is boot {actual}, not the expected boot {expected}"
                ),
            ));
        }
    }
    tracing::info!(event = "server.stop.accepted", request_id = id, boot_id = %actual, expected_boot_id = ?expected_boot_id, "server stop accepted");
    server_stop.request();
    // The answer waits for the final save so it can carry its result. The
    // server keeps its socket through that save, so the client would wait
    // that long for the socket to go anyway; its stop deadline covers both.
    let final_save_error = server_stop.wait_for_final_save();
    let response = SuccessResponse {
        id: id.to_owned(),
        result: ResponseResult::ServerStopCompleted { final_save_error },
    };
    Route::StopAnswer(crate::serialize_response_or_error_with_outcome(
        id, &response,
    ))
}

/// Reads the connection's one request line with blocking reads bounded by an
/// overall deadline.
///
/// Blocking reads wake as soon as the client's bytes arrive, so a client that
/// writes just after connecting pays no poll interval, and a large request
/// costs one syscall per chunk rather than per byte. Reading in chunks can
/// consume bytes past the newline, which are dropped. The protocol is one
/// request per connection and no method reads a payload after its line.
fn read_request_line_until(
    stream: &mut LocalStream,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    stream.set_nonblocking(false)?;
    let result = read_request_line_blocking(stream, deadline);
    // Later phases arm their own modes; don't leave a stale receive timeout.
    // A read error takes precedence over a failure to clear it.
    let reset = stream.set_read_timeout(None);
    let line = result?;
    reset?;
    Ok(line)
}

fn read_request_line_blocking(
    stream: &mut LocalStream,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    use std::io::Read as _;

    let mut reader = LocalStreamDeadlineReader::new(stream, deadline);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; INITIAL_REQUEST_READ_CHUNK_BYTES];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out reading api request",
                ));
            }
            Err(err) => return Err(err),
        };
        if read == 0 {
            return Ok(None);
        }
        let chunk = &chunk[..read];
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        bytes.extend_from_slice(&chunk[..newline.map_or(read, |index| index + 1)]);
        // The limit covers the line without its newline.
        let line_len = bytes.len() - usize::from(newline.is_some());
        if line_len > MAX_INITIAL_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "api request line is too large",
            ));
        }
        if newline.is_some() {
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
        }
    }
}

fn write_text_line(stream: &mut LocalStream, value: &str) -> std::io::Result<()> {
    stream.write_all(value.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn write_text_line_allow_disconnect(stream: &mut LocalStream, value: &str) -> std::io::Result<()> {
    match write_text_line(stream, value) {
        Err(err) if matches!(classify_stream_error(err.kind()), StreamFailure::PeerGone) => Ok(()),
        result => result,
    }
}

fn write_api_json_line<T: serde::Serialize>(
    stream: &mut LocalStream,
    request_id: &str,
    value: &T,
) -> std::io::Result<()> {
    let encoded = crate::serialize_response_or_error(request_id, value);
    write_text_line(stream, &encoded)
}

fn write_api_json_line_allow_disconnect<T: serde::Serialize>(
    stream: &mut LocalStream,
    request_id: &str,
    value: &T,
) -> std::io::Result<()> {
    match write_api_json_line(stream, request_id, value) {
        Err(err) if matches!(classify_stream_error(err.kind()), StreamFailure::PeerGone) => Ok(()),
        result => result,
    }
}

fn dispatch_to_app(request: AppRequest, api_tx: &ApiRequestSender) -> AppDispatchOutcome {
    let request_id = request.id.clone();
    let (result, late_completion) = dispatch_to_app_result(request, api_tx);
    AppDispatchOutcome {
        response: crate::error::encode_result_with_outcome(request_id, result),
        late_completion,
    }
}

struct AppDispatchOutcome {
    response: crate::error::EncodedApiResponse,
    /// Kept alive after a timeout so the connection worker can retain its app
    /// admission slot until the queued or executing request is resolved.
    late_completion: Option<std::sync::mpsc::Receiver<crate::error::ApiResult>>,
}

fn dispatch_to_app_result(
    request: AppRequest,
    api_tx: &ApiRequestSender,
) -> (
    crate::error::ApiResult,
    Option<std::sync::mpsc::Receiver<crate::error::ApiResult>>,
) {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let request_id = request.id.clone();
    if let Err(err) = api_tx.try_send(ApiRequestMessage {
        request,
        respond_to,
    }) {
        // A full queue is app-loop saturation too; keep its retry category in
        // step with the app-slot admission refusal above.
        let (code, message) = match err {
            tokio::sync::mpsc::error::TrySendError::Full(_) => (
                crate::error::ApiErrorCode::EndpointBusy,
                APP_REQUEST_BUSY_MESSAGE,
            ),
            tokio::sync::mpsc::error::TrySendError::Closed(_) => (
                crate::error::ApiErrorCode::ServerUnavailable,
                "API request handler is unavailable",
            ),
        };
        tracing::debug!(request_id, %message, "API request was not queued");
        return (Err(crate::error::ApiError::new(code, message)), None);
    }

    match response_rx.recv_timeout(ORDINARY_REQUEST_TIMEOUT) {
        Ok(response) => (response, None),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (
            Err(crate::error::ApiError::new(
                crate::error::ApiErrorCode::Timeout,
                ORDINARY_REQUEST_TIMEOUT_MESSAGE,
            )),
            Some(response_rx),
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => (
            Err(crate::error::ApiError::new(
                crate::error::ApiErrorCode::ServerUnavailable,
                "request handling failed: app response channel closed",
            )),
            None,
        ),
    }
}

const APP_REQUEST_BUSY_MESSAGE: &str = "server is busy handling API requests; retry later";

fn error_response_json(
    id: &str,
    code: crate::error::ApiErrorCode,
    message: String,
) -> crate::error::EncodedApiResponse {
    crate::error::encode_result_with_outcome(
        id.to_owned(),
        Err(crate::error::ApiError::new(code, message)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AppMethod, Method};
    use shepr_test_support::ScratchDir;
    use std::io::{BufRead, BufReader, Read};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn running() -> crate::ServerStopSignal {
        crate::ServerStopSignal::default()
    }

    /// The boot of the server these tests stand in for.
    fn this_boot() -> shepr_protocol::BootId {
        shepr_protocol::BootId::from_process_clock(1, Ok(Duration::ZERO))
    }

    /// Routes a request as a connection does, dispatching an app-bound one.
    fn handle_request(
        request: Request,
        api_tx: &ApiRequestSender,
        server_stop: &crate::ServerStopSignal,
        gate: &ClientGate,
    ) -> crate::error::EncodedApiResponse {
        match route_request(request, server_stop, &this_boot(), gate) {
            Route::Immediate(response) => response,
            Route::StopAnswer(response) => {
                server_stop.stop_answered();
                response
            }
            Route::App(request) => dispatch_to_app(request, api_tx).response,
        }
    }

    /// Serves one connection with free admission and a running server.
    fn serve_connection(
        server: LocalStream,
        api_tx: &ApiRequestSender,
        app_requests: &ConnectionAdmission,
    ) -> io::Result<()> {
        serve_connection_with_stop(server, api_tx, app_requests, &running())
    }

    /// As [`serve_connection`], under a caller's stop signal.
    fn serve_connection_with_stop(
        server: LocalStream,
        api_tx: &ApiRequestSender,
        app_requests: &ConnectionAdmission,
        server_stop: &crate::ServerStopSignal,
    ) -> io::Result<()> {
        let ingress_count = Arc::new(AtomicUsize::new(0));
        let ingress_admission = ConnectionAdmission::new(
            ingress_count,
            shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 1),
        );
        let ingress = ingress_admission
            .try_acquire()
            .map_err(|exceeded| io::Error::other(exceeded.to_string()))?;
        handle_connection(
            server,
            Instant::now() + crate::limits::INITIAL_REQUEST_TIMEOUT,
            ingress,
            app_requests,
            api_tx,
            server_stop,
            &this_boot(),
            &ClientGate::default(),
        )
    }

    fn app_admission(active: Arc<AtomicUsize>) -> ConnectionAdmission {
        ConnectionAdmission::new(
            active,
            shepr_protocol::Limit::new(
                shepr_protocol::LimitKind::ConnectionCount,
                crate::limits::MAX_APP_REQUESTS_IN_FLIGHT,
            ),
        )
    }

    fn detect_capture(id: &str) -> Request {
        Request {
            id: id.into(),
            method: Method::DetectCapture(crate::schema::PaneTarget {
                pane_id: "w1:p1".into(),
            }),
        }
    }

    /// A fresh path in its own scratch directory.
    fn unique_test_path(name: &str) -> PathBuf {
        ScratchDir::new(name).join("s")
    }

    fn read_line(stream: &mut LocalStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("test precondition");
        line
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream) {
        let path = unique_test_path(name);
        let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition").0;
        (client, server)
    }

    fn read_initial_request_line(stream: &mut LocalStream) -> io::Result<Option<String>> {
        read_request_line_until(
            stream,
            Instant::now() + crate::limits::INITIAL_REQUEST_TIMEOUT,
        )
    }

    #[test]
    fn an_excess_api_connection_is_refused_with_its_request_id() {
        let (mut client, server) = local_stream_pair("connection-limit-refusal");
        client
            .write_all(br#"{"id":"busy-request","method":"ping","params":{}}"#)
            .expect("write busy request");
        client
            .write_all(b"\n")
            .expect("terminate busy request line");
        let admission = ConnectionAdmission::new(
            Arc::new(AtomicUsize::new(0)),
            shepr_protocol::Limit::new(
                shepr_protocol::LimitKind::ConnectionCount,
                crate::limits::MAX_API_INGRESS_CONNECTIONS,
            ),
        );
        reject_busy_connection(server, &admission);

        let response: ErrorResponse =
            serde_json::from_str(&read_line(&mut client)).expect("valid refusal response");
        assert_eq!(response.id.as_deref(), Some("busy-request"));
        assert_eq!(
            response.error.code,
            crate::error::ApiErrorCode::EndpointBusy
        );
        assert!(
            response.error.message.contains(&format!(
                "{} connections",
                crate::limits::MAX_API_INGRESS_CONNECTIONS
            )),
            "{}",
            response.error.message
        );
    }

    #[test]
    fn request_line_arriving_after_connect_is_read_completely() {
        let (mut client, mut server) = local_stream_pair("request-line-latency");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let reader = std::thread::spawn(move || {
            ready_tx.send(()).expect("reader ready");
            read_initial_request_line(&mut server)
        });
        ready_rx.recv().expect("reader ready");
        client
            .write_all(br#"{"id":"late","method":"ping","#)
            .expect("test precondition");
        client.flush().expect("test precondition");
        client
            .write_all(b"\"params\":{}}\n")
            .expect("test precondition");
        client.flush().expect("test precondition");
        let line = reader
            .join()
            .expect("reader thread")
            .expect("test precondition")
            .expect("request line");
        assert_eq!(
            line,
            "{\"id\":\"late\",\"method\":\"ping\",\"params\":{}}\n"
        );
    }

    #[test]
    fn request_line_read_honours_its_deadline_and_size_limit() {
        let (_client, mut server) = local_stream_pair("request-line-deadline");
        let error =
            read_request_line_until(&mut server, Instant::now() + Duration::from_millis(30))
                .expect_err("silent client must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let (mut client, mut server) = local_stream_pair("request-line-oversize");
        let writer = std::thread::spawn(move || {
            // The server stops reading at the limit, so the tail of this write
            // may fail once it closes; only the server's verdict matters.
            drop(client.write_all(&vec![b'x'; MAX_INITIAL_REQUEST_BYTES + 1]));
        });
        let error = read_request_line_until(&mut server, Instant::now() + Duration::from_secs(5))
            .expect_err("oversized line must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(server);
        writer.join().expect("test precondition");

        let (client, mut server) = local_stream_pair("request-line-eof");
        drop(client);
        assert!(
            read_request_line_until(&mut server, Instant::now() + Duration::from_secs(5))
                .expect("test precondition")
                .is_none()
        );
    }

    #[test]
    fn api_response_logging_outcome_comes_from_the_typed_result() {
        let success =
            crate::error::encode_result_with_outcome("req".into(), Ok(ResponseResult::Ok {}));
        assert_eq!(success.outcome.as_str(), "ok");

        let timeout = crate::error::encode_result_with_outcome(
            "req".into(),
            Err(crate::error::ApiError::new(
                crate::error::ApiErrorCode::Timeout,
                "timed out waiting for agent status",
            )),
        );
        assert_eq!(timeout.outcome.as_str(), "timeout");

        let generic_error = crate::error::encode_result_with_outcome(
            "req".into(),
            Err(crate::error::ApiError::new(
                crate::error::ApiErrorCode::ServerUnavailable,
                "boom",
            )),
        );
        assert_eq!(generic_error.outcome.as_str(), "error");
    }

    #[test]
    fn unknown_method_returns_invalid_request_response() {
        let (mut client, server) = local_stream_pair("unknown-api-request");
        let (api_tx, mut api_rx) = mpsc::channel::<ApiRequestMessage>(1);
        client
            .write_all(b"{\"id\":\"unknown\",\"method\":\"nope\",\"params\":{}}\n")
            .expect("test precondition");
        client.flush().expect("test precondition");

        serve_connection(server, &api_tx, &app_admission(Arc::default()))
            .expect("test precondition");

        let response = read_line(&mut client);
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["id"], "unknown");
        assert_eq!(response["error"]["code"], "invalid_request");
        assert!(api_rx.try_recv().is_err());
    }

    #[test]
    fn ordinary_api_request_still_uses_normal_connection_path() {
        let (mut client, server) = local_stream_pair("ordinary-api-request");
        let (api_tx, _api_rx) = mpsc::channel::<ApiRequestMessage>(1);
        client
            .write_all(b"{\"id\":\"ordinary\",\"method\":\"ping\",\"params\":{}}\n")
            .expect("test precondition");
        client.flush().expect("test precondition");

        serve_connection(server, &api_tx, &app_admission(Arc::default()))
            .expect("test precondition");

        let response = read_line(&mut client);
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["id"], "ordinary");
        assert_eq!(response["result"]["type"], "pong");
    }

    /// A stalled app loop holds every app slot; a stop is still read and
    /// answered, and another app request is refused at once.
    #[test]
    fn full_app_admission_still_admits_control_requests() {
        let app_requests = app_admission(Arc::new(AtomicUsize::new(0)));
        let _held = (0..crate::limits::MAX_APP_REQUESTS_IN_FLIGHT)
            .map(|_| app_requests.try_acquire().expect("app slot"))
            .collect::<Vec<_>>();
        let (api_tx, mut api_rx) = mpsc::channel::<ApiRequestMessage>(1);

        let (mut client, server) = local_stream_pair("full-app-admission-stop");
        writeln!(
            client,
            r#"{{"id":"stop","method":"server.stop","params":{{}}}}"#
        )
        .expect("test precondition");
        // The stop's answer waits for the final save result.
        let stop = running();
        stop.complete_final_save(None);
        serve_connection_with_stop(server, &api_tx, &app_requests, &stop).expect("stop served");
        let stopped: serde_json::Value =
            serde_json::from_str(&read_line(&mut client)).expect("json");
        assert_eq!(stopped["result"]["type"], "server_stop_completed");

        let (mut client, server) = local_stream_pair("full-app-admission-report");
        writeln!(
            client,
            r#"{{"id":"capture","method":"detect.capture","params":{{"pane_id":"w1:p1"}}}}"#
        )
        .expect("test precondition");
        serve_connection(server, &api_tx, &app_requests).expect("refusal served");
        let refused: serde_json::Value =
            serde_json::from_str(&read_line(&mut client)).expect("json");
        assert_eq!(refused["id"], "capture");
        assert_eq!(refused["error"]["code"], "endpoint_busy");
        assert!(api_rx.try_recv().is_err(), "nothing reached the app");
        assert_eq!(
            app_requests.active_count(),
            crate::limits::MAX_APP_REQUESTS_IN_FLIGHT
        );
    }

    #[test]
    fn ping_request_returns_pong() {
        let (tx, _rx) = mpsc::channel(1);
        let response = handle_request(
            Request {
                id: "req_1".into(),
                method: Method::Ping(crate::schema::PingParams::default()),
            },
            &tx,
            &running(),
            &ClientGate::default(),
        );

        let parsed: SuccessResponse =
            serde_json::from_str(&response.body).expect("test precondition");
        assert_eq!(parsed.id, "req_1");
        assert!(matches!(
            parsed.result,
            ResponseResult::Pong {
                stopping: false,
                ..
            }
        ));
    }

    #[test]
    fn ping_still_answers_after_a_stop_and_says_so() {
        // A stopping server keeps its socket until the final save is on disk;
        // the pong is how a launcher tells it apart from one it can attach to.
        let (tx, _rx) = mpsc::channel(1);
        let stop = running();
        stop.request();
        let response = handle_request(
            Request {
                id: "req_1".into(),
                method: Method::Ping(crate::schema::PingParams::default()),
            },
            &tx,
            &stop,
            &ClientGate::default(),
        );

        let parsed: SuccessResponse =
            serde_json::from_str(&response.body).expect("test precondition");
        assert!(matches!(
            parsed.result,
            ResponseResult::Pong { stopping: true, .. }
        ));
    }

    #[test]
    fn server_stop_control_bypasses_app_channel() {
        let (tx, mut rx) = mpsc::channel(1);
        let stop = running();
        stop.complete_final_save(Some("data directory is read-only".to_owned()));
        let response = handle_request(
            Request {
                id: "priority_stop".into(),
                method: Method::ServerStop(crate::schema::ServerStopParams::default()),
            },
            &tx,
            &stop,
            &ClientGate::default(),
        );

        let response: serde_json::Value =
            serde_json::from_str(&response.body).expect("test precondition");
        assert_eq!(response["id"], "priority_stop");
        assert_eq!(response["result"]["type"], "server_stop_completed");
        assert_eq!(
            response["result"]["final_save_error"],
            "data directory is read-only"
        );
        assert!(stop.is_requested());

        let rejected = handle_request(
            detect_capture("after_stop"),
            &tx,
            &stop,
            &ClientGate::default(),
        );
        let rejected: serde_json::Value =
            serde_json::from_str(&rejected.body).expect("test precondition");
        assert_eq!(rejected["error"]["code"], "server_unavailable");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ping_reports_the_boot_id_a_conditional_stop_must_match() {
        let (tx, _rx) = mpsc::channel(1);
        let ping = handle_request(
            Request {
                id: "ping".into(),
                method: Method::Ping(crate::schema::PingParams::default()),
            },
            &tx,
            &running(),
            &ClientGate::default(),
        );
        let ping: SuccessResponse = serde_json::from_str(&ping.body).expect("test precondition");
        let ResponseResult::Pong { boot_id, .. } = ping.result else {
            panic!("ping did not answer with a pong");
        };
        assert_eq!(boot_id, this_boot(), "ping reports the boot it was given");
        let stop_with = |expected_boot_id: Option<shepr_protocol::BootId>,
                         stop: &crate::ServerStopSignal| {
            stop.complete_final_save(None);
            let method = match expected_boot_id {
                Some(expected_boot_id) => {
                    Method::ServerStopIfBoot(crate::schema::ServerStopIfBootParams {
                        expected_boot_id,
                    })
                }
                None => Method::ServerStop(crate::schema::ServerStopParams::default()),
            };
            let response = handle_request(
                Request {
                    id: "stop".into(),
                    method,
                },
                &tx,
                stop,
                &ClientGate::default(),
            );
            serde_json::from_str::<serde_json::Value>(&response.body).expect("test precondition")
        };

        let other_boot = running();
        let refused = stop_with(
            Some(shepr_protocol::BootId::from_process_clock(
                boot_id.process_id().unwrap_or_default().wrapping_add(1),
                Ok(Duration::ZERO),
            )),
            &other_boot,
        );
        assert_eq!(refused["error"]["code"], "server_boot_mismatch");
        assert!(!other_boot.is_requested());

        let this_boot = running();
        let stopped = stop_with(Some(boot_id), &this_boot);
        assert_eq!(stopped["result"]["type"], "server_stop_completed");
        assert!(this_boot.is_requested());
    }

    #[test]
    fn a_full_api_request_queue_refuses_without_waiting_or_dropping_queued_work() {
        let (tx, mut rx) = mpsc::channel(1);
        let (respond_to, _response_rx) = std::sync::mpsc::channel();
        assert!(
            tx.try_send(ApiRequestMessage {
                request: AppRequest {
                    id: "already-queued".into(),
                    method: AppMethod::DetectCapture(crate::schema::PaneTarget {
                        pane_id: "w1:p1".into(),
                    }),
                },
                respond_to,
            })
            .is_ok()
        );

        let response = dispatch_to_app(
            AppRequest {
                id: "overflow".into(),
                method: AppMethod::DetectCapture(crate::schema::PaneTarget {
                    pane_id: "w1:p2".into(),
                }),
            },
            &tx,
        );
        let response: serde_json::Value =
            serde_json::from_str(&response.response.body).expect("test precondition");

        assert_eq!(response["id"], "overflow");
        assert_eq!(response["error"]["code"], "endpoint_busy");
        assert_eq!(
            response["error"]["message"],
            "server is busy handling API requests; retry later"
        );
        assert_eq!(
            rx.try_recv()
                .expect("the queued request remains available")
                .request
                .id,
            "already-queued"
        );
    }

    #[test]
    fn request_dispatches_to_app_channel() {
        let (tx, mut rx) = mpsc::channel(1);
        let thread = std::thread::spawn(move || {
            handle_request(
                detect_capture("req_2"),
                &tx,
                &running(),
                &ClientGate::default(),
            )
        });

        let msg = rx.blocking_recv().expect("test precondition");
        assert_eq!(msg.request.id, "req_2");
        assert_eq!(
            msg.request.method,
            AppMethod::DetectCapture(crate::schema::PaneTarget {
                pane_id: "w1:p1".into(),
            })
        );
        msg.respond_to
            .send(Ok(ResponseResult::Ok {}))
            .expect("test precondition");

        let response = thread.join().expect("test precondition");
        let parsed: SuccessResponse =
            serde_json::from_str(&response.body).expect("test precondition");
        assert_eq!(parsed.id, "req_2");
    }

    #[test]
    fn invalid_requests_preserve_only_unambiguous_string_ids() {
        let cases = [
            (
                r#"{"id":"mine","method":"pane.report_agent","params":{"pane_id":"w1:p1","status":"working","source":"x"}}"#,
                Some("mine"),
            ),
            (
                r#"{"id":"escaped\"id","method":"unknown"}"#,
                Some("escaped\"id"),
            ),
            (r#"{"method":"unknown","params":{"id":"nested"}}"#, None),
            (r#"{"id":123,"method":"unknown"}"#, None),
            (
                r#"{"id":"first","id":"second","method":"ping","params":{}}"#,
                None,
            ),
            (
                concat!(
                    r#"{"id":"bad-boot","method":"server.stop_if_boot","params":{"#,
                    r#""expected_boot_id":"old-boot"}}"#
                ),
                Some("bad-boot"),
            ),
            (r#"{"id":"truncated","method":"ping""#, None),
            (r#"["not-an-object"]"#, None),
            (r#"{"id":"","method":"unknown"}"#, Some("")),
        ];
        for (request, expected_id) in cases {
            let (api_tx, mut api_rx) = mpsc::channel(1);
            let (mut client, server) = local_stream_pair("invalid-request-id");
            writeln!(client, "{request}").expect("test precondition");
            serve_connection(server, &api_tx, &app_admission(Arc::default()))
                .expect("test precondition");

            let mut response = String::new();
            BufReader::new(client)
                .read_to_string(&mut response)
                .expect("test precondition");
            let response: ErrorResponse =
                serde_json::from_str(&response).expect("test precondition");
            assert_eq!(response.id.as_deref(), expected_id, "{request}");
            assert_eq!(
                response.error.code,
                crate::error::ApiErrorCode::InvalidRequest
            );
            assert!(response.error.message.starts_with("invalid request: "));
            assert!(
                api_rx.try_recv().is_err(),
                "invalid requests must not dispatch"
            );
        }
    }
}
