use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;

const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";

/// Encodes a public number in bijective base 32 (digits 1..=32, no zero
/// digit), so `"0"` is the digit for 32, not zero. Public numbers start at 1;
/// zero has no digits and encodes to the empty string, which is what
/// `decode_public_number` maps back to zero.
fn encode_public_number(mut value: usize) -> String {
    let mut encoded = Vec::new();
    while value > 0 {
        let digit = (value - 1) % PUBLIC_ID_ALPHABET.len();
        encoded.push(PUBLIC_ID_ALPHABET[digit] as char);
        value = (value - 1) / PUBLIC_ID_ALPHABET.len();
    }
    encoded.iter().rev().collect()
}

fn decode_public_number(value: &str) -> Option<usize> {
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
/// and API boundaries; a launch must not mix it with pane identities.
///
/// The only spelling is `w` followed by a one-based public number
/// ([`encode_public_number`]). A value is built from that number or parsed
/// from its canonical text, deserialization included, so no workspace ID
/// exists that the server's allocator could not have issued.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(NonZeroUsize);

impl WorkspaceId {
    /// The first ID an allocator issues: public numbers are one-based, and
    /// zero spells no workspace ID.
    pub const FIRST: Self = Self(NonZeroUsize::MIN);

    /// The ID of allocator number `number`; zero has none.
    pub fn from_number(number: usize) -> Option<Self> {
        NonZeroUsize::new(number).map(Self)
    }

    /// The allocator's stable number, independent of the workspace list order.
    pub const fn number(&self) -> usize {
        self.0.get()
    }
}

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
            .strip_prefix('w')
            .and_then(parse_public_number)
            .and_then(Self::from_number)
            .ok_or(WorkspaceIdParseError)
    }
}
impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "w{}", encode_public_number(self.number()))
    }
}

/// Debug shows the canonical text, the spelling logs and the API use, not
/// the allocator number behind it.
impl fmt::Debug for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WorkspaceId")
            .field(&format_args!("{self}"))
            .finish()
    }
}

/// A pane's nonzero public number, distinct from layout positions and internal IDs.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PanePublicNumber(NonZeroUsize);
impl PanePublicNumber {
    /// The number of a workspace's first pane.
    pub const FIRST: Self = Self(NonZeroUsize::MIN);
    /// The next number of a workspace that holds only its first pane.
    pub const SECOND: Self = Self(NonZeroUsize::MIN.saturating_add(1));

    pub fn new(number: usize) -> Option<Self> {
        NonZeroUsize::new(number).map(Self)
    }

    pub fn get(self) -> usize {
        self.0.get()
    }

    pub fn checked_next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::new)
    }
}
impl fmt::Display for PanePublicNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// Stable public pane identity: its workspace and its number there, spelled
/// `<workspace>:p<number>` only at text boundaries.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicPaneId {
    workspace_id: WorkspaceId,
    number: PanePublicNumber,
}
impl PublicPaneId {
    pub fn new(workspace_id: &WorkspaceId, number: PanePublicNumber) -> Self {
        Self {
            workspace_id: *workspace_id,
            number,
        }
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn number(&self) -> PanePublicNumber {
        self.number
    }
}
impl fmt::Display for PublicPaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:p{}",
            self.workspace_id,
            encode_public_number(self.number.get())
        )
    }
}

/// Debug shows the canonical text, as for [`WorkspaceId`].
impl fmt::Debug for PublicPaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PublicPaneId")
            .field(&format_args!("{self}"))
            .finish()
    }
}

/// Text that is not a canonical public pane ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicIdParseError;
impl fmt::Display for PublicIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid public pane id")
    }
}
impl std::error::Error for PublicIdParseError {}
impl FromStr for PublicPaneId {
    type Err = PublicIdParseError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (workspace, pane) = value.split_once(':').ok_or(PublicIdParseError)?;
        let number = pane
            .strip_prefix('p')
            .and_then(parse_public_number)
            .and_then(PanePublicNumber::new)
            .ok_or(PublicIdParseError)?;
        Ok(Self::new(
            &workspace.parse().map_err(|_| PublicIdParseError)?,
            number,
        ))
    }
}
// Bijective base 32 has exactly one spelling per positive number: no digit
// is worth zero, so there are no leading-zero aliases, and the alphabet is
// case sensitive. Both text parsers share this rule; overflow and an empty
// segment fail before an ID can be constructed.
fn parse_public_number(encoded: &str) -> Option<usize> {
    decode_public_number(encoded).filter(|number| *number > 0)
}

// Human-readable formats are text boundaries. The positional TUI codec carries
// the nonzero numbers directly and never parses an ID's display spelling.
impl serde::Serialize for WorkspaceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(self)
        } else {
            serde::Serialize::serialize(&self.0, serializer)
        }
    }
}
impl<'de> serde::Deserialize<'de> for WorkspaceId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let text = <String as serde::Deserialize>::deserialize(deserializer)?;
            text.parse().map_err(serde::de::Error::custom)
        } else {
            <NonZeroUsize as serde::Deserialize>::deserialize(deserializer).map(Self)
        }
    }
}
impl serde::Serialize for PublicPaneId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(self)
        } else {
            serde::Serialize::serialize(&(self.workspace_id, self.number), serializer)
        }
    }
}
impl<'de> serde::Deserialize<'de> for PublicPaneId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let text = <String as serde::Deserialize>::deserialize(deserializer)?;
            text.parse().map_err(serde::de::Error::custom)
        } else {
            let (workspace, number) =
                <(WorkspaceId, PanePublicNumber) as serde::Deserialize>::deserialize(deserializer)?;
            Ok(Self::new(&workspace, number))
        }
    }
}
// Neither ID implements `tracing::Value` (tracing-core seals it); log them
// with `%id`, which writes the canonical text through Display.

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
/// Other crates build IDs with `PublicPaneId::new` or parse them, so no ID
/// exists that a server would not issue.
#[cfg(test)]
impl From<&str> for PublicPaneId {
    fn from(value: &str) -> Self {
        value
            .parse()
            .unwrap_or_else(|error| panic!("{value:?}: {error}"))
    }
}

#[cfg(test)]
impl From<String> for PublicPaneId {
    fn from(value: String) -> Self {
        value.as_str().into()
    }
}

#[cfg(test)]
mod public_pane_id_tests {
    use super::{PublicPaneId, WorkspaceId};

    #[test]
    fn public_pane_ids_keep_their_canonical_text_and_wire_encoding() {
        let workspace_id = WorkspaceId::from("wA");
        let pane_id = PublicPaneId::new(
            &workspace_id,
            super::PanePublicNumber::new(33).expect("number"),
        );

        assert_eq!(pane_id.to_string(), "wA:p11");
        assert_eq!(pane_id.workspace_id(), &workspace_id);
        assert_eq!(pane_id.number().get(), 33);
        assert_eq!("wA:p11".parse::<PublicPaneId>(), Ok(pane_id));
        assert!("wA:t0".parse::<PublicPaneId>().is_err());

        let pane_wire = crate::codec::to_vec(&pane_id).expect("pane id encoding");
        assert_eq!(
            pane_wire,
            crate::codec::to_vec(&(workspace_id, 33usize)).expect("pane numeric encoding")
        );
        assert_eq!(
            crate::codec::from_slice_exact::<PublicPaneId>(&pane_wire).expect("pane id decoding"),
            pane_id
        );
    }

    #[test]
    fn numeric_ids_are_copy_and_refuse_zero_on_the_wire() {
        fn require_copy<T: Copy>() {}
        require_copy::<WorkspaceId>();
        require_copy::<PublicPaneId>();
        assert!(super::PanePublicNumber::new(0).is_none());
        let zero = crate::codec::to_vec(&0usize).expect("zero encoding");
        assert!(crate::codec::from_slice_exact::<WorkspaceId>(&zero).is_err());
        for invalid in [(0usize, 1usize), (1, 0)] {
            let wire = crate::codec::to_vec(&invalid).expect("tuple encoding");
            assert!(crate::codec::from_slice_exact::<PublicPaneId>(&wire).is_err());
        }
        let workspace = WorkspaceId::from_number(usize::MAX).expect("max ID");
        let number = super::PanePublicNumber::new(usize::MAX).expect("max number");
        let pane = PublicPaneId::new(&workspace, number);
        assert_eq!(pane.to_string().parse::<PublicPaneId>(), Ok(pane));
        let wire = crate::codec::to_vec(&pane).expect("pane encoding");
        assert_eq!(
            crate::codec::from_slice_exact::<PublicPaneId>(&wire).expect("pane decoding"),
            pane
        );
    }

    #[test]
    fn public_pane_ids_refuse_a_workspace_segment_the_allocator_never_writes() {
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
        }
        let wire = crate::codec::to_vec("wOLD:p1").expect("string encoding");
        assert!(crate::codec::from_slice_exact::<PublicPaneId>(&wire).is_err());
    }
}

#[cfg(test)]
mod workspace_id_tests {
    use super::WorkspaceId;

    #[test]
    fn workspace_ids_spell_their_public_number() {
        assert_eq!(WorkspaceId::from_number(0), None);
        let id = WorkspaceId::from_number(33).expect("nonzero number");
        assert_eq!(id.to_string(), "w11");
        assert_eq!(id.number(), 33);
        assert_eq!("w11".parse::<WorkspaceId>(), Ok(id));

        let wire = crate::codec::to_vec(&id).expect("workspace id encoding");
        assert_eq!(
            wire,
            crate::codec::to_vec(&33usize).expect("workspace numeric encoding")
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
