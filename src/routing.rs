use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

use crate::config::{ResolvedBackend, RoutingStrategy};
use crate::error::ApiError;

#[derive(Clone)]
pub struct SelectedRoute {
    pub backend_id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: std::time::Duration,
    pub public_model: Option<String>,
    pub backend_model: Option<String>,
}

pub fn select_backend(
    backends: &[ResolvedBackend],
    strategy: RoutingStrategy,
    path: &str,
    model: Option<&str>,
    stream: bool,
    sticky_key: Option<&str>,
) -> Result<SelectedRoute, ApiError> {
    let path_candidates = path_capability_candidates(path);
    let mut candidates = Vec::new();

    for backend in backends {
        if stream && !has_capability(&backend.capabilities, "streaming") {
            continue;
        }
        if !supports_candidates(&backend.capabilities, &path_candidates) {
            continue;
        }

        if let Some(requested_model) = model {
            for route in &backend.models {
                if route.public != requested_model {
                    continue;
                }
                if !route.endpoints.is_empty()
                    && !supports_candidates(&route.endpoints, &path_candidates)
                {
                    continue;
                }
                candidates.push(SelectedRoute {
                    backend_id: backend.id.clone(),
                    base_url: backend.base_url.clone(),
                    api_key: backend.api_key.clone(),
                    timeout: backend.timeout,
                    public_model: Some(route.public.clone()),
                    backend_model: Some(route.backend.clone()),
                });
            }
            continue;
        }

        candidates.push(SelectedRoute {
            backend_id: backend.id.clone(),
            base_url: backend.base_url.clone(),
            api_key: backend.api_key.clone(),
            timeout: backend.timeout,
            public_model: None,
            backend_model: None,
        });
    }

    if candidates.is_empty() {
        if let Some(requested_model) = model {
            return Err(ApiError::model_not_found(requested_model));
        }
        return Err(ApiError::not_found(format!(
            "The requested endpoint '{path}' is unavailable."
        )));
    }

    let selected = match strategy {
        RoutingStrategy::Priority => candidates.remove(0),
        RoutingStrategy::Sticky => {
            let index = sticky_index(sticky_key.unwrap_or(path), candidates.len());
            candidates.remove(index)
        }
    };

    if let Some(requested_model) = model
        && selected.public_model.is_none()
        && selected.backend_model.is_none()
    {
        return Err(ApiError::model_not_found(requested_model));
    }

    Ok(selected)
}

fn sticky_index(key: &str, count: usize) -> usize {
    if count <= 1 {
        return 0;
    }

    let mut hasher = FnvHasher::default();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % count
}

#[derive(Default)]
struct FnvHasher(u64);

impl Hasher for FnvHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        };
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        self.0 = hash;
    }

    fn finish(&self) -> u64 {
        if self.0 == 0 {
            0xcbf29ce484222325
        } else {
            self.0
        }
    }
}

pub fn sticky_routing_key(
    identity: &str,
    path: &str,
    model: Option<&str>,
    prompt_cache_key: Option<&str>,
) -> String {
    format!(
        "{identity}\u{0}{path}\u{0}{}\u{0}{}",
        model.unwrap_or(""),
        prompt_cache_key.unwrap_or("")
    )
}

pub fn path_metric_name(path: &str) -> String {
    if path.ends_with("/chat/completions") {
        return "chat_completions".to_owned();
    }
    if path.ends_with("/chat/completion") {
        return "chat_completions".to_owned();
    }
    if path.ends_with("/responses") {
        return "responses".to_owned();
    }
    let candidates = path_capability_candidates(path);
    candidates
        .first()
        .cloned()
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn path_requires_model(path: &str) -> bool {
    let normalized = path.trim_end_matches('/');
    matches!(
        normalized,
        "/v1/chat/completions"
            | "/v1/chat/completion"
            | "/v1/responses"
            | "/v1/embeddings"
            | "/v1/audio/transcriptions"
            | "/v1/audio/translations"
            | "/v1/images/generations"
            | "/v1/images/edits"
            | "/v1/images/variations"
    )
}

pub fn path_capability_candidates(path: &str) -> Vec<String> {
    let trimmed = path
        .strip_prefix("/v1/")
        .unwrap_or(path)
        .trim_start_matches('/');
    if trimmed.is_empty() {
        return Vec::new();
    }

    let segments = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(normalize_segment)
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    push_unique(&mut candidates, segments[0].clone());
    if segments[0].ends_with('s') && segments[0].len() > 1 {
        push_unique(&mut candidates, singularize(&segments[0]));
    }
    if let Some(second_segment) = segments.get(1) {
        push_unique(&mut candidates, second_segment.clone());
        push_unique(
            &mut candidates,
            format!("{}_{}", segments[0], second_segment),
        );
    }
    if let Some(third_segment) = segments.get(2) {
        push_unique(
            &mut candidates,
            format!("{}_{}", segments[0], third_segment),
        );
        push_unique(&mut candidates, third_segment.clone());
    }

    match segments[0].as_str() {
        "chat" => {
            push_unique(&mut candidates, "chat_completions".to_owned());
            push_unique(&mut candidates, "completions".to_owned());
        }
        "images" => {
            push_unique(&mut candidates, "image".to_owned());
        }
        "files" => {
            push_unique(&mut candidates, "file".to_owned());
        }
        "models" => {
            push_unique(&mut candidates, "model".to_owned());
        }
        "audio" => {
            push_unique(&mut candidates, "audio".to_owned());
        }
        _ => {}
    }

    candidates
}

fn supports_candidates(capabilities: &BTreeSet<String>, candidates: &[String]) -> bool {
    candidates
        .iter()
        .any(|candidate| has_capability(capabilities, candidate))
}

fn has_capability(capabilities: &BTreeSet<String>, capability: &str) -> bool {
    capabilities.contains(capability) || capabilities.contains("all")
}

fn normalize_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|character| match character {
            '-' => '_',
            character => character.to_ascii_lowercase(),
        })
        .collect()
}

fn singularize(segment: &str) -> String {
    segment.strip_suffix('s').unwrap_or(segment).to_owned()
}

fn push_unique(candidates: &mut Vec<String>, candidate: String) {
    if !candidate.is_empty() && !candidates.iter().any(|value| value == &candidate) {
        candidates.push(candidate);
    }
}
