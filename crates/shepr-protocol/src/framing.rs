use super::codec::{self, CodecError};
use super::limits::HANDSHAKE_FRAME_SIZE;
use super::*;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

// limits-exempt: the frame format prefixes payloads with a four-byte u32 LE length.
const LENGTH_PREFIX_BYTES: usize = 4;

// ---------------------------------------------------------------------------
// Framing: length-prefixed binary messages
// ---------------------------------------------------------------------------

/// Errors that can occur during framing operations.
#[derive(Debug)]
pub enum FramingError {
    /// The decoded payload length exceeds the applicable fixed frame limit.
    Oversized { claimed: usize, max: usize },
    /// An I/O error occurred while reading or writing.
    Io(io::Error),
    /// Encoding or decoding the payload with the wire codec failed.
    Codec(CodecError),
    /// A decoded surface update did not match the connection's surface baseline.
    SurfaceDecode(super::surface_reuse::SurfaceDecodeError),
    /// The connection was closed before a complete frame could be read.
    UnexpectedEof,
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FramingError::Oversized { claimed, max } => {
                write!(f, "frame size {claimed} exceeds maximum {max}")
            }
            FramingError::Io(e) => write!(f, "I/O error: {e}"),
            FramingError::Codec(e) => write!(f, "codec error: {e}"),
            FramingError::SurfaceDecode(e) => write!(f, "surface decode error: {e}"),
            FramingError::UnexpectedEof => write!(f, "unexpected end of stream"),
        }
    }
}

impl std::error::Error for FramingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FramingError::Io(e) => Some(e),
            FramingError::Codec(e) => Some(e),
            FramingError::SurfaceDecode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FramingError {
    fn from(e: io::Error) -> Self {
        FramingError::Io(e)
    }
}

impl From<CodecError> for FramingError {
    fn from(e: CodecError) -> Self {
        FramingError::Codec(e)
    }
}

/// Serializes a message and writes it as a length-prefixed frame:
/// `[u32LE length][codec payload]` (see `protocol::codec` for the payload format).
///
/// This is a blocking/synchronous write suitable for use with `std::os::unix::net::UnixStream`
/// in blocking mode, or with any `Write` implementor.
///
/// # Errors
///
/// Returns `FramingError::Oversized`, without writing anything, if the payload
/// exceeds `MAX_FRAME_SIZE`. Every reader enforces that cap and drops the
/// connection on a larger frame, so refusing here keeps the failure local to
/// the one message instead of tearing down the peer connection.
pub fn write_message<W: Write, M: Serialize>(writer: &mut W, msg: &M) -> Result<(), FramingError> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Encodes a message as one complete frame, `[u32LE length][codec payload]`,
/// in a single buffer that is returned as is.
///
/// This is the owned-buffer form of [`write_message`] for callers that queue
/// frames rather than write them: the payload is encoded straight behind a
/// placeholder prefix, so no second copy of the frame is ever made. It retains
/// at most `MAX_FRAME_SIZE` payload bytes while counting excess bytes for an
/// exact oversized error. Passing a `Vec` to `write_message` instead would
/// encode into one buffer and then copy all of it into the `Vec`.
///
/// # Errors
///
/// `FramingError::Oversized` if the payload exceeds `MAX_FRAME_SIZE` (the
/// encoded buffer is dropped), or `FramingError::Codec` if encoding fails.
pub fn encode_frame<M: Serialize>(msg: &M) -> Result<Vec<u8>, FramingError> {
    // Keep the output bounded during the one serialization pass. Calling
    // `encoded_len` first would traverse every field again on the client
    // fanout path; this buffer counts any excess bytes without retaining them.
    let mut output = FramePayloadBuffer::new();
    let len = codec::encode_into(&mut output, msg)?;
    if !frame_payload_fits(len) {
        return Err(FramingError::Oversized {
            claimed: len,
            max: MAX_FRAME_SIZE,
        });
    }
    let prefix = u32::try_from(len).map_err(|_| FramingError::Oversized {
        claimed: len,
        max: MAX_FRAME_SIZE,
    })?;
    output.frame[..LENGTH_PREFIX_BYTES].copy_from_slice(&prefix.to_le_bytes());
    Ok(output.frame)
}

struct FramePayloadBuffer {
    frame: Vec<u8>,
    payload_len: usize,
}

impl FramePayloadBuffer {
    fn new() -> Self {
        Self {
            frame: vec![0u8; LENGTH_PREFIX_BYTES],
            payload_len: 0,
        }
    }
}

impl Write for FramePayloadBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next_len = self
            .payload_len
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("encoded frame size overflow"))?;
        let remaining = MAX_FRAME_SIZE.saturating_sub(self.payload_len);
        let retained = bytes.len().min(remaining);
        self.frame.extend_from_slice(&bytes[..retained]);
        self.payload_len = next_len;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads and deserializes a length-prefixed protocol frame from a reader.
///
/// Reassembles partial reads correctly. Rejects frames whose declared length
/// exceeds `MAX_FRAME_SIZE` without panicking or allocating oversized buffers.
pub fn read_message<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<M, FramingError> {
    read_message_with_limit(reader, super::MAX_FRAME_SIZE)
}

/// Reads a client hello with the smaller fixed handshake frame limit.
pub fn read_handshake_message<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<M, FramingError> {
    read_message_with_limit(reader, HANDSHAKE_FRAME_SIZE)
}

fn read_message_with_limit<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
    max_frame_size: usize,
) -> Result<M, FramingError> {
    // Read the 4-byte length prefix, reassembling partial reads.
    let mut len_buf = [0u8; LENGTH_PREFIX_BYTES];
    read_exact_or_eof(reader, &mut len_buf)?;
    let claimed_len = usize::try_from(u32::from_le_bytes(len_buf)).unwrap_or(usize::MAX);

    if claimed_len > max_frame_size {
        return Err(FramingError::Oversized {
            claimed: claimed_len,
            max: max_frame_size,
        });
    }

    // Read the payload, reassembling partial reads.
    let mut payload = vec![0u8; claimed_len];
    read_exact_or_eof(reader, &mut payload)?;

    // The decoder must consume the full payload. Trailing bytes after the
    // decoded message indicate a protocol violation (e.g., a corrupted length
    // prefix or concatenated payloads) and yield `CodecError::TrailingBytes`.
    codec::from_slice_exact(&payload).map_err(FramingError::Codec)
}

/// Like `Read::read_exact`, but returns `FramingError::UnexpectedEof`
/// when the reader hits end-of-stream before filling the buffer, instead
/// of the generic `io::ErrorKind::UnexpectedEof`.
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), FramingError> {
    reader.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FramingError::UnexpectedEof
        } else {
            FramingError::Io(e)
        }
    })
}
