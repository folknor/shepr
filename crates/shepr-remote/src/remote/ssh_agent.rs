use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shepr_api::client::{ApiClientError, parse_response_value};
use shepr_api::schema::{Method, Request, ResponseResult, ServerSshAgentRegisterParams};
use shepr_platform::ipc::{LocalStream, LocalStreamRead, LocalStreamReadCount};

use crate::limits::{
    SSH_AGENT_INITIAL_RETRY_DELAY, SSH_AGENT_MAX_RETRY_DELAY, SSH_AGENT_REGISTRATION_TIMEOUT,
    SSH_AGENT_RESPONSE_MAX_BYTES, SSH_AGENT_RESPONSE_POLL_INTERVAL, SSH_AGENT_RETRY_BACKOFF_FACTOR,
    SSH_AGENT_STREAM_POLL_INTERVAL,
};

pub(super) struct Registration {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Registration {
    pub(super) fn start(paths: &shepr_config::AppPaths) -> Option<Self> {
        // Unset or empty means no agent to register. The bridge has no launch
        // to fail, so a refused value is logged and registers nothing.
        let path = shepr_platform::ssh_agent::inherited_agent_socket().unwrap_or_else(|error| {
            tracing::warn!(%error, "SSH agent refresh unavailable");
            None
        })?;
        Self::start_at(path, shepr_api::socket_path(paths))
    }

    fn start_at(path: PathBuf, socket_path: PathBuf) -> Option<Self> {
        let mut stream = match connect(&path, &socket_path) {
            Ok(None) => return None,
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "SSH agent refresh unavailable; retrying while attached");
                None
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            // A single byte probe is enough while monitoring the registration stream;
            // buffering more would consume and discard extra data.
            let mut byte = [0];
            let mut retry_delay = SSH_AGENT_INITIAL_RETRY_DELAY;
            let mut failures: u32 = 0;
            while !worker_stop.load(Ordering::Acquire) {
                if let Some(connection) = stream.as_mut() {
                    if matches!(
                        shepr_platform::ipc::poll_local_stream_read(connection, &mut byte),
                        Ok(LocalStreamRead::Pending)
                    ) {
                        std::thread::park_timeout(SSH_AGENT_STREAM_POLL_INTERVAL);
                        continue;
                    }
                    stream = None;
                    retry_delay = SSH_AGENT_INITIAL_RETRY_DELAY;
                }

                // Retry both initial API readiness and connections lost during handoff.
                // There is no attempt budget on purpose: the worker lives exactly as long
                // as the bridge attachment (Drop stops and unparks it), the delay is capped,
                // and a server that comes up late should still get the agent.
                match connect(&path, &socket_path) {
                    Ok(None) => break,
                    Ok(Some(connection)) => {
                        if failures > 0 {
                            tracing::info!(
                                api_socket = %socket_path.display(),
                                agent_socket = %path.display(),
                                failures,
                                "SSH agent registration restored after failed attempts"
                            );
                        }
                        failures = 0;
                        stream = Some(connection);
                        retry_delay = SSH_AGENT_INITIAL_RETRY_DELAY;
                        std::thread::park_timeout(SSH_AGENT_STREAM_POLL_INTERVAL);
                    }
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        let wait = retry_delay;
                        retry_delay = next_retry_delay(retry_delay);
                        // A missing API server is often permanent for the bridge's
                        // life, so log when a failure streak starts and when the
                        // delay settles at its ceiling, not on every retry.
                        if wait == SSH_AGENT_INITIAL_RETRY_DELAY
                            || (wait < SSH_AGENT_MAX_RETRY_DELAY
                                && retry_delay == SSH_AGENT_MAX_RETRY_DELAY)
                        {
                            tracing::debug!(
                                %error,
                                retry_after_ms = wait.as_millis(),
                                "SSH agent registration retry failed; retrying while attached"
                            );
                        }
                        std::thread::park_timeout(wait);
                    }
                }
            }
        });
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // Wake a worker that is in its capped retry wait so bridge teardown is prompt.
            thread.thread().unpark();
            if thread.join().is_err() {
                // The panic itself went to the panic hook; this names what stopped.
                tracing::error!("SSH agent registration thread panicked");
            }
        }
    }
}

fn next_retry_delay(current: Duration) -> Duration {
    current
        .saturating_mul(SSH_AGENT_RETRY_BACKOFF_FACTOR)
        .min(SSH_AGENT_MAX_RETRY_DELAY)
}

fn connect(path: &Path, socket_path: &Path) -> io::Result<Option<LocalStream>> {
    let timeout = SSH_AGENT_REGISTRATION_TIMEOUT;
    let status = shepr_api::read_runtime_status_at(socket_path, timeout)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotConnected,
            "SSH agent status API is not ready",
        )
    })?;
    if !status
        .capabilities
        .is_some_and(|capabilities| capabilities.ssh_agent_registration)
    {
        return Ok(None);
    }
    let path = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "SSH agent path is not valid UTF-8",
        )
    })?;
    let mut stream = shepr_platform::ipc::connect_trusted_local_stream(socket_path)?;
    let request = Request {
        id: "remote:ssh-agent".into(),
        method: Method::ServerSshAgentRegister(ServerSshAgentRegisterParams {
            socket_path: path.to_owned(),
        }),
    };
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    shepr_platform::ipc::set_local_stream_polling(&mut stream, true)?;
    // clock-io-ok: the response deadline begins after socket setup and request IO.
    let deadline = Instant::now() + timeout;
    let mut response = Vec::new();
    // Single byte reads stop at the response newline without consuming later stream data.
    let mut byte = [0];
    loop {
        // clock-io-ok: each poll and response read consumes real wall time.
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH agent registration did not complete",
            ));
        }
        match shepr_platform::ipc::poll_local_stream_read_count(&mut stream, &mut byte)? {
            LocalStreamReadCount::Data(_) if byte[0] == b'\n' => {
                let response = match parse_response_value(serde_json::from_slice(&response)?) {
                    Ok(response) => response,
                    Err(ApiClientError::ErrorResponse(response))
                        if response.error.code == "invalid_ssh_agent" =>
                    {
                        tracing::debug!(error = %response.error.message, "SSH agent registration rejected");
                        return Ok(None);
                    }
                    Err(error) => return Err(io::Error::other(error)),
                };
                return match response.result {
                    ResponseResult::Ok {} => Ok(Some(stream)),
                    _ => Err(io::Error::other(
                        "unexpected SSH agent registration response",
                    )),
                };
            }
            LocalStreamReadCount::Data(_) => {
                if response.len() == SSH_AGENT_RESPONSE_MAX_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "SSH agent registration response exceeded {SSH_AGENT_RESPONSE_MAX_BYTES} bytes"
                        ),
                    ));
                }
                response.push(byte[0]);
            }
            LocalStreamReadCount::Closed => {
                return Err(io::Error::other("SSH agent registration closed"));
            }
            LocalStreamReadCount::Pending => std::thread::sleep(SSH_AGENT_RESPONSE_POLL_INTERVAL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Generous upper bound for the registration worker to reach the fake
    /// API. The retry delay is capped at `SSH_AGENT_MAX_RETRY_DELAY`, so a working
    /// worker connects well inside it.
    const ACCEPT_LIMIT: Duration = Duration::from_secs(30);

    /// Accepts the next connection, failing the test if none arrives within
    /// `ACCEPT_LIMIT`. `brokkr check` has no per-test timeout, so a bare
    /// blocking `accept()` would turn a registration regression into a hung
    /// test run. `poll` returns as soon as a peer connects, so the passing
    /// case never waits on a fixed sleep or races a clock.
    fn accept_within_limit(listener: &UnixListener) -> UnixStream {
        let deadline = Instant::now() + ACCEPT_LIMIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "no connection within {ACCEPT_LIMIT:?}"
            );
            let timeout_ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
            let mut descriptor = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd that lives on this stack frame for the call;
            // the fd stays open because `listener` is borrowed.
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if ready > 0 {
                return listener.accept().expect("test precondition").0;
            }
            if ready < 0 {
                let error = io::Error::last_os_error();
                assert_eq!(error.kind(), io::ErrorKind::Interrupted, "poll: {error}");
            }
        }
    }

    #[test]
    fn retry_delay_grows_to_a_fixed_ceiling() {
        let mut delay = SSH_AGENT_INITIAL_RETRY_DELAY;
        while delay < SSH_AGENT_MAX_RETRY_DELAY {
            let next = next_retry_delay(delay);
            assert!(next > delay);
            delay = next;
        }
        assert_eq!(delay, SSH_AGENT_MAX_RETRY_DELAY);
        assert_eq!(next_retry_delay(delay), SSH_AGENT_MAX_RETRY_DELAY);
    }

    #[test]
    fn registration_stops_when_the_server_rejects_the_agent() {
        let scratch = shepr_test_support::ScratchDir::new("agent-rejected");
        let socket_path = scratch.join("api.sock");
        let listener = UnixListener::bind(&socket_path).expect("test precondition");
        let server = std::thread::spawn(move || {
            for expected in ["ping", "server.ssh_agent.register"] {
                let mut stream = accept_within_limit(&listener);
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("test precondition");
                let mut line = String::new();
                BufReader::new(&mut stream)
                    .read_line(&mut line)
                    .expect("test precondition");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("test precondition");
                assert_eq!(request["method"], expected);
                let response = if expected == "ping" {
                    serde_json::json!({"id": request["id"], "result": {
                        "type": "pong", "version": "test",
                        "build_id": shepr_protocol::BUILD_ID,
                        "capabilities": {"ssh_agent_registration": true}
                    }})
                } else {
                    serde_json::json!({"id": request["id"], "error": {
                        "code": "invalid_ssh_agent", "message": "invalid agent path"
                    }})
                };
                writeln!(stream, "{response}").expect("test precondition");
            }
        });
        let registration =
            Registration::start_at("relative-agent.sock".into(), socket_path.clone());
        server.join().expect("test precondition");
        std::fs::remove_file(socket_path).expect("test precondition");
        assert!(
            registration.is_none(),
            "a rejected agent must not start a retry worker"
        );
    }

    #[test]
    fn registration_rejects_a_response_larger_than_the_limit() {
        let scratch = shepr_test_support::ScratchDir::new("agent-response-too-large");
        let socket_path = scratch.join("api.sock");
        let listener = UnixListener::bind(&socket_path).expect("test precondition");
        let server = std::thread::spawn(move || {
            let mut stream = accept_within_limit(&listener);
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .expect("test precondition");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("test precondition");
            assert_eq!(request["method"], "ping");
            writeln!(
                stream,
                "{}",
                serde_json::json!({"id": request["id"], "result": {
                    "type": "pong", "version": "test",
                    "build_id": shepr_protocol::BUILD_ID,
                    "capabilities": {"ssh_agent_registration": true}
                }})
            )
            .expect("test precondition");

            let mut stream = accept_within_limit(&listener);
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .expect("test precondition");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("test precondition");
            assert_eq!(request["method"], "server.ssh_agent.register");
            stream
                .write_all(&vec![b'x'; SSH_AGENT_RESPONSE_MAX_BYTES + 1])
                .expect("test precondition");
        });

        let error = connect(Path::new("/test/agent.sock"), &socket_path)
            .expect_err("oversized response must fail");
        server.join().expect("test precondition");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("response exceeded 4096 bytes"));
    }

    #[test]
    fn registration_retries_when_the_api_is_initially_missing() {
        let scratch = shepr_test_support::ScratchDir::new("agent-retry");
        let socket_path = scratch.join("api.sock");
        let registration = Registration::start_at("/test/agent.sock".into(), socket_path.clone())
            .expect("missing API must not permanently disable registration");
        let listener = UnixListener::bind(&socket_path).expect("test precondition");
        for (attempt, expected) in [
            "ping",
            "server.ssh_agent.register",
            "ping",
            "server.ssh_agent.register",
        ]
        .into_iter()
        .enumerate()
        {
            let mut stream = accept_within_limit(&listener);
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("test precondition");
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .expect("test precondition");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("test precondition");
            assert_eq!(request["method"], expected);
            if attempt == 1 {
                writeln!(stream, "{}", serde_json::json!({"id": request["id"], "error": {
                    "code": "ssh_agent_unavailable", "message": "replacement server owns the address"
                }})).expect("test precondition");
                continue;
            }
            let result = if expected == "ping" {
                serde_json::json!({"type": "pong", "version": "test",
                    "build_id": shepr_protocol::BUILD_ID,
                    "capabilities": {"ssh_agent_registration": true}})
            } else {
                serde_json::json!({"type": "ok"})
            };
            writeln!(
                stream,
                "{}",
                serde_json::json!({"id": request["id"], "result": result})
            )
            .expect("test precondition");
            if expected == "server.ssh_agent.register" {
                drop(registration);
                assert_eq!(stream.read(&mut [0]).expect("test precondition"), 0);
                break;
            }
        }
        std::fs::remove_file(socket_path).expect("test precondition");
    }
}
