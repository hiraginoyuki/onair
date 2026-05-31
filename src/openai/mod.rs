use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use url::form_urlencoded;

use crate::config::ToolSchemaMode;

mod models;

#[allow(unused_imports)]
pub use models::{
    DefaultGenerationSettings, ModelMeta, ModelObject, ModelsResponse, PropsResponse,
    model_response, models_response, props_response,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMode {
    Native,
    ResponsesViaChatCompletions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRewriteError {
    message: String,
    param: Option<String>,
}

impl RequestRewriteError {
    fn new(message: impl Into<String>, param: Option<&str>) -> Self {
        Self {
            message: message.into(),
            param: param.map(str::to_owned),
        }
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

pub fn rewrite_request_body_for_mode_with_tool_schema_mode(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    request_mode: RequestMode,
    tool_schema_mode: ToolSchemaMode,
) -> Result<Vec<u8>, RequestRewriteError> {
    if request_mode == RequestMode::ResponsesViaChatCompletions {
        return rewrite_responses_request_as_chat(
            body,
            content_type,
            backend_model,
            tool_schema_mode,
        );
    }

    let Some(backend_model) = backend_model else {
        return Ok(body.to_vec());
    };
    if body.is_empty() {
        return Ok(Vec::new());
    }

    if should_parse_json(content_type, body)
        && let Some(rewritten) = rewrite_json_request_body(body, backend_model)
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
    match request_mode {
        RequestMode::ResponsesViaChatCompletions
            if path.trim_end_matches('/') == "/v1/responses" =>
        {
            "/v1/chat/completions"
        }
        _ => path,
    }
}

fn rewrite_responses_request_as_chat(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    tool_schema_mode: ToolSchemaMode,
) -> Result<Vec<u8>, RequestRewriteError> {
    if body.is_empty() {
        return Err(RequestRewriteError::new(
            "Missing required parameter: input.",
            Some("input"),
        ));
    }
    if !should_parse_json(content_type, body) {
        return Err(RequestRewriteError::new(
            "Responses-to-chat conversion requires a JSON request body.",
            None,
        ));
    }

    let mut value = serde_json::from_slice::<Value>(body).map_err(|_| {
        RequestRewriteError::new("Responses-to-chat conversion requires valid JSON.", None)
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        RequestRewriteError::new("Responses request body must be a JSON object.", None)
    })?;
    if object.contains_key("previous_response_id") {
        return Err(RequestRewriteError::new(
            "previous_response_id is not supported by the responses-to-chat compatibility path.",
            Some("previous_response_id"),
        ));
    }

    let input = object.get("input").ok_or_else(|| {
        RequestRewriteError::new("Missing required parameter: input.", Some("input"))
    })?;
    let mut messages = Vec::new();
    if let Some(instructions) = object.get("instructions").and_then(Value::as_str)
        && !instructions.trim().is_empty()
    {
        messages.push(json!({
            "role": "system",
            "content": instructions,
        }));
    }
    messages.extend(responses_input_to_chat_messages(input)?);

    let mut chat = Map::new();
    let model = backend_model
        .map(str::to_owned)
        .or_else(|| {
            object
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            RequestRewriteError::new("Missing required parameter: model.", Some("model"))
        })?;
    chat.insert("model".to_owned(), Value::String(model));
    chat.insert("messages".to_owned(), Value::Array(messages));

    for key in [
        "temperature",
        "top_p",
        "stream",
        "stop",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "user",
        "seed",
        "n",
        "logprobs",
        "top_logprobs",
        "parallel_tool_calls",
        "stream_options",
        "response_format",
        "max_tokens",
        "prompt_cache_key",
        "prompt_cache_retention",
    ] {
        if let Some(value) = object.get(key) {
            chat.insert(key.to_owned(), value.clone());
        }
    }

    if let Some(value) = object.get("max_output_tokens") {
        chat.insert("max_tokens".to_owned(), value.clone());
    }
    if let Some(format) = object
        .get("text")
        .and_then(Value::as_object)
        .and_then(|text| text.get("format"))
    {
        chat.insert("response_format".to_owned(), format.clone());
    }
    if let Some(tools) = object.get("tools").filter(|value| !value.is_null()) {
        let tools = responses_tools_to_chat_tools(tools, tool_schema_mode)?;
        if !tools.is_empty() {
            chat.insert("tools".to_owned(), Value::Array(tools));
        }
    }
    if let Some(tool_choice) = object.get("tool_choice") {
        chat.insert(
            "tool_choice".to_owned(),
            responses_tool_choice_to_chat(tool_choice),
        );
    }

    serde_json::to_vec(&Value::Object(chat)).map_err(|_| {
        RequestRewriteError::new(
            "Responses-to-chat conversion failed to serialize JSON.",
            None,
        )
    })
}

fn responses_input_to_chat_messages(input: &Value) -> Result<Vec<Value>, RequestRewriteError> {
    match input {
        Value::String(text) => Ok(vec![json!({
            "role": "user",
            "content": text,
        })]),
        Value::Array(items) => {
            let mut messages = Vec::with_capacity(items.len());
            for item in items {
                if let Some(object) = item.as_object()
                    && object.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    append_responses_function_call_to_chat_messages(object, &mut messages)?;
                    continue;
                }
                messages.push(responses_input_item_to_chat_message(item)?);
            }
            Ok(messages)
        }
        _ => Err(RequestRewriteError::new(
            "input must be a string or an array.",
            Some("input"),
        )),
    }
}

fn responses_input_item_to_chat_message(item: &Value) -> Result<Value, RequestRewriteError> {
    if let Some(text) = item.as_str() {
        return Ok(json!({
            "role": "user",
            "content": text,
        }));
    }

    let object = item.as_object().ok_or_else(|| {
        RequestRewriteError::new(
            "Responses input items must be objects or strings.",
            Some("input"),
        )
    })?;
    match object.get("type").and_then(Value::as_str) {
        Some("function_call") => return responses_function_call_to_chat_message(object),
        Some("function_call_output") => return responses_function_output_to_chat_message(object),
        Some("reasoning") => return responses_reasoning_to_chat_message(object),
        _ => {}
    }

    let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
    let role = match role {
        "developer" => "system",
        "system" | "user" | "assistant" | "tool" => role,
        _ => {
            return Err(RequestRewriteError::new(
                "Responses input item role is unsupported by the compatibility path.",
                Some("input"),
            ));
        }
    };
    let content = object.get("content").ok_or_else(|| {
        RequestRewriteError::new("Responses input messages require content.", Some("input"))
    })?;
    let mut message = Map::new();
    message.insert("role".to_owned(), Value::String(role.to_owned()));
    message.insert("content".to_owned(), responses_content_to_chat(content)?);
    if let Some(name) = object.get("name") {
        message.insert("name".to_owned(), name.clone());
    }
    if let Some(tool_call_id) = object.get("tool_call_id").or_else(|| object.get("call_id")) {
        message.insert("tool_call_id".to_owned(), tool_call_id.clone());
    }
    Ok(Value::Object(message))
}

fn responses_content_to_chat(content: &Value) -> Result<Value, RequestRewriteError> {
    match content {
        Value::String(_) => Ok(content.clone()),
        Value::Array(parts) => {
            let mut chat_parts = Vec::new();
            let mut text_only = true;
            for part in parts {
                let object = part.as_object().ok_or_else(|| {
                    RequestRewriteError::new(
                        "Responses content parts must be objects.",
                        Some("input"),
                    )
                })?;
                let part_type = object
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("input_text");
                match part_type {
                    "input_text" | "output_text" | "text" => {
                        let text = object
                            .get("text")
                            .and_then(Value::as_str)
                            .or_else(|| object.get("content").and_then(Value::as_str))
                            .ok_or_else(|| {
                                RequestRewriteError::new(
                                    "Responses text content parts require text.",
                                    Some("input"),
                                )
                            })?;
                        chat_parts.push(json!({
                            "type": "text",
                            "text": text,
                        }));
                    }
                    "image_url" | "input_image" => {
                        text_only = false;
                        chat_parts.push(responses_image_part_to_chat(object)?);
                    }
                    "refusal" => {
                        let refusal = object
                            .get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        chat_parts.push(json!({
                            "type": "text",
                            "text": refusal,
                        }));
                    }
                    _ => {
                        return Err(RequestRewriteError::new(
                            "Responses content part type is unsupported by the compatibility path.",
                            Some("input"),
                        ));
                    }
                }
            }

            if text_only {
                let text = chat_parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
                Ok(Value::String(text))
            } else {
                Ok(Value::Array(chat_parts))
            }
        }
        _ => Ok(Value::String(content.to_string())),
    }
}

fn responses_image_part_to_chat(object: &Map<String, Value>) -> Result<Value, RequestRewriteError> {
    let image_url = object
        .get("image_url")
        .or_else(|| object.get("url"))
        .ok_or_else(|| {
            RequestRewriteError::new(
                "Responses image content parts require image_url or url.",
                Some("input"),
            )
        })?;
    let mut image_url = match image_url {
        Value::String(url) => {
            let mut image_url = Map::new();
            image_url.insert("url".to_owned(), Value::String(url.to_owned()));
            image_url
        }
        Value::Object(image_url) => image_url.clone(),
        _ => {
            return Err(RequestRewriteError::new(
                "Responses image content image_url must be a string or object.",
                Some("input"),
            ));
        }
    };
    if !image_url.contains_key("url")
        && let Some(url) = object.get("url").and_then(Value::as_str)
    {
        image_url.insert("url".to_owned(), Value::String(url.to_owned()));
    }
    if let Some(detail) = object.get("detail")
        && !image_url.contains_key("detail")
    {
        image_url.insert("detail".to_owned(), detail.clone());
    }
    if !image_url.contains_key("url") {
        return Err(RequestRewriteError::new(
            "Responses image content parts require an image URL.",
            Some("input"),
        ));
    }
    Ok(json!({
        "type": "image_url",
        "image_url": Value::Object(image_url),
    }))
}

fn responses_function_call_to_chat_message(
    object: &Map<String, Value>,
) -> Result<Value, RequestRewriteError> {
    let tool_call = responses_function_call_to_chat_tool_call(object)?;
    Ok(json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [tool_call],
    }))
}

fn append_responses_function_call_to_chat_messages(
    object: &Map<String, Value>,
    messages: &mut Vec<Value>,
) -> Result<(), RequestRewriteError> {
    let tool_call = responses_function_call_to_chat_tool_call(object)?;
    if let Some(last_message) = messages.last_mut().and_then(Value::as_object_mut)
        && last_message.get("role").and_then(Value::as_str) == Some("assistant")
        && let Some(tool_calls) = last_message
            .get_mut("tool_calls")
            .and_then(Value::as_array_mut)
    {
        tool_calls.push(tool_call);
        if last_message
            .get("content")
            .is_none_or(|content| content.is_null())
        {
            last_message.insert("content".to_owned(), Value::String(String::new()));
        }
        return Ok(());
    }

    messages.push(json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [tool_call],
    }));
    Ok(())
}

fn responses_function_call_to_chat_tool_call(
    object: &Map<String, Value>,
) -> Result<Value, RequestRewriteError> {
    let call_id = string_field(object, "call_id").ok_or_else(|| {
        RequestRewriteError::new("function_call input items require call_id.", Some("input"))
    })?;
    let name = string_field(object, "name").ok_or_else(|| {
        RequestRewriteError::new("function_call input items require name.", Some("input"))
    })?;
    let arguments = string_field(object, "arguments").unwrap_or_else(|| "{}".to_owned());
    Ok(json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    }))
}

fn responses_function_output_to_chat_message(
    object: &Map<String, Value>,
) -> Result<Value, RequestRewriteError> {
    let call_id = string_field(object, "call_id").ok_or_else(|| {
        RequestRewriteError::new(
            "function_call_output input items require call_id.",
            Some("input"),
        )
    })?;
    let output = object.get("output").ok_or_else(|| {
        RequestRewriteError::new(
            "function_call_output input items require output.",
            Some("input"),
        )
    })?;
    Ok(json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": responses_tool_output_to_string(output),
    }))
}

fn responses_reasoning_to_chat_message(
    object: &Map<String, Value>,
) -> Result<Value, RequestRewriteError> {
    let Some(summary) = object.get("summary").and_then(Value::as_array) else {
        return Ok(json!({
            "role": "assistant",
            "content": "",
        }));
    };
    let text = summary
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({
        "role": "assistant",
        "content": text,
    }))
}

fn responses_tool_output_to_string(output: &Value) -> String {
    if let Some(text) = output.as_str() {
        return text.to_owned();
    }
    if let Some(items) = output.as_array() {
        let text = items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return text;
        }
    }
    output.to_string()
}

fn responses_tools_to_chat_tools(
    tools: &Value,
    tool_schema_mode: ToolSchemaMode,
) -> Result<Vec<Value>, RequestRewriteError> {
    let tools = tools.as_array().ok_or_else(|| {
        RequestRewriteError::new(
            "tools must be an array for responses-to-chat conversion.",
            Some("tools"),
        )
    })?;
    let mut chat_tools = Vec::new();
    for tool in tools {
        let Some(object) = tool.as_object() else {
            return Err(RequestRewriteError::new(
                "tool definitions must be objects.",
                Some("tools"),
            ));
        };
        if object.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        let mut function = object.clone();
        function.remove("type");
        if tool_schema_mode == ToolSchemaMode::LlamacppCompat
            && let Some(parameters) = function.get_mut("parameters")
        {
            sanitize_llamacpp_tool_schema(parameters);
        }
        chat_tools.push(json!({
            "type": "function",
            "function": Value::Object(function),
        }));
    }
    Ok(chat_tools)
}

fn sanitize_llamacpp_tool_schema(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            object.remove("default");
            collapse_nullable_type_array(object);
            collapse_nullable_schema_array(object, "anyOf");
            collapse_nullable_schema_array(object, "oneOf");
            for value in object.values_mut() {
                sanitize_llamacpp_tool_schema(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize_llamacpp_tool_schema(item);
            }
        }
        _ => {}
    }
}

fn collapse_nullable_type_array(object: &mut Map<String, Value>) {
    let Some(types) = object.get("type").and_then(Value::as_array) else {
        return;
    };

    let mut saw_null = false;
    let mut non_null = Vec::new();
    for value in types {
        let Some(type_name) = value.as_str() else {
            return;
        };
        if type_name == "null" {
            saw_null = true;
        } else {
            non_null.push(type_name);
        }
    }

    if saw_null && non_null.len() == 1 {
        object.insert("type".to_owned(), Value::String(non_null[0].to_owned()));
    }
}

fn collapse_nullable_schema_array(object: &mut Map<String, Value>, key: &str) {
    let Some(items) = object.get(key).and_then(Value::as_array) else {
        return;
    };

    let mut saw_null = false;
    let mut non_null = Vec::new();
    for item in items {
        if is_null_schema(item) {
            saw_null = true;
        } else {
            non_null.push(item.clone());
        }
    }

    if !saw_null || non_null.len() != 1 {
        return;
    }
    if !matches!(non_null.first(), Some(Value::Object(_))) {
        return;
    }

    object.remove(key);
    if let Some(Value::Object(replacement)) = non_null.pop() {
        for (replacement_key, replacement_value) in replacement {
            object.entry(replacement_key).or_insert(replacement_value);
        }
    }
}

fn is_null_schema(value: &Value) -> bool {
    match value.as_object().and_then(|object| object.get("type")) {
        Some(Value::String(type_name)) => type_name == "null",
        Some(Value::Array(types)) => types
            .iter()
            .all(|type_name| type_name.as_str() == Some("null")),
        _ => false,
    }
}

fn responses_tool_choice_to_chat(tool_choice: &Value) -> Value {
    if let Some(object) = tool_choice.as_object()
        && object.get("type").and_then(Value::as_str) == Some("function")
        && let Some(name) = object.get("name").cloned()
    {
        return json!({
            "type": "function",
            "function": {
                "name": name,
            },
        });
    }
    tool_choice.clone()
}

fn string_field(object: &Map<String, Value>, field: &str) -> Option<String> {
    object.get(field).and_then(Value::as_str).map(str::to_owned)
}

pub fn is_event_stream_content_type(content_type: Option<&str>) -> bool {
    content_type
        .map(|value| value.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false)
}

pub fn is_json_content_type(content_type: Option<&str>) -> bool {
    content_type
        .map(|value| {
            let lowered = value.to_ascii_lowercase();
            lowered.contains("application/json") || lowered.contains("+json")
        })
        .unwrap_or(false)
}

pub fn rewrite_response_body(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    public_model: Option<&str>,
    request_mode: RequestMode,
) -> (Vec<u8>, UsageTotals) {
    if !is_json_content_type(content_type) && !looks_like_json(body) {
        return (body.to_vec(), UsageTotals::default());
    }

    let Ok(mut json) = serde_json::from_slice::<Value>(body) else {
        return (body.to_vec(), UsageTotals::default());
    };

    let usage = extract_usage(&json);
    if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
        rewrite_response_models(&mut json, backend_model, public_model);
    }
    ensure_usage_total_tokens(&mut json);

    if request_mode == RequestMode::ResponsesViaChatCompletions {
        json = chat_completion_to_response(json);
    }

    let rewritten = serde_json::to_vec(&json).unwrap_or_else(|_| body.to_vec());
    (rewritten, usage)
}

pub fn rewrite_response_models(value: &mut Value, backend_model: &str, public_model: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if key == "model" && value.as_str() == Some(backend_model) {
                    *value = Value::String(public_model.to_owned());
                } else {
                    rewrite_response_models(value, backend_model, public_model);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_response_models(value, backend_model, public_model);
            }
        }
        _ => {}
    }
}

fn ensure_usage_total_tokens(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(usage) = object.get_mut("usage").and_then(Value::as_object_mut) {
                ensure_usage_object_total_tokens(usage);
            }
            for value in object.values_mut() {
                ensure_usage_total_tokens(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                ensure_usage_total_tokens(value);
            }
        }
        _ => {}
    }
}

fn ensure_usage_object_total_tokens(usage: &mut Map<String, Value>) {
    if usage.get("total_tokens").and_then(Value::as_u64).is_some() {
        return;
    }

    let input_tokens =
        number_field(usage, "prompt_tokens").or_else(|| number_field(usage, "input_tokens"));
    let output_tokens =
        number_field(usage, "completion_tokens").or_else(|| number_field(usage, "output_tokens"));
    if let (Some(input_tokens), Some(output_tokens)) = (input_tokens, output_tokens)
        && let Some(total_tokens) = input_tokens.checked_add(output_tokens)
    {
        usage.insert(
            "total_tokens".to_owned(),
            Value::Number(total_tokens.into()),
        );
    }
}

fn chat_completion_to_response(value: Value) -> Value {
    let Some(object) = value.as_object() else {
        return value;
    };
    let chat_id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl_unknown");
    let response_id = response_id_from_chat(chat_id);
    let created_at = object
        .get("created")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let model = object
        .get("model")
        .cloned()
        .unwrap_or_else(|| Value::String("unknown".to_owned()));
    let mut output = Vec::new();
    let mut output_texts = Vec::new();

    if let Some(choices) = object.get("choices").and_then(Value::as_array) {
        for (index, choice) in choices.iter().enumerate() {
            let Some(message) = choice.get("message").and_then(Value::as_object) else {
                continue;
            };
            let message_id = message_id_from_response(&response_id, index);
            if let Some(content) = message.get("content")
                && !content.is_null()
            {
                let text = chat_message_content_to_text(content);
                if !text.is_empty() {
                    output_texts.push(text.clone());
                    output.push(json!({
                        "id": message_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                            "annotations": [],
                        }],
                    }));
                }
            }

            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for (tool_index, tool_call) in tool_calls.iter().enumerate() {
                    let function = tool_call.get("function").and_then(Value::as_object);
                    output.push(json!({
                        "type": "function_call",
                        "status": "completed",
                        "call_id": tool_call
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("call_{index}_{tool_index}")),
                        "name": function
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        "arguments": function
                            .and_then(|function| function.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("{}"),
                    }));
                }
            }
        }
    }

    let usage = chat_usage_to_responses_usage(object.get("usage"));
    json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": output,
        "output_text": output_texts.join(""),
        "usage": usage,
    })
}

fn chat_message_content_to_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    if let Some(parts) = content.as_array() {
        return parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("refusal").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("");
    }
    content.to_string()
}

fn chat_usage_to_responses_usage(usage: Option<&Value>) -> Value {
    let usage = usage.and_then(Value::as_object);
    let input_tokens = usage
        .and_then(|usage| {
            number_field(usage, "prompt_tokens").or_else(|| number_field(usage, "input_tokens"))
        })
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|usage| {
            number_field(usage, "completion_tokens")
                .or_else(|| number_field(usage, "output_tokens"))
        })
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|usage| number_field(usage, "total_tokens"))
        .unwrap_or(input_tokens + output_tokens);
    let cached_tokens = usage
        .and_then(|usage| {
            nested_number_field(usage, "prompt_tokens_details", "cached_tokens")
                .or_else(|| nested_number_field(usage, "input_tokens_details", "cached_tokens"))
        })
        .unwrap_or(0);

    json!({
        "input_tokens": input_tokens,
        "input_tokens_details": {
            "cached_tokens": cached_tokens,
        },
        "output_tokens": output_tokens,
        "output_tokens_details": {},
        "total_tokens": total_tokens,
    })
}

fn response_id_from_chat(chat_id: &str) -> String {
    if chat_id.starts_with("resp_") {
        chat_id.to_owned()
    } else {
        format!("resp_{chat_id}")
    }
}

fn message_id_from_response(response_id: &str, index: usize) -> String {
    format!("msg_{response_id}_{index}")
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UsageTotals {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub total: u64,
}

impl UsageTotals {
    pub fn is_empty(self) -> bool {
        self.input == 0 && self.cached_input == 0 && self.output == 0 && self.total == 0
    }
}

pub fn extract_usage(value: &Value) -> UsageTotals {
    let mut totals = UsageTotals::default();
    collect_usage(value, &mut totals);
    totals
}

fn collect_usage(value: &Value, totals: &mut UsageTotals) {
    match value {
        Value::Object(object) => {
            if let Some(usage) = object.get("usage").and_then(Value::as_object) {
                add_usage_object(usage, totals);
            }
            for value in object.values() {
                collect_usage(value, totals);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_usage(value, totals);
            }
        }
        _ => {}
    }
}

fn add_usage_object(object: &Map<String, Value>, totals: &mut UsageTotals) {
    let input = number_field(object, "prompt_tokens").unwrap_or(0)
        + number_field(object, "input_tokens").unwrap_or(0);
    let output = number_field(object, "completion_tokens").unwrap_or(0)
        + number_field(object, "output_tokens").unwrap_or(0);
    totals.input += input;
    totals.cached_input += nested_number_field(object, "prompt_tokens_details", "cached_tokens")
        .or_else(|| nested_number_field(object, "input_tokens_details", "cached_tokens"))
        .unwrap_or(0);
    totals.output += output;
    totals.total += number_field(object, "total_tokens").unwrap_or(input + output);
}

fn number_field(object: &Map<String, Value>, field: &str) -> Option<u64> {
    object.get(field).and_then(Value::as_u64)
}

fn nested_number_field(object: &Map<String, Value>, parent: &str, field: &str) -> Option<u64> {
    object
        .get(parent)
        .and_then(Value::as_object)
        .and_then(|object| number_field(object, field))
}

#[derive(Debug, Default)]
pub struct SseNormalizer {
    pending: Vec<u8>,
    pub usage: UsageTotals,
    backend_model: Option<String>,
    public_model: Option<String>,
}

impl SseNormalizer {
    pub fn new(backend_model: Option<String>, public_model: Option<String>) -> Self {
        Self {
            backend_model,
            public_model,
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut output = Vec::with_capacity(self.pending.len());

        while let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=position).collect::<Vec<_>>();
            output.extend(self.normalize_line(&line));
        }

        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.pending);
        self.normalize_line(&line)
    }

    fn normalize_line(&mut self, line: &[u8]) -> Vec<u8> {
        let line_without_newline = line.strip_suffix(b"\n").unwrap_or(line);
        let line_ending = if line.ends_with(b"\n") {
            b"\n".as_slice()
        } else {
            b"".as_slice()
        };
        let line_without_cr = line_without_newline
            .strip_suffix(b"\r")
            .unwrap_or(line_without_newline);
        let cr = if line_without_newline.ends_with(b"\r") {
            b"\r".as_slice()
        } else {
            b"".as_slice()
        };

        let Some(data) = line_without_cr.strip_prefix(b"data:") else {
            return line.to_vec();
        };
        let leading_space = data.starts_with(b" ");
        let data = if leading_space { &data[1..] } else { data };
        if data == b"[DONE]" {
            return line.to_vec();
        }

        let Ok(mut json) = serde_json::from_slice::<Value>(data) else {
            return line.to_vec();
        };
        let usage = extract_usage(&json);
        self.usage.input += usage.input;
        self.usage.cached_input += usage.cached_input;
        self.usage.output += usage.output;
        self.usage.total += usage.total;
        if let (Some(backend_model), Some(public_model)) = (&self.backend_model, &self.public_model)
        {
            rewrite_response_models(&mut json, backend_model, public_model);
        }
        ensure_usage_total_tokens(&mut json);

        let normalized = serde_json::to_vec(&json).unwrap_or_else(|_| data.to_vec());
        let mut output = Vec::with_capacity(line.len() + normalized.len());
        output.extend_from_slice(b"data:");
        if leading_space {
            output.extend_from_slice(b" ");
        }
        output.extend_from_slice(&normalized);
        output.extend_from_slice(cr);
        output.extend_from_slice(line_ending);
        output
    }
}

#[derive(Debug, Default)]
pub struct ResponsesSseNormalizer {
    pending: Vec<u8>,
    pub usage: UsageTotals,
    backend_model: Option<String>,
    public_model: Option<String>,
    response_id: Option<String>,
    message_id: Option<String>,
    created_at: u64,
    model: Option<String>,
    response_started: bool,
    text_started: bool,
    output_text: String,
    tool_calls: BTreeMap<usize, StreamToolCall>,
    completed: bool,
}

#[derive(Debug, Default)]
struct StreamToolCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    added: bool,
}

impl ResponsesSseNormalizer {
    pub fn new(backend_model: Option<String>, public_model: Option<String>) -> Self {
        Self {
            backend_model,
            public_model,
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut output = Vec::new();

        while let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=position).collect::<Vec<_>>();
            output.extend(self.normalize_line(&line));
        }

        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            output.extend(self.normalize_line(&line));
        }
        output.extend(self.finish_response());
        output
    }

    fn normalize_line(&mut self, line: &[u8]) -> Vec<u8> {
        let line_without_newline = line.strip_suffix(b"\n").unwrap_or(line);
        let Some(data) = line_without_newline
            .strip_suffix(b"\r")
            .unwrap_or(line_without_newline)
            .strip_prefix(b"data:")
        else {
            return line.to_vec();
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data == b"[DONE]" {
            let mut output = self.finish_response();
            output.extend_from_slice(line);
            return output;
        }

        let Ok(mut chunk) = serde_json::from_slice::<Value>(data) else {
            return line.to_vec();
        };
        let usage = extract_usage(&chunk);
        self.usage.input += usage.input;
        self.usage.cached_input += usage.cached_input;
        self.usage.output += usage.output;
        self.usage.total += usage.total;
        if let (Some(backend_model), Some(public_model)) = (&self.backend_model, &self.public_model)
        {
            rewrite_response_models(&mut chunk, backend_model, public_model);
        }
        self.process_chat_chunk(&chunk)
    }

    fn process_chat_chunk(&mut self, chunk: &Value) -> Vec<u8> {
        self.set_chunk_metadata(chunk);
        let mut output = self.start_response_events();
        if let Some(choices) = chunk.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
                    if let Some(text) = delta.get("content").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        output.extend(self.start_text_events());
                        self.output_text.push_str(text);
                        output.extend(sse_event(
                            "response.output_text.delta",
                            json!({
                                "type": "response.output_text.delta",
                                "item_id": self.message_id(),
                                "output_index": 0,
                                "content_index": 0,
                                "delta": text,
                            }),
                        ));
                    }
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                        output.extend(self.process_tool_call_deltas(tool_calls));
                    }
                }
                if !choice
                    .get("finish_reason")
                    .unwrap_or(&Value::Null)
                    .is_null()
                {
                    output.extend(self.finish_response());
                }
            }
        }
        output
    }

    fn set_chunk_metadata(&mut self, chunk: &Value) {
        if self.response_id.is_none() {
            let chat_id = chunk
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("chatcmpl_unknown");
            let response_id = response_id_from_chat(chat_id);
            self.message_id = Some(message_id_from_response(&response_id, 0));
            self.response_id = Some(response_id);
        }
        if self.created_at == 0 {
            self.created_at = chunk.get("created").and_then(Value::as_u64).unwrap_or(0);
        }
        if self.model.is_none() {
            self.model = chunk
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }

    fn start_response_events(&mut self) -> Vec<u8> {
        if self.response_started {
            return Vec::new();
        }
        self.response_started = true;
        let response = self.response_object("in_progress", Vec::new());
        let mut output = Vec::new();
        output.extend(sse_event(
            "response.created",
            json!({
                "type": "response.created",
                "response": response.clone(),
            }),
        ));
        output.extend(sse_event(
            "response.in_progress",
            json!({
                "type": "response.in_progress",
                "response": response,
            }),
        ));
        output
    }

    fn start_text_events(&mut self) -> Vec<u8> {
        if self.text_started {
            return Vec::new();
        }
        self.text_started = true;
        let mut output = Vec::new();
        output.extend(sse_event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": self.message_id(),
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        ));
        output.extend(sse_event(
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": self.message_id(),
                "output_index": 0,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": "",
                    "annotations": [],
                },
            }),
        ));
        output
    }

    fn process_tool_call_deltas(&mut self, tool_calls: &[Value]) -> Vec<u8> {
        let mut output = Vec::new();
        for tool_call in tool_calls {
            let index = tool_call
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(self.tool_calls.len());
            let state = self.tool_calls.entry(index).or_default();
            if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                state.call_id.get_or_insert_with(|| id.to_owned());
            }
            if let Some(function) = tool_call.get("function").and_then(Value::as_object) {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    state.name = Some(name.to_owned());
                }
                if state.call_id.is_none() && state.name.is_some() {
                    state.call_id = Some(format!("call_{index}"));
                }
                if !state.added
                    && let (Some(call_id), Some(name)) = (&state.call_id, &state.name)
                {
                    state.added = true;
                    output.extend(sse_event(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": index,
                            "item": {
                                "id": call_id,
                                "type": "function_call",
                                "status": "in_progress",
                                "call_id": call_id,
                                "name": name,
                                "arguments": "",
                            },
                        }),
                    ));
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str)
                    && !arguments.is_empty()
                {
                    if state.call_id.is_none() {
                        state.call_id = Some(format!("call_{index}"));
                    }
                    state.arguments.push_str(arguments);
                    let call_id = state
                        .call_id
                        .clone()
                        .unwrap_or_else(|| format!("call_{index}"));
                    output.extend(sse_event(
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": call_id,
                            "call_id": call_id,
                            "output_index": index,
                            "delta": arguments,
                        }),
                    ));
                }
            }
        }
        output
    }

    fn finish_response(&mut self) -> Vec<u8> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        let mut output = Vec::new();
        let mut response_output = Vec::new();
        if self.text_started {
            let part = json!({
                "type": "output_text",
                "text": self.output_text,
                "annotations": [],
                "logprobs": [],
            });
            output.extend(sse_event(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "item_id": self.message_id(),
                    "output_index": 0,
                    "content_index": 0,
                    "text": self.output_text,
                }),
            ));
            output.extend(sse_event(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done",
                    "item_id": self.message_id(),
                    "output_index": 0,
                    "content_index": 0,
                    "part": part.clone(),
                }),
            ));
            let item = json!({
                "id": self.message_id(),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [part],
            });
            output.extend(sse_event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": item.clone(),
                }),
            ));
            response_output.push(item);
        }

        for (index, tool_call) in &self.tool_calls {
            let call_id = tool_call
                .call_id
                .clone()
                .unwrap_or_else(|| format!("call_{index}"));
            output.extend(sse_event(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": call_id,
                    "call_id": call_id,
                    "output_index": index,
                    "arguments": tool_call.arguments,
                }),
            ));
            let item = json!({
                "id": call_id,
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": tool_call.name.clone().unwrap_or_else(|| "unknown".to_owned()),
                "arguments": tool_call.arguments,
            });
            output.extend(sse_event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": index,
                    "item": item.clone(),
                }),
            ));
            response_output.push(item);
        }

        let response = self.response_object("completed", response_output);
        output.extend(sse_event(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": response,
            }),
        ));
        output
    }

    fn response_object(&self, status: &str, output: Vec<Value>) -> Value {
        json!({
            "id": self.response_id(),
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model.clone().unwrap_or_else(|| "unknown".to_owned()),
            "output": output,
            "output_text": self.output_text,
        "usage": {
                "input_tokens": self.usage.input,
                "input_tokens_details": {
                    "cached_tokens": self.usage.cached_input,
                },
                "output_tokens": self.usage.output,
                "output_tokens_details": {},
                "total_tokens": if self.usage.total > 0 {
                    self.usage.total
                } else {
                    self.usage.input + self.usage.output
                },
            },
        })
    }

    fn response_id(&self) -> String {
        self.response_id
            .clone()
            .unwrap_or_else(|| "resp_unknown".to_owned())
    }

    fn message_id(&self) -> String {
        self.message_id
            .clone()
            .unwrap_or_else(|| message_id_from_response(&self.response_id(), 0))
    }
}

fn sse_event(event: &str, data: Value) -> Vec<u8> {
    let data = serde_json::to_vec(&data).unwrap_or_else(|_| b"{}".to_vec());
    let mut output = Vec::with_capacity(event.len() + data.len() + 16);
    output.extend_from_slice(b"event: ");
    output.extend_from_slice(event.as_bytes());
    output.extend_from_slice(b"\n");
    output.extend_from_slice(b"data: ");
    output.extend_from_slice(&data);
    output.extend_from_slice(b"\n\n");
    output
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

fn rewrite_json_request_body(body: &[u8], backend_model: &str) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(body).ok()?;
    let object = value.as_object_mut()?;
    if !object.contains_key("model") {
        return None;
    }
    object.insert("model".to_owned(), Value::String(backend_model.to_owned()));
    serde_json::to_vec(&value).ok()
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

fn should_parse_json(content_type: Option<&str>, body: &[u8]) -> bool {
    is_json_content_type(content_type) || looks_like_json(body)
}

fn looks_like_json(body: &[u8]) -> bool {
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

#[cfg(test)]
mod tests;
