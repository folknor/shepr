//! Shared helpers for integration tests.

use std::path::Path;

/// Type probes for test assertions. Absence is `false`; any other stat error
/// (`EACCES`, `ELOOP`) fails the test instead of reading as absence, which is
/// what `Path::is_file` and `Path::is_dir` would do.
pub(super) trait StatPath {
    fn stat_is_file(&self) -> bool;
    fn stat_is_dir(&self) -> bool;
}

impl StatPath for Path {
    fn stat_is_file(&self) -> bool {
        super::file_ops::is_file(self)
            .unwrap_or_else(|error| panic!("stat {}: {error}", self.display()))
    }

    fn stat_is_dir(&self) -> bool {
        super::file_ops::is_dir(self)
            .unwrap_or_else(|error| panic!("stat {}: {error}", self.display()))
    }
}

pub(super) fn symlink_file(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create symlink");
}
