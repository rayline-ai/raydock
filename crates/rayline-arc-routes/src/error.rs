//! Refusals: the public error codes, their HTTP statuses and the envelope.

use serde_json::{Value, json};

/// Every public code this crate can answer with: HTTP status and `error.type`.
const ERRORS: &[(&str, u16, &str)] = &[
    ("invalid_body", 400, "invalid_request_error"),
    ("unknown_model", 400, "invalid_request_error"),
    ("routing_invalid", 400, "invalid_request_error"),
    ("routing_unknown_member", 400, "invalid_request_error"),
    ("value_invalid", 400, "invalid_request_error"),
    ("value_out_of_bounds", 400, "invalid_request_error"),
    ("tier_value_immutable", 400, "invalid_request_error"),
    ("temperature_invalid", 400, "invalid_request_error"),
    ("models_invalid", 400, "invalid_request_error"),
    ("prices_invalid", 400, "invalid_request_error"),
    ("price_leg_invalid", 400, "invalid_request_error"),
    ("price_profile_invalid", 400, "invalid_request_error"),
    ("response_fields_invalid", 400, "invalid_request_error"),
    ("execution_invalid", 400, "invalid_request_error"),
    (
        "execution_counts_inconsistent",
        400,
        "invalid_request_error",
    ),
    (
        "execution_attribution_invalid",
        400,
        "invalid_request_error",
    ),
    ("execution_unknown_action", 400, "invalid_request_error"),
    ("execution_identity_mismatch", 409, "invalid_request_error"),
    ("execution_hold_unavailable", 409, "invalid_request_error"),
    (
        "execution_last_served_conflict",
        400,
        "invalid_request_error",
    ),
    ("execution_hold_infeasible", 409, "invalid_request_error"),
    ("policy_invalid", 400, "invalid_request_error"),
    ("policy_schedule_not_admitted", 400, "invalid_request_error"),
    ("inconsistent_upstream", 502, "api_error"),
    ("unavailable", 503, "api_error"),
    ("timeout", 504, "timeout_error"),
];

/// A request this router does not answer: an HTTP status, a machine-readable
/// code and a message. Messages name members, ids and bounds, never content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl Refusal {
    /// A declared public code at its own status.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        let status = ERRORS
            .iter()
            .find(|(name, _, _)| *name == code)
            .map_or(400, |(_, status, _)| *status);
        Self {
            status,
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// A code decided elsewhere (the decide service), at the status it came with.
    pub fn with_status(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// The decide answer is not one this router can restate.
    pub(crate) fn upstream(message: &str) -> Self {
        Self::new("inconsistent_upstream", message)
    }

    pub fn error_type(&self) -> &'static str {
        ERRORS
            .iter()
            .find(|(name, status, _)| *name == self.code && *status == self.status)
            .map_or(
                if self.status < 500 {
                    "invalid_request_error"
                } else {
                    "api_error"
                },
                |(_, _, kind)| kind,
            )
    }

    /// The error envelope: `{type: "error", error: {type, code, message}}`.
    pub fn body(&self) -> Value {
        json!({"type": "error", "error": {
            "type": self.error_type(), "code": self.code, "message": self.message}})
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.status, self.message)
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_codes_carry_their_status_and_type() {
        let refusal = Refusal::new("execution_hold_infeasible", "m");
        assert_eq!(
            (refusal.status, refusal.error_type()),
            (409, "invalid_request_error")
        );
        assert_eq!(Refusal::new("unavailable", "m").status, 503);
        assert_eq!(Refusal::new("timeout", "m").error_type(), "timeout_error");
    }

    #[test]
    fn a_foreign_code_takes_its_type_from_the_status() {
        assert_eq!(
            Refusal::with_status(409, "session_busy", "m").error_type(),
            "invalid_request_error"
        );
        assert_eq!(
            Refusal::with_status(500, "boom", "m").error_type(),
            "api_error"
        );
        let body = Refusal::new("invalid_body", "x").body();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["code"], "invalid_body");
    }
}
