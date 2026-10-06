use serde::{Deserialize, Serialize};

/// A nonempty user-facing label with surrounding whitespace removed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Label(String);

impl Label {
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let value = value.as_ref().trim();
        (!value.is_empty()).then(|| Self(value.to_owned()))
    }

    /// The name a workspace in `cwd` gets when it has no other, using the
    /// core default label policy for both a blank directory name and its
    /// fallback path.
    pub fn for_directory(cwd: &std::path::Path) -> Self {
        Self::new(shepr_core::workspace_label::default_workspace_label(cwd))
            .expect("core default workspace labels are nonempty and trimmed")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || value.trim() != value {
            return Err(serde::de::Error::custom(
                "saved label must not be empty or padded with whitespace",
            ));
        }
        Ok(Self(value))
    }
}
