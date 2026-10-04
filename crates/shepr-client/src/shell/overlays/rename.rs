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
use crate::shell::ledger::Ticket;
use crate::shell::presentation::status::panel_contrast_fg;
use crate::shell::presentation::text::put_text;
use crate::shell::state::Repaint;

#[derive(Clone, Debug)]
pub(in crate::shell) enum RenameTarget {
    NewWorkspace {
        cwd: Option<shepr_protocol::RemotePath>,
        suggested_name: String,
        label_lookup: Option<Ticket>,
    },
    Workspace {
        workspace_id: shepr_protocol::WorkspaceId,
    },
    Pane {
        pane_id: shepr_protocol::PublicPaneId,
    },
}

impl RenameTarget {
    /// The command that applies a name to this target. An empty name clears a custom name, so
    /// the automatic label returns; a new workspace keeps the suggestion unnamed.
    pub(super) fn into_command(
        self,
        label: Option<String>,
    ) -> shepr_protocol::command::EndpointCommand {
        use shepr_protocol::command::{
            EndpointCommand, PaneRenameParams, WorkspaceCreateParams, WorkspaceCreateSource,
            WorkspaceRenameParams,
        };

        match self {
            Self::NewWorkspace {
                cwd,
                suggested_name,
                ..
            } => EndpointCommand::WorkspaceCreate(WorkspaceCreateParams {
                // The prompt already resolved the directory the new workspace starts in; with
                // none known the server picks.
                source: match cwd {
                    Some(cwd) => WorkspaceCreateSource::Cwd(cwd),
                    None => WorkspaceCreateSource::Default,
                },
                label: label.filter(|label| *label != suggested_name),
            }),
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
    pub(in crate::shell) input: TextEditor,
    pub(in crate::shell) target: RenameTarget,
}

impl RenameOverlay {
    /// The new-workspace prompt, seeded with the path-based suggestion.
    pub(in crate::shell) fn new_workspace(
        cwd: Option<shepr_protocol::RemotePath>,
        suggested_name: String,
        label_lookup: Option<Ticket>,
    ) -> Self {
        Self {
            input: TextEditor::new(&suggested_name, true),
            target: RenameTarget::NewWorkspace {
                cwd,
                suggested_name,
                label_lookup,
            },
        }
    }

    pub(in crate::shell) fn workspace(
        workspace_id: shepr_protocol::WorkspaceId,
        label: &str,
    ) -> Self {
        Self {
            input: TextEditor::new(label, false),
            target: RenameTarget::Workspace { workspace_id },
        }
    }

    /// `label` is the pane's custom name; a pane without one starts empty and is replaced by
    /// the first typed character.
    pub(in crate::shell) fn pane(
        pane_id: shepr_protocol::PublicPaneId,
        label: Option<&str>,
    ) -> Self {
        Self {
            input: TextEditor::new(label.unwrap_or_default(), label.is_none()),
            target: RenameTarget::Pane { pane_id },
        }
    }

    /// The prompt's heading, which always follows its target.
    pub(in crate::shell) fn title(&self) -> &'static str {
        match self.target {
            RenameTarget::NewWorkspace { .. } => "new workspace",
            RenameTarget::Workspace { .. } => "rename workspace",
            RenameTarget::Pane { .. } => "rename pane",
        }
    }

    /// Applies the answer to a `workspace.checkout_root` request. An answer for a lookup that
    /// is not this prompt's, or for a prompt that was reopened since, is ignored, and a failed
    /// lookup keeps the path-based suggestion.
    pub(super) fn apply_checkout_root(
        &mut self,
        lookup: Ticket,
        result: Option<shepr_protocol::command::WorkspaceCheckoutRootReply>,
    ) -> Repaint {
        let RenameTarget::NewWorkspace {
            cwd,
            suggested_name,
            label_lookup,
        } = &mut self.target
        else {
            return Repaint::Unchanged;
        };
        if *label_lookup != Some(lookup) {
            return Repaint::Unchanged;
        }
        *label_lookup = None;
        let Some(shepr_protocol::command::WorkspaceCheckoutRootReply { root, home }) = result
        else {
            return Repaint::Unchanged;
        };
        let Some(cwd) = cwd.as_ref() else {
            return Repaint::Unchanged;
        };
        // Only a cwd outside Git can be labelled `~`.
        let home = if root.is_none() { home } else { None };
        let label = shepr_core::workspace_label::workspace_label_from_cwd(
            cwd.as_path(),
            root.as_ref().map(shepr_protocol::RemotePath::as_path),
            home.as_ref().map(shepr_protocol::RemotePath::as_path),
        );
        if self.input.as_str() == suggested_name.as_str() {
            self.input = TextEditor::new(&label, true);
        }
        *suggested_name = label;
        Repaint::Needed
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
            self.title(),
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
        // An empty name clears the custom name, the same as the context menu's Clear, so the
        // automatic label returns.
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
mod tests {
    use super::{RenameOverlay, RenameTarget};
    use crate::tests::{test_pane_id, test_workspace_id};

    #[test]
    fn title_follows_the_target() {
        let new = RenameOverlay::new_workspace(None, "workspace".to_owned(), None);
        assert_eq!(new.title(), "new workspace");
        assert_eq!(new.input.as_str(), "workspace");
        let workspace = RenameOverlay::workspace(test_workspace_id("w1"), "client-shell");
        assert_eq!(workspace.title(), "rename workspace");
        assert_eq!(workspace.input.as_str(), "client-shell");
        let pane = RenameOverlay::pane(test_pane_id("w1:p1"), None);
        assert_eq!(pane.title(), "rename pane");
        assert_eq!(pane.input.as_str(), "");
        assert!(matches!(pane.target, RenameTarget::Pane { .. }));
    }
}
