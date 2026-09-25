use std::{ffi::OsStr, process::Command};

/// Builds a subprocess whose stdio is controlled by the caller.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}
