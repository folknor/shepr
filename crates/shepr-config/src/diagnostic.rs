use std::path::{Path, PathBuf};

/// One segment in a key path within a TOML configuration document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigKeyPathSegment {
    Key(String),
    Index(usize),
}

/// A structured TOML key path, such as `ui.sidebar_width` or
/// `machines[1].label`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigKeyPath(Vec<ConfigKeyPathSegment>);

impl ConfigKeyPath {
    pub fn root() -> Self {
        Self::default()
    }

    pub(crate) fn from_dotted(path: &str) -> Self {
        path.split('.').fold(Self::root(), Self::key)
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.0.push(ConfigKeyPathSegment::Key(key.into()));
        self
    }

    pub fn index(mut self, index: usize) -> Self {
        self.0.push(ConfigKeyPathSegment::Index(index));
        self
    }

    pub fn segments(&self) -> &[ConfigKeyPathSegment] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn section_header(&self, array_table: bool) -> String {
        let path = self.to_string();
        if array_table {
            format!("[[{path}]]")
        } else {
            format!("[{path}]")
        }
    }
}

impl std::fmt::Display for ConfigKeyPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for segment in &self.0 {
            match segment {
                ConfigKeyPathSegment::Key(key) => {
                    if !first {
                        f.write_str(".")?;
                    }
                    if !key.is_empty()
                        && key
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    {
                        f.write_str(key)?;
                    } else {
                        write!(f, "{}", toml::Value::String(key.clone()))?;
                    }
                    first = false;
                }
                ConfigKeyPathSegment::Index(index) => {
                    write!(f, "[{index}]")?;
                    first = false;
                }
            }
        }
        Ok(())
    }
}

/// The category and detail of one config problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDiagnosticKind {
    Read(String),
    Parse(String),
    /// A key this file's program does not read. `belongs_in` is the other
    /// program's config file when that program reads the key.
    UnknownKey {
        belongs_in: Option<PathBuf>,
    },
    UnknownSection {
        array_table: bool,
        belongs_in: Option<PathBuf>,
    },
    Validation(String),
    Path(String),
    /// An internal inconsistency while resolving validated config values.
    Internal(String),
}

/// A config problem with its source file and setting path kept as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    file: Option<PathBuf>,
    key: Option<ConfigKeyPath>,
    related_keys: Vec<ConfigKeyPath>,
    kind: ConfigDiagnosticKind,
}

impl ConfigDiagnostic {
    pub fn read(reason: impl Into<String>) -> Self {
        Self::new(ConfigDiagnosticKind::Read(reason.into()), None, None)
    }

    pub fn parse(reason: impl Into<String>) -> Self {
        Self::new(ConfigDiagnosticKind::Parse(reason.into()), None, None)
    }

    pub fn unknown_key(key: ConfigKeyPath) -> Self {
        Self::new(
            ConfigDiagnosticKind::UnknownKey { belongs_in: None },
            None,
            Some(key),
        )
    }

    pub fn unknown_section(key: ConfigKeyPath, array_table: bool) -> Self {
        Self::new(
            ConfigDiagnosticKind::UnknownSection {
                array_table,
                belongs_in: None,
            },
            None,
            Some(key),
        )
    }

    /// Names `file` as where an unknown key or section belongs; any other
    /// diagnostic is returned unchanged.
    pub(crate) fn belonging_in(mut self, file: &Path) -> Self {
        if let ConfigDiagnosticKind::UnknownKey { belongs_in }
        | ConfigDiagnosticKind::UnknownSection { belongs_in, .. } = &mut self.kind
        {
            *belongs_in = Some(file.to_path_buf());
        }
        self
    }

    pub fn validation(key: ConfigKeyPath, reason: impl Into<String>) -> Self {
        Self::new(
            ConfigDiagnosticKind::Validation(reason.into()),
            None,
            Some(key),
        )
    }

    pub fn path_at(key: ConfigKeyPath, reason: impl Into<String>) -> Self {
        Self::new(ConfigDiagnosticKind::Path(reason.into()), None, Some(key))
    }

    pub(crate) fn internal(reason: impl Into<String>) -> Self {
        Self::new(ConfigDiagnosticKind::Internal(reason.into()), None, None)
    }

    pub(crate) fn validation_related(
        key: ConfigKeyPath,
        related_keys: Vec<ConfigKeyPath>,
        reason: impl Into<String>,
    ) -> Self {
        let mut diagnostic = Self::validation(key, reason);
        diagnostic.related_keys = related_keys;
        diagnostic
    }

    fn new(kind: ConfigDiagnosticKind, file: Option<PathBuf>, key: Option<ConfigKeyPath>) -> Self {
        Self {
            file,
            key,
            related_keys: Vec::new(),
            kind,
        }
    }

    pub(crate) fn with_file(mut self, path: &Path) -> Self {
        self.file = Some(path.to_path_buf());
        self
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn key(&self) -> Option<&ConfigKeyPath> {
        self.key.as_ref()
    }

    pub fn related_keys(&self) -> &[ConfigKeyPath] {
        &self.related_keys
    }

    pub fn kind(&self) -> &ConfigDiagnosticKind {
        &self.kind
    }

    /// The detail without its category, file, or key path.
    pub fn message(&self) -> &str {
        match &self.kind {
            ConfigDiagnosticKind::Read(message)
            | ConfigDiagnosticKind::Parse(message)
            | ConfigDiagnosticKind::Validation(message)
            | ConfigDiagnosticKind::Path(message)
            | ConfigDiagnosticKind::Internal(message) => message,
            ConfigDiagnosticKind::UnknownKey { .. } => "unknown key",
            ConfigDiagnosticKind::UnknownSection { .. } => "unknown section",
        }
    }

    fn write_source(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{}: ", file.display())?;
        }
        if let Some(key) = &self.key {
            write!(f, "{key}: ")?;
        }
        Ok(())
    }

    /// The file an unknown key or section was found in, then the file it
    /// belongs in when the other program reads it.
    fn write_misplacement(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        belongs_in: Option<&Path>,
    ) -> std::fmt::Result {
        if let Some(file) = &self.file {
            write!(f, " in {}", file.display())?;
        }
        if let Some(belongs_in) = belongs_in {
            write!(f, "; it belongs in {}", belongs_in.display())?;
        }
        Ok(())
    }
}

impl std::fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ConfigDiagnosticKind::Read(message) => {
                f.write_str("config read error: ")?;
                if let Some(file) = &self.file {
                    write!(f, "{}: ", file.display())?;
                }
                f.write_str(message)
            }
            ConfigDiagnosticKind::Parse(message) => {
                f.write_str("config parse error: ")?;
                if let Some(file) = &self.file {
                    write!(f, "{}: ", file.display())?;
                }
                f.write_str(message)
            }
            ConfigDiagnosticKind::Internal(message) => {
                write!(f, "internal config resolution error: {message}")
            }
            ConfigDiagnosticKind::UnknownKey { belongs_in } => {
                f.write_str("unknown config key ")?;
                if let Some(key) = &self.key {
                    write!(f, "{key}")?;
                }
                self.write_misplacement(f, belongs_in.as_deref())
            }
            ConfigDiagnosticKind::UnknownSection {
                array_table,
                belongs_in,
            } => {
                f.write_str("unknown config section ")?;
                if let Some(key) = &self.key {
                    f.write_str(&key.section_header(*array_table))?;
                }
                self.write_misplacement(f, belongs_in.as_deref())
            }
            ConfigDiagnosticKind::Validation(message) | ConfigDiagnosticKind::Path(message) => {
                self.write_source(f)?;
                f.write_str(message)?;
                if !self.related_keys.is_empty() {
                    f.write_str(" (related: ")?;
                    for (index, key) in self.related_keys.iter().enumerate() {
                        if index > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{key}")?;
                    }
                    f.write_str(")")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigDiagnostic, ConfigKeyPath};
    use std::path::Path;

    #[test]
    fn typed_diagnostic_display_keeps_file_key_and_reason() {
        let diagnostic = ConfigDiagnostic::validation(
            ConfigKeyPath::root().key("ui").key("sidebar_width"),
            "must be between the configured minimum and maximum",
        )
        .with_file(Path::new("/config/client.toml"));

        assert_eq!(
            diagnostic.to_string(),
            "/config/client.toml: ui.sidebar_width: must be between the configured minimum and maximum"
        );
        assert_eq!(
            diagnostic.key().expect("diagnostic has a key").to_string(),
            "ui.sidebar_width"
        );
        assert_eq!(
            diagnostic.message(),
            "must be between the configured minimum and maximum"
        );
    }

    #[test]
    fn parse_error_file_stays_on_the_first_line() {
        let diagnostic = ConfigDiagnostic::parse("expected key\n --> 2:1\n  |\n2 | [broken\n  | ^")
            .with_file(Path::new("/config/client.toml"));

        assert_eq!(
            diagnostic.to_string(),
            "config parse error: /config/client.toml: expected key\n --> 2:1\n  |\n2 | [broken\n  | ^"
        );
    }

    #[test]
    fn unknown_sections_keep_table_form() {
        let section =
            ConfigDiagnostic::unknown_section(ConfigKeyPath::root().key("retired"), false);
        let array = ConfigDiagnostic::unknown_section(ConfigKeyPath::root().key("machines"), true);

        assert_eq!(section.to_string(), "unknown config section [retired]");
        assert_eq!(array.to_string(), "unknown config section [[machines]]");
    }
}
