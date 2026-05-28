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
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(ApiError::authentication("Missing bearer token."));
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::authentication("Invalid bearer token."))?;
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(ApiError::authentication("Invalid bearer token."));
    };

    clients
        .iter()
        .find(|client| token_matches(token, &client.api_key))
        .map(|client| Identity {
            id: client.id.clone(),
            models: client.models.clone(),
        })
        .ok_or_else(|| ApiError::authentication("Invalid bearer token."))
}

fn token_matches(input: &str, configured: &str) -> bool {
    input.len() == configured.len() && input.as_bytes().ct_eq(configured.as_bytes()).into()
}
