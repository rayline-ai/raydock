use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::oauth::unix_now_ms;
use crate::{
    AccountLimitState, AccountPlacementLoad, CredentialDocument, CredentialError, CredentialHealth,
    CredentialStore, Entitlement, ExtraUsageState, HeaderSnapshot, LimitClaim, ModelFamily,
    OAuthRefreshClient, OAuthRefreshError, PoolPolicy, SESSION_STATUS_SCHEMA, SecretString,
    SelectionRequest, SessionAssignmentKind, SessionAssignmentReason, SessionAssignmentStatus,
    SessionCapacityStatus, SessionLimitStatus, SessionPlacementStatus, SessionStatusSnapshot,
    SubscriptionPoolConfig, UsageSnapshot, normalize_unified_extra_usage,
    normalize_unified_headers, select_account,
};

const DEFAULT_USAGE_PATH: &str = "/api/oauth/usage";
const USAGE_BETA: &str = "oauth-2025-04-20";
const MAX_USAGE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_AFFINITY_ENTRIES: usize = 4096;
const DEFAULT_ACTIVE_LEASE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub struct SubscriptionRuntimeOptions {
    pub anthropic_base_url: String,
    pub token_url: String,
    pub oauth_client_id: String,
    pub request_timeout: Duration,
    pub poll_interval: Duration,
    pub near_limit_poll_interval: Duration,
    pub refresh_margin: Duration,
    pub active_lease_ttl: Duration,
    pub trusted_ca_pem: Option<Vec<u8>>,
    pub home_dir: Option<PathBuf>,
}

impl Default for SubscriptionRuntimeOptions {
    fn default() -> Self {
        Self {
            anthropic_base_url: "https://api.anthropic.com".to_owned(),
            token_url: crate::DEFAULT_CLAUDE_TOKEN_URL.to_owned(),
            oauth_client_id: crate::DEFAULT_CLAUDE_OAUTH_CLIENT_ID.to_owned(),
            request_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_secs(5 * 60),
            near_limit_poll_interval: Duration::from_secs(45),
            refresh_margin: Duration::from_secs(120),
            active_lease_ttl: DEFAULT_ACTIVE_LEASE_TTL,
            trusted_ca_pem: None,
            home_dir: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

pub struct SubscriptionPoolRuntime {
    pool_id: String,
    policy: PoolPolicy,
    accounts: Vec<Arc<AccountWorker>>,
    placement: Mutex<PlacementState>,
    monitor_started: AtomicBool,
    poll_interval: Duration,
    near_limit_poll_interval: Duration,
    active_lease_ttl: Duration,
}

impl SubscriptionPoolRuntime {
    pub async fn start(
        pool_id: impl Into<String>,
        config: SubscriptionPoolConfig,
        options: SubscriptionRuntimeOptions,
    ) -> Result<Arc<Self>, SubscriptionRuntimeError> {
        let pool_id = pool_id.into();
        let mut http = reqwest::Client::builder()
            .timeout(options.request_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = options.trusted_ca_pem.as_deref() {
            let certificate = reqwest::Certificate::from_pem(pem)
                .map_err(SubscriptionRuntimeError::HttpClient)?;
            http = http.add_root_certificate(certificate);
        }
        let http = http.build().map_err(SubscriptionRuntimeError::HttpClient)?;
        let refresh_client =
            OAuthRefreshClient::new(http.clone(), &options.token_url, &options.oauth_client_id)?;
        let refresh_margin_ms = duration_ms(options.refresh_margin);
        let usage_url = format!(
            "{}{}",
            options.anthropic_base_url.trim_end_matches('/'),
            DEFAULT_USAGE_PATH
        );
        let mut accounts = Vec::with_capacity(config.accounts.len());
        let mut credential_sources = HashSet::new();
        for account in config.accounts {
            let path = expand_home_path(
                &account.credential_source.claude_config_dir,
                options.home_dir.as_deref(),
            )?;
            let store = CredentialStore::new(path).map_err(|source| {
                SubscriptionRuntimeError::CredentialSource {
                    account: account.id.clone(),
                    source,
                }
            })?;
            if !credential_sources.insert(store.config_dir().to_owned()) {
                return Err(SubscriptionRuntimeError::DuplicateCredentialSource {
                    path: store.config_dir().to_owned(),
                });
            }
            accounts.push(Arc::new(AccountWorker::new(
                account.id,
                store,
                http.clone(),
                refresh_client.clone(),
                usage_url.clone(),
                refresh_margin_ms,
            )));
        }

        let runtime = Arc::new(Self {
            pool_id,
            policy: config.policy,
            accounts,
            placement: Mutex::new(PlacementState::default()),
            monitor_started: AtomicBool::new(false),
            poll_interval: options.poll_interval,
            near_limit_poll_interval: options.near_limit_poll_interval,
            active_lease_ttl: options.active_lease_ttl,
        });
        join_all(runtime.accounts.iter().map(|account| account.load()))
            .await
            .into_iter()
            .for_each(drop);
        runtime.refresh_all_usage().await;

        if !runtime
            .accounts
            .iter()
            .any(|account| account.state().credential_health == CredentialHealth::Healthy)
        {
            return Err(SubscriptionRuntimeError::NoUsableCredentials {
                pool: runtime.pool_id.clone(),
            });
        }
        Ok(runtime)
    }

    pub fn pool_id(&self) -> &str {
        &self.pool_id
    }

    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    pub fn spawn_monitor(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if self.monitor_started.swap(true, Ordering::SeqCst) {
            return None;
        }
        let runtime = Arc::clone(self);
        Some(tokio::spawn(async move {
            loop {
                let interval = if runtime.near_limit() {
                    runtime.near_limit_poll_interval
                } else {
                    runtime.poll_interval
                };
                tokio::time::sleep(interval).await;
                runtime.refresh_all_usage().await;
            }
        }))
    }

    pub async fn refresh_all_usage(&self) {
        join_all(self.accounts.iter().map(|account| account.refresh_usage())).await;
    }

    /// Re-reads every credential source, so a profile the user signed in to
    /// again is picked up without restarting the daemon. One broken source
    /// never fails the call: each account reports its own outcome, which is
    /// what the daemon status endpoint shows the user.
    pub async fn reload_credentials(&self) -> CredentialReloadSummary {
        CredentialReloadSummary {
            pool_id: self.pool_id.clone(),
            accounts: join_all(
                self.accounts
                    .iter()
                    .map(|account| account.reload_credential()),
            )
            .await,
        }
    }

    pub async fn select(
        &self,
        launch_id: &str,
        requested_model: &str,
        excluded_accounts: &HashSet<String>,
    ) -> Result<SelectedSubscription, SubscriptionRuntimeError> {
        let model = ModelFamily::from_requested_model(requested_model);
        let key = AffinityKey {
            launch_id: launch_id.to_owned(),
            model: model.clone(),
        };
        let mut excluded = excluded_accounts.clone();

        loop {
            let states = self
                .accounts
                .iter()
                .map(|account| account.state())
                .filter(|state| !excluded.contains(&state.account_id))
                .collect::<Vec<_>>();
            if !states
                .iter()
                .any(|state| state.credential_health == CredentialHealth::Healthy)
            {
                return Err(SubscriptionRuntimeError::NoUsableCredentials {
                    pool: self.pool_id.clone(),
                });
            }
            let (account_id, placement_score, active_global_leases, active_model_leases) = {
                let now = Instant::now();
                let mut placement = self
                    .placement
                    .lock()
                    .expect("subscription placement lock poisoned");
                placement.prune_active_leases(now, self.active_lease_ttl);
                let affinity_account_id = placement
                    .preferred_account(&key)
                    .filter(|account| !excluded.contains(*account))
                    .map(ToOwned::to_owned);
                let placement_loads = placement.loads_for(&key);
                let decision = select_account(
                    &states,
                    &SelectionRequest {
                        model: model.clone(),
                        affinity_account_id,
                        launch_id: launch_id.to_owned(),
                        placement_loads,
                    },
                    &self.policy,
                );
                let Some(account_id) = decision.selected_account_id else {
                    return Err(SubscriptionRuntimeError::NoEligibleAccount {
                        pool: self.pool_id.clone(),
                        model: model.to_string(),
                    });
                };
                let evaluation = decision
                    .evaluations
                    .iter()
                    .find(|evaluation| evaluation.account_id == account_id);
                let placement_score = evaluation.and_then(|evaluation| evaluation.placement_score);
                let active_global_leases = evaluation
                    .map(|evaluation| evaluation.active_global_leases)
                    .unwrap_or_default();
                let active_model_leases = evaluation
                    .map(|evaluation| evaluation.active_model_leases)
                    .unwrap_or_default();
                placement.reserve(key.clone(), account_id.clone(), now);
                (
                    account_id,
                    placement_score,
                    active_global_leases,
                    active_model_leases,
                )
            };
            let worker = self
                .account(&account_id)
                .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.clone()))?;
            match worker.access_token(false).await {
                Ok(access_token) => {
                    let assignment_reason = self
                        .placement
                        .lock()
                        .expect("subscription placement lock poisoned")
                        .commit(key, account_id.clone(), Instant::now());
                    return Ok(SelectedSubscription {
                        account_id,
                        access_token,
                        model,
                        assignment_reason,
                        placement_score,
                        active_global_leases,
                        active_model_leases,
                    });
                }
                Err(error) => {
                    self.placement
                        .lock()
                        .expect("subscription placement lock poisoned")
                        .rollback_provisional(&key, &account_id);
                    worker.set_last_error(error.to_string());
                    worker.set_credential_health(CredentialHealth::Unavailable);
                    excluded.insert(account_id);
                }
            }
        }
    }

    pub async fn force_refresh(
        &self,
        account_id: &str,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        self.account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?
            .access_token(true)
            .await
    }

    pub async fn refresh_after_unauthorized(
        &self,
        account_id: &str,
        rejected_token: &SecretString,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        self.account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?
            .refresh_after_unauthorized(rejected_token)
            .await
    }

    pub fn observe_response_headers(
        &self,
        account_id: &str,
        requested_model: &str,
        headers: &HeaderSnapshot,
    ) -> Result<(), SubscriptionRuntimeError> {
        let account = self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?;
        let model = ModelFamily::from_requested_model(requested_model);
        let claims = normalize_unified_headers(headers, Some(&model));
        let extra_usage = normalize_unified_extra_usage(headers);
        account.apply_response_observation(claims, extra_usage);
        Ok(())
    }

    pub fn mark_entitlement_unavailable(
        &self,
        account_id: &str,
        requested_model: &str,
    ) -> Result<(), SubscriptionRuntimeError> {
        let account = self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?;
        account.set_entitlement(
            ModelFamily::from_requested_model(requested_model),
            Entitlement::Unavailable,
        );
        Ok(())
    }

    pub fn mark_credential_unavailable(
        &self,
        account_id: &str,
        error: impl Into<String>,
    ) -> Result<(), SubscriptionRuntimeError> {
        let account = self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?;
        account.set_credential_health(CredentialHealth::Unavailable);
        account.set_last_error(error.into());
        Ok(())
    }

    pub fn clear_affinity(&self, launch_id: &str, requested_model: &str, account_id: &str) {
        let key = AffinityKey {
            launch_id: launch_id.to_owned(),
            model: ModelFamily::from_requested_model(requested_model),
        };
        self.placement
            .lock()
            .expect("subscription placement lock poisoned")
            .clear_model_assignment(&key, account_id);
    }

    pub fn session_status(
        &self,
        launch_id: &str,
        requested_model: &str,
        current_account_id: &str,
        reason: SessionAssignmentReason,
    ) -> Result<SessionStatusSnapshot, SubscriptionRuntimeError> {
        let model = ModelFamily::from_requested_model(requested_model);
        let key = AffinityKey {
            launch_id: launch_id.to_owned(),
            model: model.clone(),
        };
        let state = self
            .account(current_account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(current_account_id.to_owned()))?
            .state();
        let account_states = self
            .accounts
            .iter()
            .map(|account| account.state())
            .collect::<Vec<_>>();
        let now = now_unix_seconds();
        let (
            primary_account_id,
            assigned_at_unix,
            last_seen_at_unix,
            placement_load,
            eligible_accounts,
        ) = {
            let placement = self
                .placement
                .lock()
                .expect("subscription placement lock poisoned");
            let primary_account_id = placement
                .primaries
                .get(launch_id)
                .map(|entry| entry.account_id.clone())
                .unwrap_or_else(|| current_account_id.to_owned());
            let lease = placement.active_leases.get(&key);
            let decision = select_account(
                &account_states,
                &SelectionRequest {
                    model: model.clone(),
                    affinity_account_id: None,
                    launch_id: launch_id.to_owned(),
                    placement_loads: placement.loads_for(&key),
                },
                &self.policy,
            );
            (
                primary_account_id,
                lease.map(|lease| lease.assigned_at_unix).unwrap_or(now),
                lease.map(|lease| lease.last_seen_at_unix).unwrap_or(now),
                placement.status_load_for(&key, current_account_id),
                decision
                    .evaluations
                    .iter()
                    .filter(|evaluation| evaluation.eligible)
                    .count(),
            )
        };

        let applicable = state
            .claims
            .iter()
            .filter(|claim| claim.applies_to(&model) && !claim.reset_has_passed())
            .filter_map(|claim| {
                let used_fraction = claim.utilization?.clamp(0.0, 1.0);
                Some(SessionLimitStatus {
                    key: claim.key.clone(),
                    scope: match &claim.scope {
                        crate::ClaimScope::Global => "global".to_owned(),
                        crate::ClaimScope::Model(family) => format!("model:{family}"),
                        crate::ClaimScope::Surface(surface) => format!("surface:{surface}"),
                        crate::ClaimScope::Unknown => "unknown".to_owned(),
                    },
                    used_fraction,
                    remaining_fraction: 1.0 - used_fraction,
                    resets_at: claim.resets_at.clone(),
                })
            })
            .collect::<Vec<_>>();
        let bottleneck = applicable
            .iter()
            .min_by(|left, right| left.remaining_fraction.total_cmp(&right.remaining_fraction))
            .cloned();
        let effective_headroom = (state.usage_snapshot_fresh && state.complete_global_snapshot)
            .then(|| {
                applicable
                    .iter()
                    .map(|limit| limit.remaining_fraction)
                    .reduce(f64::min)
                    .unwrap_or(1.0)
            });
        let placement_score =
            (state.usage_snapshot_fresh && state.complete_global_snapshot).then(|| {
                applicable
                    .iter()
                    .map(|limit| {
                        let active_leases = if limit.scope == "global" {
                            placement_load.active_global_leases
                        } else if limit.scope.starts_with("model:") {
                            placement_load.active_model_leases
                        } else {
                            0
                        };
                        limit.remaining_fraction / (1 + active_leases) as f64
                    })
                    .reduce(f64::min)
                    .unwrap_or_else(|| 1.0 / (1 + placement_load.active_global_leases) as f64)
            });
        let kind = if primary_account_id == current_account_id {
            SessionAssignmentKind::Primary
        } else {
            SessionAssignmentKind::ModelOverride
        };

        Ok(SessionStatusSnapshot {
            schema: SESSION_STATUS_SCHEMA,
            pool_id: self.pool_id.clone(),
            assignment: SessionAssignmentStatus {
                primary_account_id,
                current_account_id: current_account_id.to_owned(),
                current_model_family: model.to_string(),
                kind,
                reason,
                assigned_at_unix,
                last_seen_at_unix,
            },
            capacity: SessionCapacityStatus {
                usage_snapshot_fresh: state.usage_snapshot_fresh,
                effective_headroom,
                bottleneck,
                applicable,
                eligible_accounts,
                total_accounts: account_states.len(),
            },
            placement: SessionPlacementStatus {
                strategy: "balanced_sessions".to_owned(),
                score: placement_score,
                active_global_leases: placement_load.active_global_leases,
                active_model_leases: placement_load.active_model_leases,
            },
            route: None,
            updated_at_unix: now,
        })
    }

    pub fn status(&self) -> PoolRuntimeStatus {
        let placement = {
            let mut placement = self
                .placement
                .lock()
                .expect("subscription placement lock poisoned");
            placement.prune_active_leases(Instant::now(), self.active_lease_ttl);
            placement.runtime_status(
                self.accounts.iter().map(|account| account.id.as_str()),
                self.active_lease_ttl,
            )
        };
        PoolRuntimeStatus {
            pool_id: self.pool_id.clone(),
            accounts: self
                .accounts
                .iter()
                .map(|account| account.status())
                .collect(),
            placement: Some(placement),
        }
    }

    pub fn status_without_live_placement(&self) -> PoolRuntimeStatus {
        let mut status = self.status();
        status.placement = None;
        status
    }

    fn account(&self, account_id: &str) -> Option<&Arc<AccountWorker>> {
        self.accounts
            .iter()
            .find(|account| account.id == account_id)
    }

    fn near_limit(&self) -> bool {
        let threshold = self.policy.switch_at_fraction();
        self.accounts.iter().any(|account| {
            account.state().claims.iter().any(|claim| {
                !claim.reset_has_passed()
                    && (claim.status == crate::ClaimStatus::Rejected
                        || claim
                            .utilization
                            .is_some_and(|utilization| utilization >= threshold))
            })
        })
    }
}

impl std::fmt::Debug for SubscriptionPoolRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionPoolRuntime")
            .field("pool_id", &self.pool_id)
            .field("account_count", &self.accounts.len())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

pub struct SelectedSubscription {
    pub account_id: String,
    pub access_token: SecretString,
    pub model: ModelFamily,
    pub assignment_reason: SessionAssignmentReason,
    pub placement_score: Option<f64>,
    pub active_global_leases: usize,
    pub active_model_leases: usize,
}

impl std::fmt::Debug for SelectedSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SelectedSubscription")
            .field("account_id", &self.account_id)
            .field("access_token", &"[REDACTED]")
            .field("model", &self.model)
            .field("assignment_reason", &self.assignment_reason)
            .field("placement_score", &self.placement_score)
            .field("active_global_leases", &self.active_global_leases)
            .field("active_model_leases", &self.active_model_leases)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PoolRuntimeStatus {
    pub pool_id: String,
    pub accounts: Vec<AccountRuntimeStatus>,
    pub placement: Option<PoolPlacementRuntimeStatus>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AccountRuntimeStatus {
    pub id: String,
    pub config_dir: PathBuf,
    pub credential_health: CredentialHealth,
    pub subscription_type: Option<String>,
    pub usage_snapshot_fresh: bool,
    pub complete_global_snapshot: bool,
    pub claims: Vec<LimitClaim>,
    pub extra_usage: ExtraUsageState,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CredentialReloadSummary {
    pub pool_id: String,
    pub accounts: Vec<AccountCredentialReload>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AccountCredentialReload {
    pub id: String,
    pub previous_health: CredentialHealth,
    pub health: CredentialHealth,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PoolPlacementRuntimeStatus {
    pub active_lease_ttl_seconds: u64,
    pub accounts: Vec<AccountPlacementRuntimeStatus>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AccountPlacementRuntimeStatus {
    pub id: String,
    pub active_launch_leases: usize,
    pub active_model_leases: BTreeMap<String, usize>,
    pub primary_assignments: usize,
    pub model_overrides: usize,
}

struct AccountWorker {
    id: String,
    store: CredentialStore,
    credential: Mutex<Option<CredentialDocument>>,
    refresh_gate: Semaphore,
    state: RwLock<AccountLimitState>,
    subscription_type: RwLock<Option<String>>,
    last_error: RwLock<Option<String>>,
    http: reqwest::Client,
    refresh_client: OAuthRefreshClient,
    usage_url: String,
    refresh_margin_ms: i64,
}

impl AccountWorker {
    fn new(
        id: String,
        store: CredentialStore,
        http: reqwest::Client,
        refresh_client: OAuthRefreshClient,
        usage_url: String,
        refresh_margin_ms: i64,
    ) -> Self {
        Self {
            state: RwLock::new(AccountLimitState::unknown(id.clone())),
            id,
            store,
            credential: Mutex::new(None),
            refresh_gate: Semaphore::new(1),
            subscription_type: RwLock::new(None),
            last_error: RwLock::new(None),
            http,
            refresh_client,
            usage_url,
            refresh_margin_ms,
        }
    }

    async fn load(&self) -> Result<(), SubscriptionRuntimeError> {
        let _permit = self
            .refresh_gate
            .acquire()
            .await
            .map_err(|_| SubscriptionRuntimeError::RuntimeClosed)?;
        if self
            .credential
            .lock()
            .expect("subscription credential lock poisoned")
            .is_some()
        {
            return Ok(());
        }
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || store.load()).await {
            Ok(Ok(document)) => {
                *self
                    .subscription_type
                    .write()
                    .expect("subscription type lock poisoned") =
                    document.subscription_type().map(str::to_owned);
                *self
                    .credential
                    .lock()
                    .expect("subscription credential lock poisoned") = Some(document);
                self.set_credential_health(CredentialHealth::Healthy);
                self.clear_last_error();
                Ok(())
            }
            Ok(Err(source)) => {
                self.set_credential_health(CredentialHealth::Unavailable);
                self.set_last_error(source.to_string());
                Err(SubscriptionRuntimeError::CredentialSource {
                    account: self.id.clone(),
                    source,
                })
            }
            Err(source) => {
                self.set_credential_health(CredentialHealth::Unavailable);
                self.set_last_error("credential loader failed".to_owned());
                Err(SubscriptionRuntimeError::BackgroundTask(source))
            }
        }
    }

    async fn access_token(
        &self,
        force_refresh: bool,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        self.load().await?;
        let needs_refresh = {
            let credential = self
                .credential
                .lock()
                .expect("subscription credential lock poisoned");
            let document = credential
                .as_ref()
                .ok_or_else(|| SubscriptionRuntimeError::CredentialUnavailable(self.id.clone()))?;
            force_refresh
                || document.expires_at_unix_ms()?
                    <= unix_now_ms().saturating_add(self.refresh_margin_ms)
        };
        if !needs_refresh {
            return self.current_access_token();
        }

        let _permit = self
            .refresh_gate
            .acquire()
            .await
            .map_err(|_| SubscriptionRuntimeError::RuntimeClosed)?;
        let needs_refresh = {
            let credential = self
                .credential
                .lock()
                .expect("subscription credential lock poisoned");
            let document = credential
                .as_ref()
                .ok_or_else(|| SubscriptionRuntimeError::CredentialUnavailable(self.id.clone()))?;
            force_refresh
                || document.expires_at_unix_ms()?
                    <= unix_now_ms().saturating_add(self.refresh_margin_ms)
        };
        if !needs_refresh {
            return self.current_access_token();
        }

        self.refresh_loaded_credential().await
    }

    async fn refresh_after_unauthorized(
        &self,
        rejected_token: &SecretString,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        self.load().await?;
        let _permit = self
            .refresh_gate
            .acquire()
            .await
            .map_err(|_| SubscriptionRuntimeError::RuntimeClosed)?;
        let current = self.current_access_token()?;
        if current != *rejected_token {
            return Ok(current);
        }
        self.refresh_loaded_credential().await
    }

    async fn refresh_loaded_credential(&self) -> Result<SecretString, SubscriptionRuntimeError> {
        let mut document = self
            .credential
            .lock()
            .expect("subscription credential lock poisoned")
            .take()
            .ok_or_else(|| SubscriptionRuntimeError::CredentialUnavailable(self.id.clone()))?;
        self.set_credential_health(CredentialHealth::Refreshing);
        let mut reloaded_after_invalid_grant = false;
        loop {
            match self.refresh_client.refresh_document(&mut document).await {
                Ok(()) => return self.persist_refreshed_credential(document),
                Err(OAuthRefreshError::InvalidGrant) if !reloaded_after_invalid_grant => {
                    match self.reload_changed_credential(&document).await {
                        Ok(Some(latest)) => {
                            document = latest;
                            self.update_subscription_type(&document);
                            if document.expires_at_unix_ms().is_ok_and(|expires_at| {
                                expires_at > unix_now_ms().saturating_add(self.refresh_margin_ms)
                            }) {
                                return self.activate_credential(document);
                            }
                            reloaded_after_invalid_grant = true;
                        }
                        Ok(None) => {
                            return self.fail_refresh(document, OAuthRefreshError::InvalidGrant);
                        }
                        Err(error) => {
                            *self
                                .credential
                                .lock()
                                .expect("subscription credential lock poisoned") = Some(document);
                            self.set_credential_health(CredentialHealth::Unavailable);
                            self.set_last_error(format!(
                                "Claude OAuth refresh token was rejected and the credential source could not be reloaded: {error}"
                            ));
                            return Err(error);
                        }
                    }
                }
                Err(error) => return self.fail_refresh(document, error),
            }
        }
    }

    async fn reload_changed_credential(
        &self,
        stale: &CredentialDocument,
    ) -> Result<Option<CredentialDocument>, SubscriptionRuntimeError> {
        let latest = self.load_credential_from_store().await?;
        Ok((!stale.has_same_version(&latest)).then_some(latest))
    }

    async fn load_credential_from_store(
        &self,
    ) -> Result<CredentialDocument, SubscriptionRuntimeError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.load())
            .await
            .map_err(SubscriptionRuntimeError::BackgroundTask)?
            .map_err(|source| SubscriptionRuntimeError::CredentialSource {
                account: self.id.clone(),
                source,
            })
    }

    /// Whether the store holds a different document than the one in memory.
    /// The credential mutex is only held for the comparison, never across the
    /// load, so a slow store cannot block a request that needs the token.
    fn differs_from_loaded_credential(&self, latest: &CredentialDocument) -> bool {
        self.credential
            .lock()
            .expect("subscription credential lock poisoned")
            .as_ref()
            .is_none_or(|current| !current.has_same_version(latest))
    }

    fn persist_refreshed_credential(
        &self,
        mut document: CredentialDocument,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        if let Err(source) = self.store.save_if_unchanged(&mut document) {
            let reload_result = self.store.load();
            *self
                .credential
                .lock()
                .expect("subscription credential lock poisoned") = reload_result.ok();
            self.set_credential_health(CredentialHealth::Unavailable);
            self.set_last_error(source.to_string());
            return Err(source.into());
        }
        self.activate_credential(document)
    }

    fn activate_credential(
        &self,
        document: CredentialDocument,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        let access_token = document.access_token()?;
        self.update_subscription_type(&document);
        *self
            .credential
            .lock()
            .expect("subscription credential lock poisoned") = Some(document);
        self.set_credential_health(CredentialHealth::Healthy);
        self.clear_last_error();
        Ok(access_token)
    }

    fn fail_refresh(
        &self,
        document: CredentialDocument,
        error: OAuthRefreshError,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        *self
            .credential
            .lock()
            .expect("subscription credential lock poisoned") = Some(document);
        if matches!(error, OAuthRefreshError::InvalidGrant) {
            self.set_credential_health(CredentialHealth::Quarantined);
        } else {
            self.set_credential_health(CredentialHealth::Unavailable);
        }
        self.set_last_error(error.to_string());
        Err(error.into())
    }

    fn update_subscription_type(&self, document: &CredentialDocument) {
        *self
            .subscription_type
            .write()
            .expect("subscription type lock poisoned") =
            document.subscription_type().map(str::to_owned);
    }

    fn current_access_token(&self) -> Result<SecretString, SubscriptionRuntimeError> {
        self.credential
            .lock()
            .expect("subscription credential lock poisoned")
            .as_ref()
            .ok_or_else(|| SubscriptionRuntimeError::CredentialUnavailable(self.id.clone()))?
            .access_token()
            .map_err(Into::into)
    }

    /// Gives a quarantined account one way back into service: a credential
    /// document the store received after the refresh token was rejected, which
    /// is what a fresh sign-in writes. The rejected refresh token itself is
    /// dead, so this never contacts the OAuth endpoint. Returns `true` only
    /// when a new credential was activated and the caller may continue.
    async fn heal_quarantined_credential(&self) -> bool {
        let Ok(_permit) = self.refresh_gate.acquire().await else {
            return false;
        };
        if self.state().credential_health != CredentialHealth::Quarantined {
            return true;
        }
        let latest = match self.load_credential_from_store().await {
            Ok(latest) => latest,
            Err(error) => {
                self.set_last_error(error.to_string());
                return false;
            }
        };
        if !self.differs_from_loaded_credential(&latest) {
            return false;
        }
        match self.activate_credential(latest) {
            Ok(_access_token) => true,
            Err(error) => {
                self.set_last_error(error.to_string());
                false
            }
        }
    }

    async fn reload_credential(&self) -> AccountCredentialReload {
        let previous_health = self.state().credential_health;
        let (detail, activated) = self.reload_credential_source().await;
        if activated {
            self.refresh_usage().await;
        }
        AccountCredentialReload {
            id: self.id.clone(),
            previous_health,
            health: self.state().credential_health,
            detail,
        }
    }

    /// Loads the credential source once and reports what it means for this
    /// account. A source that cannot be read leaves health alone: the token
    /// already in memory may still work, and a status call must not evict a
    /// usable account. Returns whether a new credential became active.
    async fn reload_credential_source(&self) -> (String, bool) {
        let Ok(_permit) = self.refresh_gate.acquire().await else {
            return (SubscriptionRuntimeError::RuntimeClosed.to_string(), false);
        };
        let latest = match self.load_credential_from_store().await {
            Ok(latest) => latest,
            Err(error) => {
                let detail = format!("credential source could not be read: {error}");
                self.set_last_error(detail.clone());
                return (detail, false);
            }
        };
        if !self.differs_from_loaded_credential(&latest) {
            let detail = if self.state().credential_health == CredentialHealth::Healthy {
                "unchanged"
            } else {
                "credential source unchanged; sign in to this profile again"
            };
            return (detail.to_owned(), false);
        }
        match self.activate_credential(latest) {
            Ok(_access_token) => (
                "reloaded a new credential from the credential source".to_owned(),
                true,
            ),
            Err(error) => {
                let detail =
                    format!("credential source changed but could not be activated: {error}");
                self.set_last_error(detail.clone());
                (detail, false)
            }
        }
    }

    async fn refresh_usage(&self) {
        if self.state().credential_health == CredentialHealth::Quarantined
            && !self.heal_quarantined_credential().await
        {
            return;
        }
        let mut access_token = match self.access_token(false).await {
            Ok(access_token) => access_token,
            Err(error) => {
                self.mark_usage_stale(error.to_string());
                return;
            }
        };
        let mut response = self.send_usage_request(&access_token).await;
        if response
            .as_ref()
            .is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED)
        {
            match self.refresh_after_unauthorized(&access_token).await {
                Ok(refreshed) => {
                    access_token = refreshed;
                    response = self.send_usage_request(&access_token).await;
                }
                Err(error) => {
                    self.mark_usage_stale(error.to_string());
                    return;
                }
            }
        }

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.mark_usage_stale(format!(
                    "usage request failed: {}",
                    format_error_chain(&error)
                ));
                return;
            }
        };
        if !response.status().is_success() {
            self.mark_usage_stale(format!("usage request returned HTTP {}", response.status()));
            return;
        }
        let bytes = match read_bounded_usage_response(response).await {
            Ok(bytes) => bytes,
            Err(error) => {
                self.mark_usage_stale(error);
                return;
            }
        };
        let usage = match serde_json::from_slice::<UsageSnapshot>(&bytes) {
            Ok(usage) => usage,
            Err(error) => {
                self.mark_usage_stale(format!("usage response was invalid: {error}"));
                return;
            }
        };
        let mut normalized = usage.normalize();
        if normalized.extra_usage.enabled == Some(false) {
            normalized.extra_usage.in_use = Some(false);
        }
        let mut state = AccountLimitState::from_usage(self.id.clone(), normalized);
        let previous = self.state();
        state.entitlements = previous.entitlements;
        state.credential_health = previous.credential_health;
        *self
            .state
            .write()
            .expect("subscription state lock poisoned") = state;
        self.clear_last_error();
    }

    async fn send_usage_request(
        &self,
        access_token: &SecretString,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.http
            .get(&self.usage_url)
            .bearer_auth(access_token.expose())
            .header("anthropic-beta", USAGE_BETA)
            .header(reqwest::header::USER_AGENT, "claude-code/2.1.0")
            .send()
            .await
    }

    fn state(&self) -> AccountLimitState {
        self.state
            .read()
            .expect("subscription state lock poisoned")
            .clone()
    }

    fn set_credential_health(&self, health: CredentialHealth) {
        self.state
            .write()
            .expect("subscription state lock poisoned")
            .credential_health = health;
    }

    fn set_entitlement(&self, model: ModelFamily, entitlement: Entitlement) {
        self.state
            .write()
            .expect("subscription state lock poisoned")
            .entitlements
            .insert(model, entitlement);
    }

    fn apply_response_observation(&self, claims: Vec<LimitClaim>, extra_usage: ExtraUsageState) {
        self.state
            .write()
            .expect("subscription state lock poisoned")
            .apply_response_observation(claims, extra_usage);
    }

    fn mark_usage_stale(&self, error: String) {
        self.state
            .write()
            .expect("subscription state lock poisoned")
            .usage_snapshot_fresh = false;
        self.set_last_error(error);
    }

    fn set_last_error(&self, error: String) {
        *self
            .last_error
            .write()
            .expect("subscription error lock poisoned") = Some(error);
    }

    fn clear_last_error(&self) {
        *self
            .last_error
            .write()
            .expect("subscription error lock poisoned") = None;
    }

    fn status(&self) -> AccountRuntimeStatus {
        let state = self.state();
        AccountRuntimeStatus {
            id: self.id.clone(),
            config_dir: self.store.config_dir().to_owned(),
            credential_health: state.credential_health,
            subscription_type: self
                .subscription_type
                .read()
                .expect("subscription type lock poisoned")
                .clone(),
            usage_snapshot_fresh: state.usage_snapshot_fresh,
            complete_global_snapshot: state.complete_global_snapshot,
            claims: state.claims,
            extra_usage: state.extra_usage,
            last_error: self
                .last_error
                .read()
                .expect("subscription error lock poisoned")
                .clone(),
        }
    }
}

async fn read_bounded_usage_response(response: reqwest::Response) -> Result<Vec<u8>, String> {
    use futures::StreamExt as _;

    if response
        .content_length()
        .is_some_and(|length| length > MAX_USAGE_RESPONSE_BYTES as u64)
    {
        return Err("usage response exceeded the size limit".to_owned());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("usage response failed: {error}"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_USAGE_RESPONSE_BYTES {
            return Err("usage response exceeded the size limit".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Default)]
struct PlacementState {
    primaries: HashMap<String, AffinityEntry>,
    model_overrides: HashMap<AffinityKey, AffinityEntry>,
    active_leases: HashMap<AffinityKey, ActiveLease>,
    sequence: u64,
}

impl PlacementState {
    fn runtime_status<'a>(
        &self,
        account_ids: impl Iterator<Item = &'a str>,
        active_lease_ttl: Duration,
    ) -> PoolPlacementRuntimeStatus {
        let mut accounts = account_ids
            .map(|id| {
                (
                    id.to_owned(),
                    AccountPlacementRuntimeStatus {
                        id: id.to_owned(),
                        active_launch_leases: 0,
                        active_model_leases: BTreeMap::new(),
                        primary_assignments: 0,
                        model_overrides: 0,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut active_launches = HashSet::<(String, String)>::new();
        for (key, lease) in &self.active_leases {
            active_launches.insert((key.launch_id.clone(), lease.account_id.clone()));
            let account = accounts.entry(lease.account_id.clone()).or_insert_with(|| {
                AccountPlacementRuntimeStatus {
                    id: lease.account_id.clone(),
                    active_launch_leases: 0,
                    active_model_leases: BTreeMap::new(),
                    primary_assignments: 0,
                    model_overrides: 0,
                }
            });
            *account
                .active_model_leases
                .entry(key.model.to_string())
                .or_default() += 1;
        }
        for (_, account_id) in active_launches {
            if let Some(account) = accounts.get_mut(&account_id) {
                account.active_launch_leases += 1;
            }
        }
        for primary in self.primaries.values() {
            if let Some(account) = accounts.get_mut(&primary.account_id) {
                account.primary_assignments += 1;
            }
        }
        for model_override in self.model_overrides.values() {
            if let Some(account) = accounts.get_mut(&model_override.account_id) {
                account.model_overrides += 1;
            }
        }
        PoolPlacementRuntimeStatus {
            active_lease_ttl_seconds: active_lease_ttl.as_secs(),
            accounts: accounts.into_values().collect(),
        }
    }

    fn preferred_account(&self, key: &AffinityKey) -> Option<&str> {
        self.active_leases
            .get(key)
            .map(|lease| lease.account_id.as_str())
            .or_else(|| {
                self.model_overrides
                    .get(key)
                    .map(|entry| entry.account_id.as_str())
            })
            .or_else(|| {
                self.primaries
                    .get(&key.launch_id)
                    .map(|entry| entry.account_id.as_str())
            })
    }

    fn prune_active_leases(&mut self, now: Instant, ttl: Duration) {
        self.active_leases.retain(|_, lease| {
            now.checked_duration_since(lease.last_seen)
                .is_some_and(|age| age <= ttl)
        });
    }

    fn loads_for(&self, current: &AffinityKey) -> BTreeMap<String, AccountPlacementLoad> {
        self.collect_loads(&current.model, Some(current))
    }

    fn status_load_for(&self, current: &AffinityKey, account_id: &str) -> AccountPlacementLoad {
        self.collect_loads(&current.model, None)
            .remove(account_id)
            .unwrap_or_default()
    }

    fn collect_loads(
        &self,
        model: &ModelFamily,
        excluded: Option<&AffinityKey>,
    ) -> BTreeMap<String, AccountPlacementLoad> {
        let mut loads = BTreeMap::<String, AccountPlacementLoad>::new();
        let mut global_leases = HashSet::<(String, String)>::new();

        for (key, lease) in &self.active_leases {
            if excluded == Some(key) {
                continue;
            }
            global_leases.insert((key.launch_id.clone(), lease.account_id.clone()));
            if key.model == *model {
                loads
                    .entry(lease.account_id.clone())
                    .or_default()
                    .active_model_leases += 1;
            }
        }
        for (_, account_id) in global_leases {
            loads.entry(account_id).or_default().active_global_leases += 1;
        }
        loads
    }

    fn reserve(&mut self, key: AffinityKey, account_id: String, now: Instant) {
        let now_unix = now_unix_seconds();
        match self.active_leases.get_mut(&key) {
            Some(existing) if existing.account_id == account_id => {
                existing.last_seen = now;
                existing.last_seen_at_unix = now_unix;
            }
            _ => {
                self.active_leases.insert(
                    key,
                    ActiveLease {
                        account_id,
                        last_seen: now,
                        assigned_at_unix: now_unix,
                        last_seen_at_unix: now_unix,
                        provisional: true,
                    },
                );
            }
        }
    }

    fn commit(
        &mut self,
        key: AffinityKey,
        account_id: String,
        now: Instant,
    ) -> SessionAssignmentReason {
        self.sequence = self.sequence.saturating_add(1);
        let sequence = self.sequence;
        let now_unix = now_unix_seconds();
        let assigned_at_unix = self
            .active_leases
            .get(&key)
            .filter(|lease| lease.account_id == account_id)
            .map(|lease| lease.assigned_at_unix)
            .unwrap_or(now_unix);
        let primary_account = self
            .primaries
            .get(&key.launch_id)
            .map(|entry| entry.account_id.clone());

        let assignment_reason = match primary_account {
            None => {
                self.primaries.insert(
                    key.launch_id.clone(),
                    AffinityEntry {
                        account_id: account_id.clone(),
                        last_seen_sequence: sequence,
                    },
                );
                self.model_overrides.remove(&key);
                SessionAssignmentReason::BalancedNewLaunch
            }
            Some(primary) if primary == account_id => {
                if let Some(entry) = self.primaries.get_mut(&key.launch_id) {
                    entry.last_seen_sequence = sequence;
                }
                self.model_overrides.remove(&key);
                SessionAssignmentReason::PrimaryAffinity
            }
            Some(_) => {
                self.model_overrides.insert(
                    key.clone(),
                    AffinityEntry {
                        account_id: account_id.clone(),
                        last_seen_sequence: sequence,
                    },
                );
                SessionAssignmentReason::ModelOverride
            }
        };

        self.active_leases.insert(
            key,
            ActiveLease {
                account_id,
                last_seen: now,
                assigned_at_unix,
                last_seen_at_unix: now_unix,
                provisional: false,
            },
        );
        self.enforce_affinity_bound();
        assignment_reason
    }

    fn rollback_provisional(&mut self, key: &AffinityKey, account_id: &str) {
        if self
            .active_leases
            .get(key)
            .is_some_and(|lease| lease.provisional && lease.account_id.as_str() == account_id)
        {
            self.active_leases.remove(key);
        }
    }

    fn clear_model_assignment(&mut self, key: &AffinityKey, account_id: &str) {
        if self
            .active_leases
            .get(key)
            .is_some_and(|lease| lease.account_id == account_id)
        {
            self.active_leases.remove(key);
        }
        if self
            .model_overrides
            .get(key)
            .is_some_and(|entry| entry.account_id == account_id)
        {
            self.model_overrides.remove(key);
        }
    }

    fn enforce_affinity_bound(&mut self) {
        while self.primaries.len() + self.model_overrides.len() > MAX_AFFINITY_ENTRIES {
            let oldest_primary = self
                .primaries
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen_sequence)
                .map(|(launch_id, entry)| (launch_id.clone(), entry.last_seen_sequence));
            let oldest_override = self
                .model_overrides
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen_sequence)
                .map(|(key, entry)| (key.clone(), entry.last_seen_sequence));

            match (oldest_primary, oldest_override) {
                (Some((launch_id, primary_sequence)), Some((key, override_sequence))) => {
                    if primary_sequence <= override_sequence {
                        self.primaries.remove(&launch_id);
                    } else {
                        self.model_overrides.remove(&key);
                    }
                }
                (Some((launch_id, _)), None) => {
                    self.primaries.remove(&launch_id);
                }
                (None, Some((key, _))) => {
                    self.model_overrides.remove(&key);
                }
                (None, None) => break,
            }
        }
    }
}

struct AffinityEntry {
    account_id: String,
    last_seen_sequence: u64,
}

struct ActiveLease {
    account_id: String,
    last_seen: Instant,
    assigned_at_unix: i64,
    last_seen_at_unix: i64,
    provisional: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AffinityKey {
    launch_id: String,
    model: ModelFamily,
}

#[derive(Debug, Error)]
pub enum SubscriptionRuntimeError {
    #[error("subscription pool HTTP client setup failed: {0}")]
    HttpClient(#[source] reqwest::Error),
    #[error("subscription account {account:?} credential failed: {source}")]
    CredentialSource {
        account: String,
        #[source]
        source: CredentialError,
    },
    #[error("Claude credential failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("Claude OAuth refresh failed: {0}")]
    Refresh(#[from] OAuthRefreshError),
    #[error("subscription worker task failed: {0}")]
    BackgroundTask(#[source] tokio::task::JoinError),
    #[error("subscription runtime is closed")]
    RuntimeClosed,
    #[error("subscription credential is unavailable for account {0:?}")]
    CredentialUnavailable(String),
    #[error("subscription pool {pool:?} has no usable credentials")]
    NoUsableCredentials { pool: String },
    #[error("subscription pool {pool:?} has no eligible account for model {model:?}")]
    NoEligibleAccount { pool: String, model: String },
    #[error("unknown subscription account {0:?}")]
    UnknownAccount(String),
    #[error("subscription pool resolves more than one account to credential source {path}")]
    DuplicateCredentialSource { path: PathBuf },
    #[error("cannot expand home-relative credential path {0}")]
    HomeUnavailable(PathBuf),
}

fn expand_home_path(path: &Path, home: Option<&Path>) -> Result<PathBuf, SubscriptionRuntimeError> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home
            .map(Path::to_owned)
            .ok_or_else(|| SubscriptionRuntimeError::HomeUnavailable(path.to_owned()));
    }
    if let Some(suffix) = raw.strip_prefix("~/") {
        return home
            .map(|home| home.join(suffix))
            .ok_or_else(|| SubscriptionRuntimeError::HomeUnavailable(path.to_owned()));
    }
    Ok(path.to_owned())
}

fn duration_ms(duration: Duration) -> i64 {
    duration.as_millis().try_into().unwrap_or(i64::MAX)
}

fn now_unix_seconds() -> i64 {
    unix_now_ms() / 1000
}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut output = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        output.push_str(": ");
        output.push_str(&error.to_string());
        source = error.source();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClaimScope;

    #[test]
    fn expands_only_explicit_home_relative_paths() {
        assert_eq!(
            expand_home_path(Path::new("~/.claude"), Some(Path::new("/home/test")))
                .expect("expanded"),
            PathBuf::from("/home/test/.claude")
        );
        assert_eq!(
            expand_home_path(Path::new("/tmp/.claude"), None).expect("absolute"),
            PathBuf::from("/tmp/.claude")
        );
    }

    #[test]
    fn selected_subscription_debug_redacts_access_token() {
        let selected = SelectedSubscription {
            account_id: "account".to_owned(),
            access_token: SecretString::new("secret"),
            model: ModelFamily::from_requested_model("sonnet"),
            assignment_reason: SessionAssignmentReason::BalancedNewLaunch,
            placement_score: Some(0.5),
            active_global_leases: 1,
            active_model_leases: 1,
        };
        let debug = format!("{selected:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn account_status_contains_no_credentials() {
        let status = AccountRuntimeStatus {
            id: "account".to_owned(),
            config_dir: PathBuf::from("/tmp/profile"),
            credential_health: CredentialHealth::Healthy,
            subscription_type: Some("max".to_owned()),
            usage_snapshot_fresh: true,
            complete_global_snapshot: true,
            claims: vec![LimitClaim {
                key: "five_hour".to_owned(),
                scope: ClaimScope::Global,
                utilization: Some(0.2),
                status: crate::ClaimStatus::Allowed,
                resets_at: None,
                source: crate::LimitSource::UsageEndpoint,
            }],
            extra_usage: ExtraUsageState::default(),
            last_error: None,
        };
        let json = serde_json::to_string(&status).expect("status JSON");
        assert!(!json.contains("accessToken"));
        assert!(!json.contains("refreshToken"));
    }

    fn affinity_key(launch_id: &str, model: &str) -> AffinityKey {
        AffinityKey {
            launch_id: launch_id.to_owned(),
            model: ModelFamily::from_requested_model(model),
        }
    }

    #[test]
    fn placement_keeps_primary_and_scopes_override_to_one_model() {
        let mut placement = PlacementState::default();
        let now = Instant::now();
        let sonnet = affinity_key("launch", "sonnet");
        placement.reserve(sonnet.clone(), "a".to_owned(), now);
        assert_eq!(
            placement.commit(sonnet.clone(), "a".to_owned(), now),
            SessionAssignmentReason::BalancedNewLaunch
        );

        let fable = affinity_key("launch", "fable");
        placement.reserve(fable.clone(), "b".to_owned(), now);
        assert_eq!(
            placement.commit(fable.clone(), "b".to_owned(), now),
            SessionAssignmentReason::ModelOverride
        );
        assert_eq!(placement.preferred_account(&sonnet), Some("a"));
        assert_eq!(placement.preferred_account(&fable), Some("b"));

        placement.clear_model_assignment(&fable, "b");
        assert_eq!(placement.preferred_account(&fable), Some("a"));
        assert_eq!(placement.preferred_account(&sonnet), Some("a"));
    }

    #[test]
    fn placement_counts_global_launches_once_and_model_leases_separately() {
        let mut placement = PlacementState::default();
        let now = Instant::now();
        for key in [
            affinity_key("launch-one", "sonnet"),
            affinity_key("launch-one", "fable"),
            affinity_key("launch-two", "sonnet"),
        ] {
            placement.reserve(key.clone(), "a".to_owned(), now);
            placement.commit(key, "a".to_owned(), now);
        }

        let loads = placement.loads_for(&affinity_key("new-launch", "sonnet"));
        let account = loads.get("a").expect("account load");
        assert_eq!(account.active_global_leases, 2);
        assert_eq!(account.active_model_leases, 2);

        let status = placement.runtime_status(["a", "b"].into_iter(), Duration::from_secs(900));
        assert_eq!(status.active_lease_ttl_seconds, 900);
        let account_a = status
            .accounts
            .iter()
            .find(|account| account.id == "a")
            .expect("account a placement");
        assert_eq!(account_a.active_launch_leases, 2);
        assert_eq!(account_a.active_model_leases["sonnet"], 2);
        assert_eq!(account_a.active_model_leases["fable"], 1);
        assert_eq!(account_a.primary_assignments, 2);
        assert_eq!(account_a.model_overrides, 0);
    }

    #[test]
    fn expired_active_lease_stops_contributing_without_losing_affinity() {
        let mut placement = PlacementState::default();
        let now = Instant::now();
        let key = affinity_key("launch", "sonnet");
        placement.reserve(key.clone(), "a".to_owned(), now);
        placement.commit(key.clone(), "a".to_owned(), now);

        placement.prune_active_leases(now + Duration::from_secs(901), Duration::from_secs(900));
        assert!(
            placement
                .loads_for(&affinity_key("other", "sonnet"))
                .is_empty()
        );
        assert_eq!(placement.preferred_account(&key), Some("a"));
    }
}
