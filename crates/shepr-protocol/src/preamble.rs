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
//! [8 bytes magic "SHEPRBID"][16 bytes BUILD_ID, ASCII]
//! ```
//!
//! The layout never depends on the codec, so any two builds can read each
//! other's preamble and report exactly which builds met.
//!
//! The client writes its preamble and hello together before reading
//! anything, so the server reads first: a socket shared with line-based JSON
//! peers must not speak before it knows who connected. The server then
//! answers a recognisable preamble of any build with its own, including when
//! it refuses the connection, so each side learns the other's identity. On
//! another build's preamble the server writes its own and closes without
//! decoding the hello, whose layout is that build's.

use std::io::{self, Read};

/// Marks the start of a shepr client-protocol connection.
pub const PREAMBLE_MAGIC: [u8; 8] = *b"SHEPRBID";

// limits-exempt: the fixed build-identity preamble stores a 16-byte fingerprint.
pub(crate) const BUILD_ID_BYTES: usize = 16;
/// Total preamble length in bytes.
pub const PREAMBLE_LEN: usize = PREAMBLE_MAGIC.len() + BUILD_ID_BYTES;

/// The build identity a peer announced in its preamble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerBuild {
    pub build_id: super::BuildIdentity,
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
                "peer did not open with the shepr build-identity preamble; it is not a shepr endpoint or predates this preamble",
            ),
            Self::DifferentBuild(peer) => write!(
                f,
                "build mismatch: peer is a different shepr build (build {}); this is build {}. Install the same shepr build on both sides and restart the server",
                peer.build_id,
                super::limits::BUILD_ID,
            ),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for PreambleError {}

/// This build's preamble.
pub fn local_preamble() -> [u8; PREAMBLE_LEN] {
    preamble_for(super::limits::BUILD_ID)
}

/// Encodes the given build identity in the fixed-size raw preamble.
pub fn preamble_for(build_id: &str) -> [u8; PREAMBLE_LEN] {
    let mut preamble = [0u8; PREAMBLE_LEN];
    let (magic, id) = preamble.split_at_mut(PREAMBLE_MAGIC.len());
    magic.copy_from_slice(&PREAMBLE_MAGIC);
    // Invalid input cannot become a truncated valid fingerprint.
    let identity = build_id
        .parse::<super::BuildIdentity>()
        .unwrap_or(super::BuildIdentity::Unidentifiable);
    id.copy_from_slice(&identity.preamble_bytes());
    preamble
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
    check_against(received, super::limits::BUILD_ID)
}

/// Accepts `received` only when it announces `ours` and `ours` is an identity
/// at all: a build whose identity could not be established matches no peer,
/// including one announcing the same marker.
fn check_against(received: &[u8; PREAMBLE_LEN], ours: &str) -> Result<(), PreambleError> {
    let (magic, id) = received.split_at(PREAMBLE_MAGIC.len());
    if magic != PREAMBLE_MAGIC {
        return Err(PreambleError::NotShepr);
    }
    if *received == preamble_for(ours) && super::is_identifiable_build_id(ours) {
        return Ok(());
    }
    // The magic already identified a shepr peer, so identity bytes that are
    // not a canonical fingerprint still make it another build, one whose
    // identity cannot be named: it gets the build-mismatch guidance and a
    // restart, never the not-a-shepr-endpoint refusal.
    let build_id = std::str::from_utf8(id)
        .ok()
        .and_then(|id| id.parse().ok())
        .unwrap_or(super::BuildIdentity::Unidentifiable);
    Err(PreambleError::DifferentBuild(PeerBuild { build_id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_for_names_the_given_build() {
        let bytes = preamble_for("0123456789abcdef");
        assert_eq!(&bytes[..PREAMBLE_MAGIC.len()], &PREAMBLE_MAGIC);
        assert_eq!(&bytes[PREAMBLE_MAGIC.len()..], b"0123456789abcdef");
    }

    #[test]
    fn own_preamble_is_accepted() {
        let bytes = local_preamble();
        assert_eq!(bytes.len(), PREAMBLE_LEN);
        read_preamble(&mut bytes.as_slice()).expect("same build");
    }

    #[test]
    fn different_build_is_named() {
        let other = preamble_for("00000000deadbeef");
        match read_preamble(&mut other.as_slice()) {
            Err(PreambleError::DifferentBuild(peer)) => {
                assert_eq!(peer.build_id.to_string(), "00000000deadbeef");
                let message = PreambleError::DifferentBuild(peer).to_string();
                assert!(message.contains("build mismatch"), "{message}");
                assert!(message.contains("00000000deadbeef"), "{message}");
                assert!(
                    message.contains(super::super::limits::BUILD_ID),
                    "{message}"
                );
            }
            other => panic!("expected a different build, got {other:?}"),
        }
    }

    #[test]
    fn different_build_id_is_a_mismatch() {
        let other = preamble_for("ffffffffffffffff");
        if super::super::limits::BUILD_ID == "ffffffffffffffff" {
            return;
        }
        assert!(matches!(
            read_preamble(&mut other.as_slice()),
            Err(PreambleError::DifferentBuild(_))
        ));
    }

    /// A build that could not establish its identity is refused by a peer
    /// stating the same marker, so two such builds never talk.
    #[test]
    fn an_unidentifiable_build_matches_no_peer_not_even_itself() {
        let unidentifiable = "unidentifiable--";
        let received = preamble_for(unidentifiable);
        match check_against(&received, unidentifiable) {
            Err(PreambleError::DifferentBuild(peer)) => {
                assert_eq!(peer.build_id.to_string(), unidentifiable);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(check_against(&preamble_for("0123456789abcdef"), "0123456789abcdef").is_ok());
    }

    #[test]
    fn malformed_identity_bytes_are_an_unidentifiable_build() {
        for id in [*b"0123456789abcdeF", *b"0123456789abcd\0\0", [0xff; 16]] {
            let mut bytes = local_preamble();
            bytes[PREAMBLE_MAGIC.len()..].copy_from_slice(&id);
            assert!(matches!(
                read_preamble(&mut bytes.as_slice()),
                Err(PreambleError::DifferentBuild(PeerBuild {
                    build_id: crate::BuildIdentity::Unidentifiable
                }))
            ));
        }
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
