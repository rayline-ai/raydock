use std::fmt;

/// Why a request got no decision. `reason` and `field` are the policy
/// service's own codes where it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecideError {
    /// A request member is malformed.
    Invalid { field: &'static str },
    /// The request is well formed and the policy declines to choose.
    Refused { reason: &'static str },
    /// The request asks for something this release or crate does not do.
    Unsupported { reason: &'static str },
    /// The request could not be turned into encoder input.
    Projection(String),
    /// The request holds more than the projection reads; it is refused
    /// before the expensive work.
    TooLarge(String),
    /// The request is well formed but holds what the projection can't
    /// encode yet (images and documents, say): no decision, not a bad
    /// request.
    NotEncodable(String),
    /// The readout source failed or answered with the wrong shape.
    Readouts(String),
    /// The release's tensors did not fit the computation.
    Internal(String),
}

impl DecideError {
    /// The error code for this refusal: a declined selection's own reason.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "invalid_request",
            Self::Refused { reason } => reason,
            Self::Unsupported { .. } | Self::Projection(_) => "unsupported_request",
            Self::TooLarge(_) => "input_too_large",
            Self::NotEncodable(_) => "unsupported_request",
            Self::Readouts(_) => "backend_unavailable",
            Self::Internal(_) => "internal_error",
        }
    }
}

impl fmt::Display for DecideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { field } => write!(f, "invalid request: {field}"),
            Self::Refused { reason } => write!(f, "selection refused: {reason}"),
            Self::Unsupported { reason } => write!(f, "unsupported request: {reason}"),
            Self::Projection(reason) => write!(f, "request not encodable: {reason}"),
            Self::TooLarge(reason) => write!(f, "request too large: {reason}"),
            Self::NotEncodable(reason) => write!(f, "request not encodable yet: {reason}"),
            Self::Readouts(reason) => write!(f, "readouts unavailable: {reason}"),
            Self::Internal(reason) => write!(f, "decision failed: {reason}"),
        }
    }
}

impl std::error::Error for DecideError {}

pub(crate) fn refuse(reason: &'static str) -> DecideError {
    DecideError::Refused { reason }
}

pub(crate) fn unsupported(reason: &'static str) -> DecideError {
    DecideError::Unsupported { reason }
}

pub(crate) fn invalid(field: &'static str) -> DecideError {
    DecideError::Invalid { field }
}
