//! Which Claude Code subagents selective-subagents mode routes, and the model a
//! routed turn names on the hosted router. Read from the router config's
//! `routes.subagent` and `routes.subagents`.

use serde_json::Value;

/// The hosted router's subagent tier, named when the config names no model.
pub const ARC_SWIFT_MODEL: &str = "arc-swift-1.0";

/// One `routes.subagents` entry: an agent id or type and its route's model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubagentRoute {
    pub agent: String,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectiveSubagentRoutes {
    /// `routes.subagents`, sorted by agent. Empty routes every subagent.
    pub by_agent: Vec<SubagentRoute>,
    /// `routes.subagent.model`.
    pub default_model: Option<String>,
}

impl SelectiveSubagentRoutes {
    pub fn from_router_config(config: &Value) -> Self {
        let routes = config.get("routes");
        let mut by_agent = routes
            .and_then(|routes| routes.get("subagents"))
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(agent, route)| {
                let agent = agent.trim();
                (!agent.is_empty()).then(|| SubagentRoute {
                    agent: agent.to_owned(),
                    model: route_model(route),
                })
            })
            .collect::<Vec<_>>();
        by_agent.sort_by(|a, b| a.agent.cmp(&b.agent));
        Self {
            by_agent,
            default_model: routes
                .and_then(|routes| routes.get("subagent"))
                .and_then(route_model),
        }
    }

    /// Agents for the route-decision log line.
    pub fn agents(&self) -> impl Iterator<Item = &str> {
        self.by_agent.iter().map(|route| route.agent.as_str())
    }

    pub(crate) fn allows(&self, agent_id: &str, agent_type: Option<&str>) -> bool {
        self.by_agent.is_empty() || self.find(agent_id, agent_type).is_some()
    }

    /// The model a routed subagent turn names: its own route's model, else
    /// `routes.subagent`'s, else ARC Swift.
    pub(crate) fn model_for(&self, agent_id: &str, agent_type: Option<&str>) -> &str {
        self.find(agent_id, agent_type)
            .and_then(|route| route.model.as_deref())
            .or(self.default_model.as_deref())
            .unwrap_or(ARC_SWIFT_MODEL)
    }

    fn find(&self, agent_id: &str, agent_type: Option<&str>) -> Option<&SubagentRoute> {
        self.by_agent.iter().find(|route| {
            route.agent == agent_id
                || agent_type.is_some_and(|agent_type| route.agent.eq_ignore_ascii_case(agent_type))
        })
    }
}

fn route_model(route: &Value) -> Option<String> {
    route
        .get("model")?
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn routes(config: Value) -> SelectiveSubagentRoutes {
        SelectiveSubagentRoutes::from_router_config(&config)
    }

    #[test]
    fn reads_subagent_keys_and_models_from_router_config() {
        let parsed = routes(json!({"routes": {
            "subagent": {"endpoint": "rayline-cloud", "model": " arc-sage-1.0 "},
            "subagents": {
                "Review": {"endpoint": "rayline-cloud", "model": "z-ai/glm-5.2"},
                "Explore": {"endpoint": "rayline-cloud"},
                " ": {"endpoint": "rayline-cloud", "model": "m"}
            }
        }}));

        assert_eq!(parsed.agents().collect::<Vec<_>>(), ["Explore", "Review"]);
        assert_eq!(parsed.default_model.as_deref(), Some("arc-sage-1.0"));
        assert_eq!(parsed.by_agent[0].model, None);
        assert_eq!(parsed.by_agent[1].model.as_deref(), Some("z-ai/glm-5.2"));
    }

    #[test]
    fn empty_routes_allow_every_subagent() {
        let parsed = routes(json!({"routes": {"subagent": {"model": "arc-swift-1.0"}}}));

        assert!(parsed.allows("a4a4dba6877819a3a", None));
        assert!(parsed.allows("a4a4dba6877819a3a", Some("general-purpose")));
    }

    #[test]
    fn allowlist_matches_agent_id_or_resolved_type() {
        let parsed = routes(json!({"routes": {"subagents": {"Explore": {}}}}));

        assert!(parsed.allows("Explore", None));
        assert!(parsed.allows("a332089fa2c10afe6", Some("Explore")));
        assert!(parsed.allows("a332089fa2c10afe6", Some("explore")));
        assert!(!parsed.allows("a332089fa2c10afe6", Some("Review")));
        assert!(!parsed.allows("a332089fa2c10afe6", None));
    }

    #[test]
    fn routed_model_prefers_the_agent_route_then_the_subagent_route_then_arc_swift() {
        let parsed = routes(json!({"routes": {
            "subagent": {"model": "arc-sage-1.0"},
            "subagents": {
                "Review": {"model": "z-ai/glm-5.2"},
                "Explore": {}
            }
        }}));

        assert_eq!(parsed.model_for("id", Some("review")), "z-ai/glm-5.2");
        assert_eq!(parsed.model_for("id", Some("Explore")), "arc-sage-1.0");
        assert_eq!(parsed.model_for("id", Some("Plan")), "arc-sage-1.0");
        assert_eq!(
            SelectiveSubagentRoutes::default().model_for("id", Some("Plan")),
            ARC_SWIFT_MODEL
        );
    }
}
