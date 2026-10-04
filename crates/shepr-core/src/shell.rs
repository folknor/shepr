//! The validated pane shell path, shared by config and the PTY.

use std::path::{Path, PathBuf};

/// An absolute pane shell path accepted by configuration validation.
///
/// The validation callback owns executable access and shell-name policy. Keeping
/// the path carrier here lets the PTY consume it without depending on config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShell(PathBuf);

impl ResolvedShell {
    /// Mint a shell only after the caller's shell validation succeeds.
    ///
    /// The check is a callback because recognizing a usable shell name lives in
    /// shepr-platform, which core cannot depend on; moving the type into
    /// platform instead would tie pty and config to it for one check, and test
    /// fixtures pass a permissive check so stand-in shells are accepted.
    pub fn validate(
        path: PathBuf,
        validate: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("pane shell must be an absolute path".to_owned());
        }
        validate(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_shell_is_never_minted() {
        let mut validated = false;
        let err = ResolvedShell::validate("zsh".into(), |_| {
            validated = true;
            Ok(())
        })
        .expect_err("relative shell");
        assert!(err.contains("absolute"));
        assert!(!validated, "the caller's check never sees a relative path");
    }
}
