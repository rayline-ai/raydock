//! `rld statusline` — render the router's per-turn picked model for a Claude
//! Code status line.
//!
//! The proxy (`rld serve`/`rld proxy`) writes the router's decision to a sidecar
//! JSON file after each Rayline-routed response. This subcommand reads that file
//! and prints a single status-line fragment (ANSI colours allowed). It is
//! the fast, dependency-free reader for Claude Code's `statusLine` hook: Claude
//! Code runs it per render tick, piping the session JSON on stdin and using
//! stdout as the status text.
//!
//! Designed to compose: pipe the session JSON in and append the output to an
//! existing status line, e.g.
//! ```sh
//! input=$(cat)
//! printf '%s' "$input" | my-existing-statusline
//! printf '%s' "$input" | rld statusline
//! ```
//!
//! When the sidecar is missing or stale (idle session, override routing mode
//! with no proxy) it falls back to the virtual model name from the stdin
//! session JSON, so it degrades gracefully instead of going blank mid-turn.

use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

/// Claude Code re-renders the status line roughly every second. Treat a sidecar
/// older than this as stale: the proxy is idle, the session ended, or a
/// different session owns the (single, global) sidecar.
const STALE_AFTER_SECONDS: i64 = 300;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const ZAP: &str = "⚡";

#[derive(Debug, Deserialize, Default)]
pub struct RouteStatusFile {
    pub selected_model: Option<String>,
    pub policy: Option<String>,
    pub ts: Option<i64>,
}

impl RouteStatusFile {
    /// The picked model, but only if the sidecar is fresh (written within the
    /// staleness window, allowing for no clock skew into the future).
    fn fresh_selected_model(&self, now: i64) -> Option<&str> {
        let ts = self.ts?;
        let age = now - ts;
        if !(0..=STALE_AFTER_SECONDS).contains(&age) {
            return None;
        }
        self.selected_model.as_deref().filter(|m| !m.is_empty())
    }
}

/// Drop any `provider/` prefix; keep the concrete model id as-is.
fn short_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The virtual model name Claude Code thinks it is using, from the session JSON
/// it pipes on stdin. Used as the fallback when the sidecar is absent/stale.
fn virtual_model(session: &Value) -> Option<&str> {
    let model = session.get("model")?;
    if let Some(obj) = model.as_object() {
        for key in ["display_name", "id"] {
            if let Some(name) = obj.get(key).and_then(Value::as_str) {
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        return None;
    }
    model.as_str().filter(|s| !s.is_empty())
}

/// Render the status-line fragment. Empty string when neither a fresh picked
/// model nor a virtual name is available.
pub fn render(sidecar: Option<&RouteStatusFile>, session: Option<&Value>, now: i64) -> String {
    if let Some(model) = sidecar.and_then(|s| s.fresh_selected_model(now)) {
        let mut line = format!("{ZAP} {}", short_model(model));
        if let Some(policy) = sidecar
            .and_then(|s| s.policy.as_deref())
            .filter(|p| !p.is_empty())
        {
            line.push_str(&format!(" {DIM}· {policy}{RESET}"));
        }
        return line;
    }
    if let Some(name) = session.and_then(virtual_model) {
        return format!("{DIM}{}{RESET}", short_model(name));
    }
    String::new()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the sidecar (best-effort: missing/malformed → `None`).
fn read_sidecar(path: &PathBuf) -> Option<RouteStatusFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Read and parse the session JSON Claude Code pipes on stdin (best-effort).
fn read_stdin_session() -> Option<Value> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&raw).ok()
}

/// Entry point for the `rld statusline` subcommand. Always exits 0 — a status
/// line must never error out and break Claude Code's render.
pub fn run(route_status_path: PathBuf) {
    let sidecar = read_sidecar(&route_status_path);
    let session = read_stdin_session();
    print!("{}", render(sidecar.as_ref(), session.as_ref(), now_unix()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_000_000;

    fn sidecar(ts: i64) -> RouteStatusFile {
        RouteStatusFile {
            selected_model: Some("glm-4.6".to_string()),
            policy: Some("balanced".to_string()),
            ts: Some(ts),
        }
    }

    #[test]
    fn fresh_sidecar_shows_model_and_policy() {
        let line = render(Some(&sidecar(NOW)), None, NOW);
        assert!(line.contains("glm-4.6"));
        assert!(line.contains("balanced"));
        assert!(line.contains(ZAP));
    }

    #[test]
    fn strips_provider_prefix() {
        let s = RouteStatusFile {
            selected_model: Some("zhipu/glm-4.6".to_string()),
            policy: None,
            ts: Some(NOW),
        };
        let line = render(Some(&s), None, NOW);
        assert!(line.contains("glm-4.6"));
        assert!(!line.contains("zhipu/"));
    }

    #[test]
    fn without_policy_omits_separator() {
        let s = RouteStatusFile {
            selected_model: Some("glm-4.6".to_string()),
            policy: None,
            ts: Some(NOW),
        };
        let line = render(Some(&s), None, NOW);
        assert!(line.contains("glm-4.6"));
        assert!(!line.contains('·'));
    }

    #[test]
    fn stale_sidecar_falls_back_to_virtual_name() {
        let stale = sidecar(NOW - STALE_AFTER_SECONDS - 1);
        let session = json!({"model": {"display_name": "Rayline Router (balanced)"}});
        let line = render(Some(&stale), Some(&session), NOW);
        assert!(!line.contains("glm-4.6"));
        assert!(line.contains("Rayline Router (balanced)"));
    }

    #[test]
    fn future_timestamp_clock_skew_is_stale() {
        let future = sidecar(NOW + 10_000);
        let session = json!({"model": {"id": "rayline-auto"}});
        let line = render(Some(&future), Some(&session), NOW);
        assert!(!line.contains("glm-4.6"));
        assert!(line.contains("rayline-auto"));
    }

    #[test]
    fn missing_sidecar_uses_virtual_id() {
        let session = json!({"model": {"id": "rayline-auto"}});
        let line = render(None, Some(&session), NOW);
        assert!(line.contains("rayline-auto"));
    }

    #[test]
    fn nothing_available_is_empty() {
        assert_eq!(render(None, None, NOW), "");
    }

    #[test]
    fn virtual_model_accepts_plain_string() {
        let session = json!({"model": "rayline-auto"});
        assert_eq!(virtual_model(&session), Some("rayline-auto"));
    }
}
