//! Verifies the Anthropic Messages endpoint in front of an Anthropic-compatible upstream.
mod common;

use common::{
    event_names, messages_body, parse_sse, post_messages, start_gateway, start_gateway_with,
    start_upstream,
};
use gateway_core::config::ProtocolMode;
use gateway_core::upstream::ProviderProtocol;
use serde_json::{json, Value};

/// A text answer followed by a tool call, which also pins down block indexing: the text
/// block takes index 0 and the tool block has to shift to index 1.
const TEXT_AND_TOOL_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_upstream","type":"message","role":"assistant","model":"upstream-model","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me check"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":25}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// Reasoning with a signature that must reach the client, then a plain stop.
const THINKING_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_upstream","type":"message","role":"assistant","model":"upstream-model","content":[],"usage":{"input_tokens":7,"cache_read_input_tokens":3,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing options"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":9}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

#[tokio::test]
async fn streams_text_and_a_tool_call_with_contiguous_block_indices() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let response = post_messages(&gateway, &messages_body("test-model", true)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/event-stream",
        "a streaming request must be answered as SSE"
    );

    let frames = parse_sse(&response.text().await.unwrap());
    assert_eq!(
        event_names(&frames),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    assert_eq!(frames[1].1["index"], 0);
    assert_eq!(frames[1].1["content_block"]["type"], "text");
    assert_eq!(frames[4].1["index"], 1);
    assert_eq!(frames[4].1["content_block"]["type"], "tool_use");
    assert_eq!(frames[4].1["content_block"]["id"], "toolu_1");
    assert_eq!(frames[4].1["content_block"]["name"], "get_weather");

    assert_eq!(frames[2].1["delta"]["text"], "Let me check");
    assert_eq!(frames[5].1["delta"]["partial_json"], r#"{"city":"#);

    let final_delta = &frames[8].1;
    assert_eq!(final_delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(final_delta["usage"]["input_tokens"], 10);
    assert_eq!(final_delta["usage"]["output_tokens"], 25);
}

#[tokio::test]
async fn streams_reasoning_before_text_and_preserves_its_signature() {
    let upstream = start_upstream("/v1/messages", THINKING_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let response = post_messages(&gateway, &messages_body("test-model", true)).await;
    let frames = parse_sse(&response.text().await.unwrap());

    // Reasoning is emitted first, which is what shifts the text block to index 1.
    assert_eq!(frames[1].1["content_block"]["type"], "thinking");
    assert_eq!(frames[1].1["index"], 0);
    assert_eq!(frames[2].1["delta"]["thinking"], "weighing options");

    // The signature is not content: it rides on the block's closing delta so the client
    // can hand the block back on the next turn.
    assert_eq!(frames[3].1["delta"]["type"], "signature_delta");
    assert_eq!(frames[3].1["delta"]["signature"], "sig-abc");
    assert_eq!(frames[3].1["index"], 0);
    assert_eq!(frames[4].0, "content_block_stop");

    assert_eq!(frames[5].1["content_block"]["type"], "text");
    assert_eq!(frames[5].1["index"], 1);

    // Cached input tokens are reported separately and excluded from `input_tokens`.
    let final_delta = &frames[frames.len() - 2].1;
    assert_eq!(final_delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(final_delta["usage"]["input_tokens"], 7);
    assert_eq!(final_delta["usage"]["cache_read_input_tokens"], 3);
    assert_eq!(final_delta["usage"]["output_tokens"], 9);
}

#[tokio::test]
async fn a_non_streaming_request_returns_one_aggregated_message() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 200);
    let value: Value = response.json().await.unwrap();

    assert_eq!(value["type"], "message");
    assert_eq!(value["role"], "assistant");
    assert_eq!(value["model"], "test-model");
    assert_eq!(value["stop_reason"], "tool_use");

    let content = value["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "Let me check");
    assert_eq!(content[1]["type"], "tool_use");
    // The `input_json_delta` fragments must reassemble into an object, not stay a string.
    assert_eq!(content[1]["input"], json!({"city": "Paris"}));
    assert_eq!(value["usage"]["input_tokens"], 10);
    assert_eq!(value["usage"]["output_tokens"], 25);
}

#[tokio::test]
async fn the_upstream_receives_the_ir_rendered_as_anthropic() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let payload = json!({
        "model": "test-model",
        "max_tokens": 128,
        "system": "be terse",
        "stream": false,
        "temperature": 0.25,
        "messages": [
            {"role": "user", "content": "check the weather"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "18C"}
            ]}
        ],
        "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]
    });
    post_messages(&gateway, &payload).await;

    let sent = upstream.last_request();
    // The alias is resolved to the provider's own model id.
    assert_eq!(sent["model"], "upstream-model");
    assert_eq!(sent["max_tokens"], 128);
    assert_eq!(sent["temperature"], 0.25);
    assert_eq!(sent["stream"], true);
    assert_eq!(
        sent["system"],
        json!([{"type": "text", "text": "be terse"}])
    );
    assert_eq!(sent["tools"][0]["name"], "get_weather");
    assert_eq!(sent["tools"][0]["input_schema"], json!({"type": "object"}));
    // The tool result stays inside the user message, as Anthropic carries it.
    assert_eq!(sent["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(sent["messages"][2]["content"][0]["content"], "18C");
    assert_eq!(
        sent["messages"][1]["content"][1]["input"],
        json!({"city": "Paris"})
    );
}

#[tokio::test]
async fn an_unknown_model_is_reported_as_not_found() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let response = post_messages(&gateway, &messages_body("no-such-model", false)).await;
    assert_eq!(response.status(), 404);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn the_messages_endpoint_follows_the_configured_protocol_mode() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;

    let anthropic_only = start_gateway_with(
        &upstream,
        ProviderProtocol::Anthropic,
        ProtocolMode::PrimaryAnthropic,
        Default::default(),
    )
    .await;
    assert_eq!(
        post_messages(&anthropic_only, &messages_body("test-model", false))
            .await
            .status(),
        200
    );

    let openai_only = start_gateway_with(
        &upstream,
        ProviderProtocol::Anthropic,
        ProtocolMode::PrimaryOpenAi,
        Default::default(),
    )
    .await;
    let response = post_messages(&openai_only, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 404);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["error"]["type"], "not_found_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("primary_open_ai"),
        "the error should name the mode suppressing the endpoint"
    );

    let both = start_gateway_with(
        &upstream,
        ProviderProtocol::Anthropic,
        ProtocolMode::Route,
        Default::default(),
    )
    .await;
    assert_eq!(
        post_messages(&both, &messages_body("test-model", false))
            .await
            .status(),
        200
    );
}

/// Claude Code asks for `adaptive` thinking on models it considers able to pick their own
/// budget. The gateway used to reject the request outright, before any provider was chosen.
#[tokio::test]
async fn an_adaptive_thinking_request_reaches_an_anthropic_upstream_as_written() {
    let upstream = start_upstream("/v1/messages", THINKING_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let mut payload = messages_body("test-model", false);
    payload["thinking"] = json!({"type": "adaptive"});

    assert_eq!(post_messages(&gateway, &payload).await.status(), 200);
    // No budget is invented on the way through: the upstream sees the shape the client sent.
    assert_eq!(
        upstream.last_request()["thinking"],
        json!({"type": "adaptive"})
    );
}

/// The explicit-budget shape shares its field with the adaptive one, so it has to keep
/// rendering exactly as before.
#[tokio::test]
async fn a_budgeted_thinking_request_still_renders_as_enabled() {
    let upstream = start_upstream("/v1/messages", THINKING_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let mut payload = messages_body("test-model", false);
    payload["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});

    assert_eq!(post_messages(&gateway, &payload).await.status(), 200);
    assert_eq!(
        upstream.last_request()["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
}

/// Claude Code sends its environment preamble as a `system`-role message inside the
/// conversation, which used to be rejected outright: neither upstream protocol has a slot
/// for a system message mid-conversation.
#[tokio::test]
async fn a_system_role_message_is_folded_into_the_system_prompt() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let payload = json!({
        "model": "test-model",
        "max_tokens": 64,
        "system": [{"type": "text", "text": "be terse"}],
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "system", "content": [{"type": "text", "text": "late context"}]}
        ]
    });
    assert_eq!(post_messages(&gateway, &payload).await.status(), 200);

    let sent = upstream.last_request();
    assert_eq!(sent["system"][0]["text"], "be terse\nlate context");
    assert_eq!(
        sent["messages"].as_array().unwrap().len(),
        1,
        "a system message is context, not a turn"
    );

    // With no top-level prompt to append to, the folded text is the whole system prompt.
    let payload = json!({
        "model": "test-model",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "system", "content": "late context"}
        ]
    });
    assert_eq!(post_messages(&gateway, &payload).await.status(), 200);
    assert_eq!(upstream.last_request()["system"][0]["text"], "late context");
}

/// Claude Code's tool search answers with `tool_reference` blocks. The IR models content,
/// and neither upstream protocol has a place for a reference, so it is dropped rather
/// than failing the turn that carried it.
#[tokio::test]
async fn tool_reference_blocks_are_dropped_instead_of_rejected() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let payload = json!({
        "model": "test-model",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "find a weather tool"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "tool_search", "input": {"query": "weather"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                    {"type": "tool_reference", "tool_name": "get_weather"},
                    {"type": "text", "text": "found one"}
                ]}
            ]}
        ]
    });

    assert_eq!(post_messages(&gateway, &payload).await.status(), 200);

    let sent = upstream.last_request();
    assert!(
        !sent.to_string().contains("tool_reference"),
        "a reference has no upstream representation"
    );
    assert_eq!(sent["messages"][2]["content"][0]["content"], "found one");
}

/// Accounts sold separately carry separate quotas, so a 429 on one key must not end the
/// request while another key is configured.
#[tokio::test]
async fn a_rate_limited_key_falls_through_to_the_next_one() {
    use axum::http::{header::CONTENT_TYPE, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;
    use common::{serve_app, temp_config_path};
    use gateway_core::config::{ApiKey, Config, Provider, ProviderModel};
    use gateway_core::upstream::MaxTokensField;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap| {
            let recorder = recorder.clone();
            async move {
                let key = headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                recorder.lock().unwrap().push(key.clone());
                let response: Response = if key == "key-a" {
                    (StatusCode::TOO_MANY_REQUESTS, "quota exhausted").into_response()
                } else {
                    ([(CONTENT_TYPE, "text/event-stream")], TEXT_AND_TOOL_SSE).into_response()
                };
                response
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let config = Config {
        protocol_mode: ProtocolMode::PrimaryAnthropic,
        providers: vec![Provider {
            id: "p1".into(),
            name: "mock".into(),
            protocol: ProviderProtocol::Anthropic,
            base_url: format!("http://{address}"),
            api_key: "key-a".into(),
            api_keys: vec![ApiKey {
                id: "k2".into(),
                name: "second account".into(),
                key: "key-b".into(),
            }],
            enabled: true,
            custom_headers: Default::default(),
            models: vec![ProviderModel {
                id: "m1".into(),
                alias: "test-model".into(),
                upstream_model: "upstream-model".into(),
                enabled: true,
                max_output_tokens: None,
                max_tokens_field: MaxTokensField::default(),
                extra: Default::default(),
            }],
        }],
        ..Config::default()
    };
    let gateway = serve_app(config, temp_config_path()).await;

    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 200);

    let keys = seen.lock().unwrap().clone();
    assert_eq!(
        keys,
        vec!["key-a", "key-b"],
        "the second key should be tried once the first is rate limited"
    );
}

/// When the provider an alias names refuses (quota, credential, plan), another enabled
/// provider answers instead of the client seeing the refusal.
#[tokio::test]
async fn a_refusing_provider_falls_over_to_another() {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::Router;
    use common::{serve_app, temp_config_path};
    use gateway_core::config::{Config, Provider, ProviderModel};
    use gateway_core::upstream::MaxTokensField;
    use tokio::net::TcpListener;

    let refusing = Router::new().route(
        "/v1/messages",
        post(|| async { (StatusCode::TOO_MANY_REQUESTS, "quota exhausted").into_response() }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, refusing).await.unwrap() });

    let backup = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;

    let model = |alias: &str, upstream: &str| ProviderModel {
        id: upstream.into(),
        alias: alias.into(),
        upstream_model: upstream.into(),
        enabled: true,
        max_output_tokens: None,
        max_tokens_field: MaxTokensField::default(),
        extra: Default::default(),
    };
    let config = Config {
        protocol_mode: ProtocolMode::PrimaryAnthropic,
        failover: true,
        providers: vec![
            Provider {
                id: "primary".into(),
                name: "exhausted".into(),
                protocol: ProviderProtocol::Anthropic,
                base_url: format!("http://{primary_address}"),
                api_key: "k".into(),
                api_keys: Vec::new(),
                enabled: true,
                custom_headers: Default::default(),
                models: vec![model("test-model", "primary-model")],
            },
            Provider {
                id: "backup".into(),
                name: "backup".into(),
                protocol: ProviderProtocol::Anthropic,
                base_url: backup.base_url.clone(),
                api_key: "k".into(),
                api_keys: Vec::new(),
                enabled: true,
                custom_headers: Default::default(),
                models: vec![model("backup-model", "backup-model")],
            },
        ],
        ..Config::default()
    };
    let gateway = serve_app(config, temp_config_path()).await;

    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(backup.last_request()["model"], "backup-model");
}

/// "Working but slow" never returns an error, so only a clock can tell it apart from a
/// provider that is thinking. The gateway abandons the route and answers elsewhere.
#[tokio::test]
async fn a_stalling_route_is_abandoned_for_another_provider() {
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::Router;
    use common::{serve_app, temp_config_path};
    use gateway_core::config::{Config, Provider, ProviderModel};
    use gateway_core::upstream::MaxTokensField;
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;

    let slow = Router::new().route(
        "/v1/messages",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            ([(CONTENT_TYPE, "text/event-stream")], TEXT_AND_TOOL_SSE).into_response()
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, slow).await.unwrap() });

    let backup = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;

    let model = |alias: &str, upstream: &str| ProviderModel {
        id: upstream.into(),
        alias: alias.into(),
        upstream_model: upstream.into(),
        enabled: true,
        max_output_tokens: None,
        max_tokens_field: MaxTokensField::default(),
        extra: Default::default(),
    };
    let config = Config {
        protocol_mode: ProtocolMode::PrimaryAnthropic,
        failover: true,
        slow_after_ms: 300,
        providers: vec![
            Provider {
                id: "slow".into(),
                name: "slow".into(),
                protocol: ProviderProtocol::Anthropic,
                base_url: format!("http://{slow_address}"),
                api_key: "k".into(),
                api_keys: Vec::new(),
                enabled: true,
                custom_headers: Default::default(),
                models: vec![model("test-model", "slow-model")],
            },
            Provider {
                id: "backup".into(),
                name: "backup".into(),
                protocol: ProviderProtocol::Anthropic,
                base_url: backup.base_url.clone(),
                api_key: "k".into(),
                api_keys: Vec::new(),
                enabled: true,
                custom_headers: Default::default(),
                models: vec![model("backup-model", "backup-model")],
            },
        ],
        ..Config::default()
    };
    let gateway = serve_app(config, temp_config_path()).await;

    let started = Instant::now();
    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), 200);
    assert_eq!(backup.last_request()["model"], "backup-model");
    assert!(
        elapsed < Duration::from_secs(3),
        "the stalled route should have been abandoned, took {elapsed:?}"
    );
}

/// A fallback or a stall can move a request to a provider the client never asked for, so
/// the answer says who actually served it.
#[tokio::test]
async fn the_response_names_the_provider_that_answered() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-gateway-provider"], "mock");
    assert_eq!(
        response.headers()["x-llm-gateway-upstream"],
        "upstream-model"
    );
}
