use crossterm::event::KeyCode;

use shepr_config::Keybinds;

use shepr_term::key::TerminalKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeybindDispatch {
    Direct,
    Prefix,
}

shepr_config::keybinding_rows! {
    $ define_keybinding_actions;
    actions(variant = $action_variant)
    indexed(variant = $indexed_variant)
    => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum KeybindAction {
            $($action_variant,)*
            $($indexed_variant(usize),)*
        }
    }
}

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

fn resolve_non_indexed_action(
    keybinds: &Keybinds,
    key: &TerminalKey,
    dispatch: KeybindDispatch,
) -> Option<KeybindAction> {
    shepr_config::keybinding_rows! {
        $ resolve_actions;
        actions(field = $action_field, variant = $action_variant)
        => {
            $(
                if action_matches(&keybinds.$action_field, key, dispatch) {
                    return Some(KeybindAction::$action_variant);
                }
            )*
        }
    }
    None
}

fn resolve_indexed_action(
    keybinds: &Keybinds,
    key: &TerminalKey,
    dispatch: KeybindDispatch,
) -> Option<KeybindAction> {
    // The second pass only reaches combos accepted by the config matcher's full
    // code-and-modifier check, including its legacy shifted-key forms.
    for exact_modifiers in [true, false] {
        shepr_config::keybinding_rows! {
            $ resolve_indexed;
            indexed(field = $indexed_field, variant = $indexed_variant)
            => {
                $(
                    for binding in &keybinds.$indexed_field {
                        let dispatch_matches = match dispatch {
                            KeybindDispatch::Direct => binding.is_direct(),
                            KeybindDispatch::Prefix => binding.is_prefix(),
                        };
                        if dispatch_matches
                            && binding.modifiers_match_exactly(key) == exact_modifiers
                            && let Some(index) = binding.matched_index(key)
                        {
                            return Some(KeybindAction::$indexed_variant(index));
                        }
                    }
                )*
            }
        }
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
        let default = crate::test_config::validated("").live_keybinds().clone();
        assert!(default.keybinds.clear_pane.bindings.is_empty());
        let config =
            crate::test_config::validated("[keys]\nclear_pane = [\"super+k\", \"prefix+ctrl+k\"]");
        let keybinds = config.live_keybinds().keybinds.clone();
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
            .keybinds
            .clone();

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
        let keybinds = crate::test_config::validated("")
            .live_keybinds()
            .keybinds
            .clone();
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("?".to_owned()));

        assert!(matches!(
            resolve_prefix_binding(&keybinds, &key),
            Some(KeybindAction::Help)
        ));
    }
}
