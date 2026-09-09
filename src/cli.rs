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
use crate::search::{self, Filters, SearchRequest};
use crate::{context, discovery};

/// Facet buckets returned alongside `search --facets`. `facets` has `--top` for the same knob;
/// on `search` the facets are a side dish, so the depth is fixed.
const SEARCH_FACET_TOP: usize = 15;
/// Snippet budget, in characters, for a search hit.
const SNIPPET_CHARS: usize = 240;
/// Cap on the session scan that resolves `--around <UUID>` to a `seq`.
const UUID_SCAN_LIMIT: usize = 50_000;

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
        /// Also index assistant thinking blocks. This is an index-time choice: switching it
        /// on later needs `index --full`, since the watermarks say nothing changed.
        #[arg(long)]
        include_thinking: bool,
        /// Do not follow `Full output saved to: <path>` pointers into `tool-results/`;
        /// index only the "output too large" stub the transcript carries inline.
        #[arg(long)]
        no_spilled_results: bool,
    },
    /// Full-text search.
    Search {
        /// Query string: words, "phrases", AND/OR/NOT, `field:value`.
        query: Option<String>,
        /// Comma-separated facet fields, e.g. `tool_name,tool_input.file_path`.
        #[arg(long, value_delimiter = ',', value_name = "FIELD")]
        facets: Vec<String>,
        /// Also show N turns either side of each hit.
        #[arg(long, default_value_t = 0, value_name = "N")]
        context: usize,
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
        #[command(flatten, next_help_heading = "Filters")]
        filters: Filters,
    },
    /// Count values of a fast field or any `tool_input.<path>`.
    Facets {
        /// `tool_name`, `project`, `model`, `git_branch`, `role`, `kind`, `agent_type`,
        /// `entrypoint`, or a JSON path such as `tool_input.file_path`.
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
        /// A doc uuid or a `seq` number; prints a window instead of the whole session.
        #[arg(long, value_name = "UUID|SEQ")]
        around: Option<String>,
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
    };

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
            include_thinking,
            no_spilled_results,
        } => {
            let roots = resolve_roots(roots)?;
            let stats = index::run(
                index_dir,
                &roots,
                &IndexOptions {
                    full,
                    jobs,
                    include_thinking,
                    load_spilled_results: !no_spilled_results,
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
        } => {
            refresh(index_dir, no_refresh, include_thinking);
            let (index, fields) = index::open_or_create(index_dir)?;
            let request = SearchRequest {
                query: non_empty(query),
                filters,
                limit,
                offset,
                facets,
                facet_top: SEARCH_FACET_TOP,
                snippet_chars: SNIPPET_CHARS,
                include_thinking,
            };
            let response = search::search(&index, &fields, &request)?;
            opts.context = window;
            let around = if window > 0 {
                context_windows(&index, &fields, &response, window)
            } else {
                Vec::new()
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
            let counts = search::facets(&index, &fields, &field, &request)?;
            format::facet_list(out, &field, &counts, &opts)
        }

        Command::Show {
            session_id,
            agent,
            around,
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
            let docs = match around {
                Some(spec) => {
                    let seq = resolve_seq(&index, &fields, &session_id, agent, source, &spec)?;
                    context::around(
                        &index,
                        &fields,
                        &session_id,
                        agent,
                        source,
                        seq,
                        before,
                        after,
                    )?
                }
                None => context::session(&index, &fields, &session_id, agent, source, limit)?,
            };
            if docs.is_empty() {
                tracing::warn!(
                    session = %session_id,
                    "no indexed documents; `session-search sessions` lists what is indexed"
                );
            }
            format::session_view(out, &docs, &opts)
        }

        Command::Sessions {
            filters,
            limit,
            json: _,
            no_refresh,
        } => {
            refresh(index_dir, no_refresh, false);
            warn_unused_session_filters(&filters);
            let matcher = SessionMatcher::new(&filters)?;
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
            let stats = index_stats(index_dir)?;
            format::stats(out, &stats, &opts)
        }
    }
}

// ---------------------------------------------------------------------------
// shared behaviour
// ---------------------------------------------------------------------------

/// The read commands index first, so a search is never silently answered from a stale index.
/// A refresh failure is not fatal: an unreadable transcript root should not stop you searching
/// what was indexed yesterday.
fn refresh(index_dir: &Path, no_refresh: bool, include_thinking: bool) {
    if no_refresh {
        tracing::debug!("--no-refresh: querying the index as it stands");
        return;
    }
    let roots = match resolve_roots(Vec::new()) {
        Ok(roots) => roots,
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "cannot locate transcripts; skipping refresh");
            return;
        }
    };
    match index::run(
        index_dir,
        &roots,
        &IndexOptions {
            include_thinking,
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

/// One `context::around` window per hit. A failure on a single hit degrades that hit to "no
/// context" rather than losing the whole result set.
fn context_windows(
    index: &tantivy::Index,
    fields: &crate::schema::Fields,
    response: &search::SearchResponse,
    window: usize,
) -> Vec<Vec<crate::parse::Doc>> {
    response
        .hits
        .iter()
        .map(|hit| {
            context::around(
                index,
                fields,
                &hit.doc.session_id,
                hit.doc.agent_id.as_deref(),
                Some(hit.doc.source_path.as_str()),
                hit.doc.seq,
                window,
                window,
            )
            .unwrap_or_else(|err| {
                tracing::warn!(doc = %hit.doc.doc_id, error = %format!("{err:#}"), "context lookup failed");
                Vec::new()
            })
        })
        .collect()
}

/// `--around` takes either a `seq` (as printed next to every hit) or a record uuid. A uuid is
/// resolved by scanning the session, which is one targeted term query, not an index scan.
/// Expand an unambiguous id prefix to the full id. An unknown id is passed through
/// unchanged so the caller can report "nothing indexed" rather than "no such session".
fn resolve_id<'a>(
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
fn source_path_for(
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

fn resolve_seq(
    index: &tantivy::Index,
    fields: &crate::schema::Fields,
    session_id: &str,
    agent_id: Option<&str>,
    source_path: Option<&str>,
    spec: &str,
) -> Result<u64> {
    if let Ok(seq) = spec.trim().parse::<u64>() {
        return Ok(seq);
    }
    let docs = context::session(
        index,
        fields,
        session_id,
        agent_id,
        source_path,
        UUID_SCAN_LIMIT,
    )?;
    docs.iter()
        .find(|doc| {
            doc.uuid.as_deref() == Some(spec)
                || doc.doc_id == spec
                || doc.tool_use_id.as_deref() == Some(spec)
        })
        .map(|doc| doc.seq)
        .ok_or_else(|| anyhow!("no document with uuid {spec:?} in session {session_id}"))
}

/// Index statistics without opening (or creating) the Tantivy index: everything shown is
/// already recorded in `state.json` and `sessions.json`, whose shapes are pinned in
/// `docs/DESIGN.md`. `docs_added` is the document count those watermarks account for.
fn index_stats(index_dir: &Path) -> Result<IndexStats> {
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

/// The subset of [`Filters`] that `sessions.json` can answer, pre-resolved once.
///
/// Session records carry no tool, model, role or kind, so those filters are meaningless here
/// and are reported (see [`warn_unused_session_filters`]) rather than silently ignored.
struct SessionMatcher {
    project: Option<String>,
    branch: Option<String>,
    session: Option<String>,
    agent_type: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    no_sidechains: bool,
    sidechains_only: bool,
}

impl SessionMatcher {
    fn new(f: &Filters) -> Result<Self> {
        let now = chrono::Utc::now();
        Ok(SessionMatcher {
            project: f.project.as_deref().map(expand_tilde),
            branch: f.branch.clone(),
            session: f.session.clone(),
            agent_type: f.agent_type.clone(),
            since_ms: f
                .since
                .as_deref()
                .map(|s| when_ms(s, now, Edge::Lower))
                .transpose()
                .context("parsing --since")?,
            until_ms: f
                .until
                .as_deref()
                .map(|s| when_ms(s, now, Edge::Upper))
                .transpose()
                .context("parsing --until")?,
            no_sidechains: f.no_sidechains,
            sidechains_only: f.sidechains_only,
        })
    }

    fn matches(&self, info: &SessionInfo) -> bool {
        // Path-aware, exactly as the index-side filter is: `-p /home/user/alpha` must not drag
        // in the sibling `/home/user/alpha-beta`.
        if let Some(prefix) = &self.project
            && !info
                .project
                .as_deref()
                .is_some_and(|p| search::path_has_prefix(p, prefix))
        {
            return false;
        }
        if let Some(branch) = &self.branch
            && info.git_branch.as_deref() != Some(branch.as_str())
        {
            return false;
        }
        // A session id is long enough that a prefix is a convenience, not an ambiguity.
        if let Some(session) = &self.session
            && !info.session_id.starts_with(session.as_str())
        {
            return false;
        }
        if let Some(kind) = &self.agent_type
            && info.agent_type.as_deref() != Some(kind.as_str())
        {
            return false;
        }
        if self.no_sidechains && info.agent_id.is_some() {
            return false;
        }
        if self.sidechains_only && info.agent_id.is_none() {
            return false;
        }
        // A session overlaps the window if it ended after `since` and started before `until`.
        if let Some(since) = self.since_ms
            && info
                .last_ts_ms
                .or(info.first_ts_ms)
                .is_some_and(|t| t < since)
        {
            return false;
        }
        if let Some(until) = self.until_ms
            && info
                .first_ts_ms
                .or(info.last_ts_ms)
                .is_some_and(|t| t > until)
        {
            return false;
        }
        true
    }
}

fn warn_unused_session_filters(f: &Filters) {
    let mut ignored: Vec<&str> = Vec::new();
    if !f.tool.is_empty() {
        ignored.push("--tool");
    }
    if !f.tool_input.is_empty() {
        ignored.push("--tool-input");
    }
    if f.model.is_some() {
        ignored.push("--model");
    }
    if f.role.is_some() {
        ignored.push("--role");
    }
    if f.kind.is_some() {
        ignored.push("--kind");
    }
    if f.errors_only {
        ignored.push("--errors-only");
    }
    if !ignored.is_empty() {
        tracing::warn!(
            filters = %ignored.join(", "),
            "not applicable to `sessions` (session metadata has no per-message fields); ignored"
        );
    }
}

#[derive(Clone, Copy)]
enum Edge {
    Lower,
    Upper,
}

use search::DAY_MS;

/// RFC3339, `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS]`, `now`, or a relative span (`90s`, `30m`,
/// `24h`, `7d`, `2w`). A bare day is inclusive at both ends, matching `search.rs`: as a lower
/// bound it is midnight, as an upper bound it is the last millisecond of that day.
/// The `search.rs` date parser, resolved to a single instant at the requested edge of the
/// range. Shared so `sessions` — which filters `sessions.json`, not the index — cannot drift
/// from `--since`/`--until` on the index side.
fn when_ms(raw: &str, now: chrono::DateTime<chrono::Utc>, edge: Edge) -> Result<i64> {
    Ok(match search::parse_when(raw, now)? {
        search::When::Instant(ms) => ms,
        // A bare `YYYY-MM-DD` covers the whole day.
        search::When::Day(ms) => match edge {
            Edge::Lower => ms,
            Edge::Upper => ms + DAY_MS - 1,
        },
    })
}

fn expand_tilde(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let home = PathBuf::from(home);
        return match path.strip_prefix("~/") {
            Some(rest) => home.join(rest).to_string_lossy().into_owned(),
            None => home.to_string_lossy().into_owned(),
        };
    }
    path.to_string()
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
            "tool_name,tool_input.file_path",
            "--context",
            "2",
            "--limit",
            "5",
            "--offset",
            "10",
            "--json",
            "--no-refresh",
            "--include-thinking",
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
        } = cli.command
        else {
            panic!("expected the search subcommand");
        };
        assert_eq!(query.as_deref(), Some("cargo build"));
        assert_eq!(filters.project.as_deref(), Some("~/code"));
        assert_eq!(filters.tool, ["Bash", "Edit"]);
        assert_eq!(
            filters.tool_input,
            ["command=cargo", "file_path=src/index.rs"]
        );
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
        assert_eq!(facets, ["tool_name", "tool_input.file_path"]);
        assert_eq!((context, limit, offset), (2, 5, 10));
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
            filters,
        } = parse(&["session-search", "search", "tantivy"]).command
        else {
            panic!("expected search");
        };
        assert_eq!(query.as_deref(), Some("tantivy"));
        assert_eq!((limit, offset, context), (20, 0, 0));
        assert!(facets.is_empty());
        assert!(!json && !no_refresh && !include_thinking);
        assert!(filters.project.is_none() && filters.tool.is_empty());
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
            include_thinking,
            no_spilled_results,
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
            "--include-thinking",
        ])
        .command
        else {
            panic!("expected index");
        };
        assert!(full && include_thinking);
        // Spilled tool results are followed unless explicitly turned off.
        assert!(!no_spilled_results);
        assert_eq!(
            roots,
            [PathBuf::from("/a/projects"), PathBuf::from("/b/projects")]
        );
        assert_eq!(jobs, Some(4));

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
        assert!(!json && !no_refresh);

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

    #[test]
    fn dates_parse_in_every_documented_form() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-09T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ms = |s: &str, edge| when_ms(s, now, edge).unwrap();

        assert_eq!(ms("now", Edge::Lower), now.timestamp_millis());
        assert_eq!(ms("7d", Edge::Lower), now.timestamp_millis() - 7 * DAY_MS);
        assert_eq!(ms("30m", Edge::Lower), now.timestamp_millis() - 30 * 60_000);
        assert_eq!(
            ms("2026-09-09T19:07:19Z", Edge::Lower),
            1_788_980_839_000,
            "RFC3339"
        );
        // A bare day is inclusive at both ends.
        let midnight = ms("2026-09-09", Edge::Lower);
        assert_eq!(ms("2026-09-09", Edge::Upper), midnight + DAY_MS - 1);
        assert_eq!(ms("2026-09-09T19:07", Edge::Lower), midnight + 68_820_000);

        let err = when_ms("last tuesday", now, Edge::Lower).unwrap_err();
        assert!(format!("{err}").contains("cannot read"), "{err}");
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
        match SessionMatcher::new(filters) {
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

    #[test]
    fn a_bad_date_fails_the_command_rather_than_matching_nothing() {
        let err = match SessionMatcher::new(&Filters {
            since: Some("yesterday-ish".into()),
            ..Filters::default()
        }) {
            Ok(_) => panic!("an unreadable date must fail the command"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("--since"), "{err:#}");
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
}
