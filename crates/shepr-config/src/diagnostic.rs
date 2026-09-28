/// A config load problem, classified before it reaches a CLI or launch edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDiagnostic {
    Read(String),
    Parse(String),
    Provenance(String),
    Unknown(String),
    Validation(String),
    Path(String),
}

impl ConfigDiagnostic {
    /// The diagnostic detail without the category prefix added by `Display`.
    pub fn message(&self) -> &str {
        match self {
            Self::Read(message)
            | Self::Parse(message)
            | Self::Provenance(message)
            | Self::Unknown(message)
            | Self::Validation(message)
            | Self::Path(message) => message,
        }
    }
}

impl std::fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = self.message();
        match self {
            Self::Read(_) => write!(f, "config read error: {message}"),
            Self::Parse(_) => write!(f, "config parse error: {message}"),
            Self::Provenance(_) => write!(f, "config provenance error: {message}"),
            // The detail starts with what is unknown: `key ui.x` or `section [x]`.
            Self::Unknown(_) => write!(f, "unknown config {message}"),
            // These name the setting or directory they concern and carry their
            // own context (`state directory error: ...`), so a category
            // prefix would only restate it.
            Self::Validation(_) | Self::Path(_) => f.write_str(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigDiagnostic;

    #[test]
    fn display_adds_the_category_where_the_detail_lacks_it() {
        let diagnostics = [
            (
                ConfigDiagnostic::Read("denied".into()),
                "config read error: denied",
            ),
            (
                ConfigDiagnostic::Parse("bad TOML".into()),
                "config parse error: bad TOML",
            ),
            (
                ConfigDiagnostic::Provenance("missing source".into()),
                "config provenance error: missing source",
            ),
            (
                ConfigDiagnostic::Unknown("key ui.typo".into()),
                "unknown config key ui.typo",
            ),
            (
                ConfigDiagnostic::Validation("theme.name is invalid".into()),
                "theme.name is invalid",
            ),
            (
                ConfigDiagnostic::Path("terminal.new_cwd is unavailable".into()),
                "terminal.new_cwd is unavailable",
            ),
            (
                ConfigDiagnostic::Path("state directory error: bad XDG value".into()),
                "state directory error: bad XDG value",
            ),
        ];

        for (diagnostic, expected) in diagnostics {
            assert_eq!(diagnostic.to_string(), expected);
        }
    }
}
