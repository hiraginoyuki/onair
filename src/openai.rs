use axum::Json;
use serde::Serialize;
use serde_json::{Map, Value};
use url::form_urlencoded;

#[derive(Debug, Clone, Default)]
pub struct RequestShape {
    pub model: Option<String>,
    pub stream: bool,
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
    if !shape.stream {
        shape.stream = query_bool(query, "stream");
    }

    shape
}

pub fn rewrite_request_body(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
) -> Vec<u8> {
    let Some(backend_model) = backend_model else {
        return body.to_vec();
    };
    if body.is_empty() {
        return Vec::new();
    }

    if should_parse_json(content_type, body) {
        if let Some(rewritten) = rewrite_json_request_body(body, backend_model) {
            return rewritten;
        }
    }

    if is_urlencoded_content_type(content_type) || looks_like_urlencoded(body) {
        if let Some(rewritten) = rewrite_urlencoded_request_body(body, backend_model) {
            return rewritten;
        }
    }

    if let Some(boundary) = content_type.and_then(multipart_boundary) {
        if let Some(rewritten) = rewrite_multipart_body(body, &boundary, backend_model) {
            return rewritten;
        }
    }

    body.to_vec()
}

pub fn rewrite_query_model(query: Option<&str>, backend_model: Option<&str>) -> Option<String> {
    let Some(query) = query else {
        return None;
    };
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

#[derive(Debug, Default, Clone, Copy)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
}

impl UsageTotals {
    pub fn is_empty(self) -> bool {
        self.input == 0 && self.output == 0
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
    totals.input += number_field(object, "prompt_tokens").unwrap_or(0);
    totals.input += number_field(object, "input_tokens").unwrap_or(0);
    totals.output += number_field(object, "completion_tokens").unwrap_or(0);
    totals.output += number_field(object, "output_tokens").unwrap_or(0);
}

fn number_field(object: &Map<String, Value>, field: &str) -> Option<u64> {
    object.get(field).and_then(Value::as_u64)
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
        self.usage.output += usage.output;
        if let (Some(backend_model), Some(public_model)) = (&self.backend_model, &self.public_model)
        {
            rewrite_response_models(&mut json, backend_model, public_model);
        }

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

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelObject>,
}

#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: &'static str,
}

impl ModelObject {
    pub fn new(id: String) -> Self {
        Self {
            id,
            object: "model",
            created: 0,
            owned_by: "onair",
        }
    }
}

pub fn models_response(models: impl IntoIterator<Item = String>) -> Json<ModelsResponse> {
    let mut data = models.into_iter().map(ModelObject::new).collect::<Vec<_>>();
    data.sort_by(|left, right| left.id.cmp(&right.id));
    Json(ModelsResponse {
        object: "list",
        data,
    })
}

pub fn model_response(model: String) -> Json<ModelObject> {
    Json(ModelObject::new(model))
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
        stream: value
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn inspect_urlencoded_body(body: &[u8]) -> RequestShape {
    let mut shape = RequestShape::default();
    for (key, value) in form_urlencoded::parse(body) {
        match key.as_ref() {
            "model" if !value.trim().is_empty() => shape.model = Some(value.into_owned()),
            "stream" => shape.stream = truthy(&value),
            _ => {}
        }
    }
    shape
}

fn inspect_multipart_body(body: &[u8], boundary: &str) -> RequestShape {
    let mut shape = RequestShape::default();
    for part in multipart_parts(body, boundary) {
        let Some((headers, content)) = split_multipart_part(part) else {
            continue;
        };
        if multipart_field_is(headers, "model") {
            if let Ok(model) = std::str::from_utf8(content) {
                let model = model.trim();
                if !model.is_empty() {
                    shape.model = Some(model.to_owned());
                }
            }
        }
        if multipart_field_is(headers, "stream") {
            if let Ok(stream) = std::str::from_utf8(content) {
                shape.stream = truthy(stream);
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
