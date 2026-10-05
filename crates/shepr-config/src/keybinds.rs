use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

use super::ClientConfig;
use crate::limits::{
    FIRST_INDEXED_BINDING_KEY, LAST_INDEXED_BINDING_KEY, MAX_FUNCTION_KEY_NUMBER,
    MIN_FUNCTION_KEY_NUMBER,
};
use crate::{ConfigDiagnostic, ConfigKeyPath};
use shepr_term::key::{CanonicalKey, KeyChord, TerminalKey, single_case_char};

#[derive(Debug, Clone)]
pub struct LiveKeybindConfig {
    pub prefix: KeyChord,
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
    Direct(KeyChord),
    Prefix(KeyChord),
}

impl BindingTrigger {
    pub fn chord(self) -> KeyChord {
        match self {
            Self::Direct(chord) | Self::Prefix(chord) => chord,
        }
    }

    pub fn is_direct(self) -> bool {
        matches!(self, Self::Direct(_))
    }

    pub fn is_prefix(self) -> bool {
        matches!(self, Self::Prefix(_))
    }
}

/// The label the help screen and diagnostics show: the chord, behind
/// `prefix+` for a prefix trigger.
impl std::fmt::Display for BindingTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct(chord) => f.write_str(&format_key_chord(*chord)),
            Self::Prefix(chord) => write!(f, "prefix+{}", format_key_chord(*chord)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinding {
    pub trigger: BindingTrigger,
}

impl ResolvedBinding {
    fn matches_terminal_key(&self, key: &TerminalKey) -> bool {
        self.trigger.chord().matches(key)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionKeybinds {
    pub bindings: Vec<ResolvedBinding>,
}

impl ActionKeybinds {
    pub fn matches_prefix_key(&self, key: &TerminalKey) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.trigger.is_prefix() && binding.matches_terminal_key(key))
    }

    pub fn matches_direct_key(&self, key: &TerminalKey) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.trigger.is_direct() && binding.matches_terminal_key(key))
    }

    pub fn labels(&self) -> Vec<String> {
        self.bindings
            .iter()
            .map(|binding| binding.trigger.to_string())
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
            .map(|binding| format_key_chord(binding.trigger.chord()))
            .collect();
        if labels.is_empty() {
            None
        } else {
            Some(labels.join(" / "))
        }
    }
}

/// One configured indexed binding: a single key, or a whole digit range kept
/// as one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexedKeybind {
    Key(BindingTrigger),
    Range(IndexedRange),
}

/// The configured digit range (`1..9`) behind one trigger kind and one set of
/// modifiers, shared by indexed bindings and their help labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedRange {
    pub prefix: bool,
    pub modifiers: KeyModifiers,
}

/// The label of the range as the user writes it, such as `prefix+alt+1..9`.
impl std::fmt::Display for IndexedRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.prefix {
            f.write_str("prefix+")?;
        }
        let mut parts = modifier_labels(self.modifiers, KeyCode::Char(FIRST_INDEXED_BINDING_KEY));
        parts.push(Self::syntax());
        f.write_str(&parts.join("+"))
    }
}

impl IndexedRange {
    /// Parse a range body (no `prefix+`); `prefix` records the trigger kind.
    fn parse(s: &str, prefix: bool) -> Option<Self> {
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
        saw_range.then_some(Self { prefix, modifiers })
    }

    /// Return the range syntax derived from the configured first and last keys.
    fn syntax() -> String {
        format!("{FIRST_INDEXED_BINDING_KEY}..{LAST_INDEXED_BINDING_KEY}")
    }

    fn trigger(self, key: char) -> BindingTrigger {
        let chord = KeyChord::new(KeyCode::Char(key), self.modifiers);
        if self.prefix {
            BindingTrigger::Prefix(chord)
        } else {
            BindingTrigger::Direct(chord)
        }
    }

    /// Every key of the range as its own binding, in index order. Validation
    /// checks each against the registry; the live keymap keeps the range whole.
    fn expand(self) -> Vec<ResolvedBinding> {
        (FIRST_INDEXED_BINDING_KEY..=LAST_INDEXED_BINDING_KEY)
            .map(|key| ResolvedBinding {
                trigger: self.trigger(key),
            })
            .collect()
    }

    /// The index of the range key that `key` matches, if any.
    fn matched_index(self, key: &TerminalKey) -> Option<usize> {
        (FIRST_INDEXED_BINDING_KEY..=LAST_INDEXED_BINDING_KEY)
            .position(|number| self.trigger(number).chord().matches(key))
    }

    fn contains_key(code: KeyCode) -> bool {
        matches!(
            code,
            KeyCode::Char(FIRST_INDEXED_BINDING_KEY..=LAST_INDEXED_BINDING_KEY)
        )
    }
}

/// Every indexed binding's label is its trigger's (a range's is its own).
impl std::fmt::Display for IndexedKeybind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key(trigger) => std::fmt::Display::fmt(trigger, f),
            Self::Range(range) => std::fmt::Display::fmt(range, f),
        }
    }
}

impl IndexedKeybind {
    pub fn is_direct(&self) -> bool {
        match self {
            Self::Key(trigger) => trigger.is_direct(),
            Self::Range(range) => !range.prefix,
        }
    }

    pub fn is_prefix(&self) -> bool {
        !self.is_direct()
    }

    /// Whether the binding's modifiers are exactly the ones `key` reports.
    pub fn modifiers_match_exactly(&self, key: &TerminalKey) -> bool {
        match self {
            Self::Key(trigger) => trigger.chord().modifiers_match_exactly(key),
            Self::Range(range) => {
                KeyChord::new(KeyCode::Char(FIRST_INDEXED_BINDING_KEY), range.modifiers)
                    .modifiers_match_exactly(key)
            }
        }
    }

    pub fn matched_index(&self, key: &TerminalKey) -> Option<usize> {
        match self {
            Self::Range(range) => range.matched_index(key),
            Self::Key(trigger) => {
                let chord = trigger.chord();
                let KeyCode::Char(key_number) = chord.normalized().code else {
                    return None;
                };
                if !IndexedRange::contains_key(KeyCode::Char(key_number)) {
                    return None;
                }
                let index =
                    usize::try_from(u32::from(key_number) - u32::from(FIRST_INDEXED_BINDING_KEY))
                        .ok()?;
                chord.matches(key).then_some(index)
            }
        }
    }
}

crate::keybinding_rows! {
    $ define_resolved_keybinds;
    actions(field = $action_field)
    indexed(field = $indexed_field)
    navigate(field = $navigate_field)
    => {
        #[derive(Debug, Clone, Default)]
        pub struct NavigateKeybinds {
            $(pub $navigate_field: ActionKeybinds,)*
        }

        /// Parsed keybinds for Shepr actions.
        #[derive(Debug, Clone, Default)]
        pub struct Keybinds {
            pub navigate: NavigateKeybinds,
            $(pub $action_field: ActionKeybinds,)*
            $(pub $indexed_field: Vec<IndexedKeybind>,)*
        }
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
    Range(IndexedRange),
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
    prefix_chord: Option<KeyChord>,
    prefix_source: BindingSource,
    direct: std::collections::HashMap<CanonicalKey, RegisteredBinding>,
    prefix: std::collections::HashMap<CanonicalKey, RegisteredBinding>,
}

impl BindingRegistry {
    fn new(prefix_chord: Option<KeyChord>, prefix_source: BindingSource) -> Self {
        Self {
            prefix_chord: prefix_chord.map(KeyChord::normalized),
            prefix_source,
            direct: std::collections::HashMap::new(),
            prefix: std::collections::HashMap::new(),
        }
    }

    fn reserve_direct(&mut self, chord: KeyChord, field: &str, source: BindingSource) {
        let is_prefix = field == "keys.prefix";
        self.direct
            .entry(chord.canonical())
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

    fn reserved_prefix(&self, chord: KeyChord) -> Option<KeyChord> {
        self.prefix_chord
            .filter(|prefix| chord.canonical() == prefix.canonical())
    }

    fn conflict(&self, binding: &ResolvedBinding) -> Option<&RegisteredBinding> {
        match binding.trigger {
            BindingTrigger::Direct(chord) => self.direct.get(&chord.canonical()),
            BindingTrigger::Prefix(chord) => self.prefix.get(&chord.canonical()),
        }
    }

    fn register(&mut self, binding: &ResolvedBinding, field: &str, source: BindingSource) {
        let registered = || RegisteredBinding {
            field: field.to_string(),
            key: Some(ConfigKeyPath::from_dotted(field)),
            source,
        };
        match binding.trigger {
            BindingTrigger::Direct(chord) => {
                self.direct.insert(chord.canonical(), registered());
            }
            BindingTrigger::Prefix(chord) => {
                self.prefix.insert(chord.canonical(), registered());
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
        let prefix = parse_key_chord(&self.keys.prefix);
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
        crate::keybinding_rows! {
            $ apply_keybinding_table;
            actions(field = $action_field)
            indexed(field = $indexed_field)
            navigate(config_field = $navigate_config_field, field = $navigate_field)
            => {
                for source in [BindingSource::User, BindingSource::Default] {
                    $(apply_action!(keybinds.$action_field, $action_field, source);)*
                    $(apply_indexed!(keybinds.$indexed_field, $indexed_field, source);)*
                    $(apply_navigate!(keybinds.navigate.$navigate_field, $navigate_config_field, source);)*
                }
            }
        }

        let live = match (diagnostics.is_empty(), prefix) {
            (true, Some(prefix)) => Some(LiveKeybindConfig { prefix, keybinds }),
            _ => None,
        };
        KeybindValidation { diagnostics, live }
    }
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
                if accept_indexed_binding(field, &binding, registry, diagnostics, source) {
                    bindings.push(IndexedKeybind::Key(binding.trigger));
                }
            }
            Some(ParsedBinding::Range(range)) => {
                // Check every key so each conflict is reported, then keep the
                // range whole.
                let mut all_accepted = true;
                for binding in range.expand() {
                    all_accepted &=
                        accept_indexed_binding(field, &binding, registry, diagnostics, source);
                }
                if all_accepted {
                    bindings.push(IndexedKeybind::Range(range));
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

/// Validates one key of an indexed binding and registers it; false when it
/// was rejected (the diagnostic is recorded).
fn accept_indexed_binding(
    field: &str,
    binding: &ResolvedBinding,
    registry: &mut BindingRegistry,
    diagnostics: &mut Vec<ConfigDiagnostic>,
    source: BindingSource,
) -> bool {
    if !IndexedRange::contains_key(binding.trigger.chord().code) {
        let diag = ConfigDiagnostic::validation(
            ConfigKeyPath::from_dotted(field),
            format!(
                "indexed keybinding must use {}: {:?}",
                IndexedRange::syntax(),
                binding.trigger.to_string()
            ),
        );
        diagnostics.push(diag);
        return false;
    }
    if reject_binding(field, binding, registry, diagnostics, source) {
        return false;
    }
    registry.register(binding, field, source);
    true
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
                binding.trigger.to_string()
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
        && let Some(prefix_chord) = registry.reserved_prefix(binding.trigger.chord())
    {
        let prefix = format_key_chord(prefix_chord);
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
                    binding.trigger.to_string()
                ),
            )
        } else {
            ConfigDiagnostic::validation_related(
                key,
                vec![prefix_key],
                format!(
                    "reserved keybinding value {:?} uses prefix {prefix:?} as the action key; pressing the prefix twice sends a literal prefix key",
                    binding.trigger.to_string()
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

    if binding.trigger.is_direct()
        && binding
            .trigger
            .chord()
            .canonical()
            .is_unmodified_printable()
    {
        let label = binding.trigger.to_string();
        let suggestion = format!("prefix+{label}");
        let diag = ConfigDiagnostic::validation(
            ConfigKeyPath::from_dotted(field),
            format!(
                "unsafe direct keybinding value {label:?} would intercept typing; use {suggestion:?} to require the prefix"
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
                binding.trigger.to_string()
            ),
        )
    } else {
        let reason = if first_binding.key.is_none() {
            format!(
                "keybinding conflict: {:?} is assigned to reserved {}",
                binding.trigger.to_string(),
                first_binding.field
            )
        } else {
            format!(
                "keybinding conflict: {:?} is assigned to another binding",
                binding.trigger.to_string()
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

    if let Some(range) = IndexedRange::parse(body, trigger_prefix) {
        return Some(ParsedBinding::Range(range));
    }

    let chord = parse_key_chord(body)?;
    Some(ParsedBinding::Single(ResolvedBinding {
        trigger: if trigger_prefix {
            BindingTrigger::Prefix(chord)
        } else {
            BindingTrigger::Direct(chord)
        },
    }))
}

/// The modifier words `format_key_chord` writes before the key, in order.
fn modifier_labels(modifiers: KeyModifiers, code: KeyCode) -> Vec<String> {
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
    parts
}

pub fn format_key_chord(chord: KeyChord) -> String {
    let KeyChord { code, modifiers } = chord;
    let mut parts = modifier_labels(modifiers, code);

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
    ("meta", KeyModifiers::ALT),
    ("shift", KeyModifiers::SHIFT),
    ("cmd", KeyModifiers::SUPER),
    ("command", KeyModifiers::SUPER),
    ("super", KeyModifiers::SUPER),
    ("hyper", KeyModifiers::HYPER),
];

pub(crate) fn parse_modifier_token(token: &str) -> Option<KeyModifiers> {
    MODIFIER_ALIASES
        .iter()
        .find_map(|(alias, modifiers)| alias.eq_ignore_ascii_case(token).then_some(*modifiers))
}

pub fn parse_key_chord(s: &str) -> Option<KeyChord> {
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
        // The names `format_key_chord` prints for these codes, plus the usual
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

    Some(KeyChord::new(code, modifiers).normalized())
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

#[cfg(test)]
use crossterm::event::KeyEvent;

#[cfg(test)]
impl ResolvedBinding {
    fn matches_key_event(&self, key: &KeyEvent) -> bool {
        KeyChord::new(key.code, key.modifiers).canonical() == self.trigger.chord().canonical()
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
            parse_key_chord("v"),
            Some(KeyChord::new(KeyCode::Char('v'), KeyModifiers::empty()))
        );
    }

    #[test]
    fn parse_unicode_char_combo() {
        assert_eq!(
            parse_key_chord("ö"),
            Some(KeyChord::new(KeyCode::Char('ö'), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_chord("alt+é"),
            Some(KeyChord::new(KeyCode::Char('é'), KeyModifiers::ALT))
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
            KeyChord::new(KeyCode::Char('ö'), KeyModifiers::empty())
        );
        assert!(config.collect_diagnostics().is_empty());
    }

    #[test]
    fn parse_shift_tab_as_backtab() {
        assert_eq!(
            parse_key_chord("shift+tab"),
            Some(KeyChord::new(KeyCode::BackTab, KeyModifiers::empty()))
        );
    }

    #[test]
    fn parse_named_punctuation() {
        assert_eq!(
            parse_key_chord("minus"),
            Some(KeyChord::new(KeyCode::Char('-'), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_chord("comma"),
            Some(KeyChord::new(KeyCode::Char(','), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_chord("ampersand"),
            Some(KeyChord::new(KeyCode::Char('&'), KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_chord("plus"),
            Some(KeyChord::new(KeyCode::Char('+'), KeyModifiers::empty()))
        );
        assert_eq!(
            format_key_chord(KeyChord::new(KeyCode::Char('+'), KeyModifiers::empty())),
            "plus"
        );
        assert_eq!(
            parse_key_chord("ctrl+plus"),
            Some(KeyChord::new(KeyCode::Char('+'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            format_key_chord(KeyChord::new(KeyCode::Char('+'), KeyModifiers::CONTROL)),
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
            let combo = parse_key_chord(name);
            assert_eq!(
                combo,
                Some(KeyChord::new(code, KeyModifiers::empty())),
                "{name}"
            );
            let label = format_key_chord(KeyChord::new(code, KeyModifiers::empty()));
            assert_eq!(parse_key_chord(&label), combo, "{label}");
        }
        assert_eq!(
            parse_key_chord("ctrl+end"),
            Some(KeyChord::new(KeyCode::End, KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn meta_is_an_alias_for_alt_in_parsing_and_labels() {
        assert_eq!(
            parse_key_chord("meta+x"),
            Some(KeyChord::new(KeyCode::Char('x'), KeyModifiers::ALT))
        );
        assert_eq!(
            format_key_chord(KeyChord::new(KeyCode::Char('x'), KeyModifiers::META)),
            "alt+x"
        );
        assert_eq!(
            format_key_chord(KeyChord::new(
                KeyCode::Char('x'),
                KeyModifiers::ALT | KeyModifiers::META
            )),
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
            vec![BindingTrigger::Prefix(KeyChord::new(
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
            vec![BindingTrigger::Prefix(KeyChord::new(
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
            vec![BindingTrigger::Prefix(KeyChord::new(
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
                BindingTrigger::Prefix(KeyChord::new(KeyCode::Char('n'), KeyModifiers::empty())),
                BindingTrigger::Direct(KeyChord::new(
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
    }

    #[test]
    fn format_modified_backtab_keeps_shift_label() {
        assert_eq!(
            format_key_chord(KeyChord::new(KeyCode::BackTab, KeyModifiers::CONTROL)),
            "ctrl+shift+tab"
        );
        assert_eq!(
            format_key_chord(KeyChord::new(
                KeyCode::BackTab,
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
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
navigate_up = "j"
navigate_down = "j"
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]);
        let diagnostics = config.collect_diagnostics();

        assert!(keybinds.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.navigate_up")
                && diag.contains("keys.navigate_down")
        }));
    }

    #[test]
    fn navigate_bindings_can_reuse_navigate_mode_prefix_rhs_keys() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
navigate_down = ["n", "f"]
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
                .down
                .matches_direct_key(&TerminalKey::new(KeyCode::Char('n'), KeyModifiers::empty()))
        );
        assert!(
            keybinds
                .as_ref()
                .expect("valid keybindings")
                .navigate
                .down
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
navigate_down = "j"
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]).expect("valid keybindings");

        assert!(
            keybinds
                .navigate
                .down
                .matches_direct_key(&TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty()))
        );
    }

    #[test]
    fn navigate_bindings_reject_prefix_syntax_and_prefix_key() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
navigate_up = "prefix+j"
navigate_down = "ctrl+a"
"#,
        )
        .expect("test precondition");
        let keybinds = parse_keybinds(&config, &[]);
        let diagnostics = config.collect_diagnostics();

        assert!(keybinds.is_none());
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("navigate keybinding must not include prefix")
                && diag.contains("keys.navigate_up")
        }));
        assert!(diagnostics.iter().any(|diag| {
            diag.contains("keybinding conflict")
                && diag.contains("keys.prefix")
                && diag.contains("keys.navigate_down")
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
        assert_eq!(
            kb.switch_workspace,
            vec![IndexedKeybind::Range(IndexedRange {
                prefix: true,
                modifiers: KeyModifiers::SHIFT,
            })]
        );
        assert_eq!(kb.switch_workspace[0].to_string(), "prefix+shift+1..9");
        assert_eq!(
            IndexedRange {
                prefix: true,
                modifiers: KeyModifiers::SHIFT,
            }
            .expand()[0]
                .trigger,
            BindingTrigger::Prefix(KeyChord::new(KeyCode::Char('1'), KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn single_indexed_keys_and_ranges_label_themselves() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
focus_agent = ["prefix+alt+1..9", "ctrl+alt+2"]
"#,
        )
        .expect("test precondition");
        let kb = parse_keybinds(&config, &["focus_agent"]).expect("valid keybindings");
        let labels: Vec<String> = kb.focus_agent.iter().map(ToString::to_string).collect();
        assert_eq!(labels, ["prefix+alt+1..9", "ctrl+alt+2"]);
        assert_eq!(
            BindingTrigger::Prefix(KeyChord::new(KeyCode::Char('n'), KeyModifiers::empty()))
                .to_string(),
            "prefix+n"
        );
    }

    #[test]
    fn indexed_range_supplies_help_labels_and_key_matching() {
        let config: ClientConfig = toml::from_str(
            r#"
[keys]
switch_workspace = "prefix+alt+1..9"
focus_agent = "alt+1..9"
"#,
        )
        .expect("test precondition");
        let kb = parse_keybinds(&config, &["switch_workspace", "focus_agent"])
            .expect("valid keybindings");

        assert_eq!(kb.switch_workspace[0].to_string(), "prefix+alt+1..9");
        assert_eq!(kb.focus_agent[0].to_string(), "alt+1..9");
        assert_eq!(
            kb.focus_agent[0]
                .matched_index(&TerminalKey::new(KeyCode::Char('3'), KeyModifiers::ALT)),
            Some(2)
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
            vec![BindingTrigger::Prefix(KeyChord::new(
                KeyCode::Char('n'),
                KeyModifiers::empty()
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.previous_workspace),
            vec![BindingTrigger::Prefix(KeyChord::new(
                KeyCode::Char('p'),
                KeyModifiers::empty()
            ))]
        );
        assert_eq!(kb.switch_workspace.len(), 1);
        assert!(kb.switch_workspace.iter().all(IndexedKeybind::is_prefix));
        assert!(
            kb.new_workspace
                .bindings
                .iter()
                .all(|binding| binding.trigger.is_prefix())
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_left),
            vec![BindingTrigger::Prefix(KeyChord::new(
                KeyCode::Char('h'),
                KeyModifiers::SHIFT
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_down),
            vec![BindingTrigger::Prefix(KeyChord::new(
                KeyCode::Char('j'),
                KeyModifiers::SHIFT
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_up),
            vec![BindingTrigger::Prefix(KeyChord::new(
                KeyCode::Char('k'),
                KeyModifiers::SHIFT
            ))]
        );
        assert_eq!(
            binding_triggers(&kb.swap_pane_right),
            vec![BindingTrigger::Prefix(KeyChord::new(
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
                vec![BindingTrigger::Prefix(KeyChord::new(
                    KeyCode::Char('z'),
                    KeyModifiers::empty()
                ))]
            );
        }
    }

    #[test]
    fn user_prefix_conflicting_with_a_default_binding_is_reported() {
        for (prefix, field, diagnostic) in [
            ("enter", "keys.navigate_open", "keybinding conflict"),
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
