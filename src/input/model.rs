use crate::protocol::KittyKeyboardFlags;
use crossterm::event::KeyboardEnhancementFlags;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

/// A key as shepr understands it. A Linux host terminal reports keys as VT
/// bytes and never a physical key identity, so a key is identified by these
/// semantic fields alone; the bytes it was parsed from are not kept, and two
/// keys that decode alike compare equal whatever bytes produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: crossterm::event::KeyEventKind,
    pub repeat_count: u16,
    pub shifted_codepoint: Option<u32>,
    pub generated_text: Option<String>,
}

impl TerminalKey {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            code,
            modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
        }
    }

    pub fn with_kind(mut self, kind: crossterm::event::KeyEventKind) -> Self {
        if kind == crossterm::event::KeyEventKind::Release {
            self.repeat_count = 1;
            self.generated_text = None;
        }
        self.kind = kind;
        self
    }

    pub fn with_repeat_count(mut self, repeat_count: u16) -> Self {
        self.repeat_count = if self.kind == crossterm::event::KeyEventKind::Release {
            1
        } else {
            repeat_count.max(1)
        };
        self
    }

    pub(crate) fn with_modifiers(mut self, modifiers: KeyModifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    pub fn with_shifted_codepoint(mut self, shifted_codepoint: u32) -> Self {
        self.shifted_codepoint = Some(shifted_codepoint);
        self
    }

    pub(crate) fn with_generated_text(mut self, text: Option<String>) -> Self {
        self.generated_text = if self.kind == crossterm::event::KeyEventKind::Release {
            None
        } else {
            text
        };
        self
    }

    pub fn with_text_commit(mut self) -> Self {
        let has_text_only_modifiers = match self.code {
            KeyCode::Char(ch) if ch.is_uppercase() => {
                self.modifiers == KeyModifiers::SHIFT || self.modifiers.is_empty()
            }
            KeyCode::Char(_) => self.modifiers.is_empty(),
            _ => false,
        };
        if has_text_only_modifiers && self.kind == crossterm::event::KeyEventKind::Press {
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

impl crate::config::BindingKey for TerminalKey {
    fn code(&self) -> KeyCode {
        self.code
    }

    fn modifiers(&self) -> KeyModifiers {
        self.modifiers
    }

    fn shifted_codepoint(&self) -> Option<u32> {
        self.shifted_codepoint
    }
}

impl From<KeyEvent> for TerminalKey {
    fn from(value: KeyEvent) -> Self {
        Self::new(value.code, value.modifiers).with_kind(value.kind)
    }
}

pub(crate) const KITTY_FLAG_REPORT_ALL_KEYS: u16 = KittyKeyboardFlags::REPORT_ALL_KEYS.bits();

pub fn ime_compatible_keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    // Do not request kitty REPORT_ALL_KEYS (flag 8): it breaks IME and compose
    // input. Text keys therefore have no release events to forward to panes.
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifyOtherKeysMode {
    Mode1,
    Mode2,
}

impl ModifyOtherKeysMode {
    pub fn set_sequence(self) -> &'static [u8] {
        match self {
            Self::Mode1 => b"\x1b[>4;1m",
            Self::Mode2 => b"\x1b[>4;2m",
        }
    }
}

pub fn host_modify_other_keys_mode() -> Option<ModifyOtherKeysMode> {
    host_modify_other_keys_mode_for_env(
        std::env::var("TMUX").is_ok(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var_os("WEZTERM_PANE").is_some(),
    )
}

fn host_modify_other_keys_mode_for_env(
    in_tmux: bool,
    term_program: Option<&str>,
    wezterm_pane: bool,
) -> Option<ModifyOtherKeysMode> {
    if in_tmux {
        return Some(ModifyOtherKeysMode::Mode2);
    }

    if wezterm_pane || term_program.is_some_and(|program| program.eq_ignore_ascii_case("wezterm")) {
        return Some(ModifyOtherKeysMode::Mode1);
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocol {
    Legacy,
    Kitty { flags: u16 },
}

impl KeyboardProtocol {
    pub fn from_kitty_flags(flags: u16) -> Self {
        if flags == 0 {
            Self::Legacy
        } else {
            Self::Kitty { flags }
        }
    }

    pub(crate) fn reports_event_types(self) -> bool {
        matches!(self, Self::Kitty { flags } if flags & 0b0000_0010 != 0)
    }

    pub(crate) fn reports_all_keys(self) -> bool {
        matches!(self, Self::Kitty { flags } if flags & KITTY_FLAG_REPORT_ALL_KEYS != 0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocolMode {
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

impl MouseProtocolMode {
    #[cfg(test)]
    pub fn reporting_enabled(self) -> bool {
        self != Self::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocolEncoding {
    Default,
    Utf8,
    Sgr,
    SgrPixels,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_clears_generated_text_and_grouped_repeat_count() {
        let release = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty())
            .with_generated_text(Some("a".to_owned()))
            .with_repeat_count(4)
            .with_kind(crossterm::event::KeyEventKind::Release);
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
    fn protocol_from_zero_flags_is_legacy() {
        assert_eq!(
            KeyboardProtocol::from_kitty_flags(0),
            KeyboardProtocol::Legacy
        );
    }

    #[test]
    fn protocol_from_nonzero_flags_is_kitty() {
        assert_eq!(
            KeyboardProtocol::from_kitty_flags(7),
            KeyboardProtocol::Kitty { flags: 7 }
        );
    }

    #[test]
    fn keyboard_enhancement_flags_stay_ime_compatible() {
        let flags = ime_compatible_keyboard_enhancement_flags();

        assert!(flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES));
        assert!(flags.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }

    #[test]
    fn modify_other_keys_mode_is_enabled_for_tmux() {
        assert_eq!(
            host_modify_other_keys_mode_for_env(true, Some("WezTerm"), true),
            Some(ModifyOtherKeysMode::Mode2)
        );
    }

    #[test]
    fn modify_other_keys_mode_is_enabled_for_wezterm_hosts() {
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, Some("WezTerm"), false),
            Some(ModifyOtherKeysMode::Mode1)
        );
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, None, true),
            Some(ModifyOtherKeysMode::Mode1)
        );
    }

    #[test]
    fn modify_other_keys_mode_is_not_enabled_for_unknown_hosts() {
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, Some("ghostty"), false),
            None
        );
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, None, false),
            None
        );
    }
}
