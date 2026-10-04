//! The remote-host side of a client's wait for a server: `shepr
//! remote-wait-for-server`, which a client runs over its SSH control
//! connection to a machine that is reachable but runs no server. It blocks
//! until a server answers on this host and then exits, so the client can
//! attach. It starts nothing.

use std::io;
use std::os::fd::RawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use shepr_launch::local_server;
use shepr_launch::status::ServerPresence;
use shepr_platform::{DirectoryWake, DirectoryWatch};

use crate::limits::{
    SERVER_WAIT_MAX, SERVER_WAIT_RECHECK, SERVER_WAIT_SETTLING_RECHECK,
    SERVER_WAIT_UNWATCHED_RECHECK,
};

/// Why a wait for a server ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerWaitEnd {
    /// A server answers as running.
    Ready,
    /// The client went away: its end of stdin closed.
    ClientGone,
    /// The wait reached its longest life without a server. The client checks
    /// the machine again and starts another wait.
    Expired,
}

/// Waits on this host until a server answers as running, its client closes
/// stdin, or the wait has run for its longest life. The runtime directory is
/// watched with inotify, so a server's socket ends the wait as soon as it
/// appears; the server is also checked on a slow timer, which covers a server
/// still starting when its socket appeared and a runtime directory that does
/// not exist yet.
pub fn wait_for_server(paths: &shepr_paths::AppPaths) -> io::Result<ServerWaitEnd> {
    let stdin = std::os::fd::AsRawFd::as_raw_fd(&std::io::stdin());
    wait_for_server_with(
        paths.runtime_dir(),
        stdin,
        || match local_server::server_presence(paths) {
            Ok(ServerPresence::Running(_)) => ServerSeen::Ready,
            Ok(ServerPresence::Gone) => ServerSeen::Absent,
            // A server still restoring, one going away, one not answering
            // yet and a socket that cannot be judged right now all settle
            // without touching the directory again.
            Ok(
                ServerPresence::Starting(_)
                | ServerPresence::Stopping(_)
                | ServerPresence::Unresponsive,
            )
            | Err(_) => ServerSeen::Settling,
        },
        WaitTimes {
            recheck: SERVER_WAIT_RECHECK,
            settling_recheck: SERVER_WAIT_SETTLING_RECHECK,
            unwatched_recheck: SERVER_WAIT_UNWATCHED_RECHECK,
            max: SERVER_WAIT_MAX,
        },
    )
}

/// What one check of the host's server found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerSeen {
    /// A server answers as running.
    Ready,
    /// Something is at the socket but not ready yet.
    Settling,
    /// No server.
    Absent,
}

#[derive(Clone, Copy)]
struct WaitTimes {
    /// Between checks while the directory is watched and no server is there.
    recheck: Duration,
    /// Between checks while a server is there but not ready.
    settling_recheck: Duration,
    /// Between checks while the directory cannot be watched (it does not
    /// exist yet).
    unwatched_recheck: Duration,
    max: Duration,
}

/// The wait itself, with what it watches, what it reads for the client going
/// away and how it checks the server handed in.
fn wait_for_server_with(
    dir: &Path,
    input: RawFd,
    mut check_server: impl FnMut() -> ServerSeen,
    times: WaitTimes,
) -> io::Result<ServerWaitEnd> {
    // clock-io-ok: the longest life bounds a real wait on another process.
    let started = Instant::now();
    let mut watch = None;
    loop {
        // The watch is set up before the check, so a socket that appears
        // between the two still wakes the next wait.
        if watch.is_none() {
            watch = DirectoryWatch::new(dir).ok();
        }
        let seen = check_server();
        if seen == ServerSeen::Ready {
            return Ok(ServerWaitEnd::Ready);
        }
        let Some(remaining) = times.max.checked_sub(started.elapsed()) else {
            return Ok(ServerWaitEnd::Expired);
        };
        let recheck = match (seen, &watch) {
            (ServerSeen::Settling, _) => times.settling_recheck,
            (_, Some(_)) => times.recheck,
            (_, None) => times.unwatched_recheck,
        };
        let wake = match &watch {
            Some(watch) => watch.wait(input, remaining.min(recheck))?,
            None => {
                if shepr_platform::poll_fd_readable(input, remaining.min(recheck))? {
                    DirectoryWake::Input
                } else {
                    DirectoryWake::TimedOut
                }
            }
        };
        if wake == DirectoryWake::Input && input_closed(input)? {
            return Ok(ServerWaitEnd::ClientGone);
        }
    }
}

/// Reads what the client sent, which carries nothing, and says whether its
/// end has closed.
fn input_closed(input: RawFd) -> io::Result<bool> {
    let mut discard = [0_u8; crate::limits::SERVER_WAIT_INPUT_DISCARD_BYTES];
    loop {
        match shepr_platform::read_fd(input, &mut discard) {
            Ok(read) => return Ok(read == 0),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // A reset or hung-up pipe is a client that went away too.
            Err(_) => return Ok(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(stream: &std::os::unix::net::UnixStream) -> RawFd {
        std::os::fd::AsRawFd::as_raw_fd(stream)
    }

    /// The slow recheck is made far longer than the test may take, so only the
    /// directory watch can end the wait in time.
    fn watched_times() -> WaitTimes {
        WaitTimes {
            recheck: Duration::from_secs(600),
            settling_recheck: Duration::from_secs(600),
            unwatched_recheck: Duration::from_secs(600),
            max: Duration::from_secs(1200),
        }
    }

    fn seen(ready: bool) -> ServerSeen {
        if ready {
            ServerSeen::Ready
        } else {
            ServerSeen::Absent
        }
    }

    #[test]
    fn the_wait_ends_when_a_server_socket_appears() {
        let scratch = shepr_test_support::ScratchDir::new("server-wait-socket");
        let (input, _client) = std::os::unix::net::UnixStream::pair().expect("an open input");
        let socket = scratch.join("server.sock");
        let binder = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                std::os::unix::net::UnixListener::bind(&socket).expect("bind the server socket")
            })
        };
        let started = Instant::now();
        let end = wait_for_server_with(
            scratch.path(),
            raw(&input),
            || seen(socket.try_exists().unwrap_or(false)),
            watched_times(),
        )
        .expect("the wait runs");
        let _listener = binder.join().expect("the binder finishes");
        assert_eq!(end, ServerWaitEnd::Ready);
        assert!(started.elapsed() < Duration::from_secs(60));
    }

    #[test]
    fn a_server_already_running_ends_the_wait_at_once() {
        let scratch = shepr_test_support::ScratchDir::new("server-wait-running");
        let (input, _client) = std::os::unix::net::UnixStream::pair().expect("an open input");
        let end = wait_for_server_with(
            scratch.path(),
            raw(&input),
            || ServerSeen::Ready,
            watched_times(),
        )
        .expect("the wait runs");
        assert_eq!(end, ServerWaitEnd::Ready);
    }

    #[test]
    fn a_settling_server_is_checked_again_soon() {
        let scratch = shepr_test_support::ScratchDir::new("server-wait-settling");
        let (input, _client) = std::os::unix::net::UnixStream::pair().expect("an open input");
        let mut checks = 0;
        let end = wait_for_server_with(
            scratch.path(),
            raw(&input),
            || {
                checks += 1;
                if checks > 2 {
                    ServerSeen::Ready
                } else {
                    ServerSeen::Settling
                }
            },
            WaitTimes {
                settling_recheck: Duration::from_millis(10),
                ..watched_times()
            },
        )
        .expect("the wait runs");
        assert_eq!(end, ServerWaitEnd::Ready);
    }

    #[test]
    fn the_wait_ends_when_its_client_goes_away() {
        let scratch = shepr_test_support::ScratchDir::new("server-wait-client-gone");
        let (input, client) = std::os::unix::net::UnixStream::pair().expect("an input pair");
        drop(client);
        let end = wait_for_server_with(
            scratch.path(),
            raw(&input),
            || ServerSeen::Absent,
            watched_times(),
        )
        .expect("the wait runs");
        assert_eq!(end, ServerWaitEnd::ClientGone);
    }

    #[test]
    fn a_missing_runtime_directory_still_ends_the_wait_on_its_recheck() {
        let scratch = shepr_test_support::ScratchDir::new("server-wait-no-dir");
        let (input, _client) = std::os::unix::net::UnixStream::pair().expect("an open input");
        let mut checks = 0;
        let end = wait_for_server_with(
            &scratch.join("absent"),
            raw(&input),
            || {
                checks += 1;
                seen(checks > 2)
            },
            WaitTimes {
                unwatched_recheck: Duration::from_millis(10),
                ..watched_times()
            },
        )
        .expect("the wait runs");
        assert_eq!(end, ServerWaitEnd::Ready);
    }

    #[test]
    fn the_wait_expires_after_its_longest_life() {
        let scratch = shepr_test_support::ScratchDir::new("server-wait-expired");
        let (input, _client) = std::os::unix::net::UnixStream::pair().expect("an open input");
        let end = wait_for_server_with(
            scratch.path(),
            raw(&input),
            || ServerSeen::Absent,
            WaitTimes {
                max: Duration::from_millis(50),
                ..watched_times()
            },
        )
        .expect("the wait runs");
        assert_eq!(end, ServerWaitEnd::Expired);
    }
}
