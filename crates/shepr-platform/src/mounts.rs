//! Mount lookup without accessing the filesystem being looked up.

use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// Failures of a component-by-component physical path walk. Keep Linux errno
/// spelling here so callers preserve filesystem error identity without libc.
#[derive(Clone, Copy, Debug)]
pub enum PathWalkFailure {
    TooManySymlinks,
    NotDirectory,
}

impl From<PathWalkFailure> for std::io::Error {
    fn from(failure: PathWalkFailure) -> Self {
        Self::from_raw_os_error(match failure {
            PathWalkFailure::TooManySymlinks => libc::ELOOP,
            PathWalkFailure::NotDirectory => libc::ENOTDIR,
        })
    }
}

/// A snapshot of this process's mount namespace. Reading mountinfo only
/// consults procfs; lookup never stats or canonicalizes the supplied path.
#[derive(Clone, Debug, Default)]
pub struct MountTable {
    entries: Vec<(PathBuf, String)>,
}

impl MountTable {
    pub fn read() -> std::io::Result<Self> {
        Ok(Self::from_mountinfo(&std::fs::read_to_string(
            "/proc/self/mountinfo",
        )?))
    }

    /// Builds a snapshot from the Linux mountinfo format.
    pub fn from_mountinfo(text: &str) -> Self {
        let entries = text
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_ascii_whitespace().collect();
                let device = fields.get(2)?.to_string();
                let encoded = fields.get(4)?.as_bytes();
                // The kernel escapes space, tab, newline and backslash in a mount
                // point as a backslash and three octal digits.
                let mut decoded = Vec::new();
                let mut index = 0;
                while index < encoded.len() {
                    let escaped = encoded
                        .get(index + 1..index + 4)
                        .filter(|digits| {
                            encoded[index] == b'\\'
                                && digits.iter().all(|b| (b'0'..=b'7').contains(b))
                        })
                        .and_then(|digits| {
                            let value = digits
                                .iter()
                                .fold(0_u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                            u8::try_from(value).ok()
                        });
                    if let Some(value) = escaped {
                        decoded.push(value);
                        index += 4;
                    } else {
                        decoded.push(encoded[index]);
                        index += 1;
                    }
                }
                Some((PathBuf::from(std::ffi::OsString::from_vec(decoded)), device))
            })
            .collect();
        Self { entries }
    }

    /// Whether the deepest mount is quarantined. A healthy nested mount is
    /// independent of its parent. Non-mount paths are a conservative fallback
    /// when mountinfo was unavailable or a caller names a synthetic step.
    pub fn is_quarantined(&self, path: &Path, roots: &[PathBuf]) -> bool {
        let mounts = self.stall_paths(path);
        roots.iter().any(|root| {
            mounts.contains(root)
                || (!self.entries.iter().any(|(mount, _)| mount == root) && path.starts_with(root))
        })
    }

    /// Mount points sharing the deepest matching mount's device, including
    /// bind mounts. Paths must be absolute and have resolved symlink parents.
    pub fn stall_paths(&self, path: &Path) -> Vec<PathBuf> {
        let Some((_, device)) = self
            .entries
            .iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
        else {
            // Without namespace evidence, quarantine conservatively rather
            // than spending one thread for each path on an unknown mount.
            return vec![PathBuf::from("/")];
        };
        self.entries
            .iter()
            .filter(|(_, candidate)| candidate == device)
            .map(|(root, _)| root.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_mount_is_independent_of_quarantined_parent() {
        let table = MountTable::from_mountinfo(
            "1 0 8:1 / / rw - ext4 root rw\n2 1 0:42 / /net rw - nfs host:/ rw\n3 2 8:2 / /net/healthy rw - ext4 other rw\n",
        );
        assert!(table.is_quarantined(Path::new("/net/b"), &[PathBuf::from("/net")]));
        assert!(!table.is_quarantined(Path::new("/net/healthy/b"), &[PathBuf::from("/net")]));
    }

    #[test]
    fn lookup_decodes_paths_and_groups_bind_mounts_without_stat() {
        let table = MountTable::from_mountinfo(
            "1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 0:42 / /net rw - nfs host:/ rw\n3 1 0:42 /sub /alias\\040name rw - nfs host:/ rw\n",
        );
        assert_eq!(
            table.stall_paths(Path::new("/net/missing/a")),
            [PathBuf::from("/net"), PathBuf::from("/alias name")]
        );
        assert_eq!(
            table.stall_paths(Path::new("/network")),
            [PathBuf::from("/")]
        );
    }
}
