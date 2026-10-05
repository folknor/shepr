use crate::shell::palette::Palette;
use ratatui::style::Modifier;
use ratatui::style::Style;

#[derive(Clone, Copy)]
pub(in crate::shell) struct StatusGlyph {
    pub(in crate::shell) text: &'static str,
    pub(in crate::shell) style: Style,
}

pub(in crate::shell) fn status_glyph(
    status: shepr_protocol::AgentStatus,
    palette: &Palette,
    stale: bool,
) -> StatusGlyph {
    use shepr_protocol::AgentStatus;
    let text = match status {
        AgentStatus::Idle => "○",
        AgentStatus::Blocked => "×",
        AgentStatus::Working => "◐",
    };
    let color = if stale {
        palette.overlay0
    } else {
        match status {
            AgentStatus::Working => palette.yellow,
            AgentStatus::Blocked => palette.red,
            AgentStatus::Idle => palette.green,
        }
    };
    StatusGlyph {
        text,
        style: Style::default().fg(color).add_modifier(if stale {
            Modifier::DIM
        } else {
            Modifier::empty()
        }),
    }
}

pub(in crate::shell) fn status_priority(status: shepr_protocol::AgentStatus) -> u8 {
    status.attention_rank()
}

pub(in crate::shell) fn status_text(status: shepr_protocol::AgentStatus) -> &'static str {
    status.label()
}

pub(in crate::shell) fn panel_contrast_fg(palette: &Palette) -> ratatui::style::Color {
    match palette.panel_bg {
        ratatui::style::Color::Reset => palette.surface_dim,
        color => color,
    }
}
