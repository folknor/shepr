use super::TerminalState;
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

    /// The name a workspace in `cwd` gets when it has no other: its
    /// directory's name (`default_workspace_name`), or the whole path when that
    /// name is blank once trimmed (a directory named only with spaces). An
    /// absolute path always has a nonblank form, so `/` is only a backstop.
    pub fn for_directory(cwd: &std::path::Path) -> Self {
        Self::new(shepr_core::workspace_label::default_workspace_name(cwd))
            .or_else(|| Self::new(cwd.display().to_string()))
            .unwrap_or_else(|| Self("/".to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
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

impl TerminalState {
    pub fn set_manual_label(&mut self, label: String) {
        self.manual_label = Label::new(label);
    }

    pub fn clear_manual_label(&mut self) {
        self.manual_label = None;
    }

    pub fn is_agent_terminal(&self) -> bool {
        self.ownership.has_agent()
    }
}
