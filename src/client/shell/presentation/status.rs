use super::*;

#[derive(Clone, Copy)]
pub(super) struct StatusGlyph {
    pub(super) text: &'static str,
    pub(super) style: Style,
}

pub(super) fn status_glyph(
    status: crate::api::schema::AgentStatus,
    indicator_style: crate::config::StatusIndicatorStyle,
    palette: &Palette,
    stale: bool,
) -> StatusGlyph {
    use crate::api::schema::AgentStatus;
    use crate::config::StatusIndicatorStyle;
    let text = match (indicator_style, status) {
        (StatusIndicatorStyle::Dots, AgentStatus::Working | AgentStatus::Blocked) => "●",
        (StatusIndicatorStyle::Dots, AgentStatus::Idle) => "○",
        (StatusIndicatorStyle::Symbols, AgentStatus::Blocked) => "×",
        (StatusIndicatorStyle::Symbols, AgentStatus::Working) => "◐",
        (StatusIndicatorStyle::Symbols, AgentStatus::Idle) => "○",
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

pub(super) fn status_priority(status: crate::api::schema::AgentStatus) -> u8 {
    use crate::api::schema::AgentStatus;
    let state = match status {
        AgentStatus::Blocked => crate::detect::AgentState::Blocked,
        AgentStatus::Working => crate::detect::AgentState::Working,
        AgentStatus::Idle => crate::detect::AgentState::Idle,
    };
    state.attention_rank()
}

pub(super) fn status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
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
