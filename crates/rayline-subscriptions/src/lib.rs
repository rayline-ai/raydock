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
mod model;
mod oauth;
mod response;
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
pub use model::ModelFamily;
pub use oauth::{
    DEFAULT_CLAUDE_OAUTH_CLIENT_ID, DEFAULT_CLAUDE_TOKEN_URL, OAuthRefreshClient, OAuthRefreshError,
};
pub use response::{
    ResponseClassification, classify_response, normalize_unified_extra_usage,
    normalize_unified_headers,
};
pub use runtime::{
    AccountCredentialReload, AccountPlacementRuntimeStatus, AccountRuntimeStatus,
    CredentialReloadSummary, PoolPlacementRuntimeStatus, PoolRuntimeStatus, SelectedSubscription,
    SubscriptionPoolRuntime, SubscriptionRuntimeError, SubscriptionRuntimeOptions,
};
pub use selection::{
    AccountEvaluation, AccountPlacementLoad, IneligibilityReason, SelectionDecision,
    SelectionRequest, select_account,
};
pub use status::{
    RAYLINE_STATUS_ID_ENV, SESSION_STATUS_SCHEMA, SessionAssignmentKind, SessionAssignmentReason,
    SessionAssignmentStatus, SessionCapacityStatus, SessionLimitStatus, SessionPlacementStatus,
    SessionRouteStatus, SessionStatusSnapshot, derive_status_id, is_valid_status_id,
};
pub use usage::{ScopedLimit, UsageBucket, UsageSnapshot};
