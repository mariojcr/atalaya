// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! UI files, embedded in the binary and versioned by content hash.

use axum::{
    Router,
    extract::{Path, Query},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;

use crate::app::Shared;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/", get(|| static_file("index.html")))
        // whoever asks at the root (bookmarks, tools); browsers follow index.html
        .route("/favicon.ico", get(|| static_file("favicon.ico")))
        .route("/static/{file}", get(static_file_owned))
}

const STATIC: &[(&str, &str, &[u8])] = &[
    ("index.html", "text/html; charset=utf-8", include_bytes!("../../static/index.html")),
    ("i18n.js", "text/javascript; charset=utf-8", include_bytes!("../../static/i18n.js")),
    ("app.js", "text/javascript; charset=utf-8", include_bytes!("../../static/app.js")),
    ("units.js", "text/javascript; charset=utf-8", include_bytes!("../../static/units.js")),
    ("prom.js", "text/javascript; charset=utf-8", include_bytes!("../../static/prom.js")),
    ("dash.js", "text/javascript; charset=utf-8", include_bytes!("../../static/dash.js")),
    ("home.js", "text/javascript; charset=utf-8", include_bytes!("../../static/home.js")),
    ("timeline.js", "text/javascript; charset=utf-8", include_bytes!("../../static/timeline.js")),
    ("notebook.js", "text/javascript; charset=utf-8", include_bytes!("../../static/notebook.js")),
    ("logs.js", "text/javascript; charset=utf-8", include_bytes!("../../static/logs.js")),
    ("alerts.js", "text/javascript; charset=utf-8", include_bytes!("../../static/alerts.js")),
    ("manifest.json", "application/manifest+json", include_bytes!("../../static/manifest.json")),
    ("app.css", "text/css; charset=utf-8", include_bytes!("../../static/app.css")),
    (
        "uPlot.iife.min.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../static/uPlot.iife.min.js"),
    ),
    ("uPlot.min.css", "text/css; charset=utf-8", include_bytes!("../../static/uPlot.min.css")),
    // The brand mark (the header's watchtower) on the accent colour
    ("favicon.svg", "image/svg+xml", include_bytes!("../../static/favicon.svg")),
    ("favicon.ico", "image/x-icon", include_bytes!("../../static/favicon.ico")),
    ("icon-192.png", "image/png", include_bytes!("../../static/icon-192.png")),
    ("icon-512.png", "image/png", include_bytes!("../../static/icon-512.png")),
    ("apple-touch-icon.png", "image/png", include_bytes!("../../static/apple-touch-icon.png")),
];

/// The UI's Content-Security-Policy, sent with index.html: scripts, styles and
/// requests only from this site, and no style attributes (the UI sets styles
/// through the CSSOM). Data from logs, labels or a link that slipped past an
/// escape could not run a script.
const CSP: &str = "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Hash of each static file (xxh3 of its content), used for the ETag and to
/// version URLs: index.html references `/static/x.js?v=<hash>`, and with the
/// right hash the response is immutable (cached for a year).
fn static_hash(body: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(body))
}

/// index.html with versioned static URLs and the server's settings for the UI
fn index_html() -> &'static [u8] {
    static HTML: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    HTML.get_or_init(|| {
        let mut html = String::from_utf8_lossy(STATIC.iter().find(|(n, _, _)| *n == "index.html").map(|(_, _, b)| *b).unwrap_or_default()).into_owned();
        for (name, _, body) in STATIC {
            html = html.replace(&format!("/static/{name}\""), &format!("/static/{name}?v={}\"", static_hash(body)));
        }
        html.replace("__NOW_DELAY__", &crate::app::now_delay().to_string())
            .replace("__PAGE_REFRESH__", &crate::app::page_refresh().to_string())
            .into_bytes()
    })
}

#[derive(Deserialize)]
struct StaticParams {
    v: Option<String>,
}

async fn static_file(name: &'static str) -> Response {
    static_file_owned(Path(name.to_owned()), Query(StaticParams { v: None }), header::HeaderMap::new()).await
}

async fn static_file_owned(Path(name): Path<String>, Query(p): Query<StaticParams>, headers: header::HeaderMap) -> Response {
    let Some((_, ct, body)) = STATIC.iter().find(|(n, _, _)| *n == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body: &[u8] = if name == "index.html" { index_html() } else { body };
    let etag = format!("\"{}\"", static_hash(body));
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    let versioned = p.v.as_deref() == Some(etag.trim_matches('"'));
    let cache = if versioned { "public, max-age=31536000, immutable" } else { "no-cache" };
    let mut resp = (
        [
            (header::CONTENT_TYPE, (*ct).to_owned()),
            (header::CACHE_CONTROL, cache.to_owned()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response();
    if name == "index.html" {
        resp.headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static(CSP));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_page_carries_the_csp() {
        let page = static_file("index.html").await;
        assert_eq!(page.headers()[header::CONTENT_SECURITY_POLICY], CSP);
    }
}
