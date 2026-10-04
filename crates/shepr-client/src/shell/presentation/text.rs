use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use shepr_config::theme::Palette;

use unicode_segmentation::UnicodeSegmentation;

pub(in crate::shell) fn render_sidebar_background(
    buffer: &mut Buffer,
    area: Rect,
    palette: &Palette,
) {
    buffer.set_style(area, Style::default().bg(palette.sidebar_bg));
    let separator_x = area.right().saturating_sub(1);
    for y in area.y..area.bottom() {
        if let Some(cell) = buffer.cell_mut((separator_x, y)) {
            cell.set_symbol("│");
            cell.set_style(Style::default().fg(palette.surface_dim));
        }
    }
}

pub(in crate::shell) fn put_right_text(
    buffer: &mut Buffer,
    area: Rect,
    y: u16,
    text: &str,
    style: Style,
) {
    let width = display_width(text).min(area.width);
    put_text(
        buffer,
        area.right().saturating_sub(width),
        y,
        width,
        text,
        style,
    );
}

pub(in crate::shell) fn put_text(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
) -> u16 {
    if width == 0
        || x < buffer.area.left()
        || y < buffer.area.top()
        || y >= buffer.area.bottom()
        || x >= buffer.area.right()
    {
        return 0;
    }
    let limit = usize::from(width.min(buffer.area.right().saturating_sub(x)));
    let mut written = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = rendered_grapheme_width(grapheme);
        if grapheme_width == 0 {
            continue;
        }
        if written.saturating_add(grapheme_width) > limit {
            break;
        }
        let Ok(column_offset) = u16::try_from(written) else {
            break;
        };
        let Some(column) = x.checked_add(column_offset) else {
            break;
        };
        if let Some(cell) = buffer.cell_mut((column, y)) {
            cell.set_symbol(grapheme).set_style(style);
        }
        for offset in 1..grapheme_width {
            let Ok(offset) = u16::try_from(offset) else {
                break;
            };
            let Some(continuation_column) = column.checked_add(offset) else {
                break;
            };
            if let Some(cell) = buffer.cell_mut((continuation_column, y)) {
                cell.set_symbol(" ").set_style(style);
            }
        }
        written = written.saturating_add(grapheme_width);
    }
    u16::try_from(written).unwrap_or(u16::MAX)
}

fn rendered_grapheme_width(grapheme: &str) -> usize {
    if grapheme.contains(char::is_control) {
        0
    } else {
        shepr_term::width::text_width(grapheme)
    }
}

pub(in crate::shell) fn rendered_text_width(text: &str) -> usize {
    text.graphemes(true).fold(0usize, |width, grapheme| {
        width.saturating_add(rendered_grapheme_width(grapheme))
    })
}

pub(in crate::shell) fn display_width(text: &str) -> u16 {
    u16::try_from(rendered_text_width(text)).unwrap_or(u16::MAX)
}

/// `text` cut to at most `max_width` rendered columns, ending in an ellipsis
/// when anything was cut.
pub(in crate::shell) fn truncate_end(text: &str, max_width: usize) -> String {
    if rendered_text_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut prefix = String::new();
    let mut width = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = rendered_text_width(grapheme);
        if width.saturating_add(grapheme_width) > max_width.saturating_sub(1) {
            break;
        }
        prefix.push_str(grapheme);
        width = width.saturating_add(grapheme_width);
    }
    format!("{prefix}…")
}

pub(in crate::shell) fn put_spans(
    buffer: &mut Buffer,
    area: Rect,
    spans: &[ratatui::text::Span<'_>],
    style: Style,
) {
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    buffer.set_style(area, style);
    let mut x = area.x;
    let mut remaining = area.width;
    for span in spans {
        let span_width = rendered_text_width(span.content.as_ref());
        let written = put_text(
            buffer,
            x,
            area.y,
            remaining,
            span.content.as_ref(),
            span.style,
        );
        x = x.saturating_add(written);
        remaining = remaining.saturating_sub(written);
        if usize::from(written) < span_width || remaining == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;

    use ratatui::layout::Rect;
    use ratatui::style::Style;

    use crate::shell::presentation::text::{display_width, put_spans, rendered_text_width};

    #[test]
    fn put_spans_places_emoji_variation_titles_using_output_cell_width() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let spans = [ratatui::text::Span::raw("\u{2764}\u{fe0f}agent")];

        put_spans(&mut buffer, Rect::new(0, 0, 3, 1), &spans, Style::default());

        assert_eq!(buffer[(0, 0)].symbol(), "\u{2764}\u{fe0f}");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), "a");
        assert_eq!(display_width("\u{2764}\u{fe0f}agent"), 7);
    }

    #[test]
    fn put_spans_continues_after_skipping_control_graphemes() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let spans = [
            ratatui::text::Span::raw("agent\u{7}"),
            ratatui::text::Span::raw(" state"),
        ];

        put_spans(
            &mut buffer,
            Rect::new(0, 0, 12, 1),
            &spans,
            Style::default(),
        );

        assert_eq!(buffer[(4, 0)].symbol(), "t");
        assert_eq!(buffer[(6, 0)].symbol(), "s");
        assert_eq!(rendered_text_width("agent\u{7}"), 5);
        assert_eq!(rendered_text_width("\u{301}"), 0);
    }
}
