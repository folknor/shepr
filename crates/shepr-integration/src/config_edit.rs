use std::io;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use crate::limits::TOML_BASIC_STRING_DELIMITER_BYTES;
use shepr_agent::IntegrationTarget as Target;

use super::command::{hook_command, is_hook_command_for_path};
use super::types::{InstallErrorKind, InstallIssue};
use super::{KIMI_CONFIG_BLOCK_BEGIN, KIMI_CONFIG_BLOCK_END};

pub(crate) fn ensure_hooks_object<'a>(
    settings: &'a mut Value,
    settings_path: &Path,
    root_description: &str,
    hooks_description: &str,
) -> io::Result<&'a mut Map<String, Value>> {
    let root = settings.as_object_mut().ok_or_else(|| {
        InstallIssue::io_error(
            InstallErrorKind::ConfigShape,
            format!(
                "{root_description} at {} must be a JSON object",
                settings_path.display()
            ),
        )
    })?;

    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    hooks.as_object_mut().ok_or_else(|| {
        InstallIssue::io_error(
            InstallErrorKind::ConfigShape,
            format!(
                "{hooks_description} at {} must be a JSON object",
                settings_path.display()
            ),
        )
    })
}

pub(crate) fn ensure_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
    timeout: u64,
    matcher: Option<&str>,
) -> io::Result<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallIssue::io_error(
                InstallErrorKind::ConfigShape,
                format!("hook entries for {event} must be an array"),
            )
        })?;

    // Claude preserves an already canonical entry so its settings text stays
    // untouched; in that path this helper must not append a duplicate.
    let already_installed = entries.iter().any(|entry| {
        entry
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
pub(crate) const MASTRACODE_HOOK_DESCRIPTION: &str = "Report MastraCode agent state to Shepr";

// Claude and Codex use nested hook groups:
//   { "matcher": "...", "hooks": [{ "type": "command", ... }] }
// Copilot uses the flatter settings shape:
//   { "type": "command", "matcher": "...", "bash": "...", ... }
// Keep the helpers separate so install preserves unrelated hooks in
// each agent's native format instead of normalizing user configuration.
// Appends unconditionally: the caller strips entries carrying this hook path first.
pub(crate) fn ensure_flat_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
    timeout_ms: u64,
) -> io::Result<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallIssue::io_error(
                InstallErrorKind::ConfigShape,
                format!("hook entries for {event} must be an array"),
            )
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
) -> io::Result<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallIssue::io_error(
                InstallErrorKind::ConfigShape,
                format!("hook entries for {event} must be an array"),
            )
        })?;

    let command_field = direct_command_field();
    if let Some(entry) = entries.iter_mut().find(|entry| {
        entry.get("type").and_then(Value::as_str) == Some("command")
            && is_matching_direct_command_entry(entry, command.as_str())
    }) {
        let Some(entry_object) = entry.as_object_mut() else {
            return Ok(());
        };
        entry_object.remove("command");
        entry_object.remove("bash");
        entry_object.insert(command_field.to_string(), Value::String(command.clone()));
        entry_object.insert("timeoutSec".to_string(), Value::Number(timeout_sec.into()));
        match matcher {
            Some(matcher) => {
                entry_object.insert("matcher".to_string(), Value::String(matcher.to_string()));
            }
            None => {
                entry_object.remove("matcher");
            }
        }
        return Ok(());
    }

    let mut entry = Map::new();
    entry.insert("type".to_string(), Value::String("command".to_string()));
    if let Some(matcher) = matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    entry.insert(command_field.to_string(), Value::String(command));
    entry.insert("timeoutSec".to_string(), Value::Number(timeout_sec.into()));
    entries.push(Value::Object(entry));
    Ok(())
}

pub(super) const HOOK_COMMAND_FIELDS: &[&str] = &["command", "bash"];

pub(crate) fn direct_command_field() -> &'static str {
    "bash"
}

pub(crate) fn is_matching_direct_command_entry(entry: &Value, command: &str) -> bool {
    HOOK_COMMAND_FIELDS
        .iter()
        .any(|field| entry.get(*field).and_then(Value::as_str) == Some(command))
}

// Cursor hooks.json uses the minimal shape `{ "command": "..." }` documented at
// https://cursor.com/docs/hooks. Keep this separate from the nested codex and
// flat copilot helpers so install does not rewrite unrelated hooks.
// Appends unconditionally: the caller strips entries carrying this hook path first.
pub(crate) fn ensure_simple_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> io::Result<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            InstallIssue::io_error(
                InstallErrorKind::ConfigShape,
                format!("hook entries for {event} must be an array"),
            )
        })?;

    entries.push(json!({ "command": command }));
    Ok(())
}

pub(crate) fn remove_hook_path_commands(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
) -> io::Result<bool> {
    remove_hook_path_commands_preserving(hooks, hook_path, None)
}

pub(crate) fn remove_hook_path_commands_preserving(
    events: &mut Map<String, Value>,
    hook_path: &Path,
    preserve: Option<(&str, &Value)>,
) -> io::Result<bool> {
    let mut removed = false;
    let mut preserved = false;
    let mut empty_events = Vec::new();
    for (event, entries_value) in events.iter_mut() {
        let entries = entries_value.as_array_mut().ok_or_else(|| {
            InstallIssue::io_error(
                InstallErrorKind::ConfigShape,
                format!("hook entries for {event} must be an array"),
            )
        })?;
        let mut removed_in_event = false;
        entries.retain_mut(|entry| {
            if !preserved
                && preserve.is_some_and(|(preserve_event, canonical)| {
                    preserve_event == event && canonical == entry
                })
            {
                preserved = true;
                return true;
            }
            let Some(command_entries) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                let owned = value_uses_hook_path(entry, hook_path);
                removed |= owned;
                removed_in_event |= owned;
                return !owned;
            };
            let mut removed_in_group = false;
            command_entries.retain(|command_entry| {
                let owned = value_uses_hook_path(command_entry, hook_path);
                removed |= owned;
                removed_in_group |= owned;
                removed_in_event |= owned;
                !owned
            });
            if removed_in_group && command_entries.is_empty() {
                return false;
            }
            if value_uses_hook_path(entry, hook_path) {
                removed = true;
                removed_in_event = true;
                return false;
            }
            true
        });
        if removed_in_event && entries.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        events.remove(&event);
    }
    Ok(removed)
}

fn value_uses_hook_path(value: &Value, hook_path: &Path) -> bool {
    HOOK_COMMAND_FIELDS.iter().any(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .is_some_and(|command| is_hook_command_for_path(command, hook_path))
    })
}

/// Enable `features.hooks` in a Codex `config.toml`, preserving source layout
/// when `features` is a table or root-level dotted table.
pub(crate) fn build_codex_config_with_hooks(content: &str) -> io::Result<String> {
    let mut document = content.parse::<DocumentMut>().map_err(|error| {
        InstallIssue::io_error(
            InstallErrorKind::ConfigUnparseable,
            format!("could not parse Codex config.toml: {error}"),
        )
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
        features.remove("codex_hooks");
        features.insert("hooks", Item::Value(TomlValue::from(true)));
    } else {
        return Err(InstallIssue::io_error(
            InstallErrorKind::ConfigShape,
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
) -> io::Result<String> {
    let unmarked_content = remove_kimi_config_block(content)?;
    // Only the marked block is safe to rewrite without reformatting user TOML.
    if kimi_config_uses_hook_path(&unmarked_content, hook_path)? {
        return Err(InstallIssue::io_error(
            InstallErrorKind::ManagedBlockConflict,
            "kimi config.toml registers the Shepr hook outside its managed block; remove that hook and retry",
        ));
    }
    let mut result = unmarked_content.trim_end_matches('\n').to_string();
    if !result.is_empty() {
        result.push('\n');
        result.push('\n');
    }

    result.push_str(&kimi_integration_block(hook_path, timeout));
    result.parse::<DocumentMut>().map_err(|error| {
        InstallIssue::io_error(
            InstallErrorKind::ConfigUnparseable,
            format!("could not build Kimi config.toml: {error}"),
        )
    })?;
    Ok(result)
}

pub(super) fn kimi_config_block_with_timeout_is_current(
    content: &str,
    hook_path: &Path,
    timeout: Duration,
) -> io::Result<bool> {
    let unmarked_content = match remove_kimi_config_block(content) {
        Ok(content) => content,
        Err(_) => return Ok(false),
    };
    if kimi_config_uses_hook_path(&unmarked_content, hook_path)? {
        return Ok(false);
    }

    let expected = kimi_integration_block(hook_path, timeout);
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

    Ok(found_block && !in_block && actual == expected)
}

fn kimi_config_uses_hook_path(content: &str, hook_path: &Path) -> io::Result<bool> {
    let config = toml::from_str::<toml::Value>(content).map_err(|error| {
        InstallIssue::io_error(
            InstallErrorKind::ConfigUnparseable,
            format!("could not parse Kimi config.toml: {error}"),
        )
    })?;
    Ok(config
        .get("hooks")
        .and_then(toml::Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|command| is_hook_command_for_path(command, hook_path))
            })
        }))
}

fn kimi_integration_block(hook_path: &Path, timeout: Duration) -> String {
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
            hook_path,
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
    hook_path: &Path,
    action: &str,
    timeout: Duration,
) -> String {
    let command = hook_command(hook_path, Some(action));
    let matcher =
        matcher.map_or_default(|matcher| format!("matcher = {}\n", toml_basic_string(matcher)));
    format!(
        "[[hooks]]\nevent = {}\n{matcher}command = {}\ntimeout = {}\n\n",
        toml_basic_string(event),
        toml_basic_string(&command),
        timeout.as_secs()
    )
}

/// Remove shepr's marked block from a Kimi `config.toml`. A begin marker
/// without a matching end marker is an error: guessing where the damaged
/// block ends could delete the user's config that follows it.
pub(crate) fn remove_kimi_config_block(content: &str) -> io::Result<String> {
    let trailing_newline = content.ends_with('\n');
    let mut lines = Vec::new();
    let mut in_block = false;
    let mut removed_block = false;

    for line in content.lines() {
        if line.trim() == KIMI_CONFIG_BLOCK_BEGIN {
            if in_block {
                return Err(unterminated_kimi_block_error());
            }
            in_block = true;
            removed_block = true;
            continue;
        }
        if in_block {
            if line.trim() == KIMI_CONFIG_BLOCK_END {
                in_block = false;
            }
            continue;
        }
        lines.push(line.to_string());
    }

    if in_block {
        return Err(unterminated_kimi_block_error());
    }

    if !removed_block {
        return Ok(content.to_string());
    }

    let mut result = join_toml_lines(&lines, trailing_newline);
    while result.ends_with("\n\n") {
        result.pop();
    }
    if result == "\n" {
        Ok(String::new())
    } else {
        Ok(result)
    }
}

fn unterminated_kimi_block_error() -> io::Error {
    InstallIssue::io_error(
        InstallErrorKind::ManagedBlockConflict,
        format!(
            "kimi config.toml has a `{KIMI_CONFIG_BLOCK_BEGIN}` line without a matching \
             `{KIMI_CONFIG_BLOCK_END}` line; remove the damaged shepr block by hand and retry"
        ),
    )
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

pub(crate) fn join_toml_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut result = lines.join("\n");
    if trailing_newline || result.is_empty() {
        result.push('\n');
    }
    result
}

#[cfg(test)]
pub(crate) fn build_kimi_config_with_hooks(content: &str, hook_path: &Path) -> io::Result<String> {
    build_kimi_config_with_timeout(content, hook_path, super::HOOK_TIMEOUT)
}
