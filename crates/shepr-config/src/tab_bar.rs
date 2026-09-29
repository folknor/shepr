use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::limits::{
    DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS, DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS,
    MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS, MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS,
    MAX_TAB_BAR_RIGHT_ENTRIES, MIN_TAB_BAR_COMMAND_INTERVAL_SECONDS,
    MIN_TAB_BAR_COMMAND_TIMEOUT_SECONDS,
};

#[derive(Debug, Clone)]
pub enum ValidatedTabBarRightEntry {
    Zoom,
    Hostname,
    Datetime {
        format: time::format_description::OwnedFormatItem,
    },
    Text {
        text: String,
    },
    Command {
        command: String,
        interval_seconds: NonZeroU64,
        timeout_seconds: NonZeroU64,
    },
}

fn default_datetime_format() -> String {
    "%H:%M".to_string()
}

fn default_command_interval_seconds() -> u64 {
    DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS
}

fn default_command_timeout_seconds() -> u64 {
    DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
// toml-only-serde-shape: `wire::WireTabBarRightEntry` carries this on the wire.
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TabBarRightEntryConfig {
    Zoom,
    Hostname,
    Datetime {
        #[serde(default = "default_datetime_format")]
        format: String,
    },
    Text {
        text: String,
    },
    Command {
        command: String,
        #[serde(default = "default_command_interval_seconds")]
        interval_seconds: u64,
        #[serde(default = "default_command_timeout_seconds")]
        timeout_seconds: u64,
    },
}

pub(crate) fn parse_tab_bar_datetime_format(
    value: &str,
) -> Result<time::format_description::OwnedFormatItem, String> {
    if value.is_empty() {
        return Err("datetime format is empty".into());
    }
    let format = time::format_description::parse_strftime_owned(value)
        .map_err(|err| format!("invalid datetime format: {err}"))?;
    time::PrimitiveDateTime::MIN
        .format(&format)
        .map_err(|err| format!("unsupported datetime format: {err}"))?;
    Ok(format)
}

pub(crate) fn parse_tab_bar_right_entries(
    entries: &[TabBarRightEntryConfig],
) -> Result<Vec<ValidatedTabBarRightEntry>, Vec<String>> {
    let mut diagnostics = Vec::new();
    let mut parsed = Vec::with_capacity(entries.len().min(MAX_TAB_BAR_RIGHT_ENTRIES));
    if entries.len() > MAX_TAB_BAR_RIGHT_ENTRIES {
        diagnostics.push(format!(
            "ui.tab_bar_right may contain at most {MAX_TAB_BAR_RIGHT_ENTRIES} entries"
        ));
    }

    // An over-limit list already has a diagnostic, so validate details only for accepted
    // positions.
    for (index, entry) in entries.iter().enumerate().take(MAX_TAB_BAR_RIGHT_ENTRIES) {
        let parsed_entry = match entry {
            TabBarRightEntryConfig::Datetime { format } => {
                match parse_tab_bar_datetime_format(format) {
                    Ok(format) => Some(ValidatedTabBarRightEntry::Datetime { format }),
                    Err(error) => {
                        diagnostics.push(format!("ui.tab_bar_right[{index}] has {error}"));
                        None
                    }
                }
            }
            TabBarRightEntryConfig::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => {
                let mut valid = true;
                if command.trim().is_empty() {
                    diagnostics.push(format!("ui.tab_bar_right[{index}] command is empty"));
                    valid = false;
                }
                if *interval_seconds < MIN_TAB_BAR_COMMAND_INTERVAL_SECONDS {
                    diagnostics.push(format!(
                        "ui.tab_bar_right[{index}] interval_seconds must be at least {MIN_TAB_BAR_COMMAND_INTERVAL_SECONDS}"
                    ));
                    valid = false;
                }
                if *interval_seconds > MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS {
                    diagnostics.push(format!(
                        "ui.tab_bar_right[{index}] interval_seconds may be at most {MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS}"
                    ));
                    valid = false;
                }
                if *timeout_seconds < MIN_TAB_BAR_COMMAND_TIMEOUT_SECONDS {
                    diagnostics.push(format!(
                        "ui.tab_bar_right[{index}] timeout_seconds must be at least {MIN_TAB_BAR_COMMAND_TIMEOUT_SECONDS}"
                    ));
                    valid = false;
                }
                if *timeout_seconds > MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS {
                    diagnostics.push(format!(
                        "ui.tab_bar_right[{index}] timeout_seconds may be at most {MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS}"
                    ));
                    valid = false;
                }
                if valid {
                    match (
                        NonZeroU64::new(*interval_seconds),
                        NonZeroU64::new(*timeout_seconds),
                    ) {
                        (Some(interval_seconds), Some(timeout_seconds)) => {
                            Some(ValidatedTabBarRightEntry::Command {
                                command: command.clone(),
                                interval_seconds,
                                timeout_seconds,
                            })
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            }
            TabBarRightEntryConfig::Zoom => Some(ValidatedTabBarRightEntry::Zoom),
            TabBarRightEntryConfig::Hostname => Some(ValidatedTabBarRightEntry::Hostname),
            TabBarRightEntryConfig::Text { text } => {
                Some(ValidatedTabBarRightEntry::Text { text: text.clone() })
            }
        };
        if let Some(parsed_entry) = parsed_entry {
            parsed.push(parsed_entry);
        }
    }

    if diagnostics.is_empty() {
        Ok(parsed)
    } else {
        Err(diagnostics)
    }
}

#[cfg(test)]
pub(crate) fn tab_bar_right_diagnostics(entries: &[TabBarRightEntryConfig]) -> Vec<String> {
    match parse_tab_bar_right_entries(entries) {
        Ok(_) => Vec::new(),
        Err(diagnostics) => diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_bar_entries_parse_with_command_defaults() {
        #[derive(Deserialize)]
        struct Wrapper {
            entries: Vec<TabBarRightEntryConfig>,
        }

        let parsed: Wrapper = toml::from_str(
            r#"
entries = [
  { type = "zoom" },
  { type = "hostname" },
  { type = "datetime", format = "%H:%M" },
  { type = "text", text = "prod" },
  { type = "command", command = "status.sh" },
]
"#,
        )
        .expect("parse tab bar entries");

        assert_eq!(parsed.entries.len(), 5);
        let TabBarRightEntryConfig::Command {
            interval_seconds,
            timeout_seconds,
            ..
        } = &parsed.entries[4]
        else {
            panic!("the fifth parsed tab bar entry is a command");
        };
        assert_eq!(*interval_seconds, DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS);
        assert_eq!(*timeout_seconds, DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS);
    }

    #[test]
    fn diagnostics_reject_invalid_datetime_and_command_schedules() {
        let entries = vec![
            TabBarRightEntryConfig::Datetime {
                format: "%Q".into(),
            },
            TabBarRightEntryConfig::Datetime {
                format: "%z".into(),
            },
            TabBarRightEntryConfig::Command {
                command: String::new(),
                interval_seconds: 0,
                timeout_seconds: 0,
            },
            TabBarRightEntryConfig::Command {
                command: "status.sh".into(),
                interval_seconds: MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS + 1,
                timeout_seconds: MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS + 1,
            },
        ];

        let diagnostics = tab_bar_right_diagnostics(&entries).join("\n");
        assert!(diagnostics.contains("invalid datetime format"));
        assert!(diagnostics.contains("unsupported datetime format"));
        assert!(diagnostics.contains("command is empty"));
        assert!(diagnostics.contains("interval_seconds must be at least 1"));
        assert!(diagnostics.contains("interval_seconds may be at most"));
        assert!(diagnostics.contains("timeout_seconds must be at least 1"));
        assert!(diagnostics.contains("timeout_seconds may be at most"));
        assert!(parse_tab_bar_datetime_format("").is_err());
    }
}
