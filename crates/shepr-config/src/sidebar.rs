mod rules;

pub use rules::SidebarTokenRule;

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::{Deserialize, de::MapAccess, de::Visitor};

use super::ConfigAgent;
use crate::limits::{
    DEFAULT_SIDEBAR_ROW_GAP, MAX_SIDEBAR_ROWS, MAX_SIDEBAR_RULES, MAX_SIDEBAR_TOKENS_PER_ROW,
};

fn deserialize_sidebar_rows<'de, D, T>(deserializer: D) -> Result<Vec<Vec<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let rows = Vec::<Vec<T>>::deserialize(deserializer)?;
    validate_sidebar_rows(&rows).map_err(serde::de::Error::custom)?;
    Ok(rows)
}

fn validate_sidebar_rows<T>(rows: &[Vec<T>]) -> Result<(), String> {
    if rows.len() > MAX_SIDEBAR_ROWS {
        return Err(format!(
            "sidebar layouts may contain at most {MAX_SIDEBAR_ROWS} rows"
        ));
    }
    if rows
        .iter()
        .any(|row| row.len() > MAX_SIDEBAR_TOKENS_PER_ROW)
    {
        return Err(format!(
            "sidebar rows may contain at most {MAX_SIDEBAR_TOKENS_PER_ROW} tokens"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarTokenColor {
    r: u8,
    g: u8,
    b: u8,
}

impl SidebarTokenColor {
    pub fn ratatui(self) -> ratatui::style::Color {
        ratatui::style::Color::Rgb(self.r, self.g, self.b)
    }
}

impl<'de> Deserialize<'de> for SidebarTokenColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let Some((r, g, b)) = crate::theme_config::try_parse_hex_rgb(&value) else {
            return Err(serde::de::Error::custom(
                "sidebar token fg must be #RGB or #RRGGBB",
            ));
        };
        Ok(Self { r, g, b })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct SidebarTokenStyle {
    pub fg: Option<SidebarTokenColor>,
    pub bold: Option<bool>,
    pub dim: Option<bool>,
}

macro_rules! define_sidebar_token {
    (
        $token:ident,
        $kind:ident,
        $parse_builtin:ident,
        { $($variant:ident => $name:literal),+ $(,)? }
    ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $kind {
            $($variant,)+
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $token {
            $($variant,)+
            Styled {
                spec: SidebarTokenSpec<$kind>,
            },
        }

        impl From<$kind> for $token {
            fn from(token: $kind) -> Self {
                match token {
                    $($kind::$variant => Self::$variant,)+
                }
            }
        }

        fn $parse_builtin(name: &str) -> Option<$kind> {
            match name {
                $($name => Some($kind::$variant),)+
                _ => None,
            }
        }
    };
}

/// Presentation from one configured sidebar token and its matching rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTokenRendering {
    Hidden,
    Styled(SidebarTokenStyle),
}

/// Style and rules attached to a non-recursive token kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarTokenSpec<T> {
    pub token: T,
    pub style: SidebarTokenStyle,
    pub rules: Vec<SidebarTokenRule>,
}

impl<T> SidebarTokenSpec<T> {
    fn style_for_value(&self, value: &str) -> SidebarTokenRendering {
        rules::matching_style(&self.rules, self.style, value)
    }
}

define_sidebar_token!(
    AgentSidebarToken,
    AgentSidebarTokenKind,
    parse_agent_sidebar_builtin,
    {
        StateIcon => "state_icon",
        StateText => "state_text",
        Machine => "machine",
        Workspace => "workspace",
        Pane => "pane",
        Agent => "agent",
        TerminalTitle => "terminal_title",
        TerminalTitleStripped => "terminal_title_stripped",
    }
);

define_sidebar_token!(
    SpaceSidebarToken,
    SpaceSidebarTokenKind,
    parse_space_sidebar_builtin,
    {
        StateIcon => "state_icon",
        StateText => "state_text",
        Workspace => "workspace",
        Branch => "branch",
        GitStatus => "git_status",
    }
);

impl AgentSidebarTokenKind {
    fn allows_rules(&self) -> bool {
        match self {
            Self::StateIcon => false,
            Self::StateText
            | Self::Machine
            | Self::Workspace
            | Self::Pane
            | Self::Agent
            | Self::TerminalTitle
            | Self::TerminalTitleStripped => true,
        }
    }
}

impl AgentSidebarToken {
    pub fn style_for_value(&self, value: &str) -> SidebarTokenRendering {
        match self {
            Self::Styled { spec } => spec.style_for_value(value),
            _ => SidebarTokenRendering::Styled(SidebarTokenStyle::default()),
        }
    }

    pub fn parts(&self) -> (AgentSidebarTokenKind, SidebarTokenStyle) {
        match self {
            Self::StateIcon => (
                AgentSidebarTokenKind::StateIcon,
                SidebarTokenStyle::default(),
            ),
            Self::StateText => (
                AgentSidebarTokenKind::StateText,
                SidebarTokenStyle::default(),
            ),
            Self::Machine => (AgentSidebarTokenKind::Machine, SidebarTokenStyle::default()),
            Self::Workspace => (
                AgentSidebarTokenKind::Workspace,
                SidebarTokenStyle::default(),
            ),
            Self::Pane => (AgentSidebarTokenKind::Pane, SidebarTokenStyle::default()),
            Self::Agent => (AgentSidebarTokenKind::Agent, SidebarTokenStyle::default()),
            Self::TerminalTitle => (
                AgentSidebarTokenKind::TerminalTitle,
                SidebarTokenStyle::default(),
            ),
            Self::TerminalTitleStripped => (
                AgentSidebarTokenKind::TerminalTitleStripped,
                SidebarTokenStyle::default(),
            ),
            Self::Styled { spec } => (spec.token, spec.style),
        }
    }
}

impl SpaceSidebarTokenKind {
    fn allows_rules(&self) -> bool {
        match self {
            Self::StateIcon | Self::GitStatus => false,
            Self::StateText | Self::Workspace | Self::Branch => true,
        }
    }
}

impl SpaceSidebarToken {
    pub fn style_for_value(&self, value: &str) -> SidebarTokenRendering {
        match self {
            Self::Styled { spec } => spec.style_for_value(value),
            _ => SidebarTokenRendering::Styled(SidebarTokenStyle::default()),
        }
    }

    pub fn parts(&self) -> (SpaceSidebarTokenKind, SidebarTokenStyle) {
        match self {
            Self::StateIcon => (
                SpaceSidebarTokenKind::StateIcon,
                SidebarTokenStyle::default(),
            ),
            Self::StateText => (
                SpaceSidebarTokenKind::StateText,
                SidebarTokenStyle::default(),
            ),
            Self::Workspace => (
                SpaceSidebarTokenKind::Workspace,
                SidebarTokenStyle::default(),
            ),
            Self::Branch => (SpaceSidebarTokenKind::Branch, SidebarTokenStyle::default()),
            Self::GitStatus => (
                SpaceSidebarTokenKind::GitStatus,
                SidebarTokenStyle::default(),
            ),
            Self::Styled { spec } => (spec.token, spec.style),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStyledSidebarToken {
    token: String,
    #[serde(default)]
    fg: Option<SidebarTokenColor>,
    #[serde(default)]
    bold: Option<bool>,
    #[serde(default)]
    dim: Option<bool>,
    #[serde(default)]
    rules: Vec<SidebarTokenRule>,
}

enum RawSidebarToken {
    Plain(String),
    Styled(RawStyledSidebarToken),
}

impl<'de> Deserialize<'de> for RawSidebarToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RawSidebarTokenVisitor;

        impl<'de> Visitor<'de> for RawSidebarTokenVisitor {
            type Value = RawSidebarToken;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a sidebar token string or styled token table")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawSidebarToken::Plain(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawSidebarToken::Plain(value))
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let token = RawStyledSidebarToken::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(RawSidebarToken::Styled(token))
            }
        }

        deserializer.deserialize_any(RawSidebarTokenVisitor)
    }
}

impl RawSidebarToken {
    fn parts(self) -> Result<(String, Option<SidebarTokenStyle>, Vec<SidebarTokenRule>), String> {
        match self {
            Self::Plain(token) => Ok((token, None, Vec::new())),
            Self::Styled(token) => {
                if token.rules.len() > MAX_SIDEBAR_RULES {
                    return Err(format!(
                        "sidebar tokens may contain at most {MAX_SIDEBAR_RULES} rules"
                    ));
                }
                Ok((
                    token.token,
                    Some(SidebarTokenStyle {
                        fg: token.fg,
                        bold: token.bold,
                        dim: token.dim,
                    }),
                    token.rules,
                ))
            }
        }
    }
}

fn parse_sidebar_token<T>(value: &str, parse_builtin: fn(&str) -> Option<T>) -> Result<T, String> {
    parse_builtin(value).ok_or_else(|| format!("unknown sidebar token `{value}`"))
}

impl<'de> Deserialize<'de> for AgentSidebarToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (value, style, rules) = RawSidebarToken::deserialize(deserializer)?
            .parts()
            .map_err(serde::de::Error::custom)?;
        let token = parse_sidebar_token(&value, parse_agent_sidebar_builtin)
            .map_err(serde::de::Error::custom)?;
        if !rules.is_empty() && !token.allows_rules() {
            return Err(serde::de::Error::custom(
                "sidebar rules require a text-valued token",
            ));
        }
        Ok(match style {
            Some(style) => Self::Styled {
                spec: SidebarTokenSpec {
                    token,
                    style,
                    rules,
                },
            },
            None => token.into(),
        })
    }
}

impl<'de> Deserialize<'de> for SpaceSidebarToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (value, style, rules) = RawSidebarToken::deserialize(deserializer)?
            .parts()
            .map_err(serde::de::Error::custom)?;
        let token = parse_sidebar_token(&value, parse_space_sidebar_builtin)
            .map_err(serde::de::Error::custom)?;
        if !rules.is_empty() && !token.allows_rules() {
            return Err(serde::de::Error::custom(
                "sidebar rules require a text-valued token",
            ));
        }
        Ok(match style {
            Some(style) => Self::Styled {
                spec: SidebarTokenSpec {
                    token,
                    style,
                    rules,
                },
            },
            None => token.into(),
        })
    }
}

type AgentSidebarRows = Vec<Vec<AgentSidebarToken>>;
type SpaceSidebarRows = Vec<Vec<SpaceSidebarToken>>;

fn deserialize_rows_by_agent<'de, D>(
    deserializer: D,
) -> Result<HashMap<ConfigAgent, AgentSidebarRows>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows_by_agent = BTreeMap::<String, AgentSidebarRows>::deserialize(deserializer)?;
    let mut parsed = HashMap::with_capacity(rows_by_agent.len());
    for (id, rows) in rows_by_agent {
        // Canonical labels only: unlike the cjk_ime_agents membership list,
        // accepting aliases here would need a rule for a canonical key and an
        // alias key that name the same agent.
        let agent = ConfigAgent::parse_canonical_label(&id).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown canonical agent id `{id}` in sidebar rows_by_agent"
            ))
        })?;
        validate_sidebar_rows(&rows).map_err(serde::de::Error::custom)?;
        parsed.insert(agent, rows);
    }
    Ok(parsed)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentsSidebarConfig {
    #[serde(deserialize_with = "deserialize_sidebar_rows")]
    pub rows: AgentSidebarRows,
    #[serde(default, deserialize_with = "deserialize_rows_by_agent")]
    pub rows_by_agent: HashMap<ConfigAgent, AgentSidebarRows>,
    pub row_gap: u16,
}

impl AgentsSidebarConfig {
    /// Overrides are keyed by the parsed agent identity.
    pub fn rows_for_agent(&self, agent: Option<ConfigAgent>) -> &AgentSidebarRows {
        agent
            .and_then(|agent| self.rows_by_agent.get(&agent))
            .unwrap_or(&self.rows)
    }
}

impl Default for AgentsSidebarConfig {
    fn default() -> Self {
        Self {
            rows: vec![
                vec![
                    AgentSidebarToken::StateIcon,
                    AgentSidebarToken::Machine,
                    AgentSidebarToken::Workspace,
                ],
                vec![AgentSidebarToken::Agent],
            ],
            rows_by_agent: HashMap::new(),
            row_gap: DEFAULT_SIDEBAR_ROW_GAP,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SpacesSidebarConfig {
    #[serde(deserialize_with = "deserialize_sidebar_rows")]
    pub rows: SpaceSidebarRows,
    pub row_gap: u16,
}

impl Default for SpacesSidebarConfig {
    fn default() -> Self {
        Self {
            rows: vec![
                vec![SpaceSidebarToken::StateIcon, SpaceSidebarToken::Workspace],
                vec![SpaceSidebarToken::Branch, SpaceSidebarToken::GitStatus],
            ],
            row_gap: DEFAULT_SIDEBAR_ROW_GAP,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub agents: AgentsSidebarConfig,
    pub spaces: SpacesSidebarConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_compact_agent_and_existing_space_layouts() {
        let config = SidebarConfig::default();
        assert_eq!(
            config.agents.rows,
            vec![
                vec![
                    AgentSidebarToken::StateIcon,
                    AgentSidebarToken::Machine,
                    AgentSidebarToken::Workspace,
                ],
                vec![AgentSidebarToken::Agent],
            ]
        );
        assert!(
            toml::from_str::<crate::ClientConfig>("[ui.sidebar.agents]\nrows = [[\"tab\"]]\n")
                .is_err()
        );
        assert!(config.agents.rows_by_agent.is_empty());
        assert_eq!(config.agents.row_gap, 0);
        assert_eq!(
            config.spaces.rows,
            vec![
                vec![SpaceSidebarToken::StateIcon, SpaceSidebarToken::Workspace],
                vec![SpaceSidebarToken::Branch, SpaceSidebarToken::GitStatus],
            ]
        );
        assert_eq!(config.spaces.row_gap, 0);
    }

    #[test]
    fn parses_builtin_tokens() {
        let config: crate::ClientConfig = toml::from_str(
            r#"
[ui.sidebar.agents]
rows = [["state_icon", "workspace"], ["state_text", "agent", "pane"], ["terminal_title", "terminal_title_stripped", "pane"]]
row_gap = 1

[ui.sidebar.agents.rows_by_agent]
claude = [["terminal_title_stripped"], ["agent", "machine"]]

[ui.sidebar.spaces]
rows = [["workspace"], ["git_status"]]
row_gap = 3
"#,
        )
        .expect("sidebar token config");

        assert_eq!(
            config.ui.sidebar.agents.rows[1],
            vec![
                AgentSidebarToken::StateText,
                AgentSidebarToken::Agent,
                AgentSidebarToken::Pane,
            ]
        );
        assert_eq!(
            config.ui.sidebar.agents.rows[2],
            vec![
                AgentSidebarToken::TerminalTitle,
                AgentSidebarToken::TerminalTitleStripped,
                AgentSidebarToken::Pane,
            ]
        );
        assert_eq!(
            config.ui.sidebar.agents.rows_by_agent[&ConfigAgent::Claude],
            vec![
                vec![AgentSidebarToken::TerminalTitleStripped],
                vec![AgentSidebarToken::Agent, AgentSidebarToken::Machine,],
            ]
        );
        assert_eq!(config.ui.sidebar.agents.row_gap, 1);
        assert_eq!(
            config.ui.sidebar.spaces.rows[1],
            vec![SpaceSidebarToken::GitStatus]
        );
        assert_eq!(config.ui.sidebar.spaces.row_gap, 3);
    }

    #[test]
    fn parses_occurrence_styles_without_changing_plain_tokens() {
        let config: crate::ClientConfig = toml::from_str(
            r##"
[ui.sidebar.agents]
rows = [[{ token = "workspace", fg = "#abc", bold = false }, "workspace"], [{ token = "pane", dim = false }]]

[ui.sidebar.agents.rows_by_agent]
claude = [[{ token = "agent", fg = "#112233", bold = true, dim = false }]]

[ui.sidebar.spaces]
rows = [[{ token = "git_status", fg = "#ff00aa" }], [{ token = "branch", bold = true }]]
"##,
        )
        .expect("test precondition");

        let (token, style) = config.ui.sidebar.agents.rows[0][0].parts();
        assert_eq!(token, AgentSidebarTokenKind::Workspace);
        assert_eq!(style.bold, Some(false));
        assert_eq!(
            style.fg.expect("test precondition").ratatui(),
            ratatui::style::Color::Rgb(0xaa, 0xbb, 0xcc)
        );
        assert_eq!(
            config.ui.sidebar.agents.rows[0][1],
            AgentSidebarToken::Workspace
        );

        let (token, style) =
            config.ui.sidebar.agents.rows_by_agent[&ConfigAgent::Claude][0][0].parts();
        assert_eq!(token, AgentSidebarTokenKind::Agent);
        assert_eq!(style.bold, Some(true));
        assert_eq!(style.dim, Some(false));

        let (token, style) = config.ui.sidebar.spaces.rows[0][0].parts();
        assert_eq!(token, SpaceSidebarTokenKind::GitStatus);
        assert_eq!(
            style.fg.expect("test precondition").ratatui(),
            ratatui::style::Color::Rgb(0xff, 0x00, 0xaa)
        );
        let (token, style) = config.ui.sidebar.spaces.rows[1][0].parts();
        assert_eq!(token, SpaceSidebarTokenKind::Branch);
        assert_eq!(style.bold, Some(true));
    }

    #[test]
    fn conditional_sidebar_rules_parse() {
        let input = r##"
[agents]
rows = [[{ token = "machine", fg = "#fff", rules = [{ equals = "Local", fg = "#f00" }, { starts_with = "fed", ignore_case = true, bold = true }] }]]
[agents.rows_by_agent]
pi = [[{ token = "pane", rules = [{ gt = 80, dim = false }, { lt = 20.5, dim = true }] }]]
[spaces]
rows = [[{ token = "branch", rules = [{ contains = "error", bold = true }] }]]
"##;
        let config: SidebarConfig = toml::from_str(input).expect("conditional sidebar config");
        assert!(matches!(
            &config.agents.rows[0][0],
            AgentSidebarToken::Styled { spec } if spec.rules.len() == 2
        ));
        assert!(matches!(
            &config.spaces.rows[0][0],
            SpaceSidebarToken::Styled { spec } if spec.rules.len() == 1
        ));
    }

    #[test]
    fn conditional_sidebar_rules_reject_invalid_conditions_and_nontext_tokens() {
        for rule in [
            "{ bold = true }",
            "{ equals = 'x', contains = 'x' }",
            "{ regex = 'x' }",
            "{ gt = '80' }",
            "{ equals = 80 }",
            "{ gt = nan }",
            "{ lt = inf }",
            "{ gt = 80, ignore_case = false }",
            "{ equals = 'x', underline = true }",
            "{ equals = 'x', fg = 'red' }",
        ] {
            let input = format!("[agents]\nrows = [[{{ token = 'machine', rules = [{rule}] }}]]");
            assert!(toml::from_str::<SidebarConfig>(&input).is_err(), "{rule}");
        }
        for (section, token) in [
            ("agents", "state_icon"),
            ("spaces", "state_icon"),
            ("spaces", "git_status"),
        ] {
            let input = format!(
                "[{section}]\nrows = [[{{ token = '{token}', rules = [{{ equals = 'x' }}] }}]]"
            );
            assert!(toml::from_str::<SidebarConfig>(&input).is_err());
        }
        for count in [16, 17] {
            let rules = std::iter::repeat_n("{ equals = 'x' }", count)
                .collect::<Vec<_>>()
                .join(",");
            let input = format!("[agents]\nrows = [[{{ token = 'machine', rules = [{rules}] }}]]");
            assert_eq!(toml::from_str::<SidebarConfig>(&input).is_ok(), count == 16);
        }
    }

    #[test]
    fn rejects_invalid_occurrence_styles() {
        let invalid_color = toml::from_str::<crate::ClientConfig>(
            r##"[ui.sidebar.agents]
rows = [[{ token = "workspace", fg = "red" }]]
"##,
        )
        .expect_err("invalid token color");
        assert!(
            invalid_color
                .to_string()
                .contains("sidebar token fg must be #RGB or #RRGGBB"),
            "unexpected error: {invalid_color}"
        );

        let invalid_rule = toml::from_str::<SidebarConfig>(
            "[agents]\nrows = [[{ token = 'machine', rules = [{ gt = 80, ignore_case = true }] }]]",
        )
        .expect_err("invalid numeric rule");
        assert!(
            invalid_rule
                .to_string()
                .contains("ignore_case applies only to sidebar text conditions"),
            "unexpected error: {invalid_rule}"
        );

        for entry in [
            r##"{ token = "workspace", fg = "red" }"##,
            r##"{ token = "workspace", fg = "#abcd" }"##,
            r##"{ token = "workspace", fg = " #fff " }"##,
            r##"{ token = "workspace", underline = true }"##,
        ] {
            let input = format!("[ui.sidebar.agents]\nrows = [[{entry}]]\n");
            assert!(
                toml::from_str::<crate::ClientConfig>(&input).is_err(),
                "accepted {entry}"
            );
        }
    }

    #[test]
    fn rejects_unknown_bare_and_custom_tokens() {
        let input = |token: &str| format!("[ui.sidebar.agents]\nrows = [[\"{token}\"]]\n");
        // Controls: the same TOML with valid tokens parses, so the rejections
        // below are down to the token and not to malformed TOML.
        for token in ["workspace", "terminal_title"] {
            assert!(
                toml::from_str::<crate::ClientConfig>(&input(token)).is_ok(),
                "rejected {token}"
            );
        }
        for token in ["summary", "$", "$bad.name", "$summary"] {
            assert!(
                toml::from_str::<crate::ClientConfig>(&input(token)).is_err(),
                "accepted {token}"
            );
        }
    }

    #[test]
    fn rejects_oversized_sidebar_layouts() {
        let too_many_rows = std::iter::repeat_n("[\"agent\"]", MAX_SIDEBAR_ROWS + 1)
            .collect::<Vec<_>>()
            .join(",");
        let input = format!("[ui.sidebar.agents]\nrows = [{too_many_rows}]\n");
        assert!(toml::from_str::<crate::ClientConfig>(&input).is_err());

        let too_many_tokens = std::iter::repeat_n("\"workspace\"", MAX_SIDEBAR_TOKENS_PER_ROW + 1)
            .collect::<Vec<_>>()
            .join(",");
        let input = format!("[ui.sidebar.spaces]\nrows = [[{too_many_tokens}]]\n");
        assert!(toml::from_str::<crate::ClientConfig>(&input).is_err());

        let input = format!("[ui.sidebar.agents.rows_by_agent]\nclaude = [{too_many_rows}]\n");
        assert!(toml::from_str::<crate::ClientConfig>(&input).is_err());
    }

    #[test]
    fn accepts_every_canonical_agent_override_key() {
        let agents = ConfigAgent::all().collect::<Vec<_>>();
        let entries = agents
            .iter()
            .map(|agent| format!("{} = [[\"agent\"]]", agent.label()))
            .collect::<Vec<_>>()
            .join("\n");
        let input = format!("[ui.sidebar.agents.rows_by_agent]\n{entries}\n");
        let config: crate::ClientConfig = toml::from_str(&input).expect("canonical keys");

        assert_eq!(config.ui.sidebar.agents.rows_by_agent.len(), agents.len());
    }

    #[test]
    fn rejects_alias_case_whitespace_and_unknown_override_keys() {
        for key in ["claude-code", "Claude", "' claude '", "unknown"] {
            let input = format!("[ui.sidebar.agents.rows_by_agent]\n{key} = [[\"agent\"]]\n");
            assert!(
                toml::from_str::<crate::ClientConfig>(&input).is_err(),
                "accepted key {key:?}"
            );
        }
    }
}
