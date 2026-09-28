//! In-memory acknowledgement state for bounded native-session attention.

use serde::Serialize;
use std::collections::HashMap;
use std::fmt;

const MAX_KNOWN_IDS: usize = 64;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AttentionState {
    pub revision: String,
    pub acknowledged: bool,
    pub pending: bool,
    pub available: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckError {
    WrongEpoch,
    StaleRevision,
    NotFound,
    ObservationUnavailable,
    NoAttention,
}

impl AckError {
    pub fn code(self) -> &'static str {
        match self {
            Self::WrongEpoch => "wrong_epoch",
            Self::StaleRevision => "stale_revision",
            Self::NotFound => "not_found",
            Self::ObservationUnavailable => "observation_unavailable",
            Self::NoAttention => "no_attention",
        }
    }
}

impl fmt::Display for AckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for AckError {}

struct Entry {
    revision: u64,
    source_epoch: Option<u64>,
    pending_generation: Option<u64>,
    pending_keys: Vec<String>,
    acknowledged: bool,
    pending: bool,
    available: bool,
}

impl Entry {
    fn unavailable() -> Self {
        Self {
            revision: 0,
            source_epoch: None,
            pending_generation: None,
            pending_keys: Vec::new(),
            acknowledged: false,
            pending: false,
            available: false,
        }
    }

    fn state(&self) -> AttentionState {
        AttentionState {
            revision: self.revision.to_string(),
            acknowledged: self.acknowledged,
            pending: self.pending,
            available: self.available,
        }
    }
}

/// A daemon-lifetime acknowledgement book for configured observation IDs.
///
/// Callers must only reconcile the at most 64 globally distinct IDs accepted by
/// observation configuration validation.
pub struct AttentionBook {
    epoch: String,
    entries: HashMap<String, Entry>,
}

impl AttentionBook {
    pub fn new(epoch: String) -> Self {
        Self {
            epoch,
            entries: HashMap::new(),
        }
    }

    pub fn reconcile(
        &mut self,
        id: &str,
        source_epoch: u64,
        pending_generation: u64,
        fresh: bool,
        pending_keys: &[String],
    ) -> AttentionState {
        if !self.entries.contains_key(id) {
            assert!(
                self.entries.len() < MAX_KNOWN_IDS,
                "attention book exceeds configured observation limit"
            );
            self.entries.insert(id.to_owned(), Entry::unavailable());
        }
        let entry = self.entries.get_mut(id).expect("entry was just inserted");

        if !fresh {
            entry.available = false;
            entry.pending = false;
            return entry.state();
        }

        let mut canonical = pending_keys.to_vec();
        canonical.sort_unstable();
        canonical.dedup();
        let changed = entry.source_epoch != Some(source_epoch)
            || entry.pending_generation != Some(pending_generation)
            || entry.pending_keys != canonical;
        if changed {
            entry.revision = entry
                .revision
                .checked_add(1)
                .expect("attention revision exhausted");
            entry.source_epoch = Some(source_epoch);
            entry.pending_generation = Some(pending_generation);
            entry.pending_keys = canonical;
            entry.acknowledged = false;
        }
        entry.pending = !entry.pending_keys.is_empty();
        entry.available = true;
        entry.state()
    }

    pub fn state(&self, id: &str) -> Option<AttentionState> {
        self.entries.get(id).map(Entry::state)
    }

    pub fn acknowledge(
        &mut self,
        id: &str,
        epoch: &str,
        revision: &str,
    ) -> Result<AttentionState, AckError> {
        if epoch != self.epoch {
            return Err(AckError::WrongEpoch);
        }
        let entry = self.entries.get_mut(id).ok_or(AckError::NotFound)?;
        if revision != entry.revision.to_string() {
            return Err(AckError::StaleRevision);
        }
        if !entry.available {
            return Err(AckError::ObservationUnavailable);
        }
        if !entry.pending {
            return Err(AckError::NoAttention);
        }
        entry.acknowledged = true;
        Ok(entry.state())
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn exact_full_set_is_order_independent_and_includes_ids_after_public_limit() {
        let mut book = AttentionBook::new("daemon".into());
        let mut first: Vec<String> = (0..17)
            .map(|index| format!("permission:per_{index:02}"))
            .collect();
        let initial = book.reconcile("agent", 1, 1, true, &first);

        first.reverse();
        let reordered = book.reconcile("agent", 1, 1, true, &first);
        assert_eq!(reordered.revision, initial.revision);

        first[0] = "permission:per_replacement".into();
        let changed = book.reconcile("agent", 1, 1, true, &first);
        assert_ne!(changed.revision, initial.revision);
    }

    #[test]
    fn unavailable_preserves_identity_and_ack_until_a_real_reconnect() {
        let mut book = AttentionBook::new("daemon".into());
        let pending = keys(&["question:que_one"]);
        let current = book.reconcile("agent", 7, 1, true, &pending);
        let acknowledged = book
            .acknowledge("agent", "daemon", &current.revision)
            .unwrap();
        assert!(acknowledged.acknowledged);

        let unavailable = book.reconcile("agent", 7, 1, false, &[]);
        assert!(!unavailable.available);
        assert!(!unavailable.pending);
        assert!(unavailable.acknowledged);
        assert_eq!(
            book.acknowledge("agent", "daemon", &unavailable.revision),
            Err(AckError::ObservationUnavailable)
        );

        let restored = book.reconcile("agent", 7, 1, true, &pending);
        assert_eq!(restored.revision, current.revision);
        assert!(restored.acknowledged);

        let reconnected = book.reconcile("agent", 8, 1, true, &pending);
        assert_ne!(reconnected.revision, current.revision);
        assert!(!reconnected.acknowledged);
    }

    #[test]
    fn stale_ack_is_rejected_and_repeated_current_ack_is_idempotent() {
        let mut book = AttentionBook::new("daemon".into());
        let first = book.reconcile("agent", 1, 1, true, &keys(&["permission:per_one"]));
        let second = book.reconcile("agent", 1, 2, true, &keys(&["permission:per_two"]));

        assert_eq!(
            book.acknowledge("agent", "daemon", &first.revision),
            Err(AckError::StaleRevision)
        );
        let acknowledged = book
            .acknowledge("agent", "daemon", &second.revision)
            .unwrap();
        assert!(acknowledged.acknowledged);
        assert_eq!(
            book.acknowledge("agent", "daemon", &second.revision),
            Ok(acknowledged)
        );
    }

    #[test]
    fn resolved_then_reappearing_attention_gets_a_new_revision() {
        let mut book = AttentionBook::new("daemon".into());
        let pending = keys(&["permission:per_one"]);
        let first = book.reconcile("agent", 1, 1, true, &pending);
        book.acknowledge("agent", "daemon", &first.revision)
            .unwrap();

        let resolved = book.reconcile("agent", 1, 2, true, &[]);
        assert!(!resolved.pending);
        assert_eq!(
            book.acknowledge("agent", "daemon", &resolved.revision),
            Err(AckError::NoAttention)
        );
        let reappeared = book.reconcile("agent", 1, 3, true, &pending);
        assert_ne!(reappeared.revision, first.revision);
        assert_ne!(reappeared.revision, resolved.revision);
        assert!(!reappeared.acknowledged);
    }

    #[test]
    fn producer_generation_detects_coalesced_aba_and_old_daemon_epoch_is_rejected() {
        let mut book = AttentionBook::new("new-daemon".into());
        let pending = keys(&["question:que_one"]);
        let first = book.reconcile("agent", 1, 1, true, &pending);
        book.acknowledge("agent", "new-daemon", &first.revision)
            .unwrap();

        let after_aba = book.reconcile("agent", 1, 3, true, &pending);
        assert_ne!(after_aba.revision, first.revision);
        assert!(!after_aba.acknowledged);
        assert_eq!(
            book.acknowledge("agent", "old-daemon", &after_aba.revision),
            Err(AckError::WrongEpoch)
        );
    }
}
