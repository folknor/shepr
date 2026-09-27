use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use super::command::hook_command;
use super::{
    HERMES_PLUGIN_INSTALL_NAME, KIMI_CONFIG_BLOCK_BEGIN, KIMI_CONFIG_BLOCK_END, KIMI_HOOK_EVENTS,
};

pub(crate) fn ensure_hooks_object<'a>(
    settings: &'a mut Value,
    settings_path: &Path,
    root_description: &str,
    hooks_description: &str,
) -> io::Result<&'a mut Map<String, Value>> {
    let root = settings.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "{root_description} at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    hooks.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "{hooks_description} at {} must be a JSON object",
            settings_path.display()
        ))
    })
}

pub(crate) fn hooks_object_if_present<'a>(
    settings: &'a mut Value,
    settings_path: &Path,
    root_description: &str,
    hooks_description: &str,
) -> io::Result<Option<&'a mut Map<String, Value>>> {
    let root = settings.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "{root_description} at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let Some(hooks) = root.get_mut("hooks") else {
        return Ok(None);
    };

    hooks.as_object_mut().map(Some).ok_or_else(|| {
        io::Error::other(format!(
            "{hooks_description} at {} must be a JSON object",
            settings_path.display()
        ))
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
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

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

    let mut entry = Map::new();
    if let Some(matcher) = matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    entry.insert(
        "hooks".to_string(),
        json!([
            {
                "type": "command",
                "command": command,
                "timeout": timeout,
            }
        ]),
    );

    entries.push(Value::Object(entry));
    Ok(())
}

// Claude and Codex use nested hook groups:
//   { "matcher": "...", "hooks": [{ "type": "command", ... }] }
// Copilot uses the flatter settings shape:
//   { "type": "command", "matcher": "...", "bash": "...", ... }
// Keep the helpers separate so install/uninstall preserves unrelated hooks in
// each agent's native format instead of normalizing user configuration.
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
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    if entries.iter().any(|entry| {
        entry.get("type").and_then(Value::as_str) == Some("command")
            && entry.get("command").and_then(Value::as_str) == Some(command)
    }) {
        return Ok(());
    }

    entries.push(json!({
        "type": "command",
        "command": command,
        "timeout": timeout_ms,
        "description": "Report MastraCode agent state to Shepr",
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
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

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

pub(crate) fn direct_command_field() -> &'static str {
    "bash"
}

pub(crate) fn is_matching_direct_command_entry(entry: &Value, command: &str) -> bool {
    entry.get("command").and_then(Value::as_str) == Some(command)
        || entry.get("bash").and_then(Value::as_str) == Some(command)
}

pub(crate) fn remove_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };

    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    let mut removed = false;
    entries.retain_mut(|entry| {
        let Some(entry_object) = entry.as_object_mut() else {
            return true;
        };
        let Some(hook_entries) = entry_object.get_mut("hooks") else {
            return true;
        };
        let Some(hook_entries) = hook_entries.as_array_mut() else {
            return true;
        };

        let before = hook_entries.len();
        hook_entries.retain(|hook| !is_matching_command_hook(hook, command));
        if hook_entries.len() != before {
            removed = true;
        }

        !hook_entries.is_empty()
    });

    let remove_event = entries.is_empty();
    if remove_event {
        hooks.remove(event);
    }

    Ok(removed)
}

pub(crate) fn remove_flat_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };

    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    let before = entries.len();
    entries.retain(|entry| {
        !(entry.get("type").and_then(Value::as_str) == Some("command")
            && entry.get("command").and_then(Value::as_str) == Some(command))
    });
    let removed = entries.len() != before;
    if entries.is_empty() {
        hooks.remove(event);
    }
    Ok(removed)
}

pub(crate) fn remove_direct_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };

    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    let before = entries.len();
    entries.retain(|entry| {
        !(entry.get("type").and_then(Value::as_str) == Some("command")
            && is_matching_direct_command_entry(entry, command))
    });
    let removed = entries.len() != before;
    if entries.is_empty() {
        hooks.remove(event);
    }
    Ok(removed)
}

// Cursor hooks.json uses the minimal shape `{ "command": "..." }` documented at
// https://cursor.com/docs/hooks. Keep this separate from the nested codex and
// flat copilot helpers so install/uninstall does not rewrite unrelated hooks.
pub(crate) fn ensure_simple_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> io::Result<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    if entries
        .iter()
        .any(|entry| entry.get("command").and_then(Value::as_str) == Some(command))
    {
        return Ok(());
    }

    entries.push(json!({ "command": command }));
    Ok(())
}

pub(crate) fn remove_simple_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };

    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    let before = entries.len();
    entries.retain(|entry| entry.get("command").and_then(Value::as_str) != Some(command));
    let removed = entries.len() != before;
    if entries.is_empty() {
        hooks.remove(event);
    }
    Ok(removed)
}

pub(crate) fn remove_hook_commands(
    hooks: &mut Map<String, Value>,
    event: &str,
    hook_path: &Path,
    action: Option<&str>,
) -> io::Result<bool> {
    remove_command_hook(hooks, event, &hook_command(hook_path, action))
}

pub(crate) fn remove_direct_hook_commands(
    hooks: &mut Map<String, Value>,
    event: &str,
    hook_path: &Path,
    action: Option<&str>,
) -> io::Result<bool> {
    remove_direct_command_hook(hooks, event, &hook_command(hook_path, action))
}

pub(crate) fn is_matching_command_hook(hook: &Value, command: &str) -> bool {
    hook.get("type").and_then(Value::as_str) == Some("command")
        && hook.get("command").and_then(Value::as_str) == Some(command)
}

pub(crate) fn ensure_hermes_plugin_enabled(content: &str) -> io::Result<String> {
    try_update_hermes_enabled_plugin(content, true)
}

pub(crate) fn remove_hermes_plugin_enabled(content: &str) -> io::Result<String> {
    try_update_hermes_enabled_plugin(content, false)
}

fn try_update_hermes_enabled_plugin(content: &str, enabled: bool) -> io::Result<String> {
    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let Some(plugins_index) = top_level_yaml_key_index(&lines, "plugins") else {
        if !enabled {
            return Ok(content.to_string());
        }
        let mut result = content.trim_end_matches('\n').to_string();
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("plugins:\n  enabled:\n    - shepr-agent-state\n");
        return Ok(result);
    };

    let plugins_end =
        next_top_level_yaml_key_index(&lines, plugins_index + 1).unwrap_or(lines.len());
    if !hermes_yaml_layout_is_editable(&lines, plugins_index, plugins_end) {
        // The refusal asks the user to make the edit by hand, so a retry must
        // accept a hand-edited file: an unsupported layout that already names
        // the plugin (outside comments) counts as enabled, one that does not
        // counts as disabled. Anything else still needs the manual edit.
        let mentions_plugin = lines[plugins_index..plugins_end]
            .iter()
            .any(|line| strip_yaml_inline_comment(line).contains(HERMES_PLUGIN_INSTALL_NAME));
        if mentions_plugin == enabled {
            return Ok(content.to_string());
        }
        return Err(hermes_yaml_manual_edit_error(enabled));
    }
    let plugins_inline_items = yaml_key_value_at_indent(&lines[plugins_index], 0, "plugins")
        .and_then(yaml_flow_sequence_items);
    let enabled_index = lines[plugins_index + 1..plugins_end]
        .iter()
        .position(|line| yaml_key_at_indent(line, 2) == Some("enabled"))
        .map(|offset| plugins_index + 1 + offset);
    let flat_list_start = lines[plugins_index + 1..plugins_end]
        .iter()
        .position(|line| yaml_list_item_value_at_indent(line, 2).is_some())
        .map(|offset| plugins_index + 1 + offset);

    if let Some(enabled_index) = enabled_index {
        if let Some(mut items) = yaml_key_value_at_indent(&lines[enabled_index], 2, "enabled")
            .and_then(yaml_flow_sequence_items)
        {
            let existing_item_index = items
                .iter()
                .position(|item| yaml_scalar_value(item) == HERMES_PLUGIN_INSTALL_NAME);

            match (enabled, existing_item_index) {
                (true, Some(_)) | (false, None) => return Ok(content.to_string()),
                (true, None) => items.insert(0, HERMES_PLUGIN_INSTALL_NAME.to_string()),
                (false, Some(index)) => {
                    items.remove(index);
                }
            }

            let comment = yaml_inline_comment(&lines[enabled_index]);
            let replacement = hermes_enabled_plugin_lines(&items, comment);
            lines.splice(enabled_index..enabled_index + 1, replacement);
            return Ok(join_yaml_lines(&lines, trailing_newline));
        }

        let list_start = enabled_index + 1;
        let list_end = lines[list_start..plugins_end]
            .iter()
            .position(|line| {
                yaml_indent(line).is_some_and(|indent| indent <= 2) && yaml_key_name(line).is_some()
            })
            .map(|offset| list_start + offset)
            .unwrap_or(plugins_end);
        let existing_item_index = lines[list_start..list_end]
            .iter()
            .position(|line| yaml_list_item_matches(line, HERMES_PLUGIN_INSTALL_NAME))
            .map(|offset| list_start + offset);

        match (enabled, existing_item_index) {
            (true, Some(_)) | (false, None) => return Ok(content.to_string()),
            (true, None) => lines.insert(list_start, "    - shepr-agent-state".to_string()),
            (false, Some(index)) => {
                lines.remove(index);
            }
        }
        return Ok(join_yaml_lines(&lines, trailing_newline));
    }

    if let Some(mut items) = plugins_inline_items {
        let existing_item_index = items
            .iter()
            .position(|item| yaml_scalar_value(item) == HERMES_PLUGIN_INSTALL_NAME);

        match (enabled, existing_item_index) {
            (true, Some(_)) | (false, None) => return Ok(content.to_string()),
            (true, None) => items.insert(0, HERMES_PLUGIN_INSTALL_NAME.to_string()),
            (false, Some(index)) => {
                items.remove(index);
            }
        }

        let comment = yaml_inline_comment(&lines[plugins_index]);
        let replacement = hermes_flat_plugin_lines(&items, comment);
        lines.splice(plugins_index..plugins_end, replacement);
        return Ok(join_yaml_lines(&lines, trailing_newline));
    }

    if let Some(flat_list_start) = flat_list_start {
        let existing_item_index = lines[plugins_index + 1..plugins_end]
            .iter()
            .position(|line| yaml_list_item_matches_at_indent(line, 2, HERMES_PLUGIN_INSTALL_NAME))
            .map(|offset| plugins_index + 1 + offset);

        match (enabled, existing_item_index) {
            (true, Some(_)) | (false, None) => return Ok(content.to_string()),
            (true, None) => lines.insert(flat_list_start, "  - shepr-agent-state".to_string()),
            (false, Some(index)) => {
                lines.remove(index);
            }
        }
        return Ok(join_yaml_lines(&lines, trailing_newline));
    }

    if enabled {
        lines.insert(plugins_index + 1, "  enabled:".to_string());
        lines.insert(plugins_index + 2, "    - shepr-agent-state".to_string());
        return Ok(join_yaml_lines(&lines, trailing_newline));
    }

    Ok(content.to_string())
}

/// The Hermes editor preserves the user's source text, so it only edits layouts
/// its line-based parser understands. A flow mapping or non-canonical block
/// indentation cannot safely receive the canonical two-space insertion, and a
/// block `enabled:` list must hold its items at four spaces so the canonical
/// `    - shepr-agent-state` line joins the same sequence.
fn hermes_yaml_layout_is_editable(
    lines: &[String],
    plugins_index: usize,
    plugins_end: usize,
) -> bool {
    let plugin_value = yaml_key_value_at_indent(&lines[plugins_index], 0, "plugins")
        .map(strip_yaml_inline_comment)
        .map(str::trim)
        .unwrap_or_default();
    if !plugin_value.is_empty() && yaml_flow_sequence_items(plugin_value).is_none() {
        return false;
    }

    let Some(child_index) =
        (plugins_index + 1..plugins_end).find(|index| yaml_indent(&lines[*index]).is_some())
    else {
        return true;
    };
    if yaml_indent(&lines[child_index]) != Some(2) {
        return false;
    }

    for index in plugins_index + 1..plugins_end {
        if yaml_key_name(&lines[index]) != Some("enabled") {
            continue;
        }
        if yaml_indent(&lines[index]) != Some(2) {
            return false;
        }
        let value = yaml_key_value_at_indent(&lines[index], 2, "enabled")
            .map(strip_yaml_inline_comment)
            .map(str::trim)
            .unwrap_or_default();
        if !value.is_empty() {
            if yaml_flow_sequence_items(value).is_none() {
                return false;
            }
            continue;
        }
        for line in &lines[index + 1..plugins_end] {
            let Some(indent) = yaml_indent(line) else {
                continue;
            };
            if indent <= 2 && yaml_key_name(line).is_some() {
                break;
            }
            // Items at the key's own indent, or a nested mapping, would not
            // share a sequence with the inserted four-space item.
            if indent < 4 || (indent == 4 && yaml_list_item_value(line).is_none()) {
                return false;
            }
        }
    }

    true
}

fn hermes_yaml_manual_edit_error(enabled: bool) -> io::Error {
    let edit = if enabled {
        "add `shepr-agent-state` to"
    } else {
        "remove `shepr-agent-state` from"
    };
    io::Error::other(format!(
        "Hermes config.yaml uses a YAML layout shepr cannot safely edit; {edit} `plugins.enabled` \
         by hand and retry"
    ))
}

/// String-returning wrapper for unit tests of supported layouts; it panics on
/// a refusal so a test cannot mistake one for an unchanged file.
#[cfg(test)]
pub(crate) fn update_hermes_enabled_plugin(content: &str, enabled: bool) -> String {
    try_update_hermes_enabled_plugin(content, enabled)
        .unwrap_or_else(|err| panic!("Hermes layout refused: {err}"))
}

pub(crate) fn hermes_flat_plugin_lines(items: &[String], comment: Option<&str>) -> Vec<String> {
    if items.is_empty() {
        return vec![with_yaml_inline_comment("plugins: []".to_string(), comment)];
    }

    let mut lines = vec![with_yaml_inline_comment("plugins:".to_string(), comment)];
    lines.extend(items.iter().map(|item| format!("  - {item}")));
    lines
}

pub(crate) fn hermes_enabled_plugin_lines(items: &[String], comment: Option<&str>) -> Vec<String> {
    if items.is_empty() {
        return vec![with_yaml_inline_comment(
            "  enabled: []".to_string(),
            comment,
        )];
    }

    let mut lines = vec![with_yaml_inline_comment("  enabled:".to_string(), comment)];
    lines.extend(items.iter().map(|item| format!("    - {item}")));
    lines
}

fn with_yaml_inline_comment(mut line: String, comment: Option<&str>) -> String {
    if let Some(comment) = comment {
        line.push(' ');
        line.push_str(comment.trim_end());
    }
    line
}

pub(crate) fn top_level_yaml_key_index(lines: &[String], key: &str) -> Option<usize> {
    lines
        .iter()
        .position(|line| yaml_key_at_indent(line, 0) == Some(key))
}

pub(crate) fn next_top_level_yaml_key_index(lines: &[String], start: usize) -> Option<usize> {
    lines[start..]
        .iter()
        .position(|line| yaml_indent(line) == Some(0) && yaml_key_name(line).is_some())
        .map(|offset| start + offset)
}

pub(crate) fn yaml_key_at_indent(line: &str, indent: usize) -> Option<&str> {
    if yaml_indent(line)? != indent {
        return None;
    }
    yaml_key_name(line)
}

pub(crate) fn yaml_key_value_at_indent<'a>(
    line: &'a str,
    indent: usize,
    key: &str,
) -> Option<&'a str> {
    if yaml_indent(line)? != indent {
        return None;
    }
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    let (line_key, value) = trimmed.split_once(':')?;
    (line_key.trim() == key).then_some(value.trim())
}

pub(crate) fn yaml_key_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    let (key, _) = trimmed.split_once(':')?;
    let key = key.trim();
    (!key.is_empty()).then_some(key)
}

pub(crate) fn yaml_indent(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    Some(line.len() - trimmed.len())
}

pub(crate) fn yaml_list_item_value(line: &str) -> Option<&str> {
    line.trim().strip_prefix("- ").map(str::trim)
}

pub(crate) fn yaml_list_item_matches(line: &str, value: &str) -> bool {
    yaml_list_item_value(line).is_some_and(|item| yaml_scalar_value(item) == value)
}

pub(crate) fn yaml_list_item_value_at_indent(line: &str, indent: usize) -> Option<&str> {
    if yaml_indent(line)? != indent {
        return None;
    }
    yaml_list_item_value(line)
}

pub(crate) fn yaml_list_item_matches_at_indent(line: &str, indent: usize, value: &str) -> bool {
    yaml_list_item_value_at_indent(line, indent)
        .is_some_and(|item| yaml_scalar_value(item) == value)
}

pub(crate) fn yaml_flow_sequence_items(value: &str) -> Option<Vec<String>> {
    let value = strip_yaml_inline_comment(value).trim();
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }

    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut nested_collections = Vec::new();

    for ch in inner.chars() {
        if let Some(quote_char) = quote {
            current.push(ch);
            if quote_char == '"' && ch == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if ch == quote_char && !escaped {
                quote = None;
            }
            escaped = false;
            continue;
        }

        match ch {
            '"' | '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            '[' | '{' => {
                nested_collections.push(ch);
                current.push(ch);
            }
            ']' | '}' => {
                let expected_open = if ch == ']' { '[' } else { '{' };
                if nested_collections.pop() != Some(expected_open) {
                    return None;
                }
                current.push(ch);
            }
            ',' if nested_collections.is_empty() => {
                if current.trim().is_empty() {
                    return None;
                }
                items.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if quote.is_some() || !nested_collections.is_empty() {
        return None;
    }

    if current.trim().is_empty() {
        if inner.ends_with(',') {
            return Some(items);
        }
        return None;
    }
    items.push(current.trim().to_string());
    Some(items)
}

pub(crate) fn yaml_scalar_value(value: &str) -> String {
    let value = strip_yaml_inline_comment(value).trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let quoted = (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'');
        if quoted {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

pub(crate) fn strip_yaml_inline_comment(value: &str) -> &str {
    match yaml_inline_comment(value) {
        Some(comment) => value[..value.len() - comment.len()].trim_end(),
        None => value,
    }
}

pub(crate) fn yaml_inline_comment(value: &str) -> Option<&str> {
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in value.char_indices() {
        if let Some(quote_char) = quote {
            if quote_char == '"' && ch == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if ch == quote_char && !escaped {
                quote = None;
            }
            escaped = false;
            continue;
        }

        match ch {
            '"' | '\'' => quote = Some(ch),
            '#' if index == 0 || value[..index].ends_with(char::is_whitespace) => {
                return Some(&value[index..]);
            }
            _ => {}
        }
    }

    None
}

pub(crate) fn join_yaml_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut result = lines.join("\n");
    if trailing_newline || result.is_empty() {
        result.push('\n');
    }
    result
}

/// Enable `features.hooks` in a Codex `config.toml`, preserving source layout
/// when `features` is a table or root-level dotted table.
pub(crate) fn build_codex_config_with_hooks(content: &str) -> io::Result<String> {
    let mut document = content
        .parse::<DocumentMut>()
        .map_err(|error| io::Error::other(format!("could not parse Codex config.toml: {error}")))?;

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
        return Err(io::Error::other(
            "codex config.toml declares `features` as an inline table or non-table value; move it \
             to a [features] table (or `features.<key> = ...` lines) and retry",
        ));
    }

    Ok(document.to_string())
}

pub(crate) fn build_kimi_config_with_hooks(content: &str, hook_path: &Path) -> io::Result<String> {
    let mut result = remove_kimi_config_block(content)?
        .trim_end_matches('\n')
        .to_string();
    if !result.is_empty() {
        result.push('\n');
        result.push('\n');
    }

    result.push_str(KIMI_CONFIG_BLOCK_BEGIN);
    result.push('\n');
    for hook in KIMI_HOOK_EVENTS {
        let Some(action) = hook.action else {
            continue;
        };
        result.push_str(&kimi_hook_table(
            hook.event,
            hook.matcher,
            hook_path,
            action.as_str(),
        ));
    }
    result.push_str(KIMI_CONFIG_BLOCK_END);
    result.push('\n');
    Ok(result)
}

pub(crate) fn kimi_hook_table(
    event: &str,
    matcher: Option<&str>,
    hook_path: &Path,
    action: &str,
) -> String {
    let command = hook_command(hook_path, Some(action));
    let matcher = matcher
        .map(|matcher| format!("matcher = {}\n", toml_basic_string(matcher)))
        .unwrap_or_default();
    format!(
        "[[hooks]]\nevent = {}\n{matcher}command = {}\ntimeout = 10\n\n",
        toml_basic_string(event),
        toml_basic_string(&command)
    )
}

/// Remove shepr's marked block from a Kimi `config.toml`. A BEGIN marker
/// without a matching END marker is an error: guessing where the damaged
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
    io::Error::other(format!(
        "kimi config.toml has a `{KIMI_CONFIG_BLOCK_BEGIN}` line without a matching \
         `{KIMI_CONFIG_BLOCK_END}` line; remove the damaged shepr block by hand and retry"
    ))
}

pub(crate) fn toml_basic_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
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
