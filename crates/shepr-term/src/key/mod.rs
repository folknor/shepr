//! Key identity, chord matching and child-facing key encoding.
//!
//! [`TerminalKey`] is a key as the host reported it and the input to pane
//! key encoding. [`KeyChord`] is a configured key code and modifiers.
//! [`CanonicalKey`] is the identity both reduce to for comparison and
//! conflict detection. Canonicalization is lossy (it folds Shift into
//! produced characters and back), so chord matching accepts the normalized
//! reported chord first and falls back to canonical equality.

mod encode;
pub mod tables;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub use encode::{
    KeyEncodeModes, KeyboardProtocol, encode_terminal_key, encode_terminal_key_with_modes,
};

/// A key as shepr understands it. A Linux host terminal reports keys as VT
/// bytes and never a physical key identity, so a key is identified by these
/// semantic fields alone; the bytes it was parsed from are not kept, and two
/// keys that decode alike compare equal whatever bytes produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
    pub repeat_count: u16,
    pub shifted_codepoint: Option<char>,
    pub generated_text: Option<String>,
}

impl TerminalKey {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
        }
    }

    pub fn with_kind(mut self, kind: KeyEventKind) -> Self {
        if kind == KeyEventKind::Release {
            self.repeat_count = 1;
            self.generated_text = None;
        }
        self.kind = kind;
        self
    }

    pub fn with_repeat_count(mut self, repeat_count: u16) -> Self {
        self.repeat_count = if self.kind == KeyEventKind::Release {
            1
        } else {
            repeat_count.max(1)
        };
        self
    }

    pub fn with_modifiers(mut self, modifiers: KeyModifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    pub fn with_shifted_codepoint(mut self, shifted_codepoint: char) -> Self {
        self.shifted_codepoint = Some(shifted_codepoint);
        self
    }

    pub fn with_generated_text(mut self, text: Option<String>) -> Self {
        self.generated_text = if self.kind == KeyEventKind::Release {
            None
        } else {
            text
        };
        self
    }

    /// The reported code and modifiers, before any normalization.
    pub fn chord(&self) -> KeyChord {
        KeyChord::new(self.code, self.modifiers)
    }

    /// The character identity after Shift, before context-specific modifier policy.
    /// Uses a reported alternate before the US layout fallback. Modifier
    /// acceptance and text commits belong to callers.
    pub fn produced_char(&self) -> Option<char> {
        produced_character(self.code, self.modifiers, self.shifted_codepoint)
    }

    /// The identity this key compares by, with any reported Shift alternate.
    pub fn canonical_key(&self) -> CanonicalKey {
        CanonicalKey::from_event(self.code, self.modifiers, self.shifted_codepoint)
    }

    /// A text commit can contain several characters; binding fallback accepts one.
    pub fn committed_char(&self) -> Option<char> {
        let mut characters = self.generated_text.as_deref()?.chars();
        let character = characters.next()?;
        (!character.is_control() && characters.next().is_none()).then_some(character)
    }

    pub fn with_text_commit(mut self) -> Self {
        let has_text_only_modifiers = match self.code {
            KeyCode::Char(ch) if ch.is_uppercase() => {
                self.modifiers == KeyModifiers::SHIFT || self.modifiers.is_empty()
            }
            KeyCode::Char(_) => self.modifiers.is_empty(),
            _ => false,
        };
        if has_text_only_modifiers && self.kind == KeyEventKind::Press {
            // Legacy text has already been shifted by the host. Do not apply
            // a layout or an alternate to the committed character again.
            self.generated_text = match self.code {
                KeyCode::Char(ch) => Some(ch.to_string()),
                _ => None,
            };
        }
        self
    }

    pub fn as_key_event(&self) -> KeyEvent {
        KeyEvent::new_with_kind(self.code, self.modifiers, self.kind)
    }
}

impl From<KeyEvent> for TerminalKey {
    fn from(value: KeyEvent) -> Self {
        Self::new(value.code, value.modifiers).with_kind(value.kind)
    }
}

/// A key code and modifiers as configuration names them: a binding trigger,
/// the prefix key or a fixed alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyChord {
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Shift+Tab reads as BackTab, and BackTab carries no Shift of its own.
    pub fn normalized(self) -> Self {
        let Self {
            mut code,
            mut modifiers,
        } = self;
        if matches!(code, KeyCode::Tab) && modifiers.contains(KeyModifiers::SHIFT) {
            code = KeyCode::BackTab;
            modifiers.remove(KeyModifiers::SHIFT);
        } else if matches!(code, KeyCode::BackTab) {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        Self { code, modifiers }
    }

    pub fn canonical(self) -> CanonicalKey {
        CanonicalKey::from_event(self.code, self.modifiers, None)
    }

    /// Whether `key` triggers this chord: the normalized reported chord equals
    /// this one normalized, or the two share a canonical identity.
    pub fn matches(self, key: &TerminalKey) -> bool {
        let chord = self.normalized();
        key.chord().normalized() == chord || key.canonical_key() == chord.canonical()
    }

    /// Whether `key` was reported with exactly this chord's modifiers, after
    /// normalization. Indexed matching prefers such bindings before
    /// accepting a canonical match.
    pub fn modifiers_match_exactly(self, key: &TerminalKey) -> bool {
        key.chord().normalized().modifiers == self.normalized().modifiers
    }
}

/// The identity used for configured bindings, conflict checks and fallback
/// matching parsed terminal keys. Printable shifted punctuation is identified
/// by the character it produces; letters retain Shift as part of the chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CanonicalKey {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl CanonicalKey {
    fn from_event(code: KeyCode, modifiers: KeyModifiers, shifted_codepoint: Option<char>) -> Self {
        let KeyChord {
            mut code,
            mut modifiers,
        } = KeyChord::new(code, modifiers).normalized();
        if let KeyCode::Char(ch) = code {
            // Unicode case folds are not always one-to-one, so only ASCII
            // letters have a layout-independent base-key plus Shift form.
            if modifiers.contains(KeyModifiers::SHIFT)
                && ch.is_ascii_alphabetic()
                && let Some(lowercase) = single_case_char(ch.to_lowercase())
            {
                code = KeyCode::Char(lowercase);
            } else if modifiers.contains(KeyModifiers::SHIFT) && !ch.is_alphabetic() {
                let shifted =
                    produced_character(code, modifiers, shifted_codepoint).filter(|shifted| {
                        shifted_codepoint.is_some() || *shifted != ch || is_shifted_ascii_symbol(ch)
                    });
                if let Some(shifted) = shifted {
                    code = KeyCode::Char(shifted);
                    modifiers.remove(KeyModifiers::SHIFT);
                } else if is_shifted_ascii_symbol(ch) {
                    modifiers.remove(KeyModifiers::SHIFT);
                }
            } else if !modifiers.contains(KeyModifiers::SHIFT)
                && ch.is_ascii_uppercase()
                && let Some(lowercase) = single_case_char(ch.to_lowercase())
            {
                code = KeyCode::Char(lowercase);
                modifiers |= KeyModifiers::SHIFT;
            }
        }
        Self { code, modifiers }
    }

    pub fn code(self) -> KeyCode {
        self.code
    }

    pub fn modifiers(self) -> KeyModifiers {
        self.modifiers
    }

    /// A printable character with at most Shift held: a key that types text.
    pub fn is_unmodified_printable(self) -> bool {
        matches!(self.code, KeyCode::Char(ch) if !ch.is_control())
            && self.modifiers.difference(KeyModifiers::SHIFT).is_empty()
    }
}

/// The single character of a case mapping, if it maps to one.
pub fn single_case_char(mut chars: impl Iterator<Item = char>) -> Option<char> {
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

const SHIFTED_ASCII_KEYS: [(char, char); 21] = [
    ('0', ')'),
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('4', '$'),
    ('5', '%'),
    ('6', '^'),
    ('7', '&'),
    ('8', '*'),
    ('9', '('),
    ('-', '_'),
    ('=', '+'),
    ('[', '{'),
    (']', '}'),
    ('\\', '|'),
    (';', ':'),
    ('\'', '"'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'),
    ('`', '~'),
];

/// Map Shift on a US-layout key to the character it produces for key identity.
fn shifted_ascii_char(ch: char) -> Option<char> {
    if ch.is_ascii_lowercase() {
        return Some(ch.to_ascii_uppercase());
    }
    SHIFTED_ASCII_KEYS
        .iter()
        .find_map(|(base, shifted)| (*base == ch).then_some(*shifted))
}

/// Whether `ch` is a character Shift produces on a US-layout key.
pub fn is_shifted_ascii_symbol(ch: char) -> bool {
    SHIFTED_ASCII_KEYS.iter().any(|(_, shifted)| *shifted == ch)
}

fn produced_character(
    code: KeyCode,
    modifiers: KeyModifiers,
    shifted: Option<char>,
) -> Option<char> {
    let KeyCode::Char(ch) = code else {
        return None;
    };
    if modifiers.contains(KeyModifiers::SHIFT) {
        Some(shifted.or_else(|| shifted_ascii_char(ch)).unwrap_or(ch))
    } else {
        Some(ch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(code: KeyCode, modifiers: KeyModifiers) -> KeyChord {
        KeyChord::new(code, modifiers)
    }

    #[test]
    fn release_clears_generated_text_and_grouped_repeat_count() {
        let release = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty())
            .with_generated_text(Some("a".to_owned()))
            .with_repeat_count(4)
            .with_kind(KeyEventKind::Release);
        let regrouped_release = release
            .clone()
            .with_repeat_count(4)
            .with_generated_text(Some("ignored".to_owned()));

        assert_eq!(release.generated_text, None);
        assert_eq!(release.repeat_count, 1);
        assert_eq!(regrouped_release.generated_text, None);
        assert_eq!(regrouped_release.repeat_count, 1);
    }

    #[test]
    fn non_ascii_uppercase_with_shift_is_committed_text() {
        let key = TerminalKey::new(KeyCode::Char('É'), KeyModifiers::SHIFT).with_text_commit();

        assert_eq!(key.generated_text.as_deref(), Some("É"));
    }

    #[test]
    fn canonical_identity_folds_legacy_ascii_uppercase_and_shifted_symbols() {
        assert!(
            chord(KeyCode::Char('1'), KeyModifiers::SHIFT)
                .matches(&TerminalKey::new(KeyCode::Char('!'), KeyModifiers::empty()))
        );
        assert!(
            !chord(KeyCode::Char('ö'), KeyModifiers::SHIFT)
                .matches(&TerminalKey::new(KeyCode::Char('Ö'), KeyModifiers::empty()))
        );
        assert_eq!(
            chord(KeyCode::Char('/'), KeyModifiers::SHIFT).canonical(),
            chord(KeyCode::Char('?'), KeyModifiers::empty()).canonical()
        );
        assert_eq!(
            chord(KeyCode::Char('?'), KeyModifiers::SHIFT).canonical(),
            chord(KeyCode::Char('?'), KeyModifiers::empty()).canonical()
        );
    }

    #[test]
    fn shift_tab_normalizes_to_backtab() {
        assert_eq!(
            chord(KeyCode::Tab, KeyModifiers::CONTROL | KeyModifiers::SHIFT).normalized(),
            chord(KeyCode::BackTab, KeyModifiers::CONTROL)
        );
        assert_eq!(
            chord(KeyCode::BackTab, KeyModifiers::SHIFT).normalized(),
            chord(KeyCode::BackTab, KeyModifiers::empty())
        );
    }

    #[test]
    fn reported_alternate_decides_the_produced_character() {
        let key =
            TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT).with_shifted_codepoint('!');
        assert_eq!(key.produced_char(), Some('!'));
        assert_eq!(
            key.canonical_key(),
            chord(KeyCode::Char('!'), KeyModifiers::empty()).canonical()
        );
        assert!(chord(KeyCode::Char('!'), KeyModifiers::empty()).matches(&key));
    }

    #[test]
    fn exact_modifier_match_is_after_normalization() {
        let shifted_one = TerminalKey::new(KeyCode::Char('!'), KeyModifiers::empty());
        let binding = chord(KeyCode::Char('1'), KeyModifiers::SHIFT);
        assert!(binding.matches(&shifted_one));
        assert!(!binding.modifiers_match_exactly(&shifted_one));
        assert!(
            chord(KeyCode::Tab, KeyModifiers::SHIFT)
                .modifiers_match_exactly(&TerminalKey::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        );
    }
}
