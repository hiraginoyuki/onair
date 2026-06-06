pub const RESPONSES_PATH: &str = "/v1/responses";
pub const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

pub fn normalize_path(p: &str) -> &str {
    p.trim_end_matches('/')
}

pub fn endpoint_kind(path: &str) -> EndpointKind {
    let normalized = normalize_path(path);
    if normalized == RESPONSES_PATH {
        EndpointKind::Responses
    } else if normalized == CHAT_COMPLETIONS_PATH {
        EndpointKind::ChatCompletions
    } else {
        EndpointKind::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Responses,
    ChatCompletions,
    Other,
}

impl EndpointKind {
    pub fn is_native_responses(self) -> bool {
        matches!(self, EndpointKind::Responses)
    }

    pub fn is_chat_completions(self) -> bool {
        matches!(self, EndpointKind::ChatCompletions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_trims_trailing_slashes() {
        assert_eq!(
            normalize_path("/v1/chat/completions"),
            CHAT_COMPLETIONS_PATH
        );
        assert_eq!(
            normalize_path("/v1/chat/completions/"),
            CHAT_COMPLETIONS_PATH
        );
        assert_eq!(
            normalize_path("/v1/chat/completions//"),
            CHAT_COMPLETIONS_PATH
        );
        assert_eq!(normalize_path("/v1/responses"), RESPONSES_PATH);
    }

    #[test]
    fn endpoint_kind_recognizes_known_paths() {
        assert_eq!(endpoint_kind("/v1/responses"), EndpointKind::Responses);
        assert_eq!(endpoint_kind("/v1/responses/"), EndpointKind::Responses);
        assert_eq!(
            endpoint_kind("/v1/chat/completions"),
            EndpointKind::ChatCompletions
        );
        assert_eq!(
            endpoint_kind("/v1/chat/completions/"),
            EndpointKind::ChatCompletions
        );
        assert_eq!(endpoint_kind("/v1/embeddings"), EndpointKind::Other);
        assert_eq!(endpoint_kind("/v1/audio/speech"), EndpointKind::Other);
    }
}
