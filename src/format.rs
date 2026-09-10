//! Human-readable and JSON rendering.
//!
//! Two output modes share every entry point, switched by [`OutputOpts::json`]:
//!
//! * **human** — grouped, coloured, wrapped to a terminal width. Colour is a caller decision
//!   (`--no-color`, `$NO_COLOR`, "is stdout a tty"); this module only obeys `o.color`.
//! * **json** — exactly one JSON object per invocation, printed on a single line so the
//!   output pipes straight into `jq`. That shape is the contract agents (and the MCP server
//!   that follows) consume, so it is asserted in the tests below.
//!
//! The one deliberate omission from the JSON is [`Doc::raw`]: every document already carries
//! its parsed `text`/`tool_input`, and echoing the original JSONL line as well would double
//! the payload for no new information.
//!
//! Timestamps render in **UTC** — a transcript search is usually read alongside other
//! machines' logs, and a local rendering would not be reproducible.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use chrono::{DateTime, Utc};
use owo_colors::Style;
use serde_json::{Value, json};

use crate::index::IndexStats;
use crate::parse::{Doc, SessionInfo};
use crate::search::{FacetResult, Hit, SearchResponse};

/// The marker `search.rs` wraps matched spans in. Identical on both sides, so splitting on it
/// yields alternating plain/highlighted segments.
const HL: &str = "**";

/// Per-document text budget in the session view. Tool results reach the parser's 32 KiB cap;
/// pasting that into a transcript view buries the conversation.
const MESSAGE_BUDGET: usize = 4_000;
const TOOL_BUDGET: usize = 1_200;
/// Snippet lines shown under a hit. The snippet itself is already capped by `snippet_chars`.
const MAX_SNIPPET_LINES: usize = 4;

#[derive(Debug, Clone)]
pub struct OutputOpts {
    pub json: bool,
    pub color: bool,
    /// How many turns either side of a hit the caller asked for. Informational: the fetched
    /// windows themselves arrive as an argument to [`search_results_ctx`].
    pub context: usize,
    pub width: usize,
}

impl Default for OutputOpts {
    fn default() -> Self {
        OutputOpts {
            json: false,
            color: false,
            context: 0,
            width: 100,
        }
    }
}

impl OutputOpts {
    /// Terminal width, clamped: below ~48 columns wrapping stops helping, and beyond ~160 the
    /// eye loses the line.
    fn cols(&self) -> usize {
        self.width.clamp(48, 160)
    }
}

// ---------------------------------------------------------------------------
// colour
// ---------------------------------------------------------------------------

/// A style applier that collapses to a plain `to_string` when colour is off, so every call
/// site can be written once instead of branching.
#[derive(Clone, Copy)]
struct Ink {
    on: bool,
}

impl Ink {
    fn new(on: bool) -> Self {
        Ink { on }
    }

    fn paint(self, text: &str, style: Style) -> String {
        if self.on {
            style.style(text).to_string()
        } else {
            text.to_string()
        }
    }
}

fn dim() -> Style {
    Style::new().dimmed()
}
fn bold() -> Style {
    Style::new().bold()
}
fn head() -> Style {
    Style::new().bold().cyan()
}
fn tool_style() -> Style {
    Style::new().yellow()
}
fn err_style() -> Style {
    Style::new().red().bold()
}
fn match_style() -> Style {
    Style::new().black().on_yellow()
}
fn bar_style() -> Style {
    Style::new().cyan()
}

fn role_style(role: &str) -> Style {
    match role {
        "user" => Style::new().green().bold(),
        "assistant" => Style::new().magenta().bold(),
        "system" => Style::new().yellow(),
        _ => Style::new().blue(),
    }
}

// ---------------------------------------------------------------------------
// search results
// ---------------------------------------------------------------------------

pub fn search_results(w: &mut impl Write, r: &SearchResponse, o: &OutputOpts) -> Result<()> {
    search_results_ctx(w, r, &[], o)
}

/// [`search_results`] with pre-fetched context turns: `context[i]` is the window around
/// `r.hits[i]`, as returned by `context::around`. A short slice (or an empty one) simply means
/// "no context for those hits", which is what `--context 0` produces.
pub fn search_results_ctx(
    w: &mut impl Write,
    r: &SearchResponse,
    context: &[Vec<Doc>],
    o: &OutputOpts,
) -> Result<()> {
    if o.json {
        return json_line(w, &search_json(r, context));
    }
    human_search(w, r, context, o)
}

fn search_json(r: &SearchResponse, context: &[Vec<Doc>]) -> Value {
    let hits: Vec<Value> = r
        .hits
        .iter()
        .enumerate()
        .map(|(i, hit)| hit_json(hit, context.get(i).map(Vec::as_slice).unwrap_or(&[])))
        .collect();
    json!({
        "total": r.total,
        "count": r.hits.len(),
        "elapsed_ms": r.elapsed_ms,
        "facets": r.facets,
        "hits": hits,
    })
}

fn hit_json(hit: &Hit, context: &[Doc]) -> Value {
    let mut value = doc_json(&hit.doc);
    let object = value
        .as_object_mut()
        .expect("doc_json always builds an object");
    object.insert("score".into(), json!(hit.score));
    object.insert("snippet".into(), json!(hit.snippet));
    if !context.is_empty() {
        object.insert(
            "context".into(),
            Value::Array(context.iter().map(doc_json).collect()),
        );
    }
    value
}

/// Whichever body a document actually has. A tool call with no input, and an orphaned
/// `tool_result`, both carry an empty `text` and everything worth showing in `tool_output`.
///
/// **A failed call shows its result first.** `--errors-only` otherwise retrieves exactly the
/// right documents and previews the command that failed rather than the reason it broke — the
/// error is the answer there, and the input is only context.
fn doc_body(d: &Doc) -> &str {
    let (first, second) = if d.is_error {
        (d.tool_output.as_deref(), Some(d.text.as_str()))
    } else {
        (Some(d.text.as_str()), d.tool_output.as_deref())
    };
    [first, second, d.thinking.as_deref()]
        .into_iter()
        .flatten()
        .find(|b| !b.trim().is_empty())
        .unwrap_or("")
}

/// The stable JSON shape of one document. `raw` is deliberately not included.
pub fn doc_json(d: &Doc) -> Value {
    json!({
        "doc_id": d.doc_id,
        "kind": d.kind.as_str(),
        "seq": d.seq,
        "session_id": d.session_id,
        "agent_id": d.agent_id,
        "agent_type": d.agent_type,
        "uuid": d.uuid,
        "parent_uuid": d.parent_uuid,
        "timestamp": d.timestamp_ms.and_then(rfc3339),
        "timestamp_ms": d.timestamp_ms,
        "project": d.project,
        "git_branch": d.git_branch,
        "role": d.role,
        "model": d.model,
        "tool_name": d.tool_name,
        "tool_use_id": d.tool_use_id,
        "tool_input": d.tool_input,
        "bash_cmd": d.bash_cmd,
        "is_error": d.is_error,
        "is_sidechain": d.is_sidechain,
        "is_meta": d.is_meta,
        "entrypoint": d.entrypoint,
        "permission_mode": d.permission_mode,
        "version": d.version,
        "slug": d.slug,
        "text": d.text,
        "tool_output": d.tool_output,
        "thinking": d.thinking,
        "source_path": d.source_path,
    })
}

fn human_search(
    w: &mut impl Write,
    r: &SearchResponse,
    context: &[Vec<Doc>],
    o: &OutputOpts,
) -> Result<()> {
    let ink = Ink::new(o.color);
    let cols = o.cols();

    // "no matches" is reserved for a query that matched nothing. Zero *rendered* hits with a
    // non-zero total is `--limit 0` (documented as "totals and facets, no hits") or a page past
    // the end — in both cases the total is the only thing the caller asked for.
    if r.total == 0 {
        writeln!(w, "{}", ink.paint("no matches", dim()))?;
    } else {
        let shown = r.hits.len();
        let summary = format!(
            "{shown} of {} hit{} · {} ms",
            r.total,
            if r.total == 1 { "" } else { "s" },
            r.elapsed_ms
        );
        writeln!(w, "{}", ink.paint(&summary, dim()))?;
    }

    for group in group_hits(&r.hits) {
        writeln!(w)?;
        let lead = &r.hits[group.members[0]].doc;
        write_session_header(w, lead, Some(group.members.len()), ink, cols)?;

        for &i in &group.members {
            let hit = &r.hits[i];
            writeln!(w)?;
            write_hit_line(w, hit, i + 1, ink, cols)?;
            let body = if hit.snippet.trim().is_empty() {
                one_line(doc_body(&hit.doc), cols * MAX_SNIPPET_LINES)
            } else {
                one_line(&hit.snippet, usize::MAX)
            };
            for line in wrap(&body, cols.saturating_sub(6))
                .into_iter()
                .take(MAX_SNIPPET_LINES)
            {
                writeln!(w, "      {}", highlight(&line, ink))?;
            }
            if let Some(around) = context.get(i) {
                write_context(w, around, hit.doc.seq, ink, cols)?;
            }
        }
    }

    if !r.facets.is_empty() {
        for facet in r.facets.values() {
            writeln!(w)?;
            facet_list(w, facet, o)?;
        }
    }
    Ok(())
}

/// Hits sharing a session, in order of first appearance. Ranking interleaves sessions, so a
/// naive "start a group when the key changes" would shatter one session into several blocks.
struct Group {
    members: Vec<usize>,
}

fn group_hits(hits: &[Hit]) -> Vec<Group> {
    let mut order: Vec<(String, Group)> = Vec::new();
    for (i, hit) in hits.iter().enumerate() {
        let key = format!(
            "{}:{}",
            hit.doc.session_id,
            hit.doc.agent_id.as_deref().unwrap_or("-")
        );
        match order.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.members.push(i),
            None => order.push((key, Group { members: vec![i] })),
        }
    }
    order.into_iter().map(|(_, g)| g).collect()
}

fn write_session_header(
    w: &mut impl Write,
    doc: &Doc,
    hits: Option<usize>,
    ink: Ink,
    cols: usize,
) -> Result<()> {
    let mut title = doc.session_id.clone();
    if let Some(slug) = doc.slug.as_deref().filter(|s| !s.is_empty()) {
        title.push_str("  ");
        title.push_str(slug);
    }
    let mut line = format!("▌ {}", ink.paint(&title, head()));
    if let Some(agent) = doc.agent_id.as_deref() {
        let label = match doc.agent_type.as_deref() {
            Some(kind) if !kind.is_empty() => format!("  agent {agent} · {kind}"),
            _ => format!("  agent {agent}"),
        };
        line.push_str(&ink.paint(&label, tool_style()));
    }
    if let Some(n) = hits {
        line.push_str(&ink.paint(
            &format!("  ({n} hit{})", if n == 1 { "" } else { "s" }),
            dim(),
        ));
    }
    writeln!(w, "{line}")?;

    let mut meta: Vec<String> = Vec::new();
    if let Some(project) = doc.project.as_deref().filter(|p| !p.is_empty()) {
        meta.push(project.to_string());
    }
    if let Some(branch) = doc.git_branch.as_deref().filter(|b| !b.is_empty()) {
        meta.push(branch.to_string());
    }
    if let Some(when) = doc.timestamp_ms.and_then(stamp) {
        meta.push(when);
    }
    if !meta.is_empty() {
        let joined = truncate(&meta.join(" · "), cols.saturating_sub(2));
        writeln!(w, "▌ {}", ink.paint(&joined, dim()))?;
    }
    Ok(())
}

fn write_hit_line(w: &mut impl Write, hit: &Hit, rank: usize, ink: Ink, cols: usize) -> Result<()> {
    let doc = &hit.doc;
    let mut line = String::new();
    line.push_str(&ink.paint(&format!("{rank:>4}."), dim()));
    line.push(' ');
    line.push_str(&ink.paint(&clock(doc.timestamp_ms), dim()));
    line.push_str("  ");
    line.push_str(&ink.paint(&format!("{:<9}", doc.role), role_style(&doc.role)));

    let mut used = 4 + 1 + 8 + 2 + 9;
    if let Some(tool) = doc.tool_name.as_deref().filter(|t| !t.is_empty()) {
        line.push(' ');
        line.push_str(&ink.paint(tool, tool_style()));
        used += 1 + tool.chars().count();
    }
    if doc.is_error {
        line.push(' ');
        line.push_str(&ink.paint("error", err_style()));
        used += 6;
    }

    let tail = format!("  #{}  {:.2}", doc.seq, hit.score);
    let room = cols.saturating_sub(used + tail.chars().count());
    if let Some(params) = doc.tool_input.as_ref().map(|v| tool_params(v, room))
        && !params.is_empty()
    {
        line.push(' ');
        line.push_str(&ink.paint(&params, dim()));
    }
    line.push_str(&ink.paint(&tail, dim()));
    writeln!(w, "{line}")?;
    Ok(())
}

fn write_context(
    w: &mut impl Write,
    around: &[Doc],
    seq: u64,
    ink: Ink,
    cols: usize,
) -> Result<()> {
    for doc in around {
        if doc.seq == seq {
            continue;
        }
        let label = match doc.tool_name.as_deref() {
            Some(tool) if !tool.is_empty() => format!("{} {tool}", doc.role),
            _ => doc.role.clone(),
        };
        let prefix = format!("      #{:<5} {:<20} ", doc.seq, truncate(&label, 20));
        let body = one_line(doc_body(doc), cols.saturating_sub(prefix.chars().count()));
        writeln!(w, "{}", ink.paint(&format!("{prefix}{body}"), dim()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// facets
// ---------------------------------------------------------------------------

/// `total` — human and JSON alike — is the sum of the buckets *listed here*, not the number of
/// documents the query matched: a terms aggregation is truncated to `--top N`, so a field with
/// more values than that has documents in buckets nobody asked for. The label says "listed" so
/// the number is not read as a corpus total.
pub fn facet_list(w: &mut impl Write, r: &FacetResult, o: &OutputOpts) -> Result<()> {
    let field = r.field.as_str();
    let c = &r.values;
    if o.json {
        return json_line(w, &json!(r));
    }

    let ink = Ink::new(o.color);
    let cols = o.cols();
    // Say what is shown out of what exists. Summing the visible rows and calling it the total
    // is how a 618-value field reads as an 11-document one.
    let shown = match r.distinct {
        Some(distinct) if distinct > c.len() as u64 => {
            format!("showing {} of ~{distinct} values", c.len())
        }
        _ => format!("{} value{}", c.len(), if c.len() == 1 { "" } else { "s" }),
    };
    writeln!(
        w,
        "{}  {}",
        ink.paint(field, bold()),
        ink.paint(
            &format!(
                "{shown} · {} of {} matching doc{} have a value",
                r.docs_with_value,
                r.matching_docs,
                if r.matching_docs == 1 { "" } else { "s" }
            ),
            dim()
        )
    )?;
    if c.is_empty() {
        writeln!(w, "  {}", ink.paint("(none)", dim()))?;
        return Ok(());
    }

    let max = c.iter().map(|f| f.count).max().unwrap_or(1).max(1);
    let count_w = c
        .iter()
        .map(|f| f.count.to_string().len())
        .max()
        .unwrap_or(1);
    let label_w = c
        .iter()
        .map(|f| display_len(&f.value).min(48))
        .max()
        .unwrap_or(1);
    // 2 indent + label + 2 + count + 2 = fixed cost of a row.
    let bar_w = cols.saturating_sub(label_w + count_w + 6).min(40);

    for f in c {
        let label = truncate(&one_line(&f.value, usize::MAX), label_w);
        let pad = label_w.saturating_sub(display_len(&label));
        let filled = if bar_w == 0 {
            0
        } else {
            let scaled = (f.count as f64 / max as f64 * bar_w as f64).round() as usize;
            scaled.clamp(usize::from(f.count > 0), bar_w)
        };
        writeln!(
            w,
            "  {}{}  {}  {}",
            label,
            " ".repeat(pad),
            ink.paint(&format!("{:>count_w$}", f.count), bold()),
            ink.paint(&"█".repeat(filled), bar_style()),
        )?;
    }

    // A field whose values barely repeat has no distribution to show. Say so, and point at the
    // thing that does work — `tool_input` is full-text indexed, so the values are searchable
    // even when they are useless as buckets.
    if r.is_search_shaped() {
        let distinct = r.distinct.unwrap_or_default();
        writeln!(
            w,
            "  {}",
            ink.paint(
                &format!(
                    "note: ~{distinct} distinct values across {} docs — this field is \
                     search-shaped, not facet-shaped.",
                    r.docs_with_value
                ),
                dim()
            )
        )?;
        writeln!(
            w,
            "  {}",
            ink.paint(
                &format!("      try:  search '{}:\"<text>\"'", r.field),
                dim()
            )
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// session view / session list
// ---------------------------------------------------------------------------

pub fn session_view(w: &mut impl Write, docs: &[Doc], o: &OutputOpts) -> Result<()> {
    if o.json {
        return json_line(
            w,
            &json!({
                "count": docs.len(),
                "docs": docs.iter().map(doc_json).collect::<Vec<_>>(),
            }),
        );
    }

    let ink = Ink::new(o.color);
    let cols = o.cols();
    let Some(lead) = docs.first() else {
        writeln!(w, "{}", ink.paint("no documents", dim()))?;
        return Ok(());
    };
    write_session_header(w, lead, None, ink, cols)?;

    for doc in docs {
        writeln!(w)?;
        let mut line = format!(
            "{} {}  {}",
            ink.paint(&format!("#{:<5}", doc.seq), dim()),
            ink.paint(&clock(doc.timestamp_ms), dim()),
            ink.paint(&format!("{:<9}", doc.role), role_style(&doc.role)),
        );
        if let Some(tool) = doc.tool_name.as_deref().filter(|t| !t.is_empty()) {
            line.push(' ');
            line.push_str(&ink.paint(tool, tool_style()));
        }
        if doc.is_error {
            line.push(' ');
            line.push_str(&ink.paint("error", err_style()));
        }
        if let Some(model) = doc.model.as_deref().filter(|m| !m.is_empty()) {
            line.push_str(&ink.paint(&format!("  {model}"), dim()));
        }
        writeln!(w, "{line}")?;

        if let Some(input) = doc.tool_input.as_ref() {
            let params = tool_params_multiline(input, cols.saturating_sub(8));
            for p in params {
                writeln!(w, "      {}", ink.paint(&p, dim()))?;
            }
        }

        let budget = if doc.tool_name.is_some() {
            TOOL_BUDGET
        } else {
            MESSAGE_BUDGET
        };
        for line in wrap(&clip(&doc.text, budget), cols.saturating_sub(6)) {
            writeln!(w, "      {line}")?;
        }
        // The result gets its own budget and its own indent: on a tool call the interesting
        // half is usually what came back, and it must not be crowded out by a `Write` payload.
        if let Some(output) = doc.tool_output.as_deref().filter(|o| !o.trim().is_empty()) {
            for line in wrap(&clip(output, TOOL_BUDGET), cols.saturating_sub(8)) {
                writeln!(w, "        {line}")?;
            }
        }
        if let Some(thinking) = doc.thinking.as_deref().filter(|t| !t.trim().is_empty()) {
            for line in wrap(&clip(thinking, MESSAGE_BUDGET), cols.saturating_sub(8)) {
                writeln!(w, "      {}", ink.paint(&line, dim()))?;
            }
        }
    }
    Ok(())
}

pub fn session_list(w: &mut impl Write, s: &[SessionInfo], o: &OutputOpts) -> Result<()> {
    if o.json {
        let sessions: Vec<Value> = s.iter().map(session_json).collect();
        return json_line(w, &json!({ "count": s.len(), "sessions": sessions }));
    }

    let ink = Ink::new(o.color);
    let cols = o.cols();
    if s.is_empty() {
        writeln!(w, "{}", ink.paint("no sessions indexed", dim()))?;
        return Ok(());
    }

    for info in s {
        let mut line = ink.paint(&info.session_id, head());
        if let Some(slug) = info.slug.as_deref().filter(|s| !s.is_empty()) {
            line.push_str(&format!("  {slug}"));
        }
        if let Some(agent) = info.agent_id.as_deref() {
            let label = match info.agent_type.as_deref() {
                Some(kind) if !kind.is_empty() => format!("  agent {agent} · {kind}"),
                _ => format!("  agent {agent}"),
            };
            line.push_str(&ink.paint(&label, tool_style()));
        }
        writeln!(w, "{line}")?;

        let mut meta = vec![format!(
            "{} msg · {} tool{}",
            info.messages,
            info.tool_calls,
            if info.tool_calls == 1 { "" } else { "s" }
        )];
        if let Some(when) = info.last_ts_ms.or(info.first_ts_ms).and_then(stamp) {
            meta.insert(0, when);
        }
        if let Some(project) = info.project.as_deref().filter(|p| !p.is_empty()) {
            meta.push(project.to_string());
        }
        if let Some(branch) = info.git_branch.as_deref().filter(|b| !b.is_empty()) {
            meta.push(branch.to_string());
        }
        writeln!(
            w,
            "  {}",
            ink.paint(&truncate(&meta.join(" · "), cols.saturating_sub(2)), dim())
        )?;

        let headline = info
            .title
            .as_deref()
            .or(info.description.as_deref())
            .or(info.first_prompt.as_deref())
            .unwrap_or("");
        if !headline.trim().is_empty() {
            writeln!(
                w,
                "  {}",
                one_line(headline, cols.saturating_sub(2)).as_str()
            )?;
        }
        writeln!(w)?;
    }
    Ok(())
}

fn session_json(info: &SessionInfo) -> Value {
    json!({
        "key": match info.agent_id.as_deref() {
            Some(agent) => format!("{}:{agent}", info.session_id),
            None => info.session_id.clone(),
        },
        "session_id": info.session_id,
        "agent_id": info.agent_id,
        "agent_type": info.agent_type,
        "description": info.description,
        "title": info.title,
        "slug": info.slug,
        "project": info.project,
        "git_branch": info.git_branch,
        "source_path": info.source_path,
        "first_ts": info.first_ts_ms.and_then(rfc3339),
        "first_ts_ms": info.first_ts_ms,
        "last_ts": info.last_ts_ms.and_then(rfc3339),
        "last_ts_ms": info.last_ts_ms,
        "messages": info.messages,
        "tool_calls": info.tool_calls,
        "first_prompt": info.first_prompt,
    })
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

pub fn stats(w: &mut impl Write, s: &IndexStats, o: &OutputOpts) -> Result<()> {
    stats_scoped(w, s, &[], o)
}

/// [`stats`] that also names the corpus the index is bound to. Worth printing: a count is
/// meaningless without knowing what it counted over, and an index silently holding a corpus
/// the caller did not expect is exactly the failure this guards against.
pub fn stats_scoped(
    w: &mut impl Write,
    s: &IndexStats,
    roots: &[std::path::PathBuf],
    o: &OutputOpts,
) -> Result<()> {
    if o.json {
        let mut value = serde_json::to_value(s)?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "roots".into(),
                json!(
                    roots
                        .iter()
                        .map(|r| r.display().to_string())
                        .collect::<Vec<_>>()
                ),
            );
        }
        return json_line(w, &value);
    }
    let ink = Ink::new(o.color);
    if !roots.is_empty() {
        for (i, root) in roots.iter().enumerate() {
            writeln!(
                w,
                "  {}  {}",
                ink.paint(if i == 0 { "corpus" } else { "      " }, dim()),
                ink.paint(&root.display().to_string(), bold()),
            )?;
        }
    }
    let rows: [(&str, String); 8] = [
        ("files scanned", thousands(s.files_scanned as u64)),
        ("files updated", thousands(s.files_updated as u64)),
        ("files reset", thousands(s.files_reset as u64)),
        ("documents added", thousands(s.docs_added)),
        ("documents deleted", thousands(s.docs_deleted)),
        ("sessions", thousands(s.sessions as u64)),
        ("parse errors", thousands(s.parse_errors)),
        ("elapsed", format!("{} ms", thousands(s.elapsed_ms as u64))),
    ];
    let label_w = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    let value_w = rows.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
    for (label, value) in rows {
        let style = if label == "parse errors" && s.parse_errors > 0 {
            err_style()
        } else {
            bold()
        };
        writeln!(
            w,
            "  {}  {}",
            ink.paint(&format!("{label:<label_w$}"), dim()),
            ink.paint(&format!("{value:>value_w$}"), style),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// One JSON object, one line, newline-terminated — `jq` and `while read` both cope.
pub fn json_line(w: &mut impl Write, value: &Value) -> Result<()> {
    writeln!(w, "{}", serde_json::to_string(value)?)?;
    Ok(())
}

fn rfc3339(ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339())
}

fn stamp(ms: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
}

fn clock(ms: Option<i64>) -> String {
    ms.and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Visible width, ignoring the highlight markers that `search.rs` embeds.
fn display_len(s: &str) -> usize {
    s.chars().count() - s.matches(HL).count() * HL.len()
}

/// Replace the control characters that indexed transcript content is full of — the raw stdout
/// of `ls --color`, `cargo`, `git` — with a visible stand-in.
///
/// Left alone they reach the terminal verbatim even with `--no-color` and through a pipe, so a
/// search result could repaint or erase this program's own output (`ESC[2K`, `ESC[A`), set the
/// window title, or plant an OSC 8 hyperlink. They also break the layout, because
/// [`display_len`] counts them as visible columns. Ordinary whitespace is excluded: the
/// callers collapse or wrap on it.
fn sanitize_controls(s: &str) -> String {
    if !s.chars().any(is_control_char) {
        return s.to_string();
    }
    s.chars()
        .map(|c| if is_control_char(c) { '\u{fffd}' } else { c })
        .collect()
}

fn is_control_char(c: char) -> bool {
    !c.is_whitespace() && (c.is_control() || ('\u{80}'..='\u{9f}').contains(&c))
}

/// Whitespace collapsed onto one line, control characters neutralised, then truncated to `max`
/// visible characters.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&sanitize_controls(&flat), max)
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_len(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out = String::new();
    let mut width = 0;
    let mut markers = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        // Markers are zero-width; they are copied through so the highlight survives the cut.
        if c == '*' && chars.peek() == Some(&'*') {
            chars.next();
            out.push_str(HL);
            markers += 1;
            continue;
        }
        if width >= keep {
            break;
        }
        out.push(c);
        width += 1;
    }
    // Cutting mid-highlight would leave an unbalanced marker, which renders as a stray `**`.
    if !markers.is_multiple_of(2) {
        out.push_str(HL);
    }
    out.push('…');
    out
}

/// Hard character cap with a note, for bodies rather than single-line labels.
fn clip(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("… (+{} chars)", thousands((count - max) as u64)));
    out
}

/// Word wrap that keeps existing newlines and never counts highlight markers.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(16);
    let text = sanitize_controls(text);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in raw.split_whitespace() {
            let w = display_len(word);
            if used > 0 && used + 1 + w > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            if w > width {
                // A single unbreakable token (a path, a base64 blob) is truncated rather than
                // allowed to blow the layout apart.
                let cut = truncate(word, width);
                used += display_len(&cut);
                line.push_str(&cut);
            } else {
                line.push_str(word);
                used += w;
            }
        }
        if !line.is_empty() || raw.trim().is_empty() {
            out.push(line);
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// Turn `**matched**` runs into colour. With colour off the markers are left in place: they
/// are the only signal of *why* a line matched, and they survive a pipe.
fn highlight(s: &str, ink: Ink) -> String {
    if !ink.on || !s.contains(HL) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for (i, part) in s.split(HL).enumerate() {
        if i.is_multiple_of(2) {
            out.push_str(part);
        } else {
            out.push_str(&ink.paint(part, match_style()));
        }
    }
    out
}

/// Parameters that identify a tool call at a glance, most identifying first. Anything not on
/// the list still shows up (in key order) once the interesting ones are exhausted.
const KEY_PARAMS: [&str; 12] = [
    "command",
    "file_path",
    "path",
    "pattern",
    "query",
    "url",
    "notebook_path",
    "prompt",
    "description",
    "subagent_type",
    "old_string",
    "content",
];

fn tool_params(input: &Value, room: usize) -> String {
    if room < 8 {
        return String::new();
    }
    let pairs = param_pairs(input, 3);
    if pairs.is_empty() {
        return String::new();
    }
    truncate(&pairs.join("  "), room)
}

fn tool_params_multiline(input: &Value, width: usize) -> Vec<String> {
    param_pairs(input, 6)
        .into_iter()
        .map(|p| truncate(&p, width))
        .collect()
}

fn param_pairs(input: &Value, max: usize) -> Vec<String> {
    let Some(object) = input.as_object() else {
        // A non-object input (rare, but tolerated everywhere else) still deserves a rendering.
        return match input {
            Value::Null => Vec::new(),
            other => vec![one_line(&scalar(other), 120)],
        };
    };
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for key in KEY_PARAMS {
        if out.len() >= max {
            break;
        }
        if let Some(value) = object.get(key) {
            seen.push(key);
            out.push(format!("{key}={}", one_line(&scalar(value), 120)));
        }
    }
    for (key, value) in object {
        if out.len() >= max {
            break;
        }
        if seen.contains(&key.as_str()) {
            continue;
        }
        out.push(format!("{key}={}", one_line(&scalar(value), 80)));
    }
    out
}

/// A compact one-line rendering of any JSON value.
fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!("[{} items]", a.len()),
        Value::Object(o) => format!("{{{} keys}}", o.len()),
    }
}

/// Facet counts as a map, for callers that already hold a `BTreeMap` shape.
pub fn facets_json(facets: &BTreeMap<String, FacetResult>) -> Value {
    json!(facets)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::search::FacetCount;

    /// Wrap bare bucket counts as a `FacetResult`, as if every matching doc carried a value and
    /// nothing was truncated. Tests that care about truncation or cardinality build their own.
    fn fr(field: &str, values: Vec<FacetCount>) -> FacetResult {
        let docs: u64 = values.iter().map(|f| f.count).sum();
        FacetResult {
            field: field.to_string(),
            distinct: Some(values.len() as u64),
            matching_docs: docs,
            docs_with_value: docs,
            other_docs: 0,
            values,
        }
    }
    use crate::parse::DocKind;

    /// 2026-09-09T19:07:19Z — fixed so every rendering assertion is reproducible.
    const T0: i64 = 1_788_980_839_000;

    fn doc(seq: u64, role: &str, text: &str) -> Doc {
        Doc {
            doc_id: format!("b20208d8:-:{seq}"),
            kind: DocKind::Message,
            source_path: "/home/u/.claude/projects/p/b20208d8.jsonl".into(),
            seq,
            session_id: "b20208d8-fbdb-5918-ba69-d203de6ed6dc".into(),
            agent_id: None,
            agent_type: None,
            uuid: Some(format!("uuid-{seq}")),
            parent_uuid: None,
            timestamp_ms: Some(T0 + seq as i64 * 1000),
            project: Some("/home/user/session-search".into()),
            git_branch: Some("claude/rust-mcp".into()),
            role: role.into(),
            model: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            bash_cmd: None,
            is_error: false,
            is_sidechain: false,
            is_meta: false,
            entrypoint: Some("remote_mobile".into()),
            permission_mode: None,
            version: Some("2.1.266".into()),
            slug: Some("wild-spinning-puppy".into()),
            text: text.into(),
            tool_output: None,
            thinking: None,
            thinking_tokens: None,
            raw: "{\"type\":\"user\"}".into(),
        }
    }

    fn tool_doc(seq: u64, tool: &str, input: Value, text: &str) -> Doc {
        // Filled the way `parse::tool_call_doc` fills it, so the JSON rendering is exercised
        // with the shape the index actually holds.
        let bash_cmd = (tool == "Bash")
            .then(|| {
                input
                    .get("command")
                    .and_then(Value::as_str)
                    .and_then(crate::bash::extract)
                    .map(|c| c.to_json())
            })
            .flatten();
        Doc {
            kind: DocKind::ToolCall,
            role: "assistant".into(),
            tool_name: Some(tool.into()),
            tool_use_id: Some(format!("toolu_{seq}")),
            tool_input: Some(input),
            bash_cmd,
            model: Some("claude-opus-5".into()),
            ..doc(seq, "assistant", text)
        }
    }

    fn hit(doc: Doc, score: f32, snippet: &str) -> Hit {
        Hit {
            doc,
            score,
            snippet: snippet.into(),
        }
    }

    fn response(hits: Vec<Hit>) -> SearchResponse {
        SearchResponse {
            total: hits.len(),
            hits,
            facets: BTreeMap::new(),
            elapsed_ms: 7,
        }
    }

    fn render(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).expect("rendering must not fail");
        String::from_utf8(buf).expect("rendering must be utf-8")
    }

    fn plain() -> OutputOpts {
        OutputOpts::default()
    }

    fn json_opts() -> OutputOpts {
        OutputOpts {
            json: true,
            ..OutputOpts::default()
        }
    }

    fn sample() -> SearchResponse {
        let mut r = response(vec![
            hit(
                tool_doc(
                    41,
                    "Bash",
                    json!({"command": "cargo build --release", "description": "build"}),
                    "Compiling session-search v0.1.0",
                ),
                4.25,
                "Compiling **session**-**search** v0.1.0",
            ),
            hit(
                doc(12, "user", "index the transcripts with tantivy"),
                2.5,
                "index the **transcripts** with tantivy",
            ),
        ]);
        r.total = 42;
        r.facets.insert(
            "tool_name".into(),
            fr(
                "tool_name",
                vec![
                    FacetCount {
                        value: "Bash".into(),
                        count: 64,
                    },
                    FacetCount {
                        value: "Read".into(),
                        count: 8,
                    },
                ],
            ),
        );
        r
    }

    // -- json shape ---------------------------------------------------------

    #[test]
    fn search_json_is_one_object_on_one_line() {
        let out = render(|w| search_results(w, &sample(), &json_opts()));
        assert_eq!(out.lines().count(), 1, "json output must be a single line");
        let v: Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["total"], 42);
        assert_eq!(v["count"], 2);
        assert_eq!(v["elapsed_ms"], 7);
        assert_eq!(v["facets"]["tool_name"]["values"][0]["value"], "Bash");
        assert_eq!(v["facets"]["tool_name"]["values"][0]["count"], 64);
        assert_eq!(v["facets"]["tool_name"]["matching_docs"], 72);
        assert_eq!(v["hits"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn hit_json_shape_is_stable() {
        let out = render(|w| search_results(w, &sample(), &json_opts()));
        let v: Value = serde_json::from_str(&out).unwrap();
        let hit = &v["hits"][0];

        // Every key an agent may depend on, present even when the value is null.
        for key in [
            "doc_id",
            "kind",
            "seq",
            "session_id",
            "agent_id",
            "agent_type",
            "uuid",
            "parent_uuid",
            "timestamp",
            "timestamp_ms",
            "project",
            "git_branch",
            "role",
            "model",
            "tool_name",
            "tool_use_id",
            "tool_input",
            "bash_cmd",
            "is_error",
            "is_sidechain",
            "is_meta",
            "entrypoint",
            "permission_mode",
            "version",
            "slug",
            "text",
            "tool_output",
            "thinking",
            "source_path",
            "score",
            "snippet",
        ] {
            assert!(
                hit.get(key).is_some(),
                "hit json lost the {key:?} key: {hit}"
            );
        }
        assert!(
            hit.get("raw").is_none(),
            "the original JSONL line must not be echoed back"
        );
        assert!(
            hit.get("context").is_none(),
            "context appears only when --context asked for it"
        );

        assert_eq!(hit["kind"], "tool_call", "kind matches the --kind values");
        assert_eq!(hit["seq"], 41);
        assert_eq!(hit["timestamp"], "2026-09-09T19:08:00+00:00");
        assert_eq!(hit["timestamp_ms"], T0 + 41_000);
        assert_eq!(hit["tool_name"], "Bash");
        assert_eq!(hit["tool_input"]["command"], "cargo build --release");
        // The structured view rides along beside the raw input, verbatim.
        assert_eq!(hit["bash_cmd"]["program"], json!(["cargo"]));
        assert_eq!(hit["bash_cmd"]["args"], json!(["build", "--release"]));
        assert_eq!(hit["agent_id"], Value::Null);
        assert_eq!(hit["is_error"], false);
        assert_eq!(v["hits"][1]["kind"], "message");
    }

    #[test]
    fn context_rides_along_in_json_when_present() {
        let r = sample();
        let around = vec![vec![
            doc(40, "user", "before"),
            doc(42, "assistant", "after"),
        ]];
        let out = render(|w| search_results_ctx(w, &r, &around, &json_opts()));
        let v: Value = serde_json::from_str(&out).unwrap();
        let context = v["hits"][0]["context"].as_array().expect("context array");
        assert_eq!(context.len(), 2);
        assert_eq!(context[0]["seq"], 40);
        assert_eq!(context[1]["role"], "assistant");
        // The second hit had no window fetched.
        assert!(v["hits"][1].get("context").is_none());
    }

    #[test]
    fn facet_json_carries_the_field_and_the_total() {
        let counts = [
            FacetCount {
                value: "Bash".into(),
                count: 64,
            },
            FacetCount {
                value: "Read".into(),
                count: 8,
            },
        ];
        let out = render(|w| facet_list(w, &fr("tool_name", counts.to_vec()), &json_opts()));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["field"], "tool_name");
        assert_eq!(v["values"][1]["value"], "Read");
        // The counts a caller needs to read the buckets honestly, rather than a `total` that
        // is really just the sum of the visible rows.
        assert_eq!(v["matching_docs"], 72);
        assert_eq!(v["docs_with_value"], 72);
        assert_eq!(v["other_docs"], 0);
        assert_eq!(v["distinct"], 2);
    }

    #[test]
    fn session_and_stats_json_shapes() {
        let info = SessionInfo {
            session_id: "b20208d8".into(),
            agent_id: Some("a10845c5ff9c7d4ec".into()),
            agent_type: Some("Explore".into()),
            description: Some("Characterize transcript format".into()),
            title: None,
            slug: Some("wild-spinning-puppy".into()),
            project: Some("/home/user/session-search".into()),
            git_branch: Some("main".into()),
            source_path: "/tmp/agent-a10845c5ff9c7d4ec.jsonl".into(),
            first_ts_ms: Some(T0),
            last_ts_ms: Some(T0 + 60_000),
            messages: 49,
            tool_calls: 39,
            first_prompt: Some("characterize the format".into()),
        };
        let out = render(|w| session_list(w, std::slice::from_ref(&info), &json_opts()));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["count"], 1);
        let s = &v["sessions"][0];
        assert_eq!(s["key"], "b20208d8:a10845c5ff9c7d4ec");
        assert_eq!(s["messages"], 49);
        assert_eq!(s["first_ts"], "2026-09-09T19:07:19+00:00");
        assert_eq!(s["agent_type"], "Explore");

        let stats_json = render(|w| {
            stats(
                w,
                &IndexStats {
                    files_scanned: 4,
                    docs_added: 220,
                    sessions: 4,
                    ..IndexStats::default()
                },
                &json_opts(),
            )
        });
        let v: Value = serde_json::from_str(&stats_json).unwrap();
        assert_eq!(v["files_scanned"], 4);
        assert_eq!(v["docs_added"], 220);
        assert_eq!(v["parse_errors"], 0);

        let view = render(|w| session_view(w, &[doc(1, "user", "hello")], &json_opts()));
        let v: Value = serde_json::from_str(&view).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["docs"][0]["text"], "hello");
    }

    // -- human rendering ----------------------------------------------------

    #[test]
    fn human_search_shows_session_hit_and_facet_detail() {
        let out = render(|w| search_results(w, &sample(), &plain()));

        assert!(out.contains("2 of 42 hits · 7 ms"), "{out}");
        // Session header: full id (so `show` is copy-pasteable), slug, project, branch.
        assert!(
            out.contains("b20208d8-fbdb-5918-ba69-d203de6ed6dc"),
            "{out}"
        );
        assert!(out.contains("wild-spinning-puppy"), "{out}");
        assert!(
            out.contains("/home/user/session-search · claude/rust-mcp"),
            "{out}"
        );
        assert!(out.contains("(2 hits)"), "{out}");
        // Hit line: rank, clock, role, tool, compact params, seq handle, score.
        assert!(out.contains("19:08:00"), "{out}");
        assert!(out.contains("assistant"), "{out}");
        assert!(out.contains("Bash"), "{out}");
        assert!(out.contains("command=cargo build --release"), "{out}");
        assert!(out.contains("#41"), "{out}");
        assert!(out.contains("4.25"), "{out}");
        // Snippet, with the match markers preserved when colour is off.
        assert!(out.contains("**session**-**search**"), "{out}");
        // Facets follow the hits.
        assert!(out.contains("tool_name"), "{out}");
        assert!(out.contains("█"), "{out}");
    }

    #[test]
    fn hits_are_grouped_by_session_in_first_appearance_order() {
        let other = |seq: u64| Doc {
            session_id: "99999999-aaaa".into(),
            slug: None,
            ..doc(seq, "user", "elsewhere")
        };
        let r = response(vec![
            hit(doc(1, "user", "one"), 3.0, "one"),
            hit(other(2), 2.0, "two"),
            hit(doc(3, "user", "three"), 1.0, "three"),
        ]);
        let out = render(|w| search_results(w, &r, &plain()));
        let first = out.find("b20208d8").unwrap();
        let second = out.find("99999999").unwrap();
        assert!(first < second, "session order follows first appearance");
        // Both hits of the first session sit above the second session's header.
        assert!(out.find("   3.").unwrap() < second, "{out}");
        assert_eq!(out.matches("b20208d8-fbdb").count(), 1, "one header each");
    }

    #[test]
    fn context_turns_render_under_the_hit() {
        let r = response(vec![hit(doc(41, "assistant", "body"), 1.0, "body")]);
        let around = vec![vec![
            doc(40, "user", "the turn before"),
            doc(41, "assistant", "body"),
            doc(42, "user", "the turn after"),
        ]];
        let out = render(|w| search_results_ctx(w, &r, &around, &plain()));
        assert!(out.contains("#40"), "{out}");
        assert!(out.contains("the turn before"), "{out}");
        assert!(out.contains("the turn after"), "{out}");
        // The hit itself is not repeated as its own context line.
        assert_eq!(out.matches("#41").count(), 1, "{out}");
    }

    #[test]
    fn empty_results_say_so_without_panicking() {
        let out = render(|w| search_results(w, &response(vec![]), &plain()));
        assert_eq!(out.trim(), "no matches");
        let out = render(|w| session_view(w, &[], &plain()));
        assert_eq!(out.trim(), "no documents");
        let out = render(|w| session_list(w, &[], &plain()));
        assert_eq!(out.trim(), "no sessions indexed");
        let out = render(|w| facet_list(w, &fr("tool_name", vec![]), &plain()));
        assert!(out.contains("(none)"), "{out}");
    }

    #[test]
    fn colour_is_emitted_only_when_asked_for() {
        let plain_out = render(|w| search_results(w, &sample(), &plain()));
        assert!(!plain_out.contains('\u{1b}'), "no escapes when color=false");

        let colour = OutputOpts {
            color: true,
            ..OutputOpts::default()
        };
        let colour_out = render(|w| search_results(w, &sample(), &colour));
        assert!(colour_out.contains('\u{1b}'), "escapes when color=true");
        // With colour on the markers become styling and disappear from the text.
        assert!(!colour_out.contains("**session**"), "{colour_out}");
        assert!(colour_out.contains("session"), "{colour_out}");

        // JSON never carries colour, whatever the caller asked for.
        let json_colour = OutputOpts {
            json: true,
            color: true,
            ..OutputOpts::default()
        };
        let json_out = render(|w| search_results(w, &sample(), &json_colour));
        assert!(!json_out.contains('\u{1b}'));
    }

    #[test]
    fn facet_bars_are_aligned_and_scaled() {
        let counts = [
            FacetCount {
                value: "Bash".into(),
                count: 64,
            },
            FacetCount {
                value: "Read".into(),
                count: 32,
            },
            FacetCount {
                value: "a-very-long-tool-name-that-will-not-fit-in-the-column".into(),
                count: 1,
            },
        ];
        let out = render(|w| facet_list(w, &fr("tool_name", counts.to_vec()), &plain()));
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].contains("3 values · 97 of 97 matching docs have a value"),
            "{out}"
        );
        let bar = |line: &str| line.matches('█').count();
        assert!(bar(lines[1]) > bar(lines[2]), "counts scale the bar");
        assert!(
            bar(lines[3]) >= 1,
            "a non-zero count always draws something"
        );
        // Counts line up: same column for every row.
        let col = |line: &str| line.chars().position(|c| c == '█').unwrap();
        assert_eq!(col(lines[1]), col(lines[2]));
        assert_eq!(col(lines[2]), col(lines[3]));
        assert!(lines[3].contains('…'), "long values are truncated: {out}");
    }

    #[test]
    fn session_view_renders_the_result_under_the_call() {
        let mut tool = tool_doc(
            2,
            "Bash",
            json!({"command": "cargo build"}),
            "Bash\ncargo build",
        );
        tool.tool_output = Some("error: linker `cc` not found".into());
        let out = render(|w| session_view(w, &[tool], &plain()));
        assert!(out.contains("cargo build"), "{out}");
        assert!(out.contains("error: linker `cc` not found"), "{out}");
        // Indented one step deeper than the call it answers.
        let result_line = out
            .lines()
            .find(|l| l.contains("linker"))
            .expect("the result is rendered");
        assert!(result_line.starts_with("        "), "{result_line:?}");

        // A result that never arrived adds no blank line.
        let mut pending = tool_doc(3, "Bash", json!({"command": "ls"}), "Bash\nls");
        pending.tool_output = None;
        let out = render(|w| session_view(w, &[pending], &plain()));
        assert!(!out.contains("\n\n\n"), "{out:?}");
    }

    /// The behaviour `6fc7afe` won by reordering the stored body, now kept at the render
    /// layer: a failed call previews the error, not the heredoc that failed. `--errors-only`
    /// carries no free-text query, so it lands in the no-snippet fallback every time.
    #[test]
    fn a_failed_tool_call_previews_its_error() {
        let long = format!("cat > f <<'PY'\n{}", "x".repeat(600));
        let mut failed = tool_doc(
            9,
            "Bash",
            json!({ "command": long.clone() }),
            &format!("Bash\n{long}"),
        );
        failed.is_error = true;
        failed.tool_output = Some("InputValidationError: JSON parse failed".into());

        assert!(
            doc_body(&failed).starts_with("InputValidationError"),
            "{:?}",
            doc_body(&failed)
        );

        // An empty snippet is what a filter-only search such as `--errors-only` produces. The
        // hit *header* still identifies the call by its params — it is the body that must not
        // be the heredoc, so the assertion is on the body line, not on the whole render.
        let r = response(vec![hit(failed.clone(), 1.0, "")]);
        let out = render(|w| search_results(w, &r, &plain()));
        let body = out
            .lines()
            .skip_while(|l| !l.contains("#9"))
            .nth(1)
            .expect("a body line under the hit");
        assert!(body.contains("InputValidationError"), "{out}");
        assert!(!body.contains("cat > f"), "{out}");

        // A call that succeeded is unchanged: the command leads, as it always did.
        let mut ok = failed.clone();
        ok.is_error = false;
        ok.tool_output = Some("Finished dev profile".into());
        assert!(
            doc_body(&ok).starts_with("Bash\ncat > f"),
            "{:?}",
            doc_body(&ok)
        );
    }

    /// A document whose whole body is its output — an orphaned `tool_result` — must not render
    /// as a blank row in the hit list or in a context window.
    #[test]
    fn an_output_only_document_renders_its_output() {
        let mut orphan = doc(4, "user", "");
        orphan.kind = DocKind::ToolCall;
        orphan.tool_use_id = Some("toolu_orphan".into());
        orphan.tool_output = Some("Finished dev profile".into());

        let r = response(vec![hit(orphan.clone(), 1.0, "")]);
        let out = render(|w| search_results(w, &r, &plain()));
        assert!(out.contains("Finished dev profile"), "{out}");

        // …and the same document seen as a neighbour in a context window.
        let neighbour = doc(5, "assistant", "and then");
        let r = response(vec![hit(neighbour.clone(), 1.0, "and then")]);
        let ctx = vec![vec![orphan, neighbour]];
        let out = render(|w| search_results_ctx(w, &r, &ctx, &plain()));
        assert!(out.contains("Finished dev profile"), "{out}");
    }

    #[test]
    fn session_view_shows_params_and_caps_long_bodies() {
        let long = "x".repeat(TOOL_BUDGET + 500);
        let docs = vec![
            doc(1, "user", "please build it"),
            tool_doc(
                2,
                "Bash",
                json!({"command": "cargo build", "timeout": 600000}),
                &long,
            ),
        ];
        let out = render(|w| session_view(w, &docs, &plain()));
        assert!(out.contains("#1"), "{out}");
        assert!(out.contains("please build it"), "{out}");
        assert!(out.contains("command=cargo build"), "{out}");
        assert!(out.contains("timeout=600000"), "{out}");
        assert!(out.contains("claude-opus-5"), "{out}");
        assert!(
            out.contains("(+500 chars)"),
            "long bodies are clipped: {out}"
        );
        assert!(out.len() < long.len() + 4_000);
    }

    #[test]
    fn session_list_is_three_lines_of_what_matters() {
        let info = SessionInfo {
            session_id: "b20208d8-fbdb".into(),
            slug: Some("wild-spinning-puppy".into()),
            project: Some("/home/user/session-search".into()),
            git_branch: Some("main".into()),
            title: Some("Rust MCP session indexing".into()),
            messages: 62,
            tool_calls: 32,
            last_ts_ms: Some(T0),
            ..SessionInfo::default()
        };
        let out = render(|w| session_list(w, &[info], &plain()));
        assert!(out.contains("b20208d8-fbdb  wild-spinning-puppy"), "{out}");
        assert!(
            out.contains("2026-09-09 19:07 · 62 msg · 32 tools"),
            "{out}"
        );
        assert!(out.contains("/home/user/session-search · main"), "{out}");
        assert!(out.contains("Rust MCP session indexing"), "{out}");
    }

    #[test]
    fn stats_table_is_aligned() {
        let out = render(|w| {
            stats(
                w,
                &IndexStats {
                    files_scanned: 4,
                    docs_added: 1_234_567,
                    sessions: 4,
                    parse_errors: 2,
                    elapsed_ms: 91,
                    ..IndexStats::default()
                },
                &plain(),
            )
        });
        assert!(out.contains("documents added"), "{out}");
        assert!(out.contains("1,234,567"), "thousands separators: {out}");
        assert!(out.contains("91 ms"), "{out}");
        let value_col = |needle: &str| {
            out.lines()
                .find(|l| l.contains(needle))
                .map(|l| l.rfind(char::is_numeric).unwrap())
        };
        assert_eq!(value_col("files scanned"), value_col("sessions"));
    }

    // -- helpers ------------------------------------------------------------

    #[test]
    fn wrapping_respects_width_and_markers() {
        let text = "the quick brown fox jumps over the lazy dog";
        for line in wrap(text, 20) {
            assert!(display_len(&line) <= 20, "{line:?}");
        }
        // Markers are zero-width, so a highlighted line holds the same number of words.
        let marked = "the **quick** brown **fox** jumps over the lazy dog";
        assert_eq!(wrap(text, 20).len(), wrap(marked, 20).len());
        // A single unbreakable token is cut, not allowed to overflow.
        let long = wrap(&"z".repeat(80), 20);
        assert_eq!(long.len(), 1);
        assert!(display_len(&long[0]) <= 20);
    }

    #[test]
    fn truncation_keeps_marker_pairs_balanced() {
        let s = truncate("**abcdefgh** ijklmnop", 6);
        assert_eq!(s.matches(HL).count() % 2, 0, "{s:?}");
        assert!(s.ends_with('…'));
        assert_eq!(truncate("short", 40), "short");
        assert_eq!(truncate("anything", 0), "");
    }

    #[test]
    fn tool_params_lead_with_the_identifying_key() {
        let input = json!({"zzz": 1, "command": "ls -la", "timeout": 5});
        let rendered = tool_params(&input, 200);
        assert!(rendered.starts_with("command=ls -la"), "{rendered}");
        assert!(rendered.contains("timeout=5"), "{rendered}");

        // Collections are summarised rather than dumped.
        let todos = json!({"todos": [1, 2, 3], "nested": {"a": 1}});
        let rendered = tool_params(&todos, 200);
        assert!(rendered.contains("todos=[3 items]"), "{rendered}");
        assert!(rendered.contains("nested={1 keys}"), "{rendered}");

        // Multi-line whitespace never breaks the single-line layout.
        let multi = json!({"command": "one\ntwo\nthree"});
        assert_eq!(tool_params(&multi, 200), "command=one two three");
        // A non-object input is still rendered.
        assert_eq!(tool_params(&json!("bare"), 200), "bare");
        assert_eq!(tool_params(&Value::Null, 200), "");
    }

    #[test]
    #[ignore = "eyeball only: cargo test --lib eyeball -- --ignored --nocapture"]
    fn eyeball() {
        let mut r = sample();
        r.hits.push(hit(
            tool_doc(
                58,
                "Edit",
                json!({"file_path": "/home/user/session-search/src/format.rs", "old_string": "fn a() {}"}),
                "applied",
            ),
            1.9,
            "applied the **edit** to format.rs",
        ));
        let o = OutputOpts {
            color: true,
            ..OutputOpts::default()
        };
        print!("{}", render(|w| search_results(w, &r, &o)));
        println!("\n--- facets ---");
        print!(
            "{}",
            render(|w| facet_list(
                w,
                &fr(
                    "tool_input.file_path",
                    vec![
                        FacetCount {
                            value: "/home/user/session-search/src/search.rs".into(),
                            count: 12
                        },
                        FacetCount {
                            value: "/home/user/session-search/src/format.rs".into(),
                            count: 5
                        },
                        FacetCount {
                            value: "/home/user/session-search/Cargo.toml".into(),
                            count: 1
                        },
                    ]
                ),
                &o
            ))
        );
        println!("\n--- session view ---");
        print!(
            "{}",
            render(|w| session_view(
                w,
                &[
                    doc(
                        1,
                        "user",
                        "Create a rust MCP server that indexes Claude Code session transcripts and lets me search them."
                    ),
                    tool_doc(
                        2,
                        "Bash",
                        json!({"command": "cargo build --release"}),
                        "Compiling session-search v0.1.0\n    Finished dev profile"
                    ),
                ],
                &o
            ))
        );
        println!("\n--- sessions ---");
        print!(
            "{}",
            render(|w| session_list(
                w,
                &[SessionInfo {
                    session_id: "b20208d8-fbdb-5918-ba69-d203de6ed6dc".into(),
                    slug: Some("wild-spinning-puppy".into()),
                    project: Some("/home/user/session-search".into()),
                    git_branch: Some("claude/rust-mcp-session-indexing".into()),
                    title: Some("Rust MCP server for session indexing".into()),
                    messages: 62,
                    tool_calls: 32,
                    last_ts_ms: Some(T0),
                    ..SessionInfo::default()
                }],
                &o
            ))
        );
        println!("--- stats ---");
        print!(
            "{}",
            render(|w| stats(
                w,
                &IndexStats {
                    files_scanned: 4,
                    files_updated: 2,
                    docs_added: 1_234,
                    sessions: 4,
                    elapsed_ms: 91,
                    ..IndexStats::default()
                },
                &o
            ))
        );
    }

    // -- regressions --------------------------------------------------------

    /// Transcripts are full of the raw stdout of `ls --color`, `cargo` and `git`. Those escape
    /// sequences must not reach the terminal, where they would repaint or erase this program's
    /// own output — and they must not be counted as visible columns either.
    #[test]
    fn control_characters_from_transcript_content_never_reach_the_terminal() {
        let hostile = "before \u{1b}[2K\u{1b}[A repaint \u{7} bell \u{1b}]0;title\u{7} after";
        let r = response(vec![hit(
            doc(1, "user", hostile),
            1.0,
            &format!("**before** {hostile}"),
        )]);
        let out = render(|w| search_results(w, &r, &plain()));
        assert!(!out.contains('\u{1b}'), "escape reached stdout: {out:?}");
        assert!(!out.contains('\u{7}'), "BEL reached stdout: {out:?}");
        assert!(out.contains("repaint"), "the text itself is kept: {out}");

        let view = render(|w| session_view(w, &[doc(1, "user", hostile)], &plain()));
        assert!(!view.contains('\u{1b}'), "{view:?}");
        assert!(!view.contains('\u{7}'), "{view:?}");

        let counts = [FacetCount {
            value: format!("cargo {}[1mbuild", '\u{1b}'),
            count: 1,
        }];
        let facets =
            render(|w| facet_list(w, &fr("tool_input.command", counts.to_vec()), &plain()));
        assert!(!facets.contains('\u{1b}'), "{facets:?}");

        // Ordinary whitespace is still whitespace, not a replacement character.
        assert_eq!(one_line("a\tb\nc", 40), "a b c");
    }

    /// `--limit 0` is documented as "totals and facets with no hits", and paging past the end
    /// is not the same as matching nothing. Only `total == 0` is "no matches".
    #[test]
    fn zero_rendered_hits_still_report_the_total() {
        let mut r = response(vec![]);
        r.total = 26;
        let out = render(|w| search_results(w, &r, &plain()));
        assert!(out.contains("0 of 26 hits"), "{out}");
        assert!(!out.contains("no matches"), "{out}");

        // A genuinely empty result set still says so.
        let out = render(|w| search_results(w, &response(vec![]), &plain()));
        assert_eq!(out.trim(), "no matches");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
