use serde_json::{Map, Value, json};

use onair_core::config::{
    ChatStreamUsagePolicy, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, ToolSchemaMode,
};

use super::request::{
    RequestRewriteError, apply_chat_stream_usage_policy,
    rewrite_native_responses_max_output_tokens, should_parse_json,
};

pub(super) fn rewrite_responses_request_as_chat(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    tool_schema_mode: ToolSchemaMode,
    chat_stream_usage: ChatStreamUsagePolicy,
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
    apply_chat_stream_usage_policy(&mut chat, chat_stream_usage);

    serde_json::to_vec(&Value::Object(chat)).map_err(|_| {
        RequestRewriteError::new(
            "Responses-to-chat conversion failed to serialize JSON.",
            None,
        )
    })
}

pub(super) fn rewrite_chat_request_as_responses(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    responses_store: ResponsesStorePolicy,
    responses_max_output_tokens: ResponsesMaxOutputTokensPolicy,
) -> Result<Vec<u8>, RequestRewriteError> {
    if body.is_empty() {
        return Err(RequestRewriteError::new(
            "Missing required parameter: messages.",
            Some("messages"),
        ));
    }
    if !should_parse_json(content_type, body) {
        return Err(RequestRewriteError::new(
            "Chat-to-responses conversion requires a JSON request body.",
            None,
        ));
    }

    let value = serde_json::from_slice::<Value>(body).map_err(|_| {
        RequestRewriteError::new("Chat-to-responses conversion requires valid JSON.", None)
    })?;
    let object = value.as_object().ok_or_else(|| {
        RequestRewriteError::new("Chat request body must be a JSON object.", None)
    })?;
    validate_chat_to_responses_options(object)?;

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RequestRewriteError::new("Missing required parameter: messages.", Some("messages"))
        })?;

    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        append_chat_message_to_responses(message, &mut instructions, &mut input)?;
    }

    let mut responses = Map::new();
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
    responses.insert("model".to_owned(), Value::String(model));
    if !instructions.is_empty() {
        responses.insert(
            "instructions".to_owned(),
            Value::String(instructions.join("\n\n")),
        );
    }
    responses.insert("input".to_owned(), Value::Array(input));

    for key in [
        "temperature",
        "top_p",
        "stream",
        "store",
        "metadata",
        "parallel_tool_calls",
        "prompt_cache_key",
        "prompt_cache_retention",
    ] {
        if let Some(value) = object.get(key) {
            responses.insert(key.to_owned(), value.clone());
        }
    }

    if let Some(value) = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
    {
        responses.insert("max_output_tokens".to_owned(), value.clone());
    }
    if let Some(response_format) = object.get("response_format") {
        responses.insert(
            "text".to_owned(),
            json!({
                "format": response_format,
            }),
        );
    }
    if let Some(tools) = object.get("tools").filter(|value| !value.is_null()) {
        let tools = chat_tools_to_responses_tools(tools)?;
        if !tools.is_empty() {
            responses.insert("tools".to_owned(), Value::Array(tools));
        }
    }
    if let Some(tool_choice) = object.get("tool_choice") {
        responses.insert(
            "tool_choice".to_owned(),
            chat_tool_choice_to_responses(tool_choice)?,
        );
    }

    if responses_store == ResponsesStorePolicy::ForceFalse && !responses.contains_key("store") {
        responses.insert("store".to_owned(), Value::Bool(false));
    }
    rewrite_native_responses_max_output_tokens(&mut responses, responses_max_output_tokens);

    serde_json::to_vec(&Value::Object(responses)).map_err(|_| {
        RequestRewriteError::new(
            "Chat-to-responses conversion failed to serialize JSON.",
            None,
        )
    })
}

fn validate_chat_to_responses_options(
    object: &Map<String, Value>,
) -> Result<(), RequestRewriteError> {
    if let Some(n) = object.get("n")
        && !matches!(n.as_u64(), Some(1))
    {
        return Err(RequestRewriteError::new(
            "n > 1 is not supported by the chat-to-responses compatibility path.",
            Some("n"),
        ));
    }
    if let Some(logprobs) = object.get("logprobs")
        && !matches!(logprobs, Value::Bool(false) | Value::Null)
    {
        return Err(RequestRewriteError::new(
            "logprobs is not supported by the chat-to-responses compatibility path.",
            Some("logprobs"),
        ));
    }
    if object.contains_key("top_logprobs") {
        return Err(RequestRewriteError::new(
            "top_logprobs is not supported by the chat-to-responses compatibility path.",
            Some("top_logprobs"),
        ));
    }
    Ok(())
}

fn append_chat_message_to_responses(
    message: &Value,
    instructions: &mut Vec<String>,
    input: &mut Vec<Value>,
) -> Result<(), RequestRewriteError> {
    let object = message.as_object().ok_or_else(|| {
        RequestRewriteError::new("Chat messages must be objects.", Some("messages"))
    })?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| RequestRewriteError::new("Chat messages require role.", Some("messages")))?;

    match role {
        "system" | "developer" => {
            let content = object.get("content").ok_or_else(|| {
                RequestRewriteError::new(
                    "System and developer messages require content.",
                    Some("messages"),
                )
            })?;
            let text = chat_content_to_instruction_text(content)?;
            if !text.trim().is_empty() {
                instructions.push(text);
            }
        }
        "user" | "assistant" => {
            if let Some(content) = object.get("content")
                && !content.is_null()
            {
                let content = chat_content_to_responses_content(content, role)?;
                input.push(json!({
                    "role": role,
                    "content": content,
                }));
            }
            if role == "assistant"
                && let Some(tool_calls) = object.get("tool_calls")
            {
                append_chat_tool_calls_to_responses(tool_calls, input)?;
            }
        }
        "tool" => {
            let call_id = object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|call_id| !call_id.trim().is_empty())
                .ok_or_else(|| {
                    RequestRewriteError::new(
                        "Tool messages require tool_call_id.",
                        Some("messages"),
                    )
                })?;
            let output = object.get("content").cloned().unwrap_or(Value::Null);
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": chat_tool_output_to_string(&output),
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

fn append_chat_tool_calls_to_responses(
    tool_calls: &Value,
    input: &mut Vec<Value>,
) -> Result<(), RequestRewriteError> {
    let tool_calls = tool_calls.as_array().ok_or_else(|| {
        RequestRewriteError::new("assistant tool_calls must be an array.", Some("messages"))
    })?;
    for (index, tool_call) in tool_calls.iter().enumerate() {
        let object = tool_call.as_object().ok_or_else(|| {
            RequestRewriteError::new("assistant tool_calls must be objects.", Some("messages"))
        })?;
        if object
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|tool_type| tool_type != "function")
        {
            return Err(RequestRewriteError::new(
                "Only function tool calls are supported by the chat-to-responses compatibility path.",
                Some("messages"),
            ));
        }
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                RequestRewriteError::new(
                    "assistant function tool calls require function.",
                    Some("messages"),
                )
            })?;
        let name = string_field(function, "name").ok_or_else(|| {
            RequestRewriteError::new(
                "assistant function tool calls require function.name.",
                Some("messages"),
            )
        })?;
        let arguments = string_field(function, "arguments").unwrap_or_else(|| "{}".to_owned());
        let call_id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("call_{index}"));
        input.push(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments,
        }));
    }
    Ok(())
}

fn chat_content_to_instruction_text(content: &Value) -> Result<String, RequestRewriteError> {
    match content {
        Value::String(text) => Ok(text.to_owned()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                let object = part.as_object().ok_or_else(|| {
                    RequestRewriteError::new(
                        "Chat content parts must be objects.",
                        Some("messages"),
                    )
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(part_text) = object.get("text").and_then(Value::as_str) {
                            text.push_str(part_text);
                        }
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

fn chat_content_to_responses_content(
    content: &Value,
    role: &str,
) -> Result<Value, RequestRewriteError> {
    match content {
        Value::String(_) => Ok(content.clone()),
        Value::Array(parts) => {
            let mut responses_parts = Vec::new();
            let mut text_only = true;
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
                        responses_parts.push(json!({
                            "type": if role == "assistant" {
                                "output_text"
                            } else {
                                "input_text"
                            },
                            "text": text,
                        }));
                    }
                    Some("image_url") if role == "user" => {
                        text_only = false;
                        responses_parts.push(chat_image_part_to_responses(object)?);
                    }
                    Some("refusal") if role == "assistant" => {
                        let refusal = object
                            .get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        responses_parts.push(json!({
                            "type": "output_text",
                            "text": refusal,
                        }));
                    }
                    _ => {
                        return Err(RequestRewriteError::new(
                            "Chat content part type is unsupported by the compatibility path.",
                            Some("messages"),
                        ));
                    }
                }
            }

            if text_only {
                let text = responses_parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("");
                Ok(Value::String(text))
            } else {
                Ok(Value::Array(responses_parts))
            }
        }
        Value::Null => Ok(Value::String(String::new())),
        _ => Ok(Value::String(content.to_string())),
    }
}

fn chat_image_part_to_responses(object: &Map<String, Value>) -> Result<Value, RequestRewriteError> {
    let image_url = object.get("image_url").ok_or_else(|| {
        RequestRewriteError::new(
            "Chat image_url content parts require image_url.",
            Some("messages"),
        )
    })?;
    match image_url {
        Value::String(url) => Ok(json!({
            "type": "input_image",
            "image_url": url,
        })),
        Value::Object(image_url) => {
            let url = image_url.get("url").ok_or_else(|| {
                RequestRewriteError::new(
                    "Chat image_url content parts require image_url.url.",
                    Some("messages"),
                )
            })?;
            let mut part = Map::new();
            part.insert("type".to_owned(), Value::String("input_image".to_owned()));
            part.insert("image_url".to_owned(), url.clone());
            if let Some(detail) = image_url.get("detail") {
                part.insert("detail".to_owned(), detail.clone());
            }
            Ok(Value::Object(part))
        }
        _ => Err(RequestRewriteError::new(
            "Chat image_url content image_url must be a string or object.",
            Some("messages"),
        )),
    }
}

fn chat_tool_output_to_string(output: &Value) -> String {
    if let Some(text) = output.as_str() {
        return text.to_owned();
    }
    if let Some(parts) = output.as_array() {
        let text = parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("refusal").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return text;
        }
    }
    output.to_string()
}

fn chat_tools_to_responses_tools(tools: &Value) -> Result<Vec<Value>, RequestRewriteError> {
    let tools = tools.as_array().ok_or_else(|| {
        RequestRewriteError::new(
            "tools must be an array for chat-to-responses conversion.",
            Some("tools"),
        )
    })?;
    let mut responses_tools = Vec::new();
    for tool in tools {
        let object = tool.as_object().ok_or_else(|| {
            RequestRewriteError::new("tool definitions must be objects.", Some("tools"))
        })?;
        if object.get("type").and_then(Value::as_str) != Some("function") {
            return Err(RequestRewriteError::new(
                "Only function tools are supported by the chat-to-responses compatibility path.",
                Some("tools"),
            ));
        }
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                RequestRewriteError::new("function tools require function.", Some("tools"))
            })?;
        let mut responses_tool = function.clone();
        responses_tool.insert("type".to_owned(), Value::String("function".to_owned()));
        responses_tools.push(Value::Object(responses_tool));
    }
    Ok(responses_tools)
}

fn chat_tool_choice_to_responses(tool_choice: &Value) -> Result<Value, RequestRewriteError> {
    if let Some(object) = tool_choice.as_object() {
        if object.get("type").and_then(Value::as_str) != Some("function") {
            return Ok(tool_choice.clone());
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
        let name = function.get("name").cloned().ok_or_else(|| {
            RequestRewriteError::new(
                "function tool_choice requires function.name.",
                Some("tool_choice"),
            )
        })?;
        return Ok(json!({
            "type": "function",
            "name": name,
        }));
    }
    Ok(tool_choice.clone())
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
