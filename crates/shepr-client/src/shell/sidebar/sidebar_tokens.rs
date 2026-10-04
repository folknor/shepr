use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
};

pub(in crate::shell::sidebar) use super::token_definitions::{
    AgentTokenContext, ResolvedToken, ResolvedTokenKind, SpaceTokenContext,
    agent_rows as sidebar_agent_rows, space_rows as sidebar_space_rows,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::limits::MIN_EXPANDED_SIDEBAR_SECTION_ROWS;
use crate::shell::presentation::text::rendered_text_width;
use shepr_config::theme::Palette;

/// Workspace share of the expanded sidebar, constrained before rendering.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(in crate::shell) struct SectionSplit(shepr_core::layout::SplitRatio);

impl SectionSplit {
    pub(super) const DEFAULT: Self = Self(shepr_core::layout::SplitRatio::EVEN);

    pub(in crate::shell) fn from_drag(value: f32) -> Self {
        Self(shepr_core::layout::SplitRatio::clamped(value))
    }

    pub(in crate::shell) fn get(self) -> f32 {
        self.0.get()
    }
}

/// Whether both sections can keep their minimum height, which the split clamp in
/// `sidebar_section_heights` needs: below it the clamp's bounds would cross.
fn sidebar_sections_can_split(height: u16) -> bool {
    height >= MIN_EXPANDED_SIDEBAR_SECTION_ROWS * 2
}

fn truncate_end(text: &str, max_width: usize) -> String {
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

fn sidebar_section_heights(total_height: u16, split_ratio: SectionSplit) -> (u16, u16) {
    if total_height == 0 {
        return (0, 0);
    }
    if !sidebar_sections_can_split(total_height) {
        let workspace_height = total_height.div_ceil(2);
        return (
            workspace_height,
            total_height.saturating_sub(workspace_height),
        );
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "split_ratio is clamped to its bounds, so the scaled height is non-negative and stays within the source u16 range"
    )]
    let workspace_height = ((total_height as f32) * split_ratio.get()).round() as u16;
    let workspace_height = workspace_height.clamp(
        MIN_EXPANDED_SIDEBAR_SECTION_ROWS,
        total_height.saturating_sub(MIN_EXPANDED_SIDEBAR_SECTION_ROWS),
    );
    (
        workspace_height,
        total_height.saturating_sub(workspace_height),
    )
}

pub(super) fn expanded_sidebar_sections(area: Rect, split_ratio: SectionSplit) -> (Rect, Rect) {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.is_empty() {
        return (Rect::default(), Rect::default());
    }

    let (workspace_height, detail_height) = sidebar_section_heights(content.height, split_ratio);
    (
        Rect::new(content.x, content.y, content.width, workspace_height),
        Rect::new(
            content.x,
            content.y + workspace_height,
            content.width,
            detail_height,
        ),
    )
}

pub(super) fn sidebar_section_divider_rect(area: Rect, split_ratio: SectionSplit) -> Rect {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.width == 0 || !sidebar_sections_can_split(content.height) {
        return Rect::default();
    }

    let (workspace_height, _) = sidebar_section_heights(content.height, split_ratio);
    Rect::new(content.x, content.y + workspace_height, content.width, 1)
}

#[derive(Clone, Copy)]
pub(in crate::shell::sidebar) struct TokenStyles {
    pub(in crate::shell::sidebar) state_text: Style,
    pub(in crate::shell::sidebar) primary: Style,
    pub(in crate::shell::sidebar) secondary: Style,
    pub(in crate::shell::sidebar) terminal_title: Style,
}

pub(in crate::shell::sidebar) fn resolved_token_spans(
    resolved: &[ResolvedToken],
    state_glyph: crate::shell::presentation::status::StatusGlyph,
    styles: TokenStyles,
    palette: &Palette,
    max_width: usize,
) -> Vec<Span<'static>> {
    let fixed_widths = resolved
        .iter()
        .map(|token| match &token.kind {
            ResolvedTokenKind::StateIcon => rendered_text_width(state_glyph.text),
            ResolvedTokenKind::GitStatus { ahead, behind } => {
                usize::from(*ahead > 0) * rendered_text_width(&format!("↑{ahead}"))
                    + usize::from(*behind > 0) * rendered_text_width(&format!("↓{behind}"))
                    + usize::from(*ahead > 0 && *behind > 0)
            }
            _ => 0,
        })
        .collect::<Vec<_>>();
    let flexible_widths = resolved
        .iter()
        .map(|token| match &token.kind {
            ResolvedTokenKind::StateText(text)
            | ResolvedTokenKind::Machine(text)
            | ResolvedTokenKind::Workspace(text)
            | ResolvedTokenKind::Pane(text)
            | ResolvedTokenKind::Agent(text)
            | ResolvedTokenKind::TerminalTitle(text)
            | ResolvedTokenKind::Branch(text) => rendered_text_width(text),
            _ => 0,
        })
        .collect::<Vec<_>>();
    let minimum_width = |active: &[bool]| {
        let indices = active
            .iter()
            .enumerate()
            .filter_map(|(index, active)| active.then_some(index))
            .collect::<Vec<_>>();
        let content = indices
            .iter()
            .map(|index| fixed_widths[*index] + usize::from(flexible_widths[*index] > 0))
            .sum::<usize>();
        let separators = indices
            .windows(2)
            .map(|pair| {
                rendered_text_width(super::token_definitions::separator(
                    &resolved[pair[0]],
                    &resolved[pair[1]],
                ))
            })
            .sum::<usize>();
        content + separators
    };
    let mut active = resolved.iter().map(|_| true).collect::<Vec<_>>();
    if minimum_width(&active) > max_width {
        for (index, width) in flexible_widths.iter().enumerate() {
            if *width > 0 {
                active[index] = false;
            }
        }
        for index in (0..resolved.len()).rev() {
            if flexible_widths[index] == 0 {
                continue;
            }
            active[index] = true;
            if minimum_width(&active) > max_width {
                active[index] = false;
            }
        }
    }
    let visible_indices = active
        .iter()
        .enumerate()
        .filter_map(|(index, active)| active.then_some(index))
        .collect::<Vec<_>>();
    let separator_width = visible_indices
        .windows(2)
        .map(|pair| {
            rendered_text_width(super::token_definitions::separator(
                &resolved[pair[0]],
                &resolved[pair[1]],
            ))
        })
        .sum::<usize>();
    let fixed_width = visible_indices
        .iter()
        .map(|index| fixed_widths[*index])
        .sum::<usize>();
    let mut budgets = flexible_widths
        .iter()
        .enumerate()
        .map(|(index, width)| usize::from(active[index] && *width > 0))
        .collect::<Vec<_>>();
    let minimum = budgets.iter().sum::<usize>();
    let mut remaining = max_width
        .saturating_sub(separator_width + fixed_width)
        .saturating_sub(minimum);
    while remaining > 0 {
        let mut grew = false;
        for (budget, width) in budgets.iter_mut().zip(&flexible_widths) {
            if *budget > 0 && *budget < *width {
                *budget += 1;
                remaining -= 1;
                grew = true;
                if remaining == 0 {
                    break;
                }
            }
        }
        if !grew {
            break;
        }
    }

    let mut spans = Vec::new();
    for (position, index) in visible_indices.iter().copied().enumerate() {
        let token = &resolved[index];
        if position > 0 {
            let previous = &resolved[visible_indices[position - 1]];
            spans.push(Span::styled(
                super::token_definitions::separator(previous, token),
                Style::default().fg(palette.overlay0),
            ));
        }
        match &token.kind {
            ResolvedTokenKind::StateIcon => spans.push(Span::styled(
                state_glyph.text.to_string(),
                apply_token_style(state_glyph.style, token.style),
            )),
            ResolvedTokenKind::StateText(text) => spans.push(Span::styled(
                truncate_end(text, budgets[index]),
                apply_token_style(styles.state_text, token.style),
            )),
            ResolvedTokenKind::Workspace(text) => spans.push(Span::styled(
                truncate_end(text, budgets[index]),
                apply_token_style(styles.primary, token.style),
            )),
            ResolvedTokenKind::Machine(text)
            | ResolvedTokenKind::Pane(text)
            | ResolvedTokenKind::Agent(text)
            | ResolvedTokenKind::Branch(text) => spans.push(Span::styled(
                truncate_end(text, budgets[index]),
                apply_token_style(styles.secondary, token.style),
            )),
            ResolvedTokenKind::GitStatus { ahead, behind } => {
                if *ahead > 0 {
                    spans.push(Span::styled(
                        format!("↑{ahead}"),
                        apply_token_style(Style::default().fg(palette.green), token.style),
                    ));
                }
                if *ahead > 0 && *behind > 0 {
                    spans.push(Span::styled(
                        " ",
                        apply_token_style(Style::default(), token.style),
                    ));
                }
                if *behind > 0 {
                    spans.push(Span::styled(
                        format!("↓{behind}"),
                        apply_token_style(Style::default().fg(palette.red), token.style),
                    ));
                }
            }
            ResolvedTokenKind::TerminalTitle(text) => {
                spans.push(Span::styled(
                    truncate_end(text, budgets[index]),
                    apply_token_style(styles.terminal_title, token.style),
                ));
            }
        }
    }
    spans
}

fn apply_token_style(mut style: Style, patch: shepr_config::SidebarTokenStyle) -> Style {
    if let Some(foreground) = patch.fg {
        style = style.fg(foreground.ratatui());
    }
    if let Some(bold) = patch.bold {
        style = if bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style.remove_modifier(Modifier::BOLD)
        };
    }
    if let Some(dim) = patch.dim {
        style = if dim {
            style.add_modifier(Modifier::DIM)
        } else {
            style.remove_modifier(Modifier::DIM)
        };
    }
    style
}

#[cfg(test)]
impl SectionSplit {
    pub(in crate::shell) fn new(value: f32) -> Option<Self> {
        shepr_core::layout::SplitRatio::new(value).map(Self)
    }
}

#[cfg(test)]
mod split_tests {
    use super::SectionSplit;

    #[test]
    fn section_split_validates_saved_values_and_drag_bounds() {
        assert!(SectionSplit::new(shepr_core::layout::MIN_SPLIT_RATIO - f32::EPSILON).is_none());
        assert!(SectionSplit::new(f32::INFINITY).is_none());
        let invalid_value = shepr_core::layout::MAX_SPLIT_RATIO + f32::EPSILON;
        assert!(serde_json::from_str::<SectionSplit>(&invalid_value.to_string()).is_err());
        assert_eq!(
            SectionSplit::from_drag(shepr_core::layout::MAX_SPLIT_RATIO + 1.0).get(),
            shepr_core::layout::MAX_SPLIT_RATIO
        );
        assert_eq!(SectionSplit::from_drag(f32::NAN), SectionSplit::DEFAULT);
    }

    #[test]
    fn agent_title_budget_and_truncation_keep_emoji_variation_sequences_whole() {
        let title = "\u{2764}\u{fe0f}agent";
        let token = super::ResolvedToken {
            kind: super::ResolvedTokenKind::TerminalTitle(title.to_owned()),
            style: shepr_config::SidebarTokenStyle::default(),
        };
        let palette = super::Palette::default();
        let spans = super::resolved_token_spans(
            &[token],
            crate::shell::presentation::status::StatusGlyph {
                text: "●",
                style: ratatui::style::Style::default(),
            },
            super::TokenStyles {
                state_text: ratatui::style::Style::default(),
                primary: ratatui::style::Style::default(),
                secondary: ratatui::style::Style::default(),
                terminal_title: ratatui::style::Style::default(),
            },
            &palette,
            3,
        );

        assert_eq!(super::rendered_text_width(title), 7);
        assert_eq!(spans[0].content.as_ref(), "\u{2764}\u{fe0f}…");
        assert_eq!(super::rendered_text_width(spans[0].content.as_ref()), 3);
    }
}
