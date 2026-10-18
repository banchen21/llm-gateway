//! Verifies the OpenAI Chat Completions endpoint, in front of both upstream protocols.
mod common;

use common::{
    chat_body, event_names, parse_sse, post_chat, start_gateway_with, start_upstream, Upstream,
};
use gateway_core::config::ProtocolMode;
use gateway_core::upstream::{MaxTokensField, ProviderProtocol};
use serde_json::{json, Value};

const OPENAI_SSE: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    "\n\n",
    r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"upstream-model","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

async fn route_upstream(
    upstream: &Upstream,
    protocol: ProviderProtocol,
    mode: ProtocolMode,
) -> String {
    start_gateway_with(upstream, protocol, mode, MaxTokensField::default()).await
}

#[tokio::test]
async fn streams_from_an_openai_upstream_terminated_by_done() {
    let upstream = start_upstream("/v1/chat/completions", OPENAI_SSE).await;
    let gateway = route_upstream(&upstream, ProviderProtocol::OpenAi, ProtocolMode::Route).await;

    let response = post_chat(&gateway, &chat_body("test-model", true)).await;
    assert_eq!(response.status(), 200);
    let frames = parse_sse(&response.text().await.unwrap());
    assert_eq!(
        event_names(&frames),
        ["", "", "", "", ""],
        "OpenAI frames carry no event name, only data"
    );

    assert_eq!(frames[0].1["object"], "chat.completion.chunk");
    assert_eq!(frames[0].1["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(frames[1].1["choices"][0]["delta"]["content"], "Hello");
    assert_eq!(frames[2].1["choices"][0]["finish_reason"], "stop");
    // Usage rides on its own chunk with an empty `choices` array.
    assert_eq!(frames[3].1["choices"], json!([]));
    assert_eq!(frames[3].1["usage"]["prompt_tokens"], 5);
    assert_eq!(frames[3].1["usage"]["completion_tokens"], 2);
    assert_eq!(frames[3].1["usage"]["total_tokens"], 7);
}

#[tokio::test]
async fn an_anthropic_upstream_is_presented_as_openai() {
    let upstream = start_upstream("/v1/messages", ANTHROPIC_SSE).await;
    let gateway = route_upstream(&upstream, ProviderProtocol::Anthropic, ProtocolMode::Route).await;

    let response = post_chat(&gateway, &chat_body("test-model", true)).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    let frames = parse_sse(&body);

    assert_eq!(frames[1].1["choices"][0]["delta"]["content"], "Hello");
    assert_eq!(frames[2].1["choices"][0]["finish_reason"], "stop");
    assert_eq!(frames[3].1["usage"]["prompt_tokens"], 5);
    // The stream must end with the sentinel OpenAI clients stop on.
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "the stream must end with the [DONE] sentinel, got: {body:?}"
    );
}

#[tokio::test]
async fn a_non_streaming_chat_request_returns_one_completion() {
    let upstream = start_upstream("/v1/chat/completions", OPENAI_SSE).await;
    let gateway = route_upstream(&upstream, ProviderProtocol::OpenAi, ProtocolMode::Route).await;

    let response = post_chat(&gateway, &chat_body("test-model", false)).await;
    let value: Value = response.json().await.unwrap();

    assert_eq!(value["object"], "chat.completion");
    assert_eq!(value["model"], "test-model");
    assert_eq!(value["choices"][0]["message"]["role"], "assistant");
    assert_eq!(value["choices"][0]["message"]["content"], "Hello");
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
    assert_eq!(value["usage"]["prompt_tokens"], 5);
    assert_eq!(value["usage"]["total_tokens"], 7);
}

#[tokio::test]
async fn the_upstream_receives_a_chat_request_rendered_for_its_own_protocol() {
    let upstream = start_upstream("/v1/messages", ANTHROPIC_SSE).await;
    let gateway = route_upstream(&upstream, ProviderProtocol::Anthropic, ProtocolMode::Route).await;

    let payload = json!({
        "model": "test-model",
        "max_tokens": 128,
        "stream": false,
        "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "check the weather"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_a", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_a", "content": "18C"}
        ],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}],
        "tool_choice": "required"
    });
    post_chat(&gateway, &payload).await;

    let sent = upstream.last_request();
    assert_eq!(sent["model"], "upstream-model");
    assert_eq!(sent["max_tokens"], 128);
    // The leading system message collapses into Anthropic's top-level `system`.
    assert_eq!(
        sent["system"],
        json!([{"type": "text", "text": "be terse"}])
    );
    assert_eq!(sent["messages"][0]["role"], "user");
    // OpenAI's `parameters` becomes Anthropic's `input_schema`.
    assert_eq!(sent["tools"][0]["input_schema"], json!({"type": "object"}));
    assert_eq!(sent["tool_choice"], json!({"type": "any"}));

    // The assistant tool call becomes a `tool_use` block whose input is an object.
    let assistant = &sent["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"][0]["type"], "tool_use");
    assert_eq!(assistant["content"][0]["id"], "call_a");
    assert_eq!(assistant["content"][0]["input"], json!({"city": "Paris"}));

    // The standalone `tool` message folds back into a `tool_result` block in a user message.
    let tool_result = &sent["messages"][2];
    assert_eq!(tool_result["role"], "user");
    assert_eq!(tool_result["content"][0]["type"], "tool_result");
    assert_eq!(tool_result["content"][0]["tool_use_id"], "call_a");
    assert_eq!(tool_result["content"][0]["content"], "18C");
}

#[tokio::test]
async fn the_chat_endpoint_is_unavailable_outside_route_and_openai_modes() {
    let upstream = start_upstream("/v1/chat/completions", OPENAI_SSE).await;

    let anthropic_only = route_upstream(
        &upstream,
        ProviderProtocol::OpenAi,
        ProtocolMode::PrimaryAnthropic,
    )
    .await;
    let response = post_chat(&anthropic_only, &chat_body("test-model", false)).await;
    assert_eq!(response.status(), 404);
    let value: Value = response.json().await.unwrap();
    // Errors on this endpoint come back in the OpenAI envelope.
    assert_eq!(value["error"]["type"], "invalid_request_error");

    let openai_only = route_upstream(
        &upstream,
        ProviderProtocol::OpenAi,
        ProtocolMode::PrimaryOpenAi,
    )
    .await;
    assert_eq!(
        post_chat(&openai_only, &chat_body("test-model", false))
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn models_are_listed_for_protocol_agnostic_discovery() {
    let upstream = start_upstream("/v1/chat/completions", OPENAI_SSE).await;
    let gateway = route_upstream(&upstream, ProviderProtocol::OpenAi, ProtocolMode::Route).await;

    let value: Value = reqwest::get(format!("{gateway}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(value["object"], "list");
    assert_eq!(value["data"][0]["id"], "test-model");
}

#[tokio::test]
async fn token_counting_returns_an_estimate_flagged_as_such() {
    let upstream = start_upstream("/v1/chat/completions", OPENAI_SSE).await;
    let gateway = route_upstream(&upstream, ProviderProtocol::OpenAi, ProtocolMode::Route).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/messages/count_tokens"))
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "a".repeat(400)}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-token-count"], "approximate");
    let value: Value = response.json().await.unwrap();
    assert_eq!(
        value["input_tokens"], 100,
        "400 characters is about 100 tokens"
    );
}
