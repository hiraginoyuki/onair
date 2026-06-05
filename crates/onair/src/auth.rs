use axum::http::HeaderMap;
use onair_core::error::ApiError;
use subtle::ConstantTimeEq;

use crate::config::ResolvedClient;

#[derive(Debug, Clone)]
pub struct Identity {
    pub id: String,
    pub models: std::collections::BTreeSet<String>,
}

pub fn authenticate(headers: &HeaderMap, clients: &[ResolvedClient]) -> Result<Identity, ApiError> {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(ApiError::authentication("Missing bearer token."));
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::authentication("Invalid bearer token."))?;
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(ApiError::authentication("Invalid bearer token."));
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
        .ok_or_else(|| ApiError::authentication("Invalid bearer token."))
}

fn token_matches(input: &str, configured: &str) -> bool {
    input.len() == configured.len() && input.as_bytes().ct_eq(configured.as_bytes()).into()
}
