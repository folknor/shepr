use std::fmt;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SshTarget(String);

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

    /// Append this checked destination as one command-line argument.
    pub fn append_to(&self, command: &mut std::process::Command) {
        command.arg(self.as_str());
    }

    /// Render this destination as one POSIX shell word for operator guidance.
    pub fn shell_word(&self) -> String {
        shepr_core::shell_quote::quote(self.as_str())
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

/// The name of a server the client shows: a configured machine's identifier,
/// or the local server's label. A nonblank string without control characters
/// or leading and trailing whitespace, kept exactly as written. Inner spaces
/// and non-ASCII characters are allowed.
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

    /// Whether the two name the same server as the client compares names:
    /// equal apart from ASCII case, so a sidebar rule matched with
    /// `ignore_case` never confuses them.
    pub fn same_name(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl fmt::Display for MachineLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MachineLabel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// One `[[machines]]` entry: a label, the SSH target it reaches and the hue
/// it is drawn in.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MachineConfig {
    pub label: MachineLabel,
    pub ssh: SshTarget,
    /// The hue the client derives this machine's colours from. Required; an
    /// unknown name fails the parse.
    pub palette: shepr_term::host_tint::HostHue,
}

/// The hue of a local server that has no `[[machines]]` entry of its own.
pub const DEFAULT_LOCAL_HUE: shepr_term::host_tint::HostHue = shepr_term::host_tint::HostHue::Blue;

/// The `[local]` table: settings for the local server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    /// The name the client shows for the local server, under a machine
    /// label's rules. Unset, the client uses this host's short hostname. A
    /// `[[machines]]` entry of this name is this host's own: it is not
    /// connected to, and its `palette` is the local server's hue.
    pub label: Option<MachineLabel>,
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

    /// No name is reserved: the local server is shown by its own label, which
    /// the launch checks machine labels against.
    #[test]
    fn machine_label_reserves_no_name() {
        for label in ["Local", "local", "localhost"] {
            assert!(MachineLabel::parse(label).is_ok(), "{label:?}");
        }
    }

    #[test]
    fn labels_name_the_same_server_apart_from_ascii_case() {
        let label = |value| MachineLabel::parse(value).expect("test label");
        assert!(label("Build").same_name(&label("build")));
        assert!(!label("build").same_name(&label("build2")));
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
