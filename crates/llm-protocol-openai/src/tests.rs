use std::path::Path;

use serde_json::{Value, json};

use super::*;
use llm_protocol_core::CachePreservationStatus;

fn protocol_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol")
}

fn read_vector(relative_path: &str) -> Value {
    let bytes = std::fs::read(protocol_root().join("vectors").join(relative_path))
        .unwrap_or_else(|error| panic!("read vector {relative_path}: {error}"));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parse vector {relative_path}: {error}"))
}

fn profile_id(value: &Value) -> ProfileId {
    ProfileId::new(value.as_str().expect("profile identifier string")).unwrap()
}

fn decode_vector(vector: &Value) -> DecodedEnvelope<OpenAiPayload> {
    let wire = wire_envelope_from_json(&vector["input"]["envelope"]).unwrap();
    let decoded = decode(wire.retained_wire(), wire.adapter_metadata).unwrap();
    assert_eq!(decoded.fidelity, Fidelity::Exact);
    decoded
        .output
        .expect("active vector has a typed source payload")
}

fn diagnostic_codes(result: &ConversionResult<EncodedEnvelope>) -> Vec<Value> {
    result
        .diagnostics
        .iter()
        .map(|diagnostic| serde_json::to_value(diagnostic.code).unwrap())
        .collect()
}

#[test]
fn profile_identifiers_are_frozen() {
    assert_eq!(
        OpenAiProfile::ChatCompletions.profile_id().as_str(),
        CHAT_COMPLETIONS_PROFILE
    );
    assert_eq!(
        OpenAiProfile::Responses.profile_id().as_str(),
        RESPONSES_PROFILE
    );
}

#[test]
fn active_openai_vectors_run_through_the_common_ir_boundary() {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(protocol_root().join("vectors/manifest.json")).unwrap(),
    )
    .unwrap();

    for entry in manifest["vectors"].as_array().unwrap() {
        let vector_id = entry["id"].as_str().unwrap();
        if !vector_id.starts_with("openai.") || entry["status"] != "active" {
            continue;
        }
        let path = entry["path"].as_str().unwrap();
        let vector = read_vector(path);
        if !matches!(
            vector["target_profile"].as_str(),
            Some(CHAT_COMPLETIONS_PROFILE | RESPONSES_PROFILE)
        ) {
            continue;
        }
        let decoded = decode_vector(&vector);
        let target_profile = profile_id(&vector["target_profile"]);
        let encoded = encode_decoded(&decoded, &target_profile).unwrap();

        assert_eq!(
            serde_json::to_value(encoded.fidelity).unwrap(),
            vector["expect"]["fidelity"],
            "vector {vector_id}"
        );
        assert_eq!(
            diagnostic_codes(&encoded),
            vector["expect"]["diagnostic_codes"]
                .as_array()
                .unwrap()
                .to_vec(),
            "vector {vector_id}"
        );
        if let Some(expect_replay) = vector["expect"].get("exact_replay") {
            let output = encoded.output.as_ref().expect("replay output");
            assert_eq!(
                output
                    .wire
                    .protocol_headers
                    .iter()
                    .map(|header| header.raw_line.clone())
                    .collect::<Vec<_>>(),
                expect_replay["protocol_headers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
                "vector {vector_id}"
            );
            assert_eq!(
                encode_base64(&output.wire.body),
                expect_replay["body_base64"].as_str().unwrap(),
                "vector {vector_id}"
            );
        }
    }
}

#[test]
fn chat_and_responses_request_vectors_canonically_cross_encode() {
    let chat = read_vector("openai/chat.request.text-tool.json");
    let chat_decoded = decode_vector(&chat);
    let chat_encoded = encode_decoded(&chat_decoded, &profile_id(&chat["target_profile"])).unwrap();
    let chat_body: Value = serde_json::from_slice(
        &chat_encoded
            .output
            .as_ref()
            .expect("encoded request")
            .wire
            .body,
    )
    .expect("Responses JSON");
    assert_eq!(chat_body["input"][0]["role"], "user");
    assert_eq!(chat_body["input"][1]["type"], "function_call");
    assert_eq!(chat_body["input"][2]["type"], "function_call_output");
    assert_eq!(chat_body["tools"][0]["name"], "synthetic_lookup");
    assert_eq!(chat_body["max_output_tokens"], 24);
    assert_eq!(chat_body["text"]["format"]["type"], "json_schema");
    assert_eq!(chat_body["prompt_cache_key"], "synthetic-cache");
    assert_eq!(chat_body["prompt_cache_retention"], "24h");
    let chat_report = chat_encoded
        .output
        .as_ref()
        .expect("encoded request")
        .cache_report
        .as_ref()
        .expect("cross-profile request cache report");
    assert!(
        chat_report
            .entries
            .iter()
            .all(|entry| entry.status == CachePreservationStatus::Preserved)
    );
    assert!(!format!("{chat_report:?}").contains("synthetic request"));
    assert!(!format!("{chat_report:?}").contains("synthetic-cache"));

    let responses = read_vector("openai/responses.request.text-tool.json");
    let responses_decoded = decode_vector(&responses);
    let responses_encoded = encode_decoded(
        &responses_decoded,
        &profile_id(&responses["target_profile"]),
    )
    .unwrap();
    let responses_body: Value =
        serde_json::from_slice(&responses_encoded.output.unwrap().wire.body).expect("Chat JSON");
    assert_eq!(responses_body["messages"][0]["role"], "user");
    assert_eq!(
        responses_body["messages"][1]["tool_calls"][0]["id"],
        "call_synthetic"
    );
    assert_eq!(responses_body["messages"][2]["role"], "tool");
    assert_eq!(
        responses_body["tools"][0]["function"]["name"],
        "synthetic_lookup"
    );
    assert_eq!(responses_body["max_completion_tokens"], 24);
    assert_eq!(responses_body["response_format"]["type"], "json_schema");
}

#[test]
fn buffered_responses_and_errors_preserve_the_typed_subset() {
    let chat_response = read_vector("openai/chat.response.text-tool.json");
    let chat_decoded = decode_vector(&chat_response);
    let chat_encoded =
        encode_decoded(&chat_decoded, &profile_id(&chat_response["target_profile"])).unwrap();
    let chat_body: Value = serde_json::from_slice(&chat_encoded.output.unwrap().wire.body).unwrap();
    assert_eq!(chat_body["status"], "completed");
    assert_eq!(chat_body["output"][0]["type"], "message");
    assert_eq!(chat_body["output"][1]["type"], "function_call");
    assert_eq!(chat_body["usage"]["input_tokens"], 11);
    assert_eq!(
        chat_body["usage"]["input_tokens_details"]["cached_tokens"],
        3
    );

    let responses_response = read_vector("openai/responses.response.text-tool.json");
    let responses_decoded = decode_vector(&responses_response);
    let responses_encoded = encode_decoded(
        &responses_decoded,
        &profile_id(&responses_response["target_profile"]),
    )
    .unwrap();
    let responses_body: Value =
        serde_json::from_slice(&responses_encoded.output.unwrap().wire.body).unwrap();
    assert_eq!(
        responses_body["choices"][0]["message"]["content"],
        "synthetic reply"
    );
    assert_eq!(
        responses_body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "synthetic_lookup"
    );
    assert_eq!(responses_body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        responses_body["usage"]["completion_tokens_details"]["reasoning_tokens"],
        2
    );

    let error_vector = read_vector("openai/chat.error.cross-profile.json");
    let error_decoded = decode_vector(&error_vector);
    let error_encoded =
        encode_decoded(&error_decoded, &profile_id(&error_vector["target_profile"])).unwrap();
    let error_body: Value =
        serde_json::from_slice(&error_encoded.output.as_ref().unwrap().wire.body).unwrap();
    assert_eq!(error_body["error"]["type"], "rate_limit_error");
    assert_eq!(error_body["error"]["code"], "synthetic_rate_limit");
    assert!(
        error_encoded
            .output
            .as_ref()
            .unwrap()
            .wire
            .protocol_headers
            .iter()
            .any(|header| header.raw_line == "retry-after: 2")
    );
}

#[test]
fn same_profile_replay_is_exact_but_mutation_requires_canonical_encoding() {
    let vector = read_vector("openai/chat.replay.unmodified.json");
    let decoded = decode_vector(&vector);
    let target_profile = profile_id(&vector["target_profile"]);
    let replay = encode_decoded(&decoded, &target_profile)
        .unwrap()
        .output
        .unwrap();
    assert_eq!(replay.wire.body, br#"{"id":"synthetic-1","choices":[]}"#);
    assert_eq!(
        replay.wire.protocol_headers[0].raw_line,
        "Content-Type: application/json"
    );

    let modified = decoded.edit(|payload| {
        if let OpenAiPayload::Response(response) = payload {
            response.model = Some("synthetic-model".to_owned());
        }
    });
    let canonical = modified.into_canonical();
    let encoded = encode_canonical(canonical, &target_profile)
        .unwrap()
        .output
        .unwrap();
    assert_ne!(encoded.wire.body, replay.wire.body);
    assert_eq!(
        encoded.wire.protocol_headers[0].raw_line,
        "content-type: application/json"
    );
}

#[test]
fn same_profile_sse_replay_is_exact_but_stream_edits_encode_canonically() {
    let vector = read_vector("openai/chat.stream.text-tool.json");
    let wire = wire_envelope_from_json(&vector["input"]["envelope"]).unwrap();
    let raw_body = wire.body.clone();
    let raw_headers = wire.protocol_headers.clone();
    let decoded = decode(wire.retained_wire(), wire.adapter_metadata)
        .unwrap()
        .output
        .unwrap();
    let profile = OpenAiProfile::ChatCompletions.profile_id();

    let replay = encode_decoded(&decoded, &profile).unwrap().output.unwrap();
    assert_eq!(replay.wire.body, raw_body);
    assert_eq!(replay.wire.protocol_headers, raw_headers);

    let modified = decoded.edit(|payload| {
        let OpenAiPayload::Stream(events) = payload else {
            panic!("stream vector decodes to stream payload");
        };
        events.push(StreamEvent::TextDelta {
            text: "mutated".to_owned(),
        });
    });
    let canonical = encode_canonical(modified.into_canonical(), &profile)
        .unwrap()
        .output
        .unwrap();
    assert_ne!(canonical.wire.body, raw_body);
    assert_eq!(
        canonical.wire.protocol_headers[0].raw_line,
        "content-type: text/event-stream"
    );
}

#[test]
fn continuation_and_unknown_fields_are_contained_across_profiles() {
    let continuation = read_vector("openai/responses.request.continuation-unsupported.json");
    let continuation_decoded = decode_vector(&continuation);
    let continuation_result = encode_decoded(
        &continuation_decoded,
        &profile_id(&continuation["target_profile"]),
    )
    .unwrap();
    assert_eq!(continuation_result.fidelity, Fidelity::Unsupported);
    assert_eq!(
        diagnostic_codes(&continuation_result),
        vec![json!("non_portable_continuation_handle")]
    );
    assert!(continuation_result.output.is_none());

    let unknown = read_vector("openai/chat.request.unknown-lossy.json");
    let unknown_decoded = decode_vector(&unknown);
    let unknown_result =
        encode_decoded(&unknown_decoded, &profile_id(&unknown["target_profile"])).unwrap();
    assert_eq!(unknown_result.fidelity, Fidelity::Lossy);
    assert_eq!(
        diagnostic_codes(&unknown_result),
        vec![json!("forward_compatible_unknown")]
    );
}

#[test]
fn openai_stream_normalization_is_partition_invariant_and_cross_encodes() {
    for (path, source, target, marker) in [
        (
            "openai/chat.stream.text-tool.json",
            CHAT_COMPLETIONS_PROFILE,
            RESPONSES_PROFILE,
            "event: response.output_text.delta",
        ),
        (
            "openai/responses.stream.text-tool.json",
            RESPONSES_PROFILE,
            CHAT_COMPLETIONS_PROFILE,
            "data: ",
        ),
    ] {
        let vector = read_vector(path);
        let wire = wire_envelope_from_json(&vector["input"]["envelope"]).unwrap();
        let source_profile = ProfileId::new(source).unwrap();
        let expected = decode_sse_chunks(&source_profile, &[wire.body.as_slice()]).unwrap();
        assert_eq!(expected.fidelity, Fidelity::Exact);

        for split in 0..=wire.body.len() {
            let actual =
                decode_sse_chunks(&source_profile, &[&wire.body[..split], &wire.body[split..]])
                    .unwrap();
            assert_eq!(actual, expected, "vector {path}, split at byte {split}");
        }
        for chunk_size in 1..=wire.body.len() {
            let chunks = wire.body.chunks(chunk_size).collect::<Vec<_>>();
            let actual = decode_sse_chunks(&source_profile, &chunks).unwrap();
            assert_eq!(actual, expected, "vector {path}, chunk size {chunk_size}");
        }

        let decoded = decode(wire.retained_wire(), wire.adapter_metadata).unwrap();
        let encoded = encode_decoded(&decoded.output.unwrap(), &ProfileId::new(target).unwrap())
            .unwrap()
            .output
            .unwrap();
        assert_eq!(encoded.wire.body_kind, ProtocolBodyKind::Sse);
        assert!(
            String::from_utf8(encoded.wire.body)
                .unwrap()
                .contains(marker),
            "vector {path}"
        );
    }
}

#[test]
fn unknown_sse_events_are_opaque_for_replay_and_lossy_across_profiles() {
    let profile = OpenAiProfile::ChatCompletions.profile_id();
    let body =
        b"event: response.future\ndata: {\"type\":\"response.future\",\"synthetic\":true}\n\n";
    let events = decode_sse_chunks(&profile, &[body])
        .unwrap()
        .output
        .unwrap();
    assert!(matches!(events.last(), Some(StreamEvent::Opaque { .. })));

    let canonical = CanonicalEnvelope {
        value: OpenAiPayload::Stream(events),
        profile_id: profile,
        status: 200,
        body_kind: ProtocolBodyKind::Sse,
        adapter_metadata: AdapterMetadata::default(),
    };
    let encoded = encode_canonical(canonical, &OpenAiProfile::Responses.profile_id()).unwrap();
    assert_eq!(encoded.fidelity, Fidelity::Lossy);
    assert_eq!(
        diagnostic_codes(&encoded),
        vec![json!("forward_compatible_unknown")]
    );
}
