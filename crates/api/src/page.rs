use crate::error::ApiError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Page size when the caller names none. Sized for a model's context.
pub const DEFAULT_LIMIT: u32 = 50;
/// Hard cap; larger requests are clamped, never honoured.
pub const MAX_LIMIT: u32 = 200;

/// Turns a requested limit into the one to serve: default if absent, clamped
/// to [`MAX_LIMIT`]; zero is a malformed request.
pub fn resolve_limit(limit: Option<u32>) -> Result<usize, ApiError> {
    match limit {
        None => Ok(DEFAULT_LIMIT as usize),
        Some(0) => Err(ApiError::invalid_request("limit must be at least 1")),
        Some(n) => Ok(n.min(MAX_LIMIT) as usize),
    }
}

/// One page of a list. There is no unpaginated list in the API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Pass as the cursor of the next request; absent when this was the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
    /// Size of the whole list, when cheap to know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits() {
        assert_eq!(resolve_limit(None).unwrap(), DEFAULT_LIMIT as usize);
        assert_eq!(resolve_limit(Some(7)).unwrap(), 7);
        assert_eq!(resolve_limit(Some(u32::MAX)).unwrap(), MAX_LIMIT as usize);
        assert_eq!(
            resolve_limit(Some(0)).unwrap_err().code,
            crate::ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn page_round_trip() {
        let p = Page {
            items: vec![1u32, 2],
            next_cursor: Some(9),
            total: None,
        };
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(j, r#"{"items":[1,2],"next_cursor":9}"#);
        assert_eq!(serde_json::from_str::<Page<u32>>(&j).unwrap(), p);
    }
}
