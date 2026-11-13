//! Read-only attribution of execution-core receipts; this module never chooses a route.
use std::collections::HashMap;
use std::sync::Arc;

use hyper::HeaderMap;
use rayline_metrics::{MetricsSink, MetricsUpdate, RouterMetrics, new_request_id};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Deserialize)]
struct Deployment {
    id: String,
    kind: String,
    wire_model: String,
}

#[derive(Default)]
pub(super) struct RoutingMetadata {
    deployments: HashMap<String, Deployment>,
    actions: HashMap<String, Vec<(Option<String>, String)>>,
    derived: HashMap<(String, String), Vec<String>>,
    policy: Option<String>,
}

impl RoutingMetadata {
    pub(super) fn from_config(config: &Value) -> Self {
        let mut metadata = Self::default();
        if let Some(deployments) = config.get("deployments").and_then(Value::as_array) {
            for value in deployments {
                if let Ok(deployment) = serde_json::from_value::<Deployment>(value.clone()) {
                    metadata
                        .deployments
                        .insert(deployment.id.clone(), deployment);
                }
            }
        }
        for field in ["derived_actions", "bindings"] {
            if let Some(bindings) = config.get(field).and_then(Value::as_array) {
                for binding in bindings {
                    let Some(deployment) = binding.get("deployment_id").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if field == "derived_actions"
                        && let (Some(worker), Some(model)) = (
                            binding.get("worker").and_then(Value::as_str),
                            binding.get("model").and_then(Value::as_str),
                        )
                    {
                        metadata
                            .derived
                            .entry((worker.to_owned(), model.to_owned()))
                            .or_default()
                            .push(deployment.to_owned());
                    }
                    if field == "bindings"
                        && let Some(action) = binding.get("action_id").and_then(Value::as_str)
                    {
                        metadata
                            .actions
                            .entry(action.to_owned())
                            .or_default()
                            .push((
                                binding
                                    .get("worker")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                                deployment.to_owned(),
                            ));
                    }
                }
            }
        }
        metadata.policy = config
            .pointer("/decisions/routing/policy/preset")
            .and_then(Value::as_str)
            .map(str::to_owned);
        metadata
    }

    fn deployment(
        &self,
        worker: Option<&str>,
        action: Option<&str>,
        model: Option<&str>,
    ) -> Option<&Deployment> {
        if let Some(bindings) = action.and_then(|id| self.actions.get(id)) {
            let (_, deployment) = bindings.first()?;
            // A known action must agree with every binding and the receipt's worker.
            // Never fall through to derived attribution after contradictory evidence.
            if bindings.iter().any(|(bound_worker, candidate)| {
                candidate != deployment
                    || worker.is_some_and(|worker| bound_worker.as_deref() != Some(worker))
            }) {
                return None;
            }
            return self.deployments.get(deployment);
        }
        let (worker, model) = worker.zip(model)?;
        let candidates = self.derived.get(&(worker.to_owned(), model.to_owned()))?;
        let first = candidates.first()?;
        // Multiple controls on one deployment are fine; two deployments are ambiguous.
        if candidates.iter().any(|candidate| candidate != first) {
            return None;
        }
        self.deployments.get(first)
    }

    pub(super) fn record(
        &self,
        observation: &RequestObservation,
        headers: &HeaderMap,
        instance: &str,
    ) {
        if header(headers, "x-rayline-instance-id").as_deref() != Some(instance) {
            tracing::debug!("hybrid routing receipt missing or belongs to another instance");
            return;
        }
        let executed_model = header(headers, "x-rayline-executed-model");
        let worker = header(headers, "x-rayline-executed-worker");
        let action = header(headers, "x-rayline-executed-action-id");
        let deployment = executed_model.as_ref().and_then(|_| {
            self.deployment(
                worker.as_deref(),
                action.as_deref(),
                executed_model.as_deref(),
            )
        });
        if deployment.is_none() {
            tracing::debug!("hybrid routing receipt has no unambiguous deployment attribution");
        }
        observation
            .metrics
            .record(MetricsUpdate::ExecutionIdentity {
                request_id: observation.id.clone(),
                decision_model: header(headers, "x-rayline-selected-model"),
                executed_model,
                executed_worker: worker,
                executed_action_id: action,
                selected_action_id: header(headers, "x-rayline-action-id"),
                executed_control_id: header(headers, "x-rayline-executed-control-id"),
                wire_model: deployment.map(|deployment| deployment.wire_model.clone()),
            });
        let target = deployment
            .map(|deployment| match deployment.kind.as_str() {
                "local" => "local",
                "cloud" => "remote",
                _ => "unknown",
            })
            .unwrap_or("unknown");
        observation.metrics.record(MetricsUpdate::RouteDecided {
            request_id: observation.id.clone(),
            route_id: header(headers, "x-rayline-route-id"),
            target: target.to_owned(),
            endpoint_id: deployment.map(|deployment| deployment.id.clone()),
            requested_model: None,
            policy: self.policy.clone(),
            task_class: header(headers, "x-rayline-event"),
            agent_id: None,
            agent_type: None,
        });
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .filter(|value| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
}

/// Owned by the response body so a downstream disconnect closes its active row.
pub(super) struct RequestObservation {
    pub(super) id: String,
    pub(super) metrics: Arc<RouterMetrics>,
    finished: bool,
}

impl RequestObservation {
    pub(super) fn start(metrics: Arc<RouterMetrics>, body: &[u8]) -> Self {
        let id = new_request_id();
        // The gateway already buffers this request; retain only its short model ID.
        let requested_model = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| {
                        model.len() <= 256
                            && model.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || b"/-_.:#".contains(&byte)
                            })
                    })
                    .map(str::to_owned)
            });
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: id.clone(),
            source: "hybrid".to_owned(),
            requested_model,
            agent_id: None,
            agent_type: None,
        });
        Self {
            id,
            metrics,
            finished: false,
        }
    }

    pub(super) fn record_classifier(&self, model: &str) {
        // A cloud pin: no routing decision, only the model that serves it.
        self.metrics.record(MetricsUpdate::ExecutionIdentity {
            request_id: self.id.clone(),
            decision_model: None,
            executed_model: Some(model.to_owned()),
            executed_worker: None,
            executed_action_id: None,
            selected_action_id: None,
            executed_control_id: None,
            wire_model: None,
        });
        self.metrics.record(MetricsUpdate::RouteDecided {
            request_id: self.id.clone(),
            route_id: None,
            target: "remote".to_owned(),
            endpoint_id: None,
            requested_model: None,
            policy: Some("classifier cloud pin".to_owned()),
            task_class: Some("sidecall".to_owned()),
            agent_id: None,
            agent_type: None,
        });
    }

    pub(super) fn finish(&mut self, status: u16, error: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(error) = error {
            self.metrics.record(MetricsUpdate::RequestErrored {
                request_id: self.id.clone(),
                status_code: Some(status),
                error: error.to_owned(),
            });
        } else {
            self.metrics.record(MetricsUpdate::RequestCompleted {
                request_id: self.id.clone(),
                status_code: Some(status),
                input_tokens: None,
                output_tokens: None,
            });
        }
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        if !self.finished {
            self.metrics.record(MetricsUpdate::RequestErrored {
                request_id: self.id.clone(),
                status_code: None,
                error: "request cancelled or transport failed".to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn receipts_attribute_execution_not_selection_and_disconnect_closes_the_row() {
        let metadata = RoutingMetadata::from_config(&json!({
            "deployments": [{"id":"spark","kind":"local","wire_model":"served-glm"}],
            "bindings": [{"worker":"glm","action_id":"a1","deployment_id":"spark"}]
        }));
        let metrics = RouterMetrics::new("hybrid");
        let observation =
            RequestObservation::start(metrics.clone(), br#"{"model":"rayline-auto"}"#);
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("x-rayline-instance-id", "instance"),
            ("x-rayline-selected-model", "cloud-model"),
            ("x-rayline-executed-model", "glm"),
            ("x-rayline-executed-worker", "glm"),
            ("x-rayline-executed-action-id", "a1"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        metadata.record(&observation, &headers, "instance");
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.active[0].target.as_deref(), Some("local"));
        assert_eq!(snapshot.active[0].executed_model.as_deref(), Some("glm"));
        assert_eq!(
            snapshot.active[0].decision_model.as_deref(),
            Some("cloud-model")
        );
        assert_eq!(snapshot.active[0].endpoint_id.as_deref(), Some("spark"));
        drop(observation);
        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.recent[0].state, "error");
        assert_eq!(snapshot.totals.local_requests, 1);
    }

    #[test]
    fn unknown_or_conflicting_receipts_stay_unattributed() {
        for (config, worker, model, action) in [
            (
                json!({"derived_actions": [{"worker":"w","model":"m","deployment_id":"cloud"}]}),
                "w",
                "unknown-model",
                "unknown-action",
            ),
            (
                json!({"bindings": [{"worker":"w","action_id":"a1","deployment_id":"cloud"}]}),
                "different-worker",
                "m",
                "a1",
            ),
            (
                json!({"bindings": [{"worker":"w","action_id":"a1","deployment_id":"cloud"}, {"worker":"w","action_id":"a1","deployment_id":"local"}]}),
                "w",
                "m",
                "a1",
            ),
            (
                json!({"derived_actions": [{"worker":"w","model":"m","deployment_id":"cloud"}, {"worker":"w","model":"m","deployment_id":"local"}]}),
                "w",
                "m",
                "dynamic-action",
            ),
        ] {
            let mut config = config;
            config["deployments"] = json!([
                {"id":"cloud","kind":"cloud","wire_model":"cloud-wire"},
                {"id":"local","kind":"local","wire_model":"local-wire"}
            ]);
            let metadata = RoutingMetadata::from_config(&config);
            let metrics = RouterMetrics::new("hybrid");
            let mut observation = RequestObservation::start(metrics.clone(), b"{}");
            let mut headers = HeaderMap::new();
            for (key, value) in [
                ("x-rayline-instance-id", "instance"),
                ("x-rayline-executed-worker", worker),
                ("x-rayline-executed-model", model),
                ("x-rayline-executed-action-id", action),
            ] {
                headers.insert(key, value.parse().unwrap());
            }
            metadata.record(&observation, &headers, "instance");
            observation.finish(200, None);
            let snapshot = metrics.snapshot();
            assert_eq!(
                snapshot.recent[0].target.as_deref(),
                Some("unknown"),
                "{config}"
            );
            assert!(snapshot.recent[0].endpoint_id.is_none());
            assert_eq!(snapshot.totals.remote_requests, 0);
            assert_eq!(snapshot.totals.local_requests, 0);
        }
    }

    #[test]
    fn ambiguous_worker_and_wrong_instance_never_guess_a_target() {
        let metadata = RoutingMetadata::from_config(&json!({
            "deployments": [{"id":"a","kind":"local","wire_model":"m"},{"id":"b","kind":"cloud","wire_model":"m"}],
            "derived_actions": [{"worker":"w","deployment_id":"a"},{"worker":"w","deployment_id":"b"}]
        }));
        assert!(metadata.deployment(Some("w"), None, Some("m")).is_none());
        let metrics = RouterMetrics::new("hybrid");
        let observation = RequestObservation::start(metrics.clone(), b"{}");
        let mut headers = HeaderMap::new();
        headers.insert("x-rayline-instance-id", "old-instance".parse().unwrap());
        metadata.record(&observation, &headers, "new-instance");
        assert!(metrics.snapshot().active[0].target.is_none());
    }
}
