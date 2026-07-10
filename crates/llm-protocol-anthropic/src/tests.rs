use std::path::Path;

use llm_protocol_openai as openai;
use serde_json::Value;

use super::*;

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

fn decode_anthropic_vector(vector: &Value) -> DecodedEnvelope<ProtocolPayload> {
    let wire = wire_envelope_from_json(&vector["input"]["envelope"]).unwrap();
    let decoded = decode(wire.retained_wire(), wire.adapter_metadata).unwrap();
    assert_eq!(decoded.fidelity, Fidelity::Exact);
    decoded
        .output
        .expect("active vector has a typed source payload")
}

fn decode_openai_vector(vector: &Value) -> DecodedEnvelope<ProtocolPayload> {
    let wire = openai::wire_envelope_from_json(&vector["input"]["envelope"]).unwrap();
    let decoded = openai::decode(wire.retained_wire(), wire.adapter_metadata).unwrap();
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
fn profile_identifier_is_frozen() {
    assert_eq!(
        AnthropicProfile::Messages.profile_id().as_str(),
        MESSAGES_PROFILE
    );
    assert_eq!(AnthropicProfile::Messages.api_family(), ApiFamily::Messages);
}

#[test]
fn active_anthropic_vectors_run_through_the_shared_ir_boundary() {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(protocol_root().join("vectors/manifest.json")).unwrap(),
    )
    .unwrap();

    for entry in manifest["vectors"].as_array().unwrap() {
        let vector_id = entry["id"].as_str().unwrap();
        if !vector_id.starts_with("anthropic.") || entry["status"] != "active" {
            continue;
        }
        let path = entry["path"].as_str().unwrap();
        let vector = read_vector(path);
        let decoded = decode_anthropic_vector(&vector);
        let target_profile = profile_id(&vector["target_profile"]);
        let encoded = match target_profile.as_str() {
            MESSAGES_PROFILE => encode_decoded(&decoded, &target_profile).unwrap(),
            openai::CHAT_COMPLETIONS_PROFILE | openai::RESPONSES_PROFILE => {
                let encoded = openai::encode_decoded(&decoded, &target_profile).unwrap();
                ConversionResult {
                    output: encoded.output.map(|output| EncodedEnvelope {
                        wire: WireEnvelope {
                            profile_id: output.wire.profile_id,
                            status: output.wire.status,
                            body_kind: output.wire.body_kind,
                            protocol_headers: output.wire.protocol_headers,
                            body: output.wire.body,
                            adapter_metadata: output.wire.adapter_metadata,
                        },
                        cache_report: output.cache_report,
                    }),
                    fidelity: encoded.fidelity,
                    diagnostics: encoded.diagnostics,
                }
            }
            _ => unreachable!("manifest contains only frozen alpha profiles"),
        };
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
            let output = encoded.output.expect("replay output");
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
fn active_openai_to_messages_vectors_use_the_messages_target_codec() {
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
        if vector["target_profile"].as_str() != Some(MESSAGES_PROFILE) {
            continue;
        }
        let decoded = decode_openai_vector(&vector);
        let encoded = encode_decoded(&decoded, &profile_id(&vector["target_profile"])).unwrap();
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
    }
}

#[test]
fn cache_breakpoints_remain_ordered_and_canonical_messages_encoding_omits_beta() {
    let vector = read_vector("anthropic/messages.request.cache-breakpoint.json");
    let decoded = decode_anthropic_vector(&vector);
    let ProtocolPayload::Request(request) = decoded.value() else {
        panic!("request vector decodes to a request");
    };
    let Some(llm_protocol_core::CacheIntent::Anthropic(intent)) = &request.cache_intent else {
        panic!("request has Anthropic cache intent");
    };
    assert_eq!(intent.breakpoints.len(), 2);
    assert_eq!(
        intent.breakpoints[0].location,
        llm_protocol_core::CacheLocation::Instructions { part_index: 0 }
    );
    assert_eq!(intent.breakpoints[0].ttl.as_deref(), Some("5m"));
    assert_eq!(
        intent.breakpoints[1].location,
        llm_protocol_core::CacheLocation::ToolDefinition { tool_index: 0 }
    );
    assert!(!format!("{intent:?}").contains("synthetic instruction"));

    let canonical = decoded
        .clone()
        .edit(|payload| {
            let ProtocolPayload::Request(request) = payload else {
                panic!("request vector decodes to a request");
            };
            request.model = Some("synthetic-mutated-model".to_owned());
        })
        .into_canonical();
    let encoded = encode_canonical(canonical, &AnthropicProfile::Messages.profile_id())
        .unwrap()
        .output
        .unwrap();
    assert_eq!(
        encoded
            .wire
            .protocol_headers
            .iter()
            .map(|header| header.raw_line.as_str())
            .collect::<Vec<_>>(),
        vec![
            "content-type: application/json",
            "anthropic-version: 2023-06-01",
        ]
    );
    let body: Value = serde_json::from_slice(&encoded.wire.body).unwrap();
    assert_eq!(body["model"], "synthetic-mutated-model");
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "5m");
    assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn buffered_responses_and_errors_cross_encode_from_messages() {
    let response = read_vector("anthropic/messages.response.text-tool.json");
    let decoded = decode_anthropic_vector(&response);
    let encoded = openai::encode_decoded(&decoded, &profile_id(&response["target_profile"]))
        .unwrap()
        .output
        .unwrap();
    let body: Value = serde_json::from_slice(&encoded.wire.body).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "synthetic reply");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "synthetic_lookup"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 3);

    let error = read_vector("anthropic/messages.error.cross-profile.json");
    let decoded = decode_anthropic_vector(&error);
    let encoded = openai::encode_decoded(&decoded, &profile_id(&error["target_profile"]))
        .unwrap()
        .output
        .unwrap();
    let body: Value = serde_json::from_slice(&encoded.wire.body).unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["message"], "synthetic rate limit");
    assert!(
        encoded
            .wire
            .protocol_headers
            .iter()
            .any(|header| header.raw_line == "retry-after: 2")
    );
}

#[test]
fn messages_stream_normalization_is_partition_invariant_and_cross_encodes() {
    let vector = read_vector("anthropic/messages.stream.text-tool.json");
    let wire = wire_envelope_from_json(&vector["input"]["envelope"]).unwrap();
    let profile = AnthropicProfile::Messages.profile_id();
    let expected = decode_sse_chunks(&profile, &[wire.body.as_slice()]).unwrap();
    assert_eq!(expected.fidelity, Fidelity::Exact);

    for split in 0..=wire.body.len() {
        let actual =
            decode_sse_chunks(&profile, &[&wire.body[..split], &wire.body[split..]]).unwrap();
        assert_eq!(actual, expected, "split at byte {split}");
    }
    for chunk_size in 1..=wire.body.len() {
        let chunks = wire.body.chunks(chunk_size).collect::<Vec<_>>();
        let actual = decode_sse_chunks(&profile, &chunks).unwrap();
        assert_eq!(actual, expected, "chunk size {chunk_size}");
    }

    let decoded = decode(wire.retained_wire(), wire.adapter_metadata)
        .unwrap()
        .output
        .unwrap();
    let encoded = openai::encode_decoded(
        &decoded,
        &ProfileId::new(openai::RESPONSES_PROFILE).unwrap(),
    )
    .unwrap()
    .output
    .unwrap();
    assert_eq!(encoded.wire.body_kind, ProtocolBodyKind::Sse);
    assert!(
        String::from_utf8(encoded.wire.body)
            .unwrap()
            .contains("event: response.function_call_arguments.delta")
    );
}

#[test]
fn messages_sse_replay_is_exact_but_stream_edits_encode_canonically() {
    let vector = read_vector("anthropic/messages.stream.replay.unmodified.json");
    let wire = wire_envelope_from_json(&vector["input"]["envelope"]).unwrap();
    let raw_body = wire.body.clone();
    let raw_headers = wire.protocol_headers.clone();
    let decoded = decode(wire.retained_wire(), wire.adapter_metadata)
        .unwrap()
        .output
        .unwrap();
    let profile = AnthropicProfile::Messages.profile_id();

    let replay = encode_decoded(&decoded, &profile).unwrap().output.unwrap();
    assert_eq!(replay.wire.body, raw_body);
    assert_eq!(replay.wire.protocol_headers, raw_headers);

    let modified = decoded.edit(|payload| {
        let ProtocolPayload::Stream(events) = payload else {
            panic!("stream vector decodes to a stream payload");
        };
        events.push(StreamEvent::TextDelta {
            text: "mutated".to_owned(),
        });
    });
    let canonical = encode_canonical(modified.into_canonical(), &profile).unwrap();
    assert_eq!(canonical.fidelity, Fidelity::Lossy);
    assert_eq!(
        canonical
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<Vec<_>>(),
        vec![DiagnosticCode::NonPortableOpaqueExtension]
    );
    let canonical = canonical.output.unwrap();
    assert_ne!(canonical.wire.body, raw_body);
    assert_eq!(
        canonical
            .wire
            .protocol_headers
            .iter()
            .map(|header| header.raw_line.as_str())
            .collect::<Vec<_>>(),
        vec![
            "content-type: text/event-stream",
            "anthropic-version: 2023-06-01",
        ]
    );
}

#[test]
fn all_six_directed_profile_pairs_cross_the_same_payload_type() {
    let cases = [
        (
            "openai/chat.request.text-tool.json",
            openai::RESPONSES_PROFILE,
            "chat_to_responses",
        ),
        (
            "openai/chat.request.to-messages.json",
            MESSAGES_PROFILE,
            "chat_to_messages",
        ),
        (
            "openai/responses.request.text-tool.json",
            openai::CHAT_COMPLETIONS_PROFILE,
            "responses_to_chat",
        ),
        (
            "openai/responses.request.to-messages.json",
            MESSAGES_PROFILE,
            "responses_to_messages",
        ),
        (
            "anthropic/messages.response.text-tool.json",
            openai::CHAT_COMPLETIONS_PROFILE,
            "messages_to_chat",
        ),
        (
            "anthropic/messages.request.cache-breakpoint.json",
            openai::RESPONSES_PROFILE,
            "messages_to_responses",
        ),
    ];

    for (path, target, label) in cases {
        let vector = read_vector(path);
        let decoded = if vector["source_profile"].as_str() == Some(MESSAGES_PROFILE) {
            decode_anthropic_vector(&vector)
        } else {
            decode_openai_vector(&vector)
        };
        let result = if target == MESSAGES_PROFILE {
            encode_decoded(&decoded, &ProfileId::new(target).unwrap()).unwrap()
        } else {
            let result =
                openai::encode_decoded(&decoded, &ProfileId::new(target).unwrap()).unwrap();
            ConversionResult {
                output: result.output.map(|output| EncodedEnvelope {
                    wire: WireEnvelope {
                        profile_id: output.wire.profile_id,
                        status: output.wire.status,
                        body_kind: output.wire.body_kind,
                        protocol_headers: output.wire.protocol_headers,
                        body: output.wire.body,
                        adapter_metadata: output.wire.adapter_metadata,
                    },
                    cache_report: output.cache_report,
                }),
                fidelity: result.fidelity,
                diagnostics: result.diagnostics,
            }
        };
        assert!(result.output.is_some(), "{label}");
        assert_ne!(result.fidelity, Fidelity::Unsupported, "{label}");
    }
}

#[test]
fn cache_directives_are_lossy_across_providers_and_unknown_material_is_contained() {
    let messages = read_vector("anthropic/messages.request.cache-breakpoint.json");
    let decoded = decode_anthropic_vector(&messages);
    let result = openai::encode_decoded(
        &decoded,
        &ProfileId::new(openai::RESPONSES_PROFILE).unwrap(),
    )
    .unwrap();
    assert_eq!(result.fidelity, Fidelity::Lossy);
    let cache_diagnostics = result
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code)
        .collect::<Vec<_>>();
    assert!(cache_diagnostics.contains(&DiagnosticCode::NonPortableCacheIntent));
    assert!(cache_diagnostics.contains(&DiagnosticCode::ForwardCompatibleUnknown));
    let output = result.output.unwrap();
    let body: Value = serde_json::from_slice(&output.wire.body).unwrap();
    assert!(body.get("prompt_cache_key").is_none());
    assert!(
        output
            .cache_report
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.status == llm_protocol_core::CachePreservationStatus::NonPortable)
    );

    let unknown = read_vector("anthropic/messages.unknown-lossy.json");
    let decoded = decode_anthropic_vector(&unknown);
    let result = openai::encode_decoded(&decoded, &profile_id(&unknown["target_profile"])).unwrap();
    assert_eq!(result.fidelity, Fidelity::Lossy);
    assert_eq!(
        result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<Vec<_>>(),
        vec![DiagnosticCode::ForwardCompatibleUnknown]
    );
}

#[test]
fn version_mismatch_and_unknown_sse_events_do_not_gain_portable_semantics() {
    let profile = AnthropicProfile::Messages.profile_id();
    let retained = RetainedWire {
        profile_id: profile.clone(),
        status: 200,
        body_kind: ProtocolBodyKind::Json,
        protocol_headers: vec![
            ProtocolHeaderLine::new("content-type: application/json").unwrap(),
            ProtocolHeaderLine::new("anthropic-version: 2024-01-01").unwrap(),
        ],
        body: br#"{"model":"synthetic-model","max_tokens":1,"messages":[]}"#.to_vec(),
    };
    assert!(matches!(
        decode(retained, AdapterMetadata::default()),
        Err(CodecError::UnsupportedAnthropicVersion(version)) if version == "2024-01-01"
    ));

    let events = decode_sse_chunks(
        &profile,
        &[b"event: future.event\ndata: {\"type\":\"future.event\",\"synthetic\":true}\n\n"],
    )
    .unwrap()
    .output
    .unwrap();
    assert!(matches!(events.last(), Some(StreamEvent::Opaque { .. })));

    let canonical = CanonicalEnvelope {
        value: ProtocolPayload::Stream(events),
        profile_id: profile,
        status: 200,
        body_kind: ProtocolBodyKind::Sse,
        adapter_metadata: AdapterMetadata::default(),
    };
    let result = openai::encode_canonical(
        canonical,
        &ProfileId::new(openai::RESPONSES_PROFILE).unwrap(),
    )
    .unwrap();
    assert_eq!(result.fidelity, Fidelity::Lossy);
    assert_eq!(
        result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<Vec<_>>(),
        vec![DiagnosticCode::ForwardCompatibleUnknown]
    );
}
