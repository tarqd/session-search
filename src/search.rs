//! `SearchRequest` -> `SearchResponse`, plus facet aggregation.
//!
//! `Filters` and `SearchRequest` are plain data deliberately: the same structs back the CLI
//! (via `clap::Args`) and, later, the MCP tool parameters (via `serde`).
//!
//! Query shape, in the order the pieces are assembled:
//!
//! * the free-text query goes through `QueryParser` over `text` (+ `thinking` when opted in,
//!   + `tool_input`), so phrases, booleans and `field:value` all work;
//! * every filter is ANDed on top as a term, prefix (regex) or range query;
//! * facets are a terms aggregation collected in the *same* searcher pass as the hits.
//!
//! There is no fuzzy operator: in Tantivy 0.26 `~` is phrase slop, and `set_field_fuzzy` is
//! deliberately not wired up, so nothing here should advertise `term~1`. A query that will not
//! parse falls back to `parse_query_lenient`, and the errors that fallback discards are logged
//! rather than swallowed — a typo'd field name must not look like an empty corpus.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use tantivy::aggregation::AggregationCollector;
use tantivy::aggregation::agg_req::Aggregations;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, ExistsQuery, Occur, Query, QueryParser, RangeQuery, RegexQuery,
    TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption, OwnedValue, Schema, Term, Value as _};
use tantivy::snippet::{Snippet, SnippetGenerator, collapse_overlapped_ranges};
use tantivy::{DateTime, Searcher, TantivyDocument};

use crate::doc::{Doc, DocKind};
use crate::schema::Fields;

/// Wraps the matched span inside a snippet. Plain text on purpose: the snippet travels through
/// JSON output and MCP responses as well as the terminal, so HTML would be wrong everywhere.
const HL_PREFIX: &str = "**";
const HL_SUFFIX: &str = "**";

#[derive(Debug, Clone, Default, clap::Args, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Filters {
    /// Project path; matches by prefix, so `-p ~/code` catches subdirectories.
    #[arg(short = 'p', long, value_name = "PATH")]
    pub project: Option<String>,
    /// Tool name; repeatable.
    #[arg(short = 't', long, value_name = "NAME")]
    pub tool: Vec<String>,
    /// Tool parameter filter as `key=value`, e.g. `--tool-input command=cargo`; repeatable.
    #[arg(long = "tool-input", value_name = "KEY=VALUE")]
    pub tool_input: Vec<String>,
    /// Phrase the tool's *output* must contain, e.g. `--tool-output "No such file"`;
    /// repeatable, and ANDed.
    #[arg(long = "tool-output", value_name = "TEXT")]
    pub tool_output: Vec<String>,
    /// Only turns where the model spent at least N thinking tokens. Works even where the
    /// thinking text itself was stripped before it reached disk, which is the case for remote
    /// and web sessions.
    #[arg(long, value_name = "N")]
    pub min_thinking: Option<u64>,
    /// Program run by a Bash command — any simple command in the script, e.g.
    /// `--program cargo`; repeatable, OR.
    #[arg(long, value_name = "NAME")]
    pub program: Vec<String>,
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    #[arg(long, value_name = "ROLE")]
    pub role: Option<String>,
    /// `message` or `tool_call`.
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<String>,
    /// Which agent wrote the transcript: `claude-code`, … (see `session-search index --help`).
    #[arg(long, value_name = "AGENT")]
    pub agent: Option<String>,
    /// Subagent type (`Explore`, `Plan`, …), for sidechain transcripts.
    #[arg(long = "agent-type", value_name = "TYPE")]
    pub agent_type: Option<String>,
    /// RFC3339, `YYYY-MM-DD`, or a relative span such as `7d`.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
    #[arg(long, value_name = "WHEN")]
    pub until: Option<String>,
    #[arg(long)]
    pub errors_only: bool,
    #[arg(long, conflicts_with = "sidechains_only")]
    pub no_sidechains: bool,
    #[arg(long)]
    pub sidechains_only: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SearchRequest {
    pub query: Option<String>,
    pub filters: Filters,
    pub limit: usize,
    pub offset: usize,
    /// `"tool_name"`, `"project"`, or any JSON path such as `"tool_input.file_path"`.
    pub facets: Vec<String>,
    pub facet_top: usize,
    pub snippet_chars: usize,
    pub include_thinking: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            query: None,
            filters: Filters::default(),
            limit: 20,
            offset: 0,
            facets: Vec::new(),
            facet_top: 20,
            snippet_chars: 240,
            include_thinking: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FacetCount {
    pub value: String,
    pub count: u64,
}

/// A terms aggregation plus the context needed to read it honestly.
///
/// The bucket list alone is misleading on a high-cardinality field: summing the returned
/// buckets answers "how many documents are in the rows I am showing you", which a caller
/// naturally misreads as "how many documents matched". On `tool_input.command` those differ by
/// 50x. So the counts a caller needs to interpret the buckets travel with them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FacetResult {
    pub field: String,
    pub values: Vec<FacetCount>,
    /// Documents matching the query and filters. **Not** the sum of `values`.
    pub matching_docs: u64,
    /// Of those, the ones that actually carry a value for this field. Counted with an
    /// `ExistsQuery`, not summed from the buckets: a multi-valued field like
    /// `bash_cmd.program` buckets a document once per value, so the sum counts values and
    /// can exceed `matching_docs`.
    pub docs_with_value: u64,
    /// Documents whose value fell outside the returned buckets (`sum_other_doc_count`).
    pub other_docs: u64,
    /// Approximate count of distinct values (HyperLogLog), over the matching set.
    pub distinct: Option<u64>,
}

impl FacetResult {
    /// True when the values barely repeat, so a bucket list is just a sample of a long tail.
    ///
    /// Shell commands are the motivating case: they are near-unique strings, so faceting them
    /// returns a list rather than a distribution. Such a field wants full-text search, and
    /// `tool_input` is indexed for exactly that. The threshold is deliberately loose — this
    /// drives a hint, not behaviour.
    pub fn is_search_shaped(&self) -> bool {
        match self.distinct {
            Some(distinct) if self.docs_with_value >= 20 => {
                distinct as f64 >= 0.8 * self.docs_with_value as f64
            }
            _ => false,
        }
    }

    /// Values not shown, as an approximation. `None` when everything fit.
    pub fn hidden_values(&self) -> Option<u64> {
        let distinct = self.distinct?;
        let shown = self.values.len() as u64;
        (distinct > shown).then(|| distinct - shown)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub doc: Doc,
    pub score: f32,
    pub snippet: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
    pub total: usize,
    pub facets: BTreeMap<String, FacetResult>,
    pub elapsed_ms: u128,
}

// ---------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------

pub fn search(
    index: &tantivy::Index,
    f: &Fields,
    req: &SearchRequest,
) -> anyhow::Result<SearchResponse> {
    let started = Instant::now();
    let schema = index.schema();
    let searcher = index.reader()?.searcher();
    let query = build_query(index, f, req)?;

    // `TopDocs` preallocates a heap of `limit + offset` entries **per segment**, so unclamped
    // user-supplied numbers abort the process — or overflow the addition — before a single
    // document is read. No request can return, or skip past, more documents than the index
    // holds, so that is the ceiling for both.
    let top = TopDocs::with_limit(collector_limit(&searcher, req.limit))
        .and_offset(req.offset.min(searcher.num_docs() as usize))
        .order_by_score();

    // Facet fields are validated up front so a typo is a clear error rather than a
    // Tantivy-internal one, and so the aggregation rides along in the same pass as the hits.
    let facet_fields: Vec<&str> = req.facets.iter().map(String::as_str).collect();
    for name in &facet_fields {
        validate_agg_field(&schema, name)?;
    }

    let (top_hits, total, agg) = if facet_fields.is_empty() {
        let (hits, total) = searcher.search(&query, &(top, Count))?;
        (hits, total, None)
    } else {
        let collector = agg_collector(&facet_fields, req.facet_top);
        let (hits, total, agg) = searcher.search(&query, &(top, Count, collector))?;
        (hits, total, Some(agg))
    };

    let mut facets = BTreeMap::new();
    if let Some(agg) = agg {
        let as_json = serde_json::to_value(agg).context("serializing aggregation result")?;
        for (i, name) in facet_fields.iter().enumerate() {
            let with_value = docs_with_value(&searcher, &*query, name)?;
            facets.insert(
                (*name).to_string(),
                facet_result_from(&as_json, i, name, req.facet_top, total as u64, with_value),
            );
        }
    }

    // One snippet generator per response; `create` only keeps the query terms of one field.
    let mut snippets = SnippetGenerator::create(&searcher, &*query, f.text).ok();
    if let Some(generator) = snippets.as_mut() {
        generator.set_max_num_chars(req.snippet_chars.max(32));
    }
    // A doc matched *through* `thinking` stores its body in that field and leaves `text`
    // empty, so without a second generator the one thing `--include-thinking` is paid for is
    // the one thing never shown.
    let mut thinking_snippets = req
        .include_thinking
        .then(|| SnippetGenerator::create(&searcher, &*query, f.thinking).ok())
        .flatten();
    if let Some(generator) = thinking_snippets.as_mut() {
        generator.set_max_num_chars(req.snippet_chars.max(32));
    }
    // Likewise for a tool call matched through its result: the call side is a tool name and a
    // command line, and highlighting that instead of the output the query actually hit shows
    // the caller the one part of the document they did not ask about.
    let mut output_snippets = SnippetGenerator::create(&searcher, &*query, f.tool_output).ok();
    if let Some(generator) = output_snippets.as_mut() {
        generator.set_max_num_chars(req.snippet_chars.max(32));
    }

    // `TopDocs::with_limit(0)` panics, so the collector always asks for at least one doc;
    // an explicit `--limit 0` still means "no hits, just totals and facets".
    let top_hits = if req.limit == 0 { Vec::new() } else { top_hits };

    let mut hits = Vec::with_capacity(top_hits.len());
    for (score, address) in top_hits {
        let stored: TantivyDocument = searcher.doc(address)?;
        let doc = doc_from_stored(f, &stored);
        let highlighted = |g: &Option<SnippetGenerator>| {
            g.as_ref()
                .map(|g| render_snippet(&g.snippet_from_doc(&stored)))
                .filter(|s| !s.trim().is_empty())
        };
        let snippet = highlighted(&snippets)
            .or_else(|| highlighted(&output_snippets))
            .or_else(|| highlighted(&thinking_snippets))
            .unwrap_or_else(|| {
                // Nothing highlighted: fall back to whichever body this document actually has.
                // A failed call leads with its result — on `--errors-only`, which carries no
                // free-text query and so lands here every time, the error is the answer and
                // the command that failed is only context.
                let (first, second) = if doc.is_error {
                    (doc.tool_output.as_deref(), Some(doc.text.as_str()))
                } else {
                    (Some(doc.text.as_str()), doc.tool_output.as_deref())
                };
                let body = [first, second, doc.thinking.as_deref()]
                    .into_iter()
                    .flatten()
                    .find(|b| !b.trim().is_empty())
                    .unwrap_or("");
                excerpt(body, req.snippet_chars)
            });
        hits.push(Hit {
            doc,
            score,
            snippet,
        });
    }

    // An unqualified `word:value` is a JSON-subpath lookup, so it cannot fail to parse — it
    // just finds nothing when that subpath does not exist. Zero hits from a query shaped like
    // that is far more often a misread colon than an empty corpus, so say so rather than
    // letting it look like an authoritative "no".
    if total == 0
        && let Some(text) = non_empty(req.query.as_deref())
        && has_unqualified_field_term(&schema, text)
    {
        tracing::warn!(
            query = %text,
            "no matches: a `word:value` term here was read as a tool_input JSON subpath. \
             If you meant it as text, quote it."
        );
    }

    Ok(SearchResponse {
        hits,
        total,
        facets,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Terms aggregation over any fast field, or any `tool_input.<path>`.
pub fn facets(
    index: &tantivy::Index,
    f: &Fields,
    field: &str,
    req: &SearchRequest,
) -> anyhow::Result<FacetResult> {
    let schema = index.schema();
    validate_agg_field(&schema, field)?;
    let searcher = index.reader()?.searcher();
    let query = build_query(index, f, req)?;
    let collector = agg_collector(&[field], req.facet_top);
    // `Count` rides along so the reported total is documents matched, not the sum of the rows
    // that happened to fit under `--top`.
    let (matching, agg) = searcher.search(&query, &(Count, collector))?;
    let with_value = docs_with_value(&searcher, &*query, field)?;
    let as_json = serde_json::to_value(agg).context("serializing aggregation result")?;
    Ok(facet_result_from(
        &as_json,
        0,
        field,
        req.facet_top,
        matching as u64,
        with_value,
    ))
}

// ---------------------------------------------------------------------------
// query construction
// ---------------------------------------------------------------------------

/// The free-text query ANDed with every active filter. `AllQuery` when nothing is set.
fn build_query(
    index: &tantivy::Index,
    f: &Fields,
    req: &SearchRequest,
) -> anyhow::Result<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    if let Some(text) = non_empty(req.query.as_deref()) {
        // `tool_output` is a default field, not an opt-in one: before it existed the result
        // text lived in `text`, so leaving it out would make a bare query stop matching things
        // it has always matched.
        let mut default_fields = vec![f.text, f.tool_output];
        if req.include_thinking {
            default_fields.push(f.thinking);
        }
        default_fields.push(f.tool_input);
        let mut qp = QueryParser::for_index(index, default_fields);
        // Bare multi-word input reads as "all of these words", which is what people mean;
        // explicit `OR` / `AND` / `"phrases"` / `field:value` still work.
        qp.set_conjunction_by_default();
        let text = &escape_stray_colons(text);
        let parsed = match qp.parse_query(text) {
            Ok(q) => q,
            Err(err) => {
                // A stray `(` or `:` should narrow the search, not abort it — but a typo'd
                // field name must not be indistinguishable from an empty corpus, so say what
                // was thrown away.
                tracing::warn!(query = %text, error = %err, "query did not parse; retrying leniently");
                let (query, errors) = qp.parse_query_lenient(text);
                for err in &errors {
                    tracing::warn!(query = %text, error = %err, "part of the query was ignored");
                }
                query
            }
        };
        clauses.push((Occur::Must, parsed));
    }

    let flt = &req.filters;

    if let Some(project) = non_empty(flt.project.as_deref()) {
        clauses.push((
            Occur::Must,
            path_prefix_query(f.project, &expand_tilde(project))?,
        ));
    }
    if let Some(q) = any_of(f.tool_name, &flt.tool) {
        clauses.push((Occur::Must, q));
    }
    for spec in &flt.tool_input {
        clauses.push((Occur::Must, tool_input_query(index, spec)?));
    }
    for phrase in &flt.tool_output {
        clauses.push((Occur::Must, tool_output_query(index, phrase)?));
    }
    if let Some(q) = program_query(index, &flt.program)? {
        clauses.push((Occur::Must, q));
    }
    // Session ids are 36-char UUIDs, so `--session` matches by prefix — the same affordance
    // `sessions --session` already had, and what anyone pasting the first block expects.
    if let Some(session) = non_empty(flt.session.as_deref()) {
        clauses.push((Occur::Must, prefix_query(f.session_id, session)?));
    }
    for (field, value) in [
        (f.git_branch, flt.branch.as_deref()),
        (f.model, flt.model.as_deref()),
        (f.role, flt.role.as_deref()),
        (f.kind, flt.kind.as_deref()),
        (f.agent_type, flt.agent_type.as_deref()),
        (f.agent, flt.agent.as_deref()),
    ] {
        if let Some(value) = non_empty(value) {
            clauses.push((Occur::Must, term_query(field, value)));
        }
    }

    if flt.errors_only {
        clauses.push((Occur::Must, flag_query(f.is_error, 1)));
    }
    if let Some(min) = flt.min_thinking {
        clauses.push((
            Occur::Must,
            Box::new(RangeQuery::new(
                std::ops::Bound::Included(Term::from_field_u64(f.thinking_tokens, min)),
                std::ops::Bound::Unbounded,
            )),
        ));
    }
    if flt.sidechains_only {
        clauses.push((Occur::Must, flag_query(f.is_sidechain, 1)));
    } else if flt.no_sidechains {
        clauses.push((Occur::Must, flag_query(f.is_sidechain, 0)));
    }

    if let Some(range) = date_range(f.timestamp, flt.since.as_deref(), flt.until.as_deref())? {
        clauses.push((Occur::Must, range));
    }

    Ok(match clauses.len() {
        0 => Box::new(AllQuery),
        1 => clauses.pop().expect("checked len").1,
        _ => Box::new(BooleanQuery::new(clauses)),
    })
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn term_query(field: Field, value: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, value),
        IndexRecordOption::Basic,
    ))
}

/// Escape a `:` that punctuates prose rather than starting a field lookup.
///
/// `tool_input` is a JSON field sitting in the default search fields, so *any* `word:value`
/// parses cleanly — it reads as a lookup on the JSON subpath `word`. That is a deliberate
/// shorthand (`command:cargo` finds Bash commands without spelling out `tool_input.`), but it
/// also means the parser can never report an unknown field, so a pasted URL or ordinary prose
/// parses fine and then matches nothing, silently. The corpus this was found on holds
/// `https://github.com` 86 times and the query returned zero.
///
/// The two cases are separable by what follows the colon. A field lookup always has a value
/// immediately after it — `cargo`, `1`, `>=5000`, `[1 TO *]`, `"a phrase"`. Prose does not:
/// `https://github.com` has a `/`, and `note: this` has a space. So a colon followed by
/// whitespace, by `/`, or by nothing is punctuation, and is escaped to be searched literally.
/// Quoted spans are left exactly as written.
fn escape_stray_colons(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 8);
    let mut in_quotes = false;
    let mut chars = query.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        if c == '"' {
            in_quotes = !in_quotes;
            out.push(c);
            continue;
        }
        if c == ':' && !in_quotes {
            let starts_a_value = chars
                .peek()
                .is_some_and(|(_, next)| !next.is_whitespace() && *next != '/');
            if !starts_a_value {
                out.push('\\');
            }
        }
        out.push(c);
    }
    out
}

/// Does this query contain a `word:` whose root is not a field in the schema? Such a term is a
/// JSON-subpath lookup, which is valid but matches nothing when the subpath does not exist —
/// worth saying out loud when the search came back empty.
fn has_unqualified_field_term(schema: &Schema, query: &str) -> bool {
    let mut in_quotes = false;
    let mut token = String::new();
    for c in query.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                token.clear();
            }
            _ if in_quotes => {}
            ':' => {
                let root = token.split('.').next().unwrap_or("");
                if !root.is_empty() && schema.get_field(root).is_err() {
                    return true;
                }
                token.clear();
            }
            c if c.is_whitespace() || matches!(c, '(' | ')' | '+' | '-') => token.clear(),
            c => token.push(c),
        }
    }
    false
}

fn flag_query(field: Field, value: u64) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_u64(field, value),
        IndexRecordOption::Basic,
    ))
}

/// `-t Bash -t Read` means "Bash **or** Read", as one ANDed clause.
fn any_of(field: Field, values: &[String]) -> Option<Box<dyn Query>> {
    let mut shoulds: Vec<(Occur, Box<dyn Query>)> = values
        .iter()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| (Occur::Should, term_query(field, v)))
        .collect();
    match shoulds.len() {
        0 => None,
        1 => Some(shoulds.pop().expect("checked len").1),
        _ => Some(Box::new(BooleanQuery::new(shoulds))),
    }
}

/// A `STRING` field holds the whole value as one term, so a prefix match is a regex anchored
/// at the start of that term. Used for ids, where a bare character prefix is exactly what a
/// user pastes.
fn prefix_query(field: Field, prefix: &str) -> anyhow::Result<Box<dyn Query>> {
    let pattern = format!("{}.*", regex_escape(prefix));
    let q = RegexQuery::from_pattern(&pattern, field)
        .with_context(|| format!("building a prefix query for {prefix:?}"))?;
    Ok(Box::new(q))
}

/// [`prefix_query`] for a **path**: the prefix has to end on a path boundary, so `-p
/// /home/user/alpha` catches `/home/user/alpha/sub` but not the sibling `/home/user/alpha-beta`.
/// A trailing slash on the argument is ignored, so both spellings mean the same directory.
fn path_prefix_query(field: Field, prefix: &str) -> anyhow::Result<Box<dyn Query>> {
    let normalized = prefix.trim_end_matches('/');
    let pattern = format!("{}(/.*)?", regex_escape(normalized));
    let q = RegexQuery::from_pattern(&pattern, field)
        .with_context(|| format!("building a project filter for {prefix:?}"))?;
    Ok(Box::new(q))
}

/// Does `path` sit at or below `prefix`, on a path boundary? The non-index twin of
/// [`path_prefix_query`], for the session list — which filters `sessions.json`, not Tantivy.
pub fn path_has_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn expand_tilde(path: &str) -> String {
    let home = std::env::var("HOME").ok();
    match (path, home) {
        ("~", Some(home)) => home,
        (p, Some(home)) => match p.strip_prefix("~/") {
            Some(rest) => format!("{}/{}", home.trim_end_matches('/'), rest),
            None => p.to_string(),
        },
        (p, None) => p.to_string(),
    }
}

/// `--tool-input command=cargo` -> a query over the JSON subpath `tool_input.command`.
///
/// Routed through `QueryParser` on purpose: it emits both the tokenized text terms *and* the
/// typed fast-value term, so `path=/tmp/x.rs`, `command="cargo build"` and `timeout=600000`
/// all match the way they were indexed.
/// `--tool-output TEXT` as a phrase query over what the tool returned. Quoted, so an operator
/// or a stray colon in the text is matched literally rather than reinterpreted as grammar.
fn tool_output_query(index: &tantivy::Index, phrase: &str) -> anyhow::Result<Box<dyn Query>> {
    let phrase = phrase.trim();
    if phrase.is_empty() {
        bail!("--tool-output expects a non-empty value");
    }
    let escaped = phrase.replace('\\', r"\\").replace('"', r#"\""#);
    let qp = QueryParser::for_index(index, Vec::new());
    qp.parse_query(&format!("tool_output:\"{escaped}\""))
        .with_context(|| format!("building a tool-output filter from {phrase:?}"))
}

fn tool_input_query(index: &tantivy::Index, spec: &str) -> anyhow::Result<Box<dyn Query>> {
    let (key, value) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("--tool-input expects KEY=VALUE, got {spec:?}"))?;
    let key = key.trim();
    if key.is_empty() {
        bail!("--tool-input expects a non-empty key, got {spec:?}");
    }
    if key.contains([' ', '"', ':']) {
        bail!("--tool-input key {key:?} contains a character the query grammar reserves");
    }
    if value.trim().is_empty() {
        // `tool_input.k:""` parses cleanly and matches nothing, which is indistinguishable
        // from "this value does not occur". Say what actually went wrong instead.
        bail!("--tool-input expects a non-empty value, got {spec:?}");
    }
    let escaped = value.replace('\\', r"\\").replace('"', r#"\""#);
    let expr = format!("tool_input.{key}:\"{escaped}\"");
    let qp = QueryParser::for_index(index, Vec::new());
    qp.parse_query(&expr)
        .with_context(|| format!("building a tool-input filter from {spec:?}"))
}

/// `--program cargo --program git` -> one ANDed clause, OR over the values, each a query on
/// the JSON subpath `bash_cmd.program`.
///
/// Built through `QueryParser` exactly like [`tool_input_query`], for the same reason: the
/// parser emits the terms the JSON field actually indexed. `bash_cmd` is tokenized `raw`, so
/// the value is matched whole and case-sensitively — `cargo` finds `cargo`, never `Cargo` and
/// never `cargo-nextest`. Empty values are skipped, as in [`any_of`].
fn program_query(
    index: &tantivy::Index,
    values: &[String],
) -> anyhow::Result<Option<Box<dyn Query>>> {
    let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let qp = QueryParser::for_index(index, Vec::new());
    for value in values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
        let escaped = value.replace('\\', r"\\").replace('"', r#"\""#);
        let expr = format!("bash_cmd.program:\"{escaped}\"");
        let query = qp
            .parse_query(&expr)
            .with_context(|| format!("building a --program filter from {value:?}"))?;
        shoulds.push((Occur::Should, query));
    }
    Ok(match shoulds.len() {
        0 => None,
        1 => Some(shoulds.pop().expect("checked len").1),
        _ => Some(Box::new(BooleanQuery::new(shoulds))),
    })
}

// ---------------------------------------------------------------------------
// dates
// ---------------------------------------------------------------------------

/// A parsed `--since` / `--until` value. A bare `YYYY-MM-DD` is remembered as a *day* so that
/// `--until 2026-09-09` covers that whole day instead of stopping at its first instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    Instant(i64),
    Day(i64),
}

pub(crate) const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn date_range(
    field: Field,
    since: Option<&str>,
    until: Option<&str>,
) -> anyhow::Result<Option<Box<dyn Query>>> {
    let since = non_empty(since)
        .map(|s| parse_when(s, chrono::Utc::now()))
        .transpose()
        .context("parsing --since")?;
    let until = non_empty(until)
        .map(|s| parse_when(s, chrono::Utc::now()))
        .transpose()
        .context("parsing --until")?;
    if since.is_none() && until.is_none() {
        return Ok(None);
    }

    let at = |ms: i64| Term::from_field_date(field, DateTime::from_timestamp_millis(ms));
    let lower = match since {
        // Both forms are inclusive at the lower end: "since that day" starts at midnight.
        Some(When::Instant(ms) | When::Day(ms)) => Bound::Included(at(ms)),
        None => Bound::Unbounded,
    };
    let upper = match until {
        Some(When::Instant(ms)) => Bound::Included(at(ms)),
        Some(When::Day(ms)) => Bound::Excluded(at(ms + DAY_MS)),
        None => Bound::Unbounded,
    };
    Ok(Some(Box::new(RangeQuery::new(lower, upper))))
}

/// RFC3339, `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SS` (assumed UTC), `now`, or a relative span
/// counted back from `now`: `90s`, `30m`, `24h`, `7d`, `2w`.
pub(crate) fn parse_when(raw: &str, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<When> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(When::Instant(now.timestamp_millis()));
    }
    if let Some(ms) = parse_relative(s) {
        return Ok(When::Instant(now.timestamp_millis() - ms));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(When::Instant(dt.timestamp_millis()));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let midnight = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow!("{s:?} is not a representable date"))?;
        return Ok(When::Day(midnight.and_utc().timestamp_millis()));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(When::Instant(dt.and_utc().timestamp_millis()));
        }
    }
    bail!(
        "cannot read {raw:?} as a date: expected RFC3339, YYYY-MM-DD, `now`, \
         or a relative span such as 7d / 24h / 30m"
    )
}

/// `7d` -> milliseconds. `None` when the shape does not match.
fn parse_relative(s: &str) -> Option<i64> {
    let (digits, unit) = s.split_at(s.len().checked_sub(1)?);
    let unit_ms = match unit {
        "s" | "S" => 1_000,
        "m" | "M" => 60 * 1_000,
        "h" | "H" => 60 * 60 * 1_000,
        "d" | "D" => DAY_MS,
        "w" | "W" => 7 * DAY_MS,
        _ => return None,
    };
    let n: i64 = digits.parse().ok()?;
    n.checked_mul(unit_ms)
}

// ---------------------------------------------------------------------------
// aggregations
// ---------------------------------------------------------------------------

fn card_key(i: usize) -> String {
    format!("c{i}")
}

fn agg_key(i: usize) -> String {
    // Facet field names carry dots (`tool_input.file_path`); positional keys keep the
    // aggregation request JSON unambiguous.
    format!("f{i}")
}

fn agg_collector(fields: &[&str], top: usize) -> AggregationCollector {
    let size = top.clamp(1, 65_000) as u32;
    let mut req = serde_json::Map::new();
    for (i, field) in fields.iter().enumerate() {
        req.insert(
            agg_key(i),
            json!({ "terms": { "field": field, "size": size } }),
        );
        // Distinct-value count rides along in the same pass; it is what tells a caller that a
        // field is a long tail rather than a distribution.
        req.insert(card_key(i), json!({ "cardinality": { "field": field } }));
    }
    let aggs: Aggregations = serde_json::from_value(Value::Object(req))
        .expect("terms aggregation request is well-formed");
    AggregationCollector::from_aggs(aggs, Default::default())
}

/// Documents in `query`'s match set that carry any value for `field`.
///
/// This cannot be read off the buckets: a terms aggregation counts a document once per
/// *value*, so on a multi-valued field like `bash_cmd.program` (one entry per simple command
/// in the script) summing the buckets counts values, and can sail past `matching_docs`.
fn docs_with_value(searcher: &Searcher, query: &dyn Query, field: &str) -> tantivy::Result<u64> {
    let exists = BooleanQuery::new(vec![
        (Occur::Must, query.box_clone()),
        (
            Occur::Must,
            Box::new(ExistsQuery::new(field.to_string(), false)) as Box<dyn Query>,
        ),
    ]);
    searcher.search(&exists, &Count).map(|n| n as u64)
}

/// Assemble the buckets and the counts needed to read them, for facet `i` of a request.
fn facet_result_from(
    result: &Value,
    i: usize,
    field: &str,
    top: usize,
    matching_docs: u64,
    docs_with_value: u64,
) -> FacetResult {
    let values = buckets_from(result, &agg_key(i), top);
    let other_docs = result
        .get(agg_key(i))
        .and_then(|v| v.get("sum_other_doc_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // The cardinality metric is a float in the aggregation JSON, and absent when the field has
    // no values at all in the matching set.
    let distinct = result
        .get(card_key(i))
        .and_then(|v| v.get("value"))
        .and_then(Value::as_f64)
        .map(|v| v.round() as u64);
    FacetResult {
        field: field.to_string(),
        docs_with_value,
        matching_docs,
        other_docs,
        distinct,
        values,
    }
}

/// A terms aggregation may be keyed on a string (text fast fields, JSON subpaths) or on a
/// number (`is_error`, `seq`); both render as a display string.
fn buckets_from(result: &Value, key: &str, top: usize) -> Vec<FacetCount> {
    let Some(buckets) = result
        .get(key)
        .and_then(|v| v.get("buckets"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    buckets
        .iter()
        .take(top)
        .filter_map(|b| {
            let value = match b.get("key")? {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            let count = b.get("doc_count").and_then(Value::as_u64).unwrap_or(0);
            Some(FacetCount { value, count })
        })
        .collect()
}

/// A terms aggregation needs a fast field. Accepts a declared fast field by name, or any
/// `<json fast field>.<subpath>` — including subpaths that were never named in the schema.
fn validate_agg_field(schema: &Schema, name: &str) -> anyhow::Result<()> {
    let base = name.split('.').next().unwrap_or(name);
    let field = schema
        .get_field(base)
        .map_err(|_| anyhow!("unknown facet field {name:?}"))?;
    let entry = schema.get_field_entry(field);
    if !entry.field_type().is_fast() {
        bail!("facet field {name:?} is not a fast field and cannot be aggregated");
    }
    if base != name && !entry.field_type().is_json() {
        bail!("facet field {name:?} uses a JSON subpath but {base:?} is not a JSON field");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// snippets
// ---------------------------------------------------------------------------

/// The matched spans wrapped in `**`, taken from the stored `text`. HTML escaping (what
/// `Snippet::to_html` does) would corrupt the code and paths these transcripts are full of.
fn render_snippet(snippet: &Snippet) -> String {
    let fragment = snippet.fragment();
    let ranges = collapse_overlapped_ranges(snippet.highlighted());
    if ranges.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(fragment.len() + ranges.len() * 4);
    let mut cursor = 0;
    for range in ranges {
        if range.start < cursor || range.end > fragment.len() {
            continue;
        }
        out.push_str(&fragment[cursor..range.start]);
        out.push_str(HL_PREFIX);
        out.push_str(&fragment[range.clone()]);
        out.push_str(HL_SUFFIX);
        cursor = range.end;
    }
    out.push_str(&fragment[cursor..]);
    out
}

/// Head-of-text fallback for hits with nothing to highlight — a filter-only search, or a
/// match that landed in `tool_input` rather than `text`.
fn excerpt(text: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(16);
    let mut out = String::with_capacity(max_chars);
    let mut chars = 0;
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            chars += 1;
            pending_space = false;
        }
        if chars >= max_chars {
            out.push('…');
            return out;
        }
        out.push(c);
        chars += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// stored document -> Doc
// ---------------------------------------------------------------------------

/// Rebuild a [`Doc`] from its stored fields — the inverse of [`crate::schema::doc_to_json`].
///
/// Shared with `context.rs`, which reads the same stored documents.
pub fn doc_from_stored(f: &Fields, stored: &TantivyDocument) -> Doc {
    let s = |field: Field| -> Option<String> {
        stored
            .get_first(field)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let u = |field: Field| -> Option<u64> { stored.get_first(field).and_then(|v| v.as_u64()) };
    let flag = |field: Field| -> bool { u(field).unwrap_or(0) != 0 };

    let kind = match s(f.kind).as_deref() {
        Some("tool_call") => DocKind::ToolCall,
        _ => DocKind::Message,
    };
    let json = |field: Field| -> Option<Value> {
        stored.get_first(field).and_then(|v| {
            serde_json::to_value(OwnedValue::from(v.as_value()))
                .ok()
                .filter(|v| !v.is_null())
        })
    };
    let tool_input = json(f.tool_input);
    let bash_cmd = json(f.bash_cmd);

    Doc {
        doc_id: s(f.doc_id).unwrap_or_default(),
        agent: s(f.agent).unwrap_or_default(),
        kind,
        source_path: s(f.source_path).unwrap_or_default(),
        seq: u(f.seq).unwrap_or(0),
        session_id: s(f.session_id).unwrap_or_default(),
        agent_id: s(f.agent_id),
        agent_type: s(f.agent_type),
        uuid: s(f.uuid),
        parent_uuid: s(f.parent_uuid),
        timestamp_ms: stored
            .get_first(f.timestamp)
            .and_then(|v| v.as_datetime())
            .map(|dt| dt.into_timestamp_millis()),
        project: s(f.project),
        git_branch: s(f.git_branch),
        role: s(f.role).unwrap_or_default(),
        model: s(f.model),
        tool_name: s(f.tool_name),
        tool_use_id: s(f.tool_use_id),
        tool_input,
        bash_cmd,
        is_error: flag(f.is_error),
        is_sidechain: flag(f.is_sidechain),
        is_meta: flag(f.is_meta),
        entrypoint: s(f.entrypoint),
        permission_mode: s(f.permission_mode),
        version: s(f.version),
        slug: s(f.slug),
        text: s(f.text).unwrap_or_default(),
        tool_output: s(f.tool_output),
        thinking: s(f.thinking),
        thinking_tokens: u(f.thinking_tokens),
        raw: s(f.raw).unwrap_or_default(),
    }
}

/// No collector can usefully hold more entries than the index has documents, and `TopDocs`
/// preallocates whatever it is given — so this is what stands between a mistyped `--limit` and
/// a `capacity overflow` panic or an out-of-memory abort.
fn collector_limit(searcher: &Searcher, wanted: usize) -> usize {
    wanted.min(searcher.num_docs() as usize).max(1)
}

/// Docs matching `query`, ordered ascending by the `seq` fast field. Used by `context.rs`.
pub(crate) fn docs_by_seq(
    searcher: &Searcher,
    f: &Fields,
    query: &dyn Query,
    limit: usize,
) -> anyhow::Result<Vec<Doc>> {
    use tantivy::Order;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let collector = TopDocs::with_limit(collector_limit(searcher, limit))
        .order_by_fast_field::<u64>("seq", Order::Asc);
    let found = searcher.search(query, &collector)?;
    let mut docs = Vec::with_capacity(found.len());
    for (_, address) in found {
        let stored: TantivyDocument = searcher.doc(address)?;
        docs.push(doc_from_stored(f, &stored));
    }
    docs.sort_by_key(|d| d.seq);
    docs.dedup_by(|a, b| a.doc_id == b.doc_id);
    Ok(docs)
}

/// `session_id` (+ optional `agent_id`, + optional `source_path`) as an ANDed clause list.
///
/// `agent_id: None` means the **main** transcript, not "any": subagent files number their
/// `seq` from zero as well, so without this they would collide with the parent session.
///
/// `source_path` is the same argument one level down. `seq` is a per-*file* ordinal, and two
/// files can share a `sessionId` (§9), so a window scoped only by session id can interleave
/// two transcripts and drop the neighbours it was asked for. Pass `None` only when the caller
/// genuinely means "whichever file this session lives in".
pub(crate) fn session_clauses(
    f: &Fields,
    session_id: &str,
    agent_id: Option<&str>,
    source_path: Option<&str>,
) -> Vec<(Occur, Box<dyn Query>)> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
        vec![(Occur::Must, term_query(f.session_id, session_id))];
    match agent_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(agent) => clauses.push((Occur::Must, term_query(f.agent_id, agent))),
        None => clauses.push((
            Occur::MustNot,
            Box::new(ExistsQuery::new("agent_id".to_string(), false)),
        )),
    }
    if let Some(path) = source_path.map(str::trim).filter(|s| !s.is_empty()) {
        clauses.push((Occur::Must, term_query(f.source_path, path)));
    }
    clauses
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::schema::{build_schema, doc_to_json};
    use tantivy::Index;

    pub fn blank_doc(seq: u64) -> Doc {
        Doc {
            doc_id: format!("s1:-:{seq}"),
            agent: "claude-code".into(),
            kind: DocKind::Message,
            source_path: "/tmp/s1.jsonl".into(),
            seq,
            session_id: "s1".into(),
            agent_id: None,
            agent_type: None,
            uuid: Some(format!("u-{seq}")),
            parent_uuid: None,
            timestamp_ms: Some(1_757_000_000_000 + seq as i64 * 1000),
            project: Some("/home/user/session-search".into()),
            git_branch: Some("main".into()),
            role: "user".into(),
            model: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            bash_cmd: None,
            is_error: false,
            is_sidechain: false,
            is_meta: false,
            entrypoint: Some("cli".into()),
            permission_mode: None,
            version: Some("2.1.266".into()),
            slug: None,
            text: String::new(),
            tool_output: None,
            thinking: None,
            thinking_tokens: None,
            raw: "{}".into(),
        }
    }

    /// A RAM index built straight from hand-made [`Doc`]s — no dependency on the indexer.
    pub fn index_docs(docs: &[Doc]) -> (Index, Fields) {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema.clone());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for doc in docs {
            let json = doc_to_json(doc, true).to_string();
            writer
                .add_document(TantivyDocument::parse_json(&schema, &json).unwrap())
                .unwrap();
        }
        writer.commit().unwrap();
        (index, fields)
    }

    /// Eight docs covering every dimension the filters touch.
    pub fn corpus() -> Vec<Doc> {
        let mut docs = Vec::new();

        let mut d = blank_doc(0);
        d.text = "please make the tantivy schema faster".into();
        docs.push(d);

        let mut d = blank_doc(1);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.model = Some("claude-opus-5".into());
        d.tool_name = Some("Bash".into());
        d.tool_use_id = Some("toolu_1".into());
        d.tool_input = Some(json!({"command": "cargo build --release", "timeout": 600000}));
        d.text = "Bash\ncargo build --release".into();
        d.tool_output = Some("Compiling tantivy\nFinished dev profile".into());
        d.bash_cmd = crate::bash::extract("cargo build --release").map(|c| c.to_json());
        docs.push(d);

        let mut d = blank_doc(2);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.model = Some("claude-opus-5".into());
        d.tool_name = Some("Read".into());
        d.tool_input = Some(json!({"file_path": "/home/user/session-search/src/index.rs"}));
        d.text = "Read\n/home/user/session-search/src/index.rs".into();
        d.tool_output = Some("pub fn open_or_create(index_dir: &Path)".into());
        docs.push(d);

        let mut d = blank_doc(3);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.tool_name = Some("Bash".into());
        d.tool_input = Some(json!({"command": "cargo test"}));
        d.bash_cmd = crate::bash::extract("cargo test").map(|c| c.to_json());
        d.text = "cargo test\nerror: test failed".into();
        d.is_error = true;
        docs.push(d);

        let mut d = blank_doc(4);
        d.text = "a sidechain turn about tantivy".into();
        d.is_sidechain = true;
        d.agent_id = Some("a10845c5ff9c7d4ec".into());
        d.agent_type = Some("Explore".into());
        d.session_id = "s1".into();
        d.doc_id = "s1:a10845c5ff9c7d4ec:0".into();
        d.source_path = "/tmp/s1/subagents/agent-a10845c5ff9c7d4ec.jsonl".into();
        d.seq = 0;
        docs.push(d);

        let mut d = blank_doc(5);
        d.text = "an older turn in another project".into();
        d.project = Some("/home/user/other-project".into());
        d.git_branch = Some("wip".into());
        d.timestamp_ms = Some(
            chrono::NaiveDate::from_ymd_opt(2024, 3, 1)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis(),
        );
        docs.push(d);

        let mut d = blank_doc(6);
        d.text = "nested project below the search root".into();
        d.project = Some("/home/user/session-search/sub/dir".into());
        docs.push(d);

        let mut d = blank_doc(7);
        d.role = "assistant".into();
        d.text = "the visible answer".into();
        d.thinking = Some("a private deliberation about parsnips".into());
        docs.push(d);

        docs
    }

    pub fn texts(r: &SearchResponse) -> Vec<String> {
        r.hits.iter().map(|h| h.doc.text.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    fn req(query: &str) -> SearchRequest {
        SearchRequest {
            query: (!query.is_empty()).then(|| query.to_string()),
            ..SearchRequest::default()
        }
    }

    /// `tool_input` is a JSON field in the default search fields, so any `word:value` parses
    /// cleanly as a JSON-subpath lookup — which made a pasted URL match nothing and say nothing.
    /// The corpus this was found on holds `https://github.com` 86 times; the query returned 0.
    #[test]
    fn a_colon_in_ordinary_text_is_not_a_field_lookup() {
        let mut docs = corpus();
        let mut d = blank_doc(90);
        d.text = "see https://github.com/tarqd/session-search for the source".into();
        docs.push(d);
        let mut d = blank_doc(91);
        d.text = "note: this one is prose, not a field".into();
        docs.push(d);
        let (index, fields) = index_docs(&docs);

        let hits = |q: &str| search(&index, &fields, &req(q)).unwrap().total;

        assert_eq!(hits("https://github.com"), 1, "a URL must search as text");
        assert_eq!(
            hits("https://github.com"),
            hits("\"https://github.com\""),
            "quoting it must not change the answer"
        );
        assert_eq!(
            hits("note: this"),
            1,
            "prose with a colon must search as text"
        );
    }

    /// The other half of the same rule: a prefix that *does* name a field is still a lookup,
    /// including a JSON subpath, which is the feature the whole schema is built around.
    #[test]
    fn a_colon_after_a_real_field_name_is_still_a_field_lookup() {
        let (index, fields) = index_docs(&corpus());
        let hits = |q: &str| search(&index, &fields, &req(q)).unwrap().total;

        assert!(hits("tool_input.command:cargo") > 0, "JSON subpath lookup");
        assert!(hits("text:tantivy") > 0, "plain field lookup");
        assert_eq!(
            hits("is_error:1"),
            search(
                &index,
                &fields,
                &SearchRequest {
                    filters: Filters {
                        errors_only: true,
                        ..Filters::default()
                    },
                    ..SearchRequest::default()
                }
            )
            .unwrap()
            .total,
            "the flag and the field query are the same filter"
        );
    }

    /// The bug this guards: the facet total used to be the sum of the returned buckets, so a
    /// field with 60 near-unique values reported "3 docs" under `--top 3` when 60 matched.
    #[test]
    fn a_truncated_facet_reports_matching_docs_not_the_visible_rows() {
        let mut docs = Vec::new();
        for i in 0..60u64 {
            let mut d = blank_doc(i);
            d.kind = DocKind::ToolCall;
            d.role = "assistant".into();
            d.tool_name = Some("Bash".into());
            d.tool_use_id = Some(format!("toolu_{i}"));
            // Near-unique, like real shell commands.
            d.tool_input = Some(json!({ "command": format!("cargo test --test case_{i}") }));
            d.text = format!("cargo test --test case_{i}");
            docs.push(d);
        }
        let (index, fields) = index_docs(&docs);

        let request = SearchRequest {
            facet_top: 3,
            ..SearchRequest::default()
        };
        let r = facets(&index, &fields, "tool_input.command", &request).unwrap();

        assert_eq!(r.values.len(), 3, "only --top rows come back");
        assert_eq!(r.matching_docs, 60, "every doc matched the empty query");
        assert_eq!(r.docs_with_value, 60, "every doc carries a command");
        assert_eq!(
            r.other_docs, 57,
            "the docs behind the rows that did not fit are still counted"
        );
        // Cardinality is a HyperLogLog estimate; exact at this size, so allow a little slack.
        let distinct = r
            .distinct
            .expect("cardinality rides along with the terms agg");
        assert!((55..=60).contains(&distinct), "distinct was {distinct}");
        assert!(
            r.is_search_shaped(),
            "60 distinct values over 60 docs is a long tail, not a distribution"
        );
    }

    /// The other side of the same judgement: a field that genuinely repeats must not be
    /// labelled search-shaped, or the hint becomes noise.
    #[test]
    fn a_repeating_field_is_not_flagged_as_search_shaped() {
        let mut docs = Vec::new();
        for i in 0..60u64 {
            let mut d = blank_doc(i);
            d.kind = DocKind::ToolCall;
            d.role = "assistant".into();
            d.tool_name = Some(if i % 2 == 0 { "Bash" } else { "Read" }.into());
            d.tool_use_id = Some(format!("toolu_{i}"));
            d.text = "tool call".into();
            docs.push(d);
        }
        let (index, fields) = index_docs(&docs);

        let r = facets(&index, &fields, "tool_name", &SearchRequest::default()).unwrap();

        assert_eq!(r.values.len(), 2);
        assert_eq!(r.matching_docs, 60);
        assert_eq!(r.other_docs, 0);
        assert_eq!(r.distinct, Some(2));
        assert!(
            !r.is_search_shaped(),
            "two values over 60 docs is a distribution"
        );
    }

    /// Facet counts must describe the filtered set, not the whole index.
    #[test]
    fn facet_totals_follow_the_active_filters() {
        let (index, fields) = index_docs(&corpus());
        let mut request = SearchRequest::default();
        request.filters.tool = vec!["Bash".into()];
        let r = facets(&index, &fields, "tool_name", &request).unwrap();
        assert!(
            r.matching_docs > 0 && r.matching_docs < corpus().len() as u64,
            "matching_docs {} should be the filtered subset",
            r.matching_docs
        );
        assert_eq!(r.values.len(), 1, "only Bash survives the filter");
    }

    #[test]
    fn free_text_matches_and_ranks() {
        let (index, f) = index_docs(&corpus());
        let r = search(&index, &f, &req("tantivy")).unwrap();
        assert_eq!(r.total, 3, "{:?}", texts(&r));
        assert!(r.hits.iter().all(|h| h.score > 0.0));
    }

    #[test]
    fn bare_multi_word_input_is_conjunctive() {
        let (index, f) = index_docs(&corpus());
        // "schema" alone is in doc 0; "faster" alone is in doc 0 too. "tantivy schema" must
        // not drag in the docs that only mention tantivy.
        let r = search(&index, &f, &req("tantivy schema")).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        let r = search(&index, &f, &req("tantivy OR schema")).unwrap();
        assert_eq!(r.total, 3);
    }

    /// The whole point of the field: ask about what a tool *returned*, and get only that.
    #[test]
    fn tool_output_is_searchable_apart_from_the_call() {
        let (index, f) = index_docs(&corpus());

        // `tool_output:` reaches the result and nothing else.
        let r = search(&index, &f, &req("tool_output:Compiling")).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert_eq!(r.hits[0].doc.tool_name.as_deref(), Some("Bash"));

        // ...and it does NOT reach the command line, which lives in `text`.
        let r = search(&index, &f, &req("tool_output:release")).unwrap();
        assert_eq!(r.total, 0, "the input is not the output: {:?}", texts(&r));

        // The mirror: `text:` sees the call and not the result.
        let r = search(&index, &f, &req("text:Compiling")).unwrap();
        assert_eq!(r.total, 0, "{:?}", texts(&r));

        // A bare query still spans both, exactly as it did when the result lived in `text`.
        for query in ["Compiling", "release"] {
            let r = search(&index, &f, &req(query)).unwrap();
            assert!(r.total > 0, "bare query {query:?} lost its hits");
        }
    }

    /// `--tool-output` is a phrase filter, so an operator inside it is text, not grammar.
    #[test]
    fn the_tool_output_filter_is_a_phrase_and_ands_with_the_rest() {
        let (index, f) = index_docs(&corpus());

        let mut r0 = SearchRequest::default();
        r0.filters.tool_output = vec!["Finished dev profile".into()];
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.hits[0].doc.tool_use_id.as_deref(), Some("toolu_1"));

        // Adjacent-in-that-order, like any phrase.
        r0.filters.tool_output = vec!["profile dev Finished".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);

        // Repeated, the filters AND rather than replace one another.
        r0.filters.tool_output = vec!["Compiling".into(), "Finished".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);
        r0.filters.tool_output = vec!["Compiling".into(), "nonesuch".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);

        // And it composes with the other filters instead of overriding them.
        r0.filters.tool_output = vec!["Compiling".into()];
        r0.filters.tool = vec!["Read".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);

        // An empty value is an error, not a silent match-nothing.
        let mut bad = SearchRequest::default();
        bad.filters.tool_output = vec!["   ".into()];
        assert!(search(&index, &f, &bad).is_err());
    }

    /// A `tool_result` whose `tool_use` never appeared has an empty `text`; the response must
    /// still show what came back rather than a blank line.
    #[test]
    fn an_output_only_document_still_gets_a_snippet() {
        let mut d = blank_doc(0);
        d.kind = DocKind::ToolCall;
        d.text = String::new();
        d.tool_use_id = Some("toolu_orphan".into());
        d.tool_output = Some("error: linker `cc` not found".into());
        let (index, f) = index_docs(&[d]);

        let r = search(&index, &f, &SearchRequest::default()).unwrap();
        assert_eq!(r.hits[0].snippet, "error: linker `cc` not found");

        let r = search(&index, &f, &req("linker")).unwrap();
        assert_eq!(r.total, 1);
        assert!(r.hits[0].snippet.contains("**linker**"), "{:?}", r.hits[0]);
    }

    #[test]
    fn phrase_query_is_exact() {
        let (index, f) = index_docs(&corpus());
        let r = search(&index, &f, &req(r#""cargo build""#)).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert!(r.hits[0].doc.text.contains("cargo build --release"));
        // The words exist in two docs, but not adjacent in that order in the second.
        let r = search(&index, &f, &req(r#""build cargo""#)).unwrap();
        assert_eq!(r.total, 0);
    }

    #[test]
    fn malformed_query_degrades_instead_of_failing() {
        let (index, f) = index_docs(&corpus());
        let r = search(&index, &f, &req("tantivy AND (")).unwrap();
        assert!(r.total >= 1, "lenient parse should still find something");
    }

    #[test]
    fn filter_by_tool_name() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();
        r0.filters.tool = vec!["Bash".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 2);

        r0.filters.tool = vec!["Bash".into(), "Read".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 3);

        r0.filters.tool = vec!["Glob".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);
    }

    #[test]
    fn filter_by_tool_input_subpath() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();

        r0.filters.tool_input = vec!["command=cargo".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 2);

        // A multi-word value is a phrase over the subpath, not two loose terms.
        r0.filters.tool_input = vec!["command=cargo test".into()];
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert_eq!(
            r.hits[0].doc.tool_input.as_ref().unwrap()["command"],
            "cargo test"
        );

        // Paths survive their punctuation.
        r0.filters.tool_input = vec!["file_path=/home/user/session-search/src/index.rs".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);

        // Numbers are matched as the typed fast value the JSON field indexed.
        r0.filters.tool_input = vec!["timeout=600000".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);

        // Two --tool-input flags are ANDed.
        r0.filters.tool_input = vec!["command=cargo".into(), "timeout=600000".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);
    }

    /// `tool_input` sits in the default fields, so an unqualified `key:value` falls through
    /// to the JSON field: `command:cargo` works without spelling out `tool_input.`.
    #[test]
    fn an_unqualified_json_path_resolves_against_tool_input() {
        let (index, f) = index_docs(&corpus());
        let r = search(&index, &f, &req("command:cargo")).unwrap();
        assert_eq!(r.total, 2, "{:?}", texts(&r));
        let r = search(&index, &f, &req("file_path:index.rs")).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
    }

    #[test]
    fn tool_input_without_an_equals_is_an_error() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();
        r0.filters.tool_input = vec!["command".into()];
        let err = search(&index, &f, &r0).unwrap_err().to_string();
        assert!(err.contains("KEY=VALUE"), "{err}");
    }

    #[test]
    fn project_filter_matches_by_prefix_and_expands_tilde() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();

        r0.filters.project = Some("/home/user/session-search".into());
        // Everything except the doc in /home/user/other-project.
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 7, "{:?}", texts(&r));

        r0.filters.project = Some("/home/user/session-search/sub".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);

        r0.filters.project = Some("/home/user".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 8);

        r0.filters.project = Some("/home/user/nothing-here".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);

        // A leading `~` becomes $HOME before the prefix is applied.
        unsafe { std::env::set_var("HOME", "/home/user") };
        r0.filters.project = Some("~/session-search".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 7);
    }

    #[test]
    fn session_filter_matches_by_prefix() {
        let mut docs = corpus();
        for d in &mut docs {
            d.session_id = "aaaa1111-2222".into();
        }
        let mut other = blank_doc(9);
        other.session_id = "bbbb3333-4444".into();
        other.doc_id = "bbbb3333-4444:-:9".into();
        other.text = "a turn in a different session".into();
        docs.push(other);
        let (index, f) = index_docs(&docs);

        let mut r0 = SearchRequest::default();
        // The leading block of a uuid is what a user pastes.
        r0.filters.session = Some("aaaa".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 8);
        r0.filters.session = Some("bbbb3333-4444".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);
        r0.filters.session = Some("cccc".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);
    }

    #[test]
    fn a_zero_limit_reports_totals_without_hits() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = req("tantivy");
        r0.limit = 0;
        r0.facets = vec!["role".into()];
        let r = search(&index, &f, &r0).unwrap();
        assert!(r.hits.is_empty(), "{:?}", texts(&r));
        assert_eq!(r.total, 3);
        assert!(!r.facets["role"].values.is_empty());
    }

    #[test]
    fn scalar_filters_are_anded() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();
        r0.filters.kind = Some("tool_call".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 3);

        r0.filters.model = Some("claude-opus-5".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 2);

        r0.filters.role = Some("user".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);

        let mut r1 = SearchRequest::default();
        r1.filters.branch = Some("wip".into());
        assert_eq!(search(&index, &f, &r1).unwrap().total, 1);

        let mut r2 = SearchRequest::default();
        r2.filters.agent_type = Some("Explore".into());
        assert_eq!(search(&index, &f, &r2).unwrap().total, 1);
    }

    /// `agent` is the vendor dimension: filterable as a term and countable as a facet with
    /// no search-side code beyond the field itself.
    #[test]
    fn the_agent_field_filters_and_facets() {
        let (index, f) = index_docs(&corpus());
        let all = corpus().len() as u64;

        let mut r = SearchRequest {
            facets: vec!["agent".into()],
            ..SearchRequest::default()
        };
        r.filters.agent = Some("claude-code".into());
        let found = search(&index, &f, &r).unwrap();
        assert_eq!(found.total as u64, all);
        assert!(found.hits.iter().all(|h| h.doc.agent == "claude-code"));
        let agents = &found.facets["agent"];
        assert_eq!(agents.values.len(), 1);
        assert_eq!(agents.values[0].value, "claude-code");
        assert_eq!(agents.values[0].count, all);

        r.filters.agent = Some("codex".into());
        assert_eq!(search(&index, &f, &r).unwrap().total, 0);
    }

    #[test]
    fn flag_filters() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();
        r0.filters.errors_only = true;
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);

        let mut r1 = SearchRequest::default();
        r1.filters.no_sidechains = true;
        assert_eq!(search(&index, &f, &r1).unwrap().total, 7);

        let mut r2 = SearchRequest::default();
        r2.filters.sidechains_only = true;
        let r = search(&index, &f, &r2).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.hits[0].doc.agent_type.as_deref(), Some("Explore"));
    }

    #[test]
    fn date_range_filters() {
        let (index, f) = index_docs(&corpus());
        // Doc 5 sits at 2024-03-01; every other doc is at 2025-09-04ish.
        let mut r0 = SearchRequest::default();
        r0.filters.until = Some("2024-12-31".into());
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert!(r.hits[0].doc.text.contains("older turn"));

        let mut r1 = SearchRequest::default();
        r1.filters.since = Some("2025-01-01".into());
        assert_eq!(search(&index, &f, &r1).unwrap().total, 7);

        // A bare YYYY-MM-DD `until` covers that whole day.
        let mut r2 = SearchRequest::default();
        r2.filters.since = Some("2024-03-01".into());
        r2.filters.until = Some("2024-03-01".into());
        assert_eq!(search(&index, &f, &r2).unwrap().total, 1);

        // RFC3339 works too, and is exclusive of what it precedes.
        let mut r3 = SearchRequest::default();
        r3.filters.since = Some("2024-03-01T13:00:00Z".into());
        r3.filters.until = Some("2024-03-02T00:00:00Z".into());
        assert_eq!(search(&index, &f, &r3).unwrap().total, 0);
    }

    #[test]
    fn relative_dates_are_understood() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-09T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            parse_when("7d", now).unwrap(),
            When::Instant(
                chrono::DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        assert_eq!(
            parse_when("24h", now).unwrap(),
            parse_when("1d", now).unwrap()
        );
        assert!(matches!(parse_when("2026-09-09", now), Ok(When::Day(_))));
        assert!(matches!(parse_when("now", now), Ok(When::Instant(_))));
        assert!(parse_when("last tuesday", now).is_err());
    }

    #[test]
    fn facets_on_a_plain_fast_field() {
        let (index, f) = index_docs(&corpus());
        let counts = facets(&index, &f, "tool_name", &SearchRequest::default()).unwrap();
        let map: BTreeMap<_, _> = counts
            .values
            .iter()
            .map(|c| (c.value.as_str(), c.count))
            .collect();
        assert_eq!(map.get("Bash"), Some(&2));
        assert_eq!(map.get("Read"), Some(&1));

        let counts = facets(&index, &f, "project", &SearchRequest::default()).unwrap();
        let map: BTreeMap<_, _> = counts
            .values
            .iter()
            .map(|c| (c.value.as_str(), c.count))
            .collect();
        assert_eq!(map.get("/home/user/session-search"), Some(&6));
        assert_eq!(map.get("/home/user/other-project"), Some(&1));
    }

    #[test]
    fn facets_on_a_json_subpath_never_named_in_the_schema() {
        let (index, f) = index_docs(&corpus());
        let counts = facets(&index, &f, "tool_input.command", &SearchRequest::default()).unwrap();
        let map: BTreeMap<_, _> = counts
            .values
            .iter()
            .map(|c| (c.value.as_str(), c.count))
            .collect();
        // Aggregations key on the raw, untokenized value: whole commands, not words.
        assert_eq!(map.get("cargo build --release"), Some(&1));
        assert_eq!(map.get("cargo test"), Some(&1));

        let counts = facets(
            &index,
            &f,
            "tool_input.file_path",
            &SearchRequest::default(),
        )
        .unwrap();
        assert_eq!(counts.values.len(), 1);
        assert_eq!(
            counts.values[0].value,
            "/home/user/session-search/src/index.rs"
        );
    }

    #[test]
    fn facets_respect_the_query_and_filters() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = req("cargo");
        r0.filters.errors_only = true;
        let counts = facets(&index, &f, "tool_input.command", &r0).unwrap();
        assert_eq!(counts.values.len(), 1);
        assert_eq!(counts.values[0].value, "cargo test");
    }

    #[test]
    fn search_populates_requested_facets_in_the_same_pass() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest {
            facets: vec!["tool_name".into(), "tool_input.command".into()],
            ..SearchRequest::default()
        };
        r0.filters.kind = Some("tool_call".into());
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.hits.len(), 3);
        assert_eq!(r.facets.len(), 2);
        assert_eq!(
            r.facets["tool_name"]
                .values
                .iter()
                .map(|c| c.count)
                .sum::<u64>(),
            3
        );
        assert_eq!(r.facets["tool_input.command"].values.len(), 2);
    }

    #[test]
    fn unknown_or_non_fast_facet_fields_are_rejected() {
        let (index, f) = index_docs(&corpus());
        let err = facets(&index, &f, "nope", &SearchRequest::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown facet field"), "{err}");

        // `text` exists but is not a fast field.
        let err = facets(&index, &f, "text", &SearchRequest::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a fast field"), "{err}");
    }

    #[test]
    fn snippets_highlight_the_match_and_fall_back_to_the_head() {
        let (index, f) = index_docs(&corpus());
        let r = search(&index, &f, &req("parsnips")).unwrap();
        assert_eq!(r.total, 0, "thinking is not searched unless opted in");

        // "Compiling" occurs only in a tool *result*, so this also proves the `tool_output`
        // snippet generator: without it the hit would be highlighted on its command line.
        let r = search(&index, &f, &req("Compiling")).unwrap();
        assert_eq!(r.hits.len(), 1);
        assert!(
            r.hits[0].snippet.contains("**Compiling**"),
            "{:?}",
            r.hits[0].snippet
        );

        // No free-text query -> a head-of-text excerpt rather than an empty snippet.
        let mut r0 = SearchRequest::default();
        r0.filters.tool = vec!["Read".into()];
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(
            r.hits[0].snippet,
            "Read /home/user/session-search/src/index.rs"
        );

        // ...capped at `snippet_chars`.
        r0.snippet_chars = 20;
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.hits[0].snippet, "Read /home/user/sess…");
    }

    #[test]
    fn thinking_is_searched_only_when_opted_in() {
        let (index, f) = index_docs(&corpus());
        let r0 = SearchRequest {
            include_thinking: true,
            ..req("parsnips")
        };
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.hits[0].doc.text, "the visible answer");
    }

    #[test]
    fn limit_and_offset_page_through_the_results() {
        let (index, f) = index_docs(&corpus());
        let page = |limit, offset| {
            let r0 = SearchRequest {
                limit,
                offset,
                ..SearchRequest::default()
            };
            search(&index, &f, &r0).unwrap()
        };
        let all = page(100, 0);
        assert_eq!(all.total, 8);
        assert_eq!(all.hits.len(), 8);

        let first = page(3, 0);
        let second = page(3, 3);
        assert_eq!(first.hits.len(), 3);
        assert_eq!(second.hits.len(), 3);
        assert_eq!(second.total, 8, "total counts matches, not the page");
        let ids: Vec<_> = first.hits.iter().map(|h| h.doc.doc_id.clone()).collect();
        assert!(second.hits.iter().all(|h| !ids.contains(&h.doc.doc_id)));
    }

    #[test]
    fn stored_docs_round_trip_every_field() {
        let source = corpus();
        let (index, f) = index_docs(&source);
        let mut r0 = SearchRequest::default();
        r0.filters.tool_input = vec!["command=cargo build --release".into()];
        let r = search(&index, &f, &r0).unwrap();
        let got = &r.hits[0].doc;
        let want = &source[1];
        assert_eq!(got.doc_id, want.doc_id);
        assert_eq!(got.kind, want.kind);
        assert_eq!(got.source_path, want.source_path);
        assert_eq!(got.seq, want.seq);
        assert_eq!(got.session_id, want.session_id);
        assert_eq!(got.uuid, want.uuid);
        assert_eq!(got.timestamp_ms, want.timestamp_ms);
        assert_eq!(got.project, want.project);
        assert_eq!(got.git_branch, want.git_branch);
        assert_eq!(got.role, want.role);
        assert_eq!(got.model, want.model);
        assert_eq!(got.tool_name, want.tool_name);
        assert_eq!(got.tool_use_id, want.tool_use_id);
        assert_eq!(got.tool_input, want.tool_input);
        assert_eq!(got.is_error, want.is_error);
        assert_eq!(got.text, want.text);
        assert_eq!(got.raw, want.raw);
    }

    // -- regressions --------------------------------------------------------

    /// The flags are read back out of the *stored* payload, so they have to be stored. Without
    /// that every hit reported `is_error: false` — contradicting the `--errors-only` filter
    /// that had just selected it.
    #[test]
    fn the_boolean_flags_survive_the_round_trip_into_the_index() {
        let mut docs = corpus();
        docs[3].is_meta = true;
        let (index, f) = index_docs(&docs);

        let mut r0 = SearchRequest::default();
        r0.filters.errors_only = true;
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 1);
        assert!(
            r.hits[0].doc.is_error,
            "the filter said so; the doc must too"
        );
        assert!(r.hits[0].doc.is_meta);

        let mut r1 = SearchRequest::default();
        r1.filters.sidechains_only = true;
        let r = search(&index, &f, &r1).unwrap();
        assert_eq!(r.total, 1);
        assert!(r.hits[0].doc.is_sidechain);

        // …and a document that is none of those still reads as none of those.
        let mut r2 = SearchRequest::default();
        r2.filters.no_sidechains = true;
        r2.filters.kind = Some("message".into());
        let r = search(&index, &f, &r2).unwrap();
        assert!(
            r.hits
                .iter()
                .all(|h| !h.doc.is_sidechain && !h.doc.is_error && !h.doc.is_meta)
        );
    }

    /// `TopDocs` preallocates whatever limit it is handed, so an unclamped user number used to
    /// abort the process (SIGABRT) or panic with `capacity overflow` before reading a document.
    #[test]
    fn an_absurd_limit_is_clamped_rather_than_allocated() {
        let (index, f) = index_docs(&corpus());
        for (limit, offset) in [
            (usize::MAX, 0),
            (1_000_000_000, 0),
            (usize::MAX, usize::MAX),
        ] {
            let r0 = SearchRequest {
                limit,
                offset,
                ..SearchRequest::default()
            };
            let r = search(&index, &f, &r0).unwrap();
            assert_eq!(r.total, 8, "limit {limit} offset {offset}");
            assert!(r.hits.len() <= 8);
        }
        // The same ceiling protects the `seq`-ordered lookups `show`/`--context` use.
        let searcher = index.reader().unwrap().searcher();
        let docs = docs_by_seq(&searcher, &f, &AllQuery, usize::MAX).unwrap();
        assert_eq!(docs.len(), 8);
    }

    /// `-p /home/user/alpha` must not drag in the sibling `/home/user/alpha-beta`.
    #[test]
    fn the_project_filter_stops_at_a_path_boundary() {
        let mut docs = corpus();
        for (i, project) in [
            "/home/user/alpha",
            "/home/user/alpha/sub",
            "/home/user/alpha-beta",
            "/home/user/alphabet",
        ]
        .iter()
        .enumerate()
        {
            docs[i].project = Some((*project).to_string());
            docs[i].doc_id = format!("s1:-:tag:{i}");
        }
        let (index, f) = index_docs(&docs);
        let total = |prefix: &str| {
            let mut r0 = SearchRequest::default();
            r0.filters.project = Some(prefix.to_string());
            search(&index, &f, &r0).unwrap().total
        };
        assert_eq!(
            total("/home/user/alpha"),
            2,
            "the dir and its child, no more"
        );
        assert_eq!(
            total("/home/user/alpha/"),
            2,
            "a trailing slash is the same"
        );
        assert_eq!(total("/home/user/alpha-beta"), 1);
        assert_eq!(
            total("/home/user/alph"),
            0,
            "a partial segment is not a path"
        );

        // The non-index twin, used by `sessions`, agrees.
        assert!(path_has_prefix("/home/user/alpha", "/home/user/alpha"));
        assert!(path_has_prefix("/home/user/alpha/sub", "/home/user/alpha/"));
        assert!(!path_has_prefix(
            "/home/user/alpha-beta",
            "/home/user/alpha"
        ));
        assert!(!path_has_prefix("/home/user/beta", "/home/user/bet"));
    }

    /// `--session` keeps raw prefix semantics: a uuid has no path boundaries.
    #[test]
    fn the_session_filter_is_still_a_bare_prefix() {
        let mut docs = corpus();
        docs[0].session_id = "aaaa1111-2222".into();
        let (index, f) = index_docs(&docs);
        let mut r0 = SearchRequest::default();
        r0.filters.session = Some("aaaa1111".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, 1);
    }

    #[test]
    fn an_empty_tool_input_value_is_an_error_not_an_empty_result() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();
        r0.filters.tool_input = vec!["command=".into()];
        let err = search(&index, &f, &r0).unwrap_err().to_string();
        assert!(err.contains("non-empty value"), "{err}");

        r0.filters.tool_input = vec!["command=   ".into()];
        assert!(search(&index, &f, &r0).is_err());
    }

    /// The one reason to pay for `index --full --include-thinking` is to read the thinking, and
    /// a doc matched only through that field stores nothing in `text`.
    #[test]
    fn a_hit_matched_through_thinking_renders_its_thinking() {
        let (index, f) = index_docs(&corpus());
        let r0 = SearchRequest {
            include_thinking: true,
            ..req("parsnips")
        };
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 1);
        assert!(
            r.hits[0].snippet.contains("**parsnips**"),
            "snippet was {:?}",
            r.hits[0].snippet
        );

        // And a thinking-only doc with no highlight still shows its head rather than nothing.
        let mut docs = corpus();
        docs[7].text = String::new();
        let (index, f) = index_docs(&docs);
        let mut r1 = SearchRequest::default();
        r1.filters.role = Some("assistant".into());
        r1.include_thinking = true;
        let r = search(&index, &f, &r1).unwrap();
        let hit = r.hits.iter().find(|h| h.doc.thinking.is_some()).unwrap();
        assert!(hit.snippet.contains("parsnips"), "{:?}", hit.snippet);
    }

    // -- --program / bash_cmd -----------------------------------------------

    /// Docs parsed out of a fixture and indexed, so the whole chain is under test:
    /// `parse::tool_call_doc` -> `bash::extract` -> `schema::doc_to_json` -> the index.
    fn bash_fixture() -> (tantivy::Index, Fields, Vec<Doc>) {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bash_commands.jsonl");
        let out = crate::agents::claude::parse::parse_whole(
            &fixture,
            &crate::doc::ParseOptions::default(),
        )
        .unwrap();
        let (index, f) = index_docs(&out.docs);
        (index, f, out.docs)
    }

    /// `--program cargo` finds the Bash documents that ran cargo — and nothing else, including
    /// the `Read` call whose `file_path` merely mentions the crate.
    #[test]
    fn program_filter_finds_bash_docs_and_nothing_else() {
        let (index, f, docs) = bash_fixture();

        // Only Bash calls carry `bash_cmd`, and only when the command parsed.
        let with_bash_cmd: Vec<&Doc> = docs.iter().filter(|d| d.bash_cmd.is_some()).collect();
        assert_eq!(with_bash_cmd.len(), 2, "{docs:#?}");
        assert!(
            with_bash_cmd
                .iter()
                .all(|d| d.tool_name.as_deref() == Some("Bash"))
        );
        assert!(
            docs.iter()
                .any(|d| d.tool_name.as_deref() == Some("Read") && d.bash_cmd.is_none()),
            "a non-Bash tool must not get a bash_cmd"
        );
        // `echo 'unterminated` does not parse: no bash_cmd, and no guess either.
        assert!(
            docs.iter()
                .any(|d| d.text.contains("unterminated") && d.bash_cmd.is_none())
        );

        let total = |programs: &[&str]| {
            let mut r0 = SearchRequest::default();
            r0.filters.program = programs.iter().map(|p| (*p).to_string()).collect();
            search(&index, &f, &r0).unwrap()
        };

        let r = total(&["cargo"]);
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert_eq!(r.hits[0].doc.tool_name.as_deref(), Some("Bash"));
        assert_eq!(
            r.hits[0].doc.bash_cmd.as_ref().unwrap()["program"],
            json!(["cargo", "tail"])
        );

        // `git` runs in the second command; `cd` and `tail` are found the same way, because
        // every simple command in the script contributes its program.
        assert_eq!(total(&["git"]).total, 1);
        assert_eq!(total(&["cd"]).total, 1);
        assert_eq!(total(&["tail"]).total, 1);
        // The `Read` call is not a Bash script; `echo` never parsed.
        assert_eq!(total(&["Read"]).total, 0);
        assert_eq!(total(&["echo"]).total, 0);
        assert_eq!(total(&["nosuchprogram"]).total, 0);
    }

    #[test]
    fn repeated_program_flags_are_ored() {
        let (index, f, _) = bash_fixture();
        let total = |programs: &[&str]| {
            let mut r0 = SearchRequest::default();
            r0.filters.program = programs.iter().map(|p| (*p).to_string()).collect();
            search(&index, &f, &r0).unwrap().total
        };
        assert_eq!(total(&["cargo"]), 1);
        assert_eq!(total(&["git"]), 1);
        assert_eq!(total(&["cargo", "git"]), 2, "OR, not AND");
        assert_eq!(total(&["cargo", "nosuchprogram"]), 1);
        // Both programs live in the *same* script, so an OR still yields one document.
        assert_eq!(total(&["git", "cd"]), 1);
        // Empty values are skipped, exactly as `--tool` does.
        assert_eq!(
            total(&["", "   "]),
            5,
            "no clause at all: every doc matches"
        );
        assert_eq!(total(&["", "cargo"]), 1);
    }

    /// `--program` ANDs with everything else, and combines with the corpus' hand-made docs.
    #[test]
    fn program_filter_ands_with_the_other_filters() {
        let (index, f) = index_docs(&corpus());
        let mut r0 = SearchRequest::default();
        r0.filters.program = vec!["cargo".into()];
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 2, "{:?}", texts(&r));

        r0.filters.errors_only = true;
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert!(r.hits[0].doc.text.starts_with("cargo test"));

        // A program that ran, ANDed with a query that does not match it, is still empty.
        let mut r1 = req("parsnips");
        r1.filters.program = vec!["cargo".into()];
        assert_eq!(search(&index, &f, &r1).unwrap().total, 0);
    }

    #[test]
    fn facets_over_bash_cmd_program_bucket_every_command_in_the_script() {
        let (index, f, _) = bash_fixture();
        let counts = facets(&index, &f, "bash_cmd.program", &SearchRequest::default()).unwrap();
        let map: BTreeMap<_, _> = counts
            .values
            .iter()
            .map(|c| (c.value.as_str(), c.count))
            .collect();
        assert_eq!(map.get("cargo"), Some(&1));
        assert_eq!(map.get("tail"), Some(&1));
        assert_eq!(map.get("cd"), Some(&1));
        assert_eq!(map.get("git"), Some(&1));
        assert_eq!(map.len(), 4, "{map:?}");
        // Five documents in the fixture (one prompt, four tool calls); two carry a bash_cmd.
        assert_eq!(counts.matching_docs, 5);
        // `bash_cmd.program` is *multi-valued*: one document lands in one bucket per program
        // it ran, so the bucket counts sum to 4 — more than the two documents that carry the
        // field. `docs_with_value` counts documents, not values, so it stays at 2 and can
        // never exceed `matching_docs`.
        assert_eq!(counts.values.iter().map(|c| c.count).sum::<u64>(), 4);
        assert_eq!(counts.docs_with_value, 2);
        assert!(counts.docs_with_value <= counts.matching_docs);

        // Arguments bucket the same way, whole and unmangled.
        let args = facets(&index, &f, "bash_cmd.args", &SearchRequest::default()).unwrap();
        let args: BTreeMap<_, _> = args
            .values
            .iter()
            .map(|c| (c.value.as_str(), c.count))
            .collect();
        assert_eq!(args.get("--release"), Some(&1), "{args:?}");
        assert_eq!(args.get("--short"), Some(&1));
        assert_eq!(args.get("/tmp/x"), Some(&1));

        // And the other direction: a match set of documents that cannot carry `bash_cmd`
        // reports zero, well *below* `matching_docs` rather than above it.
        let mut messages = SearchRequest::default();
        messages.filters.kind = Some("message".into());
        let counts = facets(&index, &f, "bash_cmd.program", &messages).unwrap();
        assert!(counts.matching_docs > 0);
        assert_eq!(counts.docs_with_value, 0);
    }

    /// The free-text side of the same field: `bash_cmd` is not in the default fields, but a
    /// qualified `field:value` reaches it — and the `raw` tokenizer keeps the value exact.
    #[test]
    fn a_free_text_query_on_bash_cmd_is_exact_not_tokenized() {
        let (index, f, _) = bash_fixture();
        let total = |query: &str| search(&index, &f, &req(query)).unwrap().total;

        // Quoted, because a bare leading `-` is negation in the query grammar.
        assert_eq!(total(r#"bash_cmd.args:"--release""#), 1);
        assert_eq!(total(r#"bash_cmd.args:"--short""#), 1);
        assert_eq!(total("bash_cmd.program:cargo"), 1);
        assert_eq!(total("bash_cmd.program:git"), 1);

        // The raw tokenizer, proven at the query parser: no case folding, no splitting on
        // punctuation, no stripping of leading dashes. If any of these starts hitting, the
        // exactness `--program` promises is gone.
        assert_eq!(total("bash_cmd.program:Cargo"), 0, "case-sensitive");
        assert_eq!(total("bash_cmd.program:CARGO"), 0);
        assert_eq!(
            total("bash_cmd.args:release"),
            0,
            "dashes are part of the term"
        );
        assert_eq!(total("bash_cmd.args:short"), 0);
        assert_eq!(total(r#"bash_cmd.args:"/tmp/x""#), 1);
        assert_eq!(
            total("bash_cmd.args:tmp"),
            0,
            "a path is one term, not three"
        );
        // ...and the filter agrees with the query on case.
        let mut r0 = SearchRequest::default();
        r0.filters.program = vec!["Cargo".into()];
        assert_eq!(search(&index, &f, &r0).unwrap().total, 0);
    }

    /// A `--program` value is quoted and escaped before it reaches the query parser, so a
    /// character the grammar reserves is an empty result rather than a failed search.
    #[test]
    fn an_odd_program_value_is_matched_literally_not_parsed() {
        let (index, f, _) = bash_fixture();
        for value in ["(", "a\"b", "a\\b", "AND", "*", "cargo build"] {
            let mut r0 = SearchRequest::default();
            r0.filters.program = vec![value.to_string()];
            let r = search(&index, &f, &r0);
            assert_eq!(r.unwrap().total, 0, "{value:?} should just not match");
        }
    }

    /// `bash_cmd` has to survive the round trip into the index like `tool_input` does, or
    /// `--json` output and `show` would drop the one field `--program` filtered on.
    #[test]
    fn bash_cmd_round_trips_out_of_the_stored_document() {
        let (index, f, _) = bash_fixture();
        let mut r0 = SearchRequest::default();
        r0.filters.program = vec!["git".into()];
        let r = search(&index, &f, &r0).unwrap();
        let got = r.hits[0].doc.bash_cmd.clone().expect("stored bash_cmd");
        assert_eq!(
            got,
            json!({"program": ["cd", "git"], "args": ["/tmp/x", "status", "--short"]})
        );
        // A document with no bash_cmd reads back as None, not as an empty object.
        let mut r1 = SearchRequest::default();
        r1.filters.tool = vec!["Read".into()];
        let r = search(&index, &f, &r1).unwrap();
        assert!(r.hits[0].doc.bash_cmd.is_none());
    }

    #[test]
    fn regex_escaping_keeps_path_punctuation_literal() {
        assert_eq!(regex_escape("/a+b/c.d"), r"/a\+b/c\.d");
        assert_eq!(regex_escape("/plain/path"), "/plain/path");
    }

    #[test]
    fn excerpt_collapses_whitespace_and_truncates() {
        assert_eq!(excerpt("a\n\n  b   c", 100), "a b c");
        assert_eq!(excerpt("", 100), "");
        let long = "x".repeat(100);
        let cut = excerpt(&long, 16);
        assert_eq!(cut.chars().count(), 17);
        assert!(cut.ends_with('…'));
    }

    /// The whole chain over a redacted slice of a real transcript: parse -> index -> search,
    /// filter, facet. Hand-made docs cannot prove the shapes `parse.rs` actually emits.
    #[test]
    fn end_to_end_over_a_real_transcript_slice() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/real_main_slice.jsonl");
        let out = crate::agents::claude::parse::parse_whole(
            &fixture,
            &crate::doc::ParseOptions::default(),
        )
        .unwrap();
        assert!(out.docs.len() > 5, "fixture should yield real docs");
        let (index, f) = index_docs(&out.docs);

        // Free text reaches the tool calls, because `parse.rs` folds the tool input into `text`.
        let r = search(&index, &f, &req("git")).unwrap();
        assert!(r.total > 0);
        assert!(r.hits.iter().any(|h| h.snippet.contains("**")));

        // The project comes from the record `cwd`, and matches by prefix.
        let mut r0 = SearchRequest::default();
        r0.filters.project = Some("/home/user".into());
        assert_eq!(search(&index, &f, &r0).unwrap().total, out.docs.len());

        // Facets over a declared field and over a parameter key that is not in the schema.
        let tools = facets(&index, &f, "tool_name", &SearchRequest::default()).unwrap();
        assert!(tools.values.iter().any(|c| c.value == "Bash"));
        let commands = facets(&index, &f, "tool_input.command", &SearchRequest::default()).unwrap();
        assert!(
            commands
                .values
                .iter()
                .any(|c| c.value.starts_with("ls -la")),
            "{commands:?}"
        );
    }
}
