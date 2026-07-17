//! Anthropic Messages reference codec for LLM Protocol Alpha `0.1.0`.
//!
//! This crate implements only the frozen `2023-06-01` Messages profile. It
//! decodes to and encodes from the shared vendor-neutral payload in
//! `llm-protocol-core`; it does not depend on the OpenAI codec or participate
//! in OnAir request routing.

use std::collections::{BTreeMap, BTreeSet};

use llm_protocol_core::{
    ANTHROPIC_MESSAGES_PROFILE, AdapterMetadata, AnthropicCacheBreakpoint, AnthropicCacheIntent,
    ApiFamily, AssetReference, AssetReferenceType, CacheDirectiveCompatibility,
    CachePreservationReport, CacheSegmentPlan, CanonicalEnvelope, ContentPart, ConversationRole,
    ConversionResult, DecodedEnvelope, Diagnostic, DiagnosticCode, DiagnosticSeverity,
    EnvelopeError, ErrorCategory, Fidelity, FinishReason, GenerationControls, Message,
    OPENAI_CHAT_COMPLETIONS_PROFILE, OPENAI_RESPONSES_PROFILE, OpaqueExtension, OpaquePayload,
    OutputPartType, PROTOCOL_VERSION, ProfileId, ProtocolBodyKind, ProtocolError,
    ProtocolHeaderLine, ProtocolPayload, ProtocolRequest, ProtocolResponse, ReplayEnvelope,
    RetainedWire, SourceLocation, SseFrame, SseFramer, SseFramingError, StreamEvent,
    ToolDefinition, Usage,
};
use serde_json::{Map, Value, json};
use thiserror::Error;

pub const MESSAGES_PROFILE: &str = ANTHROPIC_MESSAGES_PROFILE;
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Canonical or retained protocol material ready for an HTTP adapter.
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
/// report. Exact same-profile replay does not need a conversion report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedEnvelope {
    pub wire: WireEnvelope,
    pub cache_report: Option<CachePreservationReport>,
}

/// The only Anthropic profile frozen by Alpha `0.1.0`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnthropicProfile {
    Messages,
}

impl AnthropicProfile {
    pub fn profile_id(self) -> ProfileId {
        ProfileId::new(MESSAGES_PROFILE).expect("Anthropic alpha profile identifier is valid")
    }

    pub fn api_family(self) -> ApiFamily {
        ApiFamily::Messages
    }

    pub fn from_id(profile_id: &ProfileId) -> Result<Self, CodecError> {
        match profile_id.as_str() {
            MESSAGES_PROFILE => Ok(Self::Messages),
            _ => Err(CodecError::UnsupportedProfile(profile_id.clone())),
        }
    }
}

/// Decode retained Anthropic Messages wire material into the common IR.
pub fn decode(
    retained: RetainedWire,
    adapter_metadata: AdapterMetadata,
) -> Result<ConversionResult<DecodedEnvelope<ProtocolPayload>>, CodecError> {
    retained.validate().map_err(CodecError::Envelope)?;
    AnthropicProfile::from_id(&retained.profile_id)?;
    validate_anthropic_version(&retained.protocol_headers)?;

    let result = match retained.body_kind {
        ProtocolBodyKind::Json => decode_json(&retained),
        ProtocolBodyKind::Sse => decode_stream(&retained),
    }?;

    match result.output {
        Some(value) => Ok(ConversionResult {
            output: Some(
                DecodedEnvelope::new(value, retained, adapter_metadata)
                    .map_err(CodecError::Envelope)?,
            ),
            fidelity: result.fidelity,
            diagnostics: result.diagnostics,
        }),
        None => Ok(ConversionResult {
            output: None,
            fidelity: result.fidelity,
            diagnostics: result.diagnostics,
        }),
    }
}

/// Encode an unmodified decoded envelope. Raw body bytes and protocol headers
/// are reused only for the exact issuing profile.
pub fn encode_decoded(
    decoded: &DecodedEnvelope<ProtocolPayload>,
    target_profile: &ProfileId,
) -> Result<ConversionResult<EncodedEnvelope>, CodecError> {
    AnthropicProfile::from_id(target_profile)?;
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

/// Canonically encode a semantically modified shared IR envelope.
pub fn encode_canonical(
    canonical: CanonicalEnvelope<ProtocolPayload>,
    target_profile: &ProfileId,
) -> Result<ConversionResult<EncodedEnvelope>, CodecError> {
    AnthropicProfile::from_id(target_profile)?;
    encode_value(
        &canonical.value,
        &canonical.profile_id,
        target_profile,
        canonical.status,
        canonical.body_kind,
        canonical.adapter_metadata,
    )
}

/// Parse a vector/test envelope object into wire material.
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
        value => {
            return Err(CodecError::InvalidEnvelope(format!(
                "unsupported body_kind {value}"
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
    #[error("unsupported Anthropic alpha profile: {0}")]
    UnsupportedProfile(ProfileId),
    #[error("invalid protocol envelope: {0}")]
    Envelope(#[source] EnvelopeError),
    #[error("invalid wire envelope: {0}")]
    InvalidEnvelope(String),
    #[error("missing required Anthropic version header")]
    MissingAnthropicVersion,
    #[error("unsupported Anthropic version for the frozen profile: {0}")]
    UnsupportedAnthropicVersion(String),
    #[error("invalid JSON body: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("invalid SSE body: {0}")]
    InvalidSse(#[source] SseFramingError),
    #[error("invalid Anthropic Messages {kind}: {message}")]
    InvalidShape { kind: &'static str, message: String },
}

#[derive(Clone, Debug)]
struct PayloadResult {
    output: Option<ProtocolPayload>,
    fidelity: Fidelity,
    diagnostics: Vec<Diagnostic>,
}

impl PayloadResult {
    fn exact(output: ProtocolPayload) -> Self {
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

fn validate_anthropic_version(headers: &[ProtocolHeaderLine]) -> Result<(), CodecError> {
    let version = headers
        .iter()
        .find(|header| header.name().eq_ignore_ascii_case("anthropic-version"))
        .map(ProtocolHeaderLine::value)
        .ok_or(CodecError::MissingAnthropicVersion)?;
    if version != ANTHROPIC_VERSION {
        return Err(CodecError::UnsupportedAnthropicVersion(version.to_owned()));
    }
    Ok(())
}

fn decode_json(retained: &RetainedWire) -> Result<PayloadResult, CodecError> {
    let value = serde_json::from_slice::<Value>(&retained.body).map_err(CodecError::InvalidJson)?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_shape("JSON body must be an object"))?;

    if retained.status >= 400 || object.get("type").and_then(Value::as_str) == Some("error") {
        return decode_error(object, retained);
    }

    if object.contains_key("messages") {
        decode_request(object, retained)
    } else if object.contains_key("content")
        || object.get("type").and_then(Value::as_str) == Some("message")
    {
        decode_response(object, retained)
    } else {
        Err(invalid_shape(
            "JSON body is neither a Messages request nor a Messages response",
        ))
    }
}

fn decode_stream(retained: &RetainedWire) -> Result<PayloadResult, CodecError> {
    let result = decode_sse_chunks(&retained.profile_id, &[retained.body.as_slice()])?;
    let output = result.output.map(|events| {
        let beta_events = beta_header_extensions(&retained.protocol_headers, &retained.profile_id)
            .into_iter()
            .map(|extension| StreamEvent::Opaque { extension });
        beta_events.chain(events).collect()
    });
    Ok(PayloadResult {
        output: output.map(ProtocolPayload::Stream),
        fidelity: result.fidelity,
        diagnostics: result.diagnostics,
    })
}

/// Normalize a complete Anthropic SSE stream from arbitrary byte chunks.
pub fn decode_sse_chunks(
    profile_id: &ProfileId,
    chunks: &[&[u8]],
) -> Result<ConversionResult<Vec<StreamEvent>>, CodecError> {
    AnthropicProfile::from_id(profile_id)?;
    let mut framer = SseFramer::new();
    let mut frames = Vec::new();
    for chunk in chunks {
        frames.extend(framer.push(chunk).map_err(CodecError::InvalidSse)?);
    }
    let _ = framer.finish().map_err(CodecError::InvalidSse)?;
    Ok(ConversionResult::exact(decode_messages_stream(
        frames, profile_id,
    )))
}

fn decode_error(
    object: &Map<String, Value>,
    retained: &RetainedWire,
) -> Result<PayloadResult, CodecError> {
    let error_object = object
        .get("error")
        .and_then(Value::as_object)
        .unwrap_or(object);
    let error_type = error_object
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| object.get("type").and_then(Value::as_str));
    let mut extensions = unknown_extensions(
        error_object,
        &["type", "message"],
        &retained.profile_id,
        "anthropic.error.unknown_field",
        "/error",
    );
    extensions.extend(beta_header_extensions(
        &retained.protocol_headers,
        &retained.profile_id,
    ));
    Ok(PayloadResult::exact(ProtocolPayload::Error(
        ProtocolError {
            category: error_category(error_type, retained.status),
            code: error_type.unwrap_or("unknown_error").to_owned(),
            message: error_object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Anthropic request failed")
                .to_owned(),
            retry_after_ms: retry_after_ms(&retained.protocol_headers),
            param: None,
            extensions,
        },
    )))
}

fn decode_request(
    object: &Map<String, Value>,
    retained: &RetainedWire,
) -> Result<PayloadResult, CodecError> {
    if object.contains_key("tool_choice") {
        return Ok(PayloadResult::unsupported(vec![unsupported_diagnostic(
            SourceLocation::JsonPointer {
                pointer: "/tool_choice".to_owned(),
            },
            "Anthropic tool_choice is outside the frozen typed alpha subset",
        )]));
    }

    let profile_id = &retained.profile_id;
    let mut breakpoints = Vec::new();
    let instructions = decode_system(object.get("system"), profile_id, &mut breakpoints)?;
    let messages = required_array(object, "messages")?
        .iter()
        .enumerate()
        .map(|(index, value)| decode_message(value, profile_id, index, &mut breakpoints))
        .collect::<Result<Vec<_>, _>>()?;
    let tools = decode_tools(object.get("tools"), profile_id, &mut breakpoints)?;
    let mut extensions = unknown_extensions(
        object,
        &[
            "model",
            "messages",
            "system",
            "max_tokens",
            "stream",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "tools",
            "tool_choice",
        ],
        profile_id,
        "anthropic.messages.request.unknown_field",
        "",
    );
    extensions.extend(beta_header_extensions(
        &retained.protocol_headers,
        profile_id,
    ));
    let max_output_tokens = object.get("max_tokens").and_then(Value::as_u64);
    if max_output_tokens.is_none() {
        return Ok(PayloadResult::unsupported(vec![unsupported_diagnostic(
            SourceLocation::JsonPointer {
                pointer: "/max_tokens".to_owned(),
            },
            "Anthropic Messages requests require a typed max_tokens limit",
        )]));
    }

    Ok(PayloadResult::exact(ProtocolPayload::Request(
        ProtocolRequest {
            model: optional_string(object, "model"),
            stream: object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            instructions,
            messages,
            tools,
            generation: GenerationControls {
                temperature: object.get("temperature").and_then(Value::as_f64),
                top_p: object.get("top_p").and_then(Value::as_f64),
                top_k: object
                    .get("top_k")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok()),
                max_output_tokens,
                stop_sequences: string_array(object.get("stop_sequences")),
            },
            output_schema: None,
            cache_intent: (!breakpoints.is_empty()).then_some(
                llm_protocol_core::CacheIntent::Anthropic(AnthropicCacheIntent { breakpoints }),
            ),
            continuation: None,
            extensions,
        },
    )))
}

fn decode_response(
    object: &Map<String, Value>,
    retained: &RetainedWire,
) -> Result<PayloadResult, CodecError> {
    let profile_id = &retained.profile_id;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .map(decode_response_role)
        .transpose()?
        .unwrap_or(ConversationRole::Assistant);
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_shape("Messages response requires content"))?;
    let mut output = Vec::new();
    let mut parts = Vec::new();
    for (index, block) in content.iter().enumerate() {
        parts.extend(decode_content_block(
            block,
            profile_id,
            &format!("/content/{index}"),
        )?);
    }
    output.push(Message {
        role,
        name: None,
        content: parts,
        extensions: Vec::new(),
    });
    let mut extensions = unknown_extensions(
        object,
        &[
            "id",
            "type",
            "role",
            "content",
            "model",
            "stop_reason",
            "stop_sequence",
            "usage",
        ],
        profile_id,
        "anthropic.messages.response.unknown_field",
        "",
    );
    extensions.extend(beta_header_extensions(
        &retained.protocol_headers,
        profile_id,
    ));
    Ok(PayloadResult::exact(ProtocolPayload::Response(
        ProtocolResponse {
            id: optional_string(object, "id"),
            model: optional_string(object, "model"),
            output,
            usage: object.get("usage").and_then(decode_usage),
            finish_reason: FinishReason::new(
                object
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .map(normalize_stop_reason)
                    .unwrap_or(FinishReason::STOP),
            )
            .expect("normalized stop reasons are non-empty"),
            continuation: None,
            extensions,
        },
    )))
}

fn decode_system(
    value: Option<&Value>,
    profile_id: &ProfileId,
    breakpoints: &mut Vec<AnthropicCacheBreakpoint>,
) -> Result<Vec<ContentPart>, CodecError> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![ContentPart::Text { text: text.clone() }]),
        Some(Value::Array(blocks)) => {
            let mut instructions = Vec::new();
            for (index, block) in blocks.iter().enumerate() {
                collect_cache_breakpoint(
                    block,
                    llm_protocol_core::CacheLocation::Instructions { part_index: index },
                    breakpoints,
                )?;
                instructions.extend(decode_content_block(
                    block,
                    profile_id,
                    &format!("/system/{index}"),
                )?);
            }
            Ok(instructions)
        }
        Some(_) => Err(invalid_shape(
            "system must be a string or an array of blocks",
        )),
    }
}

fn decode_message(
    value: &Value,
    profile_id: &ProfileId,
    message_index: usize,
    breakpoints: &mut Vec<AnthropicCacheBreakpoint>,
) -> Result<Message, CodecError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_shape("Messages request message must be an object"))?;
    let mut role = match required_str(object, "role")? {
        "user" => ConversationRole::User,
        "assistant" => ConversationRole::Assistant,
        _ => {
            return Err(invalid_shape(
                "Messages request role is outside the frozen typed alpha subset",
            ));
        }
    };
    collect_cache_breakpoint(
        value,
        llm_protocol_core::CacheLocation::Message { message_index },
        breakpoints,
    )?;
    let content = match object.get("content") {
        Some(Value::String(text)) => vec![ContentPart::Text { text: text.clone() }],
        Some(Value::Array(blocks)) => {
            let mut content = Vec::new();
            for (part_index, block) in blocks.iter().enumerate() {
                collect_cache_breakpoint(
                    block,
                    llm_protocol_core::CacheLocation::MessagePart {
                        message_index,
                        part_index,
                    },
                    breakpoints,
                )?;
                content.extend(decode_content_block(
                    block,
                    profile_id,
                    &format!("/messages/{message_index}/content/{part_index}"),
                )?);
            }
            content
        }
        _ => {
            return Err(invalid_shape(
                "Messages request content must be text or blocks",
            ));
        }
    };
    if role == ConversationRole::User
        && !content.is_empty()
        && content
            .iter()
            .all(|part| matches!(part, ContentPart::ToolResult { .. }))
    {
        role = ConversationRole::Tool;
    }
    Ok(Message {
        role,
        name: None,
        content,
        extensions: unknown_extensions(
            object,
            &["role", "content", "cache_control"],
            profile_id,
            "anthropic.messages.message.unknown_field",
            &format!("/messages/{message_index}"),
        ),
    })
}

fn decode_content_block(
    value: &Value,
    profile_id: &ProfileId,
    pointer: &str,
) -> Result<Vec<ContentPart>, CodecError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_shape("content blocks must be objects"))?;
    let block_type = required_str(object, "type")?;
    let parts = match block_type {
        "text" => {
            let mut parts = vec![ContentPart::Text {
                text: required_str(object, "text")?.to_owned(),
            }];
            if let Some(citations) = object.get("citations").and_then(Value::as_array) {
                for citation in citations {
                    parts.push(ContentPart::Citation {
                        reference: citation.clone(),
                        extensions: Vec::new(),
                    });
                }
            }
            parts
        }
        "image" => vec![ContentPart::Image {
            asset: decode_asset(object, "image", pointer)?,
        }],
        "document" => vec![ContentPart::Document {
            asset: decode_asset(object, "document", pointer)?,
        }],
        "tool_use" => vec![ContentPart::ToolCall {
            id: required_str(object, "id")?.to_owned(),
            name: required_str(object, "name")?.to_owned(),
            arguments: object.get("input").cloned().unwrap_or_else(|| json!({})),
            extensions: unknown_extensions(
                object,
                &["type", "id", "name", "input", "cache_control"],
                profile_id,
                "anthropic.messages.tool_use.unknown_field",
                pointer,
            ),
        }],
        "tool_result" => {
            let content = match object.get("content") {
                Some(Value::String(text)) => vec![ContentPart::Text { text: text.clone() }],
                Some(Value::Array(blocks)) => {
                    let mut content = Vec::new();
                    for (index, block) in blocks.iter().enumerate() {
                        content.extend(decode_content_block(
                            block,
                            profile_id,
                            &format!("{pointer}/content/{index}"),
                        )?);
                    }
                    content
                }
                None => Vec::new(),
                Some(_) => return Err(invalid_shape("tool_result content must be text or blocks")),
            };
            vec![ContentPart::ToolResult {
                tool_call_id: required_str(object, "tool_use_id")?.to_owned(),
                content,
                is_error: object
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                extensions: unknown_extensions(
                    object,
                    &[
                        "type",
                        "tool_use_id",
                        "content",
                        "is_error",
                        "cache_control",
                    ],
                    profile_id,
                    "anthropic.messages.tool_result.unknown_field",
                    pointer,
                ),
            }]
        }
        "thinking" => vec![ContentPart::Reasoning {
            summary: optional_string(object, "thinking"),
            opaque: object
                .get("signature")
                .and_then(Value::as_str)
                .map(|signature| {
                    opaque_text(
                        profile_id,
                        "anthropic.messages.thinking.signature",
                        SourceLocation::JsonPointer {
                            pointer: format!("{pointer}/signature"),
                        },
                        signature,
                    )
                }),
        }],
        "refusal" => vec![ContentPart::Refusal {
            text: optional_string(object, "refusal")
                .or_else(|| optional_string(object, "text"))
                .unwrap_or_default(),
            extensions: unknown_extensions(
                object,
                &["type", "refusal", "text", "cache_control"],
                profile_id,
                "anthropic.messages.refusal.unknown_field",
                pointer,
            ),
        }],
        _ => vec![ContentPart::Opaque {
            extension: opaque_json(
                profile_id,
                "anthropic.messages.unknown_content_block",
                SourceLocation::JsonPointer {
                    pointer: pointer.to_owned(),
                },
                value.clone(),
            ),
        }],
    };
    Ok(parts)
}

fn decode_asset(
    object: &Map<String, Value>,
    _expected_type: &str,
    _pointer: &str,
) -> Result<AssetReference, CodecError> {
    let source = object
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_shape("asset blocks require a source object"))?;
    let source_type = required_str(source, "type")?;
    let (reference_type, value) = match source_type {
        "base64" => {
            let media_type = required_str(source, "media_type")?;
            let data = required_str(source, "data")?;
            (
                AssetReferenceType::Data,
                format!("data:{media_type};base64,{data}"),
            )
        }
        "url" => (
            AssetReferenceType::Url,
            required_str(source, "url")?.to_owned(),
        ),
        "file" => (
            AssetReferenceType::ProviderFile,
            source
                .get("file_id")
                .or_else(|| source.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_shape("provider file source requires an identifier"))?
                .to_owned(),
        ),
        _ => {
            return Err(invalid_shape(
                "asset source type is outside the frozen typed alpha subset",
            ));
        }
    };
    let media_type = source
        .get("media_type")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| media_type_from_data_url(&value));
    Ok(AssetReference {
        reference_type,
        value,
        media_type,
        name: object
            .get("title")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        size_bytes: None,
    })
}

fn decode_tools(
    value: Option<&Value>,
    profile_id: &ProfileId,
    breakpoints: &mut Vec<AnthropicCacheBreakpoint>,
) -> Result<Vec<ToolDefinition>, CodecError> {
    let Some(tools) = value else {
        return Ok(Vec::new());
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid_shape("tools must be an array"))?;
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let object = tool
                .as_object()
                .ok_or_else(|| invalid_shape("tool definitions must be objects"))?;
            collect_cache_breakpoint(
                tool,
                llm_protocol_core::CacheLocation::ToolDefinition { tool_index: index },
                breakpoints,
            )?;
            Ok(ToolDefinition {
                name: required_str(object, "name")?.to_owned(),
                description: optional_string(object, "description"),
                input_schema: object
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                strict: None,
                extensions: unknown_extensions(
                    object,
                    &["name", "description", "input_schema", "cache_control"],
                    profile_id,
                    "anthropic.messages.tool.unknown_field",
                    &format!("/tools/{index}"),
                ),
            })
        })
        .collect()
}

fn collect_cache_breakpoint(
    value: &Value,
    location: llm_protocol_core::CacheLocation,
    breakpoints: &mut Vec<AnthropicCacheBreakpoint>,
) -> Result<(), CodecError> {
    let Some(control) = value.get("cache_control") else {
        return Ok(());
    };
    let control = control
        .as_object()
        .ok_or_else(|| invalid_shape("cache_control must be an object"))?;
    if required_str(control, "type")? != "ephemeral" {
        return Err(invalid_shape(
            "cache_control type is outside the frozen typed alpha subset",
        ));
    }
    breakpoints.push(AnthropicCacheBreakpoint {
        location,
        ttl: optional_string(control, "ttl"),
    });
    Ok(())
}

fn decode_response_role(role: &str) -> Result<ConversationRole, CodecError> {
    match role {
        "assistant" => Ok(ConversationRole::Assistant),
        "user" => Ok(ConversationRole::User),
        _ => Err(invalid_shape(
            "Messages response role is outside the frozen typed alpha subset",
        )),
    }
}

fn decode_usage(value: &Value) -> Option<Usage> {
    let object = value.as_object()?;
    Some(Usage {
        input_tokens: object.get("input_tokens").and_then(Value::as_u64),
        output_tokens: object.get("output_tokens").and_then(Value::as_u64),
        reasoning_tokens: None,
        cache_read_tokens: object
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64),
        cache_write_tokens: object
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
    })
}

fn encode_value(
    value: &ProtocolPayload,
    source_profile: &ProfileId,
    target_profile: &ProfileId,
    status: u16,
    requested_body_kind: ProtocolBodyKind,
    adapter_metadata: AdapterMetadata,
) -> Result<ConversionResult<EncodedEnvelope>, CodecError> {
    AnthropicProfile::from_id(target_profile)?;
    validate_known_source_profile(source_profile)?;
    let mut tracker = ConversionTracker::new();
    let body_kind = match value {
        ProtocolPayload::Stream(_) => ProtocolBodyKind::Sse,
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
        ProtocolPayload::Request(request) => {
            let result = encode_request(request, &mut tracker)?;
            if tracker.fidelity == Fidelity::Unsupported {
                return Ok(ConversionResult::unsupported(tracker.diagnostics));
            }
            result
        }
        ProtocolPayload::Response(response) => {
            let result = encode_response(response, &mut tracker)?;
            if tracker.fidelity == Fidelity::Unsupported {
                return Ok(ConversionResult::unsupported(tracker.diagnostics));
            }
            result
        }
        ProtocolPayload::Error(error) => encode_error(error, &mut tracker),
        ProtocolPayload::Stream(events) => {
            if source_profile != target_profile {
                tracker.adapted(
                    DiagnosticCode::SemanticChange,
                    None,
                    "stream lifecycle frames were adapted to Anthropic Messages",
                );
            }
            Value::String(encode_stream(events, &mut tracker))
        }
    };
    if tracker.fidelity == Fidelity::Unsupported {
        return Ok(ConversionResult::unsupported(tracker.diagnostics));
    }

    let body = match body_kind {
        ProtocolBodyKind::Json => serde_json::to_vec(&body_value)
            .expect("canonical Anthropic JSON bodies are always serializable"),
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
        ProtocolHeaderLine::new(format!("anthropic-version: {ANTHROPIC_VERSION}"))
            .expect("canonical Anthropic version is valid"),
    ];
    if let ProtocolPayload::Error(error) = value
        && let Some(retry_after_ms) = error.retry_after_ms
        && retry_after_ms > 0
    {
        protocol_headers.push(
            ProtocolHeaderLine::new(format!("retry-after: {}", retry_after_ms.div_ceil(1000)))
                .expect("canonical retry-after header is valid"),
        );
    }

    let cache_report = if source_profile != target_profile {
        match value {
            ProtocolPayload::Request(request) => Some(cache_report_for_request(
                request,
                source_profile == &AnthropicProfile::Messages.profile_id(),
                &body,
            )?),
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

fn cache_report_for_request(
    source_request: &ProtocolRequest,
    source_is_anthropic: bool,
    target_body: &[u8],
) -> Result<CachePreservationReport, CodecError> {
    let target_payload = decode_json(&RetainedWire {
        profile_id: AnthropicProfile::Messages.profile_id(),
        status: 200,
        body_kind: ProtocolBodyKind::Json,
        protocol_headers: Vec::new(),
        body: target_body.to_vec(),
    })?
    .output
    .ok_or_else(|| {
        CodecError::InvalidEnvelope(
            "canonical Anthropic request did not decode to target IR".to_owned(),
        )
    })?;
    let ProtocolPayload::Request(target_request) = target_payload else {
        return Err(CodecError::InvalidEnvelope(
            "canonical Anthropic request decoded as a non-request payload".to_owned(),
        ));
    };
    let source_plan = CacheSegmentPlan::analyze(source_request)
        .map_err(|error| CodecError::InvalidEnvelope(error.to_string()))?;
    let target_plan = CacheSegmentPlan::analyze(&target_request)
        .map_err(|error| CodecError::InvalidEnvelope(error.to_string()))?;
    let compatibility = if source_is_anthropic {
        CacheDirectiveCompatibility::SameProvider
    } else {
        CacheDirectiveCompatibility::CrossProvider
    };
    let report =
        CachePreservationReport::source_to_target(&source_plan, &target_plan, compatibility);
    report
        .validate_conservation(&source_plan, &target_plan)
        .map_err(|error| CodecError::InvalidEnvelope(error.to_string()))?;
    Ok(report)
}

fn validate_known_source_profile(profile_id: &ProfileId) -> Result<(), CodecError> {
    match profile_id.as_str() {
        MESSAGES_PROFILE | OPENAI_CHAT_COMPLETIONS_PROFILE | OPENAI_RESPONSES_PROFILE => Ok(()),
        _ => Err(CodecError::UnsupportedProfile(profile_id.clone())),
    }
}

fn encode_request(
    request: &ProtocolRequest,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    diagnose_extensions(
        &request.extensions,
        tracker,
        "unknown request fields cannot be canonically projected into Anthropic Messages",
    );
    if let Some(handle) = &request.continuation {
        tracker.unsupported(
            DiagnosticCode::NonPortableContinuationHandle,
            Some(handle.extension.source_location.clone()),
            "provider continuation handles cannot be encoded for Anthropic Messages",
        );
        return Ok(Value::Null);
    }
    if request.output_schema.is_some() {
        tracker.unsupported(
            DiagnosticCode::UnsupportedFeature,
            None,
            "the frozen Anthropic profile has no typed structured-output enforcement representation",
        );
        return Ok(Value::Null);
    }
    let Some(model) = &request.model else {
        tracker.unsupported(
            DiagnosticCode::UnsupportedFeature,
            Some(SourceLocation::JsonPointer {
                pointer: "/model".to_owned(),
            }),
            "Anthropic Messages requires a model identifier",
        );
        return Ok(Value::Null);
    };
    let Some(max_tokens) = request.generation.max_output_tokens else {
        tracker.unsupported(
            DiagnosticCode::UnsupportedFeature,
            Some(SourceLocation::JsonPointer {
                pointer: "/max_tokens".to_owned(),
            }),
            "Anthropic Messages requires a typed max_tokens limit",
        );
        return Ok(Value::Null);
    };

    let anthropic_intent = match &request.cache_intent {
        Some(llm_protocol_core::CacheIntent::Anthropic(intent)) => Some(intent),
        Some(llm_protocol_core::CacheIntent::OpenAi(_)) => {
            tracker.lossy(
                DiagnosticCode::NonPortableCacheIntent,
                None,
                "OpenAI cache directives were not synthesized for Anthropic Messages",
            );
            None
        }
        None => None,
    };
    validate_cache_locations(request, anthropic_intent, tracker);
    if tracker.fidelity == Fidelity::Unsupported {
        return Ok(Value::Null);
    }

    let mut object = Map::new();
    object.insert("model".to_owned(), Value::String(model.clone()));
    object.insert("max_tokens".to_owned(), Value::Number(max_tokens.into()));
    if request.stream {
        object.insert("stream".to_owned(), Value::Bool(true));
    }
    insert_generation(&mut object, &request.generation);
    insert_tools(&mut object, &request.tools, anthropic_intent, tracker)?;
    let system = encode_system(request, anthropic_intent, tracker)?;
    if !system.is_empty() {
        object.insert("system".to_owned(), Value::Array(system));
    }
    let mut messages = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            ConversationRole::System | ConversationRole::Developer => {}
            _ => messages.push(encode_message(message, index, anthropic_intent, tracker)?),
        }
    }
    if tracker.fidelity == Fidelity::Unsupported {
        return Ok(Value::Null);
    }
    object.insert("messages".to_owned(), Value::Array(messages));
    Ok(Value::Object(object))
}

fn encode_system(
    request: &ProtocolRequest,
    intent: Option<&AnthropicCacheIntent>,
    tracker: &mut ConversionTracker,
) -> Result<Vec<Value>, CodecError> {
    let mut system = Vec::new();
    for (index, part) in request.instructions.iter().enumerate() {
        let text = content_to_text(std::slice::from_ref(part), tracker, "system instruction")?;
        let mut block = json!({"type": "text", "text": text});
        if let Some(control) = cache_control_for(
            intent,
            &llm_protocol_core::CacheLocation::Instructions { part_index: index },
        ) {
            block
                .as_object_mut()
                .expect("encoded Anthropic content block is an object")
                .insert("cache_control".to_owned(), control);
        }
        system.push(block);
    }
    for (index, message) in request.messages.iter().enumerate() {
        if matches!(
            message.role,
            ConversationRole::System | ConversationRole::Developer
        ) {
            let text = content_to_text(&message.content, tracker, "system message")?;
            if !text.is_empty() {
                if cache_control_for(
                    intent,
                    &llm_protocol_core::CacheLocation::Message {
                        message_index: index,
                    },
                )
                .is_some()
                {
                    tracker.unsupported(
                        DiagnosticCode::UnsupportedFeature,
                        Some(SourceLocation::JsonPointer {
                            pointer: format!("/messages/{index}/cache_control"),
                        }),
                        "a cache breakpoint on a system/developer message has no Anthropic Messages location",
                    );
                }
                tracker.adapted(
                    DiagnosticCode::SemanticChange,
                    Some(SourceLocation::JsonPointer {
                        pointer: format!("/messages/{index}"),
                    }),
                    "a system or developer message was represented in the Anthropic system field",
                );
                system.push(json!({"type": "text", "text": text}));
            }
        }
    }
    Ok(system)
}

fn encode_message(
    message: &Message,
    index: usize,
    intent: Option<&AnthropicCacheIntent>,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    diagnose_extensions(
        &message.extensions,
        tracker,
        "unknown message fields cannot be canonically represented",
    );
    let role = match message.role {
        ConversationRole::User => "user",
        ConversationRole::Assistant => "assistant",
        ConversationRole::Tool => "user",
        ConversationRole::System | ConversationRole::Developer => {
            tracker.unsupported(
                DiagnosticCode::UnsupportedFeature,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/messages/{index}/role"),
                }),
                "system and developer messages must be represented in the Anthropic system field",
            );
            return Ok(Value::Null);
        }
    };
    let mut content = Vec::new();
    for (part_index, part) in message.content.iter().enumerate() {
        let mut block = encode_content_part(part, index, part_index, intent, false, tracker)?;
        if let Some(control) = cache_control_for(
            intent,
            &llm_protocol_core::CacheLocation::MessagePart {
                message_index: index,
                part_index,
            },
        ) {
            block
                .as_object_mut()
                .expect("encoded Anthropic content block is an object")
                .insert("cache_control".to_owned(), control);
        }
        content.push(block);
    }
    let mut object = Map::new();
    object.insert("role".to_owned(), Value::String(role.to_owned()));
    object.insert("content".to_owned(), Value::Array(content));
    if let Some(control) = cache_control_for(
        intent,
        &llm_protocol_core::CacheLocation::Message {
            message_index: index,
        },
    ) {
        object.insert("cache_control".to_owned(), control);
    }
    Ok(Value::Object(object))
}

fn encode_content_part(
    part: &ContentPart,
    message_index: usize,
    part_index: usize,
    _intent: Option<&AnthropicCacheIntent>,
    instruction: bool,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    let pointer = if instruction {
        format!("/system/{part_index}")
    } else {
        format!("/messages/{message_index}/content/{part_index}")
    };
    match part {
        ContentPart::Text { text } => Ok(json!({"type": "text", "text": text})),
        ContentPart::Image { asset } => Ok(json!({
            "type": "image",
            "source": encode_asset_source(asset),
        })),
        ContentPart::Document { asset } => Ok(json!({
            "type": "document",
            "source": encode_asset_source(asset),
            "title": asset.name,
        })),
        ContentPart::ToolCall {
            id,
            name,
            arguments,
            extensions,
        } => {
            diagnose_extensions(
                extensions,
                tracker,
                "unknown tool-call fields cannot be canonically represented",
            );
            Ok(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": arguments,
            }))
        }
        ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
            extensions,
        } => {
            diagnose_extensions(
                extensions,
                tracker,
                "unknown tool-result fields cannot be canonically represented",
            );
            Ok(json!({
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": encode_tool_result_content(content, tracker)?,
                "is_error": is_error,
            }))
        }
        ContentPart::Reasoning { summary, opaque } => {
            if let Some(opaque) = opaque {
                diagnose_extensions(
                    std::slice::from_ref(opaque),
                    tracker,
                    "signed or opaque reasoning payload cannot be canonically represented",
                );
            }
            Ok(json!({
                "type": "thinking",
                "thinking": summary.clone().unwrap_or_default(),
            }))
        }
        ContentPart::Citation {
            reference,
            extensions,
        } => {
            diagnose_extensions(
                extensions,
                tracker,
                "unknown citation fields cannot be canonically represented",
            );
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                Some(SourceLocation::JsonPointer { pointer }),
                "a standalone citation part was omitted from Anthropic content",
            );
            let _ = reference;
            Ok(json!({"type": "text", "text": ""}))
        }
        ContentPart::Refusal { text, extensions } => {
            diagnose_extensions(
                extensions,
                tracker,
                "unknown refusal fields cannot be canonically represented",
            );
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                Some(SourceLocation::JsonPointer { pointer }),
                "a typed refusal was represented as Anthropic text",
            );
            Ok(json!({"type": "text", "text": text}))
        }
        ContentPart::Opaque { extension } => {
            diagnose_extensions(
                std::slice::from_ref(extension),
                tracker,
                "opaque content requires exact retained-wire replay",
            );
            Ok(json!({"type": "text", "text": ""}))
        }
    }
}

fn encode_tool_result_content(
    content: &[ContentPart],
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    if content.len() == 1
        && let ContentPart::Text { text } = &content[0]
    {
        return Ok(Value::String(text.clone()));
    }
    let mut blocks = Vec::new();
    for (index, part) in content.iter().enumerate() {
        blocks.push(encode_content_part(part, 0, index, None, false, tracker)?);
    }
    Ok(Value::Array(blocks))
}

fn encode_asset_source(asset: &AssetReference) -> Value {
    match asset.reference_type {
        AssetReferenceType::Url => json!({"type": "url", "url": asset.value}),
        AssetReferenceType::ProviderFile => json!({"type": "file", "file_id": asset.value}),
        AssetReferenceType::Data => {
            if let Some((media_type, data)) = split_data_url(&asset.value) {
                json!({"type": "base64", "media_type": media_type, "data": data})
            } else {
                json!({
                    "type": "base64",
                    "media_type": asset.media_type.clone().unwrap_or_else(|| "application/octet-stream".to_owned()),
                    "data": asset.value,
                })
            }
        }
    }
}

fn insert_generation(object: &mut Map<String, Value>, generation: &GenerationControls) {
    if let Some(temperature) = generation.temperature {
        object.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = generation.top_p {
        object.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(top_k) = generation.top_k {
        object.insert("top_k".to_owned(), json!(top_k));
    }
    if !generation.stop_sequences.is_empty() {
        object.insert(
            "stop_sequences".to_owned(),
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

fn insert_tools(
    object: &mut Map<String, Value>,
    tools: &[ToolDefinition],
    intent: Option<&AnthropicCacheIntent>,
    tracker: &mut ConversionTracker,
) -> Result<(), CodecError> {
    if tools.is_empty() {
        return Ok(());
    }
    let mut encoded = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        diagnose_extensions(
            &tool.extensions,
            tracker,
            "unknown tool definition fields cannot be canonically represented",
        );
        let mut object = Map::new();
        object.insert("name".to_owned(), Value::String(tool.name.clone()));
        if let Some(description) = &tool.description {
            object.insert("description".to_owned(), Value::String(description.clone()));
        }
        object.insert("input_schema".to_owned(), tool.input_schema.clone());
        if let Some(control) = cache_control_for(
            intent,
            &llm_protocol_core::CacheLocation::ToolDefinition { tool_index: index },
        ) {
            object.insert("cache_control".to_owned(), control);
        }
        if tool.strict.is_some() {
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/tools/{index}/strict"),
                }),
                "tool strictness is not represented by the frozen Anthropic profile",
            );
        }
        encoded.push(Value::Object(object));
    }
    object.insert("tools".to_owned(), Value::Array(encoded));
    Ok(())
}

fn validate_cache_locations(
    request: &ProtocolRequest,
    intent: Option<&AnthropicCacheIntent>,
    tracker: &mut ConversionTracker,
) {
    let Some(intent) = intent else {
        return;
    };
    for (index, breakpoint) in intent.breakpoints.iter().enumerate() {
        let valid = match breakpoint.location {
            llm_protocol_core::CacheLocation::Instructions { part_index } => {
                part_index < request.instructions.len()
            }
            llm_protocol_core::CacheLocation::Message { message_index } => {
                request.messages.get(message_index).is_some_and(|message| {
                    !matches!(
                        message.role,
                        ConversationRole::System | ConversationRole::Developer
                    )
                })
            }
            llm_protocol_core::CacheLocation::MessagePart {
                message_index,
                part_index,
            } => request
                .messages
                .get(message_index)
                .is_some_and(|message| part_index < message.content.len()),
            llm_protocol_core::CacheLocation::ToolDefinition { tool_index } => {
                tool_index < request.tools.len()
            }
            _ => false,
        };
        if !valid
            || intent.breakpoints[..index]
                .iter()
                .any(|previous| previous.location == breakpoint.location)
        {
            tracker.unsupported(
                DiagnosticCode::UnsupportedFeature,
                None,
                "an Anthropic cache breakpoint location is invalid or duplicated",
            );
        }
    }
}

fn cache_control_for(
    intent: Option<&AnthropicCacheIntent>,
    location: &llm_protocol_core::CacheLocation,
) -> Option<Value> {
    let breakpoint = intent?
        .breakpoints
        .iter()
        .find(|breakpoint| &breakpoint.location == location)?;
    let mut control = Map::new();
    control.insert("type".to_owned(), Value::String("ephemeral".to_owned()));
    if let Some(ttl) = &breakpoint.ttl {
        control.insert("ttl".to_owned(), Value::String(ttl.clone()));
    }
    Some(Value::Object(control))
}

fn encode_response(
    response: &ProtocolResponse,
    tracker: &mut ConversionTracker,
) -> Result<Value, CodecError> {
    diagnose_extensions(
        &response.extensions,
        tracker,
        "unknown response fields cannot be canonically represented",
    );
    if let Some(handle) = &response.continuation {
        tracker.lossy(
            DiagnosticCode::NonPortableContinuationHandle,
            Some(handle.extension.source_location.clone()),
            "a provider continuation handle was omitted from an Anthropic response",
        );
    }
    let mut content = Vec::new();
    for (message_index, message) in response.output.iter().enumerate() {
        if message.role != ConversationRole::Assistant {
            tracker.lossy(
                DiagnosticCode::SemanticChange,
                Some(SourceLocation::JsonPointer {
                    pointer: format!("/output/{message_index}/role"),
                }),
                "a non-assistant response message was encoded into the Anthropic assistant response",
            );
        }
        diagnose_extensions(
            &message.extensions,
            tracker,
            "unknown response message fields cannot be canonically represented",
        );
        for (part_index, part) in message.content.iter().enumerate() {
            content.push(encode_content_part(
                part,
                message_index,
                part_index,
                None,
                false,
                tracker,
            )?);
        }
    }
    Ok(json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": response.model,
        "stop_reason": encode_stop_reason(response.finish_reason.as_str()),
        "usage": response.usage.as_ref().map(encode_usage),
    }))
}

fn encode_error(error: &ProtocolError, tracker: &mut ConversionTracker) -> Value {
    diagnose_extensions(
        &error.extensions,
        tracker,
        "unknown error fields cannot be canonically represented",
    );
    let target_type = encode_error_type(error.category);
    if error.code != target_type {
        tracker.lossy(
            DiagnosticCode::SemanticChange,
            None,
            "the source error code was replaced by the Anthropic category type",
        );
    }
    if error.param.is_some() {
        tracker.lossy(
            DiagnosticCode::SemanticChange,
            None,
            "the source error parameter cannot be represented by Anthropic Messages",
        );
    }
    json!({
        "type": "error",
        "error": {
            "type": target_type,
            "message": error.message,
        }
    })
}

fn decode_messages_stream(frames: Vec<SseFrame>, profile_id: &ProfileId) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut started = false;
    let mut message_id = None;
    let mut open_parts = BTreeSet::new();
    let mut tool_calls: BTreeMap<u64, (String, Option<String>)> = BTreeMap::new();
    let mut finish_reason = FinishReason::new(FinishReason::STOP).expect("stop is non-empty");
    let mut terminal = false;
    let mut event_index = 0_u64;

    for frame in frames {
        let event_name = frame.event.clone();
        let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
            events.push(StreamEvent::Opaque {
                extension: opaque_text(
                    profile_id,
                    "anthropic.messages.sse.unparseable_frame",
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
            Some("message_start") => {
                if !started {
                    events.push(StreamEvent::RequestStarted);
                    started = true;
                }
                message_id = value
                    .get("message")
                    .and_then(Value::as_object)
                    .and_then(|message| optional_string(message, "id"));
                events.push(StreamEvent::MessageStarted {
                    message_id: message_id.clone(),
                });
            }
            Some("content_block_start") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = value
                    .get("content_block")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let part_type = match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        tool_calls.insert(
                            index,
                            (
                                optional_string(&block, "id")
                                    .unwrap_or_else(|| format!("call_{index}")),
                                optional_string(&block, "name"),
                            ),
                        );
                        OutputPartType::ToolCall
                    }
                    Some("thinking") => OutputPartType::Reasoning,
                    Some("refusal") => OutputPartType::Refusal,
                    _ => OutputPartType::Text,
                };
                open_parts.insert(index);
                events.push(StreamEvent::OutputPartStarted {
                    message_id: message_id.clone(),
                    part_index: usize::try_from(index).unwrap_or(0),
                    part_type,
                });
            }
            Some("content_block_delta") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = value
                    .get("delta")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => events.push(StreamEvent::TextDelta {
                        text: optional_string(&delta, "text").unwrap_or_default(),
                    }),
                    Some("thinking_delta") => events.push(StreamEvent::ReasoningDelta {
                        text: optional_string(&delta, "thinking").unwrap_or_default(),
                    }),
                    Some("input_json_delta") => {
                        let (call_id, name) = tool_calls
                            .get(&index)
                            .cloned()
                            .unwrap_or_else(|| (format!("call_{index}"), None));
                        events.push(StreamEvent::ToolCallDelta {
                            call_id,
                            name,
                            arguments_delta: optional_string(&delta, "partial_json")
                                .unwrap_or_default(),
                        });
                    }
                    Some("refusal_delta") => events.push(StreamEvent::RefusalPart {
                        text: optional_string(&delta, "refusal").unwrap_or_default(),
                        extensions: Vec::new(),
                    }),
                    _ => events.push(StreamEvent::Opaque {
                        extension: opaque_json(
                            profile_id,
                            "anthropic.messages.sse.unknown_delta",
                            SourceLocation::SseEvent {
                                index: event_index,
                                event: event_name,
                            },
                            value,
                        ),
                    }),
                }
            }
            Some("content_block_stop") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                if open_parts.remove(&index) {
                    events.push(StreamEvent::OutputPartEnded {
                        message_id: message_id.clone(),
                        part_index: usize::try_from(index).unwrap_or(0),
                    });
                }
            }
            Some("message_delta") => {
                if let Some(usage) = value.get("usage").and_then(decode_usage) {
                    events.push(StreamEvent::Usage { usage });
                }
                if let Some(stop_reason) = value
                    .get("delta")
                    .and_then(Value::as_object)
                    .and_then(|delta| optional_string(delta, "stop_reason"))
                {
                    finish_reason = FinishReason::new(normalize_stop_reason(&stop_reason))
                        .expect("normalized stop reason is non-empty");
                }
            }
            Some("message_stop") => {
                close_stream_parts(&mut events, &mut open_parts, message_id.clone());
                events.push(StreamEvent::Terminal {
                    finish_reason: finish_reason.clone(),
                });
                terminal = true;
            }
            Some("error") => {
                let error_object = value
                    .get("error")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                events.push(StreamEvent::Error {
                    error: ProtocolError {
                        category: error_category(
                            optional_string(&error_object, "type").as_deref(),
                            500,
                        ),
                        code: optional_string(&error_object, "type")
                            .unwrap_or_else(|| "unknown_error".to_owned()),
                        message: optional_string(&error_object, "message")
                            .unwrap_or_else(|| "Anthropic stream failed".to_owned()),
                        retry_after_ms: None,
                        param: None,
                        extensions: unknown_extensions(
                            &error_object,
                            &["type", "message"],
                            profile_id,
                            "anthropic.messages.sse.error_unknown_field",
                            "/error",
                        ),
                    },
                });
                terminal = true;
            }
            _ => events.push(StreamEvent::Opaque {
                extension: opaque_json(
                    profile_id,
                    "anthropic.messages.sse.unknown_event",
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
    if !started && !events.is_empty() {
        events.insert(0, StreamEvent::RequestStarted);
    }
    if !terminal {
        close_stream_parts(&mut events, &mut open_parts, message_id);
    }
    events
}

fn encode_stream(events: &[StreamEvent], tracker: &mut ConversionTracker) -> String {
    let mut output = String::new();
    let mut message_id = "msg_alpha".to_owned();
    let mut message_started = false;
    let mut part_indices = BTreeMap::new();
    let mut next_part_index = 0_usize;
    let mut usage = events.iter().rev().find_map(|event| match event {
        StreamEvent::Usage { usage } => Some(usage.clone()),
        _ => None,
    });
    let mut saw_terminal = false;
    for event in events {
        match event {
            StreamEvent::RequestStarted => {}
            StreamEvent::MessageStarted {
                message_id: event_message_id,
            } => {
                if let Some(event_message_id) = event_message_id {
                    message_id = event_message_id.clone();
                }
                if !message_started {
                    push_sse_event(
                        &mut output,
                        "message_start",
                        json!({
                            "type": "message_start",
                            "message": {
                                "id": message_id,
                                "type": "message",
                                "role": "assistant",
                                "content": [],
                                "model": "alpha",
                                "stop_reason": null,
                                "usage": {"input_tokens": 0, "output_tokens": 0},
                            }
                        }),
                    );
                    message_started = true;
                }
            }
            StreamEvent::OutputPartStarted {
                part_index,
                part_type,
                ..
            } => {
                if !message_started {
                    push_sse_event(
                        &mut output,
                        "message_start",
                        json!({"type": "message_start", "message": {"id": message_id, "type": "message", "role": "assistant", "content": [], "model": "alpha", "usage": {"input_tokens": 0, "output_tokens": 0}}}),
                    );
                    message_started = true;
                }
                let block = match part_type {
                    OutputPartType::Reasoning => json!({"type": "thinking", "thinking": ""}),
                    OutputPartType::Refusal => json!({"type": "refusal", "refusal": ""}),
                    OutputPartType::ToolCall => {
                        json!({"type": "tool_use", "id": format!("call_{part_index}"), "name": "", "input": {}})
                    }
                    _ => json!({"type": "text", "text": ""}),
                };
                push_sse_event(
                    &mut output,
                    "content_block_start",
                    json!({"type": "content_block_start", "index": part_index, "content_block": block}),
                );
            }
            StreamEvent::OutputPartEnded { part_index, .. } => push_sse_event(
                &mut output,
                "content_block_stop",
                json!({"type": "content_block_stop", "index": part_index}),
            ),
            StreamEvent::TextDelta { text } => push_sse_event(
                &mut output,
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            ),
            StreamEvent::ReasoningDelta { text } => push_sse_event(
                &mut output,
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": text}}),
            ),
            StreamEvent::RefusalPart { text, extensions } => {
                diagnose_extensions(
                    extensions,
                    tracker,
                    "unknown refusal fields cannot be canonically represented",
                );
                push_sse_event(
                    &mut output,
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "refusal_delta", "refusal": text}}),
                );
            }
            StreamEvent::CitationPart {
                reference,
                extensions,
            } => {
                diagnose_extensions(
                    extensions,
                    tracker,
                    "unknown citation fields cannot be canonically represented",
                );
                tracker.lossy(
                    DiagnosticCode::SemanticChange,
                    None,
                    "a standalone citation stream event was omitted from Anthropic Messages",
                );
                let _ = reference;
            }
            StreamEvent::ToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => {
                let index = if let Some(index) = part_indices.get(call_id) {
                    *index
                } else {
                    let index = next_part_index;
                    next_part_index += 1;
                    part_indices.insert(call_id.clone(), index);
                    push_sse_event(
                        &mut output,
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {
                                "type": "tool_use",
                                "id": call_id,
                                "name": name.clone().unwrap_or_default(),
                                "input": {},
                            }
                        }),
                    );
                    index
                };
                push_sse_event(
                    &mut output,
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": arguments_delta}}),
                );
            }
            StreamEvent::Usage { usage: event_usage } => usage = Some(event_usage.clone()),
            StreamEvent::Terminal { finish_reason } => {
                push_sse_event(
                    &mut output,
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": encode_stop_reason(finish_reason.as_str())},
                        "usage": usage.as_ref().map(encode_usage).unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0})),
                    }),
                );
                push_sse_event(&mut output, "message_stop", json!({"type": "message_stop"}));
                saw_terminal = true;
            }
            StreamEvent::Error { error } => {
                push_sse_event(&mut output, "error", encode_error(error, tracker));
                saw_terminal = true;
            }
            StreamEvent::Opaque { extension } => diagnose_extensions(
                std::slice::from_ref(extension),
                tracker,
                "opaque stream frames require exact retained-wire replay",
            ),
        }
    }
    if !saw_terminal {
        push_sse_event(
            &mut output,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": usage.as_ref().map(encode_usage).unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0})),
            }),
        );
        push_sse_event(&mut output, "message_stop", json!({"type": "message_stop"}));
    }
    output
}

fn close_stream_parts(
    events: &mut Vec<StreamEvent>,
    open_parts: &mut BTreeSet<u64>,
    message_id: Option<String>,
) {
    for part_index in std::mem::take(open_parts) {
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
    output.push_str("data: ");
    output.push_str(
        &serde_json::to_string(&value).expect("Anthropic stream event JSON is serializable"),
    );
    output.push_str("\n\n");
}

fn encode_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_input_tokens": usage.cache_read_tokens,
        "cache_creation_input_tokens": usage.cache_write_tokens,
    })
}

fn normalize_stop_reason(reason: &str) -> &'static str {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::STOP,
        "max_tokens" => FinishReason::LENGTH,
        "tool_use" => FinishReason::TOOL_CALLS,
        "refusal" => FinishReason::REFUSAL,
        "error" => FinishReason::ERROR,
        _ => FinishReason::STOP,
    }
}

fn encode_stop_reason(reason: &str) -> &'static str {
    match reason {
        FinishReason::LENGTH => "max_tokens",
        FinishReason::TOOL_CALLS => "tool_use",
        FinishReason::REFUSAL => "refusal",
        FinishReason::ERROR => "error",
        _ => "end_turn",
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
        Some("api_error") | Some("overloaded_error") => ErrorCategory::Server,
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

fn encode_error_type(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::InvalidRequest => "invalid_request_error",
        ErrorCategory::Authentication => "authentication_error",
        ErrorCategory::Permission => "permission_error",
        ErrorCategory::NotFound => "not_found_error",
        ErrorCategory::RateLimit => "rate_limit_error",
        ErrorCategory::Conflict => "conflict_error",
        ErrorCategory::Server | ErrorCategory::Transport | ErrorCategory::Unknown => "api_error",
    }
}

fn retry_after_ms(headers: &[ProtocolHeaderLine]) -> Option<u64> {
    headers
        .iter()
        .find(|header| header.name().eq_ignore_ascii_case("retry-after"))
        .and_then(|header| header.value().parse::<u64>().ok())
        .and_then(|seconds| seconds.checked_mul(1000))
}

fn beta_header_extensions(
    headers: &[ProtocolHeaderLine],
    profile_id: &ProfileId,
) -> Vec<OpaqueExtension> {
    headers
        .iter()
        .filter(|header| header.name().eq_ignore_ascii_case("anthropic-beta"))
        .map(|header| {
            opaque_text(
                profile_id,
                "anthropic.messages.beta_header",
                SourceLocation::Header {
                    name: "anthropic-beta".to_owned(),
                },
                header.value(),
            )
        })
        .collect()
}

fn diagnose_extensions(
    extensions: &[OpaqueExtension],
    tracker: &mut ConversionTracker,
    message: &'static str,
) {
    for extension in extensions {
        let code = if extension.issuing_profile == AnthropicProfile::Messages.profile_id() {
            DiagnosticCode::NonPortableOpaqueExtension
        } else {
            DiagnosticCode::ForwardCompatibleUnknown
        };
        tracker.lossy(code, Some(extension.source_location.clone()), message);
    }
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
) -> Result<&'a Vec<Value>, CodecError> {
    object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_shape("required field must be an array"))
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
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
            } => text.push_str(part_text),
            _ => {
                tracker.unsupported(
                    DiagnosticCode::UnsupportedFeature,
                    None,
                    "the target system field only accepts text in the frozen typed alpha subset",
                );
                let _ = context;
                return Ok(text);
            }
        }
    }
    Ok(text)
}

fn media_type_from_data_url(value: &str) -> Option<String> {
    let prefix = value.strip_prefix("data:")?.split_once(',')?.0;
    let media_type = prefix.split(';').next().unwrap_or_default();
    (!media_type.is_empty()).then_some(media_type.to_owned())
}

fn split_data_url(value: &str) -> Option<(String, String)> {
    let value = value.strip_prefix("data:")?;
    let (metadata, data) = value.split_once(',')?;
    let media_type = metadata.split(';').next()?.to_owned();
    (!media_type.is_empty()).then_some((media_type, data.to_owned()))
}

fn json_pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
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

fn invalid_shape(message: impl Into<String>) -> CodecError {
    CodecError::InvalidShape {
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

pub fn encode_base64(value: &[u8]) -> String {
    use base64::Engine;

    base64::engine::general_purpose::STANDARD.encode(value)
}

#[cfg(test)]
mod tests;
