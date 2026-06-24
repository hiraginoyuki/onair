use std::collections::BTreeMap;

use crate::config::{
    ChatStreamUsagePolicy, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, ToolSchemaMode,
};
use serde_json::{Value, json};

use super::*;

fn rewrite_request_body_for_mode_with_tool_schema_mode(
    body: &[u8],
    content_type: Option<&str>,
    backend_model: Option<&str>,
    path: &str,
    request_mode: RequestMode,
    tool_schema_mode: ToolSchemaMode,
    responses_store: ResponsesStorePolicy,
) -> Result<Vec<u8>, RequestRewriteError> {
    rewrite_request_body_for_mode_with_policies(
        body,
        content_type,
        backend_model,
        path,
        request_mode,
        &RequestRewritePolicies {
            tool_schema_mode,
            responses_store,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
}

#[test]
fn responses_request_converts_to_chat_completions() {
    let body = json!({
        "model": "public-model",
        "instructions": "be useful",
        "input": [
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "hello"}
                ]
            }
        ],
        "max_output_tokens": 32,
        "prompt_cache_key": "tenant-a",
        "stream": true,
        "tools": [{
            "type": "function",
            "name": "lookup",
            "description": "look up a value",
            "parameters": {"type": "object"}
        }],
        "tool_choice": {"type": "function", "name": "lookup"}
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::Preserve,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["model"], "backend-model");
    assert_eq!(rewritten["messages"][0]["role"], "system");
    assert_eq!(rewritten["messages"][0]["content"], "be useful");
    assert_eq!(rewritten["messages"][1]["role"], "user");
    assert_eq!(rewritten["messages"][1]["content"], "hello");
    assert_eq!(rewritten["max_tokens"], 32);
    assert_eq!(rewritten["prompt_cache_key"], "tenant-a");
    assert_eq!(rewritten["stream"], true);
    assert_eq!(rewritten["tools"][0]["type"], "function");
    assert_eq!(rewritten["tools"][0]["function"]["name"], "lookup");
    assert!(rewritten["tools"][0]["function"].get("strict").is_none());
    assert_eq!(
        rewritten["tool_choice"],
        json!({"type": "function", "function": {"name": "lookup"}})
    );
    assert!(rewritten.get("input").is_none());
    assert!(rewritten.get("instructions").is_none());
    assert!(rewritten.get("max_output_tokens").is_none());
}

#[test]
fn chat_completions_request_converts_to_responses() {
    let body = json!({
        "model": "public-model",
        "messages": [
            {"role": "system", "content": "system rules"},
            {"role": "developer", "content": [{"type": "text", "text": "developer rules"}]},
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": "data:image/png;base64,AAAA",
                            "detail": "low"
                        }
                    }
                ]
            },
            {
                "role": "assistant",
                "content": "checking",
                "tool_calls": [{
                    "id": "call_lookup",
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"query\":\"tokyo\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_lookup",
                "content": "sunny"
            }
        ],
        "temperature": 0.2,
        "top_p": 0.9,
        "stream": true,
        "store": true,
        "metadata": {"tenant": "a"},
        "parallel_tool_calls": false,
        "prompt_cache_key": "tenant-a",
        "prompt_cache_retention": "24h",
        "max_completion_tokens": 42,
        "response_format": {"type": "json_object"},
        "tools": [{
            "type": "function",
            "function": {
                "name": "lookup",
                "description": "look up a value",
                "strict": true,
                "parameters": {"type": "object"}
            }
        }],
        "tool_choice": {"type": "function", "function": {"name": "lookup"}}
    });

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["model"], "backend-model");
    assert_eq!(rewritten["instructions"], "system rules\n\ndeveloper rules");
    assert_eq!(rewritten["input"][0]["role"], "user");
    assert_eq!(
        rewritten["input"][0]["content"][0],
        json!({"type": "input_text", "text": "look"})
    );
    assert_eq!(
        rewritten["input"][0]["content"][1],
        json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,AAAA",
            "detail": "low"
        })
    );
    assert_eq!(rewritten["input"][1]["role"], "assistant");
    assert_eq!(rewritten["input"][1]["content"], "checking");
    assert_eq!(rewritten["input"][2]["type"], "function_call");
    assert_eq!(rewritten["input"][2]["call_id"], "call_lookup");
    assert_eq!(rewritten["input"][2]["name"], "lookup");
    assert_eq!(rewritten["input"][2]["arguments"], "{\"query\":\"tokyo\"}");
    assert_eq!(rewritten["input"][3]["type"], "function_call_output");
    assert_eq!(rewritten["input"][3]["call_id"], "call_lookup");
    assert_eq!(rewritten["input"][3]["output"], "sunny");
    assert_eq!(rewritten["temperature"], 0.2);
    assert_eq!(rewritten["top_p"], 0.9);
    assert_eq!(rewritten["stream"], true);
    assert_eq!(rewritten["store"], true);
    assert_eq!(rewritten["metadata"]["tenant"], "a");
    assert_eq!(rewritten["parallel_tool_calls"], false);
    assert_eq!(rewritten["prompt_cache_key"], "tenant-a");
    assert_eq!(rewritten["prompt_cache_retention"], "24h");
    assert_eq!(rewritten["max_output_tokens"], 42);
    assert_eq!(rewritten["text"]["format"]["type"], "json_object");
    assert_eq!(rewritten["tools"][0]["type"], "function");
    assert_eq!(rewritten["tools"][0]["name"], "lookup");
    assert_eq!(rewritten["tools"][0]["strict"], true);
    assert_eq!(
        rewritten["tool_choice"],
        json!({"type": "function", "name": "lookup"})
    );
    assert!(rewritten.get("messages").is_none());
    assert!(rewritten.get("max_completion_tokens").is_none());
    assert!(rewritten.get("max_tokens").is_none());
}

#[test]
fn chat_completions_to_responses_applies_responses_policies() {
    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 32
    });

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::ForceFalse,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Drop,
            chat_stream_usage: ChatStreamUsagePolicy::ForceTrue,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["model"], "backend-model");
    assert_eq!(rewritten["input"][0]["content"], "hello");
    assert_eq!(rewritten["store"], false);
    assert!(rewritten.get("max_output_tokens").is_none());
    assert!(rewritten.get("stream_options").is_none());
}

#[test]
fn chat_completions_to_responses_rejects_unsupported_options() {
    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "n": 2
    });
    let error = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .expect_err("expected n > 1 to be rejected");
    assert_eq!(error.param().as_deref(), Some("n"));

    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "logprobs": true
    });
    let error = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .expect_err("expected logprobs to be rejected");
    assert_eq!(error.param().as_deref(), Some("logprobs"));

    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "top_logprobs": 1
    });
    let error = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .expect_err("expected top_logprobs to be rejected");
    assert_eq!(error.param().as_deref(), Some("top_logprobs"));
}

#[test]
fn chat_completions_to_responses_requires_json_and_messages() {
    let error = rewrite_request_body_for_mode_with_policies(
        b"model=public-model",
        Some("application/x-www-form-urlencoded"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .expect_err("expected non-json request to be rejected");
    assert_eq!(
        error.message(),
        "Chat-to-responses conversion requires a JSON request body."
    );

    let error = rewrite_request_body_for_mode_with_policies(
        br#"{"model":"public-model"}"#,
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .expect_err("expected missing messages to be rejected");
    assert_eq!(error.param().as_deref(), Some("messages"));
}

#[test]
fn native_responses_can_force_store_false_when_omitted() {
    let body = json!({
        "model": "public-model",
        "input": "hello"
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::Native,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::ForceFalse,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["model"], "backend-model");
    assert_eq!(rewritten["store"], false);
}

#[test]
fn native_responses_store_policy_preserves_explicit_store_and_chat_requests() {
    let responses_body = json!({
        "model": "public-model",
        "input": "hello",
        "store": true
    });
    let rewritten_responses = rewrite_request_body_for_mode_with_tool_schema_mode(
        responses_body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::Native,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::ForceFalse,
    )
    .unwrap();
    let rewritten_responses: Value = serde_json::from_slice(&rewritten_responses).unwrap();
    assert_eq!(rewritten_responses["store"], true);

    let chat_body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let rewritten_chat = rewrite_request_body_for_mode_with_tool_schema_mode(
        chat_body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::ForceFalse,
    )
    .unwrap();
    let rewritten_chat: Value = serde_json::from_slice(&rewritten_chat).unwrap();
    assert!(rewritten_chat.get("store").is_none());
}

#[test]
fn native_responses_can_rewrite_max_output_tokens_for_wrapper_quirks() {
    let body = json!({
        "model": "public-model",
        "input": "hello",
        "max_output_tokens": 32
    });

    let dropped = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Drop,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let dropped: Value = serde_json::from_slice(&dropped).unwrap();
    assert!(dropped.get("max_output_tokens").is_none());
    assert!(dropped.get("max_tokens").is_none());

    let renamed = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::RenameToMaxTokens,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let renamed: Value = serde_json::from_slice(&renamed).unwrap();
    assert!(renamed.get("max_output_tokens").is_none());
    assert_eq!(renamed["max_tokens"], 32);
}

#[test]
fn native_responses_max_output_tokens_policy_preserves_other_paths_and_existing_fields() {
    let responses_body = json!({
        "model": "public-model",
        "input": "hello",
        "max_output_tokens": 32,
        "max_completion_tokens": 16
    });
    let rewritten_responses = rewrite_request_body_for_mode_with_policies(
        responses_body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens:
                ResponsesMaxOutputTokensPolicy::RenameToMaxCompletionTokens,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten_responses: Value = serde_json::from_slice(&rewritten_responses).unwrap();
    assert!(rewritten_responses.get("max_output_tokens").is_none());
    assert_eq!(rewritten_responses["max_completion_tokens"], 16);

    let chat_body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "max_output_tokens": 32
    });
    let rewritten_chat = rewrite_request_body_for_mode_with_policies(
        chat_body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Drop,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten_chat: Value = serde_json::from_slice(&rewritten_chat).unwrap();
    assert_eq!(rewritten_chat["max_output_tokens"], 32);
}

#[test]
fn chat_stream_usage_policy_inserts_usage_request_for_chat_streams() {
    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true
    });

    let preserved = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let preserved: Value = serde_json::from_slice(&preserved).unwrap();
    assert!(preserved.get("stream_options").is_none());

    let inserted = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Insert,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let inserted: Value = serde_json::from_slice(&inserted).unwrap();
    assert_eq!(inserted["stream_options"]["include_usage"], true);
}

#[test]
fn chat_stream_usage_policy_preserves_client_stream_options() {
    let with_extra = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true,
        "stream_options": {
            "extra": "kept"
        }
    });

    let rewritten = rewrite_request_body_for_mode_with_policies(
        with_extra.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Insert,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
    assert_eq!(rewritten["stream_options"]["include_usage"], true);
    assert_eq!(rewritten["stream_options"]["extra"], "kept");

    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true,
        "stream_options": {
            "include_usage": false,
            "extra": "kept"
        }
    });

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Insert,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["stream_options"]["include_usage"], false);
    assert_eq!(rewritten["stream_options"]["extra"], "kept");
}

#[test]
fn chat_stream_usage_policy_force_true_overrides_client_value() {
    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true,
        "stream_options": {
            "include_usage": false,
            "extra": "kept"
        }
    });

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::ForceTrue,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["stream_options"]["include_usage"], true);
    assert_eq!(rewritten["stream_options"]["extra"], "kept");
}

#[test]
fn chat_stream_usage_policy_force_true_replaces_non_object_stream_options() {
    let body = json!({
        "model": "public-model",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true,
        "stream_options": "unexpected"
    });

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::ForceTrue,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["stream_options"]["include_usage"], true);
    assert_eq!(rewritten["stream_options"].as_object().unwrap().len(), 1);
}

#[test]
fn chat_stream_usage_policy_ignores_non_chat_and_native_responses() {
    let body = json!({
        "model": "public-model",
        "input": "hello",
        "stream": true
    });

    let responses = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::ForceTrue,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let responses: Value = serde_json::from_slice(&responses).unwrap();
    assert!(responses.get("stream_options").is_none());

    let embeddings = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/embeddings",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Insert,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let embeddings: Value = serde_json::from_slice(&embeddings).unwrap();
    assert!(embeddings.get("stream_options").is_none());

    let non_stream_chat = rewrite_request_body_for_mode_with_policies(
        json!({
            "model": "public-model",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false
        })
        .to_string()
        .as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Insert,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let non_stream_chat: Value = serde_json::from_slice(&non_stream_chat).unwrap();
    assert!(non_stream_chat.get("stream_options").is_none());
}

#[test]
fn chat_stream_usage_policy_applies_after_responses_to_chat_conversion() {
    let body = json!({
        "model": "public-model",
        "input": "hello",
        "stream": true
    });

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Insert,
        },
        &BTreeMap::new(),
        "test",
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["messages"][0]["role"], "user");
    assert_eq!(rewritten["stream_options"]["include_usage"], true);
}

#[test]
fn responses_request_preserves_tool_strictness() {
    let body = json!({
        "model": "public-model",
        "input": "hello",
        "tools": [
            {
                "type": "function",
                "name": "strict_lookup",
                "description": "strict tool",
                "strict": true,
                "parameters": {"type": "object"}
            },
            {
                "type": "function",
                "name": "loose_lookup",
                "description": "loose tool",
                "strict": false,
                "parameters": {"type": "object"}
            },
            {
                "type": "function",
                "name": "unspecified_lookup",
                "description": "unspecified strictness",
                "parameters": {"type": "object"}
            }
        ]
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::Preserve,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["tools"][0]["function"]["strict"], true);
    assert_eq!(rewritten["tools"][1]["function"]["strict"], false);
    assert!(rewritten["tools"][2]["function"].get("strict").is_none());
}

#[test]
fn llama_cpp_tool_schema_mode_sanitizes_common_schema_fragments() {
    let body = json!({
        "model": "public-model",
        "input": "hello",
        "tools": [{
            "type": "function",
            "name": "lookup",
            "description": "look up a value",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {
                        "description": "city name",
                        "anyOf": [
                            {"type": "string", "enum": ["Tokyo", "Osaka"]},
                            {"type": "null"}
                        ],
                        "default": null
                    },
                    "limit": {
                        "type": ["integer", "null"],
                        "default": 10
                    },
                    "tags": {
                        "type": "array",
                        "items": {
                            "type": ["string", "null"],
                            "default": "general"
                        }
                    }
                }
            }
        }]
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::LlamacppCompat,
        ResponsesStorePolicy::Preserve,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
    let function = &rewritten["tools"][0]["function"];

    assert_eq!(function["strict"], false);
    assert!(
        function["parameters"]["properties"]["city"]
            .get("anyOf")
            .is_none()
    );
    assert!(
        function["parameters"]["properties"]["city"]
            .get("default")
            .is_none()
    );
    assert_eq!(
        function["parameters"]["properties"]["city"]["type"],
        "string"
    );
    assert_eq!(
        function["parameters"]["properties"]["city"]["enum"],
        json!(["Tokyo", "Osaka"])
    );
    assert_eq!(
        function["parameters"]["properties"]["limit"]["type"],
        "integer"
    );
    assert!(
        function["parameters"]["properties"]["limit"]
            .get("default")
            .is_none()
    );
    assert_eq!(
        function["parameters"]["properties"]["tags"]["items"]["type"],
        "string"
    );
    assert!(
        function["parameters"]["properties"]["tags"]["items"]
            .get("default")
            .is_none()
    );
}

#[test]
fn request_inspection_detects_non_empty_tools() {
    let with_tools = inspect_request(
        json!({
            "model": "public-model",
            "input": "hello",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object"}
            }]
        })
        .to_string()
        .as_bytes(),
        Some("application/json"),
        None,
    );
    let empty_tools = inspect_request(
        br#"{"model":"public-model","tools":[]}"#,
        Some("application/json"),
        None,
    );
    let null_tools = inspect_request(
        br#"{"model":"public-model","tools":null}"#,
        Some("application/json"),
        None,
    );

    assert!(with_tools.has_tools);
    assert!(!empty_tools.has_tools);
    assert!(!null_tools.has_tools);
}

#[test]
fn request_inspection_detects_stream_usage_request() {
    let requested = inspect_request(
        br#"{"model":"public-model","stream":true,"stream_options":{"include_usage":true}}"#,
        Some("application/json"),
        None,
    );
    let omitted = inspect_request(
        br#"{"model":"public-model","stream":true}"#,
        Some("application/json"),
        None,
    );
    let explicit_false = inspect_request(
        br#"{"model":"public-model","stream":true,"stream_options":{"include_usage":false}}"#,
        Some("application/json"),
        None,
    );

    assert!(requested.stream_usage_requested);
    assert!(!omitted.stream_usage_requested);
    assert!(!explicit_false.stream_usage_requested);
}

#[test]
fn responses_request_ignores_null_tools_and_collapses_text_parts() {
    let body = json!({
        "model": "public-model",
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "Hi"},
                {"type": "text", "text": "!"}
            ]
        }],
        "tools": null
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::Preserve,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["messages"][0]["role"], "user");
    assert_eq!(rewritten["messages"][0]["content"], "Hi!");
    assert!(rewritten.get("tools").is_none());
}

#[test]
fn responses_request_converts_image_parts_to_chat_content() {
    let body = json!({
        "model": "public-model",
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "look"},
                {
                    "type": "input_image",
                    "image_url": "data:image/png;base64,AAAA",
                    "detail": "low"
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/png;base64,BBBB",
                        "detail": "high"
                    }
                }
            ]
        }]
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::Preserve,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
    let content = rewritten["messages"][0]["content"].as_array().unwrap();

    assert_eq!(content[0], json!({"type": "text", "text": "look"}));
    assert_eq!(
        content[1],
        json!({
            "type": "image_url",
            "image_url": {
                "url": "data:image/png;base64,AAAA",
                "detail": "low"
            }
        })
    );
    assert_eq!(
        content[2],
        json!({
            "type": "image_url",
            "image_url": {
                "url": "data:image/png;base64,BBBB",
                "detail": "high"
            }
        })
    );
}

#[test]
fn responses_request_converts_function_call_items_to_tool_messages() {
    let body = json!({
        "model": "public-model",
        "input": [
            {"role": "user", "content": "What time is it?"},
            {
                "type": "function_call",
                "call_id": "call_time",
                "name": "get_time",
                "arguments": "{}"
            },
            {
                "type": "function_call",
                "call_id": "call_weather",
                "name": "get_weather",
                "arguments": "{\"city\":\"Tokyo\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_time",
                "output": "10:00 AM"
            },
            {
                "type": "function_call_output",
                "call_id": "call_weather",
                "output": "Sunny"
            },
            {"role": "user", "content": "Thanks!"}
        ]
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
        ResponsesStorePolicy::Preserve,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
    let messages = rewritten["messages"].as_array().unwrap();

    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "");
    assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_time");
    assert_eq!(
        messages[1]["tool_calls"][1]["function"]["arguments"],
        "{\"city\":\"Tokyo\"}"
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_time");
    assert_eq!(messages[2]["content"], "10:00 AM");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_weather");
    assert_eq!(messages[3]["content"], "Sunny");
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(messages[4]["content"], "Thanks!");
}

#[test]
fn native_responses_rejects_function_calls_without_matching_outputs() {
    let body = json!({
        "model": "public-model",
        "input": [
            {"role": "user", "content": "What time is it?"},
            {
                "type": "function_call",
                "call_id": "call_time",
                "name": "get_time",
                "arguments": "{}"
            },
            {"role": "user", "content": "Thanks!"}
        ]
    });

    let error = rewrite_request_body_for_mode_with_policies(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &BTreeMap::new(),
        "test",
    )
    .expect_err("expected missing tool output to be rejected");

    assert_eq!(
        error.message(),
        "No tool output found for function call call_time."
    );
    assert_eq!(error.param().as_deref(), Some("input"));
}

#[test]
fn chat_completion_response_converts_to_responses_shape() {
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 123,
        "model": "backend-model",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "hello"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 4},
            "completion_tokens": 3,
            "total_tokens": 13
        }
    });

    let (rewritten, usage) = rewrite_response_body(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        Some("public-model"),
        RequestMode::ResponsesViaChatCompletions,
    );
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["id"], "resp_chatcmpl-1");
    assert_eq!(rewritten["object"], "response");
    assert_eq!(rewritten["created_at"], 123);
    assert_eq!(rewritten["model"], "public-model");
    assert_eq!(rewritten["output_text"], "hello");
    assert_eq!(rewritten["output"][0]["type"], "message");
    assert_eq!(rewritten["output"][0]["content"][0]["text"], "hello");
    assert_eq!(rewritten["usage"]["input_tokens"], 10);
    assert_eq!(
        rewritten["usage"]["input_tokens_details"]["cached_tokens"],
        4
    );
    assert_eq!(rewritten["usage"]["output_tokens"], 3);
    assert_eq!(rewritten["usage"]["total_tokens"], 13);
    assert_eq!(usage.input, 10);
    assert_eq!(usage.cached_input, 4);
    assert_eq!(usage.output, 3);
    assert_eq!(usage.total, 13);
}

#[test]
fn responses_response_converts_to_chat_completion_shape() {
    let body = json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 123,
        "model": "backend-model",
        "status": "completed",
        "output": [
            {
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "hello"
                }]
            },
            {
                "type": "function_call",
                "call_id": "call_lookup",
                "name": "lookup",
                "arguments": "{\"query\":\"tokyo\"}"
            }
        ],
        "usage": {
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 4},
            "output_tokens": 3,
            "total_tokens": 13
        }
    });

    let (rewritten, usage) = rewrite_response_body(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        Some("public-model"),
        RequestMode::ChatCompletionsViaResponses,
    );
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["id"], "chatcmpl_resp_1");
    assert_eq!(rewritten["object"], "chat.completion");
    assert_eq!(rewritten["created"], 123);
    assert_eq!(rewritten["model"], "public-model");
    assert_eq!(rewritten["choices"][0]["message"]["role"], "assistant");
    assert_eq!(rewritten["choices"][0]["message"]["content"], "hello");
    assert_eq!(
        rewritten["choices"][0]["message"]["tool_calls"][0],
        json!({
            "id": "call_lookup",
            "type": "function",
            "function": {
                "name": "lookup",
                "arguments": "{\"query\":\"tokyo\"}"
            }
        })
    );
    assert_eq!(rewritten["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(rewritten["usage"]["prompt_tokens"], 10);
    assert_eq!(
        rewritten["usage"]["prompt_tokens_details"]["cached_tokens"],
        4
    );
    assert_eq!(rewritten["usage"]["completion_tokens"], 3);
    assert_eq!(rewritten["usage"]["total_tokens"], 13);
    assert_eq!(usage.input, 10);
    assert_eq!(usage.cached_input, 4);
    assert_eq!(usage.output, 3);
    assert_eq!(usage.total, 13);
}

#[test]
fn responses_response_missing_usage_defaults_to_zero_chat_usage() {
    let body = json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 123,
        "model": "backend-model",
        "output": []
    });

    let (rewritten, usage) = rewrite_response_body(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        Some("public-model"),
        RequestMode::ChatCompletionsViaResponses,
    );
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();

    assert_eq!(rewritten["usage"]["prompt_tokens"], 0);
    assert_eq!(rewritten["usage"]["completion_tokens"], 0);
    assert_eq!(rewritten["usage"]["total_tokens"], 0);
    assert_eq!(usage.input, 0);
    assert_eq!(usage.output, 0);
    assert_eq!(usage.total, 0);
}

#[test]
fn native_json_response_adds_missing_total_tokens() {
    let responses_body = json!({
        "id": "resp_1",
        "object": "response",
        "model": "backend-model",
        "output": [],
        "usage": {
            "input_tokens": 8,
            "output_tokens": 5
        }
    });
    let (rewritten, usage) = rewrite_response_body(
        responses_body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        Some("public-model"),
        RequestMode::Native,
    );
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
    assert_eq!(rewritten["usage"]["input_tokens"], 8);
    assert_eq!(rewritten["usage"]["output_tokens"], 5);
    assert_eq!(rewritten["usage"]["total_tokens"], 13);
    assert_eq!(usage.input, 8);
    assert_eq!(usage.output, 5);
    assert_eq!(usage.total, 13);

    let chat_body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "backend-model",
        "choices": [],
        "usage": {
            "prompt_tokens": 3,
            "completion_tokens": 4
        }
    });
    let (rewritten, usage) = rewrite_response_body(
        chat_body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        Some("public-model"),
        RequestMode::Native,
    );
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
    assert_eq!(rewritten["usage"]["prompt_tokens"], 3);
    assert_eq!(rewritten["usage"]["completion_tokens"], 4);
    assert_eq!(rewritten["usage"]["total_tokens"], 7);
    assert_eq!(usage.input, 3);
    assert_eq!(usage.output, 4);
    assert_eq!(usage.total, 7);
}

#[test]
fn extract_usage_observation_collects_usage_keys() {
    let observation = extract_usage_observation(&json!({
        "outer": {
            "usage": {
                "prompt_tokens": 4,
                "prompt_tokens_details": {
                    "cached_tokens": 1
                },
                "completion_tokens": 2,
                "total_tokens": 6
            }
        },
        "inner": [
            {
                "usage": {
                    "input_tokens": 3,
                    "input_tokens_details": {
                        "cached_tokens": 2
                    },
                    "output_tokens": 5
                }
            }
        ]
    }));

    assert_eq!(observation.diagnostics.usage_object_count, 2);
    assert!(observation.diagnostics.usage_keys.contains("prompt_tokens"));
    assert!(observation.diagnostics.usage_keys.contains("input_tokens"));
    assert!(
        observation
            .diagnostics
            .usage_keys
            .contains("completion_tokens")
    );
    assert!(observation.diagnostics.usage_keys.contains("output_tokens"));
    assert!(
        observation
            .diagnostics
            .usage_keys
            .contains("prompt_tokens_details")
    );
    assert!(
        observation
            .diagnostics
            .usage_keys
            .contains("input_tokens_details")
    );
}

#[test]
fn native_stream_response_adds_missing_total_tokens() {
    let mut normalizer = SseNormalizer::new_with_usage_visibility(None, None, true);
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "response",
        "type": "response.completed",
        "choices": [],
        "usage": {
            "prompt_tokens": 6,
            "completion_tokens": 2
        }
    });
    let output =
        normalizer.push(format!("event: response.completed\ndata: {chunk}\n\n").as_bytes());
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("\"total_tokens\":8"));
    assert_eq!(normalizer.usage.input, 6);
    assert_eq!(normalizer.usage.output, 2);
    assert_eq!(normalizer.usage.total, 8);
    assert_eq!(normalizer.diagnostics.usage_object_count, 1);
    assert!(normalizer.diagnostics.usage_keys.contains("prompt_tokens"));
    assert!(
        normalizer
            .diagnostics
            .usage_keys
            .contains("completion_tokens")
    );
    assert!(
        normalizer
            .diagnostics
            .event_names
            .contains("response.completed")
    );
    assert!(
        normalizer
            .diagnostics
            .usage_event_names
            .contains("response.completed")
    );
}

#[test]
fn chat_stream_usage_filter_collects_metrics_without_forwarding_usage_chunk() {
    let mut normalizer = SseNormalizer::new_with_usage_visibility(None, None, false);
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "model": "public-model",
        "choices": [],
        "usage": {
            "prompt_tokens": 6,
            "prompt_tokens_details": {
                "cached_tokens": 1
            },
            "completion_tokens": 2,
            "total_tokens": 8
        }
    });
    let output = normalizer.push(format!("data: {chunk}\n\n").as_bytes());
    let output = String::from_utf8(output).unwrap();

    assert!(!output.contains("\"usage\""), "body={output}");
    assert!(!output.contains("\"prompt_tokens\""), "body={output}");
    assert_eq!(normalizer.usage.input, 6);
    assert_eq!(normalizer.usage.cached_input, 1);
    assert_eq!(normalizer.usage.output, 2);
    assert_eq!(normalizer.usage.total, 8);
    assert_eq!(normalizer.diagnostics.usage_object_count, 1);
    assert!(
        normalizer
            .diagnostics
            .usage_event_names
            .contains("chat.completion.chunk")
    );
}

#[test]
fn chat_completion_stream_converts_to_responses_events() {
    let mut normalizer = ResponsesSseNormalizer::new(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
    );
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "backend-model",
        "choices": [{
            "delta": {"content": "hello"},
            "finish_reason": null
        }]
    });

    let mut output = normalizer.push(format!("data: {chunk}\n\n").as_bytes());
    output.extend(normalizer.push(b"data: [DONE]\n\n"));
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("event: response.created"));
    assert!(output.contains("event: response.output_text.delta"));
    assert!(output.contains("\"delta\":\"hello\""));
    assert!(output.contains("event: response.completed"));
    assert!(output.contains("\"model\":\"public-model\""));
    assert!(output.contains("data: [DONE]"));
}

#[test]
fn chat_completion_stream_converts_tool_call_events_to_responses_events() {
    let mut normalizer = ResponsesSseNormalizer::new(None, None);
    let name_chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "public-model",
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {"name": "get_time"}
                }]
            },
            "finish_reason": null
        }]
    });
    let arguments_chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "public-model",
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": "{\"timezone\":\"Asia/Tokyo\"}"}
                }]
            },
            "finish_reason": null
        }]
    });

    let mut output = normalizer.push(format!("data: {name_chunk}\n\n").as_bytes());
    output.extend(normalizer.push(format!("data: {arguments_chunk}\n\n").as_bytes()));
    output.extend(normalizer.push(b"data: [DONE]\n\n"));
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("event: response.output_item.added"));
    assert!(output.contains("\"call_id\":\"call_0\""));
    assert!(output.contains("\"name\":\"get_time\""));
    assert!(output.contains("event: response.function_call_arguments.delta"));
    assert!(output.contains("event: response.function_call_arguments.done"));
    assert!(output.contains("\"arguments\":\"{\\\"timezone\\\":\\\"Asia/Tokyo\\\"}\""));
    assert!(output.contains("event: response.completed"));
}

#[test]
fn responses_stream_converts_text_deltas_to_chat_completion_chunks() {
    let mut normalizer = ChatCompletionsSseNormalizer::new_with_usage_visibility(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        true,
    );
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 123,
            "model": "backend-model"
        }
    });
    let delta = json!({
        "type": "response.output_text.delta",
        "delta": "hello"
    });
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 123,
            "model": "backend-model",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 8,
                "input_tokens_details": {"cached_tokens": 2},
                "output_tokens": 5,
                "total_tokens": 13
            }
        }
    });

    let mut output =
        normalizer.push(format!("event: response.created\ndata: {created}\n\n").as_bytes());
    output.extend(normalizer.push(format!("data: {delta}\n\n").as_bytes()));
    output.extend(normalizer.push(format!("data: {completed}\n\n").as_bytes()));
    output.extend(normalizer.finish());
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("\"object\":\"chat.completion.chunk\""));
    assert!(output.contains("\"model\":\"public-model\""));
    assert!(output.contains("\"role\":\"assistant\""));
    assert!(output.contains("\"content\":\"hello\""));
    assert!(output.contains("\"finish_reason\":\"stop\""));
    assert!(output.contains("\"usage\":null"));
    assert!(output.contains("\"choices\":[]"));
    assert!(output.contains("\"prompt_tokens\":8"));
    assert!(output.contains("\"cached_tokens\":2"));
    assert!(output.contains("\"completion_tokens\":5"));
    assert!(output.contains("data: [DONE]"));
    assert_eq!(normalizer.usage.input, 8);
    assert_eq!(normalizer.usage.cached_input, 2);
    assert_eq!(normalizer.usage.output, 5);
    assert_eq!(normalizer.usage.total, 13);
}

#[test]
fn responses_stream_usage_filter_collects_metrics_without_forwarding_chat_usage() {
    let mut normalizer = ChatCompletionsSseNormalizer::new_with_usage_visibility(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        false,
    );
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 123,
            "model": "backend-model"
        }
    });
    let delta = json!({
        "type": "response.output_text.delta",
        "delta": "hello"
    });
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 123,
            "model": "backend-model",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 8,
                "input_tokens_details": {"cached_tokens": 2},
                "output_tokens": 5,
                "total_tokens": 13
            }
        }
    });

    let mut output =
        normalizer.push(format!("event: response.created\ndata: {created}\n\n").as_bytes());
    output.extend(normalizer.push(format!("data: {delta}\n\n").as_bytes()));
    output.extend(normalizer.push(format!("data: {completed}\n\n").as_bytes()));
    output.extend(normalizer.finish());
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("\"object\":\"chat.completion.chunk\""));
    assert!(output.contains("\"model\":\"public-model\""));
    assert!(output.contains("\"content\":\"hello\""));
    assert!(output.contains("\"finish_reason\":\"stop\""));
    assert!(!output.contains("\"usage\""), "body={output}");
    assert!(!output.contains("\"prompt_tokens\""), "body={output}");
    assert!(output.contains("data: [DONE]"));
    assert_eq!(normalizer.usage.input, 8);
    assert_eq!(normalizer.usage.cached_input, 2);
    assert_eq!(normalizer.usage.output, 5);
    assert_eq!(normalizer.usage.total, 13);
    assert_eq!(normalizer.diagnostics.usage_object_count, 1);
    assert!(
        normalizer
            .diagnostics
            .usage_event_names
            .contains("response.completed")
    );
}

#[test]
fn responses_stream_incomplete_maps_to_length_finish_reason() {
    let mut normalizer = ChatCompletionsSseNormalizer::new_with_usage_visibility(None, None, true);
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "created_at": 123,
            "model": "public-model"
        }
    });
    let delta = json!({
        "type": "response.output_text.delta",
        "delta": "partial"
    });
    let incomplete = json!({
        "type": "response.incomplete",
        "response": {
            "id": "resp_1",
            "created_at": 123,
            "model": "public-model",
            "status": "incomplete",
            "incomplete_details": {
                "reason": "max_output_tokens"
            },
            "output": [],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 2,
                "total_tokens": 3
            }
        }
    });

    let mut output = normalizer.push(format!("data: {created}\n\n").as_bytes());
    output.extend(normalizer.push(format!("data: {delta}\n\n").as_bytes()));
    output.extend(normalizer.push(format!("data: {incomplete}\n\n").as_bytes()));
    output.extend(normalizer.finish());
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("\"content\":\"partial\""));
    assert!(output.contains("\"finish_reason\":\"length\""));
    assert!(
        !output.contains("\"finish_reason\":\"stop\""),
        "body={output}"
    );
    assert!(output.contains("\"total_tokens\":3"));
    assert!(output.contains("data: [DONE]"));
}

#[test]
fn responses_stream_failed_emits_sanitized_chat_error() {
    let mut normalizer = ChatCompletionsSseNormalizer::new_with_usage_visibility(None, None, true);
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "created_at": 123,
            "model": "public-model"
        }
    });
    let failed = json!({
        "type": "response.failed",
        "response": {
            "id": "resp_1",
            "created_at": 123,
            "model": "public-model",
            "status": "failed",
            "error": {
                "message": "private backend failure detail",
                "code": "backend_specific_failure"
            }
        }
    });

    let mut output = normalizer.push(format!("data: {created}\n\n").as_bytes());
    output.extend(normalizer.push(format!("data: {failed}\n\n").as_bytes()));
    output.extend(normalizer.finish());
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("\"error\""), "body={output}");
    assert!(output.contains("The selected model could not complete the request."));
    assert!(output.contains("\"code\":\"upstream_error\""));
    assert!(
        !output.contains("private backend failure detail"),
        "body={output}"
    );
    assert!(
        !output.contains("backend_specific_failure"),
        "body={output}"
    );
    assert!(
        !output.contains("\"finish_reason\":\"stop\""),
        "body={output}"
    );
    assert!(output.contains("data: [DONE]"));
}

#[test]
fn responses_stream_converts_function_call_deltas_to_chat_completion_chunks() {
    let mut normalizer = ChatCompletionsSseNormalizer::new_with_usage_visibility(None, None, true);
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "created_at": 123,
            "model": "public-model"
        }
    });
    let added = json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "id": "call_lookup",
            "type": "function_call",
            "call_id": "call_lookup",
            "name": "lookup",
            "arguments": ""
        }
    });
    let arguments = json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 0,
        "item_id": "call_lookup",
        "call_id": "call_lookup",
        "delta": "{\"query\":\"tokyo\"}"
    });
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "created_at": 123,
            "model": "public-model",
            "status": "completed",
            "output": [{
                "id": "call_lookup",
                "type": "function_call",
                "call_id": "call_lookup",
                "name": "lookup",
                "arguments": "{\"query\":\"tokyo\"}"
            }],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 2
            }
        }
    });

    let mut output = normalizer.push(format!("data: {created}\n\n").as_bytes());
    output.extend(normalizer.push(format!("data: {added}\n\n").as_bytes()));
    output.extend(normalizer.push(format!("data: {arguments}\n\n").as_bytes()));
    output.extend(normalizer.push(format!("data: {completed}\n\n").as_bytes()));
    output.extend(normalizer.finish());
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("\"tool_calls\""));
    assert!(output.contains("\"id\":\"call_lookup\""));
    assert!(output.contains("\"name\":\"lookup\""));
    assert!(output.contains("\"arguments\":\"{\\\"query\\\":\\\"tokyo\\\"}\""));
    assert!(output.contains("\"finish_reason\":\"tool_calls\""));
    assert!(output.contains("\"total_tokens\":3"));
    assert!(output.contains("data: [DONE]"));
}

#[test]
fn chat_completion_stream_finishes_response_when_done_marker_is_missing() {
    // Regression: a stream that ends with content but never emits
    // "data: [DONE]" must still emit the closing finish_reason chunk
    // and the [DONE] terminator on finish() so the client does not
    // hang waiting for the stream terminator.
    let mut normalizer = ChatCompletionsSseNormalizer::new_with_usage_visibility(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        false,
    );
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "backend-model",
        "choices": [{
            "delta": {"content": "hello"},
            "finish_reason": null
        }]
    });

    let mut output = normalizer.push(format!("data: {chunk}\n\n").as_bytes());
    output.extend(normalizer.finish());
    let output = String::from_utf8(output).unwrap();

    assert!(
        output.contains("\"finish_reason\":\"stop\""),
        "expected finish_reason emitted on finish() even without [DONE], got: {output}"
    );
    assert!(output.contains("data: [DONE]"));
}

#[test]
fn responses_stream_finishes_response_when_done_marker_is_missing() {
    // Regression: a Responses stream that ends with content but never
    // emits "data: [DONE]" must still emit the response.completed
    // event so the client gets a final state.
    let mut normalizer = ResponsesSseNormalizer::new(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
    );
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "backend-model",
        "choices": [{
            "delta": {"content": "hello"},
            "finish_reason": "stop"
        }]
    });

    let mut output = normalizer.push(format!("data: {chunk}\n\n").as_bytes());
    output.extend(normalizer.finish());
    let output = String::from_utf8(output).unwrap();

    assert!(
        output.contains("event: response.completed"),
        "expected response.completed on finish() even without [DONE], got: {output}"
    );
}

#[test]
fn extra_body_merges_arbitrary_keys_into_rewritten_native_request() {
    let body = br#"{
        "model": "public-model",
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let mut extra_body = BTreeMap::new();
    extra_body.insert("reasoning_split".to_owned(), toml::Value::Boolean(true));
    extra_body.insert("temperature".to_owned(), toml::Value::Float(0.7));
    extra_body.insert(
        "stop".to_owned(),
        toml::Value::Array(vec![toml::Value::String("END".to_owned())]),
    );

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body,
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &extra_body,
        "public=public-model",
    )
    .unwrap();
    let json: Value = serde_json::from_slice(&rewritten).unwrap();

    // onair's own rewrite wins: model is the backend name.
    assert_eq!(json["model"], "backend-model");
    // extra_body fields are merged in.
    assert_eq!(json["reasoning_split"], true);
    assert_eq!(json["temperature"], 0.7);
    assert_eq!(json["stop"][0], "END");
    // Untouched fields pass through.
    assert_eq!(json["messages"][0]["role"], "user");
}

#[test]
fn extra_body_drops_protected_keys_with_warn() {
    let body = br#"{
        "model": "public-model",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false
    }"#;
    let mut extra_body = BTreeMap::new();
    // All these are onair-managed and must be dropped, not merged.
    for key in [
        "model",
        "stream",
        "messages",
        "input",
        "tools",
        "tool_choice",
        "store",
        "max_output_tokens",
        "max_tokens",
        "max_completion_tokens",
        "stream_options",
    ] {
        extra_body.insert(key.to_owned(), toml::Value::String("attacker".to_owned()));
    }
    // A non-protected key should still merge.
    extra_body.insert("reasoning_split".to_owned(), toml::Value::Boolean(true));

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body,
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::Native,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &extra_body,
        "public=public-model",
    )
    .unwrap();
    let json: Value = serde_json::from_slice(&rewritten).unwrap();

    // Protected keys were dropped; onair's rewrite is what we see.
    assert_eq!(json["model"], "backend-model");
    assert_eq!(json["stream"], false);
    assert_eq!(json["messages"][0]["role"], "user");
    // The non-protected key landed.
    assert_eq!(json["reasoning_split"], true);
    // None of the protected keys ever carried the attacker's "attacker" string.
    for key in [
        "tools",
        "tool_choice",
        "store",
        "max_output_tokens",
        "max_tokens",
        "max_completion_tokens",
        "stream_options",
    ] {
        assert!(
            json.get(key).is_none(),
            "protected key {key} should have been dropped, found: {}",
            json.get(key).unwrap()
        );
    }
}

#[test]
fn extra_body_merges_into_responses_to_chat_compat_path() {
    let body = br#"{
        "model": "public-model",
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "hi"}]}
        ]
    }"#;
    let mut extra_body = BTreeMap::new();
    extra_body.insert(
        "chat_template_kwargs".to_owned(),
        toml::Value::Table(toml::map::Map::from_iter([(
            "enable_thinking".to_owned(),
            toml::Value::Boolean(true),
        )])),
    );

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body,
        Some("application/json"),
        Some("backend-model"),
        "/v1/responses",
        RequestMode::ResponsesViaChatCompletions,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &extra_body,
        "public=public-model",
    )
    .unwrap();
    let json: Value = serde_json::from_slice(&rewritten).unwrap();

    // Compat path produced a chat shape; extra_body still merged.
    assert_eq!(json["model"], "backend-model");
    assert_eq!(json["messages"][0]["role"], "user");
    assert_eq!(json["chat_template_kwargs"]["enable_thinking"], true);
}

#[test]
fn extra_body_merges_into_chat_to_responses_compat_path() {
    let body = br#"{
        "model": "public-model",
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let mut extra_body = BTreeMap::new();
    extra_body.insert(
        "metadata".to_owned(),
        toml::Value::Table(toml::map::Map::from_iter([(
            "user_id".to_owned(),
            toml::Value::String("u-1".to_owned()),
        )])),
    );

    let rewritten = rewrite_request_body_for_mode_with_policies(
        body,
        Some("application/json"),
        Some("backend-model"),
        "/v1/chat/completions",
        RequestMode::ChatCompletionsViaResponses,
        &RequestRewritePolicies {
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        },
        &extra_body,
        "public=public-model",
    )
    .unwrap();
    let json: Value = serde_json::from_slice(&rewritten).unwrap();

    // Compat path produced a responses shape; extra_body still merged.
    assert_eq!(json["model"], "backend-model");
    assert_eq!(json["metadata"]["user_id"], "u-1");
}

// ---------------------------------------------------------------------------
// SseStrategy regression tests
// ---------------------------------------------------------------------------

#[test]
fn sse_strategy_native_passthrough_matches_sse_normalizer() {
    let mut direct = SseNormalizer::new_with_usage_visibility(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        true,
    );
    let mut strategy = SseStrategy::new(
        RequestMode::Native,
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        true,
    );

    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "backend-model",
        "choices": [{"delta": {"content": "hello"}, "finish_reason": null}]
    });
    let input = format!("data: {chunk}\n\n");

    let out_direct = direct.push(input.as_bytes());
    let out_strategy = strategy.push(input.as_bytes());

    assert_eq!(out_direct, out_strategy);
    assert_eq!(direct.usage, strategy.usage());
    assert_eq!(
        direct.diagnostics.usage_object_count,
        strategy.diagnostics().usage_object_count
    );
}

#[test]
fn sse_strategy_responses_to_chat_matches_responses_normalizer() {
    let mut direct = ResponsesSseNormalizer::new(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
    );
    let mut strategy = SseStrategy::new(
        RequestMode::ResponsesViaChatCompletions,
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        false,
    );

    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "backend-model",
        "choices": [{"delta": {"content": "hello"}, "finish_reason": null}]
    });
    let input = format!("data: {chunk}\n\n");

    let mut out_direct = direct.push(input.as_bytes());
    out_direct.extend(direct.push(b"data: [DONE]\n\n"));
    let mut out_strategy = strategy.push(input.as_bytes());
    out_strategy.extend(strategy.push(b"data: [DONE]\n\n"));

    assert_eq!(out_direct, out_strategy);
    assert_eq!(direct.usage, strategy.usage());
}

#[test]
fn sse_strategy_chat_to_responses_matches_chat_completions_normalizer() {
    let mut direct = ChatCompletionsSseNormalizer::new_with_usage_visibility(
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        true,
    );
    let mut strategy = SseStrategy::new(
        RequestMode::ChatCompletionsViaResponses,
        Some("backend-model".to_owned()),
        Some("public-model".to_owned()),
        true,
    );

    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 123,
            "model": "backend-model"
        }
    });
    let delta = json!({
        "type": "response.output_text.delta",
        "delta": "hello"
    });

    let mut out_direct =
        direct.push(format!("event: response.created\ndata: {created}\n\n").as_bytes());
    out_direct.extend(direct.push(format!("data: {delta}\n\n").as_bytes()));
    out_direct.extend(direct.finish());

    let mut out_strategy =
        strategy.push(format!("event: response.created\ndata: {created}\n\n").as_bytes());
    out_strategy.extend(strategy.push(format!("data: {delta}\n\n").as_bytes()));
    out_strategy.extend(strategy.finish());

    assert_eq!(out_direct, out_strategy);
    assert_eq!(direct.usage, strategy.usage());
}

#[test]
fn sse_strategy_usage_gating_filters_usage_chunk() {
    // emit_usage_to_client = false → usage-only chunks should be stripped
    let mut strategy = SseStrategy::new(
        RequestMode::Native,
        None,
        Some("public-model".to_owned()),
        false,
    );
    let mut direct = SseNormalizer::new_with_usage_visibility(None, None, false);

    let usage_chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "model": "public-model",
        "choices": [],
        "usage": {
            "prompt_tokens": 6,
            "prompt_tokens_details": {"cached_tokens": 1},
            "completion_tokens": 2,
            "total_tokens": 8
        }
    });
    let input = format!("data: {usage_chunk}\n\n");

    let out_direct = direct.push(input.as_bytes());
    let out_strategy = strategy.push(input.as_bytes());

    assert_eq!(out_direct, out_strategy);
    assert!(!String::from_utf8_lossy(&out_strategy).contains("\"usage\""));
    assert_eq!(direct.usage, strategy.usage());
}

#[test]
fn sse_strategy_finish_emits_closing_events() {
    // Each variant should emit closing events on finish()
    // --- Native ---
    let mut native = SseStrategy::new(
        RequestMode::Native,
        None,
        Some("public-model".to_owned()),
        true,
    );
    let tail_native = native.finish();
    // Native finish is a no-op (empty) when there is no pending data.
    assert!(tail_native.is_empty());

    // --- Responses ---
    // Use finish_reason: null so the response is NOT completed during push();
    // finish() must emit the response.completed closure.
    let mut responses = SseStrategy::new(
        RequestMode::ResponsesViaChatCompletions,
        None,
        Some("public-model".to_owned()),
        false,
    );
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 123,
        "model": "backend-model",
        "choices": [{"delta": {"content": "hi"}, "finish_reason": null}]
    });
    responses.push(format!("data: {chunk}\n\n").as_bytes());
    let tail_responses = responses.finish();
    let tail_str = String::from_utf8_lossy(&tail_responses);
    assert!(
        tail_str.contains("event: response.completed"),
        "Responses finish must emit response.completed, got: {tail_str}"
    );

    // --- ChatCompletions ---
    let mut chat = SseStrategy::new(
        RequestMode::ChatCompletionsViaResponses,
        None,
        Some("public-model".to_owned()),
        true,
    );
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_1",
            "created_at": 123,
            "model": "public-model"
        }
    });
    let delta = json!({
        "type": "response.output_text.delta",
        "delta": "hello"
    });
    chat.push(format!("data: {created}\n\n").as_bytes());
    chat.push(format!("data: {delta}\n\n").as_bytes());
    let tail_chat = chat.finish();
    let tail_str = String::from_utf8_lossy(&tail_chat);
    assert!(
        tail_str.contains("\"finish_reason\":\"stop\""),
        "ChatCompletions finish must emit finish_reason, got: {tail_str}"
    );
    assert!(
        tail_str.contains("data: [DONE]"),
        "ChatCompletions finish must emit [DONE], got: {tail_str}"
    );
}

#[test]
fn sse_strategy_clear_usage_and_diagnostics_resets_counters() {
    let mut strategy = SseStrategy::new(
        RequestMode::Native,
        None,
        Some("public-model".to_owned()),
        true,
    );

    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "model": "public-model",
        "choices": [],
        "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
    });
    strategy.push(format!("data: {chunk}\n\n").as_bytes());

    assert!(strategy.usage().input > 0);
    assert!(strategy.diagnostics().usage_object_count > 0);

    strategy.clear_usage();
    strategy.clear_diagnostics();

    assert_eq!(strategy.usage().input, 0);
    assert_eq!(strategy.usage().output, 0);
    assert_eq!(strategy.usage().total, 0);
    assert_eq!(strategy.diagnostics().usage_object_count, 0);
}
