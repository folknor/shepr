//! The host-theme restore policy for OSC 10/11 default-colour overrides: which
//! foreground program set an override, and whether it has left. Both are
//! process questions (they scan `/proc`), so they belong here on the runtime
//! and detection side. The terminal only keeps the bookkeeping
//! (`PaneTerminal::note_default_color_owner`,
//! `PaneTerminal::drop_default_color_overrides_if`) and knows nothing of
//! processes.

use tracing::{debug, info};

use shepr_core::layout::PaneId;

use super::super::process_probe::Foreground;
use super::super::teardown::ChildLiveness;
use super::super::terminal::{DefaultColorGeneration, PaneTerminal};

fn foreground_job_is_shell(
    job: &shepr_platform::ForegroundJob,
    shell_pid: shepr_platform::Pid,
) -> bool {
    Foreground::from_job(Some(job), shell_pid).is_shell()
}

/// The process group of the foreground program when it is not the shell.
/// Scans `/proc`: never call it with the terminal lock held.
fn current_transient_default_color_owner(
    shell_pid: shepr_platform::Pid,
) -> Option<shepr_platform::Pgid> {
    let job = shepr_platform::foreground_job(shell_pid)?;
    match Foreground::from_job(Some(&job), shell_pid) {
        Foreground::Job(job) => Some(job.process_group_id),
        Foreground::Shell | Foreground::Unknown => None,
    }
}

/// Whether the shell is back in the foreground and the program that set the
/// override (`owner_pgid`) is no longer the foreground group. The terminal
/// decides separately whether it is in a state to take the overrides away
/// (the alternate screen defers it).
fn should_restore_host_terminal_theme(
    owner_pgid: shepr_platform::Pgid,
    shell_pid: shepr_platform::Pid,
    foreground_job: Option<&shepr_platform::ForegroundJob>,
) -> bool {
    let Some(foreground_job) = foreground_job else {
        return false;
    };

    foreground_job.process_group_id != owner_pgid
        && foreground_job_is_shell(foreground_job, shell_pid)
}

/// Records which foreground program overrode a default colour, so the
/// detection tick can drop the override once that program is gone.
///
/// Finding the program means scanning `/proc`. The caller releases the
/// terminal and reply-order locks before this scan, then the terminal's
/// generation-checked setter records the owner only if that OSC 10/11
/// override is still current. With no live child pid, nothing is scanned.
pub(in crate::pane) fn resolve_default_color_owner(
    terminal: &PaneTerminal,
    pane_id: PaneId,
    child_liveness: &ChildLiveness,
    generation: DefaultColorGeneration,
) {
    let Some(owner_pgid) = child_liveness
        .observe(current_transient_default_color_owner)
        .flatten()
    else {
        return;
    };
    if terminal.note_default_color_owner(generation, owner_pgid) {
        debug!(
            pane = %pane_id,
            owner_pgid = owner_pgid.get(),
            "tracked transient default color override"
        );
    }
}

/// Once the program that overrode the default colours has left the
/// foreground, drops its OSC 10/11 overrides so the host theme shows again.
/// This clears the core's override slots directly; nothing is written into
/// the child's byte stream. Returns whether overrides were dropped.
pub(in crate::pane) fn maybe_restore_host_terminal_theme(
    terminal: &PaneTerminal,
    pane_id: PaneId,
    child_liveness: &ChildLiveness,
) -> bool {
    let Some(owner_pgid) = terminal.theme_restore_owner() else {
        return false;
    };
    let Some((shell_pid, foreground_job)) =
        child_liveness.observe(|pid| (pid, shepr_platform::foreground_job(pid)))
    else {
        return false;
    };
    if !should_restore_host_terminal_theme(owner_pgid, shell_pid, foreground_job.as_ref()) {
        return false;
    }
    let dropped = terminal.drop_default_color_overrides_if(owner_pgid);
    if dropped {
        info!(
            pane = %pane_id,
            owner_pgid = owner_pgid.get(),
            "restored host terminal default colors after transient override"
        );
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_default_theme(pane: &PaneTerminal) -> shepr_term::host::TerminalTheme {
        let mut core = pane.core.lock().expect("test precondition");
        let super::super::super::terminal::PaneTerminalCore {
            terminal,
            render_state,
            ..
        } = &mut *core;
        render_state.update(terminal);
        let colors = render_state.colors();
        shepr_term::host::TerminalTheme {
            foreground: Some(shepr_term::host::RgbColor {
                r: colors.foreground.r,
                g: colors.foreground.g,
                b: colors.foreground.b,
            }),
            background: Some(shepr_term::host::RgbColor {
                r: colors.background.r,
                g: colors.background.g,
                b: colors.background.b,
            }),
            ..Default::default()
        }
    }

    fn shell_job(shell_pid: u32) -> shepr_platform::ForegroundJob {
        shepr_platform::ForegroundJob {
            process_group_id: shepr_platform::Pgid::led_by(
                shepr_platform::Pid::new(shell_pid).expect("test shell pid"),
            ),
            processes: vec![shepr_platform::ForegroundProcess {
                pid: shepr_platform::Pid::new(shell_pid).expect("test shell pid"),
                name: "zsh".to_string(),
                argv: Some(vec!["zsh".to_string()]),
            }],
        }
    }

    #[test]
    fn host_theme_restore_waits_for_the_shell_to_be_foreground() {
        assert!(!should_restore_host_terminal_theme(
            shepr_platform::Pgid::new(42).expect("owner group"),
            shepr_platform::Pid::new(7).expect("shell pid"),
            None,
        ));
        assert!(!should_restore_host_terminal_theme(
            shepr_platform::Pgid::new(42).expect("owner group"),
            shepr_platform::Pid::new(7).expect("shell pid"),
            Some(&shepr_platform::ForegroundJob {
                process_group_id: shepr_platform::Pgid::new(42).expect("test group"),
                processes: vec![shepr_platform::ForegroundProcess {
                    pid: shepr_platform::Pid::new(42).expect("test pid"),
                    name: "droid".to_string(),
                    argv: Some(vec!["droid".to_string()]),
                }],
            }),
        ));
        assert!(should_restore_host_terminal_theme(
            shepr_platform::Pgid::new(42).expect("owner group"),
            shepr_platform::Pid::new(7).expect("shell pid"),
            Some(&shell_job(7)),
        ));

        assert!(!should_restore_host_terminal_theme(
            shepr_platform::Pgid::new(7).expect("owner group"),
            shepr_platform::Pid::new(7).expect("shell pid"),
            Some(&shell_job(7)),
        ));
    }

    #[test]
    fn dropping_overrides_waits_for_the_main_screen() {
        let mut terminal = shepr_vt::Terminal::new(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            shepr_core::scrollback::ScrollbackBudget::new(0),
        );
        terminal.write(b"\x1b[?1049h");
        let pane = PaneTerminal::new(terminal);
        let owner = shepr_platform::Pgid::new(42).expect("test group");
        pane.apply_host_terminal_theme(shepr_term::host::TerminalTheme {
            foreground: Some(shepr_term::host::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            ..Default::default()
        });
        let generation = pane
            .process_pty_bytes(
                shepr_test_fixtures::fixed_pane_id(1),
                b"\x1b]11;rgb:dd/ee/ff\x1b\\",
            )
            .default_color_generation
            .expect("an override was set");
        assert!(pane.note_default_color_owner(generation, owner));

        assert_eq!(pane.theme_restore_owner(), None);
        assert!(!pane.drop_default_color_overrides_if(owner));
        assert!(pane.has_transient_default_color_override());
    }

    #[test]
    fn restore_host_terminal_theme_reapplies_cached_colors() {
        let terminal = shepr_vt::Terminal::new(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            shepr_core::scrollback::ScrollbackBudget::new(0),
        );
        let pane = PaneTerminal::new(terminal);
        let owner = shepr_platform::Pgid::new(42).expect("test group");
        let host_theme = shepr_term::host::TerminalTheme {
            foreground: Some(shepr_term::host::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(shepr_term::host::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };

        pane.apply_host_terminal_theme(host_theme);
        let generation = pane
            .process_pty_bytes(
                shepr_test_fixtures::fixed_pane_id(1),
                b"\x1b]10;rgb:01/02/03\x1b\\\x1b]11;rgb:dd/ee/ff\x1b\\",
            )
            .default_color_generation
            .expect("an override was set");
        assert!(pane.note_default_color_owner(generation, owner));
        assert_eq!(
            pane_default_theme(&pane).background,
            Some(shepr_term::host::RgbColor {
                r: 0xdd,
                g: 0xee,
                b: 0xff,
            })
        );

        // A different group than the recorded owner leaves the overrides.
        assert!(
            !pane.drop_default_color_overrides_if(
                shepr_platform::Pgid::new(43).expect("other group")
            )
        );
        assert!(pane.has_transient_default_color_override());

        // The child is mid-sequence when the restore runs: nothing may be
        // written into its stream.
        {
            let mut core = pane.core.lock().expect("test precondition");
            core.terminal.write(b"\x1b[3");
        }
        assert!(pane.drop_default_color_overrides_if(owner));
        assert!(!pane.has_transient_default_color_override());
        {
            let mut core = pane.core.lock().expect("test precondition");
            core.terminal.write(b"1mX");
            assert_eq!(
                core.terminal
                    .read_text_screen(
                        shepr_vt::Point::new(shepr_vt::ScreenRow(0), 0),
                        shepr_vt::Point::new(shepr_vt::ScreenRow(0), 0),
                    )
                    .expect("test precondition"),
                "X"
            );
        }

        assert_eq!(pane_default_theme(&pane).background, host_theme.background);
        assert_eq!(pane_default_theme(&pane).foreground, host_theme.foreground);
    }
}
