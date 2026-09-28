use std::{
    fmt,
    ops::Deref,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Identifies one server process lifetime in shell and surface messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct BootId(String);

impl BootId {
    /// Builds the boot identity for this server process.
    pub fn for_this_process() -> Self {
        let since_epoch = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => Ok(duration),
            Err(error) => Err(error.duration()),
        };
        Self::from_process_clock(std::process::id(), since_epoch)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_process_clock(process_id: u32, since_epoch: Result<Duration, Duration>) -> Self {
        let time = match since_epoch {
            Ok(duration) => duration.as_nanos().to_string(),
            Err(duration) => {
                let nanos = duration.as_nanos();
                format!("before-{nanos}")
            }
        };
        Self(format!("{process_id}-{time}"))
    }
}

impl From<&str> for BootId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl Deref for BootId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for BootId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for BootId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for BootId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for BootId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<BootId> for String {
    fn eq(&self, other: &BootId) -> bool {
        self == other.as_str()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::BootId;

    #[test]
    fn process_clock_before_epoch_keeps_its_offset() {
        let boot_id = BootId::from_process_clock(17, Err(Duration::from_nanos(23)));

        assert_eq!(boot_id.as_str(), "17-before-23");
    }

    #[test]
    fn process_clock_at_epoch_is_distinct_from_before_epoch() {
        let before_epoch = BootId::from_process_clock(17, Err(Duration::ZERO));
        let at_epoch = BootId::from_process_clock(17, Ok(Duration::ZERO));

        assert_eq!(before_epoch.as_str(), "17-before-0");
        assert_eq!(at_epoch.as_str(), "17-0");
        assert_ne!(before_epoch, at_epoch);
    }
}

/// Correlates one client shell operation with its endpoint response.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl Deref for RequestId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for RequestId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for RequestId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for RequestId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for RequestId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<RequestId> for String {
    fn eq(&self, other: &RequestId) -> bool {
        self == other.as_str()
    }
}
