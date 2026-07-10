//! Cache intent analysis for the unpublished protocol alpha.
//!
//! Reports deliberately describe structure, not prompt content. Segment
//! payloads stay private to the plan and are used only for in-memory equality
//! and keyed deployment-local HMAC correlation.

use std::{collections::BTreeSet, fmt};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

use crate::{
    AnthropicCacheBreakpoint, AnthropicCacheIntent, CacheIntent, CacheLocation, ContentPart,
    Diagnostic, DiagnosticCode, DiagnosticSeverity, Fidelity, OpenAiCacheIntent, ProtocolRequest,
};

type HmacSha256 = Hmac<Sha256>;

/// A stable structural category used by cache reports and comparisons.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheSegmentKind {
    Instruction,
    Message,
    ContentPart,
    ToolDefinition,
    OutputSchema,
    Asset,
    CacheDirective,
}

/// Content-free identity for a cache-relevant piece of a request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CacheSegmentDescriptor {
    pub kind: CacheSegmentKind,
    pub location: CacheLocation,
}

/// A canonical ordered cache segment. Semantic material is intentionally
/// private; callers can observe only the descriptor.
#[derive(Clone, Eq, PartialEq)]
pub struct CacheSegment {
    descriptor: CacheSegmentDescriptor,
    canonical_bytes: Vec<u8>,
}

impl fmt::Debug for CacheSegment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CacheSegment")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl CacheSegment {
    pub fn descriptor(&self) -> &CacheSegmentDescriptor {
        &self.descriptor
    }

    fn matches(&self, other: &Self) -> bool {
        self.descriptor == other.descriptor && self.canonical_bytes == other.canonical_bytes
    }
}

/// Canonical ordered source representation for cache comparison.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CacheSegmentPlan {
    segments: Vec<CacheSegment>,
}

impl CacheSegmentPlan {
    pub fn analyze(request: &ProtocolRequest) -> Result<Self, CachePlanError> {
        let mut segments = Vec::new();

        for (part_index, part) in request.instructions.iter().enumerate() {
            push_segment(
                &mut segments,
                CacheSegmentKind::Instruction,
                CacheLocation::Instructions { part_index },
                part,
            )?;
            if is_asset(part) {
                push_segment(
                    &mut segments,
                    CacheSegmentKind::Asset,
                    CacheLocation::InstructionAsset { part_index },
                    part,
                )?;
            }
        }

        for (message_index, message) in request.messages.iter().enumerate() {
            push_segment(
                &mut segments,
                CacheSegmentKind::Message,
                CacheLocation::Message { message_index },
                message,
            )?;
            for (part_index, part) in message.content.iter().enumerate() {
                push_segment(
                    &mut segments,
                    CacheSegmentKind::ContentPart,
                    CacheLocation::MessagePart {
                        message_index,
                        part_index,
                    },
                    part,
                )?;
                if is_asset(part) {
                    push_segment(
                        &mut segments,
                        CacheSegmentKind::Asset,
                        CacheLocation::Asset {
                            message_index,
                            part_index,
                        },
                        part,
                    )?;
                }
            }
        }

        for (tool_index, tool) in request.tools.iter().enumerate() {
            push_segment(
                &mut segments,
                CacheSegmentKind::ToolDefinition,
                CacheLocation::ToolDefinition { tool_index },
                tool,
            )?;
        }

        if let Some(output_schema) = &request.output_schema {
            push_segment(
                &mut segments,
                CacheSegmentKind::OutputSchema,
                CacheLocation::OutputSchema,
                output_schema,
            )?;
        }

        if let Some(cache_intent) = &request.cache_intent {
            push_cache_directives(&mut segments, cache_intent)?;
        }

        Ok(Self { segments })
    }

    pub fn segments(&self) -> &[CacheSegment] {
        &self.segments
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &CacheSegmentDescriptor> {
        self.segments.iter().map(CacheSegment::descriptor)
    }

    pub fn correlate(&self, key: &HmacSha256Key) -> CorrelationId {
        let mut mac = HmacSha256::new_from_slice(key.as_bytes())
            .expect("HMAC-SHA-256 accepts keys of every length");
        mac.update(b"llm-protocol-alpha/cache-plan/v1\0");
        for segment in &self.segments {
            let descriptor = serde_json::to_vec(&segment.descriptor)
                .expect("cache segment descriptors are always serializable");
            update_length_prefixed(&mut mac, &descriptor);
            update_length_prefixed(&mut mac, &segment.canonical_bytes);
        }
        CorrelationId::new(mac.finalize().into_bytes().into())
    }

    pub fn experimental_diff(&self, target: &Self) -> ExperimentalCacheDiff {
        let common_stable_prefix_len = self
            .segments
            .iter()
            .zip(&target.segments)
            .take_while(|(source, target)| source.matches(target))
            .count();
        let mut source_entries = Vec::new();
        let mut target_entries = Vec::new();
        let mut matched_target_indices = BTreeSet::new();

        for (source_index, source) in self
            .segments
            .iter()
            .enumerate()
            .skip(common_stable_prefix_len)
        {
            if let Some(target_segment) = target.segments.get(source_index)
                && target_segment.matches(source)
            {
                matched_target_indices.insert(source_index);
                source_entries.push(CachePreservationEntry {
                    source: Some(source.descriptor.clone()),
                    target: Some(target_segment.descriptor.clone()),
                    status: CachePreservationStatus::Preserved,
                    reason: CacheChangeReason::Unchanged,
                });
                continue;
            }

            if let Some((target_index, target_segment)) =
                target
                    .segments
                    .iter()
                    .enumerate()
                    .find(|(target_index, target_segment)| {
                        !matched_target_indices.contains(target_index)
                            && target_segment.matches(source)
                    })
            {
                matched_target_indices.insert(target_index);
                source_entries.push(CachePreservationEntry {
                    source: Some(source.descriptor.clone()),
                    target: Some(target_segment.descriptor.clone()),
                    status: CachePreservationStatus::Moved,
                    reason: CacheChangeReason::OrderChanged,
                });
                continue;
            }

            if let Some((target_index, target_segment)) =
                target
                    .segments
                    .iter()
                    .enumerate()
                    .find(|(target_index, target_segment)| {
                        !matched_target_indices.contains(target_index)
                            && target_segment.descriptor == source.descriptor
                    })
            {
                matched_target_indices.insert(target_index);
                source_entries.push(CachePreservationEntry {
                    source: Some(source.descriptor.clone()),
                    target: Some(target_segment.descriptor.clone()),
                    status: CachePreservationStatus::Changed,
                    reason: CacheChangeReason::SemanticValueChanged,
                });
                continue;
            }

            if let Some((target_index, target_segment)) =
                target
                    .segments
                    .iter()
                    .enumerate()
                    .find(|(target_index, target_segment)| {
                        !matched_target_indices.contains(target_index)
                            && target_segment.canonical_bytes == source.canonical_bytes
                    })
            {
                matched_target_indices.insert(target_index);
                source_entries.push(CachePreservationEntry {
                    source: Some(source.descriptor.clone()),
                    target: Some(target_segment.descriptor.clone()),
                    status: CachePreservationStatus::Moved,
                    reason: CacheChangeReason::OrderChanged,
                });
                continue;
            }

            source_entries.push(CachePreservationEntry {
                source: Some(source.descriptor.clone()),
                target: None,
                status: CachePreservationStatus::Dropped,
                reason: CacheChangeReason::NoTargetRepresentation,
            });
        }

        for (target_index, target_segment) in target.segments.iter().enumerate() {
            if target_index < common_stable_prefix_len
                || matched_target_indices.contains(&target_index)
            {
                continue;
            }
            target_entries.push(CachePreservationEntry {
                source: None,
                target: Some(target_segment.descriptor.clone()),
                status: CachePreservationStatus::Introduced,
                reason: CacheChangeReason::NoSourceRepresentation,
            });
        }

        ExperimentalCacheDiff {
            common_stable_prefix_len,
            source_entries,
            target_entries,
        }
    }
}

/// Current output for arbitrary IR-to-IR cache comparison.
///
/// This API is explicitly experimental. The structural comparison semantics may
/// evolve after codec consumers establish which distinctions they need.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExperimentalCacheDiff {
    pub common_stable_prefix_len: usize,
    pub source_entries: Vec<CachePreservationEntry>,
    pub target_entries: Vec<CachePreservationEntry>,
}

/// Stable source-to-target cache preservation report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CachePreservationReport {
    pub entries: Vec<CachePreservationEntry>,
}

impl CachePreservationReport {
    pub fn from_plan_diff(diff: &ExperimentalCacheDiff) -> Self {
        let mut entries = diff.source_entries.clone();
        entries.extend(diff.target_entries.clone());
        Self { entries }
    }

    /// Report that all structural segments, including source cache directives,
    /// are represented by an equivalent target profile.
    pub fn preserved(plan: &CacheSegmentPlan) -> Self {
        Self {
            entries: plan
                .descriptors()
                .map(|descriptor| CachePreservationEntry {
                    source: Some(descriptor.clone()),
                    target: Some(descriptor.clone()),
                    status: CachePreservationStatus::Preserved,
                    reason: CacheChangeReason::Unchanged,
                })
                .collect(),
        }
    }

    /// Report a cross-provider conversion that preserves typed request
    /// structure but intentionally does not synthesize target cache
    /// directives. Only directives are non-portable; all other segments retain
    /// their structural locations.
    pub fn with_non_portable_directives(plan: &CacheSegmentPlan) -> Self {
        Self {
            entries: plan
                .descriptors()
                .map(|descriptor| {
                    let is_directive = descriptor.kind == CacheSegmentKind::CacheDirective;
                    CachePreservationEntry {
                        source: Some(descriptor.clone()),
                        target: (!is_directive).then(|| descriptor.clone()),
                        status: if is_directive {
                            CachePreservationStatus::NonPortable
                        } else {
                            CachePreservationStatus::Preserved
                        },
                        reason: if is_directive {
                            CacheChangeReason::ProviderSemanticsDiffer
                        } else {
                            CacheChangeReason::Unchanged
                        },
                    }
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CachePreservationEntry {
    pub source: Option<CacheSegmentDescriptor>,
    pub target: Option<CacheSegmentDescriptor>,
    pub status: CachePreservationStatus,
    pub reason: CacheChangeReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePreservationStatus {
    Preserved,
    Moved,
    Changed,
    Dropped,
    Introduced,
    NonPortable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheChangeReason {
    Unchanged,
    SemanticValueChanged,
    OrderChanged,
    NoTargetRepresentation,
    NoSourceRepresentation,
    TargetDirectiveMustBeExplicit,
    ProviderSemanticsDiffer,
}

/// A target-specific cache-directive recommendation. Converting a request does
/// not apply this automatically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachePlanRecommendation {
    pub target_intent: CacheIntent,
    pub report: CachePreservationReport,
}

impl CachePlanRecommendation {
    pub fn apply(
        &self,
        request: ProtocolRequest,
    ) -> Result<AppliedCachePlan, CachePlanApplicationError> {
        if request.cache_intent.as_ref() == Some(&self.target_intent) {
            return Err(CachePlanApplicationError::AlreadyApplied);
        }

        Ok(AppliedCachePlan {
            request: ProtocolRequest {
                cache_intent: Some(self.target_intent.clone()),
                ..request
            },
            report: self.report.clone(),
            fidelity: Fidelity::Adapted,
            diagnostics: vec![Diagnostic {
                code: DiagnosticCode::CachePlanApplied,
                severity: DiagnosticSeverity::Info,
                location: None,
                message:
                    "target cache intent was applied explicitly from a cache-plan recommendation"
                        .to_owned(),
            }],
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppliedCachePlan {
    pub request: ProtocolRequest,
    pub report: CachePreservationReport,
    pub fidelity: Fidelity,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CachePlanApplicationError {
    #[error("the recommended target cache intent is already present on the request")]
    AlreadyApplied,
}

/// Source directive family. This is recorded separately from segment content.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheDirectiveKind {
    OpenAiRequestCacheKey,
    OpenAiRetention,
    AnthropicBreakpoint,
}

/// Caller-owned HMAC key material. It is intentionally non-serializable and
/// does not implement `Debug` so logs cannot accidentally contain it.
#[derive(Clone, Eq, PartialEq)]
pub struct HmacSha256Key(Vec<u8>);

impl HmacSha256Key {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, CorrelationError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(CorrelationError::EmptyKey);
        }
        Ok(Self(bytes))
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Opaque caller-visible correlation output. This display form is hex, but
/// reports never generate it unless the caller supplies a key.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CorrelationId([u8; 32]);

impl CorrelationId {
    fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for CorrelationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CorrelationId")
            .field(&self.to_string())
            .finish()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CorrelationError {
    #[error("HMAC-SHA-256 correlation keys cannot be empty")]
    EmptyKey,
}

/// A future codec-facing result that can include correlation only after an
/// explicit caller request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachePlanCorrelation {
    pub correlation_id: CorrelationId,
}

#[derive(Debug, Error)]
pub enum CachePlanError {
    #[error("cache segment at {location:?} could not be canonicalized: {source}")]
    Canonicalization {
        location: CacheLocation,
        #[source]
        source: serde_json::Error,
    },
}

fn push_segment<T: Serialize>(
    segments: &mut Vec<CacheSegment>,
    kind: CacheSegmentKind,
    location: CacheLocation,
    value: &T,
) -> Result<(), CachePlanError> {
    let canonical_bytes =
        serde_json::to_vec(value).map_err(|source| CachePlanError::Canonicalization {
            location: location.clone(),
            source,
        })?;
    segments.push(CacheSegment {
        descriptor: CacheSegmentDescriptor { kind, location },
        canonical_bytes,
    });
    Ok(())
}

fn is_asset(part: &ContentPart) -> bool {
    matches!(
        part,
        ContentPart::Image { .. } | ContentPart::Document { .. }
    )
}

fn push_cache_directives(
    segments: &mut Vec<CacheSegment>,
    cache_intent: &CacheIntent,
) -> Result<(), CachePlanError> {
    match cache_intent {
        CacheIntent::OpenAi(OpenAiCacheIntent {
            request_cache_key,
            retention,
        }) => {
            let mut directive_index = 0;
            if let Some(request_cache_key) = request_cache_key {
                push_segment(
                    segments,
                    CacheSegmentKind::CacheDirective,
                    CacheLocation::CacheDirective { directive_index },
                    &(CacheDirectiveKind::OpenAiRequestCacheKey, request_cache_key),
                )?;
                directive_index += 1;
            }
            if let Some(retention) = retention {
                push_segment(
                    segments,
                    CacheSegmentKind::CacheDirective,
                    CacheLocation::CacheDirective { directive_index },
                    &(CacheDirectiveKind::OpenAiRetention, retention),
                )?;
            }
        }
        CacheIntent::Anthropic(AnthropicCacheIntent { breakpoints }) => {
            for (directive_index, breakpoint) in breakpoints.iter().enumerate() {
                push_anthropic_breakpoint(segments, directive_index, breakpoint)?;
            }
        }
    }
    Ok(())
}

fn push_anthropic_breakpoint(
    segments: &mut Vec<CacheSegment>,
    directive_index: usize,
    breakpoint: &AnthropicCacheBreakpoint,
) -> Result<(), CachePlanError> {
    push_segment(
        segments,
        CacheSegmentKind::CacheDirective,
        CacheLocation::CacheDirective { directive_index },
        &(CacheDirectiveKind::AnthropicBreakpoint, breakpoint),
    )
}

fn update_length_prefixed(mac: &mut HmacSha256, bytes: &[u8]) {
    mac.update(&(bytes.len() as u64).to_be_bytes());
    mac.update(bytes);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        AssetReference, AssetReferenceType, CacheLocation, ContentPart, ConversationRole,
        JsonSchemaOutputIntent, Message,
    };

    fn request() -> ProtocolRequest {
        ProtocolRequest {
            model: Some("synthetic-model".to_owned()),
            stream: false,
            instructions: vec![ContentPart::Text {
                text: "synthetic instruction".to_owned(),
            }],
            messages: vec![Message {
                role: ConversationRole::User,
                name: None,
                content: vec![
                    ContentPart::Text {
                        text: "synthetic request".to_owned(),
                    },
                    ContentPart::Image {
                        asset: AssetReference {
                            reference_type: AssetReferenceType::Url,
                            value: "https://synthetic.invalid/image.png".to_owned(),
                            media_type: Some("image/png".to_owned()),
                            name: None,
                            size_bytes: None,
                        },
                    },
                ],
                extensions: Vec::new(),
            }],
            tools: Vec::new(),
            generation: Default::default(),
            output_schema: Some(JsonSchemaOutputIntent {
                name: Some("synthetic-output".to_owned()),
                description: None,
                schema: json!({"type": "object"}),
                enforcement: crate::OutputSchemaEnforcement::Required,
            }),
            cache_intent: Some(CacheIntent::OpenAi(OpenAiCacheIntent {
                request_cache_key: Some("synthetic-key".to_owned()),
                retention: Some("short".to_owned()),
            })),
            continuation: None,
            extensions: Vec::new(),
        }
    }

    #[test]
    fn cache_plan_is_ordered_and_content_free_at_its_public_boundary() {
        let plan = CacheSegmentPlan::analyze(&request()).unwrap();
        assert_eq!(
            plan.descriptors().collect::<Vec<_>>(),
            vec![
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::Instruction,
                    location: CacheLocation::Instructions { part_index: 0 }
                },
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::Message,
                    location: CacheLocation::Message { message_index: 0 }
                },
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::ContentPart,
                    location: CacheLocation::MessagePart {
                        message_index: 0,
                        part_index: 0
                    }
                },
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::ContentPart,
                    location: CacheLocation::MessagePart {
                        message_index: 0,
                        part_index: 1
                    }
                },
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::Asset,
                    location: CacheLocation::Asset {
                        message_index: 0,
                        part_index: 1
                    }
                },
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::OutputSchema,
                    location: CacheLocation::OutputSchema
                },
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::CacheDirective,
                    location: CacheLocation::CacheDirective { directive_index: 0 }
                },
                &CacheSegmentDescriptor {
                    kind: CacheSegmentKind::CacheDirective,
                    location: CacheLocation::CacheDirective { directive_index: 1 }
                },
            ]
        );
        assert!(!format!("{plan:?}").contains("synthetic request"));
        assert!(!format!("{plan:?}").contains("synthetic-key"));
    }

    #[test]
    fn hmac_correlation_is_stable_for_one_key_and_isolated_by_key() {
        let plan = CacheSegmentPlan::analyze(&request()).unwrap();
        let key_a = HmacSha256Key::new(b"synthetic-key-a".to_vec()).unwrap();
        let key_b = HmacSha256Key::new(b"synthetic-key-b".to_vec()).unwrap();

        assert_eq!(plan.correlate(&key_a), plan.correlate(&key_a));
        assert_ne!(plan.correlate(&key_a), plan.correlate(&key_b));
        assert!(matches!(
            HmacSha256Key::new(Vec::new()),
            Err(CorrelationError::EmptyKey)
        ));
    }

    #[test]
    fn experimental_diff_reports_prefix_change_and_introduction_without_content() {
        let source = CacheSegmentPlan::analyze(&request()).unwrap();
        let mut changed_request = request();
        changed_request.messages.push(Message {
            role: ConversationRole::Assistant,
            name: None,
            content: vec![ContentPart::Text {
                text: "synthetic response".to_owned(),
            }],
            extensions: Vec::new(),
        });
        let target = CacheSegmentPlan::analyze(&changed_request).unwrap();

        let diff = source.experimental_diff(&target);
        assert_eq!(diff.common_stable_prefix_len, 5);
        assert!(
            diff.target_entries
                .iter()
                .any(|entry| entry.status == CachePreservationStatus::Introduced)
        );
        assert!(!format!("{diff:?}").contains("synthetic response"));
    }

    #[test]
    fn applying_a_recommendation_is_explicit_and_adapted() {
        let source = request();
        let recommendation = CachePlanRecommendation {
            target_intent: CacheIntent::Anthropic(AnthropicCacheIntent {
                breakpoints: vec![AnthropicCacheBreakpoint {
                    location: CacheLocation::Message { message_index: 0 },
                    ttl: Some("5m".to_owned()),
                }],
            }),
            report: CachePreservationReport {
                entries: vec![CachePreservationEntry {
                    source: Some(CacheSegmentDescriptor {
                        kind: CacheSegmentKind::CacheDirective,
                        location: CacheLocation::CacheDirective { directive_index: 0 },
                    }),
                    target: Some(CacheSegmentDescriptor {
                        kind: CacheSegmentKind::CacheDirective,
                        location: CacheLocation::CacheDirective { directive_index: 0 },
                    }),
                    status: CachePreservationStatus::NonPortable,
                    reason: CacheChangeReason::ProviderSemanticsDiffer,
                }],
            },
        };

        assert!(matches!(source.cache_intent, Some(CacheIntent::OpenAi(_))));
        let applied = recommendation.apply(source).unwrap();
        assert!(matches!(
            applied.request.cache_intent,
            Some(CacheIntent::Anthropic(_))
        ));
        assert_eq!(applied.fidelity, Fidelity::Adapted);
        assert_eq!(
            applied.diagnostics[0].code,
            DiagnosticCode::CachePlanApplied
        );
    }

    #[test]
    fn cache_analysis_never_inserts_target_directives_implicitly() {
        let source = request();
        let analyzed = CacheSegmentPlan::analyze(&source).unwrap();

        assert!(matches!(source.cache_intent, Some(CacheIntent::OpenAi(_))));
        assert_eq!(
            analyzed
                .descriptors()
                .filter(|descriptor| descriptor.kind == CacheSegmentKind::CacheDirective)
                .count(),
            2
        );
    }

    #[test]
    fn cross_provider_cache_report_marks_directives_non_portable() {
        let source = CacheSegmentPlan::analyze(&request()).unwrap();
        let mut target_request = request();
        target_request.cache_intent = Some(CacheIntent::Anthropic(AnthropicCacheIntent {
            breakpoints: vec![AnthropicCacheBreakpoint {
                location: CacheLocation::Message { message_index: 0 },
                ttl: Some("5m".to_owned()),
            }],
        }));
        let target = CacheSegmentPlan::analyze(&target_request).unwrap();

        let mut report =
            CachePreservationReport::from_plan_diff(&source.experimental_diff(&target));
        for entry in &mut report.entries {
            if entry
                .source
                .as_ref()
                .is_some_and(|descriptor| descriptor.kind == CacheSegmentKind::CacheDirective)
            {
                entry.status = CachePreservationStatus::NonPortable;
                entry.reason = CacheChangeReason::ProviderSemanticsDiffer;
            }
        }

        assert!(report.entries.iter().any(|entry| {
            entry.status == CachePreservationStatus::NonPortable
                && entry.reason == CacheChangeReason::ProviderSemanticsDiffer
        }));
        assert!(!format!("{report:?}").contains("synthetic-key"));
    }
}
