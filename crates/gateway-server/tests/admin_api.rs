//! Verifies the administrative API: configuration round-tripping, validation, the token
//! gate, and request logging.
mod common;

use std::path::Path;

use common::{
    messages_body, parse_sse, post_messages, provider_for, serve_state, start_upstream,
    temp_config_path,
};
use gateway_core::config::{Config, ProtocolMode, WebUiConfig};
use gateway_core::upstream::ProviderProtocol;
use gateway_server::state::AppState;
use serde_json::{json, Value};

const TEXT_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"u","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// Starts a gateway whose configuration file lives at `path`, and returns its URL.
async fn gateway_at(path: &Path) -> String {
    serve_state(
        AppState::new(
            Config {
                providers: Vec::new(),
                default_provider_id: None,
                ..Config::default()
            },
            path.to_path_buf(),
        )
        .unwrap(),
    )
    .await
}

async fn get_config(gateway: &str) -> Value {
    reqwest::get(format!("{gateway}/admin/config"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn put_config(gateway: &str, config: Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(format!("{gateway}/admin/config"))
        .json(&config)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_saved_provider_is_persisted_and_becomes_routable_immediately() {
    let upstream = start_upstream("/v1/messages", TEXT_SSE).await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    // Nothing is routable before a provider is added.
    assert_eq!(
        post_messages(&gateway, &messages_body("test-model", false))
            .await
            .status(),
        404
    );

    let mut provider =
        serde_json::to_value(provider_for(&upstream, ProviderProtocol::Anthropic)).unwrap();
    // The GUI sends providers without ids, expecting the gateway to assign them.
    provider["id"] = json!("");
    provider["models"][0]["id"] = json!("");
    let response = put_config(
        &gateway,
        json!({
            "protocol_mode": "route",
            "providers": [provider],
        }),
    )
    .await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());

    // No restart: the new alias resolves on the next request.
    assert_eq!(
        post_messages(&gateway, &messages_body("test-model", false))
            .await
            .status(),
        200
    );

    // Identifiers were assigned and written to disk.
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        !saved["providers"][0]["id"].as_str().unwrap().is_empty(),
        "a provider sent without an id should be given one"
    );
    assert!(!saved["providers"][0]["models"][0]["id"]
        .as_str()
        .unwrap()
        .is_empty());
    assert_eq!(saved["protocol_mode"], "route");

    // And the running config reports them.
    let current = get_config(&gateway).await;
    assert_eq!(current["aliases"], json!(["test-model"]));
}

#[tokio::test]
async fn a_configuration_with_a_duplicate_alias_is_rejected_and_not_published() {
    let upstream = start_upstream("/v1/messages", TEXT_SSE).await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    let provider =
        serde_json::to_value(provider_for(&upstream, ProviderProtocol::Anthropic)).unwrap();
    let mut duplicate = provider.clone();
    duplicate["id"] = json!("p2");
    duplicate["name"] = json!("other");
    duplicate["models"][0]["id"] = json!("m2");

    let response = put_config(&gateway, json!({"providers": [provider, duplicate]})).await;
    assert_eq!(response.status(), 400);
    let error: Value = response.json().await.unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("test-model"),
        "the error should name the conflicting alias"
    );

    // The rejected configuration must not have been published or written.
    assert!(get_config(&gateway).await["config"]["providers"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(
        !path.exists(),
        "a rejected configuration must not be written"
    );
}

#[tokio::test]
async fn a_web_gui_token_gates_the_administrative_routes() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(
        &gateway,
        json!({"web_ui": {"enabled": true, "token": "s3cret"}}),
    )
    .await;

    let unauthenticated = reqwest::get(format!("{gateway}/admin/config"))
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);

    let wrong = reqwest::Client::new()
        .get(format!("{gateway}/admin/config"))
        .header("x-gateway-token", "nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let right = reqwest::Client::new()
        .get(format!("{gateway}/admin/config"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(right.status(), 200);
}

/// A provider whose models exist is all `/v1/models` needs, so these tests stay offline;
/// `api_key` on the provider is never used by that route.
fn config_with_named_keys() -> Value {
    json!({
        "server": {"api_keys": [
            {"name": "desktop", "key": "key-desktop"},
            {"name": "cursor", "key": "key-cursor"}
        ]},
        "providers": [{
            "id": "p1", "name": "mock", "protocol": "anthropic",
            "base_url": "http://127.0.0.1:1", "api_key": "k", "enabled": true,
            "models": [{"id": "m1", "alias": "test-model", "upstream_model": "u", "enabled": true}]
        }]
    })
}

#[tokio::test]
async fn every_named_inbound_key_is_accepted_on_either_header() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(&gateway, config_with_named_keys()).await;

    let anonymous = reqwest::get(format!("{gateway}/v1/models")).await.unwrap();
    assert_eq!(
        anonymous.status(),
        401,
        "a named key list must gate the API"
    );

    let by_x_api_key = reqwest::Client::new()
        .get(format!("{gateway}/v1/models"))
        .header("x-api-key", "key-desktop")
        .send()
        .await
        .unwrap();
    assert_eq!(by_x_api_key.status(), 200);

    let by_bearer = reqwest::Client::new()
        .get(format!("{gateway}/v1/models"))
        .bearer_auth("key-cursor")
        .send()
        .await
        .unwrap();
    assert_eq!(by_bearer.status(), 200);

    let wrong = reqwest::Client::new()
        .get(format!("{gateway}/v1/models"))
        .header("x-api-key", "not-a-key")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
}

/// The single key predates named keys; a configured gateway must not lose it on upgrade.
#[tokio::test]
async fn the_legacy_inbound_key_still_authenticates() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    let mut config = config_with_named_keys();
    config["server"] = json!({"api_key": "legacy-key"});
    put_config(&gateway, config).await;

    let anonymous = reqwest::get(format!("{gateway}/v1/models")).await.unwrap();
    assert_eq!(anonymous.status(), 401);

    let legacy = reqwest::Client::new()
        .get(format!("{gateway}/v1/models"))
        .bearer_auth("legacy-key")
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.status(), 200);
}

#[tokio::test]
async fn a_configuration_that_would_expose_the_gui_on_a_public_address_is_rejected() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    let response = put_config(
        &gateway,
        json!({
            "server": {"bind": "0.0.0.0", "port": 8787},
            "web_ui": {"enabled": true},
        }),
    )
    .await;
    assert_eq!(response.status(), 400);
    let error: Value = response.json().await.unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("web_ui.token"),
        "the error should explain how to fix it"
    );
}

#[tokio::test]
async fn served_requests_are_recorded_with_their_token_usage() {
    let upstream = start_upstream("/v1/messages", TEXT_SSE).await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(
        &gateway,
        json!({"providers": [serde_json::to_value(provider_for(&upstream, ProviderProtocol::Anthropic)).unwrap()]}),
    )
    .await;

    // A non-streaming request reports usage immediately...
    post_messages(&gateway, &messages_body("test-model", false)).await;
    // ...and a streaming one only after its last frame has been sent.
    let streamed = post_messages(&gateway, &messages_body("test-model", true)).await;
    let _ = parse_sse(&streamed.text().await.unwrap());

    let entries = wait_for_entries(&gateway, 2).await;
    assert_eq!(entries.len(), 2);

    let buffered = entries
        .iter()
        .find(|e| e["stream"] == json!(false))
        .unwrap();
    assert_eq!(buffered["status"], 200);
    assert_eq!(buffered["model"], "test-model");
    assert_eq!(buffered["provider"], "mock");
    assert_eq!(buffered["upstream_model"], "upstream-model");
    assert_eq!(buffered["usage"]["input_tokens"], 10);
    assert_eq!(buffered["usage"]["output_tokens"], 7);
    assert_eq!(buffered["prompt"]["messages"], 1);

    let streaming = entries.iter().find(|e| e["stream"] == json!(true)).unwrap();
    assert_eq!(
        streaming["usage"]["output_tokens"], 7,
        "usage must be captured from the event stream, not lost with it"
    );
}

#[tokio::test]
async fn a_failed_upstream_call_is_recorded_with_its_status() {
    let upstream = start_upstream("/v1/messages", TEXT_SSE).await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(
        &gateway,
        json!({"providers": [serde_json::to_value(provider_for(&upstream, ProviderProtocol::Anthropic)).unwrap()]}),
    )
    .await;

    // No alias matches, so nothing is sent upstream.
    assert_eq!(
        post_messages(&gateway, &messages_body("unknown", false))
            .await
            .status(),
        404
    );
    // A routing failure is not a served request, so nothing is recorded for it.
    let entries: Value = reqwest::get(format!("{gateway}/admin/logs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(entries["entries"].as_array().unwrap().is_empty());
}

/// Polls the log until `count` entries exist, since a streamed request is recorded only
/// after its body has been fully consumed.
async fn wait_for_entries(gateway: &str, count: usize) -> Vec<Value> {
    for _ in 0..100 {
        let value: Value = reqwest::get(format!("{gateway}/admin/logs"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let entries = value["entries"].as_array().cloned().unwrap_or_default();
        if entries.len() >= count {
            return entries;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the request log never reached {count} entries");
}

#[tokio::test]
async fn health_reports_what_the_gui_needs_to_display() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    let value: Value = reqwest::get(format!("{gateway}/admin/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["protocol_mode"], "primary_anthropic");
    assert_eq!(value["providers"], 0);
    assert_eq!(value["web_ui_enabled"], false);
    // Nothing has been written yet, so the file is absent rather than unreadable.
    assert_eq!(value["config_readable"], true);
}

#[tokio::test]
async fn an_unreachable_upstream_is_recorded_as_a_failure() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    // Port 0 is never a listening service.
    put_config(
        &gateway,
        json!({"providers": [{
            "id": "p1", "name": "dead", "protocol": "anthropic",
            "base_url": "http://127.0.0.1:1", "api_key": "k", "enabled": true,
            "models": [{"id": "m1", "alias": "test-model", "upstream_model": "u", "enabled": true}]
        }]}),
    )
    .await;

    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(response.status(), 502);

    let entries = wait_for_entries(&gateway, 1).await;
    assert_eq!(entries[0]["status"], 502);
    assert_eq!(entries[0]["provider"], "dead");
    assert!(entries[0]["error"].is_string());
}

/// The GUI renders the list top-down, so the newest request has to come first.
#[tokio::test]
async fn the_log_lists_the_newest_request_first() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(
        &gateway,
        json!({"providers": [{
            "id": "p1", "name": "dead", "protocol": "anthropic",
            "base_url": "http://127.0.0.1:1", "api_key": "k", "enabled": true,
            "models": [{"id": "m1", "alias": "test-model", "upstream_model": "u", "enabled": true}]
        }]}),
    )
    .await;

    post_messages(&gateway, &messages_body("test-model", false)).await;
    post_messages(&gateway, &messages_body("test-model", false)).await;

    let entries = wait_for_entries(&gateway, 2).await;
    let newest = entries[0]["at"]
        .as_u64()
        .expect("every entry is timestamped");
    let older = entries[1]["at"]
        .as_u64()
        .expect("every entry is timestamped");
    assert!(newest >= older, "the newest request should be listed first");
}

/// The gateway must keep serving after a caller has changed the mode, since it reports the
/// current surface rather than the one it started with.
#[tokio::test]
async fn switching_mode_takes_effect_without_a_restart() {
    let upstream = start_upstream("/v1/chat/completions", "data: [DONE]\n\n").await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    put_config(
        &gateway,
        json!({
            "protocol_mode": "primary_anthropic",
            "providers": [serde_json::to_value(provider_for(&upstream, ProviderProtocol::OpenAi)).unwrap()],
        }),
    )
    .await;
    assert_eq!(
        post_messages(&gateway, &messages_body("test-model", false))
            .await
            .status(),
        200
    );

    put_config(&gateway, json!({"protocol_mode": "primary_open_ai"})).await;
    let response = post_messages(&gateway, &messages_body("test-model", false)).await;
    assert_eq!(
        response.status(),
        404,
        "the Messages endpoint should close as soon as the mode changes"
    );
}

/// A provider whose key is set must receive it, so the round trip through the admin API
/// cannot silently drop credentials.
#[tokio::test]
async fn api_keys_survive_a_configuration_round_trip() {
    let upstream = start_upstream("/v1/messages", TEXT_SSE).await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    let mut provider =
        serde_json::to_value(provider_for(&upstream, ProviderProtocol::Anthropic)).unwrap();
    provider["api_key"] = json!("sk-secret");
    put_config(&gateway, json!({"providers": [provider]})).await;

    let current = get_config(&gateway).await;
    assert_eq!(current["config"]["providers"][0]["api_key"], "sk-secret");
}

/// The mode a gateway starts in is the default one, so a first run serves Anthropic.
#[tokio::test]
async fn a_fresh_configuration_uses_the_default_mode() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    let current = get_config(&gateway).await;
    assert_eq!(current["config"]["protocol_mode"], "primary_anthropic");
    assert_eq!(current["config"]["web_ui"]["enabled"], false);
    assert_eq!(current["config"]["server"]["port"], 8787);
}

/// The default provider catches model names no alias matches.
#[tokio::test]
async fn an_unmatched_model_falls_back_to_the_default_provider() {
    let upstream = start_upstream("/v1/messages", TEXT_SSE).await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    let provider =
        serde_json::to_value(provider_for(&upstream, ProviderProtocol::Anthropic)).unwrap();
    let provider_id = provider["id"].clone();
    put_config(
        &gateway,
        json!({
            "providers": [provider],
            "default_provider_id": provider_id,
        }),
    )
    .await;

    // `test-model` matches by alias; anything else lands on the default provider.
    assert_eq!(
        post_messages(&gateway, &messages_body("some-other-model", false))
            .await
            .status(),
        200
    );
    assert_eq!(
        upstream.last_request()["model"],
        "upstream-model",
        "the fallback sends the provider's own model id"
    );
}

/// A configuration fetched and written back unchanged must be accepted, which is what the
/// GUI does on every save.
#[tokio::test]
async fn a_fetched_configuration_can_be_written_back_unchanged() {
    let upstream = start_upstream("/v1/messages", TEXT_SSE).await;
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(
        &gateway,
        json!({"providers": [serde_json::to_value(provider_for(&upstream, ProviderProtocol::Anthropic)).unwrap()]}),
    )
    .await;

    let current = get_config(&gateway).await;
    let response = put_config(&gateway, current["config"].clone()).await;
    assert_eq!(
        response.status(),
        200,
        "a round-tripped configuration must validate: {}",
        response.text().await.unwrap()
    );
}

/// A gateway with no providers still answers, so the GUI can open before anything is set up.
#[tokio::test]
async fn the_ui_surface_is_reachable_before_any_provider_exists() {
    let path = temp_config_path();
    let state = AppState::new(
        Config {
            protocol_mode: ProtocolMode::Route,
            web_ui: WebUiConfig {
                enabled: true,
                token: None,
            },
            ..Config::default()
        },
        path,
    )
    .unwrap();
    let gateway = serve_state(state).await;

    for route in [
        "/admin/health",
        "/admin/config",
        "/admin/logs",
        "/v1/models",
    ] {
        let response = reqwest::get(format!("{gateway}{route}")).await.unwrap();
        assert_eq!(
            response.status(),
            200,
            "{route} should answer on an empty gateway"
        );
    }
}

/// The GUI is served only when enabled, and its fallback must never swallow API paths —
/// a typo in an API call would otherwise answer with HTML instead of an error.
#[tokio::test]
async fn the_web_gui_is_served_only_when_enabled_and_never_shadows_api_paths() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    // Off by default.
    assert_eq!(reqwest::get(&gateway).await.unwrap().status(), 404);

    put_config(&gateway, json!({"web_ui": {"enabled": true}})).await;

    let index = reqwest::get(&gateway).await.unwrap();
    assert_eq!(index.status(), 200);
    assert!(
        index.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html"),
        "the shell must be served as HTML"
    );
    let html = index.text().await.unwrap();
    assert!(
        html.contains("<div id=\"app\">"),
        "the shell must mount the app"
    );

    let asset_path = html
        .split("src=\"./")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the shell must reference its script")
        .to_owned();
    let asset = reqwest::get(format!("{gateway}/{asset_path}"))
        .await
        .unwrap();
    assert_eq!(asset.status(), 200);
    assert_eq!(
        asset.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );

    // Unknown API paths stay API errors rather than becoming the GUI.
    for route in ["/v1/nope", "/admin/nope"] {
        let response = reqwest::get(format!("{gateway}{route}")).await.unwrap();
        assert_eq!(
            response.status(),
            404,
            "{route} must not be answered by the GUI"
        );
        assert!(
            !response.text().await.unwrap().contains("<div id=\"app\">"),
            "{route} must not return the GUI shell"
        );
    }

    put_config(&gateway, json!({"web_ui": {"enabled": false}})).await;
    assert_eq!(reqwest::get(&gateway).await.unwrap().status(), 404);
}

#[tokio::test]
async fn health_reports_whether_the_gui_assets_were_built_in() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    let value: Value = reqwest::get(format!("{gateway}/admin/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        value["web_ui_assets"], true,
        "the GUI should be embedded in a normal build"
    );
}

/// The configuration endpoint is also the GUI's data source, so its shape must not vary
/// with the contents. A field that vanishes when it is empty makes every consumer
/// distinguish "absent" from "empty", and a client that reads it unguarded will break.
#[tokio::test]
async fn the_configuration_response_has_a_stable_shape() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(
        &gateway,
        json!({
            "providers": [{
                "id": "p1", "name": "bare", "protocol": "anthropic",
                "base_url": "https://example.test", "enabled": true,
                // Deliberately empty, and deliberately missing optional fields.
                "custom_headers": {},
                "models": [{
                    "id": "m1", "alias": "a", "upstream_model": "a", "enabled": true,
                    "extra": {}
                }]
            }],
        }),
    )
    .await;

    let body = get_config(&gateway).await;
    let config = &body["config"];
    for field in [
        "protocol_mode",
        "server",
        "web_ui",
        "providers",
        "default_provider_id",
    ] {
        assert!(
            !config[field].is_null() || field == "default_provider_id",
            "config.{field} is missing"
        );
    }
    for field in ["bind", "port", "api_key", "api_keys"] {
        assert!(
            config["server"].get(field).is_some(),
            "server.{field} is missing"
        );
    }
    for field in ["enabled", "token"] {
        assert!(
            config["web_ui"].get(field).is_some(),
            "web_ui.{field} is missing"
        );
    }

    let provider = &config["providers"][0];
    for field in [
        "id",
        "name",
        "protocol",
        "base_url",
        "api_key",
        "enabled",
        "custom_headers",
        "models",
    ] {
        assert!(
            provider.get(field).is_some(),
            "provider.{field} disappears when empty, which breaks the GUI"
        );
    }

    let model = &provider["models"][0];
    for field in [
        "id",
        "alias",
        "upstream_model",
        "enabled",
        "max_output_tokens",
        "max_tokens_field",
        "extra",
    ] {
        assert!(
            model.get(field).is_some(),
            "model.{field} disappears when empty, which breaks the GUI"
        );
    }
}

#[tokio::test]
async fn clearing_the_log_removes_every_record() {
    let path = temp_config_path();
    let gateway = gateway_at(&path).await;
    put_config(
        &gateway,
        json!({"providers": [{
            "id": "p1", "name": "dead", "protocol": "anthropic",
            "base_url": "http://127.0.0.1:1", "api_key": "k", "enabled": true,
            "models": [{"id": "m1", "alias": "test-model", "upstream_model": "u", "enabled": true}]
        }]}),
    )
    .await;
    post_messages(&gateway, &messages_body("test-model", false)).await;
    wait_for_entries(&gateway, 1).await;

    let cleared = reqwest::Client::new()
        .delete(format!("{gateway}/admin/logs"))
        .send()
        .await
        .unwrap();
    assert_eq!(cleared.status(), 200);
    let value: Value = cleared.json().await.unwrap();
    assert_eq!(value["cleared"], 1);

    let after: Value = reqwest::get(format!("{gateway}/admin/logs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(after["entries"].as_array().unwrap().is_empty());
}

/// The GUI fills its model table from whatever the endpoint reports, deduplicated.
#[tokio::test]
async fn the_upstream_model_list_is_fetched_for_the_gui() {
    use axum::routing::get;
    use axum::Router;
    use tokio::net::TcpListener;

    let app = Router::new().route(
        "/v1/models",
        get(|| async {
            axum::Json(json!({"data": [{"id": "zeta"}, {"id": "alpha"}, {"id": "alpha"}]}))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let path = temp_config_path();
    let gateway = gateway_at(&path).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/admin/provider-models"))
        .json(&json!({
            "protocol": "open_ai",
            "base_url": format!("http://{address}"),
            "api_key": "k"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["models"], json!(["alpha", "zeta"]));
}
