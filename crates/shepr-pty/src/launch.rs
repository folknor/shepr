//! How a pane child reports its launch back to the server.
//!
//! The server forks the pane child itself (`crate::backend`) and never waits
//! for its chdir or exec: either can block for as long as a hung mount
//! does. The child instead reports through a status channel it creates after
//! the fork, so no other process can hold it: every fd the server owns is
//! inherited by whatever it forks, including helpers std spawns, and a helper
//! hung in its own chdir would keep a parent-made pipe open indefinitely.
//!
//! One listening `SOCK_SEQPACKET` socket per server process, bound by Linux
//! abstract autobind (no filesystem path), accepts those channels. The child
//! connects to it before any filesystem step, names its launch with the ticket
//! the server gave it, then reports `ChdirOk(index)`, the first cwd
//! candidate's chdir errno when no candidate could be entered, or an exec
//! errno.
//! Its end is close-on-exec, so EOF after `ChdirOk` while the child is still
//! alive means exec passed its point of no return (ExecCommitted). The kernel
//! closes those fds before the new image is fully mapped, so it does not prove
//! that the shell already runs.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::limits::{
    LAUNCH_ACCEPT_RETRY_DELAY, LAUNCH_HELLO_TIMEOUT, LAUNCH_PARKED_CONNECTION_TTL,
    LAUNCH_STATUS_RECORD_BYTES,
};
use crate::locks::lock_auxiliary;

// limits-exempt: launch status wire protocol record kind.
const RECORD_HELLO: u32 = 1;
// limits-exempt: launch status wire protocol record kind.
const RECORD_CHDIR_OK: u32 = 2;
// limits-exempt: launch status wire protocol record kind.
const RECORD_CHDIR_FAILED: u32 = 3;
// limits-exempt: launch status wire protocol record kind.
const RECORD_EXEC_FAILED: u32 = 4;

/// One status report from a launching pane child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchRecord {
    /// The child changed into cwd candidate `index` and is about to exec.
    ChdirOk(u32),
    /// No cwd candidate could be entered; the errno of candidate 0, the
    /// directory the pane was meant to open in. The fallbacks' errnos are not
    /// reported: they are only tried because candidate 0 failed, and the user
    /// acts on candidate 0.
    ChdirFailed(i32),
    /// `execve` of the shell returned this errno.
    ExecFailed(i32),
}

/// What one nonblocking read of a status channel found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordRead {
    Record(LaunchRecord),
    /// The child's end is closed: exec committed, or the child is gone.
    Eof,
    WouldBlock,
}

pub(crate) fn encode_record(kind: u32, value: u64) -> [u8; LAUNCH_STATUS_RECORD_BYTES] {
    let mut record = [0_u8; LAUNCH_STATUS_RECORD_BYTES];
    record[..4].copy_from_slice(&kind.to_le_bytes());
    record[8..].copy_from_slice(&value.to_le_bytes());
    record
}

fn decode_record(bytes: &[u8]) -> Option<(u32, u64)> {
    if bytes.len() != LAUNCH_STATUS_RECORD_BYTES {
        return None;
    }
    let kind = u32::from_le_bytes(bytes[..4].try_into().ok()?);
    let value = u64::from_le_bytes(bytes[8..].try_into().ok()?);
    Some((kind, value))
}

pub(crate) fn chdir_ok_record(index: u32) -> [u8; LAUNCH_STATUS_RECORD_BYTES] {
    encode_record(RECORD_CHDIR_OK, u64::from(index))
}

pub(crate) fn chdir_failed_record(errno: i32) -> [u8; LAUNCH_STATUS_RECORD_BYTES] {
    encode_record(RECORD_CHDIR_FAILED, errno_value(errno))
}

pub(crate) fn exec_failed_record(errno: i32) -> [u8; LAUNCH_STATUS_RECORD_BYTES] {
    encode_record(RECORD_EXEC_FAILED, errno_value(errno))
}

pub(crate) fn hello_record(ticket: u64) -> [u8; LAUNCH_STATUS_RECORD_BYTES] {
    encode_record(RECORD_HELLO, ticket)
}

fn errno_value(errno: i32) -> u64 {
    u64::from(errno.unsigned_abs())
}

/// Reads one record from a nonblocking status channel. A record of the wrong
/// size, an unknown kind or a second hello is a protocol error.
pub fn read_record(channel: &OwnedFd) -> io::Result<RecordRead> {
    let mut buffer = [0_u8; LAUNCH_STATUS_RECORD_BYTES + 1];
    loop {
        // SAFETY: `buffer` is a live writable stack buffer of the length
        // passed, and `channel` stays open for the call.
        let read = unsafe {
            libc::recv(
                channel.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(RecordRead::WouldBlock),
                _ => return Err(error),
            }
        }
        let Ok(read) = usize::try_from(read) else {
            return Err(io::Error::other("negative recv length"));
        };
        if read == 0 {
            return Ok(RecordRead::Eof);
        }
        let Some((kind, value)) = decode_record(&buffer[..read]) else {
            return Err(protocol_error("launch status record has the wrong size"));
        };
        let errno =
            || i32::try_from(value).map_err(|_| protocol_error("launch status errno out of range"));
        return Ok(RecordRead::Record(match kind {
            RECORD_CHDIR_OK => LaunchRecord::ChdirOk(
                u32::try_from(value)
                    .map_err(|_| protocol_error("launch status cwd index out of range"))?,
            ),
            RECORD_CHDIR_FAILED => LaunchRecord::ChdirFailed(errno()?),
            RECORD_EXEC_FAILED => LaunchRecord::ExecFailed(errno()?),
            _ => return Err(protocol_error("unexpected launch status record")),
        }));
    }
}

fn protocol_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Delivers a launch's status channel to whoever waits for it.
pub type StatusDelivery = Box<dyn FnOnce(OwnedFd) + Send>;

struct Waiting {
    pid: shepr_platform::Pid,
    deliver: StatusDelivery,
}

struct Parked {
    pid: shepr_platform::Pid,
    channel: OwnedFd,
    at: Instant,
}

#[derive(Default)]
struct Routes {
    waiting: HashMap<u64, Waiting>,
    parked: HashMap<u64, Parked>,
    /// Tickets whose launch withdrew before its child's connection was
    /// routed; that connection is dropped when it arrives instead of parked.
    retired: HashMap<u64, Instant>,
}

impl Routes {
    fn prune(&mut self, now: Instant) {
        let live = |at: Instant| now.saturating_duration_since(at) < LAUNCH_PARKED_CONNECTION_TTL;
        self.parked.retain(|_, parked| live(parked.at));
        self.retired.retain(|_, at| live(*at));
    }
}

/// The process-wide status listener, and what every launch reads once.
pub(crate) struct LaunchService {
    router: std::sync::Arc<Router>,
    address: libc::sockaddr_un,
    address_len: libc::socklen_t,
    passwd_home: Option<OsString>,
    next_ticket: std::sync::atomic::AtomicU64,
}

/// The listener and its routing table, shared with the accept thread.
struct Router {
    listener: OwnedFd,
    routes: Mutex<Routes>,
}

static SERVICE: OnceLock<Result<LaunchService, String>> = OnceLock::new();

/// Binds the status listener, starts its accept thread and reads the passwd
/// home directory, once per process. The server calls this at startup so no
/// pane spawn does the passwd lookup (NSS can block) on its event loop; a
/// later call is a cheap lookup. A service that cannot accept is an error:
/// launches would never settle while their children live.
pub fn init() -> io::Result<()> {
    service().map(|_| ())
}

pub(crate) fn service() -> io::Result<&'static LaunchService> {
    SERVICE
        .get_or_init(|| LaunchService::bind().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| io::Error::other(format!("pane launch service unavailable: {error}")))
}

impl LaunchService {
    fn bind() -> io::Result<Self> {
        // SAFETY: socket(2) takes integer arguments and returns a new fd or -1.
        let fd =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socket succeeded, so `fd` is a fresh fd nothing else owns.
        let listener = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_un is a plain C struct; all-zero is valid.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX)
            .map_err(|_| io::Error::other("AF_UNIX does not fit sa_family_t"))?;
        let family_len = libc::socklen_t::try_from(std::mem::size_of::<libc::sa_family_t>())
            .map_err(|_| io::Error::other("sa_family_t size does not fit socklen_t"))?;
        // Binding only the family asks the kernel for a unique abstract name.
        // SAFETY: `address` is a live sockaddr_un and `family_len` covers only
        // its family field; bind reads and does not retain the pointer.
        if unsafe {
            libc::bind(
                listener.as_raw_fd(),
                std::ptr::from_ref(&address).cast(),
                family_len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut address_len =
            libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
                .map_err(|_| io::Error::other("sockaddr_un size does not fit socklen_t"))?;
        // SAFETY: `address` and `address_len` are live writable locals sized
        // for a sockaddr_un; getsockname writes the bound abstract address.
        if unsafe {
            libc::getsockname(
                listener.as_raw_fd(),
                std::ptr::from_mut(&mut address).cast(),
                &mut address_len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: listen(2) takes the listener fd and a backlog integer.
        if unsafe { libc::listen(listener.as_raw_fd(), libc::SOMAXCONN) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let router = std::sync::Arc::new(Router {
            listener,
            routes: Mutex::default(),
        });
        let accepting = std::sync::Arc::clone(&router);
        std::thread::Builder::new()
            .name("shepr-launch-status".into())
            .spawn(move || accepting.accept_loop())?;
        Ok(Self {
            router,
            address,
            address_len,
            passwd_home: crate::command::passwd_home(),
            next_ticket: std::sync::atomic::AtomicU64::new(1),
        })
    }

    pub(crate) fn passwd_home(&self) -> Option<&OsStr> {
        self.passwd_home.as_deref()
    }

    /// The listener's address, exactly as `getsockname` returned it: an
    /// abstract name is binary and its length is part of it.
    pub(crate) fn address(&self) -> (&libc::sockaddr_un, libc::socklen_t) {
        (&self.address, self.address_len)
    }

    pub(crate) fn next_ticket(&self) -> u64 {
        self.next_ticket
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Hands launch `ticket`'s channel to `deliver` once child `pid` has
    /// connected (at once if it already has). The guard withdraws the
    /// registration when dropped.
    pub(crate) fn register(
        &'static self,
        ticket: u64,
        pid: shepr_platform::Pid,
        deliver: StatusDelivery,
    ) -> Registration {
        let parked = {
            let mut routes = lock_auxiliary(&self.router.routes);
            // clock-io-ok: registration is the launch IO boundary that ages parked connections.
            routes.prune(Instant::now());
            match routes.parked.remove(&ticket) {
                Some(parked) if parked.pid == pid => Some(parked.channel),
                Some(_) => None,
                None => {
                    routes.waiting.insert(ticket, Waiting { pid, deliver });
                    return Registration {
                        service: self,
                        ticket,
                    };
                }
            }
        };
        if let Some(channel) = parked {
            deliver(channel);
        }
        Registration {
            service: self,
            ticket,
        }
    }
}

impl Router {
    /// Accepts status connections for the life of the process. It wakes at
    /// least once per TTL and expires parked connections on every wake, even
    /// when no launch happens or accepting fails, and rides out resource
    /// exhaustion (fd or memory limits) rather than ending: a listener that
    /// stopped accepting would leave every later launch unsettled.
    fn accept_loop(&self) {
        let mut exhausted = false;
        let wake_ms = libc::c_int::try_from(LAUNCH_PARKED_CONNECTION_TTL.as_millis())
            .unwrap_or(libc::c_int::MAX);
        loop {
            let mut listener = libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live pollfd on this stack frame.
            let ready = unsafe { libc::poll(&mut listener, 1, wake_ms) };
            // Expired parked fds are what may hold the descriptors an accept
            // needs, so they go before every accept, not only on idle wakes.
            // clock-io-ok: the listener's wake is the IO boundary that ages parking.
            lock_auxiliary(&self.routes).prune(Instant::now());
            if ready <= 0 {
                continue;
            }
            let peer = match shepr_platform::ipc::accept_peer(
                self.listener.as_raw_fd(),
                shepr_platform::ipc::PeerAdmission::ExactOwner,
            ) {
                shepr_platform::ipc::Accepted::Peer(peer) => peer,
                shepr_platform::ipc::Accepted::RetryNow => continue,
                shepr_platform::ipc::Accepted::Backoff(error) => {
                    if !exhausted {
                        tracing::warn!(%error, "pane launch status listener failed; retrying");
                    }
                    exhausted = true;
                    std::thread::sleep(LAUNCH_ACCEPT_RETRY_DELAY);
                    continue;
                }
                shepr_platform::ipc::Accepted::Fatal(error) => {
                    tracing::error!(%error, "pane launch status listener is invalid");
                    return;
                }
            };
            if exhausted {
                tracing::info!("pane launch status listener recovered");
                exhausted = false;
            }
            let channel = peer.fd;
            match accept_hello(&channel, peer.pid) {
                Ok((ticket, pid)) => self.route(ticket, pid, channel),
                Err(error) => {
                    tracing::warn!(%error, "dropping a pane launch status connection");
                }
            }
        }
    }

    fn route(&self, ticket: u64, pid: shepr_platform::Pid, channel: OwnedFd) {
        let delivery = {
            let mut routes = lock_auxiliary(&self.routes);
            // clock-io-ok: an accepted connection is the IO boundary that stamps parking.
            let now = Instant::now();
            routes.prune(now);
            match routes.waiting.remove(&ticket) {
                Some(waiting) if waiting.pid == pid => Some(waiting.deliver),
                Some(waiting) => {
                    // A connection for this ticket from another process.
                    routes.waiting.insert(ticket, waiting);
                    tracing::warn!(ticket, pid = %pid, "pane launch status from an unexpected process");
                    return;
                }
                None if routes.retired.remove(&ticket).is_some() => {
                    // Its launch already gave up on it; the channel closes here.
                    return;
                }
                None => {
                    routes.parked.insert(
                        ticket,
                        Parked {
                            pid,
                            channel,
                            at: now,
                        },
                    );
                    return;
                }
            }
        };
        if let Some(deliver) = delivery {
            deliver(channel);
        }
    }
}

/// A launch's claim on its status channel; dropping it withdraws the claim,
/// so a launch that never connects leaves nothing behind.
pub struct Registration {
    service: &'static LaunchService,
    ticket: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut routes = lock_auxiliary(&self.service.router.routes);
        routes.parked.remove(&self.ticket);
        if routes.waiting.remove(&self.ticket).is_some() {
            // Never delivered: the child's connection may still be on its way.
            // clock-io-ok: withdrawal is the launch IO boundary that stamps retirement.
            routes.retired.insert(self.ticket, Instant::now());
        }
    }
}

/// Reads the hello of a peer `accept_peer` already admitted (same user, with
/// `pid` from its credentials), bounded by `LAUNCH_HELLO_TIMEOUT` so a stray
/// local connection cannot stall the listener, then clears the receive
/// timeout for the channel's reader.
fn accept_hello(channel: &OwnedFd, pid: u32) -> io::Result<(u64, shepr_platform::Pid)> {
    let pid = shepr_platform::Pid::new(pid)
        .ok_or_else(|| protocol_error("invalid launch peer process id"))?;
    let timeout = libc::timeval {
        tv_sec: libc::time_t::try_from(LAUNCH_HELLO_TIMEOUT.as_secs())
            .map_err(|_| io::Error::other("hello timeout out of range"))?,
        tv_usec: libc::suseconds_t::from(LAUNCH_HELLO_TIMEOUT.subsec_micros()),
    };
    set_receive_timeout(channel, &timeout)?;
    let mut buffer = [0_u8; LAUNCH_STATUS_RECORD_BYTES + 1];
    let read = loop {
        // SAFETY: `buffer` is a live writable stack buffer of the length
        // passed, and `channel` stays open for the call.
        let read = unsafe {
            libc::recv(
                channel.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        break usize::try_from(read).map_err(|_| io::Error::other("negative recv length"))?;
    };
    let Some((RECORD_HELLO, ticket)) = decode_record(&buffer[..read]) else {
        return Err(protocol_error(
            "launch status connection did not open with a hello",
        ));
    };
    set_receive_timeout(
        channel,
        &libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    )?;
    Ok((ticket, pid))
}

fn set_receive_timeout(channel: &OwnedFd, timeout: &libc::timeval) -> io::Result<()> {
    let length = libc::socklen_t::try_from(std::mem::size_of::<libc::timeval>())
        .map_err(|_| io::Error::other("timeval size does not fit socklen_t"))?;
    // SAFETY: `timeout` is a live timeval of the length passed; setsockopt
    // reads it during the call only.
    if unsafe {
        libc::setsockopt(
            channel.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            std::ptr::from_ref(timeout).cast(),
            length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_the_wire_encoding() {
        for (kind, value) in [
            (RECORD_HELLO, u64::MAX),
            (RECORD_CHDIR_OK, 3),
            (RECORD_CHDIR_FAILED, 2),
        ] {
            assert_eq!(
                decode_record(&encode_record(kind, value)),
                Some((kind, value))
            );
        }
        assert_eq!(decode_record(&[0_u8; 3]), None);
    }
}
