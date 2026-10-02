//! POSIX shell word quoting for command strings that must cross a shell boundary.

/// Whether `value` is safe to place in a shell command without quoting.
///
/// A leading `=` is quoted because zsh expands it before a nested POSIX shell
/// sees the word.
pub fn is_plain_word(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('=')
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
}

/// Render one POSIX shell word, leaving safe plain words unquoted.
pub fn quote(value: &str) -> String {
    if is_plain_word(value) {
        value.to_owned()
    } else {
        quote_always(value)
    }
}

/// Render a command's arguments as POSIX shell words separated by spaces.
pub fn join_argv<I, S>(argv: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    argv.into_iter()
        .map(|argument| quote(argument.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote a word even when it could appear unquoted.
///
/// This is useful for fixtures whose output intentionally shows every argv
/// boundary explicitly.
pub fn quote_always(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
