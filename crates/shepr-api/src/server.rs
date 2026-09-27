use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::{ListenerExt as _, Stream as _};
use tracing::{debug, error, info, warn};

#[cfg(test)]
use std::fs;

use crate::schema::{
    ErrorResponse, Method, MethodTraits, Request, ResponseResult, ServerCapabilities,
    SuccessResponse,
};
use crate::subscriptions::{ActiveSubscription, SubscriptionStream};
use crate::wait::{prompt_agent, wait_for_agent, wait_for_event, wait_for_output};
use crate::{ApiRequestMessage, ApiRequestSender, EventHub, socket_path};
use shepr_platform::ipc::{
    LocalStream, SocketFileIdentity, SocketStartupLock, acquire_socket_startup_lock,
    bind_private_local_listener, is_connection_closed_error, local_stream_peer_closed,
    peer_is_same_user, remove_socket_file_if_owned, set_local_stream_polling, socket_file_identity,
};

#[cfg(test)]
mod subscription_socket_tests;

pub(super) const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const APP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on how long an ordinary (non-wait, non-stream) request waits for the
/// app main loop to answer. Without one, a stalled main loop hangs every CLI
/// call and every agent hook that shells out to the CLI.
///
/// Most requests are answered in the same loop turn. The slowest legitimate
/// case is a `pane.read`/`agent.read` of alternate-screen history, which the
/// server serves by scrolling the agent and can take up to 20 s (15 s harvest
/// plus 5 s restore in `src/server/alt_screen_read.rs`), and a second read of
/// the same pane is parked until the first finishes. A minute covers that
/// with margin. Requests that carry their own timeout (`events.wait`,
/// `agent.wait`, `pane.wait_for_output`, `agent.prompt` with `wait`) are
/// dispatched on their own paths and are not subject to this bound.
pub(super) const ORDINARY_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const ORDINARY_REQUEST_TIMEOUT_MESSAGE: &str =
    "timed out waiting for app response; the request may still run, so its outcome is unknown";
const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_INITIAL_REQUEST_BYTES: usize = 1024 * 1024;

pub struct ServerHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    identity: SocketFileIdentity,
    running: Arc<AtomicBool>,
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
            warn!(path = %self.path.display(), err = %err, "failed to remove api socket on shutdown");
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
                debug!(err = %err, "could not wake api listener for shutdown");
                false
            }
        }
    }
}

pub fn start_server_with_stop_control(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    server_stop: Arc<AtomicBool>,
    paths: &shepr_config::AppPaths,
) -> std::io::Result<ServerHandle> {
    start_server_inner(
        api_tx,
        event_hub,
        default_capabilities(),
        Some(server_stop),
        paths,
    )
}

fn default_capabilities() -> Option<ServerCapabilities> {
    Some(ServerCapabilities {
        detached_server_daemon: shepr_platform::current_process_is_detached_server_daemon(),
        ssh_agent_registration: false,
    })
}

fn start_server_inner(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    mut capabilities: Option<ServerCapabilities>,
    server_stop: Option<Arc<AtomicBool>>,
    paths: &shepr_config::AppPaths,
) -> std::io::Result<ServerHandle> {
    let path = socket_path(paths);
    // Held for the server lifetime. A second server on the same path fails
    // here with `AddrInUse` instead of racing `prepare_socket_path`, which
    // could otherwise unlink a socket whose owner has not started listening.
    let startup_lock = acquire_socket_startup_lock(&path)?;
    prepare_socket_path(&path)?;

    // Owner-only from the moment the path is reachable (see
    // `bind_private_local_listener`); peers are also checked by uid on accept.
    let listener = bind_private_local_listener(&path)?;
    let identity = socket_file_identity(&path)?;
    info!(path = %path.display(), "api server listening");

    let ssh_agents = match shepr_platform::ssh_agent::SshAgentRegistry::new(
        shepr_platform::ssh_agent::socket_path(&crate::socket_path(paths)),
        std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
    ) {
        Ok(registry) => Some(registry),
        Err(error) => {
            warn!(%error, "SSH agent refresh unavailable; retaining inherited pane environment");
            None
        }
    };

    if let Some(capabilities) = capabilities.as_mut() {
        capabilities.ssh_agent_registration = ssh_agents.is_some();
    }

    let running = Arc::new(AtomicBool::new(true));
    let listener_running = Arc::clone(&running);
    // The listener thread must outlive any single accept or spawn failure.
    // Nothing restarts it, and while the client socket stays up the server
    // looks alive to autodetection, so a dead API listener leaves a server
    // that refuses attaches (its status probe fails) and every CLI call and
    // agent hook fails until someone kills it by hand. Transient errors such
    // as EMFILE/ENFILE (one fd and thread per subscription, plus PTYs) or
    // ECONNABORTED are therefore logged and retried with a bounded backoff.
    let connection_running = Arc::clone(&running);
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
                warn!(err = %err, "api connection peer credentials unavailable; refused");
                return Ok(());
            }
        }
        let api_tx = api_tx.clone();
        let event_hub = event_hub.clone();
        let capabilities = capabilities.clone();
        let server_stop = server_stop.clone();
        let connection_running = Arc::clone(&connection_running);
        let ssh_agents = ssh_agents.clone();
        // `std::thread::spawn` panics when the OS refuses a new thread, which
        // would take the listener down with it. On failure the closure (and
        // the accepted stream) is dropped, which closes that one connection;
        // the client sees EOF and the listener keeps serving.
        std::thread::Builder::new()
            .name("shepr-api-conn".into())
            .spawn(move || {
                if let Err(err) = handle_connection_with_stop(
                    stream,
                    &api_tx,
                    &event_hub,
                    &connection_running,
                    capabilities,
                    server_stop.as_ref(),
                    ssh_agents.as_ref(),
                ) {
                    warn!(err = %err, "api connection failed");
                }
            })
            .map(|_| ())
    });

    Ok(ServerHandle {
        thread: Some(thread),
        path,
        identity,
        running,
        _startup_lock: startup_lock,
    })
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

const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

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
            error!(err = %err, "{what}; retrying");
        } else {
            debug!(err = %err, failures = self.failures, "{what}; retrying");
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

fn prepare_socket_path(path: &Path) -> std::io::Result<()> {
    shepr_platform::ipc::prepare_socket_path(path, |path| {
        format!(
            "shepr is already running (socket busy at {})",
            path.display()
        )
    })
}

#[cfg(test)]
fn handle_connection(
    stream: LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
) -> std::io::Result<()> {
    handle_connection_with_stop(stream, api_tx, event_hub, running, capabilities, None, None)
}

fn handle_connection_with_stop(
    mut stream: LocalStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
    ssh_agents: Option<&shepr_platform::ssh_agent::SshAgentRegistry>,
) -> std::io::Result<()> {
    if let Err(err) = stream.set_send_timeout(Some(STREAM_WRITE_TIMEOUT)) {
        debug!(err = %err, "api connection write timeout unavailable");
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
            #[derive(serde::Deserialize)]
            struct RequestId {
                id: String,
            }
            let id = if line.starts_with('{') {
                serde_json::from_str::<RequestId>(line)
                    .map(|request| request.id)
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let response = ErrorResponse {
                id,
                error: crate::error::ApiError::new(
                    crate::error::ApiErrorCode::InvalidRequest,
                    format!("invalid request: {request_error}"),
                )
                .into_body(),
            };
            write_json_line_allow_disconnect(&mut stream, &response)?;
            return Ok(());
        }
    };

    let request_id = request.id.clone();
    let method_traits = request.method.traits();
    shepr_platform::logging::api_request_started(
        &request_id,
        method_traits.name,
        method_traits.mutates_ui,
        method_traits.routine,
    );

    // Socket-thread waits and streams bypass `handle_request`, so gate them here too.
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
                                "invalid_ssh_agent"
                            } else {
                                "ssh_agent_unavailable"
                            },
                            error.to_string(),
                        ),
                    );
                }
            };
            write_json_line(
                &mut stream,
                &SuccessResponse {
                    id: request_id,
                    result: ResponseResult::Ok {},
                },
            )?;
            while running.load(Ordering::Relaxed) && !server_is_stopping(server_stop) {
                if local_stream_peer_closed(&stream)? {
                    break;
                }
                // SSH can unlink an inherited socket after its bridge's lease closes.
                lease.refresh()?;
                std::thread::sleep(CONNECTION_POLL_INTERVAL);
            }
            Ok(())
        }
        Method::EventsSubscribe(params) => {
            let result = stream_subscriptions(
                stream,
                request_id.clone(),
                params,
                api_tx,
                event_hub,
                running,
                server_stop,
            );
            match &result {
                Ok(()) => shepr_platform::logging::api_request_completed(
                    &request_id,
                    method_traits.name,
                    method_traits.mutates_ui,
                    method_traits.routine,
                    "stream_closed",
                ),
                Err(err) => {
                    shepr_platform::logging::api_request_failed(
                        &request_id,
                        method_traits.name,
                        &err.to_string(),
                    );
                }
            }
            result
        }
        Method::EventsWait(params) => {
            let response = wait_for_event(
                request_id.clone(),
                params,
                &mut stream,
                api_tx,
                event_hub,
                running,
                server_stop,
            )?;
            finish_wait_response(&mut stream, response, &request_id, method_traits)
        }
        Method::AgentPrompt(params) => {
            let response = prompt_agent(
                request_id.clone(),
                params,
                &mut stream,
                api_tx,
                event_hub,
                running,
                server_stop,
            )?;
            finish_wait_response(&mut stream, response, &request_id, method_traits)
        }
        Method::AgentWait(params) => {
            let response = wait_for_agent(
                request_id.clone(),
                params,
                &mut stream,
                api_tx,
                event_hub,
                running,
                server_stop,
            )?;
            finish_wait_response(&mut stream, response, &request_id, method_traits)
        }
        Method::PaneWaitForOutput(params) => {
            let response = wait_for_output(
                request_id.clone(),
                &params,
                &mut stream,
                api_tx,
                running,
                server_stop,
            )?;
            finish_wait_response(&mut stream, response, &request_id, method_traits)
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

fn finish_wait_response(
    stream: &mut LocalStream,
    response: Option<String>,
    request_id: &str,
    method: MethodTraits,
) -> std::io::Result<()> {
    let Some(response) = response else {
        shepr_platform::logging::api_request_completed(
            request_id,
            method.name,
            method.mutates_ui,
            method.routine,
            "client_disconnected",
        );
        return Ok(());
    };
    finish_api_response(stream, request_id, method, &response)
}

fn finish_api_response(
    stream: &mut LocalStream,
    request_id: &str,
    method: MethodTraits,
    response: &str,
) -> std::io::Result<()> {
    let result = write_text_line_allow_disconnect(stream, response);
    match &result {
        Ok(()) => shepr_platform::logging::api_request_completed(
            request_id,
            method.name,
            method.mutates_ui,
            method.routine,
            api_response_outcome(response),
        ),
        Err(err) => {
            shepr_platform::logging::api_request_failed(request_id, method.name, &err.to_string());
        }
    }
    result
}

fn handle_request(
    request: Request,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<&Arc<AtomicBool>>,
) -> String {
    if matches!(&request.method, Method::Ping(_)) {
        let response = SuccessResponse {
            id: request.id.clone(),
            result: ResponseResult::Pong {
                version: shepr_protocol::build_version(),
                build_id: shepr_protocol::BUILD_ID.to_owned(),
                capabilities,
            },
        };
        return crate::serialize_response_or_error(&request.id, &response);
    }

    if matches!(&request.method, Method::ClientShellSurfaceSet(_)) {
        return error_response_json(
            &request.id,
            crate::error::ApiErrorCode::ConnectionLocalOnly,
            "client_shell.surface.set is only available through a client shell endpoint".into(),
        );
    }

    if matches!(&request.method, Method::ServerStop(_)) {
        if let Some(server_stop) = server_stop {
            server_stop.store(true, Ordering::Release);
            let response = SuccessResponse {
                id: request.id.clone(),
                result: ResponseResult::Ok {},
            };
            return crate::serialize_response_or_error(&request.id, &response);
        }
    } else if let Some(response) = shutdown_rejection(&request, server_stop) {
        return response;
    }

    dispatch_to_app(
        request,
        api_tx,
        Some(ORDINARY_REQUEST_TIMEOUT),
        Some((
            crate::error::ApiErrorCode::Timeout,
            ORDINARY_REQUEST_TIMEOUT_MESSAGE,
        )),
    )
}

pub(super) fn server_is_stopping(server_stop: Option<&Arc<AtomicBool>>) -> bool {
    server_stop.is_some_and(|stop| stop.load(Ordering::Acquire))
}

fn shutdown_rejection(request: &Request, server_stop: Option<&Arc<AtomicBool>>) -> Option<String> {
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

fn api_response_outcome(response: &str) -> &'static str {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(response) else {
        return "error";
    };

    match value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(|code| code.as_str())
    {
        Some("timeout") => "timeout",
        Some(_) => "error",
        None => "ok",
    }
}

fn read_initial_request_line(stream: &mut LocalStream) -> std::io::Result<Option<String>> {
    read_request_line_until(stream, Instant::now() + INITIAL_REQUEST_TIMEOUT)
}

/// Reads the connection's one request line with blocking reads bounded by an
/// overall deadline.
///
/// Blocking reads wake as soon as the client's bytes arrive, so a client that
/// writes just after connecting pays no poll interval, and a large request
/// costs one syscall per chunk rather than per byte. Reading in chunks can
/// consume bytes past the newline, which are dropped. The protocol is one
/// request per connection and no method reads a payload after its line:
/// subscription and wait loops detect the peer's hang-up with a readiness
/// check that ignores unread bytes, and the SSH-agent lease loop ends on EOF
/// whether or not stray bytes preceded it.
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
    let mut chunk = [0u8; 8 * 1024];
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

fn stream_subscriptions(
    mut stream: LocalStream,
    request_id: String,
    params: crate::schema::EventsSubscribeParams,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    server_stop: Option<&Arc<AtomicBool>>,
) -> std::io::Result<()> {
    let event_start_sequence = event_hub.current_sequence();
    let mut subscriptions = Vec::with_capacity(params.subscriptions.len());
    for (index, subscription) in params.subscriptions.into_iter().enumerate() {
        let active = match ActiveSubscription::new(
            subscription,
            &request_id,
            index,
            api_tx,
            event_hub,
            event_start_sequence,
        ) {
            Ok(active) => active,
            Err(mut response) => {
                response.id = request_id;
                if let Err(err) = write_json_line(&mut stream, &response) {
                    if is_connection_closed_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
                return Ok(());
            }
        };
        subscriptions.push(active);
    }

    if let Err(err) = write_json_line(
        &mut stream,
        &SuccessResponse {
            id: request_id.clone(),
            result: ResponseResult::SubscriptionStarted {},
        },
    ) {
        if is_connection_closed_error(&err) {
            return Ok(());
        }
        return Err(err);
    }

    // Polled as one stream so events go out in the hub's global order.
    let mut subscriptions = SubscriptionStream::new(subscriptions, event_start_sequence);
    loop {
        if server_is_stopping(server_stop) || should_stop_connection(&mut stream, running)? {
            return Ok(());
        }

        let batch = subscriptions.poll(api_tx, event_hub);
        for event in batch.events {
            if server_is_stopping(server_stop) || should_stop_connection(&mut stream, running)? {
                return Ok(());
            }
            if let Err(err) = write_json_line(&mut stream, &event) {
                if is_connection_closed_error(&err) {
                    return Ok(());
                }
                return Err(err);
            }
        }
        if let Some(error) = batch.error {
            write_json_line_allow_disconnect(
                &mut stream,
                &ErrorResponse {
                    id: request_id,
                    error,
                },
            )?;
            return Ok(());
        }
        std::thread::sleep(CONNECTION_POLL_INTERVAL);
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

fn write_json_line<T: serde::Serialize>(
    stream: &mut LocalStream,
    value: &T,
) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    write_text_line(stream, &encoded)
}

fn write_json_line_allow_disconnect<T: serde::Serialize>(
    stream: &mut LocalStream,
    value: &T,
) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    write_text_line_allow_disconnect(stream, &encoded)
}

pub(super) fn should_stop_connection(
    stream: &mut LocalStream,
    running: &Arc<AtomicBool>,
) -> std::io::Result<bool> {
    if !running.load(Ordering::Relaxed) {
        return Ok(true);
    }

    local_stream_peer_closed(stream)
}

pub(super) fn dispatch_to_app_with_timeout_result(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
) -> crate::error::ApiResult {
    dispatch_to_app_result(request, api_tx, timeout, None)
}

#[cfg(test)]
pub(super) fn dispatch_to_app_with_caller_timeout(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
) -> String {
    dispatch_to_app(
        request,
        api_tx,
        timeout,
        Some((
            crate::error::ApiErrorCode::Timeout,
            "timed out waiting for agent status",
        )),
    )
}

pub(super) fn dispatch_to_app_with_caller_timeout_result(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
) -> crate::error::ApiResult {
    dispatch_to_app_result(
        request,
        api_tx,
        timeout,
        Some((
            crate::error::ApiErrorCode::Timeout,
            "timed out waiting for agent status",
        )),
    )
}

fn dispatch_to_app(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
    timeout_response: Option<(crate::error::ApiErrorCode, &str)>,
) -> String {
    let request_id = request.id.clone();
    crate::error::encode_result(
        request_id,
        dispatch_to_app_result(request, api_tx, timeout, timeout_response),
    )
}

fn dispatch_to_app_result(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
    timeout_response: Option<(crate::error::ApiErrorCode, &str)>,
) -> crate::error::ApiResult {
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

    let response = match timeout {
        Some(timeout) => response_rx.recv_timeout(timeout).map_err(|err| match err {
            std::sync::mpsc::RecvTimeoutError::Timeout => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for app response after {} ms",
                    timeout.as_millis()
                ),
            ),
            std::sync::mpsc::RecvTimeoutError::Disconnected => std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "app response channel closed",
            ),
        }),
        None => response_rx
            .recv()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err)),
    };

    match response {
        Ok(response) => response,
        Err(err) => {
            if err.kind() == std::io::ErrorKind::TimedOut
                && let Some((code, message)) = timeout_response
            {
                return Err(crate::error::ApiError::new(code, message));
            }
            Err(crate::error::ApiError::new(
                crate::error::ApiErrorCode::ServerUnavailable,
                format!("request handling failed: {err}"),
            ))
        }
    }
}

/// Error text for a socket-thread wait that ended because shutdown started.
const SHUTDOWN_WAIT_MESSAGE: &str =
    "server is shutting down; the wait ended before its condition was met";

pub(super) fn shutdown_wait_error() -> crate::error::ApiError {
    crate::error::ApiError::new(
        crate::error::ApiErrorCode::ServerUnavailable,
        SHUTDOWN_WAIT_MESSAGE,
    )
}

/// Dispatch without a deadline, but stop waiting for the answer once server
/// shutdown starts. The app may still act on the request (a queued prompt
/// can be typed), so the shutdown error says the outcome is unknown.
pub(super) fn dispatch_to_app_until_stopped_result(
    request: Request,
    api_tx: &ApiRequestSender,
    server_stop: Option<&Arc<AtomicBool>>,
) -> crate::error::ApiResult {
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
    loop {
        match response_rx.recv_timeout(CONNECTION_POLL_INTERVAL) {
            Ok(response) => return response,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if server_is_stopping(server_stop) {
                    return Err(crate::error::ApiError::new(
                        crate::error::ApiErrorCode::ServerUnavailable,
                        "server is shutting down; the request may still run, so its outcome is unknown",
                    ));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(crate::error::ApiError::new(
                    crate::error::ApiErrorCode::ServerUnavailable,
                    "request handling failed: app response channel closed",
                ));
            }
        }
    }
}

#[cfg(test)]
#[test]
fn stop_aware_dispatch_ends_when_shutdown_starts() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(true));
    let result = dispatch_to_app_until_stopped_result(
        Request {
            id: "prompt".into(),
            method: Method::AgentPrompt(crate::schema::AgentPromptParams {
                target: "reviewer".into(),
                text: "review this".into(),
                wait: None,
            }),
        },
        &tx,
        Some(&stop),
    );
    let error = result.expect_err("shutdown ends the dispatch");
    assert_eq!(error.code, crate::error::ApiErrorCode::ServerUnavailable);
}

#[cfg(test)]
#[test]
fn caller_timeout_dispatch_uses_timeout_error() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = dispatch_to_app_with_caller_timeout(
        Request {
            id: "prompt-timeout".into(),
            method: Method::AgentPrompt(crate::schema::AgentPromptParams {
                target: "reviewer".into(),
                text: "review this".into(),
                wait: None,
            }),
        },
        &tx,
        Some(Duration::ZERO),
    );
    let error: ErrorResponse = serde_json::from_str(&response).expect("test precondition");
    assert_eq!(error.error.code, "timeout");
}

pub(super) fn error_response_json(
    id: &str,
    code: impl Into<crate::error::ApiErrorCode>,
    message: String,
) -> String {
    crate::error::encode_result(
        id.to_owned(),
        Err(crate::error::ApiError::new(code.into(), message)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use shepr_test_support::{IsolatedEnv, ScratchDir};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use tokio::sync::mpsc;

    /// A fresh path in a scratch directory that is kept until the test
    /// process exits; every caller here removes what it creates itself.
    fn unique_test_path(name: &str) -> PathBuf {
        ScratchDir::new(name).keep_until_exit().join("s")
    }

    fn read_line(stream: &mut LocalStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("test precondition");
        line
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, PathBuf) {
        let path = unique_test_path(name);
        let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
        let client = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        (client, server, path)
    }

    #[test]
    fn request_line_arriving_after_connect_is_read_without_a_poll_delay() {
        let (mut client, mut server, path) = local_stream_pair("request-line-latency");
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
        // Polling slept a full interval after the first empty read, and again
        // between the two writes; blocking reads wake on arrival.
        assert!(
            elapsed < CONNECTION_POLL_INTERVAL,
            "request line took {elapsed:?}"
        );
        fs::remove_file(path).expect("test precondition");
    }

    #[test]
    fn request_line_read_honours_its_deadline_and_size_limit() {
        let (_client, mut server, path) = local_stream_pair("request-line-deadline");
        let error =
            read_request_line_until(&mut server, Instant::now() + Duration::from_millis(30))
                .expect_err("silent client must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        fs::remove_file(path).expect("test precondition");

        let (mut client, mut server, path) = local_stream_pair("request-line-oversize");
        let writer = std::thread::spawn(move || {
            // The server stops reading at the limit, so the tail of this write
            // may fail once it closes; only the server's verdict matters.
            let _ = client.write_all(&vec![b'x'; MAX_INITIAL_REQUEST_BYTES + 1]);
        });
        let error = read_request_line_until(&mut server, Instant::now() + Duration::from_secs(5))
            .expect_err("oversized line must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(server);
        writer.join().expect("test precondition");
        fs::remove_file(path).expect("test precondition");

        let (client, mut server, path) = local_stream_pair("request-line-eof");
        drop(client);
        assert!(
            read_request_line_until(&mut server, Instant::now() + Duration::from_secs(5))
                .expect("test precondition")
                .is_none()
        );
        fs::remove_file(path).expect("test precondition");
    }

    #[test]
    fn dropping_the_handle_stops_the_listener_thread() {
        let path = unique_test_path("listener-drop");
        let startup_lock = acquire_socket_startup_lock(&path).expect("test precondition");
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
            _startup_lock: startup_lock,
        };
        assert_eq!(
            acquire_socket_startup_lock(&path)
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::AddrInUse),
            "a live handle keeps the socket path locked"
        );

        drop(handle);
        acquire_socket_startup_lock(&path).expect("the lock is released with the handle");

        assert_eq!(
            Arc::strong_count(&alive),
            1,
            "listener thread must have exited"
        );
        assert!(!path.exists(), "socket file must be removed");
    }

    #[test]
    fn ssh_agent_registration_lasts_only_for_the_api_connection() {
        let directory = unique_test_path("agent-lease");
        fs::create_dir(&directory).expect("test precondition");
        let agent = directory.join("upstream");
        let _agent = UnixListener::bind(&agent).expect("test precondition");
        let stable = directory.join("stable");
        let registry = shepr_platform::ssh_agent::SshAgentRegistry::new(stable.clone(), None)
            .expect("test precondition");
        let (mut client, server, api_path) = local_stream_pair("agent-api");
        let (tx, _rx) = mpsc::unbounded_channel();
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || {
            handle_connection_with_stop(
                server,
                &tx,
                &EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
                None,
                Some(&worker_registry),
            )
            .expect("test precondition");
        });
        write_json_line(
            &mut client,
            &Request {
                id: "agent-lease".into(),
                method: Method::ServerSshAgentRegister(
                    crate::schema::ServerSshAgentRegisterParams {
                        socket_path: agent.to_string_lossy().into_owned(),
                    },
                ),
            },
        )
        .expect("test precondition");
        let response: SuccessResponse =
            serde_json::from_str(&read_line(&mut client)).expect("test precondition");
        assert!(matches!(response.result, ResponseResult::Ok {}));
        assert_eq!(fs::read_link(&stable).expect("test precondition"), agent);
        drop(client);
        worker.join().expect("test precondition");
        assert!(!stable.exists());
        drop(registry);
        fs::remove_file(api_path).expect("test precondition");
        fs::remove_dir_all(directory).expect("test precondition");
    }

    fn pane_info(
        pane_id: &str,
        agent_status: crate::schema::AgentStatus,
    ) -> crate::schema::PaneInfo {
        crate::schema::PaneInfo {
            pane_id: pane_id.into(),
            terminal_id: "term_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: Some("pi".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll: None,
            revision: 0,
        }
    }

    fn spawn_pane_get_responder(
        agent_status: crate::schema::AgentStatus,
    ) -> (ApiRequestSender, std::thread::JoinHandle<()>) {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            while let Some(msg) = api_rx.blocking_recv() {
                match msg.request.method {
                    Method::PaneGet(_) => msg
                        .respond_to
                        .send(Ok(ResponseResult::PaneInfo {
                            pane: pane_info("pane_1", agent_status),
                        }))
                        .expect("test precondition"),
                    Method::EventsWait(_) => msg
                        .respond_to
                        .send(Err(crate::error::ApiError::new(
                            crate::error::ApiErrorCode::External("unexpected_dispatch".into()),
                            "events.wait should be handled by the api server",
                        )))
                        .expect("test precondition"),
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        });
        (api_tx, responder)
    }

    #[test]
    fn socket_path_prefers_explicit_env_override() {
        let env = IsolatedEnv::new();
        let unique = env.path().join("override.sock");
        env.set(shepr_config::SOCKET_PATH_ENV_VAR, &unique);
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
    fn socket_path_uses_named_session_dir() {
        let env = IsolatedEnv::new();
        env.set(shepr_config::SESSION_ENV_VAR, "work");
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");

        let expected = paths
            .runtime_dir()
            .join("sessions")
            .join("work")
            .join("shepr.sock");
        assert_eq!(socket_path(&paths), expected);
    }

    #[test]
    fn api_socket_is_bound_owner_only() {
        let dir = ScratchDir::new("socket-perms");
        let path = dir.join("api.sock");
        let listener = bind_private_local_listener(&path).expect("test precondition");

        let mode = fs::metadata(&path)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        drop(listener);
    }

    #[test]
    fn api_response_outcome_uses_top_level_error_shape() {
        let ok_with_error_text = r#"{"id":"req","result":{"read":{"text":"user said \"error\": \"timeout\"","revision":1}}}"#;
        assert_eq!(api_response_outcome(ok_with_error_text), "ok");

        let timeout = r#"{"id":"req","error":{"code":"timeout","message":"timed out waiting for output match"}}"#;
        assert_eq!(api_response_outcome(timeout), "timeout");

        let generic_error =
            r#"{"id":"req","error":{"code":"server_unavailable","message":"boom"}}"#;
        assert_eq!(api_response_outcome(generic_error), "error");
    }

    #[test]
    fn unknown_method_returns_invalid_request_response() {
        let (mut client, server, _path) = local_stream_pair("unknown-api-request");
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        client
            .write_all(b"{\"id\":\"unknown\",\"method\":\"nope\",\"params\":{}}\n")
            .expect("test precondition");
        client.flush().expect("test precondition");

        handle_connection(
            server,
            &api_tx,
            &EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
        )
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
        let (mut client, server, _path) = local_stream_pair("ordinary-api-request");
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        client
            .write_all(b"{\"id\":\"ordinary\",\"method\":\"ping\",\"params\":{}}\n")
            .expect("test precondition");
        client.flush().expect("test precondition");

        handle_connection(
            server,
            &api_tx,
            &EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
        )
        .expect("test precondition");

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
                detached_server_daemon: true,
                ssh_agent_registration: false,
            }),
            None,
        );

        let parsed: SuccessResponse = serde_json::from_str(&response).expect("test precondition");
        assert_eq!(parsed.id, "req_1");
        assert!(matches!(parsed.result, ResponseResult::Pong { .. }));
    }

    #[test]
    fn server_stop_control_bypasses_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let response = handle_request(
            Request {
                id: "priority_stop".into(),
                method: Method::ServerStop(crate::schema::EmptyParams::default()),
            },
            &tx,
            None,
            Some(&stop),
        );

        let response: serde_json::Value =
            serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response["id"], "priority_stop");
        assert_eq!(response["result"]["type"], "ok");
        assert!(stop.load(Ordering::Acquire));

        let rejected = handle_request(
            Request {
                id: "after_stop".into(),
                method: Method::WorkspaceList(crate::schema::EmptyParams::default()),
            },
            &tx,
            None,
            Some(&stop),
        );
        let rejected: serde_json::Value =
            serde_json::from_str(&rejected).expect("test precondition");
        assert_eq!(rejected["error"]["code"], "server_unavailable");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn request_dispatches_to_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let request = Request {
            id: "req_2".into(),
            method: Method::WorkspaceList(crate::schema::EmptyParams::default()),
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
        let parsed: SuccessResponse = serde_json::from_str(&response).expect("test precondition");
        assert_eq!(parsed.id, "req_2");
    }

    #[test]
    fn events_wait_agent_status_returns_initial_match() {
        let (api_tx, responder) = spawn_pane_get_responder(crate::schema::AgentStatus::Blocked);

        let (mut client, server, _path) = local_stream_pair("api-events-wait-initial");
        client
            .write_all(br#"{"id":"wait_1","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"blocked"},"timeout_ms":1000}}"#)
            .expect("test precondition");
        client.write_all(b"\n").expect("test precondition");
        client.flush().expect("test precondition");

        let running = Arc::new(AtomicBool::new(true));
        let event_hub = EventHub::default();
        handle_connection(server, &api_tx, &event_hub, &running, None).expect("test precondition");

        let response: serde_json::Value =
            serde_json::from_str(&read_line(&mut client)).expect("test precondition");
        assert_eq!(response["id"], "wait_1");
        assert_eq!(response["result"]["type"], "wait_matched");
        assert_eq!(
            response["result"]["event"]["data"]["agent_status"],
            "blocked"
        );
        drop(api_tx);
        responder.join().expect("test precondition");
    }

    #[test]
    fn events_wait_agent_status_times_out_server_side() {
        let (api_tx, responder) = spawn_pane_get_responder(crate::schema::AgentStatus::Idle);

        let (mut client, server, _path) = local_stream_pair("api-events-wait-timeout");
        client
            .write_all(br#"{"id":"wait_2","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"blocked"},"timeout_ms":30}}"#)
            .expect("test precondition");
        client.write_all(b"\n").expect("test precondition");
        client.flush().expect("test precondition");

        let running = Arc::new(AtomicBool::new(true));
        let event_hub = EventHub::default();
        handle_connection(server, &api_tx, &event_hub, &running, None).expect("test precondition");

        let response: serde_json::Value =
            serde_json::from_str(&read_line(&mut client)).expect("test precondition");
        assert_eq!(response["id"], "wait_2");
        assert_eq!(response["error"]["code"], "timeout");
        assert_eq!(
            response["error"]["message"],
            "timed out waiting for event match"
        );
        drop(api_tx);
        responder.join().expect("test precondition");
    }

    #[test]
    fn events_wait_agent_status_returns_not_found_when_pane_closes() {
        let event_hub = EventHub::default();
        let responder_event_hub = event_hub.clone();
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let responder = std::thread::spawn(move || {
            let mut pane_get_count = 0;
            while let Some(msg) = api_rx.blocking_recv() {
                let Method::PaneGet(_) = msg.request.method else {
                    panic!("unexpected request: {:?}", msg.request.method);
                };
                pane_get_count += 1;
                let response = if pane_get_count == 1 {
                    Ok(ResponseResult::PaneInfo {
                        pane: pane_info("pane_1", crate::schema::AgentStatus::Idle),
                    })
                } else {
                    if pane_get_count == 2 {
                        responder_event_hub.push(crate::schema::EventEnvelope {
                            data: crate::schema::EventData::PaneClosed {
                                pane_id: "pane_1".into(),
                                workspace_id: "ws_1".into(),
                            },
                        });
                    }
                    Err(crate::error::ApiError::new(
                        crate::error::ApiErrorCode::PaneNotFound,
                        "pane pane_1 not found",
                    ))
                };
                msg.respond_to.send(response).expect("test precondition");
            }
        });

        let (mut client, server, _path) = local_stream_pair("wait-close");
        client
            .write_all(br#"{"id":"wait_close","method":"events.wait","params":{"match_event":{"event":"pane_agent_status_changed","pane_id":"pane_1","agent_status":"blocked"},"timeout_ms":500}}"#)
            .expect("test precondition");
        client.write_all(b"\n").expect("test precondition");
        client.flush().expect("test precondition");

        let running = Arc::new(AtomicBool::new(true));
        handle_connection(server, &api_tx, &event_hub, &running, None).expect("test precondition");

        let response: serde_json::Value =
            serde_json::from_str(&read_line(&mut client)).expect("test precondition");
        assert_eq!(response["id"], "wait_close");
        assert_eq!(response["error"]["code"], "pane_not_found");
        assert_eq!(response["error"]["message"], "pane pane_1 not found");
        drop(api_tx);
        responder.join().expect("test precondition");
    }

    #[test]
    fn wait_for_output_stops_when_client_disconnects() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (first_read_tx, first_read_rx) = std::sync::mpsc::channel();
        let responder = std::thread::spawn(move || {
            let mut notified = false;
            while let Some(msg) = api_rx.blocking_recv() {
                assert!(matches!(msg.request.method, Method::PaneRead(_)));
                if !notified {
                    first_read_tx.send(()).expect("test precondition");
                    notified = true;
                }
                msg.respond_to
                    .send(Ok(ResponseResult::PaneRead {
                        read: crate::schema::PaneReadResult {
                            pane_id: "pane_1".into(),
                            workspace_id: "ws_1".into(),
                            tab_id: "tab_1".into(),
                            source: crate::schema::ReadSource::RecentUnwrapped,
                            format: crate::schema::ReadFormat::Text,
                            text: String::new(),
                            revision: 0,
                            truncated: false,
                        },
                    }))
                    .expect("test precondition");
            }
        });

        let (mut client, server, _path) = local_stream_pair("api-wait-disconnect");
        client
            .write_all(br#"{"id":"req_wait","method":"pane.wait_for_output","params":{"pane_id":"pane_1","source":"recent","match":{"type":"substring","value":"never"}}}"#)
            .expect("test precondition");
        client.write_all(b"\n").expect("test precondition");
        client.flush().expect("test precondition");

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).expect("test precondition");
        });

        first_read_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition");
        drop(client);

        let result = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition");
        assert!(result.is_ok());

        server_thread.join().expect("test precondition");
        drop(running);
        responder.join().expect("test precondition");
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
            let (mut client, server, path) = local_stream_pair("invalid-request-id");
            writeln!(client, "{request}").expect("test precondition");
            let running = Arc::new(AtomicBool::new(true));
            handle_connection(server, &api_tx, &EventHub::default(), &running, None)
                .expect("test precondition");

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
            fs::remove_file(path).expect("test precondition");
        }
    }

    #[test]
    fn subscription_setup_errors_preserve_request_id_and_reject_entire_stream() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let event_hub = EventHub::default();
        let responder_event_hub = event_hub.clone();
        let responder = std::thread::spawn(move || {
            let msg = api_rx.blocking_recv().expect("test precondition");
            let Method::PaneGet(params) = msg.request.method else {
                panic!("unexpected request: {:?}", msg.request.method);
            };
            assert_eq!(params.pane_id, "w999:p9");
            responder_event_hub.push(crate::schema::EventEnvelope {
                data: crate::schema::EventData::PaneClosed {
                    pane_id: "w999:p9".into(),
                    workspace_id: "w999".into(),
                },
            });
            msg.respond_to
                .send(Err(crate::error::ApiError::new(
                    crate::error::ApiErrorCode::PaneNotFound,
                    "pane w999:p9 not found",
                )))
                .expect("test precondition");
            assert!(
                api_rx.blocking_recv().is_none(),
                "rejection must not start polling"
            );
        });
        let (mut client, server, path) = local_stream_pair("subscription-error-id");
        let request = r#"{"id":"panefold:events","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"},{"type":"pane.closed"},{"type":"pane.agent_status_changed","pane_id":"w999:p9"}]}}"#;
        writeln!(client, "{request}").expect("test precondition");
        let running = Arc::new(AtomicBool::new(true));
        handle_connection(server, &api_tx, &event_hub, &running, None).expect("test precondition");
        drop(api_tx);
        responder.join().expect("test precondition");

        let mut response = String::new();
        BufReader::new(client)
            .read_to_string(&mut response)
            .expect("test precondition");
        let response: ErrorResponse = serde_json::from_str(&response).expect("test precondition");
        assert_eq!(response.id, "panefold:events");
        assert_eq!(response.error.code, "pane_not_found");
        assert_eq!(response.error.message, "pane w999:p9 not found");
        fs::remove_file(path).expect("test precondition");
    }

    #[test]
    fn subscriptions_stop_when_client_disconnects() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-disconnect");
        client
            .write_all(
                br#"{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .expect("test precondition");
        client.write_all(b"\n").expect("test precondition");
        client.flush().expect("test precondition");

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).expect("test precondition");
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).expect("test precondition");
        assert_eq!(ack["result"]["type"], "subscription_started");
        assert_eq!(ack["id"], "sub_1");

        drop(client);

        let result = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition");
        assert!(result.is_ok());
        server_thread.join().expect("test precondition");
    }

    #[test]
    fn subscriptions_stop_when_server_shuts_down() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server, _path) = local_stream_pair("api-sub-shutdown");
        client
            .write_all(
                br#"{"id":"sub_2","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .expect("test precondition");
        client.write_all(b"\n").expect("test precondition");
        client.flush().expect("test precondition");

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(server, &api_tx, &event_hub, &server_running, None);
            done_tx.send(result).expect("test precondition");
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).expect("test precondition");
        assert_eq!(ack["result"]["type"], "subscription_started");

        running.store(false, Ordering::Relaxed);

        let result = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition");
        assert!(result.is_ok());
        server_thread.join().expect("test precondition");
    }
}
