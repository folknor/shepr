//! The opt-in OSC debug log: which OSC bodies the terminal saw, for local
//! evidence capture while debugging agent title/status behaviour. Passive:
//! nothing here affects terminal rendering or detection state. The bodies come
//! from the terminal core's scanner (`Terminal::set_osc_body_capture`), so the
//! log shows the sequences the terminal saw and nothing else.

use std::fmt;

use shepr_core::layout::PaneId;

/// Largest OSC body the OSC debug log reports; a longer one is skipped, which
/// bounds the log entries built from untrusted terminal output.
const MAX_OSC_BODY_BYTES: usize = 4096;
/// Maximum debug payload characters logged from an OSC body; enough context
/// for diagnosis without allowing a large log entry.
const MAX_OSC_DEBUG_CHARS: usize = 512;

/// The OSC commands the debug log reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OscDebugCommand {
    /// OSC 0: icon name and window title.
    IconAndTitle,
    /// OSC 2: window title.
    Title,
    /// OSC 9: notifications and ConEmu subcommands, progress among them.
    Notification,
    /// OSC 21337: agent status.
    Status,
}

impl OscDebugCommand {
    fn parse(command: &[u8]) -> Option<Self> {
        Some(match command {
            b"0" => Self::IconAndTitle,
            b"2" => Self::Title,
            b"9" => Self::Notification,
            b"21337" => Self::Status,
            _ => return None,
        })
    }

    pub(super) const fn number(self) -> &'static str {
        match self {
            Self::IconAndTitle => "0",
            Self::Title => "2",
            Self::Notification => "9",
            Self::Status => "21337",
        }
    }
}

impl fmt::Display for OscDebugCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.number())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OscDebugEvent {
    pub(super) command: OscDebugCommand,
    pub(super) payload: String,
}

/// `SHEPR_DEBUG_OSC_EVIDENCE`, read once per process under the environment
/// policy (exactly `1`, `0`, `true` or `false`). Pane construction has no
/// error path, so this optional debug-only capture flag warns and fails closed
/// when refused; it cannot affect pane behavior.
/// The flag is not among the settings pane construction takes from the
/// server, so it is read at the first pane rather than at server startup.
/// Pane children never see the variable (`pane::launch` scrubs it).
pub(super) fn enabled() -> bool {
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

/// The reportable events among the OSC bodies a terminal collected. A body
/// past `MAX_OSC_BODY_BYTES` is not reported.
pub(super) fn events(bodies: &[Vec<u8>]) -> Vec<OscDebugEvent> {
    bodies
        .iter()
        .filter(|body| body.len() <= MAX_OSC_BODY_BYTES)
        .filter_map(|body| parse_event(body))
        .collect()
}

/// Logs each event at debug level.
pub(super) fn log(pane_id: PaneId, events: &[OscDebugEvent]) {
    for event in events {
        tracing::debug!(
            pane = %pane_id,
            osc_command = %event.command,
            osc_payload = ?event.payload,
            "agent OSC evidence observed"
        );
    }
}

fn parse_event(body: &[u8]) -> Option<OscDebugEvent> {
    let separator = body.iter().position(|byte| *byte == b';')?;
    let command = OscDebugCommand::parse(&body[..separator])?;
    Some(OscDebugEvent {
        command,
        payload: sanitized_payload(&body[separator + 1..]),
    })
}

fn sanitized_payload(payload: &[u8]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Bodies as a terminal with capture on collects them from `bytes`.
    fn collected(chunks: &[&[u8]]) -> Vec<OscDebugEvent> {
        let mut terminal = shepr_vt::Terminal::new(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
            shepr_core::scrollback::ScrollbackBudget::new(0),
        );
        terminal.set_osc_body_capture(true);
        let mut bodies = Vec::new();
        for chunk in chunks {
            terminal.write(chunk);
            bodies.extend(terminal.take_effects().osc_bodies);
        }
        events(&bodies)
    }

    fn event(command: OscDebugCommand, payload: &str) -> OscDebugEvent {
        OscDebugEvent {
            command,
            payload: payload.to_string(),
        }
    }

    #[test]
    fn detects_title_with_bel() {
        assert_eq!(
            collected(&["hello\x1b]0;\u{273B} working title\x07world".as_bytes()]),
            vec![event(
                OscDebugCommand::IconAndTitle,
                "\u{273B} working title"
            )]
        );
    }

    #[test]
    fn detects_title_with_st() {
        assert_eq!(
            collected(&["hello\x1b]2;static title\x1b\\world".as_bytes()]),
            vec![event(OscDebugCommand::Title, "static title")]
        );
    }

    #[test]
    fn detects_split_status_sequences() {
        assert!(collected(&[b"\x1b]9;4;3"]).is_empty());
        assert_eq!(
            collected(&[b"\x1b]9;4;3", b"\x07\x1b]21337;status=working\x1b\\"]),
            vec![
                event(OscDebugCommand::Notification, "4;3"),
                event(OscDebugCommand::Status, "status=working"),
            ]
        );
    }

    #[test]
    fn ignores_untracked_osc_commands() {
        assert!(collected(&[b"\x1b]52;c;SGVsbG8=\x07\x1b]7;file:///tmp\x07"]).is_empty());
    }

    #[test]
    fn sanitizes_control_characters() {
        // The parser drops C0 controls inside an OSC, and so does the scanner.
        assert_eq!(
            collected(&[b"\x1b]0;before\x01after\x07"]),
            vec![event(OscDebugCommand::IconAndTitle, "beforeafter")]
        );
    }

    #[test]
    fn recovers_after_oversized_payload() {
        let oversized = vec![b'a'; MAX_OSC_BODY_BYTES + 1];
        assert_eq!(
            collected(&[b"\x1b]0;", &oversized, b"\x07\x1b]0;ok\x07"]),
            vec![event(OscDebugCommand::IconAndTitle, "ok")]
        );
    }

    #[test]
    fn truncates_long_payloads() {
        let payload = "x".repeat(MAX_OSC_DEBUG_CHARS + 1);
        let body = format!("2;{payload}").into_bytes();
        let events = events(&[body]);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].payload,
            format!("{}...", "x".repeat(MAX_OSC_DEBUG_CHARS))
        );
    }
}
