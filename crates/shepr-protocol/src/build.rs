//! Build identities at the cross-build text and preamble boundaries.

use std::{fmt, str::FromStr};

/// A fingerprint has eight bytes, encoded as sixteen lowercase hex digits.
/// An unidentifiable build matches no build, including itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum BuildIdentity {
    Known([u8; 8]),
    Unidentifiable,
}

impl BuildIdentity {
    pub fn matches(self, peer: Self) -> bool {
        matches!((self, peer), (Self::Known(ours), Self::Known(theirs)) if ours == theirs)
    }

    pub fn preamble_bytes(self) -> [u8; 16] {
        match self {
            Self::Unidentifiable => *b"unidentifiable--",
            Self::Known(bytes) => {
                let mut text = *b"unidentifiable--";
                let hex = b"0123456789abcdef";
                for (index, byte) in bytes.into_iter().enumerate() {
                    text[index * 2] = hex[usize::from(byte >> 4)];
                    text[index * 2 + 1] = hex[usize::from(byte & 15)];
                }
                text
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildIdentityParseError;

impl fmt::Display for BuildIdentityParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid build identity")
    }
}

impl std::error::Error for BuildIdentityParseError {}

impl FromStr for BuildIdentity {
    type Err = BuildIdentityParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "unidentifiable--" {
            return Ok(Self::Unidentifiable);
        }
        if value.len() != 16 {
            return Err(BuildIdentityParseError);
        }
        let digit = |byte| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(BuildIdentityParseError),
        };
        let mut bytes = [0; 8];
        let (pairs, _) = value.as_bytes().as_chunks::<2>();
        for (slot, [high, low]) in bytes.iter_mut().zip(pairs) {
            *slot = digit(*high)? * 16 + digit(*low)?;
        }
        Ok(Self::Known(bytes))
    }
}

impl fmt::Display for BuildIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.preamble_bytes() {
            write!(f, "{}", char::from(byte))?;
        }
        Ok(())
    }
}

/// The shared spelling printed after the executable name by both binaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildVersion {
    pub version: String,
    pub build_id: BuildIdentity,
}

impl FromStr for BuildVersion {
    type Err = BuildIdentityParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (version, build_id) = value.rsplit_once('+').ok_or(BuildIdentityParseError)?;
        if version.is_empty() || value.contains(char::is_whitespace) {
            return Err(BuildIdentityParseError);
        }
        Ok(Self {
            version: version.to_owned(),
            build_id: build_id.parse()?,
        })
    }
}

impl fmt::Display for BuildVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{}", self.version, self.build_id)
    }
}

impl TryFrom<String> for BuildIdentity {
    type Error = BuildIdentityParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<BuildIdentity> for String {
    fn from(value: BuildIdentity) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_fingerprints_round_trip_and_markers_never_match() {
        let known: BuildIdentity = "0123456789abcdef".parse().expect("fingerprint");
        assert_eq!(known.preamble_bytes(), *b"0123456789abcdef");
        assert_eq!(known.to_string(), "0123456789abcdef");
        assert!(known.matches(known));
        assert!(!BuildIdentity::Unidentifiable.matches(BuildIdentity::Unidentifiable));
        assert!(!known.matches(BuildIdentity::Unidentifiable));
        let wire = crate::codec::to_vec(&known).expect("encode");
        assert_eq!(
            wire,
            crate::codec::to_vec("0123456789abcdef").expect("text")
        );
        assert_eq!(
            crate::codec::from_slice_exact::<BuildIdentity>(&wire).expect("decode"),
            known
        );
        for invalid in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "0123456789abcdeF",
            "unknown",
        ] {
            assert!(invalid.parse::<BuildIdentity>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn version_lines_use_the_shared_build_spelling() {
        let text = "0.6.0+0123456789abcdef";
        let version: BuildVersion = text.parse().expect("version");
        assert_eq!(version.to_string(), text);
        assert!("0.6.0+garbled".parse::<BuildVersion>().is_err());
    }
}
