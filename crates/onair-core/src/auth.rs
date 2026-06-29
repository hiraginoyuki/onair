use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

use crate::config::ResolvedClient;
use crate::error::ApiError;

#[derive(Debug, Clone)]
pub struct Identity {
    pub id: String,
    pub models: std::collections::BTreeSet<String>,
}

pub fn authenticate(headers: &HeaderMap, clients: &[ResolvedClient]) -> Result<Identity, ApiError> {
    // Extract bearer token from Authorization header (highest precedence).
    let bearer_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    // Extract x-api-key header (fallback when bearer is absent).
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());

    let Some(token) = bearer_token.or(x_api_key) else {
        return Err(ApiError::authentication("Missing bearer token."));
    };

    // Determine error message based on which path was taken.
    let invalid_msg = if bearer_token.is_some() {
        "Invalid bearer token."
    } else {
        "Invalid x-api-key."
    };

    let mut matched = None;
    for client in clients {
        if token_matches(token, &client.api_key) && matched.is_none() {
            matched = Some(client);
        }
    }

    matched
        .map(|client| Identity {
            id: client.id.clone(),
            models: client.models.clone(),
        })
        .ok_or_else(|| ApiError::authentication(invalid_msg))
}

fn token_matches(input: &str, configured: &str) -> bool {
    input.len() == configured.len() && input.as_bytes().ct_eq(configured.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn clients(key: &str) -> Vec<ResolvedClient> {
        vec![ResolvedClient {
            id: "test-client".to_owned(),
            api_key: key.to_owned(),
            models: Default::default(),
        }]
    }

    #[test]
    fn bearer_auth_still_works() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test-key"),
        );
        let identity = authenticate(&headers, &clients("test-key")).unwrap();
        assert_eq!(identity.id, "test-client");
    }

    #[test]
    fn x_api_key_auth_works() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("test-key"));
        let identity = authenticate(&headers, &clients("test-key")).unwrap();
        assert_eq!(identity.id, "test-client");
    }

    #[test]
    fn both_present_bearer_wins() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer bearer-key"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("xapikey-key"));
        // Bearer matches "bearer-key"
        let identity = authenticate(&headers, &clients("bearer-key")).unwrap();
        assert_eq!(identity.id, "test-client");
    }

    #[test]
    fn both_present_bearer_wins_even_if_xapikey_matches() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer shared-key"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("shared-key"));
        // Both match "shared-key", but bearer path is taken
        let identity = authenticate(&headers, &clients("shared-key")).unwrap();
        assert_eq!(identity.id, "test-client");
    }

    #[test]
    fn missing_both_returns_missing_error() {
        let headers = HeaderMap::new();
        let err = authenticate(&headers, &clients("anything")).unwrap_err();
        assert!(err.message.contains("Missing bearer token."));
    }

    #[test]
    fn invalid_bearer_returns_bearer_error() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong-key"),
        );
        let err = authenticate(&headers, &clients("test-key")).unwrap_err();
        assert_eq!(err.message, "Invalid bearer token.");
    }

    #[test]
    fn invalid_x_api_key_returns_xapikey_error() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("wrong-key"));
        let err = authenticate(&headers, &clients("test-key")).unwrap_err();
        assert_eq!(err.message, "Invalid x-api-key.");
    }
}
