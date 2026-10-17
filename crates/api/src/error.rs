use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable machine-readable error code. Clients branch on this, never on the
/// message. Codes are never renamed or reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The caller's capability does not cover this folder or verb.
    CapabilityDenied,
    NotFound,
    /// Data failed verification against its BLAKE3 name or the signed root.
    Integrity,
    /// Parked by power or thermal policy; retry later.
    Deferred,
    InvalidRequest,
    /// The plan no longer matches the folder; plan again.
    StalePlan,
    Internal,
    /// A code from a newer server.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityDenied => "capability_denied",
            Self::NotFound => "not_found",
            Self::Integrity => "integrity",
            Self::Deferred => "deferred",
            Self::InvalidRequest => "invalid_request",
            Self::StalePlan => "stale_plan",
            Self::Internal => "internal",
            Self::Unknown => "unknown",
        }
    }
}

/// A typed failure with a stable code and a human message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    /// For `deferred`: seconds before a retry is worthwhile, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u32>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn capability_denied(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::CapabilityDenied, message)
    }

    pub fn deferred(message: impl Into<String>, retry_after_secs: Option<u32>) -> Self {
        Self {
            retry_after_secs,
            ..Self::new(ErrorCode::Deferred, message)
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_strings() {
        for c in [
            ErrorCode::CapabilityDenied,
            ErrorCode::NotFound,
            ErrorCode::Integrity,
            ErrorCode::Deferred,
            ErrorCode::InvalidRequest,
            ErrorCode::StalePlan,
            ErrorCode::Internal,
        ] {
            let j = serde_json::to_string(&c).unwrap();
            assert_eq!(j, format!("\"{}\"", c.as_str()));
            assert_eq!(serde_json::from_str::<ErrorCode>(&j).unwrap(), c);
        }
    }

    #[test]
    fn unknown_code_from_newer_server() {
        let e: ApiError =
            serde_json::from_str(r#"{"code":"quota_exceeded","message":"m","extra":1}"#).unwrap();
        assert_eq!(e.code, ErrorCode::Unknown);
    }

    #[test]
    fn error_round_trip() {
        let e = ApiError::deferred("on battery", Some(600));
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<ApiError>(&j).unwrap(), e);
        assert_eq!(e.to_string(), "deferred: on battery");
    }
}
