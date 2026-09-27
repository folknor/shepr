use crate::protocol::KittyKeyboardFlags;
use shepr_vt::ModifyOtherKeysLevel;
use std::io::{self, Write};

const DISABLE_HOST_MOUSE_REPORTING_SEQUENCE: &[u8] =
    b"\x1b[?1006l\x1b[?1016l\x1b[?1015l\x1b[?1005l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l";

// 1015 remains in host cleanup for legacy urxvt terminals; the core does not
// model that host-side mouse encoding.
pub(crate) fn clear_host_mouse_reporting<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(DISABLE_HOST_MOUSE_REPORTING_SEQUENCE)?;
    writer.flush()
}

pub(crate) fn set_host_kitty_keyboard_report_all<W: Write>(
    writer: &mut W,
    report_all_keys: bool,
) -> io::Result<()> {
    let mut flags = ime_compatible_keyboard_enhancement_flags();
    if report_all_keys {
        flags |= crossterm::event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        flags = crossterm::event::KeyboardEnhancementFlags::from_bits_retain(
            flags.bits()
                | u8::try_from(KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT.bits())
                    .unwrap_or_default(),
        );
    }
    crossterm::execute!(
        writer,
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::PushKeyboardEnhancementFlags(flags)
    )
}

pub(crate) fn ime_compatible_keyboard_enhancement_flags()
-> crossterm::event::KeyboardEnhancementFlags {
    use crossterm::event::KeyboardEnhancementFlags as Flags;
    Flags::DISAMBIGUATE_ESCAPE_CODES | Flags::REPORT_EVENT_TYPES | Flags::REPORT_ALTERNATE_KEYS
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectHostKeyboardState {
    kitty_flags: Option<KittyKeyboardFlags>,
    modify_other_keys_level: ModifyOtherKeysLevel,
}

pub(crate) fn set_direct_host_keyboard_protocol<W: Write>(
    writer: &mut W,
    active: &mut DirectHostKeyboardState,
    next_flags: KittyKeyboardFlags,
    next_modify_other_keys_level: ModifyOtherKeysLevel,
) -> io::Result<()> {
    let next_kitty_flags = (!next_flags.is_empty()).then_some(next_flags);
    if active.kitty_flags == next_kitty_flags
        && active.modify_other_keys_level == next_modify_other_keys_level
    {
        return Ok(());
    }

    if active.kitty_flags != next_kitty_flags {
        if active.kitty_flags.is_some() {
            writer.write_all(b"\x1b[<1u")?;
        }
        if !next_flags.is_empty() {
            write!(writer, "\x1b[>{}u", next_flags.bits())?;
        }
    }
    if active.modify_other_keys_level != next_modify_other_keys_level {
        write!(writer, "\x1b[>4;{next_modify_other_keys_level}m")?;
    }
    writer.flush()?;
    *active = DirectHostKeyboardState {
        kitty_flags: next_kitty_flags,
        modify_other_keys_level: next_modify_other_keys_level,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_enhancement_flags_stay_ime_compatible() {
        use crossterm::event::KeyboardEnhancementFlags as Flags;
        let flags = ime_compatible_keyboard_enhancement_flags();
        assert!(flags.contains(Flags::DISAMBIGUATE_ESCAPE_CODES));
        assert!(flags.contains(Flags::REPORT_EVENT_TYPES));
        assert!(flags.contains(Flags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(Flags::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }

    #[test]
    fn host_keyboard_report_all_replaces_the_current_shepr_stack_entry() {
        let mut output = Vec::new();

        set_host_kitty_keyboard_report_all(&mut output, true).expect("test precondition");
        set_host_kitty_keyboard_report_all(&mut output, false).expect("test precondition");

        assert_eq!(output, b"\x1b[<1u\x1b[>31u\x1b[<1u\x1b[>7u");
    }

    #[test]
    fn direct_keyboard_protocol_owns_exactly_one_stack_entry_and_modify_other_keys() {
        let mut output = Vec::new();
        let mut active = DirectHostKeyboardState::default();

        set_direct_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(3),
            ModifyOtherKeysLevel::from_parameter(0),
        )
        .expect("test precondition");
        set_direct_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(15),
            ModifyOtherKeysLevel::from_parameter(2),
        )
        .expect("test precondition");
        set_direct_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(0),
        )
        .expect("test precondition");

        assert_eq!(
            output,
            b"\x1b[>3u\x1b[<1u\x1b[>15u\x1b[>4;2m\x1b[<1u\x1b[>4;0m"
        );
        assert_eq!(active, DirectHostKeyboardState::default());
    }

    #[test]
    fn direct_modify_other_keys_works_without_kitty_flags() {
        let mut output = Vec::new();
        let mut active = DirectHostKeyboardState::default();

        set_direct_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(1),
        )
        .expect("test precondition");
        set_direct_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(2),
        )
        .expect("test precondition");
        set_direct_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(0),
        )
        .expect("test precondition");

        assert_eq!(output, b"\x1b[>4;1m\x1b[>4;2m\x1b[>4;0m");
        assert_eq!(active, DirectHostKeyboardState::default());
    }

    #[test]
    fn direct_legacy_keyboard_mode_does_not_pop_the_host_stack() {
        let mut output = Vec::new();
        let mut active = DirectHostKeyboardState::default();

        set_direct_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(0),
        )
        .expect("test precondition");

        assert!(output.is_empty());
        assert_eq!(active, DirectHostKeyboardState::default());
    }

    #[test]
    fn clears_all_known_host_mouse_modes() {
        let mut output = Vec::new();
        clear_host_mouse_reporting(&mut output).expect("test precondition");
        let sequence = std::str::from_utf8(&output).expect("test precondition");

        for mode in ["9", "1000", "1002", "1003", "1005", "1006", "1015", "1016"] {
            assert!(
                sequence.contains(&format!("\x1b[?{mode}l")),
                "missing mouse mode {mode}"
            );
        }
    }
}
