//! What a word is in pane text: the one character classifier behind copy-mode
//! word motions and double-click selection.
//!
//! Both read the same pane text, so they agree on which characters are
//! whitespace, which are punctuation and which are CJK punctuation. They differ
//! only in granularity, deliberately:
//!
//! - Copy-mode motions (`w`, `b`, `e`) are vim-like and use [`classify`]: a
//!   run of word characters and a run of punctuation are separate words, so
//!   `src/main.rs` is five words and the motions step through its parts.
//! - Double-click wants a whole token to copy, so it breaks only at
//!   [`is_token_delimiter`] and keeps path and URL punctuation (`/ . - : _ ?`
//!   and the like) inside the token.
//!
//! The prompt editor's `shepr-termio` word rule is separate: it edits typed
//! prompt text, not pane text, and says so there.

/// ASCII punctuation that separates copy-mode words from word characters, in
/// shell-style: punctuation is not consumed as part of a word. `_` is a word
/// character.
const ASCII_PUNCTUATION: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// ASCII punctuation that always ends a double-click token: pipes, brackets,
/// commas, semicolons and `!`. Everything else in [`ASCII_PUNCTUATION`] can sit
/// inside a path, URL or identifier token.
const ASCII_TOKEN_DELIMITERS: &str = "|()[]{},;!";

/// How copy-mode motions see one character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordClass {
    Whitespace,
    /// ASCII or CJK punctuation.
    Separator,
    Word,
}

/// Whether `ch` is CJK or fullwidth punctuation: the CJK Symbols and
/// Punctuation brackets and marks (ideographic comma and full stop, corner and
/// lenticular brackets), the katakana middle dot, and the fullwidth ASCII
/// punctuation forms. The iteration marks and `〇` (letters) and fullwidth
/// low line (a word character like `_`) are not punctuation.
pub fn is_cjk_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '\u{3001}'..='\u{3003}'
            | '\u{3008}'..='\u{3011}'
            | '\u{3014}'..='\u{301F}'
            | '\u{3030}'
            | '\u{303D}'
            | '\u{30FB}'
            | '\u{FF01}'..='\u{FF0F}'
            | '\u{FF1A}'..='\u{FF20}'
            | '\u{FF3B}'..='\u{FF3E}'
            | '\u{FF40}'
            | '\u{FF5B}'..='\u{FF65}'
    )
}

/// The class of `ch` for copy-mode word motions.
pub fn classify(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Whitespace
    } else if (ch.is_ascii() && ASCII_PUNCTUATION.contains(ch)) || is_cjk_punctuation(ch) {
        WordClass::Separator
    } else {
        WordClass::Word
    }
}

/// Whether `ch` ends a double-click token: whitespace, the ASCII token
/// delimiters and CJK punctuation.
pub fn is_token_delimiter(ch: char) -> bool {
    ch.is_whitespace()
        || (ch.is_ascii() && ASCII_TOKEN_DELIMITERS.contains(ch))
        || is_cjk_punctuation(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        assert_eq!(classify(' '), WordClass::Whitespace);
        assert_eq!(classify('\u{3000}'), WordClass::Whitespace);
        assert_eq!(classify('/'), WordClass::Separator);
        assert_eq!(classify('。'), WordClass::Separator);
        assert_eq!(classify('！'), WordClass::Separator);
        assert_eq!(classify('_'), WordClass::Word);
        assert_eq!(classify('＿'), WordClass::Word);
        assert_eq!(classify('々'), WordClass::Word);
        assert_eq!(classify('語'), WordClass::Word);
        assert_eq!(classify('é'), WordClass::Word);
    }

    #[test]
    fn token_delimiters_are_a_subset_of_separators_and_whitespace() {
        for ch in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            if is_token_delimiter(ch) {
                assert_ne!(classify(ch), WordClass::Word, "{ch:?}");
            }
        }
        assert!(is_token_delimiter('|'));
        assert!(is_token_delimiter('、'));
        assert!(!is_token_delimiter('/'));
        assert!(!is_token_delimiter('.'));
        assert!(!is_token_delimiter('_'));
    }
}
