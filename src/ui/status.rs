use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
    widgets::{Clear, Paragraph, Widget},
};

use super::widgets::panel_contrast_fg;
use crate::app::state::Palette;

pub(crate) fn render_config_diagnostic_buffer(
    buffer: &mut Buffer,
    area: Rect,
    message: &str,
    palette: &Palette,
) -> u16 {
    let style = Style::default()
        .fg(panel_contrast_fg(palette))
        .bg(palette.yellow)
        .add_modifier(Modifier::BOLD);
    let mut rendered_rows = 0u16;

    for (row, line) in message
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(area.height as usize)
        .enumerate()
    {
        let text = format!(" {line} ");
        let width = u16::try_from(text.len())
            .unwrap_or(u16::MAX)
            .min(area.width);
        let diagnostic_area = Rect::new(
            area.x + area.width.saturating_sub(width),
            area.y + u16::try_from(row).unwrap_or(u16::MAX),
            width,
            1,
        );

        Clear.render(diagnostic_area, buffer);
        Paragraph::new(Span::styled(text, style)).render(diagnostic_area, buffer);
        rendered_rows = rendered_rows.saturating_add(1);
    }

    rendered_rows
}
