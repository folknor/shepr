use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

use super::ClientConfig;
use crate::limits::{
    FIRST_INDEXED_BINDING_KEY, LAST_INDEXED_BINDING_KEY, MAX_FUNCTION_KEY_NUMBER,
    MIN_FUNCTION_KEY_NUMBER,
};
use crate::{ConfigDiagnostic, ConfigKeyPath};

pub(crate) type KeyCombo = (KeyCode, KeyModifiers);

/// The identity used for configured bindings, conflict checks and fallback
/// matching parsed terminal keys. Printable shifted punctuation is identified
/// by the character it produces; letters retain Shift as part of the chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CanonicalKey {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl CanonicalKey {
    fn from_combo((code, modifiers): KeyCombo) -> Self {
        Self::from_event(code, modifiers, None)
    }

    fn from_event(code: KeyCode, modifiers: KeyModifiers, shifted_codepoint: Option<char>) -> Self {
        let (mut code, mut modifiers) = normalize_key_combo((code, modifiers));
        if let KeyCode::Char(ch) = code {
            // Unicode case folds are not always one-to-one, so only ASCII
            // letters have a layout-independent base-key plus Shift form.
            if modifiers.contains(KeyModifiers::SHIFT)
                && ch.is_ascii_alphabetic()
                && let Some(lowercase) = single_case_char(ch.to_lowercase())
            {
                code = KeyCode::Char(lowercase);
            } else if modifiers.contains(KeyModifiers::SHIFT) && !ch.is_alphabetic() {
                let shifted = shifted_codepoint.or_else(|| shifted_ascii_char(ch));
                if let Some(shifted) = shifted {
                    code = KeyCode::Char(shifted);
                    modifiers.remove(KeyModifiers::SHIFT);
                } else if is_shifted_ascii_symbol(ch) {
                    modifiers.remove(KeyModifiers::SHIFT);
                }
            } else if !modifiers.contains(KeyModifiers::SHIFT)
                && ch.is_ascii_uppercase()
                && let Some(lowercase) = single_case_char(ch.to_lowercase())
            {
                code = KeyCode::Char(lowercase);
                modifiers |= KeyModifiers::SHIFT;
            }
        }
        Self { code, modifiers }
    }

    fn combo(self) -> KeyCombo {
        (self.code, self.modifiers)
    }
}

fn single_case_char(mut chars: impl Iterator<Item = char>) -> Option<char> {
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

const SHIFTED_ASCII_KEYS: [(char, char); 21] = [
    ('0', ')'),
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('4', '$'),
    ('5', '%'),
    ('6', '^'),
    ('7', '&'),
    ('8', '*'),
    ('9', '('),
    ('-', '_'),
    ('=', '+'),
    ('[', '{'),
    (']', '}'),
    ('\\', '|'),
    (';', ':'),
    ('\'', '"'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'),
    ('`', '~'),
];

/// Map Shift on a US-layout key to the character it produces for key identity.
fn shifted_ascii_char(ch: char) -> Option<char> {
    SHIFTED_ASCII_KEYS
        .iter()
        .find_map(|(base, shifted)| (*base == ch).then_some(*shifted))
}

fn is_shifted_ascii_symbol(ch: char) -> bool {
    SHIFTED_ASCII_KEYS.iter().any(|(_, shifted)| *shifted == ch)
}

/// The key identity used to resolve configured bindings.
pub trait BindingKey {
    fn code(&self) -> KeyCode;
    fn modifiers(&self) -> KeyModifiers;
    fn shifted_codepoint(&self) -> Option<char>;

    fn canonical_key(&self) -> (KeyCode, KeyModifiers) {
        CanonicalKey::from_event(self.code(), self.modifiers(), self.shifted_codepoint()).combo()
    }
}

#[derive(Debug, Clone)]
pub struct LiveKeybindConfig {
    pub prefix: KeyCombo,
    pub keybinds: Keybinds,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum BindingConfig {
    One(String),
    Many(Vec<String>),
}

impl Default for BindingConfig {
    fn default() -> Self {
        Self::One(String::new())
    }
}

impl BindingConfig {
    pub fn one(value: impl Into<String>) -> Self {
        Self::One(value.into())
    }

    pub fn empty() -> Self {
        Self::One(String::new())
    }

    fn values(&self) -> Vec<&str> {
        match self {
            Self::One(value) => vec![value.as_str()],
            Self::Many(values) => values.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingTrigger {
    Direct(KeyCombo),
    Prefix(KeyCombo),
}

impl BindingTrigger {
    pub fn combo(self) -> KeyCombo {
        match self {
            Self::Direct(combo) | Self::Prefix(combo) => combo,
        }
    }

    pub fn is_direct(self) -> bool {
        matches!(self, Self::Direct(_))
    }

    pub fn is_prefix(self) -> bool {
        matches!(self, Self::Prefix(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinding {
    pub trigger: BindingTrigger,
    pub label: String,
}

impl ResolvedBinding {
    fn matches_terminal_key(&self, key: &impl BindingKey) -> bool {
        terminal_key_matches_combo(key, self.trigger.combo())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionKeybinds {
    pub bindings: Vec<ResolvedBinding>,
}

impl ActionKeybinds {
    pub fn matches_prefix_key(&self, key: &impl BindingKey) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.trigger.is_prefix() && binding.matches_terminal_key(key))
    }

    pub fn matches_direct_key(&self, key: &impl BindingKey) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.trigger.is_direct() && binding.matches_terminal_key(key))
    }

    pub fn labels(&self) -> Vec<String> {
        self.bindings
            .iter()
            .map(|binding| binding.label.clone())
            .collect()
    }

    pub fn label(&self) -> Option<String> {
        let labels = self.labels();
        if labels.is_empty() {
            None
        } else {
            Some(labels.join(" / "))
        }
    }

    pub fn prefix_rhs_label(&self) -> Option<String> {
        let labels: Vec<String> = self
            .bindings
            .iter()
            .filter(|binding| binding.trigger.is_prefix())
            .map(|binding| {
                binding
                    .label
                    .strip_prefix("prefix+")
                    .unwrap_or(&binding.label)
                    .to_string()
            })
            .collect();
        if labels.is_empty() {
            None
        } else {
            Some(labels.join(" / "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedKeybind {
    pub trigger: BindingTrigger,
    pub label: String,
}

/// The configured digit range shared by indexed bindings and their help labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexedRange {
    modifiers: KeyModifiers,
}

impl IndexedRange {
    fn parse(s: &str) -> Option<Self> {
        let syntax = Self::syntax();
        let mut modifiers = KeyModifiers::empty();
        let mut saw_range = false;
        for part in s.split('+') {
            let trimmed = part.trim();
            if trimmed == syntax.as_str() {
                if saw_range {
                    return None;
                }
                saw_range = true;
            } else {
                modifiers |= parse_modifier_token(trimmed)?;
            }
        }
        saw_range.then_some(Self { modifiers })
    }

    /// Return the range syntax derived from the configured first and last keys.
    fn syntax() -> String {
        format!("{FIRST_INDEXED_BINDING_KEY}..{LAST_INDEXED_BINDING_KEY}")
    }

    fn expand(self, prefix: bool) -> Vec<ResolvedBinding> {
        (FIRST_INDEXED_BINDING_KEY..=LAST_INDEXED_BINDING_KEY)
            .map(|key| {
                let combo = (KeyCode::Char(key), self.modifiers);
                let key_label = format_key_combo(combo);
                ResolvedBinding {
                    trigger: if prefix {
                        BindingTrigger::Prefix(combo)
                    } else {
                        BindingTrigger::Direct(combo)
                    },
                    label: if prefix {
                        format!("prefix+{key_label}")
                    } else {
                        key_label
                    },
                }
            })
            .collect()
    }

    /// Compress a complete ordered indexed run into a help label.
    fn label(bindings: &[IndexedKeybind]) -> Option<(String, usize)> {
        let run_len = Self::key_count();
        let run = bindings.get(..run_len)?;
        let prefix = run.first()?.label.strip_suffix(FIRST_INDEXED_BINDING_KEY)?;
        for (offset, binding) in run.iter().enumerate() {
            let key = Self::key_at_offset(offset)?;
            if binding.label.strip_suffix(key) != Some(prefix) {
                return None;
            }
        }
        Some((format!("{prefix}{}", Self::syntax()), run_len))
    }

    /// Match an indexed key, preferring bindings with exactly reported modifiers.
    fn matched_index(bindings: &[IndexedKeybind], key: &impl BindingKey) -> Option<usize> {
        let actual_modifiers = normalize_key_combo((key.code(), key.modifiers())).1;
        for exact_modifiers in [true, false] {
            for binding in bindings {
                let expected_modifiers = normalize_key_combo(binding.trigger.combo()).1;
                if binding.trigger.is_direct()
                    && (actual_modifiers == expected_modifiers) == exact_modifiers
                    && let Some(index) = binding.matched_index(key)
                {
                    return Some(index);
                }
            }
        }
        None
    }

    fn contains_key(code: KeyCode) -> bool {
        matches!(
            code,
            KeyCode::Char(FIRST_INDEXED_BINDING_KEY..=LAST_INDEXED_BINDING_KEY)
        )
    }

    fn key_count() -> usize {
        (FIRST_INDEXED_BINDING_KEY..=LAST_INDEXED_BINDING_KEY).count()
    }

    fn key_at_offset(offset: usize) -> Option<char> {
        let codepoint =
            u32::from(FIRST_INDEXED_BINDING_KEY).checked_add(u32::try_from(offset).ok()?)?;
        (codepoint <= u32::from(LAST_INDEXED_BINDING_KEY))
            .then(|| char::from_u32(codepoint))
            .flatten()
    }
}

impl IndexedKeybind {
    /// Compress a complete ordered indexed run into a help label, if it is uniform.
    pub fn range_label(bindings: &[Self]) -> Option<(String, usize)> {
        IndexedRange::label(bindings)
    }

    /// Match an indexed key, preferring bindings with exactly reported modifiers.
    pub fn matched_range_index(bindings: &[Self], key: &impl BindingKey) -> Option<usize> {
        IndexedRange::matched_index(bindings, key)
    }

    pub fn matched_index(&self, key: &impl BindingKey) -> Option<usize> {
        let combo = self.trigger.combo();
        let (expected_code, _) = normalize_key_combo(combo);
        let KeyCode::Char(key_number) = expected_code else {
            return None;
        };
        if !IndexedRange::contains_key(KeyCode::Char(key_number)) {
            return None;
        }
        let index =
            usize::try_from(u32::from(key_number) - u32::from(FIRST_INDEXED_BINDING_KEY)).ok()?;
        if terminal_key_matches_combo(key, combo) {
            Some(index)
        } else {
            None
        }
    }
}

macro_rules! define_navigate_aliases {
    ($($variant:ident => $key_code:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum NavigateAlias {
            $($variant,)+
        }

        impl NavigateAlias {
            fn from_table_name(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($variant) => Some(Self::$variant),)+
                    _ => None,
                }
            }

            fn combo(self) -> (KeyCode, KeyModifiers) {
                match self {
                    $(Self::$variant => (KeyCode::$key_code, KeyModifiers::empty()),)+
                }
            }

            fn label(self) -> String {
                format_key_combo(self.combo())
            }
        }
    };
}

define_navigate_aliases! {
    Left => Left,
    Right => Right,
}

/// Parsed keybinds for Shepr actions.
macro_rules! define_resolved_keybinds {
    (
        actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
        indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
        navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
        navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
    ) => {
        #[derive(Debug, Clone, Default)]
        pub struct NavigateKeybinds {
            $(pub $navigate_field: ActionKeybinds,)*
            $(pub $navigate_indexed_field: Vec<IndexedKeybind>,)*
        }

        /// Parsed keybinds for Shepr actions.
        #[derive(Debug, Clone, Default)]
        pub struct Keybinds {
            pub navigate: NavigateKeybinds,
            $(pub $action_field: ActionKeybinds,)*
            $(pub $indexed_field: Vec<IndexedKeybind>,)*
        }
    };
}

crate::keybinding_table!(define_resolved_keybinds);

impl Keybinds {
    /// Resolve the key combo for one alias identifier from the central table.
    #[doc(hidden)]
    pub fn navigate_alias_combo_from_table(alias: &str) -> Option<(KeyCode, KeyModifiers)> {
        NavigateAlias::from_table_name(alias).map(NavigateAlias::combo)
    }

    /// Resolve the help label for one alias identifier from the central table.
    #[doc(hidden)]
    pub fn navigate_alias_label_from_table(alias: &str) -> Option<String> {
        NavigateAlias::from_table_name(alias).map(NavigateAlias::label)
    }
}

/// Parsing collects every diagnostic, but exposes no partial keymap when a
/// prefix or any candidate binding is invalid.
#[derive(Debug, Clone)]
pub(crate) struct KeybindValidation {
    pub(super) diagnostics: Vec<ConfigDiagnostic>,
    pub(super) live: Option<LiveKeybindConfig>,
}

#[derive(Clone)]
enum ParsedBinding {
    Single(ResolvedBinding),
    Range(Vec<ResolvedBinding>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingSource {
    Default,
    User,
}

struct RegisteredBinding {
    field: String,
    key: Option<ConfigKeyPath>,
    source: BindingSource,
}

struct BindingRegistry {
    prefix_combo: Option<KeyCombo>,
    prefix_source: BindingSource,
    direct: std::collections::HashMap<CanonicalKey, RegisteredBinding>,
    prefix: std::collections::HashMap<CanonicalKey, RegisteredBinding>,
}

impl BindingRegistry {
    fn new(prefix_combo: Option<KeyCombo>, prefix_source: BindingSource) -> Self {
        Self {
            prefix_combo: prefix_combo.map(normalize_key_combo),
            prefix_source,
            direct: std::collections::HashMap::new(),
            prefix: std::collections::HashMap::new(),
        }
    }

    fn reserve_direct(&mut self, combo: KeyCombo, field: &str, source: BindingSource) {
        let is_prefix = field == "keys.prefix";
        self.direct
            .entry(CanonicalKey::from_combo(combo))
            .or_insert_with(|| RegisteredBinding {
                field: if is_prefix {
                    "configured prefix".to_owned()
                } else {
                    field.to_owned()
                },
                key: is_prefix.then(|| ConfigKeyPath::from_dotted(field)),
                source,
            });
    }

    fn reserved_prefix(&self, combo: KeyCombo) -> Option<KeyCombo> {
        self.prefix_combo
            .filter(|prefix| CanonicalKey::from_combo(combo) == CanonicalKey::from_combo(*prefix))
    }

    fn conflict(&self, binding: &ResolvedBinding) -> Option<&RegisteredBinding> {
        match binding.trigger {
            BindingTrigger::Direct(combo) => self.direct.get(&CanonicalKey::from_combo(combo)),
            BindingTrigger::Prefix(combo) => self.prefix.get(&CanonicalKey::from_combo(combo)),
        }
    }

    fn register(&mut self, binding: &ResolvedBinding, field: &str, source: BindingSource) {
        let registered = || RegisteredBinding {
            field: field.to_string(),
            key: Some(ConfigKeyPath::from_dotted(field)),
            source,
        };
        match binding.trigger {
            BindingTrigger::Direct(combo) => {
                self.direct
                    .insert(CanonicalKey::from_combo(combo), registered());
            }
            BindingTrigger::Prefix(combo) => {
                self.prefix
                    .insert(CanonicalKey::from_combo(combo), registered());
            }
        }
    }
}

impl ClientConfig {
    /// Parse and validate `[keys]` for an in-memory config. The boot resolver
    /// calls this once and stores the result on its immutable value.
    pub(super) fn compute_keybind_validation(
        &self,
        is_configured: impl Fn(&str) -> bool,
    ) -> KeybindValidation {
        let mut diagnostics = Vec::new();
        let prefix = parse_key_combo(&self.keys.prefix);
        if prefix.is_none() {
            diagnostics.push(invalid_keybinding_diagnostic(
                "keys.prefix",
                &self.keys.prefix,
            ));
        }
        let prefix_source = if is_configured("prefix") {
            BindingSource::User
        } else {
            BindingSource::Default
        };
        let mut registry = BindingRegistry::new(prefix, prefix_source);
        if let Some(prefix) = prefix {
            registry.reserve_direct(prefix, "keys.prefix", prefix_source);
        }
        let mut navigate_registry = BindingRegistry::new(prefix, prefix_source);
        if let Some(prefix) = prefix {
            navigate_registry.reserve_direct(prefix, "keys.prefix", prefix_source);
        }
        reserve_navigate_runtime_keys(&mut navigate_registry);
        let mut keybinds = Keybinds::default();

        macro_rules! field_source {
            ($field:ident) => {
                if is_configured(stringify!($field)) {
                    BindingSource::User
                } else {
                    BindingSource::Default
                }
            };
        }
        macro_rules! apply_action {
            ($target:expr, $field:ident, $source:expr) => {
                if field_source!($field) == $source {
                    $target = parse_action_bindings(
                        concat!("keys.", stringify!($field)),
                        &self.keys.$field,
                        &mut registry,
                        &mut diagnostics,
                        $source,
                    );
                }
            };
        }
        macro_rules! apply_indexed {
            ($target:expr, $field:ident, $source:expr) => {
                if field_source!($field) == $source {
                    $target = parse_indexed_bindings(
                        concat!("keys.", stringify!($field)),
                        &self.keys.$field,
                        &mut registry,
                        &mut diagnostics,
                        $source,
                    );
                }
            };
        }
        macro_rules! apply_navigate {
            ($target:expr, $field:ident, $source:expr) => {
                if field_source!($field) == $source {
                    $target = parse_navigate_bindings(
                        concat!("keys.", stringify!($field)),
                        &self.keys.$field,
                        &mut navigate_registry,
                        &mut diagnostics,
                        $source,
                    );
                }
            };
        }
        macro_rules! apply_navigate_indexed {
            ($target:expr, $field:ident, $source:expr) => {
                if field_source!($field) == $source {
                    $target = parse_navigate_indexed_bindings(
                        concat!("keys.", stringify!($field)),
                        &self.keys.$field,
                        &mut navigate_registry,
                        &mut diagnostics,
                        $source,
                    );
                }
            };
        }
        macro_rules! apply_keybinding_table {
            (
                actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
                indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
                navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
                navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
            ) => {
                for source in [BindingSource::User, BindingSource::Default] {
                    $(apply_action!(keybinds.$action_field, $action_field, source);)*
                    $(apply_indexed!(keybinds.$indexed_field, $indexed_field, source);)*
                    $(apply_navigate!(keybinds.navigate.$navigate_field, $navigate_config_field, source);)*
                    $(apply_navigate_indexed!(keybinds.navigate.$navigate_indexed_field, $navigate_indexed_config_field, source);)*
                }
            };
        }

        crate::keybinding_table!(apply_keybinding_table);

        let live = match (diagnostics.is_empty(), prefix) {
            (true, Some(prefix)) => Some(LiveKeybindConfig { prefix, keybinds }),
            _ => None,
        };
        KeybindValidation { diagnostics, live }
    }
}

fn reserve_navigate_runtime_keys(registry: &mut BindingRegistry) {
    macro_rules! reserve_aliases {
        (
            actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
            indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
            navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
            navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
        ) => {
            $(
                if let Some(combo) = crate::navigate_alias!($navigate_alias) {
                    registry.reserve_direct(
                        combo,
                        "navigate pane arrow aliases",
                        BindingSource::Default,
                    );
                }
            )*
            $(
                if let Some(combo) = crate::navigate_alias!($navigate_indexed_alias) {
                    registry.reserve_direct(
                        combo,
                        "navigate pane arrow aliases",
                        BindingSource::Default,
                    );
                }
            )*
        };
    }

    crate::keybinding_table!(reserve_aliases);
}

fn invalid_keybinding_diagnostic(field: &str, raw: &str) -> ConfigDiagnostic {
    let unsupported_function_key = raw.split('+').any(|part| {
        let token = part.trim().to_ascii_lowercase();
        let Some(suffix) = token.strip_prefix('f') else {
            return false;
        };
        if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
            return false;
        }
        !matches!(
            suffix.parse::<u8>(),
            Ok(number) if (MIN_FUNCTION_KEY_NUMBER..=MAX_FUNCTION_KEY_NUMBER).contains(&number)
        )
    });
    let message = if unsupported_function_key {
        format!(
            "invalid keybinding value {raw:?}; supported function keys are F{MIN_FUNCTION_KEY_NUMBER} through F{MAX_FUNCTION_KEY_NUMBER}"
        )
    } else {
        format!("invalid keybinding value {raw:?}")
    };
    ConfigDiagnostic::validation(ConfigKeyPath::from_dotted(field), message)
}

fn parse_action_bindings(
    field: &str,
    config: &BindingConfig,
    registry: &mut BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
) -> ActionKeybinds {
    let mut bindings = Vec::new();
    for raw in config.values() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        match parse_binding_string(raw) {
            Some(ParsedBinding::Single(binding)) => {
                if reject_binding(field, &binding, registry, diagnostics, source) {
                    continue;
                }
                registry.register(&binding, field, source);
                bindings.push(binding);
            }
            Some(ParsedBinding::Range(_)) => {
                let diag = ConfigDiagnostic::validation(
                    ConfigKeyPath::from_dotted(field),
                    format!("range keybinding is only valid for indexed actions: {raw:?}"),
                );
                diagnostics.push(diag);
            }
            None => {
                let diag = invalid_keybinding_diagnostic(field, raw);
                diagnostics.push(diag);
            }
        }
    }
    ActionKeybinds { bindings }
}

fn parse_navigate_bindings(
    field: &'static str,
    config: &BindingConfig,
    registry: &mut BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
) -> ActionKeybinds {
    let mut bindings = Vec::new();
    for raw in config.values() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        match parse_binding_string(raw) {
            Some(ParsedBinding::Single(binding)) => {
                if reject_navigate_binding(field, &binding, registry, diagnostics, source) {
                    continue;
                }
                registry.register(&binding, field, source);
                bindings.push(binding);
            }
            Some(ParsedBinding::Range(_)) => {
                let diag = ConfigDiagnostic::validation(
                    ConfigKeyPath::from_dotted(field),
                    format!("range keybinding is only valid for indexed actions: {raw:?}"),
                );
                diagnostics.push(diag);
            }
            None => {
                let diag = invalid_keybinding_diagnostic(field, raw);
                diagnostics.push(diag);
            }
        }
    }
    ActionKeybinds { bindings }
}

fn parse_indexed_bindings(
    field: &'static str,
    config: &BindingConfig,
    registry: &mut BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
) -> Vec<IndexedKeybind> {
    let mut bindings = Vec::new();
    for raw in config.values() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        match parse_binding_string(raw) {
            Some(ParsedBinding::Single(binding)) => {
                push_indexed_binding(field, binding, registry, diagnostics, source, &mut bindings);
            }
            Some(ParsedBinding::Range(range)) => {
                for binding in range {
                    push_indexed_binding(
                        field,
                        binding,
                        registry,
                        diagnostics,
                        source,
                        &mut bindings,
                    );
                }
            }
            None => {
                let diag = invalid_keybinding_diagnostic(field, raw);
                diagnostics.push(diag);
            }
        }
    }
    bindings
}

fn parse_navigate_indexed_bindings(
    field: &'static str,
    config: &BindingConfig,
    registry: &mut BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
) -> Vec<IndexedKeybind> {
    let mut bindings = Vec::new();
    for raw in config.values() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        match parse_binding_string(raw) {
            Some(ParsedBinding::Single(binding)) => {
                push_navigate_indexed_binding(
                    field,
                    binding,
                    registry,
                    diagnostics,
                    source,
                    &mut bindings,
                );
            }
            Some(ParsedBinding::Range(range)) => {
                for binding in range {
                    push_navigate_indexed_binding(
                        field,
                        binding,
                        registry,
                        diagnostics,
                        source,
                        &mut bindings,
                    );
                }
            }
            None => {
                diagnostics.push(invalid_keybinding_diagnostic(field, raw));
            }
        }
    }
    bindings
}

fn push_indexed_binding(
    field: &str,
    binding: ResolvedBinding,
    registry: &mut BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
    bindings: &mut Vec<IndexedKeybind>,
) {
    if !IndexedRange::contains_key(binding.trigger.combo().0) {
        let diag = ConfigDiagnostic::validation(
            ConfigKeyPath::from_dotted(field),
            format!(
                "indexed keybinding must use {}: {:?}",
                IndexedRange::syntax(),
                binding.label
            ),
        );
        diagnostics.push(diag);
        return;
    }
    if reject_binding(field, &binding, registry, diagnostics, source) {
        return;
    }
    registry.register(&binding, field, source);
    bindings.push(IndexedKeybind {
        trigger: binding.trigger,
        label: binding.label,
    });
}

fn push_navigate_indexed_binding(
    field: &str,
    binding: ResolvedBinding,
    registry: &mut BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
    bindings: &mut Vec<IndexedKeybind>,
) {
    if !IndexedRange::contains_key(binding.trigger.combo().0) {
        diagnostics.push(ConfigDiagnostic::validation(
            ConfigKeyPath::from_dotted(field),
            format!(
                "indexed keybinding must use {}: {:?}",
                IndexedRange::syntax(),
                binding.label
            ),
        ));
        return;
    }
    if reject_navigate_binding(field, &binding, registry, diagnostics, source) {
        return;
    }
    registry.register(&binding, field, source);
    bindings.push(IndexedKeybind {
        trigger: binding.trigger,
        label: binding.label,
    });
}

fn reject_navigate_binding(
    field: &str,
    binding: &ResolvedBinding,
    registry: &BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
) -> bool {
    if binding.trigger.is_prefix() {
        let diag = ConfigDiagnostic::validation(
            ConfigKeyPath::from_dotted(field),
            format!(
                "navigate keybinding must not include prefix: {:?}",
                binding.label
            ),
        );
        diagnostics.push(diag);
        return true;
    }

    if let Some(first_binding) = registry.conflict(binding) {
        let diag = keybinding_conflict_diagnostic(binding, field, first_binding, source);
        diagnostics.push(diag);
        return true;
    }

    false
}

fn reject_binding(
    field: &str,
    binding: &ResolvedBinding,
    registry: &BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
) -> bool {
    if binding.trigger.is_prefix()
        && let Some(prefix_combo) = registry.reserved_prefix(binding.trigger.combo())
    {
        let prefix = format_key_combo(prefix_combo);
        let key = ConfigKeyPath::from_dotted(field);
        let prefix_key = ConfigKeyPath::from_dotted("keys.prefix");
        let diag = if source == BindingSource::Default
            && registry.prefix_source == BindingSource::User
        {
            ConfigDiagnostic::validation_related(
                key,
                vec![prefix_key],
                format!(
                    "reserved keybinding: default value {:?} conflicts with configured prefix {prefix:?}; set this key explicitly to replace or clear its default",
                    binding.label
                ),
            )
        } else {
            ConfigDiagnostic::validation_related(
                key,
                vec![prefix_key],
                format!(
                    "reserved keybinding value {:?} uses prefix {prefix:?} as the action key; pressing the prefix twice sends a literal prefix key",
                    binding.label
                ),
            )
        };
        diagnostics.push(diag);
        return true;
    }

    if let Some(first_binding) = registry.conflict(binding) {
        let diag = keybinding_conflict_diagnostic(binding, field, first_binding, source);
        diagnostics.push(diag);
        return true;
    }

    if binding.trigger.is_direct() && is_unmodified_printable(binding.trigger.combo()) {
        let suggestion = format!("prefix+{}", binding.label);
        let diag = ConfigDiagnostic::validation(
            ConfigKeyPath::from_dotted(field),
            format!(
                "unsafe direct keybinding value {:?} would intercept typing; use {:?} to require the prefix",
                binding.label, suggestion
            ),
        );
        diagnostics.push(diag);
        return true;
    }

    false
}

fn keybinding_conflict_diagnostic(
    binding: &ResolvedBinding,
    field: &str,
    first_binding: &RegisteredBinding,
    source: BindingSource,
) -> ConfigDiagnostic {
    let related_keys = first_binding.key.clone().into_iter().collect();
    if source == BindingSource::Default && first_binding.source == BindingSource::User {
        ConfigDiagnostic::validation_related(
            ConfigKeyPath::from_dotted(field),
            related_keys,
            format!(
                "keybinding conflict: default value {:?} conflicts with a configured binding; set this key explicitly to replace or clear its default",
                binding.label
            ),
        )
    } else {
        let reason = if first_binding.key.is_none() {
            format!(
                "keybinding conflict: {:?} is assigned to reserved {}",
                binding.label, first_binding.field
            )
        } else {
            format!(
                "keybinding conflict: {:?} is assigned to another binding",
                binding.label
            )
        };
        ConfigDiagnostic::validation_related(
            ConfigKeyPath::from_dotted(field),
            related_keys,
            reason,
        )
    }
}

fn parse_binding_string(raw: &str) -> Option<ParsedBinding> {
    let trimmed = raw.trim();
    let (trigger_prefix, body) = if let Some(rest) = trimmed.strip_prefix("prefix+") {
        (true, rest)
    } else {
        (false, trimmed)
    };

    if let Some(range) = IndexedRange::parse(body) {
        return Some(ParsedBinding::Range(range.expand(trigger_prefix)));
    }

    let combo = parse_key_combo(body)?;
    let label = if trigger_prefix {
        format!("prefix+{}", format_key_combo(combo))
    } else {
        format_key_combo(combo)
    };
    Some(ParsedBinding::Single(ResolvedBinding {
        trigger: if trigger_prefix {
            BindingTrigger::Prefix(combo)
        } else {
            BindingTrigger::Direct(combo)
        },
        label,
    }))
}

pub fn format_key_combo(binding: KeyCombo) -> String {
    let (code, modifiers) = binding;
    let mut parts = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    // "meta" is a config alias for Alt (the terminal convention: Meta sends an
    // ESC prefix, and SGR mouse reports carry it in the Alt bit), so no config
    // token parses to crossterm's separate META flag. Label META as "alt" so
    // config-parseable codes read back as the same binding.
    if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::META) {
        parts.push("alt".to_string());
    }
    if modifiers.contains(KeyModifiers::SHIFT) && !matches!(code, KeyCode::BackTab) {
        parts.push("shift".to_string());
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        parts.push(super_modifier_label().to_string());
    }
    if modifiers.contains(KeyModifiers::HYPER) {
        parts.push("hyper".to_string());
    }

    let key = match code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char('+') => "plus".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "shift+tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::F(n) => format!("f{n}"),
        // Config cannot create bindings for every Crossterm KeyCode. Keep a
        // readable label for arbitrary codes passed to this public formatter.
        _ => format!("{code:?}").to_lowercase(),
    };

    if matches!(code, KeyCode::BackTab) {
        return if parts.is_empty() {
            key
        } else {
            format!("{}+{key}", parts.join("+"))
        };
    }

    parts.push(key);
    parts.join("+")
}

fn super_modifier_label() -> &'static str {
    "super"
}

const MODIFIER_ALIASES: &[(&str, KeyModifiers)] = &[
    ("ctrl", KeyModifiers::CONTROL),
    ("control", KeyModifiers::CONTROL),
    ("alt", KeyModifiers::ALT),
    ("option", KeyModifiers::ALT),
    // Terminal mouse reports encode Meta in Alt.
    ("meta", KeyModifiers::ALT),
    ("shift", KeyModifiers::SHIFT),
    ("cmd", KeyModifiers::SUPER),
    ("command", KeyModifiers::SUPER),
    ("super", KeyModifiers::SUPER),
    ("hyper", KeyModifiers::HYPER),
];

pub(crate) fn modifier_aliases() -> &'static [(&'static str, KeyModifiers)] {
    MODIFIER_ALIASES
}

pub(crate) fn parse_modifier_token(token: &str) -> Option<KeyModifiers> {
    MODIFIER_ALIASES
        .iter()
        .find_map(|(alias, modifiers)| alias.eq_ignore_ascii_case(token).then_some(*modifiers))
}

pub fn parse_key_combo(s: &str) -> Option<KeyCombo> {
    let parts: Vec<&str> = s.split('+').collect();
    let mut modifiers = KeyModifiers::empty();
    let mut key_str: Option<&str> = None;

    for part in &parts {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(modifier) = parse_modifier_token(trimmed) {
            modifiers |= modifier;
        } else if key_str.is_some() {
            return None;
        } else {
            key_str = Some(trimmed);
        }
    }

    let key_str = key_str?;
    let single_char = single_key_char(key_str);
    let lower = key_str.to_lowercase();
    let code = match lower.as_str() {
        "space" | " " => KeyCode::Char(' '),
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" if modifiers.contains(KeyModifiers::SHIFT) => {
            modifiers.remove(KeyModifiers::SHIFT);
            KeyCode::BackTab
        }
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        // The names `format_key_combo` prints for these codes, plus the usual
        // spellings, so every navigation key can be bound.
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page_up" | "page-up" | "pgup" => KeyCode::PageUp,
        "pagedown" | "page_down" | "page-down" | "pgdn" => KeyCode::PageDown,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "minus" => KeyCode::Char('-'),
        "comma" => KeyCode::Char(','),
        "period" => KeyCode::Char('.'),
        "slash" => KeyCode::Char('/'),
        "backslash" => KeyCode::Char('\\'),
        "quote" => KeyCode::Char('\''),
        "double_quote" | "double-quote" => KeyCode::Char('"'),
        "semicolon" => KeyCode::Char(';'),
        "colon" => KeyCode::Char(':'),
        "percent" => KeyCode::Char('%'),
        "ampersand" => KeyCode::Char('&'),
        "backtick" => KeyCode::Char('`'),
        "plus" => KeyCode::Char('+'),
        _ if single_char.is_some() => {
            let ch = single_char?;
            if ch.is_ascii_uppercase()
                && let Some(lowercase) = single_case_char(ch.to_lowercase())
            {
                modifiers |= KeyModifiers::SHIFT;
                KeyCode::Char(lowercase)
            } else {
                KeyCode::Char(ch)
            }
        }
        s if s.starts_with('f') => {
            let number = s[1..].parse::<u8>().ok()?;
            // Crossterm's Unix keyboard parser maps extended function keys through F35.
            (MIN_FUNCTION_KEY_NUMBER..=MAX_FUNCTION_KEY_NUMBER)
                .contains(&number)
                .then_some(KeyCode::F(number))?
        }
        _ => return None,
    };

    Some(normalize_key_combo((code, modifiers)))
}

fn single_key_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let ch = chars.next()?;
    if chars.next().is_none() {
        Some(ch)
    } else {
        None
    }
}

pub fn normalize_key_combo((mut code, mut modifiers): KeyCombo) -> KeyCombo {
    if matches!(code, KeyCode::Tab) && modifiers.contains(KeyModifiers::SHIFT) {
        code = KeyCode::BackTab;
        modifiers.remove(KeyModifiers::SHIFT);
    } else if matches!(code, KeyCode::BackTab) {
        modifiers.remove(KeyModifiers::SHIFT);
    }
    (code, modifiers)
}

pub fn terminal_key_matches_combo(key: &impl BindingKey, combo: KeyCombo) -> bool {
    let combo = normalize_key_combo(combo);
    let reported = normalize_key_combo((key.code(), key.modifiers()));
    reported == combo || key.canonical_key() == CanonicalKey::from_combo(combo).combo()
}

fn is_unmodified_printable(combo: KeyCombo) -> bool {
    let key = CanonicalKey::from_combo(combo);
    matches!(key.code, KeyCode::Char(ch) if !ch.is_control())
        && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
}

#[cfg(test)]
use crossterm::event::KeyEvent;

#[cfg(test)]
pub(crate) fn key_event_matches_combo(key: &KeyEvent, combo: KeyCombo) -> bool {
    CanonicalKey::from_event(key.code, key.modifiers, None).combo()
        == CanonicalKey::from_combo(combo).combo()
}

#[cfg(test)]
impl ResolvedBinding {
    fn matches_key_event(&self, key: &KeyEvent) -> bool {
        key_event_matches_combo(key, self.trigger.combo())
    }
}

#[cfg(test)]
impl ActionKeybinds {
    pub fn prefix(label: &str) -> Self {
        let raw = if label.starts_with("prefix+") {
            label.to_string()
        } else {
            format!("prefix+{label}")
        };
        let trigger = parse_binding_string(&raw)
            .and_then(|parsed| match parsed {
                ParsedBinding::Single(binding) => Some(binding),
                ParsedBinding::Range(_) => None,
            })
            .expect("prefix binding should parse");
        Self {
            bindings: vec![trigger],
        }
    }

    pub fn direct(label: &str) -> Self {
        let trigger = parse_binding_string(label)
            .and_then(|parsed| match parsed {
                ParsedBinding::Single(binding) => Some(binding),
                ParsedBinding::Range(_) => None,
            })
            .expect("direct binding should parse");
        Self {
            bindings: vec![trigger],
        }
    }

    pub fn matches_prefix(&self, key: &KeyEvent) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.trigger.is_prefix() && binding.matches_key_event(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientConfig;

    struct TerminalKey(KeyCode, KeyModifiers, Option<char>);

    impl TerminalKey {
        fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
            Self(code, modifiers, None)
        }
        fn with_shifted_codepoint(mut self, codepoint: char) -> Self {
            self.2 = Some(codepoint);
            self
        }
    }

    impl BindingKey for TerminalKey {
        fn code(&self) -> KeyCode {
            self.0
        }
        fn modifiers(&self) -> KeyModifiers {
            self.1
        }
        fn shifted_codepoint(&self) -> Option<char> {
            self.2
        }
    }

    fn binding_triggers(bindings: &ActionKeybinds) -> Vec<BindingTrigger> {
        bindings
            .bindings
            .iter()
            .map(|binding| binding.trigger)
            .collect()
    }

    fn parse_keybinds(config: &ClientConfig, configured: &[&str]) -> Option<Keybinds> {
        let validation = config.compute_keybind_validation(|field| configured.contains(&field));
        validation.live.map(|live| live.keybinds)
    }

    fn diagnostics_and_keybinds(
        config: &ClientConfig,
        configured: &[&str],
    ) -> (Vec<String>, Option<Keybinds>) {
        let validation = config.compute_keybind_validation(|field| configured.contains(&field));
        (
            validation
                .diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.to_string())
                .collect(),
            validation.live.map(|live| live.keybinds),
        )
    }

    #[test]
    fn parse_simple_char_combo() {
        assert_eq!(
            parse_key_combo("v"),
            Some((KeyCode::Char('v'), KeyModifiers::empty()))
        );
    }

    #[test]
    fn parse_unicode_char_combo() {
        assert_eq!(
            parse_key_combo("ö"),
            Some((KeyCode::Char('ö'), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_combo("alt+é"),
            Some((KeyCode::Char('é'), KeyModifiers::ALT))
        );
    }

    #[test]
    fn unicode_prefix_config_is_valid() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
prefix = "ö"
"#,
        )
        .expect("test precondition");
        assert_eq!(
            config
                .compute_keybind_validation(|_| false)
                .live
                .expect("valid unicode prefix")
                .prefix,
            (KeyCode::Char('ö'), KeyModifiers::empty())
        );
        assert!(config.collect_diagnostics().is_empty());
    }

    #[test]
    fn parse_shift_tab_as_backtab() {
        assert_eq!(
            parse_key_combo("shift+tab"),
            Some((KeyCode::BackTab, KeyModifiers::empty()))
        );
    }

    #[test]
    fn parse_named_punctuation() {
        assert_eq!(
            parse_key_combo("minus"),
            Some((KeyCode::Char('-'), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_combo("comma"),
            Some((KeyCode::Char(','), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_combo("ampersand"),
            Some((KeyCode::Char('&'), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_combo("plus"),
            Some((KeyCode::Char('+'), KeyModifiers::empty()))
        );
        assert_eq!(
            format_key_combo((KeyCode::Char('+'), KeyModifiers::empty())),
            "plus"
        );
        assert_eq!(
            parse_key_combo("ctrl+plus"),
            Some((KeyCode::Char('+'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            format_key_combo((KeyCode::Char('+'), KeyModifiers::CONTROL)),
            "ctrl+plus"
        );
    }

    #[test]
    fn parse_navigation_key_names_and_round_trip_their_labels() {
        for (name, code) in [
            ("home", KeyCode::Home),
            ("end", KeyCode::End),
            ("pageup", KeyCode::PageUp),
            ("PageUp", KeyCode::PageUp),
            ("pgup", KeyCode::PageUp),
            ("pagedown", KeyCode::PageDown),
            ("page_down", KeyCode::PageDown),
            ("delete", KeyCode::Delete),
            ("del", KeyCode::Delete),
            ("insert", KeyCode::Insert),
        ] {
            let combo = parse_key_combo(name);
            assert_eq!(combo, Some((code, KeyModifiers::empty())), "{name}");
            let label = format_key_combo((code, KeyModifiers::empty()));
            assert_eq!(parse_key_combo(&label), combo, "{label}");
        }
        assert_eq!(
            parse_key_combo("ctrl+end"),
            Some((KeyCode::End, KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn meta_is_an_alias_for_alt_in_parsing_and_labels() {
        assert_eq!(
            parse_key_combo("meta+x"),
            Some((KeyCode::Char('x'), KeyModifiers::ALT))
        );
        assert_eq!(
            format_key_combo((KeyCode::Char('x'), KeyModifiers::META)),
            "alt+x"
        );
        assert_eq!(
            format_key_combo((KeyCode::Char('x'), KeyModifiers::ALT | KeyModifiers::META)),
            "alt+x"
        );
    }

    #[test]
    fn prefix_binding_is_not_direct_binding() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
next_workspace = "prefix+n"
"#,
        )
        .expect("test precondition");
        let kb = parse_keybinds(&config, &[]).expect("valid keybindings");
        assert_eq!(
            binding_triggers(&kb.next_workspace),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('n'),
                KeyModifiers::empty()
            ))]
        );
    }

    #[test]
    fn goto_defaults_to_prefix_g() {
        let kb = parse_keybinds(&ClientConfig::default(), &[]).expect("default keybindings");
        assert_eq!(
            binding_triggers(&kb.goto),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('g'),
                KeyModifiers::empty()
            ))]
        );
    }

    #[test]
    fn copy_mode_uses_tmux_prefix_bracket_by_default() {
        let kb = parse_keybinds(&ClientConfig::default(), &[]).expect("default keybindings");
        assert_eq!(
            binding_triggers(&kb.copy_mode),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('['),
                KeyModifiers::empty()
            ))]
        );
    }

    #[test]
    fn back_and_forth_keybinds_are_unset_by_default() {
        let kb = parse_keybinds(&ClientConfig::default(), &[]).expect("default keybindings");
        assert!(kb.last_pane.bindings.is_empty());
    }

    #[test]
    fn array_bindings_allow_prefix_and_modified_direct() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
next_workspace = ["prefix+n", "ctrl+alt+]"]
"#,
        )
        .expect("test precondition");
        let kb = parse_keybinds(&config, &[]).expect("valid keybindings");
        assert_eq!(
            binding_triggers(&kb.next_workspace),
            vec![
                BindingTrigger::Prefix((KeyCode::Char('n'), KeyModifiers::empty())),
                BindingTrigger::Direct((
                    KeyCode::Char(']'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                )),
            ]
        );
        assert_eq!(kb.next_workspace.prefix_rhs_label().as_deref(), Some("n"));
    }

    #[test]
    fn unsafe_direct_printable_binding_has_validation_diagnostic() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
new_workspace = "c"
close_workspace = "X"
"#,
        )
        .expect("test precondition");
        let diagnostics = config.collect_diagnostics();
        assert!(config.compute_keybind_validation(|_| false).live.is_none());
        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.contains("unsafe direct keybinding")
                    && diag.contains("keys.new_workspace"))
        );
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("unsafe direct keybinding") && diag.contains("keys.close_workspace")
        }));
    }

    #[test]
    fn unicode_prefix_bindings_match_non_us_keys() {
        for ch in ['ğ', 'ç', 'ş', 'ı', 'é', 'ø'] {
            let bindings = ActionKeybinds::prefix(&ch.to_string());
            assert!(
                bindings.matches_prefix_key(&TerminalKey::new(
                    KeyCode::Char(ch),
                    KeyModifiers::empty(),
                ))
            );
        }
    }

    #[test]
    fn shifted_unicode_prefix_bindings_match_layout_aware_input() {
        for (base, shifted) in [('ğ', 'Ğ'), ('ç', 'Ç'), ('ş', 'Ş'), ('ı', 'I'), ('ø', 'Ø')]
        {
            let bindings = ActionKeybinds::prefix(&format!("shift+{base}"));
            assert!(
                bindings.matches_prefix_key(
                    &TerminalKey::new(KeyCode::Char(base), KeyModifiers::SHIFT)
                        .with_shifted_codepoint(shifted)
                )
            );
        }
    }

    #[test]
    fn shifted_letter_binding_matches_uppercase_key_event() {
        let bindings = ActionKeybinds::prefix("shift+n");
        assert!(bindings.matches_prefix(&KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)));
    }

    #[test]
    fn shifted_letter_binding_matches_legacy_uppercase_key_event() {
        let bindings = ActionKeybinds::prefix("shift+n");
        assert!(
            bindings
                .matches_prefix_key(&TerminalKey::new(KeyCode::Char('N'), KeyModifiers::empty(),))
        );
    }

    #[test]
    fn shifted_letter_direct_binding_matches_legacy_uppercase_key_event() {
        let bindings = ActionKeybinds::direct("shift+n");
        assert!(
            bindings
                .matches_direct_key(&TerminalKey::new(KeyCode::Char('N'), KeyModifiers::empty(),))
        );
    }

    #[test]
    fn shifted_letter_binding_matches_modern_modified_key_event() {
        let bindings = ActionKeybinds::direct("cmd+shift+j");
        assert!(bindings.matches_direct_key(&TerminalKey::new(
            KeyCode::Char('J'),
            KeyModifiers::SUPER | KeyModifiers::SHIFT,
        )));
    }

    #[test]
    fn legacy_uppercase_key_event_does_not_match_unshifted_letter_binding() {
        let bindings = ActionKeybinds::prefix("n");
        assert!(
            !bindings
                .matches_prefix_key(&TerminalKey::new(KeyCode::Char('N'), KeyModifiers::empty(),))
        );
    }

    #[test]
    fn canonical_identity_folds_legacy_ascii_uppercase_and_shifted_symbols() {
        let shifted_number = ActionKeybinds::prefix("shift+1");
        assert!(
            shifted_number
                .matches_prefix_key(&TerminalKey::new(KeyCode::Char('!'), KeyModifiers::empty(),))
        );

        let shifted_non_ascii = ActionKeybinds::prefix("shift+ö");
        assert!(
            !shifted_non_ascii
                .matches_prefix_key(&TerminalKey::new(KeyCode::Char('Ö'), KeyModifiers::empty(),))
        );

        assert_eq!(
            CanonicalKey::from_combo((KeyCode::Char('/'), KeyModifiers::SHIFT)),
            CanonicalKey::from_combo((KeyCode::Char('?'), KeyModifiers::empty()))
        );
        assert_eq!(
            CanonicalKey::from_combo((KeyCode::Char('?'), KeyModifiers::SHIFT)),
            CanonicalKey::from_combo((KeyCode::Char('?'), KeyModifiers::empty()))
        );
    }

    #[test]
    fn keybinding_registry_rejects_shifted_punctuation_aliases() {
        for alias in ["prefix+shift+/", "prefix+shift+?"] {
            let config: ClientConfig =
                toml::from_str(&format!("[keys]\nnew_workspace = {alias:?}\n"))
                    .expect("test precondition");
            let (diagnostics, keybinds) = diagnostics_and_keybinds(&config, &["new_workspace"]);
            assert!(keybinds.is_none(), "{alias}");
            assert!(
                diagnostics.iter().any(|diag| {
                    diag.contains("keybinding conflict")
                        && diag.contains("keys.new_workspace")
                        && diag.contains("keys.help")
                }),
                "{alias}: {diagnostics:?}"
            );
        }

        let config: ClientConfig = toml::from_str(
            r#"
[keys]
switch_workspace = "prefix+shift+1..9"
zoom = "prefix+!"
"#,
        )
        .expect("test precondition");
        let (diagnostics, keybinds) =
            diagnostics_and_keybinds(&config, &["switch_workspace", "zoom"]);
        assert!(keybinds.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.zoom")
                && diag.contains("keys.switch_workspace")
        }));
    }

    #[test]
    fn shifted_tab_inputs_match_backtab_canonical_binding() {
        let bindings = ActionKeybinds::prefix("shift+tab");
        assert!(
            bindings.matches_prefix_key(&TerminalKey::new(KeyCode::BackTab, KeyModifiers::empty()))
        );
        assert!(
            bindings.matches_prefix_key(&TerminalKey::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        );
        assert!(bindings.matches_prefix_key(&TerminalKey::new(KeyCode::Tab, KeyModifiers::SHIFT)));
        assert!(
            !ActionKeybinds::prefix("tab")
                .matches_prefix_key(&TerminalKey::new(KeyCode::Tab, KeyModifiers::SHIFT))
        );
        assert_eq!(
            normalize_key_combo((KeyCode::Tab, KeyModifiers::CONTROL | KeyModifiers::SHIFT)),
            (KeyCode::BackTab, KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn format_modified_backtab_keeps_shift_label() {
        assert_eq!(
            format_key_combo((KeyCode::BackTab, KeyModifiers::CONTROL)),
            "ctrl+shift+tab"
        );
        assert_eq!(
            format_key_combo((KeyCode::BackTab, KeyModifiers::CONTROL | KeyModifiers::ALT)),
            "ctrl+alt+shift+tab"
        );
    }

    #[test]
    fn shifted_punctuation_matches_enhanced_input() {
        let help = ActionKeybinds::prefix("?");
        assert!(
            help.matches_prefix_key(&TerminalKey::new(KeyCode::Char('?'), KeyModifiers::SHIFT))
        );
        assert!(help.matches_prefix_key(
            &TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT).with_shifted_codepoint('?')
        ));

        let bang = ActionKeybinds::prefix("!");
        assert!(bang.matches_prefix_key(
            &TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT).with_shifted_codepoint('!')
        ));
    }

    #[test]
    fn prefix_rhs_equal_to_configured_prefix_is_rejected() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
help = "prefix+ctrl+a"
"#,
        )
        .expect("test precondition");
        let diagnostics = config.collect_diagnostics();
        assert!(parse_keybinds(&config, &[]).is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("reserved keybinding")
                && diag.contains("keys.help")
                && diag.contains("keys.prefix")
        }));

        let config: ClientConfig = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
help = "prefix+ctrl+b"
"#,
        )
        .expect("test precondition");
        assert!(parse_keybinds(&config, &[]).is_some());
    }

    #[test]
    fn navigate_bindings_allow_plain_keys_and_reject_local_conflicts() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
navigate_workspace_up = "j"
navigate_workspace_down = "j"
navigate_pane_down = "ctrl+j"
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]);
        let diagnostics = config.collect_diagnostics();

        assert!(keybinds.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.navigate_workspace_up")
                && diag.contains("keys.navigate_workspace_down")
        }));
    }

    #[test]
    fn navigate_bindings_reject_fixed_arrow_aliases() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
navigate_workspace_up = ["left", "right"]
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]);
        let diagnostics = config.collect_diagnostics();

        assert!(keybinds.is_none());
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diag| {
                    diag.contains("navigate pane arrow aliases")
                        && diag.contains("keys.navigate_workspace_up")
                })
                .count(),
            2
        );
    }

    #[test]
    fn navigate_bindings_can_reuse_navigate_mode_prefix_rhs_keys() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
navigate_workspace_down = ["n", "f"]
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]);
        let diagnostics = config.collect_diagnostics();

        assert!(keybinds.is_some());
        assert!(
            keybinds
                .as_ref()
                .expect("valid keybindings")
                .navigate
                .workspace_down
                .matches_direct_key(&TerminalKey::new(KeyCode::Char('n'), KeyModifiers::empty()))
        );
        assert!(
            keybinds
                .as_ref()
                .expect("valid keybindings")
                .navigate
                .workspace_down
                .matches_direct_key(&TerminalKey::new(KeyCode::Char('f'), KeyModifiers::empty()))
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diag| diag.contains("keys.next_workspace"))
        );
    }

    #[test]
    fn navigate_bindings_do_not_conflict_with_general_focus_pane_bindings() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
navigate_pane_down = "j"
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]).expect("valid keybindings");

        assert!(
            keybinds
                .navigate
                .pane_down
                .matches_direct_key(&TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty()))
        );
    }

    #[test]
    fn navigate_bindings_reject_prefix_syntax_and_prefix_key() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
navigate_workspace_up = "prefix+j"
navigate_workspace_down = "ctrl+a"
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]);
        let diagnostics = config.collect_diagnostics();

        assert!(keybinds.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("navigate keybinding must not include prefix")
                && diag.contains("keys.navigate_workspace_up")
        }));
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.prefix")
                && diag.contains("keys.navigate_workspace_down")
        }));
    }

    #[test]
    fn prefixed_indexed_bindings_support_modifiers() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
switch_workspace = "prefix+shift+1..9"
"#,
        )
        .expect("test precondition");
        let kb = parse_keybinds(&config, &[]).expect("valid keybindings");
        assert_eq!(kb.switch_workspace.len(), 9);
        assert_eq!(
            kb.switch_workspace[0].trigger,
            BindingTrigger::Prefix((KeyCode::Char('1'), KeyModifiers::SHIFT))
        );
        assert_eq!(kb.switch_workspace[0].label, "prefix+shift+1");
    }

    #[test]
    fn indexed_range_supplies_help_labels_and_key_matching() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
switch_workspace = "prefix+alt+1..9"
navigate_switch_workspace = "shift+1..9"
"#,
        )
        .expect("test precondition");
        let kb = parse_keybinds(&config, &["switch_workspace", "navigate_switch_workspace"])
            .expect("valid keybindings");

        assert_eq!(
            IndexedRange::label(&kb.switch_workspace),
            Some(("prefix+alt+1..9".to_owned(), 9))
        );
        assert_eq!(
            IndexedRange::label(&kb.navigate.switch_workspace),
            Some(("shift+1..9".to_owned(), 9))
        );
        assert_eq!(
            IndexedRange::matched_index(
                &kb.navigate.switch_workspace,
                &TerminalKey::new(KeyCode::Char('!'), KeyModifiers::empty()),
            ),
            Some(0)
        );
    }

    #[test]
    fn invalid_indexed_binding_does_not_displace_default_binding() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
switch_workspace = "prefix+?"
"#,
        )
        .expect("test precondition");

        let diagnostics = config.collect_diagnostics();
        let kb = parse_keybinds(&config, &[]);

        assert!(kb.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("indexed keybinding must use 1..9")
                && diag.contains("keys.switch_workspace")
        }));
        assert!(!diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.switch_workspace")
                && diag.contains("keys.help")
        }));
    }

    #[test]
    fn default_keymap_is_prefix_first_and_workspace_centered() {
        let kb = parse_keybinds(&ClientConfig::default(), &[]).expect("default keybindings");
        assert_eq!(
            binding_triggers(&kb.next_workspace),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('n'),
                KeyModifiers::empty()
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.previous_workspace),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('p'),
                KeyModifiers::empty()
            ))]
        );
        assert_eq!(kb.switch_workspace.len(), 9);
        assert!(
            kb.switch_workspace
                .iter()
                .all(|binding| binding.trigger.is_prefix())
        );
        assert!(
            kb.new_workspace
                .bindings
                .iter()
                .all(|binding| binding.trigger.is_prefix())
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_left),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('h'),
                KeyModifiers::SHIFT
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_down),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('j'),
                KeyModifiers::SHIFT
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_up),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('k'),
                KeyModifiers::SHIFT
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_right),
            vec![BindingTrigger::Prefix((
                KeyCode::Char('l'),
                KeyModifiers::SHIFT
            ))]
        );
    }

    #[test]
    fn duplicate_prefix_bindings_report_conflict() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
next_workspace = "prefix+n"
new_workspace = "prefix+n"
"#,
        )
        .expect("test precondition");
        let diagnostics = config.collect_diagnostics();
        let kb = parse_keybinds(&config, &[]);
        assert!(kb.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.new_workspace")
                && diag.contains("keys.next_workspace")
        }));
    }

    #[test]
    fn user_binding_conflicting_with_a_default_is_reported() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
new_workspace = "prefix+z"
"#,
        )
        .expect("test precondition");

        let (diagnostics, keybinds) = diagnostics_and_keybinds(&config, &["new_workspace"]);

        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.new_workspace")
                && diag.contains("keys.zoom")
        }));
        assert!(keybinds.is_none());
    }

    #[test]
    fn rebinding_the_displaced_action_too_resolves_the_conflict() {
        for zoom in ["\"prefix+shift+z\"", "\"\"", "[]"] {
            let config: ClientConfig = toml::from_str(&format!(
                "[keys]\nnew_workspace = \"prefix+z\"\nzoom = {zoom}\n"
            ))
            .expect("test precondition");

            let (diagnostics, keybinds) =
                diagnostics_and_keybinds(&config, &["new_workspace", "zoom"]);

            assert!(diagnostics.is_empty(), "zoom = {zoom}: {diagnostics:?}");
            let kb = keybinds.expect("all bindings valid");
            assert_eq!(
                binding_triggers(&kb.new_workspace),
                vec![BindingTrigger::Prefix((
                    KeyCode::Char('z'),
                    KeyModifiers::empty()
                ))]
            );
        }
    }

    #[test]
    fn user_prefix_conflicting_with_a_default_binding_is_reported() {
        for (prefix, field, diagnostic) in [
            ("h", "keys.navigate_pane_left", "keybinding conflict"),
            ("n", "keys.next_workspace", "reserved keybinding"),
        ] {
            let config: ClientConfig = toml::from_str(&format!("[keys]\nprefix = {prefix:?}\n"))
                .expect("test precondition");
            let (diagnostics, keybinds) = diagnostics_and_keybinds(&config, &["prefix"]);

            assert!(
                diagnostics
                    .iter()
                    .any(|diag| diag.contains(diagnostic) && diag.contains(field))
            );
            assert!(keybinds.is_none());
        }
    }

    #[test]
    fn duplicate_user_binding_still_reports_conflict() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
previous_workspace = "prefix+shift+l"
swap_pane_right = "prefix+shift+l"
"#,
        )
        .expect("test precondition");

        let diagnostics = config.collect_diagnostics();
        let kb = parse_keybinds(&config, &[]);
        assert!(kb.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.previous_workspace")
                && diag.contains("keys.swap_pane_right")
        }));
    }
}
