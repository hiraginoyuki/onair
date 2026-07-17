mod property_support;

use llm_protocol_anthropic as anthropic;
use llm_protocol_core::{
    AdapterMetadata, CanonicalEnvelope, ConversionResult, DecodedEnvelope, Diagnostic, Fidelity,
    ProfileId, ProtocolBodyKind, ProtocolHeaderLine, ProtocolPayload, RetainedWire, SseFrame,
    SseFramer, SseFramingError,
};
use llm_protocol_openai as openai;
use property_support::{
    FROZEN_PROFILES, FrozenProfile, REGRESSION_SEEDS, generated_request, generated_sse,
    malformed_envelope_values, protocol_headers, random_bytes, random_chunks, seeded_rng,
};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
struct ProjectedEncoding {
    fidelity: Fidelity,
    diagnostics: Vec<Diagnostic>,
    output: Option<ProjectedWire>,
}

#[derive(Clone, Debug, PartialEq)]
struct ProjectedWire {
    profile_id: ProfileId,
    status: u16,
    body_kind: ProtocolBodyKind,
    protocol_headers: Vec<ProtocolHeaderLine>,
    body: Vec<u8>,
    adapter_metadata: AdapterMetadata,
    cache_report: Option<Value>,
}

impl ProjectedWire {
    fn retained(&self) -> RetainedWire {
        RetainedWire {
            profile_id: self.profile_id.clone(),
            status: self.status,
            body_kind: self.body_kind,
            protocol_headers: self.protocol_headers.clone(),
            body: self.body.clone(),
        }
    }
}

#[test]
fn generated_sse_is_partition_invariant_and_oversized_state_resets() {
    for seed in REGRESSION_SEEDS {
        let mut rng = seeded_rng(seed);
        for case in 0..64 {
            let input = generated_sse(&mut rng);
            let expected = frame_sse(&[input.as_slice()], 16 * 1024).unwrap();

            for partition in 0..8 {
                let chunks = random_chunks(&input, &mut rng);
                let actual = frame_sse(&chunks, 16 * 1024).unwrap();
                assert_eq!(
                    actual, expected,
                    "seed {seed:#x}, case {case}, partition {partition}"
                );
            }
        }

        for case in 0..32 {
            let max_frame_bytes = 16 + case * 7;
            let input = vec![b'x'; max_frame_bytes + 1];
            let chunks = random_chunks(&input, &mut rng);
            let mut framer = SseFramer::with_max_frame_bytes(max_frame_bytes);
            let mut oversized = false;
            for chunk in chunks {
                match framer.push(chunk) {
                    Ok(_) => {}
                    Err(SseFramingError::FrameTooLarge {
                        max_frame_bytes: actual,
                    }) => {
                        assert_eq!(actual, max_frame_bytes);
                        assert!(framer.is_idle());
                        oversized = true;
                        break;
                    }
                    Err(error) => panic!("ASCII oversized frame returned {error}"),
                }
            }
            assert!(oversized, "seed {seed:#x}, oversized case {case}");
        }
    }
}

#[test]
fn generated_malformed_envelopes_and_bodies_do_not_panic() {
    for seed in REGRESSION_SEEDS {
        let mut rng = seeded_rng(seed);
        for value in malformed_envelope_values(&mut rng) {
            assert!(
                openai::wire_envelope_from_json(&value).is_err(),
                "OpenAI accepted malformed generated envelope for seed {seed:#x}"
            );
            assert!(
                anthropic::wire_envelope_from_json(&value).is_err(),
                "Anthropic accepted malformed generated envelope for seed {seed:#x}"
            );
        }

        for case in 0..256 {
            let body = random_bytes(&mut rng, 512);
            let status = [0, 99, 100, 200, 400, 599, 600, u16::MAX][case % 8];
            for profile in FROZEN_PROFILES {
                for body_kind in [ProtocolBodyKind::Json, ProtocolBodyKind::Sse] {
                    let retained = RetainedWire {
                        profile_id: profile.id(),
                        status,
                        body_kind,
                        protocol_headers: protocol_headers(profile),
                        body: body.clone(),
                    };
                    let _ = decode_profile(retained);
                }

                let chunks = random_chunks(&body, &mut rng);
                match profile {
                    FrozenProfile::ChatCompletions | FrozenProfile::Responses => {
                        let _ = openai::decode_sse_chunks(&profile.id(), &chunks);
                    }
                    FrozenProfile::Messages => {
                        let _ = anthropic::decode_sse_chunks(&profile.id(), &chunks);
                    }
                }
            }
        }
    }

    let unknown_profile = ProfileId::new("synthetic.unknown.alpha-0.1.0").unwrap();
    let retained = RetainedWire {
        profile_id: unknown_profile,
        status: 200,
        body_kind: ProtocolBodyKind::Json,
        protocol_headers: vec![ProtocolHeaderLine::new("content-type: application/json").unwrap()],
        body: br#"{}"#.to_vec(),
    };
    assert!(openai::decode(retained.clone(), AdapterMetadata::default()).is_err());
    assert!(anthropic::decode(retained, AdapterMetadata::default()).is_err());
}

#[test]
fn generated_requests_replay_exactly_and_round_trip_canonically() {
    for seed in REGRESSION_SEEDS {
        let mut rng = seeded_rng(seed);
        for profile in FROZEN_PROFILES {
            for case in 0..32 {
                let retained = generated_request(profile, case, &mut rng);
                let decoded = decode_profile(retained.clone())
                    .unwrap_or_else(|error| {
                        panic!("seed {seed:#x}, profile {profile:?}, case {case}: {error}")
                    })
                    .output
                    .expect("generated request produces typed output");
                let expected_ir = decoded.value().clone();

                let replay = encode_decoded_same_profile(&decoded).unwrap();
                assert_eq!(replay.fidelity, Fidelity::Exact);
                assert!(replay.diagnostics.is_empty());
                let replay = replay.output.expect("same-profile replay has output");
                assert_eq!(replay.profile_id, retained.profile_id);
                assert_eq!(replay.status, retained.status);
                assert_eq!(replay.body_kind, retained.body_kind);
                assert_eq!(replay.protocol_headers, retained.protocol_headers);
                assert_eq!(replay.body, retained.body);
                assert!(replay.cache_report.is_none());

                let canonical = decoded.edit(|_| {}).into_canonical();
                let first = encode_canonical_target(canonical.clone(), profile).unwrap();
                let second = encode_canonical_target(canonical, profile).unwrap();
                assert_eq!(
                    first, second,
                    "seed {seed:#x}, profile {profile:?}, case {case}"
                );
                assert_eq!(first.fidelity, Fidelity::Exact);
                assert!(first.diagnostics.is_empty());
                let canonical_wire = first.output.expect("canonical encoding has output");
                serde_json::from_slice::<Value>(&canonical_wire.body)
                    .expect("canonical JSON output parses");
                let round_trip = decode_profile(canonical_wire.retained())
                    .unwrap()
                    .output
                    .expect("canonical output decodes");
                assert_eq!(
                    round_trip.value(),
                    &expected_ir,
                    "seed {seed:#x}, profile {profile:?}, case {case}"
                );
            }
        }
    }
}

#[test]
fn generated_cross_profile_canonical_encoding_is_deterministic() {
    for seed in REGRESSION_SEEDS {
        let mut rng = seeded_rng(seed);
        for source in FROZEN_PROFILES {
            for case in 0..16 {
                let retained = generated_request(source, case, &mut rng);
                let decoded = decode_profile(retained)
                    .unwrap()
                    .output
                    .expect("generated request produces typed output");
                for target in FROZEN_PROFILES {
                    let canonical = decoded.clone().edit(|_| {}).into_canonical();
                    let first = encode_canonical_target(canonical.clone(), target).unwrap();
                    let second = encode_canonical_target(canonical, target).unwrap();
                    assert_eq!(
                        first, second,
                        "seed {seed:#x}, source {source:?}, target {target:?}, case {case}"
                    );
                    let wire = first.output.unwrap_or_else(|| {
                        panic!(
                            "portable generated request was unsupported: seed {seed:#x}, source {source:?}, target {target:?}, case {case}"
                        )
                    });
                    assert_eq!(wire.profile_id, target.id());
                    assert_eq!(wire.body_kind, ProtocolBodyKind::Json);
                    serde_json::from_slice::<Value>(&wire.body)
                        .expect("canonical target body parses");
                    let decoded_target = decode_profile(wire.retained()).unwrap();
                    assert!(
                        decoded_target.output.is_some(),
                        "seed {seed:#x}, source {source:?}, target {target:?}, case {case}"
                    );
                }
            }
        }
    }
}

fn frame_sse(chunks: &[&[u8]], max_frame_bytes: usize) -> Result<Vec<SseFrame>, SseFramingError> {
    let mut framer = SseFramer::with_max_frame_bytes(max_frame_bytes);
    let mut frames = Vec::new();
    for chunk in chunks {
        frames.extend(framer.push(chunk)?);
    }
    frames.extend(framer.finish()?);
    Ok(frames)
}

fn decode_profile(
    retained: RetainedWire,
) -> Result<ConversionResult<DecodedEnvelope<ProtocolPayload>>, String> {
    match retained.profile_id.as_str() {
        openai::CHAT_COMPLETIONS_PROFILE | openai::RESPONSES_PROFILE => {
            openai::decode(retained, AdapterMetadata::default()).map_err(|error| error.to_string())
        }
        anthropic::MESSAGES_PROFILE => anthropic::decode(retained, AdapterMetadata::default())
            .map_err(|error| error.to_string()),
        profile => Err(format!("unsupported generated profile {profile}")),
    }
}

fn encode_decoded_same_profile(
    decoded: &DecodedEnvelope<ProtocolPayload>,
) -> Result<ProjectedEncoding, String> {
    let target = decoded.retained().profile_id.clone();
    match target.as_str() {
        openai::CHAT_COMPLETIONS_PROFILE | openai::RESPONSES_PROFILE => {
            openai::encode_decoded(decoded, &target)
                .map(project_openai_encoding)
                .map_err(|error| error.to_string())
        }
        anthropic::MESSAGES_PROFILE => anthropic::encode_decoded(decoded, &target)
            .map(project_anthropic_encoding)
            .map_err(|error| error.to_string()),
        profile => Err(format!("unsupported generated profile {profile}")),
    }
}

fn encode_canonical_target(
    canonical: CanonicalEnvelope<ProtocolPayload>,
    target: FrozenProfile,
) -> Result<ProjectedEncoding, String> {
    match target {
        FrozenProfile::ChatCompletions | FrozenProfile::Responses => {
            openai::encode_canonical(canonical, &target.id())
                .map(project_openai_encoding)
                .map_err(|error| error.to_string())
        }
        FrozenProfile::Messages => anthropic::encode_canonical(canonical, &target.id())
            .map(project_anthropic_encoding)
            .map_err(|error| error.to_string()),
    }
}

fn project_openai_encoding(result: ConversionResult<openai::EncodedEnvelope>) -> ProjectedEncoding {
    ProjectedEncoding {
        fidelity: result.fidelity,
        diagnostics: result.diagnostics,
        output: result.output.map(|output| ProjectedWire {
            profile_id: output.wire.profile_id,
            status: output.wire.status,
            body_kind: output.wire.body_kind,
            protocol_headers: output.wire.protocol_headers,
            body: output.wire.body,
            adapter_metadata: output.wire.adapter_metadata,
            cache_report: output
                .cache_report
                .map(|report| serde_json::to_value(report).unwrap()),
        }),
    }
}

fn project_anthropic_encoding(
    result: ConversionResult<anthropic::EncodedEnvelope>,
) -> ProjectedEncoding {
    ProjectedEncoding {
        fidelity: result.fidelity,
        diagnostics: result.diagnostics,
        output: result.output.map(|output| ProjectedWire {
            profile_id: output.wire.profile_id,
            status: output.wire.status,
            body_kind: output.wire.body_kind,
            protocol_headers: output.wire.protocol_headers,
            body: output.wire.body,
            adapter_metadata: output.wire.adapter_metadata,
            cache_report: output
                .cache_report
                .map(|report| serde_json::to_value(report).unwrap()),
        }),
    }
}
