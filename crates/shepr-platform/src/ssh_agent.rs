//! Session-local indirection for SSH agents whose sockets belong to an attachment.
//!
//! No socket is bound here. The published address is a symlink, swapped in
//! with a rename, to an agent socket that sshd created; only sockets owned by
//! this user are ever a target. Who may connect is decided by the agent
//! socket's own permissions, so the owner-only staged bind and the
//! `SO_PEERCRED` accept check the server and API sockets use do not apply.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use interprocess::ConnectWaitMode;
use interprocess::local_socket::{ConnectOptions, GenericFilePath, ToFsName};

const PROBE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct SshAgentRegistry(Arc<SharedState>);

struct SharedState {
    state: Mutex<State>,
    /// Serializes symlink updates without holding the attachment bookkeeping
    /// lock through socket probes and filesystem operations.
    publisher: Mutex<()>,
}

struct State {
    path: PathBuf,
    fallback: Option<PathBuf>,
    agents: Vec<(u64, PathBuf)>,
    next_id: u64,
    revision: u64,
    identity: Option<(u64, u64)>,
    last_probe: Option<Instant>,
}

struct PublicationSnapshot {
    path: PathBuf,
    fallback: Option<PathBuf>,
    agents: Vec<(u64, PathBuf)>,
    identity: Option<(u64, u64)>,
}

pub struct SshAgentLease {
    registry: Arc<SharedState>,
    id: u64,
}

pub fn socket_path(api_socket_path: &Path) -> PathBuf {
    agent_path_for(api_socket_path)
}

fn agent_path_for(api_path: &Path) -> PathBuf {
    let mut path = api_path.as_os_str().to_os_string();
    path.push(".agent");
    path.into()
}

fn usable_socket(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| {
        // The API is user-private; do not redirect that user's panes to another user's agent.
        metadata.file_type().is_socket() && metadata.uid() == super::effective_uid()
    })
}

fn live_socket(path: &Path) -> bool {
    if !usable_socket(path) {
        return false;
    }
    let Ok(name) = path.to_fs_name::<GenericFilePath>() else {
        return false;
    };
    // Never wait for a full accept queue or retain a forwarded SSH channel after probing.
    let Ok(interprocess::local_socket::Stream::UdSocket(stream)) = ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(Duration::ZERO))
        .nonblocking_stream(true)
        .connect_sync()
    else {
        return false;
    };
    // Linux can report an unconnected socket writable after connect returns EAGAIN.
    stream.inner().peer_addr().is_ok()
}

impl SshAgentRegistry {
    pub fn new(path: PathBuf, inherited: Option<PathBuf>) -> io::Result<Self> {
        let inherited = inherited.filter(|path| !path.as_os_str().is_empty());
        let managed = inherited.is_some()
            || fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink());
        // Existing pane environments keep pointing at this stable pathname.
        let fallback = fs::read_link(&path)
            .ok()
            .filter(|target| target != &path && usable_socket(target))
            .or_else(|| inherited.filter(|target| target != &path && usable_socket(target)));
        let mut state = State {
            path,
            fallback,
            agents: Vec::new(),
            next_id: 0,
            revision: 0,
            identity: None,
            last_probe: None,
        };
        if managed {
            // Initial publication runs before the state is shared with callers.
            state.last_probe = Some(Instant::now());
            let mut snapshot = state.publication_snapshot();
            snapshot.publish()?;
            state.identity = snapshot.identity;
        }
        Ok(Self(Arc::new(SharedState {
            state: Mutex::new(state),
            publisher: Mutex::new(()),
        })))
    }

    pub fn register(&self, path: PathBuf) -> io::Result<SshAgentLease> {
        if !path.is_absolute() || !usable_socket(&path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSH agent must be an absolute, user-owned socket",
            ));
        }
        let id = {
            let mut state = self
                .0
                .state
                .lock()
                .map_err(|_| io::Error::other("SSH agent registry poisoned"))?;
            if path == state.path {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SSH agent must be an absolute, user-owned socket",
                ));
            }
            let id = state.next_id;
            state.next_id += 1;
            state.agents.push((id, path));
            state.revision = state.revision.wrapping_add(1);
            state.last_probe = Some(Instant::now());
            id
        };
        if let Err(error) = self.0.publish_latest() {
            if let Ok(mut state) = self.0.state.lock() {
                let previous_len = state.agents.len();
                state.agents.retain(|(candidate, _)| *candidate != id);
                if state.agents.len() != previous_len {
                    state.revision = state.revision.wrapping_add(1);
                }
            }
            return Err(error);
        }
        Ok(SshAgentLease {
            registry: Arc::clone(&self.0),
            id,
        })
    }
}

impl SharedState {
    /// Publish the current agent set, holding only the publisher lock across
    /// the probes and filesystem work. A change made while that work ran (a
    /// registration, a lease drop, or a failed registration rolling itself
    /// back) bumps `revision`, and the loop publishes again so the address
    /// never settles on a set that no longer exists.
    fn publish_latest(&self) -> io::Result<()> {
        loop {
            let publisher = self
                .publisher
                .lock()
                .map_err(|_| io::Error::other("SSH agent publisher poisoned"))?;
            let (revision, mut candidate) = {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| io::Error::other("SSH agent registry poisoned"))?;
                (state.revision, state.publication_snapshot())
            };

            // This private snapshot may probe sockets and update the published
            // symlink while the shared state lock remains available to leases.
            candidate.publish()?;

            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("SSH agent registry poisoned"))?;
            state.identity = candidate.identity;
            let current = state.revision == revision;
            drop(state);
            drop(publisher);
            if current {
                return Ok(());
            }
        }
    }
}

impl State {
    fn publication_snapshot(&self) -> PublicationSnapshot {
        PublicationSnapshot {
            path: self.path.clone(),
            fallback: self.fallback.clone(),
            agents: self.agents.clone(),
            identity: self.identity,
        }
    }
}

impl PublicationSnapshot {
    /// Publish this owned snapshot; shared callers run it after releasing the
    /// state lock because probes and filesystem operations can take time.
    fn publish(&mut self) -> io::Result<()> {
        if let Some(identity) = self.identity {
            let metadata = fs::symlink_metadata(&self.path)?;
            if identity != (metadata.dev(), metadata.ino()) {
                return Err(io::Error::other(
                    "SSH agent address belongs to a replacement server",
                ));
            }
        }
        // Keep a working agent rather than letting probes or a second client replace it.
        let unavailable = self.path.with_extension("unavailable");
        // Connection-level liveness cannot tell whether forwarded keys have
        // changed. Keep the daemon's inherited agent as its stable default;
        // letting a later client preempt it would redirect every shared pane
        // to that client's temporary forwarded agent.
        let target = self
            .fallback
            .as_deref()
            .filter(|path| live_socket(path))
            .or_else(|| {
                self.agents
                    .iter()
                    .map(|(_, path)| path.as_path())
                    .find(|path| live_socket(path))
            })
            .unwrap_or(&unavailable);
        if self.identity.is_some() && fs::read_link(&self.path).ok().as_deref() == Some(target) {
            return Ok(());
        }
        let temporary = self
            .path
            .with_extension(format!("{}.new", std::process::id()));
        symlink(target, &temporary)?;
        if let Err(error) = fs::rename(&temporary, &self.path) {
            // The rename error is what the caller acts on; a temporary link
            // left behind next to the agent address still needs reporting.
            if let Err(remove_error) = fs::remove_file(&temporary) {
                tracing::warn!(
                    path = %temporary.display(),
                    err = %remove_error,
                    "failed to remove temporary SSH agent link"
                );
            }
            return Err(error);
        }
        let metadata = fs::symlink_metadata(&self.path)?;
        self.identity = Some((metadata.dev(), metadata.ino()));
        Ok(())
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| self.identity == Some((metadata.dev(), metadata.ino())))
        {
            // Drop has no caller to return to; a stale published address
            // points panes at a dead agent, so it is logged.
            if let Err(error) = fs::remove_file(&self.path) {
                tracing::warn!(
                    path = %self.path.display(),
                    err = %error,
                    "failed to remove published SSH agent address"
                );
            }
        }
    }
}

impl SshAgentLease {
    pub fn refresh(&self) -> io::Result<()> {
        self.refresh_at(Instant::now())
    }

    fn refresh_at(&self, now: Instant) -> io::Result<()> {
        let should_publish = {
            let mut state = self
                .registry
                .state
                .lock()
                .map_err(|_| io::Error::other("SSH agent registry poisoned"))?;
            // Reserve the shared probe window before dropping the state lock.
            // Other attachments can then skip publication while this one does
            // socket probes and filesystem work outside that lock.
            let should_publish = state
                .last_probe
                .is_none_or(|last| now.saturating_duration_since(last) >= PROBE_INTERVAL);
            if should_publish {
                state.last_probe = Some(now);
            }
            should_publish
        };
        if should_publish {
            self.registry.publish_latest()?;
        }
        Ok(())
    }
}

impl Drop for SshAgentLease {
    fn drop(&mut self) {
        let removed = if let Ok(mut state) = self.registry.state.lock() {
            let previous_len = state.agents.len();
            state.agents.retain(|(id, _)| *id != self.id);
            if state.agents.len() != previous_len {
                state.revision = state.revision.wrapping_add(1);
                state.last_probe = Some(Instant::now());
                true
            } else {
                false
            }
        } else {
            false
        };
        if removed && let Err(error) = self.registry.publish_latest() {
            tracing::warn!(%error, "could not refresh SSH agent after attachment ended");
        }
    }
}

pub fn pane_agent_socket(api_socket_path: &Path) -> Option<PathBuf> {
    let path = agent_path_for(api_socket_path);
    fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
        .then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A fresh scratch directory.
    fn scratch(label: &str) -> PathBuf {
        shepr_test_support::ScratchDir::new(label).to_path_buf()
    }

    #[test]
    fn server_without_an_agent_leaves_local_pane_agent_setup_alone() {
        let directory = scratch("no-agent");
        let stable = directory.join("agent");
        for inherited in [None, Some(PathBuf::new())] {
            let registry =
                SshAgentRegistry::new(stable.clone(), inherited).expect("test precondition");
            assert!(
                fs::symlink_metadata(&stable).is_err(),
                "a local server without an agent must not advertise an agent address to panes"
            );
            drop(registry);
        }
    }

    #[test]
    fn closed_agent_listener_does_not_block_a_live_replacement() {
        let directory = scratch("dead-agent");
        let stable = directory.join("agent");
        let a = directory.join("a");
        let b = directory.join("b");
        let listener_a = UnixListener::bind(&a).expect("test precondition");
        let _listener_b = UnixListener::bind(&b).expect("test precondition");
        let registry =
            SshAgentRegistry::new(stable.clone(), Some(a.clone())).expect("test precondition");
        let (mut probe, _) = listener_a.accept().expect("test precondition");
        probe.set_nonblocking(true).expect("test precondition");
        assert_eq!(
            std::io::Read::read(&mut probe, &mut [0]).expect("test precondition"),
            0,
            "agent checks must not hold forwarded SSH channels open"
        );
        let lease_b = registry.register(b.clone()).expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), a);
        drop(listener_a);
        assert!(
            fs::metadata(&a)
                .expect("test precondition")
                .file_type()
                .is_socket()
        );
        lease_b
            .refresh_at(Instant::now() + PROBE_INTERVAL)
            .expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), b);
        drop(lease_b);
        drop(registry);
    }

    #[test]
    fn unestablished_agent_connection_does_not_block_a_live_replacement() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let directory = scratch("busy-agent");
        let a = directory.join("a");
        let b = directory.join("b");
        let stable = directory.join("agent");
        let listener_a = UnixListener::bind(&a).expect("test precondition");
        // Linux allows one queued connection with a zero backlog.
        // SAFETY: listen(2) on a socket `listener_a` keeps open; no memory.
        assert_eq!(unsafe { libc::listen(listener_a.as_raw_fd(), 0) }, 0);
        let _queued = UnixStream::connect(&a).expect("test precondition");
        let _listener_b = UnixListener::bind(&b).expect("test precondition");
        let registry = SshAgentRegistry::new(stable.clone(), Some(a)).expect("test precondition");
        let lease_b = registry.register(b.clone()).expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), b);
        drop(lease_b);
        drop(registry);
    }

    #[test]
    fn registration_preserves_the_supplied_socket_address() {
        let directory = scratch("agent-path");
        symlink(".", directory.join("alias")).expect("test precondition");
        let _listener = UnixListener::bind(directory.join("upstream")).expect("test precondition");
        let supplied = directory.join("alias/upstream");
        let stable = directory.join("agent");
        let registry = SshAgentRegistry::new(stable.clone(), None).expect("test precondition");
        let lease = registry
            .register(supplied.clone())
            .expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), supplied);
        drop(lease);
        drop(registry);
    }

    #[test]
    fn registration_releases_state_while_waiting_for_serial_publication() {
        use std::sync::TryLockError;

        let directory = scratch("agent-publish-lock");
        let stable = directory.join("agent");
        let forwarded = directory.join("forwarded");
        let _listener = UnixListener::bind(&forwarded).expect("test precondition");
        let registry = SshAgentRegistry::new(stable, None).expect("test precondition");
        let publisher = registry.0.publisher.lock().expect("test precondition");
        let registering = registry.clone();
        let registration = std::thread::spawn(move || registering.register(forwarded));

        let deadline = Instant::now() + Duration::from_secs(2);
        let registered = loop {
            match registry.0.state.try_lock() {
                Ok(state) if !state.agents.is_empty() => break true,
                // Not registered yet, or the state is briefly locked.
                Ok(_) | Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Poisoned(_)) => panic!("registry state should not be poisoned"),
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        drop(publisher);

        assert!(
            registered,
            "registration should not hold state while publishing"
        );
        let lease = registration
            .join()
            .expect("registration thread should finish")
            .expect("registered agent should publish");
        drop(lease);
        drop(registry);
    }

    #[test]
    fn socket_overrides_have_independent_agent_addresses() {
        let directory = scratch("agent-overrides");
        let a = directory.join("a");
        let b = directory.join("b");
        let _a_listener = UnixListener::bind(&a).expect("test precondition");
        let _b_listener = UnixListener::bind(&b).expect("test precondition");
        let stable_a = agent_path_for(&directory.join("first.sock"));
        let stable_b = agent_path_for(&directory.join("second.sock"));
        let registry_a =
            SshAgentRegistry::new(stable_a.clone(), Some(a.clone())).expect("test precondition");
        let registry_b =
            SshAgentRegistry::new(stable_b.clone(), Some(b.clone())).expect("test precondition");
        assert_eq!(fs::read_link(&stable_a).expect("test precondition"), a);
        assert_eq!(fs::read_link(&stable_b).expect("test precondition"), b);
        drop(registry_a);
        assert_eq!(fs::read_link(&stable_b).expect("test precondition"), b);
        drop(registry_b);
    }

    #[test]
    fn reconnect_and_overlapping_attachments_keep_a_stable_agent_address() {
        let directory = scratch("ssh-agent");
        let stable = directory.join("agent");
        let a = directory.join("a");
        let b = directory.join("b");
        let probe = directory.join("probe");
        let _a_listener = UnixListener::bind(&a).expect("test precondition");
        let _b_listener = UnixListener::bind(&b).expect("test precondition");
        let _probe_listener = UnixListener::bind(&probe).expect("test precondition");
        let registry =
            SshAgentRegistry::new(stable.clone(), Some(a.clone())).expect("test precondition");
        let temporary = registry.register(a.clone()).expect("test precondition");
        drop(temporary);
        assert_eq!(fs::read_link(&stable).expect("test precondition"), a);
        let lease_a = registry.register(a.clone()).expect("test precondition");
        let lease_probe = registry.register(probe).expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), a);
        drop(lease_probe);
        let lease_b = registry.register(b.clone()).expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), a);
        fs::remove_file(&a).expect("test precondition");
        lease_b
            .refresh_at(Instant::now() + PROBE_INTERVAL)
            .expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), b);
        drop(lease_a);
        assert_eq!(fs::read_link(&stable).expect("test precondition"), b);
        drop(lease_b);
        assert!(!stable.try_exists().expect("test precondition"));
        assert!(fs::symlink_metadata(&stable).is_ok());
        let _lease_b = registry.register(b.clone()).expect("test precondition");
        assert_eq!(fs::read_link(&stable).expect("test precondition"), b);
        assert!(registry.register(stable).is_err());
        drop(_lease_b);
        drop(registry);
    }
}
