//! The HTTP server: routing, the blocking-pool hop, status codes.
//!
//! Every wire shape lives in [`dto`], which does not know axum exists; everything here is
//! plumbing around it. The split is what makes the envelope testable without a socket, and it
//! is why `dto` reports failure as a `String` — a wire-shape mistake is always the caller's,
//! and this module is the only place that knows a caller's mistake is a `400`.
//!
//! Two rules the handlers all obey:
//!
//! * **Tantivy is blocking.** Searching, reading documents and reindexing all sit on
//!   [`tokio::task::spawn_blocking`]. A single search on the async worker stalls every other
//!   connection on that thread, and a reindex stalls the whole server.
//! * **A caller's mistake never looks like an empty result.** An unknown query parameter, an
//!   unknown filter field, an unsortable sort field, an ambiguous session prefix: each is a
//!   `400` naming what was wrong and what is accepted instead. Silently ignoring one widens or
//!   narrows the search and reports the wrong answer with total confidence.
//!
//! The server has no authentication, and the index is a verbatim record of every prompt,
//! command and tool output — secrets included. Binding a non-loopback address is therefore
//! allowed but announced, loudly, in the log and in the startup banner.

mod dto;

#[cfg(feature = "web-ui")]
mod assets;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde_json::{Value, json};

use crate::index::{IndexOptions, IndexStats};
use crate::parse::SessionInfo;
use crate::schema::Fields;
use crate::sessions::{self, SessionMatcher};
use crate::{cli, context, discovery, index, search};

/// How long a `/around` window reaches by default, matching `show --before/--after`.
const DEFAULT_WINDOW: usize = 3;
/// How many documents `GET /api/sessions/{id}` returns by default, matching `show --limit`.
const DEFAULT_SESSION_LIMIT: usize = 200;
/// Query keys `GET /api/sessions/{id}` accepts.
const SESSION_DOC_PARAMS: &[&str] = &["agent", "source_path", "include_raw", "limit"];
/// Query keys `GET /api/sessions/{id}/around` accepts.
const SESSION_WINDOW_PARAMS: &[&str] = &[
    "agent",
    "source_path",
    "include_raw",
    "seq",
    "before",
    "after",
];

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub host: String,
    pub port: u16,
    pub cors: Vec<String>,
    pub refresh_secs: u64,
}

/// Cap on a `POST /api/search` body. A Search UI request state is a few hundred bytes; this is
/// generous enough that no honest client meets it and small enough that an unauthenticated
/// local port cannot be used to buffer megabytes.
const MAX_BODY_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

struct AppState {
    index_dir: PathBuf,
    /// Opened once, at startup.
    ///
    /// Note that the library's read entry points take `&Index` and call `index.reader()`
    /// themselves, so a reader is in fact rebuilt per request — parsing `meta.json` and
    /// reopening every segment each time. That is *not* what makes an outside commit visible:
    /// the default `ReloadPolicy::OnCommitWithDelay` already picks those up on a long-lived
    /// reader. It is simply the shape those signatures have from their CLI origins, where a
    /// process did it once and exited. Hoisting one `IndexReader` in here means changing
    /// `search`/`context` to take a `&Searcher` — `docs_by_seq` already does — which is a
    /// change to the pinned core for a local tool's request rate, so it is recorded here and
    /// in `docs/WEB-UI.md` rather than smuggled in with the server.
    index: tantivy::Index,
    fields: Fields,
    /// Origins `--cors` allowed. Empty means the CORS middleware is not even installed.
    cors: Vec<String>,
    /// Held for the duration of a reindex. Two `IndexWriter`s on one directory is not a race
    /// that degrades gracefully — the second blocks on the lock file or fails outright — so a
    /// concurrent request is refused with a `409` instead of queued behind the first.
    ///
    /// An `AtomicBool` with an RAII guard rather than a `Mutex`, because a panic inside a
    /// reindex would poison a mutex and disable reindexing for the life of the process, while
    /// the guard's `Drop` runs during the unwind.
    reindexing: AtomicBool,
}

impl AppState {
    fn open(index_dir: &FsPath, cors: Vec<String>) -> anyhow::Result<AppState> {
        let (index, fields) = index::open_or_create(index_dir)
            .with_context(|| format!("opening the index at {}", index_dir.display()))?;
        Ok(AppState {
            index_dir: index_dir.to_path_buf(),
            index,
            fields,
            cors,
            reindexing: AtomicBool::new(false),
        })
    }
}

/// Releases [`AppState::reindexing`] however the reindex ends, panic included.
struct ReindexGuard<'a>(&'a AtomicBool);

impl<'a> ReindexGuard<'a> {
    fn try_acquire(flag: &'a AtomicBool) -> Option<ReindexGuard<'a>> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ReindexGuard(flag))
    }
}

impl Drop for ReindexGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// The one failure shape on the wire: `{"error":{"status":…,"message":…}}`.
///
/// A message is written for whoever typed the request, so it names the thing that was wrong
/// and, where there is a closed set, what would have been accepted.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
        }
    }

    /// The request was wrong. Every `String` error `dto` produces arrives here.
    fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, message)
    }
}

impl From<anyhow::Error> for ApiError {
    /// The index or the disk was wrong, not the caller. `{err:#}` keeps the whole context
    /// chain: "opening the index at /… : permission denied" is actionable, "permission denied"
    /// on its own is not.
    ///
    /// The one exception is a [`sessions::FilterError`], which the query builder raises for a
    /// filter it could not read. That is the caller's mistake and has to be a 400 for the same
    /// reason a malformed date is one, pinned by `a_malformed_date_is_a_400_wherever_it_arrives`:
    /// a caller typing a filter hits every prefix of it on the way, and a 500 claims the index or
    /// the disk broke. `dto` pre-validates the shapes it can see, so this catches the rest.
    fn from(err: anyhow::Error) -> ApiError {
        match err.downcast::<sessions::FilterError>() {
            Ok(filter) => ApiError::bad_request(format!("{}: {:#}", filter.field, filter.source)),
            Err(err) => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Only 500s are logged. A 400 is the caller's business and is already in the response
        // body; logging those turns a browser typing into the search box into log spam that
        // hides the one line that matters.
        if self.status.is_server_error() {
            tracing::error!(status = self.status.as_u16(), error = %self.message, "request failed");
        }
        let body = json!({ "error": { "status": self.status.as_u16(), "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}

/// Run Tantivy work off the async threads.
///
/// A panic inside the closure comes back as a `JoinError`; turning it into a `500` is the
/// difference between "the server told me it broke" and a connection that closes with no
/// response at all.
async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(err) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the request handler did not finish: {err}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------

/// Blocks until the server stops. Builds its own Tokio runtime, so `cli.rs` stays sync.
pub fn serve(
    index_dir: &std::path::Path,
    opts: ServeOptions,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let addr = resolve_bind(&opts.host, opts.port)?;
    let state = Arc::new(AppState::open(index_dir, opts.cors.clone())?);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("session-search-http")
        .build()
        .context("building the Tokio runtime")?;

    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        // `--port 0` is a real thing to do in a test; report where we actually landed.
        let bound = listener.local_addr().unwrap_or(addr);

        if !bound.ip().is_loopback() {
            tracing::warn!(
                address = %bound,
                "bound to a non-loopback address: this server has no authentication and the \
                 index holds every prompt, command and tool output verbatim"
            );
        }
        // The caller only flushes after `serve` returns, which is when the server *stops* —
        // so a banner left in the buffer would appear at shutdown, describing a port that is
        // already closed.
        banner(out, &bound, index_dir, &opts)?;
        out.flush().context("writing the startup banner")?;

        let stop = Arc::new(AtomicBool::new(false));
        let refresher = (opts.refresh_secs > 0).then(|| {
            spawn_refresh_loop(
                Arc::clone(&state),
                Duration::from_secs(opts.refresh_secs),
                Arc::clone(&stop),
            )
        });

        let result = axum::serve(listener, router(Arc::clone(&state)))
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("serving HTTP");

        stop.store(true, Ordering::Release);
        if let Some(handle) = refresher {
            // Waiting matters when a scheduled reindex is mid-commit: dropping the process out
            // from under an `IndexWriter` leaves its lock file behind, and the next run has to
            // be told to break it.
            let _ = handle.join();
        }
        result
    })
}

/// Resolve `--host` ourselves rather than letting the listener do it, because both the banner
/// and the loopback warning need the address that was actually chosen — `localhost` can resolve
/// to something that is not `127.0.0.1`, and warning about the name instead of the address
/// would be guessing.
fn resolve_bind(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving --host {host:?}"))?
        .next()
        .ok_or_else(|| anyhow!("--host {host:?} resolved to no address"))
}

fn banner(
    out: &mut impl std::io::Write,
    bound: &SocketAddr,
    index_dir: &FsPath,
    opts: &ServeOptions,
) -> anyhow::Result<()> {
    writeln!(out, "session-search serving on http://{bound}")?;
    writeln!(out, "  index    {}", index_dir.display())?;
    if cfg!(feature = "web-ui") {
        writeln!(out, "  ui       http://{bound}/")?;
    } else {
        writeln!(
            out,
            "  ui       not built in (rebuild with --features web-ui); JSON API only"
        )?;
    }
    writeln!(
        out,
        "  cors     {}",
        if opts.cors.is_empty() {
            "off".to_string()
        } else {
            opts.cors.join(", ")
        }
    )?;
    if opts.refresh_secs > 0 {
        writeln!(out, "  refresh  every {}s", opts.refresh_secs)?;
    }
    if !bound.ip().is_loopback() {
        writeln!(
            out,
            "\n  WARNING: {} is not a loopback address. This server has no authentication, and\n\
             \x20          the index is a verbatim record of everything you and the model typed,\n\
             \x20          secrets included. Anyone who can reach this port can read all of it.",
            bound.ip()
        )?;
    }
    Ok(())
}

async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("interrupt received; finishing in-flight requests"),
        Err(err) => {
            // A resolved future here means "shut down now", so a handler we failed to install
            // must never resolve — otherwise the server exits the instant it starts.
            tracing::warn!(error = %err, "cannot listen for ctrl-c; shutdown will not be graceful");
            std::future::pending::<()>().await
        }
    }
}

/// `--refresh-secs N`.
///
/// A plain OS thread rather than a Tokio interval: this crate's `tokio` feature set is trimmed
/// to what the server needs (`rt-multi-thread`, `net`, `signal`) and deliberately does not
/// include the timer driver. The sleep is sliced so that shutdown does not have to wait out a
/// whole interval to join the thread.
fn spawn_refresh_loop(
    state: Arc<AppState>,
    every: Duration,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    const SLICE: Duration = Duration::from_millis(250);
    std::thread::spawn(move || {
        loop {
            let mut slept = Duration::ZERO;
            while slept < every {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let slice = SLICE.min(every - slept);
                std::thread::sleep(slice);
                slept += slice;
            }
            // Skip, never queue: a corpus big enough that a reindex outlasts the interval would
            // otherwise accumulate a backlog of runs that can only ever wait on each other.
            let Some(_guard) = ReindexGuard::try_acquire(&state.reindexing) else {
                tracing::debug!("a reindex is already running; skipping this refresh");
                continue;
            };
            match reindex_once(&state.index_dir) {
                Ok(stats) => tracing::info!(
                    files = stats.files_updated,
                    docs = stats.docs_added,
                    ms = stats.elapsed_ms,
                    "refreshed"
                ),
                // A refresh failure is not fatal: an unreadable transcript root should not stop
                // the server answering questions about what was indexed yesterday.
                Err(err) => tracing::warn!(
                    error = %format!("{err:#}"),
                    "scheduled refresh failed; serving the index as it stands"
                ),
            }
        }
    })
}

/// The incremental reindex behind both `POST /api/reindex` and `--refresh-secs`.
fn reindex_once(index_dir: &FsPath) -> anyhow::Result<IndexStats> {
    let meta = index::meta(index_dir);
    // Refresh the corpus this index actually holds. Resolving the *default* root for an index
    // built over a snapshot silently merges a second corpus into it, and every count afterwards
    // describes the union without saying so.
    let roots = if meta.roots.is_empty() {
        vec![discovery::default_root()?]
    } else {
        meta.roots.clone()
    };
    index::run(
        index_dir,
        &roots,
        &IndexOptions {
            // Keep the index's own thinking setting. This is an index-time choice, and flipping
            // it from a server that happens to be running would rewrite what is stored.
            include_thinking: if meta.roots.is_empty() {
                IndexOptions::default().include_thinking
            } else {
                meta.thinking_indexed
            },
            ..IndexOptions::default()
        },
    )
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

fn router(state: Arc<AppState>) -> Router {
    let mut app = Router::new()
        .route("/api/health", get(health))
        .route("/api/stats", get(stats))
        .route("/api/reindex", post(reindex))
        // The limit is stated here rather than left to axum's default, so the error message
        // can quote the number the caller actually has to fit under.
        .route(
            "/api/search",
            get(search_get)
                .post(search_post)
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
        .route("/api/facets/{field}", get(facets_get))
        .route("/api/sessions", get(sessions_list))
        .route("/api/sessions/{session_id}", get(session_docs))
        .route("/api/sessions/{session_id}/around", get(session_window));

    #[cfg(feature = "web-ui")]
    {
        app = app.merge(assets::routes());
    }
    #[cfg(not(feature = "web-ui"))]
    {
        app = app.route("/", get(no_web_ui));
    }

    let app = app
        .fallback(not_found)
        // Axum's own 405 has an empty body, which breaks the one promise every other response
        // here keeps: a failure is JSON that says what went wrong. `curl /api/reindex` without
        // `-X POST` would otherwise print nothing at all.
        .method_not_allowed_fallback(method_not_allowed);

    // Installed only when `--cors` was given, so the default build has no per-request work and
    // no way to grow an allow-origin header by accident.
    if state.cors.is_empty() {
        app.with_state(state)
    } else {
        app.layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            cors_middleware,
        ))
        .with_state(state)
    }
}

async fn not_found(method: Method, uri: Uri) -> ApiError {
    let ui = if cfg!(feature = "web-ui") {
        ", the UI at /"
    } else {
        ""
    };
    ApiError::new(
        StatusCode::NOT_FOUND,
        format!(
            "no route for {method} {path}; this server has /api/health, /api/stats, \
             /api/reindex, /api/search, /api/facets/{{field}}, /api/sessions, \
             /api/sessions/{{session_id}} and /api/sessions/{{session_id}}/around{ui}",
            path = uri.path()
        ),
    )
}

/// The route exists but not for this method. Naming the method the route does take is the
/// whole value here: the caller already knows the path is right, or they would have got a 404.
async fn method_not_allowed(method: Method, uri: Uri) -> ApiError {
    let path = uri.path();
    let accepted = match path {
        "/api/reindex" => "POST",
        "/api/search" => "GET or POST",
        _ => "GET",
    };
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        format!("{path} does not accept {method}; use {accepted}"),
    )
}

/// `GET /` without the `web-ui` feature. A bare 404 here reads as "the server is broken"; it is
/// the single most likely URL for someone to open first.
#[cfg(not(feature = "web-ui"))]
async fn no_web_ui() -> Json<Value> {
    Json(json!({
        "web_ui": false,
        "message": "this binary was built without the `web-ui` feature, so there is no browser \
                    UI to serve here; the JSON API is under /api/",
        "health": "/api/health",
    }))
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// Hand-rolled because `tower-http` is deliberately not a dependency: this is the only
/// middleware the server has, and the whole policy is "echo an origin the user listed".
async fn cors_middleware(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let allow = origin
        .as_deref()
        .and_then(|origin| allowed_origin(&state.cors, origin));

    // Preflight is answered here rather than by a route: the router would reject `OPTIONS` on a
    // GET-only path with a 405, which a browser reports as a CORS failure with no hint that the
    // method was the problem.
    if req.method() == Method::OPTIONS
        && let Some(allow) = &allow
    {
        let requested_headers = req
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("content-type")
            .to_owned();
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        put(headers, header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
        put(
            headers,
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, OPTIONS",
        );
        put(
            headers,
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            &requested_headers,
        );
        put(headers, header::ACCESS_CONTROL_MAX_AGE, "600");
        put(
            headers,
            header::VARY,
            "origin, access-control-request-headers",
        );
        return response;
    }

    let mut response = next.run(req).await;
    if let Some(allow) = &allow {
        let headers = response.headers_mut();
        put(headers, header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
        // Without `vary`, a shared cache can hand one origin's allow-origin header to another
        // and the second browser silently refuses a response it was entitled to.
        put(headers, header::VARY, "origin");
    }
    response
}

/// The value to echo back, or `None` when this origin was not allowed. `*` answers for every
/// origin; no credentials are ever sent, so the wildcard is safe to echo verbatim.
fn allowed_origin(allowed: &[String], origin: &str) -> Option<String> {
    if allowed.iter().any(|a| a == "*") {
        return Some("*".to_string());
    }
    allowed
        .iter()
        .any(|a| a == origin)
        .then(|| origin.to_string())
}

/// A header value that will not parse is dropped rather than fataled: the request itself is
/// fine, and an unusable `Origin` only means the browser will not be allowed to read the body.
fn put(headers: &mut axum::http::HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    // Deliberately touches nothing: a health check that opens the index reports the index's
    // health, not the server's, and takes a Tantivy lock to do it.
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "index_dir": state.index_dir.display().to_string(),
        "web_ui": cfg!(feature = "web-ui"),
    }))
}

async fn stats(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let meta = index::meta(&state.index_dir);
        let stats = cli::index_stats(&state.index_dir)?;
        Ok(Json(json!({
            "docs": stats.docs_added,
            "sessions": stats.sessions,
            "files": stats.files_scanned,
            "roots": meta.roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>(),
            "thinking_indexed": meta.thinking_indexed,
            "index_dir": state.index_dir.display().to_string(),
            "size_bytes": dir_size(&state.index_dir),
        })))
    })
    .await
}

/// Bytes on disk under the index directory. An entry that cannot be stat'ed is skipped rather
/// than failing the call: `size_bytes` is a diagnostic, and a segment file deleted by a
/// concurrent merge while we walk is normal, not an error worth returning.
fn dir_size(dir: &FsPath) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|meta| meta.len())
        .sum()
}

async fn reindex(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let Some(_guard) = ReindexGuard::try_acquire(&state.reindexing) else {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "a reindex is already running",
            ));
        };
        let stats = reindex_once(&state.index_dir)?;
        Ok(Json(json!({ "started": true, "stats": stats })))
    })
    .await
}

async fn search_post(
    State(state): State<Arc<AppState>>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Value>, ApiError> {
    // Decoded here rather than through `Json<SearchBody>` so that malformed JSON comes back in
    // the same `{"error":{…}}` envelope as everything else; axum's own rejection is plain text,
    // and a client that only ever parses our envelope reads it as a transport failure.
    //
    // The body extractor has its own rejection — a body past `DefaultBodyLimit` — and it is
    // plain text for the same reason, so it is caught here too. Hand-rolling the JSON decode
    // and then letting *this* one through would leave exactly the hole the hand-rolling closed.
    let body = body.map_err(|err| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "the request body is too large (the limit is {} bytes): {err}",
                MAX_BODY_BYTES
            ),
        )
    })?;
    let parsed: dto::SearchBody = if body.is_empty() {
        // `curl -X POST /api/search` with no body means "everything", which is exactly what the
        // default request state is.
        dto::SearchBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|err| {
            ApiError::bad_request(format!(
                "the request body is not a Search UI request object: {err}"
            ))
        })?
    };
    run_search(state, parsed).await
}

async fn search_get(
    State(state): State<Arc<AppState>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    // `RawQuery`, not `Query`: repeatable keys (`?tool=Bash&tool=Read`) are the whole point and
    // a map-shaped extractor keeps only the last one.
    let params = dto::Params::parse(raw.as_deref()).map_err(ApiError::bad_request)?;
    params
        .reject_unknown(dto::SEARCH_PARAMS)
        .map_err(ApiError::bad_request)?;
    let body = dto::SearchBody::from_params(&params).map_err(ApiError::bad_request)?;
    run_search(state, body).await
}

async fn run_search(state: Arc<AppState>, body: dto::SearchBody) -> Result<Json<Value>, ApiError> {
    // `prepare` is pure and cheap, so a rejected request never occupies a blocking thread.
    let prepared = body.prepare().map_err(ApiError::bad_request)?;
    // The same pre-check `GET /api/facets/{field}` runs, and for the same reason: without it
    // `search::search` refuses the field as an `anyhow` error, which this boundary renders as a
    // `500` that names nothing. `POST /api/search` is the path a third-party Search UI connector
    // uses, so it is the one most likely to arrive with a misspelled facet name.
    let schema = state.index.schema();
    for field in &prepared.request.facets {
        facetable(&schema, field).map_err(ApiError::bad_request)?;
    }
    blocking(move || {
        let response = search::search(&state.index, &state.fields, &prepared.request)?;
        // Only when grouping is on: the pager divides `totalResults` by the page size, and
        // with turns as the unit that number has to be turns. An ungrouped response pages by
        // documents and never pays for it.
        let turns = if prepared.request.group_by_turn {
            search::count_turns(&state.index, &state.fields, &prepared.request)?
        } else {
            0
        };
        // What was being asked, for every turn on the page, in one query. `turn_prompt` is
        // never stored, and a card that had to fetch its own opener would be twenty round
        // trips for one page.
        let wanted: Vec<(String, u64)> = response
            .hits
            .iter()
            .filter(|hit| hit.doc.seq != hit.doc.turn_seq && !hit.doc.source_path.is_empty())
            .map(|hit| (hit.doc.source_path.clone(), hit.doc.turn_seq))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let openers = context::turn_openers(&state.index, &state.fields, &wanted)?
            .into_iter()
            .map(|doc| {
                (
                    (doc.source_path.clone(), doc.seq),
                    dto::api_doc(&doc, false),
                )
            })
            .collect();
        Ok(Json(dto::search_ui_response(
            &prepared, &response, turns, &openers,
        )))
    })
    .await
}

async fn facets_get(
    State(state): State<Arc<AppState>>,
    Path(field): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let params = dto::Params::parse(raw.as_deref()).map_err(ApiError::bad_request)?;
    // The same parameters as `GET /api/search`, plus the one knob that only means something
    // when a single field is being counted.
    let mut allowed: Vec<&str> = dto::SEARCH_PARAMS.to_vec();
    allowed.push("top");
    params
        .reject_unknown(&allowed)
        .map_err(ApiError::bad_request)?;

    let top = params
        .number::<usize>("top")
        .map_err(ApiError::bad_request)?;
    let body = dto::SearchBody::from_params(&params).map_err(ApiError::bad_request)?;
    let mut prepared = body.prepare().map_err(ApiError::bad_request)?;
    if let Some(top) = top {
        prepared.request.facet_top = top;
    }

    facetable(&state.index.schema(), &field).map_err(ApiError::bad_request)?;
    blocking(move || {
        let result = search::facets(&state.index, &state.fields, &field, &prepared.request)?;
        Ok(Json(dto::facet_json(&result)))
    })
    .await
}

/// Can this field be counted at all?
///
/// `search::facets` checks the same three things, but reports them as an `anyhow` error, which
/// this boundary renders as a `500` — and a misspelled field name is the caller's mistake, not
/// the index's. Checking first is what makes `/api/facets/toolname` a `400` that names the
/// field instead of a server error that names nothing.
fn facetable(schema: &tantivy::schema::Schema, name: &str) -> Result<(), String> {
    let base = name.split('.').next().unwrap_or(name);
    let field = schema.get_field(base).map_err(|_| {
        format!(
            "unknown facet field {name:?}; try a fast field (tool_name, project, model, \
             git_branch, role, kind, agent_type, entrypoint) or a JSON path such as \
             tool_input.file_path"
        )
    })?;
    let entry = schema.get_field_entry(field);
    if !entry.field_type().is_fast() {
        return Err(format!(
            "facet field {name:?} is stored but not a fast field, so it cannot be counted; \
             search it instead"
        ));
    }
    if base != name && !entry.field_type().is_json() {
        return Err(format!(
            "facet field {name:?} uses a JSON subpath, but {base:?} is not a JSON field"
        ));
    }
    Ok(())
}

async fn sessions_list(
    State(state): State<Arc<AppState>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let params = dto::Params::parse(raw.as_deref()).map_err(ApiError::bad_request)?;
    // `session_filters` refuses an unknown key itself, against `SESSION_LIST_PARAMS`, and
    // applies the default row limit.
    let (filters, limit) = dto::session_filters(&params).map_err(ApiError::bad_request)?;
    // The same list `session-search sessions` warns about, rendered as sentences instead: this
    // surface has no stderr a caller can read, so a filter it could not apply has to travel in
    // the response body or it does not travel at all.
    let warnings = sessions::unanswerable_filter_notes(&filters, sessions::SearchSurface::HttpApi);
    for warning in &warnings {
        tracing::warn!(warning, "session filter ignored");
    }
    let matcher =
        SessionMatcher::new(&filters).map_err(|err| ApiError::bad_request(format!("{err:#}")))?;

    blocking(move || {
        let mut sessions: Vec<SessionInfo> = index::load_sessions(&state.index_dir)?
            .into_values()
            .filter(|info| matcher.matches(info))
            .collect();
        // `total` counts everything that matched, not what fits under `limit`: a listing that
        // reports its own page size as the total tells the caller there is nothing more.
        let total = sessions.len();
        // Most recent first; the ids break ties so the listing is deterministic.
        sessions.sort_by(|a, b| {
            b.last_ts_ms
                .cmp(&a.last_ts_ms)
                .then_with(|| a.session_id.cmp(&b.session_id))
                .then_with(|| a.agent_id.cmp(&b.agent_id))
        });
        sessions.truncate(limit);
        Ok(Json(json!({
            "sessions": sessions.iter().map(dto::session_json).collect::<Vec<_>>(),
            "total": total,
            "warnings": warnings,
        })))
    })
    .await
}

async fn session_docs(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let params = dto::Params::parse(raw.as_deref()).map_err(ApiError::bad_request)?;
    params
        .reject_unknown(SESSION_DOC_PARAMS)
        .map_err(ApiError::bad_request)?;
    let include_raw = params.flag("include_raw").map_err(ApiError::bad_request)?;
    let limit = params
        .number::<usize>("limit")
        .map_err(ApiError::bad_request)?
        .unwrap_or(DEFAULT_SESSION_LIMIT);
    let agent = params.first("agent").map(str::to_owned);
    let source = params.first("source_path").map(str::to_owned);

    blocking(move || {
        let target = resolve_target(&state, &session_id, agent.as_deref(), source)?;
        let docs = context::session(
            &state.index,
            &state.fields,
            &target.session_id,
            target.agent_id.as_deref(),
            target.source_path.as_deref(),
            limit,
        )?;
        // The window stopped at `limit` with no way to know whether the session ended there.
        let truncated = limit > 0 && docs.len() >= limit;
        Ok(Json(target.envelope(&docs, include_raw, truncated)))
    })
    .await
}

async fn session_window(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let params = dto::Params::parse(raw.as_deref()).map_err(ApiError::bad_request)?;
    params
        .reject_unknown(SESSION_WINDOW_PARAMS)
        .map_err(ApiError::bad_request)?;
    let include_raw = params.flag("include_raw").map_err(ApiError::bad_request)?;
    let seq = params
        .number::<u64>("seq")
        .map_err(ApiError::bad_request)?
        .ok_or_else(|| {
            ApiError::bad_request(
                "`seq` is required: it is the per-file ordinal carried by every hit \
                 (`_meta.doc.seq`), and without it there is no window to centre",
            )
        })?;
    let before = params
        .number::<usize>("before")
        .map_err(ApiError::bad_request)?
        .unwrap_or(DEFAULT_WINDOW);
    let after = params
        .number::<usize>("after")
        .map_err(ApiError::bad_request)?
        .unwrap_or(DEFAULT_WINDOW);
    let agent = params.first("agent").map(str::to_owned);
    let source = params.first("source_path").map(str::to_owned);

    blocking(move || {
        let target = resolve_target(&state, &session_id, agent.as_deref(), source)?;
        let docs = context::around(
            &state.index,
            &state.fields,
            &target.session_id,
            target.agent_id.as_deref(),
            target.source_path.as_deref(),
            seq,
            before,
            after,
        )?;
        // A window is bounded by `seq`, not by a row limit, so there is nothing it silently cut.
        Ok(Json(target.envelope(&docs, include_raw, false)))
    })
    .await
}

/// One transcript, identified the way `show` identifies one.
struct Target {
    session_id: String,
    agent_id: Option<String>,
    source_path: Option<String>,
}

impl Target {
    fn envelope(&self, docs: &[crate::parse::Doc], include_raw: bool, truncated: bool) -> Value {
        json!({
            "session_id": self.session_id,
            "agent_id": self.agent_id,
            "source_path": self.source_path,
            "docs": docs.iter().map(|d| dto::api_doc(d, include_raw)).collect::<Vec<_>>(),
            "truncated": truncated,
        })
    }
}

/// Expand the ids a caller pasted and pin the window to one file.
///
/// `seq` is a per-*file* ordinal and two transcripts can share a session id (a mid-life
/// `resetSessionFile()`, or a relocated project), so a lookup scoped only by session id
/// interleaves both files and silently drops the neighbours it was asked for.
fn resolve_target(
    state: &AppState,
    session_id: &str,
    agent: Option<&str>,
    explicit_source: Option<String>,
) -> Result<Target, ApiError> {
    // `?`, not `unwrap_or_default()`: an unreadable or corrupt `sessions.json` would otherwise
    // read as "nothing is indexed", which switches off both the 404 below and the `source_path`
    // scoping this function exists for — a live `index --full` deletes that file mid-run, so it
    // is reachable beside a running server. `GET /api/sessions` already answers the same failure
    // with a `500`; a missing file is still `Ok(empty)`.
    let known = index::load_sessions(&state.index_dir)?;
    // An ambiguous prefix is the caller's to disambiguate, and `resolve_id` lists the
    // candidates in its message.
    let session_id = cli::resolve_id(
        "session",
        session_id,
        known.values().map(|i| i.session_id.as_str()),
    )
    .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    let agent_id = match agent {
        Some(agent) => Some(
            cli::resolve_id(
                "agent",
                agent,
                known
                    .values()
                    .filter(|i| i.session_id == session_id)
                    .filter_map(|i| i.agent_id.as_deref()),
            )
            .map_err(|err| ApiError::bad_request(format!("{err:#}")))?,
        ),
        None => None,
    };

    // An id that resolved to nothing would otherwise return `docs: []`, which reads as "that
    // session is empty" rather than "there is no such session". Only checkable when something
    // is indexed at all: an empty `sessions.json` means we know nothing, not that nothing is.
    if !known.is_empty()
        && !known
            .values()
            .any(|i| i.session_id == session_id && i.agent_id.as_deref() == agent_id.as_deref())
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            match &agent_id {
                Some(agent) => format!(
                    "no indexed transcript for session {session_id:?} agent {agent:?}; \
                     GET /api/sessions lists what is indexed"
                ),
                None => format!(
                    "no indexed transcript for session {session_id:?}; \
                     GET /api/sessions lists what is indexed"
                ),
            },
        ));
    }

    let source_path =
        explicit_source.or_else(|| cli::source_path_for(&known, &session_id, agent_id.as_deref()));
    Ok(Target {
        session_id,
        agent_id,
        source_path,
    })
}

// ---------------------------------------------------------------------------
// session-list filtering
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    /// The session id inside `tests/fixtures/real_main_slice.jsonl`. Discovery takes the id
    /// from the *filename*, so the copy has to be named after it or nothing resolves.
    const SESSION: &str = "b20208d8-fbdb-5918-ba69-d203de6ed6dc";

    /// The router over a real, on-disk index built from the fixture.
    ///
    /// On disk rather than in RAM because half of what is under test reads the index
    /// *directory* — `sessions.json` for the listing and for prefix resolution, `state.json`
    /// for `/api/stats` — and none of that exists for an index created in memory.
    struct Server {
        _tmp: tempfile::TempDir,
        app: Router,
    }

    fn server_with_cors(cors: Vec<String>) -> Server {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("claude/projects");
        let project = root.join("-home-user-session-search");
        std::fs::create_dir_all(&project).unwrap();
        let fixture =
            FsPath::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_main_slice.jsonl");
        std::fs::copy(&fixture, project.join(format!("{SESSION}.jsonl"))).unwrap();

        let index_dir = tmp.path().join("index");
        let stats = index::run(
            &index_dir,
            std::slice::from_ref(&root),
            &IndexOptions::default(),
        )
        .unwrap();
        // Guards every other test here: if the fixture stopped parsing, the assertions below
        // would pass against an empty index and prove nothing.
        assert!(stats.docs_added > 5, "fixture indexed {stats:?}");

        let state = Arc::new(AppState::open(&index_dir, cors).unwrap());
        Server {
            _tmp: tmp,
            app: router(state),
        }
    }

    fn server() -> Server {
        server_with_cors(Vec::new())
    }

    impl Server {
        /// `oneshot` drives the router directly, so no port is bound and nothing here can
        /// collide with another test running in parallel.
        fn send(&self, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let response = self.app.clone().oneshot(req).await.unwrap();
                let status = response.status();
                let headers = response.headers().clone();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    status,
                    headers,
                    String::from_utf8_lossy(&bytes).into_owned(),
                )
            })
        }

        fn get(&self, uri: &str) -> (StatusCode, Value) {
            let (status, _, body) =
                self.send(Request::builder().uri(uri).body(Body::empty()).unwrap());
            (status, json_body(&body))
        }

        fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
            let (status, _, body) = self.send(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            );
            (status, json_body(&body))
        }
    }

    fn json_body(body: &str) -> Value {
        serde_json::from_str(body)
            .unwrap_or_else(|err| panic!("response was not JSON ({err}): {body}"))
    }

    fn error_message(body: &Value) -> String {
        body["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("no error message in {body}"))
            .to_string()
    }

    /// The handler decodes the body by hand so that a bad one comes back in the documented
    /// envelope. The body *extractor* has a rejection of its own, and letting that one through
    /// as axum's plain text would reopen the hole by another door: a client that only ever
    /// calls `res.json()` on a failure throws instead of reading the message.
    #[test]
    fn an_oversized_body_is_refused_in_the_same_envelope_as_every_other_failure() {
        let server = server();
        let (status, headers, body) = server.send(
            Request::builder()
                .method(Method::POST)
                .uri("/api/search")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(vec![b'x'; MAX_BODY_BYTES + 1]))
                .unwrap(),
        );
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = json_body(&body);
        assert!(
            error_message(&body).contains(&MAX_BODY_BYTES.to_string()),
            "the message has to name the cap the caller must fit under: {body}"
        );
    }

    #[test]
    fn search_answers_with_the_search_ui_envelope() {
        let server = server();
        let (status, body) = server.post("/api/search", json!({ "searchTerm": "git" }));
        assert_eq!(status, StatusCode::OK, "{body}");
        // A third-party Search UI connector reads exactly these; one missing key is a blank
        // page with no error, so they are asserted by name rather than by shape.
        for key in [
            "results",
            "totalResults",
            "totalPages",
            "pagingStart",
            "pagingEnd",
            "requestId",
            "resultSearchTerm",
            "wasSearched",
            "facets",
            "info",
        ] {
            assert!(body.get(key).is_some(), "no {key:?} in {body}");
        }
        assert_eq!(body["wasSearched"], json!(true));
        assert!(body["totalResults"].as_u64().unwrap() > 0, "{body}");
        let first = &body["results"][0];
        assert!(first["_meta"]["doc"].is_object(), "{first}");
        assert!(first["id"]["raw"].is_string(), "{first}");
    }

    #[test]
    fn the_same_envelope_comes_back_from_the_get_form() {
        let server = server();
        let (status, body) = server.get("/api/search?q=git&size=5");
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["results"].as_array().unwrap().len() <= 5, "{body}");
    }

    #[test]
    fn an_unknown_query_parameter_is_a_400_that_names_what_is_accepted() {
        let server = server();
        let (status, body) = server.get("/api/search?tools=Bash");
        // The failure mode this guards: `tools` silently ignored, the unfiltered corpus
        // returned, and the caller believing every one of those hits used Bash.
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let message = error_message(&body);
        assert!(message.contains("tools"), "{message}");
        assert!(message.contains("tool"), "{message}");
    }

    #[test]
    fn an_unknown_filter_field_is_a_400() {
        let server = server();
        let (status, body) = server.post(
            "/api/search",
            json!({
                "filters": [{ "field": "toolname", "values": ["Bash"], "type": "any" }]
            }),
        );
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(error_message(&body).contains("toolname"));
    }

    #[test]
    fn an_unfacetable_field_is_a_400_not_a_500() {
        let server = server();
        let (status, body) = server.get("/api/facets/toolname");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(error_message(&body).contains("toolname"));

        let (status, body) = server.get("/api/facets/tool_name");
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["field"], json!("tool_name"));
        assert!(body["values"].is_array(), "{body}");
        // The honesty fields travel with the buckets or the buckets get misread as a total.
        assert!(body["matchingDocs"].as_u64().is_some(), "{body}");
    }

    /// The same caller mistake through the other door. `/api/facets/{field}` got this right
    /// while `run_search` did not, so the status class and the message quality depended on
    /// which endpoint the typo arrived at.
    #[test]
    fn a_bad_facet_field_is_the_same_400_through_search_as_through_the_facets_route() {
        let server = server();
        let expected = {
            let (status, body) = server.get("/api/facets/nosuchfield");
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            error_message(&body)
        };
        assert!(
            expected.contains("tool_name"),
            "lists what is accepted: {expected}"
        );

        let (status, body) = server.get("/api/search?facets=nosuchfield");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(error_message(&body), expected);

        let (status, body) = server.post(
            "/api/search",
            json!({ "facets": { "nosuchfield": { "type": "value", "size": 5 } } }),
        );
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(error_message(&body), expected);

        // Stored but not fast: a real field, still not one that can be counted.
        let (status, body) = server.post("/api/search", json!({ "facets": { "text": {} } }));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(error_message(&body).contains("fast field"), "{body}");
    }

    /// Typing a date sends every prefix of it — `2026`, `2026-0` — and each one used to be a
    /// `500` with an `ERROR request failed` log line and a retry a client could never win.
    #[test]
    fn a_malformed_date_is_a_400_wherever_it_arrives() {
        let server = server();
        for uri in [
            "/api/search?since=notadate",
            "/api/search?until=notadate",
            "/api/facets/tool_name?since=2026-0",
            "/api/sessions?since=notadate",
        ] {
            let (status, body) = server.get(uri);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        }
        let (status, body) = server.post(
            "/api/search",
            json!({ "filters": [{ "field": "timestamp", "values": [{ "from": "garbage" }] }] }),
        );
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let message = error_message(&body);
        assert!(
            message.contains("since"),
            "names the wire parameter: {message}"
        );
        assert!(
            !message.contains("--since"),
            "over HTTP there is no flag: {message}"
        );

        let (status, body) = server.get("/api/search?since=7d");
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The same argument as the date above, for the filters the query builder parses rather than
    /// `dto`. These reach `ApiError` as a `FilterError` from inside `search::search`, and without
    /// the downcast that classifies them they are a `500` telling the caller the index broke when
    /// what happened is that they typed `--tool-input command` without an `=`.
    #[test]
    fn a_malformed_tool_filter_is_a_400_and_names_the_wire_parameter() {
        let server = server();
        // Only the `tool_input` spellings: an empty `tool_output=` is dropped by the query-string
        // decoder before the builder ever sees it, so over HTTP it is an absent filter rather
        // than an unreadable one. That asymmetry with the CLI is issue #13, not this change.
        for uri in [
            "/api/search?tool_input=command",
            "/api/search?tool_input=k%3D",
            "/api/facets/tool_name?tool_input=command",
        ] {
            let (status, body) = server.get(uri);
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
            let message = error_message(&body);
            assert!(
                !message.contains("--"),
                "over HTTP there is no flag: {uri}: {message}"
            );
        }

        let (status, body) = server.get("/api/search?tool_input=command%3Dcargo");
        assert_eq!(
            status,
            StatusCode::OK,
            "a readable filter still answers: {body}"
        );
    }

    #[test]
    fn the_session_listing_reports_what_is_indexed() {
        let server = server();
        let (status, body) = server.get("/api/sessions");
        assert_eq!(status, StatusCode::OK, "{body}");
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "{body}");
        assert_eq!(sessions[0]["session_id"], json!(SESSION));
        assert_eq!(body["total"], json!(1));
    }

    #[test]
    fn a_filter_session_metadata_cannot_answer_is_reported_not_ignored() {
        let server = server();
        let (status, body) = server.get("/api/sessions?model=claude-opus-5");
        assert_eq!(status, StatusCode::OK, "{body}");
        // Silently ignoring it would hand back the whole listing as though it had been
        // filtered by model, which is the one reading that is certainly wrong.
        let warnings = body["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap().contains("model")),
            "{body}"
        );
    }

    #[test]
    fn a_session_prefix_resolves_to_its_documents_and_to_a_window() {
        let server = server();
        let prefix = &SESSION[..8];

        let (status, body) = server.get(&format!("/api/sessions/{prefix}"));
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session_id"], json!(SESSION));
        assert_eq!(body["agent_id"], Value::Null);
        assert!(body["source_path"].as_str().unwrap().ends_with(".jsonl"));
        let docs = body["docs"].as_array().unwrap();
        assert!(docs.len() > 5, "{body}");
        assert_eq!(body["truncated"], json!(false));

        // A window is centred on a `seq`, and `seq` only means anything within one file.
        let (status, window) = server.get(&format!(
            "/api/sessions/{prefix}/around?seq=3&before=1&after=1"
        ));
        assert_eq!(status, StatusCode::OK, "{window}");
        let seqs: Vec<u64> = window["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![2, 3, 4], "{window}");
    }

    #[test]
    fn a_window_without_a_seq_says_so() {
        let server = server();
        let (status, body) = server.get(&format!("/api/sessions/{SESSION}/around"));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(error_message(&body).contains("seq"));
    }

    #[test]
    fn include_raw_is_what_carries_the_jsonl_line() {
        let server = server();
        let (_, lean) = server.get(&format!("/api/sessions/{SESSION}?limit=1"));
        // 40 copies of the original line is the single biggest thing a window can carry, so it
        // is opt-in — but opting in has to actually produce it.
        assert!(lean["docs"][0].get("raw").is_none(), "{lean}");

        let (_, full) = server.get(&format!("/api/sessions/{SESSION}?limit=1&include_raw=1"));
        assert!(
            full["docs"][0]["raw"].as_str().unwrap().contains('{'),
            "{full}"
        );
    }

    #[test]
    fn an_unknown_session_is_a_404_not_an_empty_document_list() {
        let server = server();
        let (status, body) = server.get("/api/sessions/ffffffff");
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(error_message(&body).contains("/api/sessions"));
    }

    #[test]
    fn an_unrouted_path_is_a_404_that_lists_the_api() {
        let server = server();
        let (status, body) = server.get("/api/serch");
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let message = error_message(&body);
        assert!(message.contains("/api/search"), "{message}");
    }

    #[test]
    fn health_and_stats_describe_this_index() {
        let server = server();
        let (status, health) = server.get("/api/health");
        assert_eq!(status, StatusCode::OK, "{health}");
        assert_eq!(health["ok"], json!(true));
        assert_eq!(health["web_ui"], json!(cfg!(feature = "web-ui")));

        let (status, stats) = server.get("/api/stats");
        assert_eq!(status, StatusCode::OK, "{stats}");
        assert!(stats["docs"].as_u64().unwrap() > 5, "{stats}");
        assert_eq!(stats["sessions"], json!(1));
        assert!(stats["size_bytes"].as_u64().unwrap() > 0, "{stats}");
        assert_eq!(stats["roots"].as_array().unwrap().len(), 1, "{stats}");
    }

    #[test]
    fn cors_echoes_only_an_allowed_origin() {
        let server = server_with_cors(vec!["http://localhost:5173".into()]);

        let (status, headers, _) = server.send(
            Request::builder()
                .uri("/api/health")
                .header(header::ORIGIN, "http://localhost:5173")
                .body(Body::empty())
                .unwrap(),
        );
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .map(|v| v.to_str().unwrap()),
            Some("http://localhost:5173")
        );

        // An origin nobody listed gets no header, which is what stops the browser reading it.
        let (_, headers, _) = server.send(
            Request::builder()
                .uri("/api/health")
                .header(header::ORIGIN, "http://evil.example")
                .body(Body::empty())
                .unwrap(),
        );
        assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());

        // Preflight is answered by the middleware; the route itself is GET-only and the router
        // would otherwise reject OPTIONS with a 405 the browser reports as a CORS failure.
        let (status, headers, _) = server.send(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/search")
                .header(header::ORIGIN, "http://localhost:5173")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .body(Body::empty())
                .unwrap(),
        );
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("POST")
        );
    }

    #[test]
    fn cors_is_off_unless_it_was_asked_for() {
        let server = server();
        let (_, headers, _) = server.send(
            Request::builder()
                .uri("/api/health")
                .header(header::ORIGIN, "http://localhost:5173")
                .body(Body::empty())
                .unwrap(),
        );
        assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[cfg(feature = "web-ui")]
    #[test]
    fn the_bundled_ui_is_served_with_the_content_types_a_browser_needs() {
        let server = server();
        // A module served as text/plain does not execute, and the page fails with a console
        // message about a MIME type rather than anything that names this server.
        for (path, expected) in [
            ("/", "text/html; charset=utf-8"),
            ("/index.html", "text/html; charset=utf-8"),
            ("/styles.css", "text/css; charset=utf-8"),
            ("/app.js", "text/javascript; charset=utf-8"),
            ("/dom.js", "text/javascript; charset=utf-8"),
            ("/tools.js", "text/javascript; charset=utf-8"),
            ("/markdown.js", "text/javascript; charset=utf-8"),
        ] {
            let (status, headers, body) =
                server.send(Request::builder().uri(path).body(Body::empty()).unwrap());
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(
                headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
                expected,
                "{path}"
            );
            assert_eq!(
                headers
                    .get(header::CACHE_CONTROL)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "no-cache",
                "{path}"
            );
            assert!(!body.is_empty(), "{path} served nothing");
        }
    }

    #[cfg(not(feature = "web-ui"))]
    #[test]
    fn the_root_without_a_ui_points_at_the_api_rather_than_404ing() {
        let server = server();
        let (status, body) = server.get("/");
        // `/` is the first URL anyone opens, and a 404 there reads as "the server is broken"
        // rather than "this build has no UI".
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["web_ui"], json!(false));
        assert_eq!(body["health"], json!("/api/health"));
    }

    #[test]
    fn a_route_used_with_the_wrong_method_answers_in_json_like_everything_else() {
        let server = server();
        // Axum's built-in 405 has an empty body. `curl /api/reindex` — no `-X POST` — would
        // then print nothing at all, which reads as a hung server rather than a wrong verb.
        for (method, uri, wanted) in [
            (Method::GET, "/api/reindex", "POST"),
            (Method::POST, "/api/health", "GET"),
        ] {
            let (status, _, body) = server.send(
                Request::builder()
                    .method(method.clone())
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            );
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {uri}");
            let message = error_message(&json_body(&body));
            assert!(message.contains(uri), "{message}");
            assert!(message.contains(wanted), "names the verb to use: {message}");
        }
    }

    #[cfg(feature = "web-ui")]
    #[test]
    fn an_unknown_asset_path_falls_through_to_the_api_404() {
        let server = server();
        let (status, body) = server.get("/vendor.js");
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(error_message(&body).contains("/api/health"));
    }
}
