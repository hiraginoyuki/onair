use llm_protocol_core::{
    ANTHROPIC_MESSAGES_PROFILE, OPENAI_CHAT_COMPLETIONS_PROFILE, OPENAI_RESPONSES_PROFILE,
    ProfileId, ProtocolBodyKind, ProtocolHeaderLine, RetainedWire,
};
use rand::{Rng, SeedableRng, rngs::StdRng};
use serde_json::{Map, Number, Value, json};

pub const REGRESSION_SEEDS: [u64; 4] = [
    0x0A11_CE00_0000_0001,
    0x0A11_CE00_0000_0017,
    0x0A11_CE00_0000_0101,
    0x0A11_CE00_0000_1009,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrozenProfile {
    ChatCompletions,
    Responses,
    Messages,
}

pub const FROZEN_PROFILES: [FrozenProfile; 3] = [
    FrozenProfile::ChatCompletions,
    FrozenProfile::Responses,
    FrozenProfile::Messages,
];

impl FrozenProfile {
    pub fn id(self) -> ProfileId {
        ProfileId::new(match self {
            Self::ChatCompletions => OPENAI_CHAT_COMPLETIONS_PROFILE,
            Self::Responses => OPENAI_RESPONSES_PROFILE,
            Self::Messages => ANTHROPIC_MESSAGES_PROFILE,
        })
        .expect("frozen profile identifier is valid")
    }
}

pub fn seeded_rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

pub fn generated_request(profile: FrozenProfile, case: usize, rng: &mut StdRng) -> RetainedWire {
    let model = format!("synthetic-model-{case}");
    let system = format!("Synthetic system {case}.");
    let prompt = format!("Synthetic request {}.", random_ascii(rng, 4, 18));
    let temperature = f64::from(rng.gen_range(0_u16..=100)) / 100.0;
    let top_p = f64::from(rng.gen_range(1_u16..=100)) / 100.0;
    let max_tokens = rng.gen_range(1_u64..=512);
    let include_tool = case.is_multiple_of(2);

    let body = match profile {
        FrozenProfile::ChatCompletions => {
            let user_content = if case.is_multiple_of(3) {
                json!([{"type": "text", "text": prompt}])
            } else {
                Value::String(prompt)
            };
            let mut value = json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user_content}
                ],
                "temperature": temperature,
                "top_p": top_p,
                "max_completion_tokens": max_tokens
            });
            if include_tool {
                value["tools"] = openai_chat_tools(case);
            }
            value
        }
        FrozenProfile::Responses => {
            let mut value = json!({
                "model": model,
                "instructions": system,
                "input": [{
                    "role": "user",
                    "content": [{"type": "input_text", "text": prompt}]
                }],
                "temperature": temperature,
                "top_p": top_p,
                "max_output_tokens": max_tokens
            });
            if include_tool {
                value["tools"] = openai_responses_tools(case);
            }
            value
        }
        FrozenProfile::Messages => {
            let system = if case.is_multiple_of(3) {
                json!([{"type": "text", "text": system}])
            } else {
                Value::String(system)
            };
            let mut value = json!({
                "model": model,
                "system": system,
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": prompt}]
                }],
                "temperature": temperature,
                "top_p": top_p,
                "max_tokens": max_tokens
            });
            if include_tool {
                value["tools"] = anthropic_tools(case);
            }
            value
        }
    };

    RetainedWire {
        profile_id: profile.id(),
        status: 200,
        body_kind: ProtocolBodyKind::Json,
        protocol_headers: protocol_headers(profile),
        body: serde_json::to_vec(&body).expect("generated request is serializable"),
    }
}

pub fn protocol_headers(profile: FrozenProfile) -> Vec<ProtocolHeaderLine> {
    let mut headers = vec![
        ProtocolHeaderLine::new("content-type: application/json")
            .expect("synthetic content-type is valid"),
    ];
    if profile == FrozenProfile::Messages {
        headers.push(
            ProtocolHeaderLine::new("anthropic-version: 2023-06-01")
                .expect("synthetic Anthropic version is valid"),
        );
    }
    headers
}

pub fn random_bytes(rng: &mut StdRng, max_len: usize) -> Vec<u8> {
    let len = rng.gen_range(0..=max_len);
    (0..len).map(|_| rng.gen_range(0..=u8::MAX)).collect()
}

pub fn random_json(rng: &mut StdRng, depth: usize) -> Value {
    if depth == 0 {
        return random_scalar(rng);
    }
    match rng.gen_range(0..7) {
        0..=3 => random_scalar(rng),
        4 => Value::Array(
            (0..rng.gen_range(0..=4))
                .map(|_| random_json(rng, depth - 1))
                .collect(),
        ),
        _ => {
            let mut object = Map::new();
            for index in 0..rng.gen_range(0..=4) {
                object.insert(
                    format!("{}_{}", random_ascii(rng, 1, 8), index),
                    random_json(rng, depth - 1),
                );
            }
            Value::Object(object)
        }
    }
}

pub fn generated_sse(rng: &mut StdRng) -> Vec<u8> {
    let mut stream = String::new();
    for event_index in 0..rng.gen_range(1..=8) {
        let newline = if rng.gen_bool(0.5) { "\n" } else { "\r\n" };
        if rng.gen_bool(0.5) {
            stream.push_str(": synthetic-");
            stream.push_str(&event_index.to_string());
            stream.push_str(newline);
        }
        if rng.gen_bool(0.7) {
            stream.push_str("id: item-");
            stream.push_str(&event_index.to_string());
            stream.push_str(newline);
        }
        if rng.gen_bool(0.8) {
            stream.push_str("event: synthetic.delta");
            stream.push_str(newline);
        }
        for _ in 0..rng.gen_range(1..=4) {
            stream.push_str("data: ");
            stream.push_str(&random_ascii(rng, 0, 32));
            stream.push_str(newline);
        }
        if rng.gen_bool(0.4) {
            stream.push_str("retry: ");
            stream.push_str(&rng.gen_range(0_u64..=10_000).to_string());
            stream.push_str(newline);
        }
        if rng.gen_bool(0.3) {
            stream.push_str("x-synthetic: retained");
            stream.push_str(newline);
        }
        stream.push_str(newline);
    }
    stream.into_bytes()
}

pub fn random_chunks<'a>(input: &'a [u8], rng: &mut StdRng) -> Vec<&'a [u8]> {
    if input.is_empty() {
        return vec![input];
    }
    let mut chunks = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        let remaining = input.len() - offset;
        let len = rng.gen_range(1..=remaining.min(37));
        chunks.push(&input[offset..offset + len]);
        offset += len;
    }
    chunks
}

pub fn malformed_envelope_values(rng: &mut StdRng) -> Vec<Value> {
    let mut values = vec![
        Value::Null,
        json!([]),
        json!(true),
        json!(42),
        json!("synthetic"),
        json!({}),
        json!({"protocol_version": "0.1.0"}),
        json!({
            "protocol_version": "0.1.0",
            "profile_id": OPENAI_CHAT_COMPLETIONS_PROFILE,
            "status": 200,
            "body_kind": "unknown",
            "protocol_headers": [],
            "body_base64": "***"
        }),
        json!({
            "protocol_version": "0.2.0",
            "profile_id": OPENAI_CHAT_COMPLETIONS_PROFILE,
            "status": 200,
            "body_kind": "json",
            "protocol_headers": [{"raw_line": "content-type: application/json"}],
            "body_base64": "e30="
        }),
        json!({
            "protocol_version": "0.1.0",
            "profile_id": OPENAI_CHAT_COMPLETIONS_PROFILE,
            "status": 99,
            "body_kind": "json",
            "protocol_headers": [{"raw_line": "content-type: application/json"}],
            "body_base64": "e30="
        }),
        json!({
            "protocol_version": "0.1.0",
            "profile_id": OPENAI_CHAT_COMPLETIONS_PROFILE,
            "status": 200,
            "body_kind": "json",
            "protocol_headers": [{"raw_line": "authorization: synthetic"}],
            "body_base64": "e30="
        }),
    ];
    values.extend((0..128).map(|_| random_json(rng, 3)));
    values
}

fn openai_chat_tools(case: usize) -> Value {
    json!([{
        "type": "function",
        "function": {
            "name": format!("synthetic_tool_{case}"),
            "description": "Synthetic tool.",
            "parameters": {
                "type": "object",
                "properties": {"value": {"type": "string"}}
            },
            "strict": case.is_multiple_of(4)
        }
    }])
}

fn openai_responses_tools(case: usize) -> Value {
    json!([{
        "type": "function",
        "name": format!("synthetic_tool_{case}"),
        "description": "Synthetic tool.",
        "parameters": {
            "type": "object",
            "properties": {"value": {"type": "string"}}
        },
        "strict": case.is_multiple_of(4)
    }])
}

fn anthropic_tools(case: usize) -> Value {
    json!([{
        "name": format!("synthetic_tool_{case}"),
        "description": "Synthetic tool.",
        "input_schema": {
            "type": "object",
            "properties": {"value": {"type": "string"}}
        }
    }])
}

fn random_scalar(rng: &mut StdRng) -> Value {
    match rng.gen_range(0..5) {
        0 => Value::Null,
        1 => Value::Bool(rng.gen_bool(0.5)),
        2 => Value::Number(Number::from(rng.gen_range(0_u64..=u64::from(u32::MAX)))),
        _ => Value::String(random_ascii(rng, 0, 32)),
    }
}

fn random_ascii(rng: &mut StdRng, min_len: usize, max_len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-_";
    let len = rng.gen_range(min_len..=max_len);
    (0..len)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}
