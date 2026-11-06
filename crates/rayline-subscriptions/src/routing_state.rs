use serde::{Deserialize, Serialize};

use crate::limits::{
    model_scope_matches, normalize_claim_key, parse_reset_unix_seconds, persistent_model_scope,
};
use crate::{
    AccountLimitState, ClaimScope, ClaimStatus, Entitlement, ExtraUsageState, HeaderSnapshot,
    LimitClaim, LimitSource, ModelFamily, NormalizedUsage,
};

const RECOVERY_MS: u64 = 60_000;
const RANK_GRACE_MS: u64 = 5 * 60_000;
const AFFINITY_GRACE_MS: u64 = 15 * 60_000;

/// Identifies an operation at dispatch, not at completion. Contains no credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationToken {
    pub sequence: u64,
    pub credential_generation: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingBlockKind {
    Quota,
    Overage,
    Authentication,
    Entitlement,
}

/// Only normalized routing facts may be persisted; never provider text or credentials.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingBlock {
    pub kind: RoutingBlockKind,
    pub scope: ClaimScope,
    pub claim_key: Option<String>,
    pub rejected_sequence: u64,
    pub credential_generation: u64,
    pub observed_at_ms: u64,
    pub retry_at_ms: u64,
    pub resets_at_ms: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct RoutingState {
    pub limits: AccountLimitState,
    blocks: Vec<RoutingBlock>,
    recovery_revision: u64,
    healed_scopes: Vec<(RoutingBlockKind, ClaimScope, u64)>,
    sequence: u64,
    generation: u64,
    merged_sequence: u64,
    last_good_ms: Option<u64>,
    probe: Option<(u64, ModelFamily)>,
}

impl RoutingState {
    pub fn unknown(account_id: impl Into<String>) -> Self {
        Self {
            limits: AccountLimitState::unknown(account_id),
            blocks: Vec::new(),
            recovery_revision: 0,
            healed_scopes: Vec::new(),
            sequence: 0,
            generation: 0,
            merged_sequence: 0,
            last_good_ms: None,
            probe: None,
        }
    }

    pub fn recovery_revision(&self) -> u64 {
        self.recovery_revision
    }
    pub fn mark_recovery_dirty(&mut self) {
        self.recovery_revision = self.recovery_revision.saturating_add(1);
    }

    pub fn begin_observation(&mut self, credential_generation: u64) -> ObservationToken {
        if credential_generation > self.generation {
            self.generation = credential_generation;
            self.merged_sequence = 0;
            let previous_len = self.blocks.len();
            self.blocks
                .retain(|block| block.kind != RoutingBlockKind::Authentication);
            if previous_len != self.blocks.len() {
                self.mark_recovery_dirty();
            }
            self.probe = None;
            self.healed_scopes
                .retain(|(kind, _, _)| *kind != RoutingBlockKind::Authentication);
        }
        self.sequence = self.sequence.saturating_add(1);
        ObservationToken {
            sequence: self.sequence,
            credential_generation,
        }
    }

    fn accepts(&mut self, observation: ObservationToken) -> bool {
        if observation.credential_generation != self.generation
            || observation.sequence < self.merged_sequence
        {
            return false;
        }
        self.merged_sequence = observation.sequence;
        true
    }

    pub fn snapshot(&self, now_ms: u64) -> AccountLimitState {
        self.project(now_ms, RANK_GRACE_MS)
    }
    pub fn snapshot_for_affinity(&self, now_ms: u64) -> AccountLimitState {
        self.project(now_ms, AFFINITY_GRACE_MS)
    }

    pub fn credential_health(&self, now_ms: u64) -> crate::CredentialHealth {
        if self.limits.credential_health == crate::CredentialHealth::Quarantined {
            return crate::CredentialHealth::Quarantined;
        }
        if self.blocks.iter().any(|block| {
            block.kind == RoutingBlockKind::Authentication
                && (now_ms < block.retry_at_ms || self.probe.is_some())
        }) {
            return crate::CredentialHealth::Unavailable;
        }
        self.limits.credential_health
    }

    fn project(&self, now_ms: u64, grace_ms: u64) -> AccountLimitState {
        let mut limits = self.limits.clone();
        limits.credential_health = self.credential_health(now_ms);
        limits.usage_snapshot_fresh = self
            .last_good_ms
            .is_some_and(|good| now_ms.saturating_sub(good) <= grace_ms);
        // Reset means unknown allowance until new evidence, never an inferred full bucket.
        for claim in &mut limits.claims {
            if reset_ms(claim).is_some_and(|reset| reset <= now_ms) {
                claim.utilization = None;
                claim.status = ClaimStatus::Unknown;
                limits.complete_global_snapshot = false;
            }
        }
        for block in &self.blocks {
            match block.kind {
                RoutingBlockKind::Entitlement
                    if now_ms < block.retry_at_ms
                        || self
                            .probe
                            .as_ref()
                            .is_some_and(|(_, model)| applies(&block.scope, model)) =>
                {
                    if let ClaimScope::Model(model) = &block.scope {
                        limits
                            .entitlements
                            .insert(model.clone(), Entitlement::Unavailable);
                    }
                }
                RoutingBlockKind::Overage => {
                    limits.extra_usage.in_use = Some(true);
                }
                RoutingBlockKind::Quota => {
                    let probing = self
                        .probe
                        .as_ref()
                        .is_some_and(|(_, model)| applies(&block.scope, model));
                    let status = if now_ms < block.retry_at_ms || probing {
                        ClaimStatus::Rejected
                    } else {
                        ClaimStatus::Unknown
                    };
                    let key = block.claim_key.as_deref().unwrap_or("quota_revalidation");
                    if block.claim_key.is_none() {
                        for claim in limits.claims.iter_mut().filter(|claim| {
                            same_scope(&claim.scope, &block.scope)
                                && claim.status == ClaimStatus::Rejected
                        }) {
                            claim.status = ClaimStatus::Unknown;
                            if claim.utilization.is_some_and(|value| value >= 1.0) {
                                claim.utilization = None;
                            }
                        }
                    }
                    limits.claims.retain(|claim| {
                        !(claim.key == key && same_scope(&claim.scope, &block.scope))
                    });
                    limits.claims.push(LimitClaim {
                        key: key.to_owned(),
                        scope: block.scope.clone(),
                        utilization: None,
                        status,
                        resets_at: block.resets_at_ms.map(|reset| (reset / 1000).to_string()),
                        source: LimitSource::ResponseHeader,
                    });
                    if status == ClaimStatus::Unknown {
                        limits.complete_global_snapshot = false;
                    }
                }
                _ => {}
            }
        }
        limits
    }

    pub fn apply_usage(
        &mut self,
        usage: NormalizedUsage,
        observation: ObservationToken,
        now_ms: u64,
    ) {
        if !self.accepts(observation) {
            return;
        }
        self.merge_claims(usage.claims, observation, now_ms);
        self.merge_extra(usage.extra_usage, observation, now_ms);
        self.limits.complete_global_snapshot = usage.complete_global_snapshot;
        if usage.complete_global_snapshot {
            self.last_good_ms = Some(now_ms);
        }
    }

    pub fn observe_response(
        &mut self,
        claims: Vec<LimitClaim>,
        extra: ExtraUsageState,
        observation: ObservationToken,
        now_ms: u64,
    ) {
        if !self.accepts(observation) {
            if observation.credential_generation <= self.generation {
                for claim in claims
                    .into_iter()
                    .filter(|claim| claim.status == ClaimStatus::Rejected)
                {
                    self.add_block(
                        RoutingBlockKind::Quota,
                        claim.scope.clone(),
                        safe_key(&normalize_claim_key(&claim.key)),
                        reset_ms(&claim),
                        observation,
                        now_ms,
                    );
                }
                if extra.in_use == Some(true) {
                    self.add_block(
                        RoutingBlockKind::Overage,
                        ClaimScope::Global,
                        None,
                        None,
                        observation,
                        now_ms,
                    );
                }
            }
            return;
        }
        self.merge_claims(claims, observation, now_ms);
        self.merge_extra(extra, observation, now_ms);
    }

    fn merge_extra(&mut self, extra: ExtraUsageState, observation: ObservationToken, now_ms: u64) {
        if extra.in_use == Some(true) {
            self.add_block(
                RoutingBlockKind::Overage,
                ClaimScope::Global,
                None,
                None,
                observation,
                now_ms,
            );
        } else if extra.in_use == Some(false) {
            let previous_len = self.blocks.len();
            self.blocks.retain(|block| {
                block.kind != RoutingBlockKind::Overage
                    || observation.sequence <= block.rejected_sequence
            });
            if previous_len != self.blocks.len() {
                self.mark_recovery_dirty();
            }
            record_healed_scope(
                &mut self.healed_scopes,
                RoutingBlockKind::Overage,
                &ClaimScope::Global,
                observation.sequence,
            );
        }
        self.limits.apply_response_observation(Vec::new(), extra);
    }

    fn merge_claims(
        &mut self,
        claims: Vec<LimitClaim>,
        observation: ObservationToken,
        now_ms: u64,
    ) {
        for mut claim in claims {
            claim.key = normalize_claim_key(&claim.key);
            // A lower utilization in the same rejected window is not recovery evidence.
            let previous_len = self.blocks.len();
            let healed_scopes = &mut self.healed_scopes;
            self.blocks.retain(|block| {
                if block.claim_key.as_deref() != Some(claim.key.as_str())
                    || !same_scope(&block.scope, &claim.scope)
                    || observation.sequence <= block.rejected_sequence
                {
                    return true;
                }
                let renewed_window = match (reset_ms(&claim), block.resets_at_ms) {
                    (Some(new), Some(old)) => new > old && now_ms >= old,
                    _ => false,
                };
                let healed = renewed_window
                    && claim.status == ClaimStatus::Allowed
                    && claim.utilization.is_some_and(|value| value < 1.0);
                if healed {
                    record_healed_scope(
                        healed_scopes,
                        block.kind,
                        &block.scope,
                        observation.sequence,
                    );
                }
                !healed
            });
            if previous_len != self.blocks.len() {
                self.mark_recovery_dirty();
            }
            if claim.status == ClaimStatus::Rejected
                || claim.utilization.is_some_and(|value| value >= 1.0)
            {
                self.add_block(
                    RoutingBlockKind::Quota,
                    claim.scope.clone(),
                    safe_key(&claim.key),
                    reset_ms(&claim),
                    observation,
                    now_ms,
                );
            }
            self.limits
                .apply_response_observation([claim], ExtraUsageState::default());
        }
    }

    pub fn reject_quota(
        &mut self,
        model: &ModelFamily,
        headers: &HeaderSnapshot,
        observation: ObservationToken,
        now_ms: u64,
    ) {
        if observation.credential_generation > self.generation {
            return;
        }
        let current = observation.credential_generation == self.generation
            && observation.sequence >= self.merged_sequence;
        self.merged_sequence = self.merged_sequence.max(observation.sequence);
        let claims = crate::response::normalize_unified_headers(headers, Some(model));
        let mut found = false;
        for claim in &claims {
            if claim.status == ClaimStatus::Rejected
                && matches!(claim.scope, ClaimScope::Global | ClaimScope::Model(_))
            {
                found = true;
                self.add_block(
                    RoutingBlockKind::Quota,
                    claim.scope.clone(),
                    safe_key(&claim.key),
                    reset_ms(claim),
                    observation,
                    now_ms,
                );
            }
        }
        if !found {
            self.add_block(
                RoutingBlockKind::Quota,
                ClaimScope::Model(model.clone()),
                None,
                None,
                observation,
                now_ms,
            );
        }
        if current {
            self.limits
                .apply_response_observation(claims, ExtraUsageState::default());
            self.merge_extra(
                crate::response::normalize_unified_extra_usage(headers),
                observation,
                now_ms,
            );
        } else if crate::response::normalize_unified_extra_usage(headers).in_use == Some(true) {
            self.add_block(
                RoutingBlockKind::Overage,
                ClaimScope::Global,
                None,
                None,
                observation,
                now_ms,
            );
        }
        if self
            .probe
            .as_ref()
            .is_some_and(|(id, candidate)| candidate == model && observation.sequence > *id)
        {
            self.probe = None;
        }
    }

    pub fn reject_entitlement(
        &mut self,
        model: &ModelFamily,
        observation: ObservationToken,
        now_ms: u64,
    ) {
        if observation.credential_generation != self.generation {
            return;
        }
        self.merged_sequence = self.merged_sequence.max(observation.sequence);
        self.add_block(
            RoutingBlockKind::Entitlement,
            ClaimScope::Model(model.clone()),
            None,
            None,
            observation,
            now_ms,
        );
        if self
            .probe
            .as_ref()
            .is_some_and(|(id, candidate)| candidate == model && observation.sequence > *id)
        {
            self.probe = None;
        }
    }

    pub fn reject_authentication(&mut self, observation: ObservationToken, now_ms: u64) {
        if observation.credential_generation != self.generation {
            return;
        }
        self.merged_sequence = self.merged_sequence.max(observation.sequence);
        self.add_block(
            RoutingBlockKind::Authentication,
            ClaimScope::Global,
            None,
            None,
            observation,
            now_ms,
        );
        if self
            .probe
            .as_ref()
            .is_some_and(|(id, _)| observation.sequence > *id)
        {
            self.probe = None;
        }
    }

    pub fn confirm_success(
        &mut self,
        model: &ModelFamily,
        observation: ObservationToken,
        now_ms: u64,
    ) {
        if !self.accepts(observation) {
            return;
        }
        let probe = self
            .probe
            .as_ref()
            .is_some_and(|(id, candidate)| candidate == model && observation.sequence > *id);
        let previous_len = self.blocks.len();
        let claims = &mut self.limits.claims;
        let healed_scopes = &mut self.healed_scopes;
        self.blocks.retain(|block| {
            let healed = applies(&block.scope, model)
                && block.kind != RoutingBlockKind::Overage
                && observation.sequence > block.rejected_sequence
                && now_ms >= block.retry_at_ms
                && (probe || block.kind == RoutingBlockKind::Entitlement);
            if healed {
                record_healed_scope(
                    healed_scopes,
                    block.kind,
                    &block.scope,
                    observation.sequence,
                );
            }
            if healed && block.kind == RoutingBlockKind::Quota {
                for claim in claims.iter_mut().filter(|claim| {
                    same_scope(&claim.scope, &block.scope)
                        && (block.claim_key.as_deref() == Some(claim.key.as_str())
                            || (block.claim_key.is_none() && claim.status == ClaimStatus::Rejected))
                }) {
                    if claim.status == ClaimStatus::Rejected {
                        claim.status = ClaimStatus::Unknown;
                    }
                    if claim.utilization.is_some_and(|value| value >= 1.0) {
                        claim.utilization = None;
                    }
                }
            }
            !healed
        });
        if previous_len != self.blocks.len() {
            self.mark_recovery_dirty();
        }
        self.limits
            .entitlements
            .insert(model.clone(), Entitlement::Available);
        if probe {
            self.probe = None;
        }
    }

    fn add_block(
        &mut self,
        kind: RoutingBlockKind,
        scope: ClaimScope,
        claim_key: Option<String>,
        resets_at_ms: Option<u64>,
        observation: ObservationToken,
        now_ms: u64,
    ) {
        if self
            .healed_scopes
            .iter()
            .any(|(previous_kind, previous_scope, sequence)| {
                *previous_kind == kind
                    && same_scope(previous_scope, &scope)
                    && *sequence >= observation.sequence
            })
            || self.blocks.iter().any(|block| {
                block.kind == kind
                    && same_scope(&block.scope, &scope)
                    && block.rejected_sequence > observation.sequence
            })
        {
            return;
        }
        let next = RoutingBlock {
            kind,
            scope,
            claim_key,
            rejected_sequence: observation.sequence,
            credential_generation: observation.credential_generation,
            observed_at_ms: now_ms,
            retry_at_ms: resets_at_ms
                .filter(|reset| *reset > now_ms)
                .unwrap_or_else(|| now_ms.saturating_add(RECOVERY_MS)),
            resets_at_ms,
        };
        if self.blocks.iter().any(|block| block == &next) {
            return;
        }
        self.blocks.retain(|block| {
            !(block.kind == next.kind
                && same_scope(&block.scope, &next.scope)
                && block.claim_key == next.claim_key)
        });
        self.blocks.push(next);
        self.mark_recovery_dirty();
    }

    pub fn probe_due(&self, model: &ModelFamily, now_ms: u64) -> bool {
        if self.probe.is_some() {
            return false;
        }
        let mut due = false;
        for block in &self.blocks {
            if applies(&block.scope, model) && block.kind != RoutingBlockKind::Overage {
                if now_ms < block.retry_at_ms {
                    return false;
                }
                due = true;
            }
        }
        due
    }
    /// Only one half-open request may own recovery across all scoped blocks.
    pub fn reserve_probe(&mut self, model: &ModelFamily, now_ms: u64) -> Option<u64> {
        let mut relevant = false;
        for block in &self.blocks {
            if applies(&block.scope, model)
                && matches!(
                    block.kind,
                    RoutingBlockKind::Quota
                        | RoutingBlockKind::Entitlement
                        | RoutingBlockKind::Authentication
                )
            {
                relevant = true;
                if now_ms < block.retry_at_ms {
                    return None;
                }
            }
        }
        if !relevant {
            return Some(0);
        }
        if self.probe.is_some() {
            return None;
        }
        self.sequence = self.sequence.saturating_add(1);
        let id = self.sequence;
        self.probe = Some((id, model.clone()));
        Some(id)
    }
    pub fn release_probe(&mut self, id: u64) {
        if self
            .probe
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
        {
            self.probe = None;
        }
    }
    pub fn export_blocks(&self) -> Vec<RoutingBlock> {
        self.blocks
            .iter()
            .map(|block| {
                let mut block = block.clone();
                block.scope = match &block.scope {
                    ClaimScope::Model(model) => ClaimScope::Model(persistent_model_scope(model)),
                    ClaimScope::Surface(_) => ClaimScope::Unknown,
                    scope => scope.clone(),
                };
                block
            })
            .collect()
    }
    pub fn restore_blocks(&mut self, blocks: Vec<RoutingBlock>, generation: u64, _now_ms: u64) {
        self.generation = generation;
        let restored: Vec<_> = blocks
            .into_iter()
            .filter(|block| {
                block.kind != RoutingBlockKind::Authentication
                    || block.credential_generation == generation
            })
            .collect();
        if self.blocks != restored {
            self.blocks = restored;
            self.mark_recovery_dirty();
        }
        self.sequence = self.sequence.max(
            self.blocks
                .iter()
                .map(|block| block.rejected_sequence)
                .max()
                .unwrap_or(0),
        );
        self.merged_sequence = self.sequence;
        self.probe = None;
        self.healed_scopes.clear();
    }
}

fn record_healed_scope(
    versions: &mut Vec<(RoutingBlockKind, ClaimScope, u64)>,
    kind: RoutingBlockKind,
    scope: &ClaimScope,
    sequence: u64,
) {
    if let Some((_, _, previous)) =
        versions
            .iter_mut()
            .find(|(previous_kind, previous_scope, _)| {
                *previous_kind == kind && same_scope(previous_scope, scope)
            })
    {
        *previous = (*previous).max(sequence);
    } else {
        versions.push((kind, scope.clone(), sequence));
    }
}

fn applies(scope: &ClaimScope, model: &ModelFamily) -> bool {
    matches!(scope, ClaimScope::Global)
        || matches!(scope, ClaimScope::Model(candidate) if model_scope_matches(candidate, model))
}
fn same_scope(left: &ClaimScope, right: &ClaimScope) -> bool {
    left == right
        || matches!((left, right), (ClaimScope::Model(left), ClaimScope::Model(right)) if model_scope_matches(left, right) || model_scope_matches(right, left))
}
fn reset_ms(claim: &LimitClaim) -> Option<u64> {
    claim
        .resets_at
        .as_deref()
        .and_then(parse_reset_unix_seconds)
        .and_then(|seconds| u64::try_from(seconds).ok())
        .and_then(|seconds| seconds.checked_mul(1000))
}
fn safe_key(key: &str) -> Option<String> {
    matches!(
        key,
        "five_hour"
            | "seven_day"
            | "seven_day_overage_included"
            | "seven_day_opus"
            | "seven_day_sonnet"
    )
    .then(|| key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelFamily {
        ModelFamily::from_display_name("Sonnet")
    }
    fn claim(status: ClaimStatus, reset: Option<&str>) -> LimitClaim {
        LimitClaim {
            key: "5h".to_owned(),
            scope: ClaimScope::Global,
            utilization: Some(0.1),
            status,
            resets_at: reset.map(str::to_owned),
            source: LimitSource::UsageEndpoint,
        }
    }
    fn rejected() -> HeaderSnapshot {
        HeaderSnapshot::from_iter([
            ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "5h".to_owned(),
            ),
            ("anthropic-ratelimit-unified-reset", "100".to_owned()),
        ])
    }
    fn blocked(state: &RoutingState, now: u64, model: &ModelFamily) -> bool {
        state
            .snapshot(now)
            .claims
            .iter()
            .any(|claim| claim.applies_to(model) && claim.status == ClaimStatus::Rejected)
    }

    #[test]
    fn delayed_poll_and_success_cannot_undo_later_rejection() {
        let mut state = RoutingState::unknown("a");
        let old_poll = state.begin_observation(1);
        let old_success = state.begin_observation(1);
        let rejection = state.begin_observation(1);
        state.reject_quota(&model(), &rejected(), rejection, 1_000);
        state.apply_usage(
            NormalizedUsage {
                claims: vec![claim(ClaimStatus::Allowed, Some("200"))],
                complete_global_snapshot: true,
                ..Default::default()
            },
            old_poll,
            101_000,
        );
        state.confirm_success(&model(), old_success, 101_000);
        assert_eq!(state.export_blocks().len(), 1);
        assert!(!state.snapshot(101_000).complete_global_snapshot);
        assert_eq!(state.snapshot(101_000).claims[0].utilization, None);
    }

    #[test]
    fn same_window_low_utilization_does_not_heal_rejection() {
        let mut state = RoutingState::unknown("a");
        let rejection = state.begin_observation(1);
        state.reject_quota(&model(), &rejected(), rejection, 1_000);
        let poll = state.begin_observation(1);
        state.apply_usage(
            NormalizedUsage {
                claims: vec![claim(ClaimStatus::Allowed, Some("100"))],
                complete_global_snapshot: true,
                ..Default::default()
            },
            poll,
            2_000,
        );
        assert!(blocked(&state, 2_000, &model()));
        let next = state.begin_observation(1);
        state.apply_usage(
            NormalizedUsage {
                claims: vec![claim(ClaimStatus::Allowed, Some("200"))],
                complete_global_snapshot: true,
                ..Default::default()
            },
            next,
            101_000,
        );
        assert!(state.export_blocks().is_empty());
    }

    #[test]
    fn omitted_paid_usage_and_old_false_cannot_clear_overage() {
        let mut state = RoutingState::unknown("a");
        let old = state.begin_observation(1);
        let paid = state.begin_observation(1);
        state.observe_response(
            Vec::new(),
            ExtraUsageState {
                in_use: Some(true),
                ..Default::default()
            },
            paid,
            1_000,
        );
        let poll = state.begin_observation(1);
        state.apply_usage(NormalizedUsage::default(), poll, 2_000);
        state.observe_response(
            Vec::new(),
            ExtraUsageState {
                in_use: Some(false),
                ..Default::default()
            },
            old,
            3_000,
        );
        assert_eq!(state.snapshot(100_000).extra_usage.in_use, Some(true));
        assert_eq!(state.export_blocks()[0].kind, RoutingBlockKind::Overage);
        let included = state.begin_observation(1);
        state.observe_response(
            Vec::new(),
            ExtraUsageState {
                in_use: Some(false),
                ..Default::default()
            },
            included,
            101_000,
        );
        assert_eq!(state.snapshot(101_000).extra_usage.in_use, Some(false));
    }

    #[test]
    fn unknown_representative_is_bounded_and_model_scoped() {
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.reject_quota(&model(), &HeaderSnapshot::new(), token, 1_000);
        assert!(blocked(&state, 2_000, &model()));
        assert!(!blocked(
            &state,
            2_000,
            &ModelFamily::from_display_name("Opus")
        ));
        assert_eq!(state.reserve_probe(&model(), 60_999), None);
        assert!(state.reserve_probe(&model(), 61_000).is_some());
    }

    #[test]
    fn incomplete_reset_requires_exactly_one_probe_and_success_heals() {
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.reject_quota(&model(), &rejected(), token, 1_000);
        let poll = state.begin_observation(1);
        state.apply_usage(
            NormalizedUsage {
                claims: vec![claim(ClaimStatus::Allowed, None)],
                complete_global_snapshot: true,
                ..Default::default()
            },
            poll,
            101_000,
        );
        let snapshot = state.snapshot(101_000);
        assert!(!snapshot.complete_global_snapshot);
        assert!(
            snapshot
                .claims
                .iter()
                .all(|claim| claim.utilization.is_none())
        );
        let probe = state.reserve_probe(&model(), 101_000).expect("one probe");
        assert_ne!(probe, 0);
        assert_eq!(state.reserve_probe(&model(), 101_000), None);
        assert!(blocked(&state, 101_000, &model()));
        let success = state.begin_observation(1);
        state.confirm_success(&model(), success, 101_001);
        assert!(!blocked(&state, 101_001, &model()));
        assert!(state.export_blocks().is_empty());
    }

    #[test]
    fn entitlement_expires_and_old_auth_generation_is_ignored() {
        let mut state = RoutingState::unknown("a");
        let old = state.begin_observation(1);
        state.reject_entitlement(&model(), old, 1_000);
        assert_eq!(
            state.snapshot(2_000).entitlement_for(&model()),
            Entitlement::Unavailable
        );
        assert_eq!(
            state.snapshot(61_000).entitlement_for(&model()),
            Entitlement::Unknown
        );
        let new = state.begin_observation(2);
        state.confirm_success(&model(), new, 61_000);
        state.reject_authentication(old, 62_000);
        assert!(state.export_blocks().is_empty());
        assert!(new.sequence > old.sequence);
    }

    #[test]
    fn cancelled_probe_restores_admission_but_stale_release_does_not() {
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.reject_quota(&model(), &HeaderSnapshot::new(), token, 0);
        let first = state.reserve_probe(&model(), 60_000).expect("first");
        state.release_probe(first);
        let second = state.reserve_probe(&model(), 60_000).expect("second");
        state.release_probe(first);
        assert_eq!(state.reserve_probe(&model(), 60_000), None);
        state.release_probe(second);
        assert!(state.reserve_probe(&model(), 60_000).is_some());
    }

    #[test]
    fn last_good_snapshot_has_separate_ranking_and_affinity_grace() {
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.apply_usage(
            NormalizedUsage {
                complete_global_snapshot: true,
                ..Default::default()
            },
            token,
            1_000,
        );
        assert!(!state.snapshot(302_000).usage_snapshot_fresh);
        assert!(state.snapshot_for_affinity(302_000).usage_snapshot_fresh);
        assert!(!state.snapshot_for_affinity(902_000).usage_snapshot_fresh);
    }

    #[test]
    fn expired_entitlement_has_one_recovery_probe() {
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.reject_entitlement(&model(), token, 0);
        let probe = state
            .reserve_probe(&model(), 60_000)
            .expect("one entitlement probe");
        assert_eq!(
            state.snapshot(60_000).entitlement_for(&model()),
            Entitlement::Unavailable
        );
        assert_eq!(state.reserve_probe(&model(), 60_000), None);
        state.release_probe(probe);
        assert_eq!(
            state.snapshot(60_000).entitlement_for(&model()),
            Entitlement::Unknown
        );
    }

    #[test]
    fn restore_preserves_reset_unknown_and_auth_is_generation_scoped() {
        let mut state = RoutingState::unknown("a");
        let quota = state.begin_observation(1);
        state.reject_quota(&model(), &rejected(), quota, 1_000);
        let auth = state.begin_observation(1);
        state.reject_authentication(auth, 2_000);
        let mut restored = RoutingState::unknown("a");
        restored.restore_blocks(state.export_blocks(), 2, 101_000);
        assert_eq!(restored.export_blocks().len(), 1);
        let limits = restored.snapshot(101_000);
        assert!(!limits.complete_global_snapshot);
        assert_eq!(limits.credential_health, crate::CredentialHealth::Healthy);
        assert!(restored.reserve_probe(&model(), 101_000).is_some());
    }

    #[test]
    fn omitted_header_model_limit_survives_complete_global_poll() {
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        let headers = HeaderSnapshot::from_iter([
            ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
            (
                "anthropic-ratelimit-unified-representative-claim",
                "7d_sonnet".to_owned(),
            ),
            ("anthropic-ratelimit-unified-reset", "100".to_owned()),
        ]);
        state.reject_quota(&model(), &headers, token, 1_000);
        let poll = state.begin_observation(1);
        state.apply_usage(
            NormalizedUsage {
                claims: vec![claim(ClaimStatus::Allowed, Some("200"))],
                complete_global_snapshot: true,
                ..Default::default()
            },
            poll,
            2_000,
        );
        assert!(blocked(&state, 2_000, &model()));
        assert!(!blocked(
            &state,
            2_000,
            &ModelFamily::from_display_name("Opus")
        ));
        assert!(!state.probe_due(&model(), 2_000));
        assert!(state.probe_due(&model(), 101_000));
    }

    #[test]
    fn persisted_unknown_model_is_opaque_and_restores_isolated_recovery() {
        let private_model =
            ModelFamily::from_requested_model("claude-private-request-shaped-value");
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.reject_entitlement(&private_model, token, 1_000);
        let exported = state.export_blocks();
        let encoded = serde_json::to_string(&exported).expect("serialize facts");
        assert!(!encoded.contains("private-request-shaped-value"));
        assert!(encoded.contains("sha256_"));
        let mut restored = RoutingState::unknown("a");
        restored.restore_blocks(exported, 1, 2_000);
        assert_eq!(
            restored.snapshot(2_000).entitlement_for(&private_model),
            Entitlement::Unavailable
        );
        assert_eq!(
            restored.snapshot(2_000).entitlement_for(&model()),
            Entitlement::Unknown
        );
        assert!(restored.probe_due(&private_model, 61_000));
        assert!(!restored.probe_due(&model(), 61_000));
    }

    #[test]
    fn restored_opaque_quota_blocks_requested_unknown_model_not_sibling() {
        let requested = ModelFamily::from_requested_model("claude-future-private-model");
        let sibling = ModelFamily::from_requested_model("claude-other-future-model");
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.reject_quota(&requested, &HeaderSnapshot::new(), token, 1_000);
        let mut restored = RoutingState::unknown("a");
        restored.restore_blocks(state.export_blocks(), 1, 2_000);
        assert!(blocked(&restored, 2_000, &requested));
        assert!(!blocked(&restored, 2_000, &sibling));
        assert_eq!(restored.reserve_probe(&requested, 2_000), None);
        assert!(!restored.probe_due(&sibling, 61_000));
        assert!(restored.probe_due(&requested, 61_000));
        assert!(restored.reserve_probe(&requested, 61_000).is_some());
        assert!(blocked(&restored, 61_000, &requested));
        assert!(!blocked(&restored, 61_000, &sibling));
        let success = restored.begin_observation(1);
        restored.confirm_success(&requested, success, 61_001);
        assert!(!blocked(&restored, 61_001, &requested));
    }

    #[test]
    fn late_independent_model_rejection_is_authoritative() {
        let mut state = RoutingState::unknown("a");
        let opus = ModelFamily::from_display_name("Opus");
        let opus_request = state.begin_observation(1);
        let sonnet_request = state.begin_observation(1);
        state.confirm_success(&model(), sonnet_request, 1_000);
        state.reject_quota(&opus, &HeaderSnapshot::new(), opus_request, 1_001);
        assert!(blocked(&state, 1_001, &opus));
        assert!(!blocked(&state, 1_001, &model()));
    }

    #[test]
    fn fallback_probe_preserves_existing_model_utilization() {
        let mut state = RoutingState::unknown("a");
        let opus = ModelFamily::from_display_name("Opus");
        let poll = state.begin_observation(1);
        let descriptive = LimitClaim {
            key: "seven_day_opus".to_owned(),
            scope: ClaimScope::Model(opus.clone()),
            utilization: Some(0.73),
            status: ClaimStatus::Allowed,
            resets_at: Some("200".to_owned()),
            source: LimitSource::UsageEndpoint,
        };
        state.apply_usage(
            NormalizedUsage {
                claims: vec![descriptive.clone()],
                complete_global_snapshot: true,
                ..Default::default()
            },
            poll,
            0,
        );
        let rejection = state.begin_observation(1);
        state.reject_quota(&opus, &HeaderSnapshot::new(), rejection, 1_000);
        state.reserve_probe(&opus, 61_000).expect("probe");
        let success = state.begin_observation(1);
        state.confirm_success(&opus, success, 61_001);
        assert!(state.snapshot(61_001).claims.contains(&descriptive));
        assert!(!blocked(&state, 61_001, &opus));
    }

    #[test]
    fn scoped_healing_prevents_late_rejection_from_reopening_block() {
        let mut state = RoutingState::unknown("a");
        let first = state.begin_observation(1);
        state.reject_quota(&model(), &HeaderSnapshot::new(), first, 0);
        let delayed = state.begin_observation(1);
        state.reserve_probe(&model(), 60_000).expect("probe");
        let success = state.begin_observation(1);
        state.confirm_success(&model(), success, 60_001);
        state.reject_quota(&model(), &HeaderSnapshot::new(), delayed, 60_002);
        assert!(!blocked(&state, 60_002, &model()));
        assert!(state.export_blocks().is_empty());
    }

    #[test]
    fn refresh_preserves_existing_and_late_quota_and_paid_evidence() {
        let mut state = RoutingState::unknown("a");
        let existing = state.begin_observation(1);
        state.reject_quota(&model(), &HeaderSnapshot::new(), existing, 0);
        let late = state.begin_observation(1);
        state.begin_observation(2);
        assert!(blocked(&state, 1_000, &model()));
        let opus = ModelFamily::from_display_name("Opus");
        state.reject_quota(&opus, &HeaderSnapshot::new(), late, 1_000);
        state.observe_response(
            Vec::new(),
            ExtraUsageState {
                in_use: Some(true),
                ..Default::default()
            },
            late,
            1_000,
        );
        assert!(blocked(&state, 1_000, &opus));
        assert_eq!(state.snapshot(1_000).extra_usage.in_use, Some(true));
        state.reject_authentication(late, 1_000);
        assert!(
            state
                .export_blocks()
                .iter()
                .all(|block| block.kind != RoutingBlockKind::Authentication)
        );
    }

    #[test]
    fn cheap_health_projection_preserves_definitive_quarantine() {
        let mut state = RoutingState::unknown("a");
        let token = state.begin_observation(1);
        state.reject_authentication(token, 0);
        assert_eq!(
            state.credential_health(1_000),
            crate::CredentialHealth::Unavailable
        );
        assert_eq!(
            state.credential_health(60_000),
            crate::CredentialHealth::Healthy
        );
        state.limits.credential_health = crate::CredentialHealth::Quarantined;
        assert_eq!(
            state.credential_health(1_000),
            crate::CredentialHealth::Quarantined
        );
        assert_eq!(
            state.snapshot(60_000).credential_health,
            crate::CredentialHealth::Quarantined
        );
    }

    #[test]
    fn recovery_revision_tracks_durable_blocks_not_healthy_turns_or_permits() {
        let mut state = RoutingState::unknown("a");
        let observation = state.begin_observation(1);
        state.apply_usage(NormalizedUsage::default(), observation, 0);
        state.observe_response(Vec::new(), ExtraUsageState::default(), observation, 0);
        state.confirm_success(&model(), observation, 0);
        assert_eq!(state.recovery_revision(), 0);
        let rejection = state.begin_observation(1);
        state.reject_quota(&model(), &HeaderSnapshot::new(), rejection, 1_000);
        let rejected_revision = state.recovery_revision();
        assert_eq!(rejected_revision, 1);
        state.reject_quota(&model(), &HeaderSnapshot::new(), rejection, 1_000);
        assert_eq!(state.recovery_revision(), rejected_revision);
        let first = state.reserve_probe(&model(), 61_000).expect("probe");
        state.release_probe(first);
        state
            .reserve_probe(&model(), 61_000)
            .expect("replacement probe");
        assert_eq!(state.recovery_revision(), rejected_revision);
        let success = state.begin_observation(1);
        state.confirm_success(&model(), success, 61_001);
        assert_eq!(state.recovery_revision(), rejected_revision + 1);
    }

    #[test]
    fn restore_and_changed_grant_auth_removal_advance_recovery_revision() {
        let mut original = RoutingState::unknown("a");
        let observation = original.begin_observation(1);
        original.reject_authentication(observation, 0);
        let blocks = original.export_blocks();
        let mut restored = RoutingState::unknown("a");
        restored.restore_blocks(blocks.clone(), 1, 0);
        assert_eq!(restored.recovery_revision(), 1);
        restored.restore_blocks(blocks, 1, 0);
        assert_eq!(restored.recovery_revision(), 1);
        restored.begin_observation(2);
        assert_eq!(restored.recovery_revision(), 2);
        assert!(restored.export_blocks().is_empty());
        restored.mark_recovery_dirty();
        assert_eq!(restored.recovery_revision(), 3);
    }
}
