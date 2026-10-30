//! The gateway's HTTP server.
//!
//! Both the desktop shell and the standalone binary drive this same [`app`]: the server
//! is the product, and every GUI is a client of it.

pub mod api;
pub mod logging;
pub mod notify;
pub mod proxy;
pub mod quota;
pub mod state;
pub mod webui;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use state::AppState;

/// The largest request body the gateway will read.
///
/// axum caps bodies at 2 MiB by default, which one conversation carrying a screenshot
/// or two already exceeds — the client then gets a bare 413 before the request has even
/// been routed. Anthropic documents 32 MB as its own request ceiling, so mirroring it
/// keeps the gateway out of the way while still bounding what it buffers in memory.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/models", get(api::openai::models))
        // Both protocol endpoints stay registered in every mode and gate themselves
        // per request, so switching mode takes effect without a restart.
        .route("/v1/messages", post(api::anthropic::messages))
        .route(
            "/v1/messages/count_tokens",
            post(api::anthropic::count_tokens),
        )
        .route("/v1/chat/completions", post(api::openai::chat_completions))
        .route("/admin/health", get(api::admin::health))
        .route(
            "/admin/config",
            get(api::admin::get_config).put(api::admin::put_config),
        )
        .route(
            "/admin/logs",
            get(api::admin::logs).delete(api::admin::clear_logs),
        )
        .route("/admin/provider-models", post(api::admin::provider_models))
        .route(
            "/admin/provider-quota",
            get(api::admin::quotas).post(api::admin::provider_quota),
        )
        // Everything else is a request for the GUI, which is served only when enabled.
        .fallback(gui)
        // Covers every route, including the protocol endpoints whose bodies carry images.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(TraceLayer::new_for_http())
        // The desktop GUI and the Vite dev server are served from a different origin
        // than the gateway, so these surfaces have to be reachable cross-origin.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn gui(State(state): State<Arc<AppState>>, uri: axum::http::Uri) -> axum::response::Response {
    let enabled = state.snapshot().config.web_ui.enabled;
    webui::serve(&uri, enabled)
}

/// Where the configuration lives.
///
/// One resolver for every embedding — the headless binary and the desktop shell — so a
/// given machine cannot end up with the window and the API reading different files.
/// `LLM_GATEWAY_CONFIG` overrides the platform's config directory, which is what makes a
/// second instance on a different port possible.
pub fn config_path() -> std::result::Result<std::path::PathBuf, gateway_core::Error> {
    match std::env::var("LLM_GATEWAY_CONFIG") {
        Ok(path) if !path.trim().is_empty() => Ok(std::path::PathBuf::from(path)),
        _ => gateway_core::config::config_path(),
    }
}

/// Binds the configured address and serves until the process ends.
pub async fn serve(state: Arc<AppState>) -> std::io::Result<()> {
    let server = state.snapshot().config.server.clone();
    let address = format!("{}:{}", server.bind, server.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|error| {
            // A bare "address in use" gives no clue which setting to change, and the port
            // only winds up occupied because something else already has it.
            std::io::Error::new(
                error.kind(),
                format!(
                    "could not listen on {address} ({error}). \
                 Change `server.port` in {} and start again.",
                    state.config_path.display()
                ),
            )
        })?;
    tracing::info!("llm-gateway listening on http://{address}");
    // Key choice depends on account quotas, so they are refreshed in the background for
    // as long as the gateway serves. Nothing waits on the first reading.
    quota::spawn_refresh(state.clone());
    axum::serve(listener, app(state)).await
}
