//! Shared policy for routing Anthropic subscription traffic across several
//! explicitly registered Claude credential sources.
//!
//! It owns the version-tolerant data model and routing decisions as well as the
//! local credential workers used by the daemon. Provider request replay remains
//! in `rayline-proxy`, at the boundary where downstream streaming is gated.

mod config;
mod credential;
mod headers;
mod limits;
mod lock;
mod model;
mod oauth;
mod persistence;
mod placement;
mod resets;
mod response;
mod routing_state;
mod runtime;
mod selection;
mod status;
mod usage;

pub use config::{
    BillingPolicy, ConfigError, CredentialSourceConfig, PoolPolicy, SUBSCRIPTION_CONFIG_SCHEMA,
    StickyPolicy, SubscriptionAccountConfig, SubscriptionPoolConfig, SubscriptionPoolsConfig,
};
pub use credential::{CredentialDocument, CredentialError, CredentialStore, SecretString};
pub use headers::HeaderSnapshot;
pub use limits::{
    AccountLimitState, ClaimScope, ClaimStatus, CredentialHealth, Entitlement, ExtraUsageState,
    LimitClaim, LimitSource, NormalizedUsage,
};
pub use lock::{DEFAULT_POOL_STATE_DIR, PoolLock, PoolLockError, recorded_holder_pid};
pub use model::ModelFamily;
pub use oauth::{
    DEFAULT_CLAUDE_OAUTH_CLIENT_ID, DEFAULT_CLAUDE_TOKEN_URL, OAuthRefreshClient, OAuthRefreshError,
};
pub use resets::{LimitResetGrant, LimitResetProgram, LimitResetStatus};
pub use response::{
    ResponseClassification, classify_response, normalize_unified_extra_usage,
    normalize_unified_headers,
};
pub use routing_state::ObservationToken;
pub use runtime::{
    AccountCredentialReload, AccountPlacementRuntimeStatus, AccountRuntimeStatus,
    CredentialReloadSummary, POOL_STATUS_SCHEMA, PoolPlacementRuntimeStatus, PoolRuntimeStatus,
    SelectedSubscription, SubscriptionPoolRuntime, SubscriptionRuntimeError,
    SubscriptionRuntimeOptions,
};
pub use selection::{
    AccountEvaluation, AccountPlacementLoad, IneligibilityReason, SelectionDecision,
    SelectionRequest, describe_ineligibility, select_account,
};
pub use status::{
    RAYLINE_STATUS_ID_ENV, SESSION_STATUS_SCHEMA, SessionAssignmentKind, SessionAssignmentReason,
    SessionAssignmentStatus, SessionCapacityStatus, SessionLimitStatus, SessionPlacementStatus,
    SessionRouteStatus, SessionStatusSnapshot, derive_status_id, is_valid_status_id,
};
pub use usage::{ScopedLimit, UsageBucket, UsageSnapshot};
