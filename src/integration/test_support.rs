//! Shared helpers for integration tests.

use std::path::Path;

pub(super) fn symlink_file(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).expect("create symlink");
    true
}
