use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataToken {
    value: String,
    expires_at: Option<Instant>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MetadataTokens {
    entries: HashMap<String, MetadataToken>,
}

pub(crate) const MAX_SEQUENCE_SOURCES: usize = 32;

pub(crate) fn sequence_is_fresh(
    sequences: &HashMap<String, u64>,
    source: &str,
    seq: Option<u64>,
) -> bool {
    seq.is_none_or(|seq| sequences.get(source).is_none_or(|last| seq > *last))
}

pub(crate) fn accept_sequence(
    sequences: &mut HashMap<String, u64>,
    source: &str,
    seq: Option<u64>,
) -> Result<bool, ()> {
    let Some(seq) = seq else {
        return Ok(true);
    };
    if !sequence_is_fresh(sequences, source, Some(seq)) {
        return Ok(false);
    }
    if !sequences.contains_key(source) && sequences.len() >= MAX_SEQUENCE_SOURCES {
        return Err(());
    }
    sequences.insert(source.to_string(), seq);
    Ok(true)
}

/// Same-boot handoff deadlines. None means permanent, never an unreadable clock.
#[cfg(unix)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffToken {
    value: String,
    expires_at: Option<Duration>,
}

impl MetadataTokens {
    #[cfg(unix)]
    pub(crate) fn capture_handoff(
        &self,
        now: Instant,
        monotonic: Option<Duration>,
    ) -> HashMap<String, HandoffToken> {
        self.entries
            .iter()
            .filter_map(|(key, token)| {
                let expires_at = match token.expires_at {
                    None => None,
                    Some(deadline) => {
                        Some(monotonic?.checked_add(deadline.checked_duration_since(now)?)?)
                    }
                };
                Some((
                    key.clone(),
                    HandoffToken {
                        value: token.value.clone(),
                        expires_at,
                    },
                ))
            })
            .collect()
    }

    #[cfg(unix)]
    pub(crate) fn restore_handoff(
        tokens: HashMap<String, HandoffToken>,
        now: Instant,
        monotonic: Option<Duration>,
    ) -> Self {
        let entries = tokens
            .into_iter()
            .filter_map(|(key, token)| {
                let expires_at = match token.expires_at {
                    None => None,
                    Some(deadline) => {
                        let remaining = deadline.checked_sub(monotonic?)?;
                        if remaining.is_zero() {
                            return None;
                        }
                        Some(now.checked_add(remaining)?)
                    }
                };
                Some((
                    key,
                    MetadataToken {
                        value: token.value,
                        expires_at,
                    },
                ))
            })
            .collect();
        Self { entries }
    }

    pub(crate) fn patch(
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

    pub(crate) fn key_count_after_patch(&self, patch: &HashMap<String, Option<String>>) -> usize {
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

    pub(crate) fn values(&self) -> HashMap<String, String> {
        self.entries
            .iter()
            .map(|(key, token)| (key.clone(), token.value.clone()))
            .collect()
    }

    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|token| token.expires_at)
            .min()
    }

    pub(crate) fn expire_at(&mut self, now: Instant) -> bool {
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

    #[cfg(unix)]
    #[test]
    fn handoff_tokens_count_transfer_time_and_never_revive_expired_tokens() {
        let now = Instant::now();
        let monotonic = Duration::from_secs(100);
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("short", Some("one"))]),
            Some(Duration::from_secs(2)),
            now,
        );
        tokens.patch(
            patch(&[("long", Some("two"))]),
            Some(Duration::from_secs(10)),
            now,
        );
        tokens.patch(patch(&[("permanent", Some("three"))]), None, now);
        let transfer = tokens.capture_handoff(now, Some(monotonic));
        let json = serde_json::to_string(&transfer).unwrap();
        let transfer = serde_json::from_str(&json).unwrap();
        let restored = MetadataTokens::restore_handoff(
            transfer,
            now,
            Some(monotonic + Duration::from_secs(3)),
        );
        assert!(!restored.values().contains_key("short"));
        assert!(restored.values().contains_key("permanent"));
        assert_eq!(restored.next_expiry(), Some(now + Duration::from_secs(7)));
    }

    #[cfg(unix)]
    #[test]
    fn handoff_clock_rollback_does_not_revive_expired_token() {
        let now = Instant::now();
        let monotonic = Duration::from_secs(100);
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("short", Some("one"))]),
            Some(Duration::from_secs(2)),
            now,
        );
        // A wall-clock rollback cannot enter the new transfer contract.
        let transfer = tokens.capture_handoff(now, Some(monotonic));
        let restored = MetadataTokens::restore_handoff(
            transfer,
            now + Duration::from_secs(3),
            Some(monotonic + Duration::from_secs(3)),
        );
        assert!(!restored.values().contains_key("short"));
    }

    #[cfg(unix)]
    #[test]
    fn handoff_unavailable_clock_and_overflow_drop_only_expiring_tokens() {
        let now = Instant::now();
        let mut tokens = MetadataTokens::default();
        tokens.patch(
            patch(&[("temporary", Some("one"))]),
            Some(Duration::from_secs(2)),
            now,
        );
        tokens.patch(patch(&[("permanent", Some("two"))]), None, now);
        for clock in [None, Some(Duration::MAX)] {
            let transfer = tokens.capture_handoff(now, clock);
            assert_eq!(transfer.len(), 1);
            assert!(transfer.contains_key("permanent"));
        }
        let transfer = tokens.capture_handoff(now, Some(Duration::from_secs(100)));
        let restored = MetadataTokens::restore_handoff(transfer, now, None);
        assert!(!restored.values().contains_key("temporary"));
        assert!(restored.values().contains_key("permanent"));
    }

    #[cfg(unix)]
    #[test]
    fn handoff_rejects_wall_clock_deadlines_instead_of_making_them_permanent() {
        let legacy = serde_json::json!({
            "value": "temporary",
            "expires_at": {"secs_since_epoch": 100, "nanos_since_epoch": 0}
        });
        assert!(serde_json::from_value::<HandoffToken>(legacy).is_err());
    }

    #[test]
    fn sequence_sources_are_bounded() {
        let mut sequences = HashMap::new();
        for index in 0..MAX_SEQUENCE_SOURCES {
            assert_eq!(
                accept_sequence(&mut sequences, &format!("source-{index}"), Some(1)),
                Ok(true)
            );
        }
        assert_eq!(
            accept_sequence(&mut sequences, "one-too-many", Some(1)),
            Err(())
        );
        assert_eq!(
            accept_sequence(&mut sequences, "source-0", Some(1)),
            Ok(false)
        );
        assert_eq!(
            accept_sequence(&mut sequences, "source-0", Some(2)),
            Ok(true)
        );
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
