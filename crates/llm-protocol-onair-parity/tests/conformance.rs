use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use llm_protocol_anthropic as anthropic;
use llm_protocol_core::{
    ANTHROPIC_MESSAGES_PROFILE, CacheIntent, CachePlanRecommendation, CachePreservationReport,
    ConversionResult, DecodedEnvelope, Diagnostic, OPENAI_CHAT_COMPLETIONS_PROFILE,
    OPENAI_RESPONSES_PROFILE, PROTOCOL_VERSION, ProfileId, ProtocolPayload,
};
use llm_protocol_openai as openai;
use serde_json::{Map, Value, json};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CoverageCell {
    source_profile: String,
    target_profile: String,
    payload_class: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FeatureCell {
    domain: String,
    feature: String,
    stage: String,
}

#[test]
fn every_active_vector_executes_and_matches_complete_expectations() {
    run_manifest(false);
}

#[test]
#[ignore = "updates normative synthetic vector expectations"]
fn bless_complete_vector_expectations() {
    assert_eq!(
        std::env::var("LLM_PROTOCOL_BLESS").as_deref(),
        Ok("1"),
        "set LLM_PROTOCOL_BLESS=1 to update normative vector expectations"
    );
    run_manifest(true);
}

fn run_manifest(bless: bool) {
    let vectors_root = protocol_root().join("vectors");
    let manifest_path = vectors_root.join("manifest.json");
    let manifest = read_json(&manifest_path);
    let entries = manifest["vectors"]
        .as_array()
        .expect("vector manifest contains an array");
    let expected_coverage = expected_coverage_cells(&manifest);
    let expected_features = expected_feature_cells(&manifest);
    let mut coverage_claims = BTreeMap::new();
    let mut feature_claims = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut active_count = 0;

    for entry in entries {
        let id = required_string(entry, "id");
        let relative_path = required_string(entry, "path");
        let kind = required_string(entry, "kind");
        assert!(ids.insert(id.to_owned()), "duplicate vector id {id}");
        assert!(
            paths.insert(relative_path.to_owned()),
            "duplicate vector path {relative_path}"
        );

        let path = vectors_root.join(relative_path);
        let mut vector = read_json(&path);
        assert_eq!(vector["id"], id, "manifest id mismatch for {relative_path}");
        assert_eq!(
            vector["kind"], kind,
            "manifest kind mismatch for vector {id}"
        );

        if entry["status"] != "active" {
            continue;
        }
        active_count += 1;
        add_envelope_version(&mut vector);
        let actual = execute_vector(&vector);
        validate_coverage_claims(entry, &vector, &actual, &mut coverage_claims);
        validate_feature_claims(entry, &vector, &actual, &mut feature_claims);
        if bless {
            vector["expect"] = actual;
            write_json(&path, &vector);
        } else {
            assert_eq!(vector["expect"], actual, "vector {id}");
        }
    }

    assert!(active_count > 0, "manifest must contain active vectors");
    let committed_paths = collect_vector_paths(&vectors_root);
    assert_eq!(
        paths, committed_paths,
        "every committed vector document must appear exactly once in the manifest"
    );
    let claimed_coverage = coverage_claims.keys().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        claimed_coverage, expected_coverage,
        "coverage claims must contain every directed profile/payload cell exactly once"
    );
    let claimed_features = feature_claims.keys().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        claimed_features, expected_features,
        "feature claims must contain one source-decode and one cross-profile cell per feature"
    );
}

fn expected_feature_cells(manifest: &Value) -> BTreeSet<FeatureCell> {
    let matrix = &manifest["feature_matrix"];
    assert_eq!(matrix["opaque_policy"], "same_profile_only");
    let content_parts = matrix["content_parts"]
        .as_array()
        .expect("content-part features are an array")
        .iter()
        .map(|feature| feature.as_str().expect("content-part feature is a string"))
        .collect::<BTreeSet<_>>();
    let stream_events = matrix["stream_events"]
        .as_array()
        .expect("stream-event features are an array")
        .iter()
        .map(|feature| feature.as_str().expect("stream-event feature is a string"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        content_parts,
        BTreeSet::from([
            "text",
            "image",
            "document",
            "tool_call",
            "tool_result",
            "reasoning",
            "citation",
            "refusal",
            "opaque",
        ]),
        "feature matrix must contain every typed content-part variant"
    );
    assert_eq!(
        stream_events,
        BTreeSet::from([
            "request_started",
            "message_started",
            "output_part_started",
            "output_part_ended",
            "text_delta",
            "reasoning_delta",
            "refusal_part",
            "citation_part",
            "tool_call_delta",
            "usage",
            "terminal",
            "error",
            "opaque",
        ]),
        "feature matrix must contain every typed stream-event variant"
    );

    let mut cells = BTreeSet::new();
    for (domain, features) in [
        ("content_part", content_parts),
        ("stream_event", stream_events),
    ] {
        for feature in features {
            for stage in ["source_decode", "cross_profile"] {
                cells.insert(FeatureCell {
                    domain: domain.to_owned(),
                    feature: feature.to_owned(),
                    stage: stage.to_owned(),
                });
            }
        }
    }
    assert_eq!(
        cells.len(),
        44,
        "22 typed features and two stages make 44 cells"
    );
    cells
}

fn expected_coverage_cells(manifest: &Value) -> BTreeSet<CoverageCell> {
    let matrix = &manifest["coverage_matrix"];
    assert_eq!(matrix["directions"], "all_distinct_pairs");
    let profiles = matrix["profiles"]
        .as_array()
        .expect("coverage profiles are an array")
        .iter()
        .map(|profile| profile.as_str().expect("coverage profile is a string"))
        .collect::<Vec<_>>();
    let payload_classes = matrix["payload_classes"]
        .as_array()
        .expect("coverage payload classes are an array")
        .iter()
        .map(|class| class.as_str().expect("payload class is a string"))
        .collect::<Vec<_>>();
    assert_eq!(
        profiles.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            OPENAI_CHAT_COMPLETIONS_PROFILE,
            OPENAI_RESPONSES_PROFILE,
            ANTHROPIC_MESSAGES_PROFILE,
        ]),
        "Alpha 0.1.0 coverage matrix must contain all frozen profiles"
    );
    assert_eq!(
        payload_classes.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from(["request", "response", "error", "stream", "cache_report"]),
        "Alpha 0.1.0 coverage matrix must contain every required payload class"
    );

    let mut cells = BTreeSet::new();
    for source_profile in &profiles {
        for target_profile in &profiles {
            if source_profile == target_profile {
                continue;
            }
            for payload_class in &payload_classes {
                cells.insert(CoverageCell {
                    source_profile: (*source_profile).to_owned(),
                    target_profile: (*target_profile).to_owned(),
                    payload_class: (*payload_class).to_owned(),
                });
            }
        }
    }
    assert_eq!(
        cells.len(),
        30,
        "three profiles and five classes make 30 cells"
    );
    cells
}

fn validate_coverage_claims(
    entry: &Value,
    vector: &Value,
    actual: &Value,
    claims: &mut BTreeMap<CoverageCell, String>,
) {
    let Some(coverage) = entry.get("coverage") else {
        return;
    };
    assert_eq!(entry["status"], "active", "only active vectors cover cells");
    assert!(
        matches!(
            required_string(vector, "kind"),
            "cross_profile_conversion" | "unsupported"
        ),
        "matrix cells require a cross-profile conversion vector"
    );
    let source_profile = required_string(vector, "source_profile");
    let target_profile = required_string(vector, "target_profile");
    assert_ne!(source_profile, target_profile);
    let decode_kind = required_string(&actual["decode"]["ir"], "kind");
    let encode = &actual["encode"];

    for claim in coverage.as_array().expect("coverage claims are an array") {
        let payload_class = required_string(claim, "payload_class");
        let support = required_string(claim, "support");
        if payload_class == "cache_report" {
            assert_eq!(decode_kind, "request", "cache reports belong to requests");
            assert!(
                encode["cache_report"].is_object(),
                "cache-report coverage requires a complete report"
            );
        } else {
            assert_eq!(
                payload_class, decode_kind,
                "coverage class must match decoded IR"
            );
        }

        match support {
            "supported" => {
                assert_ne!(encode["fidelity"], "unsupported");
                assert!(encode["envelope"].is_object());
            }
            "unsupported" => {
                assert_eq!(encode["fidelity"], "unsupported");
                assert!(encode.get("envelope").is_none());
            }
            value => panic!("unknown coverage support classification {value}"),
        }

        let cell = CoverageCell {
            source_profile: source_profile.to_owned(),
            target_profile: target_profile.to_owned(),
            payload_class: payload_class.to_owned(),
        };
        let previous = claims.insert(cell.clone(), required_string(vector, "id").to_owned());
        assert!(
            previous.is_none(),
            "coverage cell {cell:?} is claimed by both {} and {}",
            previous.unwrap_or_default(),
            required_string(vector, "id")
        );
    }
}

fn validate_feature_claims(
    entry: &Value,
    vector: &Value,
    actual: &Value,
    claims: &mut BTreeMap<FeatureCell, String>,
) {
    let Some(feature_coverage) = entry.get("feature_coverage") else {
        return;
    };
    assert_eq!(
        entry["status"], "active",
        "only active vectors cover features"
    );
    assert_ne!(
        required_string(vector, "kind"),
        "cache_analysis",
        "feature coverage requires decoded envelope IR"
    );
    let source_ir = &actual["decode"]["ir"];
    let target_ir = actual["encode"]
        .get("round_trip_decode")
        .and_then(|decode| decode.get("ir"));

    for claim in feature_coverage
        .as_array()
        .expect("feature coverage is an array")
    {
        let domain = required_string(claim, "domain");
        let feature = required_string(claim, "feature");
        let stage = required_string(claim, "stage");
        let source_occurrences = feature_occurrences(source_ir, domain, feature);
        assert!(
            !source_occurrences.is_empty(),
            "vector {} claims {domain}.{feature} without decoding it",
            required_string(vector, "id")
        );

        match stage {
            "source_decode" => assert!(
                claim.get("disposition").is_none(),
                "source-decode claims have no cross-profile disposition"
            ),
            "cross_profile" => {
                assert_ne!(
                    vector["source_profile"], vector["target_profile"],
                    "cross-profile feature claims must change profile"
                );
                let disposition = required_string(claim, "disposition");
                let target_occurrences = target_ir
                    .map(|ir| feature_occurrences(ir, domain, feature))
                    .unwrap_or_default();
                validate_feature_disposition(
                    vector,
                    actual,
                    domain,
                    feature,
                    disposition,
                    &source_occurrences,
                    &target_occurrences,
                );
            }
            value => panic!("unknown feature coverage stage {value}"),
        }

        let cell = FeatureCell {
            domain: domain.to_owned(),
            feature: feature.to_owned(),
            stage: stage.to_owned(),
        };
        let vector_id = required_string(vector, "id").to_owned();
        let previous = claims.insert(cell.clone(), vector_id.clone());
        assert!(
            previous.is_none(),
            "feature cell {cell:?} is claimed by both {} and {vector_id}",
            previous.unwrap_or_default()
        );
    }
}

fn validate_feature_disposition(
    vector: &Value,
    actual: &Value,
    domain: &str,
    feature: &str,
    disposition: &str,
    source_occurrences: &[Value],
    target_occurrences: &[Value],
) {
    let encode = &actual["encode"];
    match disposition {
        "preserved" => {
            assert_ne!(encode["fidelity"], "unsupported");
            assert!(encode["envelope"].is_object());
            assert_eq!(
                source_occurrences,
                target_occurrences,
                "vector {} does not preserve the ordered {domain}.{feature} occurrences",
                required_string(vector, "id")
            );
        }
        "adapted" => {
            assert!(matches!(
                encode["fidelity"].as_str(),
                Some("adapted" | "lossy")
            ));
            assert!(encode["envelope"].is_object());
            assert!(!target_occurrences.is_empty());
            assert_ne!(
                source_occurrences,
                target_occurrences,
                "adapted claim for {} must demonstrate changed {domain}.{feature}",
                required_string(vector, "id")
            );
        }
        "lossy" => {
            assert_eq!(encode["fidelity"], "lossy");
            assert!(encode["envelope"].is_object());
            assert_ne!(
                source_occurrences,
                target_occurrences,
                "lossy claim for {} must demonstrate changed or dropped {domain}.{feature}",
                required_string(vector, "id")
            );
        }
        "unsupported" => {
            assert_eq!(encode["fidelity"], "unsupported");
            assert!(encode.get("envelope").is_none());
            assert!(target_occurrences.is_empty());
        }
        "non_portable" => {
            assert_eq!(feature, "opaque", "only opaque features are non-portable");
            assert_eq!(encode["fidelity"], "lossy");
            assert!(encode["envelope"].is_object());
            assert!(
                target_occurrences.is_empty(),
                "opaque material must not cross profile boundaries"
            );
            let diagnostic_codes = encode["diagnostics"]
                .as_array()
                .expect("encode diagnostics are an array")
                .iter()
                .map(|diagnostic| required_string(diagnostic, "code"))
                .collect::<BTreeSet<_>>();
            assert!(
                diagnostic_codes.contains("forward_compatible_unknown")
                    || diagnostic_codes.contains("non_portable_opaque_extension"),
                "non-portable coverage requires an opaque portability diagnostic"
            );
        }
        value => panic!("unknown feature disposition {value}"),
    }
}

fn feature_occurrences(ir: &Value, domain: &str, feature: &str) -> Vec<Value> {
    match domain {
        "content_part" => content_part_occurrences(ir, feature),
        "stream_event" => stream_event_occurrences(ir, feature),
        value => panic!("unknown feature domain {value}"),
    }
}

fn content_part_occurrences(ir: &Value, feature: &str) -> Vec<Value> {
    let mut occurrences = Vec::new();
    match required_string(ir, "kind") {
        "request" => {
            collect_content_parts(&ir["payload"]["instructions"], feature, &mut occurrences);
            if let Some(messages) = ir["payload"]["messages"].as_array() {
                for message in messages {
                    collect_content_parts(&message["content"], feature, &mut occurrences);
                }
            }
        }
        "response" => {
            if let Some(messages) = ir["payload"]["output"].as_array() {
                for message in messages {
                    collect_content_parts(&message["content"], feature, &mut occurrences);
                }
            }
        }
        _ => {}
    }
    occurrences
}

fn collect_content_parts(parts: &Value, feature: &str, occurrences: &mut Vec<Value>) {
    let Some(parts) = parts.as_array() else {
        return;
    };
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some(feature) {
            occurrences.push(part.clone());
        }
        if part.get("type").and_then(Value::as_str) == Some("tool_result") {
            collect_content_parts(&part["content"], feature, occurrences);
        }
    }
}

fn stream_event_occurrences(ir: &Value, feature: &str) -> Vec<Value> {
    if ir.get("kind").and_then(Value::as_str) != Some("stream") {
        return Vec::new();
    }
    ir["payload"]
        .as_array()
        .expect("stream IR payload is an array")
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some(feature))
        .cloned()
        .collect()
}

fn execute_vector(vector: &Value) -> Value {
    match required_string(vector, "kind") {
        "cache_analysis" => execute_cache_plan_application(vector),
        "exact_replay" | "cross_profile_conversion" | "unsupported" => {
            execute_envelope_vector(vector)
        }
        kind => panic!("unsupported vector kind {kind}"),
    }
}

fn execute_envelope_vector(vector: &Value) -> Value {
    let source_profile = profile_id(required_string(vector, "source_profile"));
    let target_profile = profile_id(required_string(vector, "target_profile"));
    let envelope = &vector["input"]["envelope"];
    assert_eq!(
        envelope["profile_id"],
        source_profile.as_str(),
        "source profile must match the input envelope"
    );

    let decoded = decode_envelope(envelope, &source_profile);
    let decoded_value = decoded
        .output
        .as_ref()
        .expect("active envelope vector must decode to typed IR");
    let decode_expectation = json!({
        "fidelity": decoded.fidelity,
        "diagnostics": diagnostic_expectations(&decoded.diagnostics),
        "ir": ir_document(decoded_value.value(), &source_profile),
    });
    let encode_expectation = encode_envelope(decoded_value, &target_profile);
    assert_envelope_vector_invariants(
        vector,
        decoded_value.value(),
        &source_profile,
        &target_profile,
        envelope,
        &encode_expectation,
    );

    json!({
        "decode": decode_expectation,
        "encode": encode_expectation,
    })
}

fn assert_envelope_vector_invariants(
    vector: &Value,
    payload: &ProtocolPayload,
    source_profile: &ProfileId,
    target_profile: &ProfileId,
    input_envelope: &Value,
    encode_expectation: &Value,
) {
    if encode_expectation["envelope"].is_object() {
        assert!(
            encode_expectation["round_trip_decode"].is_object(),
            "every canonical target envelope must be re-decoded"
        );
        assert_eq!(
            encode_expectation["round_trip_decode"]["ir"]["profile_id"],
            target_profile.as_str(),
            "round-trip IR belongs to the target profile"
        );
    }
    match required_string(vector, "kind") {
        "exact_replay" => {
            assert_eq!(
                source_profile, target_profile,
                "exact replay stays in profile"
            );
            assert_eq!(encode_expectation["fidelity"], "exact");
            let output = &encode_expectation["envelope"];
            for field in [
                "protocol_version",
                "profile_id",
                "status",
                "body_kind",
                "protocol_headers",
                "body_base64",
            ] {
                assert_eq!(output[field], input_envelope[field], "exact replay {field}");
            }
        }
        "cross_profile_conversion" => {
            assert_ne!(
                source_profile, target_profile,
                "cross-profile vector must change profile"
            );
            if matches!(payload, ProtocolPayload::Request(_))
                && encode_expectation.get("envelope").is_some()
            {
                assert!(
                    encode_expectation.get("cache_report").is_some(),
                    "converted request must include its complete cache report"
                );
            }
        }
        "unsupported" => {
            assert_eq!(encode_expectation["fidelity"], "unsupported");
            assert!(encode_expectation.get("envelope").is_none());
            assert!(encode_expectation.get("cache_report").is_none());
        }
        kind => panic!("unexpected envelope vector kind {kind}"),
    }
}

fn decode_envelope(
    envelope: &Value,
    source_profile: &ProfileId,
) -> ConversionResult<DecodedEnvelope<ProtocolPayload>> {
    match source_profile.as_str() {
        OPENAI_CHAT_COMPLETIONS_PROFILE | OPENAI_RESPONSES_PROFILE => {
            let wire = openai::wire_envelope_from_json(envelope)
                .unwrap_or_else(|error| panic!("decode OpenAI vector envelope: {error}"));
            openai::decode(wire.retained_wire(), wire.adapter_metadata)
                .unwrap_or_else(|error| panic!("decode OpenAI vector payload: {error}"))
        }
        ANTHROPIC_MESSAGES_PROFILE => {
            let wire = anthropic::wire_envelope_from_json(envelope)
                .unwrap_or_else(|error| panic!("decode Anthropic vector envelope: {error}"));
            anthropic::decode(wire.retained_wire(), wire.adapter_metadata)
                .unwrap_or_else(|error| panic!("decode Anthropic vector payload: {error}"))
        }
        profile => panic!("no source codec for profile {profile}"),
    }
}

fn encode_envelope(
    decoded: &DecodedEnvelope<ProtocolPayload>,
    target_profile: &ProfileId,
) -> Value {
    let mut expectation = match target_profile.as_str() {
        OPENAI_CHAT_COMPLETIONS_PROFILE | OPENAI_RESPONSES_PROFILE => {
            let result = openai::encode_decoded(decoded, target_profile)
                .unwrap_or_else(|error| panic!("encode OpenAI vector payload: {error}"));
            project_encoded_result(
                result,
                |output| openai::wire_envelope_to_json(&output.wire),
                |output| output.cache_report.as_ref().map(cache_report_document),
            )
        }
        ANTHROPIC_MESSAGES_PROFILE => {
            let result = anthropic::encode_decoded(decoded, target_profile)
                .unwrap_or_else(|error| panic!("encode Anthropic vector payload: {error}"));
            project_encoded_result(
                result,
                |output| anthropic::wire_envelope_to_json(&output.wire),
                |output| output.cache_report.as_ref().map(cache_report_document),
            )
        }
        profile => panic!("no target codec for profile {profile}"),
    };
    if let Some(envelope) = expectation.get("envelope").cloned() {
        let round_trip = decode_envelope(&envelope, target_profile);
        let output = round_trip
            .output
            .as_ref()
            .expect("canonical target envelope must decode to typed IR");
        expectation
            .as_object_mut()
            .expect("encode expectation is an object")
            .insert(
                "round_trip_decode".to_owned(),
                json!({
                    "fidelity": round_trip.fidelity,
                    "diagnostics": diagnostic_expectations(&round_trip.diagnostics),
                    "ir": ir_document(output.value(), target_profile),
                }),
            );
    }
    expectation
}

fn project_encoded_result<T>(
    result: ConversionResult<T>,
    envelope: impl Fn(&T) -> Value,
    cache_report: impl Fn(&T) -> Option<Value>,
) -> Value {
    let mut expectation = Map::new();
    expectation.insert(
        "fidelity".to_owned(),
        serde_json::to_value(result.fidelity).expect("fidelity serializes"),
    );
    expectation.insert(
        "diagnostics".to_owned(),
        diagnostic_expectations(&result.diagnostics),
    );
    if let Some(output) = &result.output {
        expectation.insert("envelope".to_owned(), envelope(output));
        if let Some(report) = cache_report(output) {
            expectation.insert("cache_report".to_owned(), report);
        }
    }
    Value::Object(expectation)
}

fn execute_cache_plan_application(vector: &Value) -> Value {
    let operation = &vector["input"]["cache_plan_application"];
    let source_profile = profile_id(required_string(vector, "source_profile"));
    let target_profile = profile_id(required_string(vector, "target_profile"));
    let request_ir = &operation["request_ir"];
    assert_eq!(request_ir["profile_id"], source_profile.as_str());
    let payload = payload_from_ir_document(request_ir);
    let ProtocolPayload::Request(request) = payload else {
        panic!("cache-plan application requires request IR");
    };
    let target_intent: CacheIntent = serde_json::from_value(operation["target_intent"].clone())
        .expect("cache-plan target intent matches the normative IR shape");
    let report: CachePreservationReport = serde_json::from_value(operation["report"].clone())
        .expect("cache-plan report matches the normative report shape");
    let applied = CachePlanRecommendation {
        target_intent,
        report,
    }
    .apply(request)
    .expect("synthetic cache-plan recommendation applies exactly once");

    json!({
        "analysis": {
            "fidelity": applied.fidelity,
            "diagnostics": diagnostic_expectations(&applied.diagnostics),
            "result_ir": ir_document(&ProtocolPayload::Request(applied.request), &target_profile),
            "cache_report": cache_report_document(&applied.report),
        }
    })
}

fn ir_document(payload: &ProtocolPayload, profile: &ProfileId) -> Value {
    let mut document = serde_json::to_value(payload)
        .expect("protocol payload serializes")
        .as_object()
        .expect("protocol payload serializes as an object")
        .clone();
    document.insert(
        "protocol_version".to_owned(),
        Value::String(PROTOCOL_VERSION.to_owned()),
    );
    document.insert(
        "profile_id".to_owned(),
        Value::String(profile.as_str().to_owned()),
    );
    let mut document = Value::Object(document);
    omit_absent_ir_values(&mut document);
    document
}

fn omit_absent_ir_values(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let required_null_summary =
                object.get("type").and_then(Value::as_str) == Some("reasoning");
            object.retain(|key, value| {
                !value.is_null() || (required_null_summary && key == "summary")
            });
            for value in object.values_mut() {
                omit_absent_ir_values(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                omit_absent_ir_values(value);
            }
        }
        _ => {}
    }
}

fn payload_from_ir_document(document: &Value) -> ProtocolPayload {
    serde_json::from_value(json!({
        "kind": document["kind"],
        "payload": document["payload"],
    }))
    .expect("IR document contains a typed protocol payload")
}

fn diagnostic_expectations(diagnostics: &[Diagnostic]) -> Value {
    Value::Array(
        diagnostics
            .iter()
            .map(|diagnostic| {
                json!({
                    "code": diagnostic.code,
                    "severity": diagnostic.severity,
                    "location": diagnostic.location,
                })
            })
            .collect(),
    )
}

fn cache_report_document(report: &CachePreservationReport) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "entries": report.entries,
    })
}

fn add_envelope_version(vector: &mut Value) {
    let Some(envelope) = vector["input"].get_mut("envelope") else {
        return;
    };
    envelope
        .as_object_mut()
        .expect("vector envelope is an object")
        .insert(
            "protocol_version".to_owned(),
            Value::String(PROTOCOL_VERSION.to_owned()),
        );
}

fn collect_vector_paths(vectors_root: &Path) -> BTreeSet<String> {
    fn visit(root: &Path, directory: &Path, paths: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("read vector directory") {
            let path = entry.expect("read vector directory entry").path();
            if path.is_dir() {
                visit(root, &path, paths);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("json")
                && path != root.join("manifest.json")
            {
                paths.push(path);
            }
        }
    }

    let mut paths = Vec::new();
    visit(vectors_root, vectors_root, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            path.strip_prefix(vectors_root)
                .expect("vector path is below vector root")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

fn protocol_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol")
}

fn profile_id(value: &str) -> ProfileId {
    ProfileId::new(value).expect("manifest profile id is valid")
}

fn required_string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string"))
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize vector");
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}
