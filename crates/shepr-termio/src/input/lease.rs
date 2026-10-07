use std::collections::HashMap;
use std::hash::Hash;

use crossterm::event::KeyCode;

use shepr_term::key::TerminalKey;

/// The host keyboard protocol confirmed for the input bytes being handled.
/// `reports_all_keys` is set only alongside confirmed Kitty event reporting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostKeyboardInputMode {
    pub reports_event_types: bool,
    pub reports_all_keys: bool,
}

/// A held key, identified by its source and key code. A Linux host terminal
/// reports no physical key identity, so two physical keys with the same code
/// (the two Enter keys, say) share one lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InputLeaseKey<Source> {
    source: Source,
    code: KeyCode,
}

impl<Source> InputLeaseKey<Source> {
    pub fn new(source: Source, key: &TerminalKey) -> Self {
        Self {
            source,
            code: key.code,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ForwardedInputLease<Target> {
    pub target: Target,
    pub key: TerminalKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConsumedInputLease<Context> {
    ReprocessRepeats(Context),
    SuppressRepeats,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputLease<Context, Target> {
    Forwarded(ForwardedInputLease<Target>),
    Consumed(ConsumedInputLease<Context>),
}

/// What a repeat event does: go to the pane its press was forwarded to, be
/// routed again as a key in the unchanged input context, or nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum RepeatPlan<Target> {
    Forwarded(Target),
    Reprocess,
    Ignore,
}

pub struct InputLeaseTable<Source, Context, Target> {
    leases: HashMap<InputLeaseKey<Source>, InputLease<Context, Target>>,
}

impl<Source, Context, Target> Default for InputLeaseTable<Source, Context, Target> {
    fn default() -> Self {
        Self {
            leases: HashMap::new(),
        }
    }
}

/// Whether the host will send this press's repeats and release. Event-types
/// mode covers encoded key events, not text commits. Kitty also keeps
/// unmodified Enter, Tab and Backspace in legacy form unless REPORT_ALL_KEYS
/// is active, even when it reports event types for other keys.
fn press_takes_lease(key: &TerminalKey, host_mode: HostKeyboardInputMode) -> bool {
    if host_mode.reports_event_types && host_mode.reports_all_keys {
        return true;
    }
    if !host_mode.reports_event_types || key.generated_text.is_some() {
        return false;
    }
    !key.modifiers.is_empty()
        || !matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace)
}

impl<Source, Context, Target> InputLeaseTable<Source, Context, Target>
where
    Source: Copy + Eq + Hash,
    Context: Clone + Eq,
    Target: Clone + Eq,
{
    /// A fresh press drops the previous lease for that semantic key. A second
    /// press cannot be told apart from a new one and is never a repeat here.
    pub fn prepare_press(&mut self, lease_key: &InputLeaseKey<Source>, key: &TerminalKey) {
        if key.kind == crossterm::event::KeyEventKind::Press {
            // Any fresh press supersedes the previous lease for this semantic
            // key, including one begun under a mode that reported releases.
            self.leases.remove(lease_key);
        }
    }

    /// Records the lease a completed press leaves: forwarded to `target`, or
    /// consumed by the shell, in which case its repeats are routed again only
    /// while the input context stays the one the press began and ended in.
    pub fn complete_press(
        &mut self,
        lease_key: InputLeaseKey<Source>,
        key: &TerminalKey,
        initial_context: Option<&Context>,
        resulting_context: Option<&Context>,
        target: Option<Target>,
        host_mode: HostKeyboardInputMode,
    ) {
        if !press_takes_lease(key, host_mode) {
            return;
        }
        if let Some(target) = target {
            self.insert_forwarded(lease_key, target, key.clone());
            return;
        }
        if !self.leases.contains_key(&lease_key) {
            let disposition = match (initial_context, resulting_context) {
                (Some(initial), Some(resulting)) if initial == resulting => {
                    ConsumedInputLease::ReprocessRepeats(initial.clone())
                }
                _ => ConsumedInputLease::SuppressRepeats,
            };
            self.insert_consumed(lease_key, disposition);
        }
    }

    pub fn plan_repeat(
        &mut self,
        lease_key: InputLeaseKey<Source>,
        current_context: Option<&Context>,
    ) -> RepeatPlan<Target> {
        match self.leases.get(&lease_key) {
            Some(InputLease::Forwarded(lease)) => RepeatPlan::Forwarded(lease.target.clone()),
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(context)))
                if current_context == Some(context) =>
            {
                RepeatPlan::Reprocess
            }
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(_))) => {
                self.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
                RepeatPlan::Ignore
            }
            None if current_context.is_some() => RepeatPlan::Reprocess,
            Some(InputLease::Consumed(ConsumedInputLease::SuppressRepeats)) | None => {
                RepeatPlan::Ignore
            }
        }
    }

    pub fn remove_forwarded(
        &mut self,
        key: &InputLeaseKey<Source>,
    ) -> Option<ForwardedInputLease<Target>> {
        match self.leases.remove(key) {
            Some(InputLease::Forwarded(lease)) => Some(lease),
            Some(InputLease::Consumed(_)) | None => None,
        }
    }

    pub fn insert_forwarded(
        &mut self,
        key: InputLeaseKey<Source>,
        target: Target,
        original: TerminalKey,
    ) {
        self.leases.insert(
            key,
            InputLease::Forwarded(ForwardedInputLease {
                target,
                key: original,
            }),
        );
    }

    pub fn insert_consumed(
        &mut self,
        key: InputLeaseKey<Source>,
        disposition: ConsumedInputLease<Context>,
    ) {
        self.leases.insert(key, InputLease::Consumed(disposition));
    }

    pub fn remove(&mut self, key: &InputLeaseKey<Source>) -> Option<InputLease<Context, Target>> {
        self.leases.remove(key)
    }

    pub fn remove_source(&mut self, source: Source) -> Vec<ForwardedInputLease<Target>> {
        let keys = self
            .leases
            .keys()
            .filter(|key| key.source == source)
            .copied()
            .collect::<Vec<_>>();
        self.remove_keys(keys)
    }

    fn remove_keys(
        &mut self,
        keys: impl IntoIterator<Item = InputLeaseKey<Source>>,
    ) -> Vec<ForwardedInputLease<Target>> {
        keys.into_iter()
            .filter_map(|key| match self.leases.remove(&key) {
                Some(InputLease::Forwarded(lease)) => Some(lease),
                Some(InputLease::Consumed(_)) | None => None,
            })
            .collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.leases.len()
    }
}

#[cfg(test)]
impl<Source, Context, Target> InputLeaseTable<Source, Context, Target>
where
    Source: Copy + Eq + Hash,
    Context: Clone + Eq,
    Target: Clone + Eq,
{
    pub fn contains(&self, key: &InputLeaseKey<Source>) -> bool {
        self.leases.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Context {
        Pane,
        Overlay,
    }

    type Leases = InputLeaseTable<u64, Context, u64>;

    const KITTY_EVENT_TYPES: HostKeyboardInputMode = HostKeyboardInputMode {
        reports_event_types: true,
        reports_all_keys: false,
    };
    const KITTY_REPORT_ALL: HostKeyboardInputMode = HostKeyboardInputMode {
        reports_event_types: true,
        reports_all_keys: true,
    };

    #[test]
    fn remove_source_returns_forwarded_and_discards_consumed_leases() {
        let key = TerminalKey::new(KeyCode::Esc, KeyModifiers::empty());
        let forwarded = InputLeaseKey::new(7, &key);
        let consumed = InputLeaseKey::new(
            7,
            &TerminalKey::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        let other_source = InputLeaseKey::new(8, &key);
        let mut leases = Leases::default();
        leases.insert_forwarded(forwarded, 10, key.clone());
        leases.insert_consumed(consumed, ConsumedInputLease::SuppressRepeats);
        leases.insert_forwarded(other_source, 11, key.clone());

        assert_eq!(
            leases.remove_source(7),
            vec![ForwardedInputLease { target: 10, key }]
        );
        assert_eq!(leases.len(), 1);
        assert!(leases.contains(&other_source));
    }

    #[test]
    fn a_second_press_stays_a_press_and_drops_the_old_lease() {
        let key = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let lease_key = InputLeaseKey::new(7, &key);
        let mut leases = Leases::default();

        leases.insert_forwarded(lease_key, 10, key.clone());
        leases.prepare_press(&lease_key, &key);
        assert!(!leases.contains(&lease_key));

        leases.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
        leases.prepare_press(&lease_key, &key);
        assert!(!leases.contains(&lease_key));
    }

    #[test]
    fn a_press_without_a_release_replaces_an_old_lease() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()));
        let lease_key = InputLeaseKey::new(7, &key);
        let mut leases = Leases::default();
        leases.insert_forwarded(lease_key, 10, key.clone());

        leases.prepare_press(&lease_key, &key);
        assert!(!leases.contains(&lease_key));
    }

    #[test]
    fn forwarded_press_repeats_go_to_the_same_target() {
        let key = TerminalKey::new(KeyCode::Left, KeyModifiers::empty());
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();

        leases.complete_press(
            lease_key,
            &key,
            Some(&context),
            Some(&context),
            Some(10),
            KITTY_EVENT_TYPES,
        );
        assert_eq!(
            leases.plan_repeat(lease_key, Some(&context)),
            RepeatPlan::Forwarded(10)
        );
        assert!(leases.remove_forwarded(&lease_key).is_some());
    }

    #[test]
    fn forwarded_semantic_generated_text_has_no_release_lease() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()));
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();

        leases.complete_press(
            lease_key,
            &key,
            Some(&context),
            Some(&context),
            Some(10),
            HostKeyboardInputMode::default(),
        );
        assert_eq!(leases.remove_forwarded(&lease_key), None);
    }

    #[test]
    fn leases_follow_only_releases_the_host_mode_will_report() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()));
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();

        // Plain text commits have no release with event-types alone.
        leases.prepare_press(&lease_key, &key);
        leases.complete_press(
            lease_key,
            &key,
            Some(&context),
            Some(&context),
            Some(10),
            KITTY_EVENT_TYPES,
        );
        assert!(!leases.contains(&lease_key));

        // Report-all makes even a text key's release available.
        leases.prepare_press(&lease_key, &key);
        leases.complete_press(
            lease_key,
            &key,
            Some(&context),
            Some(&context),
            Some(10),
            KITTY_REPORT_ALL,
        );
        assert_eq!(
            leases.plan_repeat(lease_key, Some(&context)),
            RepeatPlan::Forwarded(10)
        );
        // A fresh text press in report-all mode replaces the old lease.
        leases.prepare_press(&lease_key, &key);
        assert!(!leases.contains(&lease_key));

        // A fresh text press supersedes the prior report-all lease, even if
        // the host mode now sends the commit as plain text.
        leases.complete_press(
            lease_key,
            &key,
            Some(&context),
            Some(&context),
            Some(10),
            KITTY_EVENT_TYPES,
        );
        assert!(!leases.contains(&lease_key));
    }

    #[test]
    fn legacy_hosts_and_unmodified_compatibility_keys_do_not_take_leases() {
        for key in [
            TerminalKey::new(KeyCode::Left, KeyModifiers::empty()),
            TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
            TerminalKey::new(KeyCode::Tab, KeyModifiers::empty()),
            TerminalKey::new(KeyCode::Backspace, KeyModifiers::empty()),
        ] {
            assert!(!press_takes_lease(&key, HostKeyboardInputMode::default()));
        }

        for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::Backspace] {
            let key = TerminalKey::new(code, KeyModifiers::empty());
            assert!(!press_takes_lease(&key, KITTY_EVENT_TYPES));
            assert!(press_takes_lease(&key, KITTY_REPORT_ALL));
            assert!(!press_takes_lease(
                &key,
                HostKeyboardInputMode {
                    reports_event_types: false,
                    reports_all_keys: true,
                }
            ));
        }

        assert!(press_takes_lease(
            &TerminalKey::new(KeyCode::Left, KeyModifiers::empty()),
            KITTY_EVENT_TYPES
        ));
        assert!(press_takes_lease(
            &TerminalKey::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KITTY_EVENT_TYPES
        ));
    }

    #[test]
    fn new_semantic_press_recomputes_consumed_repeat_disposition() {
        let key = TerminalKey::new(KeyCode::Esc, KeyModifiers::empty());
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();
        leases.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);

        leases.prepare_press(&lease_key, &key);
        leases.complete_press(
            lease_key,
            &key,
            Some(&context),
            Some(&context),
            None,
            KITTY_EVENT_TYPES,
        );
        assert_eq!(
            leases.plan_repeat(lease_key, Some(&context)),
            RepeatPlan::Reprocess
        );
    }

    #[test]
    fn consumed_repeats_stop_for_good_once_the_context_changes() {
        let key = TerminalKey::new(KeyCode::Esc, KeyModifiers::empty());
        let lease_key = InputLeaseKey::new(7, &key);
        let mut leases = Leases::default();

        leases.complete_press(
            lease_key,
            &key,
            Some(&Context::Pane),
            Some(&Context::Pane),
            None,
            KITTY_EVENT_TYPES,
        );
        assert_eq!(
            leases.plan_repeat(lease_key, Some(&Context::Overlay)),
            RepeatPlan::Ignore
        );
        assert_eq!(
            leases.plan_repeat(lease_key, Some(&Context::Pane)),
            RepeatPlan::Ignore
        );
    }

    #[test]
    fn a_press_that_changes_the_context_suppresses_its_repeats() {
        let key = TerminalKey::new(KeyCode::Esc, KeyModifiers::empty());
        let lease_key = InputLeaseKey::new(7, &key);
        let mut leases = Leases::default();

        leases.complete_press(
            lease_key,
            &key,
            Some(&Context::Pane),
            Some(&Context::Overlay),
            None,
            KITTY_EVENT_TYPES,
        );
        assert_eq!(
            leases.plan_repeat(lease_key, Some(&Context::Overlay)),
            RepeatPlan::Ignore
        );
    }

    #[test]
    fn lease_keys_follow_the_key_code_not_its_modifiers() {
        let plain = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty());
        let chord = TerminalKey::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let other = TerminalKey::new(KeyCode::Char('b'), KeyModifiers::empty());

        assert_eq!(InputLeaseKey::new(7, &plain), InputLeaseKey::new(7, &chord));
        assert_ne!(InputLeaseKey::new(7, &plain), InputLeaseKey::new(7, &other));
        assert_ne!(InputLeaseKey::new(7, &plain), InputLeaseKey::new(8, &plain));
    }
}
