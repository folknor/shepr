//! Where credentials come from: a provider and a discovered directory, and
//! the provider rules that turn environment evidence into that directory.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The agent whose subscription limits are tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// The environment variable that overrides this agent's config directory.
    pub fn override_variable(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }

    /// The config directory under a home directory when no override applies.
    fn default_directory_name(self) -> &'static str {
        match self {
            Self::Claude => ".claude",
            Self::Codex => ".codex",
        }
    }

    /// The credentials file inside the config directory.
    pub(crate) fn credentials_file(self) -> &'static str {
        match self {
            Self::Claude => ".credentials.json",
            Self::Codex => "auth.json",
        }
    }

    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// How a source was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceOrigin {
    /// From the server's own environment.
    Server,
    /// From a running agent's environment.
    Agent,
    /// Seen before and remembered on this host.
    Remembered,
}

/// One discovered credentials location. The path is kept as discovered,
/// never canonicalized, so a symlink retarget is observed as such.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceLocator {
    pub provider: Provider,
    pub directory: PathBuf,
}

impl SourceLocator {
    pub(crate) fn credentials_path(&self) -> PathBuf {
        self.directory.join(self.provider.credentials_file())
    }
}

/// A source with how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSource {
    pub locator: SourceLocator,
    pub origin: SourceOrigin,
}

/// Why environment evidence names no directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unresolved {
    /// The override is a relative path; what it is relative to is unknown.
    RelativeOverride,
    /// The override is set and empty, and this provider does not read that as
    /// unset.
    EmptyOverride,
    /// No override applies and HOME is missing, empty or relative.
    NoHome,
}

/// The config directory `provider` uses given its override variable and HOME
/// as raw environment values (`None` when unset).
///
/// - An absolute override is the directory.
/// - An empty `CODEX_HOME` counts as unset; an empty `CLAUDE_CONFIG_DIR` is
///   unresolved, as nothing shows Claude Code treats it as unset.
/// - A relative override is unresolved, never resolved against a guess.
/// - Otherwise the default directory under an absolute HOME.
pub fn resolve_config_directory(
    provider: Provider,
    override_value: Option<&[u8]>,
    home: Option<&[u8]>,
) -> Result<PathBuf, Unresolved> {
    match override_value {
        Some([]) if provider == Provider::Codex => {}
        Some([]) => return Err(Unresolved::EmptyOverride),
        Some(value) => {
            let path = Path::new(OsStr::from_bytes(value));
            return if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                Err(Unresolved::RelativeOverride)
            };
        }
        None => {}
    }
    let home = home.ok_or(Unresolved::NoHome)?;
    let home = Path::new(OsStr::from_bytes(home));
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return Err(Unresolved::NoHome);
    }
    Ok(home.join(provider.default_directory_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_override_wins_and_defaults_use_home() {
        assert_eq!(
            resolve_config_directory(Provider::Claude, Some(b"/srv/claude-b"), Some(b"/home/u")),
            Ok(PathBuf::from("/srv/claude-b"))
        );
        assert_eq!(
            resolve_config_directory(Provider::Codex, None, Some(b"/home/u")),
            Ok(PathBuf::from("/home/u/.codex"))
        );
        assert_eq!(
            resolve_config_directory(Provider::Claude, None, Some(b"/home/u")),
            Ok(PathBuf::from("/home/u/.claude"))
        );
    }

    #[test]
    fn empty_overrides_follow_each_provider() {
        assert_eq!(
            resolve_config_directory(Provider::Codex, Some(b""), Some(b"/home/u")),
            Ok(PathBuf::from("/home/u/.codex"))
        );
        assert_eq!(
            resolve_config_directory(Provider::Claude, Some(b""), Some(b"/home/u")),
            Err(Unresolved::EmptyOverride)
        );
    }

    #[test]
    fn relative_overrides_and_missing_homes_are_never_guessed() {
        assert_eq!(
            resolve_config_directory(Provider::Codex, Some(b"codex"), Some(b"/home/u")),
            Err(Unresolved::RelativeOverride)
        );
        for home in [None, Some(&b""[..]), Some(&b"relative"[..])] {
            assert_eq!(
                resolve_config_directory(Provider::Claude, None, home),
                Err(Unresolved::NoHome)
            );
        }
    }
}
