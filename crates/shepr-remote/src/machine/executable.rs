const MAX_REMOTE_EXECUTABLE_BYTES: usize = 4096;
pub const REMOTE_EXECUTABLE_ROOT: &str = "/";
pub const REMOTE_MISE_SHIM_SUFFIX: &str = "/mise/shims/shepr";

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
        if !value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        }) {
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
}
