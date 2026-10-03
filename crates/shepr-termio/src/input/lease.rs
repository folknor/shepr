use std::collections::HashMap;
use std::hash::Hash;

use crossterm::event::KeyCode;

use super::TerminalKey;

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

pub enum RepeatPlan<Context, Target> {
    Forwarded(Target),
    Reprocess {
        context: Context,
        repetitions: u16,
        tracked: bool,
    },
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

/// Whether a press can be followed by its repeats and release, and so holds
/// a lease. The host reports those for every key while it sends all keys as
/// escape codes (kitty REPORT_ALL_KEYS, which the client enables only when a
/// pane or the shell needs it, since it breaks IME and compose input).
/// Otherwise a key that committed text gets no release event, and a lease
/// for it would go stale.
fn press_takes_lease(key: &TerminalKey, host_reports_all_keys: bool) -> bool {
    key.generated_text.is_none() || host_reports_all_keys
}

impl<Source, Context, Target> InputLeaseTable<Source, Context, Target>
where
    Source: Copy + Eq + Hash,
    Context: Clone + Eq,
    Target: Clone + Eq,
{
    /// A fresh press of a key that takes a lease (see `press_takes_lease`)
    /// starts a new lease, dropping whatever the last press of that key left
    /// behind. Keys are semantic, so a second press cannot be told apart from
    /// a new one and is never turned into a repeat here.
    pub fn prepare_press(
        &mut self,
        lease_key: &InputLeaseKey<Source>,
        key: &TerminalKey,
        host_reports_all_keys: bool,
    ) {
        if key.kind == crossterm::event::KeyEventKind::Press
            && press_takes_lease(key, host_reports_all_keys)
        {
            self.leases.remove(lease_key);
        }
    }

    pub fn complete_press(
        &mut self,
        lease_key: InputLeaseKey<Source>,
        key: &TerminalKey,
        initial_context: Option<&Context>,
        resulting_context: Option<&Context>,
        target: Option<Target>,
        host_reports_all_keys: bool,
    ) -> RepeatPlan<Context, Target> {
        if !press_takes_lease(key, host_reports_all_keys) {
            return RepeatPlan::Ignore;
        }
        if let Some(target) = target {
            self.insert_forwarded(lease_key, target, key.clone());
            return RepeatPlan::Ignore;
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
        match self.leases.get(&lease_key) {
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(context)))
                if key.repeat_count > 1 =>
            {
                RepeatPlan::Reprocess {
                    context: context.clone(),
                    repetitions: key.repeat_count - 1,
                    tracked: true,
                }
            }
            _ => RepeatPlan::Ignore,
        }
    }

    pub fn plan_repeat(
        &mut self,
        lease_key: InputLeaseKey<Source>,
        key: &TerminalKey,
        current_context: Option<&Context>,
    ) -> RepeatPlan<Context, Target> {
        match self.leases.get(&lease_key) {
            Some(InputLease::Forwarded(lease)) => {
                return RepeatPlan::Forwarded(lease.target.clone());
            }
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(context)))
                if current_context == Some(context) =>
            {
                return RepeatPlan::Reprocess {
                    context: context.clone(),
                    repetitions: key.repeat_count,
                    tracked: true,
                };
            }
            Some(InputLease::Consumed(ConsumedInputLease::ReprocessRepeats(_))) => {
                self.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
                return RepeatPlan::Ignore;
            }
            Some(InputLease::Consumed(ConsumedInputLease::SuppressRepeats)) => {
                return RepeatPlan::Ignore;
            }
            None => {}
        }
        match current_context {
            Some(context) => RepeatPlan::Reprocess {
                context: context.clone(),
                repetitions: key.repeat_count,
                tracked: false,
            },
            None => RepeatPlan::Ignore,
        }
    }

    pub fn reprocess_allowed(
        &mut self,
        lease_key: InputLeaseKey<Source>,
        expected_context: &Context,
        current_context: Option<&Context>,
        tracked: bool,
    ) -> bool {
        let allowed = current_context == Some(expected_context);
        if tracked && !allowed {
            self.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
        }
        allowed
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
    }

    type Leases = InputLeaseTable<u64, Context, u64>;

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
        leases.prepare_press(&lease_key, &key, false);
        assert!(!leases.contains(&lease_key));

        leases.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);
        leases.prepare_press(&lease_key, &key, false);
        assert!(!leases.contains(&lease_key));
    }

    #[test]
    fn a_text_press_leaves_existing_leases_alone() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()));
        let lease_key = InputLeaseKey::new(7, &key);
        let mut leases = Leases::default();
        leases.insert_forwarded(lease_key, 10, key.clone());

        leases.prepare_press(&lease_key, &key, false);
        assert!(leases.contains(&lease_key));
    }

    #[test]
    fn forwarded_press_repeats_go_to_the_same_target() {
        let key = TerminalKey::new(KeyCode::Left, KeyModifiers::empty());
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();

        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&context),
                Some(&context),
                Some(10),
                false
            ),
            RepeatPlan::Ignore
        ));
        let repeated = key.with_kind(crossterm::event::KeyEventKind::Repeat);
        assert!(matches!(
            leases.plan_repeat(lease_key, &repeated, Some(&context)),
            RepeatPlan::Forwarded(10)
        ));
        assert!(leases.remove_forwarded(&lease_key).is_some());
    }

    #[test]
    fn forwarded_semantic_generated_text_has_no_release_lease() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()))
            .with_repeat_count(3);
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();

        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&context),
                Some(&context),
                Some(10),
                false
            ),
            RepeatPlan::Ignore
        ));
        assert_eq!(leases.remove_forwarded(&lease_key), None);
    }

    #[test]
    fn text_presses_take_leases_only_while_the_host_reports_all_keys() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()));
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();

        // Report-all mode sends the release: the press leases its target,
        // and the repeat and release follow it there.
        leases.prepare_press(&lease_key, &key, true);
        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&context),
                Some(&context),
                Some(10),
                true
            ),
            RepeatPlan::Ignore
        ));
        let repeated = key
            .clone()
            .with_kind(crossterm::event::KeyEventKind::Repeat);
        assert!(matches!(
            leases.plan_repeat(lease_key, &repeated, Some(&context)),
            RepeatPlan::Forwarded(10)
        ));
        // A fresh text press in report-all mode replaces the old lease.
        leases.prepare_press(&lease_key, &key, true);
        assert!(!leases.contains(&lease_key));

        // Without report-all no release follows, so no lease is taken.
        assert!(matches!(
            leases.complete_press(
                lease_key,
                &key,
                Some(&context),
                Some(&context),
                Some(10),
                false
            ),
            RepeatPlan::Ignore
        ));
        assert!(!leases.contains(&lease_key));
    }

    #[test]
    fn new_semantic_press_recomputes_consumed_repeat_disposition() {
        let key = TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()).with_repeat_count(3);
        let lease_key = InputLeaseKey::new(7, &key);
        let context = Context::Pane;
        let mut leases = Leases::default();
        leases.insert_consumed(lease_key, ConsumedInputLease::SuppressRepeats);

        leases.prepare_press(&lease_key, &key, false);
        assert!(matches!(
            leases.complete_press(lease_key, &key, Some(&context), Some(&context), None, false),
            RepeatPlan::Reprocess {
                context: Context::Pane,
                repetitions: 2,
                tracked: true,
            }
        ));
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
