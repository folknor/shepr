use crate::endpoint::ClientEndpointStatus;
use crate::shell::notices::ClientEndpointNoticeKind;
use crate::shell::notices::ClientVisibleEndpointNotice;
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::endpoint::ClientEndpointId;
use crate::shell::endpoints::endpoint_status_presentation;
use ratatui::layout::Rect;
use shepr_config::theme::Palette;

use ratatui::{
    style::Color,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

/// A closed reason for endpoint-unavailable text. The endpoint label and the
/// sentence are composed here so callers cannot quietly invent competing wording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EndpointNoticeKind {
    NotReady,
    WorkspaceNoLongerAvailable,
    AgentNoLongerAvailable,
    WaitingForSelection(Option<ClientEndpointStatus>),
    StatusFailure(String),
    MoveRejected(String),
    MoveSurfaceTimedOut,
    ConnectionLost(&'static str),
    MoveInterrupted(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndpointNotice {
    pub(crate) endpoint: ClientEndpointId,
    pub(crate) kind: EndpointNoticeKind,
}

impl EndpointNotice {
    pub(crate) fn new(endpoint: ClientEndpointId, kind: EndpointNoticeKind) -> Self {
        Self { endpoint, kind }
    }

    /// The notice's sentence. `local` is the local server's label, which names the
    /// local endpoint.
    pub(crate) fn body(&self, local: &shepr_config::MachineLabel) -> String {
        let label = self.endpoint.display_label(local);
        match &self.kind {
            EndpointNoticeKind::NotReady => format!("{label} is not ready"),
            EndpointNoticeKind::WorkspaceNoLongerAvailable => {
                "Workspace is no longer available; select a connected workspace".to_owned()
            }
            EndpointNoticeKind::AgentNoLongerAvailable => {
                "Agent is no longer available; select a connected agent".to_owned()
            }
            EndpointNoticeKind::WaitingForSelection(status) => match status {
                Some(ClientEndpointStatus::Connecting) => {
                    format!("{label} is connecting; selection will resume when it is ready")
                }
                Some(ClientEndpointStatus::Reconnecting) => {
                    format!("{label} is reconnecting; selection will resume when it is ready")
                }
                Some(ClientEndpointStatus::Attention) => format!("{label} needs attention"),
                Some(ClientEndpointStatus::Online) | None => format!(
                    "{label} is waiting for its workspace snapshot; selection will resume when it is ready"
                ),
            },
            EndpointNoticeKind::StatusFailure(reason)
            | EndpointNoticeKind::MoveRejected(reason) => {
                format!("{label}: {reason}")
            }
            EndpointNoticeKind::MoveSurfaceTimedOut => {
                format!("{label} did not produce a coherent surface in time")
            }
            EndpointNoticeKind::ConnectionLost(reason) => format!("{label} {reason}"),
            EndpointNoticeKind::MoveInterrupted(reason) => {
                format!("machine switch interrupted: {label} {reason}")
            }
        }
    }
}

/// Where a notice card anchored to the top-right corner of `area` goes. Empty when `area`
/// is.
fn notification_card_rect(
    area: Rect,
    title: &str,
    body: &str,
    top_offset: u16,
    body_row_limit: Option<usize>,
) -> Rect {
    if area.is_empty() {
        return Rect::default();
    }
    // Explicit machine diagnostic cards can grow for their full body. Automatic notices show a
    // preview capped below, while the machine badge can reopen the complete diagnostic. A click
    // anywhere on the card dismisses it.
    let content_width = body
        .lines()
        .map(shepr_term::width::text_width)
        .chain([shepr_term::width::text_width(title)])
        .max()
        .unwrap_or(0)
        .saturating_add(6);
    let width = u16::try_from(content_width)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let desired_height = if body.is_empty() {
        3
    } else {
        let body_rows = Paragraph::new(body.lines().map(Line::from).collect::<Vec<_>>())
            .wrap(Wrap { trim: false })
            .line_count(width.saturating_sub(4));
        let body_rows = body_row_limit.map_or(body_rows, |limit| body_rows.min(limit));
        u16::try_from(body_rows.saturating_add(3)).unwrap_or(u16::MAX)
    };
    let available_height = area.height.saturating_sub(top_offset.min(area.height));
    let height = desired_height.min(available_height.max(1));
    let x = area.right().saturating_sub(width);
    let max_y = area.bottom().saturating_sub(height).max(area.y);
    let y = area.y.saturating_add(top_offset).clamp(area.y, max_y);
    Rect::new(x, y, width, height)
}

/// Draws a notice card into `rect`, which `notification_card_rect` placed.
fn draw_notification_card(
    buffer: &mut Buffer,
    rect: Rect,
    title: &str,
    body: &str,
    dot_color: Color,
    palette: &Palette,
) {
    if rect.is_empty() {
        return;
    }
    let body_paragraph = Paragraph::new(
        body.lines()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(palette.overlay0))))
            .collect::<Vec<_>>(),
    )
    .wrap(Wrap { trim: false });
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
}

/// Where the lifecycle banner goes: the first row of `area`, right-aligned. Empty when
/// nothing is drawn.
pub(in crate::shell) fn lifecycle_banner_rect(
    area: Rect,
    label: &str,
    status: ClientEndpointStatus,
    palette: &Palette,
) -> Rect {
    if area.is_empty() || status == ClientEndpointStatus::Online {
        return Rect::default();
    }
    let (symbol, state, _) = endpoint_status_presentation(status, palette);
    let text = format!("{symbol} {label} · {state}");
    let width = u16::try_from(shepr_term::width::text_width(&text) + 2)
        .unwrap_or(u16::MAX)
        .min(area.width);
    Rect::new(area.right().saturating_sub(width), area.y, width, 1)
}

/// Draws the lifecycle banner into `rect`, which `lifecycle_banner_rect` placed.
pub(in crate::shell) fn draw_lifecycle_banner(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    status: ClientEndpointStatus,
    palette: &Palette,
) {
    if rect.is_empty() {
        return;
    }
    let (symbol, state, color) = endpoint_status_presentation(status, palette);
    let text = format!("{symbol} {label} · {state}");
    Clear.render(rect, buffer);
    buffer.set_style(rect, Style::default().bg(palette.surface0));
    crate::shell::presentation::text::put_text(
        buffer,
        rect.x.saturating_add(1),
        rect.y,
        rect.width.saturating_sub(2),
        &text,
        Style::default().fg(color).bg(palette.surface0),
    );
}

/// Where the notice card goes in `area`, below `top_offset` rows.
pub(in crate::shell) fn notice_card_rect(
    area: Rect,
    notice: &ClientVisibleEndpointNotice,
    top_offset: u16,
) -> Rect {
    notification_card_rect(
        area,
        &notice.title,
        &notice.body,
        top_offset,
        notice.key.code.automatic_body_row_limit(),
    )
}

/// Draws the notice card into `rect`, which `notice_card_rect` placed.
pub(in crate::shell) fn draw_notice_card(
    buffer: &mut Buffer,
    rect: Rect,
    notice: &ClientVisibleEndpointNotice,
    palette: &Palette,
) {
    draw_notification_card(
        buffer,
        rect,
        &notice.title,
        &notice.body,
        match notice.key.kind {
            ClientEndpointNoticeKind::Rejected => palette.red,
            ClientEndpointNoticeKind::Timeout | ClientEndpointNoticeKind::Unavailable => {
                palette.yellow
            }
        },
        palette,
    );
}
