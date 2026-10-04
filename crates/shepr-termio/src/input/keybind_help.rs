use shepr_config::{ActionKeybinds, HelpGroup, IndexedKeybind, Keybinds};
use shepr_term::key::TerminalKey;

/// One line of the keybinding help screen: the keys that trigger it and what
/// they do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpRow {
    pub keys: String,
    pub label: &'static str,
}

pub type KeybindHelpGroup = (HelpGroup, Vec<HelpRow>);

pub fn keybind_help_text_char(key: &TerminalKey) -> Option<char> {
    crate::copy_mode::copy_mode_key_char(key)
}

fn row(keys: impl Into<String>, label: &'static str) -> HelpRow {
    HelpRow {
        keys: keys.into(),
        label,
    }
}

fn binding_label(bindings: &ActionKeybinds) -> String {
    bindings.label().unwrap_or_else(|| "unset".to_owned())
}

fn indexed_label(bindings: &[IndexedKeybind]) -> String {
    if bindings.is_empty() {
        return "unset".to_owned();
    }
    bindings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" / ")
}

pub fn keybind_help_groups(
    keybinds: &Keybinds,
    prefix: shepr_term::key::KeyChord,
) -> Vec<KeybindHelpGroup> {
    let mut groups: Vec<KeybindHelpGroup> = HelpGroup::DISPLAY_ORDER
        .into_iter()
        .map(|group| (group, Vec::new()))
        .collect();
    group_rows(&mut groups, HelpGroup::Global)
        .push(row(shepr_config::format_key_chord(prefix), "prefix mode"));

    // Navigate aliases follow the configured keys of the help row they belong
    // to: (group, row index in the group, alias label). They are applied after
    // every row exists, so they list last. No indexed row is inserted into the
    // navigation group, so the recorded indexes stay valid.
    let mut navigate_aliases: Vec<(HelpGroup, usize, String)> = Vec::new();
    macro_rules! build_keybind_help {
        (
            actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:ident, $action_label:literal, $action_doc:literal),)* }
            indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:ident, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
            navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:ident, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
            navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:ident, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
        ) => {
            $(group_rows(&mut groups, HelpGroup::$action_group)
                .push(row(binding_label(&keybinds.$action_field), $action_label));)*
            $(insert_help_row_after(
                group_rows(&mut groups, HelpGroup::$indexed_group),
                $indexed_help_after,
                row(indexed_label(&keybinds.$indexed_field), $indexed_label),
            );)*
            $(
                let index = merge_help_row(
                    group_rows(&mut groups, HelpGroup::$navigate_group),
                    binding_label(&keybinds.navigate.$navigate_field),
                    $navigate_label,
                );
                if let Some(label) = shepr_config::navigate_alias_label!($navigate_alias) {
                    navigate_aliases.push((HelpGroup::$navigate_group, index, label));
                }
            )*
            $(
                let index = merge_help_row(
                    group_rows(&mut groups, HelpGroup::$navigate_indexed_group),
                    indexed_label(&keybinds.navigate.$navigate_indexed_field),
                    $navigate_indexed_label,
                );
                if let Some(label) = shepr_config::navigate_alias_label!($navigate_indexed_alias) {
                    navigate_aliases.push((HelpGroup::$navigate_indexed_group, index, label));
                }
            )*
        };
    }

    shepr_config::keybinding_table!(build_keybind_help);
    for (group, index, alias) in navigate_aliases {
        if let Some(existing) = group_rows(&mut groups, group).get_mut(index) {
            existing.keys.push_str(" / ");
            existing.keys.push_str(&alias);
        }
    }
    groups
}

fn group_rows(groups: &mut Vec<KeybindHelpGroup>, group: HelpGroup) -> &mut Vec<HelpRow> {
    let index = match groups.iter().position(|(name, _)| *name == group) {
        Some(index) => index,
        None => {
            groups.push((group, Vec::new()));
            groups.len() - 1
        }
    };
    &mut groups[index].1
}

/// Places an indexed row right after the row labelled `after`, or last when no
/// such row exists.
fn insert_help_row_after(rows: &mut Vec<HelpRow>, after: &str, help_row: HelpRow) {
    match rows.iter().position(|existing| existing.label == after) {
        Some(index) => rows.insert(index + 1, help_row),
        None => rows.push(help_row),
    }
}

/// Navigate rows that share a help label share one row, keys joined in table
/// order. Returns the row's index in the group.
fn merge_help_row(rows: &mut Vec<HelpRow>, keys: String, label: &'static str) -> usize {
    match rows.iter().position(|existing| existing.label == label) {
        Some(index) => {
            rows[index].keys.push_str(" / ");
            rows[index].keys.push_str(&keys);
            index
        }
        None => {
            rows.push(row(keys, label));
            rows.len() - 1
        }
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
        .filter_map(|(group, rows)| {
            let rows = rows
                .into_iter()
                .filter(|row| {
                    row.keys.to_lowercase().contains(&query)
                        || row.label.to_lowercase().contains(&query)
                })
                .collect::<Vec<_>>();
            (!rows.is_empty()).then_some((group, rows))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn groups() -> Vec<KeybindHelpGroup> {
        vec![
            (
                HelpGroup::Workspaces,
                vec![row("w", "workspace navigation"), row("n", "new workspace")],
            ),
            (
                HelpGroup::Panes,
                vec![row("v", "split vertical"), row("x", "close pane")],
            ),
        ]
    }

    /// The help screen as `(group title, [(keys, label)])`, for exact pins.
    fn screen(groups: &[KeybindHelpGroup]) -> Vec<(&'static str, Vec<(String, &'static str)>)> {
        groups
            .iter()
            .map(|(group, rows)| {
                (
                    group.title(),
                    rows.iter()
                        .map(|row| (row.keys.clone(), row.label))
                        .collect(),
                )
            })
            .collect()
    }

    fn default_screen_rows(config: &str) -> Vec<(&'static str, Vec<(String, &'static str)>)> {
        let live = crate::test_config::validated(config)
            .live_keybinds()
            .clone();
        screen(&keybind_help_groups(&live.keybinds, live.prefix))
    }

    #[test]
    fn filter_matches_labels_and_shortcuts_case_insensitively() {
        let filtered = filter_keybind_help_groups(groups(), "WoRk");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].label, "workspace navigation");

        let filtered = filter_keybind_help_groups(groups(), "x");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1[0].label, "close pane");
        assert!(filter_keybind_help_groups(groups(), "panes").is_empty());
    }

    #[test]
    fn help_filter_and_copy_mode_agree_on_shifted_ascii_keys() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT);

        assert_eq!(keybind_help_text_char(&key), Some('?'));
        assert_eq!(
            keybind_help_text_char(&key),
            crate::copy_mode::copy_mode_key_char(&key)
        );
    }

    /// Pins the whole default help screen, row for row, so a change to the
    /// keybinding table cannot reorder, relabel or drop a row unnoticed.
    #[test]
    fn default_help_screen_rows_are_pinned() {
        let actual = default_screen_rows("");
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
                "workspaces",
                vec![
                    ("prefix+w", "workspace navigation"),
                    ("prefix+g", "session navigator"),
                    ("prefix+shift+n", "new workspace"),
                    ("prefix+shift+w", "rename workspace"),
                    ("prefix+shift+d", "close workspace"),
                    ("prefix+p", "previous workspace"),
                    ("prefix+n", "next workspace"),
                    ("prefix+1..9", "switch workspace 1-9"),
                    ("unset", "previous agent"),
                    ("unset", "next agent"),
                    ("unset", "focus agent 1-9"),
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
        let expected: Vec<(&str, Vec<(String, &str)>)> = expected
            .into_iter()
            .map(|(group, rows)| {
                (
                    group,
                    rows.into_iter()
                        .map(|(keys, label)| (keys.to_owned(), label))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn help_shows_configured_navigate_keys() {
        let actual = default_screen_rows(
            "[keys]\nnavigate_back = \"q\"\nnavigate_cycle_pane_previous = \"\"\nnavigate_switch_workspace = \"alt+1..9\"\n",
        );
        let navigation = &actual
            .iter()
            .find(|(group, _)| *group == "navigation")
            .expect("navigation group")
            .1;
        let rows: Vec<(&str, &str)> = navigation
            .iter()
            .map(|(keys, label)| (keys.as_str(), *label))
            .collect();
        assert!(rows.contains(&("q", "back")), "{rows:?}");
        assert!(rows.contains(&("tab / unset", "cycle pane")), "{rows:?}");
        assert!(rows.contains(&("alt+1..9", "switch workspace")), "{rows:?}");
    }

    /// A range and single keys configured together read as written, and a
    /// prefix range keeps its prefix and modifiers.
    #[test]
    fn help_lists_ranges_and_single_indexed_keys_as_configured() {
        let actual = default_screen_rows(
            "[keys]\nfocus_agent = [\"prefix+alt+1..9\", \"ctrl+alt+2\"]\nswitch_workspace = \"prefix+shift+1..9\"\n",
        );
        let workspaces = &actual
            .iter()
            .find(|(group, _)| *group == "workspaces")
            .expect("workspaces group")
            .1;
        let rows: Vec<(&str, &str)> = workspaces
            .iter()
            .map(|(keys, label)| (keys.as_str(), *label))
            .collect();
        assert!(
            rows.contains(&("prefix+shift+1..9", "switch workspace 1-9")),
            "{rows:?}"
        );
        assert!(
            rows.contains(&("prefix+alt+1..9 / ctrl+alt+2", "focus agent 1-9")),
            "{rows:?}"
        );
    }

    /// The groups appear in the fixed display order, every group the table
    /// names has rows, and an indexed row sits right after the row it follows.
    #[test]
    fn help_groups_are_all_shown_in_display_order() {
        let live = crate::test_config::validated("").live_keybinds().clone();
        let groups = keybind_help_groups(&live.keybinds, live.prefix);
        let shown: Vec<HelpGroup> = groups.iter().map(|(group, _)| *group).collect();
        assert_eq!(shown, HelpGroup::DISPLAY_ORDER);
        assert!(groups.iter().all(|(_, rows)| !rows.is_empty()));

        let workspaces = &groups
            .iter()
            .find(|(group, _)| *group == HelpGroup::Workspaces)
            .expect("workspaces group")
            .1;
        let position = |label: &str| {
            workspaces
                .iter()
                .position(|row| row.label == label)
                .unwrap_or_else(|| panic!("{label} missing"))
        };
        assert_eq!(
            position("switch workspace 1-9"),
            position("next workspace") + 1
        );
        assert_eq!(position("focus agent 1-9"), position("next agent") + 1);
    }

    #[test]
    fn help_lists_every_default_pane_binding() {
        let live = crate::test_config::validated("").live_keybinds().clone();
        let groups = keybind_help_groups(&live.keybinds, live.prefix);
        let rows: Vec<_> = groups.iter().flat_map(|(_, rows)| rows).collect();
        for (keys, label) in [
            ("prefix+[", "copy mode"),
            ("prefix+shift+h", "swap pane left"),
            ("prefix+shift+j", "swap pane down"),
            ("prefix+shift+k", "swap pane up"),
            ("prefix+shift+l", "swap pane right"),
        ] {
            assert!(
                rows.iter()
                    .any(|row| row.keys == keys && row.label == label),
                "{label} ({keys}) missing from the help screen"
            );
        }
    }
}
