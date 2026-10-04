//! Pane history as a save holds it. This file has the digest that names a
//! history file and the text a save serializes; `carry` keeps history from one
//! save to the next and `serialize` writes it within the file's size cap.

mod carry;
mod serialize;

use std::sync::Arc;

use shepr_protocol::PanePublicNumber;

use super::schema::{
    PaneHistorySnapshot, SessionHistorySnapshot, SnapshotVersion, WorkspaceHistorySnapshot,
};
use crate::pane::HistoryPiece;

pub use self::carry::{HistoryCarry, PendingHistory};
pub(super) use self::carry::{PendingPaneHistory, ResolvedHistory};
pub(super) use self::serialize::{
    CappedBuf, HistoryTrim, MAX_SESSION_HISTORY_FILE_BYTES, SerializedHistory, ensure_history_size,
    serialize_history,
};

/// SHA-256 of a history file's bytes: how a layout names the history it pairs
/// with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryDigest([u8; 32]);

impl HistoryDigest {
    pub(super) fn from_bytes(bytes: &[u8]) -> Self {
        Self(sha256_bytes(bytes))
    }

    pub(super) fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        // limits-exempt: a SHA-256 digest is 32 bytes by the hash definition.
        let mut bytes = [0; 32];
        for (index, [high, low]) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = (hex_digit(*high)? << 4) | hex_digit(*low)?;
        }
        Some(Self(bytes))
    }

    pub(super) fn to_hex(self) -> String {
        encode_sha256(&self.0)
    }
}

impl serde::Serialize for HistoryDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for HistoryDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::from_hex(&hex)
            .ok_or_else(|| serde::de::Error::custom("expected a 64-digit SHA-256 hex digest"))
    }
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes).into()
}

fn encode_sha256(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(digest.len() * 2);
    for &byte in digest {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 15)]));
    }
    hex
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The SHA-256 of a history file's bytes: how a layout names the history it
/// pairs with.
pub(super) fn history_digest(json: &[u8]) -> HistoryDigest {
    HistoryDigest::from_bytes(json)
}

/// One pane's history text as a save holds it: the pieces its history cache
/// keeps (or the one piece restored from the file), shared with the cache so
/// that holding the text for a save copies none of it. The text is the pieces
/// in order, each preceded by `\r\n` when its `break_before` says so.
#[derive(Clone, Debug)]
pub(super) struct HistoryText {
    pub(super) pieces: Vec<HistoryPiece>,
}

impl HistoryText {
    pub(super) fn single(text: Arc<str>) -> Self {
        Self {
            pieces: vec![HistoryPiece {
                text,
                break_before: false,
            }],
        }
    }

    /// The text as one string. A save does not need it (it serializes the
    /// pieces), which is the point of keeping them apart.
    pub(super) fn assemble(&self) -> String {
        let mut text = String::with_capacity(
            self.pieces
                .iter()
                .map(|piece| piece.text.len() + 2)
                .sum::<usize>(),
        );
        for piece in &self.pieces {
            if piece.break_before {
                text.push_str("\r\n");
            }
            text.push_str(&piece.text);
        }
        text
    }
}

/// What a save writes as the history file, before serializing: the pane
/// histories of each workspace, sorted by pane number, as
/// [`HistoryText`]. The write-side twin of [`SessionHistorySnapshot`], which
/// is what reading the file gives; it serializes to the same JSON.
#[derive(Clone)]
pub(super) struct SessionHistory {
    pub(super) version: SnapshotVersion,
    pub(super) workspaces: Vec<Vec<(PanePublicNumber, HistoryText)>>,
}

impl SessionHistory {
    /// The history with every pane's text assembled into one string.
    pub(super) fn into_snapshot(self) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: self.version,
            workspaces: self
                .workspaces
                .into_iter()
                .map(|panes| WorkspaceHistorySnapshot {
                    panes: panes
                        .into_iter()
                        .map(|(number, text)| {
                            (
                                number,
                                PaneHistorySnapshot {
                                    ansi: text.assemble(),
                                },
                            )
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}
