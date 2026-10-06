//! A wait for entries to appear in one directory, through inotify, alongside
//! one input descriptor whose readability or close also ends the wait.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::Instant;

use crate::child_io::{Wait, read_fd};

/// What ended a [`DirectoryWatch::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryWake {
    /// A watched event occurred, or the inotify queue overflowed.
    Changed,
    /// The input descriptor is readable or closed.
    Input,
    /// The wait ran out, or a signal interrupted it.
    TimedOut,
}

/// An inotify watch on one directory for entries created, moved in, or whose
/// attributes changed. A socket bound in the directory counts as created.
pub struct DirectoryWatch {
    fd: OwnedFd,
}

impl DirectoryWatch {
    /// Watches `dir`, which must exist.
    pub fn new(dir: &Path) -> io::Result<Self> {
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: inotify_init1 takes only flags and returns a new descriptor
        // or -1.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor this call just opened and owns alone.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `path` is a NUL-terminated string that lives for the call.
        let watch = unsafe {
            libc::inotify_add_watch(
                fd.as_raw_fd(),
                path.as_ptr(),
                libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_ATTRIB | libc::IN_ONLYDIR,
            )
        };
        if watch < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Blocks until any entry in the directory changes, `input` is readable or
    /// closed, or `wait` passes. Pending directory events are drained before
    /// returning, so the next wait blocks until a later change.
    pub fn wait(&self, input: RawFd, wait: impl Into<Wait>) -> io::Result<DirectoryWake> {
        self.wait_inner(input, wait.into(), None)
    }

    /// Blocks until `entry_name` changes in the directory, `input` is readable
    /// or closed, or `wait` passes. Events for other names are drained without
    /// ending the wait. An inotify queue overflow ends the wait because the
    /// named event may have been lost.
    pub fn wait_for_entry(
        &self,
        input: RawFd,
        entry_name: &OsStr,
        wait: impl Into<Wait>,
    ) -> io::Result<DirectoryWake> {
        self.wait_inner(input, wait.into(), Some(entry_name))
    }

    fn wait_inner(
        &self,
        input: RawFd,
        wait: Wait,
        entry_name: Option<&OsStr>,
    ) -> io::Result<DirectoryWake> {
        // Events for other names restart the poll, so the caller's wait is
        // kept as one deadline across those restarts.
        let deadline = match wait {
            Wait::Forever => None,
            Wait::Now => Some(poll_clock()),
            Wait::After(duration) => poll_clock().checked_add(duration),
        };
        let mut descriptors = [
            libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: input,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            descriptors[0].revents = 0;
            descriptors[1].revents = 0;
            let poll_wait = deadline.map_or(Wait::Forever, |deadline| {
                Wait::After(deadline.saturating_duration_since(poll_clock()))
            });
            // SAFETY: both descriptors live on this stack frame for the call,
            // and the array has the two entries the count names.
            let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, poll_wait.poll_millis()) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(DirectoryWake::TimedOut);
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(DirectoryWake::TimedOut);
            }
            if descriptors[1].revents != 0 {
                return Ok(DirectoryWake::Input);
            }
            if descriptors[0].revents != 0 {
                let changed = self.drain(entry_name)?;
                if entry_name.is_none() || changed {
                    return Ok(DirectoryWake::Changed);
                }
                if deadline.is_some_and(|deadline| poll_clock() >= deadline) {
                    return Ok(DirectoryWake::TimedOut);
                }
            }
        }
    }

    /// Drains queued records and says whether one named the requested entry.
    /// With no requested name, any record counts as a change.
    fn drain(&self, entry_name: Option<&OsStr>) -> io::Result<bool> {
        let mut buffer = [0_u8; crate::limits::DIRECTORY_WATCH_READ_BYTES];
        let mut matched = false;
        loop {
            match read_fd(self.fd.as_raw_fd(), &mut buffer) {
                Ok(0) => return Ok(matched),
                Ok(read) => {
                    if let Some(entry_name) = entry_name {
                        matched |= contains_entry_event(&buffer[..read], entry_name)?;
                    } else {
                        matched = true;
                    }
                }
                Err(error) => match error.kind() {
                    io::ErrorKind::Interrupted => {}
                    io::ErrorKind::WouldBlock => return Ok(matched),
                    _ => return Err(error),
                },
            }
        }
    }
}

/// The clock a [`DirectoryWatch`] wait measures its deadline on.
fn poll_clock() -> Instant {
    // clock-io-ok: the deadline bounds a real poll of inotify and the input.
    Instant::now()
}

/// Whether an inotify read contains an event for `entry_name` or an overflow
/// that may have dropped that event. Inotify returns whole event records; the
/// byte parser still checks each bound before inspecting the variable-length
/// name so a malformed buffer cannot panic. The record layout is the kernel's
/// `struct inotify_event`, read through libc's definition of it.
fn contains_entry_event(events: &[u8], entry_name: &OsStr) -> io::Result<bool> {
    const HEADER_BYTES: usize = std::mem::size_of::<libc::inotify_event>();
    const MASK_OFFSET: usize = std::mem::offset_of!(libc::inotify_event, mask);
    const NAME_LENGTH_OFFSET: usize = std::mem::offset_of!(libc::inotify_event, len);
    const FIELD_BYTES: usize = std::mem::size_of::<u32>();
    let mut offset = 0;
    let wanted = entry_name.as_bytes();
    while offset < events.len() {
        let header_end = offset.checked_add(HEADER_BYTES).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "inotify event header overflow")
        })?;
        if header_end > events.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "inotify event header is truncated",
            ));
        }
        let mask = u32::from_ne_bytes(
            events[offset + MASK_OFFSET..offset + MASK_OFFSET + FIELD_BYTES]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid inotify mask"))?,
        );
        if mask & libc::IN_Q_OVERFLOW != 0 {
            return Ok(true);
        }
        let raw_name_length = u32::from_ne_bytes(
            events[offset + NAME_LENGTH_OFFSET..offset + NAME_LENGTH_OFFSET + FIELD_BYTES]
                .try_into()
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid inotify name length")
                })?,
        );
        let name_length = usize::try_from(raw_name_length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "inotify name length is too large",
            )
        })?;
        let event_end = header_end.checked_add(name_length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "inotify event length overflow")
        })?;
        if event_end > events.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "inotify event name is truncated",
            ));
        }
        if name_length > 0 {
            let name = &events[header_end..event_end];
            let name = &name[..name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len())];
            if name == wanted {
                return Ok(true);
            }
        }
        offset = event_end;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn raw(stream: &std::os::unix::net::UnixStream) -> RawFd {
        std::os::fd::AsRawFd::as_raw_fd(stream)
    }

    #[test]
    fn a_socket_bound_in_the_directory_wakes_the_watch() {
        let scratch = shepr_test_support::ScratchDir::new("dir-watch-socket");
        let watch = DirectoryWatch::new(scratch.path()).expect("watch the scratch directory");
        let (input, _input_peer) =
            std::os::unix::net::UnixStream::pair().expect("an input that stays open");
        let socket = scratch.join("server.sock");
        let binder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            std::os::unix::net::UnixListener::bind(&socket).expect("bind the test socket")
        });
        let started = Instant::now();
        let wake = watch
            .wait(raw(&input), Duration::from_secs(30))
            .expect("the watch waits");
        let _listener = binder.join().expect("the binder finishes");
        assert_eq!(wake, DirectoryWake::Changed);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    fn inotify_event(mask: u32, name: &[u8]) -> Vec<u8> {
        const HEADER_BYTES: usize = 16;
        let name_length = if name.is_empty() {
            0
        } else {
            (name.len() + 1 + 3) & !3
        };
        let mut event = vec![0; HEADER_BYTES + name_length];
        event[4..8].copy_from_slice(&mask.to_ne_bytes());
        event[12..16].copy_from_slice(
            &u32::try_from(name_length)
                .expect("test name length fits")
                .to_ne_bytes(),
        );
        event[HEADER_BYTES..HEADER_BYTES + name.len()].copy_from_slice(name);
        event
    }

    #[test]
    fn named_event_parser_ignores_other_names_and_treats_overflow_as_a_change() {
        let unrelated = inotify_event(libc::IN_CREATE, b"unrelated");
        assert!(
            !contains_entry_event(&unrelated, OsStr::new("server.sock"))
                .expect("parse unrelated event")
        );
        let target = inotify_event(libc::IN_MOVED_TO, b"server.sock");
        assert!(
            contains_entry_event(&target, OsStr::new("server.sock")).expect("parse target event")
        );
        let overflow = inotify_event(libc::IN_Q_OVERFLOW, b"");
        assert!(
            contains_entry_event(&overflow, OsStr::new("server.sock"))
                .expect("parse overflow event")
        );
    }

    #[test]
    fn a_closed_input_ends_the_wait() {
        let scratch = shepr_test_support::ScratchDir::new("dir-watch-input");
        let watch = DirectoryWatch::new(scratch.path()).expect("watch the scratch directory");
        let (input, peer) = std::os::unix::net::UnixStream::pair().expect("an input pair");
        drop(peer);
        let wake = watch
            .wait(raw(&input), Duration::from_secs(30))
            .expect("the watch waits");
        assert_eq!(wake, DirectoryWake::Input);
    }

    #[test]
    fn a_missing_directory_cannot_be_watched() {
        let scratch = shepr_test_support::ScratchDir::new("dir-watch-missing");
        assert!(DirectoryWatch::new(&scratch.join("absent")).is_err());
    }
}
