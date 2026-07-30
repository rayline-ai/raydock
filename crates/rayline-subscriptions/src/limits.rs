use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::ModelFamily;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LimitClaim {
    pub key: String,
    pub scope: ClaimScope,
    pub utilization: Option<f64>,
    pub status: ClaimStatus,
    pub resets_at: Option<String>,
    pub source: LimitSource,
}

impl LimitClaim {
    pub fn applies_to(&self, model: &ModelFamily) -> bool {
        match &self.scope {
            ClaimScope::Global => true,
            ClaimScope::Model(family) => family == model,
            ClaimScope::Surface(_) | ClaimScope::Unknown => false,
        }
    }

    pub fn is_hard_exhausted(&self) -> bool {
        !self.reset_has_passed()
            && (self.status == ClaimStatus::Rejected
                || self
                    .utilization
                    .is_some_and(|utilization| utilization >= 1.0))
    }

    pub fn reset_has_passed(&self) -> bool {
        self.resets_at
            .as_deref()
            .and_then(parse_reset_unix_seconds)
            .is_some_and(|reset| reset <= OffsetDateTime::now_utc().unix_timestamp())
    }
}

fn parse_reset_unix_seconds(value: &str) -> Option<i64> {
    let value = value.trim();
    if let Ok(timestamp) = value.parse::<i64>() {
        // Provider headers have appeared in both seconds and millisecond-like
        // integer forms. Normalize by magnitude without assuming one shape.
        return Some(if timestamp.unsigned_abs() >= 100_000_000_000 {
            timestamp / 1000
        } else {
            timestamp
        });
    }
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|timestamp| timestamp.unix_timestamp())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ClaimScope {
    Global,
    Model(ModelFamily),
    Surface(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Allowed,
    Rejected,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    UsageEndpoint,
    ResponseHeader,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExtraUsageState {
    pub enabled: Option<bool>,
    pub in_use: Option<bool>,
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialHealth {
    #[default]
    Healthy,
    Refreshing,
    Unavailable,
    Quarantined,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Entitlement {
    #[default]
    Unknown,
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NormalizedUsage {
    pub claims: Vec<LimitClaim>,
    pub extra_usage: ExtraUsageState,
    pub complete_global_snapshot: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountLimitState {
    pub account_id: String,
    pub credential_health: CredentialHealth,
    pub entitlements: BTreeMap<ModelFamily, Entitlement>,
    pub claims: Vec<LimitClaim>,
    pub extra_usage: ExtraUsageState,
    pub usage_snapshot_fresh: bool,
    pub complete_global_snapshot: bool,
}

impl AccountLimitState {
    pub fn unknown(account_id: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            credential_health: CredentialHealth::Healthy,
            entitlements: BTreeMap::new(),
            claims: Vec::new(),
            extra_usage: ExtraUsageState::default(),
            usage_snapshot_fresh: false,
            complete_global_snapshot: false,
        }
    }

    pub fn from_usage(account_id: impl Into<String>, usage: NormalizedUsage) -> Self {
        Self {
            account_id: account_id.into(),
            credential_health: CredentialHealth::Healthy,
            entitlements: BTreeMap::new(),
            claims: usage.claims,
            extra_usage: usage.extra_usage,
            usage_snapshot_fresh: true,
            complete_global_snapshot: usage.complete_global_snapshot,
        }
    }

    pub fn entitlement_for(&self, model: &ModelFamily) -> Entitlement {
        self.entitlements.get(model).copied().unwrap_or_default()
    }

    pub fn apply_response_observation(
        &mut self,
        claims: impl IntoIterator<Item = LimitClaim>,
        extra_usage: ExtraUsageState,
    ) {
        for observed in claims {
            if let Some(existing) = self
                .claims
                .iter_mut()
                .find(|claim| claim.key == observed.key && claim.scope == observed.scope)
            {
                if observed.utilization.is_some() {
                    existing.utilization = observed.utilization;
                }
                if observed.status != ClaimStatus::Unknown {
                    existing.status = observed.status;
                }
                if observed.resets_at.is_some() {
                    existing.resets_at = observed.resets_at;
                }
                existing.source = observed.source;
            } else {
                self.claims.push(observed);
            }
        }

        if extra_usage.enabled.is_some() {
            self.extra_usage.enabled = extra_usage.enabled;
        }
        if extra_usage.in_use.is_some() {
            self.extra_usage.in_use = extra_usage.in_use;
        }
        if extra_usage.disabled_reason.is_some() {
            self.extra_usage.disabled_reason = extra_usage.disabled_reason;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exhausted_with_reset(resets_at: &str) -> LimitClaim {
        LimitClaim {
            key: "five_hour".to_owned(),
            scope: ClaimScope::Global,
            utilization: Some(1.0),
            status: ClaimStatus::Rejected,
            resets_at: Some(resets_at.to_owned()),
            source: LimitSource::ResponseHeader,
        }
    }

    #[test]
    fn expired_rfc3339_and_epoch_resets_stop_excluding_an_account() {
        assert!(!exhausted_with_reset("2020-01-01T00:00:00Z").is_hard_exhausted());
        assert!(!exhausted_with_reset("1577836800").is_hard_exhausted());
        assert!(!exhausted_with_reset("1577836800000").is_hard_exhausted());
        assert!(exhausted_with_reset("4102444800").is_hard_exhausted());
    }
}
