mod rules;

pub use rules::SidebarTokenRule;

use std::collections::BTreeMap;
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
        let hex = value.strip_prefix('#').filter(|hex| {
            hex.is_ascii()
                && matches!(hex.len(), 3 | 6)
                && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        let Some(hex) = hex else {
            return Err(serde::de::Error::custom(
                "sidebar token fg must be #RGB or #RRGGBB",
            ));
        };
        let invalid_hex = || serde::de::Error::custom("sidebar token fg must be #RGB or #RRGGBB");
        let (r, g, b) = if hex.len() == 3 {
            let mut digits = hex.bytes().map(|byte| {
                let digit = char::from(byte).to_digit(16).unwrap_or(0);
                u8::try_from(digit).unwrap_or(0) * 17
            });
            (
                digits.next().ok_or_else(invalid_hex)?,
                digits.next().ok_or_else(invalid_hex)?,
                digits.next().ok_or_else(invalid_hex)?,
            )
        } else {
            (
                u8::from_str_radix(&hex[0..2], 16).map_err(|_| invalid_hex())?,
                u8::from_str_radix(&hex[2..4], 16).map_err(|_| invalid_hex())?,
                u8::from_str_radix(&hex[4..6], 16).map_err(|_| invalid_hex())?,
            )
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
        $token_name:ident,
        $parse_builtin:ident,
        { $($variant:ident => $name:literal),+ $(,)? }
    ) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum $token {
            $($variant,)+
            Styled {
                token: Box<$token>,
                style: SidebarTokenStyle,
                rules: Vec<SidebarTokenRule>,
            },
        }

        fn $parse_builtin(name: &str) -> Option<$token> {
            match name {
                $($name => Some($token::$variant),)+
                _ => None,
            }
        }
    };
}

define_sidebar_token!(
    AgentSidebarToken,
    agent_token_name,
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
    space_token_name,
    parse_space_sidebar_builtin,
    {
        StateIcon => "state_icon",
        StateText => "state_text",
        Workspace => "workspace",
        Branch => "branch",
        GitStatus => "git_status",
    }
);

impl AgentSidebarToken {
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
            Self::Styled { token, .. } => token.allows_rules(),
        }
    }

    pub fn style_for_value(&self, value: &str) -> Option<SidebarTokenStyle> {
        match self {
            Self::Styled { style, rules, .. } => rules::matching_style(rules, *style, value),
            _ => Some(SidebarTokenStyle::default()),
        }
    }

    pub fn parts(&self) -> (&Self, SidebarTokenStyle) {
        match self {
            Self::Styled { token, style, .. } => (token, *style),
            token => (token, SidebarTokenStyle::default()),
        }
    }
}

impl SpaceSidebarToken {
    fn allows_rules(&self) -> bool {
        match self {
            Self::StateIcon | Self::GitStatus => false,
            Self::StateText | Self::Workspace | Self::Branch => true,
            Self::Styled { token, .. } => token.allows_rules(),
        }
    }

    pub fn style_for_value(&self, value: &str) -> Option<SidebarTokenStyle> {
        match self {
            Self::Styled { style, rules, .. } => rules::matching_style(rules, *style, value),
            _ => Some(SidebarTokenStyle::default()),
        }
    }

    pub fn parts(&self) -> (&Self, SidebarTokenStyle) {
        match self {
            Self::Styled { token, style, .. } => (token, *style),
            token => (token, SidebarTokenStyle::default()),
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
                token: Box::new(token),
                style,
                rules,
            },
            None => token,
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
                token: Box::new(token),
                style,
                rules,
            },
            None => token,
        })
    }
}

type AgentSidebarRows = Vec<Vec<AgentSidebarToken>>;
type SpaceSidebarRows = Vec<Vec<SpaceSidebarToken>>;

fn deserialize_rows_by_agent<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, AgentSidebarRows>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows_by_agent = BTreeMap::<String, AgentSidebarRows>::deserialize(deserializer)?;
    for (id, rows) in &rows_by_agent {
        // This map is looked up with the detector's canonical Agent::label(),
        // so aliases have no lookup meaning here. Unlike cjk_ime_agents, which
        // is a membership list, accepting aliases would also require a rule
        // for duplicate canonical and alias keys for the same agent.
        if ConfigAgent::parse_canonical_label(id).is_none() {
            return Err(serde::de::Error::custom(format!(
                "unknown canonical agent id `{id}` in sidebar rows_by_agent"
            )));
        }
        validate_sidebar_rows(rows).map_err(serde::de::Error::custom)?;
    }
    Ok(rows_by_agent)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentsSidebarConfig {
    #[serde(deserialize_with = "deserialize_sidebar_rows")]
    pub rows: AgentSidebarRows,
    #[serde(default, deserialize_with = "deserialize_rows_by_agent")]
    pub rows_by_agent: BTreeMap<String, AgentSidebarRows>,
    pub row_gap: u16,
}

impl AgentsSidebarConfig {
    /// `rows_by_agent` uses canonical labels; callers pass `Agent::label()`.
    pub fn rows_for_agent(&self, agent: Option<&str>) -> &AgentSidebarRows {
        agent
            .and_then(|agent| self.rows_by_agent.get(agent))
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
            rows_by_agent: BTreeMap::new(),
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
            config.ui.sidebar.agents.rows_by_agent["claude"],
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
        assert_eq!(token, &AgentSidebarToken::Workspace);
        assert_eq!(style.bold, Some(false));
        assert_eq!(
            style.fg.expect("test precondition").ratatui(),
            ratatui::style::Color::Rgb(0xaa, 0xbb, 0xcc)
        );
        assert_eq!(
            config.ui.sidebar.agents.rows[0][1],
            AgentSidebarToken::Workspace
        );

        let (token, style) = config.ui.sidebar.agents.rows_by_agent["claude"][0][0].parts();
        assert_eq!(token, &AgentSidebarToken::Agent);
        assert_eq!(style.bold, Some(true));
        assert_eq!(style.dim, Some(false));

        let (token, style) = config.ui.sidebar.spaces.rows[0][0].parts();
        assert_eq!(token, &SpaceSidebarToken::GitStatus);
        assert_eq!(
            style.fg.expect("test precondition").ratatui(),
            ratatui::style::Color::Rgb(0xff, 0x00, 0xaa)
        );
        let (token, style) = config.ui.sidebar.spaces.rows[1][0].parts();
        assert_eq!(token, &SpaceSidebarToken::Branch);
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
        assert!(
            matches!(&config.agents.rows[0][0], AgentSidebarToken::Styled { rules, .. } if rules.len() == 2)
        );
        assert!(
            matches!(&config.spaces.rows[0][0], SpaceSidebarToken::Styled { rules, .. } if rules.len() == 1)
        );
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
