use std::cmp::Ordering;
use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::{
    AccountLimitState, BillingPolicy, CredentialHealth, Entitlement, ModelFamily, PoolPolicy,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionRequest {
    pub model: ModelFamily,
    pub affinity_account_id: Option<String>,
    pub launch_id: String,
    pub placement_loads: BTreeMap<String, AccountPlacementLoad>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AccountPlacementLoad {
    pub active_global_leases: usize,
    pub active_model_leases: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectionDecision {
    pub selected_account_id: Option<String>,
    pub evaluations: Vec<AccountEvaluation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountEvaluation {
    pub account_id: String,
    pub eligible: bool,
    pub effective_headroom: Option<f64>,
    pub placement_score: Option<f64>,
    pub below_soft_threshold: bool,
    pub active_global_leases: usize,
    pub active_model_leases: usize,
    pub reasons: Vec<IneligibilityReason>,
    placement_rank: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IneligibilityReason {
    CredentialRefreshing,
    CredentialUnavailable,
    CredentialQuarantined,
    ModelUnavailable,
    IncludedUsageInUse,
    ExhaustedClaim(String),
}

pub fn select_account(
    accounts: &[AccountLimitState],
    request: &SelectionRequest,
    policy: &PoolPolicy,
) -> SelectionDecision {
    let mut evaluations = accounts
        .iter()
        .map(|account| evaluate_account(account, request, policy))
        .collect::<Vec<_>>();

    let selected_account_id =
        select_from_evaluations(&mut evaluations, request.affinity_account_id.as_deref());

    SelectionDecision {
        selected_account_id,
        evaluations,
    }
}

fn evaluate_account(
    account: &AccountLimitState,
    request: &SelectionRequest,
    policy: &PoolPolicy,
) -> AccountEvaluation {
    let mut reasons = Vec::new();
    match account.credential_health {
        CredentialHealth::Healthy => {}
        CredentialHealth::Refreshing => {
            reasons.push(IneligibilityReason::CredentialRefreshing);
        }
        CredentialHealth::Unavailable => {
            reasons.push(IneligibilityReason::CredentialUnavailable);
        }
        CredentialHealth::Quarantined => {
            reasons.push(IneligibilityReason::CredentialQuarantined);
        }
    }
    if account.entitlement_for(&request.model) == Entitlement::Unavailable {
        reasons.push(IneligibilityReason::ModelUnavailable);
    }
    if policy.billing == BillingPolicy::IncludedOnly && account.extra_usage.in_use == Some(true) {
        reasons.push(IneligibilityReason::IncludedUsageInUse);
    }

    let applicable_claims = account
        .claims
        .iter()
        .filter(|claim| claim.applies_to(&request.model) && !claim.reset_has_passed())
        .collect::<Vec<_>>();
    for claim in &applicable_claims {
        if claim.is_hard_exhausted() {
            reasons.push(IneligibilityReason::ExhaustedClaim(claim.key.clone()));
        }
    }

    let effective_headroom = (account.usage_snapshot_fresh && account.complete_global_snapshot)
        .then(|| {
            applicable_claims
                .iter()
                .filter_map(|claim| {
                    claim
                        .utilization
                        .map(|utilization| 1.0 - utilization.clamp(0.0, 1.0))
                })
                .reduce(f64::min)
                .unwrap_or(1.0)
        });
    let placement_load = request
        .placement_loads
        .get(&account.account_id)
        .copied()
        .unwrap_or_default();
    let placement_score =
        (account.usage_snapshot_fresh && account.complete_global_snapshot).then(|| {
            applicable_claims
                .iter()
                .filter_map(|claim| {
                    let utilization = claim.utilization?;
                    let active_leases = match claim.scope {
                        crate::ClaimScope::Global => placement_load.active_global_leases,
                        crate::ClaimScope::Model(_) => placement_load.active_model_leases,
                        crate::ClaimScope::Surface(_) | crate::ClaimScope::Unknown => 0,
                    };
                    Some((1.0 - utilization.clamp(0.0, 1.0)) / (1 + active_leases) as f64)
                })
                .reduce(f64::min)
                .unwrap_or_else(|| 1.0 / (1 + placement_load.active_global_leases) as f64)
        });
    let below_soft_threshold = effective_headroom.is_some()
        && applicable_claims
            .iter()
            .filter_map(|claim| claim.utilization)
            .all(|utilization| utilization < policy.switch_at_fraction());

    AccountEvaluation {
        account_id: account.account_id.clone(),
        eligible: reasons.is_empty(),
        effective_headroom,
        placement_score,
        below_soft_threshold,
        active_global_leases: placement_load.active_global_leases,
        active_model_leases: placement_load.active_model_leases,
        reasons,
        placement_rank: placement_rank(&request.launch_id, &request.model, &account.account_id),
    }
}

fn select_from_evaluations(
    evaluations: &mut [AccountEvaluation],
    affinity_account_id: Option<&str>,
) -> Option<String> {
    if let Some(affinity_account_id) = affinity_account_id
        && let Some(affinity) = evaluations.iter().find(|evaluation| {
            evaluation.account_id == affinity_account_id
                && evaluation.eligible
                && evaluation.below_soft_threshold
        })
    {
        return Some(affinity.account_id.clone());
    }

    evaluations
        .iter()
        .filter(|evaluation| evaluation.eligible)
        .max_by(|left, right| compare_candidates(left, right))
        .map(|evaluation| evaluation.account_id.clone())
}

fn compare_candidates(left: &AccountEvaluation, right: &AccountEvaluation) -> Ordering {
    left.below_soft_threshold
        .cmp(&right.below_soft_threshold)
        .then_with(|| match (left.placement_score, right.placement_score) {
            (Some(left), Some(right)) => left.total_cmp(&right),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        })
        .then_with(
            || match (left.effective_headroom, right.effective_headroom) {
                (Some(left), Some(right)) => left.total_cmp(&right),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (None, None) => Ordering::Equal,
            },
        )
        .then_with(|| left.placement_rank.cmp(&right.placement_rank))
        .then_with(|| right.account_id.cmp(&left.account_id))
}

fn placement_rank(launch_id: &str, model: &ModelFamily, account_id: &str) -> [u8; 32] {
    // Pool traffic not launched by Rayline has no safe session identity. Keep
    // the pre-balancing deterministic account order for that compatibility
    // path instead of pretending every unidentified request is one session.
    if launch_id.is_empty() {
        return [0; 32];
    }
    let mut hasher = Sha256::new();
    hasher.update(b"rayline-subscription-placement-v1\0");
    hasher.update(launch_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(model.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(account_id.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClaimScope, ClaimStatus, ExtraUsageState, LimitClaim, LimitSource, NormalizedUsage,
    };

    fn account(id: &str, five_hour: f64, weekly: f64) -> AccountLimitState {
        AccountLimitState::from_usage(
            id,
            NormalizedUsage {
                claims: vec![
                    claim("five_hour", ClaimScope::Global, five_hour),
                    claim("seven_day", ClaimScope::Global, weekly),
                ],
                extra_usage: ExtraUsageState::default(),
                complete_global_snapshot: true,
            },
        )
    }

    fn claim(key: &str, scope: ClaimScope, utilization: f64) -> LimitClaim {
        LimitClaim {
            key: key.to_owned(),
            scope,
            utilization: Some(utilization),
            status: ClaimStatus::Allowed,
            resets_at: None,
            source: LimitSource::UsageEndpoint,
        }
    }

    #[test]
    fn preserves_healthy_affinity_below_the_soft_threshold() {
        let accounts = [account("affinity", 0.3, 0.2), account("emptier", 0.1, 0.1)];
        let request = SelectionRequest {
            model: ModelFamily::from_requested_model("claude-sonnet-4-5"),
            affinity_account_id: Some("affinity".to_owned()),
            launch_id: "launch-affinity".to_owned(),
            placement_loads: BTreeMap::new(),
        };

        let decision = select_account(&accounts, &request, &PoolPolicy::default());
        assert_eq!(decision.selected_account_id.as_deref(), Some("affinity"));
    }

    #[test]
    fn stable_tie_breaker_spreads_equal_accounts_by_launch() {
        let accounts = [account("ws", 0.2, 0.2), account("af", 0.2, 0.2)];
        let selected = (0..64)
            .filter_map(|index| {
                select_account(
                    &accounts,
                    &SelectionRequest {
                        model: ModelFamily::from_requested_model("sonnet"),
                        affinity_account_id: None,
                        launch_id: format!("launch-{index}"),
                        placement_loads: BTreeMap::new(),
                    },
                    &PoolPolicy::default(),
                )
                .selected_account_id
            })
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn active_leases_can_outweigh_greater_raw_headroom() {
        let accounts = [account("busy", 0.2, 0.2), account("idle", 0.5, 0.5)];
        let request = SelectionRequest {
            model: ModelFamily::from_requested_model("sonnet"),
            affinity_account_id: None,
            launch_id: "new-launch".to_owned(),
            placement_loads: BTreeMap::from([(
                "busy".to_owned(),
                AccountPlacementLoad {
                    active_global_leases: 2,
                    active_model_leases: 2,
                },
            )]),
        };

        let decision = select_account(&accounts, &request, &PoolPolicy::default());
        assert_eq!(decision.selected_account_id.as_deref(), Some("idle"));
        let busy = decision
            .evaluations
            .iter()
            .find(|evaluation| evaluation.account_id == "busy")
            .expect("busy evaluation");
        assert_eq!(busy.effective_headroom, Some(0.8));
        assert_eq!(busy.placement_score, Some(0.8 / 3.0));
    }
}
