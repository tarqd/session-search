//! The browser UI, compiled into the binary.
//!
//! `include_str!` rather than reading `web/` from disk at run time: a release build stays one
//! file you can copy anywhere, and there is no way for the served UI to drift from the server
//! answering its requests — a half-updated pair of the two produces bugs that look like API
//! bugs and are not.
//!
//! Only the paths listed in [`ASSETS`] exist. Anything else under `/` falls through to the
//! router's fallback, which says what the server does serve; a static-file handler that
//! answered every unknown path with `index.html` would turn a typo'd asset URL into a blank
//! page and a console error about a MIME type.

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

/// Route path, `content-type`, body. The paths are absolute because the ES modules import each
/// other relatively (`import { el } from "./dom.js"` inside `/app.js` resolves to `/dom.js`),
/// so flattening them into one directory level is part of the contract, not an accident.
const ASSETS: &[(&str, &str, &str)] = &[
    ("/", HTML, include_str!("../../web/index.html")),
    // The same page under the name it has on disk. Bookmarking or hand-typing `/index.html` is
    // ordinary, and a 404 there — while `/` works — reads as a broken deployment.
    ("/index.html", HTML, include_str!("../../web/index.html")),
    ("/styles.css", CSS, include_str!("../../web/styles.css")),
    ("/app.js", JS, include_str!("../../web/app.js")),
    ("/dom.js", JS, include_str!("../../web/dom.js")),
    ("/tools.js", JS, include_str!("../../web/tools.js")),
    ("/highlight.js", JS, include_str!("../../web/highlight.js")),
    ("/markdown.js", JS, include_str!("../../web/markdown.js")),
];

/// The static routes, generic over the router's state so this module never has to know what
/// [`super::AppState`] holds.
pub(super) fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let mut router = Router::new();
    for &(path, content_type, body) in ASSETS {
        router = router.route(
            path,
            get(move || std::future::ready(asset(content_type, body))),
        );
    }
    router
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // This is a local tool that redeploys by being rebuilt, and the bundle carries no
            // hash in its URL. A cached copy of yesterday's `app.js` against today's API is a
            // confusing bug that survives a reload, so nothing here is cacheable.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}
