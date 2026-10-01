use std::fmt;
use std::path::Path;

use crate::limits::MAX_REMOTE_EXECUTABLE_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteExecutableError {
    NotAbsolute,
    TooLong,
    ContainsControlCharacters,
    NeedsShellQuoting,
}

impl fmt::Display for RemoteExecutableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute => {
                formatter.write_str("remote Shepr executable path must be absolute")
            }
            Self::TooLong => write!(
                formatter,
                "remote Shepr executable path must be at most {MAX_REMOTE_EXECUTABLE_BYTES} bytes"
            ),
            Self::ContainsControlCharacters => formatter
                .write_str("remote Shepr executable path must not contain control characters"),
            Self::NeedsShellQuoting => formatter.write_str(
                "remote Shepr executable path must contain only unquoted shell-safe characters",
            ),
        }
    }
}

impl std::error::Error for RemoteExecutableError {}

/// A checked absolute path to the Shepr executable on a remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteExecutable(String);

impl RemoteExecutable {
    pub fn parse(value: impl Into<String>) -> Result<Self, RemoteExecutableError> {
        let value = value.into();
        if !Path::new(&value).is_absolute() {
            return Err(RemoteExecutableError::NotAbsolute);
        }
        if value.len() > MAX_REMOTE_EXECUTABLE_BYTES {
            return Err(RemoteExecutableError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(RemoteExecutableError::ContainsControlCharacters);
        }
        // The command is nested inside /bin/sh -c and then parsed by the remote
        // account shell. Keep paths as plain shell words because nested quote
        // escaping is not reliable across the non-POSIX account shells we support.
        if !Self::is_shell_plain_word(&value) {
            return Err(RemoteExecutableError::NeedsShellQuoting);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
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
    fn remote_executable_accepts_shell_safe_absolute_paths() {
        for (path, valid) in [
            ("/usr/bin/shepr", true),
            ("/home/a b/shepr", false),
            ("$HOME/.local/bin/shepr", false),
            ("/home/user/.local/share/mise/shims/shepr", true),
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
    fn parse_keeps_the_shell_quoting_rejection_reason_typed() {
        assert_eq!(
            RemoteExecutable::parse("/home/a b/shepr"),
            Err(RemoteExecutableError::NeedsShellQuoting)
        );
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
