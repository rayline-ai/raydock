use std::cmp::Ordering;

use crate::{
    AccountLimitState, BillingPolicy, CredentialHealth, Entitlement, ModelFamily, PoolPolicy,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionRequest {
    pub model: ModelFamily,
    pub affinity_account_id: Option<String>,
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
    pub below_soft_threshold: bool,
    pub reasons: Vec<IneligibilityReason>,
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
    let below_soft_threshold = effective_headroom.is_some()
        && applicable_claims
            .iter()
            .filter_map(|claim| claim.utilization)
            .all(|utilization| utilization < policy.switch_at_fraction());

    AccountEvaluation {
        account_id: account.account_id.clone(),
        eligible: reasons.is_empty(),
        effective_headroom,
        below_soft_threshold,
        reasons,
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
        .then_with(
            || match (left.effective_headroom, right.effective_headroom) {
                (Some(left), Some(right)) => left.total_cmp(&right),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (None, None) => Ordering::Equal,
            },
        )
        // Reverse lexical order here because Iterator::max_by chooses Greater.
        .then_with(|| right.account_id.cmp(&left.account_id))
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
        };

        let decision = select_account(&accounts, &request, &PoolPolicy::default());
        assert_eq!(decision.selected_account_id.as_deref(), Some("affinity"));
    }

    #[test]
    fn stable_tie_breaker_uses_lexically_first_id() {
        let accounts = [account("ws", 0.2, 0.2), account("af", 0.2, 0.2)];
        let request = SelectionRequest {
            model: ModelFamily::from_requested_model("sonnet"),
            affinity_account_id: None,
        };

        let decision = select_account(&accounts, &request, &PoolPolicy::default());
        assert_eq!(decision.selected_account_id.as_deref(), Some("af"));
    }
}
