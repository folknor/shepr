//! Reading selected variables from another process's initial environment.
//!
//! `/proc/<pid>/environ` is the environment the process was exec'd with, not
//! what it may have set since, and reading it is subject to ptrace access
//! checks. What this returns is best-effort evidence, not an atomic snapshot:
//! the process's incarnation, mount namespace and root are checked before and
//! after the read, and anything that cannot be shown unchanged is refused.

use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;

use crate::limits::PROCESS_ENVIRON_BYTE_LIMIT;
use crate::{Pid, ProcStat};

/// One incarnation of a process: a pid and the start time it was read with.
/// A reused pid has a different start time; an `exec` keeps both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessInstance {
    pub pid: Pid,
    pub start_ticks: u64,
}

impl ProcessInstance {
    /// The incarnation currently holding `pid`, if any.
    pub fn current(pid: Pid) -> io::Result<Self> {
        let stat = ProcStat::read(pid)?;
        if stat.state.is_finished() {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(Self {
            pid,
            start_ticks: stat.start_ticks,
        })
    }

    /// Whether `pid` is still held by this incarnation.
    pub fn is_live(self) -> bool {
        Self::current(self.pid).is_ok_and(|current| current == self)
    }
}

/// Why an environment read gave no evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironRefusal {
    /// The incarnation is gone or its pid now names another process.
    NotLive,
    /// The process sees another mount namespace or root, so its paths do not
    /// name the files this process would open.
    ForeignFilesystemView,
    /// The environment or the identity facts could not be read.
    Unreadable(io::ErrorKind),
    /// The environment is larger than the read bound.
    Truncated,
    /// An entry is not NUL-terminated.
    Malformed,
    /// An allowlisted name appears more than once.
    Duplicate(Vec<u8>),
}

/// Reads the allowlisted variables of `process`'s initial environment, as raw
/// bytes, in allowlist order; `None` for a name that is absent. Blocking: a
/// `/proc` read has no time bound, so call it where a stuck read is
/// contained.
pub fn read_allowlisted_environ(
    process: ProcessInstance,
    allowlist: &[&[u8]],
) -> Result<Vec<Option<Vec<u8>>>, EnvironRefusal> {
    let before = FilesystemView::of(process.pid)?;
    if !process.is_live() {
        return Err(EnvironRefusal::NotLive);
    }
    if before != FilesystemView::own()? {
        return Err(EnvironRefusal::ForeignFilesystemView);
    }
    let bytes = read_environ(process.pid)?;
    // Recheck after the read: start time survives setns, unshare, chroot and
    // exec, so the view is compared again as well as the incarnation.
    if !process.is_live() {
        return Err(EnvironRefusal::NotLive);
    }
    if FilesystemView::of(process.pid)? != before {
        return Err(EnvironRefusal::ForeignFilesystemView);
    }
    parse_allowlisted(&bytes, allowlist)
}

/// The kernel identities of a process's mount namespace and root directory.
/// The root carries its mount id as well as device and inode: a bind mount of
/// `/` shares both with `/` but is another mount, under which absolute paths
/// can resolve differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FilesystemView {
    mount_namespace: (u64, u64),
    root: RootIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RootIdentity {
    device: (u32, u32),
    inode: u64,
    /// Which mount-id flavour the kernel reported, and the id.
    mount: (libc::c_uint, u64),
}

/// The root's identity through `statx`, requiring a mount id: without one the
/// view cannot be shown to be ours.
fn root_identity(path: &str) -> Result<RootIdentity, EnvironRefusal> {
    let c_path = std::ffi::CString::new(path)
        .map_err(|_| EnvironRefusal::Unreadable(io::ErrorKind::InvalidInput))?;
    // SAFETY: zero is a valid bit pattern for `statx`, which the call fills.
    let mut buffer: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a live, exclusive output buffer.
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            0,
            libc::STATX_INO | libc::STATX_MNT_ID | libc::STATX_MNT_ID_UNIQUE,
            &mut buffer,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.kind() {
            io::ErrorKind::NotFound => EnvironRefusal::NotLive,
            kind => EnvironRefusal::Unreadable(kind),
        });
    }
    let mount_kind = if buffer.stx_mask & libc::STATX_MNT_ID_UNIQUE != 0 {
        libc::STATX_MNT_ID_UNIQUE
    } else if buffer.stx_mask & libc::STATX_MNT_ID != 0 {
        libc::STATX_MNT_ID
    } else {
        return Err(EnvironRefusal::Unreadable(io::ErrorKind::Unsupported));
    };
    if buffer.stx_mask & libc::STATX_INO == 0 {
        return Err(EnvironRefusal::Unreadable(io::ErrorKind::Unsupported));
    }
    Ok(RootIdentity {
        device: (buffer.stx_dev_major, buffer.stx_dev_minor),
        inode: buffer.stx_ino,
        mount: (mount_kind, buffer.stx_mnt_id),
    })
}

impl FilesystemView {
    fn of(pid: Pid) -> Result<Self, EnvironRefusal> {
        Self::at(&format!("/proc/{pid}"))
    }

    fn own() -> Result<Self, EnvironRefusal> {
        Self::at("/proc/self")
    }

    fn at(base: &str) -> Result<Self, EnvironRefusal> {
        let identity = |path: String| {
            std::fs::metadata(path)
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => EnvironRefusal::NotLive,
                    kind => EnvironRefusal::Unreadable(kind),
                })
        };
        Ok(Self {
            mount_namespace: identity(format!("{base}/ns/mnt"))?,
            root: root_identity(&format!("{base}/root"))?,
        })
    }
}

fn read_environ(pid: Pid) -> Result<Vec<u8>, EnvironRefusal> {
    let unreadable = |error: io::Error| match error.kind() {
        io::ErrorKind::NotFound => EnvironRefusal::NotLive,
        kind => EnvironRefusal::Unreadable(kind),
    };
    let file = std::fs::File::open(format!("/proc/{pid}/environ")).map_err(unreadable)?;
    let limit = u64::try_from(PROCESS_ENVIRON_BYTE_LIMIT)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() > PROCESS_ENVIRON_BYTE_LIMIT {
        return Err(EnvironRefusal::Truncated);
    }
    Ok(bytes)
}

fn parse_allowlisted(
    bytes: &[u8],
    allowlist: &[&[u8]],
) -> Result<Vec<Option<Vec<u8>>>, EnvironRefusal> {
    let mut values: Vec<Option<Vec<u8>>> = vec![None; allowlist.len()];
    if bytes.is_empty() {
        return Ok(values);
    }
    let Some(entries) = bytes.strip_suffix(&[0]) else {
        return Err(EnvironRefusal::Malformed);
    };
    for entry in entries.split(|&byte| byte == 0) {
        // An entry without `=` names nothing a lookup could find.
        let Some(split) = entry.iter().position(|&byte| byte == b'=') else {
            continue;
        };
        let (name, value) = (&entry[..split], &entry[split + 1..]);
        if let Some(index) = allowlist.iter().position(|wanted| *wanted == name) {
            if values[index].is_some() {
                return Err(EnvironRefusal::Duplicate(name.to_vec()));
            }
            values[index] = Some(value.to_vec());
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlisted_values_come_back_in_allowlist_order_as_raw_bytes() {
        let environ = b"HOME=/home/a\0CODEX_HOME=/srv/\xffcodex\0OTHER=x\0";
        let values =
            parse_allowlisted(environ, &[b"CODEX_HOME", b"HOME", b"MISSING"]).expect("well formed");
        assert_eq!(
            values,
            vec![
                Some(b"/srv/\xffcodex".to_vec()),
                Some(b"/home/a".to_vec()),
                None
            ]
        );
    }

    #[test]
    fn an_empty_value_is_present_and_distinct_from_absent() {
        let values = parse_allowlisted(b"CODEX_HOME=\0", &[b"CODEX_HOME"]).expect("parse");
        assert_eq!(values, vec![Some(Vec::new())]);
    }

    #[test]
    fn a_missing_terminator_and_a_duplicate_are_refused() {
        assert_eq!(
            parse_allowlisted(b"HOME=/a\0HOME", &[b"HOME"]),
            Err(EnvironRefusal::Malformed)
        );
        assert_eq!(
            parse_allowlisted(b"HOME=/a\0HOME=/b\0", &[b"HOME"]),
            Err(EnvironRefusal::Duplicate(b"HOME".to_vec()))
        );
        // A duplicate of a name nobody asked for is not this reader's concern.
        assert!(parse_allowlisted(b"X=1\0X=2\0", &[b"HOME"]).is_ok());
    }

    #[test]
    fn the_server_reads_its_own_environment() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let pid = Pid::new(std::process::id()).expect("own pid");
        let instance = ProcessInstance::current(pid).expect("own instance");
        let values = read_allowlisted_environ(instance, &[b"PATH"]).expect("own environ");
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn a_stale_incarnation_is_refused() {
        let pid = Pid::new(std::process::id()).expect("own pid");
        let current = ProcessInstance::current(pid).expect("own instance");
        let stale = ProcessInstance {
            pid,
            start_ticks: current.start_ticks.wrapping_add(1),
        };
        assert_eq!(
            read_allowlisted_environ(stale, &[b"PATH"]),
            Err(EnvironRefusal::NotLive)
        );
    }
}
