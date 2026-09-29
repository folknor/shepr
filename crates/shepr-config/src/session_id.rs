use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::limits::MAX_SESSION_NAME_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNameError(pub String);

impl std::fmt::Display for SessionNameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SessionNameError {}

pub const DEFAULT_SESSION_NAME: &str = "default";

/// A validated non-default session name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionName(String);

impl SessionName {
    pub fn parse(name: &str) -> Result<Self, SessionNameError> {
        if name == DEFAULT_SESSION_NAME {
            return Err(SessionNameError(
                "default is reserved for the default session".into(),
            ));
        }
        validate_session_name(name)?;
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The session identity selected for this process. The default session has no
/// directory component; named sessions live below `sessions/<name>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionId {
    #[default]
    Default,
    Named(SessionName),
}

impl SessionId {
    /// Parse a session name supplied by the user. `default` is the spelling
    /// for the default session and is not a valid `SessionName`.
    pub fn parse(name: &str) -> Result<Self, SessionNameError> {
        if name == DEFAULT_SESSION_NAME {
            Ok(Self::Default)
        } else {
            SessionName::parse(name).map(Self::Named)
        }
    }

    /// Resolve the command-line selection or inherited session value. The
    /// flag reports whether the session was requested explicitly. A malformed
    /// inherited value is an error even when a socket override is set.
    pub fn resolve(
        requested: Option<Self>,
        inherited: Option<&str>,
    ) -> Result<(Self, bool), SessionNameError> {
        if let Some(requested) = requested {
            return Ok((requested, true));
        }
        match inherited {
            Some(inherited) => Self::parse(inherited).map(|session| (session, false)),
            None => Ok((Self::Default, false)),
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Default => None,
            Self::Named(name) => Some(name.as_str()),
        }
    }

    pub fn display_name(&self) -> &str {
        self.name().unwrap_or(DEFAULT_SESSION_NAME)
    }

    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }

    pub fn data_dir(&self, paths: &super::AppPaths) -> PathBuf {
        self.data_dir_under(paths.state_dir())
    }

    pub(crate) fn data_dir_under(&self, state_dir: &Path) -> PathBuf {
        match self {
            Self::Default => state_dir.to_path_buf(),
            Self::Named(name) => state_dir.join("sessions").join(name.as_str()),
        }
    }

    pub fn api_socket_path(&self, paths: &super::AppPaths) -> PathBuf {
        self.api_socket_path_under(paths.runtime_dir())
    }

    pub fn api_socket_path_under(&self, runtime_dir: &Path) -> PathBuf {
        self.data_dir_under(runtime_dir).join("shepr.sock")
    }

    pub fn client_socket_path(&self, paths: &super::AppPaths) -> PathBuf {
        self.client_socket_path_under(paths.runtime_dir())
    }

    pub fn client_socket_path_under(&self, runtime_dir: &Path) -> PathBuf {
        super::address::derive_client_socket_from_api_socket(
            &self.api_socket_path_under(runtime_dir),
        )
    }

    pub fn attach_command(&self) -> String {
        match self {
            Self::Default => "shepr".to_string(),
            Self::Named(name) => format!("shepr session attach {}", shell_safe_name(name.as_str())),
        }
    }

    pub fn stop_command(&self) -> String {
        match self {
            Self::Default => "shepr server stop".to_string(),
            Self::Named(name) => format!("shepr session stop {}", shell_safe_name(name.as_str())),
        }
    }

    pub fn apply_to_child_command(&self, command: &mut std::process::Command) {
        match self {
            Self::Default => {
                command.env_remove(shepr_core::env::EnvVar::SheprSession);
            }
            Self::Named(name) => {
                command.env(shepr_core::env::EnvVar::SheprSession, name.as_str());
            }
        }
    }
}

/// A session name is a clap positional argument (`src/cli/spec.rs`'s
/// `required("name", "NAME")`), so a name starting with `-` would otherwise
/// be parsed as a flag when the printed command is run in a shell. Prefix it
/// with `--` to force positional parsing.
fn shell_safe_name(name: &str) -> String {
    if name.starts_with('-') {
        format!("-- {name}")
    } else {
        name.to_string()
    }
}

pub fn validate_session_name(name: &str) -> Result<(), SessionNameError> {
    if name.is_empty() {
        return Err(SessionNameError("session name cannot be empty".into()));
    }
    if name.len() > MAX_SESSION_NAME_LEN {
        return Err(SessionNameError(format!(
            "session name cannot be longer than {MAX_SESSION_NAME_LEN} bytes"
        )));
    }
    if name == "." || name == ".." {
        return Err(SessionNameError("session name cannot be . or ..".into()));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(SessionNameError(
            "session name may only contain ASCII letters, numbers, '.', '_' and '-'".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_and_attach_commands_escape_leading_hyphen_names() {
        let session = SessionId::parse("-foo").expect("- is a valid leading byte");
        assert_eq!(session.stop_command(), "shepr session stop -- -foo");
        assert_eq!(session.attach_command(), "shepr session attach -- -foo");
    }

    #[test]
    fn stop_and_attach_commands_leave_ordinary_names_unquoted() {
        let session = SessionId::parse("feature").expect("valid name");
        assert_eq!(session.stop_command(), "shepr session stop feature");
        assert_eq!(session.attach_command(), "shepr session attach feature");
    }
}
