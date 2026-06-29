use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, FORWARDED, LOCATION};
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::*;
use onair_core::ContextSizeCache;
use onair_core::config::{
    ChatStreamUsagePolicy, Config, ContextLengthSpec, DebugCaptureConfig, DebugCaptureMode,
    HealthConfig, InspectorConfig, InspectorPersistenceConfig, ResolvedBackend, ResolvedClient,
    ResolvedRoute, ResponsesMaxOutputTokensPolicy, ResponsesStorePolicy, RouteBackendBinding,
    RouteKey, RoutingConfig, RoutingStrategy, ServerConfig, TelemetryConfig, ToolSchemaMode,
};
use onair_obs::observe::InspectorOutcome;
use onair_obs::observe::inspector_persisted_count;

const CLIENT_KEY: &str = "sk-test";
const PUBLIC_MODEL: &str = "gpt-public";
const BACKEND_MODEL: &str = "backend-private";

#[tokio::test]
async fn extra_body_merges_into_upstream_request_and_preserves_model_rewrite() {
    // Operator wants upstream-specific toggles the proxy does not
    // understand. extra_body on the route should flow into the
    // upstream request body alongside onair's own rewrite.
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![extra_body_test_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    // onair's model rewrite wins: the public model is replaced with
    // the backend's configured backend model id.
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    // Operator-supplied extra_body fields landed alongside the
    // client message.
    assert_eq!(captured[0]["reasoning_split"], true);
    assert_eq!(captured[0]["temperature"], 0.7);
    // The protected "stream" override in extra_body did NOT
    // override the client's stream=false — onair dropped it.
    assert_eq!(captured[0]["stream"], false);

    backend.abort();
}

#[tokio::test]
async fn extra_body_backend_defaults_are_inherited_by_route() {
    // Per-backend extra_body defaults flow into the route even when
    // the route itself sets no extra_body.
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![extra_body_backend_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    // Backend-level field landed.
    assert_eq!(captured[0]["chat_template_kwargs"]["enable_thinking"], true);
    // The model rewrite still happened.
    assert_eq!(captured[0]["model"], BACKEND_MODEL);

    backend.abort();
}

fn extra_body_test_endpoint(id: &str, base_url: String) -> TestEndpoint {
    let backend = ResolvedBackend {
        id: id.to_owned(),
        base_url,
        api_key: None,
        timeout: std::time::Duration::from_secs(5),
        supports: btree_set(["chat", "streaming"]),
        tool_schema_mode: ToolSchemaMode::Preserve,
        responses_store: ResponsesStorePolicy::Preserve,
        responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
        chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        weight: 1,
        extra_body: BTreeMap::new(),
        expose_backend_errors: false,
        stream_capture: false,
    };

    let route = ResolvedRoute {
        key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
        expose: btree_set(["chat"]),
        context_length: ContextLengthSpec::None,
        tool_schema_mode: ToolSchemaMode::Preserve,
        responses_store: ResponsesStorePolicy::Preserve,
        responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
        chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        backends: vec![RouteBackendBinding {
            backend_id: id.to_owned(),
            backend_model: BACKEND_MODEL.to_owned(),
        }],
        extra_body: {
            let mut m = BTreeMap::new();
            m.insert(
                "reasoning_split".to_owned(),
                onair_core::TomlValue::Boolean(true),
            );
            m.insert("temperature".to_owned(), onair_core::TomlValue::Float(0.7));
            // Protected key: should be dropped with a warn.
            m.insert("stream".to_owned(), onair_core::TomlValue::Boolean(true));
            m
        },
        request_headers: BTreeMap::new(),
        expose_backend_errors: false,
        stream_capture: false,
        anthropic_max_tokens: None,
    };
    TestEndpoint { backend, route }
}

fn extra_body_backend_endpoint(id: &str, base_url: String) -> TestEndpoint {
    let backend_extra = {
        let mut m = BTreeMap::new();
        m.insert(
            "chat_template_kwargs".to_owned(),
            onair_core::TomlValue::Table(onair_core::TomlTable::from_iter([(
                "enable_thinking".to_owned(),
                onair_core::TomlValue::Boolean(true),
            )])),
        );
        m
    };
    let backend = ResolvedBackend {
        id: id.to_owned(),
        base_url,
        api_key: None,
        timeout: std::time::Duration::from_secs(5),
        supports: btree_set(["chat", "streaming"]),
        tool_schema_mode: ToolSchemaMode::Preserve,
        responses_store: ResponsesStorePolicy::Preserve,
        responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
        chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        weight: 1,
        extra_body: backend_extra.clone(),
        expose_backend_errors: false,
        stream_capture: false,
    };

    let route = ResolvedRoute {
        key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
        expose: btree_set(["chat"]),
        context_length: ContextLengthSpec::None,
        tool_schema_mode: ToolSchemaMode::Preserve,
        responses_store: ResponsesStorePolicy::Preserve,
        responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
        chat_stream_usage: ChatStreamUsagePolicy::Preserve,
        backends: vec![RouteBackendBinding {
            backend_id: id.to_owned(),
            backend_model: BACKEND_MODEL.to_owned(),
        }],
        // Route has no extra_body, so it inherits the backend's.
        extra_body: backend_extra,
        request_headers: BTreeMap::new(),
        expose_backend_errors: false,
        stream_capture: false,
        anthropic_max_tokens: None,
    };
    TestEndpoint { backend, route }
}

#[tokio::test]
async fn responses_forwards_prompt_cache_fields_and_rewrites_model() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello",
                "prompt_cache_key": "tenant-a:prefix",
                "prompt_cache_retention": "24h"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["model"], PUBLIC_MODEL);
    assert_eq!(
        response_body["usage"]["input_tokens_details"]["cached_tokens"],
        7
    );
    assert_eq!(response_body["usage"]["total_tokens"], 16);

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["prompt_cache_key"], "tenant-a:prefix");
    assert_eq!(captured[0]["prompt_cache_retention"], "24h");

    backend.abort();
}

#[tokio::test]
async fn responses_translates_to_chat_completions_for_chat_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_chat_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "instructions": "answer briefly",
                "input": [
                    {
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "hello"}
                        ]
                    }
                ],
                "max_output_tokens": 16,
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object"}
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["object"], "response");
    assert_eq!(response_body["model"], PUBLIC_MODEL);
    assert_eq!(response_body["output_text"], "chat response");
    assert_eq!(response_body["usage"]["input_tokens"], 11);
    assert_eq!(response_body["usage"]["output_tokens"], 5);
    assert_eq!(response_body["usage"]["total_tokens"], 16);

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["messages"][0]["role"], "system");
    assert_eq!(captured[0]["messages"][0]["content"], "answer briefly");
    assert_eq!(captured[0]["messages"][1]["role"], "user");
    assert_eq!(captured[0]["messages"][1]["content"], "hello");
    assert_eq!(captured[0]["max_tokens"], 16);
    assert_eq!(captured[0]["tools"][0]["type"], "function");
    assert_eq!(captured[0]["tools"][0]["function"]["name"], "lookup");
    assert!(captured[0].get("input").is_none());
    assert!(captured[0].get("max_output_tokens").is_none());

    backend.abort();
}

#[tokio::test]
async fn chat_stream_usage_policy_inserts_upstream_include_usage() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_chat_backend("backend-a", backend.base_url());
    backend_config.backend.chat_stream_usage = ChatStreamUsagePolicy::Insert;
    backend_config.route.chat_stream_usage = ChatStreamUsagePolicy::Insert;
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state.clone());

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let response_body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(response_body.contains("\"content\":\"chat response\""));
    assert!(!response_body.contains("\"usage\""), "body={response_body}");

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["stream"], true);
    assert_eq!(captured[0]["stream_options"]["include_usage"], true);

    backend.abort();
}

#[tokio::test]
async fn chat_stream_usage_requested_by_client_is_forwarded_to_client() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_chat_backend("backend-a", backend.base_url())],
    );
    let app = router(state.clone());

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true,
                "stream_options": {
                    "include_usage": true
                }
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    drop(state);
    let response_body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(response_body.contains("\"usage\""), "body={response_body}");
    assert!(response_body.contains("\"prompt_tokens\":11"));
    assert!(response_body.contains("\"completion_tokens\":5"));
    assert!(response_body.contains("\"total_tokens\":16"));

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["stream_options"]["include_usage"], true);

    backend.abort();
}

#[tokio::test]
async fn responses_chat_compat_can_sanitize_tool_schema_for_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_chat_backend("backend-a", backend.base_url());
    backend_config.backend.tool_schema_mode = ToolSchemaMode::LlamacppCompat;
    backend_config.route.tool_schema_mode = ToolSchemaMode::LlamacppCompat;
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello",
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "anyOf": [
                                    {"type": "string"},
                                    {"type": "null"}
                                ],
                                "default": null
                            }
                        }
                    }
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    let query_schema = &captured[0]["tools"][0]["function"]["parameters"]["properties"]["query"];
    assert_eq!(query_schema["type"], "string");
    assert!(query_schema.get("anyOf").is_none());
    assert!(query_schema.get("default").is_none());

    backend.abort();
}

#[tokio::test]
async fn responses_native_capability_uses_native_backend_path() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_chat_backend("backend-a", backend.base_url());
    backend_config.backend.supports = btree_set(["responses", "chat", "streaming", "tools"]);
    backend_config.route.expose = btree_set(["responses", "chat", "tools"]);
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "instructions": "answer briefly",
                "input": [{"role": "user", "content": "hello"}],
                "max_output_tokens": 16,
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object"}
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["object"], "response");
    assert_eq!(response_body["model"], PUBLIC_MODEL);
    assert_eq!(response_body["usage"]["input_tokens"], 13);

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["instructions"], "answer briefly");
    assert_eq!(captured[0]["input"][0]["role"], "user");
    assert_eq!(captured[0]["input"][0]["content"], "hello");
    assert_eq!(captured[0]["max_output_tokens"], 16);
    assert_eq!(captured[0]["tools"][0]["type"], "function");
    assert!(captured[0].get("messages").is_none());
    assert!(captured[0].get("max_tokens").is_none());

    backend.abort();
}

#[tokio::test]
async fn chat_completions_translates_to_responses_for_responses_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_backend("backend-a", backend.base_url());
    backend_config.backend.supports.insert("tools".to_owned());
    backend_config.route.expose = btree_set(["chat_completions_via_responses", "tools"]);
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [
                    {"role": "system", "content": "answer briefly"},
                    {"role": "user", "content": "hello"}
                ],
                "max_tokens": 16,
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "parameters": {"type": "object"}
                    }
                }],
                "tool_choice": {"type": "function", "function": {"name": "lookup"}}
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["object"], "chat.completion");
    assert_eq!(response_body["model"], PUBLIC_MODEL);
    assert_eq!(
        response_body["choices"][0]["message"]["content"],
        "responses response"
    );
    assert_eq!(response_body["usage"]["prompt_tokens"], 13);
    assert_eq!(response_body["usage"]["completion_tokens"], 3);
    assert_eq!(response_body["usage"]["total_tokens"], 16);

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["instructions"], "answer briefly");
    assert_eq!(captured[0]["input"][0]["role"], "user");
    assert_eq!(captured[0]["input"][0]["content"], "hello");
    assert_eq!(captured[0]["max_output_tokens"], 16);
    assert_eq!(captured[0]["tools"][0]["type"], "function");
    assert_eq!(captured[0]["tools"][0]["name"], "lookup");
    assert_eq!(
        captured[0]["tool_choice"],
        json!({"type": "function", "name": "lookup"})
    );
    assert!(captured[0].get("messages").is_none());
    assert!(captured[0].get("max_tokens").is_none());

    backend.abort();
}

#[tokio::test]
async fn chat_completions_stream_translates_to_responses_stream_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_backend("backend-a", backend.base_url());
    backend_config.route.expose = btree_set(["chat_completions_via_responses"]);
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state.clone());

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("\"object\":\"chat.completion.chunk\""));
    assert!(body.contains("\"model\":\"gpt-public\""));
    assert!(body.contains("\"content\":\"responses response\""));
    assert!(body.contains("\"finish_reason\":\"stop\""));
    assert!(!body.contains("\"usage\""), "body={body}");
    assert!(!body.contains("\"prompt_tokens\""), "body={body}");
    assert!(!body.contains("\"completion_tokens\""), "body={body}");
    assert!(body.contains("data: [DONE]"));

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["stream"], true);
    assert_eq!(captured[0]["input"][0]["role"], "user");
    assert_eq!(captured[0]["input"][0]["content"], "hello");
    assert!(captured[0].get("messages").is_none());

    backend.abort();
}

#[tokio::test]
async fn chat_completions_stream_translates_mislabeled_responses_stream() {
    let backend = TestBackend::spawn_json_labeled_stream("backend-a").await;
    let mut backend_config = test_backend("backend-a", backend.base_url());
    backend_config.route.expose = btree_set(["chat_completions_via_responses"]);
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state.clone());

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"))
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    drop(state);
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("\"object\":\"chat.completion.chunk\""));
    assert!(body.contains("\"choices\""));
    assert!(body.contains("\"content\":\"responses response\""));
    assert!(body.contains("\"finish_reason\":\"stop\""));
    assert!(
        !body.contains("\"type\":\"response.created\""),
        "body={body}"
    );
    assert!(!body.contains("event: response.created"), "body={body}");
    assert!(body.contains("data: [DONE]"));

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["stream"], true);
    assert!(captured[0].get("messages").is_none());

    backend.abort();
}

#[tokio::test]
async fn native_responses_route_can_force_store_false() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_backend("backend-a", backend.base_url());
    backend_config.backend.responses_store = ResponsesStorePolicy::ForceFalse;
    backend_config.route.responses_store = ResponsesStorePolicy::ForceFalse;
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["store"], false);

    backend.abort();
}

#[tokio::test]
async fn native_responses_route_can_drop_max_output_tokens() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_backend("backend-a", backend.base_url());
    backend_config.backend.responses_max_output_tokens = ResponsesMaxOutputTokensPolicy::Drop;
    backend_config.route.responses_max_output_tokens = ResponsesMaxOutputTokensPolicy::Drop;
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello",
                "max_output_tokens": 16
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert!(captured[0].get("max_output_tokens").is_none());
    assert!(captured[0].get("max_tokens").is_none());

    backend.abort();
}

#[tokio::test]
async fn native_responses_rejects_orphan_function_calls_before_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
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
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response_body = json_body(response).await;
    assert_eq!(
        response_body["error"]["message"],
        "No tool output found for function call call_time."
    );
    assert_eq!(response_body["error"]["param"], "input");
    assert_eq!(backend.hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn tool_request_requires_tool_capable_route() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_chat_backend("backend-a", backend.base_url());
    backend_config.backend.supports.remove("tools");
    backend_config.route.expose.remove("tools");
    let state = test_state(RoutingStrategy::Priority, vec![backend_config]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello",
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object"}
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response_body = json_body(response).await;
    assert_eq!(
        response_body["error"]["message"],
        "The selected model does not support tool calling."
    );
    assert_eq!(response_body["error"]["param"], "tools");
    assert_eq!(backend.hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn responses_full_compat_payload_translates_to_chat_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_chat_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "instructions": "You are Kai, a helpful Discord bot.",
                "input": [
                    {"role": "user", "content": "What time is it in Tokyo?"},
                    {
                        "type": "function_call",
                        "call_id": "call_abc",
                        "name": "get_time",
                        "arguments": "{\"timezone\":\"Asia/Tokyo\"}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call_abc",
                        "output": "18:30 JST"
                    },
                    {"role": "user", "content": "Thanks!"}
                ],
                "stream": false,
                "max_output_tokens": 10000,
                "tools": [{
                    "type": "function",
                    "name": "get_time",
                    "description": "Get current time",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "timezone": {"type": "string"}
                        }
                    }
                }]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["object"], "response");
    assert_eq!(response_body["model"], PUBLIC_MODEL);
    assert_eq!(response_body["usage"]["total_tokens"], 16);

    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    let payload = &captured[0];
    assert_eq!(payload["model"], BACKEND_MODEL);
    assert_eq!(payload["stream"], false);
    assert_eq!(payload["max_tokens"], 10000);
    assert!(payload.get("input").is_none());
    assert!(payload.get("instructions").is_none());
    assert!(payload.get("max_output_tokens").is_none());

    assert_eq!(
        payload["messages"],
        json!([
            {
                "role": "system",
                "content": "You are Kai, a helpful Discord bot."
            },
            {
                "role": "user",
                "content": "What time is it in Tokyo?"
            },
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {
                        "name": "get_time",
                        "arguments": "{\"timezone\":\"Asia/Tokyo\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_abc",
                "content": "18:30 JST"
            },
            {
                "role": "user",
                "content": "Thanks!"
            }
        ])
    );
    assert_eq!(payload["tools"][0]["type"], "function");
    assert_eq!(payload["tools"][0]["function"]["name"], "get_time");
    assert_eq!(
        payload["tools"][0]["function"]["description"],
        "Get current time"
    );
    assert_eq!(
        payload["tools"][0]["function"]["parameters"]["properties"]["timezone"]["type"],
        "string"
    );
    assert!(payload["tools"][0]["function"].get("strict").is_none());

    backend.abort();
}

#[tokio::test]
async fn shutdown_signal_cancels_buffered_upstream_wait() {
    let backend = TestBackend::spawn_slow("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
    );
    let app = router(state.clone());
    let request = json_request(
        "/v1/responses",
        json!({
            "model": PUBLIC_MODEL,
            "input": "hello"
        }),
    );
    let pending = tokio::spawn(async move { app.oneshot(request).await.unwrap() });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    state.shutdown.send(true).unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .expect("request did not stop after shutdown signal")
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(backend.hits(), 1);
    backend.abort();
}

#[tokio::test]
async fn sticky_routing_reuses_backend_for_same_prompt_cache_key() {
    let backend_a = TestBackend::spawn("backend-a").await;
    let backend_b = TestBackend::spawn("backend-b").await;
    let state = test_state(
        RoutingStrategy::Sticky,
        vec![
            test_backend("backend-a", backend_a.base_url()),
            test_backend("backend-b", backend_b.base_url()),
        ],
    );
    let app = router(state);

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(json_request(
                "/v1/responses",
                json!({
                    "model": PUBLIC_MODEL,
                    "input": "same prefix",
                    "prompt_cache_key": "cache-affinity-key"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let hits_a = backend_a.hits();
    let hits_b = backend_b.hits();
    assert!(
        (hits_a == 2 && hits_b == 0) || (hits_a == 0 && hits_b == 2),
        "expected sticky routing to select one backend twice, got a={hits_a}, b={hits_b}"
    );

    backend_a.abort();
    backend_b.abort();
}

#[tokio::test]
async fn disallowed_model_returns_404_without_calling_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": "not-allowed",
                "input": "hello"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(backend.hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn model_required_endpoint_without_model_returns_400_without_calling_backend() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "input": "hello"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response_body = json_body(response).await;
    assert_eq!(response_body["error"]["param"], "model");
    assert_eq!(backend.hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn debug_capture_writes_inbound_and_upstream_request_bodies() {
    let backend = TestBackend::spawn("backend-a").await;
    let capture_dir = temp_capture_dir("request-bodies");
    let state = test_state_with_debug_capture(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        DebugCaptureConfig {
            enabled: true,
            mode: DebugCaptureMode::All,
            directory: capture_dir.clone(),
        },
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses?metadata=keep",
            json!({
                "model": PUBLIC_MODEL,
                "input": "long context goes here"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let _ = json_body(response).await;

    let capture_path = only_capture_path(&capture_dir);
    let inbound_body: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("inbound.body")).unwrap()).unwrap();
    let upstream_body: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("upstream.body")).unwrap())
            .unwrap();
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("metadata.json")).unwrap())
            .unwrap();

    assert_eq!(inbound_body["model"], PUBLIC_MODEL);
    assert_eq!(upstream_body["model"], BACKEND_MODEL);
    assert_eq!(metadata["identity"], "dev");
    assert_eq!(metadata["route"], "responses");
    assert_eq!(metadata["backend"], "backend-a");
    assert_eq!(metadata["client_query"], "metadata=keep");
    assert_eq!(metadata["outcome"]["kind"], "success");
    assert_eq!(metadata["outcome"]["upstream_status"], 200);

    backend.abort();
    std::fs::remove_dir_all(capture_dir).unwrap();
}

#[tokio::test]
async fn debug_capture_writes_upstream_error_response_body() {
    let backend = TestBackend::spawn_error("backend-a").await;
    let capture_dir = temp_capture_dir("error-response");
    let state = test_state_with_debug_capture(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        DebugCaptureConfig {
            enabled: true,
            mode: DebugCaptureMode::All,
            directory: capture_dir.clone(),
        },
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "please fail"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response_body = json_body(response).await;
    assert_eq!(
        response_body["error"]["message"],
        "The request could not be completed by the selected model."
    );

    let capture_path = only_capture_path(&capture_dir);
    let upstream_error_body: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("upstream_error.body")).unwrap())
            .unwrap();
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("metadata.json")).unwrap())
            .unwrap();

    assert_eq!(
        upstream_error_body["error"]["message"],
        "upstream failure detail"
    );
    assert_eq!(metadata["upstream_error_status"], 400);
    assert_eq!(metadata["upstream_error_content_type"], "application/json");
    assert_eq!(metadata["upstream_error_body_truncated"], false);
    assert!(metadata["upstream_error_body_bytes"].as_u64().unwrap() > 0);
    assert_eq!(metadata["outcome"]["kind"], "upstream_non_success");
    assert_eq!(metadata["outcome"]["upstream_status"], 400);

    backend.abort();
    std::fs::remove_dir_all(capture_dir).unwrap();
}

#[tokio::test]
async fn debug_capture_failures_mode_skips_success_and_captures_upstream_error() {
    let capture_dir = temp_capture_dir("failures-only");
    let success_backend = TestBackend::spawn("backend-a").await;
    let success_state = test_state_with_debug_capture(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", success_backend.base_url())],
        DebugCaptureConfig {
            enabled: true,
            mode: DebugCaptureMode::Failures,
            directory: capture_dir.clone(),
        },
    );
    let success_app = router(success_state);

    let success_response = success_app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "this should not be captured"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(success_response.status(), StatusCode::OK);
    let _ = json_body(success_response).await;
    assert!(!capture_dir.exists());
    success_backend.abort();

    let error_backend = TestBackend::spawn_error("backend-a").await;
    let error_state = test_state_with_debug_capture(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", error_backend.base_url())],
        DebugCaptureConfig {
            enabled: true,
            mode: DebugCaptureMode::Failures,
            directory: capture_dir.clone(),
        },
    );
    let error_app = router(error_state);

    let error_response = error_app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "please fail"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(error_response.status(), StatusCode::BAD_REQUEST);

    let capture_path = only_capture_path(&capture_dir);
    let upstream_error_body: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("upstream_error.body")).unwrap())
            .unwrap();
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("metadata.json")).unwrap())
            .unwrap();

    assert_eq!(
        upstream_error_body["error"]["message"],
        "upstream failure detail"
    );
    assert_eq!(metadata["mode"], "failures");
    assert_eq!(metadata["outcome"]["kind"], "upstream_non_success");
    assert!(metadata["id"].as_str().unwrap().len() > 10);

    error_backend.abort();
    std::fs::remove_dir_all(capture_dir).unwrap();
}

#[tokio::test]
async fn debug_capture_records_stream_usage_diagnostics() {
    let backend = TestBackend::spawn("backend-a").await;
    let capture_dir = temp_capture_dir("stream-usage");
    let mut backend_config = test_chat_backend("backend-a", backend.base_url());
    backend_config.backend.chat_stream_usage = ChatStreamUsagePolicy::Insert;
    backend_config.route.chat_stream_usage = ChatStreamUsagePolicy::Insert;
    let state = test_state_with_debug_capture(
        RoutingStrategy::Priority,
        vec![backend_config],
        DebugCaptureConfig {
            enabled: true,
            mode: DebugCaptureMode::All,
            directory: capture_dir.clone(),
        },
    );
    let app = router(state.clone());

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    drop(state);
    let response_body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!response_body.contains("\"usage\""), "body={response_body}");

    let capture_path = only_capture_path(&capture_dir);
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(capture_path.join("metadata.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["outcome"]["kind"], "stream_completed");
    assert_eq!(metadata["outcome"]["input_tokens"], 11);
    assert_eq!(metadata["outcome"]["cached_input_tokens"], 2);
    assert_eq!(metadata["outcome"]["output_tokens"], 5);
    assert_eq!(metadata["stream_usage"]["usage_object_count"], 1);
    let usage_keys = metadata["stream_usage"]["usage_keys"].as_array().unwrap();
    assert!(usage_keys.iter().any(|key| key == "prompt_tokens"));
    assert!(usage_keys.iter().any(|key| key == "completion_tokens"));
    assert!(usage_keys.iter().any(|key| key == "total_tokens"));
    let event_names = metadata["stream_usage"]["event_names"].as_array().unwrap();
    assert!(
        event_names
            .iter()
            .any(|event| event == "chat.completion.chunk")
    );
    let usage_event_names = metadata["stream_usage"]["usage_event_names"]
        .as_array()
        .unwrap();
    assert!(
        usage_event_names
            .iter()
            .any(|event| event == "chat.completion.chunk")
    );

    backend.abort();
    std::fs::remove_dir_all(capture_dir).unwrap();
}

#[tokio::test]
async fn inspector_records_completed_requests_and_serves_details() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses?metadata=keep",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello inspector"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = json_body(response).await;

    let response = app
        .clone()
        .oneshot(inspector_get("/_onair/inspector/requests"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let requests = body.as_array().unwrap();
    assert_eq!(requests.len(), 1);
    let record = &requests[0];
    assert_eq!(record["route"], "responses");
    assert_eq!(record["backend"], "backend-a");
    assert_eq!(record["peer_addr"], "127.0.0.1:55432");
    assert_eq!(record["effective_client_addr"], "127.0.0.1:55432");
    assert_eq!(record["outcome"]["kind"], "completed");
    assert!(record["timeline"]["backend_forward_start_us"].is_number());
    let attempts = record["backend_attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["attempt"], 1);
    assert_eq!(attempts[0]["backend"], "backend-a");
    assert_eq!(attempts[0]["outcome"], "completed");
    assert_eq!(attempts[0]["status"], 200);
    assert_eq!(attempts[0]["upstream_status"], 200);
    assert!(attempts[0]["started_us"].is_number());
    assert!(attempts[0]["ended_us"].as_u64() >= attempts[0]["started_us"].as_u64());
    assert!(attempts[0]["backend_forward_start_us"].is_number());
    assert!(attempts[0]["backend_headers_received_us"].is_number());
    assert!(attempts[0]["backend_body_first_chunk_us"].is_number());
    assert!(attempts[0]["backend_body_complete_us"].is_number());

    let record_id = record["record_id"].as_str().unwrap();
    let response = app
        .oneshot(inspector_get(&format!(
            "/_onair/inspector/requests/{record_id}"
        )))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail = json_body(response).await;
    assert_eq!(detail["record_id"], record_id);
    assert_eq!(detail["debug_capture_id"], serde_json::Value::Null);

    backend.abort();
}

#[tokio::test]
async fn inspector_request_list_limits_to_latest_records() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    for marker in ["first", "second"] {
        let response = app
            .clone()
            .oneshot(json_request(
                &format!("/v1/responses?marker={marker}"),
                json!({
                    "model": PUBLIC_MODEL,
                    "input": marker
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = json_body(response).await;
    }

    let response = app
        .oneshot(inspector_get("/_onair/inspector/requests?limit=1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let requests = body.as_array().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["query"], "marker=second");

    backend.abort();
}

#[tokio::test]
async fn inspector_persistence_restores_retained_records_after_restart() {
    let database_path = temp_database_path("app");
    let backend = TestBackend::spawn("backend-a").await;
    let inspector = InspectorConfig {
        enabled: true,
        retention_requests: 16,
        allow_remote: false,
        persistence: InspectorPersistenceConfig {
            enabled: true,
            path: Some(database_path.clone()),
            ..InspectorPersistenceConfig::default()
        },
    };
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        inspector.clone(),
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses?persist=1",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello persisted inspector"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = json_body(response).await;
    wait_for_persisted_count(&database_path, 1).await;

    let restored_state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        inspector,
    );
    let restored_app = router(restored_state);
    let records = json_body(
        restored_app
            .oneshot(inspector_get("/_onair/inspector/requests"))
            .await
            .unwrap(),
    )
    .await;
    let requests = records.as_array().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["query"], "persist=1");
    assert_eq!(requests[0]["backend"], "backend-a");

    backend.abort();
}

#[tokio::test]
async fn inspector_record_appears_before_request_completes() {
    let backend = TestBackend::spawn_slow("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let request_handle = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(json_request(
                "/v1/responses?inflight=1",
                json!({
                    "model": PUBLIC_MODEL,
                    "input": "hello live inspector"
                }),
            ))
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let response = app
        .clone()
        .oneshot(inspector_get("/_onair/inspector/requests"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let requests = body.as_array().unwrap();
    assert_eq!(requests.len(), 1);
    let record = &requests[0];
    assert_eq!(record["query"], "inflight=1");
    assert_eq!(record["outcome"]["kind"], "in_flight");
    assert_eq!(record["status"], 0);

    let _ = request_handle.await;
    backend.abort();
}

#[tokio::test]
async fn inspector_sse_reports_in_flight_record() {
    let backend = TestBackend::spawn_slow("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let sse_request = Request::builder()
        .method(Method::GET)
        .uri("/_onair/inspector/events")
        .extension(ConnectInfo(
            "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    let sse_response = app.clone().oneshot(sse_request).await.unwrap();
    assert_eq!(sse_response.status(), StatusCode::OK);
    let snapshot = sse_response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        snapshot.contains("text/event-stream"),
        "content-type: {snapshot}"
    );
    drop(sse_response);

    let request_handle = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(json_request(
                "/v1/responses?sse=1",
                json!({
                    "model": PUBLIC_MODEL,
                    "input": "hello live sse"
                }),
            ))
            .await
        }
    });
    let mut saw_inflight = false;
    let poll_app = app.clone();
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let body = json_body(
            poll_app
                .clone()
                .oneshot(inspector_get("/_onair/inspector/requests"))
                .await
                .unwrap(),
        )
        .await;
        let requests = body.as_array().unwrap();
        if requests
            .iter()
            .any(|r| r["query"] == "sse=1" && r["outcome"]["kind"] == "in_flight")
        {
            saw_inflight = true;
            break;
        }
    }
    assert!(saw_inflight, "polling never saw the in-flight record");
    let _ = request_handle.await;
    backend.abort();
}

#[tokio::test]
async fn inspector_preflight_failure_replaces_live_record_without_duplication() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses?preflight=deny&model=nonexistent-model",
            json!({
                "model": "nonexistent-model",
                "input": "hello preflight"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let _ = json_body(response).await;

    let response = app
        .clone()
        .oneshot(inspector_get("/_onair/inspector/requests"))
        .await
        .unwrap();
    let body = json_body(response).await;
    let requests = body.as_array().unwrap();
    assert_eq!(requests.len(), 1, "expected one record, not a duplicate");
    let record = &requests[0];
    assert_eq!(record["query"], "preflight=deny&model=nonexistent-model");
    let outcome_kind = record["outcome"]["kind"].as_str().unwrap_or("");
    assert_eq!(outcome_kind, "preflight");

    backend.abort();
}

#[tokio::test]
async fn inspector_auth_preflight_failure_emits_auth_stage_record() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let unauth_request = Request::builder()
        .method(Method::POST)
        .uri("/v1/responses?preflight=auth")
        .header(CONTENT_TYPE, "application/json")
        .extension(ConnectInfo(
            "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello auth preflight"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(unauth_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let _ = json_body(response).await;

    let response = app
        .clone()
        .oneshot(inspector_get("/_onair/inspector/requests"))
        .await
        .unwrap();
    let body = json_body(response).await;
    let requests = body.as_array().unwrap();
    assert_eq!(requests.len(), 1, "expected one record, not a duplicate");
    let record = &requests[0];
    assert_eq!(record["query"], "preflight=auth");
    assert_eq!(record["outcome"]["kind"], "preflight");
    assert_eq!(record["outcome"]["stage"], "auth");
    assert_eq!(record["status"], 401);
    assert_eq!(record["identity"], "unknown");

    backend.abort();
}

#[tokio::test]
async fn inspector_sse_event_stream_emits_in_flight_record_for_active_request() {
    let backend = TestBackend::spawn_slow("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let sse_request = Request::builder()
        .method(Method::GET)
        .uri("/_onair/inspector/events")
        .extension(ConnectInfo(
            "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::empty())
        .unwrap();
    let sse_response = app.clone().oneshot(sse_request).await.unwrap();
    assert_eq!(sse_response.status(), StatusCode::OK);
    assert!(
        sse_response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .contains("text/event-stream")
    );

    let consumer = tokio::spawn(async move {
        let mut body = sse_response.into_body();
        let mut buffer = String::new();
        loop {
            let frame = match body.frame().await {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => return Err(format!("sse body error: {error}")),
                None => return Err("sse body closed before in-flight event".to_owned()),
            };
            if let Some(data) = frame.data_ref() {
                buffer.push_str(std::str::from_utf8(data).map_err(|e| e.to_string())?);
            }
            while let Some(split) = buffer.find("\n\n") {
                let event: String = buffer.drain(..split + 2).collect();
                let mut event_name = String::new();
                let mut data = String::new();
                for line in event.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        event_name = rest.to_owned();
                    } else if let Some(rest) = line.strip_prefix("data: ") {
                        data.push_str(rest);
                        data.push('\n');
                    }
                }
                if event_name == "request" {
                    let parsed: Value = serde_json::from_str(data.trim_end())
                        .map_err(|e| format!("bad sse json: {e}"))?;
                    if parsed["query"] == "sse=1" && parsed["outcome"]["kind"] == "in_flight" {
                        return Ok(());
                    }
                }
            }
        }
    });

    let request_handle = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(json_request(
                "/v1/responses?sse=1",
                json!({
                    "model": PUBLIC_MODEL,
                    "input": "hello live sse consumer"
                }),
            ))
            .await
        }
    });

    let outcome = tokio::time::timeout(Duration::from_secs(5), consumer)
        .await
        .expect("SSE consumer did not see an in-flight event within 5s")
        .expect("SSE consumer task panicked");
    outcome.expect("SSE consumer did not return Ok");
    let _ = request_handle.await;
    backend.abort();
}

#[tokio::test]
async fn inspector_in_flight_record_persisted_as_interrupted_on_app_state_drop() {
    let database_path = temp_database_path("app-drop");
    let backend = TestBackend::spawn_slow("backend-a").await;
    let inspector = InspectorConfig {
        enabled: true,
        retention_requests: 16,
        allow_remote: false,
        persistence: InspectorPersistenceConfig {
            enabled: true,
            path: Some(database_path.clone()),
            ..InspectorPersistenceConfig::default()
        },
    };
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        inspector,
    );
    let app = router(state.clone());

    let request_handle = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(json_request(
                "/v1/responses?app-drop=1",
                json!({
                    "model": PUBLIC_MODEL,
                    "input": "hello app drop"
                }),
            ))
            .await
        }
    });

    let mut saw_inflight = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let body = json_body(
            app.clone()
                .oneshot(inspector_get("/_onair/inspector/requests"))
                .await
                .unwrap(),
        )
        .await;
        let requests = body.as_array().unwrap();
        if requests
            .iter()
            .any(|r| r["query"] == "app-drop=1" && r["outcome"]["kind"] == "in_flight")
        {
            saw_inflight = true;
            break;
        }
    }
    assert!(
        saw_inflight,
        "polling never saw the in-flight record for app-drop=1"
    );

    request_handle.abort();
    let _ = request_handle.await;

    drop(app);
    drop(state);

    wait_for_persisted_count(&database_path, 1).await;

    let restored_state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            persistence: InspectorPersistenceConfig {
                enabled: true,
                path: Some(database_path.clone()),
                ..InspectorPersistenceConfig::default()
            },
        },
    );
    let restored_app = router(restored_state);
    let body = json_body(
        restored_app
            .oneshot(inspector_get("/_onair/inspector/requests"))
            .await
            .unwrap(),
    )
    .await;
    let requests = body.as_array().unwrap();
    let record = requests
        .iter()
        .find(|r| r["query"] == "app-drop=1")
        .expect("interrupted record should be persisted");
    assert_eq!(record["outcome"]["kind"], "interrupted");
    assert_eq!(record["status"], 503);
    assert_eq!(record["error_kind"], "interrupted");

    backend.abort();
}

#[tokio::test]
async fn operator_endpoints_return_sanitized_snapshots() {
    let backend = TestBackend::spawn("backend-a").await;
    let mut backend_config = test_backend("backend-a", backend.base_url());
    backend_config.backend.api_key = Some("backend-secret".to_owned());
    let state = test_state_with_inspector(
        RoutingStrategy::Sticky,
        vec![backend_config],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let config_response = app
        .clone()
        .oneshot(inspector_get("/_onair/operator/config"))
        .await
        .unwrap();
    assert_eq!(config_response.status(), StatusCode::OK);
    let config_body = json_body(config_response).await;
    let config_text = config_body.to_string();
    assert!(!config_text.contains(CLIENT_KEY));
    assert!(!config_text.contains("backend-secret"));
    assert_eq!(config_body["routing"]["strategy"], "sticky");
    assert_eq!(config_body["clients"][0]["id"], "dev");
    assert_eq!(config_body["backends"][0]["api_key_configured"], true);

    let models_response = app
        .clone()
        .oneshot(inspector_get("/_onair/operator/models"))
        .await
        .unwrap();
    assert_eq!(models_response.status(), StatusCode::OK);
    let models_body = json_body(models_response).await;
    assert_eq!(models_body["public_models"][0]["public"], PUBLIC_MODEL);
    assert_eq!(
        models_body["public_models"][0]["routes"][0]["backend_model"],
        BACKEND_MODEL
    );

    let runtime_response = app
        .oneshot(inspector_get("/_onair/operator/runtime"))
        .await
        .unwrap();
    assert_eq!(runtime_response.status(), StatusCode::OK);
    let runtime_body = json_body(runtime_response).await;
    assert_eq!(runtime_body["clients"], 1);
    assert_eq!(runtime_body["backends"], 1);
    assert_eq!(runtime_body["public_models"], 1);
    assert!(runtime_body["uptime_ms"].is_number());

    backend.abort();
}

#[tokio::test]
async fn operator_health_tracks_backend_successes() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let initial = json_body(
        app.clone()
            .oneshot(inspector_get("/_onair/operator/health"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(initial["backends"][0]["backend"], "backend-a");
    assert_eq!(initial["backends"][0]["status"], "unknown");

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello health"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = json_body(response).await;

    let health = json_body(
        app.oneshot(inspector_get("/_onair/operator/health"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(health["backends"][0]["status"], "healthy");
    assert_eq!(health["backends"][0]["successes"], 1);
    assert_eq!(health["backends"][0]["failures"], 0);
    assert_eq!(health["backends"][0]["traffic_successes"], 1);
    assert_eq!(health["backends"][0]["probe_successes"], 0);
    assert_eq!(health["backends"][0]["last_status"], 200);
    assert!(health["backends"][0]["last_latency_ms"].is_number());

    backend.abort();
}

#[tokio::test]
async fn operator_health_tracks_backend_failures() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", format!("http://{address}"))],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello failure"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let health = json_body(
        app.oneshot(inspector_get("/_onair/operator/health"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(health["backends"][0]["status"], "degraded");
    assert_eq!(health["backends"][0]["successes"], 0);
    assert_eq!(health["backends"][0]["failures"], 1);
    assert_eq!(health["backends"][0]["traffic_failures"], 1);
    assert_eq!(health["backends"][0]["probe_failures"], 0);
    assert_eq!(health["backends"][0]["consecutive_failures"], 1);
    assert_eq!(health["backends"][0]["last_status"], 502);
    assert!(health["backends"][0]["last_error_kind"].is_string());
}

#[tokio::test]
async fn active_health_probe_marks_backend_healthy() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state_with_inspector_and_health(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
        HealthConfig {
            active: true,
            interval_ms: 25,
            timeout_ms: 500,
            path: "/v1/models".to_owned(),
        },
    );
    let app = router(state);

    wait_for_backend_health(&app, "healthy").await;
    let health = json_body(
        app.oneshot(inspector_get("/_onair/operator/health"))
            .await
            .unwrap(),
    )
    .await;
    assert!(health["backends"][0]["probe_successes"].as_u64().unwrap() >= 1);
    assert_eq!(health["backends"][0]["traffic_successes"], 0);
    assert_eq!(health["backends"][0]["last_source"], "probe");

    backend.abort();
}

#[tokio::test]
async fn backend_redirects_are_not_followed() {
    let backend = RedirectBackend::spawn().await;
    let state = test_state_with_inspector_and_health(
        RoutingStrategy::Priority,
        vec![test_backend("backend-a", backend.base_url())],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
        HealthConfig::default(),
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello redirect"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let _ = json_body(response).await;
    assert_eq!(backend.leak_hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn send_failure_falls_back_before_response_commit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let fallback = TestBackend::spawn("backend-b").await;
    let state = test_state_with_inspector_and_health(
        RoutingStrategy::Priority,
        vec![
            test_backend("backend-a", format!("http://{address}")),
            test_backend("backend-b", fallback.base_url()),
        ],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
        HealthConfig::default(),
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello fallback"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = json_body(response).await;
    assert_eq!(fallback.hits(), 1);

    let records = json_body(
        app.clone()
            .oneshot(inspector_get("/_onair/inspector/requests?limit=1"))
            .await
            .unwrap(),
    )
    .await;
    let record = &records.as_array().unwrap()[0];
    assert_eq!(record["backend"], "backend-b");
    assert_eq!(record["outcome"]["kind"], "completed");
    assert_eq!(record["retried_attempts"][0]["backend"], "backend-a");
    assert_eq!(record["retried_attempts"][0]["status"], 502);
    assert_eq!(
        record["retried_attempts"][0]["outcome"],
        "upstream_request_failed"
    );
    let attempts = record["backend_attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["attempt"], 1);
    assert_eq!(attempts[0]["backend"], "backend-a");
    assert_eq!(attempts[0]["outcome"], "upstream_request_failed");
    assert_eq!(attempts[0]["status"], 502);
    assert!(attempts[0]["backend_forward_start_us"].is_number());
    assert_eq!(attempts[1]["attempt"], 2);
    assert_eq!(attempts[1]["backend"], "backend-b");
    assert_eq!(attempts[1]["outcome"], "completed");
    assert_eq!(attempts[1]["status"], 200);
    assert!(attempts[1]["backend_body_complete_us"].is_number());
    assert_eq!(
        record["retried_attempts"][0]["started_us"],
        attempts[0]["started_us"]
    );

    let health = json_body(
        app.oneshot(inspector_get("/_onair/operator/health"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(health["backends"][0]["traffic_failures"], 1);
    assert_eq!(health["backends"][1]["traffic_successes"], 1);

    fallback.abort();
}

#[tokio::test]
async fn upstream_non_success_does_not_fall_back() {
    let redirect = RedirectBackend::spawn().await;
    let fallback = TestBackend::spawn("backend-b").await;
    let state = test_state_with_inspector_and_health(
        RoutingStrategy::Priority,
        vec![
            test_backend("backend-a", redirect.base_url()),
            test_backend("backend-b", fallback.base_url()),
        ],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
        HealthConfig::default(),
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello redirect"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let _ = json_body(response).await;
    assert_eq!(fallback.hits(), 0);
    assert_eq!(redirect.leak_hits(), 0);

    let records = json_body(
        app.oneshot(inspector_get("/_onair/inspector/requests?limit=1"))
            .await
            .unwrap(),
    )
    .await;
    let record = &records.as_array().unwrap()[0];
    assert_eq!(record["backend"], "backend-a");
    assert_eq!(record["outcome"]["kind"], "upstream_non_success");
    assert!(record.get("retried_attempts").is_none());
    let attempts = record["backend_attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["backend"], "backend-a");
    assert_eq!(attempts[0]["outcome"], "upstream_non_success");
    assert_eq!(attempts[0]["status"], 502);
    assert_eq!(attempts[0]["upstream_status"], 302);
    assert!(attempts[0]["backend_headers_received_us"].is_number());

    redirect.abort();
    fallback.abort();
}

#[tokio::test]
async fn inspector_is_local_only_by_default() {
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![],
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
    );
    let app = router(state);

    let response = app
        .oneshot(inspector_get_with_peer(
            "/_onair/inspector/requests",
            "198.51.100.20:55432",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn inspector_rejects_remote_forwarded_clients_by_default() {
    let server = ServerConfig {
        trusted_proxy_cidrs: vec!["127.0.0.1/32".parse().unwrap()],
        ..ServerConfig::default()
    };
    let state = test_state_with_server_config_and_inspector(
        RoutingStrategy::Priority,
        vec![],
        vec![],
        server,
        btree_set([PUBLIC_MODEL]),
        DebugCaptureConfig::default(),
        InspectorConfig {
            enabled: true,
            retention_requests: 16,
            allow_remote: false,
            ..InspectorConfig::default()
        },
        HealthConfig::default(),
    );
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/_onair/inspector/requests")
                .header(FORWARDED, "for=198.51.100.20")
                .extension(ConnectInfo(
                    "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn models_respect_context_length_output_policy() {
    let state = test_state_with_client_models(
        RoutingStrategy::Priority,
        vec![ResolvedBackend {
            id: "metadata-only".to_owned(),
            base_url: "http://127.0.0.1:9".to_owned(),
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["responses"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        }],
        vec![
            ResolvedRoute {
                key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
                expose: btree_set(["responses"]),
                context_length: ContextLengthSpec::Static { n_ctx: 131_072 },
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                backends: vec![RouteBackendBinding {
                    backend_id: "metadata-only".to_owned(),
                    backend_model: BACKEND_MODEL.to_owned(),
                }],
                extra_body: BTreeMap::new(),
                request_headers: BTreeMap::new(),
                expose_backend_errors: false,
                stream_capture: false,
                anthropic_max_tokens: None,
            },
            ResolvedRoute {
                key: RouteKey::Public("gpt-no-context".to_owned()),
                expose: btree_set(["responses"]),
                context_length: ContextLengthSpec::None,
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                backends: vec![RouteBackendBinding {
                    backend_id: "metadata-only".to_owned(),
                    backend_model: "backend-no-context".to_owned(),
                }],
                extra_body: BTreeMap::new(),
                request_headers: BTreeMap::new(),
                expose_backend_errors: false,
                stream_capture: false,
                anthropic_max_tokens: None,
            },
        ],
        btree_set([PUBLIC_MODEL, "gpt-no-context"]),
    );
    let app = router(state);

    let response = app.clone().oneshot(authed_get("/v1/models")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    let models = response_body["data"].as_array().unwrap();
    let model_with_context = models
        .iter()
        .find(|model| model["id"] == PUBLIC_MODEL)
        .unwrap();
    assert_eq!(model_with_context["meta"]["n_ctx"], 131_072);
    assert_eq!(model_with_context["meta"]["n_ctx_train"], 131_072);
    assert!(model_with_context.get("context_length").is_none());
    let model_without_context = models
        .iter()
        .find(|model| model["id"] == "gpt-no-context")
        .unwrap();
    assert!(model_without_context.get("meta").is_none());

    let response = app
        .clone()
        .oneshot(authed_get(&format!("/v1/models/{PUBLIC_MODEL}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["meta"]["n_ctx"], 131_072);
    assert_eq!(response_body["meta"]["n_ctx_train"], 131_072);

    let response = app
        .oneshot(authed_get(&format!("/props?model={PUBLIC_MODEL}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(
        response_body["default_generation_settings"]["n_ctx"],
        131_072
    );
    assert_eq!(response_body["model_alias"], PUBLIC_MODEL);

    let response = router(test_state_with_client_models(
        RoutingStrategy::Priority,
        vec![],
        vec![],
        BTreeSet::new(),
    ))
    .oneshot(authed_get("/props"))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["default_generation_settings"]["n_ctx"], 0);
    assert_eq!(response_body["model_alias"], "llama-server");
    assert_eq!(response_body["role"], "router");
}

#[tokio::test]
async fn upstream_context_size_is_forwarded_to_v1_models() {
    let address = TestBackend::spawn_props_only(131_072).await;
    let state = test_state_with_client_models(
        RoutingStrategy::Priority,
        vec![ResolvedBackend {
            id: "backend-a".to_owned(),
            base_url: format!("http://{address}"),
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["responses"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        }],
        vec![ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["responses"]),
            context_length: ContextLengthSpec::Upstream {
                backend_id: "backend-a".to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
                n_ctx: None,
            },
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: "backend-a".to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        }],
        btree_set([PUBLIC_MODEL]),
    );
    let app = router(state.clone());

    wait_for_cache_value(&state.context_sizes, PUBLIC_MODEL, Some(131_072)).await;

    let response = app
        .clone()
        .oneshot(authed_get(&format!("/v1/models/{PUBLIC_MODEL}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["meta"]["n_ctx"], 131_072);
    assert!(
        response_body["meta"].get("n_ctx_train").is_none(),
        "upstream models must omit n_ctx_train, got: {response_body}"
    );

    let response = app.oneshot(authed_get("/v1/models")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    let model = response_body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["id"] == PUBLIC_MODEL)
        .unwrap();
    assert_eq!(model["meta"]["n_ctx"], 131_072);
    assert!(model["meta"].get("n_ctx_train").is_none());
}

#[tokio::test]
async fn upstream_context_size_is_forwarded_to_props() {
    let address = TestBackend::spawn_props_only(65_536).await;
    let state = test_state_with_client_models(
        RoutingStrategy::Priority,
        vec![ResolvedBackend {
            id: "backend-a".to_owned(),
            base_url: format!("http://{address}"),
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["responses"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        }],
        vec![ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["responses"]),
            context_length: ContextLengthSpec::Upstream {
                backend_id: "backend-a".to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
                n_ctx: None,
            },
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: "backend-a".to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        }],
        btree_set([PUBLIC_MODEL]),
    );
    let app = router(state.clone());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if state.context_sizes.lookup(PUBLIC_MODEL).is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for upstream cache to populate");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let response = app
        .oneshot(authed_get(&format!("/props?model={PUBLIC_MODEL}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(
        response_body["default_generation_settings"]["n_ctx"],
        65_536
    );
    assert_eq!(response_body["model_alias"], PUBLIC_MODEL);
}

#[tokio::test]
async fn upstream_unreachable_hides_value() {
    let state = test_state_with_client_models(
        RoutingStrategy::Priority,
        vec![ResolvedBackend {
            id: "backend-a".to_owned(),
            base_url: "http://127.0.0.1:1".to_owned(),
            api_key: None,
            timeout: std::time::Duration::from_millis(50),
            supports: btree_set(["responses"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        }],
        vec![ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["responses"]),
            context_length: ContextLengthSpec::Upstream {
                backend_id: "backend-a".to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
                n_ctx: None,
            },
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: "backend-a".to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        }],
        btree_set([PUBLIC_MODEL]),
    );
    let app = router(state.clone());

    wait_for_cache_failure(&state.context_sizes, PUBLIC_MODEL).await;

    let response = app
        .clone()
        .oneshot(authed_get(&format!("/v1/models/{PUBLIC_MODEL}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert!(
        response_body.get("meta").is_none(),
        "upstream fetch failures must hide meta, got: {response_body}"
    );

    let response = app
        .oneshot(authed_get(&format!("/props?model={PUBLIC_MODEL}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["default_generation_settings"]["n_ctx"], 0);
}

#[tokio::test]
async fn operator_models_reports_upstream_source() {
    let address = TestBackend::spawn_props_only(8_192).await;
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![TestEndpoint {
            backend: ResolvedBackend {
                id: "backend-a".to_owned(),
                base_url: format!("http://{address}"),
                api_key: None,
                timeout: std::time::Duration::from_secs(5),
                supports: btree_set(["responses"]),
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                weight: 1,
                extra_body: BTreeMap::new(),
                expose_backend_errors: false,
                stream_capture: false,
            },
            route: ResolvedRoute {
                key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
                expose: btree_set(["responses"]),
                context_length: ContextLengthSpec::Upstream {
                    backend_id: "backend-a".to_owned(),
                    backend_model: BACKEND_MODEL.to_owned(),
                    n_ctx: None,
                },
                tool_schema_mode: ToolSchemaMode::Preserve,
                responses_store: ResponsesStorePolicy::Preserve,
                responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
                chat_stream_usage: ChatStreamUsagePolicy::Preserve,
                backends: vec![RouteBackendBinding {
                    backend_id: "backend-a".to_owned(),
                    backend_model: BACKEND_MODEL.to_owned(),
                }],
                extra_body: BTreeMap::new(),
                request_headers: BTreeMap::new(),
                expose_backend_errors: false,
                stream_capture: false,
                anthropic_max_tokens: None,
            },
        }],
        InspectorConfig {
            enabled: true,
            ..InspectorConfig::default()
        },
    );
    let app = router(state.clone());

    wait_for_cache_value(&state.context_sizes, PUBLIC_MODEL, Some(8_192)).await;

    let response = app
        .oneshot(inspector_get("/_onair/operator/models"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let model = body["public_models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["public"] == PUBLIC_MODEL)
        .unwrap();
    assert_eq!(model["context_length"], 8_192);
    assert_eq!(model["context_length_source"], "upstream");
    assert!(model["context_length_last_fetch_unix_ms"].is_u64());
}

async fn wait_for_cache_value(cache: &ContextSizeCache, public_model: &str, expected: Option<u64>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if cache.lookup(public_model) == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for cache[{public_model}] to be {expected:?}; got {:?}",
            cache.lookup(public_model)
        )
    });
}

async fn wait_for_cache_failure(cache: &ContextSizeCache, public_model: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(entry) = cache.entry(public_model)
                && entry.last_failure_unix_ms.is_some()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for cache[{public_model}] to record a failure"));
}

struct TestEndpoint {
    backend: ResolvedBackend,
    route: ResolvedRoute,
}

fn split_endpoints(endpoints: Vec<TestEndpoint>) -> (Vec<ResolvedBackend>, Vec<ResolvedRoute>) {
    let mut backends: Vec<ResolvedBackend> = Vec::with_capacity(endpoints.len());
    let mut routes: Vec<ResolvedRoute> = Vec::new();
    for endpoint in endpoints {
        let TestEndpoint { backend, route } = endpoint;
        backends.push(backend);
        if let Some(existing) = routes.iter_mut().find(|r| r.key == route.key) {
            existing.backends.extend(route.backends);
        } else {
            routes.push(route);
        }
    }
    (backends, routes)
}

fn test_state(strategy: RoutingStrategy, endpoints: Vec<TestEndpoint>) -> Arc<AppState> {
    let (backends, routes) = split_endpoints(endpoints);
    test_state_with_client_models(strategy, backends, routes, btree_set([PUBLIC_MODEL]))
}

fn test_state_with_debug_capture(
    strategy: RoutingStrategy,
    endpoints: Vec<TestEndpoint>,
    debug_capture: DebugCaptureConfig,
) -> Arc<AppState> {
    let (backends, routes) = split_endpoints(endpoints);
    test_state_with_config(
        strategy,
        backends,
        routes,
        btree_set([PUBLIC_MODEL]),
        debug_capture,
    )
}

fn test_state_with_inspector(
    strategy: RoutingStrategy,
    endpoints: Vec<TestEndpoint>,
    inspector: InspectorConfig,
) -> Arc<AppState> {
    test_state_with_inspector_and_health(strategy, endpoints, inspector, HealthConfig::default())
}

fn test_state_with_inspector_and_health(
    strategy: RoutingStrategy,
    endpoints: Vec<TestEndpoint>,
    inspector: InspectorConfig,
    health: HealthConfig,
) -> Arc<AppState> {
    let (backends, routes) = split_endpoints(endpoints);
    test_state_with_config_and_inspector(
        strategy,
        backends,
        routes,
        btree_set([PUBLIC_MODEL]),
        DebugCaptureConfig::default(),
        inspector,
        health,
    )
}

fn test_state_with_client_models(
    strategy: RoutingStrategy,
    backends: Vec<ResolvedBackend>,
    routes: Vec<ResolvedRoute>,
    client_models: BTreeSet<String>,
) -> Arc<AppState> {
    test_state_with_config(
        strategy,
        backends,
        routes,
        client_models,
        DebugCaptureConfig::default(),
    )
}

fn test_state_with_config(
    strategy: RoutingStrategy,
    backends: Vec<ResolvedBackend>,
    routes: Vec<ResolvedRoute>,
    client_models: BTreeSet<String>,
    debug_capture: DebugCaptureConfig,
) -> Arc<AppState> {
    test_state_with_config_and_inspector(
        strategy,
        backends,
        routes,
        client_models,
        debug_capture,
        InspectorConfig::default(),
        HealthConfig::default(),
    )
}

fn test_state_with_config_and_inspector(
    strategy: RoutingStrategy,
    backends: Vec<ResolvedBackend>,
    routes: Vec<ResolvedRoute>,
    client_models: BTreeSet<String>,
    debug_capture: DebugCaptureConfig,
    inspector: InspectorConfig,
    health: HealthConfig,
) -> Arc<AppState> {
    test_state_with_server_config_and_inspector(
        strategy,
        backends,
        routes,
        ServerConfig::default(),
        client_models,
        debug_capture,
        inspector,
        health,
    )
}

#[allow(clippy::too_many_arguments)]
fn test_state_with_server_config_and_inspector(
    strategy: RoutingStrategy,
    backends: Vec<ResolvedBackend>,
    routes: Vec<ResolvedRoute>,
    server: ServerConfig,
    client_models: BTreeSet<String>,
    debug_capture: DebugCaptureConfig,
    inspector: InspectorConfig,
    health: HealthConfig,
) -> Arc<AppState> {
    Arc::new(
        AppState::new(
            Config {
                server,
                telemetry: TelemetryConfig::default(),
                debug_capture,
                inspector,
                health,
                routing: RoutingConfig {
                    strategy,
                    ..RoutingConfig::default()
                },
                clients: vec![ResolvedClient {
                    id: "dev".to_owned(),
                    api_key: CLIENT_KEY.to_owned(),
                    models: client_models,
                }],
                backends,
                routes,
            },
            Metrics::new(),
            watch::channel(false).0,
        )
        .unwrap(),
    )
}

fn test_backend(id: &str, base_url: String) -> TestEndpoint {
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["responses", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["responses"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    }
}

fn test_chat_backend(id: &str, base_url: String) -> TestEndpoint {
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["chat", "streaming", "tools"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["chat", "responses_via_chat_completions", "tools"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    }
}

fn test_anthropic_backend(id: &str, base_url: String) -> TestEndpoint {
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["messages", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["messages"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    }
}

fn anthropic_messages_endpoint(id: &str, base_url: String) -> TestEndpoint {
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["messages", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["messages"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: Some(8192),
        },
    }
}

fn expose_backend_errors_endpoint(
    id: &str,
    base_url: String,
    backend_default: bool,
    route_override: Option<bool>,
) -> TestEndpoint {
    let resolved_route_value = route_override.unwrap_or(backend_default);
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["responses", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: backend_default,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["responses"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: resolved_route_value,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    }
}

fn json_request(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
        .header(CONTENT_TYPE, "application/json")
        .extension(ConnectInfo(
            "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn inspector_get(uri: &str) -> Request<Body> {
    inspector_request(uri, "127.0.0.1:55432")
}

fn inspector_get_with_peer(uri: &str, peer: &str) -> Request<Body> {
    inspector_request(uri, peer)
}

fn inspector_request(uri: &str, peer: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .extension(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()))
        .body(Body::empty())
        .unwrap()
}

fn authed_get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
        .body(Body::empty())
        .unwrap()
}

async fn json_body(response: Response<Body>) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn btree_set<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn temp_capture_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "onair-debug-capture-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn temp_database_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "onair-inspector-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

async fn wait_for_persisted_count(path: &Path, minimum: usize) {
    for _ in 0..50 {
        if inspector_persisted_count(path).unwrap_or_default() >= minimum {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("inspector persistence did not reach {minimum} records");
}

fn only_capture_path(capture_dir: &std::path::Path) -> std::path::PathBuf {
    let entries = std::fs::read_dir(capture_dir)
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1);
    entries[0].path()
}

#[derive(Clone)]
struct BackendState {
    name: String,
    requests: Arc<Mutex<Vec<Value>>>,
    hits: Arc<AtomicUsize>,
}

struct TestBackend {
    address: SocketAddr,
    state: BackendState,
    handle: JoinHandle<()>,
}

impl TestBackend {
    async fn spawn(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = BackendState {
            name: name.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route("/v1/responses", post(backend_responses))
            .route("/v1/chat/completions", post(backend_chat_completions))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    async fn spawn_props_only(n_ctx: u64) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/props", get(test_props_handler))
            .with_state(n_ctx);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        address
    }

    async fn spawn_json_labeled_stream(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = BackendState {
            name: name.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route("/v1/responses", post(json_labeled_stream_responses))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    async fn spawn_anthropic_messages(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = BackendState {
            name: name.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route("/v1/messages", post(anthropic_messages_handler))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    async fn spawn_anthropic_messages_streaming(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = BackendState {
            name: name.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route("/v1/messages", post(anthropic_messages_streaming_handler))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    async fn spawn_error(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = BackendState {
            name: name.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route("/v1/responses", post(error_backend_responses))
            .route("/v1/chat/completions", post(backend_chat_completions))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    async fn spawn_slow(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = BackendState {
            name: name.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route("/v1/responses", post(slow_backend_responses))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    /// Spawn a backend that always returns the given status code,
    /// body, and (optional) extra response headers on
    /// `/v1/responses`. `content_type` is sent only when `Some` so
    /// the new `forward_upstream_error` branch can be tested with
    /// the upstream-omits-content-type case.
    async fn spawn_error_with_headers(
        name: &str,
        status: StatusCode,
        content_type: Option<&'static str>,
        body: &'static str,
        extra_headers: Vec<(&'static str, &'static str)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = BackendState {
            name: name.to_owned(),
            requests: Arc::new(Mutex::new(Vec::new())),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route("/v1/responses", post(error_backend_responses_with_headers))
            .with_state(ExtraErrorState {
                inner: state.clone(),
                status,
                content_type: content_type.map(str::to_owned),
                body: body.to_owned(),
                extra_headers: extra_headers
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
            });
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn hits(&self) -> usize {
        self.state.hits.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<Value> {
        self.state.requests.lock().unwrap().clone()
    }

    fn abort(self) {
        self.handle.abort();
    }
}

// --- Header-capturing backend for request_headers integration tests ---

#[derive(Clone)]
struct HeaderCaptureState {
    requests: Arc<Mutex<Vec<(Value, BTreeMap<String, Vec<String>>)>>>,
}

struct HeaderCaptureBackend {
    address: SocketAddr,
    state: HeaderCaptureState,
    handle: JoinHandle<()>,
}

impl HeaderCaptureBackend {
    async fn spawn(name: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = HeaderCaptureState {
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let state_clone = state.clone();
        let app = Router::new()
            .route("/v1/models", get(backend_models))
            .route(
                "/v1/chat/completions",
                post(header_capture_chat_completions),
            )
            .route("/v1/messages", post(header_capture_chat_completions))
            .with_state((name.to_owned(), state_clone));
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            state,
            handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn captured_requests(&self) -> Vec<(Value, BTreeMap<String, String>)> {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(body, headers)| {
                let flattened: BTreeMap<String, String> = headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.last().cloned().unwrap_or_default()))
                    .collect();
                (body.clone(), flattened)
            })
            .collect()
    }

    fn captured_request_header_counts(&self) -> Vec<BTreeMap<String, usize>> {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, headers)| headers.iter().map(|(k, v)| (k.clone(), v.len())).collect())
            .collect()
    }

    fn abort(self) {
        self.handle.abort();
    }
}

async fn header_capture_chat_completions(
    State((name, state)): State<(String, HeaderCaptureState)>,
    headers: axum::http::HeaderMap,
    Json(payload): Json<Value>,
) -> Response<Body> {
    let mut captured_headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in headers.iter() {
        if *k == axum::http::header::HOST
            || *k == axum::http::header::TRANSFER_ENCODING
            || *k == axum::http::header::CONTENT_LENGTH
        {
            continue;
        }
        if let Ok(val) = v.to_str() {
            captured_headers
                .entry(k.as_str().to_owned())
                .or_default()
                .push(val.to_owned());
        }
    }
    state
        .requests
        .lock()
        .unwrap()
        .push((payload.clone(), captured_headers));

    let response = json!({
        "id": format!("chatcmpl_{name}"),
        "object": "chat.completion",
        "created": 123,
        "model": payload["model"],
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .unwrap()
}

fn request_headers_endpoint(id: &str, base_url: String) -> TestEndpoint {
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["chat", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["chat"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: {
                let mut m = BTreeMap::new();
                m.insert("x-route-header".to_owned(), "from-route".to_owned());
                m.insert("x-another-header".to_owned(), "another-value".to_owned());
                m
            },
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    }
}

fn request_headers_override_endpoint(id: &str, base_url: String) -> TestEndpoint {
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["chat", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["chat"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: {
                let mut m = BTreeMap::new();
                // Override the client's x-request-id.
                m.insert("x-request-id".to_owned(), "route-req-id".to_owned());
                m
            },
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    }
}

fn request_headers_api_key_endpoint(id: &str, base_url: String, api_key: &str) -> TestEndpoint {
    TestEndpoint {
        backend: ResolvedBackend {
            id: id.to_owned(),
            base_url,
            api_key: Some(api_key.to_owned()),
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["chat", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["chat"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: id.to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: {
                let mut m = BTreeMap::new();
                m.insert("authorization".to_owned(), "Bearer route-token".to_owned());
                m
            },
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    }
}

#[tokio::test]
async fn request_headers_injects_into_upstream_request() {
    let backend = HeaderCaptureBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![request_headers_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let captured = backend.captured_requests();
    assert_eq!(captured.len(), 1);
    let headers = &captured[0].1;
    assert_eq!(
        headers.get("x-route-header").map(String::as_str),
        Some("from-route"),
        "x-route-header should be injected from route.request_headers"
    );
    assert_eq!(
        headers.get("x-another-header").map(String::as_str),
        Some("another-value"),
        "x-another-header should be injected from route.request_headers"
    );

    backend.abort();
}

#[tokio::test]
async fn request_headers_overrides_client_header() {
    let backend = HeaderCaptureBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![request_headers_override_endpoint(
            "backend-a",
            backend.base_url(),
        )],
    );
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
                .header(CONTENT_TYPE, "application/json")
                .header("x-request-id", "client-req-id")
                .extension(ConnectInfo(
                    "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::from(
                    json!({
                        "model": PUBLIC_MODEL,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let captured = backend.captured_requests();
    assert_eq!(captured.len(), 1);
    let headers = &captured[0].1;
    assert_eq!(
        headers.get("x-request-id").map(String::as_str),
        Some("route-req-id"),
        "route.request_headers should override client-provided x-request-id"
    );
    let counts = backend.captured_request_header_counts();
    assert_eq!(
        counts[0].get("x-request-id"),
        Some(&1),
        "overridden header must be sent exactly once"
    );

    backend.abort();
}

#[tokio::test]
async fn request_headers_api_key_takes_precedence() {
    let backend = HeaderCaptureBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![request_headers_api_key_endpoint(
            "backend-a",
            backend.base_url(),
            "backend-token",
        )],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let captured = backend.captured_requests();
    assert_eq!(captured.len(), 1);
    let headers = &captured[0].1;
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer backend-token"),
        "route.api_key should override route.request_headers authorization"
    );
    let counts = backend.captured_request_header_counts();
    assert_eq!(
        counts[0].get("authorization"),
        Some(&1),
        "authorization header must be sent exactly once"
    );

    backend.abort();
}

#[tokio::test]
async fn request_headers_hot_reload() {
    let backend = HeaderCaptureBackend::spawn("backend-a").await;
    let toml_v1 = format!(
        r#"
        [access]
        default_models = ["{PUBLIC_MODEL}"]

        [[client]]
        id = "dev"
        api_key = "{CLIENT_KEY}"

        [[backend]]
        id = "backend-a"
        base_url = "{base_url}"
        supports = ["chat", "streaming"]

        [[route]]
        public = "{PUBLIC_MODEL}"
        expose = ["chat"]
        backends = ["{BACKEND_MODEL}@backend-a"]

        [route.request_headers]
        x-config-version = "v1"
        "#,
        base_url = backend.base_url()
    );
    let path = env::temp_dir().join(format!(
        "onair-request-headers-reload-{}-{}.toml",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, &toml_v1).unwrap();
    let config = Config::load(&path).unwrap();
    let state = Arc::new(AppState::new(config, Metrics::new(), watch::channel(false).0).unwrap());
    let app = router(state.clone());

    // First request: should see x-config-version = v1
    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let captured = backend.captured_requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].1.get("x-config-version").map(String::as_str),
        Some("v1"),
        "first request should use v1 config"
    );

    // Reload config with updated request_headers
    let toml_v2 = toml_v1.replace("x-config-version = \"v1\"", "x-config-version = \"v2\"");
    std::fs::write(&path, &toml_v2).unwrap();
    let new_config = Config::load(&path).unwrap();
    state.config.replace(new_config);

    // Second request: should see x-config-version = v2
    let response = app
        .oneshot(json_request(
            "/v1/chat/completions",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let captured = backend.captured_requests();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[1].1.get("x-config-version").map(String::as_str),
        Some("v2"),
        "after hot reload, request should use v2 config"
    );

    let _ = std::fs::remove_file(&path);
    backend.abort();
}

struct RedirectBackend {
    address: SocketAddr,
    leak_hits: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl RedirectBackend {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let leak_hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/responses", post(redirect_responses))
            .route("/leak", get(redirect_leak))
            .with_state(leak_hits.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            address,
            leak_hits,
            handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn leak_hits(&self) -> usize {
        self.leak_hits.load(Ordering::SeqCst)
    }

    fn abort(self) {
        self.handle.abort();
    }
}

async fn backend_responses(
    State(state): State<BackendState>,
    Json(payload): Json<Value>,
) -> Response<Body> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(payload.clone());
    let response = json!({
        "id": format!("resp_{}", state.name),
        "object": "response",
        "model": payload["model"],
        "created_at": 123,
        "status": "completed",
        "output": [{
            "id": format!("msg_{}", state.name),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "responses response",
                "annotations": []
            }]
        }],
        "output_text": "responses response",
        "usage": {
            "input_tokens": 13,
            "input_tokens_details": {
                "cached_tokens": 7
            },
            "output_tokens": 3
        }
    });

    if payload.get("stream").and_then(Value::as_bool) == Some(true) {
        let mut body = String::new();
        body.push_str(&format!(
            "event: response.created\ndata: {}\n\n",
            json!({
                "type": "response.created",
                "response": {
                    "id": response["id"],
                    "object": "response",
                    "created_at": response["created_at"],
                    "model": response["model"]
                }
            })
        ));
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            json!({
                "type": "response.output_text.delta",
                "delta": "responses response"
            })
        ));
        body.push_str(&format!(
            "event: response.completed\ndata: {}\n\n",
            json!({
                "type": "response.completed",
                "response": response
            })
        ));
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .unwrap()
}

async fn json_labeled_stream_responses(
    State(state): State<BackendState>,
    Json(payload): Json<Value>,
) -> Response<Body> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(payload.clone());
    let response = json!({
        "id": format!("resp_{}", state.name),
        "object": "response",
        "model": payload["model"],
        "created_at": 123,
        "status": "completed",
        "output": [{
            "id": format!("msg_{}", state.name),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "responses response",
                "annotations": []
            }]
        }],
        "output_text": "responses response",
        "usage": {
            "input_tokens": 13,
            "output_tokens": 3,
            "total_tokens": 16
        }
    });

    if payload.get("stream").and_then(Value::as_bool) == Some(true) {
        let mut body = String::new();
        body.push_str(&format!(
            "event: response.created\ndata: {}\n\n",
            json!({
                "type": "response.created",
                "response": {
                    "id": response["id"],
                    "object": "response",
                    "created_at": response["created_at"],
                    "model": response["model"]
                }
            })
        ));
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            json!({
                "type": "response.output_text.delta",
                "delta": "responses response"
            })
        ));
        body.push_str(&format!(
            "event: response.completed\ndata: {}\n\n",
            json!({
                "type": "response.completed",
                "response": response
            })
        ));
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .unwrap()
}

async fn error_backend_responses(
    State(state): State<BackendState>,
    Json(payload): Json<Value>,
) -> Response<Body> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(payload.clone());
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({
                "error": {
                    "message": "upstream failure detail",
                    "type": "invalid_request_error"
                }
            })
            .to_string(),
        ))
        .unwrap()
}

#[derive(Clone)]
struct ExtraErrorState {
    inner: BackendState,
    status: StatusCode,
    content_type: Option<String>,
    body: String,
    extra_headers: Vec<(String, String)>,
}

async fn error_backend_responses_with_headers(
    State(state): State<ExtraErrorState>,
    Json(payload): Json<Value>,
) -> Response<Body> {
    state.inner.hits.fetch_add(1, Ordering::SeqCst);
    state.inner.requests.lock().unwrap().push(payload.clone());
    let mut builder = Response::builder().status(state.status);
    if let Some(content_type) = &state.content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    for (name, value) in &state.extra_headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::from(state.body.clone())).unwrap()
}

async fn slow_backend_responses(
    State(state): State<BackendState>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(payload.clone());
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    Json(json!({
        "id": format!("resp_{}", state.name),
        "object": "response",
        "model": payload["model"],
        "output": []
    }))
}

async fn backend_chat_completions(
    State(state): State<BackendState>,
    Json(payload): Json<Value>,
) -> Response<Body> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(payload.clone());

    if payload.get("stream").and_then(Value::as_bool) == Some(true) {
        let model = payload
            .get("model")
            .cloned()
            .unwrap_or_else(|| Value::String("unknown".to_owned()));
        let mut body = String::new();
        body.push_str(&format!(
            "data: {}\n\n",
            json!({
                "id": format!("chatcmpl_{}", state.name),
                "object": "chat.completion.chunk",
                "created": 123,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": "chat response"
                    },
                    "finish_reason": null
                }]
            })
        ));
        if payload.pointer("/stream_options/include_usage") == Some(&Value::Bool(true)) {
            body.push_str(&format!(
                "data: {}\n\n",
                json!({
                    "id": format!("chatcmpl_{}", state.name),
                    "object": "chat.completion.chunk",
                    "created": 123,
                    "model": payload["model"],
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 11,
                        "prompt_tokens_details": {
                            "cached_tokens": 2
                        },
                        "completion_tokens": 5,
                        "total_tokens": 16
                    }
                })
            ));
        }
        body.push_str("data: [DONE]\n\n");
        return Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
            "id": format!("chatcmpl_{}", state.name),
            "object": "chat.completion",
            "created": 123,
            "model": payload["model"],
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "chat response"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 11,
                "prompt_tokens_details": {
                    "cached_tokens": 2
                },
                "completion_tokens": 5,
                "total_tokens": 16
            }
                })
            .to_string(),
        ))
        .unwrap()
}

async fn redirect_responses() -> Response<Body> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(LOCATION, "/leak")
        .body(Body::empty())
        .unwrap()
}

async fn redirect_leak(State(leak_hits): State<Arc<AtomicUsize>>) -> Response<Body> {
    leak_hits.fetch_add(1, Ordering::SeqCst);
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .unwrap()
}

async fn backend_models() -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": []
    }))
}

async fn anthropic_messages_handler(
    State(state): State<BackendState>,
    Json(payload): Json<Value>,
) -> Response<Body> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(payload.clone());
    let response = json!({
        "id": "msg_test_001",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "hello from anthropic"}],
        "model": payload["model"],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(response.to_string()))
        .unwrap()
}

async fn anthropic_messages_streaming_handler(
    State(state): State<BackendState>,
    body_bytes: Bytes,
) -> Response<Body> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let payload: Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
    state.requests.lock().unwrap().push(payload.clone());

    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let mut body = String::new();
    // message_start event
    body.push_str(&format!(
        "event: message_start\ndata: {}\n\n",
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_test_001",
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "usage": {"input_tokens": 10, "output_tokens": 0}
            }
        })
    ));
    // content_block_start event
    body.push_str(&format!(
        "event: content_block_start\ndata: {}\n\n",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        })
    ));
    // content_block_delta event
    body.push_str(&format!(
        "event: content_block_delta\ndata: {}\n\n",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello from anthropic"}
        })
    ));
    // content_block_stop event
    body.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    // message_delta event
    body.push_str(&format!(
        "event: message_delta\ndata: {}\n\n",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 5}
        })
    ));
    // message_stop event
    body.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .unwrap()
}

async fn test_props_handler(State(n_ctx): State<u64>) -> Json<Value> {
    Json(json!({
        "default_generation_settings": {
            "params": {},
            "n_ctx": n_ctx,
        }
    }))
}

async fn wait_for_backend_health(app: &Router, status: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let response = app
                .clone()
                .oneshot(inspector_get("/_onair/operator/health"))
                .await
                .unwrap();
            let health = json_body(response).await;
            if health["backends"][0]["status"] == status {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for backend health '{status}'"));
}

// ---- expose_backend_errors tests ----

const EXPOSED_BODY: &str =
    r#"{"error":{"message":"upstream says no","code":"upstream_518","type":"server_error"}}"#;

#[tokio::test]
async fn expose_off_default_sanitizes_error() {
    // Default `expose_backend_errors = false`. Upstream 500 must
    // be replaced with the generic OpenAI error envelope and the
    // upstream body must NOT reach the client.
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), false, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        !body.contains("upstream_518"),
        "upstream body must not leak; got: {body}"
    );
    assert!(
        body.contains("server_error"),
        "sanitized envelope must be present; got: {body}"
    );
    backend.abort();
}

#[tokio::test]
async fn expose_on_per_backend_forwards_upstream_body_and_status() {
    // Backend default `expose_backend_errors = true`. The 5xx
    // body is forwarded verbatim and the status is mapped through
    // `map_upstream_status` (500 → 502).
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(bytes.as_ref(), EXPOSED_BODY.as_bytes());
    backend.abort();
}

#[tokio::test]
async fn expose_on_per_route_overrides_backend_off() {
    // Backend default off, route on -> route wins.
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint =
        expose_backend_errors_endpoint("backend-a", backend.base_url(), false, Some(true));
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(bytes.as_ref(), EXPOSED_BODY.as_bytes());
    backend.abort();
}

#[tokio::test]
async fn expose_off_per_route_overrides_backend_on() {
    // Backend default on, route off -> route wins, body sanitized.
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint =
        expose_backend_errors_endpoint("backend-a", backend.base_url(), true, Some(false));
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(!body.contains("upstream_518"), "body={body}");
    backend.abort();
}

#[tokio::test]
async fn expose_on_oversize_body_falls_back_to_sanitize() {
    // The cap is 1 MiB. A 1.5 MiB body must NOT be forwarded; the
    // client gets the sanitized envelope. (The truncation is still
    // recorded in debug capture, but we don't enable it here.)
    let mut body = String::with_capacity(2 * 1024 * 1024);
    body.push_str(&"x".repeat(1_500_000));
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        Box::leak(body.into_boxed_str()),
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    // The forward-fallback returns the sanitized envelope, not a
    // megabyte of `x` characters.
    assert!(body.len() < 1024, "body length={} too large", body.len());
    assert!(body.contains("server_error"), "body={body}");
    backend.abort();
}

#[tokio::test]
async fn expose_on_unset_content_type_defaults_to_application_json() {
    // The upstream returns no content-type. The forwarded
    // response must still declare a content-type of
    // `application/json` so the client can parse the body.
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        None,
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .map(|v| v.to_str().unwrap()),
        Some("application/json"),
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(bytes.as_ref(), EXPOSED_BODY.as_bytes());
    backend.abort();
}

#[tokio::test]
async fn expose_on_unknown_status_maps_to_502() {
    // Upstream returns 518 (which `map_upstream_status` collapses
    // to 502) with a custom body. Client sees 502 + the body.
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::from_u16(518).unwrap(),
        Some("application/json"),
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(bytes.as_ref(), EXPOSED_BODY.as_bytes());
    backend.abort();
}

#[tokio::test]
async fn expose_on_no_body_still_sends_default_content_type() {
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        None,
        "",
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .map(|v| v.to_str().unwrap()),
        Some("application/json"),
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(bytes.is_empty(), "body should be empty, got: {bytes:?}");
    backend.abort();
}

#[tokio::test]
async fn expose_on_retry_after_is_forwarded() {
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::TOO_MANY_REQUESTS,
        Some("application/json"),
        EXPOSED_BODY,
        vec![("retry-after", "30")],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("30"),
    );
    backend.abort();
}

#[tokio::test]
async fn expose_on_does_not_forward_server_header() {
    // The `server` header is not in the strict allowlist and must
    // not leak the backend implementation.
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        EXPOSED_BODY,
        vec![("server", "llama.cpp/1234")],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert!(response.headers().get("server").is_none());
    backend.abort();
}

#[tokio::test]
async fn expose_on_records_exposed_backend_error_in_inspector() {
    // The inspector must record `exposed_backend_error = true` for
    // a forwarded non-success response, and `false` for the
    // sanitized path.
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), true, None);
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![endpoint],
        InspectorConfig {
            enabled: true,
            ..InspectorConfig::default()
        },
    );
    let app = router(state.clone());

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    // Pull the inspector snapshot via the public endpoint.
    let snap = state.inspector.records_limited(10);
    assert!(!snap.is_empty(), "expected at least one inspector record");
    let rec = &snap[0];
    let outcome_is_upstream_non_success =
        matches!(rec.outcome, InspectorOutcome::UpstreamNonSuccess);
    assert!(
        outcome_is_upstream_non_success,
        "expected UpstreamNonSuccess, got: {:?}",
        rec.outcome
    );
    assert!(rec.base.exposed_backend_error);
    backend.abort();
}

#[tokio::test]
async fn expose_off_records_exposed_backend_error_false_in_inspector() {
    let backend = TestBackend::spawn_error_with_headers(
        "backend-a",
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("application/json"),
        EXPOSED_BODY,
        vec![],
    )
    .await;
    let endpoint = expose_backend_errors_endpoint("backend-a", backend.base_url(), false, None);
    let state = test_state_with_inspector(
        RoutingStrategy::Priority,
        vec![endpoint],
        InspectorConfig {
            enabled: true,
            ..InspectorConfig::default()
        },
    );
    let app = router(state.clone());

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({"model": PUBLIC_MODEL, "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let snap = state.inspector.records_limited(10);
    assert!(!snap.is_empty());
    let rec = &snap[0];
    assert!(!rec.base.exposed_backend_error);
    backend.abort();
}

// ---- access-control union: default_models + [[client]].models ----

/// `Config::load` reads the TOML, runs `resolve_clients`, and
/// returns the resolved `Config`. The app tests need this when
/// the behavior under test (e.g. the `default_models ∪
/// [[client]].models` union) lives in the resolve path rather
/// than in already-resolved structs.
fn app_state_from_toml(toml: &str) -> Arc<AppState> {
    let path = env::temp_dir().join(format!(
        "onair-models-access-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    Arc::new(AppState::new(config, Metrics::new(), tokio::sync::watch::channel(false).0).unwrap())
}

const DEFAULT_ONLY_MODEL: &str = "gpt-default-only";
const CLIENT_ONLY_MODEL: &str = "gpt-client-only";

/// Two models, two clients, four `[[route]]` declarations: each
/// public model has a route so the `/v1/models` listing is gated
/// only by the per-client whitelist, not by missing routes.
const MODELS_ACCESS_TOML: &str = r#"
[access]
default_models = ["gpt-default-only", "gpt-shared"]

[[client]]
id = "dev-rich"
api_key = "sk-test"
models = ["gpt-shared", "gpt-client-only"]

[[client]]
id = "dev-default"
api_key = "sk-test-default"

[[backend]]
id = "backend-a"
base_url = "http://127.0.0.1:9"
supports = ["responses"]

[[route]]
public = "gpt-default-only"
expose = ["responses"]
backends = ["backend-private@backend-a"]

[[route]]
public = "gpt-shared"
expose = ["responses"]
backends = ["backend-private@backend-a"]

[[route]]
public = "gpt-client-only"
expose = ["responses"]
backends = ["backend-private@backend-a"]
"#;

#[tokio::test]
async fn models_listing_unions_default_models_and_client_models() {
    // A client's effective whitelist is the UNION of
    // `[access].default_models` and `[[client]].models`.
    // `/v1/models` must list every public model that is both in
    // the union AND has a matching `[[route]]` declaration. The
    // two clients in this fixture (dev-rich, dev-default) have
    // different effective whitelists; the test asserts each.
    let backend = TestBackend::spawn("backend-a").await;
    let mut toml = MODELS_ACCESS_TOML.to_owned();
    toml = toml.replace("http://127.0.0.1:9", &backend.base_url());
    let state = app_state_from_toml(&toml);
    let app = router(state);

    // dev-rich sees BOTH the default-only and the client-only
    // models, plus the shared one.
    let response = app.clone().oneshot(authed_get("/v1/models")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"gpt-default-only"),
        "dev-rich must see the default-only model; got {ids:?}"
    );
    assert!(
        ids.contains(&"gpt-shared"),
        "dev-rich must see the shared model; got {ids:?}"
    );
    assert!(
        ids.contains(&"gpt-client-only"),
        "dev-rich must see the client-only model; got {ids:?}"
    );
    assert_eq!(
        ids.len(),
        3,
        "dev-rich must see exactly 3 models; got {ids:?}"
    );

    // The /v1/models/{model} endpoint agrees for every model in
    // the effective whitelist and 404s for everything else.
    for model in &["gpt-default-only", "gpt-shared", "gpt-client-only"] {
        let response = app
            .clone()
            .oneshot(authed_get(&format!("/v1/models/{model}")))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "dev-rich GET /v1/models/{model} must be 200"
        );
    }

    // dev-default sees only the default models (NOT the
    // client-only one that dev-rich has). This pins the access
    // boundary: a client's client.models list is private to that
    // client.
    let response = app
        .clone()
        .oneshot(request_with_key("/v1/models", "sk-test-default"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"gpt-default-only"));
    assert!(ids.contains(&"gpt-shared"));
    assert!(
        !ids.contains(&"gpt-client-only"),
        "dev-default must NOT see dev-rich's client-only model; got {ids:?}"
    );
    assert_eq!(ids.len(), 2);

    let response = app
        .clone()
        .oneshot(request_with_key(
            "/v1/models/gpt-client-only",
            "sk-test-default",
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "dev-default GET /v1/models/gpt-client-only must be 404"
    );

    // /v1/props?model=... enforces the same whitelist. dev-default
    // must not be able to query metadata for gpt-client-only.
    let response = app
        .clone()
        .oneshot(request_with_key(
            "/props?model=gpt-client-only",
            "sk-test-default",
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "dev-default GET /props?model=gpt-client-only must be 404"
    );
    // dev-rich can.
    let response = app
        .clone()
        .oneshot(authed_get("/props?model=gpt-shared"))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "dev-rich GET /props?model=gpt-shared must be 200"
    );
    // /props with no model is a server-info call, not a model
    // access call, so it stays accessible to every authenticated
    // client.
    let response = app
        .clone()
        .oneshot(request_with_key("/props", "sk-test-default"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // A model that is in NEITHER default_models NOR client.models
    // (and has no [[route]]) must 404 for every client, even the
    // privileged one.
    let response = app
        .oneshot(authed_get("/v1/models/completely-unconfigured"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    backend.abort();
}

fn request_with_key(uri: &str, api_key: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn models_request_unauthorized_for_models_outside_effective_whitelist() {
    // Companion to the listing test: a request that names a
    // model the client cannot access (e.g. a fresh client that
    // has only the defaults, asking for a model that exists in
    // a different client's whitelist) must surface as
    // model_not_found, not silently fall through to some other
    // client's session.
    let backend = TestBackend::spawn("backend-a").await;
    let mut toml = MODELS_ACCESS_TOML.to_owned();
    toml = toml.replace("http://127.0.0.1:9", &backend.base_url());
    let state = app_state_from_toml(&toml);
    let app = router(state);

    // dev-default asks the proxy to route a request to a model
    // it cannot see. The proxy must reject at the access check
    // before any backend is contacted. dev-rich's
    // gpt-client-only is gated by the model's presence in
    // dev-default's effective whitelist, which is empty for
    // that model.
    let negative = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(AUTHORIZATION, "Bearer sk-test-default")
                .header(CONTENT_TYPE, "application/json")
                .extension(ConnectInfo(
                    "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::from(
                    json!({"model": "gpt-client-only", "input": "hi"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        negative.status(),
        StatusCode::NOT_FOUND,
        "dev-default must not be able to route gpt-client-only"
    );
    let _ = DEFAULT_ONLY_MODEL;
    let _ = CLIENT_ONLY_MODEL;
    backend.abort();
}

#[tokio::test]
async fn count_tokens_returns_404_with_anthropic_error_format() {
    let backend = TestBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_anthropic_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/messages/count_tokens",
            json!({
                "model": "public-model",
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await;
    // Must be in Anthropic error format, not OpenAI format.
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(body["error"]["message"].as_str().unwrap().len() > 0);

    backend.abort();
}

#[tokio::test]
async fn anthropic_version_default_injected_when_client_omits() {
    let backend = HeaderCaptureBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![test_anthropic_backend("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/messages",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 100
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let captured = backend.captured_requests();
    assert_eq!(captured.len(), 1);
    // The default anthropic-version must have been injected.
    let headers = &captured[0].1;
    assert_eq!(
        headers.get("anthropic-version").map(|v| v.as_str()),
        Some("2023-06-01"),
        "expected default anthropic-version header to be injected"
    );

    backend.abort();
}

#[tokio::test]
async fn anthropic_messages_non_streaming_success() {
    let backend = TestBackend::spawn_anthropic_messages("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![anthropic_messages_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/messages",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 100
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    // Response model must be the public model, not the backend model.
    assert_eq!(response_body["model"], PUBLIC_MODEL);
    assert_eq!(response_body["type"], "message");
    assert_eq!(response_body["role"], "assistant");
    assert_eq!(response_body["stop_reason"], "end_turn");
    assert!(response_body["usage"]["input_tokens"].is_number());
    assert!(response_body["usage"]["output_tokens"].is_number());

    // Verify the upstream request had the backend model and max_tokens filled.
    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["max_tokens"], 100);

    backend.abort();
}

#[tokio::test]
async fn anthropic_messages_non_streaming_fills_max_tokens_from_policy() {
    // When the client omits max_tokens, the route's anthropic_max_tokens
    // policy should insert it into the request body.
    let backend = TestBackend::spawn_anthropic_messages("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![anthropic_messages_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/messages",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let response_body = json_body(response).await;
    assert_eq!(response_body["model"], PUBLIC_MODEL);

    // The upstream request should have had max_tokens inserted from the policy.
    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(
        captured[0]["max_tokens"], 8192,
        "anthropic_max_tokens policy should fill missing max_tokens"
    );

    backend.abort();
}

#[tokio::test]
async fn anthropic_messages_streaming_success() {
    let backend = TestBackend::spawn_anthropic_messages_streaming("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![anthropic_messages_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .clone()
        .oneshot(json_request(
            "/v1/messages",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 100,
                "stream": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream")),
        "expected text/event-stream content type"
    );

    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(
        !bytes.is_empty(),
        "streaming response body must not be empty"
    );
    let body = String::from_utf8(bytes.to_vec()).unwrap();

    // The backend model should be rewritten to the public model
    // in the message_start event's message.model field.
    assert!(
        body.contains(&format!("\"model\":\"{PUBLIC_MODEL}\"")),
        "message_start model should be rewritten to public model; body={body}"
    );
    assert!(
        !body.contains(&format!("\"model\":\"{BACKEND_MODEL}\"")),
        "backend model must not leak into stream; body={body}"
    );
    assert!(body.contains("\"type\":\"message_start\""), "body={body}");
    assert!(
        body.contains("\"type\":\"content_block_delta\""),
        "body={body}"
    );
    assert!(body.contains("\"type\":\"message_stop\""), "body={body}");
    assert!(body.contains("\"text_delta\""), "body={body}");
    assert!(body.contains("hello from anthropic"), "body={body}");

    // Verify the upstream request was correctly rewritten.
    assert_eq!(backend.hits(), 1);
    let captured = backend.requests();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["model"], BACKEND_MODEL);
    assert_eq!(captured[0]["stream"], true);

    backend.abort();
}

#[tokio::test]
async fn anthropic_messages_streaming_anthropic_version_forwarded() {
    let backend = HeaderCaptureBackend::spawn("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![anthropic_messages_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
                .header(CONTENT_TYPE, "application/json")
                .header("anthropic-version", "2024-01-01")
                .extension(ConnectInfo(
                    "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::from(
                    json!({
                        "model": PUBLIC_MODEL,
                        "messages": [{"role": "user", "content": "hi"}],
                        "max_tokens": 100
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let captured = backend.captured_requests();
    assert_eq!(captured.len(), 1);
    let headers = &captured[0].1;
    // The client-provided anthropic-version should be forwarded.
    assert_eq!(
        headers.get("anthropic-version").map(|v| v.as_str()),
        Some("2024-01-01"),
        "client-provided anthropic-version should be forwarded, not overridden"
    );

    backend.abort();
}

#[tokio::test]
async fn anthropic_messages_missing_max_tokens_returns_400() {
    // When the client omits max_tokens and the route has no
    // anthropic_max_tokens policy, the request must fail with 400
    // in Anthropic error format.
    let backend = TestBackend::spawn_anthropic_messages("backend-a").await;
    // Use a route without anthropic_max_tokens.
    let endpoint = TestEndpoint {
        backend: ResolvedBackend {
            id: "backend-a".to_owned(),
            base_url: backend.base_url(),
            api_key: None,
            timeout: std::time::Duration::from_secs(5),
            supports: btree_set(["messages", "streaming"]),
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            weight: 1,
            extra_body: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
        },
        route: ResolvedRoute {
            key: RouteKey::Public(PUBLIC_MODEL.to_owned()),
            expose: btree_set(["messages"]),
            context_length: ContextLengthSpec::None,
            tool_schema_mode: ToolSchemaMode::Preserve,
            responses_store: ResponsesStorePolicy::Preserve,
            responses_max_output_tokens: ResponsesMaxOutputTokensPolicy::Preserve,
            chat_stream_usage: ChatStreamUsagePolicy::Preserve,
            backends: vec![RouteBackendBinding {
                backend_id: "backend-a".to_owned(),
                backend_model: BACKEND_MODEL.to_owned(),
            }],
            extra_body: BTreeMap::new(),
            request_headers: BTreeMap::new(),
            expose_backend_errors: false,
            stream_capture: false,
            anthropic_max_tokens: None,
        },
    };
    let state = test_state(RoutingStrategy::Priority, vec![endpoint]);
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/messages",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    // Must be in Anthropic error format, not OpenAI format.
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_tokens"),
        "error message should mention max_tokens; got: {:?}",
        body["error"]["message"]
    );
    // Must not have hit the backend.
    assert_eq!(backend.hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn anthropic_messages_non_json_body_returns_400() {
    let backend = TestBackend::spawn_anthropic_messages("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![anthropic_messages_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    // Use a model query parameter so the proxy's model preflight check
    // passes, but send a non-JSON body so the Anthropic-specific
    // rewrite rejects it with "requires a JSON request body".
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/messages?model={PUBLIC_MODEL}"))
                .header(AUTHORIZATION, format!("Bearer {CLIENT_KEY}"))
                .header(CONTENT_TYPE, "text/plain")
                .extension(ConnectInfo(
                    "127.0.0.1:55432".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::from("this is not json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    // Must be in Anthropic error format.
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("JSON"),
        "error message should mention JSON; got: {:?}",
        body["error"]["message"]
    );
    assert_eq!(backend.hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn anthropic_messages_count_tokens_404_exists() {
    // Sanity check: count_tokens is still 404.
    let backend = TestBackend::spawn_anthropic_messages("backend-a").await;
    let state = test_state(
        RoutingStrategy::Priority,
        vec![anthropic_messages_endpoint("backend-a", backend.base_url())],
    );
    let app = router(state);

    let response = app
        .oneshot(json_request(
            "/v1/messages/count_tokens",
            json!({
                "model": PUBLIC_MODEL,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert_eq!(backend.hits(), 0);

    backend.abort();
}

#[tokio::test]
async fn stream_capture_records_upstream_and_client_ndjson_with_monotonic_timestamps() {
    // Enable streaming capture, run a streaming `/v1/responses`
    // request, and verify both NDJSON files exist with strictly
    // non-decreasing `ts_us` values and at least one body_chunk
    // and one SSE event. Mirrors the plan's P1 integration test
    // shape (`upstream_response_ndjson_records_sse_events_with_monotonic_timestamps`).
    let backend = TestBackend::spawn_json_labeled_stream("backend-a").await;
    let mut endpoint = test_backend("backend-a", backend.base_url());
    endpoint.route.stream_capture = true;
    let capture_dir = temp_capture_root("stream-capture-monotonic");
    let debug_capture = DebugCaptureConfig {
        enabled: true,
        mode: DebugCaptureMode::All,
        directory: capture_dir.clone(),
    };
    let state =
        test_state_with_debug_capture(RoutingStrategy::Priority, vec![endpoint], debug_capture);
    let app = router(state.clone());

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello",
                "stream": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(body_bytes.to_vec()).unwrap();
    // The SSE stream was relayed to the client.
    assert!(body.contains("event: response.created"), "body={body}");
    assert!(
        body.contains("event: response.output_text.delta"),
        "body={body}"
    );
    assert!(body.contains("event: response.completed"), "body={body}");
    // Give the streaming writer thread a moment to drain.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(state);

    // The capture directory holds one subdirectory per captured
    // request (named `{timestamp}-{pid}-{seq}`); find it.
    let capture_subdir = std::fs::read_dir(&capture_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .unwrap_or_else(|| panic!("no capture subdirectory in {}", capture_dir.display()))
        .path();
    let upstream_path = capture_subdir.join("upstream_response.ndjson");
    let client_path = capture_subdir.join("client_response.ndjson");
    let metadata_path = capture_subdir.join("metadata.json");

    let upstream_lines: Vec<String> = std::fs::read_to_string(&upstream_path)
        .unwrap_or_else(|error| panic!("upstream_response.ndjson missing: {error}"))
        .lines()
        .map(str::to_owned)
        .collect();
    let client_lines: Vec<String> = std::fs::read_to_string(&client_path)
        .unwrap_or_else(|error| panic!("client_response.ndjson missing: {error}"))
        .lines()
        .map(str::to_owned)
        .collect();

    assert!(
        upstream_lines.len() >= 3,
        "expected ≥3 upstream lines, got {}: {upstream_lines:?}",
        upstream_lines.len()
    );
    let mut upstream_ts = 0u64;
    let mut saw_header = false;
    let mut saw_body_chunk = false;
    let mut saw_done = false;
    for line in &upstream_lines {
        let v: Value = serde_json::from_str(line).unwrap();
        let ts = v["ts_us"].as_u64().unwrap();
        assert!(
            ts >= upstream_ts,
            "upstream ts_us must be non-decreasing: {upstream_ts} -> {ts}"
        );
        upstream_ts = ts;
        match v["kind"].as_str() {
            Some("header") => saw_header = true,
            Some("body_chunk") => saw_body_chunk = true,
            Some("done") => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_header, "expected a header event on upstream side");
    assert!(
        saw_body_chunk,
        "expected a body_chunk event on upstream side"
    );
    assert!(saw_done, "expected a done event on upstream side");

    let mut client_ts = 0u64;
    let mut saw_client_sse = false;
    let mut saw_client_done = false;
    for line in &client_lines {
        let v: Value = serde_json::from_str(line).unwrap();
        let ts = v["ts_us"].as_u64().unwrap();
        assert!(
            ts >= client_ts,
            "client ts_us must be non-decreasing: {client_ts} -> {ts}"
        );
        client_ts = ts;
        match v["kind"].as_str() {
            Some("sse") => saw_client_sse = true,
            Some("done") => saw_client_done = true,
            _ => {}
        }
    }
    assert!(
        saw_client_sse,
        "expected at least one SSE event on client side"
    );
    assert!(saw_client_done, "expected done event on client side");

    let metadata_text = std::fs::read_to_string(&metadata_path).unwrap();
    let metadata: Value = serde_json::from_str(&metadata_text).unwrap();
    assert_eq!(
        metadata["files"]["upstream_response"],
        "upstream_response.ndjson"
    );
    assert_eq!(
        metadata["files"]["client_response"],
        "client_response.ndjson"
    );
    let upstream_timings = &metadata["timings"]["upstream_response"];
    let client_timings = &metadata["timings"]["client_response"];
    assert!(
        upstream_timings.is_object(),
        "missing upstream timings: {metadata_text}"
    );
    assert!(
        client_timings.is_object(),
        "missing client timings: {metadata_text}"
    );
    assert!(upstream_timings["event_count"].as_u64().unwrap() > 0);
    assert!(client_timings["event_count"].as_u64().unwrap() > 0);

    let mut saw_named_event = false;
    for line in &client_lines {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["kind"] == "sse" && v["event"].is_string() {
            saw_named_event = true;
            break;
        }
    }
    assert!(
        saw_named_event,
        "expected at least one sse event with a non-null event field on the client side"
    );

    backend.abort();
    let _ = std::fs::remove_dir_all(&capture_dir);
}

#[tokio::test]
async fn stream_capture_disabled_writes_no_ndjson_files() {
    // When `stream_capture` is false (the default), no NDJSON
    // files are written even though debug_capture is on. The
    // debug_capture itself still runs and writes
    // `inbound.body` / `upstream.body` / `metadata.json`.
    let backend = TestBackend::spawn_json_labeled_stream("backend-a").await;
    let endpoint = test_backend("backend-a", backend.base_url());
    assert!(!endpoint.route.stream_capture);
    let capture_dir = temp_capture_root("stream-capture-disabled");
    let debug_capture = DebugCaptureConfig {
        enabled: true,
        mode: DebugCaptureMode::All,
        directory: capture_dir.clone(),
    };
    let state =
        test_state_with_debug_capture(RoutingStrategy::Priority, vec![endpoint], debug_capture);
    let app = router(state.clone());

    let response = app
        .oneshot(json_request(
            "/v1/responses",
            json!({
                "model": PUBLIC_MODEL,
                "input": "hello",
                "stream": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(state);

    let subdir_entry = std::fs::read_dir(&capture_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false));
    let subdir = subdir_entry
        .unwrap_or_else(|| panic!("debug_capture subdir missing: {}", capture_dir.display()))
        .path();
    assert!(
        !subdir.join("upstream_response.ndjson").exists(),
        "upstream_response.ndjson must not be written when stream_capture is disabled"
    );
    assert!(
        !subdir.join("client_response.ndjson").exists(),
        "client_response.ndjson must not be written when stream_capture is disabled"
    );
    let metadata_text = std::fs::read_to_string(subdir.join("metadata.json")).unwrap();
    let metadata: Value = serde_json::from_str(&metadata_text).unwrap();
    assert!(
        metadata.get("timings").is_none() || metadata["timings"].is_null(),
        "metadata.timings must be absent when stream_capture is disabled: {metadata_text}"
    );

    backend.abort();
    let _ = std::fs::remove_dir_all(&capture_dir);
}

fn temp_capture_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "onair-app-stream-capture-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}
