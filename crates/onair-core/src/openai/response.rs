use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value, json};

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
    if let (Some(backend_model), Some(public_model)) = (backend_model, public_model) {
        rewrite_response_models(&mut json, backend_model, public_model);
    }
    ensure_usage_total_tokens(&mut json);

    match request_mode {
        RequestMode::Native => {}
        RequestMode::ResponsesViaChatCompletions => {
            json = chat_completion_to_response(json);
        }
        RequestMode::ChatCompletionsViaResponses => {
            json = response_to_chat_completion(json);
        }
    }

    let rewritten = serde_json::to_vec(&json)
        .expect("rewritten body is always serializable; this is a programmer error");
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
    let fields = usage_fields_from_object(usage.and_then(Value::as_object));
    json!({
        "input_tokens": fields.input,
        "input_tokens_details": {
            "cached_tokens": fields.cached,
        },
        "output_tokens": fields.output,
        "output_tokens_details": {},
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
        "total_tokens": fields.total,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct UsageFields {
    input: u64,
    output: u64,
    cached: u64,
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
    let total = number_field(usage, "total_tokens").unwrap_or(input + output);
    UsageFields {
        input,
        output,
        cached,
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

#[derive(Debug, Default)]
pub struct SseNormalizer {
    pending: Vec<u8>,
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

        if let Some(event_name) = utf8_field_value(line_without_cr, b"event:") {
            self.diagnostics.observe_event_name(&event_name);
            self.pending_event_name = Some(event_name);
            return line.to_vec();
        }

        let Some(data) = sse_field_value(line_without_cr, b"data:") else {
            return line.to_vec();
        };
        let leading_space = line_without_cr
            .strip_prefix(b"data:")
            .is_some_and(|data| data.starts_with(b" "));
        if data == b"[DONE]" {
            self.pending_event_name = None;
            return line.to_vec();
        }

        let Ok(mut json) = serde_json::from_slice::<Value>(data) else {
            self.pending_event_name = None;
            return line.to_vec();
        };
        let observation = extract_usage_observation(&json);
        let usage_object_count = observation.diagnostics.usage_object_count;
        let json_event_name = json_event_type(&json).and_then(safe_diagnostic_label);
        if let Some(event_name) = &json_event_name {
            self.diagnostics.observe_event_name(event_name);
        }
        if usage_object_count > 0 {
            if let Some(event_name) = &self.pending_event_name {
                self.diagnostics.observe_usage_event_name(event_name);
            }
            if let Some(event_name) = &json_event_name {
                self.diagnostics.observe_usage_event_name(event_name);
            }
        }
        self.pending_event_name = None;
        self.usage.input += observation.totals.input;
        self.usage.cached_input += observation.totals.cached_input;
        self.usage.output += observation.totals.output;
        self.usage.total += observation.totals.total;
        self.diagnostics.merge(observation.diagnostics);
        if let (Some(backend_model), Some(public_model)) = (&self.backend_model, &self.public_model)
        {
            rewrite_response_models(&mut json, backend_model, public_model);
        }
        ensure_usage_total_tokens(&mut json);
        if !self.emit_usage_to_client && usage_object_count > 0 {
            if chat_usage_only_chunk(&json) {
                return Vec::new();
            }
            remove_usage_field(&mut json);
        }

        let normalized = serde_json::to_vec(&json)
            .expect("normalized SSE data is always serializable; this is a programmer error");
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
    pending: Vec<u8>,
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
        let line_without_cr = line_without_newline
            .strip_suffix(b"\r")
            .unwrap_or(line_without_newline);
        if let Some(event_name) = utf8_field_value(line_without_cr, b"event:") {
            self.diagnostics.observe_event_name(&event_name);
            self.pending_event_name = Some(event_name);
            return line.to_vec();
        }
        let Some(data) = sse_field_value(line_without_cr, b"data:") else {
            return line.to_vec();
        };
        if data == b"[DONE]" {
            self.pending_event_name = None;
            let mut output = self.finish_response();
            output.extend_from_slice(line);
            return output;
        }

        let Ok(mut chunk) = serde_json::from_slice::<Value>(data) else {
            self.pending_event_name = None;
            return line.to_vec();
        };
        let observation = extract_usage_observation(&chunk);
        let usage_object_count = observation.diagnostics.usage_object_count;
        let json_event_name = json_event_type(&chunk).and_then(safe_diagnostic_label);
        if let Some(event_name) = &json_event_name {
            self.diagnostics.observe_event_name(event_name);
        }
        if usage_object_count > 0 {
            if let Some(event_name) = &self.pending_event_name {
                self.diagnostics.observe_usage_event_name(event_name);
            }
            if let Some(event_name) = &json_event_name {
                self.diagnostics.observe_usage_event_name(event_name);
            }
        }
        self.pending_event_name = None;
        self.usage.input += observation.totals.input;
        self.usage.cached_input += observation.totals.cached_input;
        self.usage.output += observation.totals.output;
        self.usage.total += observation.totals.total;
        self.diagnostics.merge(observation.diagnostics);
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

#[derive(Debug, Default)]
pub struct ChatCompletionsSseNormalizer {
    pending: Vec<u8>,
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
        if !self.completed {
            output.extend(self.finish_response(None));
        }
        output.extend(self.done_event());
        output
    }

    fn normalize_line(&mut self, line: &[u8]) -> Vec<u8> {
        let line_without_newline = line.strip_suffix(b"\n").unwrap_or(line);
        let line_without_cr = line_without_newline
            .strip_suffix(b"\r")
            .unwrap_or(line_without_newline);
        if let Some(event_name) = utf8_field_value(line_without_cr, b"event:") {
            self.diagnostics.observe_event_name(&event_name);
            self.pending_event_name = Some(event_name);
            return Vec::new();
        }
        let Some(data) = sse_field_value(line_without_cr, b"data:") else {
            return Vec::new();
        };
        if data == b"[DONE]" {
            self.pending_event_name = None;
            let mut output = Vec::new();
            if !self.completed {
                output.extend(self.finish_response(None));
            }
            output.extend(self.done_event());
            return output;
        }

        let Ok(mut event) = serde_json::from_slice::<Value>(data) else {
            self.pending_event_name = None;
            return Vec::new();
        };
        let observation = extract_usage_observation(&event);
        let usage_object_count = observation.diagnostics.usage_object_count;
        let json_event_name = json_event_type(&event).and_then(safe_diagnostic_label);
        if let Some(event_name) = &json_event_name {
            self.diagnostics.observe_event_name(event_name);
        }
        if usage_object_count > 0 {
            if let Some(event_name) = &self.pending_event_name {
                self.diagnostics.observe_usage_event_name(event_name);
            }
            if let Some(event_name) = &json_event_name {
                self.diagnostics.observe_usage_event_name(event_name);
            }
        }
        self.pending_event_name = None;
        self.usage.input += observation.totals.input;
        self.usage.cached_input += observation.totals.cached_input;
        self.usage.output += observation.totals.output;
        self.usage.total += observation.totals.total;
        self.diagnostics.merge(observation.diagnostics);
        if let (Some(backend_model), Some(public_model)) = (&self.backend_model, &self.public_model)
        {
            rewrite_response_models(&mut event, backend_model, public_model);
        }
        ensure_usage_total_tokens(&mut event);

        self.process_response_event(&event)
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
