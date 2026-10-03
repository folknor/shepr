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
