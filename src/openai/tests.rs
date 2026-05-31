use super::*;

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
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
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
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
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
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::LlamacppCompat,
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
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
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
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
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
            {"role": "user", "content": "Thanks!"}
        ]
    });

    let rewritten = rewrite_request_body_for_mode_with_tool_schema_mode(
        body.to_string().as_bytes(),
        Some("application/json"),
        Some("backend-model"),
        RequestMode::ResponsesViaChatCompletions,
        ToolSchemaMode::Preserve,
    )
    .unwrap();
    let rewritten: Value = serde_json::from_slice(&rewritten).unwrap();
    let messages = rewritten["messages"].as_array().unwrap();

    assert_eq!(messages.len(), 4);
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
    assert_eq!(messages[3]["role"], "user");
    assert_eq!(messages[3]["content"], "Thanks!");
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
fn native_stream_response_adds_missing_total_tokens() {
    let mut normalizer = SseNormalizer::new(None, None);
    let chunk = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": {
            "prompt_tokens": 6,
            "completion_tokens": 2
        }
    });
    let output = normalizer.push(format!("data: {chunk}\n\n").as_bytes());
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("\"total_tokens\":8"));
    assert_eq!(normalizer.usage.input, 6);
    assert_eq!(normalizer.usage.output, 2);
    assert_eq!(normalizer.usage.total, 8);
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
