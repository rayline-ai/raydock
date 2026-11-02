use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub const REQUEST_ID_HEADER: &str = "x-rayline-request-id";
/// The cloud router's own id for a forwarded request (`rt_…`). The gateway sets
/// this on every turn it routes; `x-workshop-route-id` carries the same value
/// under a legacy name, so reading one name is enough.
///
/// Deliberately NOT `x-request-id`: the gateway sets that to the route id only
/// on an ARC cell turn, and a direct-provider forward relays the *provider's*
/// own `x-request-id` instead — a different id space. Recording that here would
/// make `upstream_request_id` mean "a cloud route" sometimes and "an OpenAI
/// request" other times, which is exactly the ambiguity this field removes.
pub const UPSTREAM_ROUTE_ID_HEADER: &str = "x-rayline-route-id";
pub const DEFAULT_METRICS_PORT: u16 = 20813;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub type SharedMetricsSink = Arc<dyn MetricsSink>;

pub trait MetricsSink: Send + Sync {
    fn record(&self, update: MetricsUpdate);
}

pub fn new_request_id() -> String {
    let seq = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("req_{:x}_{seq:x}", now_unix_ms())
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricsSnapshot {
    pub ok: bool,
    pub runtime: String,
    pub started_at_unix_ms: u64,
    pub active: Vec<RequestSnapshot>,
    pub recent: Vec<RequestSnapshot>,
    pub totals: RouterTotals,
    pub llama_perf: Option<LlamaPerfSnapshot>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RouterTotals {
    pub active_requests: usize,
    pub completed_requests: u64,
    pub errored_requests: u64,
    pub local_requests: u64,
    pub remote_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Incremented each time an `agent-id` header was present but `agentType`
    /// resolution exhausted all retries without finding the meta file.
    /// Non-zero means Claude Code's internal schema may have changed (or a
    /// timing/path issue). Surface in `rld status` so operators notice.
    pub routing_uncertain: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestSnapshot {
    pub request_id: String,
    pub route_id: Option<String>,
    /// The id the upstream (cloud router) gave the forwarded request, when it
    /// sent one; `None` for local inference and for upstreams that send none.
    pub upstream_request_id: Option<String>,
    pub source: String,
    pub state: String,
    pub target: Option<String>,
    pub endpoint_id: Option<String>,
    pub requested_model: Option<String>,
    pub selected_model: Option<String>,
    pub policy: Option<String>,
    pub task_class: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub started_at_unix_ms: u64,
    pub first_token_at_unix_ms: Option<u64>,
    pub completed_at_unix_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub status_code: Option<u16>,
    pub error: Option<String>,
    pub ttft_ms: Option<u64>,
    pub duration_ms: u64,
    pub prefill_tps: Option<f64>,
    pub output_tps: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub prompt_cache_tokens: Option<u64>,
    pub prompt_processed_tokens: Option<u64>,
    pub cache_hit_ratio: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlamaPerfSnapshot {
    pub prefill_tokens_per_second: Option<f64>,
    pub generation_tokens_per_second: Option<f64>,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetricsUpdate {
    RequestStarted {
        request_id: String,
        source: String,
        requested_model: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
    },
    RouteDecided {
        request_id: String,
        route_id: Option<String>,
        target: String,
        endpoint_id: Option<String>,
        selected_model: Option<String>,
        requested_model: Option<String>,
        policy: Option<String>,
        task_class: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
    },
    FirstToken {
        request_id: String,
    },
    /// The upstream's own id for a forwarded request, read from its response
    /// headers once they arrive.
    UpstreamRequestId {
        request_id: String,
        upstream_request_id: String,
    },
    TokenUsage {
        request_id: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        selected_model: Option<String>,
    },
    PromptCache {
        request_id: String,
        prompt_tokens: Option<u64>,
        cache_tokens: Option<u64>,
        processed_tokens: Option<u64>,
        prompt_ms: Option<f64>,
        prompt_tps: Option<f64>,
    },
    RequestCompleted {
        request_id: String,
        status_code: Option<u16>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        selected_model: Option<String>,
    },
    RequestErrored {
        request_id: String,
        status_code: Option<u16>,
        error: String,
    },
    LlamaPerf(LlamaPerfSnapshot),
    /// Emitted when an `agent-id` header was present but `agentType` resolution
    /// exhausted all retries. Increments the `routing_uncertain` counter.
    RoutingUncertain {
        agent_id: String,
    },
}

pub struct RouterMetrics {
    runtime: String,
    started_at_unix_ms: u64,
    active: Mutex<HashMap<String, RequestRecord>>,
    recent: Mutex<VecDeque<RequestRecord>>,
    totals: Mutex<RouterTotals>,
    llama_perf: Mutex<Option<LlamaPerfSnapshot>>,
    updates: broadcast::Sender<MetricsUpdate>,
    recent_capacity: usize,
}

impl RouterMetrics {
    pub fn new(runtime: impl Into<String>) -> Arc<Self> {
        let (updates, _) = broadcast::channel(256);
        Arc::new(Self {
            runtime: runtime.into(),
            started_at_unix_ms: now_unix_ms(),
            active: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            totals: Mutex::new(RouterTotals::default()),
            llama_perf: Mutex::new(None),
            updates,
            recent_capacity: 200,
        })
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let now = now_unix_ms();
        let active = self
            .active
            .lock()
            .expect("metrics active lock poisoned")
            .values()
            .map(|record| record.snapshot(now))
            .collect::<Vec<_>>();
        let recent = self
            .recent
            .lock()
            .expect("metrics recent lock poisoned")
            .iter()
            .rev()
            .map(|record| record.snapshot(now))
            .collect::<Vec<_>>();
        let mut totals = self
            .totals
            .lock()
            .expect("metrics totals lock poisoned")
            .clone();
        totals.active_requests = active.len();
        MetricsSnapshot {
            ok: true,
            runtime: self.runtime.clone(),
            started_at_unix_ms: self.started_at_unix_ms,
            active,
            recent,
            totals,
            llama_perf: self
                .llama_perf
                .lock()
                .expect("metrics llama lock poisoned")
                .clone(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MetricsUpdate> {
        self.updates.subscribe()
    }

    fn apply(&self, update: MetricsUpdate) {
        let now = now_unix_ms();
        match &update {
            MetricsUpdate::RequestStarted {
                request_id,
                source,
                requested_model,
                agent_id,
                agent_type,
            } => {
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                let record = active
                    .entry(request_id.clone())
                    .or_insert_with(|| RequestRecord::new(request_id.clone(), now));
                record.source = source.clone();
                record.state = "started".to_owned();
                merge_option(&mut record.requested_model, requested_model.clone());
                merge_option(&mut record.agent_id, agent_id.clone());
                merge_option(&mut record.agent_type, agent_type.clone());
            }
            MetricsUpdate::RouteDecided {
                request_id,
                route_id,
                target,
                endpoint_id,
                selected_model,
                requested_model,
                policy,
                task_class,
                agent_id,
                agent_type,
            } => {
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                let record = active
                    .entry(request_id.clone())
                    .or_insert_with(|| RequestRecord::new(request_id.clone(), now));
                record.state = "routed".to_owned();
                record.target = Some(target.clone());
                merge_option(&mut record.route_id, route_id.clone());
                merge_option(&mut record.endpoint_id, endpoint_id.clone());
                merge_option(&mut record.selected_model, selected_model.clone());
                merge_option(&mut record.requested_model, requested_model.clone());
                merge_option(&mut record.policy, policy.clone());
                merge_option(&mut record.task_class, task_class.clone());
                merge_option(&mut record.agent_id, agent_id.clone());
                merge_option(&mut record.agent_type, agent_type.clone());
            }
            MetricsUpdate::FirstToken { request_id } => {
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                let record = active
                    .entry(request_id.clone())
                    .or_insert_with(|| RequestRecord::new(request_id.clone(), now));
                record.state = "streaming".to_owned();
                record.first_token_at_unix_ms.get_or_insert(now);
            }
            MetricsUpdate::UpstreamRequestId {
                request_id,
                upstream_request_id,
            } => {
                // The forwarder records this as soon as the upstream headers are
                // in, which can be before anything else announced the request
                // (Codex's auxiliary endpoints) or after a short response already
                // completed. Like `FirstToken`, start the row when it is new; the
                // same forwarder completes it. Holding `active` while checking
                // `recent` pairs with completion holding it across `push_recent`,
                // so the id cannot fall between the two.
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                if !self.merge_recent_upstream_request_id(request_id, upstream_request_id) {
                    let record = active
                        .entry(request_id.clone())
                        .or_insert_with(|| RequestRecord::new(request_id.clone(), now));
                    record.upstream_request_id = Some(upstream_request_id.clone());
                }
            }
            MetricsUpdate::TokenUsage {
                request_id,
                input_tokens,
                output_tokens,
                selected_model,
            } => {
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                if let Some(record) = active.get_mut(request_id) {
                    merge_option(&mut record.input_tokens, *input_tokens);
                    merge_option(&mut record.output_tokens, *output_tokens);
                    merge_option(&mut record.selected_model, selected_model.clone());
                } else {
                    drop(active);
                    self.merge_recent_token_usage(
                        request_id,
                        *input_tokens,
                        *output_tokens,
                        selected_model.clone(),
                    );
                }
            }
            MetricsUpdate::PromptCache {
                request_id,
                prompt_tokens,
                cache_tokens,
                processed_tokens,
                prompt_ms,
                prompt_tps,
            } => {
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                if let Some(record) = active.get_mut(request_id) {
                    merge_option(&mut record.prompt_tokens, *prompt_tokens);
                    merge_option(&mut record.prompt_cache_tokens, *cache_tokens);
                    merge_option(&mut record.prompt_processed_tokens, *processed_tokens);
                    merge_option(&mut record.prompt_ms, *prompt_ms);
                    merge_option(&mut record.prompt_tps, *prompt_tps);
                } else {
                    drop(active);
                    self.merge_recent_prompt_cache(
                        request_id,
                        *prompt_tokens,
                        *cache_tokens,
                        *processed_tokens,
                        *prompt_ms,
                        *prompt_tps,
                    );
                }
            }
            MetricsUpdate::RequestCompleted {
                request_id,
                status_code,
                input_tokens,
                output_tokens,
                selected_model,
            } => {
                // Hold `active` until the row is in `recent`: a late update for
                // this request (its upstream id, usage) must find it in one
                // place or the other, never in neither. `push_recent` takes
                // `totals` then `recent` under it, so the order is always
                // active -> totals -> recent.
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                let mut record = active
                    .remove(request_id)
                    .unwrap_or_else(|| RequestRecord::new(request_id.clone(), now));
                record.state = "completed".to_owned();
                record.completed_at_unix_ms = Some(now);
                record.status_code = *status_code;
                merge_option(&mut record.input_tokens, *input_tokens);
                merge_option(&mut record.output_tokens, *output_tokens);
                merge_option(&mut record.selected_model, selected_model.clone());
                self.push_recent(record);
            }
            MetricsUpdate::RequestErrored {
                request_id,
                status_code,
                error,
            } => {
                // Same lock discipline as `RequestCompleted`.
                let mut active = self.active.lock().expect("metrics active lock poisoned");
                let mut record = active
                    .remove(request_id)
                    .unwrap_or_else(|| RequestRecord::new(request_id.clone(), now));
                record.state = "error".to_owned();
                record.completed_at_unix_ms = Some(now);
                record.status_code = *status_code;
                record.error = Some(error.clone());
                {
                    let mut totals = self.totals.lock().expect("metrics totals lock poisoned");
                    totals.errored_requests = totals.errored_requests.saturating_add(1);
                }
                self.push_recent(record);
            }
            MetricsUpdate::LlamaPerf(snapshot) => {
                if let Some(prefill_tps) = snapshot.prefill_tokens_per_second {
                    self.apply_single_active_local_prefill(prefill_tps);
                }
                let mut llama_perf = self.llama_perf.lock().expect("metrics llama lock poisoned");
                let mut merged = llama_perf.clone().unwrap_or(LlamaPerfSnapshot {
                    prefill_tokens_per_second: None,
                    generation_tokens_per_second: None,
                    updated_at_unix_ms: snapshot.updated_at_unix_ms,
                });
                if snapshot.prefill_tokens_per_second.is_some() {
                    merged.prefill_tokens_per_second = snapshot.prefill_tokens_per_second;
                }
                if snapshot.generation_tokens_per_second.is_some() {
                    merged.generation_tokens_per_second = snapshot.generation_tokens_per_second;
                }
                merged.updated_at_unix_ms = snapshot.updated_at_unix_ms;
                *llama_perf = Some(merged);
            }
            MetricsUpdate::RoutingUncertain { .. } => {
                let mut totals = self.totals.lock().expect("metrics totals lock poisoned");
                totals.routing_uncertain = totals.routing_uncertain.saturating_add(1);
            }
        }
        let _ = self.updates.send(update);
    }

    fn apply_single_active_local_prefill(&self, prefill_tps: f64) {
        let mut active = self.active.lock().expect("metrics active lock poisoned");
        let local_request_ids = active
            .iter()
            .filter(|(_, record)| record.target.as_deref() == Some("local"))
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        if local_request_ids.len() != 1 {
            return;
        }
        if let Some(record) = active.get_mut(&local_request_ids[0]) {
            record.prompt_tps = Some(prefill_tps);
        }
    }

    fn merge_recent_token_usage(
        &self,
        request_id: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        selected_model: Option<String>,
    ) {
        let delta = {
            let mut recent = self.recent.lock().expect("metrics recent lock poisoned");
            let Some(record) = recent
                .iter_mut()
                .rev()
                .find(|record| record.request_id == request_id)
            else {
                return;
            };
            let old_input = record.input_tokens.unwrap_or(0);
            let old_output = record.output_tokens.unwrap_or(0);
            merge_option(&mut record.input_tokens, input_tokens);
            merge_option(&mut record.output_tokens, output_tokens);
            merge_option(&mut record.selected_model, selected_model);
            (
                old_input,
                record.input_tokens.unwrap_or(0),
                old_output,
                record.output_tokens.unwrap_or(0),
            )
        };
        if delta.0 != delta.1 || delta.2 != delta.3 {
            let mut totals = self.totals.lock().expect("metrics totals lock poisoned");
            totals.input_tokens = adjust_total(totals.input_tokens, delta.0, delta.1);
            totals.output_tokens = adjust_total(totals.output_tokens, delta.2, delta.3);
        }
    }

    /// A short response can complete before its upstream id is recorded; attach
    /// it to the finished row. Returns whether such a row was found.
    fn merge_recent_upstream_request_id(
        &self,
        request_id: &str,
        upstream_request_id: &str,
    ) -> bool {
        let mut recent = self.recent.lock().expect("metrics recent lock poisoned");
        match recent
            .iter_mut()
            .rev()
            .find(|record| record.request_id == request_id)
        {
            Some(record) => {
                record.upstream_request_id = Some(upstream_request_id.to_owned());
                true
            }
            None => false,
        }
    }

    fn merge_recent_prompt_cache(
        &self,
        request_id: &str,
        prompt_tokens: Option<u64>,
        cache_tokens: Option<u64>,
        processed_tokens: Option<u64>,
        prompt_ms: Option<f64>,
        prompt_tps: Option<f64>,
    ) {
        let mut recent = self.recent.lock().expect("metrics recent lock poisoned");
        let Some(record) = recent
            .iter_mut()
            .rev()
            .find(|record| record.request_id == request_id)
        else {
            return;
        };
        merge_option(&mut record.prompt_tokens, prompt_tokens);
        merge_option(&mut record.prompt_cache_tokens, cache_tokens);
        merge_option(&mut record.prompt_processed_tokens, processed_tokens);
        merge_option(&mut record.prompt_ms, prompt_ms);
        merge_option(&mut record.prompt_tps, prompt_tps);
    }

    fn push_recent(&self, record: RequestRecord) {
        {
            let mut totals = self.totals.lock().expect("metrics totals lock poisoned");
            totals.completed_requests = totals.completed_requests.saturating_add(1);
            if record.target.as_deref() == Some("local") {
                totals.local_requests = totals.local_requests.saturating_add(1);
            } else if record.target.is_some() {
                totals.remote_requests = totals.remote_requests.saturating_add(1);
            }
            totals.input_tokens = totals
                .input_tokens
                .saturating_add(record.input_tokens.unwrap_or(0));
            totals.output_tokens = totals
                .output_tokens
                .saturating_add(record.output_tokens.unwrap_or(0));
        }
        let mut recent = self.recent.lock().expect("metrics recent lock poisoned");
        recent.push_back(record);
        while recent.len() > self.recent_capacity {
            recent.pop_front();
        }
    }
}

impl MetricsSink for RouterMetrics {
    fn record(&self, update: MetricsUpdate) {
        self.apply(update);
    }
}

#[derive(Clone, Debug)]
struct RequestRecord {
    request_id: String,
    route_id: Option<String>,
    upstream_request_id: Option<String>,
    source: String,
    state: String,
    target: Option<String>,
    endpoint_id: Option<String>,
    requested_model: Option<String>,
    selected_model: Option<String>,
    policy: Option<String>,
    task_class: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    started_at_unix_ms: u64,
    first_token_at_unix_ms: Option<u64>,
    completed_at_unix_ms: Option<u64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    prompt_tokens: Option<u64>,
    prompt_cache_tokens: Option<u64>,
    prompt_processed_tokens: Option<u64>,
    prompt_ms: Option<f64>,
    prompt_tps: Option<f64>,
    status_code: Option<u16>,
    error: Option<String>,
}

impl RequestRecord {
    fn new(request_id: String, now: u64) -> Self {
        Self {
            request_id,
            route_id: None,
            upstream_request_id: None,
            source: "unknown".to_owned(),
            state: "started".to_owned(),
            target: None,
            endpoint_id: None,
            requested_model: None,
            selected_model: None,
            policy: None,
            task_class: None,
            agent_id: None,
            agent_type: None,
            started_at_unix_ms: now,
            first_token_at_unix_ms: None,
            completed_at_unix_ms: None,
            input_tokens: None,
            output_tokens: None,
            prompt_tokens: None,
            prompt_cache_tokens: None,
            prompt_processed_tokens: None,
            prompt_ms: None,
            prompt_tps: None,
            status_code: None,
            error: None,
        }
    }

    fn snapshot(&self, now: u64) -> RequestSnapshot {
        let completed_or_now = self.completed_at_unix_ms.unwrap_or(now);
        let duration_ms = completed_or_now.saturating_sub(self.started_at_unix_ms);
        let ttft_ms = self
            .first_token_at_unix_ms
            .map(|first| first.saturating_sub(self.started_at_unix_ms));
        let prefill_tps = if self.target.as_deref() == Some("local") {
            self.local_prefill_tps()
        } else {
            self.input_tokens.and_then(|tokens| {
                ttft_ms.and_then(|ttft_ms| {
                    (ttft_ms > 0).then(|| tokens as f64 / (ttft_ms as f64 / 1000.0))
                })
            })
        };
        let cache_hit_ratio = self
            .prompt_cache_tokens
            .zip(self.prompt_tokens)
            .and_then(|(cache, total)| (total > 0).then(|| cache as f64 / total as f64));
        let output_tps = self.output_tokens.and_then(|tokens| {
            let generation_ms = self
                .first_token_at_unix_ms
                .map(|first| completed_or_now.saturating_sub(first))
                .unwrap_or(duration_ms);
            (generation_ms > 0).then(|| tokens as f64 / (generation_ms as f64 / 1000.0))
        });
        RequestSnapshot {
            request_id: self.request_id.clone(),
            route_id: self.route_id.clone(),
            upstream_request_id: self.upstream_request_id.clone(),
            source: self.source.clone(),
            state: self.state.clone(),
            target: self.target.clone(),
            endpoint_id: self.endpoint_id.clone(),
            requested_model: self.requested_model.clone(),
            selected_model: self.selected_model.clone(),
            policy: self.policy.clone(),
            task_class: self.task_class.clone(),
            agent_id: self.agent_id.clone(),
            agent_type: self.agent_type.clone(),
            started_at_unix_ms: self.started_at_unix_ms,
            first_token_at_unix_ms: self.first_token_at_unix_ms,
            completed_at_unix_ms: self.completed_at_unix_ms,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            status_code: self.status_code,
            error: self.error.clone(),
            ttft_ms,
            duration_ms,
            prefill_tps,
            output_tps,
            prompt_tokens: self.prompt_tokens,
            prompt_cache_tokens: self.prompt_cache_tokens,
            prompt_processed_tokens: self.prompt_processed_tokens,
            cache_hit_ratio,
        }
    }

    fn local_prefill_tps(&self) -> Option<f64> {
        if self.prompt_tps.is_some() {
            return self.prompt_tps;
        }
        let prompt_ms = self.prompt_ms?;
        if prompt_ms <= 0.0 {
            return None;
        }
        let processed = self.prompt_processed_tokens.or(self.prompt_tokens)?;
        let cache = self.prompt_cache_tokens.unwrap_or(0).min(processed);
        let evaluated = processed.saturating_sub(cache);
        (evaluated > 0).then(|| evaluated as f64 / (prompt_ms / 1000.0))
    }
}

fn merge_option<T>(slot: &mut Option<T>, value: Option<T>) {
    if value.is_some() {
        *slot = value;
    }
}

fn adjust_total(total: u64, old_value: u64, new_value: u64) -> u64 {
    if new_value >= old_value {
        total.saturating_add(new_value - old_value)
    } else {
        total.saturating_sub(old_value - new_value)
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_request_lifecycle_in_memory() {
        let metrics = RouterMetrics::new("test-router");
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: "req-1".to_owned(),
            source: "proxy".to_owned(),
            requested_model: Some("rayline-router".to_owned()),
            agent_id: Some("agent-1".to_owned()),
            agent_type: Some("Explore".to_owned()),
        });
        metrics.record(MetricsUpdate::RouteDecided {
            request_id: "req-1".to_owned(),
            route_id: Some("local-1".to_owned()),
            target: "local".to_owned(),
            endpoint_id: Some("local".to_owned()),
            selected_model: Some("qwen".to_owned()),
            requested_model: Some("rayline-router".to_owned()),
            policy: Some("subagent:Explore".to_owned()),
            task_class: Some("subagent".to_owned()),
            agent_id: Some("agent-1".to_owned()),
            agent_type: Some("Explore".to_owned()),
        });

        let active = metrics.snapshot();
        assert_eq!(active.active.len(), 1);
        assert_eq!(active.active[0].route_id.as_deref(), Some("local-1"));
        assert_eq!(active.active[0].agent_type.as_deref(), Some("Explore"));

        metrics.record(MetricsUpdate::FirstToken {
            request_id: "req-1".to_owned(),
        });
        metrics.record(MetricsUpdate::RequestCompleted {
            request_id: "req-1".to_owned(),
            status_code: Some(200),
            input_tokens: Some(10),
            output_tokens: Some(20),
            selected_model: Some("qwen".to_owned()),
        });

        let completed = metrics.snapshot();
        assert!(completed.active.is_empty());
        assert_eq!(completed.recent.len(), 1);
        assert_eq!(completed.totals.completed_requests, 1);
        assert_eq!(completed.totals.local_requests, 1);
        assert_eq!(completed.totals.input_tokens, 10);
        assert_eq!(completed.totals.output_tokens, 20);
    }

    #[test]
    fn late_usage_updates_merge_recent_without_reactivating() {
        let metrics = RouterMetrics::new("test-router");
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: "req-1".to_owned(),
            source: "proxy".to_owned(),
            requested_model: Some("rayline-router".to_owned()),
            agent_id: None,
            agent_type: None,
        });
        metrics.record(MetricsUpdate::RequestCompleted {
            request_id: "req-1".to_owned(),
            status_code: Some(200),
            input_tokens: Some(10),
            output_tokens: Some(20),
            selected_model: Some("claude".to_owned()),
        });

        metrics.record(MetricsUpdate::TokenUsage {
            request_id: "req-1".to_owned(),
            input_tokens: Some(12),
            output_tokens: Some(25),
            selected_model: Some("claude-opus".to_owned()),
        });
        metrics.record(MetricsUpdate::PromptCache {
            request_id: "req-1".to_owned(),
            prompt_tokens: Some(12),
            cache_tokens: Some(5),
            processed_tokens: Some(12),
            prompt_ms: None,
            prompt_tps: None,
        });

        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.recent.len(), 1);
        assert_eq!(snapshot.recent[0].input_tokens, Some(12));
        assert_eq!(snapshot.recent[0].output_tokens, Some(25));
        assert_eq!(
            snapshot.recent[0].selected_model.as_deref(),
            Some("claude-opus")
        );
        assert_eq!(snapshot.recent[0].prompt_cache_tokens, Some(5));
        assert_eq!(snapshot.totals.input_tokens, 12);
        assert_eq!(snapshot.totals.output_tokens, 25);

        metrics.record(MetricsUpdate::TokenUsage {
            request_id: "req-1".to_owned(),
            input_tokens: Some(8),
            output_tokens: Some(18),
            selected_model: None,
        });

        let corrected = metrics.snapshot();
        assert_eq!(corrected.recent[0].input_tokens, Some(8));
        assert_eq!(corrected.recent[0].output_tokens, Some(18));
        assert_eq!(corrected.totals.input_tokens, 8);
        assert_eq!(corrected.totals.output_tokens, 18);
    }

    #[test]
    fn upstream_request_id_attaches_to_active_and_recent_rows() {
        let metrics = RouterMetrics::new("test-router");
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: "req-1".to_owned(),
            source: "local-router".to_owned(),
            requested_model: None,
            agent_id: None,
            agent_type: None,
        });
        metrics.record(MetricsUpdate::UpstreamRequestId {
            request_id: "req-1".to_owned(),
            upstream_request_id: "rt_active".to_owned(),
        });
        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot.active[0].upstream_request_id.as_deref(),
            Some("rt_active")
        );

        metrics.record(MetricsUpdate::RequestCompleted {
            request_id: "req-1".to_owned(),
            status_code: Some(200),
            input_tokens: None,
            output_tokens: None,
            selected_model: None,
        });
        // A short response completes before its handler reads the header: the
        // id still lands on the finished row and nothing is reactivated.
        metrics.record(MetricsUpdate::UpstreamRequestId {
            request_id: "req-1".to_owned(),
            upstream_request_id: "rt_recent".to_owned(),
        });
        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.recent.len(), 1);
        assert_eq!(
            snapshot.recent[0].upstream_request_id.as_deref(),
            Some("rt_recent")
        );

        // Nothing announced this request yet (Codex's auxiliary endpoints only
        // report once the stream starts): the id starts the row, and the
        // forwarder's completion finds it there.
        metrics.record(MetricsUpdate::UpstreamRequestId {
            request_id: "req-new".to_owned(),
            upstream_request_id: "rt_first".to_owned(),
        });
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.active.len(), 1);
        assert_eq!(snapshot.active[0].request_id, "req-new");
        assert_eq!(
            snapshot.active[0].upstream_request_id.as_deref(),
            Some("rt_first")
        );
        metrics.record(MetricsUpdate::FirstToken {
            request_id: "req-new".to_owned(),
        });
        metrics.record(MetricsUpdate::RequestCompleted {
            request_id: "req-new".to_owned(),
            status_code: Some(200),
            input_tokens: None,
            output_tokens: None,
            selected_model: None,
        });
        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert_eq!(
            snapshot.recent[0].upstream_request_id.as_deref(),
            Some("rt_first")
        );
    }

    /// Stress the invariant that a row is reachable throughout completion: with
    /// one thread completing requests and another recording their upstream ids,
    /// every finished row keeps its own id.
    ///
    /// Not a deterministic reproduction of the lost-update race. The barrier
    /// releases both threads before either finishes its operation, so the
    /// scheduler may serialise them; pinning the interleaving would need a sync
    /// point inside `apply` or a model checker. The sequential half of the
    /// invariant is covered deterministically by
    /// `upstream_request_id_attaches_to_active_and_recent_rows`.
    #[test]
    fn upstream_request_id_survives_concurrent_completion() {
        use std::sync::Barrier;

        let metrics = RouterMetrics::new("test-router");
        let rounds = 100;
        let barrier = Arc::new(Barrier::new(2));
        let completer = {
            let metrics = metrics.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                for round in 0..rounds {
                    metrics.record(MetricsUpdate::RequestStarted {
                        request_id: format!("req-{round}"),
                        source: "test".to_owned(),
                        requested_model: None,
                        agent_id: None,
                        agent_type: None,
                    });
                    barrier.wait();
                    metrics.record(MetricsUpdate::RequestCompleted {
                        request_id: format!("req-{round}"),
                        status_code: Some(200),
                        input_tokens: None,
                        output_tokens: None,
                        selected_model: None,
                    });
                    barrier.wait();
                }
            })
        };
        let tagger = {
            let metrics = metrics.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                for round in 0..rounds {
                    barrier.wait();
                    metrics.record(MetricsUpdate::UpstreamRequestId {
                        request_id: format!("req-{round}"),
                        upstream_request_id: format!("rt-{round}"),
                    });
                    barrier.wait();
                }
            })
        };
        completer.join().expect("completer thread");
        tagger.join().expect("tagger thread");

        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.recent.len(), rounds);
        for row in &snapshot.recent {
            let round = row.request_id.trim_start_matches("req-");
            assert_eq!(
                row.upstream_request_id.as_deref(),
                Some(format!("rt-{round}").as_str()),
                "row {} lost its upstream id",
                row.request_id
            );
        }
    }

    #[test]
    fn unknown_usage_updates_do_not_create_active_rows() {
        let metrics = RouterMetrics::new("test-router");
        metrics.record(MetricsUpdate::TokenUsage {
            request_id: "missing".to_owned(),
            input_tokens: Some(12),
            output_tokens: Some(25),
            selected_model: Some("claude".to_owned()),
        });
        metrics.record(MetricsUpdate::PromptCache {
            request_id: "missing".to_owned(),
            prompt_tokens: Some(12),
            cache_tokens: Some(5),
            processed_tokens: Some(12),
            prompt_ms: None,
            prompt_tps: None,
        });

        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert!(snapshot.recent.is_empty());
        assert_eq!(snapshot.totals.input_tokens, 0);
        assert_eq!(snapshot.totals.output_tokens, 0);
    }

    #[test]
    fn merges_partial_llama_perf_updates() {
        let metrics = RouterMetrics::new("test-router");
        metrics.record(MetricsUpdate::LlamaPerf(LlamaPerfSnapshot {
            prefill_tokens_per_second: Some(123.4),
            generation_tokens_per_second: None,
            updated_at_unix_ms: 10,
        }));
        metrics.record(MetricsUpdate::LlamaPerf(LlamaPerfSnapshot {
            prefill_tokens_per_second: None,
            generation_tokens_per_second: Some(56.7),
            updated_at_unix_ms: 20,
        }));

        let snapshot = metrics.snapshot().llama_perf.expect("llama perf");
        assert_eq!(snapshot.prefill_tokens_per_second, Some(123.4));
        assert_eq!(snapshot.generation_tokens_per_second, Some(56.7));
        assert_eq!(snapshot.updated_at_unix_ms, 20);
    }

    #[test]
    fn calculates_live_prefill_and_generation_rates() {
        let mut record = RequestRecord::new("req-1".to_owned(), 1_000);
        record.target = Some("anthropic".to_owned());
        record.first_token_at_unix_ms = Some(3_000);
        record.input_tokens = Some(1_000);
        record.output_tokens = Some(200);

        let snapshot = record.snapshot(5_000);
        assert_eq!(snapshot.prefill_tps, Some(500.0));
        assert_eq!(snapshot.output_tps, Some(100.0));
    }

    #[test]
    fn calculates_prompt_cache_hit_ratio() {
        let mut record = RequestRecord::new("req-1".to_owned(), 1_000);
        record.target = Some("local".to_owned());
        record.prompt_tokens = Some(1_000);
        record.prompt_cache_tokens = Some(750);
        record.prompt_processed_tokens = Some(1_000);
        record.prompt_ms = Some(500.0);

        let snapshot = record.snapshot(2_000);
        assert_eq!(snapshot.prompt_tokens, Some(1_000));
        assert_eq!(snapshot.prompt_cache_tokens, Some(750));
        assert_eq!(snapshot.prompt_processed_tokens, Some(1_000));
        assert_eq!(snapshot.cache_hit_ratio, Some(0.75));
        assert_eq!(snapshot.prefill_tps, Some(500.0));
    }

    #[test]
    fn local_prefill_requires_prompt_timing() {
        let mut record = RequestRecord::new("req-1".to_owned(), 1_000);
        record.target = Some("local".to_owned());
        record.first_token_at_unix_ms = Some(1_010);
        record.input_tokens = Some(1_000);

        let snapshot = record.snapshot(2_000);
        assert_eq!(snapshot.prefill_tps, None);
    }

    #[test]
    fn applies_llama_prefill_to_single_active_local_request() {
        let metrics = RouterMetrics::new("test-router");
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: "req-1".to_owned(),
            source: "adapter".to_owned(),
            requested_model: Some("local-router".to_owned()),
            agent_id: None,
            agent_type: None,
        });
        metrics.record(MetricsUpdate::RouteDecided {
            request_id: "req-1".to_owned(),
            route_id: Some("route-1".to_owned()),
            target: "local".to_owned(),
            endpoint_id: Some("local".to_owned()),
            selected_model: Some("qwen".to_owned()),
            requested_model: Some("local-router".to_owned()),
            policy: Some("local-adapter".to_owned()),
            task_class: None,
            agent_id: None,
            agent_type: Some("Explore".to_owned()),
        });
        metrics.record(MetricsUpdate::LlamaPerf(LlamaPerfSnapshot {
            prefill_tokens_per_second: Some(321.0),
            generation_tokens_per_second: None,
            updated_at_unix_ms: 20,
        }));

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.active[0].prefill_tps, Some(321.0));
    }

    #[test]
    fn skips_llama_prefill_when_multiple_local_requests_are_active() {
        let metrics = RouterMetrics::new("test-router");
        for request_id in ["req-1", "req-2"] {
            metrics.record(MetricsUpdate::RouteDecided {
                request_id: request_id.to_owned(),
                route_id: None,
                target: "local".to_owned(),
                endpoint_id: Some("local".to_owned()),
                selected_model: Some("qwen".to_owned()),
                requested_model: Some("local-router".to_owned()),
                policy: Some("local-adapter".to_owned()),
                task_class: None,
                agent_id: None,
                agent_type: Some("Explore".to_owned()),
            });
        }
        metrics.record(MetricsUpdate::LlamaPerf(LlamaPerfSnapshot {
            prefill_tokens_per_second: Some(321.0),
            generation_tokens_per_second: None,
            updated_at_unix_ms: 20,
        }));

        let snapshot = metrics.snapshot();
        assert!(snapshot.active.iter().all(|row| row.prefill_tps.is_none()));
    }
}
