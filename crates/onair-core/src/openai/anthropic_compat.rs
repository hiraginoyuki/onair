use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::request::{RequestRewriteError, RewriteParam, apply_extra_body, should_parse_json};

pub(super) fn rewrite_chat_request_as_anthropic(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    anthropic_max_tokens: Option<u32>,
    extra_body: &BTreeMap<String, toml::Value>,
    route_label: &str,
) -> Result<Vec<u8>, RequestRewriteError> {
    if body.is_empty() {
        return Err(RequestRewriteError::with_param(
            "Missing required parameter: messages.",
            RewriteParam::Messages,
        ));
    }
    if !should_parse_json(content_type, body) {
        return Err(RequestRewriteError::new(
            "Chat-to-messages conversion requires a JSON request body.",
            None,
        ));
    }

    let value = serde_json::from_slice::<Value>(body).map_err(|_| {
        RequestRewriteError::new("Chat-to-messages conversion requires valid JSON.", None)
    })?;
    let object = value.as_object().ok_or_else(|| {
        RequestRewriteError::new("Chat request body must be a JSON object.", None)
    })?;

    validate_chat_to_anthropic_options(object)?;

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RequestRewriteError::with_param(
                "Missing required parameter: messages.",
                RewriteParam::Messages,
            )
        })?;

    let model = backend_model
        .map(str::to_owned)
        .or_else(|| {
            object
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            RequestRewriteError::with_param(
                "Missing required parameter: model.",
                RewriteParam::Model,
            )
        })?;

    let max_tokens = resolve_max_tokens(object, anthropic_max_tokens)?;

    let mut anthropic_messages = Vec::new();
    let mut system_parts = Vec::new();
    let mut pending_tool_use_ids = BTreeSet::new();

    for message in messages {
        append_chat_message_to_anthropic(
            message,
            &mut system_parts,
            &mut anthropic_messages,
            &mut pending_tool_use_ids,
        )?;
    }

    let mut anthropic = Map::new();
    anthropic.insert("model".to_owned(), Value::String(model));
    anthropic.insert("messages".to_owned(), Value::Array(anthropic_messages));
    anthropic.insert("max_tokens".to_owned(), Value::Number(max_tokens.into()));

    if !system_parts.is_empty() {
        anthropic.insert(
            "system".to_owned(),
            Value::String(system_parts.join("\n\n")),
        );
    }

    if let Some(stop) = object.get("stop") {
        anthropic.insert("stop_sequences".to_owned(), normalize_stop_sequences(stop)?);
    }
    if let Some(temperature) = object.get("temperature") {
        anthropic.insert("temperature".to_owned(), temperature.clone());
    }
    if let Some(top_p) = object.get("top_p") {
        anthropic.insert("top_p".to_owned(), top_p.clone());
    }
    if let Some(stream) = object.get("stream") {
        anthropic.insert("stream".to_owned(), stream.clone());
    }
    if let Some(tools) = object.get("tools").filter(|value| !value.is_null()) {
        let anthropic_tools = chat_tools_to_anthropic_tools(tools)?;
        if !anthropic_tools.is_empty() {
            anthropic.insert("tools".to_owned(), Value::Array(anthropic_tools));
        }
    }
    if let Some(tool_choice) = object.get("tool_choice") {
        anthropic.insert(
            "tool_choice".to_owned(),
            chat_tool_choice_to_anthropic(tool_choice)?,
        );
    }

    apply_extra_body(&mut anthropic, extra_body, route_label);
    serde_json::to_vec(&Value::Object(anthropic)).map_err(|_| {
        RequestRewriteError::new(
            "Chat-to-messages conversion failed to serialize JSON.",
            None,
        )
    })
}

pub(super) fn anthropic_message_to_chat_completion(message: Value) -> Value {
    let Some(object) = message.as_object() else {
        return message;
    };

    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_unknown");
    let model = object
        .get("model")
        .cloned()
        .unwrap_or_else(|| Value::String("unknown".to_owned()));
    let created = 0u64;
    let (content, tool_calls) = anthropic_content_to_chat_message(object.get("content"));
    let stop_reason = object
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(chat_finish_reason_from_anthropic_stop_reason)
        .unwrap_or("stop");

    let mut message = Map::new();
    message.insert("role".to_owned(), Value::String("assistant".to_owned()));
    if tool_calls.is_empty() {
        message.insert(
            "content".to_owned(),
            if content.is_empty() {
                Value::Null
            } else {
                Value::String(content)
            },
        );
    } else {
        message.insert(
            "content".to_owned(),
            if content.is_empty() {
                Value::Null
            } else {
                Value::String(content)
            },
        );
        message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }

    json!({
        "id": chat_id_from_anthropic(id),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": stop_reason,
        }],
        "usage": anthropic_usage_to_chat_usage(object.get("usage")),
    })
}

fn validate_chat_to_anthropic_options(
    object: &Map<String, Value>,
) -> Result<(), RequestRewriteError> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "model"
                | "messages"
                | "tools"
                | "tool_choice"
                | "max_tokens"
                | "max_completion_tokens"
                | "stop"
                | "temperature"
                | "top_p"
                | "stream"
                | "n"
                | "logprobs"
                | "top_logprobs"
                | "presence_penalty"
                | "frequency_penalty"
                | "logit_bias"
                | "seed"
                | "response_format"
                | "parallel_tool_calls"
                | "stream_options"
                | "user"
                | "audio"
                | "modalities"
                | "prediction"
        ) {
            return Err(RequestRewriteError::new(
                format!("Unsupported parameter: {key}."),
                Some(key),
            ));
        }
    }

    if let Some(n) = object.get("n")
        && !matches!(n.as_u64(), Some(1))
    {
        return Err(RequestRewriteError::with_param(
            "n > 1 is not supported by the chat-to-messages compatibility path.",
            RewriteParam::N,
        ));
    }
    if let Some(logprobs) = object.get("logprobs")
        && !matches!(logprobs, Value::Bool(false) | Value::Null)
    {
        return Err(RequestRewriteError::with_param(
            "logprobs is not supported by the chat-to-messages compatibility path.",
            RewriteParam::Logprobs,
        ));
    }
    if object.contains_key("top_logprobs") {
        return Err(RequestRewriteError::with_param(
            "top_logprobs is not supported by the chat-to-messages compatibility path.",
            RewriteParam::TopLogprobs,
        ));
    }

    for key in [
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "seed",
        "response_format",
        "stream_options",
        "user",
        "audio",
        "modalities",
        "prediction",
    ] {
        if object.contains_key(key) {
            return Err(RequestRewriteError::new(
                format!("{key} is not supported by the chat-to-messages compatibility path."),
                Some(key),
            ));
        }
    }

    if let Some(parallel_tool_calls) = object.get("parallel_tool_calls")
        && !matches!(parallel_tool_calls, Value::Bool(false) | Value::Null)
    {
        return Err(RequestRewriteError::new(
            "parallel_tool_calls is not supported by the chat-to-messages compatibility path.",
            Some("parallel_tool_calls"),
        ));
    }

    Ok(())
}

fn resolve_max_tokens(
    object: &Map<String, Value>,
    anthropic_max_tokens: Option<u32>,
) -> Result<u32, RequestRewriteError> {
    let max_tokens = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let max_completion_tokens = object
        .get("max_completion_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());

    if let (Some(left), Some(right)) = (max_tokens, max_completion_tokens)
        && left != right
    {
        return Err(RequestRewriteError::new(
            "max_tokens and max_completion_tokens disagree.",
            Some("max_tokens"),
        ));
    }

    max_tokens
        .or(max_completion_tokens)
        .or(anthropic_max_tokens)
        .ok_or_else(|| {
            RequestRewriteError::new(
                "Missing required parameter: max_tokens.",
                Some("max_tokens"),
            )
        })
}

fn normalize_stop_sequences(stop: &Value) -> Result<Value, RequestRewriteError> {
    match stop {
        Value::String(text) => Ok(Value::Array(vec![Value::String(text.clone())])),
        Value::Array(values) => Ok(Value::Array(values.clone())),
        Value::Null => Ok(Value::Null),
        _ => Err(RequestRewriteError::new(
            "stop must be a string or array.",
            Some("stop"),
        )),
    }
}

fn append_chat_message_to_anthropic(
    message: &Value,
    system_parts: &mut Vec<String>,
    anthropic_messages: &mut Vec<Value>,
    pending_tool_use_ids: &mut BTreeSet<String>,
) -> Result<(), RequestRewriteError> {
    let object = message.as_object().ok_or_else(|| {
        RequestRewriteError::with_param("Chat messages must be objects.", RewriteParam::Messages)
    })?;
    let role = object.get("role").and_then(Value::as_str).ok_or_else(|| {
        RequestRewriteError::with_param("Chat messages require role.", RewriteParam::Messages)
    })?;

    match role {
        "system" | "developer" => {
            let content = object.get("content").ok_or_else(|| {
                RequestRewriteError::new(
                    "System and developer messages require content.",
                    Some("messages"),
                )
            })?;
            let text = chat_instruction_text(content)?;
            if !text.trim().is_empty() {
                system_parts.push(text);
            }
        }
        "user" => {
            let content = object.get("content").unwrap_or(&Value::Null);
            let content = chat_message_content_to_anthropic_blocks(content, false)?;
            anthropic_messages.push(json!({
                "role": "user",
                "content": content,
            }));
        }
        "assistant" => {
            let content = object.get("content").unwrap_or(&Value::Null);
            let mut blocks = chat_message_content_to_anthropic_blocks(content, true)?;

            if let Some(tool_calls) = object.get("tool_calls").filter(|value| !value.is_null()) {
                let tool_calls = tool_calls.as_array().ok_or_else(|| {
                    RequestRewriteError::new(
                        "assistant tool_calls must be an array.",
                        Some("messages"),
                    )
                })?;
                for (index, tool_call) in tool_calls.iter().enumerate() {
                    let tool_call = tool_call.as_object().ok_or_else(|| {
                        RequestRewriteError::new(
                            "assistant tool_calls must be objects.",
                            Some("messages"),
                        )
                    })?;
                    if tool_call
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value != "function")
                    {
                        return Err(RequestRewriteError::new(
                            "Only function tool calls are supported by the chat-to-messages compatibility path.",
                            Some("messages"),
                        ));
                    }
                    let function = tool_call
                        .get("function")
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            RequestRewriteError::new(
                                "assistant function tool calls require function.",
                                Some("messages"),
                            )
                        })?;
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.trim().is_empty())
                        .ok_or_else(|| {
                            RequestRewriteError::new(
                                "assistant function tool calls require function.name.",
                                Some("messages"),
                            )
                        })?;
                    let arguments = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    let input = serde_json::from_str::<Value>(arguments).map_err(|_| {
                        RequestRewriteError::new(
                            "assistant function tool call arguments must be valid JSON.",
                            Some("messages"),
                        )
                    })?;
                    let tool_use_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("call_{index}"));
                    pending_tool_use_ids.insert(tool_use_id.clone());
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tool_use_id,
                        "name": name,
                        "input": input,
                    }));
                }
            }

            anthropic_messages.push(json!({
                "role": "assistant",
                "content": blocks,
            }));
        }
        "tool" => {
            let tool_call_id = object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    RequestRewriteError::new(
                        "Tool messages require tool_call_id.",
                        Some("messages"),
                    )
                })?;
            if !pending_tool_use_ids.contains(tool_call_id) {
                pending_tool_use_ids.insert(tool_call_id.to_owned());
            }
            let result_content = tool_message_content_to_anthropic(object.get("content"))?;
            anthropic_messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": result_content,
                }],
            }));
        }
        _ => {
            return Err(RequestRewriteError::new(
                "Chat message role is unsupported by the compatibility path.",
                Some("messages"),
            ));
        }
    }

    Ok(())
}

fn chat_instruction_text(content: &Value) -> Result<String, RequestRewriteError> {
    match content {
        Value::String(text) => Ok(text.to_owned()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let object = part.as_object().ok_or_else(|| {
                    RequestRewriteError::new(
                        "System and developer content parts must be objects.",
                        Some("messages"),
                    )
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let part_text =
                            object.get("text").and_then(Value::as_str).ok_or_else(|| {
                                RequestRewriteError::new(
                                    "System and developer text content parts require text.",
                                    Some("messages"),
                                )
                            })?;
                        text.push_str(part_text);
                    }
                    _ => {
                        return Err(RequestRewriteError::new(
                            "System and developer messages only support text content.",
                            Some("messages"),
                        ));
                    }
                }
            }
            Ok(text)
        }
        Value::Null => Ok(String::new()),
        _ => Err(RequestRewriteError::new(
            "System and developer message content must be text.",
            Some("messages"),
        )),
    }
}

fn chat_message_content_to_anthropic_blocks(
    content: &Value,
    allow_empty_null: bool,
) -> Result<Vec<Value>, RequestRewriteError> {
    match content {
        Value::String(text) => Ok(vec![json!({
            "type": "text",
            "text": text,
        })]),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                let object = part.as_object().ok_or_else(|| {
                    RequestRewriteError::new(
                        "Chat content parts must be objects.",
                        Some("messages"),
                    )
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = object.get("text").and_then(Value::as_str).ok_or_else(|| {
                            RequestRewriteError::new(
                                "Chat text content parts require text.",
                                Some("messages"),
                            )
                        })?;
                        blocks.push(json!({
                            "type": "text",
                            "text": text,
                        }));
                    }
                    Some("image_url") | Some("input_audio") | Some("file") | Some("refusal") => {
                        return Err(RequestRewriteError::new(
                            "Chat content part type is unsupported by the chat-to-messages compatibility path.",
                            Some("messages"),
                        ));
                    }
                    _ => {
                        return Err(RequestRewriteError::new(
                            "Chat content part type is unsupported by the chat-to-messages compatibility path.",
                            Some("messages"),
                        ));
                    }
                }
            }
            Ok(blocks)
        }
        Value::Null if allow_empty_null => Ok(Vec::new()),
        Value::Null => Ok(vec![json!({
            "type": "text",
            "text": "",
        })]),
        _ => Err(RequestRewriteError::new(
            "Chat message content must be a string, array, or null.",
            Some("messages"),
        )),
    }
}

fn tool_message_content_to_anthropic(
    content: Option<&Value>,
) -> Result<Value, RequestRewriteError> {
    let Some(content) = content else {
        return Ok(Value::String(String::new()));
    };
    match content {
        Value::String(text) => Ok(Value::String(text.clone())),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let object = part.as_object().ok_or_else(|| {
                    RequestRewriteError::new(
                        "Tool message content parts must be objects.",
                        Some("messages"),
                    )
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let part_text =
                            object.get("text").and_then(Value::as_str).ok_or_else(|| {
                                RequestRewriteError::new(
                                    "Tool text content parts require text.",
                                    Some("messages"),
                                )
                            })?;
                        text.push_str(part_text);
                    }
                    _ => {
                        return Err(RequestRewriteError::new(
                            "Tool messages only support text content.",
                            Some("messages"),
                        ));
                    }
                }
            }
            Ok(Value::String(text))
        }
        Value::Null => Ok(Value::String(String::new())),
        _ => Ok(Value::String(content.to_string())),
    }
}

fn chat_tools_to_anthropic_tools(tools: &Value) -> Result<Vec<Value>, RequestRewriteError> {
    let tools = tools.as_array().ok_or_else(|| {
        RequestRewriteError::with_param("tools must be an array.", RewriteParam::Tools)
    })?;
    let mut anthropic_tools = Vec::new();
    for tool in tools {
        let object = tool.as_object().ok_or_else(|| {
            RequestRewriteError::with_param(
                "tool definitions must be objects.",
                RewriteParam::Tools,
            )
        })?;
        if object.get("type").and_then(Value::as_str) != Some("function") {
            return Err(RequestRewriteError::with_param(
                "Only function tools are supported by the chat-to-messages compatibility path.",
                RewriteParam::Tools,
            ));
        }
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                RequestRewriteError::with_param(
                    "function tools require function.",
                    RewriteParam::Tools,
                )
            })?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                RequestRewriteError::with_param(
                    "function tools require function.name.",
                    RewriteParam::Tools,
                )
            })?;
        let mut anthropic_tool = Map::new();
        anthropic_tool.insert("name".to_owned(), Value::String(name.to_owned()));
        if let Some(description) = function.get("description") {
            anthropic_tool.insert("description".to_owned(), description.clone());
        }
        anthropic_tool.insert(
            "input_schema".to_owned(),
            function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}})),
        );
        anthropic_tools.push(Value::Object(anthropic_tool));
    }
    Ok(anthropic_tools)
}

fn chat_tool_choice_to_anthropic(tool_choice: &Value) -> Result<Value, RequestRewriteError> {
    match tool_choice {
        Value::String(value) => match value.as_str() {
            "auto" => Ok(json!({"type":"auto"})),
            "required" => Ok(json!({"type":"any"})),
            "none" => Ok(json!({"type":"none"})),
            _ => Err(RequestRewriteError::new(
                "tool_choice value is unsupported by the chat-to-messages compatibility path.",
                Some("tool_choice"),
            )),
        },
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) != Some("function") {
                return Err(RequestRewriteError::new(
                    "tool_choice object is unsupported by the chat-to-messages compatibility path.",
                    Some("tool_choice"),
                ));
            }
            let function = object
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    RequestRewriteError::new(
                        "function tool_choice requires function.",
                        Some("tool_choice"),
                    )
                })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    RequestRewriteError::new(
                        "function tool_choice requires function.name.",
                        Some("tool_choice"),
                    )
                })?;
            Ok(json!({
                "type": "tool",
                "name": name,
            }))
        }
        _ => Err(RequestRewriteError::new(
            "tool_choice is unsupported by the chat-to-messages compatibility path.",
            Some("tool_choice"),
        )),
    }
}

fn anthropic_content_to_chat_message(content: Option<&Value>) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let Some(Value::Array(blocks)) = content else {
        return (text, tool_calls);
    };

    for (index, block) in blocks.iter().enumerate() {
        let Some(object) = block.as_object() else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(part_text) = object.get("text").and_then(Value::as_str) {
                    text.push_str(part_text);
                }
            }
            Some("tool_use") => {
                let arguments = serde_json::to_string(
                    &object.get("input").cloned().unwrap_or_else(|| json!({})),
                )
                .unwrap_or_else(|_| "{}".to_owned());
                tool_calls.push(json!({
                    "id": object
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("call_{index}")),
                    "type": "function",
                    "function": {
                        "name": object
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        "arguments": arguments,
                    }
                }));
            }
            _ => {}
        }
    }

    (text, tool_calls)
}

fn anthropic_usage_to_chat_usage(usage: Option<&Value>) -> Value {
    let usage = usage.and_then(Value::as_object);
    let prompt_tokens = usage
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    })
}

pub(super) fn chat_finish_reason_from_anthropic_stop_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

fn chat_id_from_anthropic(id: &str) -> String {
    if id.starts_with("chatcmpl") {
        id.to_owned()
    } else {
        format!("chatcmpl_{id}")
    }
}
