//! The window title and ConEmu progress the child emitted, retained as agent
//! detection evidence.

use shepr_vt::Progress;

/// Maximum agent OSC title characters retained from untrusted output.
/// What counts as a displayable character is `shepr_term::title`'s rule.
const AGENT_OSC_MAX_CHARS: usize = 256;

/// Retains the latest window title and OSC 9;4 progress report emitted by the
/// child process, for agent detection, `detect.explain` and the pane title.
/// Both come from the terminal core ([`apply_terminal_updates`]): the title is
/// whatever the parser made of OSC 0/2, the CSI 22/23 t title stack and RIS;
/// progress is the scanner's parse of the ConEmu `OSC 9 ; 4 ; ...` report
/// only, so an iTerm2-style `OSC 9 ; message` notification cannot overwrite
/// it.
///
/// - `latest_title` - last title, sanitized. An empty title (e.g.
///   `\x1b]0;\x07`) or a reset clears the stored value.
/// - `latest_progress` - last OSC 9;4 report, whatever its state (a `Remove`
///   report is evidence too).
///
/// [`apply_terminal_updates`]: AgentOscStateTracker::apply_terminal_updates
#[derive(Debug, Default)]
pub(super) struct AgentOscStateTracker {
    latest_title: Option<String>,
    terminal_title: Option<String>,
    latest_progress: Option<Progress>,
}

impl AgentOscStateTracker {
    /// Collects the title and progress changes the terminal core saw since
    /// the last call. Returns whether the displayed title changed.
    pub(super) fn apply_terminal_updates(
        &mut self,
        effects: &mut shepr_vt::TerminalEffects,
    ) -> bool {
        let mut terminal_title_changed = false;
        if let Some(update) = effects.title_update.take() {
            let title = match update {
                shepr_vt::TitleUpdate::Set(title) => Some(sanitize_agent_osc_string(
                    title.as_bytes(),
                    AGENT_OSC_MAX_CHARS,
                ))
                .filter(|title| !title.is_empty()),
                shepr_vt::TitleUpdate::Reset => None,
            };
            terminal_title_changed = self.terminal_title != title;
            self.terminal_title.clone_from(&title);
            self.latest_title = title;
        }
        if let Some(progress) = effects.progress_update.take() {
            self.latest_progress = Some(progress);
        }
        terminal_title_changed
    }

    pub(super) fn terminal_title(&self) -> Option<&str> {
        self.terminal_title.as_deref()
    }

    /// The latest retained OSC title; `None` if none has been seen or the
    /// last title was an empty clear.
    pub(super) fn latest_title(&self) -> Option<&str> {
        self.latest_title.as_deref()
    }

    /// The latest retained OSC 9;4 progress report, if any.
    pub(super) fn latest_progress(&self) -> Option<Progress> {
        self.latest_progress
    }

    /// Drops the retained title and progress so a new foreground agent cannot
    /// inherit OSC evidence emitted by a previous process. The displayed
    /// title is kept, and so is anything the core has not handed over yet: a
    /// title set just before the agent change is attributed to the new agent.
    pub(super) fn clear_retained(&mut self) {
        self.latest_title = None;
        self.latest_progress = None;
    }
}

fn sanitize_agent_osc_string(payload: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(payload);
    let mut out = String::new();
    for ch in text.chars().filter(|ch| !ch.is_control()).take(max_chars) {
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_vt::ProgressState;

    /// A tracker fed the way the pane feeds it: bytes go through the terminal
    /// core, the tracker collects what the core saw.
    struct TrackedTerminal {
        terminal: shepr_vt::Terminal,
        tracker: AgentOscStateTracker,
    }

    impl TrackedTerminal {
        fn new() -> Self {
            Self {
                terminal: shepr_vt::Terminal::new(
                    shepr_core::geometry::PaneGeometry::cells_only(80, 24),
                    shepr_core::scrollback::ScrollbackBudget::new(0),
                ),
                tracker: AgentOscStateTracker::default(),
            }
        }

        fn observe(&mut self, bytes: &[u8]) -> bool {
            self.terminal.write(bytes);
            let mut effects = self.terminal.take_effects();
            self.tracker.apply_terminal_updates(&mut effects)
        }
    }

    fn progress(state: ProgressState, percent: Option<u8>) -> Option<Progress> {
        Some(Progress { state, percent })
    }

    const INDETERMINATE: Option<Progress> = Some(Progress {
        state: ProgressState::Indeterminate,
        percent: None,
    });

    #[test]
    fn agent_osc_osc0_title_with_bel() {
        let mut t = TrackedTerminal::new();
        t.observe("hello\x1b]0;braille title\x07world".as_bytes());
        assert_eq!(t.tracker.latest_title(), Some("braille title"));
        assert_eq!(t.tracker.terminal_title(), Some("braille title"));
        assert_eq!(t.tracker.latest_progress(), None);
    }

    #[test]
    fn agent_osc_osc2_title_with_st() {
        let mut t = TrackedTerminal::new();
        t.observe("hello\x1b]2;static title\x1b\\world".as_bytes());
        assert_eq!(t.tracker.latest_title(), Some("static title"));
        assert_eq!(t.tracker.latest_progress(), None);
    }

    #[test]
    fn agent_osc_empty_osc0_clears_title() {
        let mut t = TrackedTerminal::new();
        // First set a title.
        t.observe(b"\x1b]0;some title\x07");
        assert_eq!(t.tracker.latest_title(), Some("some title"));
        // Then clear it with an empty payload (Codex pattern).
        assert!(t.observe(b"\x1b]0;\x07"));
        assert_eq!(t.tracker.latest_title(), None);
        assert_eq!(t.tracker.terminal_title(), None);
    }

    /// An OSC ends at any ESC, as in the parser; the old byte tracker kept
    /// collecting and produced "foo[m ...]0;bar".
    #[test]
    fn agent_osc_title_ends_where_the_parser_ends_it() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;foo\x1b[m text \x1b]0;bar\x07");
        assert_eq!(t.tracker.latest_title(), Some("bar"));
        t.observe(b"\x1b]0;half\x1b");
        assert_eq!(t.tracker.latest_title(), Some("half"));
    }

    #[test]
    fn agent_osc_title_follows_the_title_stack_and_ris() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]2;shell\x07\x1b[22t");
        assert!(t.observe(b"\x1b]2;vim\x07"));
        assert_eq!(t.tracker.terminal_title(), Some("vim"));
        // vim restores the title it saved when it exits.
        assert!(t.observe(b"\x1b[23t"));
        assert_eq!(t.tracker.terminal_title(), Some("shell"));
        assert_eq!(t.tracker.latest_title(), Some("shell"));
        assert!(t.observe(b"\x1bc"));
        assert_eq!(t.tracker.terminal_title(), None);
    }

    #[test]
    fn clearing_agent_evidence_preserves_the_terminal_title() {
        let mut t = TrackedTerminal::new();
        t.observe("\x1b]2;\u{2733} 修复\u{1F642}标题\x1b\\".as_bytes());

        t.tracker.clear_retained();

        assert_eq!(t.tracker.latest_title(), None);
        assert_eq!(
            t.tracker.terminal_title(),
            Some("\u{2733} 修复\u{1F642}标题")
        );
    }

    #[test]
    fn agent_osc_osc9_sets_progress_with_bel() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3;\x07");
        assert_eq!(t.tracker.latest_progress(), INDETERMINATE);
        assert_eq!(t.tracker.latest_title(), None);
    }

    #[test]
    fn agent_osc_osc9_clear_progress_with_st() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3;\x07");
        assert_eq!(t.tracker.latest_progress(), INDETERMINATE);
        t.observe(b"\x1b]9;4;0;\x1b\\");
        assert_eq!(
            t.tracker.latest_progress(),
            progress(ProgressState::Remove, None)
        );
    }

    #[test]
    fn agent_osc_osc9_notification_does_not_replace_progress() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3;\x07");
        t.observe(b"\x1b]9;Task finished\x07");
        assert_eq!(t.tracker.latest_progress(), INDETERMINATE);
    }

    #[test]
    fn agent_osc_split_sequence_across_chunks() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3");
        assert_eq!(t.tracker.latest_progress(), None);
        t.observe(b";\x07");
        assert_eq!(t.tracker.latest_progress(), INDETERMINATE);
    }

    #[test]
    fn agent_osc_bel_and_st_terminators_both_work() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;title-bel\x07");
        assert_eq!(t.tracker.latest_title(), Some("title-bel"));
        t.observe(b"\x1b]0;title-st\x1b\\");
        assert_eq!(t.tracker.latest_title(), Some("title-st"));
    }

    #[test]
    fn agent_osc_oversized_title_is_capped() {
        let mut t = TrackedTerminal::new();
        let mut oversized = Vec::from(b"\x1b]0;".as_slice());
        oversized.extend(std::iter::repeat_n(b'x', 4097));
        oversized.push(0x07);
        t.observe(&oversized);
        assert_eq!(
            t.tracker.latest_title(),
            Some("x".repeat(AGENT_OSC_MAX_CHARS).as_str())
        );

        t.observe(b"\x1b]0;after\x07");
        assert_eq!(t.tracker.latest_title(), Some("after"));
    }

    #[test]
    fn agent_osc_cap_length_is_respected() {
        let mut t = TrackedTerminal::new();
        // Build a title of AGENT_OSC_MAX_CHARS + 50 ASCII chars.
        let long_title: String = "a".repeat(AGENT_OSC_MAX_CHARS + 50);
        let seq = format!("\x1b]0;{long_title}\x07");
        t.observe(seq.as_bytes());
        assert_eq!(
            t.tracker.latest_title().map(str::len),
            Some(AGENT_OSC_MAX_CHARS)
        );
    }

    #[test]
    fn agent_osc_control_chars_stripped() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;before\x01after\x07");
        assert_eq!(t.tracker.latest_title(), Some("beforeafter"));
    }

    #[test]
    fn agent_osc_unrelated_osc_does_not_overwrite_title() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;my title\x07");
        // OSC 4 (palette color), OSC 52 (clipboard) - should not touch title/progress.
        t.observe(b"\x1b]4;1;rgb:aa/bb/cc\x07");
        t.observe(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(t.tracker.latest_title(), Some("my title"));
        assert_eq!(t.tracker.latest_progress(), None);
    }

    #[test]
    fn agent_osc_interleaved_sequences() {
        let mut t = TrackedTerminal::new();
        // OSC 0 title, then OSC 9 progress, then OSC 2 title update.
        t.observe(b"\x1b]0;first\x07\x1b]9;4;3;\x07\x1b]2;second\x07");
        assert_eq!(t.tracker.latest_title(), Some("second"));
        assert_eq!(t.tracker.latest_progress(), INDETERMINATE);
    }

    #[test]
    fn agent_osc_default_state_is_empty() {
        let t = AgentOscStateTracker::default();
        assert_eq!(t.latest_title(), None);
        assert_eq!(t.latest_progress(), None);
    }
}
