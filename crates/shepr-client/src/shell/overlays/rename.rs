//! The name prompt: new workspace, rename workspace, rename pane. Typed and pasted text land
//! in its editor; input content must stay out of logs and error messages here.

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_config::theme::Palette;
use shepr_term::key::TerminalKey;
use shepr_termio::text_editor::TextEditor;

use super::widgets::{button, panel, panel_inner, popup, row};
use super::{DialogView, OverlayCommand, OverlayEffect, OverlayPaint, text_editor};
use crate::shell::input::hit_test::contains;
use crate::shell::presentation::status::panel_contrast_fg;
use crate::shell::presentation::text::{put_text, rendered_text_width, truncate_end};

#[derive(Clone, Debug)]
pub(in crate::shell) enum RenameTarget {
    NewWorkspace {
        cwd: Option<shepr_protocol::RemotePath>,
        /// The display label of the machine the workspace is created on, the
        /// presented one, which the prompt names. The command itself goes to
        /// that machine's connection, so it does not carry the label.
        machine: String,
    },
    Workspace {
        workspace_id: shepr_protocol::WorkspaceId,
    },
    Pane {
        pane_id: shepr_protocol::PublicPaneId,
    },
}

impl RenameTarget {
    /// The command that applies a name to this target. An empty name names a workspace after
    /// its directory, and clears a pane's custom name.
    pub(super) fn into_command(
        self,
        label: Option<String>,
    ) -> shepr_protocol::command::EndpointCommand {
        use shepr_protocol::command::{
            EndpointCommand, PaneRenameParams, WorkspaceCreateParams, WorkspaceCreateSource,
            WorkspaceRenameParams,
        };

        match self {
            Self::NewWorkspace { cwd, .. } => {
                EndpointCommand::WorkspaceCreate(WorkspaceCreateParams {
                    // The prompt already resolved the directory the new workspace starts in; with
                    // none known the server picks.
                    source: match cwd {
                        Some(cwd) => WorkspaceCreateSource::Cwd(cwd),
                        None => WorkspaceCreateSource::Default,
                    },
                    label,
                })
            }
            Self::Workspace { workspace_id } => {
                EndpointCommand::WorkspaceRename(WorkspaceRenameParams {
                    workspace_id,
                    label,
                })
            }
            Self::Pane { pane_id } => {
                EndpointCommand::PaneRename(PaneRenameParams { pane_id, label })
            }
        }
    }
}

#[derive(Debug)]
pub(in crate::shell) struct RenameOverlay {
    input: TextEditor,
    target: RenameTarget,
}

impl RenameOverlay {
    /// The new-workspace prompt, seeded with the directory-based suggestion.
    /// `machine` is the display label of the machine the workspace is created on.
    pub(super) fn new_workspace(
        cwd: Option<shepr_protocol::RemotePath>,
        suggested_name: &str,
        machine: &str,
    ) -> Self {
        Self {
            input: TextEditor::new(suggested_name, true),
            target: RenameTarget::NewWorkspace {
                cwd,
                machine: machine.to_owned(),
            },
        }
    }

    pub(super) fn workspace(workspace_id: shepr_protocol::WorkspaceId, label: &str) -> Self {
        Self {
            input: TextEditor::new(label, false),
            target: RenameTarget::Workspace { workspace_id },
        }
    }

    /// `label` is the pane's custom name; a pane without one starts empty and is replaced by
    /// the first typed character.
    pub(super) fn pane(pane_id: shepr_protocol::PublicPaneId, label: Option<&str>) -> Self {
        Self {
            input: TextEditor::new(label.unwrap_or_default(), label.is_none()),
            target: RenameTarget::Pane { pane_id },
        }
    }

    /// The prompt's heading, which always follows its target, fitted to `width`
    /// columns. The new-workspace heading names the machine; a label too long
    /// for the prompt is cut with an ellipsis, keeping the words before it.
    fn title(&self, width: u16) -> String {
        match &self.target {
            RenameTarget::NewWorkspace { machine, .. } => {
                const LEAD: &str = "new workspace on ";
                let room = usize::from(width).saturating_sub(rendered_text_width(LEAD));
                format!("{LEAD}{}", truncate_end(machine, room))
            }
            RenameTarget::Workspace { .. } => "rename workspace".to_owned(),
            RenameTarget::Pane { .. } => "rename pane".to_owned(),
        }
    }

    pub(super) fn layout(screen: Rect) -> Option<DialogView> {
        let q = popup(screen, 56, 7)?;
        let i = panel_inner(q)?;
        let input = Rect::new(i.x, i.y + 2, i.width, 1);
        let rs = row(i, &[8, 10, 12], 2, 3);
        let [save, clear, cancel] = rs.as_slice() else {
            return None;
        };
        Some(DialogView {
            popup: q,
            inner: i,
            input: Some(input),
            primary: *save,
            clear: Some(*clear),
            cancel: *cancel,
        })
    }

    pub(super) fn draw(&self, b: &mut Buffer, view: &DialogView, p: &Palette) -> OverlayPaint {
        panel(b, view.popup, p.accent, p.panel_bg);
        let i = view.inner;
        put_text(
            b,
            i.x,
            i.y,
            i.width,
            &self.title(i.width),
            Style::default()
                .fg(p.text)
                .bg(p.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        let mut cursor = None;
        if let Some(input) = view.input {
            b.set_style(input, Style::default().fg(p.text).bg(p.surface0));
            cursor = text_editor::render(
                b,
                Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1),
                &self.input,
                Style::default().fg(p.text).bg(p.surface0),
            );
        }
        button(
            b,
            view.primary,
            " ↵ save ",
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD),
        );
        let n = Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD);
        if let Some(clear) = view.clear {
            button(b, clear, " ^c clear ", n);
        }
        button(b, view.cancel, " esc cancel ", n);
        OverlayPaint {
            opaque: vec![view.popup],
            backdrop: true,
            cursor,
        }
    }

    pub(super) fn on_key(&mut self, key: &TerminalKey) -> OverlayEffect {
        if key.code == KeyCode::Enter {
            return self.save();
        }
        if key.code == KeyCode::Esc {
            return OverlayEffect::Close;
        }
        if key
            .generated_text
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            return changed_if(self.input.handle_key(key).is_handled());
        }
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.input.clear();
            return OverlayEffect::Changed;
        }
        if key.code == KeyCode::Backspace && key.modifiers.contains(KeyModifiers::SUPER) {
            self.input.clear();
            return OverlayEffect::Changed;
        }
        changed_if(self.input.handle_key(key).is_handled())
    }

    /// A left press anywhere but save and clear closes the prompt, the popup included.
    pub(super) fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        view: Option<&DialogView>,
    ) -> OverlayEffect {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return OverlayEffect::Unchanged;
        }
        let point = (mouse.column, mouse.row);
        if view.is_some_and(|view| contains(view.primary, point)) {
            self.save()
        } else if view
            .and_then(|view| view.clear)
            .is_some_and(|clear| contains(clear, point))
        {
            self.input.clear();
            OverlayEffect::Changed
        } else {
            OverlayEffect::Close
        }
    }

    fn save(&self) -> OverlayEffect {
        // An empty name is sent as none: the server names a workspace after its directory,
        // and clears a pane's custom name, the same as the context menu's Clear.
        let trimmed = self.input.as_str().trim();
        let label = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        OverlayEffect::Command(OverlayCommand::SaveRename {
            target: self.target.clone(),
            label,
        })
    }

    pub(super) fn on_text(&mut self, text: &str) -> bool {
        self.input.insert(text);
        true
    }
}

fn changed_if(changed: bool) -> OverlayEffect {
    if changed {
        OverlayEffect::Changed
    } else {
        OverlayEffect::Unchanged
    }
}

#[cfg(test)]
impl RenameOverlay {
    pub(in crate::shell) fn input(&self) -> &TextEditor {
        &self.input
    }

    pub(in crate::shell) fn target(&self) -> &RenameTarget {
        &self.target
    }
}

#[cfg(test)]
mod tests {
    use super::{RenameOverlay, RenameTarget};
    use crate::shell::presentation::text::rendered_text_width;
    use crate::tests::{test_pane_id, test_workspace_id};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn title_follows_the_target() {
        let new = RenameOverlay::new_workspace(None, "workspace", "build");
        assert_eq!(new.title(52), "new workspace on build");
        assert_eq!(new.input.as_str(), "workspace");
        let workspace = RenameOverlay::workspace(test_workspace_id("w1"), "client-shell");
        assert_eq!(workspace.title(52), "rename workspace");
        assert_eq!(workspace.input.as_str(), "client-shell");
        let pane = RenameOverlay::pane(test_pane_id("w1:p1"), None);
        assert_eq!(pane.title(52), "rename pane");
        assert_eq!(pane.input.as_str(), "");
        assert!(matches!(pane.target, RenameTarget::Pane { .. }));
    }

    #[test]
    fn a_long_machine_label_is_cut_to_the_heading_width() {
        let new = RenameOverlay::new_workspace(None, "", &"m".repeat(80));
        let title = new.title(30);
        assert!(title.starts_with("new workspace on m"), "{title}");
        assert!(title.ends_with('…'), "{title}");
        assert_eq!(rendered_text_width(&title), 30);
        // A label that fits is not cut.
        assert_eq!(
            new.title(200),
            format!("new workspace on {}", "m".repeat(80))
        );
    }

    #[test]
    fn the_drawn_heading_names_the_machine_inside_the_prompt() {
        let palette = shepr_config::theme::Palette::default();
        let screen = Rect::new(0, 0, 80, 24);
        let view = RenameOverlay::layout(screen).expect("the prompt fits");
        let new = RenameOverlay::new_workspace(None, "", &"long-machine-name-".repeat(6));
        let mut buffer = Buffer::empty(screen);
        new.draw(&mut buffer, &view, &palette);
        let heading = (view.inner.x..view.inner.right())
            .map(|x| buffer[(x, view.inner.y)].symbol())
            .collect::<String>();
        assert!(
            heading.starts_with("new workspace on long-machine-name-"),
            "{heading}"
        );
        // The cut label ends in the prompt's last column, so the ellipsis shows.
        assert_eq!(buffer[(view.inner.right() - 1, view.inner.y)].symbol(), "…");
    }
}
