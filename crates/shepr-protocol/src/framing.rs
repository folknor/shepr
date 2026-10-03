use super::codec::{self, CodecError};
use super::limits::HANDSHAKE_FRAME_SIZE;
use super::*;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

// limits-exempt: the frame format prefixes payloads with a four-byte u32 LE length.
const LENGTH_PREFIX_BYTES: usize = 4;

// limits-exempt: the frame format's continuation marker, the top bit of the
// length prefix. `MAX_FRAME_SIZE` is far below 2^31, so no length uses it.
const CONTINUED: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// Framing: length-prefixed binary messages
// ---------------------------------------------------------------------------
//
// A message is one codec payload carried in one or more frames. Each frame is
// `[u32 LE prefix][payload bytes]`: the low 31 bits of the prefix are the
// frame's payload length, at most `MAX_FRAME_SIZE`, and the top bit says the
// message continues in the next frame. A message that fits in one frame is
// exactly the plain `[length][payload]` frame, so the common case pays nothing
// for the split. A larger one is cut into full `MAX_FRAME_SIZE` frames with the
// top bit set, then one final frame without it. Frames of one message are
// written as one buffer, so no other message can land between them.
//
// The split is below the codec: every message kind (pane surfaces, surface
// updates, clipboard data, endpoint replies) crosses the same way, and no wire
// type knows how large a frame is. A reader bounds each frame by
// `MAX_FRAME_SIZE` and the whole message by the cap it is given. Continued
// frames must be full-sized, and single-frame readers reject continuation.

/// Errors that can occur during framing operations.
#[derive(Debug)]
pub enum FramingError {
    /// A frame or a whole message exceeds the applicable payload limit.
    LimitExceeded(crate::LimitExceeded),
    /// A continued frame is not a full `MAX_FRAME_SIZE` payload.
    InvalidContinuation { claimed: usize, expected: usize },
    /// A message was continued where the reader permits only one frame.
    UnexpectedContinuation,
    /// An I/O error occurred while reading or writing.
    Io(io::Error),
    /// Encoding or decoding the payload with the wire codec failed.
    Codec(CodecError),
    /// The connection was closed before a complete frame could be read.
    UnexpectedEof,
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FramingError::LimitExceeded(error) => write!(f, "{error}"),
            FramingError::InvalidContinuation { claimed, expected } => write!(
                f,
                "continued frame size {claimed} does not match required size {expected}"
            ),
            FramingError::UnexpectedContinuation => {
                write!(f, "continued message is not allowed here")
            }
            FramingError::Io(e) => write!(f, "I/O error: {e}"),
            FramingError::Codec(e) => write!(f, "codec error: {e}"),
            FramingError::UnexpectedEof => write!(f, "unexpected end of stream"),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for FramingError {}

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

/// Serializes a message and writes it as one or more length-prefixed frames
/// (see [`encode_message`]).
///
/// This is a blocking/synchronous write suitable for use with `std::os::unix::net::UnixStream`
/// in blocking mode, or with any `Write` implementor.
///
/// # Errors
///
/// Returns `FramingError::LimitExceeded`, without writing anything, if the
/// payload exceeds `MAX_MESSAGE_SIZE`.
pub fn write_message<W: Write, M: Serialize>(writer: &mut W, msg: &M) -> Result<(), FramingError> {
    let frames = encode_message(msg)?;
    writer.write_all(&frames)?;
    writer.flush()?;
    Ok(())
}

/// Encodes a message as its complete frame sequence in a single buffer: one
/// frame when the payload fits in `MAX_FRAME_SIZE`, else full frames marked as
/// continued followed by a final one.
///
/// The payload is encoded straight into the buffer, with each frame's prefix
/// reserved as the payload reaches it, so a message is serialized once and
/// never copied. The reader reassembles it with [`read_message`].
///
/// # Errors
///
/// `FramingError::LimitExceeded` if the payload exceeds `MAX_MESSAGE_SIZE` (the
/// encoded buffer is dropped), or `FramingError::Codec` if encoding fails.
pub fn encode_message<M: Serialize>(msg: &M) -> Result<Vec<u8>, FramingError> {
    encode_frames(msg, MAX_MESSAGE_SIZE, crate::LimitKind::MessageBytes)
}

/// Encodes a message that must fit in one frame, `[u32LE length][codec
/// payload]`. Client messages are sent this way: the server reads them with a
/// one-frame cap (`MAX_CLIENT_MESSAGE_SIZE`), so refusing here keeps an
/// oversized message a local error instead of a dropped connection.
///
/// # Errors
///
/// `FramingError::LimitExceeded` if the payload exceeds `MAX_FRAME_SIZE`, or
/// `FramingError::Codec` if encoding fails.
pub fn encode_frame<M: Serialize>(msg: &M) -> Result<Vec<u8>, FramingError> {
    encode_frames(msg, MAX_FRAME_SIZE, crate::LimitKind::FrameBytes)
}

fn encode_frames<M: Serialize>(
    msg: &M,
    max_message: usize,
    limit_kind: crate::LimitKind,
) -> Result<Vec<u8>, FramingError> {
    // Keep the output bounded during the one serialization pass. Calling
    // `encoded_len` first would traverse every field again on the client
    // fanout path; this buffer counts any excess bytes without retaining them.
    let mut output = FramedPayloadBuffer::new(max_message, limit_kind);
    codec::encode_into(&mut output, msg)?;
    output.finish()
}

/// Collects an encoded payload as frames: the prefix of the next frame is
/// reserved whenever the current one is full and more payload follows.
/// Payload beyond `max_message` is counted but not kept.
struct FramedPayloadBuffer {
    frames: Vec<u8>,
    payload_len: usize,
    max_message: usize,
    limit_kind: crate::LimitKind,
}

impl FramedPayloadBuffer {
    fn new(max_message: usize, limit_kind: crate::LimitKind) -> Self {
        Self {
            frames: vec![0u8; LENGTH_PREFIX_BYTES],
            payload_len: 0,
            max_message,
            limit_kind,
        }
    }

    /// Writes every frame prefix and returns the frames.
    fn finish(mut self) -> Result<Vec<u8>, FramingError> {
        let len = self.payload_len;
        let max = self.max_message;
        let limit_kind = self.limit_kind;
        let limit_exceeded = || {
            FramingError::LimitExceeded(crate::LimitExceeded::new(
                crate::Limit::new(limit_kind, max),
                len,
            ))
        };
        if len > max {
            return Err(limit_exceeded());
        }
        let continued = len.saturating_sub(1) / MAX_FRAME_SIZE;
        let full = u32::try_from(MAX_FRAME_SIZE).map_err(|_| limit_exceeded())? | CONTINUED;
        for frame in 0..continued {
            let at = frame * (LENGTH_PREFIX_BYTES + MAX_FRAME_SIZE);
            self.frames[at..at + LENGTH_PREFIX_BYTES].copy_from_slice(&full.to_le_bytes());
        }
        let last = u32::try_from(len - continued * MAX_FRAME_SIZE).map_err(|_| limit_exceeded())?;
        let at = continued * (LENGTH_PREFIX_BYTES + MAX_FRAME_SIZE);
        self.frames[at..at + LENGTH_PREFIX_BYTES].copy_from_slice(&last.to_le_bytes());
        Ok(self.frames)
    }
}

impl Write for FramedPayloadBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next_len = self
            .payload_len
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("encoded frame size overflow"))?;
        if next_len > self.max_message {
            // Oversized: `finish` reports the full count, so nothing more is kept.
            self.payload_len = next_len;
            return Ok(bytes.len());
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let in_frame = self.payload_len % MAX_FRAME_SIZE;
            if in_frame == 0 && self.payload_len > 0 {
                // The current frame is full and payload continues: open the
                // next frame; `finish` fills in its prefix.
                self.frames.extend_from_slice(&[0u8; LENGTH_PREFIX_BYTES]);
            }
            let take = rest.len().min(MAX_FRAME_SIZE - in_frame);
            self.frames.extend_from_slice(&rest[..take]);
            self.payload_len += take;
            rest = &rest[take..];
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads and deserializes one message, reassembling it from as many frames as
/// it spans, up to `MAX_MESSAGE_SIZE` in all.
///
/// Reassembles partial reads correctly. Rejects a frame over `MAX_FRAME_SIZE`
/// or a message over the cap without panicking, before allocating for it. An
/// accepted frame's claimed length is allocated before its bytes are read, so
/// at most one frame is allocated ahead of the bytes that actually arrive.
pub fn read_message<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<M, FramingError> {
    read_message_limited(reader, MAX_MESSAGE_SIZE)
}

/// Like [`read_message`] with a smaller cap on the whole message.
pub fn read_message_limited<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
    max_message: usize,
) -> Result<M, FramingError> {
    read_frames(reader, message_frame_limit(max_message), max_message, true)
}

/// The bound on one frame of a message capped at `max_message`: the frame
/// size, or the message cap when that is smaller, named as what it is.
fn message_frame_limit(max_message: usize) -> crate::Limit {
    if max_message < MAX_FRAME_SIZE {
        crate::Limit::new(crate::LimitKind::MessageBytes, max_message)
    } else {
        crate::Limit::new(crate::LimitKind::FrameBytes, MAX_FRAME_SIZE)
    }
}

/// Like [`read_message_limited`], but requires the message to fit in one frame.
///
/// The server uses this for client messages, whose encoder always emits one
/// frame and whose protocol limit is a single frame.
pub fn read_message_single_frame_limited<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
    max_message: usize,
) -> Result<M, FramingError> {
    read_frames(reader, message_frame_limit(max_message), max_message, false)
}

/// Reads a client hello in one frame with the smaller fixed handshake limit.
pub fn read_handshake_message<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<M, FramingError> {
    read_frames(
        reader,
        crate::Limit::new(crate::LimitKind::FrameBytes, HANDSHAKE_FRAME_SIZE),
        HANDSHAKE_FRAME_SIZE,
        false,
    )
}

/// `frame_limit` bounds each frame and is the limit a refused frame reports.
fn read_frames<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
    frame_limit: crate::Limit,
    max_message: usize,
    allow_continuation: bool,
) -> Result<M, FramingError> {
    let max_frame = frame_limit.max();
    let mut payload = Vec::new();
    loop {
        // Read the 4-byte length prefix, reassembling partial reads.
        let mut len_buf = [0u8; LENGTH_PREFIX_BYTES];
        read_exact_or_eof(reader, &mut len_buf)?;
        let prefix = u32::from_le_bytes(len_buf);
        let continued = prefix & CONTINUED != 0;
        let claimed_len = usize::try_from(prefix & !CONTINUED).unwrap_or(usize::MAX);

        if continued && !allow_continuation {
            return Err(FramingError::UnexpectedContinuation);
        }
        if claimed_len > max_frame {
            return Err(FramingError::LimitExceeded(crate::LimitExceeded::new(
                frame_limit,
                claimed_len,
            )));
        }
        if continued && claimed_len != MAX_FRAME_SIZE {
            return Err(FramingError::InvalidContinuation {
                claimed: claimed_len,
                expected: MAX_FRAME_SIZE,
            });
        }
        let start = payload.len();
        let total = start.saturating_add(claimed_len);
        if total > max_message {
            return Err(FramingError::LimitExceeded(crate::LimitExceeded::new(
                crate::Limit::new(crate::LimitKind::MessageBytes, max_message),
                total,
            )));
        }

        // Read the payload, reassembling partial reads.
        payload.resize(total, 0);
        read_exact_or_eof(reader, &mut payload[start..])?;
        if !continued {
            break;
        }
    }

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
