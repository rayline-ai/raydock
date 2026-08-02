//! `rld statusline` — render Rayline routing and subscription placement for a
//! Claude Code status line.
//!
//! This command is deliberately a file-only reader: it never opens a profile,
//! talks to Anthropic, or asks the keychain for credentials. The proxy writes
//! credential-free sidecars and this command renders them best-effort.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use rayline_subscriptions::{
    RAYLINE_STATUS_ID_ENV, SESSION_STATUS_SCHEMA, SessionAssignmentKind, SessionRouteStatus,
    SessionStatusSnapshot, is_valid_status_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Route decisions use one global file, so they need a short ownership
/// heuristic. Subscription assignments use a launch-scoped hash and remain
/// useful longer, but are retired with the proxy's 24-hour sidecar retention.
const ROUTE_STALE_AFTER_SECONDS: i64 = 300;
const SESSION_STALE_AFTER_SECONDS: i64 = 24 * 60 * 60;
const MAX_STATUS_BYTES: u64 = 64 * 1024;
const MAX_ACCOUNT_LABEL_BYTES: usize = 32;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const ZAP: &str = "⚡";
const SUBSCRIPTION: &str = "◈";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum StatuslineComponent {
    All,
    Route,
    Subscription,
}

impl fmt::Display for StatuslineComponent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::All => "all",
            Self::Route => "route",
            Self::Subscription => "subscription",
        })
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct RouteStatusFile {
    pub selected_model: Option<String>,
    pub policy: Option<String>,
    pub ts: Option<i64>,
}

impl RouteStatusFile {
    fn fresh_selected_model(&self, now: i64) -> Option<&str> {
        let age = now - self.ts?;
        if !(0..=ROUTE_STALE_AFTER_SECONDS).contains(&age) {
            return None;
        }
        self.selected_model
            .as_deref()
            .filter(|model| !model.is_empty())
    }
}

fn short_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

fn virtual_model(session: &Value) -> Option<&str> {
    let model = session.get("model")?;
    if let Some(object) = model.as_object() {
        for key in ["display_name", "id"] {
            if let Some(name) = object.get(key).and_then(Value::as_str)
                && !name.is_empty()
            {
                return Some(name);
            }
        }
        return None;
    }
    model.as_str().filter(|model| !model.is_empty())
}

/// Render the route fragment, falling back to Claude's virtual model when the
/// global route sidecar is missing or belongs to an old turn.
pub fn render(sidecar: Option<&RouteStatusFile>, session: Option<&Value>, now: i64) -> String {
    if let Some(model) = sidecar.and_then(|status| status.fresh_selected_model(now)) {
        let mut line = format!("{ZAP} {}", short_model(model));
        if let Some(policy) = sidecar
            .and_then(|status| status.policy.as_deref())
            .filter(|policy| !policy.is_empty())
        {
            line.push_str(&format!(" {DIM}· {policy}{RESET}"));
        }
        return line;
    }
    session
        .and_then(virtual_model)
        .map(|name| format!("{DIM}{}{RESET}", short_model(name)))
        .unwrap_or_default()
}

fn render_session_route(
    route: Option<&SessionRouteStatus>,
    session: Option<&Value>,
    now: i64,
) -> String {
    if let Some(route) = route.filter(|route| {
        let age = now - route.updated_at_unix;
        (0..=ROUTE_STALE_AFTER_SECONDS).contains(&age) && !route.selected_model.is_empty()
    }) {
        let mut line = format!("{ZAP} {}", short_model(&route.selected_model));
        if let Some(policy) = route.policy.as_deref().filter(|policy| !policy.is_empty()) {
            line.push_str(&format!(" {DIM}· {policy}{RESET}"));
        }
        return line;
    }
    session
        .and_then(virtual_model)
        .map(|name| format!("{DIM}{}{RESET}", short_model(name)))
        .unwrap_or_default()
}

fn fresh_session_route(
    snapshot: Option<&SessionStatusSnapshot>,
    now: i64,
) -> Option<&SessionRouteStatus> {
    snapshot
        .and_then(|snapshot| snapshot.route.as_ref())
        .filter(|route| {
            let age = now - route.updated_at_unix;
            (0..=ROUTE_STALE_AFTER_SECONDS).contains(&age) && !route.selected_model.is_empty()
        })
}

fn render_subscription(snapshot: Option<&SessionStatusSnapshot>) -> String {
    let Some(snapshot) = snapshot else {
        return String::new();
    };
    let assignment = &snapshot.assignment;
    let account_label = display_account_label(&assignment.current_account_id);
    let assignment_label = if assignment.kind == SessionAssignmentKind::ModelOverride {
        format!(
            "{}→{account_label}",
            display_account_label(&assignment.primary_account_id)
        )
    } else {
        account_label.to_owned()
    };
    let allowance = if snapshot.capacity.usage_snapshot_fresh {
        snapshot.capacity.effective_headroom.map_or_else(
            || format!(" {DIM}· allowance ?{RESET}"),
            |headroom| {
                let headroom = headroom.clamp(0.0, 1.0);
                let color = if headroom <= 0.1 {
                    RED
                } else if headroom <= 0.3 {
                    YELLOW
                } else {
                    GREEN
                };
                let label = compact_limit_label(snapshot.capacity.bottleneck.as_ref());
                format!(
                    " {DIM}· {label} {RESET}{color}{:.0}%L{RESET}",
                    headroom * 100.0
                )
            },
        )
    } else {
        format!(" {DIM}· stale{RESET}")
    };
    let pool = if snapshot.capacity.total_accounts > 1 {
        format!(
            " {DIM}· {}/{}{RESET}",
            snapshot
                .capacity
                .eligible_accounts
                .min(snapshot.capacity.total_accounts),
            snapshot.capacity.total_accounts
        )
    } else {
        String::new()
    };
    format!("{SUBSCRIPTION} {assignment_label}{allowance}{pool}")
}

fn compact_limit_label(limit: Option<&rayline_subscriptions::SessionLimitStatus>) -> String {
    let Some(limit) = limit else {
        return "cap".to_owned();
    };
    if let Some(model) = limit.scope.strip_prefix("model:") {
        return match model.to_ascii_lowercase().as_str() {
            "fable" => "F".to_owned(),
            _ => model
                .chars()
                .next()
                .map(|initial| initial.to_uppercase().collect())
                .unwrap_or_else(|| "model".to_owned()),
        };
    }
    match limit.key.as_str() {
        "five_hour" | "session" => "5h".to_owned(),
        "seven_day" | "weekly" => "7d".to_owned(),
        key if key.contains("fable") => "F".to_owned(),
        _ => "cap".to_owned(),
    }
}

fn display_account_label(value: &str) -> &str {
    let valid = !value.is_empty()
        && value.len() <= MAX_ACCOUNT_LABEL_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid { value } else { "subscription" }
}

/// Write the requested status fragment to Claude Code's status-line protocol.
/// This is user-facing IPC output, not application logging.
fn write_statusline_output(value: &str) {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let _ = output.write_all(value.as_bytes());
}

fn compose_text_output(
    component: StatuslineComponent,
    route_fragment: String,
    subscription_fragment: String,
) -> String {
    let route_fragment = if component == StatuslineComponent::Subscription {
        String::new()
    } else {
        route_fragment
    };
    let subscription_fragment = if component == StatuslineComponent::Route {
        String::new()
    } else {
        subscription_fragment
    };
    match (route_fragment.is_empty(), subscription_fragment.is_empty()) {
        (false, false) => format!("{route_fragment} · {subscription_fragment}"),
        (false, true) => route_fragment,
        (true, false) => subscription_fragment,
        (true, true) => String::new(),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_STATUS_BYTES
    {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut raw = Vec::new();
    file.take(MAX_STATUS_BYTES + 1).read_to_end(&mut raw).ok()?;
    if raw.len() as u64 > MAX_STATUS_BYTES {
        return None;
    }
    serde_json::from_slice(&raw).ok()
}

fn read_session_status(directory: &Path) -> Option<SessionStatusSnapshot> {
    let status_id = std::env::var(RAYLINE_STATUS_ID_ENV).ok()?;
    if !is_valid_status_id(&status_id) {
        return None;
    }
    let snapshot: SessionStatusSnapshot =
        read_bounded_json(&directory.join(format!("{status_id}.json")))?;
    let age = now_unix() - snapshot.updated_at_unix;
    (snapshot.schema == SESSION_STATUS_SCHEMA && (0..=SESSION_STALE_AFTER_SECONDS).contains(&age))
        .then_some(snapshot)
}

fn read_stdin_session() -> Option<Value> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&raw).ok()
}

/// Entry point for `rld statusline`. It always returns successfully so a stale
/// or malformed status file cannot break Claude Code's status line hook.
pub fn run(
    route_status_path: PathBuf,
    session_status_dir: PathBuf,
    component: StatuslineComponent,
    json_output: bool,
) {
    let has_session_identity = std::env::var(RAYLINE_STATUS_ID_ENV)
        .ok()
        .is_some_and(|status_id| is_valid_status_id(&status_id));
    let session_snapshot = has_session_identity
        .then(|| read_session_status(&session_status_dir))
        .flatten();
    // Pooled launches read their own route from the session snapshot. Only
    // non-pooled launches may use the legacy global route sidecar.
    let global_route = (component != StatuslineComponent::Subscription && !has_session_identity)
        .then(|| read_bounded_json(&route_status_path))
        .flatten();
    let subscription = (component != StatuslineComponent::Route)
        .then_some(session_snapshot.as_ref())
        .flatten();
    let session = (!json_output && component != StatuslineComponent::Subscription)
        .then(read_stdin_session)
        .flatten();
    let now = now_unix();
    let session_route = fresh_session_route(session_snapshot.as_ref(), now);

    if json_output {
        let output = match component {
            StatuslineComponent::Route => session_route
                .and_then(|status| serde_json::to_value(status).ok())
                .or_else(|| {
                    global_route
                        .as_ref()
                        .and_then(|status| serde_json::to_value(status).ok())
                })
                .unwrap_or_else(|| json!({})),
            StatuslineComponent::Subscription => subscription
                .and_then(|status| serde_json::to_value(status).ok())
                .unwrap_or_else(|| json!({})),
            StatuslineComponent::All => {
                let mut fields = serde_json::Map::new();
                if let Some(route) = session_route {
                    fields.insert("route".to_owned(), json!(route));
                } else if let Some(route) = global_route.as_ref() {
                    fields.insert("route".to_owned(), json!(route));
                }
                if let Some(subscription) = subscription {
                    fields.insert("subscription".to_owned(), json!(subscription));
                }
                Value::Object(fields)
            }
        };
        if let Ok(serialized) = serde_json::to_string(&output) {
            write_statusline_output(&serialized);
        }
        return;
    }

    let route_fragment = if has_session_identity {
        render_session_route(session_route, session.as_ref(), now)
    } else {
        render(global_route.as_ref(), session.as_ref(), now)
    };
    let subscription_fragment = render_subscription(subscription);
    let output = compose_text_output(component, route_fragment, subscription_fragment);
    write_statusline_output(&output);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayline_subscriptions::{
        SessionAssignmentReason, SessionAssignmentStatus, SessionCapacityStatus,
        SessionLimitStatus, SessionPlacementStatus,
    };
    use serde_json::json;

    const NOW: i64 = 1_000_000;

    fn route_status(ts: i64) -> RouteStatusFile {
        RouteStatusFile {
            selected_model: Some("glm-4.6".to_string()),
            policy: Some("balanced".to_string()),
            ts: Some(ts),
        }
    }

    fn subscription_status(kind: SessionAssignmentKind) -> SessionStatusSnapshot {
        let primary_account_id = if kind == SessionAssignmentKind::Primary {
            "personal"
        } else {
            "work"
        };
        SessionStatusSnapshot {
            schema: SESSION_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            assignment: SessionAssignmentStatus {
                primary_account_id: primary_account_id.to_owned(),
                current_account_id: "personal".to_owned(),
                current_model_family: "fable".to_owned(),
                kind,
                reason: SessionAssignmentReason::QuotaFailover,
                assigned_at_unix: NOW,
                last_seen_at_unix: NOW,
            },
            capacity: SessionCapacityStatus {
                usage_snapshot_fresh: true,
                effective_headroom: Some(0.42),
                bottleneck: Some(SessionLimitStatus {
                    key: "fable_weekly".to_owned(),
                    scope: "model:fable".to_owned(),
                    used_fraction: 0.58,
                    remaining_fraction: 0.42,
                    resets_at: None,
                }),
                applicable: Vec::new(),
                eligible_accounts: 2,
                total_accounts: 3,
            },
            placement: SessionPlacementStatus {
                strategy: "balanced_sessions".to_owned(),
                score: Some(0.21),
                active_global_leases: 1,
                active_model_leases: 1,
            },
            route: None,
            updated_at_unix: NOW,
        }
    }

    #[test]
    fn fresh_route_status_shows_model_and_policy() {
        let line = render(Some(&route_status(NOW)), None, NOW);
        assert!(line.contains("glm-4.6"));
        assert!(line.contains("balanced"));
        assert!(line.contains(ZAP));
    }

    #[test]
    fn stale_route_status_falls_back_to_virtual_name() {
        let stale = route_status(NOW - ROUTE_STALE_AFTER_SECONDS - 1);
        let session = json!({"model": {"display_name": "Rayline Router (balanced)"}});
        let line = render(Some(&stale), Some(&session), NOW);
        assert!(!line.contains("glm-4.6"));
        assert!(line.contains("Rayline Router (balanced)"));
    }

    #[test]
    fn subscription_fragment_shows_assignment_bottleneck_and_pool_reserve() {
        let line = render_subscription(Some(&subscription_status(
            SessionAssignmentKind::ModelOverride,
        )));
        assert!(line.contains("work→personal"));
        assert!(line.contains("F"));
        assert!(line.contains("42%L"));
        assert!(line.contains("2/3"));
    }

    #[test]
    fn launch_scoped_route_renders_selected_model_and_policy() {
        let mut snapshot = subscription_status(SessionAssignmentKind::Primary);
        snapshot.route = Some(SessionRouteStatus {
            selected_model: "zhipu/glm-4.6".to_owned(),
            virtual_model: Some("rayline-router".to_owned()),
            policy: Some("balanced".to_owned()),
            task_class: Some("debugging".to_owned()),
            route_id: Some("route-1".to_owned()),
            updated_at_unix: NOW,
        });
        let route = fresh_session_route(Some(&snapshot), NOW);
        let line = render_session_route(route, None, NOW);
        assert!(line.contains("glm-4.6"));
        assert!(!line.contains("zhipu/"));
        assert!(line.contains("balanced"));
    }

    #[test]
    fn stale_launch_route_falls_back_without_hiding_subscription() {
        let mut snapshot = subscription_status(SessionAssignmentKind::Primary);
        snapshot.route = Some(SessionRouteStatus {
            selected_model: "glm-4.6".to_owned(),
            virtual_model: None,
            policy: None,
            task_class: None,
            route_id: None,
            updated_at_unix: NOW - ROUTE_STALE_AFTER_SECONDS - 1,
        });
        assert!(fresh_session_route(Some(&snapshot), NOW).is_none());
        let session = json!({"model": {"id": "rayline-router"}});
        let line = render_session_route(snapshot.route.as_ref(), Some(&session), NOW);
        assert!(line.contains("rayline-router"));
        assert!(render_subscription(Some(&snapshot)).contains("personal"));
    }

    #[test]
    fn primary_subscription_omits_override_marker() {
        let line = render_subscription(Some(&subscription_status(SessionAssignmentKind::Primary)));
        assert!(line.contains("◈ personal"));
        assert!(!line.contains('→'));
    }

    #[test]
    fn subscription_renderer_rejects_untrusted_account_labels() {
        let mut snapshot = subscription_status(SessionAssignmentKind::Primary);
        snapshot.assignment.current_account_id = "\x1b[31mprivate-account".to_owned();
        let line = render_subscription(Some(&snapshot));
        assert!(line.contains("subscription"));
        assert!(!line.contains("private-account"));
    }

    #[test]
    fn virtual_model_accepts_plain_string() {
        let session = json!({"model": "rayline-router"});
        assert_eq!(virtual_model(&session), Some("rayline-router"));
    }

    #[test]
    fn nothing_available_is_empty() {
        assert_eq!(render(None, None, NOW), "");
        assert_eq!(render_subscription(None), "");
    }

    #[test]
    fn component_filtering_keeps_subscription_and_route_output_separate() {
        assert_eq!(
            compose_text_output(
                StatuslineComponent::Subscription,
                "route".to_owned(),
                "subscription".to_owned(),
            ),
            "subscription"
        );
        assert_eq!(
            compose_text_output(
                StatuslineComponent::Route,
                "route".to_owned(),
                "subscription".to_owned(),
            ),
            "route"
        );
    }
}
