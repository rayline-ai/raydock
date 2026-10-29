use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SUBSCRIPTION_CONFIG_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SubscriptionPoolsConfig {
    pub schema: u32,
    pub pools: BTreeMap<String, SubscriptionPoolConfig>,
}

impl SubscriptionPoolsConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema != SUBSCRIPTION_CONFIG_SCHEMA {
            return Err(ConfigError::UnsupportedSchema(self.schema));
        }
        if self.pools.is_empty() {
            return Err(ConfigError::NoPools);
        }
        for (pool_id, pool) in &self.pools {
            validate_identifier("pool", pool_id)?;
            pool.validate(pool_id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SubscriptionPoolConfig {
    pub control_config_dir: PathBuf,
    pub accounts: Vec<SubscriptionAccountConfig>,
    #[serde(default)]
    pub policy: PoolPolicy,
}

impl SubscriptionPoolConfig {
    fn validate(&self, pool_id: &str) -> Result<(), ConfigError> {
        if self.control_config_dir.as_os_str().is_empty() {
            return Err(ConfigError::EmptyControlConfigDir {
                pool: pool_id.to_owned(),
            });
        }
        if self.accounts.is_empty() {
            return Err(ConfigError::NoAccounts {
                pool: pool_id.to_owned(),
            });
        }

        let mut account_ids = HashSet::new();
        let mut credential_sources = HashSet::new();
        for account in &self.accounts {
            validate_identifier("account", &account.id)?;
            if !account_ids.insert(account.id.as_str()) {
                return Err(ConfigError::DuplicateAccount {
                    pool: pool_id.to_owned(),
                    account: account.id.clone(),
                });
            }
            if account
                .credential_source
                .claude_config_dir
                .as_os_str()
                .is_empty()
            {
                return Err(ConfigError::EmptyCredentialConfigDir {
                    pool: pool_id.to_owned(),
                    account: account.id.clone(),
                });
            }
            if !credential_sources.insert(&account.credential_source.claude_config_dir) {
                return Err(ConfigError::DuplicateCredentialSource {
                    pool: pool_id.to_owned(),
                    path: account.credential_source.claude_config_dir.clone(),
                });
            }
        }

        if !(0.0..=100.0).contains(&self.policy.switch_at_percent) {
            return Err(ConfigError::InvalidSwitchPercent {
                pool: pool_id.to_owned(),
                value: self.policy.switch_at_percent,
            });
        }
        for (field, value) in [
            ("migration_at_percent", self.policy.migration_at_percent),
            (
                "migration_advantage_percent",
                self.policy.migration_advantage_percent,
            ),
        ] {
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err(ConfigError::InvalidPolicyPercent {
                    pool: pool_id.to_owned(),
                    field,
                    value,
                });
            }
        }
        if self.policy.migration_at_percent < self.policy.switch_at_percent {
            return Err(ConfigError::MigrationBelowAdmission {
                pool: pool_id.to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionAccountConfig {
    pub id: String,
    pub credential_source: CredentialSourceConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CredentialSourceConfig {
    pub claude_config_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PoolPolicy {
    pub billing: BillingPolicy,
    pub switch_at_percent: f64,
    pub migration_at_percent: f64,
    pub migration_advantage_percent: f64,
    pub sticky_warm_seconds: u64,
    pub sticky: StickyPolicy,
}

impl<'de> Deserialize<'de> for PoolPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(default)]
            billing: BillingPolicy,
            #[serde(default = "default_switch_at_percent")]
            switch_at_percent: f64,
            #[serde(default)]
            migration_at_percent: Option<f64>,
            #[serde(default = "default_migration_advantage_percent")]
            migration_advantage_percent: f64,
            #[serde(default = "default_sticky_warm_seconds")]
            sticky_warm_seconds: u64,
            #[serde(default)]
            sticky: StickyPolicy,
        }
        let fields = Fields::deserialize(deserializer)?;
        Ok(Self {
            billing: fields.billing,
            switch_at_percent: fields.switch_at_percent,
            migration_at_percent: fields
                .migration_at_percent
                .unwrap_or_else(|| default_migration_at_percent().max(fields.switch_at_percent)),
            migration_advantage_percent: fields.migration_advantage_percent,
            sticky_warm_seconds: fields.sticky_warm_seconds,
            sticky: fields.sticky,
        })
    }
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            billing: BillingPolicy::default(),
            switch_at_percent: default_switch_at_percent(),
            migration_at_percent: default_migration_at_percent(),
            migration_advantage_percent: default_migration_advantage_percent(),
            sticky_warm_seconds: default_sticky_warm_seconds(),
            sticky: StickyPolicy::default(),
        }
    }
}

impl PoolPolicy {
    pub fn switch_at_fraction(&self) -> f64 {
        (self.switch_at_percent / 100.0).clamp(0.0, 1.0)
    }
}

fn default_switch_at_percent() -> f64 {
    90.0
}

fn default_migration_at_percent() -> f64 {
    95.0
}
fn default_migration_advantage_percent() -> f64 {
    5.0
}
fn default_sticky_warm_seconds() -> u64 {
    3600
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingPolicy {
    #[default]
    IncludedOnly,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StickyPolicy {
    #[default]
    LaunchModelFamily,
}

#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("unsupported subscription pool schema {0}")]
    UnsupportedSchema(u32),
    #[error("subscription pool config must contain at least one pool")]
    NoPools,
    #[error("{kind} id {value:?} must contain only ASCII letters, digits, '-' or '_'")]
    InvalidIdentifier { kind: &'static str, value: String },
    #[error("subscription pool {pool:?} must have a control_config_dir")]
    EmptyControlConfigDir { pool: String },
    #[error("subscription pool {pool:?} must contain at least one account")]
    NoAccounts { pool: String },
    #[error("subscription pool {pool:?} contains duplicate account id {account:?}")]
    DuplicateAccount { pool: String, account: String },
    #[error("subscription pool {pool:?} account {account:?} has an empty credential directory")]
    EmptyCredentialConfigDir { pool: String, account: String },
    #[error("subscription pool {pool:?} contains duplicate credential source {path:?}")]
    DuplicateCredentialSource { pool: String, path: PathBuf },
    #[error("subscription pool {pool:?} switch_at_percent must be between 0 and 100, got {value}")]
    InvalidSwitchPercent { pool: String, value: f64 },
    #[error("subscription pool {pool:?} {field} must be finite and between 0 and 100, got {value}")]
    InvalidPolicyPercent {
        pool: String,
        field: &'static str,
        value: f64,
    },
    #[error("subscription pool {pool:?} migration_at_percent must be at least switch_at_percent")]
    MigrationBelowAdmission { pool: String },
}

pub(crate) fn validate_identifier(kind: &'static str, value: &str) -> Result<(), ConfigError> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidIdentifier {
            kind,
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_defaults_to_included_only_and_ninety_percent() {
        let policy: PoolPolicy = serde_json::from_str("{}").expect("policy");
        assert_eq!(policy.billing, BillingPolicy::IncludedOnly);
        assert_eq!(policy.sticky, StickyPolicy::LaunchModelFamily);
        assert_eq!(policy.switch_at_percent, 90.0);
        assert_eq!(policy.migration_at_percent, 95.0);
        assert_eq!(policy.migration_advantage_percent, 5.0);
        assert_eq!(policy.sticky_warm_seconds, 3600);
        assert_eq!(policy.switch_at_fraction(), 0.9);
    }

    #[test]
    fn validation_rejects_duplicate_account_ids() {
        let config: SubscriptionPoolsConfig = serde_json::from_value(serde_json::json!({
            "schema": 1,
            "pools": {
                "default": {
                    "control_config_dir": "~/.claude",
                    "accounts": [
                        {
                            "id": "work",
                            "credential_source": {
                                "claude_config_dir": "~/.claude-work"
                            }
                        },
                        {
                            "id": "work",
                            "credential_source": {
                                "claude_config_dir": "~/.claude-work-2"
                            }
                        }
                    ]
                }
            }
        }))
        .expect("config");

        assert_eq!(
            config.validate(),
            Err(ConfigError::DuplicateAccount {
                pool: "default".to_owned(),
                account: "work".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_nonfinite_out_of_range_and_inverted_policy() {
        let mut pool = SubscriptionPoolConfig {
            control_config_dir: PathBuf::from("/tmp/control"),
            accounts: vec![SubscriptionAccountConfig {
                id: "a".to_owned(),
                credential_source: CredentialSourceConfig {
                    claude_config_dir: PathBuf::from("/tmp/a"),
                },
            }],
            policy: PoolPolicy::default(),
        };
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 101.0] {
            pool.policy.migration_at_percent = invalid;
            assert!(pool.validate("pool").is_err());
            pool.policy = PoolPolicy::default();
            pool.policy.migration_advantage_percent = invalid;
            assert!(pool.validate("pool").is_err());
            pool.policy = PoolPolicy::default();
            pool.policy.switch_at_percent = invalid;
            assert!(pool.validate("pool").is_err());
            pool.policy = PoolPolicy::default();
        }
        pool.policy.migration_at_percent = 89.0;
        assert!(matches!(
            pool.validate("pool"),
            Err(ConfigError::MigrationBelowAdmission { .. })
        ));
        pool.policy.migration_at_percent = 90.0;
        pool.policy.migration_advantage_percent = 0.0;
        pool.policy.sticky_warm_seconds = 0;
        assert!(pool.validate("pool").is_ok());
    }

    #[test]
    fn rejects_per_account_tuning_and_unknown_policy_fields() {
        assert!(
            serde_json::from_value::<SubscriptionAccountConfig>(serde_json::json!({
                "id": "a",
                "credential_source": { "claude_config_dir": "/tmp/a" },
                "priority": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PoolPolicy>(serde_json::json!({
                "reserve_percent": 10
            }))
            .is_err()
        );
    }

    #[test]
    fn existing_high_admission_policy_loads_and_explicit_inversion_rejects() {
        let mut pool: SubscriptionPoolConfig = serde_json::from_value(serde_json::json!({
            "control_config_dir": "/tmp/control",
            "accounts": [{
                "id": "a",
                "credential_source": { "claude_config_dir": "/tmp/a" }
            }],
            "policy": { "switch_at_percent": 98 }
        }))
        .expect("existing pool config");
        assert!(pool.validate("pool").is_ok());
        assert_eq!(pool.policy.migration_at_percent, 98.0);
        pool.policy = serde_json::from_value(serde_json::json!({
            "switch_at_percent": 98,
            "migration_at_percent": 95
        }))
        .expect("explicit policy");
        assert!(matches!(
            pool.validate("pool"),
            Err(ConfigError::MigrationBelowAdmission { .. })
        ));
    }
}
