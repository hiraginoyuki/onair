use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::anthropic_compat::{
    anthropic_message_to_chat_completion, chat_finish_reason_from_anthropic_stop_reason,
};
use super::request::{RequestMode, looks_like_json};

pub fn is_event_stream_content_type(content_type: Option<&str>) -> bool {
    content_type
        .map(|value| value.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false)
}

pub(crate) fn is_json_content_type(content_type: Option<&str>) -> bool {
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
    match request_mode {
        RequestMode::Native => {
            if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
                rewrite_response_models(&mut json, backend_model, public_model);
            }
        }
        RequestMode::AnthropicMessagesNative => {
            if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
                rewrite_anthropic_messages_response_body_value(
                    &mut json,
                    backend_model,
                    public_model,
                );
            }
        }
        RequestMode::ResponsesViaChatCompletions => {
            if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
                rewrite_response_models(&mut json, backend_model, public_model);
            }
            json = chat_completion_to_response(json);
        }
        RequestMode::ChatCompletionsViaResponses => {
            if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
                rewrite_response_models(&mut json, backend_model, public_model);
            }
            json = response_to_chat_completion(json);
        }
        RequestMode::ChatCompletionsViaMessages => {
            if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
                rewrite_anthropic_messages_response_body_value(
                    &mut json,
                    backend_model,
                    public_model,
                );
            }
            json = anthropic_message_to_chat_completion(json);
        }
    }
    ensure_usage_total_tokens(&mut json);

    let rewritten = serde_json::to_vec(&json)
        .expect("rewritten body is always serializable; this is a programmer error");
    (rewritten, usage)
}

/// Rewrite an Anthropic Messages API response body.
///
/// Replaces the top-level `model` field if it matches `backend_model`
/// with `public_model`. Returns the original bytes when the body is
/// not valid JSON or the model field does not need rewriting.
pub fn rewrite_anthropic_messages_response_body(
    body: &[u8],
    backend_model: &str,
    public_model: &str,
) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    rewrite_anthropic_messages_response_body_value(&mut value, backend_model, public_model);
    serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec())
}

/// Rewrite the top-level `model` field in an already-parsed Anthropic
/// Messages API response value. The recursive [`rewrite_response_models`]
/// replaces every `model` key in the tree; this function only touches
/// the top-level field, which is the correct behavior for Anthropic
/// responses where nested objects may legitimately contain `model`
/// fields that should not be rewritten.
fn rewrite_anthropic_messages_response_body_value(
    value: &mut Value,
    backend_model: &str,
    public_model: &str,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if let Some(model) = object.get("model").and_then(Value::as_str)
        && model == backend_model
    {
        object.insert("model".to_owned(), Value::String(public_model.to_owned()));
    }
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
    let fields = usage_fields_from_object(usage.and_then(Value::as_object));
    json!({
        "input_tokens": fields.input,
        "input_tokens_details": {
            "cached_tokens": fields.cached,
        },
        "output_tokens": fields.output,
        "output_tokens_details": {
            "reasoning_tokens": fields.reasoning,
        },
        "total_tokens": fields.total,
    })
}

fn response_to_chat_completion(value: Value) -> Value {
    let Some(object) = value.as_object() else {
        return value;
    };
    let response_id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_unknown");
    let created = object
        .get("created_at")
        .or_else(|| object.get("created"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let model = object
        .get("model")
        .cloned()
        .unwrap_or_else(|| Value::String("unknown".to_owned()));
    let (content, tool_calls) = response_output_to_chat_message(object.get("output"));
    let has_tool_calls = !tool_calls.is_empty();

    let mut message = Map::new();
    message.insert("role".to_owned(), Value::String("assistant".to_owned()));
    message.insert("content".to_owned(), Value::String(content));
    if has_tool_calls {
        message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }

    json!({
        "id": chat_id_from_response(response_id),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": response_finish_reason(object, has_tool_calls),
        }],
        "usage": responses_usage_to_chat_usage(object.get("usage")),
    })
}

fn response_output_to_chat_message(output: Option<&Value>) -> (String, Vec<Value>) {
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let Some(items) = output.and_then(Value::as_array) else {
        return (content, tool_calls);
    };

    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("message") => {
                if object
                    .get("role")
                    .and_then(Value::as_str)
                    .is_none_or(|role| role == "assistant")
                    && let Some(message_content) = object.get("content")
                {
                    content.push_str(&responses_message_content_to_chat_text(message_content));
                }
            }
            Some("function_call") => {
                tool_calls.push(response_function_call_to_chat_tool_call(object, index));
            }
            _ => {}
        }
    }

    (content, tool_calls)
}

fn responses_message_content_to_chat_text(content: &Value) -> String {
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
    String::new()
}

fn response_function_call_to_chat_tool_call(object: &Map<String, Value>, index: usize) -> Value {
    let call_id = object
        .get("call_id")
        .or_else(|| object.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("call_{index}"));
    json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            "arguments": object
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}"),
        },
    })
}

fn response_finish_reason(object: &Map<String, Value>, has_tool_calls: bool) -> &'static str {
    if has_tool_calls {
        return "tool_calls";
    }
    if object.get("status").and_then(Value::as_str) == Some("incomplete") {
        return "length";
    }
    "stop"
}

fn responses_usage_to_chat_usage(usage: Option<&Value>) -> Value {
    let fields = usage_fields_from_object(usage.and_then(Value::as_object));
    json!({
        "prompt_tokens": fields.input,
        "prompt_tokens_details": {
            "cached_tokens": fields.cached,
        },
        "completion_tokens": fields.output,
        "completion_tokens_details": {
            "reasoning_tokens": fields.reasoning,
        },
        "total_tokens": fields.total,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct UsageFields {
    input: u64,
    output: u64,
    cached: u64,
    reasoning: u64,
    total: u64,
}

fn usage_fields_from_object(usage: Option<&Map<String, Value>>) -> UsageFields {
    let Some(usage) = usage else {
        return UsageFields::default();
    };
    let input = number_field(usage, "prompt_tokens").unwrap_or(0)
        + number_field(usage, "input_tokens").unwrap_or(0);
    let output = number_field(usage, "completion_tokens").unwrap_or(0)
        + number_field(usage, "output_tokens").unwrap_or(0);
    let cached = nested_number_field(usage, "prompt_tokens_details", "cached_tokens")
        .or_else(|| nested_number_field(usage, "input_tokens_details", "cached_tokens"))
        .unwrap_or(0);
    let reasoning = nested_number_field(usage, "completion_tokens_details", "reasoning_tokens")
        .or_else(|| nested_number_field(usage, "output_tokens_details", "reasoning_tokens"))
        .unwrap_or(0);
    let total = number_field(usage, "total_tokens").unwrap_or(input + output);
    UsageFields {
        input,
        output,
        cached,
        reasoning,
        total,
    }
}

fn response_id_from_chat(chat_id: &str) -> String {
    if chat_id.starts_with("resp_") {
        chat_id.to_owned()
    } else {
        format!("resp_{chat_id}")
    }
}

fn chat_id_from_response(response_id: &str) -> String {
    if response_id.starts_with("chatcmpl") {
        response_id.to_owned()
    } else {
        format!("chatcmpl_{response_id}")
    }
}

fn message_id_from_response(response_id: &str, index: usize) -> String {
    format!("msg_{response_id}_{index}")
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
    pub total: u64,
}

impl UsageTotals {
    pub fn is_empty(self) -> bool {
        self.input == 0
            && self.cached_input == 0
            && self.output == 0
            && self.reasoning_output == 0
            && self.total == 0
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct UsageDiagnostics {
    pub usage_object_count: u64,
    pub usage_keys: BTreeSet<String>,
    pub event_names: BTreeSet<String>,
    pub usage_event_names: BTreeSet<String>,
}

impl UsageDiagnostics {
    fn observe_object(&mut self, object: &Map<String, Value>) {
        self.usage_object_count += 1;
        self.usage_keys
            .extend(object.keys().filter_map(|key| safe_diagnostic_label(key)));
    }

    fn observe_event_name(&mut self, event_name: &str) {
        if let Some(event_name) = safe_diagnostic_label(event_name) {
            self.event_names.insert(event_name);
        }
    }

    fn observe_usage_event_name(&mut self, event_name: &str) {
        if let Some(event_name) = safe_diagnostic_label(event_name) {
            self.usage_event_names.insert(event_name);
        }
    }

    pub fn merge(&mut self, other: UsageDiagnostics) {
        self.usage_object_count += other.usage_object_count;
        self.usage_keys.extend(other.usage_keys);
        self.event_names.extend(other.event_names);
        self.usage_event_names.extend(other.usage_event_names);
    }
}

#[derive(Debug, Default, Clone)]
pub struct UsageObservation {
    pub totals: UsageTotals,
    pub diagnostics: UsageDiagnostics,
}

pub(crate) fn extract_usage(value: &Value) -> UsageTotals {
    extract_usage_observation(value).totals
}

pub(crate) fn extract_usage_observation(value: &Value) -> UsageObservation {
    let mut observation = UsageObservation::default();
    collect_usage(value, &mut observation);
    observation
}

fn collect_usage(value: &Value, observation: &mut UsageObservation) {
    match value {
        Value::Object(object) => {
            if let Some(usage) = object.get("usage").and_then(Value::as_object) {
                observation.diagnostics.observe_object(usage);
                add_usage_object(usage, &mut observation.totals);
            }
            for value in object.values() {
                collect_usage(value, observation);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_usage(value, observation);
            }
        }
        _ => {}
    }
}

fn add_usage_object(object: &Map<String, Value>, totals: &mut UsageTotals) {
    let fields = usage_fields_from_object(Some(object));
    totals.input += fields.input;
    totals.cached_input += fields.cached;
    totals.output += fields.output;
    totals.reasoning_output += fields.reasoning;
    totals.total += fields.total;
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

fn safe_diagnostic_label(value: &str) -> Option<String> {
    let label = value
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':') {
                Some(character)
            } else if character.is_ascii_graphic() {
                Some('_')
            } else {
                None
            }
        })
        .take(80)
        .collect::<String>();
    (!label.is_empty()).then_some(label)
}

fn sse_field_value<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let value = line.strip_prefix(prefix)?;
    Some(value.strip_prefix(b" ").unwrap_or(value))
}

fn utf8_field_value(line: &[u8], prefix: &[u8]) -> Option<String> {
    let value = sse_field_value(line, prefix)?;
    std::str::from_utf8(value)
        .ok()
        .map(str::trim)
        .and_then(safe_diagnostic_label)
}

fn json_event_type(value: &Value) -> Option<&str> {
    value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| value.get("object").and_then(Value::as_str))
}

/// A single SSE frame yielded by [`SseLineParser`].
///
/// `line` carries the original line bytes (including the trailing
/// `\n` when the parser saw one, and any preceding `\r`) so the
/// caller can re-emit the raw line for `PassThrough`, `Event`, and
/// unparseable `data:` lines.
#[derive(Debug)]
pub enum SseFrame {
    /// `event: <name>` line. The caller decides whether to re-emit
    /// the raw line.
    Event { name: String, line: Vec<u8> },
    /// `data: <json>` line whose value parsed as JSON.
    Data {
        value: Value,
        leading_space: bool,
        line: Vec<u8>,
    },
    /// `data:` line whose value was empty, malformed, or otherwise
    /// not parseable as JSON.
    DataUnparseable { line: Vec<u8> },
    /// `data: [DONE]` line.
    Done { line: Vec<u8> },
    /// Any other line (blank, comment, or non-`data:` non-`event:`).
    PassThrough { line: Vec<u8> },
}

/// Splits a byte stream into [`SseFrame`]s on `\n`.
///
/// The three SSE normalizers all share the same line-splitting +
/// field-extraction logic, so the parser is factored out. The
/// per-type emission strategy is intentionally kept inside each
/// normalizer; see `SseNormalizer`, `ResponsesSseNormalizer`, and
/// `ChatCompletionsSseNormalizer`. The shared `data:`-frame
/// prologue (usage extraction, diagnostics, model rewrite,
/// `ensure_usage_total_tokens`) is factored into
/// [`apply_data_prologue`].
#[derive(Debug, Default)]
pub struct SseLineParser {
    pending: Vec<u8>,
}

impl SseLineParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.pending.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=position).collect();
            frames.push(parse_sse_line(&line));
        }
        frames
    }

    pub fn finish(&mut self) -> Option<SseFrame> {
        if self.pending.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.pending);
        Some(parse_sse_line(&line))
    }
}

fn parse_sse_line(line: &[u8]) -> SseFrame {
    let line_without_newline = line.strip_suffix(b"\n").unwrap_or(line);
    let line_without_cr = line_without_newline
        .strip_suffix(b"\r")
        .unwrap_or(line_without_newline);
    if let Some(event_name) = utf8_field_value(line_without_cr, b"event:") {
        return SseFrame::Event {
            name: event_name,
            line: line.to_vec(),
        };
    }
    if let Some(data) = sse_field_value(line_without_cr, b"data:") {
        let leading_space = line_without_cr
            .strip_prefix(b"data:")
            .is_some_and(|data| data.starts_with(b" "));
        if data == b"[DONE]" {
            return SseFrame::Done {
                line: line.to_vec(),
            };
        }
        if let Ok(value) = serde_json::from_slice::<Value>(data) {
            return SseFrame::Data {
                value,
                leading_space,
                line: line.to_vec(),
            };
        }
        return SseFrame::DataUnparseable {
            line: line.to_vec(),
        };
    }
    SseFrame::PassThrough {
        line: line.to_vec(),
    }
}

fn line_suffix(line: &[u8]) -> (&[u8], &[u8]) {
    let line_ending = if line.ends_with(b"\n") {
        b"\n".as_slice()
    } else {
        b"".as_slice()
    };
    let line_without_newline = line.strip_suffix(b"\n").unwrap_or(line);
    let cr = if line_without_newline.ends_with(b"\r") {
        b"\r".as_slice()
    } else {
        b"".as_slice()
    };
    (cr, line_ending)
}

/// Result of running the shared SSE `data:`-frame prologue.
///
/// All three normalizers (`SseNormalizer`, `ResponsesSseNormalizer`,
/// `ChatCompletionsSseNormalizer`) share the same opening moves on
/// a parsed JSON data value: usage extraction, event-name diagnostics,
/// model rewrite, and `ensure_usage_total_tokens`. The function
/// [`apply_data_prologue`] runs them and returns the per-call
/// summary that the protocol-specific strategies consult to decide
/// what to emit.
pub(crate) struct DataPrologue {
    pub usage_object_count: u64,
}

/// Shared opening moves for an SSE `data:`-frame.
///
/// Mutates `value` in place (model rewrite, `ensure_usage_total_tokens`).
/// Updates `usage` and `diagnostics` with the extracted usage
/// totals. Clears `pending_event_name` once the data frame has
/// been attributed. Returns the [`DataPrologue`] summary the
/// strategy needs to decide what to emit.
///
/// The per-strategy work happens after this call returns; the
/// three normalizers differ only in what they emit, not in what
/// they observe about the data.
pub(crate) fn apply_data_prologue(
    value: &mut Value,
    pending_event_name: &mut Option<String>,
    usage: &mut UsageTotals,
    diagnostics: &mut UsageDiagnostics,
    backend_model: Option<&str>,
    public_model: Option<&str>,
) -> DataPrologue {
    let observation = extract_usage_observation(value);
    let usage_object_count = observation.diagnostics.usage_object_count;
    let json_event_name = json_event_type(value).and_then(safe_diagnostic_label);
    if let Some(event_name) = &json_event_name {
        diagnostics.observe_event_name(event_name);
    }
    if usage_object_count > 0 {
        if let Some(event_name) = &*pending_event_name {
            diagnostics.observe_usage_event_name(event_name);
        }
        if let Some(event_name) = &json_event_name {
            diagnostics.observe_usage_event_name(event_name);
        }
    }
    *pending_event_name = None;
    usage.input += observation.totals.input;
    usage.cached_input += observation.totals.cached_input;
    usage.output += observation.totals.output;
    usage.reasoning_output += observation.totals.reasoning_output;
    usage.total += observation.totals.total;
    diagnostics.merge(observation.diagnostics);
    if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
        rewrite_response_models(value, backend_model, public_model);
    }
    ensure_usage_total_tokens(value);
    DataPrologue { usage_object_count }
}

#[derive(Debug, Default)]
pub struct SseNormalizer {
    parser: SseLineParser,
    pub usage: UsageTotals,
    pub diagnostics: UsageDiagnostics,
    pending_event_name: Option<String>,
    backend_model: Option<String>,
    public_model: Option<String>,
    emit_usage_to_client: bool,
}

impl SseNormalizer {
    pub fn new_with_usage_visibility(
        backend_model: Option<String>,
        public_model: Option<String>,
        emit_usage_to_client: bool,
    ) -> Self {
        Self {
            backend_model,
            public_model,
            emit_usage_to_client,
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.parser.push(chunk);
        let mut output = Vec::with_capacity(chunk.len());
        for frame in frames {
            self.handle_frame(frame, &mut output);
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        if let Some(frame) = self.parser.finish() {
            self.handle_frame(frame, &mut output);
        }
        output
    }

    fn handle_frame(&mut self, frame: SseFrame, output: &mut Vec<u8>) {
        match frame {
            SseFrame::Event { name, line } => {
                self.diagnostics.observe_event_name(&name);
                self.pending_event_name = Some(name);
                output.extend_from_slice(&line);
            }
            SseFrame::Data {
                value,
                leading_space,
                line,
            } => {
                self.handle_data(value, leading_space, &line, output);
            }
            SseFrame::DataUnparseable { line } => {
                self.pending_event_name = None;
                output.extend_from_slice(&line);
            }
            SseFrame::Done { line } => {
                self.pending_event_name = None;
                output.extend_from_slice(&line);
            }
            SseFrame::PassThrough { line } => {
                output.extend_from_slice(&line);
            }
        }
    }

    fn handle_data(
        &mut self,
        mut json: Value,
        leading_space: bool,
        line: &[u8],
        output: &mut Vec<u8>,
    ) {
        let prologue = apply_data_prologue(
            &mut json,
            &mut self.pending_event_name,
            &mut self.usage,
            &mut self.diagnostics,
            self.backend_model.as_deref(),
            self.public_model.as_deref(),
        );
        if !self.emit_usage_to_client && prologue.usage_object_count > 0 {
            if chat_usage_only_chunk(&json) {
                return;
            }
            remove_usage_field(&mut json);
        }

        let normalized = serde_json::to_vec(&json)
            .expect("normalized SSE data is always serializable; this is a programmer error");
        let (cr, line_ending) = line_suffix(line);
        let mut rewritten = Vec::with_capacity(line.len() + normalized.len());
        rewritten.extend_from_slice(b"data:");
        if leading_space {
            rewritten.extend_from_slice(b" ");
        }
        rewritten.extend_from_slice(&normalized);
        rewritten.extend_from_slice(cr);
        rewritten.extend_from_slice(line_ending);
        output.extend_from_slice(&rewritten);
    }
}

fn chat_usage_only_chunk(value: &Value) -> bool {
    value.get("usage").is_some()
        && value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
}

fn remove_usage_field(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("usage");
    }
}

#[derive(Debug, Default)]
pub struct ResponsesSseNormalizer {
    parser: SseLineParser,
    pub usage: UsageTotals,
    pub diagnostics: UsageDiagnostics,
    pending_event_name: Option<String>,
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
        let frames = self.parser.push(chunk);
        let mut output = Vec::new();
        for frame in frames {
            self.handle_frame(frame, &mut output);
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        if let Some(frame) = self.parser.finish() {
            self.handle_frame(frame, &mut output);
        }
        output.extend(self.finish_response());
        output
    }

    fn handle_frame(&mut self, frame: SseFrame, output: &mut Vec<u8>) {
        match frame {
            SseFrame::Event { name, line } => {
                self.diagnostics.observe_event_name(&name);
                self.pending_event_name = Some(name);
                output.extend_from_slice(&line);
            }
            SseFrame::Data { value, .. } => {
                self.handle_data(value, output);
            }
            SseFrame::DataUnparseable { line } => {
                self.pending_event_name = None;
                output.extend_from_slice(&line);
            }
            SseFrame::Done { line } => {
                self.pending_event_name = None;
                output.extend(self.finish_response());
                output.extend_from_slice(&line);
            }
            SseFrame::PassThrough { line } => {
                output.extend_from_slice(&line);
            }
        }
    }

    fn handle_data(&mut self, mut chunk: Value, output: &mut Vec<u8>) {
        apply_data_prologue(
            &mut chunk,
            &mut self.pending_event_name,
            &mut self.usage,
            &mut self.diagnostics,
            self.backend_model.as_deref(),
            self.public_model.as_deref(),
        );
        output.extend(self.process_chat_chunk(&chunk));
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
                "output_tokens_details": {
                    "reasoning_tokens": self.usage.reasoning_output,
                },
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

#[derive(Debug, Default)]
pub struct ChatCompletionsSseNormalizer {
    parser: SseLineParser,
    pub usage: UsageTotals,
    pub diagnostics: UsageDiagnostics,
    pending_event_name: Option<String>,
    backend_model: Option<String>,
    public_model: Option<String>,
    emit_usage_to_client: bool,
    response_id: Option<String>,
    created_at: u64,
    model: Option<String>,
    role_sent: bool,
    text_sent: bool,
    tool_calls: BTreeMap<usize, ChatCompletionStreamToolCall>,
    completed: bool,
    done_sent: bool,
}

#[derive(Debug, Default)]
struct ChatCompletionStreamToolCall {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    announced: bool,
}

impl ChatCompletionsSseNormalizer {
    pub fn new_with_usage_visibility(
        backend_model: Option<String>,
        public_model: Option<String>,
        emit_usage_to_client: bool,
    ) -> Self {
        Self {
            backend_model,
            public_model,
            emit_usage_to_client,
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.parser.push(chunk);
        let mut output = Vec::new();
        for frame in frames {
            self.handle_frame(frame, &mut output);
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        if let Some(frame) = self.parser.finish() {
            self.handle_frame(frame, &mut output);
        }
        if !self.completed {
            output.extend(self.finish_response(None));
        }
        output.extend(self.done_event());
        output
    }

    fn handle_frame(&mut self, frame: SseFrame, output: &mut Vec<u8>) {
        match frame {
            SseFrame::Event { name, .. } => {
                self.diagnostics.observe_event_name(&name);
                self.pending_event_name = Some(name);
            }
            SseFrame::Data { value, .. } => {
                self.handle_data(value, output);
            }
            SseFrame::DataUnparseable { .. } => {
                self.pending_event_name = None;
            }
            SseFrame::Done { .. } => {
                self.pending_event_name = None;
                if !self.completed {
                    output.extend(self.finish_response(None));
                }
                output.extend(self.done_event());
            }
            SseFrame::PassThrough { .. } => {}
        }
    }

    fn handle_data(&mut self, mut event: Value, output: &mut Vec<u8>) {
        apply_data_prologue(
            &mut event,
            &mut self.pending_event_name,
            &mut self.usage,
            &mut self.diagnostics,
            self.backend_model.as_deref(),
            self.public_model.as_deref(),
        );
        output.extend(self.process_response_event(&event));
    }

    fn process_response_event(&mut self, event: &Value) -> Vec<u8> {
        self.set_event_metadata(event);
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => self.process_text_delta(event),
            Some("response.function_call_arguments.delta") => {
                self.process_function_arguments_delta(event)
            }
            Some("response.output_item.added") => self.process_output_item(event, false),
            Some("response.output_item.done") => self.process_output_item(event, true),
            Some("response.completed") | Some("response.incomplete") => {
                let response = event.get("response");
                let mut output = self.emit_completed_output_if_needed(response);
                output.extend(self.finish_response(response));
                output
            }
            Some("response.failed") | Some("response.cancelled") | Some("error") => {
                self.fail_response(event.get("response"))
            }
            _ => Vec::new(),
        }
    }

    fn set_event_metadata(&mut self, event: &Value) {
        if let Some(response) = event.get("response") {
            self.set_response_metadata(response);
        }
        if self.response_id.is_none() {
            self.response_id = event
                .get("response_id")
                .or_else(|| event.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if self.created_at == 0 {
            self.created_at = event
                .get("created_at")
                .or_else(|| event.get("created"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if self.model.is_none() {
            self.model = event
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }

    fn set_response_metadata(&mut self, response: &Value) {
        if self.response_id.is_none() {
            self.response_id = response
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if self.created_at == 0 {
            self.created_at = response
                .get("created_at")
                .or_else(|| response.get("created"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if self.model.is_none() {
            self.model = response
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }

    fn process_text_delta(&mut self, event: &Value) -> Vec<u8> {
        let Some(delta) = event.get("delta").and_then(Value::as_str) else {
            return Vec::new();
        };
        if delta.is_empty() {
            return Vec::new();
        }
        self.text_sent = true;
        let mut delta_object = Map::new();
        if !self.role_sent {
            self.role_sent = true;
            delta_object.insert("role".to_owned(), Value::String("assistant".to_owned()));
        }
        delta_object.insert("content".to_owned(), Value::String(delta.to_owned()));
        self.chat_chunk(Value::Object(delta_object), None)
    }

    fn process_function_arguments_delta(&mut self, event: &Value) -> Vec<u8> {
        let index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(self.tool_calls.len());
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let call_id = event
            .get("call_id")
            .or_else(|| event.get("item_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        let state = self.tool_calls.entry(index).or_default();
        if let Some(call_id) = call_id {
            state.call_id.get_or_insert(call_id);
        }
        if !delta.is_empty() {
            state.arguments.push_str(&delta);
        }

        let mut output = Vec::new();
        output.extend(self.announce_tool_call_if_ready(index));
        if !delta.is_empty() {
            output.extend(self.chat_chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "function": {
                            "arguments": delta,
                        },
                    }],
                }),
                None,
            ));
        }
        output
    }

    fn process_output_item(&mut self, event: &Value, done: bool) -> Vec<u8> {
        let Some(item) = event.get("item").and_then(Value::as_object) else {
            return Vec::new();
        };
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => self.process_function_item(event, item, done),
            Some("message") if done && !self.text_sent => {
                let Some(content) = item.get("content") else {
                    return Vec::new();
                };
                let text = responses_message_content_to_chat_text(content);
                if text.is_empty() {
                    return Vec::new();
                }
                self.text_sent = true;
                let mut delta_object = Map::new();
                if !self.role_sent {
                    self.role_sent = true;
                    delta_object.insert("role".to_owned(), Value::String("assistant".to_owned()));
                }
                delta_object.insert("content".to_owned(), Value::String(text));
                self.chat_chunk(Value::Object(delta_object), None)
            }
            _ => Vec::new(),
        }
    }

    fn process_function_item(
        &mut self,
        event: &Value,
        item: &Map<String, Value>,
        done: bool,
    ) -> Vec<u8> {
        let index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(self.tool_calls.len());
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let name = item.get("name").and_then(Value::as_str).map(str::to_owned);
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let emit_arguments = {
            let state = self.tool_calls.entry(index).or_default();
            if let Some(call_id) = call_id {
                state.call_id.get_or_insert(call_id);
            }
            if let Some(name) = name {
                state.name.get_or_insert(name);
            }
            if done && !arguments.is_empty() && state.arguments.is_empty() {
                state.arguments.push_str(&arguments);
                Some(arguments)
            } else {
                None
            }
        };

        let mut output = self.announce_tool_call_if_ready(index);
        if let Some(arguments) = emit_arguments {
            output.extend(self.chat_chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "function": {
                            "arguments": arguments,
                        },
                    }],
                }),
                None,
            ));
        }
        output
    }

    fn announce_tool_call_if_ready(&mut self, index: usize) -> Vec<u8> {
        let Some(state) = self.tool_calls.get_mut(&index) else {
            return Vec::new();
        };
        if state.announced {
            return Vec::new();
        }
        let Some(name) = state.name.clone() else {
            return Vec::new();
        };
        state.announced = true;
        let call_id = state
            .call_id
            .clone()
            .unwrap_or_else(|| format!("call_{index}"));
        let mut delta = Map::new();
        if !self.role_sent {
            self.role_sent = true;
            delta.insert("role".to_owned(), Value::String("assistant".to_owned()));
        }
        delta.insert(
            "tool_calls".to_owned(),
            json!([{
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": "",
                },
            }]),
        );
        self.chat_chunk(Value::Object(delta), None)
    }

    fn emit_completed_output_if_needed(&mut self, response: Option<&Value>) -> Vec<u8> {
        if self.text_sent || !self.tool_calls.is_empty() {
            return Vec::new();
        }
        let Some(response) = response else {
            return Vec::new();
        };
        let (content, tool_calls) = response_output_to_chat_message(response.get("output"));
        let mut output = Vec::new();
        if !content.is_empty() {
            self.text_sent = true;
            let mut delta = Map::new();
            if !self.role_sent {
                self.role_sent = true;
                delta.insert("role".to_owned(), Value::String("assistant".to_owned()));
            }
            delta.insert("content".to_owned(), Value::String(content));
            output.extend(self.chat_chunk(Value::Object(delta), None));
        }
        for (index, tool_call) in tool_calls.into_iter().enumerate() {
            let call_id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let function = tool_call.get("function").and_then(Value::as_object);
            let arguments = {
                let state = self.tool_calls.entry(index).or_default();
                state.call_id = call_id;
                state.name = function
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                state.arguments = function
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                state.arguments.clone()
            };
            output.extend(self.announce_tool_call_if_ready(index));
            if !arguments.is_empty() {
                output.extend(self.chat_chunk(
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "function": {
                                "arguments": arguments,
                            },
                        }],
                    }),
                    None,
                ));
            }
        }
        output
    }

    fn finish_response(&mut self, response: Option<&Value>) -> Vec<u8> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        if let Some(response) = response {
            self.set_response_metadata(response);
        }
        let finish_reason = response
            .and_then(Value::as_object)
            .map(|object| response_finish_reason(object, !self.tool_calls.is_empty()))
            .unwrap_or_else(|| {
                if self.tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                }
            });
        let mut output = self.chat_chunk(json!({}), Some(finish_reason));
        if self.emit_usage_to_client
            && let Some(usage) = response
                .and_then(|response| response.get("usage"))
                .filter(|usage| usage.as_object().is_some())
        {
            output.extend(self.chat_usage_chunk(responses_usage_to_chat_usage(Some(usage))));
        }
        output
    }

    fn fail_response(&mut self, response: Option<&Value>) -> Vec<u8> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        if let Some(response) = response {
            self.set_response_metadata(response);
        }
        let mut output = sse_data(json!({
            "error": {
                "message": "The selected model could not complete the request.",
                "type": "server_error",
                "param": null,
                "code": "upstream_error",
            }
        }));
        output.extend(self.done_event());
        output
    }

    fn chat_chunk(&self, delta: Value, finish_reason: Option<&str>) -> Vec<u8> {
        let mut chunk = Map::new();
        chunk.insert(
            "id".to_owned(),
            Value::String(chat_id_from_response(&self.response_id())),
        );
        chunk.insert(
            "object".to_owned(),
            Value::String("chat.completion.chunk".to_owned()),
        );
        chunk.insert("created".to_owned(), Value::Number(self.created_at.into()));
        chunk.insert("model".to_owned(), Value::String(self.model()));
        chunk.insert(
            "choices".to_owned(),
            json!([{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]),
        );
        if self.emit_usage_to_client {
            chunk.insert("usage".to_owned(), Value::Null);
        }
        sse_data(Value::Object(chunk))
    }

    fn chat_usage_chunk(&self, usage: Value) -> Vec<u8> {
        sse_data(json!({
            "id": chat_id_from_response(&self.response_id()),
            "object": "chat.completion.chunk",
            "created": self.created_at,
            "model": self.model(),
            "choices": [],
            "usage": usage,
        }))
    }

    fn done_event(&mut self) -> Vec<u8> {
        if self.done_sent {
            return Vec::new();
        }
        self.done_sent = true;
        b"data: [DONE]\n\n".to_vec()
    }

    fn response_id(&self) -> String {
        self.response_id
            .clone()
            .unwrap_or_else(|| "resp_unknown".to_owned())
    }

    fn model(&self) -> String {
        self.model.clone().unwrap_or_else(|| "unknown".to_owned())
    }
}

fn sse_data(data: Value) -> Vec<u8> {
    let data = serde_json::to_vec(&data)
        .expect("SSE data is always serializable; this is a programmer error");
    let mut output = Vec::with_capacity(data.len() + 8);
    output.extend_from_slice(b"data: ");
    output.extend_from_slice(&data);
    output.extend_from_slice(b"\n\n");
    output
}

/// Anthropic Messages API SSE normalizer.
///
/// Passes through the stream mostly unchanged, only rewriting
/// the `model` field when it matches `backend_model` to `public_model`.
/// Checks `event["model"]` and `event["message"]["model"]` at the top
/// level; all other fields are preserved exactly.
#[derive(Debug, Default)]
pub struct AnthropicSseNormalizer {
    parser: SseLineParser,
    backend_model: Option<String>,
    public_model: Option<String>,
    pending_event_name: Option<String>,
}

impl AnthropicSseNormalizer {
    pub fn new(backend_model: Option<String>, public_model: Option<String>) -> Self {
        Self {
            backend_model,
            public_model,
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.parser.push(chunk);
        let mut output = Vec::with_capacity(chunk.len());
        for frame in frames {
            self.handle_frame(frame, &mut output);
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        if let Some(frame) = self.parser.finish() {
            self.handle_frame(frame, &mut output);
        }
        output
    }

    fn handle_frame(&mut self, frame: SseFrame, output: &mut Vec<u8>) {
        match frame {
            SseFrame::Event { name, line } => {
                self.pending_event_name = Some(name);
                output.extend_from_slice(&line);
            }
            SseFrame::Data {
                value,
                leading_space,
                line,
            } => {
                let mut value = value;
                self.rewrite_model(&mut value);
                let normalized = serde_json::to_vec(&value).expect(
                    "rewritten Anthropic SSE data is always serializable; this is a programmer error",
                );
                let (cr, line_ending) = line_suffix(&line);
                let mut rewritten = Vec::with_capacity(line.len() + normalized.len());
                rewritten.extend_from_slice(b"data:");
                if leading_space {
                    rewritten.extend_from_slice(b" ");
                }
                rewritten.extend_from_slice(&normalized);
                rewritten.extend_from_slice(cr);
                rewritten.extend_from_slice(line_ending);
                self.pending_event_name = None;
                output.extend_from_slice(&rewritten);
            }
            SseFrame::DataUnparseable { line } => {
                self.pending_event_name = None;
                output.extend_from_slice(&line);
            }
            SseFrame::Done { line } => {
                self.pending_event_name = None;
                output.extend_from_slice(&line);
            }
            SseFrame::PassThrough { line } => {
                output.extend_from_slice(&line);
            }
        }
    }

    /// Rewrite `event["model"]` and `event["message"]["model"]` if they
    /// match `backend_model`, replacing with `public_model`.
    fn rewrite_model(&self, value: &mut Value) {
        let (Some(backend), Some(public)) =
            (self.backend_model.as_deref(), self.public_model.as_deref())
        else {
            return;
        };
        if let Some(object) = value.as_object_mut() {
            if object.get("model").and_then(Value::as_str) == Some(backend) {
                object.insert("model".to_owned(), Value::String(public.to_owned()));
            }
            if let Some(message) = object.get_mut("message").and_then(Value::as_object_mut)
                && message.get("model").and_then(Value::as_str) == Some(backend)
            {
                message.insert("model".to_owned(), Value::String(public.to_owned()));
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct AnthropicMessagesToChatSseNormalizer {
    parser: SseLineParser,
    pub usage: UsageTotals,
    pub diagnostics: UsageDiagnostics,
    pending_event_name: Option<String>,
    backend_model: Option<String>,
    public_model: Option<String>,
    emit_usage_to_client: bool,
    message_id: Option<String>,
    model: Option<String>,
    role_sent: bool,
    content_started: bool,
    tool_calls: BTreeMap<usize, ChatCompletionStreamToolCall>,
    finish_reason: Option<&'static str>,
    completed: bool,
    done_sent: bool,
}

impl AnthropicMessagesToChatSseNormalizer {
    pub fn new_with_usage_visibility(
        backend_model: Option<String>,
        public_model: Option<String>,
        emit_usage_to_client: bool,
    ) -> Self {
        Self {
            backend_model,
            public_model,
            emit_usage_to_client,
            ..Self::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let frames = self.parser.push(chunk);
        let mut output = Vec::new();
        for frame in frames {
            self.handle_frame(frame, &mut output);
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        if let Some(frame) = self.parser.finish() {
            self.handle_frame(frame, &mut output);
        }
        if self.message_id.is_some() && !self.completed {
            output.extend(self.finish_response());
        }
        output.extend(self.done_event());
        output
    }

    fn handle_frame(&mut self, frame: SseFrame, output: &mut Vec<u8>) {
        match frame {
            SseFrame::Event { name, .. } => {
                self.diagnostics.observe_event_name(&name);
                self.pending_event_name = Some(name);
            }
            SseFrame::Data { value, .. } => {
                self.handle_data(value, output);
            }
            SseFrame::DataUnparseable { .. } => {
                self.pending_event_name = None;
            }
            SseFrame::Done { .. } => {
                self.pending_event_name = None;
                if self.message_id.is_some() && !self.completed {
                    output.extend(self.finish_response());
                }
                output.extend(self.done_event());
            }
            SseFrame::PassThrough { .. } => {}
        }
    }

    fn handle_data(&mut self, mut event: Value, output: &mut Vec<u8>) {
        if let (Some(backend_model), Some(public_model)) =
            (self.backend_model.as_deref(), self.public_model.as_deref())
        {
            rewrite_anthropic_messages_response_body_value(&mut event, backend_model, public_model);
        }

        if let Some(message) = event.get("message") {
            if self.message_id.is_none() {
                self.message_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| chat_id_from_response(id));
            }
            if self.model.is_none() {
                self.model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
        if self.model.is_none() {
            self.model = event
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if let (Some(backend_model), Some(public_model), Some(model)) = (
            self.backend_model.as_deref(),
            self.public_model.as_deref(),
            self.model.as_deref(),
        ) && model == backend_model
        {
            self.model = Some(public_model.to_owned());
        }

        self.observe_usage(&event);
        output.extend(self.process_event(&event));
    }

    fn process_event(&mut self, event: &Value) -> Vec<u8> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => Vec::new(),
            Some("content_block_start") => self.process_content_block_start(event),
            Some("content_block_delta") => self.process_content_block_delta(event),
            Some("message_delta") => {
                self.process_message_delta(event);
                Vec::new()
            }
            Some("message_stop") => self.finish_response(),
            Some("error") => self.error_frame(event),
            _ => Vec::new(),
        }
    }

    fn process_content_block_start(&mut self, event: &Value) -> Vec<u8> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(self.tool_calls.len());
        let Some(content_block) = event.get("content_block").and_then(Value::as_object) else {
            return Vec::new();
        };

        match content_block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let state = self.tool_calls.entry(index).or_default();
                state.call_id = content_block
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| Some(format!("call_{index}")));
                state.name = content_block
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.announce_tool_call_if_ready(index)
            }
            Some("text") => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn process_content_block_delta(&mut self, event: &Value) -> Vec<u8> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(self.tool_calls.len());
        let Some(delta) = event.get("delta").and_then(Value::as_object) else {
            return Vec::new();
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    return Vec::new();
                }
                self.content_started = true;
                let mut delta_object = Map::new();
                if !self.role_sent {
                    self.role_sent = true;
                    delta_object.insert("role".to_owned(), Value::String("assistant".to_owned()));
                }
                delta_object.insert("content".to_owned(), Value::String(text.to_owned()));
                self.chat_chunk(Value::Object(delta_object), None)
            }
            Some("input_json_delta") => {
                let partial_json = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if partial_json.is_empty() {
                    return Vec::new();
                }
                let state = self.tool_calls.entry(index).or_default();
                state.arguments.push_str(partial_json);
                let mut output = self.announce_tool_call_if_ready(index);
                output.extend(self.chat_chunk(
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "function": {
                                "arguments": partial_json,
                            },
                        }],
                    }),
                    None,
                ));
                output
            }
            _ => Vec::new(),
        }
    }

    fn process_message_delta(&mut self, event: &Value) {
        if let Some(stop_reason) = event
            .get("delta")
            .and_then(Value::as_object)
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
        {
            self.finish_reason = Some(chat_finish_reason_from_anthropic_stop_reason(stop_reason));
        }
    }

    fn announce_tool_call_if_ready(&mut self, index: usize) -> Vec<u8> {
        let Some(state) = self.tool_calls.get_mut(&index) else {
            return Vec::new();
        };
        if state.announced {
            return Vec::new();
        }
        let Some(name) = state.name.clone() else {
            return Vec::new();
        };
        state.announced = true;
        let call_id = state
            .call_id
            .clone()
            .unwrap_or_else(|| format!("call_{index}"));
        let mut delta = Map::new();
        if !self.role_sent {
            self.role_sent = true;
            delta.insert("role".to_owned(), Value::String("assistant".to_owned()));
        }
        delta.insert(
            "tool_calls".to_owned(),
            json!([{
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": "",
                },
            }]),
        );
        self.chat_chunk(Value::Object(delta), None)
    }

    fn finish_response(&mut self) -> Vec<u8> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        let finish_reason = self.finish_reason.unwrap_or_else(|| {
            if self.tool_calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
        });
        let mut output = self.chat_chunk(json!({}), Some(finish_reason));
        if self.emit_usage_to_client && self.diagnostics.usage_object_count > 0 {
            output.extend(self.chat_usage_chunk());
        }
        output.extend(self.done_event());
        output
    }

    fn error_frame(&mut self, event: &Value) -> Vec<u8> {
        self.completed = true;
        let message = event
            .get("error")
            .and_then(Value::as_object)
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("The selected model could not complete the request.");
        let mut output = sse_data(json!({
            "error": {
                "message": message,
                "type": "server_error",
                "param": null,
                "code": "upstream_error",
            }
        }));
        output.extend(self.done_event());
        output
    }

    fn observe_usage(&mut self, event: &Value) {
        let usage = event
            .get("usage")
            .or_else(|| {
                event
                    .get("message")
                    .and_then(|message| message.get("usage"))
            })
            .or_else(|| {
                event
                    .get("delta")
                    .and_then(Value::as_object)
                    .and_then(|delta| delta.get("usage"))
            });
        let Some(usage) = usage.and_then(Value::as_object) else {
            return;
        };
        self.diagnostics.observe_object(usage);
        let input = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(self.usage.input);
        let output = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(self.usage.output);
        self.usage = UsageTotals {
            input,
            output,
            total: input + output,
            ..self.usage
        };
    }

    fn chat_chunk(&self, delta: Value, finish_reason: Option<&str>) -> Vec<u8> {
        let mut chunk = Map::new();
        chunk.insert(
            "id".to_owned(),
            Value::String(
                self.message_id
                    .clone()
                    .unwrap_or_else(|| "chatcmpl_unknown".to_owned()),
            ),
        );
        chunk.insert(
            "object".to_owned(),
            Value::String("chat.completion.chunk".to_owned()),
        );
        chunk.insert("created".to_owned(), Value::Number(0u64.into()));
        chunk.insert(
            "model".to_owned(),
            Value::String(self.model.clone().unwrap_or_else(|| "unknown".to_owned())),
        );
        chunk.insert(
            "choices".to_owned(),
            json!([{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]),
        );
        if self.emit_usage_to_client {
            chunk.insert("usage".to_owned(), Value::Null);
        }
        sse_data(Value::Object(chunk))
    }

    fn chat_usage_chunk(&self) -> Vec<u8> {
        sse_data(json!({
            "id": self.message_id.clone().unwrap_or_else(|| "chatcmpl_unknown".to_owned()),
            "object": "chat.completion.chunk",
            "created": 0,
            "model": self.model.clone().unwrap_or_else(|| "unknown".to_owned()),
            "choices": [],
            "usage": {
                "prompt_tokens": self.usage.input,
                "completion_tokens": self.usage.output,
                "total_tokens": self.usage.total,
            },
        }))
    }

    fn done_event(&mut self) -> Vec<u8> {
        if self.done_sent {
            return Vec::new();
        }
        self.done_sent = true;
        b"data: [DONE]\n\n".to_vec()
    }
}

/// Unified dispatch enum that wraps the three SSE normalizers behind a
/// uniform `push` / `finish` / `usage` / `diagnostics` API.
///
/// Call [`SseStrategy::new`] with the routing parameters to obtain the
/// correct variant; then delegate `push` and `finish` for every upstream
/// chunk.  The caller reads `usage()` / `diagnostics()` after each
/// `push` and resets them via `clear_usage()` / `clear_diagnostics()`.
#[derive(Debug)]
pub enum SseStrategy {
    /// Native passthrough — no protocol conversion.
    Native(SseNormalizer),
    /// Responses backend → Chat Completions client.
    Responses(ResponsesSseNormalizer),
    /// Chat Completions backend → Responses client.
    ChatCompletions(ChatCompletionsSseNormalizer),
    /// Anthropic Messages API SSE passthrough with model rewriting.
    AnthropicMessages(AnthropicSseNormalizer),
    /// Anthropic Messages backend → Chat Completions client.
    AnthropicMessagesToChat(AnthropicMessagesToChatSseNormalizer),
}

impl SseStrategy {
    /// Build the right normalizer variant for the given routing
    /// parameters.
    pub fn new(
        request_mode: RequestMode,
        backend_model: Option<String>,
        public_model: Option<String>,
        emit_usage_to_client: bool,
    ) -> Self {
        match request_mode {
            RequestMode::ResponsesViaChatCompletions => {
                Self::Responses(ResponsesSseNormalizer::new(backend_model, public_model))
            }
            RequestMode::ChatCompletionsViaResponses => {
                Self::ChatCompletions(ChatCompletionsSseNormalizer::new_with_usage_visibility(
                    backend_model,
                    public_model,
                    emit_usage_to_client,
                ))
            }
            RequestMode::ChatCompletionsViaMessages => Self::AnthropicMessagesToChat(
                AnthropicMessagesToChatSseNormalizer::new_with_usage_visibility(
                    backend_model,
                    public_model,
                    emit_usage_to_client,
                ),
            ),
            RequestMode::Native => Self::Native(SseNormalizer::new_with_usage_visibility(
                backend_model,
                public_model,
                emit_usage_to_client,
            )),
            RequestMode::AnthropicMessagesNative => {
                Self::AnthropicMessages(AnthropicSseNormalizer::new(backend_model, public_model))
            }
        }
    }

    /// Feed a raw upstream chunk through the normalizer.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            Self::Native(normalizer) => normalizer.push(chunk),
            Self::Responses(normalizer) => normalizer.push(chunk),
            Self::ChatCompletions(normalizer) => normalizer.push(chunk),
            Self::AnthropicMessages(normalizer) => normalizer.push(chunk),
            Self::AnthropicMessagesToChat(normalizer) => normalizer.push(chunk),
        }
    }

    /// Flush any remaining buffered state and emit closing events.
    pub fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::Native(normalizer) => normalizer.finish(),
            Self::Responses(normalizer) => normalizer.finish(),
            Self::ChatCompletions(normalizer) => normalizer.finish(),
            Self::AnthropicMessages(normalizer) => normalizer.finish(),
            Self::AnthropicMessagesToChat(normalizer) => normalizer.finish(),
        }
    }

    /// Accumulated usage counters observed so far.
    pub fn usage(&self) -> UsageTotals {
        match self {
            Self::Native(normalizer) => normalizer.usage,
            Self::Responses(normalizer) => normalizer.usage,
            Self::ChatCompletions(normalizer) => normalizer.usage,
            Self::AnthropicMessages(_) => UsageTotals::default(),
            Self::AnthropicMessagesToChat(normalizer) => normalizer.usage,
        }
    }

    /// Accumulated diagnostic metadata observed so far.
    pub fn diagnostics(&self) -> UsageDiagnostics {
        match self {
            Self::Native(normalizer) => normalizer.diagnostics.clone(),
            Self::Responses(normalizer) => normalizer.diagnostics.clone(),
            Self::ChatCompletions(normalizer) => normalizer.diagnostics.clone(),
            Self::AnthropicMessages(_) => UsageDiagnostics::default(),
            Self::AnthropicMessagesToChat(normalizer) => normalizer.diagnostics.clone(),
        }
    }

    /// Reset the usage counters to zero.
    pub fn clear_usage(&mut self) {
        match self {
            Self::Native(normalizer) => normalizer.usage = UsageTotals::default(),
            Self::Responses(normalizer) => normalizer.usage = UsageTotals::default(),
            Self::ChatCompletions(normalizer) => normalizer.usage = UsageTotals::default(),
            Self::AnthropicMessages(_) => {} // no-op
            Self::AnthropicMessagesToChat(normalizer) => normalizer.usage = UsageTotals::default(),
        }
    }

    /// Reset the diagnostics to the default.
    pub fn clear_diagnostics(&mut self) {
        match self {
            Self::Native(normalizer) => normalizer.diagnostics = UsageDiagnostics::default(),
            Self::Responses(normalizer) => normalizer.diagnostics = UsageDiagnostics::default(),
            Self::ChatCompletions(normalizer) => {
                normalizer.diagnostics = UsageDiagnostics::default()
            }
            Self::AnthropicMessages(_) => {} // no-op
            Self::AnthropicMessagesToChat(normalizer) => {
                normalizer.diagnostics = UsageDiagnostics::default()
            }
        }
    }
}

fn sse_event(event: &str, data: Value) -> Vec<u8> {
    let data = serde_json::to_vec(&data)
        .expect("SSE event data is always serializable; this is a programmer error");
    let mut output = Vec::with_capacity(event.len() + data.len() + 16);
    output.extend_from_slice(b"event: ");
    output.extend_from_slice(event.as_bytes());
    output.extend_from_slice(b"\n");
    output.extend_from_slice(b"data: ");
    output.extend_from_slice(&data);
    output.extend_from_slice(b"\n\n");
    output
}
