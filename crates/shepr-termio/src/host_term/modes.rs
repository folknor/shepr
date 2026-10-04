use shepr_term::{KittyKeyboardFlags, ModifyOtherKeysLevel};
use std::io::{self, Write};

/// Host mouse modes cleared on exit, in this order: SGR, SGR pixels, urxvt,
/// UTF-8, any motion, button motion, press and release, X10. This is a host
/// list, not the emulator's: urxvt (1015) is cleared for legacy hosts although
/// the pane core does not model it.
const HOST_MOUSE_REPORTING_DISABLE_MODES: &[u16] = &[1006, 1016, 1015, 1005, 1003, 1002, 1000, 9];

pub const HOST_KEYBOARD_QUERY_SEQUENCE: &[u8] = shepr_term::seq::HOST_KEYBOARD_QUERY_SEQUENCE;
pub const HOST_CELL_SIZE_QUERY_SEQUENCE: &[u8] = shepr_term::seq::HOST_CELL_SIZE_QUERY_SEQUENCE;
pub const HOST_MODIFY_OTHER_KEYS_RESET_SEQUENCE: &[u8] = ModifyOtherKeysLevel::Off.set_sequence();
pub const HOST_KITTY_KEYBOARD_POP_SEQUENCE: &[u8] =
    shepr_term::seq::HOST_KITTY_KEYBOARD_POP_SEQUENCE;
pub const HOST_CURSOR_SHAPE_DEFAULT_SEQUENCE: &[u8] =
    shepr_term::seq::HOST_CURSOR_SHAPE_DEFAULT_SEQUENCE;
pub const HOST_MOUSE_SGR_PIXELS_ENABLE_SEQUENCE: shepr_term::seq::DecModeSequence =
    shepr_term::seq::HOST_MOUSE_SGR_PIXELS_ENABLE_SEQUENCE;
pub const HOST_WINDOW_TITLE_PUSH_SEQUENCE: &[u8] = shepr_term::seq::HOST_WINDOW_TITLE_PUSH_SEQUENCE;
pub const HOST_WINDOW_TITLE_POP_SEQUENCE: &[u8] = shepr_term::seq::HOST_WINDOW_TITLE_POP_SEQUENCE;

pub fn clear_host_mouse_reporting<W: Write>(writer: &mut W) -> io::Result<()> {
    for mode in HOST_MOUSE_REPORTING_DISABLE_MODES {
        write!(writer, "\x1b[?{mode}l")?;
    }
    writer.flush()
}

pub fn enable_host_sgr_pixel_mouse_reporting<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(HOST_MOUSE_SGR_PIXELS_ENABLE_SEQUENCE.as_bytes())?;
    writer.flush()
}

/// Resets the host's modifyOtherKeys mode.
pub fn restore_host_modify_other_keys<W: Write>(writer: &mut W) -> io::Result<()> {
    writer
        .write_all(HOST_MODIFY_OTHER_KEYS_RESET_SEQUENCE)
        .and_then(|()| writer.flush())
}

/// Pops the kitty keyboard entry the client pushed onto the host's stack.
pub fn restore_host_kitty_keyboard_entry<W: Write>(writer: &mut W) -> io::Result<()> {
    writer
        .write_all(HOST_KITTY_KEYBOARD_POP_SEQUENCE)
        .and_then(|()| writer.flush())
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
        flags.insert(KittyKeyboardFlags::REPORT_ALL_KEYS);
        flags.insert(KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT);
    }
    let modify_other_keys_level = active.modify_other_keys_level;
    set_host_keyboard_protocol(writer, active, flags, modify_other_keys_level)
}

pub fn set_host_modify_other_keys<W: Write>(
    writer: &mut W,
    active: &mut HostKeyboardState,
    level: ModifyOtherKeysLevel,
) -> io::Result<()> {
    let flags = active.kitty_flags.unwrap_or(KittyKeyboardFlags::NONE);
    set_host_keyboard_protocol(writer, active, flags, level)
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

pub fn ime_compatible_keyboard_enhancement_flags() -> KittyKeyboardFlags {
    KittyKeyboardFlags::DISAMBIGUATE
        | KittyKeyboardFlags::REPORT_EVENT_TYPES
        | KittyKeyboardFlags::REPORT_ALTERNATE_KEYS
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
            write!(writer, "{}", shepr_term::seq::KittyPush(next_flags.bits()))?;
        }
    }
    if active.modify_other_keys_level != next_modify_other_keys_level {
        writer.write_all(next_modify_other_keys_level.set_sequence())?;
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
        let flags = ime_compatible_keyboard_enhancement_flags();
        assert!(flags.contains(KittyKeyboardFlags::DISAMBIGUATE));
        assert!(flags.contains(KittyKeyboardFlags::REPORT_EVENT_TYPES));
        assert!(flags.contains(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS));
        assert!(!flags.contains(KittyKeyboardFlags::REPORT_ALL_KEYS));
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
            ModifyOtherKeysLevel::Off,
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(15),
            ModifyOtherKeysLevel::All,
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::Off,
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
            ModifyOtherKeysLevel::ExceptWellDefined,
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::All,
        )
        .expect("test precondition");
        set_host_keyboard_protocol(
            &mut output,
            &mut active,
            KittyKeyboardFlags::from_bits_retain(0),
            ModifyOtherKeysLevel::Off,
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
            ModifyOtherKeysLevel::Off,
        )
        .expect("test precondition");

        assert!(output.is_empty());
        assert_eq!(active, HostKeyboardState::default());
    }

    #[test]
    fn keyboard_restore_flushes_what_it_writes() {
        let mut output = io::BufWriter::new(Vec::new());
        restore_host_modify_other_keys(&mut output).expect("test precondition");
        assert_eq!(output.buffer(), b"", "nothing is left in the buffer");
        assert_eq!(output.get_ref().as_slice(), b"\x1b[>4;0m");
        restore_host_kitty_keyboard_entry(&mut output).expect("test precondition");
        assert_eq!(output.buffer(), b"", "nothing is left in the buffer");
        assert_eq!(output.get_ref().as_slice(), b"\x1b[>4;0m\x1b[<1u");
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

    #[test]
    fn clears_all_known_host_mouse_modes() {
        let mut output = Vec::new();
        clear_host_mouse_reporting(&mut output).expect("test precondition");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x1b[?1006l\x1b[?1016l\x1b[?1015l\x1b[?1005l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l");
        assert_eq!(output, expected);
    }
}
