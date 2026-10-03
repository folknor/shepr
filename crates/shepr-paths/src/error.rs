/// A path-resolution failure: every problem found while resolving the
/// process's directories, socket target and pane markers, each as one line of
/// operator text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsError {
    messages: Vec<String>,
}

impl PathsError {
    pub(crate) fn new(messages: Vec<String>) -> Self {
        Self { messages }
    }

    pub(crate) fn one(message: impl Into<String>) -> Self {
        Self::new(vec![message.into()])
    }

    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

impl std::fmt::Display for PathsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.messages.join("; "))
    }
}

impl std::error::Error for PathsError {}
