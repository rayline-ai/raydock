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
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SubscriptionAccountConfig {
    pub id: String,
    pub credential_source: CredentialSourceConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CredentialSourceConfig {
    pub claude_config_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PoolPolicy {
    #[serde(default)]
    pub billing: BillingPolicy,
    #[serde(default = "default_switch_at_percent")]
    pub switch_at_percent: f64,
    #[serde(default)]
    pub sticky: StickyPolicy,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            billing: BillingPolicy::default(),
            switch_at_percent: default_switch_at_percent(),
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
}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), ConfigError> {
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
}
