//! Raw build-identity preamble for client-protocol connections.
//!
//! The codec is positional and not self-describing, so a version carried
//! inside a codec message only helps a peer that can still decode that
//! message; a build whose hello or welcome layout differs would see garbage
//! or a closed connection instead of a mismatch. Every client-protocol
//! connection therefore opens, in both directions, with this fixed-size raw
//! record before any frame:
//!
//! ```text
//! [8 bytes magic "SHEPRBID"][u32 LE PROTOCOL_VERSION][16 bytes BUILD_ID, ASCII]
//! ```
//!
//! The layout never depends on the codec, so any two builds can read each
//! other's preamble and report exactly which builds met.
//!
//! The server writes its preamble as soon as it accepts a connection and
//! then reads the client's; the client writes its preamble and hello, then
//! reads the server's. Each side therefore always receives the other's
//! identity, even when the other side is about to hang up on a mismatch.

use std::io::{self, Read, Write};

/// Marks the start of a shepr client-protocol connection.
pub const PREAMBLE_MAGIC: [u8; 8] = *b"SHEPRBID";

const BUILD_ID_BYTES: usize = 16;
const VERSION_BYTES: usize = 4;

/// Total preamble length in bytes.
pub const PREAMBLE_LEN: usize = PREAMBLE_MAGIC.len() + VERSION_BYTES + BUILD_ID_BYTES;

/// The build identity a peer announced in its preamble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerBuild {
    pub protocol_version: u32,
    pub build_id: String,
}

/// Why a peer's preamble was not accepted.
#[derive(Debug)]
pub enum PreambleError {
    /// The stream ended before a whole preamble arrived.
    UnexpectedEof,
    /// Reading failed (including a passed deadline).
    Io(io::Error),
    /// The first bytes were not a shepr preamble: not a shepr peer, or a
    /// build that predates the preamble.
    NotShepr,
    /// A shepr peer of a different build.
    DifferentBuild(PeerBuild),
}

impl std::fmt::Display for PreambleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEof => f.write_str("connection closed during the build-identity exchange"),
            Self::Io(error) => write!(f, "build-identity exchange failed: {error}"),
            Self::NotShepr => f.write_str(
                "peer did not open with the shepr build-identity preamble; it is not a shepr endpoint of a compatible build",
            ),
            Self::DifferentBuild(peer) => write!(
                f,
                "protocol mismatch: peer is a different shepr build (build {}, protocol {}); this is build {} (protocol {}). Install the same shepr build on both sides and restart the server",
                peer.build_id,
                peer.protocol_version,
                crate::build_info::BUILD_ID,
                super::PROTOCOL_VERSION,
            ),
        }
    }
}

impl std::error::Error for PreambleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// This build's preamble.
pub fn local_preamble() -> [u8; PREAMBLE_LEN] {
    encode(super::PROTOCOL_VERSION, crate::build_info::BUILD_ID)
}

fn encode(protocol_version: u32, build_id: &str) -> [u8; PREAMBLE_LEN] {
    let mut preamble = [0u8; PREAMBLE_LEN];
    let (magic, rest) = preamble.split_at_mut(PREAMBLE_MAGIC.len());
    magic.copy_from_slice(&PREAMBLE_MAGIC);
    let (version, id) = rest.split_at_mut(VERSION_BYTES);
    version.copy_from_slice(&protocol_version.to_le_bytes());
    // `BUILD_ID` is 16 hex digits (`build.rs`); anything shorter is padded
    // with zeros and anything longer truncated, so the record stays fixed.
    for (slot, byte) in id.iter_mut().zip(build_id.bytes()) {
        *slot = byte;
    }
    preamble
}

/// Writes this build's preamble and flushes.
pub fn write_preamble<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(&local_preamble())?;
    writer.flush()
}

/// Reads the peer's preamble and accepts it only if it is this exact build.
pub fn read_preamble<R: Read>(reader: &mut R) -> Result<(), PreambleError> {
    let mut received = [0u8; PREAMBLE_LEN];
    reader.read_exact(&mut received).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            PreambleError::UnexpectedEof
        } else {
            PreambleError::Io(error)
        }
    })?;
    check(&received)
}

fn check(received: &[u8; PREAMBLE_LEN]) -> Result<(), PreambleError> {
    let (magic, rest) = received.split_at(PREAMBLE_MAGIC.len());
    if magic != PREAMBLE_MAGIC {
        return Err(PreambleError::NotShepr);
    }
    if *received == local_preamble() {
        return Ok(());
    }
    let (version, id) = rest.split_at(VERSION_BYTES);
    let mut version_bytes = [0u8; VERSION_BYTES];
    version_bytes.copy_from_slice(version);
    let build_id = id
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| {
            if byte.is_ascii_graphic() {
                char::from(*byte)
            } else {
                '?'
            }
        })
        .collect();
    Err(PreambleError::DifferentBuild(PeerBuild {
        protocol_version: u32::from_le_bytes(version_bytes),
        build_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_preamble_is_accepted() {
        let mut bytes = Vec::new();
        write_preamble(&mut bytes).expect("test precondition");
        assert_eq!(bytes.len(), PREAMBLE_LEN);
        read_preamble(&mut bytes.as_slice()).expect("same build");
    }

    #[test]
    fn different_build_is_named() {
        let other = encode(super::super::PROTOCOL_VERSION + 1, "00000000deadbeef");
        match read_preamble(&mut other.as_slice()) {
            Err(PreambleError::DifferentBuild(peer)) => {
                assert_eq!(peer.build_id, "00000000deadbeef");
                assert_eq!(peer.protocol_version, super::super::PROTOCOL_VERSION + 1);
                let message = PreambleError::DifferentBuild(peer).to_string();
                assert!(message.contains("protocol mismatch"), "{message}");
                assert!(message.contains("00000000deadbeef"), "{message}");
                assert!(message.contains(crate::build_info::BUILD_ID), "{message}");
            }
            other => panic!("expected a different build, got {other:?}"),
        }
    }

    #[test]
    fn same_version_but_different_build_id_is_still_a_mismatch() {
        let other = encode(super::super::PROTOCOL_VERSION, "ffffffffffffffff");
        if crate::build_info::BUILD_ID == "ffffffffffffffff" {
            return;
        }
        assert!(matches!(
            read_preamble(&mut other.as_slice()),
            Err(PreambleError::DifferentBuild(_))
        ));
    }

    #[test]
    fn a_codec_frame_is_not_a_preamble() {
        // A peer that starts straight with a frame (a length prefix) fails
        // the magic check instead of being decoded.
        let mut bytes = super::super::encode_frame(&super::super::ClientMessage::Detach)
            .expect("test precondition");
        bytes.resize(PREAMBLE_LEN.max(bytes.len()), 0);
        assert!(matches!(
            read_preamble(&mut bytes.as_slice()),
            Err(PreambleError::NotShepr)
        ));
    }

    #[test]
    fn short_stream_is_an_eof() {
        let preamble = local_preamble();
        assert!(matches!(
            read_preamble(&mut &preamble[..PREAMBLE_LEN - 1]),
            Err(PreambleError::UnexpectedEof)
        ));
    }
}
