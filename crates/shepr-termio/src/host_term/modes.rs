use shepr_protocol::KittyKeyboardFlags;
use shepr_vt::ModifyOtherKeysLevel;
use std::io::{self, Write};

const HOST_MOUSE_REPORTING_DISABLE_SEQUENCES: &[&[u8]] = &[
    b"\x1b[?1006l",
    b"\x1b[?1016l",
    b"\x1b[?1015l",
    b"\x1b[?1005l",
    b"\x1b[?1003l",
    b"\x1b[?1002l",
    b"\x1b[?1000l",
    b"\x1b[?9l",
];

pub const HOST_KEYBOARD_QUERY_SEQUENCE: &[u8] = b"\x1b[?u\x1b[c";
pub const HOST_CELL_SIZE_QUERY_SEQUENCE: &[u8] = b"\x1b[16t";
pub const HOST_MODIFY_OTHER_KEYS_RESET_SEQUENCE: &[u8] = b"\x1b[>4;0m";
pub const HOST_KITTY_KEYBOARD_POP_SEQUENCE: &[u8] = b"\x1b[<1u";
pub const HOST_CURSOR_AND_SHAPE_RESTORE_SEQUENCE: &[u8] = b"\x1b[?25h\x1b[0 q";
pub const HOST_MOUSE_SGR_PIXELS_ENABLE_SEQUENCE: &[u8] = b"\x1b[?1016h";
pub const HOST_WINDOW_TITLE_PUSH_SEQUENCE: &[u8] = b"\x1b[22;0t";
pub const HOST_WINDOW_TITLE_POP_SEQUENCE: &[u8] = b"\x1b[23;0t";

// 1015 remains in host cleanup for legacy urxvt terminals; the core does not
// model that host-side mouse encoding.
pub fn clear_host_mouse_reporting<W: Write>(writer: &mut W) -> io::Result<()> {
    for sequence in HOST_MOUSE_REPORTING_DISABLE_SEQUENCES.iter().copied() {
        writer.write_all(sequence)?;
    }
    writer.flush()
}

pub fn enable_host_sgr_pixel_mouse_reporting<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(HOST_MOUSE_SGR_PIXELS_ENABLE_SEQUENCE)?;
    writer.flush()
}

pub fn restore_host_keyboard_protocol<W: Write>(
    writer: &mut W,
    modify_other_keys_active: bool,
    kitty_entry_active: bool,
) -> io::Result<()> {
    // Every step runs even after a failure; the first error wins.
    let mut result = Ok(());
    if modify_other_keys_active {
        result = result.and(writer.write_all(HOST_MODIFY_OTHER_KEYS_RESET_SEQUENCE));
    }
    if kitty_entry_active {
        result = result.and(writer.write_all(HOST_KITTY_KEYBOARD_POP_SEQUENCE));
    }
    result.and(writer.flush())
}

/// Selects the client's keyboard enhancement entry for shell input.
///
/// A first enable pushes without popping; later changes replace only the
/// entry recorded in `active`.
pub fn set_host_kitty_keyboard_report_all<W: Write>(
    writer: &mut W,
    active: &mut HostKeyboardState,
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
    let modify_other_keys_level = active.modify_other_keys_level;
    set_host_keyboard_protocol(
        writer,
        active,
        KittyKeyboardFlags::from_bits_retain(u16::from(flags.bits())),
        modify_other_keys_level,
    )
}

pub fn set_host_modify_other_keys<W: Write>(
    writer: &mut W,
    active: &mut HostKeyboardState,
    level: ModifyOtherKeysLevel,
) -> io::Result<()> {
    let flags = active.kitty_flags.unwrap_or(KittyKeyboardFlags::NONE);
    set_host_keyboard_protocol(writer, active, flags, level)
}

pub fn ime_compatible_keyboard_enhancement_flags() -> crossterm::event::KeyboardEnhancementFlags {
    use crossterm::event::KeyboardEnhancementFlags as Flags;
    Flags::DISAMBIGUATE_ESCAPE_CODES | Flags::REPORT_EVENT_TYPES | Flags::REPORT_ALTERNATE_KEYS
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HostKeyboardState {
    kitty_flags: Option<KittyKeyboardFlags>,
    modify_other_keys_level: ModifyOtherKeysLevel,
}

impl HostKeyboardState {
    pub fn has_kitty_keyboard_entry(&self) -> bool {
        self.kitty_flags.is_some()
    }

    pub fn modify_other_keys_active(&self) -> bool {
        self.modify_other_keys_level != ModifyOtherKeysLevel::Off
    }
}

pub fn set_host_keyboard_protocol<W: Write>(
    writer: &mut W,
    active: &mut HostKeyboardState,
    next_flags: KittyKeyboardFlags,
    next_modify_other_keys_level: ModifyOtherKeysLevel,
) -> io::Result<()> {
    let next_kitty_flags = (!next_flags.is_empty()).then_some(next_flags);
    if active.kitty_flags == next_kitty_flags
        && active.modify_other_keys_level == next_modify_other_keys_level
    {
        return Ok(());
    }

    // `active` is updated only after a successful flush, on purpose. A failed
    // host write is fatal to the client, and the final restore is driven by the
    // client's own restore mask (`HostModes` in shepr-client), which is raised
    // before every keyboard write and never narrowed by a failed one, so it
    // over-approximates what shepr owns. The worst case after a write that
    // fails part way is one extra pop at restore, which could remove an entry
    // the shell or an outer multiplexer pushed if the terminal is still alive;
    // leaking a pushed entry into the shell would be the worse bias. A stale
    // `active` is never read again.
    if active.kitty_flags != next_kitty_flags {
        if active.kitty_flags.is_some() {
            writer.write_all(HOST_KITTY_KEYBOARD_POP_SEQUENCE)?;
        }
        if !next_flags.is_empty() {
            write!(writer, "\x1b[>{}u", next_flags.bits())?;
        }
    }
    if active.modify_other_keys_level != next_modify_other_keys_level {
        if next_modify_other_keys_level == ModifyOtherKeysLevel::Off {
            writer.write_all(HOST_MODIFY_OTHER_KEYS_RESET_SEQUENCE)?;
        } else {
            write!(writer, "\x1b[>4;{next_modify_other_keys_level}m")?;
        }
    }
    writer.flush()?;
    *active = HostKeyboardState {
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
        let mut active = HostKeyboardState::default();

        set_host_kitty_keyboard_report_all(&mut output, &mut active, true)
            .expect("test precondition");
        set_host_kitty_keyboard_report_all(&mut output, &mut active, false)
            .expect("test precondition");

        assert_eq!(output, b"\x1b[>31u\x1b[<1u\x1b[>7u");
    }

    #[test]
    fn keyboard_protocol_owns_exactly_one_stack_entry_and_modify_other_keys() {
        let mut output = Vec::new();
        let mut active = HostKeyboardState::default();

        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(3),
            ModifyOtherKeysLevel::from_parameter(0),
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(15),
            ModifyOtherKeysLevel::from_parameter(2),
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
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
        assert_eq!(active, HostKeyboardState::default());
    }

    #[test]
    fn modify_other_keys_works_without_kitty_flags() {
        let mut output = Vec::new();
        let mut active = HostKeyboardState::default();

        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(1),
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(2),
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(0),
        )
        .expect("test precondition");

        assert_eq!(output, b"\x1b[>4;1m\x1b[>4;2m\x1b[>4;0m");
        assert_eq!(active, HostKeyboardState::default());
    }

    #[test]
    fn legacy_keyboard_mode_does_not_pop_the_host_stack() {
        let mut output = Vec::new();
        let mut active = HostKeyboardState::default();

        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::from_parameter(0),
        )
        .expect("test precondition");

        assert!(output.is_empty());
        assert_eq!(active, HostKeyboardState::default());
    }

    #[test]
    fn keyboard_restore_flushes_what_it_writes() {
        let mut output = io::BufWriter::new(Vec::new());
        restore_host_keyboard_protocol(&mut output, true, true).expect("test precondition");
        assert_eq!(output.buffer(), b"", "nothing is left in the buffer");
        assert_eq!(output.get_ref().as_slice(), b"\x1b[>4;0m\x1b[<1u");
    }

    #[test]
    fn clears_all_known_host_mouse_modes() {
        let mut output = Vec::new();
        clear_host_mouse_reporting(&mut output).expect("test precondition");
        let mut expected = Vec::new();
        for sequence in HOST_MOUSE_REPORTING_DISABLE_SEQUENCES.iter().copied() {
            expected.extend_from_slice(sequence);
        }
        assert_eq!(output, expected);
    }
}
