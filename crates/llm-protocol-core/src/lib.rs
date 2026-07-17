//! Unpublished Rust reference types for LLM Protocol Alpha `0.1.0`.
//!
//! The normative contract lives under the repository's `protocol/` directory.
//! These APIs are deliberately small and provisional; they are not a public
//! package contract.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub mod cache;
pub mod sse;

pub use cache::{
    AppliedCachePlan, CacheChangeReason, CacheDirectiveKind, CachePlanApplicationError,
    CachePlanCorrelation, CachePlanError, CachePlanRecommendation, CachePreservationEntry,
    CachePreservationReport, CachePreservationStatus, CacheSegment, CacheSegmentDescriptor,
    CacheSegmentKind, CacheSegmentPlan, CorrelationError, CorrelationId, ExperimentalCacheDiff,
    HmacSha256Key,
};
pub use sse::{
    DEFAULT_MAX_SSE_FRAME_BYTES, SseField, SseFieldKind, SseFrame, SseFramer, SseFramingError,
};

/// Version shared by the normative specification, schemas, and vectors.
pub const PROTOCOL_VERSION: &str = "0.1.0";

/// The three frozen Alpha `0.1.0` profile identities.
///
/// They live in the reference core because vendor codecs must accept the same
/// profile-scoped IR envelopes without depending on one another.
pub const OPENAI_CHAT_COMPLETIONS_PROFILE: &str = "openai.chat-completions.alpha-0.1.0";
pub const OPENAI_RESPONSES_PROFILE: &str = "openai.responses.alpha-0.1.0";
pub const ANTHROPIC_MESSAGES_PROFILE: &str = "anthropic.messages.2023-06-01.alpha-0.1.0";

/// A frozen, profile-scoped wire contract.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProfileError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ProfileError::EmptyId);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        }) {
            return Err(ProfileError::InvalidId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProfileId {
    type Error = ProfileError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Provider identity is separate from API family so a provider can own several
/// distinct profile contracts.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[serde(rename = "openai")]
    OpenAi,
    Anthropic,
}

/// The API family addressed by a profile endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFamily {
    ChatCompletions,
    Responses,
    Messages,
}

/// The complete immutable identity for one alpha wire profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: ProfileId,
    pub provider: Provider,
    pub api_family: ApiFamily,
    pub endpoint: String,
    pub vendor_version_selector: Option<String>,
    pub enabled_features: BTreeSet<String>,
    pub contract_revision: String,
}

impl Profile {
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.endpoint.is_empty() || !self.endpoint.starts_with('/') {
            return Err(ProfileError::InvalidEndpoint(self.endpoint.clone()));
        }
        if self.contract_revision.is_empty() {
            return Err(ProfileError::EmptyContractRevision(self.id.clone()));
        }
        if self
            .vendor_version_selector
            .as_ref()
            .is_some_and(String::is_empty)
        {
            return Err(ProfileError::EmptyVendorVersionSelector(self.id.clone()));
        }
        if self
            .enabled_features
            .iter()
            .any(|feature| feature.is_empty())
        {
            return Err(ProfileError::EmptyFeature(self.id.clone()));
        }
        Ok(())
    }
}

/// A profile registry rejects duplicate identities so a vector can select one
/// profile unambiguously.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProfileRegistry {
    profiles: BTreeMap<ProfileId, Profile>,
}

impl ProfileRegistry {
    pub fn new(profiles: impl IntoIterator<Item = Profile>) -> Result<Self, ProfileError> {
        let mut registry = Self::default();
        for profile in profiles {
            registry.insert(profile)?;
        }
        Ok(registry)
    }

    pub fn insert(&mut self, profile: Profile) -> Result<(), ProfileError> {
        profile.validate()?;
        if self.profiles.contains_key(&profile.id) {
            return Err(ProfileError::DuplicateId(profile.id));
        }
        self.profiles.insert(profile.id.clone(), profile);
        Ok(())
    }

    pub fn get(&self, id: &ProfileId) -> Option<&Profile> {
        self.profiles.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.values()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProfileError {
    #[error("profile id cannot be empty")]
    EmptyId,
    #[error("profile id must use lowercase ASCII letters, digits, '.', '_', or '-': {0}")]
    InvalidId(String),
    #[error("profile endpoint must be an absolute path: {0}")]
    InvalidEndpoint(String),
    #[error("profile {0} has an empty contract revision")]
    EmptyContractRevision(ProfileId),
    #[error("profile {0} has an empty vendor version selector")]
    EmptyVendorVersionSelector(ProfileId),
    #[error("profile {0} has an empty enabled feature")]
    EmptyFeature(ProfileId),
    #[error("duplicate profile id: {0}")]
    DuplicateId(ProfileId),
}

/// The body family retained by a protocol envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolBodyKind {
    Json,
    Sse,
}

/// Protocol-owned header names in Alpha `0.1.0`.
pub const PROTOCOL_OWNED_HEADERS: &[&str] = &[
    "content-type",
    "retry-after",
    "anthropic-version",
    "anthropic-beta",
];

pub fn is_protocol_owned_header(name: &str) -> bool {
    PROTOCOL_OWNED_HEADERS
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// A raw protocol header line, excluding its CRLF line ending.
///
/// The raw line is retained rather than reconstructed so same-profile replay
/// preserves original casing and whitespace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProtocolHeaderLine {
    pub raw_line: String,
}

impl ProtocolHeaderLine {
    pub fn new(raw_line: impl Into<String>) -> Result<Self, EnvelopeError> {
        let raw_line = raw_line.into();
        if raw_line.is_empty() || raw_line.contains(['\r', '\n']) {
            return Err(EnvelopeError::InvalidHeaderLine(raw_line));
        }
        let Some((name, _value)) = raw_line.split_once(':') else {
            return Err(EnvelopeError::InvalidHeaderLine(raw_line));
        };
        if name.trim().is_empty() {
            return Err(EnvelopeError::InvalidHeaderLine(raw_line));
        }
        Ok(Self { raw_line })
    }

    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.raw_line.is_empty() || self.raw_line.contains(['\r', '\n']) {
            return Err(EnvelopeError::InvalidHeaderLine(self.raw_line.clone()));
        }
        let Some((name, _value)) = self.raw_line.split_once(':') else {
            return Err(EnvelopeError::InvalidHeaderLine(self.raw_line.clone()));
        };
        if name.trim().is_empty() {
            return Err(EnvelopeError::InvalidHeaderLine(self.raw_line.clone()));
        }
        Ok(())
    }

    pub fn name(&self) -> &str {
        self.raw_line
            .split_once(':')
            .map(|(name, _)| name.trim())
            .expect("ProtocolHeaderLine is validated during construction")
    }

    pub fn value(&self) -> &str {
        self.raw_line
            .split_once(':')
            .map(|(_, value)| value.trim())
            .expect("ProtocolHeaderLine is validated during construction")
    }
}

/// Original protocol wire material retained after decode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedWire {
    pub profile_id: ProfileId,
    pub status: u16,
    pub body_kind: ProtocolBodyKind,
    pub protocol_headers: Vec<ProtocolHeaderLine>,
    pub body: Vec<u8>,
}

impl RetainedWire {
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if !(100..=599).contains(&self.status) {
            return Err(EnvelopeError::InvalidStatus(self.status));
        }
        for header in &self.protocol_headers {
            header.validate()?;
            if !is_protocol_owned_header(header.name()) {
                return Err(EnvelopeError::NonProtocolHeader);
            }
        }
        Ok(())
    }
}

/// A replayable, byte-exact projection of an unmodified decoded envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayEnvelope {
    pub profile_id: ProfileId,
    pub status: u16,
    pub body_kind: ProtocolBodyKind,
    pub protocol_headers: Vec<ProtocolHeaderLine>,
    pub body: Vec<u8>,
}

/// Generic adapter metadata is deliberately outside exact replay guarantees.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdapterMetadata {
    pub generic_headers: BTreeMap<String, String>,
}

/// A decoded envelope that still owns exact retained wire material.
///
/// Its semantic value is private. Calling [`DecodedEnvelope::edit`] consumes
/// this type and returns [`ModifiedEnvelope`], making accidental use of the
/// retained raw bytes impossible after mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedEnvelope<T> {
    value: T,
    retained: RetainedWire,
    adapter_metadata: AdapterMetadata,
}

impl<T> DecodedEnvelope<T> {
    pub fn new(
        value: T,
        retained: RetainedWire,
        adapter_metadata: AdapterMetadata,
    ) -> Result<Self, EnvelopeError> {
        retained.validate()?;
        Ok(Self {
            value,
            retained,
            adapter_metadata,
        })
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn retained(&self) -> &RetainedWire {
        &self.retained
    }

    pub fn adapter_metadata(&self) -> &AdapterMetadata {
        &self.adapter_metadata
    }

    pub fn replay(&self) -> ReplayEnvelope {
        ReplayEnvelope {
            profile_id: self.retained.profile_id.clone(),
            status: self.retained.status,
            body_kind: self.retained.body_kind,
            protocol_headers: self.retained.protocol_headers.clone(),
            body: self.retained.body.clone(),
        }
    }

    /// Apply a semantic edit. The returned modified envelope has no retained
    /// body or raw protocol headers and therefore requires canonical encoding.
    pub fn edit(self, edit: impl FnOnce(&mut T)) -> ModifiedEnvelope<T> {
        let mut value = self.value;
        edit(&mut value);
        ModifiedEnvelope {
            value,
            profile_id: self.retained.profile_id,
            status: self.retained.status,
            body_kind: self.retained.body_kind,
            adapter_metadata: self.adapter_metadata,
        }
    }

    pub fn into_value(self) -> T {
        self.value
    }
}

/// A semantically changed envelope. Codec encoders must construct fresh,
/// canonical protocol headers and body bytes from this value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModifiedEnvelope<T> {
    value: T,
    profile_id: ProfileId,
    status: u16,
    body_kind: ProtocolBodyKind,
    adapter_metadata: AdapterMetadata,
}

impl<T> ModifiedEnvelope<T> {
    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn into_canonical(self) -> CanonicalEnvelope<T> {
        CanonicalEnvelope {
            value: self.value,
            profile_id: self.profile_id,
            status: self.status,
            body_kind: self.body_kind,
            adapter_metadata: self.adapter_metadata,
        }
    }
}

/// Input to a profile codec that must be encoded canonically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalEnvelope<T> {
    pub value: T,
    pub profile_id: ProfileId,
    pub status: u16,
    pub body_kind: ProtocolBodyKind,
    pub adapter_metadata: AdapterMetadata,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum EnvelopeError {
    #[error("protocol header line is invalid: {0}")]
    InvalidHeaderLine(String),
    #[error("HTTP status is outside the valid range: {0}")]
    InvalidStatus(u16),
    #[error("retained protocol headers may only contain protocol-owned header names")]
    NonProtocolHeader,
}

/// A source location for opaque vendor material.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceLocation {
    JsonPointer { pointer: String },
    SseEvent { index: u64, event: Option<String> },
    Header { name: String },
}

/// Opaque payloads retain unknown vendor fields without assigning them
/// unsupported portable semantics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum OpaquePayload {
    Json(Value),
    Text(String),
    Bytes(Vec<u8>),
}

/// Provider-owned or unknown data scoped to its issuing profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpaqueExtension {
    pub issuing_profile: ProfileId,
    pub namespace: String,
    pub source_location: SourceLocation,
    pub payload: OpaquePayload,
}

impl OpaqueExtension {
    pub fn portability_to(&self, target_profile: &ProfileId) -> Option<Diagnostic> {
        (self.issuing_profile != *target_profile).then(|| {
            Diagnostic::warning(
                DiagnosticCode::ForwardCompatibleUnknown,
                Some(self.source_location.clone()),
                "opaque provider material cannot be assigned portable cross-profile semantics",
            )
        })
    }
}

/// An opaque provider continuation token, valid only for the exact profile
/// that issued it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContinuationHandle {
    pub issuing_profile: ProfileId,
    pub extension: OpaqueExtension,
}

impl ContinuationHandle {
    pub fn is_issued_by(&self, profile: &ProfileId) -> bool {
        self.issuing_profile == *profile && self.extension.issuing_profile == *profile
    }

    pub fn portability_to(&self, target_profile: &ProfileId) -> Option<Diagnostic> {
        (!self.is_issued_by(target_profile)).then(|| {
            Diagnostic::warning(
                DiagnosticCode::NonPortableContinuationHandle,
                Some(self.extension.source_location.clone()),
                "provider continuation handles require consistent issuing-profile metadata and are valid only for that profile",
            )
        })
    }
}

/// Roles normalized by the alpha IR.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// An ordered conversation message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: ConversationRole,
    pub name: Option<String>,
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub extensions: Vec<OpaqueExtension>,
}

/// A reference to an image or document. The alpha transfers no binary assets.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssetReference {
    pub reference_type: AssetReferenceType,
    pub value: String,
    pub media_type: Option<String>,
    pub name: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetReferenceType {
    Url,
    Data,
    ProviderFile,
}

/// Ordered typed content in a message or request instruction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        asset: AssetReference,
    },
    Document {
        asset: AssetReference,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        #[serde(default)]
        extensions: Vec<OpaqueExtension>,
    },
    ToolResult {
        tool_call_id: String,
        content: Vec<ContentPart>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        extensions: Vec<OpaqueExtension>,
    },
    Reasoning {
        summary: Option<String>,
        opaque: Option<OpaqueExtension>,
    },
    Citation {
        reference: Value,
        #[serde(default)]
        extensions: Vec<OpaqueExtension>,
    },
    Refusal {
        text: String,
        #[serde(default)]
        extensions: Vec<OpaqueExtension>,
    },
    Opaque {
        extension: OpaqueExtension,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub strict: Option<bool>,
    #[serde(default)]
    pub extensions: Vec<OpaqueExtension>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationControls {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
}

/// Intent to ask a vendor for JSON Schema shaped output. The protocol does not
/// validate generated output against this schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonSchemaOutputIntent {
    pub name: Option<String>,
    pub description: Option<String>,
    pub schema: Value,
    pub enforcement: OutputSchemaEnforcement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSchemaEnforcement {
    Required,
    Preferred,
}

/// Source cache semantics. These variants intentionally do not claim that
/// their directives have equivalent behavior across vendors.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CacheIntent {
    #[serde(rename = "openai")]
    OpenAi(OpenAiCacheIntent),
    Anthropic(AnthropicCacheIntent),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OpenAiCacheIntent {
    pub request_cache_key: Option<String>,
    pub retention: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnthropicCacheIntent {
    #[serde(default)]
    pub breakpoints: Vec<AnthropicCacheBreakpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnthropicCacheBreakpoint {
    pub location: CacheLocation,
    pub ttl: Option<String>,
}

/// Stable structural locations used by future cache-segment reporting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CacheLocation {
    Instructions {
        part_index: usize,
    },
    Message {
        message_index: usize,
    },
    MessagePart {
        message_index: usize,
        part_index: usize,
    },
    ToolDefinition {
        tool_index: usize,
    },
    OutputSchema,
    Asset {
        message_index: usize,
        part_index: usize,
    },
    InstructionAsset {
        part_index: usize,
    },
    CacheDirective {
        directive_index: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtocolRequest {
    pub model: Option<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub instructions: Vec<ContentPart>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub generation: GenerationControls,
    pub output_schema: Option<JsonSchemaOutputIntent>,
    pub cache_intent: Option<CacheIntent>,
    pub continuation: Option<ContinuationHandle>,
    #[serde(default)]
    pub extensions: Vec<OpaqueExtension>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtocolResponse {
    pub id: Option<String>,
    pub model: Option<String>,
    pub output: Vec<Message>,
    pub usage: Option<Usage>,
    pub finish_reason: FinishReason,
    pub continuation: Option<ContinuationHandle>,
    #[serde(default)]
    pub extensions: Vec<OpaqueExtension>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FinishReason(String);

impl FinishReason {
    pub const STOP: &'static str = "stop";
    pub const LENGTH: &'static str = "length";
    pub const TOOL_CALLS: &'static str = "tool_calls";
    pub const CONTENT_FILTER: &'static str = "content_filter";
    pub const REFUSAL: &'static str = "refusal";
    pub const ERROR: &'static str = "error";

    pub fn new(value: impl Into<String>) -> Result<Self, IrError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IrError::EmptyFinishReason);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub category: ErrorCategory,
    pub code: String,
    pub message: String,
    pub retry_after_ms: Option<u64>,
    pub param: Option<String>,
    #[serde(default)]
    pub extensions: Vec<OpaqueExtension>,
}

/// The shared, vendor-neutral payload carried by decoded and canonical
/// envelopes. Vendor codec APIs use this type directly so every dialect pair
/// crosses the same IR boundary rather than a pair-specific translator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum ProtocolPayload {
    Request(ProtocolRequest),
    Response(ProtocolResponse),
    Error(ProtocolError),
    Stream(Vec<StreamEvent>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    InvalidRequest,
    Authentication,
    Permission,
    NotFound,
    RateLimit,
    Conflict,
    Server,
    Transport,
    Unknown,
}

/// Profile-independent lifecycle events for streamed responses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    RequestStarted,
    MessageStarted {
        message_id: Option<String>,
    },
    OutputPartStarted {
        message_id: Option<String>,
        part_index: usize,
        part_type: OutputPartType,
    },
    OutputPartEnded {
        message_id: Option<String>,
        part_index: usize,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    RefusalPart {
        text: String,
        #[serde(default)]
        extensions: Vec<OpaqueExtension>,
    },
    CitationPart {
        reference: Value,
        #[serde(default)]
        extensions: Vec<OpaqueExtension>,
    },
    ToolCallDelta {
        call_id: String,
        name: Option<String>,
        arguments_delta: String,
    },
    Usage {
        usage: Usage,
    },
    Terminal {
        finish_reason: FinishReason,
    },
    Error {
        error: ProtocolError,
    },
    Opaque {
        extension: OpaqueExtension,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputPartType {
    Text,
    Image,
    Document,
    ToolCall,
    ToolResult,
    Reasoning,
    Citation,
    Refusal,
}

/// Fidelity is chosen by the codec that performed the conversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    Exact,
    Adapted,
    Lossy,
    Unsupported,
}

/// The acceptance policy is an explicit caller decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptancePolicy {
    ExactOnly,
    AllowAdapted,
    AllowLossy,
}

impl AcceptancePolicy {
    pub fn accepts(self, fidelity: Fidelity) -> bool {
        match self {
            Self::ExactOnly => fidelity == Fidelity::Exact,
            Self::AllowAdapted => matches!(fidelity, Fidelity::Exact | Fidelity::Adapted),
            Self::AllowLossy => {
                matches!(
                    fidelity,
                    Fidelity::Exact | Fidelity::Adapted | Fidelity::Lossy
                )
            }
        }
    }
}

/// The stable machine-readable diagnostic codes used by the reference core.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    CachePlanApplied,
    ForwardCompatibleUnknown,
    NonPortableCacheIntent,
    NonPortableContinuationHandle,
    NonPortableOpaqueExtension,
    UnsupportedFeature,
    SemanticChange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub location: Option<SourceLocation>,
    pub message: String,
}

impl Diagnostic {
    pub fn warning(
        code: DiagnosticCode,
        location: Option<SourceLocation>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Warning,
            location,
            message: message.into(),
        }
    }
}

/// A codec output and its transparent conversion report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversionResult<T> {
    pub output: Option<T>,
    pub fidelity: Fidelity,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum IrError {
    #[error("finish reason cannot be empty")]
    EmptyFinishReason,
}

impl<T> ConversionResult<T> {
    pub fn exact(output: T) -> Self {
        Self {
            output: Some(output),
            fidelity: Fidelity::Exact,
            diagnostics: Vec::new(),
        }
    }

    pub fn adapted(output: T, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            output: Some(output),
            fidelity: Fidelity::Adapted,
            diagnostics,
        }
    }

    pub fn lossy(output: T, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            output: Some(output),
            fidelity: Fidelity::Lossy,
            diagnostics,
        }
    }

    pub fn unsupported(diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            output: None,
            fidelity: Fidelity::Unsupported,
            diagnostics,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn protocol_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol")
    }

    fn read_protocol_json(relative_path: &str) -> Value {
        let bytes = std::fs::read(protocol_root().join(relative_path))
            .unwrap_or_else(|error| panic!("read protocol artifact {relative_path}: {error}"));
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parse protocol artifact {relative_path}: {error}"))
    }

    fn validate_with_schema(schema_path: &str, instance_path: &str) {
        let validator = compile_protocol_schema(schema_path);
        let instance = read_protocol_json(instance_path);
        let errors: Vec<_> = validator.iter_errors(&instance).collect();
        assert!(
            errors.is_empty(),
            "{instance_path} fails {schema_path}:\n{}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    fn validation_errors(schema_path: &str, instance: &Value) -> Vec<String> {
        let validator = compile_protocol_schema(schema_path);
        validator
            .iter_errors(instance)
            .map(|error| error.to_string())
            .collect()
    }

    fn compile_protocol_schema(schema_path: &str) -> jsonschema::Validator {
        const CACHE_REPORT_SCHEMA_ID: &str =
            "https://onair.dev/llm-protocol-alpha/0.1.0/cache-report.schema.json";
        const DIAGNOSTIC_SCHEMA_ID: &str =
            "https://onair.dev/llm-protocol-alpha/0.1.0/diagnostic.schema.json";
        const ENVELOPE_SCHEMA_ID: &str =
            "https://onair.dev/llm-protocol-alpha/0.1.0/envelope.schema.json";
        const IR_SCHEMA_ID: &str = "https://onair.dev/llm-protocol-alpha/0.1.0/ir.schema.json";

        let schema = read_protocol_json(schema_path);
        let cache_report_schema = read_protocol_json("schemas/cache-report.schema.json");
        let diagnostic_schema = read_protocol_json("schemas/diagnostic.schema.json");
        let envelope_schema = read_protocol_json("schemas/envelope.schema.json");
        let ir_schema = read_protocol_json("schemas/ir.schema.json");
        jsonschema::options()
            .with_resource(
                CACHE_REPORT_SCHEMA_ID,
                jsonschema::Resource::from_contents(cache_report_schema),
            )
            .with_resource(
                DIAGNOSTIC_SCHEMA_ID,
                jsonschema::Resource::from_contents(diagnostic_schema),
            )
            .with_resource(
                ENVELOPE_SCHEMA_ID,
                jsonschema::Resource::from_contents(envelope_schema),
            )
            .with_resource(IR_SCHEMA_ID, jsonschema::Resource::from_contents(ir_schema))
            .build(&schema)
            .unwrap_or_else(|error| panic!("compile schema {schema_path}: {error}"))
    }

    fn profile_id(value: &str) -> ProfileId {
        ProfileId::new(value).expect("valid test profile id")
    }

    fn valid_profile(id: &str) -> Profile {
        Profile {
            id: profile_id(id),
            provider: Provider::OpenAi,
            api_family: ApiFamily::ChatCompletions,
            endpoint: "/v1/chat/completions".to_owned(),
            vendor_version_selector: None,
            enabled_features: BTreeSet::from(["sse".to_owned()]),
            contract_revision: "test.snapshot".to_owned(),
        }
    }

    #[test]
    fn profile_registry_rejects_duplicates() {
        let result = ProfileRegistry::new([
            valid_profile("openai.chat.test"),
            valid_profile("openai.chat.test"),
        ]);

        assert_eq!(
            result,
            Err(ProfileError::DuplicateId(profile_id("openai.chat.test")))
        );
    }

    #[test]
    fn openai_cache_intent_uses_the_normative_provider_spelling() {
        let intent = CacheIntent::OpenAi(OpenAiCacheIntent {
            request_cache_key: Some("synthetic-cache-key".to_owned()),
            retention: Some("24h".to_owned()),
        });
        let encoded = serde_json::to_value(&intent).unwrap();

        assert_eq!(encoded["kind"], "openai");
        assert_eq!(
            serde_json::from_value::<CacheIntent>(encoded).unwrap(),
            intent
        );
    }

    #[test]
    fn unmodified_envelope_replays_exact_protocol_material() {
        let profile = profile_id("openai.chat.test");
        let retained = RetainedWire {
            profile_id: profile.clone(),
            status: 200,
            body_kind: ProtocolBodyKind::Json,
            protocol_headers: vec![
                ProtocolHeaderLine::new("Content-Type: application/json").unwrap(),
                ProtocolHeaderLine::new("Retry-After: 1").unwrap(),
            ],
            body: br#"{"id":"synthetic-1","choices":[]}"#.to_vec(),
        };
        let envelope = DecodedEnvelope::new(
            "decoded value",
            retained.clone(),
            AdapterMetadata::default(),
        )
        .unwrap();

        assert_eq!(envelope.replay().body, retained.body);
        assert_eq!(
            envelope.replay().protocol_headers,
            retained.protocol_headers
        );

        let modified = envelope.edit(|value| *value = "changed value");
        let canonical = modified.into_canonical();
        assert_eq!(canonical.value, "changed value");
        assert_eq!(canonical.profile_id, profile);
        assert_eq!(canonical.status, 200);
    }

    #[test]
    fn opaque_extensions_and_handles_are_not_portable() {
        let source = profile_id("openai.responses.test");
        let target = profile_id("anthropic.messages.test");
        let extension = OpaqueExtension {
            issuing_profile: source.clone(),
            namespace: "openai.responses.future_event".to_owned(),
            source_location: SourceLocation::JsonPointer {
                pointer: "/output/0/future_field".to_owned(),
            },
            payload: OpaquePayload::Json(serde_json::json!({"synthetic": true})),
        };
        let handle = ContinuationHandle {
            issuing_profile: source,
            extension: extension.clone(),
        };

        assert_eq!(
            extension.portability_to(&target).unwrap().code,
            DiagnosticCode::ForwardCompatibleUnknown
        );
        assert_eq!(
            handle.portability_to(&target).unwrap().code,
            DiagnosticCode::NonPortableContinuationHandle
        );
        assert!(handle.is_issued_by(&handle.issuing_profile));

        let mut contradictory = handle.clone();
        contradictory.extension.issuing_profile = target;
        assert!(!contradictory.is_issued_by(&contradictory.issuing_profile));
        assert_eq!(
            contradictory
                .portability_to(&contradictory.issuing_profile)
                .unwrap()
                .code,
            DiagnosticCode::NonPortableContinuationHandle
        );
    }

    #[test]
    fn acceptance_policy_requires_an_explicit_lossy_opt_in() {
        assert!(AcceptancePolicy::ExactOnly.accepts(Fidelity::Exact));
        assert!(!AcceptancePolicy::ExactOnly.accepts(Fidelity::Adapted));
        assert!(AcceptancePolicy::AllowAdapted.accepts(Fidelity::Adapted));
        assert!(!AcceptancePolicy::AllowAdapted.accepts(Fidelity::Lossy));
        assert!(AcceptancePolicy::AllowLossy.accepts(Fidelity::Lossy));
        assert!(!AcceptancePolicy::AllowLossy.accepts(Fidelity::Unsupported));
    }

    #[test]
    fn ir_content_parts_serialize_to_the_normative_flat_shape() {
        let part = ContentPart::ToolResult {
            tool_call_id: "call-synthetic".to_owned(),
            content: vec![ContentPart::Text {
                text: "synthetic result".to_owned(),
            }],
            is_error: false,
            extensions: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(part).unwrap(),
            serde_json::json!({
                "type": "tool_result",
                "tool_call_id": "call-synthetic",
                "content": [{"type": "text", "text": "synthetic result"}],
                "is_error": false,
                "extensions": []
            })
        );
        assert_eq!(
            serde_json::to_value(FinishReason::new(FinishReason::TOOL_CALLS).unwrap()).unwrap(),
            serde_json::json!("tool_calls")
        );
    }

    #[test]
    fn ir_stream_event_schema_rejects_incomplete_and_unknown_shapes() {
        for event in [
            serde_json::json!({"type": "text_delta"}),
            serde_json::json!({"type": "usage", "usage": "not-an-object"}),
            serde_json::json!({"type": "terminal", "finish_reason": "stop", "extra": true}),
            serde_json::json!({
                "type": "error",
                "error": {
                    "category": "not-a-category",
                    "code": "synthetic_error",
                    "message": "synthetic failure"
                }
            }),
        ] {
            let document = serde_json::json!({
                "protocol_version": PROTOCOL_VERSION,
                "profile_id": OPENAI_RESPONSES_PROFILE,
                "kind": "stream",
                "payload": [event]
            });
            assert!(
                !validation_errors("schemas/ir.schema.json", &document).is_empty(),
                "malformed stream event unexpectedly passed the normative schema"
            );
        }
    }

    #[test]
    fn committed_protocol_json_is_well_formed_and_uses_alpha_version() {
        const JSON_ARTIFACTS: &[&[u8]] = &[
            include_bytes!("../../../protocol/profiles/registry.json"),
            include_bytes!("../../../protocol/schemas/profile-registry.schema.json"),
            include_bytes!("../../../protocol/schemas/envelope.schema.json"),
            include_bytes!("../../../protocol/schemas/ir.schema.json"),
            include_bytes!("../../../protocol/schemas/cache-report.schema.json"),
            include_bytes!("../../../protocol/schemas/diagnostic.schema.json"),
            include_bytes!("../../../protocol/schemas/vector.schema.json"),
            include_bytes!("../../../protocol/schemas/vector-manifest.schema.json"),
            include_bytes!("../../../protocol/vectors/manifest.json"),
            include_bytes!("../../../protocol/vectors/core/exact-replay.chat-json.json"),
            include_bytes!("../../../protocol/vectors/core/opaque-extension.cross-profile.json"),
            include_bytes!("../../../protocol/vectors/core/cache-report.content-free.json"),
            include_bytes!("../../../protocol/vectors/openai/chat.request.text-tool.json"),
            include_bytes!("../../../protocol/vectors/openai/responses.request.text-tool.json"),
            include_bytes!("../../../protocol/vectors/openai/chat.response.text-tool.json"),
            include_bytes!("../../../protocol/vectors/openai/responses.response.text-tool.json"),
            include_bytes!("../../../protocol/vectors/openai/chat.error.cross-profile.json"),
            include_bytes!("../../../protocol/vectors/openai/chat.stream.text-tool.json"),
            include_bytes!("../../../protocol/vectors/openai/responses.stream.text-tool.json"),
            include_bytes!("../../../protocol/vectors/openai/chat.replay.unmodified.json"),
            include_bytes!(
                "../../../protocol/vectors/openai/responses.request.continuation-unsupported.json"
            ),
            include_bytes!("../../../protocol/vectors/openai/chat.request.unknown-lossy.json"),
            include_bytes!(
                "../../../protocol/vectors/anthropic/messages.request.cache-breakpoint.json"
            ),
            include_bytes!("../../../protocol/vectors/anthropic/messages.response.text-tool.json"),
            include_bytes!("../../../protocol/vectors/anthropic/messages.error.cross-profile.json"),
            include_bytes!("../../../protocol/vectors/anthropic/messages.stream.text-tool.json"),
            include_bytes!("../../../protocol/vectors/anthropic/messages.replay.unmodified.json"),
            include_bytes!(
                "../../../protocol/vectors/anthropic/messages.stream.replay.unmodified.json"
            ),
            include_bytes!("../../../protocol/vectors/anthropic/messages.unknown-lossy.json"),
            include_bytes!("../../../protocol/vectors/openai/chat.request.to-messages.json"),
            include_bytes!("../../../protocol/vectors/openai/responses.request.to-messages.json"),
            include_bytes!("../../../protocol/vectors/openai/chat.stream.to-messages.json"),
            include_bytes!("../../../protocol/vectors/openai/responses.stream.to-messages.json"),
            include_bytes!(
                "../../../protocol/vectors/openai/chat.request.cache-to-messages-lossy.json"
            ),
            include_bytes!(
                "../../../protocol/vectors/openai/responses.request.continuation-to-messages-unsupported.json"
            ),
            include_bytes!("../../../protocol/benchmarks/scenarios.json"),
        ];

        for artifact in JSON_ARTIFACTS {
            let value: Value = serde_json::from_slice(artifact).expect("valid committed JSON");
            if let Some(version) = value.get("protocol_version") {
                assert_eq!(version, PROTOCOL_VERSION);
            }
        }
    }

    #[test]
    fn protocol_schemas_compile_as_draft_2020_12() {
        for schema_path in [
            "schemas/profile-registry.schema.json",
            "schemas/envelope.schema.json",
            "schemas/ir.schema.json",
            "schemas/cache-report.schema.json",
            "schemas/diagnostic.schema.json",
            "schemas/vector.schema.json",
            "schemas/vector-manifest.schema.json",
        ] {
            compile_protocol_schema(schema_path);
        }
    }

    #[test]
    fn profile_registry_and_vector_manifest_validate_against_their_schemas() {
        validate_with_schema(
            "schemas/profile-registry.schema.json",
            "profiles/registry.json",
        );
        validate_with_schema(
            "schemas/vector-manifest.schema.json",
            "vectors/manifest.json",
        );
    }

    #[test]
    fn profile_registry_schema_rejects_unknown_metadata() {
        let mut registry = read_protocol_json("profiles/registry.json");
        registry["unexpected_metadata"] = Value::Bool(true);

        let errors = validation_errors("schemas/profile-registry.schema.json", &registry);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("Additional properties are not allowed"))
        );
    }

    #[test]
    fn active_vectors_validate_against_the_vector_schema_and_reference_profiles() {
        let manifest = read_protocol_json("vectors/manifest.json");
        assert_eq!(manifest["protocol_version"], PROTOCOL_VERSION);
        let registry = read_protocol_json("profiles/registry.json");
        let profile_ids: BTreeSet<_> = registry["profiles"]
            .as_array()
            .expect("profile registry contains an array")
            .iter()
            .map(|profile| profile["id"].as_str().expect("profile has an id"))
            .collect();

        for vector in manifest["vectors"]
            .as_array()
            .expect("vector manifest contains an array")
            .iter()
            .filter(|vector| vector["status"] == "active")
        {
            let relative_path = vector["path"]
                .as_str()
                .expect("active vector has a relative path");
            let document_path = format!("vectors/{relative_path}");
            validate_with_schema("schemas/vector.schema.json", &document_path);
            let document = read_protocol_json(&document_path);

            assert_eq!(document["protocol_version"], PROTOCOL_VERSION);
            assert_eq!(document["synthetic"], true);
            assert_eq!(
                document["id"],
                vector["id"].as_str().expect("manifest vector has an id")
            );
            assert_eq!(
                document["kind"],
                vector["kind"].as_str().expect("manifest vector has a kind")
            );
            assert!(
                document["source_profile"]
                    .as_str()
                    .is_some_and(|profile| !profile.is_empty())
            );
            assert!(
                document["target_profile"]
                    .as_str()
                    .is_some_and(|profile| !profile.is_empty())
            );
            assert!(
                profile_ids.contains(
                    document["source_profile"]
                        .as_str()
                        .expect("active vector has a source profile")
                )
            );
            assert!(
                profile_ids.contains(
                    document["target_profile"]
                        .as_str()
                        .expect("active vector has a target profile")
                )
            );
            if document["kind"] == "cache_analysis" {
                assert!(document["expect"]["analysis"]["result_ir"].is_object());
                assert!(document["expect"]["analysis"]["diagnostics"].is_array());
            } else {
                assert!(document["expect"]["decode"]["ir"].is_object());
                assert!(document["expect"]["decode"]["diagnostics"].is_array());
                assert!(document["expect"]["encode"]["diagnostics"].is_array());
            }
            assert_cache_report_is_content_free(&document);
        }
    }

    fn assert_cache_report_is_content_free(document: &Value) {
        let Some(cache_report) = document["expect"].get("cache_report") else {
            return;
        };
        for forbidden_key in [
            "content",
            "prompt",
            "text",
            "fingerprint",
            "hash",
            "correlation_id",
            "request_cache_key",
            "retention",
        ] {
            assert!(
                !contains_key(cache_report, forbidden_key),
                "cache report fixture must not contain {forbidden_key}"
            );
        }
    }

    fn contains_key(value: &Value, needle: &str) -> bool {
        match value {
            Value::Object(object) => {
                object.contains_key(needle)
                    || object.values().any(|value| contains_key(value, needle))
            }
            Value::Array(values) => values.iter().any(|value| contains_key(value, needle)),
            _ => false,
        }
    }

    #[test]
    fn profile_registry_contains_exactly_the_three_initial_pinned_profiles() {
        let registry: Value =
            serde_json::from_slice(include_bytes!("../../../protocol/profiles/registry.json"))
                .expect("valid profile registry JSON");
        let profiles = registry["profiles"]
            .as_array()
            .expect("profile registry contains an array");
        assert_eq!(registry["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(profiles.len(), 3);

        let ids: BTreeSet<_> = profiles
            .iter()
            .map(|profile| {
                assert!(
                    profile["contract_revision"]
                        .as_str()
                        .is_some_and(|revision| !revision.is_empty())
                );
                assert_ne!(profile["id"], "latest");
                assert_ne!(profile["vendor_version_selector"], "latest");
                profile["id"].as_str().expect("profile has an id")
            })
            .collect();
        assert_eq!(
            ids,
            BTreeSet::from([
                "openai.chat-completions.alpha-0.1.0",
                "openai.responses.alpha-0.1.0",
                "anthropic.messages.2023-06-01.alpha-0.1.0",
            ])
        );
    }
}
