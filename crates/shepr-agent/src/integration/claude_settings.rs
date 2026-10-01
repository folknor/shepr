use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::Duration;

use jsonc_parser::ast::{Array as AstArray, Object as AstObject, Value as AstValue};
use jsonc_parser::common::Ranged;
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{CollectOptions, ParseOptions, json, parse_to_ast};
use serde_json::{Map, Value, json as serde_json_value};

use crate::agent::resume::AgentSessionStartSource;
use crate::agent::{IntegrationHookAction, IntegrationHookEvent};

use super::command::{hook_command, is_hook_command_for_path};
use super::config_edit::{
    ensure_command_hook, ensure_hooks_object, remove_hook_path_commands_preserving,
};

// This is Claude's event-source subset; the shared source enum covers the
// broader vocabulary reported by the other integrations.
const CLAUDE_SESSION_START_SOURCES: &[AgentSessionStartSource] = &[
    AgentSessionStartSource::Startup,
    AgentSessionStartSource::Resume,
    AgentSessionStartSource::Clear,
    AgentSessionStartSource::Compact,
    AgentSessionStartSource::Fork,
];

pub(crate) fn claude_session_start_matcher() -> String {
    let mut matcher = String::from("^(");
    for (index, source) in CLAUDE_SESSION_START_SOURCES.iter().enumerate() {
        if index > 0 {
            matcher.push('|');
        }
        matcher.push_str(source.as_str());
    }
    matcher.push_str(")$");
    matcher
}

pub(crate) fn install(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
    events: &[IntegrationHookEvent],
    timeout: Duration,
) -> io::Result<String> {
    let hook = claude_hook_event(events)?;
    let event = hook.event;
    let action = hook.action.map(IntegrationHookAction::as_str);
    let original = parse_value(content, settings_path)?;
    let matcher = claude_session_start_matcher();
    let mut desired = original.clone();
    let hooks = ensure_hooks_object(
        &mut desired,
        settings_path,
        "claude settings",
        "claude settings hooks",
    )?;
    let canonical = canonical_hook_value(hook_path, &matcher, action, timeout.as_secs());
    apply_value_removals(hooks, hook_path, &canonical, event)?;
    ensure_command_hook(
        hooks,
        event,
        &hook_command(hook_path, action),
        timeout.as_secs(),
        Some(&matcher),
    )?;

    if desired == original {
        return Ok(content.to_string());
    }

    rewrite(
        content,
        settings_path,
        hook_path,
        &desired,
        &matcher,
        event,
        action,
        timeout.as_secs(),
    )
}

fn claude_hook_event(events: &[IntegrationHookEvent]) -> io::Result<&IntegrationHookEvent> {
    // This source-preserving editor handles one SessionStart matcher group.
    match events {
        [event] if event.event == "SessionStart" => Ok(event),
        _ => Err(io::Error::other(
            "Claude settings integration requires exactly one SessionStart hook event",
        )),
    }
}

fn apply_value_removals(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
    canonical: &Value,
    event: &str,
) -> io::Result<()> {
    let _ = remove_hook_path_commands_preserving(hooks, hook_path, Some((event, canonical)))?;
    Ok(())
}

fn rewrite(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
    desired: &Value,
    matcher: &str,
    event: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> io::Result<String> {
    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })?;
    let root_value = root.value().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    reject_duplicate_keys(&root_value, settings_path)?;
    let root_object = root_value.as_object().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let hooks = match root_object.get("hooks") {
        Some(property) => property.object_value().ok_or_else(|| {
            io::Error::other(format!(
                "claude settings hooks at {} must be a JSON object",
                settings_path.display()
            ))
        })?,
        None if direct_children_are_compact(&root_object.children()) => {
            let updated = append_hooks_property_compact(
                content,
                hook_path,
                settings_path,
                event,
                matcher,
                action,
                timeout_seconds,
            )?;
            return verify_updated(updated, settings_path, desired);
        }
        None => root_object
            .append("hooks", CstInputValue::Object(Vec::new()))
            .object_value()
            .ok_or_else(|| io::Error::other("failed to create claude settings hooks object"))?,
    };

    let canonical = canonical_hook_value(hook_path, matcher, action, timeout_seconds);
    let canonical_preserved = remove_hook_path_commands(&hooks, hook_path, event, &canonical)?;

    if !canonical_preserved {
        match hooks.get(event) {
            Some(property) => {
                let session_start = property.array_value().ok_or_else(|| {
                    io::Error::other(format!("hook entries for {event} must be an array"))
                })?;
                if direct_children_are_compact(&session_start.children()) {
                    let updated = append_session_entry_compact(
                        &root.to_string(),
                        hook_path,
                        settings_path,
                        event,
                        matcher,
                        action,
                        timeout_seconds,
                    )?;
                    return verify_updated(updated, settings_path, desired);
                }
                session_start.append(canonical_hook_input(
                    hook_path,
                    matcher,
                    action,
                    timeout_seconds,
                ));
            }
            None if direct_children_are_compact(&hooks.children()) => {
                let updated = append_session_property_compact(
                    &root.to_string(),
                    hook_path,
                    settings_path,
                    event,
                    matcher,
                    action,
                    timeout_seconds,
                )?;
                return verify_updated(updated, settings_path, desired);
            }
            None => {
                let session_start = hooks
                    .append(event, CstInputValue::Array(Vec::new()))
                    .array_value()
                    .ok_or_else(|| {
                        io::Error::other(format!("failed to create {event} hook array"))
                    })?;
                session_start.append(canonical_hook_input(
                    hook_path,
                    matcher,
                    action,
                    timeout_seconds,
                ));
            }
        }
    }

    verify_updated(root.to_string(), settings_path, desired)
}

fn remove_hook_path_commands(
    hooks: &CstObject,
    hook_path: &Path,
    event: &str,
    canonical: &Value,
) -> io::Result<bool> {
    let mut canonical_preserved = false;
    for event_property in hooks.properties() {
        let property_event = event_property.decoded_name().ok_or_else(|| {
            io::Error::other("Claude settings hooks contain an undecodable event name")
        })?;
        let Some(entries) = event_property.value().and_then(|value| value.as_array()) else {
            continue;
        };
        let mut removed_in_event = false;
        for entry in entries.elements() {
            if property_event == event
                && !canonical_preserved
                && entry.to_serde_value().as_ref() == Some(canonical)
            {
                canonical_preserved = true;
                continue;
            }
            let Some(command_entries) = entry
                .as_object()
                .and_then(|object| object.get("hooks"))
                .and_then(|property| property.array_value())
            else {
                if cst_value_uses_hook_path(&entry, hook_path) {
                    removed_in_event = true;
                    entry.remove();
                }
                continue;
            };
            let mut removed_in_group = false;
            for command_entry in command_entries.elements() {
                if cst_value_uses_hook_path(&command_entry, hook_path) {
                    removed_in_group = true;
                    removed_in_event = true;
                    command_entry.remove();
                }
            }

            if removed_in_group && command_entries.elements().is_empty() {
                entry.remove();
                continue;
            }
            if cst_value_uses_hook_path(&entry, hook_path) {
                removed_in_event = true;
                entry.remove();
            }
        }
        if removed_in_event && entries.elements().is_empty() {
            event_property.remove();
        }
    }

    Ok(canonical_preserved)
}

fn cst_value_uses_hook_path(value: &CstNode, hook_path: &Path) -> bool {
    value.to_serde_value().is_some_and(|value| {
        ["command", "bash"].iter().any(|field| {
            value
                .get(*field)
                .and_then(Value::as_str)
                .is_some_and(|command| is_hook_command_for_path(command, hook_path))
        })
    })
}

fn canonical_hook_value(
    hook_path: &Path,
    matcher: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> Value {
    serde_json_value!({
        "matcher": matcher,
        "hooks": [{
            "type": "command",
            "command": hook_command(hook_path, action),
            "timeout": timeout_seconds,
        }],
    })
}

fn canonical_hook_input(
    hook_path: &Path,
    matcher: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> CstInputValue {
    let command = hook_command(hook_path, action);
    json!({
        matcher: matcher,
        hooks: [{
            "type": "command",
            command: command,
            timeout: timeout_seconds,
        }],
    })
}

fn append_hooks_property_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
    event: &str,
    matcher: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let event = serde_json::to_string(event)?;
    let value = format!(
        "{{{event}:[{}]}}",
        canonical_hook_json(hook_path, matcher, action, timeout_seconds)?
    );
    append_object_property(content, &root, "hooks", &value)
}

fn append_session_property_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
    event: &str,
    matcher: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let hooks = root.get_object("hooks").ok_or_else(|| {
        io::Error::other(format!(
            "claude settings hooks at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    let value = format!(
        "[{}]",
        canonical_hook_json(hook_path, matcher, action, timeout_seconds)?
    );
    append_object_property(content, hooks, event, &value)
}

fn append_session_entry_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
    event: &str,
    matcher: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let event_entries = root
        .get_object("hooks")
        .and_then(|hooks| hooks.get_array(event))
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;
    Ok(append_array_element(
        content,
        event_entries,
        &canonical_hook_json(hook_path, matcher, action, timeout_seconds)?,
    ))
}

fn parse_ast_root_object<'a>(content: &'a str, settings_path: &Path) -> io::Result<AstObject<'a>> {
    let parsed = parse_to_ast(content, &CollectOptions::default(), &strict_parse_options())
        .map_err(|err| {
            io::Error::other(format!(
                "failed to parse {}: {err}",
                settings_path.display()
            ))
        })?;
    match parsed.value {
        Some(AstValue::Object(object)) => Ok(object),
        _ => Err(io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))),
    }
}

fn append_object_property(
    content: &str,
    object: &AstObject<'_>,
    name: &str,
    value: &str,
) -> io::Result<String> {
    let key = serde_json::to_string(name).map_err(|err| {
        io::Error::other(format!(
            "failed to encode Claude settings property name: {err}"
        ))
    })?;
    let key_value_separator = object.properties.first().map_or(":", |property| {
        &content[property.name.range().end..property.value.range().start]
    });
    let insertion = format!("{key}{key_value_separator}{value}");
    let delimiter = object_delimiter(content, object);
    Ok(append_to_container(
        content,
        object.range,
        !object.properties.is_empty(),
        delimiter,
        &insertion,
    ))
}

fn append_array_element(content: &str, array: &AstArray<'_>, value: &str) -> String {
    let delimiter = array_delimiter(content, array);
    append_to_container(
        content,
        array.range,
        !array.elements.is_empty(),
        delimiter,
        value,
    )
}

fn object_delimiter<'a>(content: &'a str, object: &AstObject<'_>) -> &'a str {
    match object.properties.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range.end..second.range.start]),
        [first] => &content[object.range.start + 1..first.range.start],
        [] => "",
    }
}

fn array_delimiter<'a>(content: &'a str, array: &AstArray<'_>) -> &'a str {
    match array.elements.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range().end..second.range().start]),
        [first] => &content[array.range.start + 1..first.range().start],
        [] => "",
    }
}

fn delimiter_suffix(delimiter: &str) -> &str {
    delimiter
        .split_once(',')
        .map_or(delimiter, |(_, suffix)| suffix)
}

fn append_to_container(
    content: &str,
    range: jsonc_parser::common::Range,
    has_elements: bool,
    delimiter: &str,
    value: &str,
) -> String {
    let closing = range.end - 1;
    let insertion_index = if has_elements {
        content[..closing].trim_end_matches([' ', '\t']).len()
    } else {
        closing
    };
    let separator = if has_elements { "," } else { "" };
    let mut updated =
        String::with_capacity(content.len() + delimiter.len() + value.len() + separator.len());
    updated.push_str(&content[..insertion_index]);
    if has_elements {
        updated.push_str(separator);
        updated.push_str(delimiter);
    }
    updated.push_str(value);
    updated.push_str(&content[insertion_index..]);
    updated
}

fn canonical_hook_json(
    hook_path: &Path,
    matcher: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> io::Result<String> {
    let matcher_json = serde_json::to_string(matcher)?;
    let command = serde_json::to_string(&hook_command(hook_path, action))?;
    Ok(format!(
        "{{\"matcher\":{matcher_json},\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":{timeout_seconds}}}]}}"
    ))
}

fn verify_updated(updated: String, settings_path: &Path, desired: &Value) -> io::Result<String> {
    let actual = parse_value(&updated, settings_path)?;
    if &actual != desired {
        return Err(io::Error::other(format!(
            "failed to safely update claude settings at {}",
            settings_path.display()
        )));
    }
    Ok(updated)
}

fn direct_children_are_compact(children: &[CstNode]) -> bool {
    !children.iter().any(CstNode::is_newline)
}

fn parse_value(content: &str, settings_path: &Path) -> io::Result<Value> {
    serde_json::from_str(content).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })
}

fn reject_duplicate_keys(node: &CstNode, settings_path: &Path) -> io::Result<()> {
    if let Some(object) = node.as_object() {
        let mut names = HashSet::new();
        for property in object.properties() {
            let name = property
                .name()
                .ok_or_else(|| io::Error::other("JSON object property is missing a name"))?
                .decoded_value()
                .map_err(|err| io::Error::other(format!("failed to decode JSON key: {err}")))?;
            if !names.insert(name.clone()) {
                return Err(io::Error::other(format!(
                    "claude settings at {} contains duplicate key {name:?}",
                    settings_path.display()
                )));
            }
            if let Some(value) = property.value() {
                reject_duplicate_keys(&value, settings_path)?;
            }
        }
    } else if let Some(array) = node.as_array() {
        for element in array.elements() {
            reject_duplicate_keys(&element, settings_path)?;
        }
    }
    Ok(())
}

fn strict_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: false,
        allow_loose_object_property_names: false,
        allow_trailing_commas: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_for_test(
        content: &str,
        settings_path: &Path,
        hook_path: &Path,
    ) -> io::Result<String> {
        let target = crate::agent::IntegrationTarget::Claude;
        super::install(
            content,
            settings_path,
            hook_path,
            super::super::registry::integration_hook_events(target),
            super::super::registry::integration_hook_timeout(target)?,
        )
    }

    fn paths() -> (&'static Path, &'static Path) {
        (
            Path::new("/home/test/.claude/settings.json"),
            Path::new("/home/test/.claude/hooks/shepr-agent-state.sh"),
        )
    }

    #[test]
    fn install_preserves_untouched_formatting_and_complete_trailing_suffix() {
        let (settings_path, hook_path) = paths();
        let input = concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"Notification\" : [{\"matcher\":\"keep\",\"hooks\":[]}]\r\n",
            "    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        );

        let updated = install_for_test(input, settings_path, hook_path).expect("test precondition");

        assert!(updated.starts_with(concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"Notification\" : [{\"matcher\":\"keep\",\"hooks\":[]}],\r\n",
        )));
        assert!(updated.ends_with(concat!(
            "\r\n    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        )));
        assert!(!updated.replace("\r\n", "").contains('\n'));
        assert!(updated.contains("\"SessionStart\""));
        assert_eq!(
            serde_json::from_str::<Value>(&updated).expect("test precondition")["zeta"]["number"],
            100.0
        );
    }

    #[test]
    fn install_keeps_compact_containers_compact() {
        let (settings_path, hook_path) = paths();
        let matcher = claude_session_start_matcher();
        let canonical = canonical_hook_json(
            hook_path,
            &matcher,
            Some("session"),
            super::super::HOOK_TIMEOUT.as_secs(),
        )
        .expect("test precondition");
        let cases = [
            (
                "{\"zeta\":{\"escaped\":\"\\u0061\",\"n\":1e+02},\"alpha\":1}\r\n",
                format!(
                    "{{\"zeta\":{{\"escaped\":\"\\u0061\",\"n\":1e+02}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}]}}}}\r\n"
                ),
            ),
            (
                "{\"hooks\":{\"Notification\":[{\"matcher\":\"keep\",\"hooks\":[]}]}, \"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"Notification\":[{{\"matcher\":\"keep\",\"hooks\":[]}}],\"SessionStart\":[{canonical}]}}, \"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"keep\",\"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"keep\",\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]}},{canonical}]}}}}"
                ),
            ),
            (
                "{\"zeta\":{\n  \"x\":1\n},\"alpha\":1}",
                format!(
                    "{{\"zeta\":{{\n  \"x\":1\n}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}]}}}}"
                ),
            ),
            (
                "{\"hooks\":{\"Notification\":[\n  {\"matcher\":\"keep\",\"hooks\":[]}\n]},\"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"Notification\":[\n  {{\"matcher\":\"keep\",\"hooks\":[]}}\n],\"SessionStart\":[{canonical}]}},\"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\n  \"matcher\":\"keep\",\n  \"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]\n}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\n  \"matcher\":\"keep\",\n  \"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]\n}},{canonical}]}}}}"
                ),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                install_for_test(input, settings_path, hook_path).expect("test precondition"),
                expected
            );
        }
    }

    #[test]
    fn install_scopes_claude_session_start_sources() {
        let (settings_path, hook_path) = paths();
        let installed =
            install_for_test("{}", settings_path, hook_path).expect("test precondition");
        let settings: Value = serde_json::from_str(&installed).expect("test precondition");
        let matcher = settings["hooks"]["SessionStart"][0]["matcher"]
            .as_str()
            .expect("test precondition");
        let expected_matcher = claude_session_start_matcher();
        assert_eq!(matcher, expected_matcher.as_str());
        let pattern = regex::Regex::new(matcher).expect("test precondition");
        for source in CLAUDE_SESSION_START_SOURCES {
            assert!(
                pattern.is_match(source.as_str()),
                "Claude source: {source:?}"
            );
        }
        for source in ["new", "load", "", "future-source", "startup-extra"] {
            assert!(!pattern.is_match(source), "non-Claude source: {source}");
        }
    }

    #[test]
    fn install_is_a_byte_exact_noop_for_a_canonical_hook() {
        let (settings_path, hook_path) = paths();
        let matcher = claude_session_start_matcher();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session")))
            .expect("test precondition");
        let input = format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"hooks\":[{{\"timeout\":10,\"command\":{command},\"type\":\"command\"}}],\"matcher\":\"{matcher}\"}}]}},\"escaped\":\"\\u0061\"}}  \r\n\r\n"
        );

        let updated =
            install_for_test(&input, settings_path, hook_path).expect("test precondition");

        assert_eq!(updated, input);
    }

    #[test]
    fn install_replaces_noncanonical_session_start_and_preserves_user_hook() {
        let (settings_path, hook_path) = paths();
        let matcher = claude_session_start_matcher();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session")))
            .expect("test precondition");
        let user_hook = r#"{ "type" : "command", "command" : "echo keep", "timeout" : 3 }"#;
        let input = format!(
            "{{\n  \"hooks\": {{\n    \"SessionStart\": [{{\"matcher\":\"*\",\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":10}},{user_hook}]}}]\n  }}\n}}\n\n"
        );
        let installed =
            install_for_test(&input, settings_path, hook_path).expect("test precondition");
        assert!(installed.contains(user_hook));
        assert!(installed.ends_with("}\n\n"));
        let settings: Value = serde_json::from_str(&installed).expect("test precondition");
        let groups = settings["hooks"]["SessionStart"]
            .as_array()
            .expect("test precondition");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["matcher"], "*");
        assert_eq!(
            groups[0]["hooks"]
                .as_array()
                .expect("test precondition")
                .len(),
            1
        );
        assert_eq!(groups[0]["hooks"][0]["command"], "echo keep");
        assert_eq!(
            groups[1],
            canonical_hook_value(
                hook_path,
                &matcher,
                Some("session"),
                super::super::HOOK_TIMEOUT.as_secs()
            )
        );
        assert_eq!(
            install_for_test(&installed, settings_path, hook_path).expect("test precondition"),
            installed
        );
    }

    #[test]
    fn install_rejects_duplicate_keys() {
        let (settings_path, hook_path) = paths();
        let error = install_for_test(
            r#"{"alpha": 1, "alpha": 2, "hooks": {}}"#,
            settings_path,
            hook_path,
        )
        .expect_err("test precondition")
        .to_string();

        assert!(error.contains("duplicate key \"alpha\""), "{error}");
    }

    #[test]
    fn install_keeps_structurally_invalid_content_unchanged() {
        let (settings_path, hook_path) = paths();
        for input in ["[]", r#"{"hooks": []}"#, r#"{"hooks":{"SessionStart":{}}}"#] {
            assert!(install_for_test(input, settings_path, hook_path).is_err());
        }
    }
}
