//! A wait for entries to appear in one directory, through inotify, alongside
//! one input descriptor whose readability or close also ends the wait.

use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use crate::child_io::Wait;

/// What ended a [`DirectoryWatch::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryWake {
    /// An entry was created in, moved into, or changed in the directory.
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

    /// Blocks until the directory changes, `input` is readable or closed, or
    /// `wait` passes. Pending directory events are drained before returning,
    /// so the next wait blocks until a later change.
    pub fn wait(&self, input: RawFd, wait: impl Into<Wait>) -> io::Result<DirectoryWake> {
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
        // SAFETY: both descriptors live on this stack frame for the call, and
        // the array has the two entries the count names.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, wait.into().poll_millis()) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(DirectoryWake::TimedOut);
            }
            return Err(error);
        }
        if descriptors[1].revents != 0 {
            return Ok(DirectoryWake::Input);
        }
        if descriptors[0].revents != 0 {
            self.drain()?;
            return Ok(DirectoryWake::Changed);
        }
        Ok(DirectoryWake::TimedOut)
    }

    fn drain(&self) -> io::Result<()> {
        let mut buffer = [0_u8; crate::limits::DIRECTORY_WATCH_READ_BYTES];
        loop {
            match crate::child_io::read_fd(self.fd.as_raw_fd(), &mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) => match error.kind() {
                    io::ErrorKind::Interrupted => {}
                    io::ErrorKind::WouldBlock => return Ok(()),
                    _ => return Err(error),
                },
            }
        }
    }
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
