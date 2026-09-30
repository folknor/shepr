//! Typed reads from the `ArgMatches` that `spec.rs` produces. Clap's lookup
//! errors mean the spec and handler disagree. The infallible helpers retain
//! their default-on-error behavior for callers that need it; typed command
//! parsers use the fallible helpers so a mismatch rejects the command instead
//! of changing an option's meaning.

use clap::ArgMatches;

pub(super) fn try_value<T: Clone + Send + Sync + 'static>(
    matches: &ArgMatches,
    id: &str,
) -> Result<Option<T>, String> {
    matches
        .try_get_one::<T>(id)
        .map(Option::<&T>::cloned)
        .map_err(|error| error.to_string())
}

pub(super) fn value<T: Clone + Send + Sync + 'static>(matches: &ArgMatches, id: &str) -> Option<T> {
    try_value(matches, id).ok().flatten()
}

pub(super) fn try_string(matches: &ArgMatches, id: &str) -> Result<Option<String>, String> {
    try_value::<String>(matches, id)
}

pub(super) fn try_flag(matches: &ArgMatches, id: &str) -> Result<bool, String> {
    try_value::<bool>(matches, id).map(|value| value.unwrap_or(false))
}

pub(super) fn flag(matches: &ArgMatches, id: &str) -> bool {
    value::<bool>(matches, id).unwrap_or(false)
}

#[cfg(test)]
pub(super) fn string(matches: &ArgMatches, id: &str) -> Option<String> {
    value::<String>(matches, id)
}

#[cfg(test)]
mod tests {
    use super::string;
    use clap::Command;

    #[test]
    fn missing_match_is_not_fabricated_as_an_empty_value() {
        let matches = Command::new("test")
            .try_get_matches_from(["test"])
            .expect("test precondition");
        assert_eq!(string(&matches, "missing"), None);
    }
}
