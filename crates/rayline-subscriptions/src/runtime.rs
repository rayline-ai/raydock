use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::future::join_all;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::oauth::unix_now_ms;
use crate::{
    AccountLimitState, CredentialDocument, CredentialError, CredentialHealth, CredentialStore,
    Entitlement, ExtraUsageState, HeaderSnapshot, LimitClaim, ModelFamily, OAuthRefreshClient,
    OAuthRefreshError, PoolPolicy, SecretString, SelectionRequest, SubscriptionPoolConfig,
    UsageSnapshot, normalize_unified_extra_usage, normalize_unified_headers, select_account,
};

const DEFAULT_USAGE_PATH: &str = "/api/oauth/usage";
const USAGE_BETA: &str = "oauth-2025-04-20";
const MAX_USAGE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_AFFINITY_ENTRIES: usize = 4096;

#[derive(Clone, Debug)]
pub struct SubscriptionRuntimeOptions {
    pub anthropic_base_url: String,
    pub token_url: String,
    pub oauth_client_id: String,
    pub request_timeout: Duration,
    pub poll_interval: Duration,
    pub near_limit_poll_interval: Duration,
    pub refresh_margin: Duration,
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
            home_dir: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

pub struct SubscriptionPoolRuntime {
    pool_id: String,
    policy: PoolPolicy,
    accounts: Vec<Arc<AccountWorker>>,
    affinities: Mutex<HashMap<AffinityKey, String>>,
    monitor_started: AtomicBool,
    poll_interval: Duration,
    near_limit_poll_interval: Duration,
}

impl SubscriptionPoolRuntime {
    pub async fn start(
        pool_id: impl Into<String>,
        config: SubscriptionPoolConfig,
        options: SubscriptionRuntimeOptions,
    ) -> Result<Arc<Self>, SubscriptionRuntimeError> {
        let pool_id = pool_id.into();
        let http = reqwest::Client::builder()
            .timeout(options.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(SubscriptionRuntimeError::HttpClient)?;
        let refresh_client =
            OAuthRefreshClient::new(http.clone(), &options.token_url, &options.oauth_client_id);
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
            affinities: Mutex::new(HashMap::new()),
            monitor_started: AtomicBool::new(false),
            poll_interval: options.poll_interval,
            near_limit_poll_interval: options.near_limit_poll_interval,
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
        let affinity = self
            .affinities
            .lock()
            .expect("subscription affinity lock poisoned")
            .get(&key)
            .cloned();
        let mut excluded = excluded_accounts.clone();

        loop {
            let states = self
                .accounts
                .iter()
                .map(|account| account.state())
                .filter(|state| !excluded.contains(&state.account_id))
                .collect::<Vec<_>>();
            let decision = select_account(
                &states,
                &SelectionRequest {
                    model: model.clone(),
                    affinity_account_id: affinity
                        .as_ref()
                        .filter(|account| !excluded.contains(*account))
                        .cloned(),
                },
                &self.policy,
            );
            let Some(account_id) = decision.selected_account_id else {
                return Err(SubscriptionRuntimeError::NoEligibleAccount {
                    pool: self.pool_id.clone(),
                    model: model.to_string(),
                });
            };
            let worker = self
                .account(&account_id)
                .ok_or_else(|| SubscriptionRuntimeError::UnknownAccount(account_id.clone()))?;
            match worker.access_token(false).await {
                Ok(access_token) => {
                    let mut affinities = self
                        .affinities
                        .lock()
                        .expect("subscription affinity lock poisoned");
                    if affinities.len() >= MAX_AFFINITY_ENTRIES
                        && !affinities.contains_key(&key)
                        && let Some(oldest) = affinities.keys().next().cloned()
                    {
                        affinities.remove(&oldest);
                    }
                    affinities.insert(key, account_id.clone());
                    return Ok(SelectedSubscription {
                        account_id,
                        access_token,
                        model,
                    });
                }
                Err(error) => {
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
        let mut affinities = self
            .affinities
            .lock()
            .expect("subscription affinity lock poisoned");
        if affinities.get(&key).map(String::as_str) == Some(account_id) {
            affinities.remove(&key);
        }
    }

    pub fn status(&self) -> PoolRuntimeStatus {
        PoolRuntimeStatus {
            pool_id: self.pool_id.clone(),
            accounts: self
                .accounts
                .iter()
                .map(|account| account.status())
                .collect(),
        }
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
}

impl std::fmt::Debug for SelectedSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SelectedSubscription")
            .field("account_id", &self.account_id)
            .field("access_token", &"[REDACTED]")
            .field("model", &self.model)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolRuntimeStatus {
    pub pool_id: String,
    pub accounts: Vec<AccountRuntimeStatus>,
}

#[derive(Clone, Debug, Serialize)]
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
        let refresh_result = self.refresh_client.refresh_document(&mut document).await;
        match refresh_result {
            Ok(()) => {
                let save_result = self.store.save_if_unchanged(&mut document);
                if let Err(source) = save_result {
                    let reload_result = self.store.load();
                    *self
                        .credential
                        .lock()
                        .expect("subscription credential lock poisoned") = reload_result.ok();
                    self.set_credential_health(CredentialHealth::Unavailable);
                    self.set_last_error(source.to_string());
                    return Err(source.into());
                }
                let access_token = document.access_token()?;
                *self
                    .credential
                    .lock()
                    .expect("subscription credential lock poisoned") = Some(document);
                self.set_credential_health(CredentialHealth::Healthy);
                self.clear_last_error();
                Ok(access_token)
            }
            Err(error) => {
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
        }
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

    async fn refresh_usage(&self) {
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
}
