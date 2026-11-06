use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::{AccountPlacementRuntimeStatus, PoolPlacementRuntimeStatus};
use crate::{AccountPlacementLoad, ModelFamily, SessionAssignmentReason};

const MAX_AFFINITY_ENTRIES: usize = 4096;

#[derive(Default)]
pub(crate) struct PlacementState {
    pub(crate) primaries: HashMap<String, AffinityEntry>,
    model_overrides: HashMap<AffinityKey, AffinityEntry>,
    pub(crate) active_leases: HashMap<AffinityKey, ActiveLease>,
    rollback_leases: HashMap<AffinityKey, ActiveLease>,
    sequence: u64,
}

#[derive(Clone)]
pub(crate) struct AffinityEntry {
    pub(crate) account_id: String,
    last_seen_sequence: u64,
    last_seen: Instant,
}

#[derive(Clone)]
pub(crate) struct ActiveLease {
    pub(crate) account_id: String,
    last_seen: Instant,
    pub(crate) assigned_at_unix: i64,
    pub(crate) last_seen_at_unix: i64,
    generation: u64,
    provisional: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AffinityKey {
    pub(crate) launch_id: String,
    pub(crate) model: ModelFamily,
}

pub(crate) struct ReservationTicket {
    key: AffinityKey,
    account_id: String,
    generation: u64,
    previous: Option<ActiveLease>,
    reason: SessionAssignmentReason,
}

pub(crate) struct PlacementReservation {
    state: Arc<Mutex<PlacementState>>,
    ticket: Option<ReservationTicket>,
    reason: SessionAssignmentReason,
}

impl PlacementReservation {
    pub(crate) fn new(state: Arc<Mutex<PlacementState>>, ticket: ReservationTicket) -> Self {
        let reason = ticket.reason;
        Self {
            state,
            ticket: Some(ticket),
            reason,
        }
    }

    pub(crate) fn assignment_reason(&self) -> SessionAssignmentReason {
        self.reason
    }

    pub(crate) fn accept(&mut self, now: Instant) -> Option<SessionAssignmentReason> {
        let ticket = self.ticket.take()?;
        let mut state = self
            .state
            .lock()
            .expect("subscription placement lock poisoned");
        if !state.is_current(&ticket) {
            return None;
        }
        let reason = state.commit(&ticket.key, &ticket.account_id, now);
        self.reason = reason;
        Some(reason)
    }
}

impl Drop for PlacementReservation {
    fn drop(&mut self) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .expect("subscription placement lock poisoned");
        if state.is_current(&ticket) {
            state.rollback_leases.remove(&ticket.key);
            match ticket.previous {
                Some(previous) => {
                    state.active_leases.insert(ticket.key, previous);
                }
                None => {
                    state.active_leases.remove(&ticket.key);
                }
            }
        }
    }
}

impl PlacementState {
    fn is_current(&self, ticket: &ReservationTicket) -> bool {
        self.active_leases
            .get(&ticket.key)
            .is_some_and(|lease| lease.generation == ticket.generation)
    }

    pub(crate) fn preferred_account(&self, key: &AffinityKey) -> Option<&str> {
        self.active_leases
            .get(key)
            .filter(|lease| !lease.provisional)
            .map(|lease| lease.account_id.as_str())
            .or_else(|| {
                self.model_overrides
                    .get(key)
                    .map(|entry| entry.account_id.as_str())
            })
            .or_else(|| {
                self.primaries
                    .get(&key.launch_id)
                    .map(|entry| entry.account_id.as_str())
            })
    }

    pub(crate) fn affinity_warm(&self, key: &AffinityKey, now: Instant, ttl: Duration) -> bool {
        let last_seen = self
            .model_overrides
            .get(key)
            .or_else(|| self.primaries.get(&key.launch_id))
            .map(|entry| entry.last_seen);
        last_seen.is_some_and(|seen| now.saturating_duration_since(seen) <= ttl)
    }

    pub(crate) fn prune_active_leases(&mut self, now: Instant, ttl: Duration) {
        self.active_leases
            .retain(|_, lease| now.saturating_duration_since(lease.last_seen) <= ttl);
        self.rollback_leases
            .retain(|key, _| self.active_leases.contains_key(key));
    }

    pub(crate) fn loads_for(
        &self,
        current: &AffinityKey,
    ) -> BTreeMap<String, AccountPlacementLoad> {
        self.collect_loads(&current.model, Some(current))
    }

    pub(crate) fn status_load_for(
        &self,
        current: &AffinityKey,
        account_id: &str,
    ) -> AccountPlacementLoad {
        self.collect_loads(&current.model, None)
            .remove(account_id)
            .unwrap_or_default()
    }

    fn collect_loads(
        &self,
        model: &ModelFamily,
        excluded: Option<&AffinityKey>,
    ) -> BTreeMap<String, AccountPlacementLoad> {
        let mut loads = BTreeMap::<String, AccountPlacementLoad>::new();
        let mut global = HashSet::new();
        for (key, lease) in &self.active_leases {
            if excluded == Some(key) {
                continue;
            }
            global.insert((&key.launch_id, &lease.account_id));
            if key.model == *model {
                loads
                    .entry(lease.account_id.clone())
                    .or_default()
                    .active_model_leases += 1;
            }
        }
        for (_, account_id) in global {
            loads
                .entry(account_id.clone())
                .or_default()
                .active_global_leases += 1;
        }
        loads
    }

    pub(crate) fn reserve(
        &mut self,
        key: AffinityKey,
        account_id: String,
        now: Instant,
    ) -> ReservationTicket {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("placement generation exhausted");
        let generation = self.sequence;
        // Only accepted leases are restorable: a superseded attempt must never
        // become active again when a newer request is cancelled.
        let previous = self
            .active_leases
            .get(&key)
            .filter(|lease| !lease.provisional)
            .cloned()
            .or_else(|| self.rollback_leases.get(&key).cloned());
        if let Some(previous) = previous.as_ref() {
            self.rollback_leases.insert(key.clone(), previous.clone());
        }
        let reason = self.reason_for(&key, &account_id);
        let now_unix = crate::oauth::unix_now_ms() / 1000;
        let assigned_at_unix = previous
            .as_ref()
            .filter(|lease| lease.account_id == account_id)
            .map(|lease| lease.assigned_at_unix)
            .unwrap_or(now_unix);
        self.active_leases.insert(
            key.clone(),
            ActiveLease {
                account_id: account_id.clone(),
                last_seen: now,
                assigned_at_unix,
                last_seen_at_unix: now_unix,
                generation,
                provisional: true,
            },
        );
        while self.active_leases.len() > MAX_AFFINITY_ENTRIES {
            let oldest = self
                .active_leases
                .iter()
                .min_by_key(|(_, lease)| lease.generation)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.active_leases.remove(&oldest);
                self.rollback_leases.remove(&oldest);
            }
        }
        ReservationTicket {
            key,
            account_id,
            generation,
            previous,
            reason,
        }
    }

    fn reason_for(&self, key: &AffinityKey, account_id: &str) -> SessionAssignmentReason {
        match self.primaries.get(&key.launch_id) {
            None => SessionAssignmentReason::BalancedNewLaunch,
            Some(primary) if primary.account_id == account_id => {
                SessionAssignmentReason::PrimaryAffinity
            }
            Some(_) => SessionAssignmentReason::ModelOverride,
        }
    }

    fn commit(
        &mut self,
        key: &AffinityKey,
        account_id: &str,
        now: Instant,
    ) -> SessionAssignmentReason {
        let reason = self.reason_for(key, account_id);
        let entry = AffinityEntry {
            account_id: account_id.to_owned(),
            last_seen_sequence: self.sequence,
            last_seen: now,
        };
        match reason {
            SessionAssignmentReason::BalancedNewLaunch
            | SessionAssignmentReason::PrimaryAffinity => {
                self.primaries.insert(key.launch_id.clone(), entry);
                self.model_overrides.remove(key);
            }
            _ => {
                self.model_overrides.insert(key.clone(), entry);
            }
        }
        if let Some(lease) = self.active_leases.get_mut(key) {
            lease.provisional = false;
            lease.last_seen = now;
            lease.last_seen_at_unix = crate::oauth::unix_now_ms() / 1000;
        }
        self.rollback_leases.remove(key);
        self.enforce_affinity_bound();
        reason
    }

    pub(crate) fn clear_model_assignment(&mut self, key: &AffinityKey, account_id: &str) {
        if self
            .active_leases
            .get(key)
            .is_some_and(|lease| lease.account_id == account_id)
        {
            self.active_leases.remove(key);
        }
        if !self.active_leases.contains_key(key) {
            self.rollback_leases.remove(key);
        }
        if self
            .model_overrides
            .get(key)
            .is_some_and(|entry| entry.account_id == account_id)
        {
            self.model_overrides.remove(key);
        }
    }

    fn enforce_affinity_bound(&mut self) {
        while self.primaries.len() + self.model_overrides.len() > MAX_AFFINITY_ENTRIES {
            let primary = self
                .primaries
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen_sequence)
                .map(|(key, entry)| (key.clone(), entry.last_seen_sequence));
            let model = self
                .model_overrides
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen_sequence)
                .map(|(key, entry)| (key.clone(), entry.last_seen_sequence));
            match (primary, model) {
                (Some((key, seq)), Some((model, model_seq))) => {
                    if seq <= model_seq {
                        self.primaries.remove(&key);
                    } else {
                        self.model_overrides.remove(&model);
                    }
                }
                (Some((key, _)), None) => {
                    self.primaries.remove(&key);
                }
                (None, Some((key, _))) => {
                    self.model_overrides.remove(&key);
                }
                (None, None) => break,
            }
        }
    }

    pub(crate) fn runtime_status<'a>(
        &self,
        account_ids: impl Iterator<Item = &'a str>,
        active_lease_ttl: Duration,
    ) -> PoolPlacementRuntimeStatus {
        let mut accounts = account_ids
            .map(|id| {
                (
                    id.to_owned(),
                    AccountPlacementRuntimeStatus {
                        id: id.to_owned(),
                        active_launch_leases: 0,
                        active_model_leases: BTreeMap::new(),
                        primary_assignments: 0,
                        model_overrides: 0,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut global = HashSet::new();
        for (key, lease) in &self.active_leases {
            global.insert((&key.launch_id, &lease.account_id));
            let account = accounts.entry(lease.account_id.clone()).or_insert_with(|| {
                AccountPlacementRuntimeStatus {
                    id: lease.account_id.clone(),
                    active_launch_leases: 0,
                    active_model_leases: BTreeMap::new(),
                    primary_assignments: 0,
                    model_overrides: 0,
                }
            });
            *account
                .active_model_leases
                .entry(key.model.to_string())
                .or_default() += 1;
        }
        for (_, account_id) in global {
            if let Some(account) = accounts.get_mut(account_id) {
                account.active_launch_leases += 1;
            }
        }
        for entry in self.primaries.values() {
            if let Some(account) = accounts.get_mut(&entry.account_id) {
                account.primary_assignments += 1;
            }
        }
        for entry in self.model_overrides.values() {
            if let Some(account) = accounts.get_mut(&entry.account_id) {
                account.model_overrides += 1;
            }
        }
        PoolPlacementRuntimeStatus {
            active_lease_ttl_seconds: active_lease_ttl.as_secs(),
            accounts: accounts.into_values().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(launch: &str, model: &str) -> AffinityKey {
        AffinityKey {
            launch_id: launch.to_owned(),
            model: ModelFamily::from_requested_model(model),
        }
    }

    fn reserve(
        state: &Arc<Mutex<PlacementState>>,
        key: AffinityKey,
        account: &str,
        now: Instant,
    ) -> PlacementReservation {
        let ticket = state
            .lock()
            .expect("placement")
            .reserve(key, account.to_owned(), now);
        PlacementReservation::new(Arc::clone(state), ticket)
    }

    #[test]
    fn cancellation_releases_new_reservation_without_publishing_affinity() {
        let state = Arc::new(Mutex::new(PlacementState::default()));
        let key = key("launch", "sonnet");
        let reservation = reserve(&state, key.clone(), "a", Instant::now());
        assert_eq!(
            state.lock().expect("placement").preferred_account(&key),
            None
        );
        assert_eq!(
            state
                .lock()
                .expect("placement")
                .status_load_for(&key, "a")
                .active_model_leases,
            1
        );
        drop(reservation);
        assert_eq!(
            state.lock().expect("placement").preferred_account(&key),
            None
        );
        assert_eq!(
            state
                .lock()
                .expect("placement")
                .status_load_for(&key, "a")
                .active_model_leases,
            0
        );
    }

    #[test]
    fn cancellation_restores_prior_serving_lease_and_timestamps() {
        let state = Arc::new(Mutex::new(PlacementState::default()));
        let key = key("launch", "sonnet");
        let now = Instant::now();
        reserve(&state, key.clone(), "a", now).accept(now);
        let assigned = state.lock().expect("placement").active_leases[&key].assigned_at_unix;
        let failed = reserve(&state, key.clone(), "b", now + Duration::from_secs(5));
        drop(failed);
        let state = state.lock().expect("placement");
        assert_eq!(state.preferred_account(&key), Some("a"));
        assert_eq!(state.active_leases[&key].assigned_at_unix, assigned);
        assert_eq!(state.status_load_for(&key, "a").active_model_leases, 1);
        assert_eq!(state.status_load_for(&key, "b").active_model_leases, 0);
    }

    #[test]
    fn stale_accept_and_drop_never_undo_newer_same_account_activity() {
        let state = Arc::new(Mutex::new(PlacementState::default()));
        let key = key("launch", "sonnet");
        let now = Instant::now();
        let mut old = reserve(&state, key.clone(), "a", now);
        let mut newer = reserve(&state, key.clone(), "a", now);
        assert_eq!(old.accept(now), None);
        assert_eq!(
            newer.accept(now),
            Some(SessionAssignmentReason::BalancedNewLaunch)
        );
        let stale = reserve(&state, key.clone(), "a", now);
        reserve(&state, key.clone(), "a", now).accept(now);
        drop(stale);
        assert_eq!(
            state.lock().expect("placement").preferred_account(&key),
            Some("a")
        );
        assert_eq!(
            state
                .lock()
                .expect("placement")
                .status_load_for(&key, "a")
                .active_model_leases,
            1
        );
        let mut superseded = reserve(&state, key.clone(), "a", now);
        reserve(&state, key.clone(), "b", now).accept(now);
        assert_eq!(superseded.accept(now), None);
        assert_eq!(
            state.lock().expect("placement").preferred_account(&key),
            Some("b")
        );
        assert_eq!(
            state
                .lock()
                .expect("placement")
                .status_load_for(&key, "a")
                .active_model_leases,
            0
        );
    }

    #[test]
    fn overlapping_cancelled_attempts_restore_only_original_accepted_lease() {
        let state = Arc::new(Mutex::new(PlacementState::default()));
        let key = key("launch", "sonnet");
        let now = Instant::now();
        reserve(&state, key.clone(), "a", now).accept(now);
        let older = reserve(&state, key.clone(), "b", now);
        let newer = reserve(&state, key.clone(), "c", now);
        drop(older);
        drop(newer);
        let state = state.lock().expect("placement");
        assert_eq!(state.status_load_for(&key, "a").active_model_leases, 1);
        assert_eq!(state.status_load_for(&key, "b").active_model_leases, 0);
        assert_eq!(state.status_load_for(&key, "c").active_model_leases, 0);
    }

    #[test]
    fn placement_preserves_primary_model_overrides_and_counting() {
        let state = Arc::new(Mutex::new(PlacementState::default()));
        let now = Instant::now();
        let sonnet = key("launch", "sonnet");
        let fable = key("launch", "fable");
        assert_eq!(
            reserve(&state, sonnet.clone(), "a", now).accept(now),
            Some(SessionAssignmentReason::BalancedNewLaunch)
        );
        assert_eq!(
            reserve(&state, fable.clone(), "b", now).accept(now),
            Some(SessionAssignmentReason::ModelOverride)
        );
        reserve(&state, key("launch", "opus"), "a", now).accept(now);
        reserve(&state, key("second", "sonnet"), "a", now).accept(now);
        let mut state = state.lock().expect("placement");
        assert_eq!(state.preferred_account(&sonnet), Some("a"));
        assert_eq!(state.preferred_account(&fable), Some("b"));
        let status = state.runtime_status(["a", "b"].into_iter(), Duration::from_secs(900));
        let a = &status.accounts[0];
        assert_eq!(a.active_launch_leases, 2);
        assert_eq!(a.active_model_leases["sonnet"], 2);
        assert_eq!(a.primary_assignments, 2);
        assert_eq!(status.accounts[1].model_overrides, 1);
        state.clear_model_assignment(&fable, "b");
        assert_eq!(state.preferred_account(&fable), Some("a"));
    }

    #[test]
    fn lease_expiry_and_warmth_are_independent_and_attempts_do_not_extend_warmth() {
        let state = Arc::new(Mutex::new(PlacementState::default()));
        let now = Instant::now();
        let key = key("launch", "sonnet");
        reserve(&state, key.clone(), "a", now).accept(now);
        let failed = reserve(&state, key.clone(), "a", now + Duration::from_secs(3500));
        drop(failed);
        let mut state = state.lock().expect("placement");
        state.prune_active_leases(now + Duration::from_secs(901), Duration::from_secs(900));
        assert!(state.loads_for(&self::key("other", "sonnet")).is_empty());
        assert_eq!(state.preferred_account(&key), Some("a"));
        assert!(state.affinity_warm(
            &key,
            now + Duration::from_secs(3600),
            Duration::from_secs(3600)
        ));
        assert!(!state.affinity_warm(
            &key,
            now + Duration::from_secs(3601),
            Duration::from_secs(3600)
        ));
    }

    #[test]
    fn placement_tables_remain_bounded() {
        let state = Arc::new(Mutex::new(PlacementState::default()));
        let now = Instant::now();
        for index in 0..=MAX_AFFINITY_ENTRIES {
            reserve(&state, key(&format!("launch-{index}"), "sonnet"), "a", now).accept(now);
        }
        let state = state.lock().expect("placement");
        assert!(state.primaries.len() + state.model_overrides.len() <= MAX_AFFINITY_ENTRIES);
        assert!(state.active_leases.len() <= MAX_AFFINITY_ENTRIES);
    }
}
