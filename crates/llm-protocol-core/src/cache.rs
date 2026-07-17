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
        let entries = compare_segments(self, target, CacheDirectiveCompatibility::SameProvider);
        let source_entries = entries
            .iter()
            .filter(|entry| {
                entry.source.as_ref().is_some_and(|descriptor| {
                    !self.segments[..common_stable_prefix_len]
                        .iter()
                        .any(|segment| segment.descriptor == *descriptor)
                })
            })
            .cloned()
            .collect();
        let target_entries = entries
            .into_iter()
            .filter(|entry| entry.source.is_none())
            .collect();

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
    /// Compare the source request with the request obtained from the canonical
    /// target envelope. This stable report covers every source and target
    /// segment exactly once; it does not predict provider cache hits.
    pub fn source_to_target(
        source: &CacheSegmentPlan,
        target: &CacheSegmentPlan,
        directive_compatibility: CacheDirectiveCompatibility,
    ) -> Self {
        Self {
            entries: compare_segments(source, target, directive_compatibility),
        }
    }

    pub fn validate_conservation(
        &self,
        source: &CacheSegmentPlan,
        target: &CacheSegmentPlan,
    ) -> Result<(), CacheReportValidationError> {
        for (entry_index, entry) in self.entries.iter().enumerate() {
            if !entry.has_valid_shape() {
                return Err(CacheReportValidationError::InvalidEntryShape {
                    entry_index,
                    status: entry.status,
                    reason: entry.reason,
                });
            }
            if let Some(descriptor) = &entry.source
                && !source
                    .descriptors()
                    .any(|candidate| candidate == descriptor)
            {
                return Err(CacheReportValidationError::UnknownSource {
                    entry_index,
                    descriptor: descriptor.clone(),
                });
            }
            if let Some(descriptor) = &entry.target
                && !target
                    .descriptors()
                    .any(|candidate| candidate == descriptor)
            {
                return Err(CacheReportValidationError::UnknownTarget {
                    entry_index,
                    descriptor: descriptor.clone(),
                });
            }
        }

        for descriptor in source.descriptors() {
            let occurrences = self
                .entries
                .iter()
                .filter(|entry| entry.source.as_ref() == Some(descriptor))
                .count();
            if occurrences != 1 {
                return Err(CacheReportValidationError::SourceOccurrenceCount {
                    descriptor: descriptor.clone(),
                    occurrences,
                });
            }
        }
        for descriptor in target.descriptors() {
            let occurrences = self
                .entries
                .iter()
                .filter(|entry| entry.target.as_ref() == Some(descriptor))
                .count();
            if occurrences != 1 {
                return Err(CacheReportValidationError::TargetOccurrenceCount {
                    descriptor: descriptor.clone(),
                    occurrences,
                });
            }
        }

        let source_len = source.segments.len();
        if source
            .descriptors()
            .enumerate()
            .any(|(entry_index, expected)| {
                self.entries
                    .get(entry_index)
                    .and_then(|entry| entry.source.as_ref())
                    != Some(expected)
            })
        {
            return Err(CacheReportValidationError::SourceOrder);
        }
        let represented_targets = self.entries[..source_len]
            .iter()
            .filter_map(|entry| entry.target.as_ref())
            .collect::<Vec<_>>();
        let expected_introductions = target
            .descriptors()
            .filter(|descriptor| !represented_targets.contains(descriptor))
            .collect::<Vec<_>>();
        let actual_introductions = self.entries[source_len..]
            .iter()
            .filter_map(|entry| entry.target.as_ref())
            .collect::<Vec<_>>();
        if actual_introductions != expected_introductions {
            return Err(CacheReportValidationError::IntroducedTargetOrder);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CachePreservationEntry {
    pub source: Option<CacheSegmentDescriptor>,
    pub target: Option<CacheSegmentDescriptor>,
    pub status: CachePreservationStatus,
    pub reason: CacheChangeReason,
}

impl CachePreservationEntry {
    fn has_valid_shape(&self) -> bool {
        match self.status {
            CachePreservationStatus::Preserved => {
                self.source.is_some()
                    && self.target.is_some()
                    && self.reason == CacheChangeReason::Unchanged
            }
            CachePreservationStatus::Moved => {
                self.source.is_some()
                    && self.target.is_some()
                    && self.reason == CacheChangeReason::OrderChanged
            }
            CachePreservationStatus::Changed => {
                self.source.is_some()
                    && self.target.is_some()
                    && self.reason == CacheChangeReason::SemanticValueChanged
            }
            CachePreservationStatus::Dropped => {
                self.source.is_some()
                    && self.target.is_none()
                    && self.reason == CacheChangeReason::NoTargetRepresentation
            }
            CachePreservationStatus::Introduced => {
                self.source.is_none()
                    && self.target.is_some()
                    && matches!(
                        self.reason,
                        CacheChangeReason::NoSourceRepresentation
                            | CacheChangeReason::TargetDirectiveMustBeExplicit
                    )
            }
            CachePreservationStatus::NonPortable => {
                self.source.is_some()
                    && matches!(
                        self.reason,
                        CacheChangeReason::ProviderSemanticsDiffer
                            | CacheChangeReason::TargetDirectiveMustBeExplicit
                    )
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
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

/// Whether source and target cache directives belong to the same provider
/// semantics. Cross-provider directives are never matched as equivalents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheDirectiveCompatibility {
    SameProvider,
    CrossProvider,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CacheReportValidationError {
    #[error("cache report entry {entry_index} has an invalid {status:?}/{reason:?} shape")]
    InvalidEntryShape {
        entry_index: usize,
        status: CachePreservationStatus,
        reason: CacheChangeReason,
    },
    #[error("cache report entry {entry_index} references an unknown source segment {descriptor:?}")]
    UnknownSource {
        entry_index: usize,
        descriptor: CacheSegmentDescriptor,
    },
    #[error("cache report entry {entry_index} references an unknown target segment {descriptor:?}")]
    UnknownTarget {
        entry_index: usize,
        descriptor: CacheSegmentDescriptor,
    },
    #[error("source cache segment {descriptor:?} occurs {occurrences} times in the report")]
    SourceOccurrenceCount {
        descriptor: CacheSegmentDescriptor,
        occurrences: usize,
    },
    #[error("target cache segment {descriptor:?} occurs {occurrences} times in the report")]
    TargetOccurrenceCount {
        descriptor: CacheSegmentDescriptor,
        occurrences: usize,
    },
    #[error("cache report source entries do not retain source-plan order")]
    SourceOrder,
    #[error("cache report introduced entries do not retain target-plan order")]
    IntroducedTargetOrder,
}

fn compare_segments(
    source: &CacheSegmentPlan,
    target: &CacheSegmentPlan,
    directive_compatibility: CacheDirectiveCompatibility,
) -> Vec<CachePreservationEntry> {
    let mut entries = Vec::new();
    let mut matched_target_indices = BTreeSet::new();

    for (source_index, source_segment) in source.segments.iter().enumerate() {
        if directive_compatibility == CacheDirectiveCompatibility::CrossProvider
            && source_segment.descriptor.kind == CacheSegmentKind::CacheDirective
        {
            entries.push(CachePreservationEntry {
                source: Some(source_segment.descriptor.clone()),
                target: None,
                status: CachePreservationStatus::NonPortable,
                reason: CacheChangeReason::ProviderSemanticsDiffer,
            });
            continue;
        }

        if let Some(target_segment) = target.segments.get(source_index)
            && target_is_eligible(
                &matched_target_indices,
                source_index,
                target_segment,
                directive_compatibility,
            )
            && target_segment.matches(source_segment)
        {
            matched_target_indices.insert(source_index);
            entries.push(CachePreservationEntry {
                source: Some(source_segment.descriptor.clone()),
                target: Some(target_segment.descriptor.clone()),
                status: CachePreservationStatus::Preserved,
                reason: CacheChangeReason::Unchanged,
            });
            continue;
        }

        if let Some((target_index, target_segment)) = target
            .segments
            .iter()
            .enumerate()
            .filter(|(target_index, target_segment)| {
                target_is_eligible(
                    &matched_target_indices,
                    *target_index,
                    target_segment,
                    directive_compatibility,
                )
            })
            .find(|(_, target_segment)| target_segment.matches(source_segment))
        {
            matched_target_indices.insert(target_index);
            entries.push(CachePreservationEntry {
                source: Some(source_segment.descriptor.clone()),
                target: Some(target_segment.descriptor.clone()),
                status: CachePreservationStatus::Moved,
                reason: CacheChangeReason::OrderChanged,
            });
            continue;
        }

        if let Some((target_index, target_segment)) = target
            .segments
            .iter()
            .enumerate()
            .filter(|(target_index, target_segment)| {
                target_is_eligible(
                    &matched_target_indices,
                    *target_index,
                    target_segment,
                    directive_compatibility,
                )
            })
            .find(|(_, target_segment)| target_segment.descriptor == source_segment.descriptor)
        {
            matched_target_indices.insert(target_index);
            entries.push(CachePreservationEntry {
                source: Some(source_segment.descriptor.clone()),
                target: Some(target_segment.descriptor.clone()),
                status: CachePreservationStatus::Changed,
                reason: CacheChangeReason::SemanticValueChanged,
            });
            continue;
        }

        if let Some((target_index, target_segment)) = target
            .segments
            .iter()
            .enumerate()
            .filter(|(target_index, target_segment)| {
                target_is_eligible(
                    &matched_target_indices,
                    *target_index,
                    target_segment,
                    directive_compatibility,
                )
            })
            .find(|(_, target_segment)| {
                target_segment.canonical_bytes == source_segment.canonical_bytes
            })
        {
            matched_target_indices.insert(target_index);
            entries.push(CachePreservationEntry {
                source: Some(source_segment.descriptor.clone()),
                target: Some(target_segment.descriptor.clone()),
                status: CachePreservationStatus::Moved,
                reason: CacheChangeReason::OrderChanged,
            });
            continue;
        }

        entries.push(CachePreservationEntry {
            source: Some(source_segment.descriptor.clone()),
            target: None,
            status: CachePreservationStatus::Dropped,
            reason: CacheChangeReason::NoTargetRepresentation,
        });
    }

    entries.extend(
        target
            .segments
            .iter()
            .enumerate()
            .filter(|(target_index, _)| !matched_target_indices.contains(target_index))
            .map(|(_, target_segment)| CachePreservationEntry {
                source: None,
                target: Some(target_segment.descriptor.clone()),
                status: CachePreservationStatus::Introduced,
                reason: CacheChangeReason::NoSourceRepresentation,
            }),
    );
    entries
}

fn target_is_eligible(
    matched_target_indices: &BTreeSet<usize>,
    target_index: usize,
    target_segment: &CacheSegment,
    directive_compatibility: CacheDirectiveCompatibility,
) -> bool {
    !(matched_target_indices.contains(&target_index)
        || directive_compatibility == CacheDirectiveCompatibility::CrossProvider
            && target_segment.descriptor.kind == CacheSegmentKind::CacheDirective)
}

/// A target-specific cache-directive recommendation. Converting a request does
/// not apply this automatically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachePlanRecommendation {
    target_intent: CacheIntent,
    report: CachePreservationReport,
    source_plan: CacheSegmentPlan,
}

impl CachePlanRecommendation {
    pub fn for_request(
        request: &ProtocolRequest,
        target_intent: CacheIntent,
    ) -> Result<Self, CachePlanError> {
        let source_plan = CacheSegmentPlan::analyze(request)?;
        let target_request = ProtocolRequest {
            cache_intent: Some(target_intent.clone()),
            ..request.clone()
        };
        let target_plan = CacheSegmentPlan::analyze(&target_request)?;
        let compatibility =
            cache_intent_compatibility(request.cache_intent.as_ref(), &target_intent);
        let mut report =
            CachePreservationReport::source_to_target(&source_plan, &target_plan, compatibility);
        for entry in &mut report.entries {
            if entry.status == CachePreservationStatus::Introduced
                && entry
                    .target
                    .as_ref()
                    .is_some_and(|target| target.kind == CacheSegmentKind::CacheDirective)
            {
                entry.reason = CacheChangeReason::TargetDirectiveMustBeExplicit;
            }
        }
        debug_assert_eq!(
            report.validate_conservation(&source_plan, &target_plan),
            Ok(())
        );
        Ok(Self {
            target_intent,
            report,
            source_plan,
        })
    }

    pub fn target_intent(&self) -> &CacheIntent {
        &self.target_intent
    }

    pub fn report(&self) -> &CachePreservationReport {
        &self.report
    }

    pub fn apply(
        &self,
        request: ProtocolRequest,
    ) -> Result<AppliedCachePlan, CachePlanApplicationError> {
        if request.cache_intent.as_ref() == Some(&self.target_intent) {
            return Err(CachePlanApplicationError::AlreadyApplied);
        }
        let source_plan = CacheSegmentPlan::analyze(&request)
            .map_err(|_| CachePlanApplicationError::AnalysisFailed)?;
        if source_plan != self.source_plan {
            return Err(CachePlanApplicationError::SourceRequestMismatch);
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

fn cache_intent_compatibility(
    source: Option<&CacheIntent>,
    target: &CacheIntent,
) -> CacheDirectiveCompatibility {
    match (source, target) {
        (Some(CacheIntent::OpenAi(_)), CacheIntent::Anthropic(_))
        | (Some(CacheIntent::Anthropic(_)), CacheIntent::OpenAi(_)) => {
            CacheDirectiveCompatibility::CrossProvider
        }
        _ => CacheDirectiveCompatibility::SameProvider,
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
    #[error("the request cache plan could not be analyzed")]
    AnalysisFailed,
    #[error("the recommendation was created for a different source request")]
    SourceRequestMismatch,
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
        assert_eq!(
            diff.source_entries
                .iter()
                .filter_map(|entry| entry.source.as_ref())
                .collect::<Vec<_>>(),
            source
                .descriptors()
                .skip(diff.common_stable_prefix_len)
                .collect::<Vec<_>>()
        );
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
        let source_plan = CacheSegmentPlan::analyze(&source).unwrap();
        let recommendation = CachePlanRecommendation::for_request(
            &source,
            CacheIntent::Anthropic(AnthropicCacheIntent {
                breakpoints: vec![AnthropicCacheBreakpoint {
                    location: CacheLocation::Message { message_index: 0 },
                    ttl: Some("5m".to_owned()),
                }],
            }),
        )
        .unwrap();

        assert!(matches!(source.cache_intent, Some(CacheIntent::OpenAi(_))));
        assert_eq!(
            recommendation
                .report()
                .entries
                .iter()
                .filter(|entry| entry.status == CachePreservationStatus::NonPortable)
                .count(),
            2
        );
        assert!(recommendation.report().entries.iter().any(|entry| {
            entry.status == CachePreservationStatus::Introduced
                && entry.reason == CacheChangeReason::TargetDirectiveMustBeExplicit
        }));
        assert!(!format!("{recommendation:?}").contains("synthetic-key"));
        assert!(!format!("{recommendation:?}").contains("synthetic request"));
        let mut different_source = source.clone();
        different_source.messages[0].content[0] = ContentPart::Text {
            text: "different synthetic request".to_owned(),
        };
        assert!(matches!(
            recommendation.apply(different_source),
            Err(CachePlanApplicationError::SourceRequestMismatch)
        ));
        let applied = recommendation.apply(source).unwrap();
        let target_plan = CacheSegmentPlan::analyze(&applied.request).unwrap();
        assert_eq!(
            applied
                .report
                .validate_conservation(&source_plan, &target_plan),
            Ok(())
        );
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
        target_request.messages[0].content[0] = ContentPart::Text {
            text: "synthetic changed request".to_owned(),
        };
        target_request.messages[0].content.truncate(1);
        target_request.tools.push(crate::ToolDefinition {
            name: "synthetic_tool".to_owned(),
            description: None,
            input_schema: json!({"type": "object"}),
            strict: None,
            extensions: Vec::new(),
        });
        target_request.cache_intent = None;
        let target = CacheSegmentPlan::analyze(&target_request).unwrap();

        let report = CachePreservationReport::source_to_target(
            &source,
            &target,
            CacheDirectiveCompatibility::CrossProvider,
        );

        assert_eq!(report.validate_conservation(&source, &target), Ok(()));
        assert_eq!(
            report
                .entries
                .iter()
                .map(|entry| entry.status)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                CachePreservationStatus::Preserved,
                CachePreservationStatus::Moved,
                CachePreservationStatus::Changed,
                CachePreservationStatus::Dropped,
                CachePreservationStatus::Introduced,
                CachePreservationStatus::NonPortable,
            ])
        );
        assert!(!format!("{report:?}").contains("synthetic-key"));
    }

    #[test]
    fn cache_target_eligibility_excludes_matches_and_cross_provider_directives() {
        let plan = CacheSegmentPlan::analyze(&request()).unwrap();
        let (normal_index, normal) = plan
            .segments()
            .iter()
            .enumerate()
            .find(|(_, segment)| segment.descriptor.kind == CacheSegmentKind::Message)
            .unwrap();
        let (directive_index, directive) = plan
            .segments()
            .iter()
            .enumerate()
            .find(|(_, segment)| segment.descriptor.kind == CacheSegmentKind::CacheDirective)
            .unwrap();
        let unmatched = BTreeSet::new();

        assert!(target_is_eligible(
            &unmatched,
            normal_index,
            normal,
            CacheDirectiveCompatibility::SameProvider,
        ));
        assert!(target_is_eligible(
            &unmatched,
            normal_index,
            normal,
            CacheDirectiveCompatibility::CrossProvider,
        ));
        assert!(target_is_eligible(
            &unmatched,
            directive_index,
            directive,
            CacheDirectiveCompatibility::SameProvider,
        ));
        assert!(!target_is_eligible(
            &unmatched,
            directive_index,
            directive,
            CacheDirectiveCompatibility::CrossProvider,
        ));
        assert!(!target_is_eligible(
            &BTreeSet::from([normal_index]),
            normal_index,
            normal,
            CacheDirectiveCompatibility::SameProvider,
        ));
    }

    #[test]
    fn cache_report_conservation_rejects_duplicate_and_invalid_entries() {
        let plan = CacheSegmentPlan::analyze(&request()).unwrap();
        let mut report = CachePreservationReport::source_to_target(
            &plan,
            &plan,
            CacheDirectiveCompatibility::SameProvider,
        );
        report.entries.push(report.entries[0].clone());
        assert!(matches!(
            report.validate_conservation(&plan, &plan),
            Err(CacheReportValidationError::SourceOccurrenceCount { occurrences: 2, .. })
        ));

        let mut report = CachePreservationReport::source_to_target(
            &plan,
            &plan,
            CacheDirectiveCompatibility::SameProvider,
        );
        report.entries[0].reason = CacheChangeReason::NoSourceRepresentation;
        assert!(matches!(
            report.validate_conservation(&plan, &plan),
            Err(CacheReportValidationError::InvalidEntryShape { entry_index: 0, .. })
        ));

        let mut report = CachePreservationReport::source_to_target(
            &plan,
            &plan,
            CacheDirectiveCompatibility::SameProvider,
        );
        report.entries.swap(0, 1);
        assert_eq!(
            report.validate_conservation(&plan, &plan),
            Err(CacheReportValidationError::SourceOrder)
        );
    }

    #[test]
    fn cache_report_conservation_rejects_unknown_descriptors() {
        let mut single_segment_request = request();
        single_segment_request.messages.clear();
        single_segment_request.output_schema = None;
        single_segment_request.cache_intent = None;
        let plan = CacheSegmentPlan::analyze(&single_segment_request).unwrap();
        assert_eq!(plan.segments().len(), 1);

        let unknown = CacheSegmentDescriptor {
            kind: CacheSegmentKind::Message,
            location: CacheLocation::Message { message_index: 42 },
        };
        let report = CachePreservationReport {
            entries: vec![CachePreservationEntry {
                source: Some(unknown.clone()),
                target: None,
                status: CachePreservationStatus::Dropped,
                reason: CacheChangeReason::NoTargetRepresentation,
            }],
        };
        assert!(matches!(
            report.validate_conservation(&plan, &plan),
            Err(CacheReportValidationError::UnknownSource {
                entry_index: 0,
                descriptor,
            }) if descriptor == unknown
        ));

        let report = CachePreservationReport {
            entries: vec![CachePreservationEntry {
                source: None,
                target: Some(unknown.clone()),
                status: CachePreservationStatus::Introduced,
                reason: CacheChangeReason::NoSourceRepresentation,
            }],
        };
        assert!(matches!(
            report.validate_conservation(&plan, &plan),
            Err(CacheReportValidationError::UnknownTarget {
                entry_index: 0,
                descriptor,
            }) if descriptor == unknown
        ));
    }

    #[test]
    fn cache_report_entry_shapes_require_all_status_fields() {
        let descriptor = CacheSegmentDescriptor {
            kind: CacheSegmentKind::Message,
            location: CacheLocation::Message { message_index: 0 },
        };
        let entry = |source: bool,
                     target: bool,
                     status: CachePreservationStatus,
                     reason: CacheChangeReason| CachePreservationEntry {
            source: source.then(|| descriptor.clone()),
            target: target.then(|| descriptor.clone()),
            status,
            reason,
        };

        for (valid, invalid_entries) in [
            (
                entry(
                    true,
                    true,
                    CachePreservationStatus::Moved,
                    CacheChangeReason::OrderChanged,
                ),
                vec![
                    entry(
                        false,
                        true,
                        CachePreservationStatus::Moved,
                        CacheChangeReason::OrderChanged,
                    ),
                    entry(
                        true,
                        false,
                        CachePreservationStatus::Moved,
                        CacheChangeReason::OrderChanged,
                    ),
                    entry(
                        true,
                        true,
                        CachePreservationStatus::Moved,
                        CacheChangeReason::Unchanged,
                    ),
                ],
            ),
            (
                entry(
                    true,
                    true,
                    CachePreservationStatus::Changed,
                    CacheChangeReason::SemanticValueChanged,
                ),
                vec![
                    entry(
                        false,
                        true,
                        CachePreservationStatus::Changed,
                        CacheChangeReason::SemanticValueChanged,
                    ),
                    entry(
                        true,
                        false,
                        CachePreservationStatus::Changed,
                        CacheChangeReason::SemanticValueChanged,
                    ),
                    entry(
                        true,
                        true,
                        CachePreservationStatus::Changed,
                        CacheChangeReason::Unchanged,
                    ),
                ],
            ),
            (
                entry(
                    true,
                    false,
                    CachePreservationStatus::Dropped,
                    CacheChangeReason::NoTargetRepresentation,
                ),
                vec![
                    entry(
                        false,
                        false,
                        CachePreservationStatus::Dropped,
                        CacheChangeReason::NoTargetRepresentation,
                    ),
                    entry(
                        true,
                        true,
                        CachePreservationStatus::Dropped,
                        CacheChangeReason::NoTargetRepresentation,
                    ),
                    entry(
                        true,
                        false,
                        CachePreservationStatus::Dropped,
                        CacheChangeReason::Unchanged,
                    ),
                ],
            ),
            (
                entry(
                    false,
                    true,
                    CachePreservationStatus::Introduced,
                    CacheChangeReason::NoSourceRepresentation,
                ),
                vec![
                    entry(
                        true,
                        true,
                        CachePreservationStatus::Introduced,
                        CacheChangeReason::NoSourceRepresentation,
                    ),
                    entry(
                        false,
                        false,
                        CachePreservationStatus::Introduced,
                        CacheChangeReason::NoSourceRepresentation,
                    ),
                    entry(
                        false,
                        true,
                        CachePreservationStatus::Introduced,
                        CacheChangeReason::Unchanged,
                    ),
                ],
            ),
            (
                entry(
                    true,
                    false,
                    CachePreservationStatus::NonPortable,
                    CacheChangeReason::ProviderSemanticsDiffer,
                ),
                vec![
                    entry(
                        false,
                        false,
                        CachePreservationStatus::NonPortable,
                        CacheChangeReason::ProviderSemanticsDiffer,
                    ),
                    entry(
                        true,
                        false,
                        CachePreservationStatus::NonPortable,
                        CacheChangeReason::Unchanged,
                    ),
                ],
            ),
        ] {
            assert!(valid.has_valid_shape(), "expected valid entry: {valid:?}");
            for invalid in invalid_entries {
                assert!(
                    !invalid.has_valid_shape(),
                    "expected invalid entry: {invalid:?}"
                );
            }
        }
    }
}
