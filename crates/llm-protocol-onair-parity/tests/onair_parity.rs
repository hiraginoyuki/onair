mod support;

use std::collections::BTreeMap;

use llm_protocol_anthropic as anthropic;
use llm_protocol_core::{
    ANTHROPIC_MESSAGES_PROFILE, AdapterMetadata, OPENAI_CHAT_COMPLETIONS_PROFILE,
    OPENAI_RESPONSES_PROFILE, ProfileId, ProtocolBodyKind, ProtocolHeaderLine,
};
use llm_protocol_openai as openai;
use onair_core::{
    config::{
        ChatStreamUsagePolicy, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, ToolSchemaMode,
    },
    openai::{
        RequestMode, RequestRewritePolicies, rewrite_request_body_for_mode_with_policies,
        rewrite_response_body,
    },
};
use serde_json::{Value, json};
use support::wire_semantics::{
    ConversationItem, PortableUsage, ResponseItem, SelectedCacheIntent, SelectedGeneration,
    SelectedRequest, SelectedResponse, SelectedRole, SelectedTool, parse_chat_request,
    parse_chat_response, parse_messages_request, parse_responses_request,
};

fn policies() -> RequestRewritePolicies {
    RequestRewritePolicies {
        tool_schema_mode: ToolSchemaMode::Preserve,
        responses_store: ResponsesStorePolicy::Preserve,
        responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
        chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        anthropic_max_tokens: None,
    }
}

fn profile(value: &str) -> ProfileId {
    ProfileId::new(value).expect("test profile is valid")
}

fn content_type_header() -> ProtocolHeaderLine {
    ProtocolHeaderLine::new("content-type: application/json").unwrap()
}

fn json_value(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("codec output is JSON")
}

fn alpha_openai_request_target(request: &Value, source: &str, target: &str) -> Value {
    let decoded = openai::decode(
        openai::WireEnvelope {
            profile_id: profile(source),
            status: 200,
            body_kind: ProtocolBodyKind::Json,
            protocol_headers: vec![content_type_header()],
            body: serde_json::to_vec(request).unwrap(),
            adapter_metadata: AdapterMetadata::default(),
        }
        .retained_wire(),
        AdapterMetadata::default(),
    )
    .unwrap()
    .output
    .unwrap();
    let target = openai::encode_decoded(&decoded, &profile(target))
        .unwrap()
        .output
        .unwrap()
        .wire;
    json_value(&target.body)
}

fn onair_openai_request_target(request: &Value, mode: RequestMode, path: &str) -> Value {
    let body = rewrite_request_body_for_mode_with_policies(
        &serde_json::to_vec(request).unwrap(),
        Some("application/json"),
        Some("synthetic-model"),
        path,
        mode,
        &policies(),
        &BTreeMap::new(),
        "alpha-parity",
    )
    .unwrap();
    json_value(&body)
}

fn expected_text_tool_request(strict: Option<bool>) -> SelectedRequest {
    SelectedRequest {
        model: "synthetic-model".to_owned(),
        stream: false,
        instructions: vec!["Synthetic system.".to_owned()],
        conversation: vec![ConversationItem::Text {
            role: SelectedRole::User,
            text: "Synthetic request.".to_owned(),
        }],
        tools: vec![SelectedTool {
            name: "synthetic_lookup".to_owned(),
            description: Some("Synthetic lookup.".to_owned()),
            input_schema: json!({"type": "object"}),
            strict,
        }],
        generation: SelectedGeneration {
            temperature: Some(0.2),
            top_p: Some(0.9),
            max_output_tokens: Some(24),
            stop_sequences: Vec::new(),
        },
        output_format: None,
        cache: SelectedCacheIntent::default(),
    }
}

#[test]
fn chat_request_to_responses_matches_selected_onair_semantics() {
    let request = json!({
        "model": "synthetic-model",
        "messages": [
            {"role": "system", "content": "Synthetic system."},
            {"role": "user", "content": "Synthetic request."}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "synthetic_lookup",
                "description": "Synthetic lookup.",
                "parameters": {"type": "object"},
                "strict": true
            }
        }],
        "temperature": 0.2,
        "top_p": 0.9,
        "max_completion_tokens": 24
    });
    let alpha = alpha_openai_request_target(
        &request,
        OPENAI_CHAT_COMPLETIONS_PROFILE,
        OPENAI_RESPONSES_PROFILE,
    );
    let onair = onair_openai_request_target(
        &request,
        RequestMode::ChatCompletionsViaResponses,
        "/v1/chat/completions",
    );
    let expected = expected_text_tool_request(Some(true));

    assert_eq!(parse_responses_request(&alpha).unwrap(), expected);
    assert_eq!(parse_responses_request(&onair).unwrap(), expected);
}

#[test]
fn responses_request_to_chat_matches_selected_onair_semantics() {
    let request = json!({
        "model": "synthetic-model",
        "instructions": "Synthetic system.",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "Synthetic request."}]
        }],
        "tools": [{
            "type": "function",
            "name": "synthetic_lookup",
            "description": "Synthetic lookup.",
            "parameters": {"type": "object"},
            "strict": true
        }],
        "temperature": 0.2,
        "top_p": 0.9,
        "max_output_tokens": 24
    });
    let alpha = alpha_openai_request_target(
        &request,
        OPENAI_RESPONSES_PROFILE,
        OPENAI_CHAT_COMPLETIONS_PROFILE,
    );
    let onair = onair_openai_request_target(
        &request,
        RequestMode::ResponsesViaChatCompletions,
        "/v1/responses",
    );
    let expected = expected_text_tool_request(Some(true));

    assert_eq!(parse_chat_request(&alpha).unwrap(), expected);
    assert_eq!(parse_chat_request(&onair).unwrap(), expected);
}

#[test]
fn chat_request_to_messages_matches_selected_onair_semantics() {
    let request = json!({
        "model": "synthetic-model",
        "messages": [
            {"role": "system", "content": "Synthetic system."},
            {"role": "user", "content": "Synthetic request."},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_synthetic",
                    "type": "function",
                    "function": {
                        "name": "synthetic_lookup",
                        "arguments": "{\"subject\":\"alpha\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_synthetic",
                "content": "Synthetic result."
            }
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "synthetic_lookup",
                "description": "Synthetic lookup.",
                "parameters": {"type": "object"}
            }
        }],
        "max_tokens": 24,
        "temperature": 0.2,
        "top_p": 0.9
    });
    let decoded = openai::decode(
        openai::WireEnvelope {
            profile_id: profile(OPENAI_CHAT_COMPLETIONS_PROFILE),
            status: 200,
            body_kind: ProtocolBodyKind::Json,
            protocol_headers: vec![content_type_header()],
            body: serde_json::to_vec(&request).unwrap(),
            adapter_metadata: AdapterMetadata::default(),
        }
        .retained_wire(),
        AdapterMetadata::default(),
    )
    .unwrap()
    .output
    .unwrap();
    let alpha = anthropic::encode_canonical(
        decoded.edit(|_| {}).into_canonical(),
        &profile(ANTHROPIC_MESSAGES_PROFILE),
    )
    .unwrap()
    .output
    .unwrap()
    .wire;
    let onair = onair_openai_request_target(
        &request,
        RequestMode::ChatCompletionsViaMessages,
        "/v1/chat/completions",
    );
    let expected = SelectedRequest {
        conversation: vec![
            ConversationItem::Text {
                role: SelectedRole::User,
                text: "Synthetic request.".to_owned(),
            },
            ConversationItem::Text {
                role: SelectedRole::Assistant,
                text: String::new(),
            },
            ConversationItem::ToolCall {
                role: SelectedRole::Assistant,
                id: "call_synthetic".to_owned(),
                name: "synthetic_lookup".to_owned(),
                arguments: json!({"subject": "alpha"}),
            },
            ConversationItem::ToolResult {
                tool_call_id: "call_synthetic".to_owned(),
                content: vec!["Synthetic result.".to_owned()],
                is_error: false,
            },
        ],
        tools: vec![SelectedTool {
            name: "synthetic_lookup".to_owned(),
            description: Some("Synthetic lookup.".to_owned()),
            input_schema: json!({"type": "object"}),
            strict: None,
        }],
        ..expected_text_tool_request(None)
    };

    assert_eq!(
        parse_messages_request(&json_value(&alpha.body)).unwrap(),
        expected
    );
    assert_eq!(parse_messages_request(&onair).unwrap(), expected);
}

#[test]
fn messages_response_to_chat_matches_selected_onair_semantics() {
    let response = json!({
        "id": "msg_synthetic",
        "type": "message",
        "role": "assistant",
        "model": "synthetic-model",
        "content": [
            {"type": "text", "text": "Synthetic reply."},
            {
                "type": "tool_use",
                "id": "call_synthetic",
                "name": "synthetic_lookup",
                "input": {"subject": "alpha"}
            }
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 11,
            "output_tokens": 7,
            "cache_read_input_tokens": 3
        }
    });
    let decoded = anthropic::decode(
        anthropic::WireEnvelope {
            profile_id: profile(ANTHROPIC_MESSAGES_PROFILE),
            status: 200,
            body_kind: ProtocolBodyKind::Json,
            protocol_headers: vec![
                content_type_header(),
                ProtocolHeaderLine::new("anthropic-version: 2023-06-01").unwrap(),
            ],
            body: serde_json::to_vec(&response).unwrap(),
            adapter_metadata: AdapterMetadata::default(),
        }
        .retained_wire(),
        AdapterMetadata::default(),
    )
    .unwrap()
    .output
    .unwrap();
    let alpha = openai::encode_canonical(
        decoded.edit(|_| {}).into_canonical(),
        &profile(OPENAI_CHAT_COMPLETIONS_PROFILE),
    )
    .unwrap()
    .output
    .unwrap()
    .wire;
    let (onair, _) = rewrite_response_body(
        &serde_json::to_vec(&response).unwrap(),
        Some("application/json"),
        Some("synthetic-model"),
        Some("synthetic-model"),
        RequestMode::ChatCompletionsViaMessages,
    );
    let expected = SelectedResponse {
        model: "synthetic-model".to_owned(),
        output: vec![
            ResponseItem::Text("Synthetic reply.".to_owned()),
            ResponseItem::ToolCall {
                id: "call_synthetic".to_owned(),
                name: "synthetic_lookup".to_owned(),
                arguments: json!({"subject": "alpha"}),
            },
        ],
        usage: Some(PortableUsage {
            input_tokens: 11,
            output_tokens: 7,
        }),
        finish_reason: "tool_calls".to_owned(),
    };

    assert_eq!(
        parse_chat_response(&json_value(&alpha.body)).unwrap(),
        expected
    );
    assert_eq!(parse_chat_response(&json_value(&onair)).unwrap(), expected);
}
