//! Serves the built GUI over the gateway's own port.
//!
//! The assets are compiled into the binary, so enabling the web GUI needs nothing on
//! disk at runtime — the same property the desktop shell relies on. Serving them is what
//! makes one build usable both as a desktop window and as a page on the gateway's port.
use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

/// Paths are relative to this crate, and the folder must exist when the crate is built.
/// `../../apps/desktop/dist` is produced by `pnpm build` in `apps/desktop`.
#[derive(Embed)]
#[folder = "../../apps/desktop/dist"]
struct Assets;

/// Serves a GUI asset, or 404 when the web GUI is off.
///
/// This is the router's fallback, so it must refuse API paths itself rather than relying
/// on routing order — otherwise a typo in an API path would answer with the GUI's HTML.
pub fn serve(uri: &Uri, enabled: bool) -> Response {
    let path = uri.path();
    if path.starts_with("/v1/") || path.starts_with("/admin/") {
        return not_found();
    }
    if !enabled {
        return not_found();
    }

    // The SPA uses hash routing, so only the shell and its assets are ever requested;
    // anything else that is not a real file is still answered with the shell.
    //
    // The content type is taken from the asset that was resolved, not from the request
    // path: the shell is served for paths with no extension at all, which would
    // otherwise be labelled as an opaque binary.
    let requested = path.strip_prefix('/').filter(|path| !path.is_empty());
    let resolved = requested.and_then(|name| Assets::get(name).map(|asset| (name, asset)));
    let (name, asset) = match resolved {
        Some(found) => found,
        None => match Assets::get("index.html") {
            Some(asset) => ("index.html", asset),
            None => return not_found(),
        },
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_for(name))
        .body(Body::from(asset.data.into_owned()))
        .expect("a response head with a static content type always builds")
}

/// A minimal extension map, which is all a fixed set of build outputs needs.
fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Whether the compiled assets are present, so a misconfigured build is visible in
/// `/admin/health` rather than only as a 404 in the browser.
pub fn assets_are_embedded() -> bool {
    Assets::get("index.html").is_some()
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}
