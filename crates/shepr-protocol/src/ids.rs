use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;
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

const WORKSPACE_ID_PREFIX: char = 'w';

/// Stable public workspace identity. Its spelling is only needed at process
/// and API boundaries; a launch must not mix it with tab or pane identities.
///
/// The only spelling is `w` followed by a one-based public number
/// ([`encode_public_number`]). A value is built from that number or parsed
/// from its canonical text, deserialization included, so no workspace ID
/// exists that the server's allocator could not have issued.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct WorkspaceId {
    // Declared first so the derived order and hash follow the text.
    text: String,
    number: usize,
}

impl WorkspaceId {
    /// The ID of one-based public workspace number `number`; zero has none.
    pub fn from_number(number: usize) -> Option<Self> {
        (number > 0).then(|| Self {
            text: format!("{WORKSPACE_ID_PREFIX}{}", encode_public_number(number)),
            number,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The one-based public number this ID spells.
    pub fn number(&self) -> usize {
        self.number
    }
}

/// Text that is not a canonical workspace ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceIdParseError;

impl fmt::Display for WorkspaceIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid workspace id")
    }
}

impl std::error::Error for WorkspaceIdParseError {}

impl FromStr for WorkspaceId {
    type Err = WorkspaceIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .strip_prefix(WORKSPACE_ID_PREFIX)
            .and_then(parse_public_number)
            .and_then(Self::from_number)
            // Decoding accepts no alternative spellings today; the round trip
            // keeps that true if the alphabet or decoder ever loosens.
            .filter(|id| id.text == value)
            .ok_or(WorkspaceIdParseError)
    }
}

impl TryFrom<String> for WorkspaceId {
    type Error = WorkspaceIdParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl serde::Serialize for WorkspaceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

/// Debug shows the text alone, as a newtype over it would; the number is
/// derived from it.
impl fmt::Debug for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WorkspaceId").field(&self.text).finish()
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<WorkspaceId> for String {
    fn from(id: WorkspaceId) -> Self {
        id.text
    }
}

impl From<&WorkspaceId> for String {
    fn from(id: &WorkspaceId) -> Self {
        id.text.clone()
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
///
/// The workspace segment is a [`WorkspaceId`], so a value is built from one
/// or parsed from text whose workspace segment is canonical, deserialization
/// included: no child ID names a workspace the server's allocator could not
/// have issued.
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
    /// Builds a child ID from its workspace and a one-based number.
    ///
    /// # Panics
    ///
    /// Panics if `number` is zero, which no canonical public child ID spells.
    pub fn new(workspace_id: &WorkspaceId, number: usize) -> Self {
        assert!(number > 0, "public child IDs use one-based numbers");
        Self {
            encoded: format!("{}:{}{}", workspace_id, KIND, encode_public_number(number)),
            workspace_id: workspace_id.clone(),
            number,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
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
        parse_public_child_id(value, KIND)
            .map(|(workspace_id, number)| Self::new(&workspace_id, number))
            // As for `WorkspaceId`: the round trip keeps alternative spellings
            // out if the alphabet or decoder ever loosens.
            .filter(|id| id.encoded == value)
            .ok_or(PublicIdParseError { kind: KIND })
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

/// Splits canonical child text into its workspace and number. Both parts must
/// be canonical, so re-encoding the result reproduces `value` exactly.
fn parse_public_child_id(value: &str, kind: char) -> Option<(WorkspaceId, usize)> {
    let (workspace_id, encoded_id) = value.split_once(':')?;
    let encoded_number = encoded_id.strip_prefix(kind)?;
    Some((
        workspace_id.parse().ok()?,
        parse_public_number(encoded_number)?,
    ))
}

/// Opaque identity for a server-owned terminal.
///
/// During the pane-backed transition this is stored one-to-one beside panes,
/// but callers must not derive it from a pane id or layout position. That is
/// structural: a value comes from [`TerminalId::alloc`] or from parsing text
/// in the exact form `alloc` writes (`term_<stamp>_<counter>`), and
/// deserialization goes through the same parse.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct TerminalId(String);

// Starting at one keeps generated terminal ID suffixes nonzero.
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

/// The stamp every terminal ID of this process carries, taken at the first
/// allocation. It only has to tell one server lifetime from another (the
/// counter restarts with each process), so a stale attach target from an
/// earlier server never names a new terminal. It is identity, not a time any
/// decision reads, which is why it is sampled here once rather than passed in
/// through the clock seam.
static TERMINAL_ID_STAMP: OnceLock<Result<Duration, Duration>> = OnceLock::new();

impl TerminalId {
    pub fn alloc() -> Self {
        let stamp =
            *TERMINAL_ID_STAMP.get_or_init(|| match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(duration) => Ok(duration),
                Err(error) => Err(error.duration()),
            });
        // One allocation per terminal never exhausts a u64; wrapping would
        // repeat an earlier id, so exhaustion is refused rather than wrapped.
        let counter =
            match NEXT_TERMINAL_ID.try_update(Ordering::Relaxed, Ordering::Relaxed, |counter| {
                counter.checked_add(1)
            }) {
                Ok(counter) => counter,
                Err(_) => panic!("terminal id allocation counter exhausted"),
            };
        Self::from_clock_and_counter(stamp, counter)
    }

    pub fn as_str(&self) -> &str {
        &self.0
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

/// Text that is not a terminal ID in the form [`TerminalId::alloc`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalIdParseError;

impl fmt::Display for TerminalIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid terminal id")
    }
}

impl std::error::Error for TerminalIdParseError {}

impl FromStr for TerminalId {
    type Err = TerminalIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let rest = value.strip_prefix("term_").ok_or(TerminalIdParseError)?;
        let (before_epoch, rest) = match rest.strip_prefix("before_") {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        let (micros, counter) = rest.split_once('_').ok_or(TerminalIdParseError)?;
        let micros = Duration::from_micros(micros.parse().map_err(|_| TerminalIdParseError)?);
        let counter = u64::from_str_radix(counter, 16)
            .ok()
            .filter(|counter| *counter > 0)
            .ok_or(TerminalIdParseError)?;
        let stamp = if before_epoch {
            Err(micros)
        } else {
            Ok(micros)
        };
        // The number parsers accept signs, leading zeros and upper-case hex;
        // re-encoding and comparing refuses every spelling `alloc` never writes.
        Some(Self::from_clock_and_counter(stamp, counter))
            .filter(|id| id.0 == value)
            .ok_or(TerminalIdParseError)
    }
}

impl TryFrom<String> for TerminalId {
    type Error = TerminalIdParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// This crate's own tests spell workspace IDs as literals; the text must be
/// canonical. Other crates build them with `from_number` or parse them.
#[cfg(test)]
impl From<&str> for WorkspaceId {
    fn from(value: &str) -> Self {
        value
            .parse()
            .unwrap_or_else(|error| panic!("{value:?}: {error}"))
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

#[cfg(test)]
mod public_child_id_tests {
    use super::{PublicPaneId, PublicTabId, WorkspaceId};

    #[test]
    fn public_child_id_aliases_keep_their_canonical_text_and_wire_encoding() {
        let workspace_id = WorkspaceId::from("wA");
        let tab_id = PublicTabId::new(&workspace_id, 32);
        let pane_id = PublicPaneId::new(&workspace_id, 33);

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

    #[test]
    fn public_child_ids_refuse_a_workspace_segment_the_allocator_never_writes() {
        for invalid in [
            "",
            ":p1",
            "w1:",
            "w1:p",
            "w1:p1 ",
            "w1:p1:p1",
            "wOLD:p1",
            "ws_1:p1",
            "old-workspace:p9",
            "w_1:p1",
            "W1:p1",
            "a:b:p1",
        ] {
            assert!(invalid.parse::<PublicPaneId>().is_err(), "{invalid:?}");
            let tab = invalid.replacen(":p", ":t", 1);
            assert!(tab.parse::<PublicTabId>().is_err(), "{tab:?}");
        }
        let wire = crate::codec::to_vec("wOLD:p1").expect("string encoding");
        assert!(crate::codec::from_slice_exact::<PublicPaneId>(&wire).is_err());
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

    #[test]
    fn allocated_terminal_ids_parse_back_and_differ() {
        let first = TerminalId::alloc();
        let second = TerminalId::alloc();

        assert_ne!(first, second);
        assert_eq!(first.as_str().parse::<TerminalId>(), Ok(first.clone()));
        let wire = crate::codec::to_vec(&second).expect("terminal id encoding");
        assert_eq!(
            wire,
            crate::codec::to_vec(second.as_str()).expect("terminal id string encoding")
        );
        assert_eq!(
            crate::codec::from_slice_exact::<TerminalId>(&wire).expect("terminal id decoding"),
            second
        );
    }

    #[test]
    fn terminal_ids_refuse_every_spelling_alloc_never_writes() {
        for canonical in ["term_0_1", "term_before_7_1", "term_17_ff"] {
            assert!(canonical.parse::<TerminalId>().is_ok(), "{canonical}");
        }
        for invalid in [
            "",
            "t1",
            "terminal-a",
            "7",
            "term_",
            "term_1",
            "term_1_",
            "term__1",
            "term_1_0",
            "term_01_1",
            "term_1_01",
            "term_1_FF",
            "term_+1_1",
            "term_1_+1",
            "term_before__1",
            "term_1_1_1",
        ] {
            assert!(invalid.parse::<TerminalId>().is_err(), "{invalid:?}");
        }
        let wire = crate::codec::to_vec("terminal-a").expect("string encoding");
        assert!(crate::codec::from_slice_exact::<TerminalId>(&wire).is_err());
    }
}

#[cfg(test)]
mod workspace_id_tests {
    use super::WorkspaceId;

    #[test]
    fn workspace_ids_spell_their_public_number() {
        assert_eq!(WorkspaceId::from_number(0), None);
        let id = WorkspaceId::from_number(33).expect("nonzero number");
        assert_eq!(id.as_str(), "w11");
        assert_eq!(id.number(), 33);
        assert_eq!("w11".parse::<WorkspaceId>(), Ok(id.clone()));

        let wire = crate::codec::to_vec(&id).expect("workspace id encoding");
        assert_eq!(
            wire,
            crate::codec::to_vec("w11").expect("workspace string encoding")
        );
        assert_eq!(
            crate::codec::from_slice_exact::<WorkspaceId>(&wire).expect("workspace id decoding"),
            id
        );
    }

    #[test]
    fn workspace_ids_refuse_text_the_allocator_never_writes() {
        for invalid in [
            "",
            "w",
            "1",
            "wA:p1",
            "ws_1",
            "wOLD",
            "W1",
            "w1 ",
            "old-workspace",
        ] {
            assert!(invalid.parse::<WorkspaceId>().is_err(), "{invalid:?}");
        }
        let wire = crate::codec::to_vec("ws_1").expect("string encoding");
        assert!(crate::codec::from_slice_exact::<WorkspaceId>(&wire).is_err());
    }
}
