//! The one JSON config editor every JSON integration target goes through. It
//! edits the concrete syntax tree, touching only shepr's own entries and the
//! containers it inserts, so every other byte of the user's file (key order,
//! spacing, line endings, number and escape spellings) is kept, and refuses a
//! document with duplicate keys. Each edit is checked by decoding the result
//! against the value it was meant to produce.

use crate::types::{InstallError, InstallResult};
use std::collections::HashSet;
use std::io;
use std::path::Path;

use jsonc_parser::ast::{Array as AstArray, Object as AstObject, Value as AstValue};
use jsonc_parser::common::Ranged;
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{CollectOptions, ParseOptions, parse_to_ast};
use serde_json::{Map, Value};

use super::command::{hook_command_prefix, legacy_hook_command};
use super::registration::{HooksRoot, RequiredJsonField};
use shepr_agent::IntegrationTarget as Target;

/// Replace shepr's hook entries in a JSON agent config. An entry that contains
/// the expected fields is kept where it is, including harmless additional
/// fields; every other entry naming this managed hook file and one of its
/// descriptor commands is removed, including entries left by another host's
/// path spelling.
/// Required top-level fields are added from the target's registration row when
/// absent. Parsing and duplicate validation precede even a no-op, so an
/// ambiguous user document is never accepted.
pub(super) fn install_json(
    target: Target,
    content: &str,
    path: &Path,
    hook_path: &Path,
    location: HooksRoot,
    mut expected: Map<String, Value>,
    required_fields: &[RequiredJsonField],
    document_description: &str,
) -> InstallResult<String> {
    let mut root = parse_root(content, path)?;
    let mut object = root
        .value()
        .and_then(|value| value.as_object())
        .ok_or_else(|| {
            shape_error(&format!(
                "{document_description} at {} must be a JSON object",
                path.display()
            ))
        })?;
    let mut desired = parse_value(content, path)?;
    for field in required_fields {
        if object.get(field.key).is_none() {
            let value = field.value();
            desired[field.key] = value.clone();
            let updated = append_property(&root, &object, path, false, field.key, &value)?;
            root = parse_root(&updated, path)?;
            object = root
                .value()
                .and_then(|value| value.as_object())
                .ok_or_else(|| shape_error("missing root"))?;
        }
    }
    let hooks = match location {
        HooksRoot::Document => object,
        HooksRoot::HooksKey => match object.get("hooks") {
            Some(property) => property.object_value().ok_or_else(|| {
                shape_error(&format!(
                    "agent config hooks at {} must be a JSON object",
                    path.display()
                ))
            })?,
            None => {
                let updated = append_property(
                    &root,
                    &object,
                    path,
                    false,
                    "hooks",
                    &Value::Object(expected.clone()),
                )?;
                desired["hooks"] = Value::Object(expected);
                return verify_updated(updated, path, &desired);
            }
        },
    };
    remove_managed_hook_commands(&hooks, target, hook_path, &mut expected)?;
    // The removal has one implementation. Its decoded result is the baseline
    // for verifying subsequent insertions, rather than a second removal model.
    desired = parse_value(&root.to_string(), path)?;
    let desired_hooks = match location {
        HooksRoot::Document => desired.as_object_mut(),
        HooksRoot::HooksKey => desired.get_mut("hooks").and_then(Value::as_object_mut),
    }
    .ok_or_else(|| {
        shape_error(&format!(
            "agent config hooks at {} must be a JSON object",
            path.display()
        ))
    })?;
    let mut updated = root.to_string();
    for (event, value) in expected {
        let additions = value
            .as_array()
            .ok_or_else(|| shape_error("expected hooks must be arrays"))?;
        for addition in additions {
            let root = parse_root(&updated, path)?;
            let object = root
                .value()
                .and_then(|value| value.as_object())
                .ok_or_else(|| shape_error("missing root"))?;
            let hooks = match location {
                HooksRoot::Document => object,
                HooksRoot::HooksKey => object
                    .get("hooks")
                    .and_then(|p| p.object_value())
                    .ok_or_else(|| shape_error("missing hooks"))?,
            };
            match hooks.get(&event) {
                Some(property) => {
                    let array = property.array_value().ok_or_else(|| {
                        shape_error(&format!("hook entries for {event} must be an array"))
                    })?;
                    if direct_children_are_compact(&array.children()) {
                        let ast = parse_ast_root_object(&updated, path)?;
                        let ast_hooks = match location {
                            HooksRoot::Document => &ast,
                            HooksRoot::HooksKey => ast
                                .get_object("hooks")
                                .ok_or_else(|| shape_error("missing hooks"))?,
                        };
                        let ast_array = ast_hooks
                            .get_array(&event)
                            .ok_or_else(|| shape_error("missing event array"))?;
                        updated = append_array_element(
                            &updated,
                            ast_array,
                            &serde_json::to_string(addition).map_err(|err| {
                                shape_error(&format!("failed to serialize hook entry: {err}"))
                            })?,
                        );
                    } else {
                        array.append(input_value(addition));
                        updated = root.to_string();
                    }
                }
                None => {
                    updated = append_property(
                        &root,
                        &hooks,
                        path,
                        matches!(location, HooksRoot::HooksKey),
                        &event,
                        &Value::Array(vec![addition.clone()]),
                    )?;
                }
            }
            desired_hooks
                .entry(event.clone())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| shape_error("expected event array"))?
                .push(addition.clone());
        }
    }
    verify_updated(updated, path, &desired)
}

fn shape_error(message: &str) -> super::types::InstallError {
    InstallError::config_shape(message)
}

fn parse_root(content: &str, path: &Path) -> InstallResult<CstRootNode> {
    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        InstallError::config_unparseable(format!("failed to parse {}: {err}", path.display()))
    })?;
    if let Some(value) = root.value() {
        reject_duplicate_keys(&value, path)?;
    }
    Ok(root)
}

fn input_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(v) => CstInputValue::Bool(*v),
        Value::Number(v) => CstInputValue::Number(v.to_string()),
        Value::String(v) => CstInputValue::String(v.clone()),
        Value::Array(v) => CstInputValue::Array(v.iter().map(input_value).collect()),
        Value::Object(v) => {
            CstInputValue::Object(v.iter().map(|(k, v)| (k.clone(), input_value(v))).collect())
        }
    }
}

fn append_property(
    root: &CstRootNode,
    object: &CstObject,
    path: &Path,
    under_hooks: bool,
    name: &str,
    value: &Value,
) -> InstallResult<String> {
    if direct_children_are_compact(&object.children()) {
        let text = root.to_string();
        let ast = parse_ast_root_object(&text, path)?;
        let container = if under_hooks {
            ast.get_object("hooks")
                .ok_or_else(|| shape_error("missing hooks"))?
        } else {
            &ast
        };
        let value_text = serde_json::to_string(value)
            .map_err(|err| shape_error(&format!("failed to serialize {name}: {err}")))?;
        append_object_property(&text, container, name, &value_text)
    } else {
        object.append(name, input_value(value));
        Ok(root.to_string())
    }
}

/// Replace a dedicated owned property while retaining every other property's bytes.
pub(super) fn install_block(
    content: &str,
    path: &Path,
    name: &str,
    block: &Value,
) -> InstallResult<String> {
    let root = parse_root(content, path)?;
    let object = root
        .value()
        .and_then(|value| value.as_object())
        .ok_or_else(|| {
            shape_error(&format!(
                "agent config at {} must be a JSON object",
                path.display()
            ))
        })?;
    let mut desired = parse_value(content, path)?;
    if desired.get(name) == Some(block) {
        return Ok(content.to_string());
    }
    desired[name] = block.clone();
    let updated = if let Some(property) = object.get(name) {
        property.set_value(input_value(block));
        root.to_string()
    } else {
        append_property(&root, &object, path, false, name, block)?
    };
    verify_updated(updated, path, &desired)
}

fn remove_managed_hook_commands(
    hooks: &CstObject,
    target: Target,
    hook_path: &Path,
    expected: &mut Map<String, Value>,
) -> InstallResult<()> {
    let expected_commands = expected
        .values()
        .flat_map(expected_hook_commands)
        .collect::<Vec<_>>();
    for event_property in hooks.properties() {
        let property_event = event_property.decoded_name().ok_or_else(|| {
            InstallError::config_shape("agent config hooks contain an undecodable event name")
        })?;
        let Some(entries) = event_property.value().and_then(|value| value.as_array()) else {
            return Err(shape_error(&format!(
                "hook entries for {property_event} must be an array"
            )));
        };
        let mut removed_in_event = false;
        for entry in entries.elements() {
            if let Some(canonicals) = expected
                .get_mut(&property_event)
                .and_then(Value::as_array_mut)
                && let Some(index) = canonicals.iter().position(|canonical| {
                    entry.to_serde_value().as_ref().is_some_and(|actual| {
                        canonical_registration_entry_matches(actual, canonical)
                    })
                })
            {
                canonicals.remove(index);
                continue;
            }
            let Some(command_entries) = entry
                .as_object()
                .and_then(|object| object.get("hooks"))
                .and_then(|property| property.array_value())
            else {
                if cst_value_uses_managed_hook_command(
                    &entry,
                    target,
                    hook_path,
                    &expected_commands,
                ) {
                    removed_in_event = true;
                    entry.remove();
                }
                continue;
            };
            let mut removed_in_group = false;
            for command_entry in command_entries.elements() {
                if cst_value_uses_managed_hook_command(
                    &command_entry,
                    target,
                    hook_path,
                    &expected_commands,
                ) {
                    removed_in_group = true;
                    removed_in_event = true;
                    command_entry.remove();
                }
            }

            if removed_in_group && command_entries.elements().is_empty() {
                entry.remove();
                continue;
            }
            if cst_value_uses_managed_hook_command(&entry, target, hook_path, &expected_commands) {
                removed_in_event = true;
                entry.remove();
            }
        }
        if removed_in_event && entries.elements().is_empty() {
            event_property.remove();
        }
    }

    Ok(())
}

/// Recursive subset comparison for registration fields and array entries.
/// The registration-level wrapper also checks matcher absence and is shared by
/// install and status.
pub(super) fn canonical_entry_matches(actual: &Value, expected: &Value) -> bool {
    match expected {
        Value::Object(fields) => actual.as_object().is_some_and(|object| {
            fields.iter().all(|(key, value)| {
                object
                    .get(key)
                    .is_some_and(|actual| canonical_entry_matches(actual, value))
            })
        }),
        Value::Array(entries) => actual.as_array().is_some_and(|actual| {
            entries.iter().all(|entry| {
                actual
                    .iter()
                    .any(|value| canonical_entry_matches(value, entry))
            })
        }),
        _ => actual == expected,
    }
}

/// Registration-level matching also requires an absent matcher to stay
/// absent. Keep this shared by install and status so a Current entry remains
/// a byte-for-byte no-op when installed. Fields the user added to a shepr
/// entry are ignored on purpose, one that switches the hook off (such as
/// `"disabled": true`) included: nobody disables shepr's own hook, so such an
/// entry is left as written and reads as Current rather than being flagged.
pub(super) fn canonical_registration_entry_matches(actual: &Value, expected: &Value) -> bool {
    (expected.get("matcher").is_some() || actual.get("matcher").is_none())
        && canonical_entry_matches(actual, expected)
}

fn cst_value_uses_managed_hook_command(
    value: &CstNode,
    target: Target,
    hook_path: &Path,
    expected_commands: &[String],
) -> bool {
    value.to_serde_value().is_some_and(|value| {
        super::config_edit::HOOK_COMMAND_FIELDS.iter().any(|field| {
            value
                .get(*field)
                .and_then(Value::as_str)
                .is_some_and(|command| {
                    is_managed_hook_command(command, target, hook_path, expected_commands)
                })
        })
    })
}

pub(super) fn is_managed_hook_command(
    command: &str,
    target: Target,
    hook_path: &Path,
    expected_commands: &[String],
) -> bool {
    if expected_commands.iter().any(|expected| expected == command) {
        return true;
    }
    // Repair absolute registrations left by an earlier installer on this or
    // another host. Match only the managed filename and a descriptor action;
    // a user hook that runs a differently named script or the same script
    // with other arguments remains untouched.
    let Some(file_name) = hook_path.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    let current_prefix = hook_command_prefix(hook_path);
    target.hook_events().iter().any(|event| {
        let expected = legacy_hook_command(
            hook_path,
            event.action.map(shepr_agent::IntegrationHookAction::as_str),
        );
        let Some(arguments) = expected.strip_prefix(&current_prefix) else {
            return false;
        };
        let suffix = format!("{file_name}'{arguments}");
        let Some(path_prefix) = command.strip_suffix(&suffix) else {
            return false;
        };
        command.starts_with("sh '") && (path_prefix == "sh '" || path_prefix.ends_with('/'))
    })
}

pub(super) fn expected_hook_commands(value: &Value) -> Vec<String> {
    fn collect(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    collect(value, output);
                }
            }
            Value::Object(object) => {
                for field in super::config_edit::HOOK_COMMAND_FIELDS {
                    if let Some(command) = object.get(*field).and_then(Value::as_str) {
                        output.push(command.to_string());
                    }
                }
                for value in object.values() {
                    collect(value, output);
                }
            }
            _ => {}
        }
    }

    let mut commands = Vec::new();
    collect(value, &mut commands);
    commands
}

fn parse_ast_root_object<'a>(
    content: &'a str,
    settings_path: &Path,
) -> InstallResult<AstObject<'a>> {
    let parsed = parse_to_ast(content, &CollectOptions::default(), &strict_parse_options())
        .map_err(|err| {
            InstallError::config_unparseable(format!(
                "failed to parse {}: {err}",
                settings_path.display()
            ))
        })?;
    match parsed.value {
        Some(AstValue::Object(object)) => Ok(object),
        _ => Err(InstallError::config_shape(format!(
            "agent config at {} must be a JSON object",
            settings_path.display()
        ))),
    }
}

fn append_object_property(
    content: &str,
    object: &AstObject<'_>,
    name: &str,
    value: &str,
) -> InstallResult<String> {
    let key = serde_json::to_string(name).map_err(|err| {
        InstallError::from(io::Error::other(format!(
            "failed to encode agent config property name: {err}"
        )))
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

fn verify_updated(updated: String, settings_path: &Path, desired: &Value) -> InstallResult<String> {
    let actual = parse_value(&updated, settings_path)?;
    if &actual != desired {
        return Err(InstallError::config_shape(format!(
            "failed to safely update agent config at {}",
            settings_path.display()
        )));
    }
    Ok(updated)
}

fn direct_children_are_compact(children: &[CstNode]) -> bool {
    !children.iter().any(CstNode::is_newline)
}

fn parse_value(content: &str, settings_path: &Path) -> InstallResult<Value> {
    serde_json::from_str(content).map_err(|err| {
        InstallError::config_unparseable(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })
}

fn reject_duplicate_keys(node: &CstNode, settings_path: &Path) -> InstallResult<()> {
    if let Some(object) = node.as_object() {
        let mut names = HashSet::new();
        for property in object.properties() {
            let name = property
                .name()
                .ok_or_else(|| {
                    InstallError::config_unparseable("JSON object property is missing a name")
                })?
                .decoded_value()
                .map_err(|err| {
                    InstallError::config_unparseable(format!("failed to decode JSON key: {err}"))
                })?;
            if !names.insert(name.clone()) {
                return Err(InstallError::config_shape(format!(
                    "agent config at {} contains duplicate key {name:?}",
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
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

/// Claude's settings edit as the installer makes it, for tests that drive it
/// without an installation.
#[cfg(test)]
pub(crate) fn install_claude_settings(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
    timeout: std::time::Duration,
) -> InstallResult<String> {
    install_json(
        shepr_agent::IntegrationTarget::Claude,
        content,
        settings_path,
        hook_path,
        HooksRoot::HooksKey,
        super::registration::JsonShape::Nested(timeout).expected_events(
            shepr_agent::IntegrationTarget::Claude,
            super::registration::HookEventPolicy::CLAUDE,
        )?,
        &[],
        "Claude settings",
    )
}

#[cfg(test)]
fn canonical_hook_value(matcher: &str, action: Option<&str>, timeout_seconds: u64) -> Value {
    super::config_edit::command_hook_group(
        &super::command::hook_command(shepr_agent::IntegrationTarget::Claude, action),
        timeout_seconds,
        Some(matcher),
    )
}

#[cfg(test)]
fn canonical_hook_json(
    matcher: &str,
    action: Option<&str>,
    timeout_seconds: u64,
) -> InstallResult<String> {
    serde_json::to_string(&canonical_hook_value(matcher, action, timeout_seconds))
        .map_err(|error| InstallError::from(io::Error::other(error)))
}

#[cfg(test)]
mod tests {
    use super::super::command::hook_command;
    use super::super::registration::JsonShape;
    use super::super::registration::{claude_session_start_matcher, claude_session_start_sources};
    use super::*;
    use shepr_agent::Agent;
    use shepr_agent::resume::{AgentSessionStartSource, ReportedSessionStart};

    fn install_for_test(
        content: &str,
        settings_path: &Path,
        hook_path: &Path,
    ) -> InstallResult<String> {
        let target = shepr_agent::IntegrationTarget::Claude;
        super::install_claude_settings(
            content,
            settings_path,
            hook_path,
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
    fn every_shared_json_target_preserves_user_bytes_and_is_idempotent() {
        use super::super::registration::Registration;
        use super::super::registry::registration;
        use shepr_agent::IntegrationTarget as Target;
        let path = Path::new("/settings.json");
        let hook = Path::new("/hooks/shepr-agent-state.sh");
        let user = r#"{ "escaped":"\u0061\/", "number":1e+02 }"#;
        for target in [
            Target::Claude,
            Target::Codex,
            Target::Copilot,
            Target::Devin,
            Target::Droid,
            Target::Cursor,
            Target::Mastracode,
            Target::AntigravityCli,
        ] {
            let (location, events, required_fields, document_description) =
                match registration(target) {
                    Registration::Json {
                        root,
                        shape,
                        required_fields,
                        document_description,
                        event_policy,
                        ..
                    } => (
                        root,
                        Some(shape.expected_events(target, event_policy).expect("events")),
                        required_fields,
                        document_description,
                    ),
                    Registration::Codex {
                        timeout,
                        event_policy,
                        required_fields,
                        document_description,
                        ..
                    } => (
                        HooksRoot::HooksKey,
                        Some(
                            JsonShape::Nested(timeout)
                                .expected_events(target, event_policy)
                                .expect("events"),
                        ),
                        required_fields,
                        document_description,
                    ),
                    Registration::AntigravityCli { .. } => (
                        HooksRoot::Document,
                        None,
                        <&[RequiredJsonField]>::default(),
                        "Antigravity hooks file",
                    ),
                    _ => panic!("JSON target"),
                };
            let unrelated = match location {
                HooksRoot::HooksKey => {
                    format!("\"zeta\" : {user},\r\n    \"hooks\" : {{\"Unrelated\":[{user}]}}")
                }
                HooksRoot::Document => format!("\"Unrelated\" : [{user}]"),
            };
            for input in [
                format!("{{\r\n    {unrelated}\r\n}}  \r\n\r\n"),
                format!("{{{unrelated}}}  \r\n\r\n"),
            ] {
                let edit = |text: &str| match &events {
                    Some(events) => install_json(
                        target,
                        text,
                        path,
                        hook,
                        location,
                        events.clone(),
                        required_fields,
                        document_description,
                    ),
                    None => install_block(
                        text,
                        path,
                        super::super::ANTIGRAVITY_CLI_HOOK_BLOCK_NAME,
                        &super::super::targets::antigravity_cli_hook_block_with_timeout(
                            super::super::HOOK_TIMEOUT,
                        )
                        .expect("block"),
                    ),
                };
                let updated = edit(&input).expect("edit JSON");
                if target == Target::Cursor {
                    let value: Value = serde_json::from_str(&updated).expect("Cursor JSON");
                    assert_eq!(value.get("version"), Some(&Value::from(1)));
                }
                // Every edit lands after the user's last entry, so the whole
                // text up to it is kept byte for byte.
                let kept = &input[..=input.rfind(']').expect("test precondition")];
                assert!(updated.starts_with(kept), "{target:?}: {updated}");
                assert_ne!(updated, input, "{target:?}: nothing was installed");
                assert!(updated.ends_with("}  \r\n\r\n"), "{target:?}: {updated}");
                assert!(!updated.replace("\r\n", "").contains('\n'), "{target:?}");
                assert_eq!(edit(&updated).expect("repeat edit"), updated, "{target:?}");
                assert!(
                    edit(r#"{"x":{"duplicate":1,"duplicate":2}}"#).is_err(),
                    "{target:?}"
                );
            }
        }
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
        for source in claude_session_start_sources() {
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
    fn claude_startup_reports_without_replacing_and_every_other_source_replaces() {
        let policy = Agent::Claude.descriptor().hook_session_policy();
        for source in claude_session_start_sources() {
            assert_eq!(
                policy.allows_replacement(ReportedSessionStart::Known(source)),
                source != AgentSessionStartSource::Startup,
                "Claude source: {source:?}"
            );
        }
        assert!(
            policy.allows_replacement(ReportedSessionStart::Known(AgentSessionStartSource::Fork))
        );
    }

    #[test]
    fn install_is_a_byte_exact_noop_for_a_canonical_hook() {
        let (settings_path, hook_path) = paths();
        let matcher = claude_session_start_matcher();
        let command = serde_json::to_string(&hook_command(Target::Claude, Some("session")))
            .expect("test precondition");
        let timeout = super::super::HOOK_TIMEOUT.as_secs();
        let input = format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"hooks\":[{{\"timeout\":{timeout},\"command\":{command},\"type\":\"command\"}}],\"matcher\":\"{matcher}\"}}]}},\"escaped\":\"\\u0061\"}}  \r\n\r\n"
        );

        let updated =
            install_for_test(&input, settings_path, hook_path).expect("test precondition");

        assert_eq!(updated, input);
    }

    #[test]
    fn install_is_a_byte_exact_noop_when_a_canonical_hook_has_extra_fields() {
        let (settings_path, hook_path) = paths();
        let matcher = claude_session_start_matcher();
        let command = serde_json::to_string(&hook_command(Target::Claude, Some("session")))
            .expect("test precondition");
        let timeout = super::super::HOOK_TIMEOUT.as_secs();
        let input = format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"{matcher}\",\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":{timeout},\"disabled\":true}}]}}]}}}}"
        );

        let updated =
            install_for_test(&input, settings_path, hook_path).expect("test precondition");

        assert_eq!(updated, input);
    }

    #[test]
    fn install_replaces_noncanonical_session_start_and_preserves_user_hook() {
        let (settings_path, hook_path) = paths();
        let matcher = claude_session_start_matcher();
        let command = serde_json::to_string(&hook_command(Target::Claude, Some("session")))
            .expect("test precondition");
        let user_hook = r#"{ "type" : "command", "command" : "echo keep", "timeout" : 3 }"#;
        let timeout = super::super::HOOK_TIMEOUT.as_secs();
        let input = format!(
            "{{\n  \"hooks\": {{\n    \"SessionStart\": [{{\"matcher\":\"*\",\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":{timeout}}},{user_hook}]}}]\n  }}\n}}\n\n"
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
