use crate::limits::{AGENT_OSC_MAX_CHARS, MAX_OSC_BODY_BYTES, MAX_OSC_DEBUG_CHARS};
use std::path::PathBuf;

use tracing::info;

use shepr_core::layout::PaneId;

use super::terminal::PaneTerminalCore;

pub(super) fn parse_reported_cwd(value: &[u8]) -> Option<PathBuf> {
    let value = std::str::from_utf8(value).ok()?.trim();
    if value.starts_with("file://") {
        return parse_file_uri_cwd(value);
    }
    let path = value.trim_matches('"');
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Collects complete OSC bodies from a raw byte stream for the opt-in OSC
/// debug log. Its framing mirrors the pinned `vte` crate's parser (its source
/// is in the cargo registry at `~/.cargo/registry/src/*/vte-<version>/`,
/// version per `Cargo.lock`), so it
/// sees the same sequences the terminal does: an OSC ends at BEL, CAN, SUB
/// or any ESC (the ESC of an `ESC \` terminator, or one that starts a new
/// sequence); other C0 controls inside it are dropped; SOS/PM/APC and DCS
/// strings end at ESC, CAN or SUB.
#[derive(Debug, Default)]
struct OscStreamCollector {
    state: OscStreamState,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OscStreamState {
    #[default]
    Ground,
    Escape,
    Body,
    IgnoringString,
    Discarding,
}

impl OscStreamCollector {
    fn observe(&mut self, bytes: &[u8], mut receive: impl FnMut(&[u8])) {
        let mut cursor = 0;
        while cursor < bytes.len() {
            let rest = &bytes[cursor..];
            let skip = match self.state {
                OscStreamState::Ground => rest.iter().position(|&byte| byte == 0x1b),
                OscStreamState::IgnoringString => rest
                    .iter()
                    .position(|&byte| matches!(byte, 0x1b | 0x18 | 0x1a)),
                OscStreamState::Discarding => rest
                    .iter()
                    .position(|&byte| matches!(byte, 0x07 | 0x1b | 0x18 | 0x1a)),
                OscStreamState::Escape | OscStreamState::Body => Some(0),
            };
            let Some(skip) = skip else {
                break;
            };
            cursor += skip;
            let byte = bytes[cursor];
            cursor += 1;
            match self.state {
                // Only ESC stops the ground scan.
                OscStreamState::Ground => self.state = OscStreamState::Escape,
                OscStreamState::Escape => match byte {
                    b']' => {
                        self.body.clear();
                        self.state = OscStreamState::Body;
                    }
                    b'P' | b'X' | b'^' | b'_' => self.state = OscStreamState::IgnoringString,
                    0x18 | 0x1a => self.state = OscStreamState::Ground,
                    // ESC ESC restarts the escape; other C0 controls execute
                    // in place; DEL and high bytes are ignored.
                    0x00..=0x1f | 0x7f..=0xff => {}
                    _ => self.state = OscStreamState::Ground,
                },
                OscStreamState::Body => match byte {
                    0x07 | 0x18 | 0x1a => self.finish(&mut receive, OscStreamState::Ground),
                    0x1b => self.finish(&mut receive, OscStreamState::Escape),
                    0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => {}
                    _ => self.push(byte),
                },
                OscStreamState::IgnoringString | OscStreamState::Discarding => {
                    self.state = if byte == 0x1b {
                        OscStreamState::Escape
                    } else {
                        OscStreamState::Ground
                    };
                }
            }
        }
    }

    fn push(&mut self, byte: u8) {
        self.body.push(byte);
        if self.body.len() > MAX_OSC_BODY_BYTES {
            self.body.clear();
            self.state = OscStreamState::Discarding;
        }
    }

    fn finish(&mut self, receive: &mut impl FnMut(&[u8]), next: OscStreamState) {
        receive(&self.body);
        self.body.clear();
        self.state = next;
    }
}

/// Retains the latest window title and OSC 9;4 progress payload emitted by
/// the child process, for agent detection, `detect.explain` and the pane
/// title. Both come from the terminal core ([`apply_terminal_updates`]):
/// the title is whatever the parser made of OSC 0/2, the CSI 22/23 t title
/// stack and RIS; progress is the ConEmu `OSC 9 ; 4 ; ...` payload only, so
/// an iTerm2-style `OSC 9 ; message` notification cannot overwrite it.
///
/// - `latest_title` - last title, sanitized. An empty title (e.g.
///   `\x1b]0;\x07`) or a reset clears the stored value.
/// - `latest_progress` - last OSC 9;4 payload (the part after `9;`), stored
///   as-is after sanitization. E.g. `"4;3;"` or `"4;0;"`.
///
/// [`apply_terminal_updates`]: AgentOscStateTracker::apply_terminal_updates
#[derive(Debug, Default)]
pub(super) struct AgentOscStateTracker {
    latest_title: Option<String>,
    terminal_title: Option<String>,
    latest_progress: Option<String>,
}

impl AgentOscStateTracker {
    /// Collects the title and progress changes the terminal core saw since
    /// the last call. Returns whether the displayed title changed.
    pub(super) fn apply_terminal_updates(&mut self, terminal: &mut shepr_vt::Terminal) -> bool {
        let mut terminal_title_changed = false;
        if let Some(update) = terminal.take_title_update() {
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
        if let Some(progress) = terminal.take_progress_update() {
            self.latest_progress =
                Some(sanitize_agent_osc_string(&progress.0, AGENT_OSC_MAX_CHARS));
        }
        terminal_title_changed
    }

    pub(super) fn terminal_title(&self) -> Option<&str> {
        self.terminal_title.as_deref()
    }

    /// Returns the latest retained OSC title, or `""` if none has been seen or
    /// the last title was an empty clear.
    pub(super) fn latest_title(&self) -> &str {
        self.latest_title.as_deref().unwrap_or("")
    }

    /// Returns the latest retained OSC 9;4 progress payload, or `""` if none.
    pub(super) fn latest_progress(&self) -> &str {
        self.latest_progress.as_deref().unwrap_or("")
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

/// Reconstructs selected OSC sequences for local evidence capture while
/// debugging agent title/status behavior. This is intentionally passive:
/// nothing here affects terminal rendering or detection state. Off unless
/// `SHEPR_DEBUG_OSC_EVIDENCE` is set, in which case it is the one extra scan
/// of each PTY read besides the terminal core's.
#[derive(Debug)]
pub(super) struct OscDebugTracker {
    enabled: bool,
    collector: OscStreamCollector,
    pending: Vec<OscDebugEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OscDebugEvent {
    pub(super) command: String,
    pub(super) payload: String,
}

impl OscDebugTracker {
    pub(super) fn from_env() -> Self {
        Self {
            enabled: osc_debug_enabled_from_env(),
            collector: OscStreamCollector::default(),
            pending: Vec::new(),
        }
    }

    pub(super) fn observe(&mut self, bytes: &[u8]) {
        if !self.enabled {
            return;
        }
        let (collector, pending) = (&mut self.collector, &mut self.pending);
        collector.observe(bytes, |body| {
            if let Some(event) = parse_osc_debug_event(body) {
                pending.push(event);
            }
        });
    }

    pub(super) fn drain_pending(&mut self) -> Vec<OscDebugEvent> {
        std::mem::take(&mut self.pending)
    }
}

impl Default for OscDebugTracker {
    fn default() -> Self {
        Self::from_env()
    }
}

/// `SHEPR_DEBUG_OSC_EVIDENCE`, read once per process under the environment
/// policy (exactly `1`, `0`, `true` or `false`). Pane construction has no
/// error path, so this optional debug-only capture flag warns and fails closed
/// when refused; it cannot affect pane behavior.
/// Pane runtime construction takes no launch-resolved settings from the
/// server, so this is read at the first pane rather than at server startup.
/// Pane children never see the variable (`pane::launch` scrubs it).
fn osc_debug_enabled_from_env() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        shepr_core::env::read_flag(shepr_core::env::EnvVar::SheprDebugOscEvidence)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "OSC evidence capture stays off");
                None
            })
            .unwrap_or(false)
    })
}

fn parse_osc_debug_event(body: &[u8]) -> Option<OscDebugEvent> {
    let separator = body.iter().position(|byte| *byte == b';')?;
    let command = &body[..separator];
    let payload = &body[separator + 1..];
    if !matches!(command, b"0" | b"2" | b"9" | b"21337") {
        return None;
    }
    Some(OscDebugEvent {
        command: std::str::from_utf8(command).ok()?.to_string(),
        payload: sanitized_osc_debug_payload(payload),
    })
}

fn sanitized_osc_debug_payload(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    let mut sanitized = String::new();
    let mut visible_chars = text.chars().filter(|ch| !ch.is_control());
    for ch in visible_chars.by_ref().take(MAX_OSC_DEBUG_CHARS) {
        sanitized.push(ch);
    }
    if visible_chars.next().is_some() {
        sanitized.push_str("...");
    }
    sanitized
}

fn parse_file_uri_cwd(uri: &str) -> Option<PathBuf> {
    // Standard shell integrations (vte.sh for bash/zsh, fish) report
    // `file://$HOSTNAME/path`, so the machine's own name must be accepted.
    // Looked up per report rather than cached: OSC 7 arrives about once per
    // prompt, and a renamed host keeps matching.
    parse_file_uri_cwd_for_host(uri, shepr_platform::hostname().as_deref())
}

/// Parse a `file://` cwd report, accepting an empty host, `localhost`, or
/// `local_host`. Any other host is a different machine (for example a shell
/// reached over SSH inside the pane), whose path means nothing here.
fn parse_file_uri_cwd_for_host(uri: &str, local_host: Option<&str>) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else if let Some(slash) = rest.find('/') {
        let host = &rest[..slash];
        if !(host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || local_host.is_some_and(|local| is_same_host(host, local)))
        {
            return None;
        }
        &rest[slash..]
    } else {
        rest
    };
    let path = percent_decode_utf8(path)?;
    Some(PathBuf::from(path))
}

/// Host names compare case-insensitively. A reported short name may match a
/// locally known qualified name. A reported qualified name cannot be checked
/// against only a local short name: its domain could name another machine.
fn is_same_host(reported: &str, local: &str) -> bool {
    fn short(name: &str) -> &str {
        name.split('.').next().unwrap_or(name)
    }
    reported.eq_ignore_ascii_case(local)
        || (!reported.contains('.') && reported.eq_ignore_ascii_case(short(local)))
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = *bytes.get(idx + 1)?;
            let lo = *bytes.get(idx + 2)?;
            output.push(hex_value(hi)? * 16 + hex_value(lo)?);
            idx += 3;
        } else {
            output.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn foreground_job_is_shell(job: &shepr_agent::detect::ForegroundJob, shell_pid: u32) -> bool {
    job.processes.iter().any(|process| process.pid == shell_pid)
}

/// The process group of the foreground program when it is not the shell.
/// Scans `/proc`: never call it with the terminal lock held.
pub(super) fn current_transient_default_color_owner(shell_pid: u32) -> Option<u32> {
    let job = shepr_agent::detect::foreground_job(shell_pid)?;
    (!foreground_job_is_shell(&job, shell_pid)).then_some(job.process_group_id)
}

pub(super) fn should_restore_host_terminal_theme(
    owner_pgid: u32,
    shell_pid: u32,
    alternate_screen: bool,
    foreground_job: Option<&shepr_agent::detect::ForegroundJob>,
) -> bool {
    if alternate_screen {
        return false;
    }

    let Some(foreground_job) = foreground_job else {
        return false;
    };

    foreground_job.process_group_id != owner_pgid
        && foreground_job_is_shell(foreground_job, shell_pid)
}

/// Once the program that overrode the default colours has left the
/// foreground, drops its OSC 10/11 overrides so the host theme shows again.
/// This clears the core's override slots directly; nothing is written into
/// the child's byte stream.
pub(super) fn restore_host_terminal_theme_if_needed(
    core: &mut PaneTerminalCore,
    pane_id: PaneId,
    shell_pid: u32,
    alternate_screen: bool,
    foreground_job: Option<&shepr_agent::detect::ForegroundJob>,
) -> bool {
    let Some(owner_pgid) = core.transient_default_color_owner_pgid else {
        return false;
    };
    if core.host_terminal_theme.is_empty() {
        return false;
    }
    if !should_restore_host_terminal_theme(owner_pgid, shell_pid, alternate_screen, foreground_job)
    {
        return false;
    }

    core.transient_default_color_owner_pgid = None;
    core.terminal.reset_default_color_overrides();
    info!(
        pane = pane_id.raw(),
        owner_pgid, "restored host terminal default colors after transient override"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_osc_scan_matches_bytewise_state() {
        let mut input = b"text\x1b_Gm=1;".to_vec();
        input.extend(std::iter::repeat_n(b'A', 8192));
        input.extend_from_slice(b"\x1b\\\x1b]10;?\x07\x1b]11;red\x1b\\\x1bPignored");
        input.extend(0u8..=255);
        input.extend_from_slice(b"\x1b\\\x1b]12;");
        input.extend(std::iter::repeat_n(b'B', 4200));
        input.extend_from_slice(b"\x07\x1b]10;?\x1b\\\x1b]0;a\x18\x1b]11;?\x07\x1b");
        for chunk_size in [1, 2, 3, 17, 4096, input.len()] {
            let mut bulk_stream = OscStreamCollector::default();
            let mut scalar_stream = OscStreamCollector::default();
            for chunk in input.chunks(chunk_size) {
                let mut expected_bodies = Vec::new();
                for byte in chunk {
                    scalar_stream.observe(std::slice::from_ref(byte), |body| {
                        expected_bodies.push(body.to_vec());
                    });
                }
                let mut bodies = Vec::new();
                bulk_stream.observe(chunk, |body| bodies.push(body.to_vec()));
                assert_eq!(bodies, expected_bodies);
                assert_eq!(
                    (bulk_stream.state, &bulk_stream.body),
                    (scalar_stream.state, &scalar_stream.body)
                );
            }
        }
    }

    fn pane_default_theme(
        pane: &super::super::PaneTerminal,
    ) -> shepr_termio::host_term::theme::TerminalTheme {
        let mut core = shepr_vt::lock_terminal_core(&pane.core).expect("test precondition");
        let super::super::terminal::PaneTerminalCore {
            terminal,
            render_state,
            ..
        } = &mut *core;
        render_state.update(terminal);
        let colors = render_state.colors();
        shepr_termio::host_term::theme::TerminalTheme {
            foreground: Some(shepr_termio::host_term::theme::RgbColor {
                r: colors.foreground.r,
                g: colors.foreground.g,
                b: colors.foreground.b,
            }),
            background: Some(shepr_termio::host_term::theme::RgbColor {
                r: colors.background.r,
                g: colors.background.g,
                b: colors.background.b,
            }),
            ..Default::default()
        }
    }

    fn shell_job(shell_pid: u32) -> shepr_agent::detect::ForegroundJob {
        shepr_agent::detect::ForegroundJob {
            process_group_id: shell_pid,
            processes: vec![shepr_agent::detect::ForegroundProcess {
                pid: shell_pid,
                name: "zsh".to_string(),
                argv: Some(vec!["zsh".to_string()]),
            }],
        }
    }

    fn enabled_osc_debug_tracker() -> OscDebugTracker {
        OscDebugTracker {
            enabled: true,
            collector: OscStreamCollector::default(),
            pending: Vec::new(),
        }
    }

    /// A tracker fed the way the pane feeds it: bytes go through the terminal
    /// core, the tracker collects what the core saw.
    struct TrackedTerminal {
        terminal: shepr_vt::Terminal,
        tracker: AgentOscStateTracker,
    }

    impl TrackedTerminal {
        fn new() -> Self {
            Self {
                terminal: shepr_vt::Terminal::new(80, 24, 0),
                tracker: AgentOscStateTracker::default(),
            }
        }

        fn observe(&mut self, bytes: &[u8]) -> bool {
            self.terminal.write(bytes);
            self.tracker.apply_terminal_updates(&mut self.terminal)
        }
    }

    #[test]
    fn osc_stream_collector_ends_osc_like_the_parser() {
        let mut collector = OscStreamCollector::default();
        let mut bodies = Vec::new();

        collector.observe(
            b"\x1bPignored\x1b]0;not-osc\x07\x1b\\\x1b]9;a\x1b",
            |body| bodies.push(body.to_vec()),
        );
        collector.observe(b"\\\x1b]2;b\x1b[m\x1b]0;c\x18d\x1b]0;e\x01f\x07", |body| {
            bodies.push(body.to_vec());
        });

        // The DCS ends at its ESC, so the OSC after it is real. An ESC ends
        // an OSC whatever follows it, and CAN ends one too.
        assert_eq!(
            bodies,
            vec![
                b"0;not-osc".to_vec(),
                b"9;a".to_vec(),
                b"2;b".to_vec(),
                b"0;c".to_vec(),
                b"0;ef".to_vec(),
            ]
        );
    }

    #[test]
    fn reported_cwd_parses_file_uri_and_bare_paths() {
        assert_eq!(
            parse_reported_cwd(b"file:///tmp/shepr%20repo"),
            Some(std::path::PathBuf::from("/tmp/shepr repo"))
        );
    }

    #[test]
    fn reported_cwd_rejects_invalid_or_empty_values() {
        assert_eq!(parse_reported_cwd(b""), None);
        assert_eq!(parse_reported_cwd(b"\xff"), None);
        assert_eq!(
            parse_file_uri_cwd_for_host("file://remote/tmp", Some("workstation")),
            None
        );
        assert_eq!(parse_file_uri_cwd_for_host("file://remote/tmp", None), None);
    }

    #[test]
    fn reported_cwd_accepts_the_machines_own_hostname() {
        let expected = Some(std::path::PathBuf::from("/home/me/src"));
        for (uri, local) in [
            ("file://workstation/home/me/src", "workstation"),
            ("file://WorkStation/home/me/src", "workstation"),
            ("file://workstation/home/me/src", "workstation.lan"),
            ("file://localhost/home/me/src", "workstation"),
        ] {
            assert_eq!(
                parse_file_uri_cwd_for_host(uri, Some(local)),
                expected,
                "{uri} on {local}"
            );
        }
        assert_eq!(
            parse_file_uri_cwd_for_host(
                "file://workstation.other/home/me/src",
                Some("workstation.lan")
            ),
            None
        );
        assert_eq!(
            parse_file_uri_cwd_for_host(
                "file://workstation.other/home/me/src",
                Some("workstation")
            ),
            None
        );
        assert_eq!(
            parse_file_uri_cwd_for_host("file://workstation.lan/home/me/src", Some("workstation")),
            None
        );
    }

    #[test]
    fn reported_cwd_uses_the_live_hostname() {
        let Some(hostname) = shepr_platform::hostname() else {
            return;
        };
        let uri = format!("file://{hostname}/tmp/shepr%20repo");
        assert_eq!(
            parse_reported_cwd(uri.as_bytes()),
            Some(std::path::PathBuf::from("/tmp/shepr repo"))
        );
    }

    // -----------------------------------------------------------------------
    // AgentOscStateTracker tests
    // -----------------------------------------------------------------------

    #[test]
    fn agent_osc_osc0_title_with_bel() {
        let mut t = TrackedTerminal::new();
        t.observe("hello\x1b]0;braille title\x07world".as_bytes());
        assert_eq!(t.tracker.latest_title(), "braille title");
        assert_eq!(t.tracker.terminal_title(), Some("braille title"));
        assert_eq!(t.tracker.latest_progress(), "");
    }

    #[test]
    fn agent_osc_osc2_title_with_st() {
        let mut t = TrackedTerminal::new();
        t.observe("hello\x1b]2;static title\x1b\\world".as_bytes());
        assert_eq!(t.tracker.latest_title(), "static title");
        assert_eq!(t.tracker.latest_progress(), "");
    }

    #[test]
    fn agent_osc_empty_osc0_clears_title() {
        let mut t = TrackedTerminal::new();
        // First set a title.
        t.observe(b"\x1b]0;some title\x07");
        assert_eq!(t.tracker.latest_title(), "some title");
        // Then clear it with an empty payload (Codex pattern).
        assert!(t.observe(b"\x1b]0;\x07"));
        assert_eq!(t.tracker.latest_title(), "");
        assert_eq!(t.tracker.terminal_title(), None);
    }

    /// An OSC ends at any ESC, as in the parser; the old byte tracker kept
    /// collecting and produced "foo[m ...]0;bar".
    #[test]
    fn agent_osc_title_ends_where_the_parser_ends_it() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;foo\x1b[m text \x1b]0;bar\x07");
        assert_eq!(t.tracker.latest_title(), "bar");
        t.observe(b"\x1b]0;half\x1b");
        assert_eq!(t.tracker.latest_title(), "half");
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
        assert_eq!(t.tracker.latest_title(), "shell");
        assert!(t.observe(b"\x1bc"));
        assert_eq!(t.tracker.terminal_title(), None);
    }

    #[test]
    fn clearing_agent_evidence_preserves_the_terminal_title() {
        let mut t = TrackedTerminal::new();
        t.observe("\x1b]2;\u{2733} 修复\u{1F642}标题\x1b\\".as_bytes());

        t.tracker.clear_retained();

        assert_eq!(t.tracker.latest_title(), "");
        assert_eq!(
            t.tracker.terminal_title(),
            Some("\u{2733} 修复\u{1F642}标题")
        );
    }

    #[test]
    fn agent_osc_osc9_sets_progress_with_bel() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3;\x07");
        assert_eq!(t.tracker.latest_progress(), "4;3;");
        assert_eq!(t.tracker.latest_title(), "");
    }

    #[test]
    fn agent_osc_osc9_clear_progress_with_st() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3;\x07");
        assert_eq!(t.tracker.latest_progress(), "4;3;");
        t.observe(b"\x1b]9;4;0;\x1b\\");
        assert_eq!(t.tracker.latest_progress(), "4;0;");
    }

    #[test]
    fn agent_osc_osc9_notification_does_not_replace_progress() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3;\x07");
        t.observe(b"\x1b]9;Task finished\x07");
        assert_eq!(t.tracker.latest_progress(), "4;3;");
    }

    #[test]
    fn agent_osc_split_sequence_across_chunks() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]9;4;3");
        assert_eq!(t.tracker.latest_progress(), "");
        t.observe(b";\x07");
        assert_eq!(t.tracker.latest_progress(), "4;3;");
    }

    #[test]
    fn agent_osc_bel_and_st_terminators_both_work() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;title-bel\x07");
        assert_eq!(t.tracker.latest_title(), "title-bel");
        t.observe(b"\x1b]0;title-st\x1b\\");
        assert_eq!(t.tracker.latest_title(), "title-st");
    }

    #[test]
    fn agent_osc_oversized_title_is_capped() {
        let mut t = TrackedTerminal::new();
        let mut oversized = Vec::from(b"\x1b]0;".as_slice());
        oversized.extend(std::iter::repeat_n(b'x', 4097));
        oversized.push(0x07);
        t.observe(&oversized);
        assert_eq!(t.tracker.latest_title(), "x".repeat(AGENT_OSC_MAX_CHARS));

        t.observe(b"\x1b]0;after\x07");
        assert_eq!(t.tracker.latest_title(), "after");
    }

    #[test]
    fn agent_osc_cap_length_is_respected() {
        let mut t = TrackedTerminal::new();
        // Build a title of AGENT_OSC_MAX_CHARS + 50 ASCII chars.
        let long_title: String = "a".repeat(AGENT_OSC_MAX_CHARS + 50);
        let seq = format!("\x1b]0;{long_title}\x07");
        t.observe(seq.as_bytes());
        assert_eq!(t.tracker.latest_title().len(), AGENT_OSC_MAX_CHARS);
    }

    #[test]
    fn agent_osc_control_chars_stripped() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;before\x01after\x07");
        assert_eq!(t.tracker.latest_title(), "beforeafter");
    }

    #[test]
    fn agent_osc_unrelated_osc_does_not_overwrite_title() {
        let mut t = TrackedTerminal::new();
        t.observe(b"\x1b]0;my title\x07");
        // OSC 4 (palette color), OSC 52 (clipboard) - should not touch title/progress.
        t.observe(b"\x1b]4;1;rgb:aa/bb/cc\x07");
        t.observe(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(t.tracker.latest_title(), "my title");
        assert_eq!(t.tracker.latest_progress(), "");
    }

    #[test]
    fn agent_osc_interleaved_sequences() {
        let mut t = TrackedTerminal::new();
        // OSC 0 title, then OSC 9 progress, then OSC 2 title update.
        t.observe(b"\x1b]0;first\x07\x1b]9;4;3;\x07\x1b]2;second\x07");
        assert_eq!(t.tracker.latest_title(), "second");
        assert_eq!(t.tracker.latest_progress(), "4;3;");
    }

    #[test]
    fn agent_osc_default_state_is_empty() {
        let t = AgentOscStateTracker::default();
        assert_eq!(t.latest_title(), "");
        assert_eq!(t.latest_progress(), "");
    }

    // -----------------------------------------------------------------------
    // OscDebugTracker tests
    // -----------------------------------------------------------------------

    #[test]
    fn osc_debug_tracker_detects_title_with_bel() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe("hello\x1b]0;\u{273B} working title\x07world".as_bytes());

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "0".to_string(),
                payload: "\u{273B} working title".to_string(),
            }]
        );
    }

    #[test]
    fn osc_debug_tracker_detects_title_with_st() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe("hello\x1b]2;static title\x1b\\world".as_bytes());

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "2".to_string(),
                payload: "static title".to_string(),
            }]
        );
    }

    #[test]
    fn osc_debug_tracker_detects_split_status_sequences() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe(b"\x1b]9;4;3");
        assert!(tracker.drain_pending().is_empty());
        tracker.observe(b"\x07\x1b]21337;status=working\x1b\\");

        assert_eq!(
            tracker.drain_pending(),
            vec![
                OscDebugEvent {
                    command: "9".to_string(),
                    payload: "4;3".to_string(),
                },
                OscDebugEvent {
                    command: "21337".to_string(),
                    payload: "status=working".to_string(),
                },
            ]
        );
    }

    #[test]
    fn osc_debug_tracker_ignores_untracked_osc_commands() {
        let mut tracker = enabled_osc_debug_tracker();

        tracker.observe(b"\x1b]52;c;SGVsbG8=\x07\x1b]7;file:///tmp\x07");

        assert!(tracker.drain_pending().is_empty());
    }

    #[test]
    fn osc_debug_tracker_sanitizes_control_characters() {
        let mut tracker = enabled_osc_debug_tracker();

        // The parser drops C0 controls inside an OSC; so does the collector.
        tracker.observe(b"\x1b]0;before\x01after\x07");

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "0".to_string(),
                payload: "beforeafter".to_string(),
            }]
        );
    }

    #[test]
    fn osc_debug_tracker_recovers_after_oversized_payload() {
        let mut tracker = enabled_osc_debug_tracker();
        let oversized = vec![b'a'; 4097];

        tracker.observe(b"\x1b]0;");
        tracker.observe(&oversized);
        tracker.observe(b"\x07\x1b]0;ok\x07");

        assert_eq!(
            tracker.drain_pending(),
            vec![OscDebugEvent {
                command: "0".to_string(),
                payload: "ok".to_string(),
            }]
        );
    }

    #[test]
    fn host_theme_restore_waits_for_shell_and_non_alternate_screen() {
        assert!(!should_restore_host_terminal_theme(
            42,
            7,
            true,
            Some(&shell_job(7)),
        ));
        assert!(!should_restore_host_terminal_theme(42, 7, false, None));
        assert!(!should_restore_host_terminal_theme(
            42,
            7,
            false,
            Some(&shepr_agent::detect::ForegroundJob {
                process_group_id: 42,
                processes: vec![shepr_agent::detect::ForegroundProcess {
                    pid: 42,
                    name: "droid".to_string(),
                    argv: Some(vec!["droid".to_string()]),
                }],
            }),
        ));
        assert!(should_restore_host_terminal_theme(
            42,
            7,
            false,
            Some(&shell_job(7)),
        ));

        assert!(!should_restore_host_terminal_theme(
            7,
            7,
            false,
            Some(&shell_job(7)),
        ));
    }

    #[test]
    fn restore_host_terminal_theme_reapplies_cached_colors() {
        let terminal = shepr_vt::Terminal::new(80, 24, 0);
        let pane = super::super::PaneTerminal::new(terminal);
        let pane_id = shepr_test_fixtures::fixed_pane_id(1);
        let shell_pid = 7;
        let host_theme = shepr_termio::host_term::theme::TerminalTheme {
            foreground: Some(shepr_termio::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(shepr_termio::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };

        pane.apply_host_terminal_theme(host_theme);
        {
            let mut core = shepr_vt::lock_terminal_core(&pane.core).expect("test precondition");
            core.transient_default_color_owner_pgid = Some(42);
            core.terminal
                .write(b"\x1b]10;rgb:01/02/03\x1b\\\x1b]11;rgb:dd/ee/ff\x1b\\");
        }
        assert_eq!(
            pane_default_theme(&pane).background,
            Some(shepr_termio::host_term::theme::RgbColor {
                r: 0xdd,
                g: 0xee,
                b: 0xff,
            })
        );

        {
            let mut core = shepr_vt::lock_terminal_core(&pane.core).expect("test precondition");
            // The child is mid-sequence when the restore runs: nothing may be
            // written into its stream.
            core.terminal.write(b"\x1b[3");
            assert!(restore_host_terminal_theme_if_needed(
                &mut core,
                pane_id,
                shell_pid,
                false,
                Some(&shell_job(shell_pid)),
            ));
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
