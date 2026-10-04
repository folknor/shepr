//! Outer terminal window title.
//!
//! Shepr is a terminal emulator, so `OSC 0`/`OSC 2` written by a pane stops at
//! Shepr and never reaches the terminal Shepr itself runs in. Without this the
//! host window title keeps whatever the shell or `ssh` left behind, which is
//! what window managers show in their title and group bars.
//!
//! The title is rendered on the server so `{hostname}` names the host the panes
//! actually live on, not the machine a thin remote client runs on. The server
//! renders it for each client's own view (`render_window_title`) and pushes it
//! to that client, which writes the `OSC 0`. Rendering is pure: the settings
//! and the app state, nothing else.

use crate::app::AppState;
use shepr_config::{WindowTitlePart, WindowTitleTemplate, WindowTitleToken};

/// The server's own `ui.window_title`, with the short host name it renders for
/// `{hostname}`. They belong to the server beside the pane chrome it draws and
/// never enter `AppState`.
pub(crate) struct WindowTitleSettings {
    template: WindowTitleTemplate,
    /// The short host name; `None` when the host name is unknown, which
    /// renders empty.
    hostname: Option<String>,
}

impl WindowTitleSettings {
    /// `None` when `ui.window_title` is unset or empty: titles are disabled and
    /// Shepr leaves whatever the shell or `ssh` set.
    pub(crate) fn from_config(
        template: Option<&WindowTitleTemplate>,
        hostname: Option<&shepr_platform::HostNames>,
    ) -> Option<Self> {
        Some(Self {
            template: template?.clone(),
            hostname: hostname.map(|names| names.short().to_owned()),
        })
    }

    /// Whether the title depends on the focused pane's own terminal title,
    /// which is the one input that arrives through PTY parsing rather than app
    /// state.
    pub(crate) fn uses_terminal_title(&self) -> bool {
        self.template.uses(WindowTitleToken::TerminalTitle)
    }
}

/// The title for a client viewing workspace `workspace_id`, or no workspace:
/// no workspace or pane target, and never the session's bookmark. Tokens with
/// nothing to substitute render empty.
pub(crate) fn render_window_title(
    settings: &WindowTitleSettings,
    state: &AppState,
    workspace_id: Option<&shepr_protocol::WorkspaceId>,
) -> String {
    let workspace = workspace_id.and_then(|id| state.workspace(id));
    let terminal = workspace
        .and_then(|workspace| workspace.tree().pane(workspace.tree().focused()))
        .map(shepr_mux::workspace::PaneRecord::terminal);

    let mut title = String::new();
    for part in settings.template.parts() {
        match part {
            WindowTitlePart::Literal(literal) => title.push_str(literal),
            WindowTitlePart::Token(WindowTitleToken::Hostname) => {
                if let Some(hostname) = &settings.hostname {
                    title.push_str(hostname);
                }
            }
            WindowTitlePart::Token(WindowTitleToken::Workspace) => {
                if let Some(workspace) = workspace {
                    title.push_str(workspace.name());
                }
            }
            WindowTitlePart::Token(WindowTitleToken::Pane) => {
                if let Some(label) = terminal.and_then(|terminal| terminal.manual_label()) {
                    title.push_str(label);
                }
            }
            WindowTitlePart::Token(WindowTitleToken::TerminalTitle) => {
                if let Some(terminal_title) =
                    terminal.and_then(shepr_mux::terminal::TerminalState::terminal_title_stripped)
                {
                    title.push_str(&terminal_title);
                }
            }
        }
    }
    title
}

#[cfg(test)]
impl WindowTitleSettings {
    /// Test helper: parse like config validation does. An invalid template is
    /// a broken test, not a disabled title, so it panics. An empty template
    /// parses to `None`.
    pub(crate) fn for_test(template: &str) -> Option<Self> {
        let template =
            WindowTitleTemplate::parse(template).expect("test window title template is valid");
        Self::from_config(template.as_ref(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::{WindowTitleSettings, render_window_title};
    use crate::app::AppState;
    use crate::test_support::WorkspaceFixture as _;
    use shepr_mux::workspace::Workspace;

    fn test_state() -> AppState {
        let mut state = AppState::test_new();
        state.test_set_workspaces(vec![Workspace::test_new("herd")]);
        state
    }

    fn settings(template: &str) -> WindowTitleSettings {
        WindowTitleSettings::for_test(template).expect("template enables titles")
    }

    #[test]
    fn renders_the_workspace_name() {
        let mut state = test_state();
        let settings = settings("{workspace}");

        assert_eq!(
            render_window_title(&settings, &state, Some(&state.ws(0).id())),
            "herd"
        );

        state
            .ws_mut(0)
            .set_name(shepr_mux::terminal::Label::new("build").expect("test name"));
        assert_eq!(
            render_window_title(&settings, &state, Some(&state.ws(0).id())),
            "build"
        );
    }

    #[test]
    fn renders_focused_pane_label_and_terminal_title() {
        let mut state = test_state();
        let settings = settings("{pane}|{terminal_title}");

        let pane_id = state.ws(0).tree().root();
        let terminal = state.terminal_mut(pane_id);
        terminal.set_manual_label("api".into());
        terminal.set_terminal_title(Some("⠋ building".into()));

        assert_eq!(
            render_window_title(&settings, &state, Some(&state.ws(0).id())),
            "api|building"
        );
    }

    #[test]
    fn a_client_with_no_workspace_renders_no_workspace_or_pane_target() {
        let mut state = test_state();
        let settings = settings("{workspace}|{pane}|{terminal_title}|x");
        // The bookmark names the workspace, but a client with no location does
        // not borrow it.
        state.seed_bookmark_index(Some(0));

        assert_eq!(render_window_title(&settings, &state, None), "|||x");
    }

    #[test]
    fn empty_template_disables_window_titles() {
        assert!(WindowTitleSettings::for_test("").is_none());
        assert!(WindowTitleSettings::from_config(None, None).is_none());
    }

    #[test]
    fn invalid_template_is_rejected_before_it_reaches_the_app() {
        assert!(shepr_config::WindowTitleTemplate::parse("{nope}").is_err());
    }

    #[test]
    fn renders_the_short_hostname_and_nothing_when_unknown() {
        let state = test_state();
        let template = shepr_config::WindowTitleTemplate::parse("{hostname}|x").expect("valid");
        let names = shepr_platform::HostNames::from_node_name("buildbox.lan");

        let known = WindowTitleSettings::from_config(template.as_ref(), names.as_ref())
            .expect("titles enabled");
        assert_eq!(
            render_window_title(&known, &state, Some(&state.ws(0).id())),
            "buildbox|x"
        );

        let unknown =
            WindowTitleSettings::from_config(template.as_ref(), None).expect("titles enabled");
        assert_eq!(
            render_window_title(&unknown, &state, Some(&state.ws(0).id())),
            "|x"
        );
    }

    #[test]
    fn unset_tokens_render_empty() {
        let state = test_state();
        let settings = settings("[{pane}]");

        assert_eq!(
            render_window_title(&settings, &state, Some(&state.ws(0).id())),
            "[]"
        );
    }
}
