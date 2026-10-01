use super::*;
use ratatui::{
    style::Color,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

/// Draws a notice card anchored to the top-right corner of `area`.
fn render_notification_card(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    body: &str,
    top_offset: u16,
    body_row_limit: Option<usize>,
    dot_color: Color,
    palette: &Palette,
) -> Rect {
    if area.is_empty() {
        return Rect::default();
    }
    // Explicit machine diagnostic cards can grow for their full body. Automatic notices show a
    // preview capped below, while the machine badge can reopen the complete diagnostic. A click
    // anywhere on the card dismisses it.
    let content_width = body
        .lines()
        .map(shepr_termio::blit::text_width)
        .chain([shepr_termio::blit::text_width(title)])
        .max()
        .unwrap_or(0)
        .saturating_add(6);
    let width = u16::try_from(content_width)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let body_paragraph = Paragraph::new(
        body.lines()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(palette.overlay0))))
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false });
    let desired_height = if body.is_empty() {
        3
    } else {
        let body_rows = body_paragraph.line_count(width.saturating_sub(4));
        let body_rows = body_row_limit.map_or(body_rows, |limit| body_rows.min(limit));
        u16::try_from(body_rows.saturating_add(3)).unwrap_or(u16::MAX)
    };
    let available_height = area.height.saturating_sub(top_offset.min(area.height));
    let height = desired_height.min(available_height.max(1));
    let x = area.right().saturating_sub(width);
    let max_y = area.bottom().saturating_sub(height).max(area.y);
    let y = area.y.saturating_add(top_offset).clamp(area.y, max_y);
    let rect = Rect::new(x, y, width, height);
    Clear.render(rect, buffer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.overlay0))
        .style(Style::default().bg(palette.panel_bg));
    let inner = block.inner(rect);
    block.render(rect, buffer);
    Paragraph::new(Line::from(vec![
        Span::styled("●", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(
            title,
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .render(Rect::new(inner.x, inner.y, inner.width, 1), buffer);
    if !body.is_empty() && inner.height > 1 {
        let body_area = Rect::new(
            inner.x.saturating_add(2),
            inner.y + 1,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(1),
        );
        Widget::render(body_paragraph, body_area, buffer);
    }
    rect
}

/// Draws the lifecycle banner on the first row of `area`, right-aligned, and returns the rect
/// it cleared and painted (empty when nothing is drawn).
pub(super) fn render_lifecycle_banner(
    buffer: &mut Buffer,
    area: Rect,
    label: &str,
    status: ClientEndpointStatus,
    palette: &Palette,
) -> Rect {
    if area.is_empty() || status == ClientEndpointStatus::Online {
        return Rect::default();
    }
    let (symbol, state, color) = endpoint_status_presentation(status, palette);
    let text = format!("{symbol} {label} · {state}");
    let width = u16::try_from(shepr_termio::blit::text_width(&text) + 2)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let rect = Rect::new(area.right().saturating_sub(width), area.y, width, 1);
    Clear.render(rect, buffer);
    buffer.set_style(rect, Style::default().bg(palette.surface0));
    super::render::put_text(
        buffer,
        rect.x.saturating_add(1),
        rect.y,
        rect.width.saturating_sub(2),
        &text,
        Style::default().fg(color).bg(palette.surface0),
    );
    rect
}

/// Draws the notice card and returns the rect it cleared and painted.
pub(super) fn render_notice(
    buffer: &mut Buffer,
    area: Rect,
    notice: &ClientVisibleEndpointNotice,
    top_offset: u16,
    palette: &Palette,
) -> Rect {
    render_notification_card(
        buffer,
        area,
        &notice.title,
        &notice.body,
        top_offset,
        (!notice
            .key
            .code
            .starts_with(super::machine_diagnostics::MACHINE_DIAGNOSTIC_NOTICE_PREFIX))
        .then_some(crate::limits::MAX_AUTOMATIC_NOTICE_BODY_ROWS),
        match notice.key.kind {
            ClientEndpointNoticeKind::Rejected => palette.red,
            ClientEndpointNoticeKind::Timeout | ClientEndpointNoticeKind::Unavailable => {
                palette.yellow
            }
        },
        palette,
    )
}
