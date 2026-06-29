use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use tracing::warn;
use url::form_urlencoded;

use crate::config::{
    ChatStreamUsagePolicy, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, ToolSchemaMode,
};

use super::{
    is_json_content_type,
    paths::endpoint_kind,
    responses_compat::{rewrite_chat_request_as_responses, rewrite_responses_request_as_chat},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMode {
    Native,
    ResponsesViaChatCompletions,
    ChatCompletionsViaResponses,
    AnthropicMessagesNative,
}

#[derive(Debug, Clone, Copy)]
pub struct RequestRewritePolicies {
    pub tool_schema_mode: ToolSchemaMode,
    pub responses_store: ResponsesStorePolicy,
    pub responses_max_output_tokens: ResponsesMaxOutputTokensPolicy,
    pub chat_stream_usage: ChatStreamUsagePolicy,
    pub anthropic_max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteParam {
    Input,
    Model,
    Messages,
    Tools,
    ToolChoice,
    PreviousResponseId,
    N,
    Logprobs,
    TopLogprobs,
    Stream,
    FunctionCallId,
}

impl RewriteParam {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Model => "model",
            Self::Messages => "messages",
            Self::Tools => "tools",
            Self::ToolChoice => "tool_choice",
            Self::PreviousResponseId => "previous_response_id",
            Self::N => "n",
            Self::Logprobs => "logprobs",
            Self::TopLogprobs => "top_logprobs",
            Self::Stream => "stream",
            Self::FunctionCallId => "function_call_id",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRewriteError {
    message: String,
    param: Option<String>,
}

impl RequestRewriteError {
    pub(super) fn new(message: impl Into<String>, param: Option<&str>) -> Self {
        Self {
            message: message.into(),
            param: param.map(str::to_owned),
        }
    }

    pub fn with_param(message: impl Into<String>, param: RewriteParam) -> Self {
        Self::new(message, Some(param.as_str()))
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn param(&self) -> Option<String> {
        self.param.clone()
    }
}

#[derive(Debug, Clone, Default)]
pub struct RequestShape {
    pub model: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub stream: bool,
    pub stream_usage_requested: bool,
    pub has_tools: bool,
}

pub fn inspect_request(
    body: &[u8],
    content_type: Option<&str>,
    query: Option<&str>,
) -> RequestShape {
    let mut shape = inspect_body(body, content_type);

    if shape.model.is_none() {
        shape.model = query_field(query, "model");
    }
    if shape.prompt_cache_key.is_none() {
        shape.prompt_cache_key = query_field(query, "prompt_cache_key");
    }
    if !shape.stream {
        shape.stream = query_bool(query, "stream");
    }

    shape
}

#[allow(clippy::too_many_arguments)]
pub fn rewrite_request_body_for_mode_with_policies(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    path: &str,
    request_mode: RequestMode,
    policies: &RequestRewritePolicies,
    extra_body: &BTreeMap<String, toml::Value>,
    route_label: &str,
) -> Result<Vec<u8>, RequestRewriteError> {
    let kind = endpoint_kind(path);
    let native_responses = kind.is_native_responses();
    let chat_completions = kind.is_chat_completions();
    if native_responses && should_parse_json(content_type, body) {
        validate_responses_tool_history(body)?;
    }

    if request_mode == RequestMode::AnthropicMessagesNative {
        return rewrite_anthropic_messages_request_body(
            body,
            content_type,
            backend_model,
            policies.anthropic_max_tokens,
        );
    }
    if request_mode == RequestMode::ResponsesViaChatCompletions {
        return rewrite_responses_request_as_chat(
            body,
            content_type,
            backend_model,
            policies.tool_schema_mode,
            policies.chat_stream_usage,
            extra_body,
            route_label,
        );
    }
    if request_mode == RequestMode::ChatCompletionsViaResponses {
        return rewrite_chat_request_as_responses(
            body,
            content_type,
            backend_model,
            policies.responses_store,
            policies.responses_max_output_tokens,
            extra_body,
            route_label,
        );
    }

    let Some(backend_model) = backend_model else {
        return Ok(body.to_vec());
    };
    if body.is_empty() {
        return Ok(Vec::new());
    }

    if should_parse_json(content_type, body)
        && let Some(rewritten) = rewrite_json_request_body(
            body,
            backend_model,
            native_responses,
            chat_completions,
            policies.responses_store,
            policies.responses_max_output_tokens,
            policies.chat_stream_usage,
            extra_body,
            route_label,
        )
    {
        return Ok(rewritten);
    }

    if (is_urlencoded_content_type(content_type) || looks_like_urlencoded(body))
        && let Some(rewritten) = rewrite_urlencoded_request_body(body, backend_model)
    {
        return Ok(rewritten);
    }

    if let Some(boundary) = content_type.and_then(multipart_boundary)
        && let Some(rewritten) = rewrite_multipart_body(body, &boundary, backend_model)
    {
        return Ok(rewritten);
    }

    Ok(body.to_vec())
}

fn validate_responses_tool_history(body: &[u8]) -> Result<(), RequestRewriteError> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Ok(());
    };
    let Some(input) = value.get("input").and_then(Value::as_array) else {
        return Ok(());
    };

    let mut function_calls = Vec::new();
    let mut function_outputs = BTreeSet::new();

    for item in input {
        let Some(item) = item.as_object() else {
            continue;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let Some(call_id) = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|call_id| !call_id.is_empty())
                else {
                    return Err(RequestRewriteError::new(
                        "function_call input items require call_id.",
                        Some("input"),
                    ));
                };
                function_calls.push(call_id.to_owned());
            }
            Some("function_call_output") => {
                if let Some(call_id) = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|call_id| !call_id.is_empty())
                {
                    function_outputs.insert(call_id.to_owned());
                }
            }
            _ => {}
        }
    }

    for call_id in function_calls {
        if !function_outputs.contains(&call_id) {
            return Err(RequestRewriteError::new(
                format!("No tool output found for function call {call_id}."),
                Some("input"),
            ));
        }
    }

    Ok(())
}

/// Rewrite an Anthropic Messages API request body.
///
/// - Rejects empty bodies.
/// - Requires JSON content type / valid JSON.
/// - Rewrites the `model` field to `backend_model`.
/// - Validates/fills `max_tokens`: client-provided values are kept;
///   otherwise `anthropic_max_tokens` is inserted; if neither is
///   available, a `RequestRewriteError` is returned.
pub fn rewrite_anthropic_messages_request_body(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    anthropic_max_tokens: Option<u32>,
) -> Result<Vec<u8>, RequestRewriteError> {
    if body.is_empty() {
        return Err(RequestRewriteError::new(
            "Missing required parameter: messages.",
            Some("messages"),
        ));
    }

    if !should_parse_json(content_type, body) {
        return Err(RequestRewriteError::new(
            "Anthropic Messages API requires a JSON request body.",
            None,
        ));
    }

    let mut value: Value = serde_json::from_slice(body)
        .map_err(|_| RequestRewriteError::new("Request body is not valid JSON.", None))?;

    let object = value
        .as_object_mut()
        .ok_or_else(|| RequestRewriteError::new("Request body is not a JSON object.", None))?;

    // Rewrite model
    if let Some(model) = backend_model {
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    }

    // Validate/fill max_tokens
    if !object.contains_key("max_tokens") {
        match anthropic_max_tokens {
            Some(tokens) => {
                object.insert("max_tokens".to_owned(), Value::Number(tokens.into()));
            }
            None => {
                return Err(RequestRewriteError::new(
                    "Missing required parameter: max_tokens.",
                    Some("max_tokens"),
                ));
            }
        }
    }

    serde_json::to_vec(&value)
        .map_err(|_| RequestRewriteError::new("Failed to serialize request body.", None))
}

pub fn rewrite_query_model(query: Option<&str>, backend_model: Option<&str>) -> Option<String> {
    let query = query?;
    let Some(backend_model) = backend_model else {
        return Some(query.to_owned());
    };

    let mut saw_model = false;
    let rewritten = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            form_urlencoded::parse(query.as_bytes()).map(|(key, value)| {
                if key == "model" {
                    saw_model = true;
                    (key.into_owned(), backend_model.to_owned())
                } else {
                    (key.into_owned(), value.into_owned())
                }
            }),
        )
        .finish();

    if saw_model {
        Some(rewritten)
    } else {
        Some(query.to_owned())
    }
}

pub fn upstream_path_for_mode(path: &str, request_mode: RequestMode) -> &str {
    let kind = endpoint_kind(path);
    match request_mode {
        RequestMode::ResponsesViaChatCompletions if kind.is_native_responses() => {
            super::paths::CHAT_COMPLETIONS_PATH
        }
        RequestMode::ChatCompletionsViaResponses if kind.is_chat_completions() => {
            super::paths::RESPONSES_PATH
        }
        _ => path,
    }
}

/// Keys that onair always owns in the upstream request body. A
/// route's `extra_body` may set any other key, but these are
/// dropped with a `tracing::warn!` so onair's model rewrite and
/// per-mode policy logic cannot be silently overridden.
const PROTECTED_UPSTREAM_KEYS: &[&str] = &[
    "model",
    "stream",
    "messages",
    "input",
    "tools",
    "tool_choice",
    "store",
    "max_output_tokens",
    "max_tokens",
    "max_completion_tokens",
    "stream_options",
];

/// Shallow-merge a route's `extra_body` into an upstream request
/// body. Fields named in `PROTECTED_UPSTREAM_KEYS` are dropped with
/// a warning so onair's own rewrite always wins. Caller is
/// responsible for converting the `toml::Value` map to a
/// `serde_json::Value` map — this layer operates on JSON because
/// that is what is actually written to the upstream socket.
pub(crate) fn apply_extra_body(
    object: &mut Map<String, Value>,
    extra_body: &BTreeMap<String, toml::Value>,
    route_label: &str,
) {
    for (key, toml_value) in extra_body {
        if PROTECTED_UPSTREAM_KEYS.contains(&key.as_str()) {
            warn!(
                route = %route_label,
                key = %key,
                "dropping extra_body key that onair manages; use a per-route policy field instead"
            );
            continue;
        }
        let json_value = toml_value_to_json_value(toml_value);
        object.insert(key.clone(), json_value);
    }
}

fn toml_value_to_json_value(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(string) => Value::String(string.clone()),
        toml::Value::Integer(integer) => Value::Number((*integer).into()),
        toml::Value::Float(float) => serde_json::Number::from_f64(*float)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(boolean) => Value::Bool(*boolean),
        toml::Value::Datetime(datetime) => Value::String(datetime.to_string()),
        toml::Value::Array(array) => {
            Value::Array(array.iter().map(toml_value_to_json_value).collect())
        }
        toml::Value::Table(table) => {
            let mut object = Map::with_capacity(table.len());
            for (key, inner) in table {
                object.insert(key.clone(), toml_value_to_json_value(inner));
            }
            Value::Object(object)
        }
    }
}

fn inspect_body(body: &[u8], content_type: Option<&str>) -> RequestShape {
    if body.is_empty() {
        return RequestShape::default();
    }

    if should_parse_json(content_type, body) {
        return inspect_json_body(body);
    }

    if is_urlencoded_content_type(content_type) || looks_like_urlencoded(body) {
        let shape = inspect_urlencoded_body(body);
        if shape.model.is_some() || shape.stream {
            return shape;
        }
    }

    if let Some(boundary) = content_type.and_then(multipart_boundary) {
        return inspect_multipart_body(body, &boundary);
    }

    inspect_json_body(body)
}

fn inspect_json_body(body: &[u8]) -> RequestShape {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return RequestShape::default();
    };
    RequestShape {
        model: value
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .map(str::to_owned),
        prompt_cache_key: value
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .filter(|key| !key.trim().is_empty())
            .map(str::to_owned),
        stream: value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        stream_usage_requested: value
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        has_tools: json_has_tools(&value),
    }
}

fn inspect_urlencoded_body(body: &[u8]) -> RequestShape {
    let mut shape = RequestShape::default();
    for (key, value) in form_urlencoded::parse(body) {
        match key.as_ref() {
            "model" if !value.trim().is_empty() => shape.model = Some(value.into_owned()),
            "prompt_cache_key" if !value.trim().is_empty() => {
                shape.prompt_cache_key = Some(value.into_owned())
            }
            "stream" => shape.stream = truthy(&value),
            _ => {}
        }
    }
    shape
}

fn json_has_tools(value: &Value) -> bool {
    match value.get("tools") {
        Some(Value::Array(tools)) => !tools.is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

fn inspect_multipart_body(body: &[u8], boundary: &str) -> RequestShape {
    let mut shape = RequestShape::default();
    for part in multipart_parts(body, boundary) {
        let Some((headers, content)) = split_multipart_part(part) else {
            continue;
        };
        if multipart_field_is(headers, "model")
            && let Ok(model) = std::str::from_utf8(content)
        {
            let model = model.trim();
            if !model.is_empty() {
                shape.model = Some(model.to_owned());
            }
        }
        if multipart_field_is(headers, "stream")
            && let Ok(stream) = std::str::from_utf8(content)
        {
            shape.stream = truthy(stream);
        }
        if multipart_field_is(headers, "prompt_cache_key")
            && let Ok(key) = std::str::from_utf8(content)
        {
            let key = key.trim();
            if !key.is_empty() {
                shape.prompt_cache_key = Some(key.to_owned());
            }
        }
    }
    shape
}

#[allow(clippy::too_many_arguments)]
fn rewrite_json_request_body(
    body: &[u8],
    backend_model: &str,
    native_responses: bool,
    chat_completions: bool,
    responses_store: ResponsesStorePolicy,
    responses_max_output_tokens: ResponsesMaxOutputTokensPolicy,
    chat_stream_usage: ChatStreamUsagePolicy,
    extra_body: &BTreeMap<String, toml::Value>,
    route_label: &str,
) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(body).ok()?;
    let object = value.as_object_mut()?;
    if !object.contains_key("model") {
        return None;
    }
    object.insert("model".to_owned(), Value::String(backend_model.to_owned()));
    if native_responses
        && responses_store == ResponsesStorePolicy::ForceFalse
        && !object.contains_key("store")
    {
        object.insert("store".to_owned(), Value::Bool(false));
    }
    if native_responses {
        rewrite_native_responses_max_output_tokens(object, responses_max_output_tokens);
    } else if chat_completions {
        apply_chat_stream_usage_policy(object, chat_stream_usage);
    }
    apply_extra_body(object, extra_body, route_label);
    serde_json::to_vec(&value).ok()
}

pub(super) fn rewrite_native_responses_max_output_tokens(
    object: &mut Map<String, Value>,
    policy: ResponsesMaxOutputTokensPolicy,
) {
    match policy {
        ResponsesMaxOutputTokensPolicy::Preserve => {}
        ResponsesMaxOutputTokensPolicy::Drop => {
            object.remove("max_output_tokens");
        }
        ResponsesMaxOutputTokensPolicy::RenameToMaxTokens => {
            if let Some(value) = object.remove("max_output_tokens") {
                object.entry("max_tokens".to_owned()).or_insert(value);
            }
        }
        ResponsesMaxOutputTokensPolicy::RenameToMaxCompletionTokens => {
            if let Some(value) = object.remove("max_output_tokens") {
                object
                    .entry("max_completion_tokens".to_owned())
                    .or_insert(value);
            }
        }
    }
}

pub(super) fn apply_chat_stream_usage_policy(
    object: &mut Map<String, Value>,
    policy: ChatStreamUsagePolicy,
) {
    if policy == ChatStreamUsagePolicy::Preserve {
        return;
    }
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        return;
    }

    let include_usage = Value::Bool(true);
    match object.get_mut("stream_options") {
        Some(value) => match value.as_object_mut() {
            Some(options) => {
                if policy == ChatStreamUsagePolicy::Insert {
                    options
                        .entry("include_usage".to_owned())
                        .or_insert(include_usage);
                } else {
                    options.insert("include_usage".to_owned(), include_usage);
                }
            }
            None if policy == ChatStreamUsagePolicy::ForceTrue => {
                *value = Value::Object(Map::from_iter([(
                    "include_usage".to_owned(),
                    include_usage,
                )]));
            }
            None => {}
        },
        None => {
            object.insert(
                "stream_options".to_owned(),
                Value::Object(Map::from_iter([(
                    "include_usage".to_owned(),
                    include_usage,
                )])),
            );
        }
    }
}

fn rewrite_urlencoded_request_body(body: &[u8], backend_model: &str) -> Option<Vec<u8>> {
    let mut saw_model = false;
    let rewritten = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form_urlencoded::parse(body).map(|(key, value)| {
            if key == "model" {
                saw_model = true;
                (key.into_owned(), backend_model.to_owned())
            } else {
                (key.into_owned(), value.into_owned())
            }
        }))
        .finish();

    saw_model.then(|| rewritten.into_bytes())
}

fn rewrite_multipart_body(body: &[u8], boundary: &str, backend_model: &str) -> Option<Vec<u8>> {
    let marker = format!("--{boundary}");
    let marker_bytes = marker.as_bytes();
    let closing_marker = format!("--{boundary}--");
    let closing_marker_bytes = closing_marker.as_bytes();
    let next_delimiter = format!("\r\n--{boundary}");
    let next_delimiter_bytes = next_delimiter.as_bytes();

    if !body.starts_with(marker_bytes) {
        return None;
    }

    let mut cursor = marker_bytes.len();
    if !starts_with_at(body, cursor, b"\r\n") {
        return None;
    }
    cursor += 2;

    let mut output = Vec::with_capacity(body.len());
    output.extend_from_slice(marker_bytes);
    output.extend_from_slice(b"\r\n");

    loop {
        let delimiter_position = find_bytes(body, next_delimiter_bytes, cursor)?;
        let part = &body[cursor..delimiter_position];
        output.extend(rewrite_multipart_part(part, backend_model));

        let boundary_position = delimiter_position + 2;
        if starts_with_at(body, boundary_position, closing_marker_bytes) {
            output.extend_from_slice(b"\r\n");
            output.extend_from_slice(closing_marker_bytes);
            if starts_with_at(
                body,
                boundary_position + closing_marker_bytes.len(),
                b"\r\n",
            ) {
                output.extend_from_slice(b"\r\n");
            }
            break;
        }
        if !starts_with_at(body, boundary_position, marker_bytes) {
            return None;
        }
        let after_marker = boundary_position + marker_bytes.len();
        if !starts_with_at(body, after_marker, b"\r\n") {
            return None;
        }
        output.extend_from_slice(b"\r\n");
        output.extend_from_slice(marker_bytes);
        output.extend_from_slice(b"\r\n");
        cursor = after_marker + 2;
    }

    Some(output)
}

fn rewrite_multipart_part(part: &[u8], backend_model: &str) -> Vec<u8> {
    let Some((headers, _content)) = split_multipart_part(part) else {
        return part.to_vec();
    };
    if !multipart_field_is(headers, "model") {
        return part.to_vec();
    }

    let mut output = Vec::with_capacity(headers.len() + backend_model.len() + 4);
    output.extend_from_slice(headers);
    output.extend_from_slice(b"\r\n\r\n");
    output.extend_from_slice(backend_model.as_bytes());
    output
}

fn multipart_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let marker = format!("--{boundary}");
    let marker_bytes = marker.as_bytes();
    let closing_marker = format!("--{boundary}--");
    let closing_marker_bytes = closing_marker.as_bytes();
    let next_delimiter = format!("\r\n--{boundary}");
    let next_delimiter_bytes = next_delimiter.as_bytes();

    if !body.starts_with(marker_bytes) {
        return Vec::new();
    }

    let mut cursor = marker_bytes.len();
    if !starts_with_at(body, cursor, b"\r\n") {
        return Vec::new();
    }
    cursor += 2;

    let mut parts = Vec::new();
    while let Some(delimiter_position) = find_bytes(body, next_delimiter_bytes, cursor) {
        parts.push(&body[cursor..delimiter_position]);
        let boundary_position = delimiter_position + 2;
        if starts_with_at(body, boundary_position, closing_marker_bytes) {
            break;
        }
        if !starts_with_at(body, boundary_position, marker_bytes) {
            break;
        }
        let after_marker = boundary_position + marker_bytes.len();
        if !starts_with_at(body, after_marker, b"\r\n") {
            break;
        }
        cursor = after_marker + 2;
    }

    parts
}

fn split_multipart_part(part: &[u8]) -> Option<(&[u8], &[u8])> {
    let separator = b"\r\n\r\n";
    let separator_position = find_bytes(part, separator, 0)?;
    Some((
        &part[..separator_position],
        &part[separator_position + separator.len()..],
    ))
}

fn multipart_field_is(headers: &[u8], name: &str) -> bool {
    let headers = String::from_utf8_lossy(headers).to_ascii_lowercase();
    let target_quoted = format!("name=\"{}\"", name.to_ascii_lowercase());
    let target_unquoted = format!("name={}", name.to_ascii_lowercase());
    headers
        .lines()
        .filter(|line| line.starts_with("content-disposition:"))
        .any(|line| line.contains(&target_quoted) || line.contains(&target_unquoted))
}

fn query_field(query: Option<&str>, field: &str) -> Option<String> {
    let query = query?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, value)| key == field && !value.trim().is_empty())
        .map(|(_key, value)| value.into_owned())
}

fn query_bool(query: Option<&str>, field: &str) -> bool {
    let Some(query) = query else {
        return false;
    };
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _value)| key == field)
        .map(|(_key, value)| truthy(&value))
        .unwrap_or(false)
}

pub(super) fn should_parse_json(content_type: Option<&str>, body: &[u8]) -> bool {
    is_json_content_type(content_type) || looks_like_json(body)
}

pub(super) fn looks_like_json(body: &[u8]) -> bool {
    body.iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| byte == b'{' || byte == b'[')
}

fn is_urlencoded_content_type(content_type: Option<&str>) -> bool {
    content_type
        .map(|value| {
            value
                .to_ascii_lowercase()
                .contains("application/x-www-form-urlencoded")
        })
        .unwrap_or(false)
}

fn looks_like_urlencoded(body: &[u8]) -> bool {
    std::str::from_utf8(body)
        .map(|value| value.contains('=') && !value.contains('{'))
        .unwrap_or(false)
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    if !content_type
        .to_ascii_lowercase()
        .contains("multipart/form-data")
    {
        return None;
    }

    content_type.split(';').find_map(|parameter| {
        let parameter = parameter.trim();
        let value = parameter.strip_prefix("boundary=")?;
        Some(value.trim_matches('"').to_owned())
    })
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes"
    )
}

fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || start > haystack.len() {
        return None;
    }

    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + start)
}

fn starts_with_at(haystack: &[u8], start: usize, needle: &[u8]) -> bool {
    haystack
        .get(start..)
        .is_some_and(|remaining| remaining.starts_with(needle))
}
