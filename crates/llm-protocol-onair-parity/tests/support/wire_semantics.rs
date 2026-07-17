use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct SelectedRequest {
    pub model: String,
    pub stream: bool,
    pub instructions: Vec<String>,
    pub conversation: Vec<ConversationItem>,
    pub tools: Vec<SelectedTool>,
    pub generation: SelectedGeneration,
    pub output_format: Option<Value>,
    pub cache: SelectedCacheIntent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConversationItem {
    Text {
        role: SelectedRole,
        text: String,
    },
    ToolCall {
        role: SelectedRole,
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: Vec<String>,
        is_error: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectedRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectedTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub strict: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectedGeneration {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_output_tokens: Option<u64>,
    pub stop_sequences: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectedCacheIntent {
    pub request_cache_key: Option<String>,
    pub retention: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectedResponse {
    pub model: String,
    pub output: Vec<ResponseItem>,
    pub usage: Option<PortableUsage>,
    pub finish_reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResponseItem {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortableUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub fn parse_chat_request(value: &Value) -> Result<SelectedRequest, String> {
    let object = required_object(value, "Chat request")?;
    let mut instructions = Vec::new();
    let mut conversation = Vec::new();
    for (index, message) in required_array_field(object, "messages", "Chat request")?
        .iter()
        .enumerate()
    {
        let context = format!("Chat message {index}");
        let message = required_object(message, &context)?;
        let role = required_string_field(message, "role", &context)?;
        match role.as_str() {
            "system" | "developer" => instructions.extend(text_fragments(
                message.get("content").unwrap_or(&Value::Null),
                &["text"],
                &context,
            )?),
            "user" | "assistant" => {
                let selected_role = selected_role(&role, &context)?;
                append_text_items(
                    &mut conversation,
                    selected_role,
                    message.get("content").unwrap_or(&Value::Null),
                    &["text"],
                    &context,
                )?;
                if role == "assistant"
                    && let Some(tool_calls) = message.get("tool_calls")
                {
                    append_chat_tool_calls(&mut conversation, tool_calls, &context)?;
                }
            }
            "tool" => {
                let tool_call_id = required_string_field(message, "tool_call_id", &context)?;
                let content = text_fragments(
                    message.get("content").unwrap_or(&Value::Null),
                    &["text"],
                    &context,
                )?;
                conversation.push(ConversationItem::ToolResult {
                    tool_call_id,
                    content,
                    is_error: false,
                });
            }
            _ => return Err(format!("{context} has unsupported role {role:?}")),
        }
    }

    Ok(SelectedRequest {
        model: required_string_field(object, "model", "Chat request")?,
        stream: optional_bool_field(object, "stream", "Chat request")?.unwrap_or(false),
        instructions,
        conversation,
        tools: parse_chat_tools(object.get("tools"))?,
        generation: SelectedGeneration {
            temperature: optional_f64_field(object, "temperature", "Chat request")?,
            top_p: optional_f64_field(object, "top_p", "Chat request")?,
            max_output_tokens: equivalent_optional_u64_fields(
                object,
                &["max_completion_tokens", "max_tokens"],
                "Chat request",
            )?,
            stop_sequences: optional_string_list_field(object, "stop", "Chat request")?,
        },
        output_format: object
            .get("response_format")
            .filter(|value| !value.is_null())
            .cloned(),
        cache: parse_openai_cache(object, "Chat request")?,
    })
}

pub fn parse_responses_request(value: &Value) -> Result<SelectedRequest, String> {
    let object = required_object(value, "Responses request")?;
    let mut instructions = match object.get("instructions") {
        Some(value) => text_fragments(value, &["input_text", "text"], "Responses instructions")?,
        None => Vec::new(),
    };
    let mut conversation = Vec::new();
    for (index, item) in required_array_field(object, "input", "Responses request")?
        .iter()
        .enumerate()
    {
        let context = format!("Responses input item {index}");
        let item = required_object(item, &context)?;
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => conversation.push(ConversationItem::ToolCall {
                role: SelectedRole::Assistant,
                id: required_string_field(item, "call_id", &context)?,
                name: required_string_field(item, "name", &context)?,
                arguments: parse_arguments(
                    item.get("arguments")
                        .ok_or_else(|| format!("{context} requires function-call arguments"))?,
                    &context,
                )?,
            }),
            Some("function_call_output") => conversation.push(ConversationItem::ToolResult {
                tool_call_id: required_string_field(item, "call_id", &context)?,
                content: text_fragments(
                    item.get("output").unwrap_or(&Value::Null),
                    &["input_text", "output_text", "text"],
                    &context,
                )?,
                is_error: false,
            }),
            Some("message") | None => {
                let role = required_string_field(item, "role", &context)?;
                if matches!(role.as_str(), "system" | "developer") {
                    instructions.extend(text_fragments(
                        item.get("content").unwrap_or(&Value::Null),
                        &["input_text", "output_text", "text"],
                        &context,
                    )?);
                } else {
                    let role = selected_role(&role, &context)?;
                    append_text_items(
                        &mut conversation,
                        role,
                        item.get("content").unwrap_or(&Value::Null),
                        &["input_text", "output_text", "text"],
                        &context,
                    )?;
                }
            }
            Some(kind) => return Err(format!("{context} has unsupported type {kind:?}")),
        }
    }

    let output_format = object
        .get("text")
        .and_then(Value::as_object)
        .and_then(|text| text.get("format"))
        .filter(|value| !value.is_null())
        .cloned();

    Ok(SelectedRequest {
        model: required_string_field(object, "model", "Responses request")?,
        stream: optional_bool_field(object, "stream", "Responses request")?.unwrap_or(false),
        instructions,
        conversation,
        tools: parse_responses_tools(object.get("tools"))?,
        generation: SelectedGeneration {
            temperature: optional_f64_field(object, "temperature", "Responses request")?,
            top_p: optional_f64_field(object, "top_p", "Responses request")?,
            max_output_tokens: optional_u64_field(
                object,
                "max_output_tokens",
                "Responses request",
            )?,
            stop_sequences: Vec::new(),
        },
        output_format,
        cache: parse_openai_cache(object, "Responses request")?,
    })
}

pub fn parse_messages_request(value: &Value) -> Result<SelectedRequest, String> {
    let object = required_object(value, "Messages request")?;
    let instructions = match object.get("system") {
        Some(value) => text_fragments(value, &["text"], "Messages system")?,
        None => Vec::new(),
    };
    let mut conversation = Vec::new();
    for (index, message) in required_array_field(object, "messages", "Messages request")?
        .iter()
        .enumerate()
    {
        let context = format!("Messages message {index}");
        let message = required_object(message, &context)?;
        let role = required_string_field(message, "role", &context)?;
        let selected_role = selected_role(&role, &context)?;
        let content = message.get("content").unwrap_or(&Value::Null);
        match content {
            Value::String(text) => conversation.push(ConversationItem::Text {
                role: selected_role,
                text: text.clone(),
            }),
            Value::Null => {}
            Value::Array(parts) => {
                for (part_index, part) in parts.iter().enumerate() {
                    let part_context = format!("{context} content part {part_index}");
                    let part = required_object(part, &part_context)?;
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => conversation.push(ConversationItem::Text {
                            role: selected_role,
                            text: required_string_field(part, "text", &part_context)?,
                        }),
                        Some("tool_use") => conversation.push(ConversationItem::ToolCall {
                            role: require_role(
                                selected_role,
                                SelectedRole::Assistant,
                                "tool_use",
                                &part_context,
                            )?,
                            id: required_string_field(part, "id", &part_context)?,
                            name: required_string_field(part, "name", &part_context)?,
                            arguments: part
                                .get("input")
                                .ok_or_else(|| format!("{part_context} requires input"))?
                                .clone(),
                        }),
                        Some("tool_result") => {
                            require_role(
                                selected_role,
                                SelectedRole::User,
                                "tool_result",
                                &part_context,
                            )?;
                            conversation.push(ConversationItem::ToolResult {
                                tool_call_id: required_string_field(
                                    part,
                                    "tool_use_id",
                                    &part_context,
                                )?,
                                content: text_fragments(
                                    part.get("content").unwrap_or(&Value::Null),
                                    &["text"],
                                    &part_context,
                                )?,
                                is_error: optional_bool_field(part, "is_error", &part_context)?
                                    .unwrap_or(false),
                            });
                        }
                        Some(kind) => {
                            return Err(format!("{part_context} has unsupported type {kind:?}"));
                        }
                        None => return Err(format!("{part_context} requires type")),
                    }
                }
            }
            _ => return Err(format!("{context} content must be text, an array, or null")),
        }
    }

    Ok(SelectedRequest {
        model: required_string_field(object, "model", "Messages request")?,
        stream: optional_bool_field(object, "stream", "Messages request")?.unwrap_or(false),
        instructions,
        conversation,
        tools: parse_messages_tools(object.get("tools"))?,
        generation: SelectedGeneration {
            temperature: optional_f64_field(object, "temperature", "Messages request")?,
            top_p: optional_f64_field(object, "top_p", "Messages request")?,
            max_output_tokens: optional_u64_field(object, "max_tokens", "Messages request")?,
            stop_sequences: optional_string_list_field(
                object,
                "stop_sequences",
                "Messages request",
            )?,
        },
        output_format: None,
        cache: SelectedCacheIntent::default(),
    })
}

pub fn parse_chat_response(value: &Value) -> Result<SelectedResponse, String> {
    let object = required_object(value, "Chat response")?;
    let choices = required_array_field(object, "choices", "Chat response")?;
    if choices.len() != 1 {
        return Err(format!(
            "selected Chat response parity requires one choice, found {}",
            choices.len()
        ));
    }
    let choice = required_object(&choices[0], "Chat response choice")?;
    let message = choice
        .get("message")
        .ok_or_else(|| "Chat response choice requires message".to_owned())?;
    let message = required_object(message, "Chat response message")?;
    if required_string_field(message, "role", "Chat response message")? != "assistant" {
        return Err("selected Chat response requires an assistant message".to_owned());
    }

    let mut output = text_fragments(
        message.get("content").unwrap_or(&Value::Null),
        &["text"],
        "Chat response content",
    )?
    .into_iter()
    .map(ResponseItem::Text)
    .collect::<Vec<_>>();
    if let Some(tool_calls) = message.get("tool_calls") {
        for (index, tool_call) in required_array(tool_calls, "Chat response tool_calls")?
            .iter()
            .enumerate()
        {
            let context = format!("Chat response tool call {index}");
            let tool_call = required_object(tool_call, &context)?;
            let function = required_object_field(tool_call, "function", &context)?;
            output.push(ResponseItem::ToolCall {
                id: required_string_field(tool_call, "id", &context)?,
                name: required_string_field(function, "name", &context)?,
                arguments: parse_arguments(
                    function
                        .get("arguments")
                        .ok_or_else(|| format!("{context} requires arguments"))?,
                    &context,
                )?,
            });
        }
    }

    let usage = match object.get("usage").filter(|value| !value.is_null()) {
        Some(value) => {
            let usage = required_object(value, "Chat response usage")?;
            Some(PortableUsage {
                input_tokens: required_u64_field(usage, "prompt_tokens", "Chat response usage")?,
                output_tokens: required_u64_field(
                    usage,
                    "completion_tokens",
                    "Chat response usage",
                )?,
            })
        }
        None => None,
    };

    Ok(SelectedResponse {
        model: required_string_field(object, "model", "Chat response")?,
        output,
        usage,
        finish_reason: required_string_field(choice, "finish_reason", "Chat response choice")?,
    })
}

fn append_text_items(
    output: &mut Vec<ConversationItem>,
    role: SelectedRole,
    content: &Value,
    allowed_types: &[&str],
    context: &str,
) -> Result<(), String> {
    output.extend(
        text_fragments(content, allowed_types, context)?
            .into_iter()
            .map(|text| ConversationItem::Text { role, text }),
    );
    Ok(())
}

fn append_chat_tool_calls(
    output: &mut Vec<ConversationItem>,
    value: &Value,
    context: &str,
) -> Result<(), String> {
    for (index, tool_call) in required_array(value, &format!("{context} tool_calls"))?
        .iter()
        .enumerate()
    {
        let context = format!("{context} tool call {index}");
        let tool_call = required_object(tool_call, &context)?;
        let function = required_object_field(tool_call, "function", &context)?;
        output.push(ConversationItem::ToolCall {
            role: SelectedRole::Assistant,
            id: required_string_field(tool_call, "id", &context)?,
            name: required_string_field(function, "name", &context)?,
            arguments: parse_arguments(
                function
                    .get("arguments")
                    .ok_or_else(|| format!("{context} requires arguments"))?,
                &context,
            )?,
        });
    }
    Ok(())
}

fn parse_chat_tools(value: Option<&Value>) -> Result<Vec<SelectedTool>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    required_array(value, "Chat tools")?
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let context = format!("Chat tool {index}");
            let tool = required_object(tool, &context)?;
            if required_string_field(tool, "type", &context)? != "function" {
                return Err(format!("{context} must be a function"));
            }
            let function = required_object_field(tool, "function", &context)?;
            parse_tool_fields(function, "parameters", &context)
        })
        .collect()
}

fn parse_responses_tools(value: Option<&Value>) -> Result<Vec<SelectedTool>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    required_array(value, "Responses tools")?
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let context = format!("Responses tool {index}");
            let tool = required_object(tool, &context)?;
            if required_string_field(tool, "type", &context)? != "function" {
                return Err(format!("{context} must be a function"));
            }
            parse_tool_fields(tool, "parameters", &context)
        })
        .collect()
}

fn parse_messages_tools(value: Option<&Value>) -> Result<Vec<SelectedTool>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    required_array(value, "Messages tools")?
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let context = format!("Messages tool {index}");
            let tool = required_object(tool, &context)?;
            parse_tool_fields(tool, "input_schema", &context)
        })
        .collect()
}

fn parse_tool_fields(
    object: &Map<String, Value>,
    schema_field: &str,
    context: &str,
) -> Result<SelectedTool, String> {
    Ok(SelectedTool {
        name: required_string_field(object, "name", context)?,
        description: optional_string_field(object, "description", context)?,
        input_schema: object
            .get(schema_field)
            .ok_or_else(|| format!("{context} requires {schema_field}"))?
            .clone(),
        strict: optional_bool_field(object, "strict", context)?,
    })
}

fn parse_openai_cache(
    object: &Map<String, Value>,
    context: &str,
) -> Result<SelectedCacheIntent, String> {
    Ok(SelectedCacheIntent {
        request_cache_key: optional_string_field(object, "prompt_cache_key", context)?,
        retention: optional_string_field(object, "prompt_cache_retention", context)?,
    })
}

fn selected_role(value: &str, context: &str) -> Result<SelectedRole, String> {
    match value {
        "user" => Ok(SelectedRole::User),
        "assistant" => Ok(SelectedRole::Assistant),
        _ => Err(format!(
            "{context} has unsupported conversational role {value:?}"
        )),
    }
}

fn require_role(
    actual: SelectedRole,
    expected: SelectedRole,
    part_type: &str,
    context: &str,
) -> Result<SelectedRole, String> {
    if actual != expected {
        return Err(format!(
            "{context} has {part_type} under the wrong message role"
        ));
    }
    Ok(actual)
}

fn parse_arguments(value: &Value, context: &str) -> Result<Value, String> {
    match value {
        Value::String(arguments) => serde_json::from_str(arguments)
            .map_err(|error| format!("{context} arguments are not JSON: {error}")),
        _ => Ok(value.clone()),
    }
}

fn text_fragments(
    value: &Value,
    allowed_types: &[&str],
    context: &str,
) -> Result<Vec<String>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![text.clone()]),
        Value::Array(parts) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let context = format!("{context} text part {index}");
                let part = required_object(part, &context)?;
                let kind = required_string_field(part, "type", &context)?;
                if !allowed_types.contains(&kind.as_str()) {
                    return Err(format!("{context} has unsupported type {kind:?}"));
                }
                required_string_field(part, "text", &context)
            })
            .collect(),
        _ => Err(format!("{context} must be text, an array, or null")),
    }
}

fn required_object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{context} must be an object"))
}

fn required_object_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a Map<String, Value>, String> {
    required_object(
        object
            .get(field)
            .ok_or_else(|| format!("{context} requires {field}"))?,
        &format!("{context}.{field}"),
    )
}

fn required_array<'a>(value: &'a Value, context: &str) -> Result<&'a Vec<Value>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{context} must be an array"))
}

fn required_array_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a Vec<Value>, String> {
    required_array(
        object
            .get(field)
            .ok_or_else(|| format!("{context} requires {field}"))?,
        &format!("{context}.{field}"),
    )
}

fn required_string_field(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<String, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{context}.{field} must be a string"))
}

fn optional_string_field(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<Option<String>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{context}.{field} must be a string or null")),
    }
}

fn optional_bool_field(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<Option<bool>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{context}.{field} must be a boolean or null")),
    }
}

fn optional_f64_field(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<Option<f64>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("{context}.{field} must be a number or null")),
    }
}

fn required_u64_field(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<u64, String> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{context}.{field} must be an unsigned integer"))
}

fn optional_u64_field(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<Option<u64>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{context}.{field} must be an unsigned integer or null")),
    }
}

fn equivalent_optional_u64_fields(
    object: &Map<String, Value>,
    fields: &[&str],
    context: &str,
) -> Result<Option<u64>, String> {
    let mut selected = None;
    for field in fields {
        if let Some(value) = optional_u64_field(object, field, context)? {
            if selected.is_some_and(|selected| selected != value) {
                return Err(format!("{context} has conflicting token limits"));
            }
            selected = Some(value);
        }
    }
    Ok(selected)
}

fn optional_string_list_field(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<Vec<String>, String> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(value)) => Ok(vec![value.clone()]),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{context}.{field}[{index}] must be a string"))
            })
            .collect(),
        Some(_) => Err(format!(
            "{context}.{field} must be a string, array, or null"
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn selected_request_projection_detects_wire_mutations() {
        let base = json!({
            "model": "synthetic-model",
            "instructions": "Synthetic system.",
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "Synthetic request."}]
            }],
            "tools": [{
                "type": "function",
                "name": "synthetic_lookup",
                "description": "Synthetic lookup.",
                "parameters": {"type": "object"},
                "strict": true
            }],
            "temperature": 0.2,
            "top_p": 0.9,
            "max_output_tokens": 24
        });
        let expected = parse_responses_request(&base).unwrap();

        for changed in [
            ("/model", json!("changed-model")),
            ("/input/0/content/0/text", json!("Changed request.")),
            ("/tools/0/strict", json!(false)),
            ("/top_p", json!(0.8)),
            ("/max_output_tokens", json!(25)),
        ] {
            let mut value = base.clone();
            *value.pointer_mut(changed.0).unwrap() = changed.1;
            assert_ne!(parse_responses_request(&value).unwrap(), expected);
        }
    }

    #[test]
    fn selected_response_projection_detects_wire_mutations() {
        let base = json!({
            "id": "generated-one",
            "object": "chat.completion",
            "model": "synthetic-model",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Synthetic reply.",
                    "tool_calls": [{
                        "id": "call_synthetic",
                        "type": "function",
                        "function": {
                            "name": "synthetic_lookup",
                            "arguments": "{\"subject\":\"alpha\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7}
        });
        let expected = parse_chat_response(&base).unwrap();

        for changed in [
            ("/model", json!("changed-model")),
            ("/choices/0/message/content", json!("Changed reply.")),
            (
                "/choices/0/message/tool_calls/0/function/arguments",
                json!("{\"subject\":\"changed\"}"),
            ),
            ("/choices/0/finish_reason", json!("stop")),
            ("/usage/prompt_tokens", json!(12)),
        ] {
            let mut value = base.clone();
            *value.pointer_mut(changed.0).unwrap() = changed.1;
            assert_ne!(parse_chat_response(&value).unwrap(), expected);
        }
    }

    #[test]
    fn response_projection_ignores_unclaimed_provider_metadata() {
        let first = json!({
            "id": "generated-one",
            "object": "chat.completion",
            "created": 1,
            "model": "synthetic-model",
            "choices": [{
                "message": {"role": "assistant", "content": "Synthetic reply."},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 11,
                "prompt_tokens_details": {"cached_tokens": 3},
                "completion_tokens": 7,
                "total_tokens": 18
            }
        });
        let mut second = first.clone();
        second["id"] = json!("generated-two");
        second["created"] = json!(2);
        second["usage"]["prompt_tokens_details"]["cached_tokens"] = json!(4);

        assert_eq!(
            parse_chat_response(&first).unwrap(),
            parse_chat_response(&second).unwrap()
        );
    }

    #[test]
    fn messages_projection_rejects_tool_parts_under_the_wrong_role() {
        let request = json!({
            "model": "synthetic-model",
            "max_tokens": 24,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_use",
                    "id": "call_synthetic",
                    "name": "synthetic_lookup",
                    "input": {"subject": "alpha"}
                }]
            }]
        });

        assert!(
            parse_messages_request(&request)
                .unwrap_err()
                .contains("wrong message role")
        );
    }
}
