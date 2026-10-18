//! Verifies the Anthropic Messages endpoint in front of an OpenAI-compatible upstream —
//! the Anthropic -> IR -> OpenAI -> IR -> Anthropic round trip.
mod common;

use common::{
    event_names, messages_body, parse_sse, post_messages, start_gateway, start_gateway_with,
    start_upstream,
};
use gateway_core::config::ProtocolMode;
use gateway_core::upstream::{MaxTokensField, ProviderProtocol};
use serde_json::{json, Value};

/// Text, then a tool call, then the trailing usage chunk — all with an empty `choices`
/// array, which is how Chat Completions reports usage for a streamed response.
const TEXT_AND_TOOL_SSE: &str = concat!(
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Let me check"},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "\n\n",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":25,"total_tokens":35}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// Verifies the two asymmetries that break naive OpenAI parsers: a server that resends
/// the whole id and name on every chunk, and a server that reports `stop` for a turn
/// that actually called tools.
const RESENT_ID_AND_PLAIN_STOP_SSE: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"get_weather","arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"get_weather","arguments":"\"Paris\"}"}}]},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// Two tool calls whose argument fragments arrive interleaved by index.
const PARALLEL_TOOLS_SSE: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"one","arguments":"{}"}},{"index":1,"id":"call_b","function":{"name":"two","arguments":"{}"}}]},"finish_reason":null}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn an_openai_upstream_is_presented_as_anthropic() {
    let upstream = start_upstream("/v1/chat/completions", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::OpenAi).await;

    let response = post_messages(&gateway, &messages_body("test-model", true)).await;
    assert_eq!(response.status(), 200);
    let frames = parse_sse(&response.text().await.unwrap());

    assert_eq!(
        event_names(&frames),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            // The text block is closed before the tool block opens, which is how
            // Anthropic orders blocks. Chat Completions never says text ended.
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
    assert_eq!(frames[2].1["delta"]["text"], "Let me check");
    assert_eq!(frames[3].1["index"], 0);

    assert_eq!(frames[4].1["index"], 1);
    assert_eq!(frames[4].1["content_block"]["type"], "tool_use");
    assert_eq!(frames[4].1["content_block"]["id"], "call_abc");
    assert_eq!(frames[4].1["content_block"]["name"], "get_weather");
    assert_eq!(frames[5].1["delta"]["partial_json"], r#"{"city":"#);

    let final_delta = &frames[8].1;
    assert_eq!(final_delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(final_delta["usage"]["input_tokens"], 10);
    assert_eq!(final_delta["usage"]["output_tokens"], 25);
}

#[tokio::test]
async fn a_resent_id_is_not_concatenated_and_a_plain_stop_still_means_tool_use() {
    let upstream = start_upstream("/v1/chat/completions", RESENT_ID_AND_PLAIN_STOP_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::OpenAi).await;

    let response = post_messages(&gateway, &messages_body("test-model", true)).await;
    let frames = parse_sse(&response.text().await.unwrap());

    let tool_start = frames
        .iter()
        .find(|(name, _)| name == "content_block_start")
        .expect("a tool block should have opened");
    assert_eq!(
        tool_start.1["content_block"]["id"], "call_abc",
        "a resent id must not be appended to itself"
    );
    assert_eq!(tool_start.1["content_block"]["name"], "get_weather");

    let final_delta = &frames[frames.len() - 2].1;
    assert_eq!(
        final_delta["delta"]["stop_reason"], "tool_use",
        "a plain `stop` from an upstream that emitted tools still means a tool turn"
    );
}

#[tokio::test]
async fn parallel_tool_calls_keep_their_own_indices_and_arguments() {
    let upstream = start_upstream("/v1/chat/completions", PARALLEL_TOOLS_SSE).await;
    let gateway = start_gateway(&upstream, ProviderProtocol::OpenAi).await;

    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    let value: Value = response.json().await.unwrap();
    let content = value["content"].as_array().unwrap();

    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "tool_use");
    assert_eq!(content[0]["id"], "call_a");
    assert_eq!(content[0]["name"], "one");
    assert_eq!(content[1]["id"], "call_b");
    assert_eq!(content[1]["name"], "two");
    assert_eq!(value["stop_reason"], "tool_use");
}

#[tokio::test]
async fn the_upstream_receives_the_ir_rendered_as_openai() {
    let upstream = start_upstream("/v1/chat/completions", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway_with(
        &upstream,
        ProviderProtocol::OpenAi,
        ProtocolMode::PrimaryAnthropic,
        MaxTokensField::MaxTokens,
    )
    .await;

    let payload = json!({
        "model": "test-model",
        "max_tokens": 128,
        "system": "be terse",
        "stream": false,
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
        "tools": [{"name": "get_weather", "description": "weather", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"}
    });
    post_messages(&gateway, &payload).await;

    let sent = upstream.last_request();
    assert_eq!(sent["model"], "upstream-model");
    assert_eq!(sent["max_tokens"], 128);
    assert_eq!(sent["stream"], true);
    // Usage has to be requested explicitly or an OpenAI-compatible upstream omits it.
    assert_eq!(sent["stream_options"]["include_usage"], true);
    // The system prompt becomes a leading message rather than a top-level field.
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "be terse");
    // Anthropic's `input_schema` becomes OpenAI's `parameters` under a function wrapper.
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "get_weather");
    assert_eq!(
        sent["tools"][0]["function"]["parameters"],
        json!({"type": "object"})
    );
    // Anthropic's `any` becomes OpenAI's `required`.
    assert_eq!(sent["tool_choice"], "required");

    // The assistant tool call becomes `tool_calls` with stringified arguments...
    let assistant = &sent["messages"][2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "checking");
    assert_eq!(assistant["tool_calls"][0]["id"], "toolu_1");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"], "{\"city\":\"Paris\"}",
        "OpenAI carries arguments as a JSON string, not an object"
    );
    // ...and the tool result becomes its own `tool` message.
    let tool = &sent["messages"][3];
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "toolu_1");
    assert_eq!(tool["content"], "18C");
}

#[tokio::test]
async fn the_output_limit_field_follows_the_model_setting() {
    let upstream = start_upstream("/v1/chat/completions", TEXT_AND_TOOL_SSE).await;

    let modern = start_gateway_with(
        &upstream,
        ProviderProtocol::OpenAi,
        ProtocolMode::PrimaryAnthropic,
        MaxTokensField::MaxCompletionTokens,
    )
    .await;
    post_messages(&modern, &messages_body("test-model", false)).await;
    let sent = upstream.last_request();
    assert_eq!(sent["max_completion_tokens"], 64);
    assert!(
        sent.get("max_tokens").is_none(),
        "only one output limit field may be sent"
    );
}

/// Anthropic's adaptive thinking has no Chat Completions equivalent, so it is dropped
/// rather than translated into a field the upstream would reject.
#[tokio::test]
async fn a_request_to_reason_is_accepted_and_left_out_of_an_openai_body() {
    let upstream = start_upstream("/v1/chat/completions", TEXT_AND_TOOL_SSE).await;
    let gateway = start_gateway_with(
        &upstream,
        ProviderProtocol::OpenAi,
        ProtocolMode::PrimaryAnthropic,
        Default::default(),
    )
    .await;

    let mut payload = messages_body("test-model", false);
    payload["thinking"] = json!({"type": "adaptive"});

    assert_eq!(post_messages(&gateway, &payload).await.status(), 200);
    assert!(
        upstream.last_request().get("thinking").is_none(),
        "OpenAI carries reasoning as `reasoning_effort`, not as an Anthropic block"
    );
}
