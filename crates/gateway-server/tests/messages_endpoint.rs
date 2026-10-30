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

/// axum's 2 MiB default body cap must not become the gateway's: one conversation with a
/// couple of screenshots is larger than that, and the client would see a bare 413 before
/// the request was ever routed.
#[tokio::test]
async fn a_body_larger_than_axums_default_limit_is_accepted_and_forwarded_whole() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let padding = "a".repeat(3 * 1024 * 1024);
    let mut payload = messages_body("test-model", false);
    payload["messages"][0]["content"] = json!(padding);

    let response = post_messages(&gateway, &payload).await;
    assert_eq!(response.status(), 200);

    // The upstream receives Anthropic-shaped content blocks, and the text is intact.
    let sent = upstream.last_request();
    let forwarded = sent["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(forwarded.len(), padding.len());
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

/// Some compatible endpoints route on a header instead of the URL, so the headers a
/// provider carries in its configuration have to reach the upstream request unchanged.
#[tokio::test]
async fn provider_custom_headers_reach_the_upstream() {
    use axum::http::{header::CONTENT_TYPE, HeaderMap};
    use axum::routing::post;
    use axum::Router;
    use common::{serve_app, temp_config_path};
    use gateway_core::config::{Config, Provider, ProviderModel};
    use gateway_core::upstream::MaxTokensField;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap| {
            let recorder = recorder.clone();
            async move {
                let name = "x-opencode-session";
                let value = headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                recorder.lock().unwrap().push((name.to_owned(), value));
                ([(CONTENT_TYPE, "text/event-stream")], TEXT_AND_TOOL_SSE)
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
            api_key: "key".into(),
            api_keys: Vec::new(),
            enabled: true,
            custom_headers: BTreeMap::from([("x-opencode-session".into(), "session-abc".into())]),
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

    assert_eq!(
        post_messages(&gateway, &messages_body("test-model", false))
            .await
            .status(),
        200
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [("x-opencode-session".to_owned(), "session-abc".to_owned())],
        "the configured header must be sent upstream"
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

/// CommandCode reports an exhausted balance as 400. Provider failover must treat that
/// like any other account refusal, not as a malformed request.
#[tokio::test]
async fn a_400_balance_error_falls_over_to_another_provider() {
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
        post(|| async {
            (
                StatusCode::BAD_REQUEST,
                r#"{"error":{"message":"You have insufficient credits to make this request.","type":"invalid_request_error"}}"#,
            )
                .into_response()
        }),
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
                name: "out of credit".into(),
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

/// A fallback reached through a refusal must be sanitized the same way a fallback reached
/// through a stall is.
#[tokio::test]
async fn a_refusal_fallback_does_not_receive_another_providers_thinking() {
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

    let payload = json!({
        "model": "test-model",
        "max_tokens": 64,
        "stream": false,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "the primary's reasoning", "signature": "sig-primary"},
                {"type": "text", "text": "hi"}
            ]},
            {"role": "user", "content": "again"}
        ]
    });
    let response = post_messages(&gateway, &payload).await;
    assert_eq!(response.status(), 200);

    let sent = backup.last_request();
    assert_eq!(sent["model"], "backup-model");
    assert!(
        sent.get("thinking").is_none(),
        "thinking mode must be off for a provider that never issued the blocks"
    );
    assert!(
        !sent.to_string().contains("\"thinking\""),
        "no reasoning block may be replayed to another provider: {sent}"
    );
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

    // The stalled route already produced reasoning that the backup never issued. Replaying
    // it would be rejected by an endpoint that runs in thinking mode (DeepSeek does), so
    // the fallback must see neither the blocks nor the switch.
    let payload = json!({
        "model": "test-model",
        "max_tokens": 64,
        "stream": false,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "the slow provider's reasoning", "signature": "sig-slow"},
                {"type": "text", "text": "hi"}
            ]},
            {"role": "user", "content": "again"}
        ]
    });
    let started = Instant::now();
    let response = post_messages(&gateway, &payload).await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), 200);
    let sent = backup.last_request();
    assert_eq!(sent["model"], "backup-model");
    assert!(
        sent.get("thinking").is_none(),
        "thinking mode must be off for a provider that never issued the blocks"
    );
    assert!(
        !sent.to_string().contains("\"thinking\""),
        "no reasoning block may be replayed to another provider: {sent}"
    );
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

/// Reasoning that came from an OpenAI-compatible upstream has no signature. The official
/// API requires one, compatible vendors instead require the block itself to be present.
#[tokio::test]
async fn unsigned_reasoning_is_replayed_to_a_compatible_upstream() {
    let upstream = start_upstream("/v1/messages", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::Anthropic).await;

    let payload = json!({
        "model": "test-model",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "weighing options"},
                {"type": "text", "text": "hi"}
            ]},
            {"role": "user", "content": "again"}
        ]
    });
    assert_eq!(post_messages(&gateway, &payload).await.status(), 200);

    let sent = upstream.last_request();
    assert_eq!(sent["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(
        sent["messages"][1]["content"][0]["thinking"],
        "weighing options"
    );
    assert!(
        sent["messages"][1]["content"][0].get("signature").is_none(),
        "a signature is not invented"
    );
}

/// A provider can send its opening event and then fall silent. The connection stays
/// healthy, so only a clock between events can end the wait; without it the client hangs
/// for as long as the provider stays quiet.
#[tokio::test]
async fn a_stream_that_goes_quiet_is_ended_with_a_timeout_error() {
    use axum::body::Body;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::Router;
    use bytes::Bytes;
    use common::{serve_state, temp_config_path};
    use gateway_core::config::{Config, Provider, ProviderModel};
    use gateway_core::upstream::MaxTokensField;
    use gateway_server::state::AppState;
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;

    let quiet = Router::new().route(
        "/v1/messages",
        post(|| async {
            let body = Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::io::Error>(Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"upstream-model\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                ));
                // Still connected, but it has nothing more to say.
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
            ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, quiet).await.unwrap() });

    let config = Config {
        protocol_mode: ProtocolMode::PrimaryAnthropic,
        stream_idle_ms: 300,
        providers: vec![Provider {
            id: "p1".into(),
            name: "quiet".into(),
            protocol: ProviderProtocol::Anthropic,
            base_url: format!("http://{address}"),
            api_key: "k".into(),
            api_keys: Vec::new(),
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
    let state = AppState::new(config, temp_config_path()).unwrap();
    let gateway = serve_state(state.clone()).await;

    let started = Instant::now();
    let response = post_messages(&gateway, &messages_body("test-model", true)).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("timeout_error"),
        "the stream should end with a timeout error, got: {body}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the idle timeout should end the stream quickly, took {:?}",
        started.elapsed()
    );
    assert!(
        state.is_slow("p1/m1"),
        "a route that stalled mid-stream must be avoided next time"
    );
}

/// A non-streaming client has received nothing when the upstream stream fails, so unlike
/// a streaming client it can still be answered by another provider.
#[tokio::test]
async fn a_failed_whole_response_is_replayed_elsewhere() {
    use axum::body::Body;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::Router;
    use bytes::Bytes;
    use common::{serve_app, temp_config_path};
    use gateway_core::config::{Config, Provider, ProviderModel};
    use gateway_core::upstream::MaxTokensField;
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;

    let quiet = Router::new().route(
        "/v1/messages",
        post(|| async {
            let body = Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::io::Error>(Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"upstream-model\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                ));
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
            ([(CONTENT_TYPE, "text/event-stream")], body).into_response()
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, quiet).await.unwrap() });

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
        stream_idle_ms: 300,
        providers: vec![
            Provider {
                id: "primary".into(),
                name: "quiet".into(),
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

    let started = Instant::now();
    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 200);
    let provider = response.headers()["x-llm-gateway-provider"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.text().await.unwrap();
    assert_eq!(provider, "backup");
    assert!(
        body.contains("Let me check"),
        "the backup's answer should be returned, got: {body}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the stalled attempt should not delay the replay, took {:?}",
        started.elapsed()
    );
    assert_eq!(backup.last_request()["model"], "backup-model");
}

/// Quota ranking decides which account is tried first, and a refusal takes that account
/// out of the running for the next request.
#[tokio::test]
async fn quota_ranking_uses_the_best_account_and_skips_a_refused_one() {
    use axum::http::{header::CONTENT_TYPE, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;
    use common::{serve_state, temp_config_path};
    use gateway_core::config::{ApiKey, Config, Provider, ProviderModel};
    use gateway_core::upstream::MaxTokensField;
    use gateway_server::quota::{KeyQuota, ProviderQuota, WindowUsage};
    use gateway_server::state::AppState;
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
                let response: Response = match key.as_str() {
                    // CommandCode reports an exhausted balance as a 400, not a 429.
                    "key-best" => (
                        StatusCode::BAD_REQUEST,
                        r#"{"error":{"message":"You have insufficient credits to make this request.","type":"invalid_request_error"}}"#,
                    )
                        .into_response(),
                    "key-exceeded" => {
                        (StatusCode::TOO_MANY_REQUESTS, "weekly limit").into_response()
                    }
                    _ => {
                        ([(CONTENT_TYPE, "text/event-stream")], TEXT_AND_TOOL_SSE).into_response()
                    }
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
            api_key: "key-low".into(),
            api_keys: vec![
                ApiKey {
                    id: "k2".into(),
                    name: "exceeded".into(),
                    key: "key-exceeded".into(),
                },
                ApiKey {
                    id: "k3".into(),
                    name: "best".into(),
                    key: "key-best".into(),
                },
            ],
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

    let reset = gateway_server::quota::now_ms() + 3_600_000;
    let state = AppState::new(config, temp_config_path()).unwrap();
    state.store_quota(ProviderQuota {
        provider_id: "p1".into(),
        supported: true,
        fetched_at: gateway_server::quota::now_ms(),
        keys: vec![
            KeyQuota {
                key: "key-low".into(),
                monthly_credits: Some(2.0),
                ..KeyQuota::default()
            },
            KeyQuota {
                key: "key-exceeded".into(),
                monthly_credits: Some(34.9),
                weekly: Some(WindowUsage {
                    used: 35.0,
                    cap: 35.0,
                    exceeded: true,
                    reset_at: Some(reset),
                }),
                ..KeyQuota::default()
            },
            KeyQuota {
                key: "key-best".into(),
                monthly_credits: Some(55.8),
                weekly: Some(WindowUsage {
                    used: 14.2,
                    cap: 35.0,
                    exceeded: false,
                    reset_at: Some(reset),
                }),
                ..KeyQuota::default()
            },
        ],
    });
    let gateway = serve_state(state).await;

    // The account with the most usable credit is tried first; the exhausted one is not.
    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(seen.lock().unwrap().clone(), vec!["key-best", "key-low"]);

    // `key-best` was refused, so the next request starts with the account that answered.
    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec!["key-best", "key-low", "key-low"]
    );
}
