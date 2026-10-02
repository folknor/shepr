use ratatui::style::Modifier;
use ratatui::style::Style;
use shepr_config::theme::Palette;

#[derive(Clone, Copy)]
pub(in crate::shell) struct StatusGlyph {
    pub(in crate::shell) text: &'static str,
    pub(in crate::shell) style: Style,
}

pub(in crate::shell) fn status_glyph(
    status: shepr_protocol::AgentStatus,
    indicator_style: shepr_config::StatusIndicatorStyle,
    palette: &Palette,
    stale: bool,
) -> StatusGlyph {
    use shepr_config::StatusIndicatorStyle;
    use shepr_protocol::AgentStatus;
    let text = match (indicator_style, status) {
        (StatusIndicatorStyle::Dots, AgentStatus::Working | AgentStatus::Blocked) => "●",
        (_, AgentStatus::Idle) => "○",
        (StatusIndicatorStyle::Symbols, AgentStatus::Blocked) => "×",
        (StatusIndicatorStyle::Symbols, AgentStatus::Working) => "◐",
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
    use shepr_protocol::AgentStatus;
    let state = match status {
        AgentStatus::Blocked => shepr_agent::detect::AgentState::Blocked,
        AgentStatus::Working => shepr_agent::detect::AgentState::Working,
        AgentStatus::Idle => shepr_agent::detect::AgentState::Idle,
    };
    state.attention_rank()
}

pub(in crate::shell) fn status_text(status: shepr_protocol::AgentStatus) -> &'static str {
    use shepr_protocol::AgentStatus;
    match status {
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Idle => "idle",
    }
}

pub(in crate::shell) fn panel_contrast_fg(palette: &Palette) -> ratatui::style::Color {
    match palette.panel_bg {
        ratatui::style::Color::Reset => palette.surface_dim,
        color => color,
    }
}
