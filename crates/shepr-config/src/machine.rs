use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

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

/// A checked SSH destination shared by the machine config and SSH transport.
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

impl Serialize for SshTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SshTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as Deserialize>::deserialize(deserializer)?;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineLabelError {
    Blank,
    ControlCharacters,
    SurroundingWhitespace,
}

impl fmt::Display for MachineLabelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => formatter.write_str("machine label must not be blank"),
            Self::ControlCharacters => {
                formatter.write_str("machine label must not contain control characters")
            }
            Self::SurroundingWhitespace => {
                formatter.write_str("machine label must not start or end with whitespace")
            }
        }
    }
}

impl std::error::Error for MachineLabelError {}

/// The identifier of a configured machine: a nonblank string without control
/// characters or leading and trailing whitespace, kept exactly as written.
/// Inner spaces and non-ASCII characters are allowed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MachineLabel(String);

impl MachineLabel {
    pub fn parse(value: impl Into<String>) -> Result<Self, MachineLabelError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(MachineLabelError::Blank);
        }
        if value.chars().any(char::is_control) {
            return Err(MachineLabelError::ControlCharacters);
        }
        if value.trim() != value {
            return Err(MachineLabelError::SurroundingWhitespace);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MachineLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for MachineLabel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MachineLabel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// One `[[machines]]` entry: a label and the SSH target it reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    pub label: MachineLabel,
    pub ssh: SshTarget,
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

    #[test]
    fn machine_label_rejects_blank_values() {
        for blank in ["", " ", "\t\n"] {
            assert_eq!(MachineLabel::parse(blank), Err(MachineLabelError::Blank));
        }
        let label = MachineLabel::parse("build").expect("a nonblank label");
        assert_eq!(label.as_str(), "build");
        assert_eq!(label.to_string(), "build");
    }

    #[test]
    fn machine_label_rejects_control_characters() {
        for label in [
            "a\nb",
            "a\tb",
            "a\u{0}b",
            "a\u{7f}b",
            "a\u{9b}b",
            "\u{1b}[31mred",
        ] {
            assert_eq!(
                MachineLabel::parse(label),
                Err(MachineLabelError::ControlCharacters),
                "{label:?}"
            );
        }
    }

    #[test]
    fn machine_label_rejects_leading_or_trailing_whitespace() {
        for label in [
            " build",
            "build ",
            " build ",
            "\u{a0}build",
            "build\u{2003}",
        ] {
            assert_eq!(
                MachineLabel::parse(label),
                Err(MachineLabelError::SurroundingWhitespace),
                "{label:?}"
            );
        }
    }

    #[test]
    fn machine_label_keeps_inner_spaces_and_non_ascii_letters() {
        let label = MachineLabel::parse("Bygg server \u{e6}\u{f8}\u{e5} \u{4e2d}\u{6587}")
            .expect("inner spaces and non-ASCII letters are valid");
        assert_eq!(
            label.as_str(),
            "Bygg server \u{e6}\u{f8}\u{e5} \u{4e2d}\u{6587}"
        );
    }

    #[test]
    fn machine_label_error_messages_name_the_rule() {
        assert_eq!(
            MachineLabelError::ControlCharacters.to_string(),
            "machine label must not contain control characters"
        );
        assert_eq!(
            MachineLabelError::SurroundingWhitespace.to_string(),
            "machine label must not start or end with whitespace"
        );
    }
}
