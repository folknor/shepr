use crate::machine::SshTarget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKeybindings {
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

    pub fn from_env() -> Result<Option<Self>, String> {
        let var = shepr_core::env::EnvVar::SheprRemoteKeybindings;
        match shepr_core::env::read_text(var) {
            Ok(Some(value)) => Self::parse(&value)
                .map(Some)
                .map_err(|_| format!("{var} must be 'local' or 'server', got {value:?}")),
            Ok(None) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    /// The value both the `--remote-keybindings` flag and the environment
    /// variable carry.
    pub fn to_env_value(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLaunch {
    pub target: SshTarget,
    pub keybindings: RemoteKeybindings,
}

/// Builds the remote launch from the parsed `--remote` and
/// `--remote-keybindings` options. The command-line parser (`cli/spec.rs`)
/// only accepts them before the subcommand, rejects repeats, and requires
/// `--remote` for `--remote-keybindings`; the values are validated here.
pub fn remote_launch(
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
        target: SshTarget::parse(target.to_owned()).map_err(|error| error.to_string())?,
        keybindings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::env::EnvVar;
    use shepr_test_support::IsolatedEnv;

    #[test]
    fn remote_keybindings_environment_round_trips() {
        let env = IsolatedEnv::new();
        env.remove(EnvVar::SheprRemoteKeybindings);
        assert_eq!(RemoteKeybindings::from_env().expect("unset env"), None);

        for keybindings in [RemoteKeybindings::Local, RemoteKeybindings::Server] {
            env.set(EnvVar::SheprRemoteKeybindings, keybindings.to_env_value());
            assert_eq!(
                RemoteKeybindings::from_env().expect("valid env"),
                Some(keybindings)
            );
        }
    }

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
    fn remote_launch_rejects_invalid_options_and_targets() {
        assert!(remote_launch(Some("-oProxyCommand=x"), None).is_err());
        assert!(remote_launch(Some(""), None).is_err());
        assert!(remote_launch(Some("dev@box"), Some("both")).is_err());
    }
}
