//! Typed reads from the `ArgMatches` that `spec.rs` produces. Presence
//! (`required`) and value types (value parsers) are enforced by the spec; these
//! helpers use clap's non-panicking lookups so a spec/handler mismatch shows up
//! as a missing value in tests rather than a panic in production.

use clap::ArgMatches;

pub(super) fn value<T: Clone + Send + Sync + 'static>(matches: &ArgMatches, id: &str) -> Option<T> {
    matches.try_get_one::<T>(id).ok().flatten().cloned()
}

pub(super) fn string(matches: &ArgMatches, id: &str) -> Option<String> {
    value::<String>(matches, id)
}

/// A value the spec marks as required; clap has already rejected argv
/// without it.
pub(super) fn required(matches: &ArgMatches, id: &str) -> String {
    string(matches, id).unwrap_or_default()
}

pub(super) fn flag(matches: &ArgMatches, id: &str) -> bool {
    value::<bool>(matches, id).unwrap_or(false)
}

pub(super) fn values<T: Clone + Send + Sync + 'static>(matches: &ArgMatches, id: &str) -> Vec<T> {
    matches
        .try_get_many::<T>(id)
        .ok()
        .flatten()
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}

/// Trailing words (`text_words` in the spec) joined back into one string.
pub(super) fn words(matches: &ArgMatches, id: &str) -> String {
    values::<String>(matches, id).join(" ")
}

/// Values paired with their argv position, so options that write the same
/// map (`--token` and `--clear-token`) can be applied in the order given.
pub(super) fn positioned<T: Clone + Send + Sync + 'static>(
    matches: &ArgMatches,
    id: &str,
) -> Vec<(usize, T)> {
    let indices: Vec<usize> = matches
        .indices_of(id)
        .map(Iterator::collect)
        .unwrap_or_default();
    indices.into_iter().zip(values::<T>(matches, id)).collect()
}

/// The `--token NAME=VALUE` / `--clear-token NAME` pairs as a metadata patch.
/// A name given more than once takes its last setting.
pub(super) fn metadata_tokens(
    matches: &ArgMatches,
) -> std::collections::HashMap<String, Option<String>> {
    let mut patch: Vec<(usize, String, Option<String>)> =
        positioned::<(String, Option<String>)>(matches, "token")
            .into_iter()
            .map(|(index, (name, value))| (index, name, value))
            .chain(
                positioned::<String>(matches, "clear-token")
                    .into_iter()
                    .map(|(index, name)| (index, name, None)),
            )
            .collect();
    patch.sort_by_key(|(index, ..)| *index);
    patch
        .into_iter()
        .map(|(_, name, value)| (name, value))
        .collect()
}

/// The `--source ID` of a report command, trimmed; `None` when blank.
pub(super) fn report_source(matches: &ArgMatches) -> Option<String> {
    string(matches, "source")
        .map(|source| source.trim().to_string())
        .filter(|source| !source.is_empty())
}
