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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use shepr_core::absolute_path::AbsolutePath;

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
pub(crate) enum LaunchRecord {
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
pub(crate) enum RecordRead {
    Record(LaunchRecord),
    /// The child's end is closed: exec committed, or the child is gone.
    Eof,
    WouldBlock,
}

/// A validated report from the child. EOF is only a commitment candidate:
/// the caller must still establish that the child lives at that instant.
#[derive(Debug)]
pub enum LaunchStatusEvent {
    Entered(AbsolutePath),
    DirectoryFailed {
        path: AbsolutePath,
        error: io::Error,
    },
    ExecFailed(io::Error),
    CommitCandidate(AbsolutePath),
    Unconfirmed,
    WouldBlock,
}

/// Owns the status protocol's ordering and candidate validation. It performs
/// nonblocking reads only; waiting and the lifetime policy belong to the mux.
pub struct LaunchStatusReader {
    candidates: Vec<AbsolutePath>,
    phase: StatusPhase,
}

enum StatusPhase {
    AwaitDirectory,
    Entered(AbsolutePath),
    Finished,
}

impl LaunchStatusReader {
    pub fn new(candidates: Vec<AbsolutePath>) -> Self {
        Self {
            candidates,
            phase: StatusPhase::AwaitDirectory,
        }
    }

    pub fn read(&mut self, channel: &OwnedFd) -> io::Result<LaunchStatusEvent> {
        let record = read_record(channel)?;
        self.accept(record)
    }

    fn accept(&mut self, record: RecordRead) -> io::Result<LaunchStatusEvent> {
        if matches!(record, RecordRead::WouldBlock) {
            return Ok(LaunchStatusEvent::WouldBlock);
        }
        let phase = std::mem::replace(&mut self.phase, StatusPhase::Finished);
        match (phase, record) {
            (StatusPhase::AwaitDirectory, RecordRead::Record(LaunchRecord::ChdirOk(index))) => {
                let path = usize::try_from(index)
                    .ok()
                    .and_then(|index| self.candidates.get(index))
                    .cloned()
                    .ok_or_else(|| protocol_error("unknown launch cwd candidate"))?;
                self.phase = StatusPhase::Entered(path.clone());
                Ok(LaunchStatusEvent::Entered(path))
            }
            (StatusPhase::AwaitDirectory, RecordRead::Record(LaunchRecord::ChdirFailed(errno))) => {
                let path =
                    self.candidates.first().cloned().ok_or_else(|| {
                        protocol_error("directory failure without a cwd candidate")
                    })?;
                Ok(LaunchStatusEvent::DirectoryFailed {
                    path,
                    error: io::Error::from_raw_os_error(errno),
                })
            }
            (StatusPhase::Entered(_), RecordRead::Record(LaunchRecord::ExecFailed(errno))) => Ok(
                LaunchStatusEvent::ExecFailed(io::Error::from_raw_os_error(errno)),
            ),
            (StatusPhase::Entered(path), RecordRead::Eof) => {
                Ok(LaunchStatusEvent::CommitCandidate(path))
            }
            (StatusPhase::AwaitDirectory, RecordRead::Eof) => Ok(LaunchStatusEvent::Unconfirmed),
            _ => Err(protocol_error("launch status record out of order")),
        }
    }
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
pub(crate) fn read_record(channel: &OwnedFd) -> io::Result<RecordRead> {
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
        let errno = || {
            i32::try_from(value)
                .ok()
                .filter(|errno| *errno > 0)
                .ok_or_else(|| protocol_error("launch status errno out of range"))
        };
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
    channel: OwnedFd,
    at: Instant,
}

#[derive(Default)]
struct Routes {
    waiting: HashMap<u64, Waiting>,
    parked: HashMap<(u64, u32), Parked>,
    /// Tickets whose launch withdrew before its child's connection was
    /// routed; that connection is dropped when it arrives instead of parked.
    retired: HashMap<u64, Instant>,
    failure: Option<Arc<io::Error>>,
}

impl Routes {
    fn prune(&mut self, now: Instant, ttl: Duration) {
        let live = |at: Instant| now.saturating_duration_since(at) < ttl;
        self.parked.retain(|_, parked| live(parked.at));
        self.retired.retain(|_, at| live(*at));
    }

    fn register(&mut self, ticket: u64, waiting: Waiting) -> Option<(StatusDelivery, OwnedFd)> {
        if self.failure.is_some() {
            // Dropping delivery wakes a launch racing the listener's failure.
            return None;
        }
        let matching = self.parked.remove(&(ticket, waiting.pid.get()));
        self.parked.retain(|(parked_ticket, pid), _| {
            let keep = *parked_ticket != ticket;
            if !keep {
                tracing::warn!(
                    ticket,
                    pid = *pid,
                    "pane launch status from an unexpected process"
                );
            }
            keep
        });
        if let Some(parked) = matching {
            return Some((waiting.deliver, parked.channel));
        }
        self.waiting.insert(ticket, waiting);
        None
    }

    fn route(
        &mut self,
        ticket: u64,
        pid: shepr_platform::Pid,
        channel: OwnedFd,
        now: Instant,
    ) -> Option<(StatusDelivery, OwnedFd)> {
        if self.failure.is_some() || self.retired.contains_key(&ticket) {
            return None;
        }
        match self.waiting.remove(&ticket) {
            Some(waiting) if waiting.pid == pid => Some((waiting.deliver, channel)),
            Some(waiting) => {
                self.waiting.insert(ticket, waiting);
                tracing::warn!(ticket, pid = %pid, "pane launch status from an unexpected process");
                None
            }
            None => {
                let key = (ticket, pid.get());
                if self.parked.contains_key(&key) {
                    tracing::warn!(ticket, %pid, "duplicate pane launch status connection");
                    return None;
                }
                self.parked.insert(key, Parked { channel, at: now });
                None
            }
        }
    }

    fn retire(&mut self, ticket: u64, now: Instant) {
        self.parked
            .retain(|(parked_ticket, _), _| *parked_ticket != ticket);
        if self.waiting.remove(&ticket).is_some() {
            self.retired.insert(ticket, now);
        }
    }

    fn fail(&mut self, error: io::Error) {
        self.failure = Some(Arc::new(error));
        self.waiting.clear();
        self.parked.clear();
        self.retired.clear();
    }

    fn check_health(&self) -> io::Result<()> {
        match &self.failure {
            Some(error) => Err(clone_failure(error)),
            None => Ok(()),
        }
    }
}

fn clone_failure(error: &Arc<io::Error>) -> io::Error {
    io::Error::new(error.kind(), CachedLaunchFailure(Arc::clone(error)))
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
    timing: RouterTiming,
}

#[derive(Clone, Copy)]
struct RouterTiming {
    hello: Duration,
    parked: Duration,
    retry: Duration,
}

impl Default for RouterTiming {
    fn default() -> Self {
        Self {
            hello: LAUNCH_HELLO_TIMEOUT,
            parked: LAUNCH_PARKED_CONNECTION_TTL,
            retry: LAUNCH_ACCEPT_RETRY_DELAY,
        }
    }
}

struct PendingHello {
    channel: OwnedFd,
    pid: u32,
    at: Instant,
}

impl PendingHello {
    fn live(&self, now: Instant, timeout: Duration) -> bool {
        now.saturating_duration_since(self.at) < timeout
    }
}

#[derive(Debug)]
struct CachedLaunchFailure(Arc<io::Error>);

impl std::fmt::Display for CachedLaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pane launch service unavailable: {}", self.0)
    }
}

impl std::error::Error for CachedLaunchFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

static SERVICE: OnceLock<Result<LaunchService, Arc<io::Error>>> = OnceLock::new();

/// Binds the status listener, starts its accept thread and reads the passwd
/// home directory, once per process. The server calls this at startup so no
/// pane spawn does the passwd lookup (NSS can block) on its event loop; a
/// later call is a cheap lookup. A service that cannot accept is an error:
/// launches would never settle while their children live.
pub fn init() -> io::Result<()> {
    service().map(|_| ())
}

pub(crate) fn service() -> io::Result<&'static LaunchService> {
    let service = SERVICE
        .get_or_init(|| LaunchService::bind().map_err(Arc::new))
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), CachedLaunchFailure(Arc::clone(error))))?;
    lock_auxiliary(&service.router.routes).check_health()?;
    Ok(service)
}

impl LaunchService {
    fn bind() -> io::Result<Self> {
        // SAFETY: socket(2) takes integer arguments and returns a new fd or -1.
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
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
            timing: RouterTiming::default(),
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
        let delivery = {
            let mut routes = lock_auxiliary(&self.router.routes);
            // clock-io-ok: registration ages parked connections at the launch IO boundary.
            routes.prune(Instant::now(), self.router.timing.parked);
            routes.register(ticket, Waiting { pid, deliver })
        };
        if let Some((deliver, channel)) = delivery {
            deliver(channel);
        }
        Registration {
            service: self,
            ticket,
        }
    }
}

impl Router {
    fn failure(&self) -> Option<io::Error> {
        lock_auxiliary(&self.routes)
            .failure
            .as_ref()
            .map(clone_failure)
    }

    /// Polls every unfinished hello together. A silent peer has its own
    /// deadline and cannot hold up a child's hello or failure report.
    /// The mux's post-exit status grace therefore need not exceed a stray
    /// peer's hello timeout: that timeout is no longer spent serially before
    /// reading the child's already queued hello.
    fn accept_loop(&self) {
        self.accept_loop_with(poll_hellos, std::thread::sleep, || {
            shepr_platform::ipc::accept_peer(
                self.listener.as_raw_fd(),
                shepr_platform::ipc::PeerAdmission::ExactOwner,
            )
        });
    }

    fn accept_loop_with(
        &self,
        mut poll: impl FnMut(&mut [libc::pollfd], Duration) -> io::Result<()>,
        mut sleep: impl FnMut(Duration),
        mut accept: impl FnMut() -> shepr_platform::ipc::Accepted,
    ) {
        let mut pending: Vec<PendingHello> = Vec::new();
        // A shortage of fds or memory lasts many retries; it is logged when it
        // starts and when the listener recovers, not on every retry.
        let mut exhausted = false;
        loop {
            // clock-io-ok: listener wake ages pending hellos and routing entries.
            let now = Instant::now();
            lock_auxiliary(&self.routes).prune(now, self.timing.parked);
            pending.retain(|peer| {
                let live = peer.live(now, self.timing.hello);
                if !live {
                    tracing::warn!(pid = peer.pid, "pane launch status hello timed out");
                }
                live
            });
            let wait = pending
                .iter()
                .map(|peer| {
                    self.timing
                        .hello
                        .saturating_sub(now.saturating_duration_since(peer.at))
                })
                .min()
                .unwrap_or(self.timing.parked)
                .min(self.timing.parked);
            let mut fds = Vec::with_capacity(pending.len() + 1);
            fds.push(libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.extend(pending.iter().map(|peer| libc::pollfd {
                fd: peer.channel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }));
            if let Err(error) = poll(&mut fds, wait) {
                if error.kind() != io::ErrorKind::Interrupted {
                    if !exhausted {
                        tracing::warn!(%error, "pane launch status poll failed; retrying");
                    }
                    exhausted = true;
                    sleep(self.timing.retry);
                }
                continue;
            }
            // Process established peers before accepting more, so a stream
            // of new connections cannot starve already queued child reports.
            for index in (0..pending.len()).rev() {
                if fds[index + 1].revents == 0 {
                    continue;
                }
                let peer = pending.swap_remove(index);
                match accept_hello(&peer.channel, peer.pid) {
                    Ok(Some((ticket, pid))) => self.route(ticket, pid, peer.channel),
                    Ok(None) => pending.push(peer),
                    Err(error) => {
                        tracing::warn!(%error, "dropping a pane launch status connection");
                    }
                }
            }
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                self.fail(io::Error::other("pane launch status listener is invalid"));
                return;
            }
            if fds[0].revents & libc::POLLIN == 0 {
                continue;
            }
            match accept() {
                shepr_platform::ipc::Accepted::Peer(peer) => {
                    if exhausted {
                        tracing::info!("pane launch status listener recovered");
                        exhausted = false;
                    }
                    pending.push(PendingHello {
                        channel: peer.fd,
                        pid: peer.pid,
                        // clock-io-ok: accepted peer starts its own hello deadline.
                        at: Instant::now(),
                    });
                }
                shepr_platform::ipc::Accepted::RetryNow => {}
                shepr_platform::ipc::Accepted::Backoff(error) => {
                    if !exhausted {
                        tracing::warn!(%error, "pane launch status accept failed; retrying");
                    }
                    exhausted = true;
                    sleep(self.timing.retry);
                }
                shepr_platform::ipc::Accepted::Fatal(error) => {
                    self.fail(error);
                    return;
                }
            }
        }
    }

    fn fail(&self, error: io::Error) {
        tracing::error!(%error, "pane launch status listener stopped; launches unavailable");
        lock_auxiliary(&self.routes).fail(error);
    }

    fn route(&self, ticket: u64, pid: shepr_platform::Pid, channel: OwnedFd) {
        let delivery = {
            let mut routes = lock_auxiliary(&self.routes);
            // clock-io-ok: routing stamps and ages parking at the IO boundary.
            let now = Instant::now();
            routes.prune(now, self.timing.parked);
            routes.route(ticket, pid, channel, now)
        };
        if let Some((deliver, channel)) = delivery {
            deliver(channel);
        }
    }
}

fn poll_hellos(fds: &mut [libc::pollfd], wait: Duration) -> io::Result<()> {
    let count = libc::nfds_t::try_from(fds.len())
        .map_err(|_| io::Error::other("too many launch hello channels"))?;
    // SAFETY: the slice contains count live pollfd entries for this call.
    let result = unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            count,
            shepr_platform::Wait::After(wait).poll_millis(),
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A launch's claim on its status channel; dropping it withdraws the claim,
/// so a launch that never connects leaves nothing behind.
pub struct Registration {
    service: &'static LaunchService,
    ticket: u64,
}

impl Registration {
    /// Returns a live probe for a listener failure that may race delivery.
    /// The probe outlives this registration guard without keeping its ticket
    /// registered.
    pub fn failure_probe(&self) -> impl Fn() -> Option<io::Error> + Send + Sync + 'static {
        let router = std::sync::Arc::clone(&self.service.router);
        move || router.failure()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut routes = lock_auxiliary(&self.service.router.routes);
        // clock-io-ok: withdrawal stamps retirement at the launch IO boundary.
        routes.retire(self.ticket, Instant::now());
    }
}

/// Attempts one hello without waiting; the router owns each peer's deadline.
fn accept_hello(channel: &OwnedFd, pid: u32) -> io::Result<Option<(u64, shepr_platform::Pid)>> {
    let pid = shepr_platform::Pid::new(pid)
        .ok_or_else(|| protocol_error("invalid launch peer process id"))?;
    let mut buffer = [0_u8; LAUNCH_STATUS_RECORD_BYTES + 1];
    let read = loop {
        // SAFETY: buffer is writable and channel stays open during recv.
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
                io::ErrorKind::WouldBlock => return Ok(None),
                _ => return Err(error),
            }
        }
        break usize::try_from(read).map_err(|_| io::Error::other("negative recv length"))?;
    };
    let Some((RECORD_HELLO, ticket)) = decode_record(&buffer[..read]) else {
        return Err(protocol_error(
            "launch status connection did not open with a hello",
        ));
    };
    Ok(Some((ticket, pid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        // SAFETY: socketpair writes two fresh descriptors into the live array.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: successful socketpair returned two independently owned fds.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn pid(value: u32) -> shepr_platform::Pid {
        shepr_platform::Pid::new(value).expect("positive test pid")
    }

    fn waiter(value: u32) -> (Waiting, std::sync::mpsc::Receiver<OwnedFd>) {
        let (send, receive) = std::sync::mpsc::channel();
        (
            Waiting {
                pid: pid(value),
                deliver: Box::new(move |fd| {
                    send.send(fd).expect("deliver");
                }),
            },
            receive,
        )
    }

    fn dispatch(delivery: Option<(StatusDelivery, OwnedFd)>) {
        if let Some((deliver, channel)) = delivery {
            deliver(channel);
        }
    }

    fn router() -> Router {
        let (listener, _other) = pair();
        Router {
            listener,
            routes: Mutex::default(),
            timing: RouterTiming::default(),
        }
    }

    fn send_record(channel: &OwnedFd, record: &[u8]) {
        // SAFETY: record is readable and channel is open during send.
        assert_eq!(
            unsafe {
                libc::send(
                    channel.as_raw_fd(),
                    record.as_ptr().cast(),
                    record.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            isize::try_from(record.len()).expect("small record")
        );
    }

    #[test]
    fn parked_and_waiting_routes_deliver_only_the_registered_pid() {
        for parked_first in [false, true] {
            let mut routes = Routes::default();
            let now = Instant::now();
            let (foreign, _foreign_peer) = pair();
            let (waiting, receive) = waiter(42);
            if parked_first {
                assert!(routes.route(1, pid(7), foreign, now).is_none());
                assert!(routes.register(1, waiting).is_none());
            } else {
                assert!(routes.register(1, waiting).is_none());
                assert!(routes.route(1, pid(7), foreign, now).is_none());
            }
            assert!(matches!(
                receive.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            assert!(routes.waiting.contains_key(&1));
            let (child, _child_peer) = pair();
            dispatch(routes.route(1, pid(42), child, now));
            receive.try_recv().expect("real child delivered");
            assert!(routes.waiting.is_empty());
        }
    }

    #[test]
    fn early_child_is_delivered_at_registration_and_parking_expires() {
        let mut routes = Routes::default();
        let now = Instant::now();
        let ttl = Duration::from_millis(10);
        let (child, _peer) = pair();
        routes.route(1, pid(42), child, now);
        let (waiting, receive) = waiter(42);
        dispatch(routes.register(1, waiting));
        receive.try_recv().expect("parked child delivered");
        let (child, _peer) = pair();
        routes.route(2, pid(42), child, now);
        routes.prune(now + ttl, ttl);
        assert!(routes.parked.is_empty());
    }

    #[test]
    fn parked_channels_are_kept_per_ticket_and_pid() {
        let mut routes = Routes::default();
        let now = Instant::now();
        let (child, child_peer) = pair();
        let (stray, stray_peer) = pair();
        send_record(&child_peer, &chdir_ok_record(0));
        send_record(&stray_peer, &exec_failed_record(libc::ENOENT));

        assert!(routes.route(3, pid(42), child, now).is_none());
        assert!(routes.route(3, pid(7), stray, now).is_none());
        assert_eq!(routes.parked.len(), 2);

        let (waiting, receive) = waiter(42);
        dispatch(routes.register(3, waiting));
        let delivered = receive.try_recv().expect("registered child delivered");
        assert_eq!(
            read_record(&delivered).expect("read child's report"),
            RecordRead::Record(LaunchRecord::ChdirOk(0))
        );
        assert!(routes.parked.is_empty());
    }

    #[test]
    fn retirement_rejects_every_late_connection_until_expiry() {
        let mut routes = Routes::default();
        let now = Instant::now();
        let ttl = Duration::from_millis(10);
        let (waiting, _receive) = waiter(42);
        routes.register(1, waiting);
        routes.retire(1, now);
        for _ in 0..2 {
            let (child, _peer) = pair();
            assert!(routes.route(1, pid(42), child, now).is_none());
            assert!(routes.parked.is_empty());
        }
        routes.prune(now + ttl, ttl);
        assert!(routes.retired.is_empty());
    }

    #[test]
    fn listener_failure_drops_waiters_and_rejects_future_launches() {
        let router = router();
        let failure_probe = || router.failure();
        assert!(failure_probe().is_none());
        let (waiting, receive) = waiter(42);
        lock_auxiliary(&router.routes).register(1, waiting);
        router.accept_loop_with(
            |fds, _| {
                fds[0].revents = libc::POLLNVAL;
                Ok(())
            },
            |_| panic!("fatal listener must not retry"),
            || panic!("must not accept"),
        );
        assert!(failure_probe().is_some());
        let mut routes = lock_auxiliary(&router.routes);
        assert!(routes.check_health().is_err());
        assert!(matches!(
            receive.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        let (waiting, receive) = waiter(42);
        routes.register(2, waiting);
        assert!(matches!(
            receive.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert!(routes.waiting.is_empty());
    }

    #[test]
    fn fatal_accept_poisons_the_router() {
        let router = router();
        router.accept_loop_with(
            |fds, _| {
                fds[0].revents = libc::POLLIN;
                Ok(())
            },
            |_| panic!("fatal accept must not retry"),
            || shepr_platform::ipc::Accepted::Fatal(io::Error::from_raw_os_error(libc::EBADF)),
        );
        assert!(lock_auxiliary(&router.routes).check_health().is_err());
    }

    #[test]
    fn poll_errors_back_off_but_interrupts_retry_immediately() {
        let router = router();
        let mut iteration = 0;
        let mut sleeps = Vec::new();
        router.accept_loop_with(
            |fds, _| {
                iteration += 1;
                match iteration {
                    1 => Err(io::Error::from_raw_os_error(libc::EINTR)),
                    2 | 3 => Err(io::Error::from_raw_os_error(libc::ENOMEM)),
                    _ => {
                        fds[0].revents = libc::POLLNVAL;
                        Ok(())
                    }
                }
            },
            |wait| sleeps.push(wait),
            || panic!("must not accept"),
        );
        assert_eq!(sleeps, vec![router.timing.retry; 2]);
    }

    #[test]
    fn hello_is_nonblocking_validated_and_expires_at_its_own_deadline() {
        let (channel, peer) = pair();
        assert_eq!(accept_hello(&channel, 42).expect("pending"), None);
        send_record(&peer, &hello_record(9));
        assert_eq!(
            accept_hello(&channel, 42).expect("hello"),
            Some((9, pid(42)))
        );
        send_record(&peer, &chdir_ok_record(0));
        assert_eq!(
            accept_hello(&channel, 42).expect_err("wrong kind").kind(),
            io::ErrorKind::InvalidData
        );
        send_record(&peer, &[0; 3]);
        assert!(accept_hello(&channel, 42).is_err());
        assert!(accept_hello(&channel, 0).is_err());
        drop(peer);
        assert!(accept_hello(&channel, 42).is_err());
        let now = Instant::now();
        let pending = PendingHello {
            channel,
            pid: 42,
            at: now,
        };
        let timeout = Duration::from_millis(10);
        assert!(pending.live(now, timeout));
        assert!(!pending.live(now + timeout, timeout));
    }

    #[test]
    fn silent_peer_does_not_delay_a_later_child_failure() {
        let router = router();
        let (silent, _silent_peer) = pair();
        let (child, child_peer) = pair();
        send_record(&child_peer, &hello_record(1));
        send_record(&child_peer, &chdir_failed_record(libc::ENOENT));
        let (waiting, receive) = waiter(42);
        lock_auxiliary(&router.routes).register(1, waiting);
        let mut peers = std::collections::VecDeque::from([
            shepr_platform::ipc::AdmittedPeer { fd: silent, pid: 7 },
            shepr_platform::ipc::AdmittedPeer { fd: child, pid: 42 },
        ]);
        let mut iteration = 0;
        router.accept_loop_with(
            |fds, _| {
                iteration += 1;
                match iteration {
                    1 | 2 => fds[0].revents = libc::POLLIN,
                    3 => fds[2].revents = libc::POLLIN,
                    _ => fds[0].revents = libc::POLLNVAL,
                }
                Ok(())
            },
            |_| panic!("must not sleep"),
            || shepr_platform::ipc::Accepted::Peer(peers.pop_front().expect("queued peer")),
        );
        let channel = receive.try_recv().expect("child bypassed silent hello");
        assert_eq!(
            read_record(&channel).expect("failure retained"),
            RecordRead::Record(LaunchRecord::ChdirFailed(libc::ENOENT))
        );
    }

    fn abs(path: &str) -> AbsolutePath {
        AbsolutePath::new(path).expect("test path is absolute")
    }

    #[test]
    fn directory_selection_and_commitment_are_validated_together() {
        let mut reader = LaunchStatusReader::new(vec![abs("/requested"), abs("/fallback")]);
        assert!(
            matches!(reader.accept(RecordRead::Record(LaunchRecord::ChdirOk(1))),
            Ok(LaunchStatusEvent::Entered(path)) if path == std::path::Path::new("/fallback"))
        );
        assert!(matches!(
            reader.accept(RecordRead::WouldBlock),
            Ok(LaunchStatusEvent::WouldBlock)
        ));
        assert!(matches!(reader.accept(RecordRead::Eof),
            Ok(LaunchStatusEvent::CommitCandidate(path)) if path == std::path::Path::new("/fallback")));
        assert!(reader.accept(RecordRead::Eof).is_err());
    }

    #[test]
    fn invalid_candidate_and_out_of_order_reports_are_protocol_errors() {
        for record in [
            LaunchRecord::ChdirOk(1),
            LaunchRecord::ExecFailed(libc::ENOENT),
        ] {
            let mut reader = LaunchStatusReader::new(vec![abs("/requested")]);
            assert_eq!(
                reader
                    .accept(RecordRead::Record(record))
                    .expect_err("invalid report")
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        for record in [
            LaunchRecord::ChdirOk(0),
            LaunchRecord::ChdirFailed(libc::ENOENT),
        ] {
            let mut reader = LaunchStatusReader::new(vec![abs("/requested")]);
            reader
                .accept(RecordRead::Record(LaunchRecord::ChdirOk(0)))
                .expect("enter directory");
            assert!(reader.accept(RecordRead::Record(record)).is_err());
        }
    }

    #[test]
    fn failures_keep_the_requested_path_and_errno() {
        let mut reader = LaunchStatusReader::new(vec![abs("/requested"), abs("/fallback")]);
        let LaunchStatusEvent::DirectoryFailed { path, error } = reader
            .accept(RecordRead::Record(LaunchRecord::ChdirFailed(libc::EACCES)))
            .expect("directory failure")
        else {
            panic!("wrong report")
        };
        assert_eq!(path, std::path::Path::new("/requested"));
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
        let mut reader = LaunchStatusReader::new(vec![abs("/requested")]);
        reader
            .accept(RecordRead::Record(LaunchRecord::ChdirOk(0)))
            .expect("directory selected");
        let LaunchStatusEvent::ExecFailed(error) = reader
            .accept(RecordRead::Record(LaunchRecord::ExecFailed(libc::ENOEXEC)))
            .expect("exec failure")
        else {
            panic!("wrong report")
        };
        assert_eq!(error.raw_os_error(), Some(libc::ENOEXEC));
    }

    #[test]
    fn eof_without_entering_is_unconfirmed() {
        let mut reader = LaunchStatusReader::new(vec![abs("/requested")]);
        assert!(matches!(
            reader.accept(RecordRead::Eof),
            Ok(LaunchStatusEvent::Unconfirmed)
        ));
        let mut reader = LaunchStatusReader::new(Vec::new());
        assert!(
            reader
                .accept(RecordRead::Record(LaunchRecord::ChdirFailed(libc::ENOENT)))
                .is_err()
        );
    }

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
