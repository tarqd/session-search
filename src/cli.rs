//! clap definitions and the command dispatch for the `session-search` binary.
//!
//! `main.rs` does nothing but build the subscriber and hand the parsed [`Cli`] to [`run`];
//! everything that decides *what* to do lives here, and everything that decides *how it
//! looks* lives in `format.rs`. The filter set is `search::Filters` itself — one struct wearing
//! both a `clap::Args` and a `serde::Deserialize` hat — so the MCP server that follows accepts
//! exactly the arguments the CLI does, with no translation layer to drift.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::format::{self, OutputOpts};
use crate::index::{self, IndexOptions, IndexStats};
use crate::parse::SessionInfo;
use crate::search::{self, Filters, SearchRequest, SimilarField, SortBy};
use crate::sessions::{self, SessionMatcher};
use crate::{context, discovery};

/// Facet buckets returned alongside `search --facets`. `facets` has `--top` for the same knob;
/// on `search` the facets are a side dish, so the depth is fixed.
const SEARCH_FACET_TOP: usize = 15;
/// Snippet budget, in characters, for a search hit.
const SNIPPET_CHARS: usize = 240;
/// Documents one `--context turn` window may show, per hit.
///
/// A turn is not a bounded thing: one prompt can spawn hundreds of tool calls over an hour, and
/// a sidechain file is a single turn by rule 3 of the "Turns" section, so "the turn" there is a
/// whole subagent transcript. `TopDocs` preallocates whatever it is handed, so an uncapped turn
/// is a process abort rather than a long scroll — and twenty hits would each pay for it. What
/// the cap leaves out is reported, never dropped silently. `show --around ... --turn` has
/// `--limit` for the same job, and defers to whatever the caller set there.
const TURN_WINDOW_LIMIT: usize = 200;

/// What `search --context` asks for beside each hit.
///
/// A fixed `N` is the wrong shape for a hit inside a forty-call turn — it shows three
/// neighbouring `Bash` calls and never the prompt that explains them — and the wrong shape for a
/// short turn too, where it drags in turns that have nothing to do with the hit. `turn` snaps to
/// the boundary the transcript already defines, so what comes back is the prompt, what was
/// tried and what came back, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindow {
    /// N documents either side of the hit, as `context::around` returns them.
    Docs(usize),
    /// The hit's whole enclosing turn, capped at `TURN_WINDOW_LIMIT`.
    Turn,
    /// The same window as [`ContextWindow::Turn`], rendered as a skeleton: one line per
    /// document, call signatures without their output. A window, not a filter — the fetch is
    /// identical and only the rendering differs, which is why it lives on `--context`.
    Skeleton,
}

impl std::str::FromStr for ContextWindow {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.trim();
        if value.eq_ignore_ascii_case("turn") {
            return Ok(ContextWindow::Turn);
        }
        if value.eq_ignore_ascii_case("skeleton") {
            return Ok(ContextWindow::Skeleton);
        }
        value
            .parse::<usize>()
            .map(ContextWindow::Docs)
            .map_err(|_| format!("expected a document count, `turn` or `skeleton`, got {value:?}"))
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "session-search",
    version,
    about = "Search Claude Code session transcripts",
    max_term_width = 100
)]
pub struct Cli {
    /// Index directory. Defaults to `$XDG_DATA_HOME/session-search`.
    #[arg(long, global = true, env = "SESSION_SEARCH_INDEX", value_name = "DIR")]
    pub index: Option<PathBuf>,
    /// Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv` trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty stdout.
    #[arg(long, global = true)]
    pub no_color: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Index (or re-index) transcripts.
    Index {
        /// Ignore the watermarks and rebuild every file from scratch.
        #[arg(long)]
        full: bool,
        /// Transcript root; repeatable. Defaults to `$CLAUDE_CONFIG_DIR/projects`.
        #[arg(long = "root", value_name = "DIR")]
        roots: Vec<PathBuf>,
        /// Parser threads. Defaults to the rayon pool size.
        #[arg(long, value_name = "N")]
        jobs: Option<usize>,
        /// Do NOT index assistant thinking blocks. Thinking is indexed by default; searching
        /// it still requires `search --include-thinking`. Switching this later needs
        /// `index --full`, since the watermarks say nothing changed.
        #[arg(long)]
        no_thinking: bool,
        /// Do not follow `Full output saved to: <path>` pointers into `tool-results/`;
        /// index only the "output too large" stub the transcript carries inline.
        #[arg(long)]
        no_spilled_results: bool,
        /// Cap on each indexed body field of one document, in bytes. Defaults to 1 MiB, which
        /// is far above anything a transcript carries; lower it to keep a pathological
        /// `cat` of a binary out of the term dictionary. Changing it needs `index --full`.
        #[arg(long = "max-text-bytes", value_name = "BYTES")]
        max_text_bytes: Option<usize>,
    },
    /// Full-text search.
    Search {
        /// Query string: words, "phrases", AND/OR/NOT, `field:value`.
        query: Option<String>,
        /// Comma-separated facet fields, e.g. `tool_name,code_lang,tool_input.file_path`.
        #[arg(long, value_delimiter = ',', value_name = "FIELD")]
        facets: Vec<String>,
        /// Also show N documents either side of each hit, `turn` for the hit's whole enclosing
        /// turn — the prompt that opened it, what was tried, and what came back — or
        /// `skeleton` for that same turn as one line per document: call signatures with no
        /// output, except the first line of a failed one.
        #[arg(long, default_value = "0", value_name = "N|turn|skeleton")]
        context: ContextWindow,
        /// Collapse hits that share a turn into one, keeping the best-scoring member and
        /// reporting how many others matched. `--limit` and `--offset` then count turns.
        #[arg(long = "group-by-turn")]
        group_by_turn: bool,
        #[arg(long, default_value_t = 20, value_name = "N")]
        limit: usize,
        #[arg(long, default_value_t = 0, value_name = "N")]
        offset: usize,
        /// One JSON object on stdout instead of the human rendering.
        #[arg(long)]
        json: bool,
        /// Skip the incremental index refresh that normally runs first.
        #[arg(long)]
        no_refresh: bool,
        /// Search assistant thinking blocks too.
        #[arg(long)]
        include_thinking: bool,
        /// Hit order. Relevance is meaningless without a query, so a filter-only search is
        /// worth ordering by time.
        #[arg(long, value_enum, default_value_t = SortBy::Relevance, value_name = "ORDER")]
        sort: SortBy,
        /// Rank by similarity to the turn a document reference names. REF is `SESSION:SEQ`,
        /// `SESSION:AGENT:SEQ`, a record uuid or a `doc_id` — any of them by unambiguous
        /// prefix. Composes with the query and with every filter.
        #[arg(long = "similar-to", value_name = "REF")]
        similar_to: Option<String>,
        /// Which bodies of the source turn seed the similarity: `text` (the default), `code`,
        /// `tool_output`, `thinking`. Comma-separated. `thinking` additionally needs
        /// `--include-thinking`.
        // No `default_value`: an empty vector means "the default", spelled out in `dispatch`,
        // which keeps `SimilarField` free of the `Display` impl `default_values_t` would ask
        // for on an enum whose display name is already its `ValueEnum` name.
        #[arg(
            long = "similar-in",
            value_enum,
            value_delimiter = ',',
            value_name = "FIELD",
            requires = "similar_to"
        )]
        similar_in: Vec<SimilarField>,
        /// Keep the source turn in the results of a `--similar-to` search. It is left out by
        /// default, because it is the turn you are already looking at.
        #[arg(long = "include-source", requires = "similar_to")]
        include_source: bool,
        #[command(flatten, next_help_heading = "Filters")]
        filters: Filters,
    },
    /// Count values of a fast field or any `tool_input.<path>`.
    Facets {
        /// `tool_name`, `code_lang`, `project`, `model`, `git_branch`, `role`, `kind`,
        /// `agent_type`, `entrypoint`, or a JSON path such as `tool_input.file_path` or
        /// `bash_cmd.program`.
        field: String,
        /// Restrict the counted set to documents matching this query.
        #[arg(long, value_name = "QUERY")]
        query: Option<String>,
        #[arg(long, default_value_t = 20, value_name = "N")]
        top: usize,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_refresh: bool,
        #[command(flatten, next_help_heading = "Filters")]
        filters: Filters,
    },
    /// Print a session, or the turns around one hit.
    Show {
        session_id: String,
        /// Subagent id, for a sidechain transcript.
        #[arg(long, value_name = "AGENT_ID")]
        agent: Option<String>,
        /// Where to centre the window: a `seq` number, a record uuid, a `tool_use_id`, a
        /// `doc_id`, or a `SESSION[:AGENT]:SEQ` reference — the same grammar
        /// `search --similar-to` takes, by unambiguous prefix.
        #[arg(long, value_name = "REF|SEQ")]
        around: Option<String>,
        /// Snap the `--around` window to the enclosing turn instead of counting documents with
        /// `--before`/`--after`. Capped by `--limit`, and what the cap left out is reported.
        // A turn is defined relative to a document, so there is nothing to snap to without
        // `--around`; requiring it turns a silent no-op into a usage error.
        #[arg(long, requires = "around")]
        turn: bool,
        /// Render the `--turn` window as a skeleton: one line per document, call signatures
        /// with no output, except the first line of a failed one.
        #[arg(long, requires = "turn")]
        skeleton: bool,
        #[arg(long, default_value_t = 3, value_name = "N")]
        before: usize,
        #[arg(long, default_value_t = 3, value_name = "N")]
        after: usize,
        #[arg(long, default_value_t = 200, value_name = "N")]
        limit: usize,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_refresh: bool,
    },
    /// List indexed sessions, most recent first.
    Sessions {
        #[arg(long, default_value_t = 50, value_name = "N")]
        limit: usize,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_refresh: bool,
        #[command(flatten, next_help_heading = "Filters")]
        filters: Filters,
    },
    /// Index statistics, read from the index directory without touching it.
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Serve the index over HTTP (and, with the `web-ui` feature, the browser UI).
    #[cfg(feature = "http-api")]
    Serve {
        /// Interface to bind. Anything but a loopback address publishes every transcript this
        /// index holds to the network, unauthenticated; the server says so loudly when asked.
        #[arg(long, default_value = "127.0.0.1", value_name = "ADDR")]
        host: String,
        #[arg(long, default_value_t = 7777, value_name = "PORT")]
        port: u16,
        /// Allow browser requests from this origin (`*` for any). Repeatable. Off by default:
        /// the bundled UI is same-origin, and only a separately hosted frontend needs this.
        #[arg(long = "cors", value_name = "ORIGIN")]
        cors: Vec<String>,
        /// Re-index every N seconds while the server runs. 0 (the default) never does.
        #[arg(long = "refresh-secs", default_value_t = 0, value_name = "N")]
        refresh_secs: u64,
        /// Skip the incremental index refresh that normally runs before the port opens.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Serve the index to an agent over MCP on stdio.
    ///
    /// The transport owns stdout: one JSON-RPC message per line and nothing else. Diagnostics
    /// go to stderr as they always do, and no renderer in this crate is called on this path.
    ///
    /// The index this serves is the global `--index` / `$SESSION_SEARCH_INDEX`, resolved before
    /// dispatch like every other command — an MCP server started by an agent host inherits
    /// whatever environment the host gives it, so the server reports the directory it actually
    /// opened back in its `instructions`, where the model can see it.
    #[cfg(feature = "mcp")]
    Mcp {
        /// Re-index at most every N seconds while the server runs. 0 never re-indexes after
        /// startup. Unlike the one-shot commands, a long-lived server cannot afford an index
        /// scan on every request.
        #[arg(long = "refresh-secs", default_value_t = crate::mcp::DEFAULT_REFRESH_SECS, value_name = "N")]
        refresh_secs: u64,
        /// Skip the startup refresh too, and serve the index exactly as it stands.
        #[arg(long)]
        no_refresh: bool,
    },
}

impl Command {
    fn wants_json(&self) -> bool {
        match self {
            Command::Index { .. } => false,
            Command::Search { json, .. }
            | Command::Facets { json, .. }
            | Command::Show { json, .. }
            | Command::Sessions { json, .. }
            | Command::Stats { json } => *json,
            #[cfg(feature = "http-api")]
            Command::Serve { .. } => false,
            // Not "this command emits JSON" so much as "this command emits nothing a human
            // reads": `OutputOpts::color` is derived from it, and the one thing that must never
            // happen on this path is an ANSI escape near stdout.
            #[cfg(feature = "mcp")]
            Command::Mcp { .. } => true,
        }
    }

    /// True when the command writes to stdout itself and must not be handed a borrowed one.
    ///
    /// Only the MCP server does. Its stdio transport *is* stdout — JSON-RPC framing written from
    /// the transport's own threads — and [`run`]'s buffered writer holds `stdout().lock()` for
    /// the whole of [`dispatch`]. A `std::io::Stdout` lock is reentrant within a thread and
    /// blocking across threads, so the transport's first write parks forever: no response, no
    /// error, no log line, just a server that never answers. That is a five-minute bug to hit
    /// and an hour to find, so the two paths are separated here rather than defended by a
    /// comment.
    fn owns_stdout(&self) -> bool {
        #[cfg(feature = "mcp")]
        {
            matches!(self, Command::Mcp { .. })
        }
        #[cfg(not(feature = "mcp"))]
        {
            false
        }
    }
}

/// `$SESSION_SEARCH_INDEX` / `--index`, else `$XDG_DATA_HOME/session-search`, else
/// `~/.local/share/session-search`.
pub fn default_index_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("session-search"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set; pass --index explicitly"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("session-search"))
}

pub fn run(cli: Cli) -> Result<()> {
    let index_dir = match cli.index.clone() {
        Some(dir) => dir,
        None => default_index_dir()?,
    };
    let opts = OutputOpts {
        json: cli.command.wants_json(),
        color: color_enabled(cli.no_color, cli.command.wants_json()),
        context: 0,
        width: terminal_width(),
        // Filled in by the `Search` arm once `--similar-to` has resolved to a document.
        similar_to: None,
    };

    // A command that owns stdout gets it unborrowed; see `Command::owns_stdout`. `sink` rather
    // than `stdout` because such a command writes nothing through this channel by definition,
    // and handing it a second handle to the stream it is framing would invite exactly the
    // interleaving the separation exists to prevent.
    if cli.command.owns_stdout() {
        return dispatch(cli.command, &index_dir, opts, &mut std::io::sink());
    }

    // Buffered: the human renderings are many small writes, and a `| head` should not cost a
    // syscall per line.
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let result = dispatch(cli.command, &index_dir, opts, &mut out);
    // A flush error matters as much as a write error — report whichever came first.
    let flushed = out.flush().context("writing to stdout");
    result.and(flushed)
}

fn dispatch(
    command: Command,
    index_dir: &Path,
    mut opts: OutputOpts,
    out: &mut impl Write,
) -> Result<()> {
    match command {
        Command::Index {
            full,
            roots,
            jobs,
            no_thinking,
            no_spilled_results,
            max_text_bytes,
        } => {
            // An explicit --root wins; otherwise stay with whatever corpus this index already
            // holds, and only fall back to the default root for a brand-new index.
            let roots = if roots.is_empty() {
                let bound = index::meta(index_dir).roots;
                if bound.is_empty() {
                    resolve_roots(Vec::new())?
                } else {
                    bound
                }
            } else {
                resolve_roots(roots)?
            };
            let stats = index::run(
                index_dir,
                &roots,
                &IndexOptions {
                    full,
                    jobs,
                    include_thinking: !no_thinking,
                    load_spilled_results: !no_spilled_results,
                    // A cap of 0 would index no body at all, which is never what anyone means
                    // by passing the flag; the default stands in for it.
                    max_text_bytes: max_text_bytes
                        .filter(|n| *n > 0)
                        .unwrap_or(IndexOptions::default().max_text_bytes),
                    ..IndexOptions::default()
                },
            )?;
            format::stats(out, &stats, &opts)
        }

        Command::Search {
            query,
            filters,
            facets,
            context: window,
            limit,
            offset,
            json: _,
            no_refresh,
            include_thinking,
            sort,
            similar_to,
            similar_in,
            include_source,
            group_by_turn,
        } => {
            refresh(index_dir, no_refresh, include_thinking);
            let (index, fields) = index::open_or_create(index_dir)?;
            // Resolved here rather than inside `search()`: expanding a reference is an index
            // lookup with its own ambiguity error, and "your reference matched three documents"
            // must not reach the caller as "your search found nothing".
            let similar = match similar_to.as_deref() {
                Some(spec) => Some(search::resolve_similar(
                    &index,
                    &fields,
                    spec,
                    &similar_fields(&similar_in, include_thinking),
                    include_source,
                )?),
                None => None,
            };
            opts.similar_to = similar.as_ref().map(search::SimilarSource::label);
            let request = SearchRequest {
                query: non_empty(query),
                filters,
                limit,
                offset,
                facets,
                facet_top: SEARCH_FACET_TOP,
                snippet_chars: SNIPPET_CHARS,
                include_thinking,
                sort,
                similar_to: similar,
                group_by_turn,
            };
            let response = search::search(&index, &fields, &request).map_err(cli_dialect)?;
            opts.context = match window {
                ContextWindow::Docs(n) => n,
                ContextWindow::Turn | ContextWindow::Skeleton => 0,
            };
            let around = match window {
                ContextWindow::Docs(0) => Vec::new(),
                _ => context_windows(&index, &fields, &response, window),
            };
            format::search_results_ctx(out, &response, &around, &opts)
        }

        Command::Facets {
            field,
            query,
            filters,
            top,
            json: _,
            no_refresh,
        } => {
            refresh(index_dir, no_refresh, false);
            let (index, fields) = index::open_or_create(index_dir)?;
            let request = SearchRequest {
                query: non_empty(query),
                filters,
                limit: top.max(1),
                facet_top: top,
                snippet_chars: SNIPPET_CHARS,
                ..SearchRequest::default()
            };
            let result = search::facets(&index, &fields, &field, &request).map_err(cli_dialect)?;
            format::facet_list(out, &result, &opts)
        }

        Command::Show {
            session_id,
            agent,
            around,
            turn,
            skeleton,
            before,
            after,
            limit,
            json: _,
            no_refresh,
        } => {
            refresh(index_dir, no_refresh, false);
            let (index, fields) = index::open_or_create(index_dir)?;
            // Ids are long; accept the unambiguous prefix a user actually types or pastes.
            let known = index::load_sessions(index_dir).unwrap_or_default();
            let session_id = resolve_id(
                "session",
                &session_id,
                known.values().map(|i| i.session_id.as_str()),
            )?;
            let agent = match agent.as_deref() {
                Some(a) => Some(resolve_id(
                    "agent",
                    a,
                    known
                        .values()
                        .filter(|i| i.session_id == session_id)
                        .filter_map(|i| i.agent_id.as_deref()),
                )?),
                None => None,
            };
            let agent = agent.as_deref();
            // `seq` numbers documents within one *file*, and a session id can appear in two
            // (§9). When it does, the window has to be scoped to one of them or it interleaves
            // both and silently drops the neighbours it was asked for.
            let source = source_path_for(&known, &session_id, agent);
            let source = source.as_deref();
            let (docs, span) = match around {
                Some(spec) => {
                    let anchor = resolve_seq(&index, &fields, &session_id, agent, source, &spec)?;
                    // A uuid names one record in one file. When the session id alone did not
                    // narrow to a file, the file the uuid was found in is the only one the
                    // window can honestly be scoped to.
                    let source = anchor.source_path.as_deref().or(source);
                    let seq = anchor.seq;
                    if turn {
                        let window =
                            turn_at(&index, &fields, &session_id, agent, source, seq, limit)?;
                        let span = format::TurnSpan {
                            turn_seq: window.turn_seq,
                            total: window.total,
                        };
                        (window.docs, Some(span))
                    } else {
                        let docs = context::around(
                            &index,
                            &fields,
                            &session_id,
                            agent,
                            source,
                            seq,
                            before,
                            after,
                        )?;
                        (docs, None)
                    }
                }
                None => (
                    context::session(&index, &fields, &session_id, agent, source, limit)?,
                    None,
                ),
            };
            if docs.is_empty() {
                tracing::warn!(
                    session = %session_id,
                    "no indexed documents; `session-search sessions` lists what is indexed"
                );
            }
            match span {
                Some(span) if skeleton => format::turn_skeleton_view(out, &docs, span, &opts),
                Some(span) => format::turn_view(out, &docs, span, &opts),
                None => format::session_view(out, &docs, &opts),
            }
        }

        Command::Sessions {
            filters,
            limit,
            json: _,
            no_refresh,
        } => {
            refresh(index_dir, no_refresh, false);
            warn_unused_session_filters(&filters);
            let matcher = session_matcher(&filters)?;
            let mut sessions: Vec<SessionInfo> = index::load_sessions(index_dir)?
                .into_values()
                .filter(|info| matcher.matches(info))
                .collect();
            // Most recent first; the id breaks ties so the listing is deterministic.
            sessions.sort_by(|a, b| {
                b.last_ts_ms
                    .cmp(&a.last_ts_ms)
                    .then_with(|| a.session_id.cmp(&b.session_id))
                    .then_with(|| a.agent_id.cmp(&b.agent_id))
            });
            sessions.truncate(limit);
            format::session_list(out, &sessions, &opts)
        }

        Command::Stats { json: _ } => {
            let roots = index::meta(index_dir).roots;
            let stats = index_stats(index_dir)?;
            format::stats_scoped(out, &stats, &roots, &opts)
        }

        #[cfg(feature = "mcp")]
        Command::Mcp {
            refresh_secs,
            no_refresh,
        } => {
            // `out` is deliberately untouched: the stdio transport is the only thing allowed to
            // write to stdout, and `run` flushes an empty buffer afterwards.
            crate::mcp::serve(
                index_dir,
                crate::mcp::ServeOptions {
                    refresh_secs,
                    no_refresh,
                },
            )
        }

        #[cfg(feature = "http-api")]
        Command::Serve {
            host,
            port,
            cors,
            refresh_secs,
            no_refresh,
        } => {
            refresh(index_dir, no_refresh, false);
            crate::api::serve(
                index_dir,
                crate::api::ServeOptions {
                    host,
                    port,
                    cors,
                    refresh_secs,
                },
                out,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// shared behaviour
// ---------------------------------------------------------------------------

/// The read commands index first, so a search is never silently answered from a stale index.
/// A refresh failure is not fatal: an unreadable transcript root should not stop you searching
/// what was indexed yesterday.
pub(crate) fn refresh(index_dir: &Path, no_refresh: bool, query_wants_thinking: bool) {
    let meta = index::meta(index_dir);

    // Searching thinking that was never indexed matches nothing and looks like an empty corpus.
    if query_wants_thinking && !meta.roots.is_empty() && !meta.thinking_indexed {
        tracing::warn!(
            "this index was built with --no-thinking, so --include-thinking cannot match; \
             rebuild with `index --full` to search thinking"
        );
    }

    if no_refresh {
        tracing::debug!("--no-refresh: querying the index as it stands");
        return;
    }

    // Refresh the corpus this index actually holds. Resolving the *default* root here is what
    // silently merged a second corpus into an index built over a snapshot.
    let roots = if meta.roots.is_empty() {
        match resolve_roots(Vec::new()) {
            Ok(roots) => roots,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "cannot locate transcripts; skipping refresh");
                return;
            }
        }
    } else {
        meta.roots.clone()
    };
    match index::run(
        index_dir,
        &roots,
        &IndexOptions {
            // Keep the index's own thinking setting; a query-time flag must never silently
            // change what is stored.
            include_thinking: if meta.roots.is_empty() {
                IndexOptions::default().include_thinking
            } else {
                meta.thinking_indexed
            },
            ..IndexOptions::default()
        },
    ) {
        Ok(stats) => tracing::info!(
            files = stats.files_updated,
            docs = stats.docs_added,
            ms = stats.elapsed_ms,
            "refreshed"
        ),
        Err(err) => tracing::warn!(
            error = %format!("{err:#}"),
            "refresh failed; querying the index as it stands"
        ),
    }
}

fn resolve_roots(explicit: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if explicit.is_empty() {
        Ok(vec![discovery::default_root()?])
    } else {
        Ok(explicit)
    }
}

/// Which bodies `--similar-in` selected, with the defaults and the `thinking` gate applied.
///
/// An empty selection means `text` — the field that answers "about the same thing" rather than
/// "built out of the same files", and the only one every document has some of. `thinking` is
/// gated on `--include-thinking` for the same reason the query side is: the model's private
/// reasoning is opt-in everywhere, and a seed that silently included it would let a search
/// rank documents by something the caller never asked to look at. Asking for it without the
/// flag is a warning rather than an error, so a saved command line keeps working when the index
/// was built with `--no-thinking`.
fn similar_fields(selected: &[SimilarField], include_thinking: bool) -> Vec<SimilarField> {
    if selected.is_empty() {
        return vec![SimilarField::Text];
    }
    let mut fields = Vec::with_capacity(selected.len());
    for field in selected {
        if *field == SimilarField::Thinking && !include_thinking {
            tracing::warn!("--similar-in thinking needs --include-thinking; seeding without it");
            continue;
        }
        fields.push(*field);
    }
    if fields.is_empty() {
        vec![SimilarField::Text]
    } else {
        fields
    }
}

/// One context window per hit. A failure on a single hit degrades that hit to "no context"
/// rather than losing the whole result set.
fn context_windows(
    index: &tantivy::Index,
    fields: &crate::schema::Fields,
    response: &search::SearchResponse,
    window: ContextWindow,
) -> Vec<format::HitContext> {
    response
        .hits
        .iter()
        .map(|hit| {
            let fetched = match window {
                ContextWindow::Docs(n) => context::around(
                    index,
                    fields,
                    &hit.doc.session_id,
                    hit.doc.agent_id.as_deref(),
                    Some(hit.doc.source_path.as_str()),
                    hit.doc.seq,
                    n,
                    n,
                )
                .map(format::HitContext::around),
                // `turn_seq` rides on the hit itself, so snapping to the turn costs one query,
                // not a lookup of the doc first. A skeleton walks the very same window: it is a
                // rendering of the turn, so fetching anything else would let the two disagree.
                ContextWindow::Turn | ContextWindow::Skeleton => context::turn_window(
                    index,
                    fields,
                    &hit.doc.source_path,
                    hit.doc.turn_seq,
                    TURN_WINDOW_LIMIT,
                )
                .map(|w| match window {
                    ContextWindow::Skeleton => {
                        format::HitContext::skeleton(w.docs, w.turn_seq, w.total)
                    }
                    _ => format::HitContext::turn(w.docs, w.turn_seq, w.total),
                }),
            };
            fetched.unwrap_or_else(|err| {
                tracing::warn!(doc = %hit.doc.doc_id, error = %format!("{err:#}"), "context lookup failed");
                format::HitContext::default()
            })
        })
        .collect()
}

/// `--around` takes either a `seq` (as printed next to every hit) or a record uuid. A uuid is
/// resolved by scanning the session, which is one targeted term query, not an index scan.
/// Expand an unambiguous id prefix to the full id. An unknown id is passed through
/// unchanged so the caller can report "nothing indexed" rather than "no such session".
pub(crate) fn resolve_id<'a>(
    what: &str,
    given: &str,
    known: impl Iterator<Item = &'a str>,
) -> anyhow::Result<String> {
    let mut matches: Vec<&str> = known.filter(|id| id.starts_with(given)).collect();
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [] => Ok(given.to_string()),
        [only] => Ok((*only).to_string()),
        many if many.contains(&given) => Ok(given.to_string()),
        many => bail!(
            "ambiguous {what} id {given:?} matches {}: {}",
            many.len(),
            many.join(", ")
        ),
    }
}

/// The one transcript file a `(session_id, agent_id)` pair lives in, when there is exactly one.
/// Two files sharing a session id (`resetSessionFile()`, `relocated` — §9) is ambiguous, and
/// `None` there means "do not constrain", which is the old, id-only behaviour.
pub(crate) fn source_path_for(
    known: &std::collections::BTreeMap<String, SessionInfo>,
    session_id: &str,
    agent_id: Option<&str>,
) -> Option<String> {
    let mut matching = known
        .values()
        .filter(|info| info.session_id == session_id && info.agent_id.as_deref() == agent_id);
    let first = matching.next()?;
    matching.next().is_none().then(|| first.source_path.clone())
}

/// What `--around` resolved to: a `seq`, and the file it belongs to when the spec said.
///
/// A bare number carries no file: `seq` is a per-file ordinal, and when a session id names two
/// files (§9) the caller has nothing to pick one by. A uuid does — it was found by scanning
/// the session, in exactly one document of exactly one file — and throwing that away would
/// let the window that follows re-fetch "some document at that seq" from the *other* file.
#[derive(Debug)]
struct Anchor {
    seq: u64,
    source_path: Option<String>,
}

/// `--around` takes a bare `seq` or any of the document references `--similar-to` takes, and
/// `search::resolve_doc_in` is what defines "any of them" for both commands.
///
/// What routing this through the shared resolver buys: `abc123` means the same document to
/// `show` and to `--similar-to`, `--around` gains the prefix and coordinate spellings it did
/// not have (it previously required an id in full, and answered anything else with "no document
/// with uuid ... in session ..."), and a linear scan of up to fifty thousand documents becomes
/// a term query.
///
/// The session the caller named is an **input** to the resolution, not a check after it. That
/// distinction is the whole of `DocScope`: a reference that is unique inside the named
/// transcript has to resolve even when the index holds another document with the same uuid
/// somewhere else, which §9's `resetSessionFile()`/`relocated` case guarantees it sometimes
/// does. Only when the scope finds nothing is the reference resolved index-wide again, and then
/// solely to say where it actually lives — because "no such uuid" would be a lie about a
/// document that plainly exists, and windowing it would print a transcript the caller never
/// asked for under the heading of the one they did.
fn resolve_seq(
    index: &tantivy::Index,
    fields: &crate::schema::Fields,
    session_id: &str,
    agent_id: Option<&str>,
    source_path: Option<&str>,
    spec: &str,
) -> Result<Anchor> {
    if let Ok(seq) = spec.trim().parse::<u64>() {
        return Ok(Anchor {
            seq,
            source_path: None,
        });
    }
    let scope = search::DocScope::Transcript {
        session_id,
        agent_id,
        source_path,
    };
    if let Some(doc) = search::resolve_doc_in(index, fields, spec, scope)? {
        return Ok(Anchor {
            seq: doc.seq,
            source_path: Some(doc.source_path),
        });
    }

    // Nothing inside the transcript the caller named. Resolve index-wide, purely to report
    // where the reference does live; an ambiguity out here is reported as one.
    let doc = search::resolve_doc(index, fields, spec)?;
    if doc.session_id != session_id || doc.agent_id.as_deref() != agent_id {
        bail!(
            "{spec:?} is {} of session {}{}, not of {session_id}{}",
            doc.doc_id,
            doc.session_id,
            agent_label(doc.agent_id.as_deref()),
            agent_label(agent_id)
        );
    }
    bail!(
        "{spec:?} is in {}, but this window is scoped to {}",
        doc.source_path,
        source_path.unwrap_or(&doc.source_path)
    );
}

/// ` (agent <id>)`, or nothing for a main transcript. Only ever used inside an error message,
/// where "session s1" and "session s1, agent a3" have to be distinguishable.
fn agent_label(agent_id: Option<&str>) -> String {
    match agent_id {
        Some(agent) => format!(" (agent {agent})"),
        None => String::new(),
    }
}

/// The turn the document at `seq` belongs to, for `show --around ... --turn`.
///
/// Which turn that is, is recorded on the document and nowhere else, so the anchor is fetched
/// first — a zero-width `around` window, which is the same term query the caller would make
/// anyway. Its own `source_path` scopes the turn, because `turn_seq` is a per-file ordinal and
/// the caller's `source` is `None` whenever a session id names two files (§9).
fn turn_at(
    index: &tantivy::Index,
    fields: &crate::schema::Fields,
    session_id: &str,
    agent_id: Option<&str>,
    source_path: Option<&str>,
    seq: u64,
    limit: usize,
) -> Result<context::TurnWindow> {
    let anchor = context::around(index, fields, session_id, agent_id, source_path, seq, 0, 0)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no document at seq {seq} in session {session_id}"))?;
    context::turn_window(
        index,
        fields,
        &anchor.source_path,
        anchor.turn_seq,
        limit.max(1),
    )
}

/// Index statistics without opening (or creating) the Tantivy index: everything shown is
/// already recorded in `state.json` and `sessions.json`, whose shapes are pinned in
/// `docs/DESIGN.md`. `docs_added` is the document count those watermarks account for.
pub(crate) fn index_stats(index_dir: &Path) -> Result<IndexStats> {
    let started = Instant::now();
    let mut stats = IndexStats::default();

    let state_path = index_dir.join("state.json");
    match std::fs::read(&state_path) {
        Ok(bytes) => {
            let state: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", state_path.display()))?;
            if let Some(files) = state.get("files").and_then(Value::as_object) {
                stats.files_scanned = files.len();
                stats.docs_added = files
                    .values()
                    .filter_map(|f| f.get("docs").and_then(Value::as_u64))
                    .sum();
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(dir = %index_dir.display(), "nothing indexed yet; run `session-search index`");
        }
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("reading {}", state_path.display()));
        }
    }

    stats.sessions = index::load_sessions(index_dir)?.len();
    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

// ---------------------------------------------------------------------------
// session filtering
// ---------------------------------------------------------------------------

/// [`SessionMatcher::new`] with an unreadable date named the way the reader typed it.
///
/// The shared matcher reports the plain field (`since`), because the same matcher serves an HTTP
/// surface where `--since` is a flag nobody can type. Here the flag *is* what was typed, so it
/// goes back on at the boundary — this is the whole of the CLI's dialect.
fn session_matcher(f: &Filters) -> Result<SessionMatcher> {
    SessionMatcher::new(f).map_err(|err| cli_flag(err.field, &err.source))
}

/// A [`sessions::FilterError`] re-spelled as the flag the user actually typed.
///
/// The shared core names the plain field (`tool_input`), because the same builder answers an HTTP
/// API and an MCP server, and neither of those callers can type a flag — putting `--tool-input`
/// in a JSON-RPC error tells a model to send something it has no way to send. Here the flag *is*
/// what was typed, so it goes back on, at the boundary that owns the dialect.
fn cli_flag(field: &str, source: &anyhow::Error) -> anyhow::Error {
    anyhow!("parsing --{}: {:#}", field.replace('_', "-"), source)
}

/// The same re-spelling, for an error that arrived as an opaque `anyhow` from the query builder.
///
/// `search::build_query` reports an unreadable `--tool-input` / `--tool-output` as a
/// [`sessions::FilterError`] for the reason above. Everything else it can fail with is a genuine
/// internal fault and passes through untouched.
fn cli_dialect(err: anyhow::Error) -> anyhow::Error {
    match err.downcast::<sessions::FilterError>() {
        Ok(filter) => cli_flag(filter.field, &filter.source),
        Err(err) => err,
    }
}

/// The filters `sessions` was handed that a session listing cannot answer, on stderr.
///
/// The list is [`sessions::unanswerable_filters`], shared with `GET /api/sessions` so the two
/// front doors cannot disagree about which questions `sessions.json` can be asked. Only the
/// rendering is the CLI's: flag spelling, and a warning rather than a field in a response body,
/// because the CLI's answer is a table a human is looking at and stderr is where it says what it
/// did not do.
/// Returns the flags it warned about, so the shared list and this dialect are testable together.
fn warn_unused_session_filters(f: &Filters) -> Vec<String> {
    let flags: Vec<String> = sessions::unanswerable_filters(f)
        .iter()
        .map(|name| format!("--{}", name.replace('_', "-")))
        .collect();
    if !flags.is_empty() {
        tracing::warn!(
            filters = %flags.join(", "),
            "not applicable to `sessions` (session metadata has no per-message fields); ignored"
        );
    }
    flags
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

// ---------------------------------------------------------------------------
// terminal
// ---------------------------------------------------------------------------

/// Colour is off unless every gate agrees: no `--no-color`, no non-empty `$NO_COLOR`
/// (the NO_COLOR convention), not `--json`, and stdout is a terminal. `$CLICOLOR_FORCE`
/// overrides the tty check for people piping into `less -R`.
fn color_enabled(no_color_flag: bool, json: bool) -> bool {
    if no_color_flag || json {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    std::io::stdout().is_terminal()
}

/// `$COLUMNS` when it is exported and sane, else a readable default. Nothing here justifies a
/// dependency on an ioctl wrapper.
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w >= 40)
        .unwrap_or(100)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(argv: &[&str]) -> Cli {
        Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} should parse: {e}"))
    }

    /// clap's own consistency check: duplicate long/short flags, broken groups, bad defaults.
    #[test]
    fn command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn search_accepts_the_full_filter_set() {
        let cli = parse(&[
            "session-search",
            "-vv",
            "--index",
            "/tmp/idx",
            "search",
            "cargo build",
            "-p",
            "~/code",
            "-t",
            "Bash",
            "-t",
            "Edit",
            "--tool-input",
            "command=cargo",
            "--tool-input",
            "file_path=src/index.rs",
            "--lang",
            "rust",
            "--lang",
            "bash",
            "--branch",
            "main",
            "--model",
            "claude-opus-5",
            "--role",
            "assistant",
            "--kind",
            "tool_call",
            "--session",
            "b20208d8",
            "--agent-type",
            "Explore",
            "--since",
            "7d",
            "--until",
            "2026-09-09",
            "--errors-only",
            "--no-sidechains",
            "--facets",
            "tool_name,code_lang",
            "--context",
            "2",
            "--limit",
            "5",
            "--offset",
            "10",
            "--json",
            "--no-refresh",
            "--include-thinking",
            "--sort",
            "newest",
        ]);
        assert_eq!(cli.verbose, 2);
        assert_eq!(cli.index.as_deref(), Some(Path::new("/tmp/idx")));
        assert!(cli.command.wants_json());

        let Command::Search {
            query,
            filters,
            facets,
            context,
            limit,
            offset,
            json,
            no_refresh,
            include_thinking,
            sort,
            ..
        } = cli.command
        else {
            panic!("expected the search subcommand");
        };
        assert_eq!(sort, SortBy::Newest);
        assert_eq!(query.as_deref(), Some("cargo build"));
        assert_eq!(filters.project.as_deref(), Some("~/code"));
        assert_eq!(filters.tool, ["Bash", "Edit"]);
        assert_eq!(
            filters.tool_input,
            ["command=cargo", "file_path=src/index.rs"]
        );
        assert_eq!(filters.lang, ["rust", "bash"]);
        assert_eq!(filters.branch.as_deref(), Some("main"));
        assert_eq!(filters.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(filters.role.as_deref(), Some("assistant"));
        assert_eq!(filters.kind.as_deref(), Some("tool_call"));
        assert_eq!(filters.session.as_deref(), Some("b20208d8"));
        assert_eq!(filters.agent_type.as_deref(), Some("Explore"));
        assert_eq!(filters.since.as_deref(), Some("7d"));
        assert_eq!(filters.until.as_deref(), Some("2026-09-09"));
        assert!(filters.errors_only);
        assert!(filters.no_sidechains);
        assert!(!filters.sidechains_only);
        // `--facets a,b` is one flag, two fields.
        assert_eq!(facets, ["tool_name", "code_lang"]);
        assert_eq!((context, limit, offset), (ContextWindow::Docs(2), 5, 10));
        assert!(json && no_refresh && include_thinking);
    }

    #[test]
    fn search_defaults_are_the_documented_ones() {
        let Command::Search {
            query,
            limit,
            offset,
            context,
            facets,
            json,
            no_refresh,
            include_thinking,
            sort,
            filters,
            similar_to,
            similar_in,
            include_source,
            group_by_turn,
        } = parse(&["session-search", "search", "tantivy"]).command
        else {
            panic!("expected search");
        };
        assert_eq!(query.as_deref(), Some("tantivy"));
        assert_eq!((limit, offset, context), (20, 0, ContextWindow::Docs(0)));
        assert!(facets.is_empty());
        assert!(!json && !no_refresh && !include_thinking);
        // Grouping changes what a hit is, so it is opt-in: a saved command line must not start
        // returning one hit where it used to return four.
        assert!(!group_by_turn);
        assert_eq!(sort, SortBy::Relevance);
        assert!(filters.project.is_none() && filters.tool.is_empty());
        // Similarity is entirely opt-in: no reference, no seed fields, source not re-included.
        assert!(similar_to.is_none() && similar_in.is_empty() && !include_source);
        assert_eq!(
            similar_fields(&similar_in, false),
            vec![SimilarField::Text],
            "an empty --similar-in means text"
        );
    }

    #[test]
    fn a_query_is_optional_so_filters_alone_work() {
        let Command::Search { query, filters, .. } =
            parse(&["session-search", "search", "--errors-only", "-t", "Bash"]).command
        else {
            panic!("expected search");
        };
        assert!(query.is_none());
        assert!(filters.errors_only);
    }

    #[test]
    fn the_other_subcommands_round_trip() {
        let Command::Index {
            full,
            roots,
            jobs,
            no_thinking,
            no_spilled_results,
            max_text_bytes,
        } = parse(&[
            "session-search",
            "index",
            "--full",
            "--root",
            "/a/projects",
            "--root",
            "/b/projects",
            "--jobs",
            "4",
            "--no-thinking",
            "--max-text-bytes",
            "4096",
        ])
        .command
        else {
            panic!("expected index");
        };
        assert!(full && no_thinking);
        // Spilled tool results are followed unless explicitly turned off.
        assert!(!no_spilled_results);
        assert_eq!(
            roots,
            [PathBuf::from("/a/projects"), PathBuf::from("/b/projects")]
        );
        assert_eq!(jobs, Some(4));
        assert_eq!(max_text_bytes, Some(4096));

        let Command::Facets {
            field,
            query,
            top,
            json,
            no_refresh,
            filters,
        } = parse(&[
            "session-search",
            "facets",
            "tool_input.file_path",
            "--query",
            "index",
            "--top",
            "5",
            "--json",
            "--no-refresh",
            "-p",
            "/home/user/session-search",
        ])
        .command
        else {
            panic!("expected facets");
        };
        assert_eq!(field, "tool_input.file_path");
        assert_eq!(query.as_deref(), Some("index"));
        assert_eq!(top, 5);
        assert!(json && no_refresh);
        assert_eq!(
            filters.project.as_deref(),
            Some("/home/user/session-search")
        );

        let Command::Show {
            session_id,
            agent,
            around,
            turn,
            skeleton: _,
            before,
            after,
            limit,
            json,
            no_refresh,
        } = parse(&[
            "session-search",
            "show",
            "b20208d8-fbdb-5918-ba69-d203de6ed6dc",
            "--agent",
            "a10845c5ff9c7d4ec",
            "--around",
            "41",
            "--before",
            "2",
            "--after",
            "4",
        ])
        .command
        else {
            panic!("expected show");
        };
        assert_eq!(session_id, "b20208d8-fbdb-5918-ba69-d203de6ed6dc");
        assert_eq!(agent.as_deref(), Some("a10845c5ff9c7d4ec"));
        assert_eq!(around.as_deref(), Some("41"));
        assert_eq!((before, after, limit), (2, 4, 200));
        assert!(!turn && !json && !no_refresh);

        let Command::Sessions { limit, filters, .. } = parse(&[
            "session-search",
            "sessions",
            "--sidechains-only",
            "--limit",
            "3",
        ])
        .command
        else {
            panic!("expected sessions");
        };
        assert_eq!(limit, 3);
        assert!(filters.sidechains_only);

        assert!(
            parse(&["session-search", "stats", "--json"])
                .command
                .wants_json()
        );
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand() {
        let cli = parse(&["session-search", "search", "x", "--no-color", "-v"]);
        assert!(cli.no_color);
        assert_eq!(cli.verbose, 1);
    }

    #[test]
    fn contradictory_sidechain_filters_are_rejected() {
        let err = Cli::try_parse_from([
            "session-search",
            "search",
            "x",
            "--no-sidechains",
            "--sidechains-only",
        ])
        .expect_err("the two flags cannot both hold");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn an_unknown_subcommand_is_an_error_not_a_panic() {
        assert!(Cli::try_parse_from(["session-search", "grep", "x"]).is_err());
    }

    // -- helpers ------------------------------------------------------------

    #[test]
    fn colour_needs_every_gate_to_agree() {
        // `--no-color` and `--json` both veto, whatever the terminal says.
        assert!(!color_enabled(true, false));
        assert!(!color_enabled(false, true));
    }

    fn session(id: &str, project: &str) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            project: Some(project.into()),
            git_branch: Some("main".into()),
            first_ts_ms: Some(1_788_980_839_000),
            last_ts_ms: Some(1_788_980_899_000),
            ..SessionInfo::default()
        }
    }

    fn matches(filters: &Filters, info: &SessionInfo) -> bool {
        match session_matcher(filters) {
            Ok(matcher) => matcher.matches(info),
            Err(err) => panic!("filters should parse: {err:#}"),
        }
    }

    #[test]
    fn session_filters_narrow_the_listing() {
        let info = session("b20208d8-fbdb", "/home/user/session-search");
        assert!(matches(&Filters::default(), &info));

        // `project` is a prefix, so a parent directory catches its children.
        assert!(matches(
            &Filters {
                project: Some("/home/user".into()),
                ..Filters::default()
            },
            &info
        ));
        assert!(!matches(
            &Filters {
                project: Some("/etc".into()),
                ..Filters::default()
            },
            &info
        ));
        // A session id prefix is enough to name a session.
        assert!(matches(
            &Filters {
                session: Some("b20208".into()),
                ..Filters::default()
            },
            &info
        ));
        assert!(!matches(
            &Filters {
                session: Some("deadbeef".into()),
                ..Filters::default()
            },
            &info
        ));
        assert!(!matches(
            &Filters {
                branch: Some("other".into()),
                ..Filters::default()
            },
            &info
        ));
        // The window is an overlap test, not a containment test.
        assert!(matches(
            &Filters {
                since: Some("2026-09-09".into()),
                until: Some("2026-09-09".into()),
                ..Filters::default()
            },
            &info
        ));
        assert!(!matches(
            &Filters {
                since: Some("2026-09-10".into()),
                ..Filters::default()
            },
            &info
        ));
        assert!(!matches(
            &Filters {
                until: Some("2026-09-08".into()),
                ..Filters::default()
            },
            &info
        ));
    }

    /// The session listing filters `sessions.json`, not the index, so it has its own copy of
    /// the project test — and it has to agree with the index-side one: a prefix ends on a path
    /// boundary, so `-p /home/user/alpha` is not `/home/user/alpha-beta`.
    #[test]
    fn the_session_project_filter_stops_at_a_path_boundary() {
        let matching = |project: &str, prefix: &str| {
            matches(
                &Filters {
                    project: Some(prefix.into()),
                    ..Filters::default()
                },
                &session("b20208d8", project),
            )
        };
        assert!(matching("/home/user/alpha", "/home/user/alpha"));
        assert!(matching("/home/user/alpha/sub", "/home/user/alpha"));
        assert!(matching("/home/user/alpha/sub", "/home/user/alpha/"));
        assert!(!matching("/home/user/alpha-beta", "/home/user/alpha"));
        assert!(!matching("/home/user/beta", "/home/user/bet"));
    }

    /// `show` scopes its window to one transcript when the id names exactly one, and declines
    /// to guess when a session id appears in two files (§9).
    #[test]
    fn the_source_path_for_a_session_is_only_used_when_it_is_unambiguous() {
        let entry = |path: &str, agent: Option<&str>| SessionInfo {
            session_id: "s1".into(),
            agent_id: agent.map(str::to_string),
            source_path: path.into(),
            ..SessionInfo::default()
        };
        let mut known = std::collections::BTreeMap::new();
        known.insert("/a/s1.jsonl".to_string(), entry("/a/s1.jsonl", None));
        known.insert(
            "/a/s1/subagents/agent-x.jsonl".to_string(),
            entry("/a/s1/subagents/agent-x.jsonl", Some("x")),
        );
        assert_eq!(
            source_path_for(&known, "s1", None).as_deref(),
            Some("/a/s1.jsonl")
        );
        assert_eq!(
            source_path_for(&known, "s1", Some("x")).as_deref(),
            Some("/a/s1/subagents/agent-x.jsonl")
        );
        assert_eq!(source_path_for(&known, "nope", None), None);

        // The same session id in two files: no single answer, so do not constrain.
        known.insert("/b/s1.jsonl".to_string(), entry("/b/s1.jsonl", None));
        assert_eq!(source_path_for(&known, "s1", None), None);
    }

    #[test]
    fn sidechain_filters_split_main_transcripts_from_subagents() {
        let main = session("b20208d8", "/p");
        let agent = SessionInfo {
            agent_id: Some("a10845c5ff9c7d4ec".into()),
            agent_type: Some("Explore".into()),
            ..session("b20208d8", "/p")
        };
        let only = Filters {
            sidechains_only: true,
            ..Filters::default()
        };
        let none = Filters {
            no_sidechains: true,
            ..Filters::default()
        };
        assert!(!matches(&only, &main) && matches(&only, &agent));
        assert!(matches(&none, &main) && !matches(&none, &agent));
        assert!(matches(
            &Filters {
                agent_type: Some("Explore".into()),
                ..Filters::default()
            },
            &agent
        ));
        assert!(!matches(
            &Filters {
                agent_type: Some("Plan".into()),
                ..Filters::default()
            },
            &main
        ));
    }

    /// Same list as `GET /api/sessions` returns, spelled the way it was typed. The list itself
    /// is pinned in `sessions.rs`; what is the CLI's own is the `--kebab-case`.
    #[test]
    fn the_sessions_command_names_every_filter_it_cannot_apply_as_a_flag() {
        assert!(warn_unused_session_filters(&Filters::default()).is_empty());
        let flagged = warn_unused_session_filters(&Filters {
            tool_input: vec!["command=cargo".into()],
            lang: vec!["rust".into()],
            min_thinking: Some(500),
            program: vec!["cargo".into()],
            errors_only: true,
            // Answerable, so absent from the warning.
            project: Some("/home/user".into()),
            ..Filters::default()
        });
        assert_eq!(
            flagged,
            [
                "--tool-input",
                "--lang",
                "--min-thinking",
                "--program",
                "--errors-only"
            ]
        );
    }

    /// The shared matcher names the field; `session_matcher` is where the CLI puts its own flag
    /// spelling back on, so that is what this pins.
    #[test]
    fn a_bad_date_fails_the_command_rather_than_matching_nothing() {
        let err = match session_matcher(&Filters {
            since: Some("yesterday-ish".into()),
            ..Filters::default()
        }) {
            Ok(_) => panic!("an unreadable date must fail the command"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("--since"), "{err:#}");
    }

    /// The shared query builder names the plain field, because an HTTP or MCP caller cannot type
    /// a flag. Without this, that correctness for the other two front ends arrives here as a
    /// message telling someone at a shell prompt to fix a `tool_input` they never typed.
    #[test]
    fn an_unreadable_tool_filter_is_reported_as_the_flag_that_was_typed() {
        let (index, fields) = search::testkit::index_docs(&search::testkit::corpus());
        for (filters, flag) in [
            (
                Filters {
                    tool_input: vec!["command".into()],
                    ..Filters::default()
                },
                "--tool-input",
            ),
            (
                Filters {
                    tool_output: vec![String::new()],
                    ..Filters::default()
                },
                "--tool-output",
            ),
        ] {
            let request = search::SearchRequest {
                filters,
                ..Default::default()
            };
            let err = search::search(&index, &fields, &request)
                .map_err(cli_dialect)
                .expect_err("an unreadable filter must fail the command");
            let message = format!("{err:#}");
            assert!(message.contains(flag), "{message}");
        }
    }

    #[test]
    fn stats_read_the_state_file_without_creating_an_index() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing indexed yet: zeroes, not an error.
        let empty = index_stats(dir.path()).unwrap();
        assert_eq!(empty.files_scanned, 0);
        assert_eq!(empty.docs_added, 0);

        std::fs::write(
            dir.path().join("state.json"),
            r#"{"version":1,"files":{
                "/a.jsonl":{"size":10,"mtime_ms":1,"byte_offset":10,"docs":62},
                "/b.jsonl":{"size":20,"mtime_ms":2,"byte_offset":20,"docs":49}}}"#,
        )
        .unwrap();
        let sessions = serde_json::json!({
            "s1": SessionInfo {
                session_id: "s1".into(),
                source_path: "/a.jsonl".into(),
                ..SessionInfo::default()
            }
        });
        std::fs::write(
            dir.path().join("sessions.json"),
            serde_json::to_string(&sessions).unwrap(),
        )
        .unwrap();

        let stats = index_stats(dir.path()).unwrap();
        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.docs_added, 111);
        assert_eq!(stats.sessions, 1);
        assert!(
            !dir.path().join("tantivy").exists(),
            "`stats` must not create an index as a side effect"
        );

        std::fs::write(dir.path().join("state.json"), "{ not json").unwrap();
        assert!(index_stats(dir.path()).is_err(), "corruption is reported");
    }

    #[test]
    fn roots_fall_back_to_the_default_only_when_none_are_given() {
        let explicit = resolve_roots(vec![PathBuf::from("/a")]).unwrap();
        assert_eq!(explicit, [PathBuf::from("/a")]);
    }

    #[test]
    fn empty_query_strings_are_treated_as_no_query() {
        assert_eq!(non_empty(Some("  ".into())), None);
        assert_eq!(non_empty(Some(" x ".into())), Some(" x ".into()));
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn an_id_prefix_resolves_when_it_is_unambiguous() {
        let known = ["aaaa-1111", "bbbb-2222", "bbbb-3333"];
        let one = |given: &str| resolve_id("session", given, known.iter().copied());

        assert_eq!(one("aaaa").unwrap(), "aaaa-1111");
        assert_eq!(one("aaaa-1111").unwrap(), "aaaa-1111");
        // Nothing indexed under it: pass through, so the caller reports "no documents".
        assert_eq!(one("zzzz").unwrap(), "zzzz");
        // An exact hit wins even when it is also the prefix of another id.
        assert_eq!(
            resolve_id("session", "bbbb", ["bbbb", "bbbb-2222"].iter().copied()).unwrap(),
            "bbbb"
        );
        let err = one("bbbb").unwrap_err().to_string();
        assert!(err.contains("ambiguous session id"), "{err}");
        assert!(err.contains("bbbb-2222, bbbb-3333"), "{err}");
    }

    /// `--context` takes a document count, the word `turn` or the word `skeleton`; anything
    /// else is a usage error rather than a silently-zero window.
    #[test]
    fn the_context_flag_takes_a_count_or_a_turn() {
        let window = |value: &str| {
            let Command::Search { context, .. } =
                parse(&["session-search", "search", "q", "--context", value]).command
            else {
                panic!("expected search");
            };
            context
        };
        assert_eq!(window("0"), ContextWindow::Docs(0));
        assert_eq!(window("3"), ContextWindow::Docs(3));
        assert_eq!(window("turn"), ContextWindow::Turn);
        assert_eq!(window("TURN"), ContextWindow::Turn);
        assert_eq!(window("skeleton"), ContextWindow::Skeleton);
        assert_eq!(window("Skeleton"), ContextWindow::Skeleton);

        let err = Cli::try_parse_from(["session-search", "search", "q", "--context", "session"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("expected a document count, `turn` or `skeleton`"),
            "{err}"
        );
        assert!(Cli::try_parse_from(["session-search", "search", "q", "--context", "-1"]).is_err());
    }

    /// A turn is defined relative to a document, so `--turn` without `--around` has nothing to
    /// snap to; clap reports that instead of printing the whole session.
    #[test]
    fn show_turn_requires_an_anchor() {
        let Command::Show { around, turn, .. } = parse(&[
            "session-search",
            "show",
            "b20208d8",
            "--around",
            "41",
            "--turn",
        ])
        .command
        else {
            panic!("expected show");
        };
        assert_eq!(around.as_deref(), Some("41"));
        assert!(turn);

        let err = Cli::try_parse_from(["session-search", "show", "b20208d8", "--turn"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--around"), "{err}");
    }

    /// A session id can name two files (§9), and `seq` restarts in each. A uuid is found in
    /// exactly one of them, and the window that follows has to be scoped to that one: fetched
    /// by `seq` alone it can come back from the other file, and `--turn` would then print a
    /// turn the uuid was never in.
    #[test]
    fn a_uuid_anchor_names_the_file_it_was_found_in() {
        use crate::search::testkit::{blank_doc, index_docs};
        let mut docs = Vec::new();
        for seq in 0..4u64 {
            let mut d = blank_doc(seq);
            d.turn_seq = 0;
            d.body = format!("original {seq}");
            docs.push(d);
        }
        for seq in 0..4u64 {
            let mut d = blank_doc(seq);
            d.doc_id = format!("s1:-:relocated:{seq}");
            d.uuid = Some(format!("r-{seq}"));
            d.source_path = "/tmp/relocated/s1.jsonl".into();
            d.turn_seq = 2 * (seq / 2);
            d.body = format!("relocated {seq}");
            docs.push(d);
        }
        let (index, fields) = index_docs(&docs);

        let anchor = resolve_seq(&index, &fields, "s1", None, None, "r-3").unwrap();
        assert_eq!(anchor.seq, 3);
        assert_eq!(
            anchor.source_path.as_deref(),
            Some("/tmp/relocated/s1.jsonl")
        );
        let window = turn_at(
            &index,
            &fields,
            "s1",
            None,
            anchor.source_path.as_deref(),
            anchor.seq,
            10,
        )
        .unwrap();
        assert_eq!(window.turn_seq, 2);
        let bodies: Vec<&str> = window.docs.iter().map(|d| d.body.as_str()).collect();
        assert_eq!(bodies, ["relocated 2", "relocated 3"]);

        // A bare number says nothing about the file, and the caller's own scoping stands.
        let bare = resolve_seq(&index, &fields, "s1", None, None, "3").unwrap();
        assert_eq!(bare.seq, 3);
        assert!(bare.source_path.is_none());
    }

    /// §9's `relocated` case: one transcript indexed under two project keys, so one session id
    /// names two files and the *same record uuid* appears in both. `--around <that uuid>` has
    /// to keep working.
    ///
    /// This is the regression test for scoping the resolution rather than checking it
    /// afterwards. Resolved index-wide, the uuid matches two documents and the shared resolver
    /// refuses it as ambiguous — and `source_path_for` returns `None` in exactly this case, so
    /// there is no spelling of the command the caller could have used instead. The two
    /// candidates are the same `seq` of the same session, i.e. one record read from two files,
    /// which is the one ambiguity with only one answer.
    #[test]
    fn a_uuid_duplicated_across_a_relocated_session_still_anchors() {
        use crate::search::testkit::{blank_doc, index_docs};
        let mut docs = Vec::new();
        for (tag, path) in [
            ("2c9bbdfa", "/tmp/a/s1.jsonl"),
            ("1a3f8831", "/tmp/b/s1.jsonl"),
        ] {
            for seq in 0..4u64 {
                let mut d = blank_doc(seq);
                d.doc_id = format!("s1:-:{tag}:{seq}");
                d.source_path = path.into();
                // Identical uuids in both files — that is what makes it the same transcript.
                d.uuid = Some(format!("eval-notes-a0{seq}"));
                d.turn_seq = 2 * (seq / 2);
                d.body = format!("{tag} {seq}");
                docs.push(d);
            }
        }
        let (index, fields) = index_docs(&docs);

        // The §9 duplicate resolves to one record: two files, same session, same `seq`, so
        // there is nothing to choose between them. That holds unscoped too, which is what lets
        // `--similar-to` seed from a relocated transcript at all.
        let doc = search::resolve_doc(&index, &fields, "eval-notes-a03").unwrap();
        assert_eq!(doc.seq, 3);

        let anchor = resolve_seq(&index, &fields, "s1", None, None, "eval-notes-a03").unwrap();
        assert_eq!(anchor.seq, 3);
        assert!(
            anchor.source_path.is_some(),
            "the window is pinned to a file"
        );

        // Genuine ambiguity inside the session is still a question, not a silent pick: `a0` is
        // the prefix of four different `seq`s.
        let err = format!(
            "{:#}",
            resolve_seq(&index, &fields, "s1", None, None, "eval-notes-a0").unwrap_err()
        );
        assert!(err.contains("ambiguous document reference"), "{err}");
    }

    /// A prefix that is unique inside the session the caller named resolves, even when the same
    /// prefix matches other documents elsewhere in the index.
    ///
    /// This is the other half of scoping the resolution. The candidates here are *not* one
    /// record — different sessions, different files, different uuids — so nothing collapses
    /// them; the only thing that makes the reference answerable is that `show` already named a
    /// transcript, and index-wide resolution threw that away.
    #[test]
    fn an_anchor_unique_inside_the_named_session_beats_a_collision_elsewhere() {
        use crate::search::testkit::{blank_doc, index_docs};
        let mut docs = Vec::new();
        // One document in s1 whose uuid starts with `anchor-ab`.
        let mut d = blank_doc(2);
        d.uuid = Some("anchor-abc".into());
        docs.push(d);
        for seq in 0..2u64 {
            let mut d = blank_doc(seq);
            d.uuid = Some(format!("other-{seq}"));
            docs.push(d);
        }
        // Two more in s2, so `anchor-ab` is a three-way prefix collision index-wide.
        for (seq, uuid) in [(0u64, "anchor-abd"), (1, "anchor-abe")] {
            let mut d = blank_doc(seq);
            d.doc_id = format!("s2:-:{seq}");
            d.session_id = "s2".into();
            d.source_path = "/tmp/s2.jsonl".into();
            d.uuid = Some(uuid.into());
            docs.push(d);
        }
        let (index, fields) = index_docs(&docs);

        // Index-wide the prefix is ambiguous, and `--similar-to` still says so.
        let err = format!(
            "{:#}",
            search::resolve_doc(&index, &fields, "anchor-ab").unwrap_err()
        );
        assert!(err.contains("ambiguous document reference"), "{err}");

        // Scoped to s1 it names exactly one document, which is the window `show` asked for.
        let anchor = resolve_seq(&index, &fields, "s1", None, None, "anchor-ab").unwrap();
        assert_eq!(anchor.seq, 2);
        assert_eq!(anchor.source_path.as_deref(), Some("/tmp/s1.jsonl"));

        // Scoped to s2 it is still ambiguous, because there it really is.
        let err = format!(
            "{:#}",
            resolve_seq(&index, &fields, "s2", None, None, "anchor-ab").unwrap_err()
        );
        assert!(err.contains("ambiguous document reference"), "{err}");
    }

    /// `--around` now resolves index-wide, so it can land somewhere the caller did not ask
    /// about. Windowing that would print one transcript under the heading of another; both
    /// sides are named instead, because "no such uuid" would be a lie about a document that
    /// plainly exists.
    #[test]
    fn an_anchor_outside_the_named_session_is_refused_rather_than_windowed() {
        use crate::search::testkit::{blank_doc, index_docs};
        let mut docs = Vec::new();
        for seq in 0..3u64 {
            docs.push(blank_doc(seq));
        }
        for seq in 0..3u64 {
            let mut d = blank_doc(seq);
            d.doc_id = format!("s2:-:{seq}");
            d.session_id = "s2".into();
            d.source_path = "/tmp/s2.jsonl".into();
            d.uuid = Some(format!("v-{seq}"));
            docs.push(d);
        }
        let (index, fields) = index_docs(&docs);

        let err = format!(
            "{:#}",
            resolve_seq(&index, &fields, "s1", None, None, "v-1").unwrap_err()
        );
        assert!(err.contains("s2") && err.contains("s1"), "{err}");

        // And the ambiguous-prefix error the shared resolver raises reaches `show` unchanged:
        // `u-` is the prefix of every uuid in the main transcript.
        let err = format!(
            "{:#}",
            resolve_seq(&index, &fields, "s1", None, None, "u-").unwrap_err()
        );
        assert!(err.contains("ambiguous document reference"), "{err}");
    }
}
