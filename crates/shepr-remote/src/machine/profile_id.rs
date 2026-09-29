use std::fmt;
use std::io;

use serde::{Deserialize, Deserializer, Serialize};

const PROFILE_ID_BYTES: usize = 16;
/// Hex characters `ProfileId::short` keeps. A character count, unlike
/// `PROFILE_ID_BYTES`, which counts the bytes the full hex id encodes.
const SHORT_ID_HEX_CHARS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileIdError {
    InvalidFormat,
}

impl fmt::Display for ProfileIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat => formatter
                .write_str("endpoint profile id must be 32 lowercase hexadecimal characters"),
        }
    }
}

impl std::error::Error for ProfileIdError {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn parse(value: impl Into<String>) -> Result<Self, ProfileIdError> {
        let value = value.into();
        if value.len() != PROFILE_ID_BYTES * 2
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ProfileIdError::InvalidFormat);
        }
        Ok(Self(value))
    }

    pub fn generate() -> io::Result<Self> {
        let high = shepr_platform::unpredictable_token()?;
        let low = shepr_platform::unpredictable_token()?;
        Ok(Self(format!("{high:016x}{low:016x}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The leading hex characters of this id, for names that must stay short
    /// (socket paths). `parse` and `generate` guarantee the id is longer.
    pub fn short(&self) -> &str {
        &self.0[..SHORT_ID_HEX_CHARS]
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_ids_are_opaque_and_stable_when_parsed() {
        let first = ProfileId::generate().expect("kernel random source is available");
        let second = ProfileId::generate().expect("kernel random source is available");
        assert_ne!(first, second);
        assert_eq!(
            ProfileId::parse(first.to_string()).expect("test precondition"),
            first
        );
        assert_eq!(first.as_str().len(), 32);
        let known = ProfileId::parse("0123456789abcdef0123456789abcdef").expect("valid id");
        assert_eq!(known.short(), "0123456789abcdef");
    }

    #[test]
    fn profile_id_deserialization_preserves_the_type_invariant() {
        assert_eq!(
            ProfileId::parse("not-a-profile-id"),
            Err(ProfileIdError::InvalidFormat)
        );
        assert!(serde_json::from_str::<ProfileId>("\"not-a-profile-id\"").is_err());
    }
}
