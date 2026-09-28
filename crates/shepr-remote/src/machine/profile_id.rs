use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

const PROFILE_ID_BYTES: usize = 16;
/// Hex characters `ProfileId::short` keeps. A character count, unlike
/// `PROFILE_ID_BYTES`, which counts the bytes the full hex id encodes.
const SHORT_ID_HEX_CHARS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() != PROFILE_ID_BYTES * 2
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("endpoint profile id must be 32 lowercase hexadecimal characters".into());
        }
        Ok(Self(value))
    }

    pub fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        // Profile IDs identify catalog rows and may label bridge socket paths, but those paths
        // get a separate unpredictable token in the private runtime directory; the IDs need
        // practical uniqueness rather than secrecy.
        let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let digest = Sha256::digest(format!("{}:{now}:{sequence}", std::process::id()).as_bytes());
        Self(
            digest[..PROFILE_ID_BYTES]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
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
        let first = ProfileId::generate();
        let second = ProfileId::generate();
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
        assert!(serde_json::from_str::<ProfileId>("\"not-a-profile-id\"").is_err());
    }
}
