const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;
pub(crate) const REMOTE_EXECUTABLE_ROOT: &str = "/";
pub(crate) const REMOTE_MISE_SHIM_SUFFIX: &str = "/mise/shims/shepr";

/// A checked absolute path to the Shepr executable on a remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExecutable(String);

impl RemoteExecutable {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || !value.starts_with(REMOTE_EXECUTABLE_ROOT) {
            return Err("remote Shepr executable path must be absolute".into());
        }
        if value.len() > MAX_REMOTE_EXECUTABLE_BYTES {
            return Err(format!(
                "remote Shepr executable path must be at most {MAX_REMOTE_EXECUTABLE_BYTES} bytes"
            ));
        }
        if value.chars().any(char::is_control) {
            return Err("remote Shepr executable path must not contain control characters".into());
        }
        // The command is nested inside /bin/sh -c and then parsed by the remote
        // login shell. Keep paths as plain shell words because nested quote
        // escaping is not reliable across the non-POSIX login shells we support.
        if !Self::is_shell_plain_word(&value) {
            return Err(
                "remote Shepr executable path must contain only unquoted shell-safe characters"
                    .into(),
            );
        }
        if value.ends_with(REMOTE_MISE_SHIM_SUFFIX) {
            return Err("remote Shepr executable path must not be a mise shim".into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    // Keep this predicate until the typed parse error can be re-exported from
    // `machine.rs`; callers need the rejection reason reachable through that API.
    /// Whether `value` would be rejected only because a nested remote shell
    /// command could not use it as a plain word.
    pub fn needs_shell_quoting(value: &str) -> bool {
        !value.is_empty() && !Self::is_shell_plain_word(value)
    }

    pub(crate) fn is_shell_plain_word(value: &str) -> bool {
        !value.is_empty()
            && value.chars().all(|ch| {
                ch.is_ascii_alphanumeric()
                    || matches!(
                        ch,
                        '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                    )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_executable_accepts_only_cacheable_absolute_paths() {
        for (path, valid) in [
            ("/home/a b/shepr", false),
            ("$HOME/.local/bin/shepr", false),
            ("/home/user/.local/share/mise/shims/shepr", false),
            ("/bin/shepr\nmalformed", false),
        ] {
            assert_eq!(
                RemoteExecutable::parse(path.to_owned()).is_ok(),
                valid,
                "{path}"
            );
        }
    }

    #[test]
    fn shell_quote_uses_the_remote_executable_plain_word_predicate() {
        for value in [
            "",
            "/usr/bin/shepr",
            "user@host:22",
            "/home/a b/shepr",
            "/home/user's/shepr",
            "/home/$user/shepr",
            "/opt/shepr-0.1+dev",
        ] {
            assert_eq!(
                crate::shell_quote(value) == value,
                RemoteExecutable::is_shell_plain_word(value),
                "{value:?}"
            );
        }
    }
}
