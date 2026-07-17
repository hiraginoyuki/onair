use std::path::Path;

use llm_protocol_openai as openai;
use serde_json::{Value, json};

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
fn message_text_assets_and_response_roles_cover_the_typed_variants() {
    let profile = AnthropicProfile::Messages.profile_id();
    let mut breakpoints = Vec::new();
    let message = decode_message(
        &json!({"role": "user", "content": "synthetic text"}),
        &profile,
        0,
        &mut breakpoints,
    )
    .unwrap();
    assert_eq!(message.role, ConversationRole::User);
    assert_eq!(
        message.content,
        vec![ContentPart::Text {
            text: "synthetic text".to_owned(),
        }]
    );
    assert!(breakpoints.is_empty());

    let asset_object = json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": "image/png",
            "data": "c3ludGhldGlj"
        }
    })
    .as_object()
    .unwrap()
    .clone();
    let asset = decode_asset(&asset_object, "image", "/content/0").unwrap();
    assert_eq!(asset.reference_type, AssetReferenceType::Data);
    assert_eq!(asset.value, "data:image/png;base64,c3ludGhldGlj".to_owned());
    assert_eq!(asset.media_type.as_deref(), Some("image/png"));

    assert_eq!(
        decode_response_role("user").unwrap(),
        ConversationRole::User
    );
    assert!(decode_response_role("system").is_err());
}

#[test]
fn error_categories_prefer_known_types_then_fall_back_to_status() {
    for (error_type, expected) in [
        ("invalid_request_error", ErrorCategory::InvalidRequest),
        ("authentication_error", ErrorCategory::Authentication),
        ("permission_error", ErrorCategory::Permission),
        ("not_found_error", ErrorCategory::NotFound),
        ("rate_limit_error", ErrorCategory::RateLimit),
        ("conflict_error", ErrorCategory::Conflict),
        ("api_error", ErrorCategory::Server),
        ("overloaded_error", ErrorCategory::Server),
    ] {
        assert_eq!(
            error_category(Some(error_type), 418),
            expected,
            "{error_type}"
        );
    }

    for (status, expected) in [
        (400, ErrorCategory::InvalidRequest),
        (422, ErrorCategory::InvalidRequest),
        (401, ErrorCategory::Authentication),
        (403, ErrorCategory::Permission),
        (404, ErrorCategory::NotFound),
        (409, ErrorCategory::Conflict),
        (429, ErrorCategory::RateLimit),
        (500, ErrorCategory::Server),
        (599, ErrorCategory::Server),
        (418, ErrorCategory::Unknown),
        (600, ErrorCategory::Unknown),
    ] {
        assert_eq!(error_category(None, status), expected, "status {status}");
    }
}

#[test]
fn typed_stream_parts_and_errors_have_a_closed_ordered_lifecycle() {
    let body = [
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {"id": "msg_synthetic"}
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "call_synthetic", "name": "lookup"}
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "thinking"}
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "thinking_delta", "thinking": "synthetic reasoning"}
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": {"type": "refusal"}
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "refusal_delta", "refusal": "synthetic refusal"}
            }),
        ),
        (
            "error",
            json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "synthetic failure"}
            }),
        ),
    ]
    .into_iter()
    .map(|(event, value)| format!("event: {event}\ndata: {value}\n\n"))
    .collect::<String>();
    let events = decode_sse_chunks(&AnthropicProfile::Messages.profile_id(), &[body.as_bytes()])
        .unwrap()
        .output
        .unwrap();

    assert!(matches!(events.first(), Some(StreamEvent::RequestStarted)));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::OutputPartStarted {
            part_index: 0,
            part_type: OutputPartType::ToolCall,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::OutputPartStarted {
            part_index: 1,
            part_type: OutputPartType::Reasoning,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::OutputPartStarted {
            part_index: 2,
            part_type: OutputPartType::Refusal,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolCallDelta {
            call_id,
            name: Some(name),
            arguments_delta,
        } if call_id == "call_synthetic" && name == "lookup" && arguments_delta == "{\"q\":"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ReasoningDelta { text } if text == "synthetic reasoning"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::RefusalPart { text, .. } if text == "synthetic refusal"
    )));
    let error_index = events
        .iter()
        .position(|event| matches!(event, StreamEvent::Error { .. }))
        .unwrap();
    assert_eq!(
        events[..error_index]
            .iter()
            .filter(|event| matches!(event, StreamEvent::OutputPartEnded { .. }))
            .count(),
        3
    );
    assert!(matches!(
        &events[error_index],
        StreamEvent::Error { error }
            if error.category == ErrorCategory::Server && error.message == "synthetic failure"
    ));
}

#[test]
fn opaque_stream_locations_advance_across_unparseable_frames() {
    let body = concat!(
        "event: synthetic.invalid\n",
        "data: {not-json}\n\n",
        "event: synthetic.future\n",
        "data: {\"type\":\"synthetic.future\"}\n\n",
        "event: synthetic.later\n",
        "data: {\"type\":\"synthetic.later\"}\n\n",
    );
    let events = decode_sse_chunks(&AnthropicProfile::Messages.profile_id(), &[body.as_bytes()])
        .unwrap()
        .output
        .unwrap();
    let indices = events
        .iter()
        .filter_map(|event| {
            let StreamEvent::Opaque { extension } = event else {
                return None;
            };
            let SourceLocation::SseEvent { index, .. } = &extension.source_location else {
                return None;
            };
            Some(*index)
        })
        .collect::<Vec<_>>();
    assert_eq!(indices, vec![0, 1, 2]);
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
            vector["expect"]["encode"]["fidelity"],
            "vector {vector_id}"
        );
        assert_eq!(
            diagnostic_codes(&encoded),
            vector["expect"]["encode"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|diagnostic| diagnostic["code"].clone())
                .collect::<Vec<_>>(),
            "vector {vector_id}"
        );
        if vector["kind"] == "exact_replay" {
            let expected_envelope = &vector["expect"]["encode"]["envelope"];
            let output = encoded.output.expect("replay output");
            assert_eq!(
                output
                    .wire
                    .protocol_headers
                    .iter()
                    .map(|header| header.raw_line.clone())
                    .collect::<Vec<_>>(),
                expected_envelope["protocol_headers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value["raw_line"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
                "vector {vector_id}"
            );
            assert_eq!(
                encode_base64(&output.wire.body),
                expected_envelope["body_base64"].as_str().unwrap(),
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
            vector["expect"]["encode"]["fidelity"],
            "vector {vector_id}"
        );
        assert_eq!(
            diagnostic_codes(&encoded),
            vector["expect"]["encode"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|diagnostic| diagnostic["code"].clone())
                .collect::<Vec<_>>(),
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
        vec![
            DiagnosticCode::SemanticChange,
            DiagnosticCode::ForwardCompatibleUnknown,
        ]
    );
}
