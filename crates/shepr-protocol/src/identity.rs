use std::{fmt, ops::Deref, time::Duration};

/// Identifies one server lifetime in shell and surface messages.
///
/// A value comes from [`BootId::from_process_clock`], which the server calls
/// once when it constructs its lifetime and hands to everything that reports
/// the boot (the client shell lane, the API's `ping` and the
/// `server.stop_if_boot` guard), or from parsing text in exactly the form it
/// writes (`<pid>-<nanos>` or `<pid>-before-<nanos>`); deserialization goes
/// through the same parse. Clients only echo a boot id they were sent, so
/// there is no empty or placeholder boot id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct BootId {
    text: Box<str>,
    process_id: u32,
    before_epoch: bool,
    // Split in two words so the identity keeps 8-byte alignment: a 16-byte
    // aligned field would push every error that carries two boot ids past
    // clippy's large-error threshold.
    nanos: [u64; 2],
}

impl BootId {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The pid of the server process this boot identity names.
    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn clock_nanos(&self) -> u128 {
        (u128::from(self.nanos[0]) << 64) | u128::from(self.nanos[1])
    }

    pub fn is_before_epoch(&self) -> bool {
        self.before_epoch
    }

    /// The boot identity of process `process_id` whose clock read
    /// `since_epoch` (`Err` for a clock before the epoch, holding how far
    /// before). The server mints its boot with this; tests use it to build
    /// distinct canonical boot ids.
    pub fn from_process_clock(process_id: u32, since_epoch: Result<Duration, Duration>) -> Self {
        match since_epoch {
            Ok(duration) => Self::from_parts(process_id, false, duration.as_nanos()),
            Err(duration) => Self::from_parts(process_id, true, duration.as_nanos()),
        }
    }

    fn from_parts(process_id: u32, before_epoch: bool, nanos: u128) -> Self {
        let text = if before_epoch {
            format!("{process_id}-before-{nanos}")
        } else {
            format!("{process_id}-{nanos}")
        };
        Self {
            text: text.into_boxed_str(),
            process_id,
            before_epoch,
            nanos: [
                u64::try_from(nanos >> 64).unwrap_or_default(),
                u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or_default(),
            ],
        }
    }
}

/// Text that is not a boot id in the form [`BootId::from_process_clock`]
/// writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootIdParseError;

impl fmt::Display for BootIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid boot id")
    }
}

impl std::error::Error for BootIdParseError {}

impl std::str::FromStr for BootId {
    type Err = BootIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (process_id, time) = value.split_once('-').ok_or(BootIdParseError)?;
        let (before_epoch, nanos) = match time.strip_prefix("before-") {
            Some(nanos) => (true, nanos),
            None => (false, time),
        };
        let process_id = process_id.parse().map_err(|_| BootIdParseError)?;
        let nanos = nanos.parse().map_err(|_| BootIdParseError)?;
        // The number parsers accept signs and leading zeros; re-encoding and
        // comparing refuses every spelling `from_process_clock` never writes.
        Some(Self::from_parts(process_id, before_epoch, nanos))
            .filter(|id| &*id.text == value)
            .ok_or(BootIdParseError)
    }
}

impl TryFrom<String> for BootId {
    type Error = BootIdParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl serde::Serialize for BootId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl From<BootId> for String {
    fn from(value: BootId) -> Self {
        value.text.into_string()
    }
}

impl Deref for BootId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for BootId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for BootId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for BootId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for BootId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<BootId> for String {
    fn eq(&self, other: &BootId) -> bool {
        self == other.as_str()
    }
}

/// Correlates one client operation with its endpoint response.
///
/// Live clients allocate identities from one process-wide sequence. Request purpose
/// and view ownership live in the client's ledger and lanes, never in this value.
/// It travels as one number; a client allocates every identity it sends and a
/// server only echoes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RequestId(u64);

impl RequestId {
    /// Allocates a distinct identity across all request lanes in this client process.
    pub fn allocate() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        // The counter would wrap at u64::MAX, which no process reaches: at a
        // billion requests per second that takes centuries.
        let value = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(value)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// This crate's own tests spell boot ids as literals; the text must be
/// canonical. Other crates build them with `from_process_clock` or parse them.
#[cfg(test)]
impl From<&str> for BootId {
    fn from(value: &str) -> Self {
        value
            .parse()
            .unwrap_or_else(|error| panic!("{value:?}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::BootId;

    #[test]
    fn process_clock_before_epoch_keeps_its_offset() {
        let boot_id = BootId::from_process_clock(17, Err(Duration::from_nanos(23)));

        assert_eq!(boot_id.as_str(), "17-before-23");
    }

    #[test]
    fn process_clock_at_epoch_is_distinct_from_before_epoch() {
        let before_epoch = BootId::from_process_clock(17, Err(Duration::ZERO));
        let at_epoch = BootId::from_process_clock(17, Ok(Duration::ZERO));

        assert_eq!(before_epoch.as_str(), "17-before-0");
        assert_eq!(at_epoch.as_str(), "17-0");
        assert_ne!(before_epoch, at_epoch);
    }

    #[test]
    fn boot_ids_parse_back_and_keep_their_wire_encoding() {
        for boot_id in [
            BootId::from_process_clock(17, Err(Duration::from_nanos(23))),
            BootId::from_process_clock(0, Ok(Duration::ZERO)),
            BootId::from_process_clock(u32::MAX, Ok(Duration::MAX)),
        ] {
            assert_eq!(boot_id.as_str().parse::<BootId>(), Ok(boot_id.clone()));
            let wire = crate::codec::to_vec(&boot_id).expect("boot id encoding");
            assert_eq!(
                wire,
                crate::codec::to_vec(boot_id.as_str()).expect("boot id string encoding")
            );
            assert_eq!(
                crate::codec::from_slice_exact::<BootId>(&wire).expect("boot id decoding"),
                boot_id
            );
        }
    }

    #[test]
    fn boot_ids_refuse_every_spelling_the_process_clock_never_writes() {
        for invalid in [
            "",
            "boot",
            "boot-1",
            "remote-boot",
            "17",
            "17-",
            "-23",
            "17-before-",
            "17-before",
            "017-23",
            "17-023",
            "+17-23",
            "17-+23",
            "17-before-+23",
            "17-23-1",
            "4294967296-1",
            "17-before-before-1",
        ] {
            assert!(invalid.parse::<BootId>().is_err(), "{invalid:?}");
        }
        let wire = crate::codec::to_vec("boot").expect("string encoding");
        assert!(crate::codec::from_slice_exact::<BootId>(&wire).is_err());
    }
}
