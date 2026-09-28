//! PTY allocation and child spawn.
//!
//! Shepr owns this directly on libc rather than delegating to
//! `alacritty_terminal::tty`: that module cannot remove inherited environment
//! variables or set a login-shell argv0, injects its own environment
//! (`ALACRITTY_WINDOW_ID`, `WINDOWID`), keeps the `Child` behind a shared
//! reference with a blocking SIGHUP-and-wait `Drop`, registers a SIGCHLD
//! handler per PTY, and exits the process when a resize ioctl fails. Shepr's
//! PTY actor (`crate::actor`) owns the master fd, the IO loop, and
//! resizing; this module only opens the PTY and starts the child.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};

use crate::command::PtyCommand;
use crate::fd;

/// Both ends of a freshly opened PTY. Both fds are close-on-exec.
pub struct OpenedPty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

/// A running child and the parent's only handle on its PTY: the master fd.
pub struct SpawnedPty {
    pub master_fd: OwnedFd,
    pub child: Child,
}

/// Open a PTY pair with the given grid size (pixel size starts at zero; the
/// actor reports pixel geometry on resize).
pub fn open_pty(rows: u16, cols: u16) -> io::Result<OpenedPty> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `master` and `slave` are live locals openpty writes one fd each
    // into; the name buffer is null (openpty then writes no name), the termios
    // pointer is null (keep defaults), and `size` is a valid winsize it only
    // reads. None of the pointers is retained after the call.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // Own both fds before anything else can fail so they are always closed.
    // SAFETY: openpty succeeded, so both are fresh open fds that nothing else
    // in this process owns; each is wrapped exactly once.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: as for `master`.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    fd::set_cloexec(master.as_raw_fd())?;
    fd::set_cloexec(slave.as_raw_fd())?;
    enable_utf8_input(&master);
    Ok(OpenedPty { master, slave })
}

/// Mark the line discipline as UTF-8 so canonical-mode erase removes whole
/// characters. Best effort, as in alacritty's tty setup: the pane still works
/// without it, only canonical-mode erase of multibyte characters degrades, so
/// a failure is logged rather than failing the spawn.
fn enable_utf8_input(master: &OwnedFd) {
    // SAFETY: termios is a plain C struct of integers and arrays, for which
    // all-zero bytes are a valid value; tcgetattr overwrites it below.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `master` is an open fd borrowed for the call, and `termios` is a
    // live, writable termios that tcgetattr fills in and does not retain.
    if unsafe { libc::tcgetattr(master.as_raw_fd(), &mut termios) } != 0 {
        let err = io::Error::last_os_error();
        tracing::warn!(%err, "could not read PTY attributes to enable UTF-8 input");
        return;
    }
    termios.c_iflag |= libc::IUTF8;
    // SAFETY: as above; tcsetattr only reads `termios`.
    if unsafe { libc::tcsetattr(master.as_raw_fd(), libc::TCSANOW, &termios) } != 0 {
        let err = io::Error::last_os_error();
        tracing::warn!(%err, "could not enable UTF-8 input on the PTY");
    }
}

/// Spawn `cmd` as a session leader whose controlling terminal and stdio are
/// `slave`. The caller keeps ownership of `slave` and should drop it once no
/// more children will be spawned into this PTY.
pub fn spawn_in_pty(slave: &OwnedFd, cmd: &PtyCommand) -> io::Result<Child> {
    let mut command = cmd.to_std_command()?;
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: `prepare_pty_child` only uses async-signal-safe operations on
    // the child's own process state.
    unsafe {
        command.pre_exec(prepare_pty_child);
    }
    let mut child = command.spawn()?;
    // Command never creates pipes for explicit fds, but make sure the parent
    // holds no slave handles through the Child.
    child.stdin.take();
    child.stdout.take();
    child.stderr.take();
    Ok(child)
}

/// Open a PTY, spawn `cmd` into it, and return the child with the master fd.
/// The parent's slave fd is closed before returning.
pub fn spawn_pty(rows: u16, cols: u16, cmd: &PtyCommand) -> io::Result<SpawnedPty> {
    let OpenedPty { master, slave } = open_pty(rows, cols)?;
    let child = spawn_in_pty(&slave, cmd)?;
    drop(slave);
    Ok(SpawnedPty {
        master_fd: master,
        child,
    })
}

/// Runs in the forked child before exec. std has already dup'd the slave onto
/// fds 0-2 and changed directory.
///
/// Every call here is async-signal-safe (the forked child of a multithreaded
/// process may only make such calls) and touches only this child's own process
/// state, never memory shared with the parent.
fn prepare_pty_child() -> io::Result<()> {
    // Clear dispositions and the signal mask inherited from the server
    // (ignored signals survive exec; handlers do not).
    // Linux architectures use signal numbers up to 64 or 128. SIGKILL and
    // SIGSTOP cannot have their dispositions changed; unsupported numbers and
    // libc-reserved signals report EINVAL and are skipped below.
    // SAFETY: sigaction is async-signal-safe; this stack value is zeroed and
    // then initialized as a default action with an empty mask.
    let mut default_action: libc::sigaction = unsafe { std::mem::zeroed() };
    default_action.sa_sigaction = libc::SIG_DFL;
    default_action.sa_flags = 0;
    // SAFETY: `default_action.sa_mask` is a live, writable sigset_t.
    if unsafe { libc::sigemptyset(&mut default_action.sa_mask) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for signo in 1..=128 {
        if matches!(signo, libc::SIGKILL | libc::SIGSTOP) {
            continue;
        }
        // SAFETY: sigaction is async-signal-safe. The signal number is in the
        // Linux range that can be changed, and both pointers remain valid for
        // the duration of the call; the null old-action pointer is permitted.
        if unsafe { libc::sigaction(signo, &default_action, std::ptr::null_mut()) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINVAL) {
                continue;
            }
            return Err(err);
        }
    }
    // SAFETY: sigset_t is a plain bit array; all-zero is a valid value, and
    // sigemptyset below sets it properly anyway.
    let mut empty_set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // A mask left in place would start the pane child with the server's
    // blocked signals, so a failure here fails the spawn.
    // SAFETY: `empty_set` is a live, writable sigset_t on this stack frame.
    if unsafe { libc::sigemptyset(&mut empty_set) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: sigprocmask is async-signal-safe; it only reads `empty_set`,
    // and the old-mask pointer is null.
    if unsafe { libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // New session, then take the PTY (already on stdin) as the controlling
    // terminal so job control and SIGWINCH reach the child.
    // SAFETY: setsid(2) takes no arguments and touches no memory.
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: TIOCSCTTY on fd 0 (std has dup'd the PTY slave there) takes an
    // integer argument, not a pointer, so no memory is read or written.
    if unsafe { libc::ioctl(0, libc::TIOCSCTTY, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    mark_inherited_fds_cloexec()?;
    Ok(())
}

/// Stop descriptors leaked into the server (for example by a desktop session)
/// from reaching pane processes. They are marked close-on-exec rather than
/// closed so std's own exec-error pipe keeps working and a failed exec is still
/// reported by `spawn()`.
fn mark_inherited_fds_cloexec() -> io::Result<()> {
    let first_fd: libc::c_uint = 3;
    // SAFETY: close_range(2) with CLOSE_RANGE_CLOEXEC only sets a flag on
    // this child's own descriptor table; it takes integers and reads no memory.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first_fd,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result == 0 {
        return Ok(());
    }

    // Kernels before 5.11 lack CLOSE_RANGE_CLOEXEC. Walk procfs with raw
    // syscalls and a stack buffer so the fallback stays async-signal-safe.
    const PROC_SELF_FD: &[u8] = b"/proc/self/fd\0";
    // SAFETY: `PROC_SELF_FD` is a live, NUL-terminated path and open(2) reads
    // it during the call without retaining the pointer.
    let directory_fd = unsafe {
        libc::open(
            PROC_SELF_FD.as_ptr().cast(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if directory_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    const DIRENT_RECLEN_OFFSET: usize = 16;
    const DIRENT_NAME_OFFSET: usize = 19;
    let mut buffer = [0u8; 4096];
    loop {
        // SAFETY: getdents64 writes at most `buffer.len()` bytes into this
        // live stack buffer and reads only the integer directory fd.
        let bytes_read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory_fd,
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if bytes_read == 0 {
            break;
        }
        if bytes_read < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }

        let Ok(bytes_read) = usize::try_from(bytes_read) else {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        };
        if bytes_read > buffer.len() {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let mut offset = 0;
        while offset < bytes_read {
            if bytes_read - offset < DIRENT_NAME_OFFSET {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            let reclen_offset = offset + DIRENT_RECLEN_OFFSET;
            let record_len =
                u16::from_ne_bytes([buffer[reclen_offset], buffer[reclen_offset + 1]]) as usize;
            if record_len <= DIRENT_NAME_OFFSET || offset + record_len > bytes_read {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            let name = &buffer[offset + DIRENT_NAME_OFFSET..offset + record_len];
            let Some(name_len) = name.iter().position(|byte| *byte == 0) else {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            };
            let mut fd = 0i32;
            let mut valid_fd = name_len > 0;
            for byte in &name[..name_len] {
                if !byte.is_ascii_digit() {
                    valid_fd = false;
                    break;
                }
                let Some(next_fd) = fd
                    .checked_mul(10)
                    .and_then(|fd| fd.checked_add(i32::from(*byte - b'0')))
                else {
                    valid_fd = false;
                    break;
                };
                fd = next_fd;
            }
            if valid_fd && fd > 2 {
                // SAFETY: fcntl only reads integer arguments. A descriptor
                // closed since the directory snapshot simply reports EBADF.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                if flags >= 0 {
                    // SAFETY: as above; this changes only the child's own
                    // descriptor flags and reads no memory.
                    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EBADF) {
                        return Err(err);
                    }
                }
            }
            offset += record_len;
        }
    }

    // SAFETY: `directory_fd` was returned by open above and is owned here.
    unsafe { libc::close(directory_fd) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{self, Step};
    use std::io::Read;
    use std::sync::{Mutex, OnceLock};

    fn fixture_command(steps: &[Step]) -> PtyCommand {
        let mut cmd = PtyCommand::new(fixture::path());
        cmd.args(fixture::args(steps));
        cmd
    }

    fn pty_fd_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn parent_pty_fd_targets() -> Vec<String> {
        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
            return Vec::new();
        };
        let mut targets: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .map(|target| target.to_string_lossy().into_owned())
            .filter(|target| target.starts_with("/dev/pts/") || target == "/dev/ptmx")
            .collect();
        targets.sort();
        targets
    }

    fn parent_pty_fd_count() -> usize {
        parent_pty_fd_targets().len()
    }

    #[test]
    fn pty_spawn_leaves_one_parent_pty_fd() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let before = parent_pty_fd_count();
        let mut cmd = fixture_command(&[Step::Cat]);
        cmd.env("SHEPR_ENV", "1");

        let mut spawned = spawn_pty(24, 80, &cmd).expect("pty setup succeeds");
        let after_spawn = parent_pty_fd_count();

        assert_eq!(
            after_spawn,
            before + 1,
            "pty setup should leave only the Shepr-owned master fd in the parent: {:?}",
            parent_pty_fd_targets()
        );

        spawned.child.kill().expect("kill the cat child");
        spawned.child.wait().expect("reap the cat child");
        drop(spawned.master_fd);
    }

    #[test]
    fn child_is_session_leader_with_pty_as_controlling_terminal() {
        let cmd = fixture_command(&[Step::Sleep(std::time::Duration::from_secs(30))]);
        let mut spawned = spawn_pty(24, 80, &cmd).expect("pty setup succeeds");
        let pid = libc::pid_t::try_from(spawned.child.id()).expect("pid fits pid_t");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let foreground = loop {
            // SAFETY: tcgetpgrp(3) on an fd `spawned` keeps open; no memory.
            let pgrp = unsafe { libc::tcgetpgrp(spawned.master_fd.as_raw_fd()) };
            if pgrp == pid || std::time::Instant::now() >= deadline {
                break pgrp;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        // SAFETY: getsid(2) takes a pid and touches no memory.
        let session = unsafe { libc::getsid(pid) };

        spawned.child.kill().expect("kill the sleeping child");
        spawned.child.wait().expect("reap the sleeping child");
        assert_eq!(session, pid, "child must lead its own session");
        assert_eq!(foreground, pid, "child must own the PTY foreground group");
    }

    #[test]
    fn child_output_reaches_master_and_exit_status_is_reported() {
        let cmd = fixture_command(&[Step::Print("shepr-pty-ok".into()), Step::Exit(7)]);
        let mut spawned = spawn_pty(24, 80, &cmd).expect("pty setup succeeds");
        let status = spawned.child.wait().expect("wait for child");
        assert_eq!(status.code(), Some(7));

        let mut master = std::fs::File::from(spawned.master_fd);
        let mut output = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => output.extend_from_slice(&buf[..read]),
                // EIO means the slave side is closed: all output was read.
                Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
                Err(err) => panic!("read pty master: {err}"),
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains("shepr-pty-ok"),
            "unexpected pty output: {output:?}"
        );
    }
}
