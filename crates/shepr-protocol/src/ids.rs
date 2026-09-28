use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";

/// Encodes a public number in bijective base 32 (digits 1..=32, no zero
/// digit), so `"0"` is the digit for 32, not zero. Public numbers start at 1;
/// zero has no digits and encodes to the empty string, which is what
/// `decode_public_number` maps back to zero.
pub fn encode_public_number(mut value: usize) -> String {
    let mut encoded = Vec::new();
    while value > 0 {
        let digit = (value - 1) % PUBLIC_ID_ALPHABET.len();
        encoded.push(PUBLIC_ID_ALPHABET[digit] as char);
        value = (value - 1) / PUBLIC_ID_ALPHABET.len();
    }
    encoded.iter().rev().collect()
}

pub fn decode_public_number(value: &str) -> Option<usize> {
    let mut decoded = 0usize;
    for ch in value.chars() {
        let digit = PUBLIC_ID_ALPHABET
            .iter()
            .position(|candidate| *candidate as char == ch)?;
        decoded = decoded
            .checked_mul(PUBLIC_ID_ALPHABET.len())?
            .checked_add(digit + 1)?;
    }
    Some(decoded)
}

/// Stable public workspace identity. Its spelling is only needed at process
/// and API boundaries; a launch must not mix it with tab or pane identities.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for WorkspaceId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for WorkspaceId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<WorkspaceId> for String {
    fn from(id: WorkspaceId) -> Self {
        id.0
    }
}

impl From<&WorkspaceId> for String {
    fn from(id: &WorkspaceId) -> Self {
        id.0.clone()
    }
}

impl std::ops::Deref for WorkspaceId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl PartialEq<String> for WorkspaceId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<WorkspaceId> for String {
    fn eq(&self, other: &WorkspaceId) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<str> for WorkspaceId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for WorkspaceId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicTabId {
    workspace_id: WorkspaceId,
    number: usize,
    encoded: String,
}

impl PublicTabId {
    /// Builds a tab ID from a non-empty workspace ID and a one-based number.
    ///
    /// # Panics
    ///
    /// Panics if `workspace_id` is empty or `number` is zero, because neither
    /// value can be represented by a canonical public tab ID.
    pub fn new(workspace_id: impl Into<String>, number: usize) -> Self {
        let workspace_id = WorkspaceId::new(workspace_id);
        assert!(
            !workspace_id.as_str().is_empty(),
            "public tab IDs require a non-empty workspace ID"
        );
        assert!(number > 0, "public tab IDs use one-based numbers");
        Self {
            encoded: format!("{}:t{}", workspace_id, encode_public_number(number)),
            workspace_id,
            number,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub fn workspace_id(&self) -> &str {
        self.workspace_id.as_str()
    }

    pub fn number(&self) -> usize {
        self.number
    }
}

impl fmt::Display for PublicTabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PublicTabId {
    type Err = PublicIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (workspace_id, number) = parse_public_child_id(value, 't')?;
        Ok(Self::new(workspace_id, number))
    }
}

impl serde::Serialize for PublicTabId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for PublicTabId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = <String as serde::Deserialize>::deserialize(deserializer)?;
        id.parse()
            .map_err(|_| serde::de::Error::custom("invalid public tab id"))
    }
}

impl std::ops::Deref for PublicTabId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

/// This crate's own tests spell ids as literals; the text must be canonical.
/// Other crates have no conversion: they build ids with [`PublicTabId::new`]
/// or parse them, so no id exists that a server would not issue.
#[cfg(test)]
impl From<&str> for PublicTabId {
    fn from(value: &str) -> Self {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{value:?} is not a canonical public tab id"))
    }
}

#[cfg(test)]
impl From<String> for PublicTabId {
    fn from(value: String) -> Self {
        value.as_str().into()
    }
}

impl PartialEq<str> for PublicTabId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for PublicTabId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for PublicTabId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<PublicTabId> for String {
    fn eq(&self, other: &PublicTabId) -> bool {
        self == other.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicPaneId {
    workspace_id: WorkspaceId,
    number: usize,
    encoded: String,
}

impl PublicPaneId {
    /// Builds a pane ID from a non-empty workspace ID and a one-based number.
    ///
    /// # Panics
    ///
    /// Panics if `workspace_id` is empty or `number` is zero, because neither
    /// value can be represented by a canonical public pane ID.
    pub fn new(workspace_id: impl Into<String>, number: usize) -> Self {
        let workspace_id = WorkspaceId::new(workspace_id);
        assert!(
            !workspace_id.as_str().is_empty(),
            "public pane IDs require a non-empty workspace ID"
        );
        assert!(number > 0, "public pane IDs use one-based numbers");
        Self {
            encoded: format!("{}:p{}", workspace_id, encode_public_number(number)),
            workspace_id,
            number,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub fn workspace_id(&self) -> &str {
        self.workspace_id.as_str()
    }

    pub fn number(&self) -> usize {
        self.number
    }
}

impl fmt::Display for PublicPaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PublicPaneId {
    type Err = PublicIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (workspace_id, number) = parse_public_child_id(value, 'p')?;
        Ok(Self::new(workspace_id, number))
    }
}

impl serde::Serialize for PublicPaneId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for PublicPaneId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = <String as serde::Deserialize>::deserialize(deserializer)?;
        id.parse()
            .map_err(|_| serde::de::Error::custom("invalid public pane id"))
    }
}

impl std::ops::Deref for PublicPaneId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

/// This crate's own tests spell ids as literals; the text must be canonical.
/// Other crates have no conversion: they build ids with [`PublicPaneId::new`]
/// or parse them, so no id exists that a server would not issue.
#[cfg(test)]
impl From<&str> for PublicPaneId {
    fn from(value: &str) -> Self {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{value:?} is not a canonical public pane id"))
    }
}

#[cfg(test)]
impl From<String> for PublicPaneId {
    fn from(value: String) -> Self {
        value.as_str().into()
    }
}

impl PartialEq<str> for PublicPaneId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for PublicPaneId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for PublicPaneId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<PublicPaneId> for String {
    fn eq(&self, other: &PublicPaneId) -> bool {
        self == other.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicIdParseError;

fn parse_public_number(encoded: &str) -> Result<usize, PublicIdParseError> {
    if encoded.is_empty() {
        return Err(PublicIdParseError);
    }
    decode_public_number(encoded)
        .filter(|number| *number > 0)
        .ok_or(PublicIdParseError)
}

fn parse_public_child_id(value: &str, kind: char) -> Result<(&str, usize), PublicIdParseError> {
    let (workspace_id, encoded_id) = value.rsplit_once(':').ok_or(PublicIdParseError)?;
    let encoded_number = encoded_id.strip_prefix(kind).ok_or(PublicIdParseError)?;
    if workspace_id.is_empty() {
        return Err(PublicIdParseError);
    }
    Ok((workspace_id, parse_public_number(encoded_number)?))
}

/// Opaque identity for a server-owned terminal.
///
/// During the pane-backed transition this is stored one-to-one beside panes,
/// but callers must not derive it from a pane id or layout position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalId(String);

static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

impl TerminalId {
    pub fn alloc() -> Self {
        let since_epoch = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => Ok(duration),
            Err(error) => Err(error.duration()),
        };
        // One allocation per terminal never exhausts a u64; wrapping would
        // repeat an earlier id, so exhaustion is refused rather than wrapped.
        let counter =
            match NEXT_TERMINAL_ID.try_update(Ordering::Relaxed, Ordering::Relaxed, |counter| {
                counter.checked_add(1)
            }) {
                Ok(counter) => counter,
                Err(_) => panic!("terminal id allocation counter exhausted"),
            };
        Self::from_clock_and_counter(since_epoch, counter)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Builds fixed IDs for tests in dependent crates. Those crates build this
    /// library as a normal dependency, where its own `cfg(test)` does not apply.
    pub fn test_new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    fn from_clock_and_counter(since_epoch: Result<Duration, Duration>, counter: u64) -> Self {
        let micros = match since_epoch {
            Ok(duration) => duration.as_micros().to_string(),
            Err(duration) => {
                let micros = duration.as_micros();
                format!("before_{micros}")
            }
        };
        Self(format!("term_{micros}_{counter:x}"))
    }
}

// The client carries its owned CLI attach target as a TerminalId, and protocol
// and server tests also construct fixed wire IDs. Removing this conversion
// needs a parsing boundary and test updates outside the protocol crate. The
// rule against pane-derived IDs therefore remains a caller convention.
impl From<String> for TerminalId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod terminal_id_tests {
    use std::time::Duration;

    use super::TerminalId;

    #[test]
    fn terminal_id_clock_encoding_distinguishes_before_epoch_from_epoch() {
        let before_epoch = TerminalId::from_clock_and_counter(Err(Duration::from_micros(7)), 1);
        let at_epoch = TerminalId::from_clock_and_counter(Ok(Duration::ZERO), 2);

        assert_eq!(before_epoch.as_str(), "term_before_7_1");
        assert_eq!(at_epoch.as_str(), "term_0_2");
        assert_ne!(before_epoch, at_epoch);
    }

    #[test]
    fn terminal_id_clock_and_counter_fields_have_unambiguous_boundaries() {
        let first = TerminalId::from_clock_and_counter(Ok(Duration::from_micros(1)), 0x11);
        let second = TerminalId::from_clock_and_counter(Ok(Duration::from_micros(0x11)), 1);

        assert_eq!(first.as_str(), "term_1_11");
        assert_eq!(second.as_str(), "term_17_1");
        assert_ne!(first, second);
    }
}
