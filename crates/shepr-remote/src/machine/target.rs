use std::fmt;
use std::ops::Deref;

use crate::limits::MAX_SSH_TARGET_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshTargetError {
    Empty,
    StartsWithDash,
    ControlCharacters,
    TooLong,
    PasswordInAuthority,
}

impl fmt::Display for SshTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("SSH target must not be empty"),
            Self::StartsWithDash => formatter.write_str("SSH target must not start with '-'"),
            Self::ControlCharacters => {
                formatter.write_str("SSH target must not contain control characters")
            }
            Self::TooLong => write!(
                formatter,
                "SSH target must be at most {MAX_SSH_TARGET_BYTES} bytes"
            ),
            Self::PasswordInAuthority => {
                formatter.write_str("SSH target must not contain a password")
            }
        }
    }
}

impl std::error::Error for SshTargetError {}

/// A checked SSH destination shared by saved-machine state and SSH transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget(String);

pub trait IntoSshTarget {
    fn into_ssh_target(self) -> Result<SshTarget, SshTargetError>;
}

impl IntoSshTarget for SshTarget {
    fn into_ssh_target(self) -> Result<SshTarget, SshTargetError> {
        Ok(self)
    }
}

impl IntoSshTarget for String {
    fn into_ssh_target(self) -> Result<SshTarget, SshTargetError> {
        SshTarget::parse(self)
    }
}

impl IntoSshTarget for &str {
    fn into_ssh_target(self) -> Result<SshTarget, SshTargetError> {
        SshTarget::parse(self)
    }
}

impl IntoSshTarget for &String {
    fn into_ssh_target(self) -> Result<SshTarget, SshTargetError> {
        SshTarget::parse(self.clone())
    }
}

impl SshTarget {
    pub fn parse(value: impl Into<String>) -> Result<Self, SshTargetError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SshTargetError::Empty);
        }
        if value.starts_with('-') {
            return Err(SshTargetError::StartsWithDash);
        }
        if value.chars().any(char::is_control) {
            return Err(SshTargetError::ControlCharacters);
        }
        if value.len() > MAX_SSH_TARGET_BYTES {
            return Err(SshTargetError::TooLong);
        }
        let authority = value.strip_prefix("ssh://").unwrap_or(&value);
        if authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
        {
            return Err(SshTargetError::PasswordInAuthority);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl serde::Serialize for SshTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for SshTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for SshTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Deref for SshTarget {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_target_rejects_unsafe_or_unsupported_values() {
        assert_eq!(SshTarget::parse(""), Err(SshTargetError::Empty));
        assert_eq!(
            SshTarget::parse("-oProxyCommand=x"),
            Err(SshTargetError::StartsWithDash)
        );
        for target in [
            "-oProxyCommand=x",
            "",
            "host\ncommand",
            "host\u{7f}",
            "ssh://user:password@host",
        ] {
            assert!(SshTarget::parse(target).is_err(), "{target:?}");
        }
        let oversized = "x".repeat(MAX_SSH_TARGET_BYTES + 1);
        assert!(SshTarget::parse(oversized).is_err());
    }
}
