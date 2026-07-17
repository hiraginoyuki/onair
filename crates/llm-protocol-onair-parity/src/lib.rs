//! Test-only selected parity checks and a local benchmark harness for LLM
//! Protocol Alpha `0.1.0`.
//!
//! This crate is deliberately not a dependency of the OnAir proxy. Its parity
//! tests compare selected, synthetic compatibility rewrites through public
//! `onair-core` entry points. Its benchmark command is dry-run by default and
//! retains only redacted local observations in explicitly local output paths.
//! Parity assertions use independent test-only target-wire projections rather
//! than alpha target decoders. They normalize only the representation choices
//! named by the selected subset, such as instruction placement and generated
//! identifiers.

use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Instant,
};

use llm_protocol_anthropic as anthropic;
use llm_protocol_core::{
    AdapterMetadata, CanonicalEnvelope, ContentPart, ConversationRole, GenerationControls, Message,
    OPENAI_CHAT_COMPLETIONS_PROFILE, OPENAI_RESPONSES_PROFILE, ProfileId, ProtocolBodyKind,
    ProtocolPayload, ProtocolRequest, ToolDefinition,
};
use llm_protocol_openai as openai;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const BENCHMARK_PROTOCOL_VERSION: &str = "0.1.0";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ScenarioManifest {
    pub protocol_version: String,
    pub synthetic: bool,
    pub mode: String,
    pub scenarios: Vec<BenchmarkScenario>,
    pub live_mode_requirements: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct BenchmarkScenario {
    pub id: String,
    pub description: String,
    pub max_calls: u64,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct LiveBenchmarkConfig {
    pub hard_caps: BenchmarkCaps,
    pub profiles: Vec<LiveProfileConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct BenchmarkCaps {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct LiveProfileConfig {
    pub profile_id: String,
    pub endpoint: String,
    pub credential_header: String,
    pub credential_env: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BenchmarkRunOptions {
    pub live: bool,
    pub confirmed: bool,
    pub config_path: Option<PathBuf>,
    pub output_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedBenchmarkRun {
    pub mode: BenchmarkMode,
    pub output_path: Option<PathBuf>,
    pub config: Option<LiveBenchmarkConfig>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BenchmarkMode {
    DryRun,
    Live,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BenchmarkReport {
    pub protocol_version: String,
    pub mode: String,
    pub synthetic: bool,
    pub observations: Vec<BenchmarkObservation>,
    pub note: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BenchmarkObservation {
    pub scenario_id: String,
    pub profile_id: String,
    pub outcome: BenchmarkObservationOutcome,
    pub status: Option<u16>,
    pub latency_ms: u128,
    pub cache_tokens: CacheTokenObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkObservationOutcome {
    NotRun,
    ResponseReceived,
    RequestFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CacheTokenObservation {
    pub state: CacheObservationState,
    pub read_tokens: Option<u64>,
    pub write_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheObservationState {
    Observed,
    Inconclusive,
}

#[derive(Debug, Error)]
pub enum BenchmarkError {
    #[error("failed to read benchmark manifest {path}: {source}")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse benchmark manifest {path}: {source}")]
    ParseManifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to read local benchmark config {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse local benchmark config {path}: {source}")]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "benchmark manifest must be synthetic and use protocol version {BENCHMARK_PROTOCOL_VERSION}"
    )]
    InvalidManifest,
    #[error("live benchmarks require both --live and --confirm-live")]
    LiveConfirmationRequired,
    #[error("live benchmarks require --config PATH")]
    LiveConfigRequired,
    #[error("benchmark config within the repository must be inside .local/: {0}")]
    ConfigMustBeLocal(PathBuf),
    #[error("benchmark output must be inside the repository .local/ directory: {0}")]
    OutputMustBeLocal(PathBuf),
    #[error("benchmark output path may not traverse a symbolic link: {0}")]
    OutputPathContainsSymlink(PathBuf),
    #[error("configured hard caps must be nonzero")]
    MissingHardCaps,
    #[error("configured {kind} cap {configured} is lower than required total {required}")]
    CapBelowTotal {
        kind: &'static str,
        configured: u64,
        required: u64,
    },
    #[error("live benchmark profile {0} is not a frozen Alpha profile")]
    UnsupportedProfile(String),
    #[error(
        "live benchmark endpoint must be an absolute HTTPS URL without credentials, query, or fragment for profile {0}"
    )]
    InvalidEndpoint(String),
    #[error("credential environment variable {0} is not set")]
    MissingCredential(String),
    #[error("invalid configured HTTP header name for profile {0}")]
    InvalidCredentialHeader(String),
    #[error("benchmark request construction failed: {0}")]
    RequestConstruction(String),
    #[error("live benchmark execution requires a prepared live configuration")]
    InvalidLivePreparation,
    #[error("failed to create local benchmark output directory {path}: {source}")]
    CreateOutputDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write local benchmark output {path}: {source}")]
    WriteOutput {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize redacted benchmark report: {0}")]
    SerializeReport(#[source] serde_json::Error),
}

pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("protocol parity crate is nested under crates/")
        .to_path_buf()
}

pub fn default_manifest_path(repo_root: &Path) -> PathBuf {
    repo_root.join("protocol/benchmarks/scenarios.json")
}

pub fn default_output_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".local/llm-protocol-alpha/benchmarks/observations.json")
}

pub fn read_manifest(path: &Path) -> Result<ScenarioManifest, BenchmarkError> {
    let bytes = fs::read(path).map_err(|source| BenchmarkError::ReadManifest {
        path: path.to_path_buf(),
        source,
    })?;
    let manifest =
        serde_json::from_slice(&bytes).map_err(|source| BenchmarkError::ParseManifest {
            path: path.to_path_buf(),
            source,
        })?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub fn prepare_benchmark_run(
    repo_root: &Path,
    manifest: &ScenarioManifest,
    options: &BenchmarkRunOptions,
) -> Result<PreparedBenchmarkRun, BenchmarkError> {
    validate_manifest(manifest)?;
    if !options.live {
        return Ok(PreparedBenchmarkRun {
            mode: BenchmarkMode::DryRun,
            output_path: None,
            config: None,
        });
    }
    if !options.confirmed {
        return Err(BenchmarkError::LiveConfirmationRequired);
    }
    let config_path = options
        .config_path
        .as_deref()
        .ok_or(BenchmarkError::LiveConfigRequired)?;
    if !is_local_config_path(repo_root, config_path) {
        return Err(BenchmarkError::ConfigMustBeLocal(config_path.to_path_buf()));
    }
    let bytes = fs::read(config_path).map_err(|source| BenchmarkError::ReadConfig {
        path: config_path.to_path_buf(),
        source,
    })?;
    let config = serde_json::from_slice(&bytes).map_err(|source| BenchmarkError::ParseConfig {
        path: config_path.to_path_buf(),
        source,
    })?;
    validate_live_config(manifest, &config)?;
    let output_path = options
        .output_path
        .clone()
        .unwrap_or_else(|| default_output_path(repo_root));
    if !is_local_only_path(repo_root, &output_path) {
        return Err(BenchmarkError::OutputMustBeLocal(output_path));
    }
    ensure_output_path_has_no_symlinks(repo_root, &output_path)?;
    Ok(PreparedBenchmarkRun {
        mode: BenchmarkMode::Live,
        output_path: Some(output_path),
        config: Some(config),
    })
}

pub fn dry_run_report(manifest: &ScenarioManifest) -> BenchmarkReport {
    BenchmarkReport {
        protocol_version: manifest.protocol_version.clone(),
        mode: "dry_run".to_owned(),
        synthetic: true,
        observations: manifest
            .scenarios
            .iter()
            .map(|scenario| BenchmarkObservation {
                scenario_id: scenario.id.clone(),
                profile_id: "not_contacted".to_owned(),
                outcome: BenchmarkObservationOutcome::NotRun,
                status: None,
                latency_ms: 0,
                cache_tokens: inconclusive_cache_tokens(),
            })
            .collect(),
        note: "Dry run only. No provider request was made; cache results are not portability evidence."
            .to_owned(),
    }
}

pub async fn run_live_benchmark(
    manifest: &ScenarioManifest,
    prepared: &PreparedBenchmarkRun,
) -> Result<BenchmarkReport, BenchmarkError> {
    let transport = ReqwestBenchmarkTransport {
        client: reqwest::Client::new(),
    };
    run_live_benchmark_with_transport(manifest, prepared, &transport, |name| {
        std::env::var(name).ok()
    })
    .await
}

#[derive(Clone, Debug)]
struct BenchmarkTransportResponse {
    status: u16,
    body: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct BenchmarkTransportError;

trait BenchmarkTransport {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<BenchmarkTransportResponse, BenchmarkTransportError>;
}

struct ReqwestBenchmarkTransport {
    client: reqwest::Client,
}

impl BenchmarkTransport for ReqwestBenchmarkTransport {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<BenchmarkTransportResponse, BenchmarkTransportError> {
        let response = self
            .client
            .execute(request)
            .await
            .map_err(|_| BenchmarkTransportError)?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|_| BenchmarkTransportError)?
            .to_vec();
        Ok(BenchmarkTransportResponse { status, body })
    }
}

async fn run_live_benchmark_with_transport<T, F>(
    manifest: &ScenarioManifest,
    prepared: &PreparedBenchmarkRun,
    transport: &T,
    credential_for: F,
) -> Result<BenchmarkReport, BenchmarkError>
where
    T: BenchmarkTransport,
    F: Fn(&str) -> Option<String>,
{
    if prepared.mode != BenchmarkMode::Live {
        return Err(BenchmarkError::InvalidLivePreparation);
    }
    let config = prepared
        .config
        .as_ref()
        .ok_or(BenchmarkError::InvalidLivePreparation)?;
    validate_manifest(manifest)?;
    validate_live_config(manifest, config)?;

    let mut observations = Vec::new();
    for scenario in &manifest.scenarios {
        for profile in &config.profiles {
            let request = synthetic_request_for_profile(&profile.profile_id, scenario)?;
            let credential = credential_for(&profile.credential_env)
                .ok_or_else(|| BenchmarkError::MissingCredential(profile.credential_env.clone()))?;
            let request = benchmark_http_request(profile, request, &credential)?;
            let started = Instant::now();
            let response = transport.execute(request).await;
            let latency_ms = started.elapsed().as_millis();
            match response {
                Ok(response) => observations.push(BenchmarkObservation {
                    scenario_id: scenario.id.clone(),
                    profile_id: profile.profile_id.clone(),
                    outcome: BenchmarkObservationOutcome::ResponseReceived,
                    status: Some(response.status),
                    latency_ms,
                    cache_tokens: redacted_cache_tokens(&response.body),
                }),
                Err(_) => observations.push(BenchmarkObservation {
                    scenario_id: scenario.id.clone(),
                    profile_id: profile.profile_id.clone(),
                    outcome: BenchmarkObservationOutcome::RequestFailed,
                    status: None,
                    latency_ms,
                    cache_tokens: inconclusive_cache_tokens(),
                }),
            }
        }
    }
    Ok(BenchmarkReport {
        protocol_version: manifest.protocol_version.clone(),
        mode: "live".to_owned(),
        synthetic: true,
        observations,
        note: "Request failures are retained without response bodies. Cache token values are observed provider fields or inconclusive; they are not portability evidence."
            .to_owned(),
    })
}

fn benchmark_http_request(
    profile: &LiveProfileConfig,
    request: SyntheticWireRequest,
    credential: &str,
) -> Result<reqwest::Request, BenchmarkError> {
    let endpoint = reqwest::Url::parse(&profile.endpoint)
        .map_err(|_| BenchmarkError::InvalidEndpoint(profile.profile_id.clone()))?;
    let credential_name =
        reqwest::header::HeaderName::from_bytes(profile.credential_header.as_bytes())
            .map_err(|_| BenchmarkError::InvalidCredentialHeader(profile.profile_id.clone()))?;
    let credential_value = reqwest::header::HeaderValue::from_str(credential)
        .map_err(|_| BenchmarkError::InvalidCredentialHeader(profile.profile_id.clone()))?;
    let mut http_request = reqwest::Request::new(reqwest::Method::POST, endpoint);
    http_request
        .headers_mut()
        .insert(credential_name, credential_value);
    for header in request.headers {
        let name = reqwest::header::HeaderName::from_bytes(header.name().as_bytes())
            .map_err(|error| BenchmarkError::RequestConstruction(error.to_string()))?;
        let value = reqwest::header::HeaderValue::from_str(header.value())
            .map_err(|error| BenchmarkError::RequestConstruction(error.to_string()))?;
        http_request.headers_mut().append(name, value);
    }
    *http_request.body_mut() = Some(request.body.into());
    Ok(http_request)
}

pub fn write_local_report(
    repo_root: &Path,
    path: &Path,
    report: &BenchmarkReport,
) -> Result<(), BenchmarkError> {
    if !is_local_only_path(repo_root, path) {
        return Err(BenchmarkError::OutputMustBeLocal(path.to_path_buf()));
    }
    ensure_output_path_has_no_symlinks(repo_root, path)?;
    let parent = path
        .parent()
        .expect("a benchmark output path always has a parent");
    fs::create_dir_all(parent).map_err(|source| BenchmarkError::CreateOutputDirectory {
        path: parent.to_path_buf(),
        source,
    })?;
    let bytes = serde_json::to_vec_pretty(report).map_err(BenchmarkError::SerializeReport)?;
    fs::write(path, bytes).map_err(|source| BenchmarkError::WriteOutput {
        path: path.to_path_buf(),
        source,
    })
}

fn validate_manifest(manifest: &ScenarioManifest) -> Result<(), BenchmarkError> {
    if manifest.protocol_version != BENCHMARK_PROTOCOL_VERSION
        || !manifest.synthetic
        || manifest.mode != "dry_run_default"
        || manifest.scenarios.is_empty()
        || manifest.scenarios.iter().any(|scenario| {
            scenario.id.is_empty()
                || scenario.max_calls == 0
                || scenario.max_input_tokens == 0
                || scenario.max_output_tokens == 0
        })
    {
        return Err(BenchmarkError::InvalidManifest);
    }
    Ok(())
}

fn validate_live_config(
    manifest: &ScenarioManifest,
    config: &LiveBenchmarkConfig,
) -> Result<(), BenchmarkError> {
    if config.hard_caps.calls == 0
        || config.hard_caps.input_tokens == 0
        || config.hard_caps.output_tokens == 0
        || config.profiles.is_empty()
    {
        return Err(BenchmarkError::MissingHardCaps);
    }
    for profile in &config.profiles {
        match profile.profile_id.as_str() {
            OPENAI_CHAT_COMPLETIONS_PROFILE
            | OPENAI_RESPONSES_PROFILE
            | llm_protocol_core::ANTHROPIC_MESSAGES_PROFILE => {}
            _ => {
                return Err(BenchmarkError::UnsupportedProfile(
                    profile.profile_id.clone(),
                ));
            }
        }
        if profile.endpoint.is_empty()
            || profile.credential_header.is_empty()
            || profile.credential_env.is_empty()
        {
            return Err(BenchmarkError::MissingHardCaps);
        }
        reqwest::header::HeaderName::from_bytes(profile.credential_header.as_bytes())
            .map_err(|_| BenchmarkError::InvalidCredentialHeader(profile.profile_id.clone()))?;
        let endpoint = reqwest::Url::parse(&profile.endpoint)
            .map_err(|_| BenchmarkError::InvalidEndpoint(profile.profile_id.clone()))?;
        if endpoint.scheme() != "https"
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(BenchmarkError::InvalidEndpoint(profile.profile_id.clone()));
        }
    }
    let profile_count = u64::try_from(config.profiles.len()).expect("profile count fits in u64");
    let required_calls = required_total(manifest, profile_count, |scenario| scenario.max_calls)?;
    let required_input_tokens = required_total(manifest, profile_count, |scenario| {
        scenario.max_input_tokens
    })?;
    let required_output_tokens = required_total(manifest, profile_count, |scenario| {
        scenario.max_output_tokens
    })?;
    validate_cap("calls", config.hard_caps.calls, required_calls)?;
    validate_cap(
        "input_tokens",
        config.hard_caps.input_tokens,
        required_input_tokens,
    )?;
    validate_cap(
        "output_tokens",
        config.hard_caps.output_tokens,
        required_output_tokens,
    )?;
    Ok(())
}

fn required_total(
    manifest: &ScenarioManifest,
    profile_count: u64,
    value: impl Fn(&BenchmarkScenario) -> u64,
) -> Result<u64, BenchmarkError> {
    let per_profile = manifest
        .scenarios
        .iter()
        .try_fold(0_u64, |total, scenario| total.checked_add(value(scenario)))
        .ok_or(BenchmarkError::MissingHardCaps)?;
    per_profile
        .checked_mul(profile_count)
        .ok_or(BenchmarkError::MissingHardCaps)
}

fn validate_cap(kind: &'static str, configured: u64, required: u64) -> Result<(), BenchmarkError> {
    if configured < required {
        return Err(BenchmarkError::CapBelowTotal {
            kind,
            configured,
            required,
        });
    }
    Ok(())
}

fn is_local_only_path(repo_root: &Path, path: &Path) -> bool {
    let path = absolute_normalized(path);
    let local = absolute_normalized(&repo_root.join(".local"));
    path.starts_with(local)
}

fn is_local_config_path(repo_root: &Path, path: &Path) -> bool {
    let path = absolute_normalized(path);
    let repo_root = absolute_normalized(repo_root);
    !path.starts_with(&repo_root) || is_local_only_path(&repo_root, &path)
}

fn ensure_output_path_has_no_symlinks(
    repo_root: &Path,
    output_path: &Path,
) -> Result<(), BenchmarkError> {
    let output_path = absolute_normalized(output_path);
    let local_root = absolute_normalized(&repo_root.join(".local"));
    let relative = output_path
        .strip_prefix(&local_root)
        .expect("local-only output paths are checked before symlink validation");
    let mut current = local_root;
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(BenchmarkError::OutputPathContainsSymlink(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(BenchmarkError::CreateOutputDirectory {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn absolute_normalized(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .expect("the current directory is available")
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

struct SyntheticWireRequest {
    headers: Vec<llm_protocol_core::ProtocolHeaderLine>,
    body: Vec<u8>,
}

fn synthetic_wire_request(
    protocol_headers: Vec<llm_protocol_core::ProtocolHeaderLine>,
    body: Vec<u8>,
) -> SyntheticWireRequest {
    SyntheticWireRequest {
        headers: protocol_headers,
        body,
    }
}

fn synthetic_request_for_profile(
    profile_id: &str,
    scenario: &BenchmarkScenario,
) -> Result<SyntheticWireRequest, BenchmarkError> {
    let profile_id = ProfileId::new(profile_id)
        .map_err(|error| BenchmarkError::RequestConstruction(error.to_string()))?;
    let payload = ProtocolPayload::Request(ProtocolRequest {
        model: Some("synthetic-benchmark-model".to_owned()),
        stream: false,
        instructions: vec![ContentPart::Text {
            text: "Synthetic benchmark instruction.".to_owned(),
        }],
        messages: vec![Message {
            role: ConversationRole::User,
            name: None,
            content: vec![ContentPart::Text {
                text: "Synthetic benchmark request.".to_owned(),
            }],
            extensions: Vec::new(),
        }],
        tools: vec![ToolDefinition {
            name: "synthetic_lookup".to_owned(),
            description: Some("Synthetic benchmark tool.".to_owned()),
            input_schema: json!({"type": "object"}),
            strict: Some(true),
            extensions: Vec::new(),
        }],
        generation: GenerationControls {
            max_output_tokens: Some(scenario.max_output_tokens),
            ..GenerationControls::default()
        },
        output_schema: None,
        cache_intent: None,
        continuation: None,
        extensions: Vec::new(),
    });
    let canonical = CanonicalEnvelope {
        value: payload,
        profile_id: profile_id.clone(),
        status: 200,
        body_kind: ProtocolBodyKind::Json,
        adapter_metadata: AdapterMetadata::default(),
    };
    match profile_id.as_str() {
        OPENAI_CHAT_COMPLETIONS_PROFILE | OPENAI_RESPONSES_PROFILE => {
            let wire = openai::encode_canonical(canonical, &profile_id)
                .map_err(|error| BenchmarkError::RequestConstruction(error.to_string()))?
                .output
                .ok_or_else(|| {
                    BenchmarkError::RequestConstruction(
                        "OpenAI codec declined the synthetic request".to_owned(),
                    )
                })?
                .wire;
            Ok(synthetic_wire_request(wire.protocol_headers, wire.body))
        }
        llm_protocol_core::ANTHROPIC_MESSAGES_PROFILE => {
            let wire = anthropic::encode_canonical(canonical, &profile_id)
                .map_err(|error| BenchmarkError::RequestConstruction(error.to_string()))?
                .output
                .ok_or_else(|| {
                    BenchmarkError::RequestConstruction(
                        "Anthropic codec declined the synthetic request".to_owned(),
                    )
                })?
                .wire;
            Ok(synthetic_wire_request(wire.protocol_headers, wire.body))
        }
        _ => Err(BenchmarkError::UnsupportedProfile(profile_id.to_string())),
    }
}

fn redacted_cache_tokens(body: &[u8]) -> CacheTokenObservation {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return inconclusive_cache_tokens();
    };
    let usage = value.get("usage").and_then(Value::as_object);
    let read_tokens = usage
        .and_then(|usage| usage.get("cache_read_input_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            usage
                .and_then(|usage| usage.get("input_tokens_details"))
                .and_then(Value::as_object)
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            usage
                .and_then(|usage| usage.get("prompt_tokens_details"))
                .and_then(Value::as_object)
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
        });
    let write_tokens = usage
        .and_then(|usage| usage.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64);
    CacheTokenObservation {
        state: if read_tokens.is_some() || write_tokens.is_some() {
            CacheObservationState::Observed
        } else {
            CacheObservationState::Inconclusive
        },
        read_tokens,
        write_tokens,
    }
}

fn inconclusive_cache_tokens() -> CacheTokenObservation {
    CacheTokenObservation {
        state: CacheObservationState::Inconclusive,
        read_tokens: None,
        write_tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use llm_protocol_core::ANTHROPIC_MESSAGES_PROFILE;

    fn unique_test_root() -> PathBuf {
        static NEXT_TEST_ROOT: AtomicUsize = AtomicUsize::new(0);
        let sequence = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "llm-protocol-alpha-parity-{}-{sequence}",
            std::process::id()
        ))
    }

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        method: reqwest::Method,
        url: String,
        headers: reqwest::header::HeaderMap,
        body: Vec<u8>,
    }

    struct RecordingTransport {
        requests: Mutex<Vec<CapturedRequest>>,
        responses: Mutex<VecDeque<Result<BenchmarkTransportResponse, BenchmarkTransportError>>>,
    }

    impl RecordingTransport {
        fn new(
            responses: impl IntoIterator<
                Item = Result<BenchmarkTransportResponse, BenchmarkTransportError>,
            >,
        ) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
            }
        }

        fn requests(&self) -> Vec<CapturedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl BenchmarkTransport for RecordingTransport {
        async fn execute(
            &self,
            request: reqwest::Request,
        ) -> Result<BenchmarkTransportResponse, BenchmarkTransportError> {
            let captured = CapturedRequest {
                method: request.method().clone(),
                url: request.url().to_string(),
                headers: request.headers().clone(),
                body: request
                    .body()
                    .and_then(reqwest::Body::as_bytes)
                    .expect("synthetic benchmark requests use in-memory bodies")
                    .to_vec(),
            };
            self.requests.lock().unwrap().push(captured);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(BenchmarkTransportError))
        }
    }

    fn successful_response(status: u16, body: Value) -> BenchmarkTransportResponse {
        BenchmarkTransportResponse {
            status,
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    fn all_profile_config() -> LiveBenchmarkConfig {
        LiveBenchmarkConfig {
            hard_caps: BenchmarkCaps {
                calls: 3,
                input_tokens: 768,
                output_tokens: 384,
            },
            profiles: vec![
                LiveProfileConfig {
                    profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                    endpoint: "https://chat.example.invalid/v1/chat/completions".to_owned(),
                    credential_header: "authorization".to_owned(),
                    credential_env: "SYNTHETIC_OPENAI_KEY".to_owned(),
                },
                LiveProfileConfig {
                    profile_id: OPENAI_RESPONSES_PROFILE.to_owned(),
                    endpoint: "https://responses.example.invalid/v1/responses".to_owned(),
                    credential_header: "authorization".to_owned(),
                    credential_env: "SYNTHETIC_OPENAI_KEY".to_owned(),
                },
                LiveProfileConfig {
                    profile_id: ANTHROPIC_MESSAGES_PROFILE.to_owned(),
                    endpoint: "https://messages.example.invalid/v1/messages".to_owned(),
                    credential_header: "x-api-key".to_owned(),
                    credential_env: "SYNTHETIC_ANTHROPIC_KEY".to_owned(),
                },
            ],
        }
    }

    fn prepared_live(config: LiveBenchmarkConfig) -> PreparedBenchmarkRun {
        PreparedBenchmarkRun {
            mode: BenchmarkMode::Live,
            output_path: Some(PathBuf::from(
                "/synthetic-repository/.local/observations.json",
            )),
            config: Some(config),
        }
    }

    fn synthetic_credential(name: &str) -> Option<String> {
        match name {
            "SYNTHETIC_OPENAI_KEY" => Some("Bearer synthetic-openai-key".to_owned()),
            "SYNTHETIC_ANTHROPIC_KEY" => Some("synthetic-anthropic-key".to_owned()),
            _ => None,
        }
    }

    fn test_manifest() -> ScenarioManifest {
        ScenarioManifest {
            protocol_version: BENCHMARK_PROTOCOL_VERSION.to_owned(),
            synthetic: true,
            mode: "dry_run_default".to_owned(),
            scenarios: vec![BenchmarkScenario {
                id: "synthetic".to_owned(),
                description: "Synthetic scenario.".to_owned(),
                max_calls: 1,
                max_input_tokens: 256,
                max_output_tokens: 128,
            }],
            live_mode_requirements: Vec::new(),
        }
    }

    #[test]
    fn benchmark_defaults_to_dry_run_without_network_configuration() {
        let prepared = prepare_benchmark_run(
            Path::new("/synthetic-repository"),
            &test_manifest(),
            &BenchmarkRunOptions {
                live: false,
                confirmed: false,
                config_path: None,
                output_path: None,
            },
        )
        .unwrap();
        assert_eq!(prepared.mode, BenchmarkMode::DryRun);
        let report = dry_run_report(&test_manifest());
        assert_eq!(report.mode, "dry_run");
        assert_eq!(report.observations.len(), 1);
        assert_eq!(
            report.observations[0].outcome,
            BenchmarkObservationOutcome::NotRun
        );
        assert_eq!(
            report.observations[0].cache_tokens.state,
            CacheObservationState::Inconclusive
        );
    }

    #[test]
    fn live_benchmark_requires_explicit_confirmation() {
        let error = prepare_benchmark_run(
            Path::new("/synthetic-repository"),
            &test_manifest(),
            &BenchmarkRunOptions {
                live: true,
                confirmed: false,
                config_path: Some(PathBuf::from("/tmp/config.json")),
                output_path: None,
            },
        )
        .unwrap_err();
        assert!(matches!(error, BenchmarkError::LiveConfirmationRequired));
    }

    #[test]
    fn live_benchmark_requires_caps_that_cover_the_scenario() {
        let manifest = test_manifest();
        let config = LiveBenchmarkConfig {
            hard_caps: BenchmarkCaps {
                calls: 1,
                input_tokens: 255,
                output_tokens: 128,
            },
            profiles: vec![LiveProfileConfig {
                profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                endpoint: "https://example.invalid/v1/chat/completions".to_owned(),
                credential_header: "authorization".to_owned(),
                credential_env: "SYNTHETIC_BENCHMARK_KEY".to_owned(),
            }],
        };
        let error = validate_live_config(&manifest, &config).unwrap_err();
        assert!(matches!(
            error,
            BenchmarkError::CapBelowTotal {
                kind: "input_tokens",
                configured: 255,
                required: 256,
            }
        ));
    }

    #[test]
    fn benchmark_manifest_requires_nonzero_scenario_budgets() {
        for field in ["max_calls", "max_input_tokens", "max_output_tokens"] {
            let mut manifest = test_manifest();
            match field {
                "max_calls" => manifest.scenarios[0].max_calls = 0,
                "max_input_tokens" => manifest.scenarios[0].max_input_tokens = 0,
                "max_output_tokens" => manifest.scenarios[0].max_output_tokens = 0,
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    validate_manifest(&manifest),
                    Err(BenchmarkError::InvalidManifest)
                ),
                "accepted zero {field}"
            );
        }
    }

    #[test]
    fn benchmark_config_may_be_external_but_output_must_be_local_only() {
        let root = Path::new("/synthetic-repository");
        assert!(is_local_only_path(
            root,
            Path::new("/synthetic-repository/.local/config.json")
        ));
        assert!(!is_local_only_path(
            root,
            Path::new("/synthetic-repository/protocol/config.json")
        ));
        assert!(is_local_config_path(root, Path::new("/tmp/config.json")));
        assert!(!is_local_only_path(
            root,
            Path::new("/tmp/observations.json")
        ));
    }

    #[test]
    fn preparation_rejects_output_outside_local_directory() {
        let test_root = unique_test_root();
        let root = test_root.join("repository");
        let config_path = test_root.join("config.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &config_path,
            serde_json::to_vec(&LiveBenchmarkConfig {
                hard_caps: BenchmarkCaps {
                    calls: 1,
                    input_tokens: 256,
                    output_tokens: 128,
                },
                profiles: vec![LiveProfileConfig {
                    profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                    endpoint: "https://example.invalid/v1/chat/completions".to_owned(),
                    credential_header: "authorization".to_owned(),
                    credential_env: "SYNTHETIC_BENCHMARK_KEY".to_owned(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let error = prepare_benchmark_run(
            &root,
            &test_manifest(),
            &BenchmarkRunOptions {
                live: true,
                confirmed: true,
                config_path: Some(config_path),
                output_path: Some(root.join("observations.json")),
            },
        )
        .unwrap_err();
        fs::remove_dir_all(&test_root).unwrap();
        assert!(matches!(error, BenchmarkError::OutputMustBeLocal(_)));
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_output_through_local_symlink() {
        use std::os::unix::fs::symlink;

        let test_root = unique_test_root();
        let root = test_root.join("repository");
        let local = root.join(".local");
        let external = test_root.join("external");
        let config_path = test_root.join("config.json");
        fs::create_dir_all(&local).unwrap();
        fs::create_dir_all(&external).unwrap();
        symlink(&external, local.join("redirect")).unwrap();
        fs::write(
            &config_path,
            serde_json::to_vec(&LiveBenchmarkConfig {
                hard_caps: BenchmarkCaps {
                    calls: 1,
                    input_tokens: 256,
                    output_tokens: 128,
                },
                profiles: vec![LiveProfileConfig {
                    profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                    endpoint: "https://example.invalid/v1/chat/completions".to_owned(),
                    credential_header: "authorization".to_owned(),
                    credential_env: "SYNTHETIC_BENCHMARK_KEY".to_owned(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let error = prepare_benchmark_run(
            &root,
            &test_manifest(),
            &BenchmarkRunOptions {
                live: true,
                confirmed: true,
                config_path: Some(config_path),
                output_path: Some(local.join("redirect/observations.json")),
            },
        )
        .unwrap_err();
        fs::remove_dir_all(&test_root).unwrap();
        assert!(matches!(
            error,
            BenchmarkError::OutputPathContainsSymlink(_)
        ));
    }

    #[test]
    fn benchmark_observations_retain_only_cache_token_totals() {
        let raw = br#"{
            "id": "synthetic-response-id",
            "output_text": "synthetic private text",
            "usage": {
                "input_tokens_details": {"cached_tokens": 5},
                "cache_creation_input_tokens": 2
            }
        }"#;
        let observation = redacted_cache_tokens(raw);
        assert_eq!(observation.state, CacheObservationState::Observed);
        assert_eq!(observation.read_tokens, Some(5));
        assert_eq!(observation.write_tokens, Some(2));
        let report = BenchmarkReport {
            protocol_version: BENCHMARK_PROTOCOL_VERSION.to_owned(),
            mode: "live".to_owned(),
            synthetic: true,
            observations: vec![BenchmarkObservation {
                scenario_id: "synthetic".to_owned(),
                profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                outcome: BenchmarkObservationOutcome::ResponseReceived,
                status: Some(200),
                latency_ms: 1,
                cache_tokens: observation,
            }],
            note: "observed or inconclusive only".to_owned(),
        };
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("synthetic-response-id"));
        assert!(!serialized.contains("synthetic private text"));
    }

    #[tokio::test]
    async fn injected_transport_covers_all_profiles_and_redacts_responses() {
        let transport = RecordingTransport::new([
            Ok(successful_response(
                200,
                json!({
                    "id": "chat-sensitive-response",
                    "choices": [{"message": {"content": "chat-sensitive-output"}}],
                    "usage": {"prompt_tokens_details": {"cached_tokens": 3}}
                }),
            )),
            Ok(successful_response(
                201,
                json!({
                    "id": "responses-sensitive-response",
                    "output_text": "responses-sensitive-output",
                    "usage": {"input_tokens_details": {"cached_tokens": 5}}
                }),
            )),
            Ok(successful_response(
                202,
                json!({
                    "id": "messages-sensitive-response",
                    "content": [{"type": "text", "text": "messages-sensitive-output"}],
                    "usage": {
                        "cache_read_input_tokens": 7,
                        "cache_creation_input_tokens": 2
                    }
                }),
            )),
        ]);
        let report = run_live_benchmark_with_transport(
            &test_manifest(),
            &prepared_live(all_profile_config()),
            &transport,
            synthetic_credential,
        )
        .await
        .unwrap();

        assert_eq!(report.mode, "live");
        assert_eq!(report.observations.len(), 3);
        assert_eq!(
            report
                .observations
                .iter()
                .map(|observation| (
                    observation.profile_id.as_str(),
                    observation.outcome,
                    observation.status,
                    observation.cache_tokens.read_tokens,
                    observation.cache_tokens.write_tokens,
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    OPENAI_CHAT_COMPLETIONS_PROFILE,
                    BenchmarkObservationOutcome::ResponseReceived,
                    Some(200),
                    Some(3),
                    None,
                ),
                (
                    OPENAI_RESPONSES_PROFILE,
                    BenchmarkObservationOutcome::ResponseReceived,
                    Some(201),
                    Some(5),
                    None,
                ),
                (
                    ANTHROPIC_MESSAGES_PROFILE,
                    BenchmarkObservationOutcome::ResponseReceived,
                    Some(202),
                    Some(7),
                    Some(2),
                ),
            ]
        );

        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(request.method, reqwest::Method::POST);
            assert_eq!(
                request.headers.get("content-type").unwrap(),
                "application/json"
            );
        }
        assert_eq!(
            requests[0].url,
            "https://chat.example.invalid/v1/chat/completions"
        );
        assert_eq!(
            requests[0].headers.get("authorization").unwrap(),
            "Bearer synthetic-openai-key"
        );
        let chat: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(chat["model"], "synthetic-benchmark-model");
        assert_eq!(chat["max_completion_tokens"], 128);
        assert!(chat["messages"].is_array());

        assert_eq!(
            requests[1].url,
            "https://responses.example.invalid/v1/responses"
        );
        assert_eq!(
            requests[1].headers.get("authorization").unwrap(),
            "Bearer synthetic-openai-key"
        );
        let responses: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(responses["model"], "synthetic-benchmark-model");
        assert_eq!(responses["max_output_tokens"], 128);
        assert!(responses["input"].is_array());

        assert_eq!(
            requests[2].url,
            "https://messages.example.invalid/v1/messages"
        );
        assert_eq!(
            requests[2].headers.get("x-api-key").unwrap(),
            "synthetic-anthropic-key"
        );
        assert_eq!(
            requests[2].headers.get("anthropic-version").unwrap(),
            "2023-06-01"
        );
        let messages: Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(messages["model"], "synthetic-benchmark-model");
        assert_eq!(messages["max_tokens"], 128);
        assert!(messages["messages"].is_array());

        let serialized = serde_json::to_string(&report).unwrap();
        for forbidden in [
            "sensitive-response",
            "sensitive-output",
            "synthetic-openai-key",
            "synthetic-anthropic-key",
            "example.invalid",
            "Synthetic benchmark request.",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "redacted report contained {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn live_execution_revalidates_hard_caps_before_transport() {
        for kind in ["calls", "input_tokens", "output_tokens"] {
            let mut config = all_profile_config();
            let (configured, required) = match kind {
                "calls" => {
                    config.hard_caps.calls = 2;
                    (2, 3)
                }
                "input_tokens" => {
                    config.hard_caps.input_tokens = 767;
                    (767, 768)
                }
                "output_tokens" => {
                    config.hard_caps.output_tokens = 383;
                    (383, 384)
                }
                _ => unreachable!(),
            };
            let transport = RecordingTransport::new([]);
            let error = run_live_benchmark_with_transport(
                &test_manifest(),
                &prepared_live(config),
                &transport,
                |_| panic!("credential lookup must follow cap validation"),
            )
            .await
            .unwrap_err();

            assert!(matches!(
                error,
                BenchmarkError::CapBelowTotal {
                    kind: actual_kind,
                    configured: actual_configured,
                    required: actual_required,
                } if actual_kind == kind
                    && actual_configured == configured
                    && actual_required == required
            ));
            assert!(transport.requests().is_empty());
        }
    }

    #[tokio::test]
    async fn live_execution_accounts_for_transport_failure_and_continues() {
        let mut config = all_profile_config();
        config.hard_caps = BenchmarkCaps {
            calls: 2,
            input_tokens: 512,
            output_tokens: 256,
        };
        config.profiles.truncate(2);
        let transport = RecordingTransport::new([
            Err(BenchmarkTransportError),
            Ok(successful_response(
                200,
                json!({"usage": {"input_tokens_details": {"cached_tokens": 5}}}),
            )),
        ]);
        let report = run_live_benchmark_with_transport(
            &test_manifest(),
            &prepared_live(config),
            &transport,
            synthetic_credential,
        )
        .await
        .unwrap();

        assert_eq!(transport.requests().len(), 2);
        assert_eq!(report.observations.len(), 2);
        assert_eq!(
            report.observations[0],
            BenchmarkObservation {
                scenario_id: "synthetic".to_owned(),
                profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                outcome: BenchmarkObservationOutcome::RequestFailed,
                status: None,
                latency_ms: report.observations[0].latency_ms,
                cache_tokens: inconclusive_cache_tokens(),
            }
        );
        assert_eq!(
            report.observations[1].outcome,
            BenchmarkObservationOutcome::ResponseReceived
        );
        assert_eq!(report.observations[1].status, Some(200));
        assert_eq!(report.observations[1].cache_tokens.read_tokens, Some(5));
    }

    #[tokio::test]
    async fn live_execution_rejects_missing_or_invalid_credentials_without_transport() {
        let mut config = all_profile_config();
        config.hard_caps = BenchmarkCaps {
            calls: 1,
            input_tokens: 256,
            output_tokens: 128,
        };
        config.profiles.truncate(1);

        let missing_transport = RecordingTransport::new([]);
        let missing = run_live_benchmark_with_transport(
            &test_manifest(),
            &prepared_live(config.clone()),
            &missing_transport,
            |_| None,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            missing,
            BenchmarkError::MissingCredential(name) if name == "SYNTHETIC_OPENAI_KEY"
        ));
        assert!(missing_transport.requests().is_empty());

        let invalid_transport = RecordingTransport::new([]);
        let invalid = run_live_benchmark_with_transport(
            &test_manifest(),
            &prepared_live(config),
            &invalid_transport,
            |_| Some("synthetic\ninvalid".to_owned()),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            invalid,
            BenchmarkError::InvalidCredentialHeader(profile)
                if profile == OPENAI_CHAT_COMPLETIONS_PROFILE
        ));
        assert!(invalid_transport.requests().is_empty());
    }

    #[test]
    fn report_writer_requires_a_local_only_destination() {
        let root = unique_test_root().join("repository");
        let report = dry_run_report(&test_manifest());
        let error =
            write_local_report(&root, Path::new("/tmp/observations.json"), &report).unwrap_err();
        assert!(matches!(error, BenchmarkError::OutputMustBeLocal(_)));
    }

    #[test]
    fn live_config_rejects_endpoint_credentials_and_query_material() {
        let config = LiveBenchmarkConfig {
            hard_caps: BenchmarkCaps {
                calls: 1,
                input_tokens: 256,
                output_tokens: 128,
            },
            profiles: vec![LiveProfileConfig {
                profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                endpoint: "https://token@example.invalid/v1/chat/completions?debug=true".to_owned(),
                credential_header: "authorization".to_owned(),
                credential_env: "SYNTHETIC_BENCHMARK_KEY".to_owned(),
            }],
        };
        let error = validate_live_config(&test_manifest(), &config).unwrap_err();
        assert!(matches!(error, BenchmarkError::InvalidEndpoint(_)));
    }

    #[test]
    fn live_config_rejects_invalid_https_endpoint() {
        let config = LiveBenchmarkConfig {
            hard_caps: BenchmarkCaps {
                calls: 1,
                input_tokens: 256,
                output_tokens: 128,
            },
            profiles: vec![LiveProfileConfig {
                profile_id: OPENAI_CHAT_COMPLETIONS_PROFILE.to_owned(),
                endpoint: "https://".to_owned(),
                credential_header: "authorization".to_owned(),
                credential_env: "SYNTHETIC_BENCHMARK_KEY".to_owned(),
            }],
        };
        let error = validate_live_config(&test_manifest(), &config).unwrap_err();
        assert!(matches!(error, BenchmarkError::InvalidEndpoint(_)));
    }

    #[test]
    fn live_config_rejects_invalid_credential_header_name() {
        let mut config = all_profile_config();
        config.profiles.truncate(1);
        config.hard_caps = BenchmarkCaps {
            calls: 1,
            input_tokens: 256,
            output_tokens: 128,
        };
        config.profiles[0].credential_header = "invalid header".to_owned();

        let error = validate_live_config(&test_manifest(), &config).unwrap_err();
        assert!(matches!(
            error,
            BenchmarkError::InvalidCredentialHeader(profile)
                if profile == OPENAI_CHAT_COMPLETIONS_PROFILE
        ));
    }

    #[test]
    fn synthetic_request_supports_all_frozen_profiles() {
        let scenario = &test_manifest().scenarios[0];
        for profile_id in [
            OPENAI_CHAT_COMPLETIONS_PROFILE,
            OPENAI_RESPONSES_PROFILE,
            ANTHROPIC_MESSAGES_PROFILE,
        ] {
            let request = synthetic_request_for_profile(profile_id, scenario).unwrap();
            assert!(!request.body.is_empty());
            assert!(
                request
                    .headers
                    .iter()
                    .any(|header| header.name().eq_ignore_ascii_case("content-type"))
            );
        }
    }
}
