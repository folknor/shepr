use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
    widgets::{Clear, Paragraph, Widget},
};
use unicode_width::UnicodeWidthStr;

use super::widgets::panel_contrast_fg;
use crate::app::state::Palette;

/// Draws the config diagnostic banner right-aligned from the top of `area`, one row per
/// non-empty message line, and returns the rect bounding every drawn row (empty when nothing
/// was drawn). Its height is the number of rows the banner occupies.
pub(crate) fn render_config_diagnostic_buffer(
    buffer: &mut Buffer,
    area: Rect,
    message: &str,
    palette: &Palette,
) -> Rect {
    let style = Style::default()
        .fg(panel_contrast_fg(palette))
        .bg(palette.yellow)
        .add_modifier(Modifier::BOLD);
    let mut bounds = Rect::default();

    for (row, line) in message
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(area.height as usize)
        .enumerate()
    {
        let text = format!(" {line} ");
        // Columns, not bytes: a non-ASCII message would otherwise get a banner wider than
        // its text, padded with blank highlighted cells.
        let width = u16::try_from(UnicodeWidthStr::width(text.as_str()))
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
        bounds = if bounds.is_empty() {
            diagnostic_area
        } else {
            bounds.union(diagnostic_area)
        };
    }

    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_width_follows_display_columns_not_bytes() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(area);
        // "é" is two bytes but one column; the banner must be exactly as wide as the text.
        let drawn = render_config_diagnostic_buffer(&mut buffer, area, "ééé\nok", &palette);
        assert_eq!(drawn, Rect::new(35, 0, 5, 2));
        assert_eq!(buffer[(34, 0)].style().bg, buffer[(0, 2)].style().bg);
        assert_eq!(buffer[(35, 0)].style().bg, Some(palette.yellow));
        assert_eq!(buffer[(36, 1)].style().bg, Some(palette.yellow));
        assert_eq!(buffer[(35, 1)].style().bg, buffer[(0, 2)].style().bg);
    }
}
