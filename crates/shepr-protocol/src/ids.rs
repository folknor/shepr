use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub fn new(workspace_id: impl Into<String>, number: usize) -> Self {
        let workspace_id = WorkspaceId::new(workspace_id);
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

/// Text that does not parse as a public tab id is kept verbatim, with no
/// workspace and number 0: an id no server issues, which client tests use as
/// an opaque label. Deserializing refuses such text.
impl From<&str> for PublicTabId {
    fn from(value: &str) -> Self {
        value.parse().unwrap_or_else(|_| Self {
            workspace_id: WorkspaceId::new(""),
            number: 0,
            encoded: value.to_owned(),
        })
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicPaneId {
    workspace_id: WorkspaceId,
    number: usize,
    encoded: String,
}

impl PublicPaneId {
    pub fn new(workspace_id: impl Into<String>, number: usize) -> Self {
        let workspace_id = WorkspaceId::new(workspace_id);
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

/// Text that does not parse as a public pane id is kept verbatim, with no
/// workspace and number 0: an id no server issues, which client tests use as
/// an opaque label. Deserializing refuses such text.
impl From<&str> for PublicPaneId {
    fn from(value: &str) -> Self {
        value.parse().unwrap_or_else(|_| Self {
            workspace_id: WorkspaceId::new(""),
            number: 0,
            encoded: value.to_owned(),
        })
    }
}

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
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let counter = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
        Self(format!("term_{micros:x}{counter:x}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn test_new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

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
