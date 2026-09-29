use std::{
    fmt,
    ops::Deref,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Identifies one server process lifetime in shell and surface messages.
///
/// A value comes from [`BootId::for_this_process`], from
/// [`BootId::from_process_clock`] (the same encoding with the process id and
/// clock passed in), or from parsing text in exactly the form those write
/// (`<pid>-<nanos>` or `<pid>-before-<nanos>`); deserialization goes through
/// the same parse. Clients only echo a boot id they were sent, so there is no
/// empty or placeholder boot id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct BootId(String);

impl BootId {
    /// The boot identity of this server process, built on first use and the
    /// same value on every later call, so every place that reports the boot
    /// (the client shell lane and the API's `ping`) reports one identity.
    pub fn for_this_process() -> Self {
        static THIS_PROCESS: OnceLock<BootId> = OnceLock::new();
        THIS_PROCESS
            .get_or_init(|| {
                let since_epoch = match SystemTime::now().duration_since(UNIX_EPOCH) {
                    Ok(duration) => Ok(duration),
                    Err(error) => Err(error.duration()),
                };
                Self::from_process_clock(std::process::id(), since_epoch)
            })
            .clone()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The boot identity of process `process_id` whose clock read
    /// `since_epoch` (`Err` for a clock before the epoch, holding how far
    /// before). Tests use it to build distinct canonical boot ids.
    pub fn from_process_clock(process_id: u32, since_epoch: Result<Duration, Duration>) -> Self {
        match since_epoch {
            Ok(duration) => Self::from_parts(process_id, false, duration.as_nanos()),
            Err(duration) => Self::from_parts(process_id, true, duration.as_nanos()),
        }
    }

    fn from_parts(process_id: u32, before_epoch: bool, nanos: u128) -> Self {
        if before_epoch {
            Self(format!("{process_id}-before-{nanos}"))
        } else {
            Self(format!("{process_id}-{nanos}"))
        }
    }
}

/// Text that is not a boot id in the form [`BootId::for_this_process`]
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
        // comparing refuses every spelling `for_this_process` never writes.
        Some(Self::from_parts(process_id, before_epoch, nanos))
            .filter(|id| id.0 == value)
            .ok_or(BootIdParseError)
    }
}

impl TryFrom<String> for BootId {
    type Error = BootIdParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
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

/// Correlates one client shell operation with its endpoint response.
///
/// Unlike the other identities this one is minted from any text, on purpose.
/// Its value is the `id` of a JSON API request, which the API client picks:
/// the shell client writes `client-shell...` forms, but the server answers
/// whatever id a request carried, so any string, empty included, is a
/// legitimate request id and there is no canonical form to check. It is only
/// ever compared for equality.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl Deref for RequestId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for RequestId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for RequestId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for RequestId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for RequestId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<RequestId> for String {
    fn eq(&self, other: &RequestId) -> bool {
        self == other.as_str()
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
    fn a_process_has_one_boot_id() {
        assert_eq!(BootId::for_this_process(), BootId::for_this_process());
    }

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
            BootId::for_this_process(),
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
