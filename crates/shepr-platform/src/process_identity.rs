use crate::{Pid, ProcStat, ProcState};
use std::io;
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcessIdentity {
    pid: Pid,
    start_ticks: u64,
    pid_namespace: (u64, u64),
}

struct ProcessSnapshot {
    start_ticks: u64,
    pid_namespace: (u64, u64),
    state: ProcState,
}

impl ProcessIdentity {
    pub(super) fn current() -> io::Result<Self> {
        let pid = Pid::new(std::process::id()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid current process id")
        })?;
        if proc_self_pid()? != pid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the mounted /proc view does not match the current PID namespace",
            ));
        }
        let snapshot = process_snapshot(pid)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "current process is absent from /proc",
            )
        })?;
        Ok(Self {
            pid,
            start_ticks: snapshot.start_ticks,
            pid_namespace: snapshot.pid_namespace,
        })
    }

    /// Serialize the process identity used by runtime ownership markers.
    pub(super) fn tag(self) -> String {
        format!(
            "{:08x}-{:016x}-{:016x}-{:016x}",
            self.pid.get(),
            self.start_ticks,
            self.pid_namespace.0,
            self.pid_namespace.1
        )
    }

    /// Parse a serialized runtime ownership identity.
    pub(super) fn parse_tag(value: &str) -> Option<Self> {
        let mut fields = value.split('-');
        let identity = Self {
            pid: Pid::new(u32::from_str_radix(fields.next()?, 16).ok()?)?,
            start_ticks: u64::from_str_radix(fields.next()?, 16).ok()?,
            pid_namespace: (
                u64::from_str_radix(fields.next()?, 16).ok()?,
                u64::from_str_radix(fields.next()?, 16).ok()?,
            ),
        };
        fields.next().is_none().then_some(identity)
    }

    /// A process is gone only when the current proc view is in its recorded
    /// PID namespace and `/proc` proves the pid is absent, reused, or a zombie.
    pub(super) fn is_provably_gone(self) -> bool {
        let Some(current_pid) = Pid::new(std::process::id()) else {
            return false;
        };
        if proc_self_pid().ok() != Some(current_pid) {
            return false;
        }
        let Ok(current_namespace) = pid_namespace_identity(current_pid) else {
            return false;
        };
        if current_namespace != self.pid_namespace {
            return false;
        }
        match process_snapshot(self.pid) {
            Ok(None) => true,
            Ok(Some(snapshot)) => {
                snapshot.start_ticks != self.start_ticks
                    || snapshot.pid_namespace != self.pid_namespace
                    || snapshot.state.is_finished()
            }
            Err(_) => false,
        }
    }
}

fn proc_self_pid() -> io::Result<Pid> {
    std::fs::read_link("/proc/self")?
        .to_str()
        .and_then(|pid| pid.parse::<u32>().ok())
        .and_then(Pid::new)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/self target"))
}

fn process_snapshot(pid: Pid) -> io::Result<Option<ProcessSnapshot>> {
    let stat = match ProcStat::read(pid) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let pid_namespace = match pid_namespace_identity(pid) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    Ok(Some(ProcessSnapshot {
        start_ticks: stat.start_ticks,
        pid_namespace,
        state: stat.state,
    }))
}

fn pid_namespace_identity(pid: Pid) -> io::Result<(u64, u64)> {
    let metadata = std::fs::metadata(format!("/proc/{pid}/ns/pid"))?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_identity_is_live_and_round_trips() {
        let identity = ProcessIdentity::current().expect("current process identity");
        let tag = identity.tag();
        assert_eq!(ProcessIdentity::parse_tag(&tag), Some(identity));
        assert!(!identity.is_provably_gone());
    }

    #[test]
    fn absent_process_in_the_same_pid_namespace_is_provably_gone() {
        let current = ProcessIdentity::current().expect("current process identity");
        let absent = ProcessIdentity {
            pid: Pid::new(i32::MAX as u32).expect("valid absent pid"),
            start_ticks: 1,
            pid_namespace: current.pid_namespace,
        };
        assert!(absent.is_provably_gone());
    }

    #[test]
    fn malformed_identity_tags_are_rejected() {
        for tag in ["", "1-2-3", "zz-2-3-4", "1-2-3-4-extra"] {
            assert_eq!(ProcessIdentity::parse_tag(tag), None, "{tag:?}");
        }
    }
}
