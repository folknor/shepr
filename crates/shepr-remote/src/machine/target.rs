use std::fmt;
use std::ops::Deref;

const MAX_SSH_TARGET_BYTES: usize = 1024;

/// A checked SSH destination shared by saved-machine state and SSH transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget(String);

pub trait IntoSshTarget {
    fn into_ssh_target(self) -> Result<SshTarget, String>;
}

impl IntoSshTarget for SshTarget {
    fn into_ssh_target(self) -> Result<SshTarget, String> {
        Ok(self)
    }
}

impl IntoSshTarget for String {
    fn into_ssh_target(self) -> Result<SshTarget, String> {
        SshTarget::parse(self)
    }
}

impl IntoSshTarget for &str {
    fn into_ssh_target(self) -> Result<SshTarget, String> {
        SshTarget::parse(self)
    }
}

impl IntoSshTarget for &String {
    fn into_ssh_target(self) -> Result<SshTarget, String> {
        SshTarget::parse(self.clone())
    }
}

impl SshTarget {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() {
            return Err("SSH target must not be empty".into());
        }
        if value.starts_with('-') {
            return Err("SSH target must not start with '-'".into());
        }
        if value.chars().any(char::is_control) {
            return Err("SSH target must not contain control characters".into());
        }
        if value.len() > MAX_SSH_TARGET_BYTES {
            return Err(format!(
                "SSH target must be at most {MAX_SSH_TARGET_BYTES} bytes"
            ));
        }
        let authority = value.strip_prefix("ssh://").unwrap_or(&value);
        if authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
        {
            return Err("SSH target must not contain a password".into());
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
