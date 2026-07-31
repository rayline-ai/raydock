use std::collections::BTreeMap;

use rayline_subscriptions::{
    AccountLimitState, BillingPolicy, ClaimScope, ClaimStatus, ExtraUsageState, HeaderSnapshot,
    IneligibilityReason, LimitClaim, LimitSource, ModelFamily, NormalizedUsage, PoolPolicy,
    ResponseClassification, SelectionRequest, UsageSnapshot, classify_response,
    normalize_unified_extra_usage, normalize_unified_headers, select_account,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct UsagePoolFixture {
    accounts: Vec<UsageAccountFixture>,
}

#[derive(Deserialize)]
struct UsageAccountFixture {
    id: String,
    usage: UsageSnapshot,
}

#[derive(Deserialize)]
struct ResponseFixture {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Value,
}

fn usage_pool() -> Vec<AccountLimitState> {
    let fixture: UsagePoolFixture =
        serde_json::from_str(include_str!("fixtures/usage_pool.json")).expect("usage fixture");
    fixture
        .accounts
        .into_iter()
        .map(|account| AccountLimitState::from_usage(account.id, account.usage.normalize()))
        .collect()
}

fn select(
    accounts: &[AccountLimitState],
    model: &str,
    affinity_account_id: Option<&str>,
) -> rayline_subscriptions::SelectionDecision {
    select_account(
        accounts,
        &SelectionRequest {
            model: ModelFamily::from_requested_model(model),
            affinity_account_id: affinity_account_id.map(str::to_owned),
            launch_id: "policy-test-launch".to_owned(),
            placement_loads: BTreeMap::new(),
        },
        &PoolPolicy::default(),
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
fn sanitized_pool_selects_the_only_globally_eligible_account() {
    let accounts = usage_pool();

    assert_eq!(
        select(&accounts, "claude-sonnet-4-5", None)
            .selected_account_id
            .as_deref(),
        Some("ws")
    );
    assert_eq!(
        select(&accounts, "claude-fable-5", None)
            .selected_account_id
            .as_deref(),
        Some("ws")
    );
}

#[test]
fn effective_headroom_is_the_minimum_applicable_pool() {
    let accounts = usage_pool();
    let decision = select(&accounts, "claude-fable-5", None);
    let ws = decision
        .evaluations
        .iter()
        .find(|evaluation| evaluation.account_id == "ws")
        .expect("ws evaluation");

    assert_eq!(ws.effective_headroom, Some(0.53));
}

#[test]
fn fable_exhaustion_does_not_move_sonnet_affinity() {
    let mut accounts = usage_pool();
    let ws = accounts
        .iter_mut()
        .find(|account| account.account_id == "ws")
        .expect("ws account");
    ws.claims
        .iter_mut()
        .find(|claim| claim.scope == ClaimScope::Model(ModelFamily::from_display_name("Fable")))
        .expect("Fable claim")
        .utilization = Some(1.0);

    let memex = accounts
        .iter_mut()
        .find(|account| account.account_id == "memex")
        .expect("memex account");
    memex
        .claims
        .iter_mut()
        .find(|claim| claim.key == "five_hour")
        .expect("five-hour claim")
        .utilization = Some(0.2);

    assert_eq!(
        select(&accounts, "claude-fable-5", Some("ws"))
            .selected_account_id
            .as_deref(),
        Some("memex")
    );
    assert_eq!(
        select(&accounts, "claude-sonnet-4-5", Some("ws"))
            .selected_account_id
            .as_deref(),
        Some("ws")
    );
}

#[test]
fn soft_threshold_does_not_strand_remaining_capacity() {
    let accounts = [
        AccountLimitState::from_usage(
            "a",
            NormalizedUsage {
                claims: vec![
                    claim("five_hour", ClaimScope::Global, 0.95),
                    claim("seven_day", ClaimScope::Global, 0.92),
                ],
                extra_usage: ExtraUsageState::default(),
                complete_global_snapshot: true,
            },
        ),
        AccountLimitState::from_usage(
            "b",
            NormalizedUsage {
                claims: vec![
                    claim("five_hour", ClaimScope::Global, 0.93),
                    claim("seven_day", ClaimScope::Global, 0.91),
                ],
                extra_usage: ExtraUsageState::default(),
                complete_global_snapshot: true,
            },
        ),
    ];

    assert_eq!(
        select(&accounts, "sonnet", Some("a"))
            .selected_account_id
            .as_deref(),
        Some("b")
    );
}

#[test]
fn included_only_excludes_an_account_already_using_overage() {
    let mut accounts = usage_pool();
    let ws = accounts
        .iter_mut()
        .find(|account| account.account_id == "ws")
        .expect("ws account");
    ws.extra_usage.in_use = Some(true);

    let decision = select(&accounts, "sonnet", None);
    let ws_evaluation = decision
        .evaluations
        .iter()
        .find(|evaluation| evaluation.account_id == "ws")
        .expect("ws evaluation");
    assert_eq!(
        ws_evaluation.reasons,
        vec![IneligibilityReason::IncludedUsageInUse]
    );
    assert_eq!(BillingPolicy::IncludedOnly, PoolPolicy::default().billing);
}

#[test]
fn sanitized_weekly_rejection_is_safe_to_fail_over() {
    let fixture: ResponseFixture =
        serde_json::from_str(include_str!("fixtures/weekly_limit_429.json"))
            .expect("response fixture");
    let headers = HeaderSnapshot::from_iter(fixture.headers);
    let body = serde_json::to_vec(&fixture.body).expect("response body");

    assert_eq!(
        classify_response(fixture.status, &headers, Some(&body)),
        ResponseClassification::FailoverQuota {
            representative_claim: Some("seven_day".to_owned()),
            rejected_claims: vec!["seven_day".to_owned()],
            resets_at: Some("1893459600".to_owned()),
        }
    );

    let claims =
        normalize_unified_headers(&headers, Some(&ModelFamily::from_requested_model("sonnet")));
    assert!(claims.iter().any(|claim| {
        claim.key == "seven_day"
            && claim.scope == ClaimScope::Global
            && claim.status == ClaimStatus::Rejected
            && claim.utilization == Some(1.0)
    }));
}

#[test]
fn unknown_representative_claim_is_scoped_to_the_requested_model() {
    let headers = HeaderSnapshot::from_iter([
        ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "future_weekly_scoped".to_owned(),
        ),
    ]);
    let requested_model = ModelFamily::from_requested_model("claude-next-1");
    let claims = normalize_unified_headers(&headers, Some(&requested_model));

    assert_eq!(
        claims,
        vec![LimitClaim {
            key: "future_weekly_scoped".to_owned(),
            scope: ClaimScope::Model(requested_model),
            utilization: None,
            status: ClaimStatus::Rejected,
            resets_at: None,
            source: LimitSource::ResponseHeader,
        }]
    );
}

#[test]
fn overage_in_use_is_normalized_for_included_only_policy() {
    let headers = HeaderSnapshot::from_iter([
        (
            "anthropic-ratelimit-unified-overage-in-use",
            "true".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-overage-disabled-reason",
            "organization_cap".to_owned(),
        ),
    ]);

    assert_eq!(
        normalize_unified_extra_usage(&headers),
        ExtraUsageState {
            enabled: None,
            in_use: Some(true),
            disabled_reason: Some("organization_cap".to_owned()),
        }
    );
}

#[test]
fn response_observations_update_existing_canonical_claims() {
    let mut account = usage_pool()
        .into_iter()
        .find(|account| account.account_id == "ws")
        .expect("ws account");
    let headers = HeaderSnapshot::from_iter([
        ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-7d-status",
            "rejected".to_owned(),
        ),
        ("anthropic-ratelimit-unified-7d-utilization", "1".to_owned()),
        ("anthropic-ratelimit-unified-reset", "1893459600".to_owned()),
    ]);

    account.apply_response_observation(
        normalize_unified_headers(&headers, None),
        normalize_unified_extra_usage(&headers),
    );

    let weekly_claims = account
        .claims
        .iter()
        .filter(|claim| claim.key == "seven_day")
        .collect::<Vec<_>>();
    assert_eq!(weekly_claims.len(), 1);
    assert_eq!(weekly_claims[0].status, ClaimStatus::Rejected);
    assert_eq!(weekly_claims[0].utilization, Some(1.0));
    assert_eq!(weekly_claims[0].resets_at.as_deref(), Some("1893459600"));
}

#[test]
fn credits_required_is_distinct_from_a_generic_429() {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "details": {
                "error_code": "credits_required"
            }
        }
    }))
    .expect("body");

    assert_eq!(
        classify_response(429, &HeaderSnapshot::new(), Some(&body)),
        ResponseClassification::EntitlementRejected {
            error_code: "credits_required".to_owned(),
            representative_claim: None,
        }
    );
}

#[test]
fn successful_response_is_never_reclassified_from_an_error_shaped_body() {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "details": {
                "error_code": "credits_required"
            }
        }
    }))
    .expect("body");

    assert_eq!(
        classify_response(200, &HeaderSnapshot::new(), Some(&body)),
        ResponseClassification::Success
    );
}
