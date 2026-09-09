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

use crate::parse::{Doc, DocKind};
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
    /// Of those, the ones that actually carry a value for this field.
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
            facets.insert(
                (*name).to_string(),
                facet_result_from(&as_json, i, name, req.facet_top, total as u64),
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

    // `TopDocs::with_limit(0)` panics, so the collector always asks for at least one doc;
    // an explicit `--limit 0` still means "no hits, just totals and facets".
    let top_hits = if req.limit == 0 { Vec::new() } else { top_hits };

    let mut hits = Vec::with_capacity(top_hits.len());
    for (score, address) in top_hits {
        let stored: TantivyDocument = searcher.doc(address)?;
        let doc = doc_from_stored(f, &stored);
        let snippet = snippets
            .as_ref()
            .map(|g| render_snippet(&g.snippet_from_doc(&stored)))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                thinking_snippets
                    .as_ref()
                    .map(|g| render_snippet(&g.snippet_from_doc(&stored)))
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| {
                let body = match (doc.text.trim().is_empty(), doc.thinking.as_deref()) {
                    (true, Some(thinking)) => thinking,
                    _ => &doc.text,
                };
                excerpt(body, req.snippet_chars)
            });
        hits.push(Hit {
            doc,
            score,
            snippet,
        });
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
    let as_json = serde_json::to_value(agg).context("serializing aggregation result")?;
    Ok(facet_result_from(
        &as_json,
        0,
        field,
        req.facet_top,
        matching as u64,
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
        let mut default_fields = vec![f.text];
        if req.include_thinking {
            default_fields.push(f.thinking);
        }
        default_fields.push(f.tool_input);
        let mut qp = QueryParser::for_index(index, default_fields);
        // Bare multi-word input reads as "all of these words", which is what people mean;
        // explicit `OR` / `AND` / `"phrases"` / `field:value` still work.
        qp.set_conjunction_by_default();
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
    ] {
        if let Some(value) = non_empty(value) {
            clauses.push((Occur::Must, term_query(field, value)));
        }
    }

    if flt.errors_only {
        clauses.push((Occur::Must, flag_query(f.is_error, 1)));
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

/// Assemble the buckets and the counts needed to read them, for facet `i` of a request.
fn facet_result_from(
    result: &Value,
    i: usize,
    field: &str,
    top: usize,
    matching_docs: u64,
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
        docs_with_value: values.iter().map(|f| f.count).sum::<u64>() + other_docs,
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
    let tool_input = stored.get_first(f.tool_input).and_then(|v| {
        serde_json::to_value(OwnedValue::from(v.as_value()))
            .ok()
            .filter(|v| !v.is_null())
    });

    Doc {
        doc_id: s(f.doc_id).unwrap_or_default(),
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
        is_error: flag(f.is_error),
        is_sidechain: flag(f.is_sidechain),
        is_meta: flag(f.is_meta),
        entrypoint: s(f.entrypoint),
        permission_mode: s(f.permission_mode),
        version: s(f.version),
        slug: s(f.slug),
        text: s(f.text).unwrap_or_default(),
        thinking: s(f.thinking),
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
            is_error: false,
            is_sidechain: false,
            is_meta: false,
            entrypoint: Some("cli".into()),
            permission_mode: None,
            version: Some("2.1.266".into()),
            slug: None,
            text: String::new(),
            thinking: None,
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
        d.text = "cargo build --release\nCompiling tantivy".into();
        docs.push(d);

        let mut d = blank_doc(2);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.model = Some("claude-opus-5".into());
        d.tool_name = Some("Read".into());
        d.tool_input = Some(json!({"file_path": "/home/user/session-search/src/index.rs"}));
        d.text = "pub fn open_or_create(index_dir: &Path)".into();
        docs.push(d);

        let mut d = blank_doc(3);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.tool_name = Some("Bash".into());
        d.tool_input = Some(json!({"command": "cargo test"}));
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

    #[test]
    fn phrase_query_is_exact() {
        let (index, f) = index_docs(&corpus());
        let r = search(&index, &f, &req(r#""cargo build""#)).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert!(r.hits[0].doc.text.starts_with("cargo build"));
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
        assert_eq!(r.hits[0].snippet, "pub fn open_or_create(index_dir: &Path)");

        // ...capped at `snippet_chars`.
        r0.snippet_chars = 20;
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.hits[0].snippet, "pub fn open_or_creat…");
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
        let out =
            crate::parse::parse_whole(&fixture, &crate::parse::ParseOptions::default()).unwrap();
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
