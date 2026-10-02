//! Shepr's own positional serde data format for the wire protocol.
//!
//! The format is not self-describing: both sides must decode into the same
//! Rust type, which holds because client and server are always the same build.
//!
//! Encoding:
//!
//! - `u8`: one raw byte. `u16`/`u32`/`u64`/`usize`/`u128`: unsigned LEB128
//!   varint, canonical (overlong encodings and overflow are rejected).
//! - `i8`..`i64`/`i128`: zigzag, then LEB128 varint.
//! - `bool`: one byte, `0` or `1` (anything else is rejected).
//! - `f32`/`f64`: fixed little-endian IEEE 754 bytes.
//! - `char`: varint of the Unicode scalar value (validated on decode).
//! - `str`/`String`/bytes: varint length, then the raw bytes (UTF-8 validated
//!   for strings). Borrowed `&str`/`&[u8]` deserialization is supported.
//! - `Option`: tag byte `0` (None) or `1` (Some, followed by the value).
//! - unit, unit struct: nothing. Newtype struct: the inner value.
//! - seq/map: varint element count, then the elements (map: key, value pairs).
//!   The serializer must know the length up front; counts use the codec's
//!   logical item cap, with tighter wire-field caps where specified.
//! - tuple, tuple struct, struct: the fields in order, no count, no names.
//! - enum: variant index as varint, then the payload for that variant kind.
//!
//! Positional encoding means `#[serde(skip_serializing_if)]`, `flatten`,
//! `untagged`, internally/adjacently tagged enums and anything else that needs
//! `deserialize_any` or `deserialize_identifier` cannot be used on wire types.
//! The serializer rejects skipped fields and the decoder rejects those calls.
//! A brokkr textlint rule rejects these serde shapes in protocol, core and VT
//! source. Config types use separate TOML shapes, do not cross the wire and are
//! outside that rule's paths. Custom serde implementations still rely on the
//! runtime backstop.
//!
//! Hardening against hostile or corrupt input: every length prefix is checked
//! against the remaining input before allocation (so variable sequences of
//! zero-byte elements are not decodable), sequence and map counts have a
//! logical item cap before allocation, nesting depth is bounded, and
//! `from_slice_exact` rejects trailing bytes.

use std::fmt;
use std::io::Write;

use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Visitor};
use serde::ser::{self, Serialize};

pub use crate::limits::{DEFAULT_MAX_DEPTH, MAX_COLLECTION_ITEMS};

// limits-exempt: canonical u64 LEB128 values use at most ten bytes by the wire format.
const MAX_VARINT_U64_BYTES: usize = 10;
// limits-exempt: canonical u128 LEB128 values use at most nineteen bytes by the wire format.
const MAX_VARINT_U128_BYTES: usize = 19;

/// Errors produced while encoding or decoding the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The input ended before a value was complete.
    UnexpectedEof { needed: usize, remaining: usize },
    /// A varint does not fit the 64-bit (or 128-bit) accumulator.
    VarintOverflow,
    /// A varint used more bytes than its canonical encoding.
    OverlongVarint,
    /// A decoded integer does not fit the requested type.
    IntegerOutOfRange,
    /// A bool byte other than 0 or 1.
    InvalidBool(u8),
    /// An Option tag byte other than 0 or 1.
    InvalidOptionTag(u8),
    /// A decoded `char` is not a Unicode scalar value.
    InvalidChar(u32),
    /// A decoded string is not valid UTF-8.
    InvalidUtf8,
    /// A length prefix claims more items or bytes than the input can hold.
    LengthExceedsInput { len: u64, remaining: usize },
    /// A sequence or map exceeds the codec's logical item limit.
    CollectionLimitExceeded { len: u64, max: usize },
    /// A sequence or map was serialized without a known length.
    UnknownLength,
    /// A struct field was skipped (`skip_serializing_if`), which a positional
    /// format cannot represent.
    SkippedField,
    /// Compound values are nested deeper than the decoder allows.
    DepthLimitExceeded,
    /// The type asked for a self-describing operation (`deserialize_any`,
    /// `deserialize_identifier` or `deserialize_ignored_any`).
    NotSelfDescribing(&'static str),
    /// A complete value was decoded but input bytes remain.
    TrailingBytes { consumed: usize, total: usize },
    /// The encoded size does not fit in `usize` / `u64`.
    SizeOverflow,
    /// Writing to the output failed.
    Io(String),
    /// A custom error raised by a `Serialize` or `Deserialize` implementation.
    Message(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { needed, remaining } => {
                write!(
                    f,
                    "unexpected end of input: needed {needed} bytes, {remaining} remaining"
                )
            }
            Self::VarintOverflow => f.write_str("varint overflows its integer type"),
            Self::OverlongVarint => f.write_str("overlong varint encoding"),
            Self::IntegerOutOfRange => f.write_str("integer out of range for its type"),
            Self::InvalidBool(byte) => write!(f, "invalid bool byte {byte:#04x}"),
            Self::InvalidOptionTag(byte) => write!(f, "invalid option tag {byte:#04x}"),
            Self::InvalidChar(value) => write!(f, "invalid char scalar value {value:#x}"),
            Self::InvalidUtf8 => f.write_str("string is not valid UTF-8"),
            Self::LengthExceedsInput { len, remaining } => {
                write!(
                    f,
                    "length prefix {len} exceeds the {remaining} remaining input bytes"
                )
            }
            Self::CollectionLimitExceeded { len, max } => {
                write!(f, "collection length {len} exceeds the item limit {max}")
            }
            Self::UnknownLength => f.write_str("sequence or map length must be known"),
            Self::SkippedField => f.write_str("skipped struct fields are not supported"),
            Self::DepthLimitExceeded => f.write_str("nesting depth limit exceeded"),
            Self::NotSelfDescribing(operation) => {
                write!(f, "{operation} is not supported by a positional format")
            }
            Self::TrailingBytes { consumed, total } => write!(
                f,
                "decoded {consumed} bytes but payload length was {total}; trailing bytes are not allowed"
            ),
            Self::SizeOverflow => f.write_str("encoded size overflow"),
            Self::Io(error) => write!(f, "write failed: {error}"),
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CodecError {}

impl ser::Error for CodecError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::Message(msg.to_string())
    }
}

impl de::Error for CodecError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::Message(msg.to_string())
    }
}

/// Encodes `value` into `writer` and returns the number of bytes written.
pub fn encode_into<W, T>(writer: &mut W, value: &T) -> Result<usize, CodecError>
where
    W: Write + ?Sized,
    T: Serialize + ?Sized,
{
    let mut encoder = Encoder {
        sink: IoSink { writer, written: 0 },
    };
    value.serialize(&mut encoder)?;
    Ok(encoder.sink.written)
}

/// Returns the encoded size of `value` without allocating an output buffer.
pub fn encoded_len<T: Serialize + ?Sized>(value: &T) -> Result<usize, CodecError> {
    let mut encoder = Encoder {
        sink: Counter { len: 0 },
    };
    value.serialize(&mut encoder)?;
    Ok(encoder.sink.len)
}

/// Decodes one value from the start of `input` and returns it together with
/// the number of bytes consumed.
pub fn from_slice<'de, T: Deserialize<'de>>(input: &'de [u8]) -> Result<(T, usize), CodecError> {
    let mut decoder = Decoder::new(input);
    let value = decoder.decode()?;
    Ok((value, decoder.position()))
}

/// Decodes one value that must span all of `input`.
pub fn from_slice_exact<'de, T: Deserialize<'de>>(input: &'de [u8]) -> Result<T, CodecError> {
    let (value, consumed) = from_slice(input)?;
    if consumed == input.len() {
        Ok(value)
    } else {
        Err(CodecError::TrailingBytes {
            consumed,
            total: input.len(),
        })
    }
}

/// Serializes a vector after checking its field-specific logical item limit.
///
/// The codec applies its own general collection limit too; this adapter lets
/// wire fields state a tighter rule without changing their in-memory `Vec`
/// type.
pub fn serialize_bounded_vec<const MAX: usize, T, S>(
    values: &Vec<T>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: ser::Serializer,
{
    use ser::SerializeSeq as _;

    if values.len() > MAX {
        return Err(<S::Error as ser::Error>::custom(format!(
            "collection length {} exceeds the item limit {MAX}",
            values.len()
        )));
    }
    let mut sequence = serializer.serialize_seq(Some(values.len()))?;
    for value in values {
        sequence.serialize_element(value)?;
    }
    sequence.end()
}

/// Deserializes a vector after checking its field-specific logical item limit
/// before reserving or decoding the announced items.
pub fn deserialize_bounded_vec<'de, const MAX: usize, T, D>(
    deserializer: D,
) -> Result<Vec<T>, D::Error>
where
    T: Deserialize<'de>,
    D: de::Deserializer<'de>,
{
    struct BoundedVecVisitor<T, const MAX: usize>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for BoundedVecVisitor<T, MAX> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a sequence with at most {MAX} items")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Vec<T>, A::Error> {
            if let Some(len) = sequence.size_hint()
                && len > MAX
            {
                return Err(<A::Error as de::Error>::custom(format!(
                    "collection length {len} exceeds the item limit {MAX}"
                )));
            }
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX));
            while let Some(value) = sequence.next_element()? {
                if values.len() == MAX {
                    return Err(<A::Error as de::Error>::custom(format!(
                        "collection length exceeds the item limit {MAX}"
                    )));
                }
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedVecVisitor::<T, MAX>(std::marker::PhantomData))
}

// ---------------------------------------------------------------------------
// Integer helpers
// ---------------------------------------------------------------------------

fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)).cast_unsigned()
}

fn zigzag_decode(value: u64) -> i64 {
    (value >> 1).cast_signed() ^ (value & 1).cast_signed().wrapping_neg()
}

fn zigzag_encode128(value: i128) -> u128 {
    ((value << 1) ^ (value >> 127)).cast_unsigned()
}

fn zigzag_decode128(value: u128) -> i128 {
    (value >> 1).cast_signed() ^ (value & 1).cast_signed().wrapping_neg()
}

fn len_to_u64(len: usize) -> Result<u64, CodecError> {
    u64::try_from(len).map_err(|_| CodecError::SizeOverflow)
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

trait Sink {
    fn put(&mut self, bytes: &[u8]) -> Result<(), CodecError>;
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.extend_from_slice(bytes);
        Ok(())
    }
}

struct Counter {
    len: usize,
}

impl Sink for Counter {
    fn put(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.len = self
            .len
            .checked_add(bytes.len())
            .ok_or(CodecError::SizeOverflow)?;
        Ok(())
    }
}

struct IoSink<'w, W: ?Sized> {
    writer: &'w mut W,
    written: usize,
}

impl<W: Write + ?Sized> Sink for IoSink<'_, W> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.writer
            .write_all(bytes)
            .map_err(|error| CodecError::Io(error.to_string()))?;
        self.written = self
            .written
            .checked_add(bytes.len())
            .ok_or(CodecError::SizeOverflow)?;
        Ok(())
    }
}

struct Encoder<S> {
    sink: S,
}

impl<S: Sink> Encoder<S> {
    fn put_u8(&mut self, value: u8) -> Result<(), CodecError> {
        self.sink.put(&[value])
    }

    fn put_varint(&mut self, mut value: u64) -> Result<(), CodecError> {
        if value < 0x80 {
            return self.sink.put(&[value.to_le_bytes()[0]]);
        }
        let mut buf = [0u8; MAX_VARINT_U64_BYTES];
        let mut len = 0;
        loop {
            let low = value.to_le_bytes()[0] & 0x7f;
            value >>= 7;
            if value == 0 {
                buf[len] = low;
                len += 1;
                break;
            }
            buf[len] = low | 0x80;
            len += 1;
        }
        self.sink.put(&buf[..len])
    }

    fn put_varint128(&mut self, mut value: u128) -> Result<(), CodecError> {
        let mut buf = [0u8; MAX_VARINT_U128_BYTES];
        let mut len = 0;
        loop {
            let low = value.to_le_bytes()[0] & 0x7f;
            value >>= 7;
            if value == 0 {
                buf[len] = low;
                len += 1;
                break;
            }
            buf[len] = low | 0x80;
            len += 1;
        }
        self.sink.put(&buf[..len])
    }

    fn put_len(&mut self, len: usize) -> Result<(), CodecError> {
        self.put_varint(len_to_u64(len)?)
    }

    fn put_collection_len(&mut self, len: usize) -> Result<(), CodecError> {
        if len > MAX_COLLECTION_ITEMS {
            return Err(CodecError::CollectionLimitExceeded {
                len: len_to_u64(len)?,
                max: MAX_COLLECTION_ITEMS,
            });
        }
        self.put_len(len)
    }
}

impl<S: Sink> ser::Serializer for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, v: bool) -> Result<(), CodecError> {
        self.put_u8(u8::from(v))
    }

    fn serialize_i8(self, v: i8) -> Result<(), CodecError> {
        self.put_varint(zigzag_encode(i64::from(v)))
    }

    fn serialize_i16(self, v: i16) -> Result<(), CodecError> {
        self.put_varint(zigzag_encode(i64::from(v)))
    }

    fn serialize_i32(self, v: i32) -> Result<(), CodecError> {
        self.put_varint(zigzag_encode(i64::from(v)))
    }

    fn serialize_i64(self, v: i64) -> Result<(), CodecError> {
        self.put_varint(zigzag_encode(v))
    }

    fn serialize_i128(self, v: i128) -> Result<(), CodecError> {
        self.put_varint128(zigzag_encode128(v))
    }

    fn serialize_u8(self, v: u8) -> Result<(), CodecError> {
        self.put_u8(v)
    }

    fn serialize_u16(self, v: u16) -> Result<(), CodecError> {
        self.put_varint(u64::from(v))
    }

    fn serialize_u32(self, v: u32) -> Result<(), CodecError> {
        self.put_varint(u64::from(v))
    }

    fn serialize_u64(self, v: u64) -> Result<(), CodecError> {
        self.put_varint(v)
    }

    fn serialize_u128(self, v: u128) -> Result<(), CodecError> {
        self.put_varint128(v)
    }

    fn serialize_f32(self, v: f32) -> Result<(), CodecError> {
        self.sink.put(&v.to_le_bytes())
    }

    fn serialize_f64(self, v: f64) -> Result<(), CodecError> {
        self.sink.put(&v.to_le_bytes())
    }

    fn serialize_char(self, v: char) -> Result<(), CodecError> {
        self.put_varint(u64::from(u32::from(v)))
    }

    fn serialize_str(self, v: &str) -> Result<(), CodecError> {
        self.put_len(v.len())?;
        self.sink.put(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), CodecError> {
        self.put_len(v.len())?;
        self.sink.put(v)
    }

    fn serialize_none(self) -> Result<(), CodecError> {
        self.put_u8(0)
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), CodecError> {
        self.put_u8(1)?;
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), CodecError> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), CodecError> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
    ) -> Result<(), CodecError> {
        self.put_varint(u64::from(variant_index))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), CodecError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<(), CodecError> {
        self.put_varint(u64::from(variant_index))?;
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self, CodecError> {
        self.put_collection_len(len.ok_or(CodecError::UnknownLength)?)?;
        Ok(self)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self, CodecError> {
        Ok(self)
    }

    fn serialize_tuple_struct(self, _name: &'static str, _len: usize) -> Result<Self, CodecError> {
        Ok(self)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self, CodecError> {
        self.put_varint(u64::from(variant_index))?;
        Ok(self)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self, CodecError> {
        self.put_collection_len(len.ok_or(CodecError::UnknownLength)?)?;
        Ok(self)
    }

    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self, CodecError> {
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self, CodecError> {
        self.put_varint(u64::from(variant_index))?;
        Ok(self)
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

impl<S: Sink> ser::SerializeSeq for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), CodecError> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), CodecError> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeTuple for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), CodecError> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), CodecError> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeTupleStruct for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), CodecError> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), CodecError> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeTupleVariant for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), CodecError> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), CodecError> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeMap for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), CodecError> {
        key.serialize(&mut **self)
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), CodecError> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), CodecError> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeStruct for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), CodecError> {
        value.serialize(&mut **self)
    }

    fn skip_field(&mut self, _key: &'static str) -> Result<(), CodecError> {
        Err(CodecError::SkippedField)
    }

    fn end(self) -> Result<(), CodecError> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeStructVariant for &mut Encoder<S> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), CodecError> {
        value.serialize(&mut **self)
    }

    fn skip_field(&mut self, _key: &'static str) -> Result<(), CodecError> {
        Err(CodecError::SkippedField)
    }

    fn end(self) -> Result<(), CodecError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Positional decoder over a borrowed input buffer.
///
/// Besides implementing `serde::Deserializer`, it exposes position and finish
/// checks for framed values.
pub struct Decoder<'de> {
    input: &'de [u8],
    pos: usize,
    depth: usize,
    max_depth: usize,
}

impl<'de> Decoder<'de> {
    pub fn new(input: &'de [u8]) -> Self {
        Self {
            input,
            pos: 0,
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    /// Number of input bytes consumed so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.pos)
    }

    /// Fails with `TrailingBytes` unless all input has been consumed.
    pub fn finish(&self) -> Result<(), CodecError> {
        if self.pos == self.input.len() {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes {
                consumed: self.pos,
                total: self.input.len(),
            })
        }
    }

    /// Decodes the next value through its `Deserialize` implementation.
    pub fn decode<T: Deserialize<'de>>(&mut self) -> Result<T, CodecError> {
        T::deserialize(&mut *self)
    }

    /// Reads a raw length prefix and verifies it fits in the remaining input.
    fn read_len(&mut self) -> Result<usize, CodecError> {
        let len = self.read_varint()?;
        let remaining = self.remaining();
        match usize::try_from(len) {
            Ok(len) if len <= remaining => Ok(len),
            _ => Err(CodecError::LengthExceedsInput { len, remaining }),
        }
    }

    fn read_collection_len(&mut self) -> Result<usize, CodecError> {
        let len = self.read_len()?;
        if len > MAX_COLLECTION_ITEMS {
            return Err(CodecError::CollectionLimitExceeded {
                len: u64::try_from(len).map_err(|_| CodecError::SizeOverflow)?,
                max: MAX_COLLECTION_ITEMS,
            });
        }
        Ok(len)
    }

    fn take(&mut self, len: usize) -> Result<&'de [u8], CodecError> {
        let input: &'de [u8] = self.input;
        let remaining = self.remaining();
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= input.len())
            .ok_or(CodecError::UnexpectedEof {
                needed: len,
                remaining,
            })?;
        let bytes = input.get(self.pos..end).ok_or(CodecError::UnexpectedEof {
            needed: len,
            remaining,
        })?;
        self.pos = end;
        Ok(bytes)
    }

    fn read_byte(&mut self) -> Result<u8, CodecError> {
        let byte = *self.input.get(self.pos).ok_or(CodecError::UnexpectedEof {
            needed: 1,
            remaining: 0,
        })?;
        self.pos += 1;
        Ok(byte)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let remaining = self.remaining();
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| CodecError::UnexpectedEof {
            needed: N,
            remaining,
        })
    }

    fn read_varint(&mut self) -> Result<u64, CodecError> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.read_byte()?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return Err(CodecError::VarintOverflow);
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(CodecError::OverlongVarint);
                }
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(CodecError::VarintOverflow);
            }
        }
    }

    fn read_varint128(&mut self) -> Result<u128, CodecError> {
        let mut value = 0u128;
        let mut shift = 0u32;
        loop {
            let byte = self.read_byte()?;
            let low = u128::from(byte & 0x7f);
            if shift == 126 && low > 3 {
                return Err(CodecError::VarintOverflow);
            }
            value |= low << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(CodecError::OverlongVarint);
                }
                return Ok(value);
            }
            shift += 7;
            if shift > 126 {
                return Err(CodecError::VarintOverflow);
            }
        }
    }

    fn read_signed(&mut self) -> Result<i64, CodecError> {
        Ok(zigzag_decode(self.read_varint()?))
    }

    fn read_str(&mut self) -> Result<&'de str, CodecError> {
        let len = self.read_len()?;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes).map_err(|_| CodecError::InvalidUtf8)
    }

    /// Runs `f` one nesting level deeper, failing past the depth limit.
    fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, CodecError>,
    ) -> Result<T, CodecError> {
        if self.depth >= self.max_depth {
            return Err(CodecError::DepthLimitExceeded);
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }
}

impl<'de> de::Deserializer<'de> for &mut Decoder<'de> {
    type Error = CodecError;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, CodecError> {
        Err(CodecError::NotSelfDescribing("deserialize_any"))
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        match self.read_byte()? {
            0 => visitor.visit_bool(false),
            1 => visitor.visit_bool(true),
            byte => Err(CodecError::InvalidBool(byte)),
        }
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor
            .visit_i8(i8::try_from(self.read_signed()?).map_err(|_| CodecError::IntegerOutOfRange)?)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_i16(
            i16::try_from(self.read_signed()?).map_err(|_| CodecError::IntegerOutOfRange)?,
        )
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_i32(
            i32::try_from(self.read_signed()?).map_err(|_| CodecError::IntegerOutOfRange)?,
        )
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_i64(self.read_signed()?)
    }

    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_i128(zigzag_decode128(self.read_varint128()?))
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_u8(self.read_byte()?)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_u16(
            u16::try_from(self.read_varint()?).map_err(|_| CodecError::IntegerOutOfRange)?,
        )
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_u32(
            u32::try_from(self.read_varint()?).map_err(|_| CodecError::IntegerOutOfRange)?,
        )
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_u64(self.read_varint()?)
    }

    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_u128(self.read_varint128()?)
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_f32(f32::from_le_bytes(self.read_array::<4>()?))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_f64(f64::from_le_bytes(self.read_array::<8>()?))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        let value =
            u32::try_from(self.read_varint()?).map_err(|_| CodecError::IntegerOutOfRange)?;
        let ch = char::from_u32(value).ok_or(CodecError::InvalidChar(value))?;
        visitor.visit_char(ch)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_borrowed_str(self.read_str()?)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_borrowed_str(self.read_str()?)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        let len = self.read_len()?;
        visitor.visit_borrowed_bytes(self.take(len)?)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        let len = self.read_len()?;
        visitor.visit_borrowed_bytes(self.take(len)?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        match self.read_byte()? {
            0 => visitor.visit_none(),
            1 => self.nested(|decoder| visitor.visit_some(decoder)),
            tag => Err(CodecError::InvalidOptionTag(tag)),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| visitor.visit_newtype_struct(decoder))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        let len = self.read_collection_len()?;
        self.nested(|decoder| {
            visitor.visit_seq(Access {
                decoder,
                remaining: len,
            })
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| {
            visitor.visit_seq(Access {
                decoder,
                remaining: len,
            })
        })
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| {
            visitor.visit_seq(Access {
                decoder,
                remaining: len,
            })
        })
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CodecError> {
        let len = self.read_collection_len()?;
        self.nested(|decoder| {
            visitor.visit_map(Access {
                decoder,
                remaining: len,
            })
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| {
            visitor.visit_seq(Access {
                decoder,
                remaining: fields.len(),
            })
        })
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        self.nested(|decoder| visitor.visit_enum(decoder))
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, CodecError> {
        Err(CodecError::NotSelfDescribing("deserialize_identifier"))
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, CodecError> {
        Err(CodecError::NotSelfDescribing("deserialize_ignored_any"))
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

/// Sequence / map access over a known number of remaining items.
struct Access<'a, 'de> {
    decoder: &'a mut Decoder<'de>,
    remaining: usize,
}

impl<'de> de::SeqAccess<'de> for Access<'_, 'de> {
    type Error = CodecError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, CodecError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.decoder).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining)
    }
}

impl<'de> de::MapAccess<'de> for Access<'_, 'de> {
    type Error = CodecError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, CodecError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.decoder).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, CodecError> {
        seed.deserialize(&mut *self.decoder)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining)
    }
}

impl<'de> de::EnumAccess<'de> for &mut Decoder<'de> {
    type Error = CodecError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), CodecError> {
        let index =
            u32::try_from(self.read_varint()?).map_err(|_| CodecError::IntegerOutOfRange)?;
        let value = seed.deserialize(de::value::U32Deserializer::<CodecError>::new(index))?;
        Ok((value, self))
    }
}

impl<'de> de::VariantAccess<'de> for &mut Decoder<'de> {
    type Error = CodecError;

    fn unit_variant(self) -> Result<(), CodecError> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, CodecError> {
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        de::Deserializer::deserialize_tuple(self, len, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        de::Deserializer::deserialize_tuple(self, fields.len(), visitor)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Encodes `value` into a new buffer.
#[cfg(test)]
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder { sink: Vec::new() };
    value.serialize(&mut encoder)?;
    Ok(encoder.sink)
}

#[cfg(test)]
impl<'de> Decoder<'de> {
    pub fn with_max_depth(input: &'de [u8], max_depth: usize) -> Self {
        Self {
            max_depth,
            ..Self::new(input)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::de::DeserializeOwned;
    use serde::{Deserialize, Serialize, Serializer};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn roundtrip<T: Serialize + DeserializeOwned>(value: &T) -> Result<T, CodecError> {
        from_slice_exact(&to_vec(value)?)
    }

    fn decode_err<T: DeserializeOwned + std::fmt::Debug>(
        bytes: &[u8],
    ) -> Result<CodecError, String> {
        match from_slice_exact::<T>(bytes) {
            Ok(value) => Err(format!("expected an error, decoded {value:?}")),
            Err(error) => Ok(error),
        }
    }

    #[test]
    fn varint_edges_encode_canonically() -> TestResult {
        assert_eq!(to_vec(&0u64)?, [0x00]);
        assert_eq!(to_vec(&127u64)?, [0x7f]);
        assert_eq!(to_vec(&128u64)?, [0x80, 0x01]);
        assert_eq!(to_vec(&300u32)?, [0xac, 0x02]);
        assert_eq!(to_vec(&u16::MAX)?, [0xff, 0xff, 0x03]);
        assert_eq!(
            to_vec(&u64::MAX)?,
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
        for value in [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            assert_eq!(roundtrip(&value)?, value);
        }
        assert_eq!(roundtrip(&usize::MAX)?, usize::MAX);
        Ok(())
    }

    #[test]
    fn varint_rejects_overlong_overflow_and_truncation() -> TestResult {
        assert_eq!(
            decode_err::<u64>(&[0x80, 0x00])?,
            CodecError::OverlongVarint
        );
        assert_eq!(
            decode_err::<u64>(&[0x81, 0x80, 0x00])?,
            CodecError::OverlongVarint
        );
        let mut ten = [0xffu8; 10];
        ten[9] = 0x02;
        assert_eq!(decode_err::<u64>(&ten)?, CodecError::VarintOverflow);
        assert_eq!(decode_err::<u64>(&[0xff; 11])?, CodecError::VarintOverflow);
        assert!(matches!(
            decode_err::<u64>(&[0x80])?,
            CodecError::UnexpectedEof { .. }
        ));
        assert!(matches!(
            decode_err::<u32>(&[])?,
            CodecError::UnexpectedEof { .. }
        ));
        assert_eq!(
            decode_err::<u16>(&to_vec(&70_000u32)?)?,
            CodecError::IntegerOutOfRange
        );
        assert_eq!(
            decode_err::<u32>(&to_vec(&(u64::from(u32::MAX) + 1))?)?,
            CodecError::IntegerOutOfRange
        );
        Ok(())
    }

    #[test]
    fn zigzag_signed_integers() -> TestResult {
        assert_eq!(to_vec(&0i64)?, [0]);
        assert_eq!(to_vec(&-1i64)?, [1]);
        assert_eq!(to_vec(&1i64)?, [2]);
        assert_eq!(to_vec(&-2i64)?, [3]);
        assert_eq!(to_vec(&-64i32)?, [0x7f]);
        assert_eq!(to_vec(&64i32)?, [0x80, 0x01]);
        for value in [0, 1, -1, i64::MIN, i64::MAX, 1 << 40, -(1 << 40)] {
            assert_eq!(roundtrip(&value)?, value);
        }
        for value in [i8::MIN, -1, 0, 1, i8::MAX] {
            assert_eq!(roundtrip(&value)?, value);
        }
        for value in [i16::MIN, i16::MAX] {
            assert_eq!(roundtrip(&value)?, value);
        }
        for value in [i32::MIN, i32::MAX] {
            assert_eq!(roundtrip(&value)?, value);
        }
        assert_eq!(
            decode_err::<i8>(&to_vec(&200i64)?)?,
            CodecError::IntegerOutOfRange
        );
        Ok(())
    }

    #[test]
    fn wide_integers_roundtrip() -> TestResult {
        for value in [0u128, 1, u128::from(u64::MAX) + 1, u128::MAX] {
            assert_eq!(roundtrip(&value)?, value);
        }
        for value in [0i128, -1, i128::MIN, i128::MAX] {
            assert_eq!(roundtrip(&value)?, value);
        }
        assert_eq!(to_vec(&u128::MAX)?.len(), MAX_VARINT_U128_BYTES);
        assert_eq!(decode_err::<u128>(&[0xff; 20])?, CodecError::VarintOverflow);
        Ok(())
    }

    #[test]
    fn fixed_width_primitives() -> TestResult {
        assert_eq!(to_vec(&0xabu8)?, [0xab]);
        assert_eq!(to_vec(&true)?, [1]);
        assert_eq!(to_vec(&false)?, [0]);
        assert!(roundtrip(&true)?);
        assert_eq!(decode_err::<bool>(&[2])?, CodecError::InvalidBool(2));

        assert_eq!(to_vec(&1.5f32)?, 1.5f32.to_le_bytes());
        assert_eq!(to_vec(&-2.25f64)?, (-2.25f64).to_le_bytes());
        assert_eq!(roundtrip(&1.5f32)?.to_bits(), 1.5f32.to_bits());
        assert_eq!(
            roundtrip(&f64::MIN_POSITIVE)?.to_bits(),
            f64::MIN_POSITIVE.to_bits()
        );
        assert_eq!(roundtrip(&f64::NAN)?.to_bits(), f64::NAN.to_bits());
        assert!(matches!(
            decode_err::<f64>(&[0; 7])?,
            CodecError::UnexpectedEof { .. }
        ));
        Ok(())
    }

    #[test]
    fn chars_are_validated_scalar_values() -> TestResult {
        assert_eq!(to_vec(&'a')?, [0x61]);
        for ch in ['a', 'é', '→', '\u{1F980}', char::MAX] {
            assert_eq!(roundtrip(&ch)?, ch);
        }
        assert_eq!(
            decode_err::<char>(&to_vec(&0xD800u32)?)?,
            CodecError::InvalidChar(0xD800)
        );
        Ok(())
    }

    #[test]
    fn strings_and_bytes() -> TestResult {
        assert_eq!(to_vec("hi")?, [2, b'h', b'i']);
        let text = "héllo \u{1F980}".to_owned();
        assert_eq!(roundtrip(&text)?, text);
        assert_eq!(roundtrip(&String::new())?, "");

        let encoded = to_vec(&text)?;
        let borrowed: &str = from_slice_exact(&encoded)?;
        assert_eq!(borrowed, text);

        let bytes = vec![1u8, 2, 3, 255];
        let encoded = to_vec(&bytes)?;
        assert_eq!(encoded, [4, 1, 2, 3, 255]);
        let borrowed: &[u8] = from_slice_exact(&encoded)?;
        assert_eq!(borrowed, bytes.as_slice());
        assert_eq!(roundtrip(&bytes)?, bytes);

        assert_eq!(
            decode_err::<String>(&[2, 0xff, 0xfe])?,
            CodecError::InvalidUtf8
        );
        assert!(matches!(
            decode_err::<String>(&[0xff, 0xff, 0x03, b'a'])?,
            CodecError::LengthExceedsInput { .. }
        ));
        Ok(())
    }

    #[test]
    fn options_use_a_tag_byte() -> TestResult {
        assert_eq!(to_vec(&None::<u8>)?, [0]);
        assert_eq!(to_vec(&Some(5u8))?, [1, 5]);
        assert_eq!(roundtrip(&Some(Some(3u32)))?, Some(Some(3)));
        assert_eq!(roundtrip(&Some(None::<u32>))?, Some(None));
        assert_eq!(
            decode_err::<Option<u8>>(&[2, 0])?,
            CodecError::InvalidOptionTag(2)
        );
        Ok(())
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Unit;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Wrapper(u32);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Pair(u8, String);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Shape {
        Empty,
        Circle(u32),
        Line(i16, String),
        Nested {
            label: String,
            inner: Option<Box<Shape>>,
        },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Outer {
        id: u64,
        unit: (),
        marker: Unit,
        wrapper: Wrapper,
        pair: Pair,
        tuple: (u8, bool, char),
        shapes: Vec<Shape>,
        map: BTreeMap<String, u32>,
        #[serde(default)]
        defaulted: u16,
    }

    fn sample_outer() -> Outer {
        Outer {
            id: 42,
            unit: (),
            marker: Unit,
            wrapper: Wrapper(7),
            pair: Pair(9, "pair".into()),
            tuple: (1, true, 'x'),
            shapes: vec![
                Shape::Empty,
                Shape::Circle(300),
                Shape::Line(-5, "line".into()),
                Shape::Nested {
                    label: "outer".into(),
                    inner: Some(Box::new(Shape::Nested {
                        label: "inner".into(),
                        inner: None,
                    })),
                },
            ],
            map: BTreeMap::from([("a".to_owned(), 1), ("b".to_owned(), 2)]),
            defaulted: 11,
        }
    }

    #[test]
    fn nested_structs_and_enums_roundtrip() -> TestResult {
        let value = sample_outer();
        assert_eq!(roundtrip(&value)?, value);
        Ok(())
    }

    #[test]
    fn enum_and_struct_layout_is_positional() -> TestResult {
        assert_eq!(to_vec(&Shape::Empty)?, [0]);
        assert_eq!(to_vec(&Shape::Circle(5))?, [1, 5]);
        assert_eq!(to_vec(&Shape::Line(-1, "a".into()))?, [2, 1, 1, b'a']);
        assert_eq!(
            to_vec(&Shape::Nested {
                label: "z".into(),
                inner: None
            })?,
            [3, 1, b'z', 0]
        );
        assert_eq!(to_vec(&Unit)?, Vec::<u8>::new());
        assert_eq!(to_vec(&Wrapper(3))?, [3]);
        assert_eq!(to_vec(&Pair(1, "b".into()))?, [1, 1, b'b']);
        assert!(matches!(decode_err::<Shape>(&[9])?, CodecError::Message(_)));
        Ok(())
    }

    #[test]
    fn length_prefixes_are_bounded_by_the_input() -> TestResult {
        let mut bomb = to_vec(&(1u64 << 40))?;
        bomb.extend_from_slice(&[0, 0, 0]);
        assert!(matches!(
            decode_err::<Vec<u32>>(&bomb)?,
            CodecError::LengthExceedsInput { .. }
        ));
        assert!(matches!(
            decode_err::<BTreeMap<u8, u8>>(&bomb)?,
            CodecError::LengthExceedsInput { .. }
        ));
        let max = to_vec(&u64::MAX)?;
        assert!(matches!(
            decode_err::<Vec<u8>>(&max)?,
            CodecError::LengthExceedsInput { .. }
        ));
        // A count that fits the input but whose elements run out.
        assert!(matches!(
            decode_err::<Vec<u32>>(&[2, 0x80, 0x80])?,
            CodecError::UnexpectedEof { .. }
        ));
        Ok(())
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct FieldBounded {
        #[serde(
            serialize_with = "super::serialize_bounded_vec::<2, _, _>",
            deserialize_with = "super::deserialize_bounded_vec::<2, _, _>"
        )]
        values: Vec<u8>,
    }

    #[test]
    fn codec_logical_item_limits_apply_before_sequence_and_map_allocation() -> TestResult {
        let over_limit = MAX_COLLECTION_ITEMS + 1;
        let mut encoded_vec = to_vec(&u64::try_from(over_limit)?)?;
        encoded_vec.resize(encoded_vec.len() + over_limit, 0);
        assert!(matches!(
            from_slice_exact::<Vec<()>>(&encoded_vec),
            Err(CodecError::CollectionLimitExceeded { max, .. })
                if max == MAX_COLLECTION_ITEMS
        ));
        assert!(matches!(
            to_vec(&vec![(); over_limit]),
            Err(CodecError::CollectionLimitExceeded { max, .. })
                if max == MAX_COLLECTION_ITEMS
        ));

        let mut encoded_map = to_vec(&u64::try_from(over_limit)?)?;
        encoded_map.resize(encoded_map.len() + over_limit * 2, 0);
        assert!(matches!(
            from_slice_exact::<BTreeMap<u8, u8>>(&encoded_map),
            Err(CodecError::CollectionLimitExceeded { max, .. })
                if max == MAX_COLLECTION_ITEMS
        ));

        let oversized_field = FieldBounded {
            values: vec![1, 2, 3],
        };
        assert!(matches!(
            to_vec(&oversized_field),
            Err(CodecError::Message(message)) if message.contains("item limit 2")
        ));
        assert!(matches!(
            from_slice_exact::<FieldBounded>(&[3, 1, 2, 3]),
            Err(CodecError::Message(message)) if message.contains("item limit 2")
        ));
        Ok(())
    }

    #[test]
    fn depth_limit_rejects_deep_nesting() -> TestResult {
        let mut shape = Shape::Empty;
        for depth in 0..20 {
            shape = Shape::Nested {
                label: depth.to_string(),
                inner: Some(Box::new(shape)),
            };
        }
        let encoded = to_vec(&shape)?;
        assert_eq!(roundtrip(&shape)?, shape);

        let mut shallow = Decoder::with_max_depth(&encoded, 8);
        assert_eq!(
            shallow.decode::<Shape>().err(),
            Some(CodecError::DepthLimitExceeded)
        );
        let mut exact = Decoder::with_max_depth(&encoded, DEFAULT_MAX_DEPTH);
        assert_eq!(exact.decode::<Shape>()?, shape);
        exact.finish()?;
        Ok(())
    }

    #[test]
    fn trailing_bytes_are_reported() -> TestResult {
        assert_eq!(from_slice::<u8>(&[1, 2])?, (1, 1));
        assert_eq!(
            decode_err::<u8>(&[1, 2])?,
            CodecError::TrailingBytes {
                consumed: 1,
                total: 2
            }
        );
        let mut decoder = Decoder::new(&[5, 6]);
        assert_eq!(decoder.decode::<u8>()?, 5);
        assert!(decoder.finish().is_err());
        assert_eq!(decoder.position(), 1);
        Ok(())
    }

    struct UnknownLength;

    impl Serialize for UnknownLength {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq as _;
            serializer.serialize_seq(None)?.end()
        }
    }

    #[derive(Serialize)]
    struct Skipping {
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<u8>,
    }

    #[test]
    fn unsupported_shapes_are_rejected() -> TestResult {
        assert_eq!(
            to_vec(&UnknownLength).err(),
            Some(CodecError::UnknownLength)
        );
        assert_eq!(
            to_vec(&Skipping { value: None }).err(),
            Some(CodecError::SkippedField)
        );
        assert_eq!(to_vec(&Skipping { value: Some(1) })?, [1, 1]);
        assert!(matches!(
            from_slice_exact::<serde::de::IgnoredAny>(&[0]).err(),
            Some(CodecError::NotSelfDescribing(_))
        ));
        Ok(())
    }

    #[test]
    fn measuring_and_streaming_match_to_vec() -> TestResult {
        let value = sample_outer();
        let bytes = to_vec(&value)?;
        assert_eq!(encoded_len(&value)?, bytes.len());
        let mut streamed = vec![0xaa];
        let written = encode_into(&mut streamed, &value)?;
        assert_eq!(written, bytes.len());
        assert_eq!(streamed.get(1..), Some(bytes.as_slice()));
        Ok(())
    }
}
