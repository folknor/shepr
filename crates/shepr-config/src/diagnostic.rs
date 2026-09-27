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
        f.write_str(self.message())
    }
}
