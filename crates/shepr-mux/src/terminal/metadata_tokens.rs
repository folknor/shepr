use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataToken {
    value: String,
    expires_at: Option<Instant>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataTokens {
    entries: HashMap<String, MetadataToken>,
}

pub const MAX_SEQUENCE_SOURCES: usize = 32;

/// Per-source report sequences of one resource's metadata tokens (a
/// workspace's), with when each was accepted.
pub type SequenceMarks = HashMap<String, crate::terminal::state::MetadataReportSeq>;

/// Whether a report carrying `seq` from `source`, arriving at `now`, is not
/// older than the source's last accepted one. Ordered by the same rule as
/// pane hook and metadata reports
/// (`crate::terminal::state::report_seq_superseded`): reporters take seqs
/// from their own wall clock, so a non-increasing seq long after the last
/// acceptance is a clock step and re-anchors the source instead of dropping
/// every report until the clock catches up.
pub fn sequence_is_fresh(
    sequences: &SequenceMarks,
    source: &str,
    seq: Option<u64>,
    now: Instant,
) -> bool {
    seq.is_none_or(|seq| {
        sequences
            .get(source)
            .is_none_or(|last| !last.supersedes(seq, now))
    })
}

// Callers distinguish capacity exhaustion from stale reports with this small result.
#[allow(clippy::result_unit_err)]
pub fn accept_sequence(
    sequences: &mut SequenceMarks,
    source: &str,
    seq: Option<u64>,
    now: Instant,
) -> Result<bool, ()> {
    let Some(seq) = seq else {
        return Ok(true);
    };
    if !sequence_is_fresh(sequences, source, Some(seq), now) {
        return Ok(false);
    }
    if !sequences.contains_key(source) && sequences.len() >= MAX_SEQUENCE_SOURCES {
        return Err(());
    }
    sequences.insert(
        source.to_string(),
        crate::terminal::state::MetadataReportSeq {
            seq,
            accepted_at: now,
        },
    );
    Ok(true)
}

impl MetadataTokens {
    pub fn patch(
        &mut self,
        patch: HashMap<String, Option<String>>,
        ttl: Option<Duration>,
        now: Instant,
    ) -> bool {
        let expires_at = ttl.and_then(|ttl| now.checked_add(ttl));
        let mut changed = false;
        for (key, value) in patch {
            match value {
                Some(value) => {
                    let token = MetadataToken { value, expires_at };
                    if self.entries.get(&key) != Some(&token) {
                        self.entries.insert(key, token);
                        changed = true;
                    }
                }
                None => {
                    changed |= self.entries.remove(&key).is_some();
                }
            }
        }
        changed
    }

    pub fn key_count_after_patch(&self, patch: &HashMap<String, Option<String>>) -> usize {
        let mut keys = self
            .values()
            .into_keys()
            .collect::<std::collections::HashSet<_>>();
        for (key, value) in patch {
            if value.is_some() {
                keys.insert(key.clone());
            } else {
                keys.remove(key);
            }
        }
        keys.len()
    }

    pub fn values(&self) -> HashMap<String, String> {
        self.entries
            .iter()
            .map(|(key, token)| (key.clone(), token.value.clone()))
            .collect()
    }

    pub fn next_expiry(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|token| token.expires_at)
            .min()
    }

    pub fn expire_at(&mut self, now: Instant) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|_, token| token.expires_at.is_none_or(|deadline| deadline > now));
        self.entries.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(items: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        items
            .iter()
            .map(|(key, value)| ((*key).into(), value.map(str::to_string)))
            .collect()
    }

    #[test]
    fn sequence_sources_are_bounded() {
        let now = Instant::now();
        let mut sequences = SequenceMarks::new();
        for index in 0..MAX_SEQUENCE_SOURCES {
            assert_eq!(
                accept_sequence(&mut sequences, &format!("source-{index}"), Some(1), now),
                Ok(true)
            );
        }
        assert_eq!(
            accept_sequence(&mut sequences, "one-too-many", Some(1), now),
            Err(())
        );
        assert_eq!(
            accept_sequence(&mut sequences, "source-0", Some(1), now),
            Ok(false)
        );
        assert_eq!(
            accept_sequence(&mut sequences, "source-0", Some(2), now),
            Ok(true)
        );
    }

    #[test]
    fn sequence_reanchors_after_a_backwards_clock_step() {
        let start = Instant::now();
        let mut sequences = SequenceMarks::new();
        assert_eq!(
            accept_sequence(&mut sequences, "git", Some(1_000), start),
            Ok(true)
        );
        // A racing straggler moments later is still dropped.
        let soon = start + Duration::from_millis(50);
        assert!(!sequence_is_fresh(&sequences, "git", Some(900), soon));
        assert_eq!(
            accept_sequence(&mut sequences, "git", Some(900), soon),
            Ok(false)
        );
        // Long after the last acceptance, a lower seq means the reporter's
        // clock stepped back: it is accepted and becomes the new baseline.
        let later = start + crate::terminal::state::HOOK_SEQUENCE_REANCHOR_AFTER;
        assert!(sequence_is_fresh(&sequences, "git", Some(10), later));
        assert_eq!(
            accept_sequence(&mut sequences, "git", Some(10), later),
            Ok(true)
        );
        assert_eq!(
            accept_sequence(
                &mut sequences,
                "git",
                Some(11),
                later + Duration::from_millis(1)
            ),
            Ok(true)
        );
        assert!(!sequence_is_fresh(
            &sequences,
            "git",
            Some(10),
            later + Duration::from_millis(2)
        ));
    }

    #[test]
    fn patches_and_clears_individual_keys() {
        let now = Instant::now();
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("summary", Some("one")), ("model", Some("opus"))]),
            None,
            now,
        );
        tokens.patch(
            patch(&[("summary", Some("two")), ("model", None)]),
            None,
            now,
        );

        assert_eq!(
            tokens.values(),
            HashMap::from([("summary".into(), "two".into())])
        );
    }

    #[test]
    fn ttl_only_changes_keys_in_the_patch() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("short", Some("one"))]),
            Some(Duration::from_secs(1)),
            now,
        );
        tokens.patch(patch(&[("persistent", Some("two"))]), None, now);

        assert!(tokens.expire_at(deadline));
        assert_eq!(
            tokens.values(),
            HashMap::from([("persistent".into(), "two".into())])
        );
    }

    #[test]
    fn values_remain_stable_until_expiry_mutates_state() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("summary", Some("temporary"))]),
            Some(Duration::from_secs(1)),
            now,
        );

        assert_eq!(
            tokens.values(),
            HashMap::from([("summary".into(), "temporary".into())])
        );
        assert!(tokens.expire_at(deadline));
        assert!(tokens.values().is_empty());
    }

    #[test]
    fn delayed_expiry_sweep_removes_every_token_due_at_now() {
        let now = Instant::now();
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("first", Some("one"))]),
            Some(Duration::from_secs(1)),
            now,
        );
        tokens.patch(
            patch(&[("second", Some("two"))]),
            Some(Duration::from_secs(2)),
            now,
        );

        assert!(tokens.expire_at(now + Duration::from_secs(10)));
        assert!(tokens.values().is_empty());
    }

    #[test]
    fn stale_expiry_does_not_clear_replacement() {
        let now = Instant::now();
        let first_deadline = now + Duration::from_secs(1);
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("summary", Some("old"))]),
            Some(Duration::from_secs(1)),
            now,
        );
        tokens.patch(
            patch(&[("summary", Some("new"))]),
            Some(Duration::from_secs(5)),
            now,
        );

        assert!(!tokens.expire_at(first_deadline));
        assert_eq!(
            tokens.values(),
            HashMap::from([("summary".into(), "new".into())])
        );
    }

    #[test]
    fn update_without_ttl_cancels_previous_expiry() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(1);
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("summary", Some("temporary"))]),
            Some(Duration::from_secs(1)),
            now,
        );
        tokens.patch(patch(&[("summary", Some("persistent"))]), None, now);

        assert!(!tokens.expire_at(deadline));
        assert_eq!(
            tokens.values(),
            HashMap::from([("summary".into(), "persistent".into())])
        );
    }
}
