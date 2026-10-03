use std::fmt;

/// Text that has been made safe to render in a local terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteText(String);

impl RemoteText {
    /// Accepts text from a remote command or another untrusted diagnostic source.
    /// Newlines and tabs stay readable, carriage returns are dropped, and other
    /// control characters become `?` so they cannot start terminal sequences.
    pub fn from_untrusted(value: &str) -> Self {
        Self(
            value
                .chars()
                .filter(|ch| *ch != '\r')
                .map(|ch| {
                    if ch.is_control() && ch != '\n' && ch != '\t' {
                        '?'
                    } else {
                        ch
                    }
                })
                .collect(),
        )
    }

    /// Characters already sanitized for terminal output.
    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        self.0.chars()
    }
}

impl fmt::Display for RemoteText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
