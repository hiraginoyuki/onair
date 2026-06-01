use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::request::{RequestMode, looks_like_json};

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

#[derive(Debug, Default, Clone, Serialize)]
pub struct UsageDiagnostics {
    pub usage_object_count: u64,
    pub usage_keys: BTreeSet<String>,
}

impl UsageDiagnostics {
    fn observe_object(&mut self, object: &Map<String, Value>) {
        self.usage_object_count += 1;
        self.usage_keys.extend(object.keys().cloned());
    }

    pub fn merge(&mut self, other: UsageDiagnostics) {
        self.usage_object_count += other.usage_object_count;
        self.usage_keys.extend(other.usage_keys);
    }
}

#[derive(Debug, Default, Clone)]
pub struct UsageObservation {
    pub totals: UsageTotals,
    pub diagnostics: UsageDiagnostics,
}

pub fn extract_usage(value: &Value) -> UsageTotals {
    extract_usage_observation(value).totals
}

pub fn extract_usage_observation(value: &Value) -> UsageObservation {
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
    pub diagnostics: UsageDiagnostics,
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
        let observation = extract_usage_observation(&json);
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
    pub diagnostics: UsageDiagnostics,
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
        let observation = extract_usage_observation(&chunk);
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
