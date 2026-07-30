use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ClaimScope, ClaimStatus, ExtraUsageState, LimitClaim, LimitSource, ModelFamily, NormalizedUsage,
};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageSnapshot {
    pub five_hour: Option<UsageBucket>,
    pub seven_day: Option<UsageBucket>,
    pub seven_day_oauth_apps: Option<UsageBucket>,
    pub seven_day_opus: Option<UsageBucket>,
    pub seven_day_sonnet: Option<UsageBucket>,
    pub cinder_cove: Option<UsageBucket>,
    pub extra_usage: Option<ExtraUsage>,
    #[serde(default, deserialize_with = "deserialize_limits")]
    pub limits: Vec<ScopedLimit>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

impl UsageSnapshot {
    pub fn normalize(&self) -> NormalizedUsage {
        let mut claims = Vec::new();
        push_bucket_claim(
            &mut claims,
            "five_hour",
            ClaimScope::Global,
            self.five_hour.as_ref(),
        );
        push_bucket_claim(
            &mut claims,
            "seven_day",
            ClaimScope::Global,
            self.seven_day.as_ref(),
        );
        push_bucket_claim(
            &mut claims,
            "seven_day_oauth_apps",
            ClaimScope::Surface("oauth_apps".to_owned()),
            self.seven_day_oauth_apps.as_ref(),
        );
        push_bucket_claim(
            &mut claims,
            "seven_day_opus",
            ClaimScope::Model(ModelFamily::from_display_name("Opus")),
            self.seven_day_opus.as_ref(),
        );
        push_bucket_claim(
            &mut claims,
            "seven_day_sonnet",
            ClaimScope::Model(ModelFamily::from_display_name("Sonnet")),
            self.seven_day_sonnet.as_ref(),
        );
        push_bucket_claim(
            &mut claims,
            "cinder_cove",
            ClaimScope::Unknown,
            self.cinder_cove.as_ref(),
        );

        for limit in &self.limits {
            let scope = limit
                .scope
                .as_ref()
                .and_then(|scope| scope.model.as_ref())
                .and_then(|model| model.display_name.as_deref())
                .map(ModelFamily::from_display_name)
                .map(ClaimScope::Model)
                .unwrap_or(ClaimScope::Unknown);
            let key = limit
                .group
                .as_deref()
                .filter(|group| !group.is_empty())
                .or(limit.kind.as_deref())
                .unwrap_or("scoped_limit")
                .to_owned();
            claims.push(LimitClaim {
                key,
                scope,
                utilization: limit.percent.and_then(normalize_percent),
                status: ClaimStatus::Allowed,
                resets_at: limit.resets_at.clone(),
                source: LimitSource::UsageEndpoint,
            });
        }

        NormalizedUsage {
            claims,
            extra_usage: self
                .extra_usage
                .as_ref()
                .map(ExtraUsage::normalize)
                .unwrap_or_default(),
            complete_global_snapshot: bucket_has_utilization(self.five_hour.as_ref())
                && bucket_has_utilization(self.seven_day.as_ref()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageBucket {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExtraUsage {
    pub is_enabled: Option<bool>,
    pub disabled_reason: Option<String>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

impl ExtraUsage {
    fn normalize(&self) -> ExtraUsageState {
        ExtraUsageState {
            enabled: self.is_enabled,
            // Whether overage is actively paying for the current request is
            // learned from unified response headers, not inferred from this
            // extensible usage payload.
            in_use: None,
            disabled_reason: self.disabled_reason.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ScopedLimit {
    pub kind: Option<String>,
    pub group: Option<String>,
    pub percent: Option<f64>,
    pub resets_at: Option<String>,
    pub scope: Option<UsageScope>,
    pub is_active: Option<bool>,
    pub severity: Option<String>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageScope {
    pub model: Option<ScopedModel>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ScopedModel {
    pub id: Option<String>,
    pub display_name: Option<String>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

fn push_bucket_claim(
    claims: &mut Vec<LimitClaim>,
    key: &str,
    scope: ClaimScope,
    bucket: Option<&UsageBucket>,
) {
    let Some(bucket) = bucket else {
        return;
    };
    claims.push(LimitClaim {
        key: key.to_owned(),
        scope,
        utilization: bucket.utilization.and_then(normalize_percent),
        status: ClaimStatus::Allowed,
        resets_at: bucket.resets_at.clone(),
        source: LimitSource::UsageEndpoint,
    });
}

fn normalize_percent(percent: f64) -> Option<f64> {
    percent
        .is_finite()
        .then(|| (percent / 100.0).clamp(0.0, 1.0))
}

fn bucket_has_utilization(bucket: Option<&UsageBucket>) -> bool {
    bucket
        .and_then(|bucket| bucket.utilization)
        .and_then(normalize_percent)
        .is_some()
}

fn deserialize_limits<'de, D>(deserializer: D) -> Result<Vec<ScopedLimit>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Vec<ScopedLimit>>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unknown_top_level_and_bucket_fields() {
        let snapshot: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "five_hour": {
                "utilization": 12,
                "future_bucket_field": "retained"
            },
            "future_top_level": {"enabled": true}
        }))
        .expect("usage snapshot");

        assert!(snapshot.unknown.contains_key("future_top_level"));
        assert!(
            snapshot
                .five_hour
                .as_ref()
                .expect("five hour")
                .unknown
                .contains_key("future_bucket_field")
        );
    }

    #[test]
    fn accepts_null_limits_as_an_empty_extensible_collection() {
        let snapshot: UsageSnapshot =
            serde_json::from_value(serde_json::json!({"limits": null})).expect("usage snapshot");
        assert!(snapshot.limits.is_empty());
    }
}
