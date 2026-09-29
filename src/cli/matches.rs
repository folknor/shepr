//! Typed reads from the `ArgMatches` that `spec.rs` produces. Presence
//! (`required`) and value types (value parsers) are enforced by the spec; these
//! helpers use clap's non-panicking lookups so a spec/handler mismatch is
//! rejected rather than turned into a valid-looking empty value.

use clap::ArgMatches;

pub(super) fn value<T: Clone + Send + Sync + 'static>(matches: &ArgMatches, id: &str) -> Option<T> {
    matches.try_get_one::<T>(id).ok().flatten().cloned()
}

pub(super) fn string(matches: &ArgMatches, id: &str) -> Option<String> {
    value::<String>(matches, id)
}

/// A value the spec marks as required; clap has already rejected argv
/// without it. `None` means the typed handler and spec disagree.
pub(super) fn required(matches: &ArgMatches, id: &str) -> Option<String> {
    string(matches, id)
}

pub(super) fn flag(matches: &ArgMatches, id: &str) -> bool {
    value::<bool>(matches, id).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::required;
    use clap::Command;

    #[test]
    fn missing_required_match_is_not_fabricated_as_an_empty_value() {
        let matches = Command::new("test")
            .try_get_matches_from(["test"])
            .expect("test precondition");
        assert_eq!(required(&matches, "missing"), None);
    }
}
