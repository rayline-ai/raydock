use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SESSION_STATUS_SCHEMA: u32 = 1;
pub const RAYLINE_STATUS_ID_ENV: &str = "RAYLINE_STATUS_ID";

pub fn derive_status_id(launch_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rayline-status-v1\0");
    hasher.update(launch_id.as_bytes());
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub fn is_valid_status_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAssignmentKind {
    Primary,
    ModelOverride,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAssignmentReason {
    BalancedNewLaunch,
    PrimaryAffinity,
    ModelOverride,
    QuotaFailover,
    EntitlementFailover,
    CredentialFailover,
    MigrationGuard,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionAssignmentStatus {
    pub primary_account_id: String,
    pub current_account_id: String,
    pub current_model_family: String,
    pub kind: SessionAssignmentKind,
    pub reason: SessionAssignmentReason,
    pub assigned_at_unix: i64,
    pub last_seen_at_unix: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionLimitStatus {
    pub key: String,
    pub scope: String,
    pub used_fraction: f64,
    pub remaining_fraction: f64,
    pub resets_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionCapacityStatus {
    pub usage_snapshot_fresh: bool,
    pub effective_headroom: Option<f64>,
    pub bottleneck: Option<SessionLimitStatus>,
    pub applicable: Vec<SessionLimitStatus>,
    #[serde(default)]
    pub eligible_accounts: usize,
    #[serde(default)]
    pub total_accounts: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionPlacementStatus {
    pub strategy: String,
    pub score: Option<f64>,
    pub active_global_leases: usize,
    pub active_model_leases: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionRouteStatus {
    pub selected_model: String,
    pub virtual_model: Option<String>,
    pub policy: Option<String>,
    pub task_class: Option<String>,
    pub route_id: Option<String>,
    pub updated_at_unix: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionStatusSnapshot {
    pub schema: u32,
    pub pool_id: String,
    pub assignment: SessionAssignmentStatus,
    pub capacity: SessionCapacityStatus,
    pub placement: SessionPlacementStatus,
    pub route: Option<SessionRouteStatus>,
    pub updated_at_unix: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_id_is_domain_separated_lowercase_sha256() {
        let first = derive_status_id("launch-a");
        let second = derive_status_id("launch-b");
        assert!(is_valid_status_id(&first));
        assert!(is_valid_status_id(&second));
        assert_ne!(first, second);
        assert!(!first.contains("launch"));
    }

    #[test]
    fn status_id_validation_rejects_paths_and_uppercase() {
        assert!(!is_valid_status_id("../status"));
        assert!(!is_valid_status_id(&"A".repeat(64)));
        assert!(!is_valid_status_id(&"0".repeat(63)));
    }

    #[test]
    fn older_capacity_snapshots_default_pool_counts() {
        let capacity: SessionCapacityStatus = serde_json::from_value(serde_json::json!({
            "usage_snapshot_fresh": true,
            "effective_headroom": 0.8,
            "bottleneck": null,
            "applicable": []
        }))
        .expect("capacity snapshot");
        assert_eq!(capacity.eligible_accounts, 0);
        assert_eq!(capacity.total_accounts, 0);
    }
}
