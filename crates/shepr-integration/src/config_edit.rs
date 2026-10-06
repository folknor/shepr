use crate::types::{InstallError, InstallResult};
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use crate::limits::TOML_BASIC_STRING_DELIMITER_BYTES;
use shepr_agent::IntegrationTarget as Target;

use super::command::hook_command;
use super::{KIMI_CONFIG_BLOCK_BEGIN, KIMI_CONFIG_BLOCK_END};

pub(crate) fn ensure_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
    timeout: u64,
    matcher: Option<&str>,
) -> InstallResult<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallError::config_shape(format!("hook entries for {event} must be an array"))
        })?;

    // Identical registrations collapse, while a distinct matcher gets its own
    // group even when it calls the same command for the same event.
    // The matcher sits on the group (see `command_hook_group`), the command on
    // the group's hook entries.
    let already_installed = entries.iter().any(|entry| {
        entry.get("matcher").and_then(Value::as_str) == matcher
            && entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|hook_entries| {
                    hook_entries.iter().any(|hook| {
                        hook.get("type").and_then(Value::as_str) == Some("command")
                            && hook.get("command").and_then(Value::as_str) == Some(command)
                    })
                })
    });
    if already_installed {
        return Ok(());
    }

    entries.push(command_hook_group(command, timeout, matcher));
    Ok(())
}

pub(super) fn command_hook_group(command: &str, timeout: u64, matcher: Option<&str>) -> Value {
    let mut entry = Map::new();
    if let Some(matcher) = matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    entry.insert(
        "hooks".to_string(),
        json!([{
            "type": "command",
            "command": command,
            "timeout": timeout,
        }]),
    );
    Value::Object(entry)
}

/// The description MastraCode's flat hook entries carry; status matches it.
pub(crate) const MASTRACODE_HOOK_DESCRIPTION: &str = "Report MastraCode agent state to shepr";

// These helpers build the canonical entries of one agent's hook shape, which
// install merges into the user's file through `json_edit` and status matches.
// Claude and Codex use nested hook groups:
//   { "matcher": "...", "hooks": [{ "type": "command", ... }] }
// MastraCode uses flat hook entries with a command field and description.
// Copilot uses the direct-command settings shape:
//   { "type": "command", "matcher": "...", "bash": "...", ... }
pub(crate) fn ensure_flat_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
    timeout_ms: u64,
) -> InstallResult<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallError::config_shape(format!("hook entries for {event} must be an array"))
        })?;

    entries.push(json!({
        "type": "command",
        "command": command,
        "timeout": timeout_ms,
        "description": MASTRACODE_HOOK_DESCRIPTION,
    }));
    Ok(())
}

pub(crate) fn ensure_direct_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: String,
    timeout_sec: u64,
    matcher: Option<&str>,
) -> InstallResult<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallError::config_shape(format!("hook entries for {event} must be an array"))
        })?;

    let mut entry = Map::new();
    entry.insert("type".to_string(), Value::String("command".to_string()));
    if let Some(matcher) = matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    // Copilot's direct-command registration is built in a fresh map for each
    // target install. Existing entries are removed by the shared CST editor.
    entry.insert("bash".to_string(), Value::String(command));
    entry.insert("timeoutSec".to_string(), Value::Number(timeout_sec.into()));
    entries.push(Value::Object(entry));
    Ok(())
}

pub(super) const HOOK_COMMAND_FIELDS: &[&str] = &["command", "bash"];

// Cursor hooks.json uses the minimal shape `{ "command": "..." }` documented at
// https://cursor.com/docs/hooks.
pub(crate) fn ensure_simple_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> InstallResult<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallError::config_shape(format!("hook entries for {event} must be an array"))
        })?;

    entries.push(json!({ "command": command }));
    Ok(())
}

/// Enable `features.hooks` in a Codex `config.toml`, preserving source layout
/// when `features` is a table or root-level dotted table. An explicit false is
/// the user's global opt-out, so installation must leave it alone.
pub(crate) fn build_codex_config_with_hooks(content: &str) -> InstallResult<String> {
    let mut document = content.parse::<DocumentMut>().map_err(|error| {
        InstallError::config_unparseable(format!("could not parse Codex config.toml: {error}"))
    })?;

    let Some(features) = document.as_table_mut().get_mut("features") else {
        let mut features = Table::new();
        features.insert("hooks", Item::Value(TomlValue::from(true)));
        document
            .as_table_mut()
            .insert("features", Item::Table(features));
        return Ok(document.to_string());
    };

    if let Some(features) = features.as_table_mut() {
        if features.get("hooks").and_then(Item::as_bool) == Some(false) {
            return Err(InstallError::managed_block_conflict(
                "codex config.toml disables hooks with `features.hooks = false`; leaving the user's setting unchanged",
            ));
        }
        features.insert("hooks", Item::Value(TomlValue::from(true)));
    } else {
        return Err(InstallError::config_shape(
            "codex config.toml declares `features` as an inline table or non-table value; move it \
             to a [features] table (or `features.<key> = ...` lines) and retry",
        ));
    }

    Ok(document.to_string())
}

pub(super) fn build_kimi_config_with_timeout(
    content: &str,
    hook_path: &Path,
    timeout: Duration,
) -> InstallResult<String> {
    let unmarked_content = remove_kimi_config_block(content)?;
    // Only the marked block is safe to rewrite without reformatting user TOML.
    if kimi_config_uses_hook_path(&unmarked_content, hook_path)? {
        return Err(InstallError::managed_block_conflict(
            "kimi config.toml registers the shepr hook outside its managed block; remove that hook and retry",
        ));
    }
    let separator = kimi_line_ending(content);
    let block = kimi_integration_block(timeout).replace('\n', separator);
    let result = if let Some(start) = content
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line))
        })
        .find_map(|(start, line)| (line.trim() == KIMI_CONFIG_BLOCK_BEGIN).then_some(start))
    {
        let mut end = start;
        for line in content[start..].split_inclusive('\n') {
            end += line.len();
            if line.trim() == KIMI_CONFIG_BLOCK_END {
                break;
            }
        }
        // Multiple blocks are ambiguous; never silently drop the later one.
        if content[end..]
            .lines()
            .any(|line| line.trim() == KIMI_CONFIG_BLOCK_BEGIN)
        {
            return Err(InstallError::managed_block_conflict(
                "kimi config.toml contains multiple managed blocks",
            ));
        }
        let block = if content[start..end].ends_with('\n') {
            block
        } else {
            block.trim_end_matches(['\r', '\n']).to_string()
        };
        format!("{}{}{}", &content[..start], block, &content[end..])
    } else {
        let mut result = unmarked_content;
        if !result.is_empty() {
            if !result.ends_with('\n') {
                result.push_str(separator);
            }
            if trailing_line_feed_count(&result) < 2 {
                result.push_str(separator);
            }
        }
        result.push_str(&block);
        result
    };
    result.parse::<DocumentMut>().map_err(|error| {
        InstallError::config_unparseable(format!("could not build Kimi config.toml: {error}"))
    })?;
    Ok(result)
}

pub(super) fn kimi_config_block_with_timeout_is_current(
    content: &str,
    hook_path: &Path,
    timeout: Duration,
) -> InstallResult<bool> {
    let unmarked_content = match remove_kimi_config_block(content) {
        Ok(content) => content,
        Err(_) => return Ok(false),
    };
    if kimi_config_uses_hook_path(&unmarked_content, hook_path)? {
        return Ok(false);
    }

    let expected = kimi_integration_block(timeout);
    let mut actual = String::new();
    let mut in_block = false;
    let mut found_block = false;

    for line in content.split_inclusive('\n') {
        let marker = line.trim();
        if marker == KIMI_CONFIG_BLOCK_BEGIN {
            if found_block {
                return Ok(false);
            }
            found_block = true;
            in_block = true;
        }
        if in_block {
            actual.push_str(line);
        }
        if marker == KIMI_CONFIG_BLOCK_END {
            in_block = false;
        }
    }

    // TOML accepts CRLF, so compare line-ending independent managed bytes.
    Ok(found_block && !in_block && actual.replace("\r\n", "\n") == expected)
}

fn kimi_config_uses_hook_path(content: &str, hook_path: &Path) -> InstallResult<bool> {
    let config = toml::from_str::<toml::Value>(content).map_err(|error| {
        InstallError::config_unparseable(format!("could not parse Kimi config.toml: {error}"))
    })?;
    let expected_commands = Target::Kimi
        .hook_events()
        .iter()
        .map(|event| {
            hook_command(
                Target::Kimi,
                event.action.map(shepr_agent::IntegrationHookAction::as_str),
            )
        })
        .collect::<Vec<_>>();
    let Some(hooks) = config.get("hooks").and_then(toml::Value::as_array) else {
        return Ok(false);
    };
    Ok(hooks.iter().any(|hook| {
        hook.get("command")
            .and_then(toml::Value::as_str)
            .is_some_and(|command| {
                super::json_edit::is_managed_hook_command(
                    command,
                    Target::Kimi,
                    hook_path,
                    &expected_commands,
                )
            })
    }))
}

fn kimi_integration_block(timeout: Duration) -> String {
    let events = Target::Kimi.hook_events();
    let mut block = String::from(KIMI_CONFIG_BLOCK_BEGIN);
    block.push('\n');
    for hook in events {
        let Some(action) = hook.action else {
            continue;
        };
        block.push_str(&kimi_hook_table(
            hook.event,
            hook.matcher,
            action.as_str(),
            timeout,
        ));
    }
    block.push_str(KIMI_CONFIG_BLOCK_END);
    block.push('\n');
    block
}

pub(crate) fn kimi_hook_table(
    event: &str,
    matcher: Option<&str>,
    action: &str,
    timeout: Duration,
) -> String {
    let command = hook_command(Target::Kimi, Some(action));
    let matcher =
        matcher.map_or_default(|matcher| format!("matcher = {}\n", toml_basic_string(matcher)));
    format!(
        "[[hooks]]\nevent = {}\n{matcher}command = {}\ntimeout = {}\n\n",
        toml_basic_string(event),
        toml_basic_string(&command),
        timeout.as_secs()
    )
}

/// Remove shepr's marked block from a Kimi `config.toml`, leaving all bytes
/// outside it untouched. An unmatched begin marker is an error: guessing where
/// the damaged block ends could delete the user's config that follows it.
pub(crate) fn remove_kimi_config_block(content: &str) -> InstallResult<String> {
    let mut result = String::with_capacity(content.len());
    let mut in_block = false;
    let mut removed_block = false;

    for line in content.split_inclusive('\n') {
        let marker = line.trim();
        if marker == KIMI_CONFIG_BLOCK_BEGIN {
            if in_block {
                return Err(unterminated_kimi_block_error());
            }
            in_block = true;
            removed_block = true;
            continue;
        }
        if in_block {
            if marker == KIMI_CONFIG_BLOCK_END {
                in_block = false;
            }
            continue;
        }
        result.push_str(line);
    }

    if in_block {
        return Err(unterminated_kimi_block_error());
    }

    Ok(if removed_block {
        result
    } else {
        content.to_string()
    })
}

fn kimi_line_ending(content: &str) -> &'static str {
    content.rfind('\n').map_or("\n", |index| {
        if index > 0 && content.as_bytes()[index - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        }
    })
}

fn trailing_line_feed_count(content: &str) -> usize {
    content
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| matches!(**byte, b'\r' | b'\n'))
        .filter(|byte| **byte == b'\n')
        .count()
}

fn unterminated_kimi_block_error() -> super::types::InstallError {
    InstallError::managed_block_conflict(format!(
        "kimi config.toml has a `{KIMI_CONFIG_BLOCK_BEGIN}` line without a matching \
             `{KIMI_CONFIG_BLOCK_END}` line; remove the damaged shepr block by hand and retry"
    ))
}

pub(crate) fn toml_basic_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + TOML_BASIC_STRING_DELIMITER_BYTES);
    result.push('"');
    for ch in value.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{08}' => result.push_str("\\b"),
            '\t' => result.push_str("\\t"),
            '\n' => result.push_str("\\n"),
            '\u{0c}' => result.push_str("\\f"),
            '\r' => result.push_str("\\r"),
            ch if ch <= '\u{1f}' || ch == '\u{7f}' => {
                result.push_str(&format!("\\u{:04X}", ch as u32));
            }
            ch => result.push(ch),
        }
    }
    result.push('"');
    result
}

#[cfg(test)]
pub(crate) fn build_kimi_config_with_hooks(
    content: &str,
    hook_path: &Path,
) -> InstallResult<String> {
    build_kimi_config_with_timeout(content, hook_path, super::HOOK_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_kimi_block_preserves_crlf_and_trailing_blank_lines() {
        let content = format!(
            "user = true\r\n\r\n{KIMI_CONFIG_BLOCK_BEGIN}\r\n\
             managed = true\r\n{KIMI_CONFIG_BLOCK_END}\r\n\r\n"
        );

        // The blank line before the block and the one after it both stay.
        assert_eq!(
            remove_kimi_config_block(&content).expect("remove managed block"),
            "user = true\r\n\r\n\r\n"
        );
    }

    #[test]
    fn kimi_status_accepts_crlf_managed_block() {
        let hook_path = Path::new("/home/test/.kimi-code/hooks/shepr-agent-state.sh");
        let config = build_kimi_config_with_timeout(
            "user = true\n\n",
            hook_path,
            super::super::HOOK_TIMEOUT,
        )
        .expect("build config");
        // The whole file as a CRLF editor would save it.
        let crlf = config.replace('\n', "\r\n");

        assert!(
            kimi_config_block_with_timeout_is_current(&crlf, hook_path, super::super::HOOK_TIMEOUT)
                .expect("read status")
        );
    }

    #[test]
    fn kimi_update_preserves_crlf_user_text_and_blank_suffix() {
        let hook_path = Path::new("/home/test/.kimi-code/hooks/shepr-agent-state.sh");
        let original = format!(
            "user = true\r\n\r\n{KIMI_CONFIG_BLOCK_BEGIN}\r\nmanaged = true\r\n\
             {KIMI_CONFIG_BLOCK_END}\r\n\r\n\r\n"
        );

        let updated =
            build_kimi_config_with_timeout(&original, hook_path, super::super::HOOK_TIMEOUT)
                .expect("update Kimi config");

        assert!(updated.starts_with(&format!("user = true\r\n\r\n{KIMI_CONFIG_BLOCK_BEGIN}\r\n")));
        assert!(updated.ends_with(&format!("{KIMI_CONFIG_BLOCK_END}\r\n\r\n\r\n")));
        assert!(!updated.replace("\r\n", "").contains('\n'));
        assert_eq!(
            build_kimi_config_with_timeout(&updated, hook_path, super::super::HOOK_TIMEOUT)
                .expect("repeat update"),
            updated
        );
    }

    #[test]
    fn codex_explicitly_disabled_hooks_are_refused() {
        let content = "model = \"x\"\n[features]\nhooks = false\ncodex_hooks = true\n";

        let error = build_codex_config_with_hooks(content).expect_err("false is an opt-out");

        assert!(error.to_string().contains("features.hooks = false"));
    }

    #[test]
    fn codex_config_edit_does_not_remove_old_feature_keys() {
        let content = "[features]\ncodex_hooks = true\n";

        let updated = build_codex_config_with_hooks(content).expect("enable Codex hooks");

        assert!(updated.contains("codex_hooks = true"));
        assert!(updated.contains("hooks = true"));
    }

    #[test]
    fn same_event_and_command_keep_distinct_matchers() {
        let mut hooks = Map::<String, Value>::new();
        let command = "sh '/hooks/shepr-agent-state.sh'";
        for matcher in [Some("Bash"), Some("Edit"), Some("Bash"), None, None] {
            ensure_command_hook(&mut hooks, "PreToolUse", command, 10, matcher)
                .expect("register the hook");
        }

        let entries = hooks["PreToolUse"].as_array().expect("event groups");
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert_eq!(entries[0]["matcher"], "Bash");
        assert_eq!(entries[1]["matcher"], "Edit");
        assert!(entries[2].get("matcher").is_none());
    }
}
