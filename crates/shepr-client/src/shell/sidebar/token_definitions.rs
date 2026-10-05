use shepr_config::{
    AgentSidebarTokenKind, AgentsSidebarConfig, SidebarTokenRendering, SidebarTokenStyle,
    SpaceSidebarTokenKind, SpacesSidebarConfig,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::shell::sidebar) struct ResolvedToken {
    pub(in crate::shell::sidebar) kind: ResolvedTokenKind,
    pub(in crate::shell::sidebar) style: SidebarTokenStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::shell::sidebar) enum ResolvedTokenKind {
    StateIcon,
    StateText(String),
    Machine(String),
    Workspace(String),
    Pane(String),
    Agent(String),
    TerminalTitle(String),
    Branch(String),
    GitStatus { ahead: usize, behind: usize },
}

impl ResolvedTokenKind {
    fn text_value(&self) -> Option<&str> {
        match self {
            Self::StateText(value)
            | Self::Machine(value)
            | Self::Workspace(value)
            | Self::Pane(value)
            | Self::Agent(value)
            | Self::TerminalTitle(value)
            | Self::Branch(value) => Some(value),
            Self::StateIcon | Self::GitStatus { .. } => None,
        }
    }
}

impl ResolvedToken {
    fn new(kind: ResolvedTokenKind, style: SidebarTokenStyle) -> Self {
        Self { kind, style }
    }
}

pub(in crate::shell::sidebar) struct AgentTokenContext<'a> {
    pub(in crate::shell::sidebar) machine: Option<&'a str>,
    pub(in crate::shell::sidebar) workspace: &'a str,
    pub(in crate::shell::sidebar) pane: Option<&'a str>,
    pub(in crate::shell::sidebar) agent_label: Option<&'a str>,
    pub(in crate::shell::sidebar) terminal_title: Option<&'a str>,
    pub(in crate::shell::sidebar) terminal_title_stripped: Option<&'a str>,
    pub(in crate::shell::sidebar) canonical_agent: Option<shepr_config::ConfigAgent>,
}

pub(in crate::shell::sidebar) fn agent_rows(
    config: &AgentsSidebarConfig,
    context: &AgentTokenContext<'_>,
    state_text: &str,
) -> Vec<Vec<ResolvedToken>> {
    config
        .rows_for_agent(context.canonical_agent)
        .iter()
        .filter_map(|row| {
            let resolved = row
                .iter()
                .filter_map(|configured| {
                    let (token, style) = configured.parts();
                    let kind = match token {
                        AgentSidebarTokenKind::StateIcon => Some(ResolvedTokenKind::StateIcon),
                        AgentSidebarTokenKind::StateText => {
                            Some(ResolvedTokenKind::StateText(state_text.to_string()))
                        }
                        AgentSidebarTokenKind::Machine => context
                            .machine
                            .map(|value| ResolvedTokenKind::Machine(value.to_string())),
                        AgentSidebarTokenKind::Workspace => {
                            Some(ResolvedTokenKind::Workspace(context.workspace.to_string()))
                        }
                        AgentSidebarTokenKind::Pane => context
                            .pane
                            .map(|value| ResolvedTokenKind::Pane(value.to_string())),
                        AgentSidebarTokenKind::Agent => context
                            .agent_label
                            .map(|value| ResolvedTokenKind::Agent(value.to_string())),
                        AgentSidebarTokenKind::TerminalTitle => context
                            .terminal_title
                            .map(|value| ResolvedTokenKind::TerminalTitle(value.to_string())),
                        AgentSidebarTokenKind::TerminalTitleStripped => context
                            .terminal_title_stripped
                            .map(|value| ResolvedTokenKind::TerminalTitle(value.to_string())),
                    }?;
                    let rendering = kind
                        .text_value()
                        .map_or(SidebarTokenRendering::Styled(style), |value| {
                            configured.style_for_value(value)
                        });
                    let SidebarTokenRendering::Styled(style) = rendering else {
                        return None;
                    };
                    Some(ResolvedToken::new(kind, style))
                })
                .collect::<Vec<_>>();
            (!resolved.is_empty()).then_some(resolved)
        })
        .collect()
}

pub(in crate::shell::sidebar) struct SpaceTokenContext<'a> {
    pub(in crate::shell::sidebar) workspace: &'a str,
    pub(in crate::shell::sidebar) branch: Option<&'a str>,
    pub(in crate::shell::sidebar) state_text: &'a str,
    /// Carries the projection's adjacent ahead and behind counts to the one renderer.
    pub(in crate::shell::sidebar) ahead_behind: Option<(usize, usize)>,
}

pub(in crate::shell::sidebar) fn space_rows(
    config: &SpacesSidebarConfig,
    context: &SpaceTokenContext<'_>,
) -> Vec<Vec<ResolvedToken>> {
    config
        .rows
        .iter()
        .filter_map(|row| {
            let resolved = row
                .iter()
                .filter_map(|configured| {
                    let (token, style) = configured.parts();
                    let kind = match token {
                        SpaceSidebarTokenKind::StateIcon => Some(ResolvedTokenKind::StateIcon),
                        SpaceSidebarTokenKind::StateText => {
                            Some(ResolvedTokenKind::StateText(context.state_text.to_string()))
                        }
                        SpaceSidebarTokenKind::Workspace => {
                            Some(ResolvedTokenKind::Workspace(context.workspace.to_string()))
                        }
                        SpaceSidebarTokenKind::Branch => context
                            .branch
                            .map(|branch| ResolvedTokenKind::Branch(branch.to_string())),
                        SpaceSidebarTokenKind::GitStatus => context
                            .ahead_behind
                            .filter(|(ahead, behind)| *ahead > 0 || *behind > 0)
                            .map(|(ahead, behind)| ResolvedTokenKind::GitStatus { ahead, behind }),
                    }?;
                    let rendering = kind
                        .text_value()
                        .map_or(SidebarTokenRendering::Styled(style), |value| {
                            configured.style_for_value(value)
                        });
                    let SidebarTokenRendering::Styled(style) = rendering else {
                        return None;
                    };
                    Some(ResolvedToken::new(kind, style))
                })
                .collect::<Vec<_>>();
            (!resolved.is_empty()).then_some(resolved)
        })
        .collect()
}

pub(in crate::shell::sidebar) fn separator(
    previous: &ResolvedToken,
    current: &ResolvedToken,
) -> &'static str {
    if matches!(previous.kind, ResolvedTokenKind::StateIcon)
        || matches!(current.kind, ResolvedTokenKind::GitStatus { .. })
    {
        " "
    } else {
        " · "
    }
}

#[cfg(test)]
impl ResolvedToken {
    fn unstyled(kind: ResolvedTokenKind) -> Self {
        Self::new(kind, SidebarTokenStyle::default())
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;
    use ratatui::style::Modifier;
    use ratatui::style::Style;

    use super::AgentsSidebarConfig;
    use crate::shell::sidebar::sidebar_tokens::{
        AgentTokenContext, ResolvedToken, ResolvedTokenKind, SpaceTokenContext,
    };
    use crate::shell::sidebar::token_definitions::{agent_rows, space_rows};
    use shepr_config::AgentSidebarToken;

    struct Entry {
        workspace: String,
        pane: Option<String>,
        agent_label: Option<String>,
        terminal_title: Option<String>,
        terminal_title_stripped: Option<String>,
        canonical_agent: Option<shepr_config::ConfigAgent>,
    }

    fn entry() -> Entry {
        Entry {
            workspace: "repo".into(),
            pane: None,
            agent_label: Some("pi".into()),
            terminal_title: None,
            terminal_title_stripped: None,
            canonical_agent: Some(shepr_config::ConfigAgent::Pi),
        }
    }

    fn context(entry: &Entry) -> AgentTokenContext<'_> {
        AgentTokenContext {
            machine: None,
            workspace: &entry.workspace,
            pane: entry.pane.as_deref(),
            agent_label: entry.agent_label.as_deref(),
            terminal_title: entry.terminal_title.as_deref(),
            terminal_title_stripped: entry.terminal_title_stripped.as_deref(),
            canonical_agent: entry.canonical_agent,
        }
    }

    #[test]
    fn conditional_styles_merge_first_match_and_keep_missing_values_absent() {
        let config: AgentsSidebarConfig = toml::from_str(r##"
rows = [["state_icon", { token = "machine", fg = "#fff", bold = true, dim = true, rules = [{ equals = "Local", fg = "#f00", bold = false }, { contains = "Loc", fg = "#0f0", dim = false }] }], [{ token = "pane", rules = [{ equals = "", bold = true }] }]]
"##).expect("test precondition");
        let entry = entry();
        for (machine, color, bold, dim) in [
            ("Local", (255, 0, 0), false, true),
            ("Localhost", (0, 255, 0), true, false),
            ("local", (255, 255, 255), true, true),
        ] {
            let mut context = context(&entry);
            context.machine = Some(machine);
            let rows = agent_rows(&config, &context, "working");
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0][0],
                ResolvedToken::unstyled(ResolvedTokenKind::StateIcon)
            );
            let token = &rows[0][1];
            assert_eq!(token.kind, ResolvedTokenKind::Machine(machine.into()));
            assert_eq!(
                token.style.fg.expect("test precondition").ratatui(),
                ratatui::style::Color::Rgb(color.0, color.1, color.2)
            );
            assert_eq!(token.style.bold, Some(bold));
            assert_eq!(token.style.dim, Some(dim));
        }
        assert_eq!(agent_rows(&config, &context(&entry), "working")[0].len(), 1);
    }

    #[test]
    fn conditional_style_survives_truncation_and_removes_default_modifiers() {
        let config: AgentsSidebarConfig = toml::from_str(r##"
rows = [[{ token = "workspace", rules = [{ equals = "long-workspace-name", fg = "#f00", bold = false, dim = false }] }]]
"##).expect("test precondition");
        let mut entry = entry();
        entry.workspace = "long-workspace-name".into();
        let rows = agent_rows(&config, &context(&entry), "working");
        let theme = Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD | Modifier::DIM);
        for width in [4, 40] {
            let spans = crate::shell::sidebar::sidebar_tokens::resolved_token_spans(
                &rows[0],
                crate::shell::presentation::status::status_glyph(
                    shepr_protocol::AgentStatus::Working,
                    &crate::shell::palette::Palette::test_dark(),
                    false,
                ),
                crate::shell::sidebar::sidebar_tokens::TokenStyles::plain(
                    theme,
                    theme,
                    theme,
                    theme,
                    &crate::shell::palette::Palette::test_dark(),
                ),
                &crate::shell::palette::Palette::test_dark(),
                width,
            );
            assert_eq!(spans.len(), 1);
            assert!(
                crate::shell::presentation::text::rendered_text_width(&spans[0].content) <= width
            );
            assert_eq!(spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
            assert!(
                !spans[0]
                    .style
                    .add_modifier
                    .intersects(Modifier::BOLD | Modifier::DIM)
            );
            assert!(
                spans[0]
                    .style
                    .sub_modifier
                    .contains(Modifier::BOLD | Modifier::DIM)
            );
        }
    }

    #[test]
    fn numeric_rules_resolve_in_agent_overrides_and_space_rows() {
        let config: shepr_config::SidebarConfig = toml::from_str(
            r#"
[agents]
rows = [["agent"]]
[agents.rows_by_agent]
pi = [[{ token = "workspace", rules = [{ gt = 80, bold = true }, { gt = 50, dim = true }] }]]
[spaces]
rows = [[{ token = "workspace", rules = [{ lt = 50, dim = true }] }]]
"#,
        )
        .expect("test precondition");
        let mut entry = entry();
        for (value, bold, dim) in [
            ("90", Some(true), None),
            ("60", None, Some(true)),
            ("20", None, None),
            ("90%", None, None),
        ] {
            entry.workspace = value.into();
            let rows = agent_rows(&config.agents, &context(&entry), "working");
            assert_eq!(rows[0][0].kind, ResolvedTokenKind::Workspace(value.into()));
            assert_eq!(rows[0][0].style.bold, bold);
            assert_eq!(rows[0][0].style.dim, dim);
            let spaces = space_rows(
                &config.spaces,
                &SpaceTokenContext {
                    workspace: value,
                    branch: None,
                    state_text: "working",
                    ahead_behind: None,
                },
            );
            assert_eq!(spaces[0][0].style.dim, (value == "20").then_some(true));
        }
    }

    #[test]
    fn conditional_hide_removes_tokens_and_empty_rows() {
        let config: shepr_config::SidebarConfig = toml::from_str(
            r##"
[agents]
rows = [[{ token = "machine", fg = "#61afef", rules = [{ equals = "Local", hide = true }] }, "agent"]]
[agents.rows_by_agent]
pi = [[{ token = "workspace", rules = [{ lt = 50, hide = true }] }], ["agent"]]
[spaces]
rows = [[{ token = "workspace", rules = [{ lt = 50, hide = true }] }], ["state_text"]]
"##,
        ).expect("test precondition");
        let mut entry = entry();
        entry.canonical_agent = None;
        for (machine, count) in [("Local", 1), ("Remote", 2)] {
            let mut ctx = context(&entry);
            ctx.machine = Some(machine);
            let rows = agent_rows(&config.agents, &ctx, "working");
            assert_eq!(rows[0].len(), count);
            assert_eq!(
                rows[0].last().expect("test precondition").kind,
                ResolvedTokenKind::Agent("pi".into())
            );
        }
        entry.canonical_agent = Some(shepr_config::ConfigAgent::Pi);
        for (value, count) in [("20", 1), ("90", 2)] {
            entry.workspace = value.into();
            assert_eq!(
                agent_rows(&config.agents, &context(&entry), "working").len(),
                count
            );
            let rows = space_rows(
                &config.spaces,
                &SpaceTokenContext {
                    workspace: value,
                    branch: None,
                    state_text: "working",
                    ahead_behind: None,
                },
            );
            assert_eq!(rows.len(), count);
        }
    }

    #[test]
    fn conditional_hide_preserves_first_match_wins() {
        for first in ["hide = false", "bold = true"] {
            let config: AgentsSidebarConfig = toml::from_str(&format!(
                "rows = [[{{ token = 'agent', rules = [{{ equals = 'pi', {first} }}, {{ contains = '', hide = true }}] }}]]"
            )).expect("test precondition");
            let entry = entry();
            let rows = agent_rows(&config, &context(&entry), "working");
            assert_eq!(rows[0][0].kind, ResolvedTokenKind::Agent("pi".into()));
        }
    }

    #[test]
    fn missing_tokens_elide_rows_and_separators() {
        let entry = entry();
        let config = AgentsSidebarConfig {
            rows: vec![
                vec![AgentSidebarToken::StateIcon, AgentSidebarToken::Machine],
                vec![AgentSidebarToken::Machine],
                vec![AgentSidebarToken::Agent],
            ],
            ..Default::default()
        };

        let rows = agent_rows(&config, &context(&entry), "working");

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            vec![ResolvedToken::unstyled(ResolvedTokenKind::StateIcon)]
        );
        assert_eq!(
            rows[1],
            vec![ResolvedToken::unstyled(ResolvedTokenKind::Agent(
                "pi".into()
            ))]
        );
    }

    #[test]
    fn machine_token_only_resolves_for_multi_machine_rows() {
        let entry = entry();
        let config = AgentsSidebarConfig {
            rows: vec![vec![
                AgentSidebarToken::Machine,
                AgentSidebarToken::Workspace,
            ]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &context(&entry), "working"),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Workspace(
                "repo".into()
            ))]]
        );

        let mut remote_context = context(&entry);
        remote_context.machine = Some("Build");
        assert_eq!(
            agent_rows(&config, &remote_context, "working"),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::Machine("Build".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::Workspace("repo".into())),
            ]]
        );
    }

    #[test]
    fn terminal_title_builtins_resolve_raw_and_stripped_titles() {
        let mut entry = entry();
        entry.terminal_title = Some("⠋ raw title".into());
        entry.terminal_title_stripped = Some("raw title".into());
        let config = AgentsSidebarConfig {
            rows: vec![vec![
                AgentSidebarToken::TerminalTitle,
                AgentSidebarToken::TerminalTitleStripped,
            ]],
            ..Default::default()
        };

        assert_eq!(
            agent_rows(&config, &context(&entry), "working"),
            vec![vec![
                ResolvedToken::unstyled(ResolvedTokenKind::TerminalTitle("⠋ raw title".into())),
                ResolvedToken::unstyled(ResolvedTokenKind::TerminalTitle("raw title".into())),
            ]]
        );
    }

    #[test]
    fn known_agent_override_replaces_default_rows() {
        let mut config = AgentsSidebarConfig {
            rows: vec![vec![AgentSidebarToken::Workspace]],
            ..Default::default()
        };
        config.rows_by_agent.insert(
            shepr_config::ConfigAgent::Pi,
            vec![vec![AgentSidebarToken::Agent]],
        );
        let mut pi = entry();
        pi.agent_label = Some("renamed pi".into());

        assert_eq!(
            agent_rows(&config, &context(&pi), "working"),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Agent(
                "renamed pi".into()
            ))]]
        );

        pi.canonical_agent = None;
        assert_eq!(
            agent_rows(&config, &context(&pi), "working"),
            vec![vec![ResolvedToken::unstyled(ResolvedTokenKind::Workspace(
                "repo".into()
            ))]]
        );
    }
}
