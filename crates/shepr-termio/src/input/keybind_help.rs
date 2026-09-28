use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::input::TerminalKey;
use shepr_config::{ActionKeybinds, IndexedKeybind, Keybinds};

pub(crate) type KeybindHelpEntry = (String, Cow<'static, str>);
pub(crate) type KeybindHelpGroup = (&'static str, Vec<KeybindHelpEntry>);

pub fn keybind_help_text_char(key: &TerminalKey) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    if let Some(character) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(character);
    }
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    Some(character)
}

fn entry(key: impl Into<String>, label: &'static str) -> KeybindHelpEntry {
    (key.into(), Cow::Borrowed(label))
}

fn binding_label(bindings: &ActionKeybinds) -> String {
    bindings.label().unwrap_or_else(|| "unset".to_owned())
}

fn indexed_label(bindings: &[IndexedKeybind]) -> String {
    if bindings.is_empty() {
        return "unset".to_owned();
    }
    let mut parts = Vec::new();
    let mut index = 0;
    while index < bindings.len() {
        if let Some(prefix) = indexed_range_prefix(&bindings[index..]) {
            parts.push(format!("{prefix}1..9"));
            index += 9;
        } else {
            parts.push(bindings[index].label.clone());
            index += 1;
        }
    }
    parts.join(" / ")
}

fn indexed_range_prefix(bindings: &[IndexedKeybind]) -> Option<&str> {
    let run = bindings.get(..9)?;
    let prefix = run[0].label.strip_suffix('1')?;
    for (offset, binding) in run.iter().enumerate() {
        // `run` has exactly 9 elements, so offset is always < 9 and fits in a u8.
        let digit = char::from(b'1' + u8::try_from(offset).unwrap_or(u8::MAX));
        if binding.label.strip_suffix(digit) != Some(prefix) {
            return None;
        }
    }
    Some(prefix)
}

pub fn keybind_help_groups(
    keybinds: &Keybinds,
    prefix: (crossterm::event::KeyCode, crossterm::event::KeyModifiers),
) -> Vec<KeybindHelpGroup> {
    let mut groups = vec![
        (
            "global",
            vec![entry(shepr_config::format_key_combo(prefix), "prefix mode")],
        ),
        ("navigation", Vec::new()),
        ("workspaces / tabs", Vec::new()),
        ("panes", Vec::new()),
    ];

    // The fixed arrow aliases of navigate rows, listed after the configured
    // keys of the help row they belong to.
    let mut navigate_aliases: Vec<(&'static str, &'static str, &'static str)> = Vec::new();
    macro_rules! navigate_alias {
        (None, $group:expr, $label:expr) => {};
        (Left, $group:expr, $label:expr) => {
            navigate_aliases.push(($group, $label, "left"))
        };
        (Right, $group:expr, $label:expr) => {
            navigate_aliases.push(($group, $label, "right"))
        };
    }

    macro_rules! build_keybind_help {
        (
            actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
            indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
            navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
            navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
        ) => {
            $(group_entries(&mut groups, $action_group)
                .push(entry(binding_label(&keybinds.$action_field), $action_label));)*
            $(insert_help_entry_after(
                group_entries(&mut groups, $indexed_group),
                $indexed_help_after,
                entry(indexed_label(&keybinds.$indexed_field), $indexed_label),
            );)*
            $(
                merge_help_entry(
                    group_entries(&mut groups, $navigate_group),
                    binding_label(&keybinds.navigate.$navigate_field),
                    $navigate_label,
                );
                navigate_alias!($navigate_alias, $navigate_group, $navigate_label);
            )*
            $(
                merge_help_entry(
                    group_entries(&mut groups, $navigate_indexed_group),
                    indexed_label(&keybinds.navigate.$navigate_indexed_field),
                    $navigate_indexed_label,
                );
                navigate_alias!(
                    $navigate_indexed_alias,
                    $navigate_indexed_group,
                    $navigate_indexed_label
                );
            )*
        };
    }

    shepr_config::keybinding_table!(build_keybind_help);
    for (group, label, alias) in navigate_aliases {
        if let Some(existing) = group_entries(&mut groups, group)
            .iter_mut()
            .find(|existing| existing.1 == label)
        {
            existing.0.push_str(" / ");
            existing.0.push_str(alias);
        }
    }
    groups
}

fn group_entries<'a>(
    groups: &'a mut Vec<KeybindHelpGroup>,
    group: &'static str,
) -> &'a mut Vec<KeybindHelpEntry> {
    let index = match groups.iter().position(|(name, _)| *name == group) {
        Some(index) => index,
        None => {
            groups.push((group, Vec::new()));
            groups.len() - 1
        }
    };
    &mut groups[index].1
}

/// Places an indexed entry right after the entry labelled `after`, or last
/// when no such entry exists.
fn insert_help_entry_after(
    entries: &mut Vec<KeybindHelpEntry>,
    after: &str,
    help_entry: KeybindHelpEntry,
) {
    match entries.iter().position(|existing| existing.1 == after) {
        Some(index) => entries.insert(index + 1, help_entry),
        None => entries.push(help_entry),
    }
}

/// Navigate rows that share a help label share one row, keys joined in
/// table order.
fn merge_help_entry(entries: &mut Vec<KeybindHelpEntry>, keys: String, label: &'static str) {
    match entries.iter_mut().find(|existing| existing.1 == label) {
        Some(existing) => {
            existing.0.push_str(" / ");
            existing.0.push_str(&keys);
        }
        None => entries.push(entry(keys, label)),
    }
}

pub fn filter_keybind_help_groups(
    groups: Vec<KeybindHelpGroup>,
    query: &str,
) -> Vec<KeybindHelpGroup> {
    if query.is_empty() {
        return groups;
    }
    let query = query.to_lowercase();
    groups
        .into_iter()
        .filter_map(|(group, entries)| {
            let entries = entries
                .into_iter()
                .filter(|(key, label)| {
                    key.to_lowercase().contains(&query) || label.to_lowercase().contains(&query)
                })
                .collect::<Vec<_>>();
            (!entries.is_empty()).then_some((group, entries))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups() -> Vec<KeybindHelpGroup> {
        vec![
            (
                "workspaces / tabs",
                vec![entry("w", "workspace navigation"), entry("c", "new tab")],
            ),
            (
                "panes",
                vec![entry("v", "split vertical"), entry("x", "close pane")],
            ),
        ]
    }

    #[test]
    fn filter_matches_labels_and_shortcuts_case_insensitively() {
        let filtered = filter_keybind_help_groups(groups(), "WoRk");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].1, "workspace navigation");

        let filtered = filter_keybind_help_groups(groups(), "x");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].1, "close pane");
        assert!(filter_keybind_help_groups(groups(), "panes").is_empty());
    }

    /// Pins the whole default help screen, row for row, so a change to the
    /// keybinding table cannot reorder, relabel or drop a row unnoticed.
    #[test]
    fn default_help_screen_rows_are_pinned() {
        let live = crate::test_config::validated("").live_keybinds();
        let groups = keybind_help_groups(&live.keybinds, live.prefix);
        let actual: Vec<(&str, Vec<(&str, &str)>)> = groups
            .iter()
            .map(|(group, entries)| {
                (
                    *group,
                    entries
                        .iter()
                        .map(|(key, label)| (key.as_str(), label.as_ref()))
                        .collect(),
                )
            })
            .collect();
        let expected: Vec<(&str, Vec<(&str, &str)>)> = vec![
            (
                "global",
                vec![
                    ("ctrl+b", "prefix mode"),
                    ("prefix+?", "keybinds"),
                    ("prefix+q", "detach"),
                ],
            ),
            (
                "navigation",
                vec![
                    ("esc", "back"),
                    ("up / down", "workspace list"),
                    ("h / j / k / l / left / right", "move focus"),
                    ("tab / shift+tab", "cycle pane"),
                    ("enter", "open workspace"),
                    ("1..9", "switch workspace"),
                ],
            ),
            (
                "workspaces / tabs",
                vec![
                    ("prefix+w", "workspace navigation"),
                    ("prefix+g", "session navigator"),
                    ("prefix+shift+n", "new workspace"),
                    ("prefix+shift+w", "rename workspace"),
                    ("prefix+shift+d", "close workspace"),
                    ("unset", "previous workspace"),
                    ("unset", "next workspace"),
                    ("unset", "switch workspace 1-9"),
                    ("unset", "previous agent"),
                    ("unset", "next agent"),
                    ("unset", "focus agent 1-9"),
                    ("prefix+c", "new tab"),
                    ("prefix+shift+t", "rename tab"),
                    ("prefix+p", "previous tab"),
                    ("prefix+n", "next tab"),
                    ("unset", "move tab left"),
                    ("unset", "move tab right"),
                    ("prefix+1..9", "switch tab 1-9"),
                    ("prefix+shift+x", "close tab"),
                ],
            ),
            (
                "panes",
                vec![
                    ("prefix+v", "split vertical"),
                    ("prefix+-", "split horizontal"),
                    ("prefix+x", "close pane"),
                    ("prefix+shift+p", "rename pane"),
                    ("unset", "clear pane"),
                    ("prefix+[", "copy mode"),
                    ("prefix+z", "zoom pane"),
                    ("prefix+r", "resize mode"),
                    ("unset", "resize pane left"),
                    ("unset", "resize pane down"),
                    ("unset", "resize pane up"),
                    ("unset", "resize pane right"),
                    ("prefix+b", "toggle sidebar"),
                    ("prefix+h", "focus pane left"),
                    ("prefix+j", "focus pane down"),
                    ("prefix+k", "focus pane up"),
                    ("prefix+l", "focus pane right"),
                    ("prefix+shift+h", "swap pane left"),
                    ("prefix+shift+j", "swap pane down"),
                    ("prefix+shift+k", "swap pane up"),
                    ("prefix+shift+l", "swap pane right"),
                    ("prefix+tab", "cycle pane next"),
                    ("prefix+shift+tab", "cycle pane previous"),
                    ("unset", "last pane"),
                ],
            ),
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn help_shows_configured_navigate_keys() {
        let live = crate::test_config::validated(
            "[keys]\nnavigate_back = \"q\"\nnavigate_cycle_pane_previous = \"\"\nnavigate_switch_workspace = \"alt+1..9\"\n",
        )
        .live_keybinds();
        let groups = keybind_help_groups(&live.keybinds, live.prefix);
        let navigation = &groups
            .iter()
            .find(|(group, _)| *group == "navigation")
            .expect("navigation group")
            .1;
        let rows: Vec<(&str, &str)> = navigation
            .iter()
            .map(|(key, label)| (key.as_str(), label.as_ref()))
            .collect();
        assert!(rows.contains(&("q", "back")), "{rows:?}");
        assert!(rows.contains(&("tab / unset", "cycle pane")), "{rows:?}");
        assert!(rows.contains(&("alt+1..9", "switch workspace")), "{rows:?}");
    }

    #[test]
    fn help_lists_every_default_pane_binding() {
        let live = crate::test_config::validated("").live_keybinds();
        let groups = keybind_help_groups(&live.keybinds, live.prefix);
        let entries: Vec<_> = groups.iter().flat_map(|(_, entries)| entries).collect();
        for (key, label) in [
            ("prefix+[", "copy mode"),
            ("prefix+shift+h", "swap pane left"),
            ("prefix+shift+j", "swap pane down"),
            ("prefix+shift+k", "swap pane up"),
            ("prefix+shift+l", "swap pane right"),
        ] {
            assert!(
                entries
                    .iter()
                    .any(|(entry_key, entry_label)| entry_key == key && entry_label == label),
                "{label} ({key}) missing from the help screen"
            );
        }
    }
}
