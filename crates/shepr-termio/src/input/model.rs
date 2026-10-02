use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use shepr_protocol::KittyKeyboardFlags;
use shepr_vt::ModifyOtherKeysLevel;

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

    pub fn with_modifiers(mut self, modifiers: KeyModifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    pub fn with_shifted_codepoint(mut self, shifted_codepoint: u32) -> Self {
        self.shifted_codepoint = Some(shifted_codepoint);
        self
    }

    pub fn with_generated_text(mut self, text: Option<String>) -> Self {
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

impl shepr_config::BindingKey for TerminalKey {
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

/// The modifyOtherKeys mode the host terminal wants, from `TMUX`,
/// `TERM_PROGRAM` and `WEZTERM_PANE` read under the environment policy.
///
/// # Errors
///
/// Raw and presence values have no content-based refusals. Padded and
/// non-UTF-8 `TERM_PROGRAM` values do not match the name shepr recognizes and
/// cannot fail setup.
pub fn host_modify_other_keys_mode()
-> Result<Option<ModifyOtherKeysLevel>, shepr_core::env::EnvError> {
    use shepr_core::env::{EnvVar, read_os, read_present};
    use std::os::unix::ffi::OsStrExt;

    let term_program = read_os(EnvVar::TermProgram)?;
    Ok(host_modify_other_keys_mode_for_env(
        read_present(EnvVar::Tmux)?,
        term_program.as_deref().map(OsStrExt::as_bytes),
        read_present(EnvVar::WeztermPane)?,
    ))
}

fn host_modify_other_keys_mode_for_env(
    in_tmux: bool,
    term_program: Option<&[u8]>,
    wezterm_pane: bool,
) -> Option<ModifyOtherKeysLevel> {
    if in_tmux {
        return Some(ModifyOtherKeysLevel::All);
    }

    if wezterm_pane || term_program.is_some_and(|program| program.eq_ignore_ascii_case(b"wezterm"))
    {
        return Some(ModifyOtherKeysLevel::ExceptWellDefined);
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardProtocol(KeyboardProtocolMode);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardProtocolMode {
    Legacy,
    Kitty(KittyKeyboardFlags),
}

impl KeyboardProtocol {
    pub const fn legacy() -> Self {
        Self(KeyboardProtocolMode::Legacy)
    }

    pub const fn from_flags(flags: KittyKeyboardFlags) -> Self {
        if flags.is_empty() {
            Self::legacy()
        } else {
            Self(KeyboardProtocolMode::Kitty(flags))
        }
    }

    /// Retains the integer constructor for callers that still receive raw flags.
    pub fn from_kitty_flags(flags: u16) -> Self {
        Self::from_flags(KittyKeyboardFlags::from_bits_retain(flags))
    }

    pub const fn is_kitty(self) -> bool {
        matches!(self.0, KeyboardProtocolMode::Kitty(_))
    }

    pub const fn kitty_flags(self) -> KittyKeyboardFlags {
        match self.0 {
            KeyboardProtocolMode::Legacy => KittyKeyboardFlags::NONE,
            KeyboardProtocolMode::Kitty(flags) => flags,
        }
    }

    pub fn reports_event_types(self) -> bool {
        self.kitty_flags()
            .contains(KittyKeyboardFlags::REPORT_EVENT_TYPES)
    }

    pub fn reports_all_keys(self) -> bool {
        self.kitty_flags()
            .contains(KittyKeyboardFlags::REPORT_ALL_KEYS)
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
            KeyboardProtocol::legacy()
        );
    }

    #[test]
    fn protocol_from_nonzero_flags_is_kitty() {
        assert_eq!(
            KeyboardProtocol::from_kitty_flags(7),
            KeyboardProtocol::from_flags(KittyKeyboardFlags::from_bits_retain(7))
        );
    }

    #[test]
    fn modify_other_keys_mode_is_enabled_for_tmux() {
        assert_eq!(
            host_modify_other_keys_mode_for_env(true, Some(&b"WezTerm"[..]), true),
            Some(ModifyOtherKeysLevel::All)
        );
    }

    #[test]
    fn modify_other_keys_mode_is_enabled_for_wezterm_hosts() {
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, Some(&b"WezTerm"[..]), false),
            Some(ModifyOtherKeysLevel::ExceptWellDefined)
        );
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, None, true),
            Some(ModifyOtherKeysLevel::ExceptWellDefined)
        );
    }

    #[test]
    fn modify_other_keys_mode_is_not_enabled_for_unknown_hosts() {
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, Some(&b"ghostty"[..]), false),
            None
        );
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, None, false),
            None
        );
    }

    #[test]
    fn unknown_or_malformed_terminal_names_do_not_enable_modify_other_keys() {
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, Some(&b" WezTerm"[..]), false),
            None
        );
        assert_eq!(
            host_modify_other_keys_mode_for_env(false, Some(&b"WezTerm\xff"[..]), false),
            None
        );
    }
}
