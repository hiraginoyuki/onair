pub mod anthropic;
pub mod anthropic_compat;
pub mod models;
pub mod paths;
pub mod request;
pub mod response;
pub mod responses_compat;

pub use models::{
    ModelMeta, ModelObject, ModelsResponse, model_response, model_response_with_n_ctx_train,
    models_response, props_response,
};

pub use request::{
    RequestMode, RequestRewriteError, RequestRewritePolicies, RequestShape, RewriteParam,
    inspect_request, rewrite_anthropic_messages_request_body, rewrite_query_model,
    rewrite_request_body_for_mode_with_policies, upstream_path_for_mode,
};

pub(crate) use response::is_json_content_type;
pub use response::{
    AnthropicSseNormalizer, ChatCompletionsSseNormalizer, ResponsesSseNormalizer, SseNormalizer,
    SseStrategy, UsageDiagnostics, UsageTotals, is_event_stream_content_type,
    rewrite_anthropic_messages_response_body, rewrite_response_body, rewrite_response_models,
};

#[cfg(test)]
pub(crate) use response::extract_usage_observation;

#[cfg(test)]
mod tests;
