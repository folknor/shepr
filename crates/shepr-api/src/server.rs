use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::{ListenerExt as _, Stream as _};
use tracing::{debug, error, info, warn};

use crate::limits::{
    ACCEPT_BACKOFF_MAX, ACCEPT_BACKOFF_MIN, BUSY_REFUSAL_QUEUE, BUSY_REQUEST_ID_TIMEOUT,
    INITIAL_REQUEST_READ_CHUNK_BYTES, INITIAL_REQUEST_TIMEOUT, MAX_ACTIVE_CONNECTIONS,
    MAX_INITIAL_REQUEST_BYTES, ORDINARY_REQUEST_TIMEOUT, STREAM_WRITE_TIMEOUT,
};
use crate::schema::{
    ErrorResponse, Method, MethodTraits, Request, ResponseResult, ServerCapabilities,
    SuccessResponse,
};
use crate::{ApiRequestMessage, ApiRequestSender, socket_path};
use shepr_platform::ipc::{
    HangupWait, LocalStream, ShutdownTrigger, ShutdownWatch, SocketFileIdentity, SocketStartupLock,
    bind_private_socket, is_connection_closed_error, peer_is_same_user,
    remove_socket_file_if_owned, set_local_stream_polling, shutdown_pipe, socket_file_identity,
    wait_local_stream_hangup,
};
use shepr_platform::ssh_agent::SshAgentLease;

const ORDINARY_REQUEST_TIMEOUT_MESSAGE: &str =
    "timed out waiting for app response; the request may still run, so its outcome is unknown";

struct ConnectionAdmission {
    active: Arc<AtomicUsize>,
}

impl ConnectionAdmission {
    fn try_acquire(active: &Arc<AtomicUsize>) -> Option<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_ACTIVE_CONNECTIONS).then_some(count + 1)
            })
            .ok()?;
        Some(Self {
            active: Arc::clone(active),
        })
    }
}

impl Drop for ConnectionAdmission {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

pub struct ServerHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    identity: SocketFileIdentity,
    running: Arc<AtomicBool>,
    /// Dropped first in `drop`: wakes every connection thread holding a
    /// long-lived connection (the SSH agent leases) so it ends at once.
    shutdown: Option<ShutdownTrigger>,
    // Declared last so it is released only after `drop` has removed the
    // socket file and joined the listener: a racing server cannot claim the
    // path while this one still owns it.
    _startup_lock: SocketStartupLock,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        drop(self.shutdown.take());

        // The listener thread only looks at `running` after an accept returns,
        // so without a wake-up it would sit in `accept` holding the listening
        // fd until the process exits. Connect to it once while the socket file
        // is still ours; the accept returns, the thread sees `running` false
        // and exits, dropping the listener. Only then is joining safe.
        let woke = self.wake_listener();

        if let Err(err) = self.remove_socket_file_if_owned()
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), error = %err, "failed to remove api socket on shutdown");
        }

        if let Some(thread) = self.thread.take() {
            if woke {
                // Bounded by one accept-failure backoff (at most a second).
                if thread.join().is_err() {
                    warn!("api listener thread panicked");
                }
            } else {
                debug!("api listener not woken; leaving its thread to process exit");
            }
        }
    }
}

impl ServerHandle {
    pub fn remove_socket_file_if_owned(&self) -> std::io::Result<()> {
        remove_socket_file_if_owned(&self.path, &self.identity)
    }

    /// Unblocks the listener's `accept` with a throwaway connection. Skipped
    /// when the socket path no longer names this listener's socket (removed,
    /// or replaced by another server), since connecting would then reach
    /// somebody else. Returns whether the wake-up connection was made.
    fn wake_listener(&self) -> bool {
        let ours = socket_file_identity(&self.path).is_ok_and(|found| found == self.identity);
        if !ours {
            return false;
        }
        match shepr_platform::ipc::connect_local_stream(&self.path) {
            Ok(_stream) => true,
            Err(err) => {
                debug!(error = %err, "could not wake api listener for shutdown");
                false
            }
        }
    }
}

pub fn start_server_with_stop_control(
    api_tx: ApiRequestSender,
    server_stop: Arc<crate::ServerStopSignal>,
    paths: &shepr_config::AppPaths,
) -> std::io::Result<ServerHandle> {
    let inherited_agent = shepr_platform::ssh_agent::inherited_agent_socket()?;
    start_server_inner(
        api_tx,
        default_capabilities(),
        Some(server_stop),
        inherited_agent,
        paths,
    )
}

fn default_capabilities() -> Option<ServerCapabilities> {
    Some(ServerCapabilities {
        ssh_agent_registration: false,
    })
}

fn start_server_inner(
    api_tx: ApiRequestSender,
    mut capabilities: Option<ServerCapabilities>,
    server_stop: Option<Arc<crate::ServerStopSignal>>,
    inherited_agent: Option<PathBuf>,
    paths: &shepr_config::AppPaths,
) -> std::io::Result<ServerHandle> {
    let path = socket_path(paths);
    // Made before the bind, so its failure leaves no socket file behind.
    let (shutdown, shutdown_watch) = shutdown_pipe()?;
    let (listener, startup_lock, identity) = bind_private_socket(&path)?;
    info!(path = %path.display(), "api server listening");

    let ssh_agents = match shepr_platform::ssh_agent::SshAgentRegistry::new(
        shepr_platform::ssh_agent::socket_path(&crate::socket_path(paths)),
        inherited_agent,
    ) {
        Ok(registry) => Some(registry),
        Err(error) => {
            // Setup failure disables SSH agent registration for this server process.
            warn!(%error, "SSH agent refresh unavailable; retaining inherited pane environment");
            None
        }
    };

    if let Some(capabilities) = capabilities.as_mut() {
        capabilities.ssh_agent_registration = ssh_agents.is_some();
    }

    let running = Arc::new(AtomicBool::new(true));
    let listener_running = Arc::clone(&running);
    let active_connections = Arc::new(AtomicUsize::new(0));
    let connection_admission = Arc::clone(&active_connections);
    // The listener thread must outlive any single accept or spawn failure.
    // Nothing restarts it, and while the client socket stays up the server
    // looks alive to autodetection, so a dead API listener leaves a server
    // that refuses attaches (its status probe fails) and every CLI call and
    // agent hook fails until someone kills it by hand. Transient errors such
    // as EMFILE/ENFILE (one fd and thread per connection, plus PTYs) or
    // ECONNABORTED are therefore logged and retried with a bounded backoff.
    let busy_refuser = spawn_busy_refuser();
    let thread = spawn_listener_thread(listener, listener_running, move |stream| {
        // Dropping the stream closes the refused connection; that is not an
        // accept failure, so it does not feed the backoff.
        match peer_is_same_user(&stream) {
            Ok(true) => {}
            Ok(false) => {
                warn!("api connection from another user refused");
                return Ok(());
            }
            Err(err) => {
                warn!(error = %err, "api connection peer credentials unavailable; refused");
                return Ok(());
            }
        }
        let Some(admission) = ConnectionAdmission::try_acquire(&connection_admission) else {
            hand_off_busy_connection(busy_refuser.as_ref(), stream);
            return Ok(());
        };
        let api_tx = api_tx.clone();
        let capabilities = capabilities.clone();
        let server_stop = server_stop.clone();
        let shutdown_watch = shutdown_watch.clone();
        let ssh_agents = ssh_agents.clone();
        // `std::thread::spawn` panics when the OS refuses a new thread, which
        // would take the listener down with it. On failure the closure (and
        // the accepted stream) is dropped, which closes that one connection;
        // the client sees EOF and the listener keeps serving.
        std::thread::Builder::new()
            .name("shepr-api-conn".into())
            .spawn(move || {
                let _admission = admission;
                if let Err(err) = handle_connection_with_stop(
                    stream,
                    &api_tx,
                    &shutdown_watch,
                    capabilities,
                    server_stop.as_ref(),
                    ssh_agents.as_ref(),
                ) {
                    warn!(error = %err, "api connection failed");
                }
            })
            .map(|_| ())
    });

    Ok(ServerHandle {
        thread: Some(thread),
        path,
        identity,
        running,
        shutdown: Some(shutdown),
        _startup_lock: startup_lock,
    })
}

/// Reads the bounded initial request line so server errors can preserve its ID.
fn request_id_from_line(line: &str) -> String {
    #[derive(serde::Deserialize)]
    struct RequestId {
        id: String,
    }

    if line.starts_with('{') {
        serde_json::from_str::<RequestId>(line).map_or_default(|request| request.id)
    } else {
        String::new()
    }
}

/// Starts the one thread that answers connections over the limit, so reading
/// their request IDs never holds up the accept loop. The thread ends when the
/// returned sender (owned by the listener) is dropped. `None` when the thread
/// could not be spawned; refusals then carry no request ID.
fn spawn_busy_refuser() -> Option<std::sync::mpsc::SyncSender<LocalStream>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<LocalStream>(BUSY_REFUSAL_QUEUE);
    let spawned = std::thread::Builder::new()
        .name("shepr-api-busy".into())
        .spawn(move || {
            for stream in rx {
                reject_busy_connection(stream);
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(err) => {
            warn!(error = %err, "api busy refuser thread unavailable; refusals carry no request id");
            None
        }
    }
}

/// Called on the accept loop for a connection over the limit: queue it for
/// the refuser thread, or refuse it at once when that queue is full.
fn hand_off_busy_connection(
    refuser: Option<&std::sync::mpsc::SyncSender<LocalStream>>,
    stream: LocalStream,
) {
    let Some(refuser) = refuser else {
        send_busy_refusal(stream, "");
        return;
    };
    match refuser.try_send(stream) {
        Ok(()) => {}
        Err(
            std::sync::mpsc::TrySendError::Full(stream)
            | std::sync::mpsc::TrySendError::Disconnected(stream),
        ) => send_busy_refusal(stream, ""),
    }
}

/// Refuses a connection over the limit, echoing the caller's request ID when
/// its request line arrives within a short bound. Runs on the refuser thread.
fn reject_busy_connection(mut stream: LocalStream) {
    // clock-io-ok: the bound covers a real socket read of the request line.
    let deadline = Instant::now() + BUSY_REQUEST_ID_TIMEOUT;
    let request_id = match read_request_line_until(&mut stream, deadline) {
        Ok(Some(line)) => request_id_from_line(line.trim()),
        Ok(None) => String::new(),
        Err(error) => {
            debug!(%error, "could not read api request id for connection limit refusal");
            String::new()
        }
    };
    send_busy_refusal(stream, &request_id);
}

fn send_busy_refusal(mut stream: LocalStream, request_id: &str) {
    let response = error_response_json(
        request_id,
        crate::error::ApiErrorCode::EndpointBusy,
        format!("API server is at its limit of {MAX_ACTIVE_CONNECTIONS} active connections"),
    );
    if let Err(err) = write_text_line_allow_disconnect(&mut stream, &response.body) {
        debug!(error = %err, "failed to send API connection limit refusal");
    }
}

/// Runs the accept loop on its own thread, handing each accepted connection
/// to `serve`, whose error (a failed thread spawn) feeds the backoff.
fn spawn_listener_thread(
    listener: shepr_platform::ipc::LocalListener,
    running: Arc<AtomicBool>,
    mut serve: impl FnMut(LocalStream) -> io::Result<()> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut backoff = AcceptBackoff::default();
        for stream in listener.incoming() {
            // Checked for every accept outcome, errors included, so the
            // shutdown wake-up in `ServerHandle::drop` ends the loop even
            // while accepts are failing.
            if !running.load(Ordering::Acquire) {
                break;
            }
            match stream {
                Ok(stream) => match serve(stream) {
                    Ok(()) => backoff.recovered(),
                    Err(err) => backoff.failed("api connection thread spawn failed", &err),
                },
                Err(err) => backoff.failed("api listener accept failed", &err),
            }
        }
        debug!("api server thread exiting");
    })
}

/// Retry pacing for the API listener after an accept or spawn failure.
///
/// Errors like EMFILE persist until some fd is released, and a blocking
/// `accept` returns them immediately, so retrying without a pause would spin
/// a core. The delay doubles per consecutive failure up to a cap and resets on
/// the next successful accept. Only the first failure of a streak is logged at
/// error level, so a long outage does not flood the log.
#[derive(Default)]
struct AcceptBackoff {
    delay: Option<Duration>,
    failures: u64,
}

impl AcceptBackoff {
    fn failed(&mut self, what: &'static str, err: &io::Error) {
        self.failures = self.failures.saturating_add(1);
        if self.failures == 1 {
            error!(error = %err, "{what}; retrying");
        } else {
            debug!(error = %err, failures = self.failures, "{what}; retrying");
        }
        let delay = self
            .delay
            .map_or(ACCEPT_BACKOFF_MIN, |delay| delay.saturating_mul(2))
            .min(ACCEPT_BACKOFF_MAX);
        self.delay = Some(delay);
        std::thread::sleep(delay);
    }

    fn recovered(&mut self) {
        if self.failures > 0 {
            info!(
                failures = self.failures,
                "api listener recovered after accept failures"
            );
        }
        self.delay = None;
        self.failures = 0;
    }
}

fn handle_connection_with_stop(
    mut stream: LocalStream,
    api_tx: &ApiRequestSender,
    shutdown: &ShutdownWatch,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<crate::ServerStopSignal>>,
    ssh_agents: Option<&shepr_platform::ssh_agent::SshAgentRegistry>,
) -> std::io::Result<()> {
    if let Err(err) = stream.set_send_timeout(Some(STREAM_WRITE_TIMEOUT)) {
        debug!(error = %err, "api connection write timeout unavailable");
    }

    let Some(line) = read_initial_request_line(&mut stream)? else {
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
            let response = ErrorResponse {
                id,
                error: crate::error::ApiError::new(
                    crate::error::ApiErrorCode::InvalidRequest,
                    format!("invalid request: {request_error}"),
                )
                .into_body(),
            };
            write_api_json_line_allow_disconnect(&mut stream, &response.id, &response)?;
            return Ok(());
        }
    };

    let request_id = request.id.clone();
    let method_traits = request.method.traits();
    crate::logging::api_request_started(
        &request_id,
        method_traits.name,
        method_traits.mutates_ui,
        method_traits.routine,
    );

    // Socket-thread methods bypass `handle_request`, so gate them here too.
    if let Some(response) = shutdown_rejection(&request, server_stop) {
        return finish_api_response(&mut stream, &request_id, method_traits, &response);
    }

    // Requests sent to the app loop are handled there. The method facts are
    // the single routing classification; this thread only handles methods
    // whose response or connection lifetime belongs here.
    if !method_traits.runs_on_socket_thread {
        let response = handle_request(request, api_tx, capabilities, server_stop);
        return finish_api_response(&mut stream, &request_id, method_traits, &response);
    }

    match request.method {
        Method::ServerSshAgentRegister(params) => {
            let lease = ssh_agents
                .ok_or_else(|| io::Error::other("SSH agent registration is unavailable"))
                .and_then(|registry| registry.register(PathBuf::from(params.socket_path)));
            let lease = match lease {
                Ok(lease) => lease,
                Err(error) => {
                    return write_text_line_allow_disconnect(
                        &mut stream,
                        &error_response_json(
                            &request_id,
                            if error.kind() == io::ErrorKind::InvalidInput {
                                crate::error::ApiErrorCode::InvalidSshAgent
                            } else {
                                crate::error::ApiErrorCode::SshAgentUnavailable
                            },
                            error.to_string(),
                        )
                        .body,
                    );
                }
            };
            write_api_json_line(
                &mut stream,
                &request_id,
                &SuccessResponse {
                    id: request_id.clone(),
                    result: ResponseResult::Ok {},
                },
            )?;
            // The lease lasts as long as the connection: it ends when the
            // client hangs up or the API server shuts down (its handle drops
            // after the final session save, so panes keep a working agent
            // through a stop's drain). Between those the wait wakes only to
            // re-probe agent liveness, because SSH can unlink an inherited
            // socket after its bridge's lease closes and nothing announces it.
            loop {
                match wait_local_stream_hangup(&stream, shutdown, SshAgentLease::REFRESH_INTERVAL)?
                {
                    HangupWait::PeerClosed | HangupWait::Shutdown => break,
                    HangupWait::Elapsed => lease.refresh()?,
                }
            }
            Ok(())
        }
        method_body => {
            let response = handle_request(
                Request {
                    id: request_id.clone(),
                    method: method_body,
                },
                api_tx,
                capabilities,
                server_stop,
            );
            finish_api_response(&mut stream, &request_id, method_traits, &response)
        }
    }
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
        Ok(()) => response.outcome.as_str(),
        Err(err) if is_connection_closed_error(&err) => "client_disconnected",
        Err(err) => {
            crate::logging::api_request_failed(request_id, method.name, &err.to_string());
            return Err(err);
        }
    };
    crate::logging::api_request_completed(
        request_id,
        method.name,
        method.mutates_ui,
        method.routine,
        outcome,
    );
    Ok(())
}

fn handle_request(
    request: Request,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<crate::ServerStopSignal>>,
) -> crate::error::EncodedApiResponse {
    if matches!(&request.method, Method::Ping(_)) {
        let response = SuccessResponse {
            id: request.id.clone(),
            result: ResponseResult::Pong {
                version: shepr_protocol::build_version(),
                build_id: shepr_protocol::BUILD_ID.to_owned(),
                boot_id: shepr_protocol::BootId::for_this_process().to_string(),
                capabilities,
            },
        };
        return crate::serialize_response_or_error_with_outcome(&request.id, &response);
    }

    if matches!(&request.method, Method::ClientShellSurfaceSet(_)) {
        return error_response_json(
            &request.id,
            crate::error::ApiErrorCode::ConnectionLocalOnly,
            "client_shell.surface.set is only available through a client shell endpoint".into(),
        );
    }

    if let Method::ServerStop(params) = &request.method {
        if let Some(server_stop) = server_stop {
            // A stop aimed at one boot must not stop another: the caller
            // observed that instance, and the occupant may have been replaced
            // since. The refusal leaves this server running.
            if let Some(expected) = &params.expected_boot_id {
                let actual = shepr_protocol::BootId::for_this_process();
                if actual != expected.as_str() {
                    return error_response_json(
                        &request.id,
                        crate::error::ApiErrorCode::ServerBootMismatch,
                        format!(
                            "refusing to stop: this server is boot {actual}, not the expected boot {expected}"
                        ),
                    );
                }
            }
            server_stop.request();
            let response = SuccessResponse {
                id: request.id.clone(),
                result: ResponseResult::Ok {},
            };
            return crate::serialize_response_or_error_with_outcome(&request.id, &response);
        }
    } else if let Some(response) = shutdown_rejection(&request, server_stop) {
        return response;
    }

    dispatch_to_app(request, api_tx)
}

fn server_is_stopping(server_stop: Option<&Arc<crate::ServerStopSignal>>) -> bool {
    server_stop.is_some_and(|stop| stop.is_requested())
}

fn shutdown_rejection(
    request: &Request,
    server_stop: Option<&Arc<crate::ServerStopSignal>>,
) -> Option<crate::error::EncodedApiResponse> {
    if !server_is_stopping(server_stop)
        || matches!(
            &request.method,
            Method::Ping(_) | Method::ServerStop(_) | Method::ClientShellSurfaceSet(_)
        )
    {
        return None;
    }

    Some(error_response_json(
        &request.id,
        crate::error::ApiErrorCode::ServerUnavailable,
        "server is shutting down".into(),
    ))
}

pub fn api_method_name(method: &Method) -> &'static str {
    method.traits().name
}

fn read_initial_request_line(stream: &mut LocalStream) -> std::io::Result<Option<String>> {
    // clock-io-ok: the bound covers a real socket read of the request line.
    read_request_line_until(stream, Instant::now() + INITIAL_REQUEST_TIMEOUT)
}

/// Reads the connection's one request line with blocking reads bounded by an
/// overall deadline.
///
/// Blocking reads wake as soon as the client's bytes arrive, so a client that
/// writes just after connecting pays no poll interval, and a large request
/// costs one syscall per chunk rather than per byte. Reading in chunks can
/// consume bytes past the newline, which are dropped. The protocol is one
/// request per connection and no method reads a payload after its line: the
/// SSH-agent lease loop detects the peer's hang-up with a readiness check that
/// ignores unread bytes, and ends on EOF whether or not stray bytes preceded it.
fn read_request_line_until(
    stream: &mut LocalStream,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    set_local_stream_polling(stream, false)?;
    let result = read_request_line_blocking(stream, deadline);
    // Later phases arm their own modes; don't leave a stale receive timeout.
    // A read error takes precedence over a failure to clear it.
    let reset = stream.set_recv_timeout(None);
    let line = result?;
    reset?;
    Ok(line)
}

fn read_request_line_blocking(
    stream: &mut LocalStream,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    use std::io::Read as _;

    let mut reader = shepr_platform::ipc::DeadlineReader::new(stream, deadline);
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
        Err(err) if is_connection_closed_error(&err) => Ok(()),
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
        Err(err) if is_connection_closed_error(&err) => Ok(()),
        result => result,
    }
}

fn dispatch_to_app(
    request: Request,
    api_tx: &ApiRequestSender,
) -> crate::error::EncodedApiResponse {
    let request_id = request.id.clone();
    crate::error::encode_result_with_outcome(request_id, dispatch_to_app_result(request, api_tx))
}

fn dispatch_to_app_result(request: Request, api_tx: &ApiRequestSender) -> crate::error::ApiResult {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    if let Err(err) = api_tx.send(ApiRequestMessage {
        request,
        respond_to,
    }) {
        return Err(crate::error::ApiError::new(
            crate::error::ApiErrorCode::ServerUnavailable,
            format!("failed to dispatch request: {err}"),
        ));
    }

    match response_rx.recv_timeout(ORDINARY_REQUEST_TIMEOUT) {
        Ok(response) => response,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(crate::error::ApiError::new(
            crate::error::ApiErrorCode::Timeout,
            ORDINARY_REQUEST_TIMEOUT_MESSAGE,
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(crate::error::ApiError::new(
            crate::error::ApiErrorCode::ServerUnavailable,
            "request handling failed: app response channel closed",
        )),
    }
}

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

/// Serves one short-lived request; the shutdown watch is never waited on.
#[cfg(test)]
fn handle_connection(
    stream: LocalStream,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
) -> std::io::Result<()> {
    let (_shutdown, watch) = shutdown_pipe()?;
    handle_connection_with_stop(stream, api_tx, &watch, capabilities, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use shepr_test_support::{IsolatedEnv, ScratchDir};
    use std::fs;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use tokio::sync::mpsc;

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
        let server = listener.accept().expect("test precondition");
        (client, server)
    }

    #[test]
    fn connection_admission_caps_workers_and_releases_slots() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut admissions = (0..MAX_ACTIVE_CONNECTIONS)
            .map(|_| ConnectionAdmission::try_acquire(&active).expect("available slot"))
            .collect::<Vec<_>>();

        assert_eq!(active.load(Ordering::Acquire), MAX_ACTIVE_CONNECTIONS);
        assert!(ConnectionAdmission::try_acquire(&active).is_none());

        drop(admissions.pop());
        let replacement = ConnectionAdmission::try_acquire(&active).expect("released slot");
        assert_eq!(active.load(Ordering::Acquire), MAX_ACTIVE_CONNECTIONS);
        drop(replacement);
        drop(admissions);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_full_connection_limit_sends_endpoint_busy() {
        let (mut client, server) = local_stream_pair("connection-limit-refusal");
        client
            .write_all(br#"{"id":"busy-request","method":"ping","params":{}}"#)
            .expect("write busy request");
        client
            .write_all(b"\n")
            .expect("terminate busy request line");
        reject_busy_connection(server);

        let response: ErrorResponse =
            serde_json::from_str(&read_line(&mut client)).expect("valid refusal response");
        assert_eq!(response.id, "busy-request");
        assert_eq!(response.error.code, "endpoint_busy");
        assert!(response.error.message.contains("64 active connections"));
    }

    /// The accept loop never waits for a refused caller's request line: with
    /// the refuser's queue full the refusal goes out at once, without an ID.
    #[test]
    fn a_full_refusal_queue_refuses_at_once_without_reading() {
        let (mut client, server) = local_stream_pair("connection-limit-queue-full");
        let (refuser, _queue) = std::sync::mpsc::sync_channel::<LocalStream>(0);

        let started = Instant::now();
        hand_off_busy_connection(Some(&refuser), server);
        assert!(started.elapsed() < BUSY_REQUEST_ID_TIMEOUT);

        let response: ErrorResponse =
            serde_json::from_str(&read_line(&mut client)).expect("valid refusal response");
        assert_eq!(response.id, "");
        assert_eq!(response.error.code, "endpoint_busy");
    }

    /// A queued refusal is answered by the refuser thread with the caller's ID.
    #[test]
    fn the_busy_refuser_thread_echoes_the_request_id() {
        let (mut client, server) = local_stream_pair("connection-limit-refuser-thread");
        let refuser = spawn_busy_refuser().expect("spawn refuser");
        hand_off_busy_connection(Some(&refuser), server);
        client
            .write_all(b"{\"id\":\"queued-request\",\"method\":\"ping\",\"params\":{}}\n")
            .expect("write busy request");

        let response: ErrorResponse =
            serde_json::from_str(&read_line(&mut client)).expect("valid refusal response");
        assert_eq!(response.id, "queued-request");
        assert_eq!(response.error.code, "endpoint_busy");
    }

    #[test]
    fn request_line_arriving_after_connect_is_read_without_a_poll_delay() {
        let (mut client, mut server) = local_stream_pair("request-line-latency");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            client
                .write_all(br#"{"id":"late","method":"ping","#)
                .expect("test precondition");
            client.flush().expect("test precondition");
            std::thread::sleep(Duration::from_millis(20));
            client
                .write_all(b"\"params\":{}}\n")
                .expect("test precondition");
            client.flush().expect("test precondition");
            client
        });
        let started = Instant::now();
        let line = read_initial_request_line(&mut server)
            .expect("test precondition")
            .expect("request line");
        let elapsed = started.elapsed();
        let _client = writer.join().expect("test precondition");
        assert_eq!(
            line,
            "{\"id\":\"late\",\"method\":\"ping\",\"params\":{}}\n"
        );
        // The writer sleeps 40 ms in all. A 100 ms poll would have slept a
        // full interval after the first empty read, and again between the two
        // writes; blocking reads wake on arrival.
        assert!(
            elapsed < Duration::from_millis(100),
            "request line took {elapsed:?}"
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
    fn dropping_the_handle_stops_the_listener_thread() {
        let path = unique_test_path("listener-drop");
        let startup_lock =
            shepr_platform::ipc::acquire_socket_startup_lock(&path).expect("test precondition");
        let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
        let identity = socket_file_identity(&path).expect("test precondition");
        let running = Arc::new(AtomicBool::new(true));
        // The serve closure lives exactly as long as the listener thread.
        let alive = Arc::new(());
        let thread_alive = Arc::clone(&alive);
        let thread = spawn_listener_thread(listener, Arc::clone(&running), move |_stream| {
            let _ = &thread_alive;
            Ok(())
        });
        let handle = ServerHandle {
            thread: Some(thread),
            path: path.clone(),
            identity,
            running,
            shutdown: None,
            _startup_lock: startup_lock,
        };
        let refusal = shepr_platform::ipc::acquire_socket_startup_lock(&path)
            .err()
            .expect("a live handle keeps the socket path locked");
        let busy = shepr_platform::ipc::SocketBusy::from_io(&refusal)
            .expect("the refusal is a busy socket naming its path");
        assert_eq!(busy.path(), path);

        drop(handle);
        shepr_platform::ipc::acquire_socket_startup_lock(&path)
            .expect("the lock is released with the handle");

        assert_eq!(
            Arc::strong_count(&alive),
            1,
            "listener thread must have exited"
        );
        assert!(
            !path.try_exists().expect("stat socket file"),
            "socket file must be removed"
        );
    }

    struct LeaseFixture {
        _directory: ScratchDir,
        _agent_listener: UnixListener,
        stable: PathBuf,
        registry: shepr_platform::ssh_agent::SshAgentRegistry,
        client: LocalStream,
        worker: std::thread::JoinHandle<()>,
    }

    /// Registers an agent lease over a fresh connection served on a worker
    /// thread, and checks the success line and the published address.
    fn register_lease(name: &str, shutdown: ShutdownWatch) -> LeaseFixture {
        let directory = ScratchDir::new(name);
        let agent = directory.join("upstream");
        let agent_listener = UnixListener::bind(&agent).expect("test precondition");
        let stable = directory.join("stable");
        let registry = shepr_platform::ssh_agent::SshAgentRegistry::new(stable.clone(), None)
            .expect("test precondition");
        let (mut client, server) = local_stream_pair(name);
        let (tx, _rx) = mpsc::unbounded_channel();
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || {
            handle_connection_with_stop(server, &tx, &shutdown, None, None, Some(&worker_registry))
                .expect("test precondition");
        });
        let request = Request {
            id: "agent-lease".into(),
            method: Method::ServerSshAgentRegister(crate::schema::ServerSshAgentRegisterParams {
                socket_path: agent.to_string_lossy().into_owned(),
            }),
        };
        let encoded = serde_json::to_string(&request).expect("test precondition");
        write_text_line(&mut client, &encoded).expect("test precondition");
        let response: SuccessResponse =
            serde_json::from_str(&read_line(&mut client)).expect("test precondition");
        assert!(matches!(response.result, ResponseResult::Ok {}));
        assert_eq!(fs::read_link(&stable).expect("test precondition"), agent);
        LeaseFixture {
            _directory: directory,
            _agent_listener: agent_listener,
            stable,
            registry,
            client,
            worker,
        }
    }

    #[test]
    fn ssh_agent_registration_lasts_only_for_the_api_connection() {
        let (_shutdown, watch) = shutdown_pipe().expect("test precondition");
        let lease = register_lease("agent-lease", watch);
        drop(lease.client);
        lease.worker.join().expect("test precondition");
        assert!(!lease.stable.try_exists().expect("stat stable agent link"));
        drop(lease.registry);
    }

    /// Dropping the server handle's shutdown trigger ends a lease whose client
    /// is still connected, and the lease's address goes with it.
    #[test]
    fn ssh_agent_lease_ends_when_the_api_server_shuts_down() {
        let (shutdown, watch) = shutdown_pipe().expect("test precondition");
        let lease = register_lease("agent-lease-shutdown", watch);
        drop(shutdown);
        lease.worker.join().expect("test precondition");
        assert!(!lease.stable.try_exists().expect("stat stable agent link"));
        drop(lease.client);
        drop(lease.registry);
    }

    #[test]
    fn socket_path_prefers_explicit_env_override() {
        let env = IsolatedEnv::new();
        let unique = env.path().join("override.sock");
        env.set(shepr_core::env::EnvVar::SheprSocketPath, &unique);
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
        assert_eq!(socket_path(&paths), unique);
    }

    #[test]
    fn socket_path_defaults_to_runtime_dir() {
        let env = IsolatedEnv::new();
        env.set("XDG_RUNTIME_DIR", env.path().join("runtime"));
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        assert_eq!(socket_path(&paths), paths.runtime_dir().join("shepr.sock"));
    }

    #[test]
    fn api_socket_is_bound_owner_only() {
        let dir = ScratchDir::new("socket-perms");
        let path = dir.join("api.sock");
        let listener =
            shepr_platform::ipc::bind_private_local_listener(&path).expect("test precondition");

        let mode = fs::metadata(&path)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        drop(listener);
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
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        client
            .write_all(b"{\"id\":\"unknown\",\"method\":\"nope\",\"params\":{}}\n")
            .expect("test precondition");
        client.flush().expect("test precondition");

        handle_connection(server, &api_tx, None).expect("test precondition");

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
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        client
            .write_all(b"{\"id\":\"ordinary\",\"method\":\"ping\",\"params\":{}}\n")
            .expect("test precondition");
        client.flush().expect("test precondition");

        handle_connection(server, &api_tx, None).expect("test precondition");

        let response = read_line(&mut client);
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["id"], "ordinary");
        assert_eq!(response["result"]["type"], "pong");
    }

    #[test]
    fn ping_request_returns_pong() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let response = handle_request(
            Request {
                id: "req_1".into(),
                method: Method::Ping(crate::schema::PingParams::default()),
            },
            &tx,
            Some(ServerCapabilities {
                ssh_agent_registration: false,
            }),
            None,
        );

        let parsed: SuccessResponse =
            serde_json::from_str(&response.body).expect("test precondition");
        assert_eq!(parsed.id, "req_1");
        assert!(matches!(parsed.result, ResponseResult::Pong { .. }));
    }

    #[test]
    fn server_stop_control_bypasses_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(crate::ServerStopSignal::default());
        let response = handle_request(
            Request {
                id: "priority_stop".into(),
                method: Method::ServerStop(crate::schema::ServerStopParams::default()),
            },
            &tx,
            None,
            Some(&stop),
        );

        let response: serde_json::Value =
            serde_json::from_str(&response.body).expect("test precondition");
        assert_eq!(response["id"], "priority_stop");
        assert_eq!(response["result"]["type"], "ok");
        assert!(stop.is_requested());

        let rejected = handle_request(
            Request {
                id: "after_stop".into(),
                method: Method::SessionSnapshot(crate::schema::EmptyParams::default()),
            },
            &tx,
            None,
            Some(&stop),
        );
        let rejected: serde_json::Value =
            serde_json::from_str(&rejected.body).expect("test precondition");
        assert_eq!(rejected["error"]["code"], "server_unavailable");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ping_reports_the_boot_id_a_conditional_stop_must_match() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let ping = handle_request(
            Request {
                id: "ping".into(),
                method: Method::Ping(crate::schema::PingParams::default()),
            },
            &tx,
            None,
            None,
        );
        let ping: SuccessResponse = serde_json::from_str(&ping.body).expect("test precondition");
        let ResponseResult::Pong { boot_id, .. } = ping.result else {
            panic!("ping did not answer with a pong");
        };
        let stop_with = |expected_boot_id: Option<String>, stop: &Arc<crate::ServerStopSignal>| {
            let response = handle_request(
                Request {
                    id: "stop".into(),
                    method: Method::ServerStop(crate::schema::ServerStopParams {
                        expected_boot_id,
                    }),
                },
                &tx,
                None,
                Some(stop),
            );
            serde_json::from_str::<serde_json::Value>(&response.body).expect("test precondition")
        };

        let other_boot = Arc::new(crate::ServerStopSignal::default());
        let refused = stop_with(Some(format!("{boot_id}0")), &other_boot);
        assert_eq!(refused["error"]["code"], "server_boot_mismatch");
        assert!(!other_boot.is_requested());

        let this_boot = Arc::new(crate::ServerStopSignal::default());
        let stopped = stop_with(Some(boot_id), &this_boot);
        assert_eq!(stopped["result"]["type"], "ok");
        assert!(this_boot.is_requested());
    }

    #[test]
    fn request_dispatches_to_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let request = Request {
            id: "req_2".into(),
            method: Method::SessionSnapshot(crate::schema::EmptyParams::default()),
        };

        let request_for_thread = request.clone();
        let thread =
            std::thread::spawn(move || handle_request(request_for_thread, &tx, None, None));

        let msg = rx.blocking_recv().expect("test precondition");
        assert_eq!(msg.request.id, "req_2");
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
                "mine",
            ),
            (r#"{"id":"escaped\"id","method":"unknown"}"#, "escaped\"id"),
            (r#"{"method":"unknown","params":{"id":"nested"}}"#, ""),
            (r#"{"id":123,"method":"unknown"}"#, ""),
            (
                r#"{"id":"first","id":"second","method":"ping","params":{}}"#,
                "",
            ),
            (r#"{"id":"truncated","method":"ping""#, ""),
            (r#"["not-an-object"]"#, ""),
        ];
        for (request, expected_id) in cases {
            let (api_tx, mut api_rx) = mpsc::unbounded_channel();
            let (mut client, server) = local_stream_pair("invalid-request-id");
            writeln!(client, "{request}").expect("test precondition");
            handle_connection(server, &api_tx, None).expect("test precondition");

            let mut response = String::new();
            BufReader::new(client)
                .read_to_string(&mut response)
                .expect("test precondition");
            let response: ErrorResponse =
                serde_json::from_str(&response).expect("test precondition");
            assert_eq!(response.id, expected_id, "{request}");
            assert_eq!(response.error.code, "invalid_request");
            assert!(response.error.message.starts_with("invalid request: "));
            assert!(
                api_rx.try_recv().is_err(),
                "invalid requests must not dispatch"
            );
        }
    }
}
