//! The MCP stdio server: five read-only tools over the index the CLI already searches.
//!
//! Shape, and why. `rmcp` builds a tool's input schema from the `Parameters<T>` struct and its
//! output schema from a return type that *literally reads* `Json<T>`, so the request and
//! response types in [`types`] are the contract: change one and every client's schema changes
//! with it. The prose on each `#[tool]` method below becomes the tool's description, and the
//! `schemars(description = …)` attributes on [`crate::search::Filters`] become the eighteen
//! filter descriptions. Per issue #28, those descriptions are the work — there is more leverage
//! in them than in the code that reads them.
//!
//! # stdout belongs to the transport
//!
//! The MCP framing *is* stdout: one JSON-RPC message per line and nothing else. So nothing in
//! this module, or anything it calls, may write there. `tracing` goes to stderr (see
//! `main::init_tracing`), colour is off, and the human renderers in `format.rs` — which take a
//! `&mut impl Write` rather than reaching for stdout — are simply never called. A single stray
//! `println!` in a library module becomes a protocol parse error on the client side with no
//! useful diagnostic, which is why `stdout_belongs_to_the_transport` below is a test rather
//! than a comment.
//!
//! # Index refresh policy
//!
//! One `tantivy::Index` and one [`Fields`] are opened at startup and held for the life of the
//! process. Each call takes its own reader — `search::search`, `search::facets` and the
//! `context::*` entry points each call `index.reader()?.searcher()` internally — which is what
//! lets this server reuse those signatures verbatim rather than pinning a new set that threads
//! an `IndexReader` through. That question was open in `docs/MCP.md` and this answers it: a
//! hoisted reader would buy one avoided `reader()` call per request and cost a change to every
//! search entry point, plus a reader that has to be told when to reload.
//!
//! Refreshing is a different question, because it *writes*. The CLI re-indexes before every read
//! command, which is right for a process that lives for 200 ms and wrong for one that lives for
//! a day: a model's tool call would pay for an incremental index scan, and two concurrent calls
//! would contend for the writer lock. So:
//!
//! * **once at startup**, before the transport opens, unless `--no-refresh`. A server that came
//!   up against a week-old index would answer "0 results" about work that happened yesterday,
//!   and the zero-hit envelope would blame a filter for it;
//! * **at most once every `--refresh-secs`** afterwards, checked at the start of a tool call and
//!   guarded so concurrent calls never run two indexers. `0` disables it, and the server then
//!   only ever sees what was indexed at startup;
//! * a refresh failure is logged to stderr and the call proceeds against the index as it stands.
//!   An unreadable transcript root must not stop the server answering from what it has.
//!
//! Because each call opens a fresh reader, a refresh performed by this process — or by a
//! `session-search index` running beside it — is visible to the very next call with no reader
//! reload to arrange.

pub mod envelope;
pub mod tools;
pub mod types;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};

use crate::schema::Fields;
use crate::sessions::FilterError;
use types::{
    AggregateRequest, AggregateResponse, Corpus, GetOutputRequest, GetOutputResponse,
    GetTurnRequest, GetTurnResponse, SearchSessionsRequest, SearchSessionsResponse,
    SearchTurnsRequest, SearchTurnsResponse,
};

/// How often the server may re-index while it runs, when the caller does not say. Five minutes:
/// long enough that a burst of tool calls costs one scan, short enough that a session the model
/// is *currently* in becomes searchable within the same conversation.
pub const DEFAULT_REFRESH_SECS: u64 = 300;

/// Startup configuration. The index directory is passed separately because it is resolved by
/// `cli::run` from the global `--index` / `$SESSION_SEARCH_INDEX`, exactly as every other
/// command resolves it.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Seconds between permitted re-indexes. `0` never re-indexes after startup.
    pub refresh_secs: u64,
    /// Skip the startup refresh as well. Serves whatever is already in the index directory.
    pub no_refresh: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            refresh_secs: DEFAULT_REFRESH_SECS,
            no_refresh: false,
        }
    }
}

/// Everything a tool body needs: the index, its field handles, where it lives, and the refresh
/// clock.
///
/// Shared behind an `Arc` and reachable from concurrent calls, so every field is either
/// immutable or behind its own lock. rmcp handles requests concurrently and may write responses
/// out of order; nothing here may assume it is alone.
pub struct State {
    pub index: tantivy::Index,
    pub fields: Fields,
    /// The index directory, for `load_sessions` and for the refresh.
    pub index_dir: PathBuf,
    /// Counts reported in `instructions` and in a no-filter zero-hit message. Read at startup
    /// and not updated by a refresh: they are an orientation for the model, not an answer, and
    /// re-deriving them per call would cost a `sessions.json` parse on every request.
    pub corpus: Corpus,
    refresh_secs: u64,
    /// When a refresh was last attempted. `None` means never.
    last_refresh: Mutex<Option<Instant>>,
}

impl State {
    /// Re-index if the interval has elapsed. Never fatal: an unreadable transcript root must not
    /// stop the server answering from what it already holds.
    ///
    /// The lock is held across the refresh on purpose. Two concurrent calls must not run two
    /// indexers against the same writer lock, and the second one waiting a moment is cheaper
    /// than the contention it would otherwise hit inside tantivy.
    pub fn refresh_if_due(&self) {
        if self.refresh_secs == 0 {
            return;
        }
        let interval = Duration::from_secs(self.refresh_secs);
        let Ok(mut last) = self.last_refresh.lock() else {
            // A poisoned lock means a previous refresh panicked. Serving a slightly stale index
            // is strictly better than propagating that panic into every later tool call.
            tracing::warn!("refresh lock poisoned; serving the index as it stands");
            return;
        };
        if last.is_some_and(|at| at.elapsed() < interval) {
            return;
        }
        *last = Some(Instant::now());
        crate::cli::refresh(&self.index_dir, false, false);
    }

    /// Sessions on disk, for `search_sessions` and for turn addressing.
    pub fn sessions(
        &self,
    ) -> anyhow::Result<std::collections::BTreeMap<String, crate::parse::SessionInfo>> {
        crate::index::load_sessions(&self.index_dir)
    }
}

/// The MCP server. `Clone` over an `Arc`, because `ToolRouter<S>` requires `S: Send + 'static`
/// and every handler takes `&self`.
#[derive(Clone)]
pub struct Server {
    state: Arc<State>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl Server {
    /// Open the index and read the corpus counts. Does not refresh; [`serve`] does that first.
    pub fn new(index_dir: &Path) -> anyhow::Result<Server> {
        Server::with_options(index_dir, &ServeOptions::default())
    }

    pub fn with_options(index_dir: &Path, opts: &ServeOptions) -> anyhow::Result<Server> {
        let (index, fields) = crate::index::open_or_create(index_dir)?;
        let corpus = read_corpus(index_dir);
        Ok(Server {
            state: Arc::new(State {
                index,
                fields,
                index_dir: index_dir.to_path_buf(),
                corpus,
                refresh_secs: opts.refresh_secs,
                last_refresh: Mutex::new(Some(Instant::now())),
            }),
            tool_router: Server::tool_router(),
        })
    }

    /// Full-text search over transcript documents, returned as turn skeletons.
    ///
    /// This is the retrieval tool: reach for it when you need to *read* a specific moment — why a
    /// build failed, how an error was fixed, what command actually worked. One query searches the
    /// prose, the code, the tool output, the markdown headings and the tool inputs at once, plus a
    /// per-document context header carrying the session title and the turn's opening prompt. The
    /// header is what makes a paraphrase work: "build fails" finds a session where neither word
    /// appears in any message body, only in its title.
    ///
    /// Returns one result per conversational turn, ranked by relevance, each as a skeleton — the
    /// prompt, the prose, and one line per tool call ending in `-> ok`, `-> no result`, or
    /// `-> error:` followed by the first line of what failed. Every result carries the pair
    /// `(source_path, turn_seq)` that addresses its turn, and a count of the other documents in that
    /// turn that also matched. Tool output is not here; that is the point.
    ///
    /// Then drill: `get_turn` on the one turn worth reading in full, `get_output` on the one call
    /// whose output you need. Not this tool when the answer is a distribution rather than a passage
    /// (`aggregate`), when the answer is which sessions rather than which moments
    /// (`search_sessions`), or when you already hold a turn reference (`get_turn` — searching for a
    /// turn you can already address wastes a call and may not return it).
    ///
    /// Query grammar: bare words are ANDed, `"quoted phrases"` are exact, `AND`/`OR`/`NOT` and
    /// `field:value` work. A leading `-` is negation, so `cargo build --release` asks for documents
    /// that do NOT contain `release`; quote anything with a flag in it. Do not put a filter's value
    /// in the query — `cargo` as free text also matches `Cargo.toml`, `--cargo-flag` and a path
    /// segment; use the `program` filter for "the program that ran".
    #[tool(
        name = "search_turns",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_turns(
        &self,
        Parameters(req): Parameters<SearchTurnsRequest>,
    ) -> Result<Json<SearchTurnsResponse>, ErrorData> {
        self.blocking(move |state| tools::turns::run(state, req))
            .await
    }

    /// Every document of one conversational turn, in order, with its full text.
    ///
    /// The second step of a skeleton-first search: `search_turns` told you which turn, this returns
    /// what it actually said — the prompt, the assistant messages, each tool call with its parsed
    /// input, and each call's result. Also the way to read a session forwards or backwards: ask for
    /// the neighbouring turns of the one you landed on.
    ///
    /// Address a turn by the pair `(source_path, turn_seq)` as returned, or by a single `doc_id`,
    /// which resolves to that document's turn. `turn_seq` alone is not an address: it is an ordinal
    /// within one file, and two transcripts can share a session id, so a bare number can name two
    /// different conversations.
    ///
    /// Tool results come back truncated to a per-document budget with a marker saying what was cut,
    /// because one turn can carry hundreds of kilobytes of logs. Not this tool when you want a
    /// command's output in full — that is `get_output`, which slices instead of truncating — and not
    /// this tool for browsing: a turn you have not already located by search is a turn you are
    /// guessing at, and a sidechain is a single turn covering an entire subagent transcript, so it
    /// will hit the cap.
    #[tool(
        name = "get_turn",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_turn(
        &self,
        Parameters(req): Parameters<GetTurnRequest>,
    ) -> Result<Json<GetTurnResponse>, ErrorData> {
        self.blocking(move |state| tools::drill::run_turn(state, req))
            .await
    }

    /// The full result of one tool call, sliced so it fits a budget.
    ///
    /// The last step of the drill-down, and the only route to a tool's output in full: skeletons omit
    /// output entirely and `get_turn` truncates it. Use it when the answer is in what a command
    /// printed — the compiler error under the first line, the rows a query returned, the diff a
    /// command refused to apply.
    ///
    /// Address the call by `doc_id`, or by `tool_use_id` if that is what you are holding. Slice with
    /// `head`, `tail` and `grep` rather than pulling the whole thing: a build log is routinely 200 KB
    /// and the six lines you want are findable. The response always reports the total size and what
    /// the slice left out, so a partial answer never reads as a complete one.
    ///
    /// Not this tool for finding *which* call to read — that is `search_turns`, or the `tool_output`
    /// filter if you know the exact phrase the output contains. Not this tool for counting across
    /// many outputs; that is `aggregate`. A call whose result never reached the index returns an
    /// explicit "no result" rather than an empty string, because a tool that was interrupted and a
    /// tool that printed nothing are different facts.
    #[tool(
        name = "get_output",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_output(
        &self,
        Parameters(req): Parameters<GetOutputRequest>,
    ) -> Result<Json<GetOutputResponse>, ErrorData> {
        self.blocking(move |state| tools::drill::run_output(state, req))
            .await
    }

    /// Indexed sessions, most recent first, with the metadata that identifies them.
    ///
    /// The tool for "what was I working on last week", "which sessions touched this repo", "when did
    /// I last look at this". Each row is a session, not a passage: id, subagent id and type, title,
    /// opening prompt, project, git branch, first and last timestamp, message count and tool-call
    /// count. Answering that question with `search_turns` gives you twenty documents from three
    /// sessions and makes you infer the list; this gives you the list.
    ///
    /// Reads session metadata, not the document index, so the per-message filters do not apply and
    /// are ignored with a warning rather than silently narrowing anything: `tool`, `tool_input`,
    /// `tool_output`, `program`, `lang`, `model`, `role`, `kind`, `errors_only`. What does apply is
    /// what a session has: `project`, `session`, `branch`, `agent_type`, `since`, `until`, and the
    /// sidechain flags.
    ///
    /// Not this tool when you need the content of what was said — take the session id from here and
    /// pass it to `search_turns` as the `session` filter, or `get_turn` for a specific turn. Not this
    /// tool for "how many sessions used cargo": a program is a per-message fact, so that is
    /// `aggregate` on `session_id` with a `program` filter.
    #[tool(
        name = "search_sessions",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_sessions(
        &self,
        Parameters(req): Parameters<SearchSessionsRequest>,
    ) -> Result<Json<SearchSessionsResponse>, ErrorData> {
        self.blocking(move |state| tools::sessions::run(state, req))
            .await
    }

    /// Count the distinct values of one field across everything that matches a query and filters.
    ///
    /// The tool for questions whose answer is a table: what errors did we see, which files failed to
    /// read, which programs does this project run, which tools error most, which languages show up
    /// around the index. These have no defensible top-k — seven failures spread over four tools, and
    /// any ten of them ranked by relevance is an arbitrary sample of a distribution. Ranking answers
    /// them by accident and answers them differently for every limit passed.
    ///
    /// Returns the buckets, plus the four counts needed to read them honestly: `matching_docs`,
    /// `docs_with_value`, `other_docs` and `distinct`. Read those before reporting a total. The
    /// buckets do not sum to anything you can quote.
    ///
    /// Not this tool when you need to read what was said — a bucket is a value and a number, never a
    /// passage, so follow up with `search_turns` filtered to the value you found. Not this tool when
    /// the field's values barely repeat: a response flagged as search-shaped (near-unique values,
    /// such as whole shell commands) is a sample of a long tail, not a distribution, and the field
    /// wants full-text search instead.
    #[tool(
        name = "aggregate",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn aggregate(
        &self,
        Parameters(req): Parameters<AggregateRequest>,
    ) -> Result<Json<AggregateResponse>, ErrorData> {
        self.blocking(move |state| tools::aggregate::run(state, req))
            .await
    }
}

impl Server {
    /// Run one tool body off the async runtime, refreshing the index first if it is due.
    ///
    /// Every tool body is blocking, CPU-bound tantivy work, and a refresh is blocking I/O; both
    /// would stall the reactor that owns the stdio transport. `spawn_blocking` is where they
    /// belong, and it is also what makes the concurrency in the note at the top of this module
    /// real rather than nominal.
    async fn blocking<T, F>(&self, body: F) -> Result<Json<T>, ErrorData>
    where
        F: FnOnce(&State) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let state = Arc::clone(&self.state);
        let joined = tokio::task::spawn_blocking(move || {
            state.refresh_if_due();
            body(&state)
        })
        .await;
        match joined {
            Ok(Ok(value)) => Ok(Json(value)),
            Ok(Err(err)) => Err(from_anyhow(&err)),
            Err(err) => Err(ErrorData::internal_error(
                format!("tool task failed: {err}"),
                None,
            )),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo::new` calls `Implementation::from_build_env()`, whose `env!` expands inside
        // the rmcp crate — so the default `serverInfo` on the wire is `{"name":"rmcp",
        // "version":"3.2.0"}`. Naming ourselves is not optional.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "session-search",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions(&self.state))
    }
}

/// Serve on stdio until the client disconnects.
///
/// Sync, like `api::serve`, so `cli::dispatch` stays one non-async match: the runtime is built
/// here and does not leak into the CLI.
pub fn serve(index_dir: &Path, opts: ServeOptions) -> anyhow::Result<()> {
    use anyhow::Context as _;

    // Before the transport opens, so the first tool call does not pay for it and so a server
    // that came up against a stale index is not answering "0 results" about yesterday.
    if !opts.no_refresh {
        crate::cli::refresh(index_dir, false, false);
    }

    let server = Server::with_options(index_dir, &opts)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("session-search-mcp")
        .build()
        .context("building the Tokio runtime")?;

    runtime.block_on(async move {
        tracing::info!(dir = %index_dir.display(), "serving MCP on stdio");
        let service = server
            .serve(stdio())
            .await
            .context("starting the MCP stdio server")?;
        service.waiting().await.context("serving MCP")?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// A caller mistake: a malformed date, a reference that names nothing, a missing address.
///
/// `ErrorData` becomes a JSON-RPC protocol error, which clients render opaquely and models often
/// cannot act on. It is used only for requests that could not be *started* — everything a tool
/// can answer, including "nothing matched", comes back as a normal result with an
/// [`types::Envelope`] explaining itself.
pub fn invalid_params(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(message.into(), None)
}

/// A malformed `since`/`until`, reported as the caller's mistake rather than the server's.
pub fn from_filter_error(err: &FilterError) -> ErrorData {
    invalid_params(format!("{err:#}"))
}

/// Anything a tool body failed with. Kept as `anyhow` down there so tool code reads like the
/// rest of the crate; the classification happens once, here.
fn from_anyhow(err: &anyhow::Error) -> ErrorData {
    if let Some(filter) = err.downcast_ref::<FilterError>() {
        return from_filter_error(filter);
    }
    ErrorData::internal_error(format!("{err:#}"), None)
}

// ---------------------------------------------------------------------------
// instructions
// ---------------------------------------------------------------------------

/// Corpus counts, read once at startup, without opening the tantivy index.
fn read_corpus(index_dir: &Path) -> Corpus {
    let sessions = crate::index::load_sessions(index_dir).unwrap_or_default();
    let newest_ts = sessions
        .values()
        .filter_map(|s| s.last_ts_ms)
        .max()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let docs = crate::cli::index_stats(index_dir)
        .map(|s| s.docs_added)
        .unwrap_or_default();
    Corpus {
        sessions: sessions.len(),
        docs,
        newest_ts,
    }
}

/// The server's `instructions`: what this index is, how to route a question to a tool, and the
/// two facts about addressing and zero-hit answers that a caller cannot infer from the schemas.
fn instructions(state: &State) -> String {
    format!(
        r#"session-search indexes the Claude Code session transcripts on this machine — every prompt,
assistant message, tool call and tool result under the transcript root, subagent sidechains
included. It is the record of work already done on this computer: what was tried, what it
printed, what failed, and when. It knows nothing about anything else. Every tool here is
read-only: nothing writes to a transcript, and nothing writes to the index.

Index: {index_dir} · {sessions} sessions · {docs} documents · newest transcript {newest_ts}.

Route by the shape of the answer, not by the words in the question.

- The answer is a list of SESSIONS — "what was I working on last week", "which sessions
  touched this repo", "when did I last work on X" — use `search_sessions`. It reads session
  metadata (title, opening prompt, project, branch, first and last timestamp, message and tool
  counts), so one call answers what twenty passage hits only imply.
- The answer is a MOMENT you have to read — "why did the build fail", "have I hit this error
  before", "what was the command that actually worked" — use `search_turns`, then drill in with
  `get_turn` and `get_output`.
- The answer is a TABLE of values and counts — "what errors did we see", "which files failed to
  read", "which programs does this project run", "which tools error most" — use `aggregate`.
  A ranked list answers a distribution question only by accident, and answers it differently for
  every limit you happen to pass.

Skeleton first, then drill. `search_turns` returns one result per conversational turn as a
skeleton: the human prompt, the assistant prose, and one line per tool call giving its signature
and how it ended — with tool output omitted entirely, except the first line of a failed call.
Tool output is most of the bytes in a transcript and almost none of the intent; a turn's full
context averages 5,651 bytes and its skeleton 555. Read twenty skeletons, decide which single
turn answers the question, and pay full price only there: `get_turn` for that turn's documents in
full, `get_output` for one call's output, sliced by head, tail or grep so a 200 KB log costs a few
hundred bytes. Do not ask for turns in full to find out which one you want.

A turn is addressed by the pair (source_path, turn_seq), never by turn_seq alone: turn_seq is an
ordinal within one file, and two transcripts can legitimately carry the same session id. Pass the
pair back exactly as it was returned.

Filters are ANDed, and every one is matched exactly unless its description says otherwise. A
value outside a field's vocabulary matches nothing rather than erroring, so "0 results" is as
often a misspelled filter as it is an empty corpus — read a field's description before guessing
its value. A zero-hit response tells you the filters that were actually applied, the resolved
absolute time range, and which single filter to drop first; take that suggestion before rewriting
the query.

Two things no tool will give you: assistant thinking never appears in a skeleton at any budget,
and nothing here returns the raw transcript line."#,
        index_dir = state.index_dir.display(),
        sessions = state.corpus.sessions,
        docs = state.corpus.docs,
        newest_ts = state
            .corpus
            .newest_ts
            .as_deref()
            .unwrap_or("(nothing indexed)"),
    )
}

#[cfg(test)]
mod tests {
    /// The MCP framing *is* stdout. A `println!` anywhere in this module tree becomes a
    /// protocol parse error on the client with no useful diagnostic, and it would appear at the
    /// exact moment some rare branch is taken — so it is checked here rather than trusted.
    ///
    /// Source-level rather than a running server on purpose: a test that drives the transport
    /// only proves the branches it happened to take, and the failure this guards against lives
    /// in the branch nobody exercised.
    #[test]
    fn stdout_belongs_to_the_transport() {
        // Comments are stripped first: this module's own docs discuss the failure by name, and
        // a scanner that matched them would be unfixable without making the docs worse.
        let code = |source: &str| {
            source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<String>()
        };
        for (name, source) in [
            ("mcp/mod.rs", include_str!("mod.rs")),
            ("mcp/types.rs", include_str!("types.rs")),
            ("mcp/envelope.rs", include_str!("envelope.rs")),
            ("mcp/tools/mod.rs", include_str!("tools/mod.rs")),
            ("mcp/tools/turns.rs", include_str!("tools/turns.rs")),
            ("mcp/tools/drill.rs", include_str!("tools/drill.rs")),
            ("mcp/tools/sessions.rs", include_str!("tools/sessions.rs")),
            ("mcp/tools/aggregate.rs", include_str!("tools/aggregate.rs")),
        ] {
            // Assembled from fragments so this list does not match its own source file.
            for needle in [
                concat!("print", "ln!"),
                concat!("pri", "nt!"),
                concat!("io::std", "out"),
            ] {
                assert!(
                    !code(source).contains(needle),
                    "{name} writes to stdout ({needle}); the MCP transport owns it"
                );
            }
        }
    }

    #[test]
    fn the_instructions_substitute_every_runtime_value() {
        let state = super::State {
            index: tantivy::Index::create_in_ram(crate::schema::build_schema().0),
            fields: crate::schema::build_schema().1,
            index_dir: std::path::PathBuf::from("/tmp/session-search-index"),
            corpus: super::Corpus {
                sessions: 412,
                docs: 190_233,
                newest_ts: Some("2026-09-10T11:20:14Z".into()),
            },
            refresh_secs: 0,
            last_refresh: std::sync::Mutex::new(None),
        };
        let text = super::instructions(&state);
        assert!(
            !text.contains('{'),
            "an unsubstituted placeholder survived: {text}"
        );
        assert!(text.contains(
            "Index: /tmp/session-search-index · 412 sessions · 190233 documents · newest \
             transcript 2026-09-10T11:20:14Z."
        ));
        // The routing table is the load-bearing half; a truncated instruction string would
        // silently stop routing questions to the right tool.
        for tool in [
            "search_sessions",
            "search_turns",
            "get_turn",
            "get_output",
            "aggregate",
        ] {
            assert!(text.contains(tool), "instructions never mention {tool}");
        }
    }

    #[test]
    fn an_empty_index_still_produces_usable_instructions() {
        let state = super::State {
            index: tantivy::Index::create_in_ram(crate::schema::build_schema().0),
            fields: crate::schema::build_schema().1,
            index_dir: std::path::PathBuf::from("/tmp/empty"),
            corpus: super::Corpus::default(),
            refresh_secs: 0,
            last_refresh: std::sync::Mutex::new(None),
        };
        let text = super::instructions(&state);
        assert!(
            text.contains("newest transcript (nothing indexed)"),
            "{text}"
        );
    }
}
