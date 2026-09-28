use super::*;

#[derive(Clone, Copy)]
pub(super) struct StatusGlyph {
    pub(super) text: &'static str,
    pub(super) style: Style,
}

pub(super) fn status_glyph(
    status: shepr_api::schema::AgentStatus,
    indicator_style: shepr_config::StatusIndicatorStyle,
    palette: &Palette,
    stale: bool,
) -> StatusGlyph {
    use shepr_api::schema::AgentStatus;
    use shepr_config::StatusIndicatorStyle;
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

pub(super) fn status_priority(status: shepr_api::schema::AgentStatus) -> u8 {
    use shepr_api::schema::AgentStatus;
    let state = match status {
        AgentStatus::Blocked => shepr_agent::detect::AgentState::Blocked,
        AgentStatus::Working => shepr_agent::detect::AgentState::Working,
        AgentStatus::Idle => shepr_agent::detect::AgentState::Idle,
    };
    state.attention_rank()
}

pub(super) fn status_text(status: shepr_api::schema::AgentStatus) -> &'static str {
    use shepr_api::schema::AgentStatus;
    match status {
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Idle => "idle",
    }
}

pub(super) fn panel_contrast_fg(palette: &Palette) -> ratatui::style::Color {
    match palette.panel_bg {
        ratatui::style::Color::Reset => palette.surface_dim,
        color => color,
    }
}
