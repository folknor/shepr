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

/// Public identity for a child of a workspace: `<workspace>:<KIND><number>`.
/// Use the [`PublicTabId`] and [`PublicPaneId`] aliases; `KIND` is the
/// letter that tells the two apart in the canonical text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicChildId<const KIND: char> {
    workspace_id: WorkspaceId,
    number: usize,
    encoded: String,
}

/// Public identity for a tab in a workspace.
pub type PublicTabId = PublicChildId<'t'>;

/// Public identity for a pane in a workspace.
pub type PublicPaneId = PublicChildId<'p'>;

impl<const KIND: char> PublicChildId<KIND> {
    /// Builds a child ID from a non-empty workspace ID and a one-based number.
    ///
    /// # Panics
    ///
    /// Panics if `workspace_id` is empty or `number` is zero, because neither
    /// value can be represented by a canonical public child ID.
    pub fn new(workspace_id: impl Into<String>, number: usize) -> Self {
        let workspace_id = WorkspaceId::new(workspace_id);
        assert!(
            !workspace_id.as_str().is_empty(),
            "public child IDs require a non-empty workspace ID"
        );
        assert!(number > 0, "public child IDs use one-based numbers");
        Self {
            encoded: format!("{}:{}{}", workspace_id, KIND, encode_public_number(number)),
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

impl<const KIND: char> fmt::Display for PublicChildId<KIND> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const KIND: char> FromStr for PublicChildId<KIND> {
    type Err = PublicIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (workspace_id, number) =
            parse_public_child_id(value, KIND).ok_or(PublicIdParseError { kind: KIND })?;
        Ok(Self::new(workspace_id, number))
    }
}

impl<const KIND: char> serde::Serialize for PublicChildId<KIND> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de, const KIND: char> serde::Deserialize<'de> for PublicChildId<KIND> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = <String as serde::Deserialize>::deserialize(deserializer)?;
        id.parse()
            .map_err(|error: PublicIdParseError| serde::de::Error::custom(error))
    }
}

impl<const KIND: char> std::ops::Deref for PublicChildId<KIND> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

/// This crate's own tests spell IDs as literals; the text must be canonical.
/// Other crates build IDs with the public aliases' `new` methods or parse
/// them, so no ID exists that a server would not issue.
#[cfg(test)]
impl<const KIND: char> From<&str> for PublicChildId<KIND> {
    fn from(value: &str) -> Self {
        value
            .parse()
            .unwrap_or_else(|error| panic!("{value:?}: {error}"))
    }
}

#[cfg(test)]
impl<const KIND: char> From<String> for PublicChildId<KIND> {
    fn from(value: String) -> Self {
        value.as_str().into()
    }
}

impl<const KIND: char> PartialEq<str> for PublicChildId<KIND> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<const KIND: char> PartialEq<&str> for PublicChildId<KIND> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<const KIND: char> PartialEq<String> for PublicChildId<KIND> {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl<const KIND: char> PartialEq<PublicChildId<KIND>> for String {
    fn eq(&self, other: &PublicChildId<KIND>) -> bool {
        self == other.as_str()
    }
}

/// Text that is not a canonical public tab or pane ID of the requested kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicIdParseError {
    kind: char,
}

impl fmt::Display for PublicIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let noun = match self.kind {
            't' => "tab",
            'p' => "pane",
            _ => "child",
        };
        write!(f, "invalid public {noun} id")
    }
}

impl std::error::Error for PublicIdParseError {}

fn parse_public_number(encoded: &str) -> Option<usize> {
    if encoded.is_empty() {
        return None;
    }
    decode_public_number(encoded).filter(|number| *number > 0)
}

fn parse_public_child_id(value: &str, kind: char) -> Option<(&str, usize)> {
    let (workspace_id, encoded_id) = value.rsplit_once(':')?;
    let encoded_number = encoded_id.strip_prefix(kind)?;
    if workspace_id.is_empty() {
        return None;
    }
    Some((workspace_id, parse_public_number(encoded_number)?))
}

#[cfg(test)]
mod public_child_id_tests {
    use super::{PublicPaneId, PublicTabId};

    #[test]
    fn public_child_id_aliases_keep_their_canonical_text_and_wire_encoding() {
        let tab_id = PublicTabId::new("wA", 32);
        let pane_id = PublicPaneId::new("wA", 33);

        assert_eq!(tab_id.as_str(), "wA:t0");
        assert_eq!(tab_id.workspace_id(), "wA");
        assert_eq!(tab_id.number(), 32);
        assert_eq!(pane_id.as_str(), "wA:p11");
        assert_eq!(pane_id.workspace_id(), "wA");
        assert_eq!(pane_id.number(), 33);
        assert_eq!("wA:t0".parse::<PublicTabId>(), Ok(tab_id.clone()));
        assert_eq!("wA:p11".parse::<PublicPaneId>(), Ok(pane_id.clone()));
        assert!("wA:p11".parse::<PublicTabId>().is_err());
        assert!("wA:t0".parse::<PublicPaneId>().is_err());

        let tab_wire = crate::codec::to_vec(&tab_id).expect("tab id encoding");
        let pane_wire = crate::codec::to_vec(&pane_id).expect("pane id encoding");
        assert_eq!(
            tab_wire,
            crate::codec::to_vec("wA:t0").expect("tab string encoding")
        );
        assert_eq!(
            pane_wire,
            crate::codec::to_vec("wA:p11").expect("pane string encoding")
        );
        assert_eq!(
            crate::codec::from_slice_exact::<PublicTabId>(&tab_wire).expect("tab id decoding"),
            tab_id
        );
        assert_eq!(
            crate::codec::from_slice_exact::<PublicPaneId>(&pane_wire).expect("pane id decoding"),
            pane_id
        );
    }
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
