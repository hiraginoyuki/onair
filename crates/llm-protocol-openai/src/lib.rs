//! OpenAI Chat Completions and Responses reference codecs for LLM Protocol
//! Alpha `0.1.0`.
//!
//! The normative contract is in `protocol/`. This crate deliberately owns
//! only frozen OpenAI profile snapshots and does not participate in OnAir
//! request routing.

use std::collections::{BTreeMap, BTreeSet};

use llm_protocol_core::{
    ANTHROPIC_MESSAGES_PROFILE, AdapterMetadata, ApiFamily, AssetReference, AssetReferenceType,
    CachePreservationReport, CacheSegmentPlan, CanonicalEnvelope, ContentPart, ContinuationHandle,
    ConversationRole, ConversionResult, DecodedEnvelope, Diagnostic, DiagnosticCode,
    DiagnosticSeverity, EnvelopeError, ErrorCategory, Fidelity, FinishReason, GenerationControls,
    JsonSchemaOutputIntent, OPENAI_CHAT_COMPLETIONS_PROFILE, OPENAI_RESPONSES_PROFILE,
    OpaqueExtension, OpaquePayload, OpenAiCacheIntent, OutputPartType, OutputSchemaEnforcement,
    PROTOCOL_VERSION, ProfileId, ProtocolBodyKind, ProtocolError, ProtocolHeaderLine,
    ProtocolPayload, ProtocolRequest, ProtocolResponse, ReplayEnvelope, RetainedWire,
    SourceLocation, SseFrame, SseFramer, SseFramingError, StreamEvent, ToolDefinition, Usage,
};
use serde_json::{Map, Value, json};
use thiserror::Error;

pub const CHAT_COMPLETIONS_PROFILE: &str = OPENAI_CHAT_COMPLETIONS_PROFILE;
pub const RESPONSES_PROFILE: &str = OPENAI_RESPONSES_PROFILE;

const RESPONSES_QUEUED_HANDLE_NAMESPACE: &str = "openai.responses.response_handle.queued";
const RESPONSES_IN_PROGRESS_HANDLE_NAMESPACE: &str = "openai.responses.response_handle.in_progress";

/// A decoded OpenAI envelope. A codec determines the payload from the frozen
/// profile and envelope shape; callers do not choose a pair-specific
/// translator.
pub type OpenAiPayload = ProtocolPayload;

/// Canonical or raw protocol material ready for an HTTP adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WireEnvelope {
    pub profile_id: ProfileId,
    pub status: u16,
    pub body_kind: ProtocolBodyKind,
    pub protocol_headers: Vec<ProtocolHeaderLine>,
    pub body: Vec<u8>,
    pub adapter_metadata: AdapterMetadata,
}

impl WireEnvelope {
    pub fn retained_wire(&self) -> RetainedWire {
        RetainedWire {
            profile_id: self.profile_id.clone(),
            status: self.status,
            body_kind: self.body_kind,
            protocol_headers: self.protocol_headers.clone(),
            body: self.body.clone(),
        }
    }
}

impl From<ReplayEnvelope> for WireEnvelope {
    fn from(replay: ReplayEnvelope) -> Self {
        Self {
            profile_id: replay.profile_id,
            status: replay.status,
            body_kind: replay.body_kind,
            protocol_headers: replay.protocol_headers,
            body: replay.body,
            adapter_metadata: AdapterMetadata::default(),
        }
    }
}

/// Encoded wire material and an optional content-free cache preservation
/// report. Same-profile raw replay does not need a conversion cache report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedEnvelope {
    pub wire: WireEnvelope,
    pub cache_report: Option<CachePreservationReport>,
}

/// The only supported profile codecs in Phase 3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiProfile {
    ChatCompletions,
    Responses,
}

impl OpenAiProfile {
    pub fn profile_id(self) -> ProfileId {
        ProfileId::new(match self {
            Self::ChatCompletions => CHAT_COMPLETIONS_PROFILE,
            Self::Responses => RESPONSES_PROFILE,
        })
        .expect("OpenAI alpha profile identifiers are valid")
    }

    pub fn api_family(self) -> ApiFamily {
        match self {
            Self::ChatCompletions => ApiFamily::ChatCompletions,
            Self::Responses => ApiFamily::Responses,
        }
    }

    pub fn from_id(profile_id: &ProfileId) -> Result<Self, CodecError> {
        match profile_id.as_str() {
            CHAT_COMPLETIONS_PROFILE => Ok(Self::ChatCompletions),
            RESPONSES_PROFILE => Ok(Self::Responses),
            _ => Err(CodecError::UnsupportedProfile(profile_id.clone())),
        }
    }
}

/// Decode a retained profile envelope to the common IR.
///
/// `Unsupported` is used for valid wire material outside the frozen typed
/// alpha subset. Invalid profile JSON and invalid envelope construction remain
/// ordinary codec errors.
pub fn decode(
    retained: RetainedWire,
    adapter_metadata: AdapterMetadata,
) -> Result<ConversionResult<DecodedEnvelope<OpenAiPayload>>, CodecError> {
    retained.validate().map_err(CodecError::Envelope)?;
    let profile = OpenAiProfile::from_id(&retained.profile_id)?;
    let result = match retained.body_kind {
        ProtocolBodyKind::Json => decode_json(profile, &retained),
        ProtocolBodyKind::Sse => decode_stream(profile, &retained),
    }?;

    match result.output {
        Some(value) => {
            let envelope = DecodedEnvelope::new(value, retained, adapter_metadata)
                .map_err(CodecError::Envelope)?;
            Ok(ConversionResult {
                output: Some(envelope),
                fidelity: result.fidelity,
                diagnostics: result.diagnostics,
            })
        }
        None => Ok(ConversionResult {
            output: None,
            fidelity: result.fidelity,
            diagnostics: result.diagnostics,
        }),
    }
}

/// Encode an unmodified decoded envelope. It uses byte-exact raw replay only
/// when the requested target is the exact issuing profile.
pub fn encode_decoded(
    decoded: &DecodedEnvelope<OpenAiPayload>,
    target_profile: &ProfileId,
) -> Result<ConversionResult<EncodedEnvelope>, CodecError> {
    OpenAiProfile::from_id(target_profile)?;
    if decoded.retained().profile_id == *target_profile {
        return Ok(ConversionResult::exact(EncodedEnvelope {
            wire: WireEnvelope {
                profile_id: target_profile.clone(),
                status: decoded.retained().status,
                body_kind: decoded.retained().body_kind,
                protocol_headers: decoded.retained().protocol_headers.clone(),
                body: decoded.retained().body.clone(),
                adapter_metadata: decoded.adapter_metadata().clone(),
            },
            cache_report: None,
        }));
    }

    encode_value(
        decoded.value(),
        &decoded.retained().profile_id,
        target_profile,
        decoded.retained().status,
        decoded.retained().body_kind,
        decoded.adapter_metadata().clone(),
    )
}

/// Encode a semantically modified envelope. A canonical envelope has no raw
/// protocol body/header material, so this function always serializes fresh
/// output even when its target profile is unchanged.
pub fn encode_canonical(
    canonical: CanonicalEnvelope<OpenAiPayload>,
    target_profile: &ProfileId,
) -> Result<ConversionResult<EncodedEnvelope>, CodecError> {
    OpenAiProfile::from_id(target_profile)?;
    encode_value(
        &canonical.value,
        &canonical.profile_id,
        target_profile,
        canonical.status,
        canonical.body_kind,
        canonical.adapter_metadata,
    )
}

/// Parse a JSON object into an envelope representation suitable for tests,
/// adapters, and conformance vectors.
pub fn wire_envelope_from_json(value: &Value) -> Result<WireEnvelope, CodecError> {
    let object = value
        .as_object()
        .ok_or_else(|| CodecError::InvalidEnvelope("envelope must be an object".to_owned()))?;
    let protocol_version = required_str(object, "protocol_version")?;
    if protocol_version != PROTOCOL_VERSION {
        return Err(CodecError::InvalidEnvelope(format!(
            "unsupported protocol_version {protocol_version}"
        )));
    }
    let profile_id = profile_id_field(object, "profile_id")?;
    let status = required_u16(object, "status")?;
    let body_kind = match required_str(object, "body_kind")? {
        "json" => ProtocolBodyKind::Json,
        "sse" => ProtocolBodyKind::Sse,
        other => {
            return Err(CodecError::InvalidEnvelope(format!(
                "unsupported body_kind {other}"
            )));
        }
    };
    let protocol_headers = object
        .get("protocol_headers")
        .and_then(Value::as_array)
        .ok_or_else(|| CodecError::InvalidEnvelope("protocol_headers must be an array".to_owned()))?
        .iter()
        .map(|header| {
            header
                .get("raw_line")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CodecError::InvalidEnvelope("protocol header must contain raw_line".to_owned())
                })
                .and_then(|line| ProtocolHeaderLine::new(line).map_err(CodecError::Envelope))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let body = object
        .get("body_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| CodecError::InvalidEnvelope("body_base64 must be a string".to_owned()))
        .and_then(decode_base64)?;
    let adapter_metadata = object
        .get("adapter_headers")
        .and_then(Value::as_object)
        .map(adapter_metadata_from_json)
        .transpose()?
        .unwrap_or_default();
    let wire = WireEnvelope {
        profile_id,
        status,
        body_kind,
        protocol_headers,
        body,
        adapter_metadata,
    };
    wire.retained_wire()
        .validate()
        .map_err(CodecError::Envelope)?;
    Ok(wire)
}

pub fn wire_envelope_to_json(wire: &WireEnvelope) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "profile_id": wire.profile_id,
        "status": wire.status,
        "body_kind": match wire.body_kind {
            ProtocolBodyKind::Json => "json",
            ProtocolBodyKind::Sse => "sse",
        },
        "protocol_headers": wire.protocol_headers.iter().map(|header| {
            json!({"raw_line": header.raw_line})
        }).collect::<Vec<_>>(),
        "adapter_headers": wire.adapter_metadata.generic_headers,
        "body_base64": encode_base64(&wire.body),
    })
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("unsupported OpenAI alpha profile: {0}")]
    UnsupportedProfile(ProfileId),
    #[error("invalid protocol envelope: {0}")]
    Envelope(#[source] EnvelopeError),
    #[error("invalid wire envelope: {0}")]
    InvalidEnvelope(String),
    #[error("invalid JSON body: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("invalid SSE body: {0}")]
    InvalidSse(#[source] SseFramingError),
    #[error("invalid OpenAI {profile} {kind}: {message}")]
    InvalidShape {
        profile: &'static str,
        kind: &'static str,
        message: String,
    },
}

#[derive(Clone, Debug)]
struct PayloadResult {
    output: Option<OpenAiPayload>,
    fidelity: Fidelity,
    diagnostics: Vec<Diagnostic>,
}

impl PayloadResult {
    fn exact(output: OpenAiPayload) -> Self {
        Self {
            output: Some(output),
            fidelity: Fidelity::Exact,
            diagnostics: Vec::new(),
        }
    }

    fn unsupported(diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            output: None,
            fidelity: Fidelity::Unsupported,
            diagnostics,
        }
    }
}

#[derive(Clone, Debug)]
struct ConversionTracker {
    fidelity: Fidelity,
    diagnostics: Vec<Diagnostic>,
}

impl ConversionTracker {
    fn new() -> Self {
        Self {
            fidelity: Fidelity::Exact,
            diagnostics: Vec::new(),
        }
    }

    fn adapted(
        &mut self,
        code: DiagnosticCode,
        location: Option<SourceLocation>,
        message: &'static str,
    ) {
        self.promote(Fidelity::Adapted);
        self.diagnostics.push(diagnostic(
            code,
            DiagnosticSeverity::Info,
            location,
            message,
        ));
    }

    fn lossy(
        &mut self,
        code: DiagnosticCode,
        location: Option<SourceLocation>,
        message: &'static str,
    ) {
        self.promote(Fidelity::Lossy);
        self.diagnostics.push(diagnostic(
            code,
            DiagnosticSeverity::Warning,
            location,
            message,
        ));
    }

    fn unsupported(
        &mut self,
        code: DiagnosticCode,
        location: Option<SourceLocation>,
        message: &'static str,
    ) {
        self.promote(Fidelity::Unsupported);
        self.diagnostics.push(diagnostic(
            code,
            DiagnosticSeverity::Error,
            location,
            message,
        ));
    }

    fn promote(&mut self, candidate: Fidelity) {
        if fidelity_rank(candidate) > fidelity_rank(self.fidelity) {
            self.fidelity = candidate;
        }
    }

    fn finish<T>(self, output: T) -> ConversionResult<T> {
        match self.fidelity {
            Fidelity::Exact => ConversionResult::exact(output),
            Fidelity::Adapted => ConversionResult::adapted(output, self.diagnostics),
            Fidelity::Lossy => ConversionResult::lossy(output, self.diagnostics),
            Fidelity::Unsupported => ConversionResult::unsupported(self.diagnostics),
        }
    }
}

fn fidelity_rank(fidelity: Fidelity) -> u8 {
    match fidelity {
        Fidelity::Exact => 0,
        Fidelity::Adapted => 1,
        Fidelity::Lossy => 2,
        Fidelity::Unsupported => 3,
    }
}

fn decode_json(
    profile: OpenAiProfile,
    retained: &RetainedWire,
) -> Result<PayloadResult, CodecError> {
    let value = serde_json::from_slice::<Value>(&retained.body).map_err(CodecError::InvalidJson)?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_shape(profile, "JSON body must be an object"))?;

    if object.get("error").is_some_and(Value::is_object) || retained.status >= 400 {
        return decode_error(profile, object, retained);
    }

    match profile {
        OpenAiProfile::ChatCompletions => {
            if object.contains_key("choices") {
                decode_chat_response(object, &retained.profile_id)
            } else {
                decode_chat_request(object, &retained.profile_id)
            }
        }
        OpenAiProfile::Responses => {
            if object.contains_key("output") || object.contains_key("status") {
                decode_responses_response(object, &retained.profile_id).map(PayloadResult::exact)
            } else {
                decode_responses_request(object, &retained.profile_id)
            }
        }
    }
}

fn decode_stream(
    profile: OpenAiProfile,
    retained: &RetainedWire,
) -> Result<PayloadResult, CodecError> {
    let result = decode_sse_chunks(&retained.profile_id, &[retained.body.as_slice()])?;
    let _ = profile;
    Ok(PayloadResult {
        output: result.output.map(OpenAiPayload::Stream),
        fidelity: result.fidelity,
        diagnostics: result.diagnostics,
    })
}

/// Normalize a complete SSE stream from arbitrary byte chunks. This is the
/// provider-specific half of stream conformance; generic SSE framing remains
/// in `llm-protocol-core`.
pub fn decode_sse_chunks(
    profile_id: &ProfileId,
    chunks: &[&[u8]],
) -> Result<ConversionResult<Vec<StreamEvent>>, CodecError> {
    let profile = OpenAiProfile::from_id(profile_id)?;
    let mut framer = SseFramer::new();
    let mut frames = Vec::new();
    for chunk in chunks {
        frames.extend(framer.push(chunk).map_err(CodecError::InvalidSse)?);
    }
    let _ = framer.finish().map_err(CodecError::InvalidSse)?;
    let events = match profile {
        OpenAiProfile::ChatCompletions => decode_chat_stream(frames, profile_id),
        OpenAiProfile::Responses => decode_responses_stream(frames, profile_id),
    };
    Ok(ConversionResult::exact(events))
}

fn decode_error(
    profile: OpenAiProfile,
    object: &Map<String, Value>,
    retained: &RetainedWire,
) -> Result<PayloadResult, CodecError> {
    let error_object = object
        .get("error")
        .and_then(Value::as_object)
        .unwrap_or(object);
    let message = error_object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI request failed")
        .to_owned();
    let code = error_object
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error_object.get("type").and_then(Value::as_str))
        .unwrap_or("unknown_error")
        .to_owned();
    let category = error_category(
        error_object.get("type").and_then(Value::as_str),
        retained.status,
    );
    let extensions = unknown_extensions(
        error_object,
        &["message", "code", "type", "param"],
        &retained.profile_id,
        "openai.error.unknown_field",
        "",
    );
    let error = ProtocolError {
        category,
        code,
        message,
        retry_after_ms: retry_after_ms(&retained.protocol_headers),
        param: error_object
            .get("param")
            .and_then(Value::as_str)
            .map(str::to_owned),
        extensions,
    };
    let _ = profile;
    Ok(PayloadResult::exact(OpenAiPayload::Error(error)))
}

fn decode_chat_request(
    object: &Map<String, Value>,
    profile_id: &ProfileId,
) -> Result<PayloadResult, CodecError> {
    if object
        .get("n")
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 1)
    {
        return Ok(PayloadResult::unsupported(vec![unsupported_diagnostic(
            SourceLocation::JsonPointer {
                pointer: "/n".to_owned(),
            },
            "multiple Chat Completions choices are outside the typed alpha request subset",
        )]));
    }
    if non_default_chat_option(object, "tool_choice")
        || non_default_chat_option(object, "parallel_tool_calls")
        || non_default_chat_option(object, "logprobs")
        || object.contains_key("top_logprobs")
        || object.contains_key("stream_options")
    {
        return Ok(PayloadResult::unsupported(vec![unsupported_diagnostic(
            SourceLocation::JsonPointer {
                pointer: "/".to_owned(),
            },
            "the request uses an OpenAI option outside the frozen typed alpha subset",
        )]));
    }

    let messages = required_array(object, "messages", OpenAiProfile::ChatCompletions)?;
    let mut decoded_messages = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        decoded_messages.push(decode_chat_message(message, profile_id, index)?);
    }

    let generation = generation_from_chat(object);
    let output_schema = decode_chat_output_schema(object, profile_id)?;
    let tools = decode_chat_tools(object, profile_id)?;
    let cache_intent = openai_cache_intent(object);
    let extensions = unknown_extensions(
        object,
        &[
            "model",
            "stream",
            "messages",
            "temperature",
            "top_p",
            "stop",
            "max_tokens",
            "max_completion_tokens",
            "tools",
            "response_format",
            "prompt_cache_key",
            "prompt_cache_retention",
            "n",
            "tool_choice",
            "parallel_tool_calls",
            "logprobs",
            "top_logprobs",
            "stream_options",
        ],
        profile_id,
        "openai.chat.request.unknown_field",
        "",
    );

    Ok(PayloadResult::exact(OpenAiPayload::Request(
        ProtocolRequest {
            model: optional_string(object, "model"),
            stream: object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            instructions: Vec::new(),
            messages: decoded_messages,
            tools,
            generation,
            output_schema,
            cache_intent,
            continuation: None,
            extensions,
        },
    )))
}

fn decode_responses_request(
    object: &Map<String, Value>,
    profile_id: &ProfileId,
) -> Result<PayloadResult, CodecError> {
    if object.contains_key("tool_choice")
        || object.contains_key("parallel_tool_calls")
        || object.contains_key("truncation")
        || object.contains_key("include")
        || object.contains_key("background")
    {
        return Ok(PayloadResult::unsupported(vec![unsupported_diagnostic(
            SourceLocation::JsonPointer {
                pointer: "/".to_owned(),
            },
            "the request uses a Responses option outside the frozen typed alpha subset",
        )]));
    }

    let mut messages = Vec::new();
    if let Some(input) = object.get("input") {
        decode_responses_input(input, profile_id, &mut messages)?;
    }
    let instructions = object
        .get("instructions")
        .map(|instructions| decode_instruction_parts(instructions, profile_id))
        .transpose()?
        .unwrap_or_default();
    let tools = decode_responses_tools(object, profile_id)?;
    let output_schema = decode_responses_output_schema(object, profile_id)?;
    let continuation = object
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(|response_id| ContinuationHandle {
            issuing_profile: profile_id.clone(),
            extension: opaque_text(
                profile_id,
                "openai.responses.previous_response_id",
                SourceLocation::JsonPointer {
                    pointer: "/previous_response_id".to_owned(),
                },
                response_id,
            ),
        });
    let extensions = unknown_extensions(
        object,
        &[
            "model",
            "stream",
            "input",
            "instructions",
            "temperature",
            "top_p",
            "max_output_tokens",
            "tools",
            "text",
            "prompt_cache_key",
            "prompt_cache_retention",
            "previous_response_id",
            "tool_choice",
            "parallel_tool_calls",
            "truncation",
            "include",
            "background",
        ],
        profile_id,
        "openai.responses.request.unknown_field",
        "",
    );

    Ok(PayloadResult::exact(OpenAiPayload::Request(
        ProtocolRequest {
            model: optional_string(object, "model"),
            stream: object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            instructions,
            messages,
            tools,
            generation: generation_from_responses(object),
            output_schema,
            cache_intent: openai_cache_intent(object),
            continuation,
            extensions,
        },
    )))
}

fn decode_chat_response(
    object: &Map<String, Value>,
    profile_id: &ProfileId,
) -> Result<PayloadResult, CodecError> {
    let choices = required_array(object, "choices", OpenAiProfile::ChatCompletions)?;
    if choices.len() > 1 {
        return Ok(PayloadResult::unsupported(vec![unsupported_diagnostic(
            SourceLocation::JsonPointer {
                pointer: "/choices".to_owned(),
            },
            "multiple Chat Completions choices are outside the typed alpha response subset",
        )]));
    }
    let mut output = Vec::with_capacity(choices.len());
    for (index, choice) in choices.iter().enumerate() {
        let message = choice.get("message").ok_or_else(|| {
            invalid_shape(OpenAiProfile::ChatCompletions, "choice requires message")
        })?;
        output.push(decode_chat_message(message, profile_id, index)?);
    }
    let extensions = unknown_extensions(
        object,
        &[
            "id",
            "model",
            "choices",
            "usage",
            "object",
            "created",
            "service_tier",
        ],
        profile_id,
        "openai.chat.response.unknown_field",
        "",
    );
    let finish_reason = choices
        .first()
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .map(normalize_chat_finish_reason)
        .unwrap_or_else(|| FinishReason::STOP.to_owned());
    Ok(PayloadResult::exact(OpenAiPayload::Response(
        ProtocolResponse {
            id: optional_string(object, "id"),
            model: optional_string(object, "model"),
            output,
            usage: object.get("usage").and_then(decode_chat_usage),
            finish_reason: FinishReason::new(finish_reason)
                .expect("normal finish reasons are non-empty"),
            continuation: None,
            extensions,
        },
    )))
}

fn decode_responses_response(
    object: &Map<String, Value>,
    profile_id: &ProfileId,
) -> Result<OpenAiPayload, CodecError> {
    let output_items = object
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_shape(OpenAiProfile::Responses, "response requires output"))?;
    let mut output = Vec::new();
    for (index, item) in output_items.iter().enumerate() {
        decode_responses_output_item(item, profile_id, index, &mut output)?;
    }
    let status = object
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let finish_reason = match status {
        "completed" | "in_progress" | "queued" => FinishReason::STOP,
        "incomplete" => FinishReason::LENGTH,
        "failed" | "cancelled" => FinishReason::ERROR,
        _ => FinishReason::STOP,
    };
    let continuation = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|_| matches!(status, "in_progress" | "queued"))
        .map(|response_id| ContinuationHandle {
            issuing_profile: profile_id.clone(),
            extension: opaque_text(
                profile_id,
                match status {
                    "queued" => RESPONSES_QUEUED_HANDLE_NAMESPACE,
                    "in_progress" => RESPONSES_IN_PROGRESS_HANDLE_NAMESPACE,
                    _ => unreachable!("continuation is created only for nonterminal status"),
                },
                SourceLocation::JsonPointer {
                    pointer: "/id".to_owned(),
                },
                response_id,
            ),
        });
    let extensions = unknown_extensions(
        object,
        &[
            "id",
            "model",
            "output",
            "usage",
            "status",
            "object",
            "created_at",
            "output_text",
            "error",
            "incomplete_details",
        ],
        profile_id,
        "openai.responses.response.unknown_field",
        "",
    );

    Ok(OpenAiPayload::Response(ProtocolResponse {
        id: optional_string(object, "id"),
        model: optional_string(object, "model"),
        output,
        usage: object.get("usage").and_then(decode_responses_usage),
        finish_reason: FinishReason::new(finish_reason).expect("normal finish reason is non-empty"),
        continuation,
        extensions,
    }))
}

fn encode_value(
    value: &OpenAiPayload,
    source_profile: &ProfileId,
    target_profile: &ProfileId,
    status: u16,
    requested_body_kind: ProtocolBodyKind,
    adapter_metadata: AdapterMetadata,
) -> Result<ConversionResult<EncodedEnvelope>, CodecError> {
    let target = OpenAiProfile::from_id(target_profile)?;
    if !matches!(
        source_profile.as_str(),
        CHAT_COMPLETIONS_PROFILE | RESPONSES_PROFILE | ANTHROPIC_MESSAGES_PROFILE
    ) {
        return Err(CodecError::UnsupportedProfile(source_profile.clone()));
    }
    let source_openai = OpenAiProfile::from_id(source_profile).ok();
    let mut tracker = ConversionTracker::new();

    let body_kind = match value {
        OpenAiPayload::Stream(_) => ProtocolBodyKind::Sse,
        _ => ProtocolBodyKind::Json,
    };
    if requested_body_kind != body_kind {
        tracker.adapted(
            DiagnosticCode::SemanticChange,
            None,
            "the target envelope body kind was selected from the typed payload",
        );
    }

    let body_value = match value {
        OpenAiPayload::Request(request) => {
            let result = encode_request(request, source_profile, target, &mut tracker)?;
            if tracker.fidelity == Fidelity::Unsupported {
                return Ok(ConversionResult::unsupported(tracker.diagnostics));
            }
            result
        }
        OpenAiPayload::Response(response) => {
            let result = encode_response(response, target, &mut tracker)?;
            if tracker.fidelity == Fidelity::Unsupported {
                return Ok(ConversionResult::unsupported(tracker.diagnostics));
            }
            result
        }
        OpenAiPayload::Error(error) => encode_error(error, target, &mut tracker),
        OpenAiPayload::Stream(events) => {
            if source_profile != target_profile {
                tracker.adapted(
                    DiagnosticCode::SemanticChange,
                    None,
                    "stream lifecycle frames were adapted to the target OpenAI dialect",
                );
            }
            Value::String(encode_stream(events, target, &mut tracker))
        }
    };

    if tracker.fidelity == Fidelity::Unsupported {
        return Ok(ConversionResult::unsupported(tracker.diagnostics));
    }

    let body = match body_kind {
        ProtocolBodyKind::Json => serde_json::to_vec(&body_value)
            .expect("OpenAI canonical JSON bodies are always serializable"),
        ProtocolBodyKind::Sse => body_value
            .as_str()
            .expect("stream encoder returns an SSE string")
            .as_bytes()
            .to_vec(),
    };
    let mut protocol_headers = vec![
        ProtocolHeaderLine::new(match body_kind {
            ProtocolBodyKind::Json => "content-type: application/json",
            ProtocolBodyKind::Sse => "content-type: text/event-stream",
        })
        .expect("canonical content type is valid"),
    ];
    if let OpenAiPayload::Error(error) = value
        && let Some(retry_after_ms) = error.retry_after_ms
        && retry_after_ms > 0
    {
        let retry_after_seconds = retry_after_ms.div_ceil(1000);
        protocol_headers.push(
            ProtocolHeaderLine::new(format!("retry-after: {retry_after_seconds}"))
                .expect("canonical retry-after header is valid"),
        );
    }

    let cache_report = if source_profile != target_profile {
        match value {
            OpenAiPayload::Request(request) => {
                let plan = CacheSegmentPlan::analyze(request)
                    .map_err(|error| CodecError::InvalidEnvelope(error.to_string()))?;
                if source_openai.is_some() {
                    Some(CachePreservationReport::preserved(&plan))
                } else if request.cache_intent.is_some() {
                    Some(CachePreservationReport::with_non_portable_directives(&plan))
                } else {
                    Some(CachePreservationReport::preserved(&plan))
                }
            }
            _ => None,
        }
    } else {
        None
    };
    Ok(tracker.finish(EncodedEnvelope {
        wire: WireEnvelope {
            profile_id: target_profile.clone(),
            status,
            body_kind,
            protocol_headers,
            body,
            adapter_metadata,
        },
        cache_report,
    }))
}

fn encode_request(
    request: &ProtocolRequest,
    source_profile: &ProfileId,
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    diagnose_cross_profile_extensions(
        &request.extensions,
        target,
        tracker,
        "unknown request fields cannot be canonically projected across OpenAI profiles",
    );
    if let Some(handle) = &request.continuation
        && !handle.is_issued_by(&target.profile_id())
    {
        tracker.unsupported(
            DiagnosticCode::NonPortableContinuationHandle,
            Some(handle.extension.source_location.clone()),
            "a provider continuation handle cannot be encoded with contradictory metadata or for a different profile",
        );
        return Ok(Value::Null);
    }

    let mut object = match target {
        OpenAiProfile::ChatCompletions => encode_chat_request(request, tracker)?,
        OpenAiProfile::Responses => encode_responses_request(request, tracker)?,
    };
    if let Some(model) = &request.model {
        object.insert("model".to_owned(), Value::String(model.clone()));
    }
    if request.stream {
        object.insert("stream".to_owned(), Value::Bool(true));
    }
    insert_generation(&mut object, &request.generation, target);
    insert_openai_cache_intent(&mut object, request, target, tracker);
    insert_output_schema(&mut object, request.output_schema.as_ref(), target, tracker)?;
    insert_tools(&mut object, &request.tools, target, tracker)?;
    if source_profile != &target.profile_id()
        && !request.instructions.is_empty()
        && target == OpenAiProfile::ChatCompletions
    {
        tracker.adapted(
            DiagnosticCode::SemanticChange,
            Some(SourceLocation::JsonPointer {
                pointer: "/instructions".to_owned(),
            }),
            "Responses instructions were represented as a Chat system message",
        );
    }
    Ok(Value::Object(object))
}

fn encode_response(
    response: &ProtocolResponse,
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    diagnose_cross_profile_extensions(
        &response.extensions,
        target,
        tracker,
        "unknown response fields cannot be canonically projected across OpenAI profiles",
    );
    if let Some(handle) = &response.continuation {
        if !handle.is_issued_by(&target.profile_id()) {
            tracker.lossy(
                DiagnosticCode::NonPortableContinuationHandle,
                Some(handle.extension.source_location.clone()),
                "a provider continuation handle with contradictory metadata or a different profile was omitted from the response",
            );
        } else if target != OpenAiProfile::Responses {
            tracker.lossy(
                DiagnosticCode::NonPortableContinuationHandle,
                Some(handle.extension.source_location.clone()),
                "the target profile cannot represent a Responses continuation handle",
            );
        }
    }

    match target {
        OpenAiProfile::ChatCompletions => encode_chat_response(response, tracker),
        OpenAiProfile::Responses => encode_responses_response(response, tracker),
    }
}

fn encode_error(
    error: &ProtocolError,
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
) -> Value {
    diagnose_cross_profile_extensions(
        &error.extensions,
        target,
        tracker,
        "unknown error fields cannot be canonically projected across OpenAI profiles",
    );
    json!({
        "error": {
            "message": error.message,
            "type": error_type(error.category),
            "param": error.param,
            "code": error.code,
        }
    })
}

fn decode_chat_message(
    value: &Value,
    profile_id: &ProfileId,
    message_index: usize,
) -> Result<llm_protocol_core::Message, CodecError> {
    let object = value.as_object().ok_or_else(|| {
        invalid_shape(OpenAiProfile::ChatCompletions, "message must be an object")
    })?;
    let role = decode_role(
        required_str(object, "role")?,
        OpenAiProfile::ChatCompletions,
    )?;
    let mut content = object
        .get("content")
        .filter(|content| !content.is_null())
        .map(|content| decode_chat_content(content, profile_id, message_index))
        .transpose()?
        .unwrap_or_default();
    if let Some(tool_calls) = object.get("tool_calls").and_then(Value::as_array) {
        for (tool_index, tool_call) in tool_calls.iter().enumerate() {
            content.push(decode_chat_tool_call(
                tool_call,
                profile_id,
                message_index,
                tool_index,
            )?);
        }
    }
    if role == ConversationRole::Tool {
        let call_id = object
            .get("tool_call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_shape(
                    OpenAiProfile::ChatCompletions,
                    "tool messages require tool_call_id",
                )
            })?;
        content = vec![ContentPart::ToolResult {
            tool_call_id: call_id.to_owned(),
            content,
            is_error: false,
            extensions: Vec::new(),
        }];
    }
    Ok(llm_protocol_core::Message {
        role,
        name: optional_string(object, "name"),
        content,
        extensions: unknown_extensions(
            object,
            &["role", "name", "content", "tool_calls", "tool_call_id"],
            profile_id,
            "openai.chat.message.unknown_field",
            &format!("/messages/{message_index}"),
        ),
    })
}

fn decode_chat_content(
    value: &Value,
    profile_id: &ProfileId,
    message_index: usize,
) -> Result<Vec<ContentPart>, CodecError> {
    match value {
        Value::String(text) => Ok(vec![ContentPart::Text {
            text: text.to_owned(),
        }]),
        Value::Array(parts) => {
            let mut content = Vec::new();
            for (part_index, part) in parts.iter().enumerate() {
                let object = part.as_object().ok_or_else(|| {
                    invalid_shape(
                        OpenAiProfile::ChatCompletions,
                        "content parts must be objects",
                    )
                })?;
                let pointer = format!("/messages/{message_index}/content/{part_index}");
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => content.push(ContentPart::Text {
                        text: object
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                invalid_shape(
                                    OpenAiProfile::ChatCompletions,
                                    "text content parts require text",
                                )
                            })?
                            .to_owned(),
                    }),
                    Some("image_url") => content.push(ContentPart::Image {
                        asset: decode_chat_image(object, profile_id, &pointer)?,
                    }),
                    Some("refusal") => content.push(ContentPart::Refusal {
                        text: object
                            .get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        extensions: unknown_extensions(
                            object,
                            &["type", "refusal"],
                            profile_id,
                            "openai.chat.refusal.unknown_field",
                            &pointer,
                        ),
                    }),
                    _ => content.push(ContentPart::Opaque {
                        extension: opaque_json(
                            profile_id,
                            "openai.chat.unknown_content_part",
                            SourceLocation::JsonPointer { pointer },
                            part.clone(),
                        ),
                    }),
                }
            }
            Ok(content)
        }
        _ => Err(invalid_shape(
            OpenAiProfile::ChatCompletions,
            "message content must be a string or array",
        )),
    }
}

fn decode_chat_image(
    object: &Map<String, Value>,
    profile_id: &ProfileId,
    pointer: &str,
) -> Result<AssetReference, CodecError> {
    let image_url = object.get("image_url").ok_or_else(|| {
        invalid_shape(
            OpenAiProfile::ChatCompletions,
            "image_url parts require image_url",
        )
    })?;
    let (url, media_type) = match image_url {
        Value::String(url) => (url.to_owned(), media_type_from_data_url(url)),
        Value::Object(image_url) => {
            let url = image_url
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid_shape(
                        OpenAiProfile::ChatCompletions,
                        "image_url objects require url",
                    )
                })?;
            let extensions = unknown_extensions(
                image_url,
                &["url"],
                profile_id,
                "openai.chat.image_url.unknown_field",
                &format!("{pointer}/image_url"),
            );
            if !extensions.is_empty() {
                // AssetReference has no provider extension slot. Retained raw
                // replay covers same-profile material; cross-profile encoding
                // reports the message-level opaque extension below.
            }
            (url.to_owned(), media_type_from_data_url(url))
        }
        _ => {
            return Err(invalid_shape(
                OpenAiProfile::ChatCompletions,
                "image_url must be a string or object",
            ));
        }
    };
    Ok(AssetReference {
        reference_type: if url.starts_with("data:") {
            AssetReferenceType::Data
        } else {
            AssetReferenceType::Url
        },
        value: url,
        media_type,
        name: None,
        size_bytes: None,
    })
}

fn decode_chat_tool_call(
    value: &Value,
    _profile_id: &ProfileId,
    _message_index: usize,
    tool_index: usize,
) -> Result<ContentPart, CodecError> {
    let object = value.as_object().ok_or_else(|| {
        invalid_shape(OpenAiProfile::ChatCompletions, "tool calls must be objects")
    })?;
    if object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "function")
    {
        return Err(invalid_shape(
            OpenAiProfile::ChatCompletions,
            "only function tool calls are in the typed alpha subset",
        ));
    }
    let function = object
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            invalid_shape(
                OpenAiProfile::ChatCompletions,
                "function tool calls require function",
            )
        })?;
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .map(parse_json_or_string)
        .unwrap_or_else(|| Value::Object(Map::new()));
    Ok(ContentPart::ToolCall {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("call_{tool_index}")),
        name: function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_shape(
                    OpenAiProfile::ChatCompletions,
                    "function tool calls require function.name",
                )
            })?
            .to_owned(),
        arguments,
        extensions: Vec::new(),
    })
}

fn decode_chat_tools(
    object: &Map<String, Value>,
    profile_id: &ProfileId,
) -> Result<Vec<ToolDefinition>, CodecError> {
    let Some(tools) = object.get("tools") else {
        return Ok(Vec::new());
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid_shape(OpenAiProfile::ChatCompletions, "tools must be an array"))?;
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let object = tool.as_object().ok_or_else(|| {
                invalid_shape(
                    OpenAiProfile::ChatCompletions,
                    "tool definitions must be objects",
                )
            })?;
            if object.get("type").and_then(Value::as_str) != Some("function") {
                return Err(invalid_shape(
                    OpenAiProfile::ChatCompletions,
                    "only function tool definitions are in the typed alpha subset",
                ));
            }
            let function = object
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    invalid_shape(
                        OpenAiProfile::ChatCompletions,
                        "function tools require function",
                    )
                })?;
            Ok(ToolDefinition {
                name: required_str(function, "name")?.to_owned(),
                description: optional_string(function, "description"),
                input_schema: function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
                strict: function.get("strict").and_then(Value::as_bool),
                extensions: unknown_extensions(
                    function,
                    &["name", "description", "parameters", "strict"],
                    profile_id,
                    "openai.chat.tool.unknown_field",
                    &format!("/tools/{index}/function"),
                ),
            })
        })
        .collect()
}

fn decode_chat_output_schema(
    object: &Map<String, Value>,
    _profile_id: &ProfileId,
) -> Result<Option<JsonSchemaOutputIntent>, CodecError> {
    let Some(response_format) = object.get("response_format") else {
        return Ok(None);
    };
    let response_format = response_format.as_object().ok_or_else(|| {
        invalid_shape(
            OpenAiProfile::ChatCompletions,
            "response_format must be an object",
        )
    })?;
    match response_format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let schema = response_format
                .get("json_schema")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    invalid_shape(
                        OpenAiProfile::ChatCompletions,
                        "json_schema response formats require json_schema",
                    )
                })?;
            Ok(Some(JsonSchemaOutputIntent {
                name: optional_string(schema, "name"),
                description: optional_string(schema, "description"),
                schema: schema.get("schema").cloned().ok_or_else(|| {
                    invalid_shape(
                        OpenAiProfile::ChatCompletions,
                        "json_schema response formats require schema",
                    )
                })?,
                enforcement: if schema.get("strict").and_then(Value::as_bool) == Some(true) {
                    OutputSchemaEnforcement::Required
                } else {
                    OutputSchemaEnforcement::Preferred
                },
            }))
        }
        Some("json_object") => Ok(Some(JsonSchemaOutputIntent {
            name: None,
            description: None,
            schema: json!({"type": "object"}),
            enforcement: OutputSchemaEnforcement::Preferred,
        })),
        _ => Ok(None),
    }
}

fn generation_from_chat(object: &Map<String, Value>) -> GenerationControls {
    GenerationControls {
        temperature: object.get("temperature").and_then(Value::as_f64),
        top_p: object.get("top_p").and_then(Value::as_f64),
        top_k: None,
        max_output_tokens: object
            .get("max_completion_tokens")
            .or_else(|| object.get("max_tokens"))
            .and_then(Value::as_u64),
        stop_sequences: string_or_array(object.get("stop")),
    }
}

fn decode_responses_input(
    input: &Value,
    profile_id: &ProfileId,
    messages: &mut Vec<llm_protocol_core::Message>,
) -> Result<(), CodecError> {
    match input {
        Value::String(text) => messages.push(text_message(ConversationRole::User, text)),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                decode_responses_input_item(item, profile_id, index, messages)?;
            }
        }
        _ => {
            return Err(invalid_shape(
                OpenAiProfile::Responses,
                "input must be a string or array",
            ));
        }
    }
    Ok(())
}

fn decode_responses_input_item(
    value: &Value,
    profile_id: &ProfileId,
    index: usize,
    messages: &mut Vec<llm_protocol_core::Message>,
) -> Result<(), CodecError> {
    if let Some(text) = value.as_str() {
        messages.push(text_message(ConversationRole::User, text));
        return Ok(());
    }
    let object = value.as_object().ok_or_else(|| {
        invalid_shape(
            OpenAiProfile::Responses,
            "input array items must be objects or strings",
        )
    })?;
    match object.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            messages.push(llm_protocol_core::Message {
                role: ConversationRole::Assistant,
                name: None,
                content: vec![ContentPart::ToolCall {
                    id: required_str(object, "call_id")?.to_owned(),
                    name: required_str(object, "name")?.to_owned(),
                    arguments: object
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(parse_json_or_string)
                        .unwrap_or_else(|| Value::Object(Map::new())),
                    extensions: unknown_extensions(
                        object,
                        &["type", "call_id", "name", "arguments"],
                        profile_id,
                        "openai.responses.function_call.unknown_field",
                        &format!("/input/{index}"),
                    ),
                }],
                extensions: Vec::new(),
            });
        }
        Some("function_call_output") => {
            let output = object.get("output").ok_or_else(|| {
                invalid_shape(
                    OpenAiProfile::Responses,
                    "function_call_output requires output",
                )
            })?;
            messages.push(llm_protocol_core::Message {
                role: ConversationRole::Tool,
                name: None,
                content: vec![ContentPart::ToolResult {
                    tool_call_id: required_str(object, "call_id")?.to_owned(),
                    content: decode_responses_content(output, profile_id, index)?,
                    is_error: false,
                    extensions: unknown_extensions(
                        object,
                        &["type", "call_id", "output"],
                        profile_id,
                        "openai.responses.function_output.unknown_field",
                        &format!("/input/{index}"),
                    ),
                }],
                extensions: Vec::new(),
            });
        }
        Some("reasoning") => {
            let summary = object
                .get("summary")
                .and_then(Value::as_array)
                .map(|summary| {
                    summary
                        .iter()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|summary| !summary.is_empty());
            messages.push(llm_protocol_core::Message {
                role: ConversationRole::Assistant,
                name: None,
                content: vec![ContentPart::Reasoning {
                    summary,
                    opaque: object.get("encrypted_content").map(|payload| {
                        opaque_json(
                            profile_id,
                            "openai.responses.reasoning.encrypted_content",
                            SourceLocation::JsonPointer {
                                pointer: format!("/input/{index}/encrypted_content"),
                            },
                            payload.clone(),
                        )
                    }),
                }],
                extensions: unknown_extensions(
                    object,
                    &["type", "summary", "encrypted_content"],
                    profile_id,
                    "openai.responses.reasoning.unknown_field",
                    &format!("/input/{index}"),
                ),
            });
        }
        _ => {
            let role = object
                .get("role")
                .map(|_| required_str(object, "role"))
                .transpose()?
                .map(|role| decode_role(role, OpenAiProfile::Responses))
                .transpose()?
                .unwrap_or(ConversationRole::User);
            let content = object.get("content").ok_or_else(|| {
                invalid_shape(
                    OpenAiProfile::Responses,
                    "message input items require content",
                )
            })?;
            messages.push(llm_protocol_core::Message {
                role,
                name: optional_string(object, "name"),
                content: decode_responses_content(content, profile_id, index)?,
                extensions: unknown_extensions(
                    object,
                    &["type", "role", "name", "content"],
                    profile_id,
                    "openai.responses.message.unknown_field",
                    &format!("/input/{index}"),
                ),
            });
        }
    }
    Ok(())
}

fn decode_responses_content(
    value: &Value,
    profile_id: &ProfileId,
    input_index: usize,
) -> Result<Vec<ContentPart>, CodecError> {
    match value {
        Value::String(text) => Ok(vec![ContentPart::Text {
            text: text.to_owned(),
        }]),
        Value::Array(parts) => {
            let mut content = Vec::new();
            for (part_index, part) in parts.iter().enumerate() {
                let object = part.as_object().ok_or_else(|| {
                    invalid_shape(OpenAiProfile::Responses, "content parts must be objects")
                })?;
                let pointer = format!("/input/{input_index}/content/{part_index}");
                match object.get("type").and_then(Value::as_str) {
                    Some("output_text") if object.get("annotations").is_some() => {
                        let text = object
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        content.push(ContentPart::Text { text });
                        for annotation in object
                            .get("annotations")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            content.push(ContentPart::Citation {
                                reference: annotation.clone(),
                                extensions: Vec::new(),
                            });
                        }
                    }
                    Some("input_text") | Some("output_text") | Some("text") => {
                        let text = object
                            .get("text")
                            .or_else(|| object.get("content"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                invalid_shape(
                                    OpenAiProfile::Responses,
                                    "text content parts require text",
                                )
                            })?;
                        content.push(ContentPart::Text {
                            text: text.to_owned(),
                        });
                    }
                    Some("input_image") | Some("image_url") => {
                        content.push(ContentPart::Image {
                            asset: decode_responses_image(object)?,
                        });
                    }
                    Some("input_file") => {
                        content.push(ContentPart::Document {
                            asset: decode_responses_file(object)?,
                        });
                    }
                    Some("refusal") => content.push(ContentPart::Refusal {
                        text: object
                            .get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        extensions: unknown_extensions(
                            object,
                            &["type", "refusal"],
                            profile_id,
                            "openai.responses.refusal.unknown_field",
                            &pointer,
                        ),
                    }),
                    _ => content.push(ContentPart::Opaque {
                        extension: opaque_json(
                            profile_id,
                            "openai.responses.unknown_content_part",
                            SourceLocation::JsonPointer { pointer },
                            part.clone(),
                        ),
                    }),
                }
            }
            Ok(content)
        }
        _ => Err(invalid_shape(
            OpenAiProfile::Responses,
            "message content must be a string or array",
        )),
    }
}

fn decode_instruction_parts(
    value: &Value,
    _profile_id: &ProfileId,
) -> Result<Vec<ContentPart>, CodecError> {
    let text = value.as_str().ok_or_else(|| {
        invalid_shape(
            OpenAiProfile::Responses,
            "instructions must be a string in the frozen alpha profile",
        )
    })?;
    Ok(vec![ContentPart::Text {
        text: text.to_owned(),
    }])
}

fn decode_responses_image(object: &Map<String, Value>) -> Result<AssetReference, CodecError> {
    let image_url = object
        .get("image_url")
        .or_else(|| object.get("url"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            invalid_shape(
                OpenAiProfile::Responses,
                "image content parts require image_url or url",
            )
        })?;
    Ok(AssetReference {
        reference_type: if image_url.starts_with("data:") {
            AssetReferenceType::Data
        } else {
            AssetReferenceType::Url
        },
        value: image_url.to_owned(),
        media_type: media_type_from_data_url(image_url),
        name: None,
        size_bytes: None,
    })
}

fn decode_responses_file(object: &Map<String, Value>) -> Result<AssetReference, CodecError> {
    let (reference_type, value) =
        if let Some(file_id) = object.get("file_id").and_then(Value::as_str) {
            (AssetReferenceType::ProviderFile, file_id.to_owned())
        } else if let Some(file_url) = object.get("file_url").and_then(Value::as_str) {
            (
                if file_url.starts_with("data:") {
                    AssetReferenceType::Data
                } else {
                    AssetReferenceType::Url
                },
                file_url.to_owned(),
            )
        } else {
            return Err(invalid_shape(
                OpenAiProfile::Responses,
                "input_file parts require file_id or file_url",
            ));
        };
    Ok(AssetReference {
        reference_type,
        value,
        media_type: optional_string(object, "mime_type"),
        name: optional_string(object, "filename"),
        size_bytes: object.get("size_bytes").and_then(Value::as_u64),
    })
}

fn decode_responses_tools(
    object: &Map<String, Value>,
    profile_id: &ProfileId,
) -> Result<Vec<ToolDefinition>, CodecError> {
    let Some(tools) = object.get("tools") else {
        return Ok(Vec::new());
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid_shape(OpenAiProfile::Responses, "tools must be an array"))?;
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let object = tool.as_object().ok_or_else(|| {
                invalid_shape(OpenAiProfile::Responses, "tool definitions must be objects")
            })?;
            if object.get("type").and_then(Value::as_str) != Some("function") {
                return Err(invalid_shape(
                    OpenAiProfile::Responses,
                    "only function tool definitions are in the typed alpha subset",
                ));
            }
            Ok(ToolDefinition {
                name: required_str(object, "name")?.to_owned(),
                description: optional_string(object, "description"),
                input_schema: object
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
                strict: object.get("strict").and_then(Value::as_bool),
                extensions: unknown_extensions(
                    object,
                    &["type", "name", "description", "parameters", "strict"],
                    profile_id,
                    "openai.responses.tool.unknown_field",
                    &format!("/tools/{index}"),
                ),
            })
        })
        .collect()
}

fn decode_responses_output_schema(
    object: &Map<String, Value>,
    _profile_id: &ProfileId,
) -> Result<Option<JsonSchemaOutputIntent>, CodecError> {
    let Some(text) = object.get("text").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(format) = text.get("format").and_then(Value::as_object) else {
        return Ok(None);
    };
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => Ok(Some(JsonSchemaOutputIntent {
            name: optional_string(format, "name"),
            description: optional_string(format, "description"),
            schema: format.get("schema").cloned().ok_or_else(|| {
                invalid_shape(
                    OpenAiProfile::Responses,
                    "json_schema formats require schema",
                )
            })?,
            enforcement: if format.get("strict").and_then(Value::as_bool) == Some(true) {
                OutputSchemaEnforcement::Required
            } else {
                OutputSchemaEnforcement::Preferred
            },
        })),
        Some("json_object") => Ok(Some(JsonSchemaOutputIntent {
            name: None,
            description: None,
            schema: json!({"type": "object"}),
            enforcement: OutputSchemaEnforcement::Preferred,
        })),
        _ => Ok(None),
    }
}

fn generation_from_responses(object: &Map<String, Value>) -> GenerationControls {
    GenerationControls {
        temperature: object.get("temperature").and_then(Value::as_f64),
        top_p: object.get("top_p").and_then(Value::as_f64),
        top_k: None,
        max_output_tokens: object.get("max_output_tokens").and_then(Value::as_u64),
        stop_sequences: Vec::new(),
    }
}

fn decode_responses_output_item(
    value: &Value,
    profile_id: &ProfileId,
    index: usize,
    output: &mut Vec<llm_protocol_core::Message>,
) -> Result<(), CodecError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_shape(OpenAiProfile::Responses, "output items must be objects"))?;
    match object.get("type").and_then(Value::as_str) {
        Some("message") => {
            output.push(llm_protocol_core::Message {
                role: object
                    .get("role")
                    .map(|_| required_str(object, "role"))
                    .transpose()?
                    .map(|role| decode_role(role, OpenAiProfile::Responses))
                    .transpose()?
                    .unwrap_or(ConversationRole::Assistant),
                name: None,
                content: object
                    .get("content")
                    .map(|content| decode_responses_content(content, profile_id, index))
                    .transpose()?
                    .unwrap_or_default(),
                extensions: unknown_extensions(
                    object,
                    &["id", "type", "status", "role", "content"],
                    profile_id,
                    "openai.responses.output_message.unknown_field",
                    &format!("/output/{index}"),
                ),
            });
        }
        Some("function_call") => {
            output.push(llm_protocol_core::Message {
                role: ConversationRole::Assistant,
                name: None,
                content: vec![ContentPart::ToolCall {
                    id: object
                        .get("call_id")
                        .or_else(|| object.get("id"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            invalid_shape(
                                OpenAiProfile::Responses,
                                "function_call output requires call_id or id",
                            )
                        })?
                        .to_owned(),
                    name: required_str(object, "name")?.to_owned(),
                    arguments: object
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(parse_json_or_string)
                        .unwrap_or_else(|| Value::Object(Map::new())),
                    extensions: unknown_extensions(
                        object,
                        &["id", "type", "status", "call_id", "name", "arguments"],
                        profile_id,
                        "openai.responses.output_function_call.unknown_field",
                        &format!("/output/{index}"),
                    ),
                }],
                extensions: Vec::new(),
            });
        }
        Some("reasoning") => {
            output.push(llm_protocol_core::Message {
                role: ConversationRole::Assistant,
                name: None,
                content: vec![ContentPart::Reasoning {
                    summary: object
                        .get("summary")
                        .and_then(Value::as_array)
                        .map(|summary| {
                            summary
                                .iter()
                                .filter_map(|item| item.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .filter(|summary| !summary.is_empty()),
                    opaque: object.get("encrypted_content").map(|payload| {
                        opaque_json(
                            profile_id,
                            "openai.responses.output_reasoning.encrypted_content",
                            SourceLocation::JsonPointer {
                                pointer: format!("/output/{index}/encrypted_content"),
                            },
                            payload.clone(),
                        )
                    }),
                }],
                extensions: unknown_extensions(
                    object,
                    &["id", "type", "status", "summary", "encrypted_content"],
                    profile_id,
                    "openai.responses.output_reasoning.unknown_field",
                    &format!("/output/{index}"),
                ),
            });
        }
        _ => output.push(llm_protocol_core::Message {
            role: ConversationRole::Assistant,
            name: None,
            content: Vec::new(),
            extensions: vec![opaque_json(
                profile_id,
                "openai.responses.output_unknown_item",
                SourceLocation::JsonPointer {
                    pointer: format!("/output/{index}"),
                },
                value.clone(),
            )],
        }),
    }
    Ok(())
}

fn decode_chat_usage(value: &Value) -> Option<Usage> {
    let object = value.as_object()?;
    Some(Usage {
        input_tokens: object.get("prompt_tokens").and_then(Value::as_u64),
        output_tokens: object.get("completion_tokens").and_then(Value::as_u64),
        reasoning_tokens: object
            .get("completion_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        cache_read_tokens: object
            .get("prompt_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_tokens: None,
    })
}

fn decode_responses_usage(value: &Value) -> Option<Usage> {
    let object = value.as_object()?;
    Some(Usage {
        input_tokens: object.get("input_tokens").and_then(Value::as_u64),
        output_tokens: object.get("output_tokens").and_then(Value::as_u64),
        reasoning_tokens: object
            .get("output_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64),
        cache_read_tokens: object
            .get("input_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_tokens: None,
    })
}

fn encode_chat_request(
    request: &ProtocolRequest,
    tracker: &mut ConversionTracker,
) -> Result<Map<String, Value>, CodecError> {
    let mut messages = Vec::new();
    if !request.instructions.is_empty() {
        messages.push(json!({
            "role": "system",
            "content": encode_instruction_text(&request.instructions, tracker)?,
        }));
    }
    for (index, message) in request.messages.iter().enumerate() {
        messages.push(encode_chat_message(message, index, tracker)?);
    }
    let mut object = Map::new();
    object.insert("messages".to_owned(), Value::Array(messages));
    Ok(object)
}

fn encode_responses_request(
    request: &ProtocolRequest,
    tracker: &mut ConversionTracker,
) -> Result<Map<String, Value>, CodecError> {
    let mut object = Map::new();
    if !request.instructions.is_empty() {
        object.insert(
            "instructions".to_owned(),
            Value::String(encode_instruction_text(&request.instructions, tracker)?),
        );
    }
    let mut input = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        append_responses_input_message(&mut input, message, index, tracker)?;
    }
    object.insert("input".to_owned(), Value::Array(input));
    if let Some(continuation) = &request.continuation {
        let previous_response_id = match &continuation.extension.payload {
            OpaquePayload::Text(value) => value,
            _ => {
                tracker.unsupported(
                    DiagnosticCode::NonPortableContinuationHandle,
                    Some(continuation.extension.source_location.clone()),
                    "the continuation handle cannot be canonically represented as previous_response_id",
                );
                return Ok(object);
            }
        };
        object.insert(
            "previous_response_id".to_owned(),
            Value::String(previous_response_id.clone()),
        );
    }
    Ok(object)
}

fn encode_chat_message(
    message: &llm_protocol_core::Message,
    index: usize,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    diagnose_extensions(
        &message.extensions,
        OpenAiProfile::ChatCompletions,
        tracker,
        "unknown message fields cannot be canonically represented",
    );
    let mut object = Map::new();
    object.insert(
        "role".to_owned(),
        Value::String(encode_chat_role(message.role).to_owned()),
    );
    if let Some(name) = &message.name {
        object.insert("name".to_owned(), Value::String(name.clone()));
    }
    if message.role == ConversationRole::Tool {
        let mut tool_results = message.content.iter().filter_map(|part| match part {
            ContentPart::ToolResult {
                tool_call_id,
                content,
                ..
            } => Some((tool_call_id, content)),
            _ => None,
        });
        let Some((tool_call_id, content)) = tool_results.next() else {
            tracker.unsupported(
                DiagnosticCode::UnsupportedFeature,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{index}"),
                }),
                "Chat tool messages require a tool result content part",
            );
            return Ok(Value::Object(object));
        };
        if tool_results.next().is_some() {
            tracker.unsupported(
                DiagnosticCode::UnsupportedFeature,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{index}/content"),
                }),
                "a Chat tool message cannot encode multiple tool result parts",
            );
            return Ok(Value::Object(object));
        }
        object.insert(
            "tool_call_id".to_owned(),
            Value::String(tool_call_id.clone()),
        );
        object.insert(
            "content".to_owned(),
            Value::String(content_to_text(content, tracker, "Chat tool output")?),
        );
        return Ok(Value::Object(object));
    }

    let mut regular_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for (part_index, part) in message.content.iter().enumerate() {
        match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
                extensions,
            } => {
                diagnose_extensions(
                    extensions,
                    OpenAiProfile::ChatCompletions,
                    tracker,
                    "unknown tool-call fields cannot be canonically represented",
                );
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": compact_json(arguments),
                    }
                }));
            }
            _ => {
                let encoded = encode_chat_content_part(part, index, part_index, tracker)?;
                if !encoded.is_null() {
                    regular_parts.push(encoded);
                }
            }
        }
    }
    if regular_parts.is_empty() {
        object.insert("content".to_owned(), Value::String(String::new()));
    } else if regular_parts.len() == 1
        && regular_parts[0].get("type") == Some(&Value::String("text".to_owned()))
    {
        object.insert(
            "content".to_owned(),
            regular_parts[0]
                .get("text")
                .cloned()
                .unwrap_or(Value::String(String::new())),
        );
    } else {
        object.insert("content".to_owned(), Value::Array(regular_parts));
    }
    if !tool_calls.is_empty() {
        object.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }
    Ok(Value::Object(object))
}

fn append_responses_input_message(
    input: &mut Vec<Value>,
    message: &llm_protocol_core::Message,
    index: usize,
    tracker: &mut ConversionTracker,
) -> Result<(), CodecError> {
    diagnose_extensions(
        &message.extensions,
        OpenAiProfile::Responses,
        tracker,
        "unknown message fields cannot be canonically represented",
    );
    if message.role == ConversationRole::Tool {
        for part in &message.content {
            let ContentPart::ToolResult {
                tool_call_id,
                content,
                extensions,
                ..
            } = part
            else {
                tracker.unsupported(
                    DiagnosticCode::UnsupportedFeature,
                    Some(SourceLocation::JsonPointer {
                        pointer: format!("/messages/{index}/content"),
                    }),
                    "Responses tool messages require tool result content parts",
                );
                return Ok(());
            };
            diagnose_extensions(
                extensions,
                OpenAiProfile::Responses,
                tracker,
                "unknown tool result fields cannot be canonically represented",
            );
            input.push(json!({
                "type": "function_call_output",
                "call_id": tool_call_id,
                "output": content_to_text(content, tracker, "Responses function output")?,
            }));
        }
        return Ok(());
    }

    let mut regular_content = Vec::new();
    let mut has_tool_call = false;
    for (part_index, part) in message.content.iter().enumerate() {
        match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
                extensions,
            } => {
                has_tool_call = true;
                diagnose_extensions(
                    extensions,
                    OpenAiProfile::Responses,
                    tracker,
                    "unknown tool-call fields cannot be canonically represented",
                );
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": compact_json(arguments),
                }));
            }
            ContentPart::Reasoning { summary, opaque } => {
                if let Some(opaque) = opaque {
                    diagnose_extensions(
                        std::slice::from_ref(opaque),
                        OpenAiProfile::Responses,
                        tracker,
                        "opaque reasoning payload cannot be canonically represented",
                    );
                }
                input.push(json!({
                    "type": "reasoning",
                    "summary": summary.as_ref().map(|summary| vec![json!({"type": "summary_text", "text": summary})]).unwrap_or_default(),
                }));
            }
            _ => {
                let encoded =
                    encode_responses_content_part(part, message.role, index, part_index, tracker)?;
                if !encoded.is_null() {
                    regular_content.push(encoded);
                }
            }
        }
    }
    if has_tool_call
        && regular_content.len() == 1
        && regular_content[0].get("text").and_then(Value::as_str) == Some("")
    {
        regular_content.clear();
    }
    if !regular_content.is_empty() {
        let mut item = Map::new();
        item.insert(
            "role".to_owned(),
            Value::String(encode_responses_role(message.role).to_owned()),
        );
        if let Some(name) = &message.name {
            item.insert("name".to_owned(), Value::String(name.clone()));
        }
        item.insert("content".to_owned(), Value::Array(regular_content));
        input.push(Value::Object(item));
    }
    Ok(())
}

fn encode_chat_content_part(
    part: &ContentPart,
    message_index: usize,
    part_index: usize,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"type": "text", "text": text})),
        ContentPart::Image { asset } => Ok(json!({
            "type": "image_url",
            "image_url": {"url": asset.value},
        })),
        ContentPart::Document { .. } => {
            tracker.unsupported(
                DiagnosticCode::UnsupportedFeature,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{message_index}/content/{part_index}"),
                }),
                "Chat Completions has no typed document input representation in this alpha profile",
            );
            Ok(Value::Null)
        }
        ContentPart::Reasoning { summary, opaque } => {
            if opaque.is_some() {
                tracker.lossy(
                    DiagnosticCode::NonPortableOpaqueExtension,
                    Some(SourceLocation::JsonPointer {
                        pointer: format!("/messages/{message_index}/content/{part_index}"),
                    }),
                    "opaque reasoning payload cannot be represented by Chat Completions",
                );
            }
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{message_index}/content/{part_index}"),
                }),
                "typed reasoning was represented as Chat text",
            );
            Ok(json!({"type": "text", "text": summary.clone().unwrap_or_default()}))
        }
        ContentPart::Citation {
            reference,
            extensions,
        } => {
            diagnose_extensions(
                extensions,
                OpenAiProfile::ChatCompletions,
                tracker,
                "unknown citation fields cannot be canonically represented",
            );
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{message_index}/content/{part_index}"),
                }),
                "typed citation metadata was omitted from Chat content",
            );
            let _ = reference;
            Ok(Value::Null)
        }
        ContentPart::Refusal { text, extensions } => {
            diagnose_extensions(
                extensions,
                OpenAiProfile::ChatCompletions,
                tracker,
                "unknown refusal fields cannot be canonically represented",
            );
            Ok(json!({"type": "refusal", "refusal": text}))
        }
        ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. } => {
            tracker.unsupported(
                DiagnosticCode::UnsupportedFeature,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{message_index}/content/{part_index}"),
                }),
                "tool call and result parts have message-level OpenAI representations",
            );
            Ok(Value::Null)
        }
        ContentPart::Opaque { extension } => {
            diagnose_extensions(
                std::slice::from_ref(extension),
                OpenAiProfile::ChatCompletions,
                tracker,
                "opaque content parts require exact same-profile raw replay",
            );
            Ok(Value::Null)
        }
    }
}

fn encode_responses_content_part(
    part: &ContentPart,
    role: ConversationRole,
    message_index: usize,
    part_index: usize,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    match part {
        ContentPart::Text { text } => Ok(json!({
            "type": if role == ConversationRole::Assistant { "output_text" } else { "input_text" },
            "text": text,
        })),
        ContentPart::Image { asset } => Ok(json!({
            "type": "input_image",
            "image_url": asset.value,
        })),
        ContentPart::Document { asset } => {
            let mut file = Map::new();
            file.insert("type".to_owned(), Value::String("input_file".to_owned()));
            match asset.reference_type {
                AssetReferenceType::ProviderFile => {
                    file.insert("file_id".to_owned(), Value::String(asset.value.clone()));
                }
                AssetReferenceType::Url | AssetReferenceType::Data => {
                    file.insert("file_url".to_owned(), Value::String(asset.value.clone()));
                }
            }
            if let Some(name) = &asset.name {
                file.insert("filename".to_owned(), Value::String(name.clone()));
            }
            if let Some(media_type) = &asset.media_type {
                file.insert("mime_type".to_owned(), Value::String(media_type.clone()));
            }
            Ok(Value::Object(file))
        }
        ContentPart::Reasoning { summary, opaque } => {
            if let Some(opaque) = opaque {
                diagnose_extensions(
                    std::slice::from_ref(opaque),
                    OpenAiProfile::Responses,
                    tracker,
                    "opaque reasoning payload cannot be canonically represented",
                );
            }
            Ok(json!({
                "type": "reasoning",
                "summary": summary.as_ref().map(|summary| vec![json!({"type": "summary_text", "text": summary})]).unwrap_or_default(),
            }))
        }
        ContentPart::Citation {
            reference,
            extensions,
        } => {
            diagnose_extensions(
                extensions,
                OpenAiProfile::Responses,
                tracker,
                "unknown citation fields cannot be canonically represented",
            );
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{message_index}/content/{part_index}"),
                }),
                "typed citation metadata was omitted from request content",
            );
            let _ = reference;
            Ok(Value::Null)
        }
        ContentPart::Refusal { text, extensions } => {
            diagnose_extensions(
                extensions,
                OpenAiProfile::Responses,
                tracker,
                "unknown refusal fields cannot be canonically represented",
            );
            Ok(json!({"type": "refusal", "refusal": text}))
        }
        ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. } => {
            tracker.unsupported(
                DiagnosticCode::UnsupportedFeature,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{message_index}/content/{part_index}"),
                }),
                "tool call and result parts have item-level Responses representations",
            );
            Ok(Value::Null)
        }
        ContentPart::Opaque { extension } => {
            diagnose_extensions(
                std::slice::from_ref(extension),
                OpenAiProfile::Responses,
                tracker,
                "opaque content parts require exact same-profile raw replay",
            );
            Ok(Value::Null)
        }
    }
}

fn encode_instruction_text(
    instructions: &[ContentPart],
    tracker: &mut ConversionTracker,
) -> Result<String, CodecError> {
    content_to_text(instructions, tracker, "instructions")
}

fn content_to_text(
    content: &[ContentPart],
    tracker: &mut ConversionTracker,
    context: &'static str,
) -> Result<String, CodecError> {
    let mut text = String::new();
    for part in content {
        match part {
            ContentPart::Text { text: part_text }
            | ContentPart::Refusal {
                text: part_text, ..
            } => {
                text.push_str(part_text);
            }
            ContentPart::Opaque { extension } => {
                diagnose_extensions(
                    std::slice::from_ref(extension),
                    OpenAiProfile::Responses,
                    tracker,
                    "opaque content parts require exact same-profile raw replay",
                );
                tracker.unsupported(
                    DiagnosticCode::UnsupportedFeature,
                    Some(extension.source_location.clone()),
                    "the target field only accepts text content in the typed alpha subset",
                );
                let _ = context;
                return Ok(text);
            }
            _ => {
                tracker.unsupported(
                    DiagnosticCode::UnsupportedFeature,
                    None,
                    "the target field only accepts text content in the typed alpha subset",
                );
                let _ = context;
                return Ok(text);
            }
        }
    }
    Ok(text)
}

fn insert_generation(
    object: &mut Map<String, Value>,
    generation: &GenerationControls,
    target: OpenAiProfile,
) {
    if let Some(temperature) = generation.temperature {
        object.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = generation.top_p {
        object.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(max_output_tokens) = generation.max_output_tokens {
        object.insert(
            match target {
                OpenAiProfile::ChatCompletions => "max_completion_tokens",
                OpenAiProfile::Responses => "max_output_tokens",
            }
            .to_owned(),
            json!(max_output_tokens),
        );
    }
    if target == OpenAiProfile::ChatCompletions && !generation.stop_sequences.is_empty() {
        object.insert(
            "stop".to_owned(),
            Value::Array(
                generation
                    .stop_sequences
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
}

fn insert_openai_cache_intent(
    object: &mut Map<String, Value>,
    request: &ProtocolRequest,
    _target: OpenAiProfile,
    tracker: &mut ConversionTracker,
) {
    if let Some(llm_protocol_core::CacheIntent::OpenAi(OpenAiCacheIntent {
        request_cache_key,
        retention,
    })) = &request.cache_intent
    {
        if let Some(request_cache_key) = request_cache_key {
            object.insert(
                "prompt_cache_key".to_owned(),
                Value::String(request_cache_key.clone()),
            );
        }
        if let Some(retention) = retention {
            object.insert(
                "prompt_cache_retention".to_owned(),
                Value::String(retention.clone()),
            );
        }
    } else if request.cache_intent.is_some() {
        tracker.lossy(
            DiagnosticCode::NonPortableCacheIntent,
            None,
            "non-OpenAI cache directives were not synthesized for the OpenAI target profile",
        );
    }
}

fn insert_output_schema(
    object: &mut Map<String, Value>,
    output_schema: Option<&JsonSchemaOutputIntent>,
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
) -> Result<(), CodecError> {
    let Some(output_schema) = output_schema else {
        return Ok(());
    };
    let Some(name) = output_schema
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
    else {
        tracker.unsupported(
            DiagnosticCode::UnsupportedFeature,
            Some(SourceLocation::JsonPointer {
                pointer: "/output_schema/name".to_owned(),
            }),
            "OpenAI JSON Schema output formats require a non-empty provider schema name",
        );
        return Ok(());
    };
    let strict = matches!(output_schema.enforcement, OutputSchemaEnforcement::Required);
    match target {
        OpenAiProfile::ChatCompletions => {
            object.insert(
                "response_format".to_owned(),
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": name,
                        "description": output_schema.description,
                        "schema": output_schema.schema,
                        "strict": strict,
                    }
                }),
            );
        }
        OpenAiProfile::Responses => {
            object.insert(
                "text".to_owned(),
                json!({
                    "format": {
                        "type": "json_schema",
                        "name": name,
                        "description": output_schema.description,
                        "schema": output_schema.schema,
                        "strict": strict,
                    }
                }),
            );
        }
    }
    Ok(())
}

fn insert_tools(
    object: &mut Map<String, Value>,
    tools: &[ToolDefinition],
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
) -> Result<(), CodecError> {
    if tools.is_empty() {
        return Ok(());
    }
    let mut encoded = Vec::with_capacity(tools.len());
    for tool in tools {
        diagnose_extensions(
            &tool.extensions,
            target,
            tracker,
            "unknown tool definition fields cannot be canonically represented",
        );
        match target {
            OpenAiProfile::ChatCompletions => encoded.push(json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": tool.strict,
                }
            })),
            OpenAiProfile::Responses => encoded.push(json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": tool.strict,
            })),
        }
    }
    object.insert("tools".to_owned(), Value::Array(encoded));
    Ok(())
}

fn encode_chat_response(
    response: &ProtocolResponse,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    let mut merged = llm_protocol_core::Message {
        role: ConversationRole::Assistant,
        name: None,
        content: Vec::new(),
        extensions: Vec::new(),
    };
    for message in &response.output {
        if message.role != ConversationRole::Assistant {
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                None,
                "a non-assistant response message was encoded into the Chat assistant choice",
            );
        }
        merged.content.extend(message.content.clone());
        merged.extensions.extend(message.extensions.clone());
    }
    let finish_reason = if merged
        .content
        .iter()
        .any(|part| matches!(part, ContentPart::ToolCall { .. }))
    {
        FinishReason::TOOL_CALLS
    } else {
        response.finish_reason.as_str()
    };
    let choices = if response.output.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "index": 0,
            "message": encode_chat_message(&merged, 0, tracker)?,
            "finish_reason": encode_chat_finish_reason(finish_reason),
        })]
    };
    Ok(json!({
        "id": response.id,
        "object": "chat.completion",
        "model": response.model,
        "choices": choices,
        "usage": response.usage.as_ref().map(encode_chat_usage),
    }))
}

fn encode_responses_response(
    response: &ProtocolResponse,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    let mut output = Vec::new();
    for (index, message) in response.output.iter().enumerate() {
        append_responses_output_message(&mut output, message, index, tracker)?;
    }
    let status = if let Some(handle) = &response.continuation {
        match handle.extension.namespace.as_str() {
            RESPONSES_QUEUED_HANDLE_NAMESPACE => "queued",
            RESPONSES_IN_PROGRESS_HANDLE_NAMESPACE => "in_progress",
            _ => {
                tracker.lossy(
                    DiagnosticCode::NonPortableContinuationHandle,
                    Some(handle.extension.source_location.clone()),
                    "the Responses continuation handle does not identify a canonical response lifecycle state",
                );
                encode_responses_finish_status(response.finish_reason.as_str())
            }
        }
    } else {
        encode_responses_finish_status(response.finish_reason.as_str())
    };
    Ok(json!({
        "id": response.id,
        "object": "response",
        "status": status,
        "model": response.model,
        "output": output,
        "usage": response.usage.as_ref().map(encode_responses_usage),
    }))
}

fn encode_responses_finish_status(finish_reason: &str) -> &'static str {
    match finish_reason {
        FinishReason::LENGTH => "incomplete",
        FinishReason::ERROR => "failed",
        _ => "completed",
    }
}

fn append_responses_output_message(
    output: &mut Vec<Value>,
    message: &llm_protocol_core::Message,
    index: usize,
    tracker: &mut ConversionTracker,
) -> Result<(), CodecError> {
    let mut regular_content = Vec::new();
    let mut deferred_items = Vec::new();
    for (part_index, part) in message.content.iter().enumerate() {
        match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
                extensions,
            } => {
                diagnose_extensions(
                    extensions,
                    OpenAiProfile::Responses,
                    tracker,
                    "unknown tool-call fields cannot be canonically represented",
                );
                deferred_items.push(json!({
                    "id": id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": id,
                    "name": name,
                    "arguments": compact_json(arguments),
                }));
            }
            ContentPart::Reasoning { summary, opaque } => {
                if let Some(opaque) = opaque {
                    diagnose_extensions(
                        std::slice::from_ref(opaque),
                        OpenAiProfile::Responses,
                        tracker,
                        "opaque reasoning payload cannot be canonically represented",
                    );
                }
                deferred_items.push(json!({
                    "id": format!("rsn_{index}_{part_index}"),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": summary.as_ref().map(|summary| vec![json!({"type": "summary_text", "text": summary})]).unwrap_or_default(),
                }));
            }
            _ => regular_content.push(encode_responses_content_part(
                part,
                message.role,
                index,
                part_index,
                tracker,
            )?),
        }
    }
    if !regular_content.is_empty() {
        output.push(json!({
            "id": format!("msg_{index}"),
            "type": "message",
            "status": "completed",
            "role": encode_responses_role(message.role),
            "content": regular_content,
        }));
    }
    output.extend(deferred_items);
    Ok(())
}

fn encode_chat_usage(usage: &Usage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "prompt_tokens_details": {"cached_tokens": usage.cache_read_tokens},
        "completion_tokens": usage.output_tokens,
        "completion_tokens_details": {"reasoning_tokens": usage.reasoning_tokens},
        "total_tokens": match (usage.input_tokens, usage.output_tokens) {
            (Some(input), Some(output)) => input.checked_add(output),
            _ => None,
        },
    })
}

fn encode_responses_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "input_tokens_details": {"cached_tokens": usage.cache_read_tokens},
        "output_tokens": usage.output_tokens,
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens},
        "total_tokens": match (usage.input_tokens, usage.output_tokens) {
            (Some(input), Some(output)) => input.checked_add(output),
            _ => None,
        },
    })
}

fn decode_chat_stream(frames: Vec<SseFrame>, profile_id: &ProfileId) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::RequestStarted];
    let mut started_parts = BTreeSet::new();
    let mut terminal = false;
    let mut event_index = 0_u64;
    for frame in frames {
        let event_name = frame.event.clone();
        if frame.data == "[DONE]" {
            close_stream_parts(&mut events, &mut started_parts, None);
            if !terminal {
                events.push(StreamEvent::Terminal {
                    finish_reason: FinishReason::new(FinishReason::STOP)
                        .expect("stop is non-empty"),
                });
                terminal = true;
            }
            event_index += 1;
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
            events.push(StreamEvent::Opaque {
                extension: opaque_text(
                    profile_id,
                    "openai.chat.sse.unparseable_frame",
                    SourceLocation::SseEvent {
                        index: event_index,
                        event: event_name,
                    },
                    &frame.data,
                ),
            });
            event_index += 1;
            continue;
        };
        if value.get("error").is_some() {
            let retained = RetainedWire {
                profile_id: profile_id.clone(),
                status: 500,
                body_kind: ProtocolBodyKind::Json,
                protocol_headers: Vec::new(),
                body: serde_json::to_vec(&value).expect("error JSON serializes"),
            };
            if let Ok(result) = decode_json(OpenAiProfile::ChatCompletions, &retained)
                && let Some(OpenAiPayload::Error(error)) = result.output
            {
                events.push(StreamEvent::Error { error });
                terminal = true;
            }
            event_index += 1;
            continue;
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            if let Some(usage) = value.get("usage").and_then(decode_chat_usage) {
                events.push(StreamEvent::Usage { usage });
            } else {
                events.push(StreamEvent::Opaque {
                    extension: opaque_json(
                        profile_id,
                        "openai.chat.sse.unknown_frame",
                        SourceLocation::SseEvent {
                            index: event_index,
                            event: event_name,
                        },
                        value,
                    ),
                });
            }
            event_index += 1;
            continue;
        };
        let message_id = optional_string(&value.as_object().cloned().unwrap_or_default(), "id");
        for choice in choices {
            let choice_index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
            let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
                continue;
            };
            if delta.get("role").and_then(Value::as_str).is_some() {
                events.push(StreamEvent::MessageStarted {
                    message_id: message_id.clone(),
                });
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                let key = (choice_index, OutputPartType::Text);
                if started_parts.insert(key) {
                    events.push(StreamEvent::OutputPartStarted {
                        message_id: message_id.clone(),
                        part_index: usize::try_from(choice_index).unwrap_or(0),
                        part_type: OutputPartType::Text,
                    });
                }
                events.push(StreamEvent::TextDelta {
                    text: text.to_owned(),
                });
            }
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
            {
                events.push(StreamEvent::ReasoningDelta {
                    text: reasoning.to_owned(),
                });
            }
            if let Some(refusal) = delta.get("refusal").and_then(Value::as_str) {
                events.push(StreamEvent::RefusalPart {
                    text: refusal.to_owned(),
                    extensions: Vec::new(),
                });
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let tool_index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let call_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("call_{tool_index}"));
                    let name = tool_call
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let arguments_delta = tool_call
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let key = (tool_index, OutputPartType::ToolCall);
                    if started_parts.insert(key) {
                        events.push(StreamEvent::OutputPartStarted {
                            message_id: message_id.clone(),
                            part_index: usize::try_from(tool_index).unwrap_or(0),
                            part_type: OutputPartType::ToolCall,
                        });
                    }
                    events.push(StreamEvent::ToolCallDelta {
                        call_id,
                        name,
                        arguments_delta,
                    });
                }
            }
            if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
                close_stream_parts(&mut events, &mut started_parts, message_id.clone());
                events.push(StreamEvent::Terminal {
                    finish_reason: FinishReason::new(normalize_chat_finish_reason(finish_reason))
                        .expect("normal finish reason is non-empty"),
                });
                terminal = true;
            }
        }
        if let Some(usage) = value.get("usage").and_then(decode_chat_usage) {
            events.push(StreamEvent::Usage { usage });
        }
        event_index += 1;
    }
    if !terminal {
        close_stream_parts(&mut events, &mut started_parts, None);
    }
    events
}

fn decode_responses_stream(frames: Vec<SseFrame>, profile_id: &ProfileId) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut request_started = false;
    let mut current_message_id = None;
    let mut open_parts = BTreeSet::new();
    let mut terminal = false;
    let mut event_index = 0_u64;
    for frame in frames {
        let event_name = frame.event.clone();
        if frame.data == "[DONE]" {
            close_stream_parts(&mut events, &mut open_parts, current_message_id.clone());
            if !terminal {
                events.push(StreamEvent::Terminal {
                    finish_reason: FinishReason::new(FinishReason::STOP)
                        .expect("stop is non-empty"),
                });
                terminal = true;
            }
            event_index += 1;
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
            events.push(StreamEvent::Opaque {
                extension: opaque_text(
                    profile_id,
                    "openai.responses.sse.unparseable_frame",
                    SourceLocation::SseEvent {
                        index: event_index,
                        event: event_name,
                    },
                    &frame.data,
                ),
            });
            event_index += 1;
            continue;
        };
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .or(event_name.as_deref());
        match event_type {
            Some("response.created") | Some("response.in_progress") => {
                if !request_started {
                    events.push(StreamEvent::RequestStarted);
                    request_started = true;
                }
            }
            Some("response.output_item.added") => {
                let item = value.get("item").and_then(Value::as_object);
                let item_id = item
                    .and_then(|item| item.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if item
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("message")
                {
                    current_message_id = item_id.clone();
                    events.push(StreamEvent::MessageStarted {
                        message_id: item_id,
                    });
                } else if item
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("function_call")
                {
                    let output_index = value
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    open_parts.insert((output_index, OutputPartType::ToolCall));
                    events.push(StreamEvent::OutputPartStarted {
                        message_id: item_id,
                        part_index: usize::try_from(output_index).unwrap_or(0),
                        part_type: OutputPartType::ToolCall,
                    });
                }
            }
            Some("response.content_part.added") => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let content_index = value
                    .get("content_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let part_type = match value
                    .get("part")
                    .and_then(Value::as_object)
                    .and_then(|part| part.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("reasoning") => OutputPartType::Reasoning,
                    Some("refusal") => OutputPartType::Refusal,
                    _ => OutputPartType::Text,
                };
                open_parts.insert((content_index, part_type));
                events.push(StreamEvent::OutputPartStarted {
                    message_id: value
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| current_message_id.clone()),
                    part_index: usize::try_from(output_index).unwrap_or(0),
                    part_type,
                });
            }
            Some("response.output_text.delta") => {
                events.push(StreamEvent::TextDelta {
                    text: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
            Some("response.reasoning_summary_text.delta") | Some("response.reasoning.delta") => {
                events.push(StreamEvent::ReasoningDelta {
                    text: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                });
            }
            Some("response.refusal.delta") => events.push(StreamEvent::RefusalPart {
                text: value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                extensions: Vec::new(),
            }),
            Some("response.output_text.annotation.added") => {
                if let Some(annotation) = value.get("annotation") {
                    events.push(StreamEvent::CitationPart {
                        reference: annotation.clone(),
                        extensions: Vec::new(),
                    });
                }
            }
            Some("response.function_call_arguments.delta") => {
                events.push(StreamEvent::ToolCallDelta {
                    call_id: value
                        .get("call_id")
                        .or_else(|| value.get("item_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("call_unknown")
                        .to_owned(),
                    name: None,
                    arguments_delta: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
            }
            Some("response.content_part.done") | Some("response.output_item.done") => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                events.push(StreamEvent::OutputPartEnded {
                    message_id: value
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| current_message_id.clone()),
                    part_index: usize::try_from(output_index).unwrap_or(0),
                });
            }
            Some("response.completed") | Some("response.incomplete") => {
                if let Some(usage) = value
                    .get("response")
                    .and_then(|response| response.get("usage"))
                    .and_then(decode_responses_usage)
                {
                    events.push(StreamEvent::Usage { usage });
                }
                close_stream_parts(&mut events, &mut open_parts, current_message_id.clone());
                events.push(StreamEvent::Terminal {
                    finish_reason: FinishReason::new(
                        if event_type == Some("response.incomplete") {
                            FinishReason::LENGTH
                        } else {
                            FinishReason::STOP
                        },
                    )
                    .expect("normal finish reason is non-empty"),
                });
                terminal = true;
            }
            Some("response.failed") | Some("response.cancelled") | Some("error") => {
                let error_object = value
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .or_else(|| value.get("error"))
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                events.push(StreamEvent::Error {
                    error: ProtocolError {
                        category: ErrorCategory::Server,
                        code: optional_string(&error_object, "code")
                            .unwrap_or_else(|| "unknown_error".to_owned()),
                        message: optional_string(&error_object, "message")
                            .unwrap_or_else(|| "OpenAI response failed".to_owned()),
                        retry_after_ms: None,
                        param: optional_string(&error_object, "param"),
                        extensions: unknown_extensions(
                            &error_object,
                            &["code", "message", "param", "type"],
                            profile_id,
                            "openai.responses.sse.error_unknown_field",
                            "",
                        ),
                    },
                });
                terminal = true;
            }
            _ => events.push(StreamEvent::Opaque {
                extension: opaque_json(
                    profile_id,
                    "openai.responses.sse.unknown_event",
                    SourceLocation::SseEvent {
                        index: event_index,
                        event: event_name,
                    },
                    value,
                ),
            }),
        }
        event_index += 1;
    }
    if !request_started && !events.is_empty() {
        events.insert(0, StreamEvent::RequestStarted);
    }
    if !terminal {
        close_stream_parts(&mut events, &mut open_parts, current_message_id);
    }
    events
}

fn encode_stream(
    events: &[StreamEvent],
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
) -> String {
    match target {
        OpenAiProfile::ChatCompletions => encode_chat_stream(events, tracker),
        OpenAiProfile::Responses => encode_responses_stream(events, tracker),
    }
}

fn encode_chat_stream(events: &[StreamEvent], tracker: &mut ConversionTracker) -> String {
    let mut output = String::new();
    let mut tool_indices = BTreeMap::new();
    let mut saw_terminal = false;
    for event in events {
        match event {
            StreamEvent::RequestStarted
            | StreamEvent::OutputPartStarted { .. }
            | StreamEvent::OutputPartEnded { .. } => {}
            StreamEvent::MessageStarted { .. } => {
                push_sse_data(
                    &mut output,
                    json!({"id": "chatcmpl_alpha", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]}),
                );
            }
            StreamEvent::TextDelta { text } => push_sse_data(
                &mut output,
                json!({"id": "chatcmpl_alpha", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}),
            ),
            StreamEvent::ReasoningDelta { text } => {
                tracker.lossy(
                    DiagnosticCode::SemanticChange,
                    None,
                    "typed reasoning stream deltas were encoded as a Chat provider extension",
                );
                push_sse_data(
                    &mut output,
                    json!({"id": "chatcmpl_alpha", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"reasoning_content": text}, "finish_reason": null}]}),
                );
            }
            StreamEvent::RefusalPart { text, extensions } => {
                diagnose_extensions(
                    extensions,
                    OpenAiProfile::ChatCompletions,
                    tracker,
                    "unknown refusal fields cannot be canonically represented",
                );
                push_sse_data(
                    &mut output,
                    json!({"id": "chatcmpl_alpha", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"refusal": text}, "finish_reason": null}]}),
                );
            }
            StreamEvent::CitationPart {
                reference,
                extensions,
            } => {
                diagnose_extensions(
                    extensions,
                    OpenAiProfile::ChatCompletions,
                    tracker,
                    "unknown citation fields cannot be canonically represented",
                );
                tracker.lossy(
                    DiagnosticCode::SemanticChange,
                    None,
                    "typed citation stream data was omitted from Chat chunks",
                );
                let _ = reference;
            }
            StreamEvent::ToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => {
                let next_index = tool_indices.len();
                let index = *tool_indices.entry(call_id.clone()).or_insert(next_index);
                let mut function = Map::new();
                if let Some(name) = name {
                    function.insert("name".to_owned(), Value::String(name.clone()));
                }
                if !arguments_delta.is_empty() {
                    function.insert(
                        "arguments".to_owned(),
                        Value::String(arguments_delta.clone()),
                    );
                }
                push_sse_data(
                    &mut output,
                    json!({"id": "chatcmpl_alpha", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "id": call_id, "type": "function", "function": function}]}, "finish_reason": null}]}),
                );
            }
            StreamEvent::Usage { usage } => push_sse_data(
                &mut output,
                json!({"id": "chatcmpl_alpha", "object": "chat.completion.chunk", "choices": [], "usage": encode_chat_usage(usage)}),
            ),
            StreamEvent::Terminal { finish_reason } => {
                push_sse_data(
                    &mut output,
                    json!({"id": "chatcmpl_alpha", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": encode_chat_finish_reason(finish_reason.as_str())}]}),
                );
                output.push_str("data: [DONE]\n\n");
                saw_terminal = true;
            }
            StreamEvent::Error { error } => {
                push_sse_data(
                    &mut output,
                    encode_error(error, OpenAiProfile::ChatCompletions, tracker),
                );
                saw_terminal = true;
            }
            StreamEvent::Opaque { extension } => {
                diagnose_extensions(
                    std::slice::from_ref(extension),
                    OpenAiProfile::ChatCompletions,
                    tracker,
                    "opaque stream frames require exact same-profile raw replay",
                );
            }
        }
    }
    if !saw_terminal {
        output.push_str("data: [DONE]\n\n");
    }
    output
}

fn encode_responses_stream(events: &[StreamEvent], tracker: &mut ConversionTracker) -> String {
    let mut output = String::new();
    let mut usage = events.iter().rev().find_map(|event| match event {
        StreamEvent::Usage { usage } => Some(usage.clone()),
        _ => None,
    });
    let mut message_id = "msg_alpha".to_owned();
    let mut started_response = false;
    let mut started_tool_calls = BTreeSet::new();
    let mut saw_terminal = false;
    for event in events {
        match event {
            StreamEvent::RequestStarted => {
                if !started_response {
                    push_sse_event(
                        &mut output,
                        "response.created",
                        json!({"type": "response.created", "response": {"id": "resp_alpha", "object": "response", "status": "in_progress", "output": []}}),
                    );
                    started_response = true;
                }
            }
            StreamEvent::MessageStarted {
                message_id: event_message_id,
            } => {
                if let Some(event_message_id) = event_message_id {
                    message_id = event_message_id.clone();
                }
                push_sse_event(
                    &mut output,
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": message_id, "type": "message", "status": "in_progress", "role": "assistant", "content": []}}),
                );
            }
            StreamEvent::OutputPartStarted {
                part_index,
                part_type,
                ..
            } => {
                if *part_type == OutputPartType::Text {
                    push_sse_event(
                        &mut output,
                        "response.content_part.added",
                        json!({"type": "response.content_part.added", "item_id": message_id, "output_index": 0, "content_index": part_index, "part": {"type": "output_text", "text": "", "annotations": []}}),
                    );
                }
            }
            StreamEvent::OutputPartEnded { .. } => {}
            StreamEvent::TextDelta { text } => push_sse_event(
                &mut output,
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "item_id": message_id, "output_index": 0, "content_index": 0, "delta": text}),
            ),
            StreamEvent::ReasoningDelta { text } => push_sse_event(
                &mut output,
                "response.reasoning_summary_text.delta",
                json!({"type": "response.reasoning_summary_text.delta", "item_id": message_id, "output_index": 0, "delta": text}),
            ),
            StreamEvent::RefusalPart { text, extensions } => {
                diagnose_extensions(
                    extensions,
                    OpenAiProfile::Responses,
                    tracker,
                    "unknown refusal fields cannot be canonically represented",
                );
                push_sse_event(
                    &mut output,
                    "response.refusal.delta",
                    json!({"type": "response.refusal.delta", "item_id": message_id, "output_index": 0, "delta": text}),
                );
            }
            StreamEvent::CitationPart {
                reference,
                extensions,
            } => {
                diagnose_extensions(
                    extensions,
                    OpenAiProfile::Responses,
                    tracker,
                    "unknown citation fields cannot be canonically represented",
                );
                push_sse_event(
                    &mut output,
                    "response.output_text.annotation.added",
                    json!({"type": "response.output_text.annotation.added", "item_id": message_id, "output_index": 0, "annotation": reference}),
                );
            }
            StreamEvent::ToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => {
                if started_tool_calls.insert(call_id.clone()) {
                    push_sse_event(
                        &mut output,
                        "response.output_item.added",
                        json!({"type": "response.output_item.added", "output_index": started_tool_calls.len(), "item": {"id": call_id, "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": ""}}),
                    );
                }
                push_sse_event(
                    &mut output,
                    "response.function_call_arguments.delta",
                    json!({"type": "response.function_call_arguments.delta", "item_id": call_id, "call_id": call_id, "output_index": started_tool_calls.len(), "delta": arguments_delta}),
                );
            }
            StreamEvent::Usage { usage: event_usage } => usage = Some(event_usage.clone()),
            StreamEvent::Terminal { finish_reason } => {
                push_sse_event(
                    &mut output,
                    if finish_reason.as_str() == FinishReason::LENGTH {
                        "response.incomplete"
                    } else {
                        "response.completed"
                    },
                    json!({
                        "type": if finish_reason.as_str() == FinishReason::LENGTH { "response.incomplete" } else { "response.completed" },
                        "response": {
                            "id": "resp_alpha",
                            "object": "response",
                            "status": if finish_reason.as_str() == FinishReason::LENGTH { "incomplete" } else { "completed" },
                            "output": [],
                            "usage": usage.as_ref().map(encode_responses_usage),
                        }
                    }),
                );
                saw_terminal = true;
            }
            StreamEvent::Error { error } => {
                push_sse_event(
                    &mut output,
                    "response.failed",
                    json!({"type": "response.failed", "response": {"id": "resp_alpha", "status": "failed", "error": encode_error(error, OpenAiProfile::Responses, tracker)["error"]}}),
                );
                saw_terminal = true;
            }
            StreamEvent::Opaque { extension } => {
                diagnose_extensions(
                    std::slice::from_ref(extension),
                    OpenAiProfile::Responses,
                    tracker,
                    "opaque stream frames require exact same-profile raw replay",
                );
            }
        }
    }
    if !saw_terminal {
        push_sse_event(
            &mut output,
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_alpha", "status": "completed", "output": [], "usage": usage.as_ref().map(encode_responses_usage)}}),
        );
    }
    output
}

fn close_stream_parts(
    events: &mut Vec<StreamEvent>,
    open_parts: &mut BTreeSet<(u64, OutputPartType)>,
    message_id: Option<String>,
) {
    for (part_index, _) in std::mem::take(open_parts) {
        events.push(StreamEvent::OutputPartEnded {
            message_id: message_id.clone(),
            part_index: usize::try_from(part_index).unwrap_or(0),
        });
    }
}

fn push_sse_event(output: &mut String, event: &str, value: Value) {
    output.push_str("event: ");
    output.push_str(event);
    output.push('\n');
    push_sse_data(output, value);
}

fn push_sse_data(output: &mut String, value: Value) {
    output.push_str("data: ");
    output.push_str(
        &serde_json::to_string(&value).expect("OpenAI stream event JSON is serializable"),
    );
    output.push_str("\n\n");
}

fn openai_cache_intent(object: &Map<String, Value>) -> Option<llm_protocol_core::CacheIntent> {
    let request_cache_key = optional_string(object, "prompt_cache_key");
    let retention = optional_string(object, "prompt_cache_retention");
    (request_cache_key.is_some() || retention.is_some()).then_some(
        llm_protocol_core::CacheIntent::OpenAi(OpenAiCacheIntent {
            request_cache_key,
            retention,
        }),
    )
}

fn non_default_chat_option(object: &Map<String, Value>, key: &str) -> bool {
    match object.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(false)) if matches!(key, "parallel_tool_calls" | "logprobs") => false,
        Some(Value::String(value)) if key == "tool_choice" && value == "auto" => false,
        _ => true,
    }
}

fn decode_role(value: &str, profile: OpenAiProfile) -> Result<ConversationRole, CodecError> {
    match value {
        "system" => Ok(ConversationRole::System),
        "developer" => Ok(ConversationRole::Developer),
        "user" => Ok(ConversationRole::User),
        "assistant" => Ok(ConversationRole::Assistant),
        "tool" => Ok(ConversationRole::Tool),
        _ => Err(invalid_shape(
            profile,
            "message role is outside the typed alpha subset",
        )),
    }
}

fn encode_chat_role(role: ConversationRole) -> &'static str {
    match role {
        ConversationRole::System => "system",
        ConversationRole::Developer => "developer",
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
        ConversationRole::Tool => "tool",
    }
}

fn encode_responses_role(role: ConversationRole) -> &'static str {
    match role {
        ConversationRole::System => "system",
        ConversationRole::Developer => "developer",
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
        ConversationRole::Tool => "tool",
    }
}

fn normalize_chat_finish_reason(reason: &str) -> String {
    match reason {
        "stop" => FinishReason::STOP,
        "length" => FinishReason::LENGTH,
        "tool_calls" | "function_call" => FinishReason::TOOL_CALLS,
        "content_filter" => FinishReason::CONTENT_FILTER,
        _ => reason,
    }
    .to_owned()
}

fn encode_chat_finish_reason(reason: &str) -> &'static str {
    match reason {
        FinishReason::TOOL_CALLS => "tool_calls",
        FinishReason::LENGTH => "length",
        FinishReason::CONTENT_FILTER => "content_filter",
        _ => "stop",
    }
}

fn error_category(error_type: Option<&str>, status: u16) -> ErrorCategory {
    match error_type {
        Some("invalid_request_error") => ErrorCategory::InvalidRequest,
        Some("authentication_error") => ErrorCategory::Authentication,
        Some("permission_error") => ErrorCategory::Permission,
        Some("not_found_error") => ErrorCategory::NotFound,
        Some("rate_limit_error") => ErrorCategory::RateLimit,
        Some("conflict_error") => ErrorCategory::Conflict,
        Some("server_error") => ErrorCategory::Server,
        _ => match status {
            400 | 422 => ErrorCategory::InvalidRequest,
            401 => ErrorCategory::Authentication,
            403 => ErrorCategory::Permission,
            404 => ErrorCategory::NotFound,
            409 => ErrorCategory::Conflict,
            429 => ErrorCategory::RateLimit,
            500..=599 => ErrorCategory::Server,
            _ => ErrorCategory::Unknown,
        },
    }
}

fn error_type(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::InvalidRequest => "invalid_request_error",
        ErrorCategory::Authentication => "authentication_error",
        ErrorCategory::Permission => "permission_error",
        ErrorCategory::NotFound => "not_found_error",
        ErrorCategory::RateLimit => "rate_limit_error",
        ErrorCategory::Conflict => "conflict_error",
        ErrorCategory::Server | ErrorCategory::Transport | ErrorCategory::Unknown => "server_error",
    }
}

fn retry_after_ms(headers: &[ProtocolHeaderLine]) -> Option<u64> {
    headers
        .iter()
        .find(|header| header.name().eq_ignore_ascii_case("retry-after"))
        .and_then(|header| header.value().parse::<u64>().ok())
        .and_then(|seconds| seconds.checked_mul(1000))
}

fn optional_string(object: &Map<String, Value>, field: &str) -> Option<String> {
    object.get(field).and_then(Value::as_str).map(str::to_owned)
}

fn required_str<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str, CodecError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| CodecError::InvalidEnvelope(format!("{field} must be a string")))
}

fn required_u16(object: &Map<String, Value>, field: &str) -> Result<u16, CodecError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| CodecError::InvalidEnvelope(format!("{field} must be a u16")))
}

fn profile_id_field(object: &Map<String, Value>, field: &str) -> Result<ProfileId, CodecError> {
    ProfileId::new(required_str(object, field)?)
        .map_err(|error| CodecError::InvalidEnvelope(error.to_string()))
}

fn required_array<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    profile: OpenAiProfile,
) -> Result<&'a Vec<Value>, CodecError> {
    object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_shape(profile, "required field must be an array"))
}

fn text_message(role: ConversationRole, text: &str) -> llm_protocol_core::Message {
    llm_protocol_core::Message {
        role,
        name: None,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        extensions: Vec::new(),
    }
}

fn string_or_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_json_or_string(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).expect("IR JSON values are serializable")
}

fn media_type_from_data_url(value: &str) -> Option<String> {
    let prefix = value.strip_prefix("data:")?.split_once(',')?.0;
    let media_type = prefix.split(';').next().unwrap_or_default();
    (!media_type.is_empty()).then_some(media_type.to_owned())
}

fn unknown_extensions(
    object: &Map<String, Value>,
    known: &[&str],
    profile_id: &ProfileId,
    namespace: &str,
    pointer: &str,
) -> Vec<OpaqueExtension> {
    object
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| {
            opaque_json(
                profile_id,
                namespace,
                SourceLocation::JsonPointer {
                    pointer: format!("{pointer}/{}", json_pointer_escape(key)),
                },
                value.clone(),
            )
        })
        .collect()
}

fn opaque_json(
    profile_id: &ProfileId,
    namespace: &str,
    source_location: SourceLocation,
    payload: Value,
) -> OpaqueExtension {
    OpaqueExtension {
        issuing_profile: profile_id.clone(),
        namespace: namespace.to_owned(),
        source_location,
        payload: OpaquePayload::Json(payload),
    }
}

fn opaque_text(
    profile_id: &ProfileId,
    namespace: &str,
    source_location: SourceLocation,
    payload: impl Into<String>,
) -> OpaqueExtension {
    OpaqueExtension {
        issuing_profile: profile_id.clone(),
        namespace: namespace.to_owned(),
        source_location,
        payload: OpaquePayload::Text(payload.into()),
    }
}

fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn diagnose_extensions(
    extensions: &[OpaqueExtension],
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
    message: &'static str,
) {
    for extension in extensions {
        if extension.issuing_profile != target.profile_id() {
            tracker.lossy(
                DiagnosticCode::ForwardCompatibleUnknown,
                Some(extension.source_location.clone()),
                message,
            );
        } else {
            tracker.lossy(
                DiagnosticCode::NonPortableOpaqueExtension,
                Some(extension.source_location.clone()),
                "opaque vendor material requires exact retained-wire replay",
            );
        }
    }
}

fn diagnose_cross_profile_extensions(
    extensions: &[OpaqueExtension],
    target: OpenAiProfile,
    tracker: &mut ConversionTracker,
    message: &'static str,
) {
    diagnose_extensions(extensions, target, tracker, message);
}

fn diagnostic(
    code: DiagnosticCode,
    severity: DiagnosticSeverity,
    location: Option<SourceLocation>,
    message: impl Into<String>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity,
        location,
        message: message.into(),
    }
}

fn unsupported_diagnostic(location: SourceLocation, message: &'static str) -> Diagnostic {
    diagnostic(
        DiagnosticCode::UnsupportedFeature,
        DiagnosticSeverity::Error,
        Some(location),
        message,
    )
}

fn invalid_shape(profile: OpenAiProfile, message: impl Into<String>) -> CodecError {
    CodecError::InvalidShape {
        profile: match profile {
            OpenAiProfile::ChatCompletions => "Chat Completions",
            OpenAiProfile::Responses => "Responses",
        },
        kind: "envelope",
        message: message.into(),
    }
}

fn adapter_metadata_from_json(headers: &Map<String, Value>) -> Result<AdapterMetadata, CodecError> {
    let mut generic_headers = BTreeMap::new();
    for (name, value) in headers {
        let value = value.as_str().ok_or_else(|| {
            CodecError::InvalidEnvelope("adapter headers must be strings".to_owned())
        })?;
        generic_headers.insert(name.clone(), value.to_owned());
    }
    Ok(AdapterMetadata { generic_headers })
}

fn decode_base64(value: &str) -> Result<Vec<u8>, CodecError> {
    use base64::Engine;

    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| CodecError::InvalidEnvelope(format!("invalid body_base64: {error}")))
}

fn encode_base64(value: &[u8]) -> String {
    use base64::Engine;

    base64::engine::general_purpose::STANDARD.encode(value)
}

#[cfg(test)]
mod tests;
