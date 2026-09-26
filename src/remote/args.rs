use std::fmt;
use std::ops::Deref;

pub(crate) const REATTACH_COMMAND_ENV_VAR: &str = "SHEPR_REATTACH_COMMAND";
pub(crate) const REMOTE_KEYBINDINGS_ENV_VAR: &str = "SHEPR_REMOTE_KEYBINDINGS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKeybindings {
    Local,
    Server,
}

impl RemoteKeybindings {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            _ => Err("--remote-keybindings must be 'local' or 'server'".to_string()),
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteLaunch {
    pub(crate) target: SshTarget,
    pub(crate) keybindings: RemoteKeybindings,
}

const MAX_SSH_TARGET_BYTES: usize = 1024;
const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;
pub(crate) const REMOTE_EXECUTABLE_ROOT: &str = "/";
pub(crate) const REMOTE_MISE_SHIM_SUFFIX: &str = "/mise/shims/shepr";

/// A validated absolute path for a candidate Shepr executable on a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteExecutable(String);

impl RemoteExecutable {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, String> {
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
        if value.ends_with(REMOTE_MISE_SHIM_SUFFIX) {
            return Err("remote Shepr executable path must not be a mise shim".into());
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A checked SSH destination. Every place that launches ssh takes this type so the
/// argument-safety and saved-profile restrictions have one owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshTarget(String);

pub(crate) trait IntoSshTarget {
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
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, String> {
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

    pub(crate) fn as_str(&self) -> &str {
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

/// Builds the remote launch from the parsed `--remote` and
/// `--remote-keybindings` options. The command-line parser (`cli/spec.rs`)
/// only accepts them before the subcommand, rejects repeats, and requires
/// `--remote` for `--remote-keybindings`; the values are validated here.
pub(crate) fn remote_launch(
    target: Option<&str>,
    keybindings: Option<&str>,
) -> Result<Option<RemoteLaunch>, String> {
    let keybindings = keybindings
        .map(RemoteKeybindings::parse)
        .transpose()?
        .unwrap_or(RemoteKeybindings::Local);
    let Some(target) = target else {
        return Ok(None);
    };
    Ok(Some(RemoteLaunch {
        target: SshTarget::parse(target.to_owned())?,
        keybindings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_launch_defaults_to_local_keybindings() {
        assert_eq!(remote_launch(None, None), Ok(None));
        assert_eq!(
            remote_launch(Some("dev@box"), None),
            Ok(Some(RemoteLaunch {
                target: SshTarget::parse("dev@box").expect("test precondition"),
                keybindings: RemoteKeybindings::Local,
            }))
        );
        assert_eq!(
            remote_launch(Some("dev@box"), Some("server"))
                .expect("test precondition")
                .map(|launch| launch.keybindings),
            Some(RemoteKeybindings::Server)
        );
    }

    #[test]
    fn ssh_target_rejects_unsafe_or_unsupported_values() {
        assert!(remote_launch(Some("-oProxyCommand=x"), None).is_err());
        assert!(remote_launch(Some(""), None).is_err());
        assert!(remote_launch(Some("dev@box"), Some("both")).is_err());
        for target in ["host\ncommand", "host\u{7f}", "ssh://user:password@host"] {
            assert!(SshTarget::parse(target).is_err(), "{target:?}");
        }
        let oversized = "x".repeat(MAX_SSH_TARGET_BYTES + 1);
        assert!(SshTarget::parse(oversized).is_err());
    }
}
