use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::oauth::unix_now_ms;
use crate::persistence::{AccountBlocks, BlockStore, StoreAccount};
use crate::placement::{AffinityKey, PlacementReservation, PlacementState};
use crate::resets::{LimitResetProgram, LimitResetStatus, PROGRAMS, parse_program};
use crate::routing_state::{ObservationToken, RoutingState};
use crate::{
    AccountLimitState, CredentialDocument, CredentialError, CredentialHealth, CredentialStore,
    ExtraUsageState, HeaderSnapshot, LimitClaim, ModelFamily, OAuthRefreshClient,
    OAuthRefreshError, PoolPolicy, SESSION_STATUS_SCHEMA, SecretString, SelectionRequest,
    SessionAssignmentKind, SessionAssignmentReason, SessionAssignmentStatus, SessionCapacityStatus,
    SessionLimitStatus, SessionPlacementStatus, SessionStatusSnapshot, SubscriptionPoolConfig,
    UsageSnapshot, describe_ineligibility, normalize_unified_extra_usage,
    normalize_unified_headers, select_account,
};

const DEFAULT_USAGE_PATH: &str = "/api/oauth/usage";
const USAGE_BETA: &str = "oauth-2025-04-20";
const MAX_USAGE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
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
    pub read_only: bool,
    /// Opt into a one-shot usage GET with existing, unexpired credentials.
    /// Only applies to read-only runtimes; never rotates or persists tokens.
    pub poll_usage_read_only: bool,
    pub state_dir: Option<PathBuf>,
    pub recovery_backoff: Duration,
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
            read_only: false,
            poll_usage_read_only: false,
            state_dir: None,
            recovery_backoff: Duration::from_secs(60),
        }
    }
}

pub struct SubscriptionPoolRuntime {
    pool_id: String,
    policy: PoolPolicy,
    accounts: Vec<Arc<AccountWorker>>,
    placement: Arc<Mutex<PlacementState>>,
    read_only: bool,
    block_store: Option<Arc<BlockStore>>,
    persistence_gate: Arc<tokio::sync::Mutex<()>>,
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
        if !is_credential_safe_base_url(&options.anthropic_base_url) {
            return Err(SubscriptionRuntimeError::InsecureBaseUrl);
        }
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
                WorkerRecoveryOptions {
                    refresh_margin_ms,
                    read_only: options.read_only,
                    recovery_backoff: options.recovery_backoff,
                },
            )));
        }

        let control_dir = config.control_config_dir.clone();
        let control_identity = tokio::task::spawn_blocking(move || {
            std::fs::canonicalize(&control_dir).unwrap_or(control_dir)
        })
        .await
        .map_err(SubscriptionRuntimeError::BackgroundTask)?;
        let pool_identity = format!(
            "{}|{}|{}",
            pool_id,
            control_identity.to_string_lossy(),
            serde_json::to_string(&config.policy).map_err(|_| {
                SubscriptionRuntimeError::RecoveryState("invalid pool identity".to_owned())
            })?
        );
        let runtime = Arc::new(Self {
            pool_id: pool_id.clone(),
            policy: config.policy,
            accounts,
            placement: Arc::new(Mutex::new(PlacementState::default())),
            read_only: options.read_only,
            block_store: match options.state_dir {
                Some(path) => {
                    if options.read_only {
                        None
                    } else {
                        let identity = pool_identity;
                        Some(Arc::new(
                            tokio::task::spawn_blocking(move || BlockStore::new(path, &identity))
                                .await
                                .map_err(SubscriptionRuntimeError::BackgroundTask)?
                                .map_err(|error| {
                                    SubscriptionRuntimeError::RecoveryState(error.to_string())
                                })?,
                        ))
                    }
                }
                None => None,
            },
            persistence_gate: Arc::new(tokio::sync::Mutex::new(())),
            monitor_started: AtomicBool::new(false),
            poll_interval: options.poll_interval,
            near_limit_poll_interval: options.near_limit_poll_interval,
            active_lease_ttl: options.active_lease_ttl,
        });
        join_all(runtime.accounts.iter().map(|account| account.load()))
            .await
            .into_iter()
            .for_each(drop);
        runtime.restore_blocks().await;
        if !runtime.read_only {
            runtime.refresh_all_usage().await;
        } else if options.poll_usage_read_only {
            join_all(
                runtime
                    .accounts
                    .iter()
                    .map(|account| account.refresh_usage()),
            )
            .await;
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
        if self.read_only || self.monitor_started.swap(true, Ordering::SeqCst) {
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
        if self.read_only {
            return;
        }
        join_all(self.accounts.iter().map(|account| account.refresh_usage())).await;
        if let Err(error) = self.flush_recovery_state().await {
            for account in &self.accounts {
                account.set_last_error(error.to_string());
            }
        }
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
        if self.read_only {
            return Err(SubscriptionRuntimeError::ReadOnly);
        }
        join_all(
            self.accounts
                .iter()
                .filter(|account| {
                    matches!(
                        account.state().credential_health,
                        CredentialHealth::Unavailable | CredentialHealth::Quarantined
                    )
                })
                .map(|account| account.reload_credential()),
        )
        .await;
        join_all(
            self.accounts
                .iter()
                .filter(|account| {
                    !excluded.contains(&account.id)
                        && account.persistence_pending.load(Ordering::SeqCst)
                })
                .map(|account| account.access_token(false)),
        )
        .await;

        loop {
            let states = self
                .accounts
                .iter()
                .map(|account| account.state())
                .filter(|state| !excluded.contains(&state.account_id))
                .collect::<Vec<_>>();
            if !states.iter().any(|state| {
                matches!(
                    state.credential_health,
                    CredentialHealth::Healthy | CredentialHealth::Refreshing
                )
            }) {
                return Err(SubscriptionRuntimeError::NoUsableCredentials {
                    pool: self.pool_id.clone(),
                });
            }
            let (
                account_id,
                placement_score,
                active_global_leases,
                active_model_leases,
                reservation,
            ) = {
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
                let affinity_warm = placement.affinity_warm(
                    &key,
                    now,
                    Duration::from_secs(self.policy.sticky_warm_seconds),
                ) && affinity_account_id
                    .as_deref()
                    .and_then(|id| self.account(id))
                    .is_some_and(|account| {
                        account
                            .state
                            .read()
                            .expect("state lock poisoned")
                            .snapshot_for_affinity(state_now_ms())
                            .usage_snapshot_fresh
                    });
                let placement_loads = placement.loads_for(&key);
                let decision = select_account(
                    &states,
                    &SelectionRequest {
                        model: model.clone(),
                        affinity_account_id,
                        launch_id: launch_id.to_owned(),
                        affinity_warm,
                        placement_loads,
                    },
                    &self.policy,
                );
                let candidate = decision.selected_account_id.clone().or_else(|| {
                    states
                        .iter()
                        .find(|state| {
                            matches!(
                                state.credential_health,
                                CredentialHealth::Healthy | CredentialHealth::Refreshing
                            ) && state.extra_usage.in_use != Some(true)
                                && self.account(&state.account_id).is_some_and(|account| {
                                    account
                                        .state
                                        .read()
                                        .expect("state lock poisoned")
                                        .probe_due(&model, state_now_ms())
                                })
                        })
                        .map(|state| state.account_id.clone())
                });
                let Some(account_id) = candidate else {
                    return Err(SubscriptionRuntimeError::NoEligibleAccount {
                        pool: self.pool_id.clone(),
                        model: model.to_string(),
                        detail: describe_ineligibility(&states, &decision.evaluations),
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
                let ticket = placement.reserve(key.clone(), account_id.clone(), now);
                let reservation = PlacementReservation::new(Arc::clone(&self.placement), ticket);
                (
                    account_id,
                    placement_score,
                    active_global_leases,
                    active_model_leases,
                    reservation,
                )
            };
            let worker = self
                .account(&account_id)
                .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.clone()))?;
            let probe_id = worker
                .state
                .write()
                .expect("state lock poisoned")
                .reserve_probe(&model, state_now_ms());
            let Some(probe_id) = probe_id else {
                drop(reservation);
                excluded.insert(account_id);
                continue;
            };
            let probe = ProbeGuard {
                worker: Arc::clone(worker),
                id: probe_id,
            };
            match worker.access_token(false).await {
                Ok(access_token) => {
                    let assignment_reason = reservation.assignment_reason();
                    return Ok(SelectedSubscription {
                        account_id,
                        access_token,
                        model,
                        assignment_reason,
                        placement_score,
                        active_global_leases,
                        active_model_leases,
                        reservation: Some(reservation),
                        _probe: Some(probe),
                    });
                }
                Err(error) => {
                    drop(reservation);
                    worker.set_last_error(error.to_string());
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

    pub fn begin_observation(
        &self,
        account_id: &str,
    ) -> Result<ObservationToken, SubscriptionRuntimeError> {
        Ok(self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?
            .begin_observation())
    }

    pub async fn record_quota_rejection(
        &self,
        account_id: &str,
        requested_model: &str,
        headers: &HeaderSnapshot,
        observation: ObservationToken,
    ) -> Result<(), SubscriptionRuntimeError> {
        let account = self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?;
        account
            .state
            .write()
            .expect("state lock poisoned")
            .reject_quota(
                &ModelFamily::from_requested_model(requested_model),
                headers,
                observation,
                state_now_ms(),
            );
        self.flush_recovery_state().await
    }

    pub async fn corroborate_credential_rejection(
        &self,
        account_id: &str,
        rejected: &SecretString,
    ) -> Result<bool, SubscriptionRuntimeError> {
        let account = self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?;
        let corroborated = account.corroborate_credential_rejection(rejected).await;
        if corroborated {
            self.flush_recovery_state().await?;
        }
        Ok(corroborated)
    }

    async fn restore_blocks(&self) {
        let Some(store) = &self.block_store else {
            return;
        };
        let store = Arc::clone(store);
        let identities = self
            .accounts
            .iter()
            .map(|account| StoreAccount {
                account_id: account.id.clone(),
                source_identity: account.store.config_dir().to_string_lossy().into_owned(),
            })
            .collect::<Vec<_>>();
        match tokio::task::spawn_blocking(move || store.load(&identities, state_now_ms())).await {
            Ok(Ok(restored)) => {
                for record in restored {
                    if let Some(account) = self.account(&record.account_id) {
                        account
                            .state
                            .write()
                            .expect("state lock poisoned")
                            .restore_blocks(
                                record.blocks,
                                account.generation.load(Ordering::SeqCst),
                                state_now_ms(),
                            );
                    }
                }
            }
            _ => {
                for account in &self.accounts {
                    account.set_last_error(
                        "recovery state could not be restored; allowance requires reconciliation"
                            .to_owned(),
                    );
                    account
                        .state
                        .write()
                        .expect("state lock poisoned")
                        .mark_recovery_dirty();
                }
            }
        }
    }

    /// For orderly shutdown: waits for every in-flight OAuth refresh to finish,
    /// makes one more attempt to save a rotated grant whose save failed
    /// earlier, then refuses new refreshes. Recoveries run on their own
    /// tasks, so without this, dropping the Tokio runtime could cancel a
    /// refresh after the provider already rotated the grant, leaving the
    /// stored login unusable. Returns the accounts whose rotated grant is
    /// still only in memory.
    pub async fn quiesce_credentials(&self) -> Vec<String> {
        let mut unsaved = Vec::new();
        for account in &self.accounts {
            account.oauth_closed.store(true, Ordering::SeqCst);
        }
        for account in &self.accounts {
            // Err only when already closed, which is what we want anyway.
            let permit = account.refresh_gate.acquire().await;
            if permit.is_ok() {
                account.persist_pending_grant().await;
            }
            account.refresh_gate.close();
            if account.persistence_pending.load(Ordering::SeqCst) {
                unsaved.push(account.id.clone());
            }
        }
        unsaved
    }

    /// Waits for a complete recovery snapshot write (for orderly shutdown).
    pub async fn flush_recovery_state(&self) -> Result<(), SubscriptionRuntimeError> {
        if self.read_only {
            return Ok(());
        }
        let Some(store) = &self.block_store else {
            return Ok(());
        };
        if !self
            .accounts
            .iter()
            .any(|account| account.recovery_needs_persistence())
        {
            return Ok(());
        }
        let store = Arc::clone(store);
        let gate = Arc::clone(&self.persistence_gate);
        let accounts = self.accounts.clone();
        // Owned writer keeps ordering even when the requesting future is cancelled.
        tokio::spawn(async move {
            let _guard = gate.lock_owned().await;
            if !accounts
                .iter()
                .any(|account| account.recovery_needs_persistence())
            {
                return Ok(());
            }
            let mut revisions = Vec::with_capacity(accounts.len());
            let records = accounts
                .iter()
                .map(|account| {
                    let state = account.state.read().expect("state lock poisoned");
                    revisions.push(state.recovery_revision());
                    AccountBlocks {
                        account_id: account.id.clone(),
                        source_identity: account.store.config_dir().to_string_lossy().into_owned(),
                        blocks: state.export_blocks(),
                    }
                })
                .collect::<Vec<_>>();
            let result = tokio::task::spawn_blocking(move || store.save(&records))
                .await
                .map_err(SubscriptionRuntimeError::BackgroundTask)
                .and_then(|saved| {
                    saved
                        .map_err(|error| SubscriptionRuntimeError::RecoveryState(error.to_string()))
                });
            if let Err(error) = &result {
                for account in &accounts {
                    account.set_last_error(error.to_string());
                }
            }
            if result.is_ok() {
                for (account, revision) in accounts.iter().zip(revisions) {
                    account
                        .durable_recovery_revision
                        .store(revision, Ordering::SeqCst);
                }
            }
            result
        })
        .await
        .map_err(SubscriptionRuntimeError::BackgroundTask)?
    }
    pub async fn observe_response_headers(
        &self,
        account_id: &str,
        requested_model: &str,
        headers: &HeaderSnapshot,
        observation: ObservationToken,
    ) -> Result<(), SubscriptionRuntimeError> {
        let account = self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?;
        let model = ModelFamily::from_requested_model(requested_model);
        let claims = normalize_unified_headers(headers, Some(&model));
        let extra_usage = normalize_unified_extra_usage(headers);
        {
            let mut state = account
                .state
                .write()
                .expect("subscription state lock poisoned");
            state.observe_response(claims, extra_usage, observation, state_now_ms());
            state.confirm_success(&model, observation, state_now_ms());
        }
        self.flush_recovery_state().await
    }

    pub async fn mark_entitlement_unavailable(
        &self,
        account_id: &str,
        requested_model: &str,
        observation: ObservationToken,
    ) -> Result<(), SubscriptionRuntimeError> {
        let account = self
            .account(account_id)
            .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.to_owned()))?;
        account
            .state
            .write()
            .expect("subscription state lock poisoned")
            .reject_entitlement(
                &ModelFamily::from_requested_model(requested_model),
                observation,
                state_now_ms(),
            );
        self.flush_recovery_state().await
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
                    affinity_warm: false,
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
            schema: POOL_STATUS_SCHEMA,
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
    reservation: Option<PlacementReservation>,
    _probe: Option<ProbeGuard>,
}

impl SelectedSubscription {
    pub fn accept(&mut self) {
        if let Some(reservation) = &mut self.reservation {
            if let Some(reason) = reservation.accept(Instant::now()) {
                self.assignment_reason = reason;
            }
        }
        // The probe lives through response observation, which may occur after acceptance.
    }
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

/// Version of the [`PoolRuntimeStatus`] JSON served to status consumers. A
/// document without the field predates versioning and deserializes as 0.
pub const POOL_STATUS_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PoolRuntimeStatus {
    #[serde(default)]
    pub schema: u32,
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
    #[serde(default)]
    pub limit_resets: Option<LimitResetStatus>,
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
    rejected_refresh_grant: Mutex<Option<SecretString>>,
    refresh_gate: Arc<Semaphore>,
    /// Set for every account before shutdown waits on any gate, so a request
    /// still open cannot start an OAuth refresh on an account quiesce has not
    /// reached yet.
    oauth_closed: AtomicBool,
    state: RwLock<RoutingState>,
    generation: AtomicU64,
    durable_recovery_revision: AtomicU64,
    persistence_pending: AtomicBool,
    retry_after: Mutex<Option<Instant>>,
    subscription_type: RwLock<Option<String>>,
    limit_resets: Mutex<ResetCache>,
    last_error: RwLock<Option<String>>,
    http: reqwest::Client,
    refresh_client: OAuthRefreshClient,
    usage_url: String,
    refresh_margin_ms: i64,
    read_only: bool,
    recovery_backoff: Duration,
}

#[derive(Default)]
struct ResetCache {
    generation: u64,
    sequence: u64,
    inventory: Option<LimitResetStatus>,
}

struct WorkerRecoveryOptions {
    refresh_margin_ms: i64,
    read_only: bool,
    recovery_backoff: Duration,
}
impl AccountWorker {
    fn new(
        id: String,
        store: CredentialStore,
        http: reqwest::Client,
        refresh_client: OAuthRefreshClient,
        usage_url: String,
        options: WorkerRecoveryOptions,
    ) -> Self {
        Self {
            state: RwLock::new(RoutingState::unknown(id.clone())),
            id,
            store,
            credential: Mutex::new(None),
            refresh_gate: Arc::new(Semaphore::new(1)),
            oauth_closed: AtomicBool::new(false),
            rejected_refresh_grant: Mutex::new(None),
            generation: AtomicU64::new(0),
            persistence_pending: AtomicBool::new(false),
            durable_recovery_revision: AtomicU64::new(0),
            retry_after: Mutex::new(None),
            subscription_type: RwLock::new(None),
            limit_resets: Mutex::new(ResetCache::default()),
            last_error: RwLock::new(None),
            http,
            refresh_client,
            usage_url,
            refresh_margin_ms: options.refresh_margin_ms,
            read_only: options.read_only,
            recovery_backoff: options.recovery_backoff.min(Duration::from_secs(300)),
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
            .expect("credential lock poisoned")
            .is_some()
        {
            return Ok(());
        }
        match self.load_credential_from_store().await {
            Ok(document) => {
                let expired = document.expires_at_unix_ms()? <= unix_now_ms();
                self.activate_credential(document)?;
                if self.read_only && expired {
                    self.set_credential_health(CredentialHealth::Unavailable);
                    self.set_last_error(
                        "credential expired; read-only status does not refresh".to_owned(),
                    );
                }
                Ok(())
            }
            Err(error) => {
                self.set_credential_health(CredentialHealth::Unavailable);
                self.set_last_error(error.to_string());
                Err(error)
            }
        }
    }

    async fn access_token(
        self: &Arc<Self>,
        force: bool,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        if self.read_only {
            return Err(SubscriptionRuntimeError::ReadOnly);
        }
        if !force
            && self
                .state
                .read()
                .expect("state lock poisoned")
                .credential_health(state_now_ms())
                == CredentialHealth::Healthy
            && !self.persistence_pending.load(Ordering::SeqCst)
            && self
                .retry_after
                .lock()
                .expect("retry lock poisoned")
                .is_none()
            && self
                .rejected_refresh_grant
                .lock()
                .expect("rejected grant lock poisoned")
                .is_none()
        {
            let credential = self.credential.lock().expect("credential lock poisoned");
            if let Some(document) = credential.as_ref() {
                if document.expires_at_unix_ms().is_ok_and(|expiry| {
                    expiry > unix_now_ms().saturating_add(self.refresh_margin_ms)
                }) {
                    return document.access_token().map_err(Into::into);
                }
            }
        }
        // The owned task retains the semaphore and transaction even if the request is cancelled.
        let baseline = self.current_access_token().ok();
        let worker = Arc::clone(self);
        tokio::spawn(async move { worker.recover(force, baseline).await })
            .await
            .map_err(SubscriptionRuntimeError::BackgroundTask)?
    }

    async fn refresh_after_unauthorized(
        self: &Arc<Self>,
        rejected: &SecretString,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        if self.read_only {
            return Err(SubscriptionRuntimeError::ReadOnly);
        }
        let worker = Arc::clone(self);
        let rejected = rejected.clone();
        tokio::spawn(async move { worker.recover(true, Some(rejected)).await })
            .await
            .map_err(SubscriptionRuntimeError::BackgroundTask)?
    }

    /// Shutdown's last attempt to save a rotated grant held only in memory.
    /// Save-only: ignores the recovery backoff and never calls OAuth, so it
    /// cannot itself rotate a grant that a deadline might then drop. The
    /// caller holds `refresh_gate`.
    async fn persist_pending_grant(&self) {
        if !self.persistence_pending.load(Ordering::SeqCst) {
            return;
        }
        let Some(document) = self
            .credential
            .lock()
            .expect("credential lock poisoned")
            .clone()
        else {
            return;
        };
        let store = self.store.clone();
        let saved = tokio::task::spawn_blocking(move || {
            let tx = store.begin_refresh(Duration::from_secs(5))?;
            let mut document = document;
            let _ = tx.rebase_if_same_auth(&mut document);
            tx.save_if_unchanged(&mut document).map(|()| document)
        })
        .await;
        match saved {
            Ok(Ok(document)) => {
                self.persistence_pending.store(false, Ordering::SeqCst);
                let _ = self.activate_credential(document);
            }
            Ok(Err(error)) => {
                self.set_last_error(format!("credential persistence pending: {error}"))
            }
            Err(error) => self.set_last_error(format!("credential persistence pending: {error}")),
        }
    }

    async fn recover(
        &self,
        force: bool,
        baseline: Option<SecretString>,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        let _permit = Arc::clone(&self.refresh_gate)
            .acquire_owned()
            .await
            .map_err(|_| SubscriptionRuntimeError::RuntimeClosed)?;
        if self.oauth_closed.load(Ordering::SeqCst) {
            return Err(SubscriptionRuntimeError::RuntimeClosed);
        }
        let health = self.state().credential_health;
        let valid = self.credential_valid();
        let current = self.current_access_token().ok();
        if force && baseline.is_some() && current != baseline && valid {
            return self.current_access_token();
        }
        let needs_refresh = self
            .credential
            .lock()
            .expect("credential lock poisoned")
            .as_ref()
            .is_none_or(|document| {
                document.expires_at_unix_ms().map_or(true, |expiry| {
                    expiry <= unix_now_ms().saturating_add(self.refresh_margin_ms)
                })
            });
        if !force
            && !needs_refresh
            && matches!(
                health,
                CredentialHealth::Healthy | CredentialHealth::Refreshing
            )
            && !self.persistence_pending.load(Ordering::SeqCst)
        {
            return self.current_access_token();
        }
        let store = self.store.clone();
        let tx =
            match tokio::task::spawn_blocking(move || store.begin_refresh(Duration::from_secs(5)))
                .await
            {
                Ok(Ok(tx)) => tx,
                Ok(Err(error)) => {
                    return self.temporary_recovery_failure(error.into(), force, valid, health);
                }
                Err(error) => {
                    return self.temporary_recovery_failure(
                        SubscriptionRuntimeError::BackgroundTask(error),
                        force,
                        valid,
                        health,
                    );
                }
            };
        let (tx, latest) = tokio::task::spawn_blocking(move || {
            let latest = tx.load();
            (tx, latest)
        })
        .await
        .map_err(SubscriptionRuntimeError::BackgroundTask)?;
        let latest = match latest {
            Ok(latest) => latest,
            Err(error) => {
                return self.temporary_recovery_failure(error.into(), force, valid, health);
            }
        };
        if self.refresh_grant_rejected(&latest) {
            return Err(SubscriptionRuntimeError::CredentialUnavailable(
                self.id.clone(),
            ));
        }
        let changed = self
            .credential
            .lock()
            .expect("credential lock poisoned")
            .as_ref()
            .is_none_or(|document| !document.has_same_auth_version(&latest));
        if changed && !self.persistence_pending.load(Ordering::SeqCst) {
            self.activate_credential(latest)?;
            if self.credential_valid() {
                // A usage success validates only the adopted access grant, not inference entitlement.
                let token = self.current_access_token()?;
                if self.validate_access_grant(&token).await {
                    return Ok(token);
                }
                self.set_credential_health(CredentialHealth::Unavailable);
                return Err(SubscriptionRuntimeError::CredentialUnavailable(
                    self.id.clone(),
                ));
            }
        }
        if self.state().credential_health == CredentialHealth::Quarantined {
            return Err(SubscriptionRuntimeError::CredentialUnavailable(
                self.id.clone(),
            ));
        }
        let backed_off = self
            .retry_after
            .lock()
            .expect("retry lock poisoned")
            .is_some_and(|deadline| deadline > Instant::now());
        if backed_off {
            if !force && valid && self.credential_valid() && health == CredentialHealth::Healthy {
                return self.current_access_token();
            }
            return Err(SubscriptionRuntimeError::RecoveryBackoff);
        }
        let mut document = self
            .credential
            .lock()
            .expect("credential lock poisoned")
            .clone()
            .ok_or_else(|| SubscriptionRuntimeError::CredentialUnavailable(self.id.clone()))?;
        let mut transaction = tx;
        let mut refreshed_once = false;
        loop {
            let tx = transaction;
            self.set_credential_health(CredentialHealth::Refreshing);
            if !self.persistence_pending.load(Ordering::SeqCst) {
                if refreshed_once {
                    return self.temporary_recovery_failure(
                        SubscriptionRuntimeError::CredentialUnavailable(self.id.clone()),
                        true,
                        false,
                        CredentialHealth::Unavailable,
                    );
                }
                refreshed_once = true;
                if let Err(error) = self.refresh_client.refresh_document(&mut document).await {
                    self.set_last_error(error.to_string());
                    *self.retry_after.lock().expect("retry lock poisoned") =
                        Some(Instant::now() + self.recovery_backoff);
                    if error.is_definitive() {
                        *self
                            .rejected_refresh_grant
                            .lock()
                            .expect("rejected grant lock poisoned") = document.refresh_token().ok();
                        self.set_credential_health(CredentialHealth::Quarantined);
                        let stale = document.clone();
                        let winner = tokio::task::spawn_blocking(move || tx.reload(&stale))
                            .await
                            .map_err(SubscriptionRuntimeError::BackgroundTask)?;
                        if let Ok(winner) = winner {
                            if !document.has_same_auth_version(&winner)
                                && !self.refresh_grant_rejected(&winner)
                                && winner
                                    .expires_at_unix_ms()
                                    .is_ok_and(|expiry| expiry > unix_now_ms())
                            {
                                let token = winner.access_token()?;
                                if self.validate_access_grant(&token).await {
                                    return self.activate_credential(winner);
                                }
                            }
                        }
                    } else if !force
                        && valid
                        && self.credential_valid()
                        && health == CredentialHealth::Healthy
                    {
                        self.set_credential_health(CredentialHealth::Healthy);
                        return self.current_access_token();
                    } else {
                        self.set_credential_health(CredentialHealth::Unavailable);
                    }
                    return Err(error.into());
                }
            }
            let (tx, saved_document, saved) = tokio::task::spawn_blocking(move || {
                let mut document = document;
                let _ = tx.rebase_if_same_auth(&mut document);
                let saved = tx.save_if_unchanged(&mut document);
                (tx, document, saved)
            })
            .await
            .map_err(SubscriptionRuntimeError::BackgroundTask)?;
            match saved {
                Ok(()) => {
                    self.persistence_pending.store(false, Ordering::SeqCst);
                    if saved_document.expires_at_unix_ms()? <= unix_now_ms() {
                        // Save the rotated refresh grant first, then mint from that grant, never old disk state.
                        self.activate_credential(saved_document.clone())?;
                        document = saved_document;
                        transaction = tx;
                        continue;
                    }
                    return self.activate_credential(saved_document);
                }
                Err(error) => {
                    let (winner, document) = tokio::task::spawn_blocking(move || {
                        let winner = tx.reload(&saved_document);
                        (winner, saved_document)
                    })
                    .await
                    .map_err(SubscriptionRuntimeError::BackgroundTask)?;
                    if let Ok(winner) = winner {
                        if !document.has_same_auth_version(&winner)
                            && winner
                                .expires_at_unix_ms()
                                .is_ok_and(|expiry| expiry > unix_now_ms())
                        {
                            let token = winner.access_token()?;
                            if self.validate_access_grant(&token).await {
                                self.persistence_pending.store(false, Ordering::SeqCst);
                                return self.activate_credential(winner);
                            }
                        }
                    }
                    // Keep the rotating grant owned, including when its access token has expired.
                    self.persistence_pending.store(true, Ordering::SeqCst);
                    let token = self.activate_credential(document)?;
                    self.set_last_error(format!("credential persistence pending: {error}"));
                    *self.retry_after.lock().expect("retry lock poisoned") =
                        Some(Instant::now() + self.recovery_backoff);
                    if !self.credential_valid() {
                        self.set_credential_health(CredentialHealth::Unavailable);
                        return Err(SubscriptionRuntimeError::CredentialUnavailable(
                            self.id.clone(),
                        ));
                    }
                    return Ok(token);
                }
            }
        }
    }
    fn temporary_recovery_failure(
        &self,
        error: SubscriptionRuntimeError,
        force: bool,
        valid: bool,
        health: CredentialHealth,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        self.set_last_error(error.to_string());
        *self.retry_after.lock().expect("retry lock poisoned") =
            Some(Instant::now() + self.recovery_backoff);
        if !force && valid && self.credential_valid() && health == CredentialHealth::Healthy {
            self.current_access_token()
        } else {
            if health != CredentialHealth::Quarantined {
                self.set_credential_health(CredentialHealth::Unavailable);
            }
            Err(error)
        }
    }

    fn credential_valid(&self) -> bool {
        self.credential
            .lock()
            .expect("credential lock poisoned")
            .as_ref()
            .is_some_and(|document| {
                document
                    .expires_at_unix_ms()
                    .is_ok_and(|expiry| expiry > unix_now_ms())
            })
    }

    async fn validate_access_grant(&self, token: &SecretString) -> bool {
        match self.send_usage_request(token).await {
            Ok(response) if response.status().is_success() => {
                read_bounded_usage_response(response).await.is_ok()
            }
            _ => false,
        }
    }

    async fn corroborate_credential_rejection(&self, rejected: &SecretString) -> bool {
        if self.read_only
            || self
                .current_access_token()
                .is_ok_and(|current| current != *rejected)
        {
            return false;
        }
        let observation = self.begin_observation();
        match self.send_usage_request(rejected).await {
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                if read_bounded_usage_response(response).await.is_err() {
                    return false;
                }
                if self
                    .current_access_token()
                    .is_ok_and(|current| current == *rejected)
                {
                    self.state
                        .write()
                        .expect("state lock poisoned")
                        .reject_authentication(observation, state_now_ms());
                    self.set_last_error(
                        "credential authentication rejected by usage endpoint".to_owned(),
                    );
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn refresh_grant_rejected(&self, document: &CredentialDocument) -> bool {
        let rejected = self
            .rejected_refresh_grant
            .lock()
            .expect("rejected grant lock poisoned");
        rejected.as_ref().is_some_and(|rejected| {
            document
                .refresh_token()
                .is_ok_and(|token| token == *rejected)
        })
    }
    async fn load_credential_from_store(
        &self,
    ) -> Result<CredentialDocument, SubscriptionRuntimeError> {
        let store = self.store.clone();
        let read_only = self.read_only;
        tokio::task::spawn_blocking(move || {
            if read_only {
                store.load_read_only()
            } else {
                store.load()
            }
        })
        .await
        .map_err(SubscriptionRuntimeError::BackgroundTask)?
        .map_err(|source| SubscriptionRuntimeError::CredentialSource {
            account: self.id.clone(),
            source,
        })
    }

    fn activate_credential(
        &self,
        document: CredentialDocument,
    ) -> Result<SecretString, SubscriptionRuntimeError> {
        let token = document.access_token()?;
        if self.refresh_grant_rejected(&document) {
            return Err(SubscriptionRuntimeError::CredentialUnavailable(
                self.id.clone(),
            ));
        }
        *self
            .rejected_refresh_grant
            .lock()
            .expect("rejected grant lock poisoned") = None;
        let changed = self
            .credential
            .lock()
            .expect("credential lock poisoned")
            .as_ref()
            .is_none_or(|current| !current.has_same_auth_version(&document));
        *self
            .subscription_type
            .write()
            .expect("subscription type lock poisoned") =
            document.subscription_type().map(str::to_owned);
        *self.credential.lock().expect("credential lock poisoned") = Some(document);
        if changed {
            let mut resets = self.limit_resets.lock().expect("reset cache lock poisoned");
            resets.generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            resets.sequence = 0;
            resets.inventory = None;
            drop(resets);
            self.begin_observation();
        }
        *self.retry_after.lock().expect("retry lock poisoned") = None;
        self.set_credential_health(CredentialHealth::Healthy);
        self.clear_last_error();
        Ok(token)
    }

    fn current_access_token(&self) -> Result<SecretString, SubscriptionRuntimeError> {
        self.credential
            .lock()
            .expect("credential lock poisoned")
            .as_ref()
            .ok_or_else(|| SubscriptionRuntimeError::CredentialUnavailable(self.id.clone()))?
            .access_token()
            .map_err(Into::into)
    }

    async fn reload_credential(self: &Arc<Self>) -> AccountCredentialReload {
        let previous_health = self.state().credential_health;
        let (detail, activated) = self.reload_credential_source().await;
        if activated && !self.read_only {
            self.refresh_usage().await;
        }
        AccountCredentialReload {
            id: self.id.clone(),
            previous_health,
            health: self.state().credential_health,
            detail,
        }
    }

    async fn reload_credential_source(&self) -> (String, bool) {
        let Ok(_permit) = self.refresh_gate.acquire().await else {
            return (SubscriptionRuntimeError::RuntimeClosed.to_string(), false);
        };
        if self.persistence_pending.load(Ordering::SeqCst) {
            return ("credential persistence pending".to_owned(), false);
        }
        let latest = match self.load_credential_from_store().await {
            Ok(latest) => latest,
            Err(error) => {
                let detail = format!("credential source could not be read: {error}");
                self.set_last_error(detail.clone());
                return (detail, false);
            }
        };
        let changed = self
            .credential
            .lock()
            .expect("credential lock poisoned")
            .as_ref()
            .is_none_or(|current| !current.has_same_auth_version(&latest));
        if !changed || self.refresh_grant_rejected(&latest) {
            return (
                if self.state().credential_health == CredentialHealth::Healthy {
                    "unchanged"
                } else {
                    "credential source unchanged; sign in to this profile again"
                }
                .to_owned(),
                false,
            );
        }
        match self.activate_credential(latest) {
            Ok(_) => (
                "reloaded a new credential from the credential source".to_owned(),
                true,
            ),
            Err(error) => {
                self.set_last_error(error.to_string());
                (error.to_string(), false)
            }
        }
    }

    async fn refresh_usage(self: &Arc<Self>) {
        if !self.read_only
            && matches!(
                self.state().credential_health,
                CredentialHealth::Quarantined | CredentialHealth::Unavailable
            )
        {
            self.reload_credential_source().await;
        }
        let access = if self.read_only {
            if !self.credential_valid() {
                self.mark_usage_stale(
                    "credential missing or expired; sign in with Claude to restore usage monitoring".to_owned(),
                );
                return;
            }
            self.current_access_token()
        } else {
            self.access_token(false).await
        };
        let mut token = match access {
            Ok(token) => token,
            Err(error) => {
                self.mark_usage_stale(error.to_string());
                return;
            }
        };
        let mut observation = self.begin_observation();
        self.begin_reset_observation(observation);
        let mut response = self.send_usage_request(&token).await;
        if !self.read_only
            && response
                .as_ref()
                .is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED)
        {
            match self.refresh_after_unauthorized(&token).await {
                Ok(next) => {
                    token = next;
                    observation = self.begin_observation();
                    self.begin_reset_observation(observation);
                    response = self.send_usage_request(&token).await;
                }
                Err(error) => {
                    self.mark_usage_stale(error.to_string());
                    return;
                }
            }
        }
        let response = match response {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                let status = response.status();
                if status == reqwest::StatusCode::UNAUTHORIZED
                    && read_bounded_usage_response(response).await.is_ok()
                {
                    self.state
                        .write()
                        .expect("state lock poisoned")
                        .reject_authentication(observation, state_now_ms());
                    *self.retry_after.lock().expect("retry lock poisoned") =
                        Some(Instant::now() + self.recovery_backoff);
                }
                self.mark_usage_stale(format!("usage request returned HTTP {status}"));
                return;
            }
            Err(error) => {
                self.mark_usage_stale(format!(
                    "usage request failed: {}",
                    format_error_chain(&error)
                ));
                return;
            }
        };
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
        self.state
            .write()
            .expect("state lock poisoned")
            .apply_usage(usage.normalize(), observation, state_now_ms());
        if !self.persistence_pending.load(Ordering::SeqCst)
            && self
                .retry_after
                .lock()
                .expect("retry lock poisoned")
                .is_none()
        {
            self.clear_last_error();
        }
        if !self.read_only {
            self.discover_resets(&token, &usage, observation).await;
        }
    }

    fn begin_reset_observation(&self, observation: ObservationToken) {
        let mut cache = self.limit_resets.lock().expect("reset cache lock poisoned");
        if observation.credential_generation == cache.generation
            && observation.sequence > cache.sequence
        {
            cache.sequence = observation.sequence;
            if let Some(inventory) = cache.inventory.as_mut() {
                inventory.fresh = false;
            }
        }
    }

    async fn discover_resets(
        &self,
        token: &SecretString,
        usage: &UsageSnapshot,
        observation: ObservationToken,
    ) {
        let now = unix_now_ms();
        let mut answers = Vec::with_capacity(PROGRAMS.len());
        for program in PROGRAMS {
            let inline = parse_program(program, usage.unknown.get(program), now);
            let answer = match inline {
                Some(answer) => Some(answer),
                None => self.probe_reset_program(token, program).await,
            };
            answers.push(answer);
        }
        // Token and generation must still describe the account that answered.
        // Credential activation never acquires this cache while holding its credential lock.
        let credential = self.credential.lock().expect("credential lock poisoned");
        if !credential.as_ref().is_some_and(|document| {
            document
                .access_token()
                .is_ok_and(|current| current == *token)
        }) {
            return;
        }
        let mut cache = self.limit_resets.lock().expect("reset cache lock poisoned");
        if cache.generation != observation.credential_generation
            || cache.sequence != observation.sequence
        {
            return;
        }
        let complete = answers.iter().all(Option::is_some);
        let mut inventory = cache.inventory.take().unwrap_or(LimitResetStatus {
            checked_at_unix_ms: now,
            fresh: false,
            programs: Vec::new(),
        });
        for answer in answers.into_iter().flatten() {
            if let Some(previous) = inventory
                .programs
                .iter_mut()
                .find(|p| p.program == answer.program)
            {
                *previous = answer;
            } else {
                inventory.programs.push(answer);
            }
        }
        if !inventory.programs.is_empty() {
            inventory.checked_at_unix_ms = unix_now_ms();
            inventory.fresh = complete;
            cache.inventory = Some(inventory);
        }
    }

    async fn probe_reset_program(
        &self,
        token: &SecretString,
        program: &str,
    ) -> Option<LimitResetProgram> {
        let flag = match program {
            "cedar_ember" => "cedar_ember",
            "juniper_tide" => "at_wall",
            _ => return None,
        };
        // Discovery is a safe GET only. In particular, a probe 401 must not
        // refresh credentials or change quota/routing/authentication state.
        let response = self
            .http
            .get(&self.usage_url)
            .query(&[(flag, "1"), ("skip_spend", "1")])
            .bearer_auth(token.expose())
            .header("anthropic-beta", USAGE_BETA)
            .header(reqwest::header::USER_AGENT, "claude-code/2.1.0")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bytes = read_bounded_usage_response(response).await.ok()?;
        let payload: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        parse_program(program, payload.get(program), unix_now_ms())
    }

    fn begin_observation(&self) -> ObservationToken {
        self.state
            .write()
            .expect("state lock poisoned")
            .begin_observation(self.generation.load(Ordering::SeqCst))
    }

    async fn send_usage_request(
        &self,
        token: &SecretString,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.http
            .get(&self.usage_url)
            .bearer_auth(token.expose())
            .header("anthropic-beta", USAGE_BETA)
            .header(reqwest::header::USER_AGENT, "claude-code/2.1.0")
            .send()
            .await
    }
    fn state(&self) -> AccountLimitState {
        self.state
            .read()
            .expect("state lock poisoned")
            .snapshot(state_now_ms())
    }
    fn set_credential_health(&self, health: CredentialHealth) {
        self.state
            .write()
            .expect("state lock poisoned")
            .limits
            .credential_health = health;
    }
    fn mark_usage_stale(&self, error: String) {
        if let Some(inventory) = self
            .limit_resets
            .lock()
            .expect("reset cache lock poisoned")
            .inventory
            .as_mut()
        {
            inventory.fresh = false;
        }
        self.state
            .write()
            .expect("state lock poisoned")
            .limits
            .usage_snapshot_fresh = false;
        self.set_last_error(error);
    }
    fn set_last_error(&self, error: String) {
        *self.last_error.write().expect("error lock poisoned") = Some(error);
    }
    fn clear_last_error(&self) {
        *self.last_error.write().expect("error lock poisoned") = None;
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
                .expect("type lock poisoned")
                .clone(),
            usage_snapshot_fresh: state.usage_snapshot_fresh,
            complete_global_snapshot: state.complete_global_snapshot,
            claims: state.claims,
            extra_usage: state.extra_usage,
            limit_resets: self
                .limit_resets
                .lock()
                .expect("reset cache lock poisoned")
                .inventory
                .clone(),
            last_error: self.last_error.read().expect("error lock poisoned").clone(),
        }
    }
    fn recovery_needs_persistence(&self) -> bool {
        self.state
            .read()
            .expect("state lock poisoned")
            .recovery_revision()
            != self.durable_recovery_revision.load(Ordering::SeqCst)
    }
}

struct ProbeGuard {
    worker: Arc<AccountWorker>,
    id: u64,
}
impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.worker
            .state
            .write()
            .expect("state lock poisoned")
            .release_probe(self.id);
    }
}

fn state_now_ms() -> u64 {
    unix_now_ms().try_into().unwrap_or(0)
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
    #[error("subscription pool {pool:?} has no eligible account for model {model:?}: {detail}")]
    NoEligibleAccount {
        pool: String,
        model: String,
        /// Why each account was passed over, one line, no credentials.
        detail: String,
    },
    #[error("unknown subscription account {0:?}")]
    UnknownAccount(String),
    #[error("subscription pool resolves more than one account to credential source {path}")]
    DuplicateCredentialSource { path: PathBuf },
    #[error("cannot expand home-relative credential path {0}")]
    HomeUnavailable(PathBuf),
    #[error("subscription runtime is read-only")]
    ReadOnly,
    #[error("Anthropic base URL must use https unless it is a loopback address")]
    InsecureBaseUrl,
    #[error("subscription recovery state failed: {0}")]
    RecoveryState(String),
    #[error("subscription account recovery is temporarily backed off")]
    RecoveryBackoff,
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

/// Bearer tokens are sent to the base URL, so it must be https. Plain http is
/// accepted only for loopback hosts (local gateways and test fixtures).
pub fn is_credential_safe_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        _ => false,
    }
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
            reservation: None,
            _probe: None,
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
            limit_resets: None,
            last_error: None,
        };
        let json = serde_json::to_string(&status).expect("status JSON");
        assert!(!json.contains("accessToken"));
        assert!(!json.contains("refreshToken"));
    }

    #[test]
    fn status_json_is_versioned_and_legacy_documents_read_as_schema_zero() {
        let status = PoolRuntimeStatus {
            schema: POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: Vec::new(),
            placement: None,
        };
        let json = serde_json::to_value(&status).expect("status JSON");
        assert_eq!(json["schema"], serde_json::json!(POOL_STATUS_SCHEMA));

        let legacy: PoolRuntimeStatus = serde_json::from_value(serde_json::json!({
            "pool_id": "default",
            "accounts": [],
            "placement": null,
        }))
        .expect("legacy status");
        assert_eq!(legacy.schema, 0);
    }

    #[test]
    fn only_https_or_loopback_base_urls_carry_credentials() {
        assert!(is_credential_safe_base_url("https://api.anthropic.com"));
        assert!(is_credential_safe_base_url("http://127.0.0.1:20816"));
        assert!(is_credential_safe_base_url("http://localhost:9"));
        assert!(is_credential_safe_base_url("http://[::1]:9"));
        assert!(!is_credential_safe_base_url("http://api.anthropic.com"));
        assert!(!is_credential_safe_base_url("http://10.0.0.5"));
        assert!(!is_credential_safe_base_url("ftp://127.0.0.1"));
        assert!(!is_credential_safe_base_url("not a url"));
    }
}
