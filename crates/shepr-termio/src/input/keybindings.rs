use crossterm::event::KeyCode;

use shepr_config::Keybinds;

use super::TerminalKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindDispatch {
    Direct,
    Prefix,
}

macro_rules! define_keybinding_actions {
    (
        actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
        indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
        navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
        navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
    ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum KeybindAction {
            $($action_variant,)*
            $($indexed_variant(usize),)*
        }

    };
}

shepr_config::keybinding_table!(define_keybinding_actions);

pub fn resolve_direct_binding(keybinds: &Keybinds, key: &TerminalKey) -> Option<KeybindAction> {
    resolve_exact_binding(keybinds, key, KeybindDispatch::Direct)
}

pub fn resolve_prefix_binding(keybinds: &Keybinds, key: &TerminalKey) -> Option<KeybindAction> {
    resolve_exact_binding(keybinds, key, KeybindDispatch::Prefix).or_else(|| {
        generated_character_key(key).and_then(|generated_key| {
            resolve_exact_binding(keybinds, &generated_key, KeybindDispatch::Prefix)
        })
    })
}

pub fn resolve_non_indexed_action(
    keybinds: &Keybinds,
    key: &TerminalKey,
    dispatch: KeybindDispatch,
) -> Option<KeybindAction> {
    macro_rules! resolve_actions {
        (
            actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
            indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
            navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
            navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
        ) => {
            $(
                if action_matches(&keybinds.$action_field, key, dispatch) {
                    return Some(KeybindAction::$action_variant);
                }
            )*
        }
    }
    shepr_config::keybinding_table!(resolve_actions);
    None
}

pub fn resolve_indexed_action(
    keybinds: &Keybinds,
    key: &TerminalKey,
    dispatch: KeybindDispatch,
) -> Option<KeybindAction> {
    let actual_modifiers = shepr_config::normalize_key_combo((key.code, key.modifiers)).1;

    // The second pass only reaches combos accepted by the config matcher's full
    // code-and-modifier check, including its legacy shifted-key forms.
    for exact_modifiers in [true, false] {
        macro_rules! resolve_indexed {
            (
                actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
                indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
                navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
                navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
            ) => {
                $(
                    for binding in &keybinds.$indexed_field {
                        let dispatch_matches = match dispatch {
                            KeybindDispatch::Direct => binding.trigger.is_direct(),
                            KeybindDispatch::Prefix => binding.trigger.is_prefix(),
                        };
                        let expected_modifiers =
                            shepr_config::normalize_key_combo(binding.trigger.combo()).1;
                        if dispatch_matches
                            && (actual_modifiers == expected_modifiers) == exact_modifiers
                            && let Some(index) = binding.matched_index(key)
                        {
                            return Some(KeybindAction::$indexed_variant(index));
                        }
                    }
                )*
            };
        }
        shepr_config::keybinding_table!(resolve_indexed);
    }

    None
}

fn resolve_exact_binding(
    keybinds: &Keybinds,
    key: &TerminalKey,
    dispatch: KeybindDispatch,
) -> Option<KeybindAction> {
    resolve_non_indexed_action(keybinds, key, dispatch)
        .or_else(|| resolve_indexed_action(keybinds, key, dispatch))
}

fn generated_character_key(key: &TerminalKey) -> Option<TerminalKey> {
    let character = key.committed_char()?;
    Some(TerminalKey::new(
        KeyCode::Char(character),
        crossterm::event::KeyModifiers::empty(),
    ))
}

fn action_matches(
    bindings: &shepr_config::ActionKeybinds,
    key: &TerminalKey,
    dispatch: KeybindDispatch,
) -> bool {
    match dispatch {
        KeybindDispatch::Direct => bindings.matches_direct_key(key),
        KeybindDispatch::Prefix => bindings.matches_prefix_key(key),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    #[test]
    fn clear_pane_is_unbound_by_default_and_configurable() {
        let default = crate::test_config::validated("").live_keybinds();
        assert!(default.keybinds.clear_pane.bindings.is_empty());
        let config =
            crate::test_config::validated("[keys]\nclear_pane = [\"super+k\", \"prefix+ctrl+k\"]");
        let keybinds = config.live_keybinds().keybinds;
        assert!(matches!(
            resolve_direct_binding(
                &keybinds,
                &TerminalKey::new(KeyCode::Char('k'), KeyModifiers::SUPER)
            ),
            Some(KeybindAction::ClearPane)
        ));
        assert!(matches!(
            resolve_prefix_binding(
                &keybinds,
                &TerminalKey::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
            ),
            Some(KeybindAction::ClearPane)
        ));
        assert!(matches!(
            resolve_prefix_binding(
                &keybinds,
                &TerminalKey::new(KeyCode::Char('k'), KeyModifiers::SHIFT)
            ),
            Some(KeybindAction::SwapPaneUp)
        ));
    }

    #[test]
    fn one_shared_resolver_handles_direct_prefix_and_indexed_bindings() {
        let keybinds = crate::test_config::validated("[keys]\nnext_workspace = \"ctrl+n\"\n")
            .live_keybinds()
            .keybinds;

        let direct = TerminalKey::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(matches!(
            resolve_direct_binding(&keybinds, &direct),
            Some(KeybindAction::NextWorkspace)
        ));

        let help = TerminalKey::new(KeyCode::Char('?'), KeyModifiers::empty());
        assert!(matches!(
            resolve_prefix_binding(&keybinds, &help),
            Some(KeybindAction::Help)
        ));

        let one = TerminalKey::new(KeyCode::Char('1'), KeyModifiers::empty());
        assert!(matches!(
            resolve_prefix_binding(&keybinds, &one),
            Some(KeybindAction::SwitchWorkspace(0))
        ));
    }

    #[test]
    fn prefix_resolution_uses_shared_generated_character_fallback() {
        let keybinds = crate::test_config::validated("").live_keybinds().keybinds;
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("?".to_owned()));

        assert!(matches!(
            resolve_prefix_binding(&keybinds, &key),
            Some(KeybindAction::Help)
        ));
    }
}
