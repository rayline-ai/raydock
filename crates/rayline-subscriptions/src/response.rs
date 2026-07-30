use std::collections::BTreeMap;

use serde_json::Value;

use crate::{
    ClaimScope, ClaimStatus, ExtraUsageState, HeaderSnapshot, LimitClaim, LimitSource, ModelFamily,
};

const UNIFIED_PREFIX: &str = "anthropic-ratelimit-unified-";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResponseClassification {
    Success,
    FailoverQuota {
        representative_claim: Option<String>,
        rejected_claims: Vec<String>,
        resets_at: Option<String>,
    },
    RefreshCredential,
    EntitlementRejected {
        error_code: String,
        representative_claim: Option<String>,
    },
    TransientRateLimit,
    ProviderOverloaded,
    PassThrough,
}

pub fn classify_response(
    status: u16,
    headers: &HeaderSnapshot,
    body: Option<&[u8]>,
) -> ResponseClassification {
    if (200..=299).contains(&status) {
        return ResponseClassification::Success;
    }

    let representative_claim = headers
        .get("anthropic-ratelimit-unified-representative-claim")
        .map(canonical_header_claim);
    if let Some(error_code) = body.and_then(credits_required_error_code) {
        return ResponseClassification::EntitlementRejected {
            error_code,
            representative_claim,
        };
    }

    match status {
        401 => ResponseClassification::RefreshCredential,
        429 if is_unified_quota_rejection(headers) => ResponseClassification::FailoverQuota {
            representative_claim,
            rejected_claims: rejected_claims(headers),
            resets_at: headers
                .get("anthropic-ratelimit-unified-reset")
                .map(str::to_owned),
        },
        429 => ResponseClassification::TransientRateLimit,
        529 => ResponseClassification::ProviderOverloaded,
        _ => ResponseClassification::PassThrough,
    }
}

pub fn normalize_unified_headers(
    headers: &HeaderSnapshot,
    requested_model: Option<&ModelFamily>,
) -> Vec<LimitClaim> {
    #[derive(Default)]
    struct HeaderClaim {
        utilization: Option<f64>,
        status: Option<ClaimStatus>,
        resets_at: Option<String>,
    }

    let mut claims = BTreeMap::<String, HeaderClaim>::new();
    for (name, value) in headers.iter() {
        let Some(field) = name.strip_prefix(UNIFIED_PREFIX) else {
            continue;
        };
        if let Some(key) = field.strip_suffix("-utilization") {
            if !key.is_empty() {
                claims
                    .entry(canonical_header_claim(key))
                    .or_default()
                    .utilization = value.parse::<f64>().ok().and_then(normalize_fraction);
            }
        } else if let Some(key) = field.strip_suffix("-status") {
            if !key.is_empty() {
                claims
                    .entry(canonical_header_claim(key))
                    .or_default()
                    .status = Some(parse_claim_status(value));
            }
        } else if let Some(key) = field.strip_suffix("-reset")
            && !key.is_empty()
        {
            claims
                .entry(canonical_header_claim(key))
                .or_default()
                .resets_at = Some(value.to_owned());
        }
    }

    let representative_claim = headers
        .get("anthropic-ratelimit-unified-representative-claim")
        .map(canonical_header_claim);
    if header_value_is(headers, "anthropic-ratelimit-unified-status", "rejected")
        && let Some(representative_claim) = representative_claim.as_deref()
    {
        let claim = claims.entry(representative_claim.to_owned()).or_default();
        claim.status = Some(ClaimStatus::Rejected);
        if claim.resets_at.is_none() {
            claim.resets_at = headers
                .get("anthropic-ratelimit-unified-reset")
                .map(str::to_owned);
        }
    }

    claims
        .into_iter()
        .map(|(key, claim)| LimitClaim {
            scope: scope_for_header_claim(
                &key,
                requested_model,
                representative_claim.as_deref() == Some(key.as_str()),
            ),
            key,
            utilization: claim.utilization,
            status: claim.status.unwrap_or(ClaimStatus::Unknown),
            resets_at: claim.resets_at,
            source: LimitSource::ResponseHeader,
        })
        .collect()
}

pub fn normalize_unified_extra_usage(headers: &HeaderSnapshot) -> ExtraUsageState {
    ExtraUsageState {
        enabled: None,
        in_use: headers
            .get("anthropic-ratelimit-unified-overage-in-use")
            .and_then(parse_bool),
        disabled_reason: headers
            .get("anthropic-ratelimit-unified-overage-disabled-reason")
            .map(str::to_owned),
    }
}

fn is_unified_quota_rejection(headers: &HeaderSnapshot) -> bool {
    header_value_is(headers, "anthropic-ratelimit-unified-status", "rejected")
        && (headers
            .get("anthropic-ratelimit-unified-representative-claim")
            .is_some()
            || headers
                .get("anthropic-ratelimit-unified-overage-status")
                .is_some()
            || headers
                .get("anthropic-ratelimit-unified-overage-disabled-reason")
                .is_some())
}

fn rejected_claims(headers: &HeaderSnapshot) -> Vec<String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            if !value.trim().eq_ignore_ascii_case("rejected") {
                return None;
            }
            name.strip_prefix(UNIFIED_PREFIX)
                .and_then(|field| field.strip_suffix("-status"))
                .filter(|claim| !claim.is_empty() && *claim != "overage")
                .map(canonical_header_claim)
        })
        .collect()
}

fn credits_required_error_code(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    [
        "/error/details/error_code",
        "/error/error/details/error_code",
        "/details/error_code",
    ]
    .into_iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
    .filter(|code| *code == "credits_required")
    .map(str::to_owned)
}

fn parse_claim_status(value: &str) -> ClaimStatus {
    match value.trim().to_ascii_lowercase().as_str() {
        "allowed" => ClaimStatus::Allowed,
        "rejected" => ClaimStatus::Rejected,
        _ => ClaimStatus::Unknown,
    }
}

fn normalize_fraction(value: f64) -> Option<f64> {
    value.is_finite().then(|| value.clamp(0.0, 1.0))
}

fn scope_for_header_claim(
    claim: &str,
    requested_model: Option<&ModelFamily>,
    is_representative: bool,
) -> ClaimScope {
    match claim {
        "five_hour" | "seven_day" => ClaimScope::Global,
        "seven_day_overage_included" => ClaimScope::Model(ModelFamily::from_display_name("Fable")),
        "seven_day_opus" => ClaimScope::Model(ModelFamily::from_display_name("Opus")),
        "seven_day_sonnet" => ClaimScope::Model(ModelFamily::from_display_name("Sonnet")),
        key if key.starts_with("overage") => ClaimScope::Surface("overage".to_owned()),
        _ => requested_model
            .filter(|_| is_representative || claim.contains("scoped"))
            .cloned()
            .map(ClaimScope::Model)
            .unwrap_or(ClaimScope::Unknown),
    }
}

fn canonical_header_claim(claim: &str) -> String {
    match claim {
        "5h" => "five_hour",
        "7d" => "seven_day",
        "7d_oi" => "seven_day_overage_included",
        other => other,
    }
    .to_owned()
}

fn header_value_is(headers: &HeaderSnapshot, name: &str, expected: &str) -> bool {
    headers
        .get(name)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_429_remains_transient() {
        let headers = HeaderSnapshot::from_iter([("x-should-retry", "true".to_owned())]);
        assert_eq!(
            classify_response(429, &headers, None),
            ResponseClassification::TransientRateLimit
        );
    }

    #[test]
    fn overload_does_not_rotate_accounts() {
        assert_eq!(
            classify_response(529, &HeaderSnapshot::new(), None),
            ResponseClassification::ProviderOverloaded
        );
    }
}
