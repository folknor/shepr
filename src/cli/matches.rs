//! Typed reads from the `ArgMatches` that `spec.rs` produces. Clap's lookup
//! errors mean the spec and handler disagree. Root help and version flags keep
//! their default-on-error read; typed command parsers use fallible helpers so a
//! mismatch rejects the command instead of changing an option's meaning.

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

pub(super) fn try_string(matches: &ArgMatches, id: &str) -> Result<Option<String>, String> {
    try_value::<String>(matches, id)
}

pub(super) fn try_flag(matches: &ArgMatches, id: &str) -> Result<bool, String> {
    try_value::<bool>(matches, id).map(|value| value.unwrap_or(false))
}

pub(super) fn flag(matches: &ArgMatches, id: &str) -> bool {
    try_flag(matches, id).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::try_string;
    use clap::{Arg, Command};

    #[test]
    fn missing_match_is_not_fabricated_as_an_empty_value() {
        let matches = Command::new("test")
            .arg(Arg::new("declared").long("declared"))
            .try_get_matches_from(["test"])
            .expect("test precondition");
        // A declared option that was not given is absent, not an empty value.
        assert_eq!(try_string(&matches, "declared"), Ok(None));
        // An id the spec never declared is a spec and handler mismatch, which
        // must surface instead of reading as an absent option.
        assert!(try_string(&matches, "missing").is_err());
    }
}
