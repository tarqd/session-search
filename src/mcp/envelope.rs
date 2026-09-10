//! The one implementation of the zero-hit contract: filter echo, resolved time range, and the
//! ranked retry suggestion.
//!
//! Issue #28 names the failure this exists to prevent: *"a model that receives `{hits: []}` will
//! tell the user nothing happened last week."* The documented failure mode of LLM-driven
//! structured querying is that reliability holds for simple queries and degrades sharply as
//! complexity rises, and every degradation here looks identical from the outside — a misspelled
//! `kind`, a case-wrong `program`, a branch name spelled short, and a genuinely empty corpus all
//! return the same zero. So a zero must never travel alone.
//!
//! Four tools would otherwise each write their own version of this, and the four would disagree
//! about which filter is narrowest — which is the same class of bug `sessions.rs` was extracted
//! to end. One module, four callers, one ranking.
//!
//! # The ranking
//!
//! Nothing in the codebase ranks filters, and the order cannot be read off the request: it
//! follows from *how* each filter is matched, which the caller cannot see. Five mechanisms, most
//! to least selective:
//!
//! 0. an address (`turn_of` + `turn_seq`) — an intersection naming one turn of one file. Every
//!    other filter selects a set; this one selects a place, so nothing below it can be narrower;
//! 1. phrase over analyzed text (`tool_input`, `tool_output`) — adjacent, in order, unstemmed;
//! 2. exact term over an **open** vocabulary (`program`, `branch`, `model`, `tool`,
//!    `agent_type`, `lang`) — byte equality against a value reproduced from memory, so a silent
//!    zero whenever it was misremembered;
//! 3. prefix / range (`session`, `project`, `min_thinking`, `since`/`until`) — width is the
//!    caller's choice, not the field's;
//! 4. exact term over a **closed** vocabulary, and flags (`kind`, `role`, `errors_only`, the
//!    sidechain flags) — wide when right, total when wrong.
//!
//! Two overrides on plain narrowness:
//!
//! * **silent-zero traps are promoted.** `kind` and `role` have statically known legal sets, so
//!   an illegal value is *provably* the cause: it is dropped first and the retry names the legal
//!   set. `program` gets the same promotion one rung down — raw byte equality — but can only be
//!   probed, not proved;
//! * **scope filters are held back.** `project` and the time window drop last, because dropping
//!   them does not widen the question, it answers a different one. A hit from another repository
//!   or another month is not a better answer than zero; it is a wrong answer that reads as a
//!   right one.
//!
//! And one filter is outside the ranking altogether. `all_records` only ever widens — it brings
//! the apparatus records (attachments, `system`, meta turns) back into scope — so it cannot be
//! the cause of a zero, and naming it would tell a caller to drop the one thing holding the
//! search open. It is still echoed as applied, because the retry is rebuilt from that echo and a
//! retry that dropped it would search less than the call it is answering. What the scope did to
//! a result is reported as a number instead: `hidden` on the `search_turns` response counts the
//! documents the default scope refused.
//!
//! # What the retry does with the diagnosis
//!
//! The ranking names a filter; the retry is the object a client sends without reading the prose,
//! so the two must not disagree. Three rules keep them together:
//!
//! * **a contradiction drops the exclusion, never the intent.** `agent_type` lives only on
//!   sidechain documents and `no_sidechains` deletes every one of them, so the pair is provably
//!   empty. The retry drops the flag and keeps `agent_type`: the caller's question is in the
//!   value, and a retry that kept the exclusion would answer a question nobody asked and return
//!   zero a second time;
//! * **an exact-match value the caller mis-spelled is corrected, not deleted.** `program`,
//!   `tool`, `branch`, `lang` and `model` are exact terms over a vocabulary, so `Cargo` and
//!   `cargo build` are silent zeroes where `cargo` matches. Deleting the filter answers a
//!   different, much wider question — every document mentioning the query — while correcting it
//!   answers the one that was asked. The correction is derived from the value the caller sent
//!   (case-folded, cut at the first word, or spelled against the capitalised tool vocabulary),
//!   so **the ranking still never probes the index**: it stays a total function of the request.
//!   A correction that is itself wrong costs one extra call and no more — the second pass finds
//!   nothing left to repair and drops the filter;
//! * **a retry never hands back a term the message just warned about.** A query term beginning
//!   with `-` is a negation, and the rephrase retry quotes the whole query rather than offering
//!   the negated term as the one to keep.

use serde_json::{Value, json};

use crate::mcp::types::{AppliedFilter, Corpus, Envelope, NoResults, RetryAction, TimeRange};
use crate::search::{Edge, Filters, when_ms};
use crate::sessions::FilterError;

/// Legal values of `kind`. Statically known, which is what makes an illegal one provable.
pub const KIND_VALUES: &[&str] = &["message", "tool_call"];
/// Legal values of `role`.
pub const ROLE_VALUES: &[&str] = &["user", "assistant", "system", "attachment"];

// ---------------------------------------------------------------------------
// time
// ---------------------------------------------------------------------------

/// Resolve `since`/`until` to absolute instants, against one `now` for both ends.
///
/// Called in the tool layer *before* the index is touched. Two things depend on that placement:
/// an unreadable date becomes `invalid_params` naming the field, rather than an `anyhow` chain
/// surfacing from inside `search()` where the caller cannot tell a bad date from an empty
/// corpus; and the echo is impossible unless somebody resolved it. One `now` for both ends
/// because a window measured against two different clocks is not a window.
pub fn resolve_time_range(f: &Filters) -> Result<TimeRange, FilterError> {
    resolve_time_range_at(f, chrono::Utc::now())
}

/// [`resolve_time_range`] against a fixed clock, for tests.
pub fn resolve_time_range_at(
    f: &Filters,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<TimeRange, FilterError> {
    let at = |raw: &Option<String>, field: &'static str, edge: Edge| {
        raw.as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| when_ms(s, now, edge))
            .transpose()
            .map_err(|source| FilterError { field, source })
    };
    let since_ms = at(&f.since, "since", Edge::Lower)?;
    let until_ms = at(&f.until, "until", Edge::Upper)?;
    Ok(TimeRange {
        since: since_ms.and_then(rfc3339),
        since_ms,
        until: until_ms.and_then(rfc3339),
        until_ms,
    })
}

fn rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

// ---------------------------------------------------------------------------
// the filter echo
// ---------------------------------------------------------------------------

/// Every filter the request set, in [`Filters`] declaration order.
///
/// Declaration order rather than narrowness order: this is a report of what was applied, and a
/// caller comparing it against what they sent should not have to re-sort it. The ranking is a
/// separate question, answered by [`narrowest`].
pub fn applied_filters(f: &Filters, range: &TimeRange) -> Vec<AppliedFilter> {
    let mut out = Vec::new();
    let mut push = |name: &str, value: Value| {
        out.push(AppliedFilter {
            name: name.to_string(),
            value,
            resolved: None,
        });
    };
    if let Some(v) = opt(&f.project) {
        push("project", json!(v));
    }
    if !f.tool.is_empty() {
        push("tool", json!(f.tool));
    }
    if !f.tool_input.is_empty() {
        push("tool_input", json!(f.tool_input));
    }
    if !f.tool_output.is_empty() {
        push("tool_output", json!(f.tool_output));
    }
    if !f.lang.is_empty() {
        push("lang", json!(f.lang));
    }
    if let Some(n) = f.min_thinking {
        push("min_thinking", json!(n));
    }
    if !f.program.is_empty() {
        push("program", json!(f.program));
    }
    if let Some(v) = opt(&f.branch) {
        push("branch", json!(v));
    }
    if let Some(v) = opt(&f.model) {
        push("model", json!(v));
    }
    if let Some(v) = opt(&f.role) {
        push("role", json!(v));
    }
    if let Some(v) = opt(&f.kind) {
        push("kind", json!(v));
    }
    if let Some(v) = opt(&f.session) {
        push("session", json!(v));
    }
    if let Some(v) = opt(&f.agent_type) {
        push("agent_type", json!(v));
    }
    if let Some(v) = opt(&f.since) {
        out.push(AppliedFilter {
            name: "since".into(),
            value: json!(v),
            resolved: range.since.clone(),
        });
    }
    if let Some(v) = opt(&f.until) {
        out.push(AppliedFilter {
            name: "until".into(),
            value: json!(v),
            resolved: range.until.clone(),
        });
    }
    // Written straight into `out` rather than through `push`, like the two dates above it: the
    // closure holds a mutable borrow of `out` from where it is defined to where it is last used.
    //
    // All three are echoed, and `all_records` is the one worth defending. It widens rather than
    // narrows, so it looks like nothing a "filters applied" line needs to carry — but this echo
    // is also what [`retry_body`] rebuilds the retry from, and a retry assembled without it
    // would quietly re-narrow the scope the caller had opened. A suggested retry that searches
    // less than the call it is answering is the one shape of advice nobody can debug.
    if f.all_records {
        out.push(AppliedFilter {
            name: "all_records".into(),
            value: json!(true),
            resolved: None,
        });
    }
    if let Some(v) = f
        .turn_of
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(AppliedFilter {
            name: "turn_of".into(),
            value: json!(v),
            resolved: None,
        });
    }
    if let Some(n) = f.turn_seq {
        out.push(AppliedFilter {
            name: "turn_seq".into(),
            value: json!(n),
            resolved: None,
        });
    }
    if f.errors_only {
        out.push(AppliedFilter {
            name: "errors_only".into(),
            value: json!(true),
            resolved: None,
        });
    }
    if f.no_sidechains {
        out.push(AppliedFilter {
            name: "no_sidechains".into(),
            value: json!(true),
            resolved: None,
        });
    }
    if f.sidechains_only {
        out.push(AppliedFilter {
            name: "sidechains_only".into(),
            value: json!(true),
            resolved: None,
        });
    }
    out
}

fn opt(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// `project=/home/u/code, program=cargo, since=7d` — the `{applied}` slot of the retry sentence.
fn render_applied(applied: &[AppliedFilter]) -> String {
    applied
        .iter()
        .map(|a| format!("{}={}", a.name, render_value(&a.value)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(render_value).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// the ranking
// ---------------------------------------------------------------------------

/// The filter a retry should act on first, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// The filter's name, as a request spells it.
    pub filter: &'static str,
    /// Its value, as it arrived.
    pub value: Value,
    /// The mechanism that makes it the narrowest, in one sentence a model can act on.
    pub why: String,
    /// Drop it, or correct it.
    pub action: RetryAction,
    /// The closed vocabulary, when the action is [`RetryAction::Fix`] over one. Empty when the
    /// fix is a spelling repair over an open vocabulary, where there is no set to list.
    pub legal: &'static [&'static str],
    /// The value the retry substitutes, when the action is [`RetryAction::Fix`].
    ///
    /// A `Value` rather than a `String` because three of the repairable filters are repeatable
    /// and arrive as arrays; a retry that corrected `program: ["Cargo"]` to the string `cargo`
    /// would not be sendable in the shape the field takes.
    pub fix_to: Option<Value>,
    /// The other filter of a contradictory pair — the one whose intent [`Ranked::filter`]
    /// destroys, and which the retry therefore keeps. Set only where the pair is provably empty
    /// together, and it is what makes the message say *these two cannot both hold* rather than
    /// *this one is narrow*.
    pub contradicts: Option<&'static str>,
}

/// The narrowest filter the request actually set, per the drop order above. `None` when no
/// filter is set at all — the query itself is then the only thing that can have failed.
///
/// The order is exhaustive over [`Filters`] — every field but `all_records`, which is excluded
/// on purpose and for the reason given at its place in the order — and stops at the first filter
/// present, so it is a total function of the request and never depends on the corpus. That
/// matters: a ranking that probed the index would take a second pass over the same query for
/// every zero-hit answer, and would still be a guess.
///
/// `the_drop_order_is_exhaustive_and_stops_at_the_first_filter_set` reads the field list off
/// `Filters` itself rather than repeating it, so a field added to that struct and forgotten here
/// fails as a missing name instead of passing quietly.
pub fn narrowest(f: &Filters) -> Option<Ranked> {
    // 1. `kind` / `role` with a value outside its legal set — statically provable.
    if let Some(v) = opt(&f.kind)
        && !KIND_VALUES.contains(&v)
    {
        return Some(Ranked {
            filter: "kind",
            value: json!(v),
            why: format!(
                "`kind` accepts only `message` or `tool_call`; `{v}` matches nothing and is not \
                 an error"
            ),
            action: RetryAction::Fix,
            legal: KIND_VALUES,
            fix_to: Some(json!(nearest(v, KIND_VALUES))),
            contradicts: None,
        });
    }
    if let Some(v) = opt(&f.role)
        && !ROLE_VALUES.contains(&v)
    {
        return Some(Ranked {
            filter: "role",
            value: json!(v),
            why: format!(
                "`role` accepts only `user`, `assistant`, `system` or `attachment`; `{v}` matches \
                 nothing and is not an error. Note that a tool call's role is `assistant` — use \
                 `kind` to isolate tool calls"
            ),
            action: RetryAction::Fix,
            legal: ROLE_VALUES,
            fix_to: Some(json!(nearest(v, ROLE_VALUES))),
            contradicts: None,
        });
    }

    // 2. A contradictory pair, which is provable in the same way an illegal `kind` is: the two
    //    filters select disjoint halves of the corpus, so their intersection is empty however
    //    the rest of the request is spelled. What goes is the broad exclusion flag, never the
    //    filter carrying the value — see the module docs.
    if f.no_sidechains && f.sidechains_only {
        return Some(Ranked {
            filter: "no_sidechains",
            value: json!(true),
            why: "`sidechains_only` keeps subagent transcripts and `no_sidechains` removes them; \
                  set together they select nothing at all. `sidechains_only` is the one that \
                  asks for something, so it is the flag that stays"
                .to_string(),
            action: RetryAction::Drop,
            legal: &[],
            fix_to: None,
            contradicts: Some("sidechains_only"),
        });
    }
    if let Some(v) = opt(&f.agent_type)
        && f.no_sidechains
    {
        return Some(Ranked {
            filter: "no_sidechains",
            value: json!(true),
            why: format!(
                "`agent_type` is an exact term that only sidechain documents carry, so \
                 `agent_type={v}` silently implies subagent transcripts and `no_sidechains` \
                 excludes every one of them — the pair matches nothing whatever else is set. \
                 The intent is in `agent_type`; the flag is what goes"
            ),
            action: RetryAction::Drop,
            legal: &[],
            fix_to: None,
            contradicts: Some("agent_type"),
        });
    }

    // 3..15. Narrowness, most to least selective. `rank` corrects the value where the value
    //        itself says how, and drops the filter where it does not.
    let rank = |filter: &'static str, value: Value, why: &str| {
        let fix_to = repaired(filter, &value);
        Some(Ranked {
            filter,
            value,
            why: why.to_string(),
            action: if fix_to.is_some() {
                RetryAction::Fix
            } else {
                RetryAction::Drop
            },
            legal: &[],
            fix_to,
            contradicts: None,
        })
    };

    // 3. The turn address. Narrower than anything below it and narrow in a different way: every
    //    other filter selects a set, this one names a place — one turn of one file, ten to fifty
    //    documents. It is ranked under `turn_of` because a `Ranked` names one filter and the
    //    path is the half that carries the meaning; `retry_body` drops both, since half an
    //    address is not a request.
    if let Some(path) = opt(&f.turn_of)
        && let Some(seq) = f.turn_seq
    {
        return rank(
            "turn_of",
            json!(path),
            &format!(
                "`turn_of` + `turn_seq` address one turn of one file — the narrowest filter \
                 there is, and the only one that names a place rather than a set. A zero means \
                 that turn does not contain the query terms; the address itself is rarely the \
                 fault, since it came back from a `search_turns` hit. Dropping it asks the same \
                 question of the whole corpus, and `get_turn` on the same pair (source_path \
                 {path}, turn_seq {seq}) is how to read what the turn actually says instead of \
                 searching inside it"
            ),
        );
    }

    // `all_records` is deliberately not in this order, and this note is here so that it cannot
    // be added later by someone reading the list as a checklist of fields. It is the only filter
    // that widens: setting it brings the apparatus records back into scope, so it can turn a
    // zero into a hit and never the other way round. Naming it as the narrowest would tell a
    // caller to drop the one thing holding the search open. `hidden` on the response is where
    // the scope belongs in a zero-hit story, and it is a count rather than a suspicion.

    if !f.tool_input.is_empty() {
        return rank(
            "tool_input",
            json!(f.tool_input),
            "`tool_input` is an exact match on one named parameter of one tool, and it has two \
             independent ways to be wrong: the key may never appear on any tool, and the value \
             may not be spelled the way it was recorded. Both are silent zeroes. `aggregate` on \
             `tool_input.<key>` returns the vocabulary",
        );
    }
    if !f.tool_output.is_empty() {
        return rank(
            "tool_output",
            json!(f.tool_output),
            "`tool_output` is a phrase query: the words must appear adjacent and in order, and \
             nothing is stemmed, so one extra or missing word ends the match set. The free-text \
             query already searches tool output and is the forgiving version",
        );
    }
    if !f.program.is_empty() {
        return rank(
            "program",
            json!(f.program),
            "`program` is an exact, case-sensitive match on the program name as it was typed, so \
             `Cargo` and `cargo build` match nothing where `cargo` matches. It also exists only \
             where the shell grammar parsed the command",
        );
    }
    if let Some(n) = f.min_thinking {
        return rank(
            "min_thinking",
            json!(n),
            "`min_thinking` is nominally a range and in practice a presence filter: the count is \
             attached to exactly one document per API message, so even `1` cuts the corpus to a \
             small minority",
        );
    }
    if !f.lang.is_empty() {
        return rank(
            "lang",
            json!(f.lang),
            "`lang` matches the info string of a markdown code fence, so it excludes every tool \
             call and every unfenced message before it compares anything",
        );
    }
    if let Some(v) = opt(&f.agent_type) {
        return rank(
            "agent_type",
            json!(v),
            "`agent_type` is an exact term that only sidechain documents carry, so it silently \
             implies subagent transcripts and contradicts `no_sidechains` outright",
        );
    }
    if let Some(v) = opt(&f.model) {
        return rank(
            "model",
            json!(v),
            "`model` is an exact term on the full recorded id, so a family name such as `opus` \
             or `claude-opus` is a guaranteed zero. It also excludes every non-assistant record",
        );
    }
    if let Some(v) = opt(&f.branch) {
        return rank(
            "branch",
            json!(v),
            "`branch` is an exact term on the whole recorded name, never a prefix: `claude/` does \
             not match `claude/some-branch`, and a name spelled short is a silent zero",
        );
    }
    if !f.tool.is_empty() {
        return rank(
            "tool",
            json!(f.tool),
            "`tool` is exact and case-sensitive against the capitalised vocabulary the transcript \
             writes — `Bash`, not `bash`",
        );
    }
    if let Some(v) = opt(&f.session) {
        return rank(
            "session",
            json!(v),
            "`session` is a prefix match, but it scopes the answer to one conversation, so \
             anything the question is really about that happened elsewhere is excluded",
        );
    }
    if f.sidechains_only {
        return rank(
            "sidechains_only",
            json!(true),
            "`sidechains_only` keeps only subagent transcripts, which are a minority of files",
        );
    }
    if f.errors_only {
        return rank(
            "errors_only",
            json!(true),
            "`errors_only` keeps only calls the transcript itself flagged as failed, which are a \
             small minority of the corpus",
        );
    }
    if f.no_sidechains {
        return rank(
            "no_sidechains",
            json!(true),
            "`no_sidechains` removes a minority of documents, so it rarely explains a zero on its \
             own — but it is the last non-scope filter set here",
        );
    }

    // 15. `kind`, then `role`, with legal values — the widest term filters.
    if let Some(v) = opt(&f.kind) {
        return rank(
            "kind",
            json!(v),
            "`kind` splits the corpus roughly in half, so it is wide when right — but it is the \
             widest filter still set, and `message` versus `tool_call` is the single most common \
             way to look in the wrong half",
        );
    }
    if let Some(v) = opt(&f.role) {
        return rank(
            "role",
            json!(v),
            "`role` is a legal value here, so it cannot be provably wrong — but a tool call's \
             role is `assistant`, which is the standard way a `role` filter excludes the exact \
             documents the question was about",
        );
    }

    // 16. The time window. `since` before `until`, and it is not arbitrary: a lower bound
    //     excludes everything before it, which on a transcript corpus is nearly all of it,
    //     while an upper bound usually sits at or near `now` and excludes nothing.
    if let Some(v) = opt(&f.since) {
        return rank(
            "since",
            json!(v),
            "only the time window and the project scope are left. Dropping `since` widens the \
             search to older work — the retry covers a different period than the one you asked \
             about, so say so when you report the result",
        );
    }
    if let Some(v) = opt(&f.until) {
        return rank(
            "until",
            json!(v),
            "only the time window and the project scope are left. Dropping `until` widens the \
             search to more recent work — the retry covers a different period than the one you \
             asked about, so say so when you report the result",
        );
    }

    // 17. `project`, always last.
    if let Some(v) = opt(&f.project) {
        return rank(
            "project",
            json!(v),
            "this is the last filter. Dropping it does not widen this question, it answers a \
             different one — results will come from other repositories on this machine",
        );
    }
    None
}

/// Tool names as the transcript capitalises them, for repairing a lower-cased `tool`.
///
/// A spelling aid, **not** a legal set: the vocabulary is open — an MCP server contributes tool
/// names nobody here can know — so a value that matches nothing in this list is left alone and
/// the filter is dropped rather than "corrected" to something invented. `aggregate` on
/// `tool_name` is still the way to learn what a corpus actually holds.
const TOOL_SPELLINGS: &[&str] = &[
    "Bash",
    "BashOutput",
    "Edit",
    "ExitPlanMode",
    "Glob",
    "Grep",
    "KillShell",
    "NotebookEdit",
    "Read",
    "SlashCommand",
    "Task",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
    "Write",
];

/// The corrected value for an exact-match filter, or `None` when the value says nothing about
/// how it is wrong.
///
/// Index-free by construction: every candidate is derived from the value the caller sent, so
/// this stays a pure function of the request and the promise above — that the ranking never
/// takes a second pass over the corpus — still holds. Three repairs, all of them observed:
/// a value carrying more than the term (`cargo build`, cut at the first word), a case-wrong
/// value over a lower-case vocabulary (`Cargo`), and a lower-cased tool name (`bash`), which
/// goes the other way and is repaired against [`TOOL_SPELLINGS`] rather than by guessing at
/// capitalisation.
///
/// A repair that is itself wrong is bounded: the retry carries the corrected value, finds
/// nothing, and the next ranking has nothing left to repair — so it drops the filter, exactly
/// as it would have on the first pass.
fn repaired(filter: &str, value: &Value) -> Option<Value> {
    match value {
        Value::String(one) => repaired_term(filter, one).map(Value::String),
        Value::Array(items) => {
            let fixed: Vec<Value> = items
                .iter()
                .map(|item| match item {
                    Value::String(one) => repaired_term(filter, one)
                        .map(Value::String)
                        .unwrap_or_else(|| item.clone()),
                    other => other.clone(),
                })
                .collect();
            (fixed != *items).then_some(Value::Array(fixed))
        }
        _ => None,
    }
}

fn repaired_term(filter: &str, value: &str) -> Option<String> {
    let head = value.split_whitespace().next().unwrap_or_default();
    if head.is_empty() {
        return None;
    }
    let proposal = match filter {
        "tool" => tool_spelling(head)?.to_string(),
        // `lang` is lower-cased on both sides before it is matched, so case is never the fault
        // here and proposing a case change would be a retry that cannot behave differently.
        "lang" => head.to_string(),
        "program" | "branch" | "model" => head.to_lowercase(),
        _ => return None,
    };
    (proposal != value).then_some(proposal)
}

/// The capitalisation the transcript uses for a tool the caller spelled some other way.
fn tool_spelling(value: &str) -> Option<&'static str> {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let want = squash(value);
    TOOL_SPELLINGS.iter().copied().find(|t| squash(t) == want)
}

/// The legal value closest to what the caller wrote, for the `fix` retry.
///
/// Deliberately crude: strip everything but alphanumerics and compare, which catches the whole
/// observed family — `toolcall`, `tool-call`, `ToolCall`, `Tool_Call`. Anything else falls back
/// to the first legal value, so the retry is always sendable.
fn nearest(value: &str, legal: &'static [&'static str]) -> &'static str {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let want = squash(value);
    legal
        .iter()
        .copied()
        .find(|l| squash(l) == want)
        .unwrap_or(legal[0])
}

// ---------------------------------------------------------------------------
// building the envelope
// ---------------------------------------------------------------------------

/// Everything the envelope needs that is not the result count.
pub struct Context<'a> {
    /// The tool being answered, as the model calls it. Only used in prose.
    pub tool: &'a str,
    /// The free-text query, when the tool takes one.
    pub query: Option<&'a str>,
    pub filters: &'a Filters,
    /// Already resolved, by [`resolve_time_range`], before the index was touched.
    pub time_range: TimeRange,
    /// Tool-specific arguments the retry must carry through — `("field", json!("tool_name"))`
    /// for `aggregate`, whose retry is meaningless without it. Emitted after the filters, in
    /// the order given.
    pub extra: Vec<(&'static str, Value)>,
    pub corpus: &'a Corpus,
}

/// The envelope for one answer. `total` is what the tool is about to return.
///
/// `no_results` is `Some` **exactly** when `total == 0`, and never otherwise: an envelope that
/// suggested a retry beside a page of hits would train a caller to ignore it.
pub fn build(ctx: &Context<'_>, total: usize, warnings: Vec<String>) -> Envelope {
    let applied = applied_filters(ctx.filters, &ctx.time_range);
    let no_results = (total == 0).then(|| no_results(ctx, &applied));
    Envelope {
        applied_filters: applied,
        time_range: ctx.time_range.clone(),
        warnings,
        no_results,
    }
}

fn no_results(ctx: &Context<'_>, applied: &[AppliedFilter]) -> NoResults {
    let Some(ranked) = narrowest(ctx.filters) else {
        return no_filters_at_all(ctx);
    };
    let retry = retry_body(ctx, Some(&ranked));
    let head = format!(
        "0 results. Filters applied: {}.{}",
        render_applied(applied),
        time_sentence(&ctx.time_range)
    );
    let body = match ranked.action {
        // A contradiction is named as a pair, because "the narrowest of these" would be a lie
        // about a flag that is wide on its own and only fatal beside the filter it cancels.
        _ if ranked.contradicts.is_some() => format!(
            "{}.\n\nRetry without {}, keeping {}: {}",
            ranked.why,
            ranked.filter,
            ranked.contradicts.unwrap_or_default(),
            compact(&retry)
        ),
        RetryAction::Fix => {
            let fix = ranked.fix_to.as_ref().map(render_value).unwrap_or_default();
            format!(
                "{}.\n\nRetry with {}={fix}: {}",
                ranked.why,
                ranked.filter,
                compact(&retry)
            )
        }
        _ if ranked.filter == "project" => format!(
            "The last filter is project={}. Dropping it does not widen this question, it answers \
             a different one — results will come from other repositories on this machine.\n\n\
             Retry across all projects: {}",
            render_value(&ranked.value),
            compact(&retry)
        ),
        _ => format!(
            "The narrowest of these is {}={}, and it is the likeliest cause: {}.\n\nRetry \
             without it: {}",
            ranked.filter,
            render_value(&ranked.value),
            ranked.why,
            compact(&retry)
        ),
    };
    NoResults {
        message: format!("{head}\n\n{body}"),
        narrowest_filter: Some(ranked.filter.to_string()),
        narrowest_value: Some(ranked.value.clone()),
        why: Some(ranked.why.clone()),
        action: ranked.action,
        legal_values: ranked.legal.iter().map(|s| (*s).to_string()).collect(),
        retry,
    }
}

/// `" Time range resolved to A .. B."`, or nothing when neither bound was set.
///
/// Omitted rather than rendered as `unbounded .. unbounded`: a sentence reporting a window
/// nobody asked for is one more thing to read past, and its absence is unambiguous.
fn time_sentence(range: &TimeRange) -> String {
    if range.since.is_none() && range.until.is_none() {
        return String::new();
    }
    let lo = range.since.as_deref().unwrap_or("unbounded");
    let hi = range.until.as_deref().unwrap_or("unbounded");
    format!(" Time range resolved to {lo} .. {hi}.")
}

/// The zero that has no filter to blame.
fn no_filters_at_all(ctx: &Context<'_>) -> NoResults {
    let query = ctx.query.map(str::trim).filter(|q| !q.is_empty());
    let retry = retry_body(ctx, None);
    let Some(query) = query else {
        // No query, no filters, and still nothing: the index itself is the answer.
        return NoResults {
            message: format!(
                "0 results across {} documents in {} indexed sessions, with no query and no \
                 filters applied. Nothing narrowed this and nothing matched, which means this \
                 index is empty or holds nothing this tool can return. Run `session-search \
                 index` to build it; `search_sessions` with no arguments shows what it holds.",
                ctx.corpus.docs, ctx.corpus.sessions
            ),
            narrowest_filter: None,
            narrowest_value: None,
            why: None,
            action: RetryAction::Rephrase,
            legal_values: Vec::new(),
            retry,
        };
    };
    let terms: Vec<&str> = query.split_whitespace().collect();
    let (rephrased, how) = rephrase(query, &terms);
    let retry = retry_with_query(ctx, &rephrased);
    let message = format!(
        "0 results for {query} across {} documents in {} indexed sessions, no filters applied.\n\
         The query itself matched nothing. Three things that commonly cause this:\n\
         \x20 - Words are ANDed by default. {query} requires every one of {n} terms in the same \
         document;\n\x20   retry with the rarest term alone.\n\
         \x20 - A leading `-` is negation. `cargo build --release` asks for documents that do NOT \
         contain\n\x20   `release`; quote anything with a flag in it: \"cargo build --release\".\n\
         \x20 - This index covers only Claude Code session transcripts on this machine. If the \
         work happened\n\x20   elsewhere, it is not here, and no query will find it.\n\
         {how}: {}",
        ctx.corpus.docs,
        ctx.corpus.sessions,
        compact(&retry),
        n = terms.len(),
    );
    NoResults {
        message,
        narrowest_filter: None,
        narrowest_value: None,
        why: None,
        action: RetryAction::Rephrase,
        legal_values: Vec::new(),
        retry,
    }
}

/// The query a rephrase retry should carry, and the sentence that introduces it.
///
/// The message three lines above this one explains that a leading `-` negates a term and tells
/// the caller to quote the query. Handing back the negated term as "the single most distinctive
/// term" contradicts that in the one field a client acts on without reading: the prose is skimmed
/// and the retry is sent. So a query carrying a flag retries as the whole query, quoted — the
/// repair the message already recommends — and only a query that cannot be quoted again without
/// nesting its own quotes falls back to a term, with the negation stripped so the retry cannot
/// ask for the absence of the thing it is looking for.
fn rephrase(query: &str, terms: &[&str]) -> (String, &'static str) {
    let negated = |t: &&str| t.starts_with('-') && t.len() > 1;
    if terms.iter().any(negated) {
        if !query.contains('"') {
            return (
                format!("\"{query}\""),
                "Retry with the whole query quoted, so the flag is text and not a negation",
            );
        }
        let positive: Vec<&str> = terms.iter().copied().filter(|t| !negated(t)).collect();
        let pick = rarest_term(if positive.is_empty() {
            terms
        } else {
            &positive
        });
        return (
            // Edge punctuation goes with it: half of a `"quoted phrase"` is not a term, and a
            // stray quote would make the retry a syntax error rather than a narrower query.
            pick.trim_matches(|c: char| !c.is_alphanumeric())
                .to_string(),
            "Retry with the most distinctive term that is not negated",
        );
    }
    (
        rarest_term(terms).to_string(),
        "Retry with the single most distinctive term",
    )
}

/// The most distinctive term of a query, as a proxy for the rarest.
///
/// Longest wins, ties broken by first occurrence. Term frequency would be the real answer and
/// costs a dictionary lookup per term against a query that already returned nothing; length is
/// the standard cheap proxy and is deterministic, which is what makes the suggestion testable.
fn rarest_term<'a>(terms: &[&'a str]) -> &'a str {
    terms
        .iter()
        .copied()
        .max_by_key(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).len())
        .unwrap_or("")
}

/// A ready-to-send arguments object for the same tool.
///
/// Built from the request's parts rather than by serializing the request and deleting a key:
/// `Filters` carries `#[serde(default)]`, not `skip_serializing_if`, so a round-trip emits every
/// one of its fields as nulls and empty arrays — a "ready-to-send retry" nobody would send.
pub fn retry_body(ctx: &Context<'_>, ranked: Option<&Ranked>) -> Value {
    let drop = ranked
        .filter(|r| r.action != RetryAction::Fix)
        .map(|r| r.filter);
    let fix = ranked
        .filter(|r| r.action == RetryAction::Fix)
        .and_then(|r| r.fix_to.as_ref().map(|to| (r.filter, to)));
    // One filter written in two fields: dropping `turn_of` alone would hand back a `turn_seq`
    // with no path, which every front end refuses — a "ready-to-send" retry that cannot be sent.
    let dropped = |name: &str| match drop {
        Some("turn_of") => name == "turn_of" || name == "turn_seq",
        Some(one) => name == one,
        None => false,
    };
    let mut map = serde_json::Map::new();
    if let Some(q) = ctx.query.map(str::trim).filter(|q| !q.is_empty()) {
        map.insert("query".into(), json!(q));
    }
    for applied in applied_filters(ctx.filters, &ctx.time_range) {
        if dropped(&applied.name) {
            continue;
        }
        let value = match fix {
            Some((name, to)) if name == applied.name => to.clone(),
            _ => applied.value,
        };
        map.insert(applied.name, value);
    }
    for (key, value) in &ctx.extra {
        map.insert((*key).to_string(), value.clone());
    }
    Value::Object(map)
}

fn retry_with_query(ctx: &Context<'_>, query: &str) -> Value {
    let mut body = retry_body(ctx, None);
    if let Some(map) = body.as_object_mut() {
        map.insert("query".into(), json!(query));
    }
    body
}

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters(mutate: impl FnOnce(&mut Filters)) -> Filters {
        let mut f = Filters::default();
        mutate(&mut f);
        f
    }

    fn corpus() -> Corpus {
        Corpus {
            sessions: 412,
            docs: 190_233,
            newest_ts: Some("2026-09-10T11:20:14Z".into()),
        }
    }

    fn ctx<'a>(query: Option<&'a str>, f: &'a Filters, corpus: &'a Corpus) -> Context<'a> {
        Context {
            tool: "search_turns",
            query,
            filters: f,
            time_range: resolve_time_range_at(f, fixed_now()).expect("dates parse"),
            extra: Vec::new(),
            corpus,
        }
    }

    /// 2026-09-10T11:20:14Z, the instant the worked examples in the retry policy use.
    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(1_789_039_214_000).expect("representable")
    }

    #[test]
    fn a_bad_date_names_the_field_before_the_index_is_touched() {
        let f = filters(|f| f.since = Some("yesterday-ish".into()));
        let err = resolve_time_range_at(&f, fixed_now()).expect_err("must not resolve");
        assert_eq!(err.field, "since");
        // The plain field name, never a flag: this serves a tool call, not a shell.
        assert!(!format!("{err:#}").contains("--since"));
    }

    #[test]
    fn a_bare_day_resolves_inclusive_at_both_ends() {
        let f = filters(|f| {
            f.since = Some("2026-09-03".into());
            f.until = Some("2026-09-05".into());
        });
        let range = resolve_time_range_at(&f, fixed_now()).expect("dates parse");
        assert_eq!(range.since.as_deref(), Some("2026-09-03T00:00:00Z"));
        // The last instant of the 5th, not its midnight — the whole reason `Edge` exists.
        assert_eq!(range.until.as_deref(), Some("2026-09-05T23:59:59Z"));
    }

    #[test]
    fn a_relative_span_is_echoed_as_the_absolute_window_it_resolved_to() {
        let f = filters(|f| f.since = Some("7d".into()));
        let range = resolve_time_range_at(&f, fixed_now()).expect("parses");
        assert_eq!(range.since.as_deref(), Some("2026-09-03T11:20:14Z"));
        let applied = applied_filters(&f, &range);
        // Both: the span as sent, and what it meant on this call.
        assert_eq!(render_value(&applied[0].value), "7d");
        assert_eq!(applied[0].resolved.as_deref(), Some("2026-09-03T11:20:14Z"));
    }

    #[test]
    fn an_illegal_kind_is_dropped_first_and_the_retry_corrects_it() {
        // The statically provable case: `kind` outranks even `tool_input`, because an illegal
        // value cannot have matched and nothing else needs to be considered.
        let f = filters(|f| {
            f.kind = Some("toolcall".into());
            f.tool_input = vec!["command=cargo".into()];
            f.since = Some("7d".into());
        });
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.filter, "kind");
        assert_eq!(ranked.action, RetryAction::Fix);
        assert_eq!(ranked.fix_to, Some(json!("tool_call")));

        let corpus = corpus();
        let c = ctx(Some("cargo"), &f, &corpus);
        let envelope = build(&c, 0, Vec::new());
        let none = envelope.no_results.expect("zero carries a suggestion");
        assert!(
            none.message
                .contains("`kind` accepts only `message` or `tool_call`")
        );
        assert!(none.message.contains("Retry with kind=tool_call"));
        assert_eq!(none.legal_values, vec!["message", "tool_call"]);
        // The retry keeps every other filter and corrects the one that was provably wrong.
        assert_eq!(
            none.retry,
            json!({
                "query": "cargo",
                "tool_input": ["command=cargo"],
                "kind": "tool_call",
                "since": "7d",
            })
        );
    }

    #[test]
    fn an_illegal_role_says_a_tool_call_is_an_assistant() {
        // The trap the corpus actually sets: `role: "tool"` is the natural way to ask for tool
        // calls and matches nothing, because a tool call's role is `assistant`.
        let f = filters(|f| f.role = Some("tool".into()));
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.filter, "role");
        assert_eq!(ranked.action, RetryAction::Fix);
        assert!(
            ranked.why.contains("use\n                 `kind`")
                || ranked.why.contains("use `kind`")
        );
    }

    #[test]
    fn the_case_wrong_program_is_corrected_by_the_retry_rather_than_deleted_from_it() {
        // The worked example from the issue: a program name reproduced from memory with the
        // wrong case, inside a project scope and a time window that must both survive the retry.
        // Dropping `program` answers a much wider question — every document that merely mentions
        // the query — and reads as an answer to the one that was asked; `program: ["cargo"]`
        // answers the question itself.
        let f = filters(|f| {
            f.project = Some("/home/user/code/other-tool".into());
            f.program = vec!["Cargo".into()];
            f.since = Some("7d".into());
        });
        let corpus = corpus();
        let c = ctx(Some("build failure"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert!(
            none.message.starts_with(
                "0 results. Filters applied: project=/home/user/code/other-tool, program=Cargo, \
                 since=7d. Time range resolved to 2026-09-03T11:20:14Z .. unbounded."
            ),
            "{}",
            none.message
        );
        assert_eq!(none.narrowest_filter.as_deref(), Some("program"));
        assert_eq!(none.action, RetryAction::Fix);
        assert!(
            none.message.contains("Retry with program=cargo"),
            "{}",
            none.message
        );
        // Open vocabulary: there is a correction to send and no set to enumerate, so the two
        // are not the same field.
        assert!(none.legal_values.is_empty());
        assert_eq!(
            none.retry,
            json!({
                "query": "build failure",
                "project": "/home/user/code/other-tool",
                "program": ["cargo"],
                "since": "7d",
            })
        );
    }

    #[test]
    fn a_program_carrying_its_arguments_is_cut_back_to_the_program() {
        // The second measured case. `cargo build` is a command line, not a program name, and
        // `bash_cmd.program` stores the head of it; dropping the filter answers "everything
        // mentioning release", which is a different question with sixteen times the results.
        let f = filters(|f| f.program = vec!["cargo build".into()]);
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.action, RetryAction::Fix);
        assert_eq!(ranked.fix_to, Some(json!(["cargo"])));

        // And a value with nothing to repair is still dropped: a correction is only offered
        // where the value itself says how it is wrong.
        let f = filters(|f| f.program = vec!["cargo".into()]);
        assert_eq!(narrowest(&f).expect("set").action, RetryAction::Drop);
    }

    #[test]
    fn a_lower_cased_tool_is_respelled_and_a_tool_nobody_knows_is_left_alone() {
        // `tool` runs the other way from `program`: the transcript writes `Bash`, so the repair
        // is a capitalisation and it can only come from a list. The list is a spelling aid, not
        // a legal set — the vocabulary is open, so an unrecognised name must be dropped rather
        // than "corrected" into something this module invented.
        let f = filters(|f| f.tool = vec!["bash".into(), "Read".into()]);
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.action, RetryAction::Fix);
        assert_eq!(ranked.fix_to, Some(json!(["Bash", "Read"])));

        let f = filters(|f| f.tool = vec!["mcp__thing__do".into()]);
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.action, RetryAction::Drop);
        assert_eq!(ranked.fix_to, None);
    }

    #[test]
    fn a_contradiction_drops_the_flag_and_keeps_the_filter_that_carries_the_intent() {
        // `agent_type` lives only on sidechain documents and `no_sidechains` deletes every one
        // of them, so the pair matches nothing. Dropping `agent_type` — the value the caller
        // actually asked for — returns zero again and costs a second retry to reach a query
        // with neither filter; dropping the flag answers the question in one call.
        let f = filters(|f| {
            f.agent_type = Some("Explore".into());
            f.no_sidechains = true;
        });
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.filter, "no_sidechains");
        assert_eq!(ranked.contradicts, Some("agent_type"));

        let corpus = corpus();
        let c = ctx(Some("limit"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert_eq!(none.narrowest_filter.as_deref(), Some("no_sidechains"));
        // The message names both halves, because a flag that is wide on its own is only fatal
        // beside the filter it cancels.
        assert!(
            none.message.contains("agent_type=Explore"),
            "{}",
            none.message
        );
        assert!(
            none.message
                .contains("Retry without no_sidechains, keeping agent_type"),
            "{}",
            none.message
        );
        assert_eq!(
            none.retry,
            json!({ "query": "limit", "agent_type": "Explore" })
        );

        // The other contradictory pair, where neither half carries a value: the exclusion goes
        // and the positive selection stays.
        let f = filters(|f| {
            f.no_sidechains = true;
            f.sidechains_only = true;
        });
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.filter, "no_sidechains");
        assert_eq!(ranked.contradicts, Some("sidechains_only"));
    }

    #[test]
    fn scope_is_held_back_until_nothing_else_is_left() {
        // `project` outranks nothing: every other filter is dropped before it, and when it is
        // the last one the sentence says the retry answers a different question.
        for f in [
            filters(|f| {
                f.project = Some("/p".into());
                f.no_sidechains = true;
            }),
            filters(|f| {
                f.project = Some("/p".into());
                f.kind = Some("message".into());
            }),
        ] {
            assert_ne!(narrowest(&f).expect("set").filter, "project");
        }

        let f = filters(|f| f.project = Some("/home/user/code/other-tool".into()));
        let corpus = corpus();
        let c = ctx(Some("SIGBUS"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert!(
            none.message.contains(
                "Dropping it does not widen this question, it answers a different one — results \
                 will come from other repositories on this machine."
            ),
            "{}",
            none.message
        );
        assert!(
            none.message
                .contains("Retry across all projects: {\"query\":\"SIGBUS\"}")
        );
        assert_eq!(none.retry, json!({ "query": "SIGBUS" }));
    }

    #[test]
    fn the_time_window_drops_after_every_content_filter_and_before_the_project() {
        let f = filters(|f| {
            f.project = Some("/p".into());
            f.since = Some("7d".into());
            f.until = Some("now".into());
        });
        assert_eq!(narrowest(&f).expect("set").filter, "since");
        let f = filters(|f| {
            f.project = Some("/p".into());
            f.until = Some("now".into());
        });
        assert_eq!(narrowest(&f).expect("set").filter, "until");
    }

    /// The filters this order deliberately does not rank, and why. See [`narrowest`].
    const NOT_RANKED: &[&str] = &["all_records"];

    #[test]
    fn the_drop_order_is_exhaustive_and_stops_at_the_first_filter_set() {
        // Every filter, alone, ranks as itself: the order cannot skip one and silently blame a
        // filter the caller never sent.
        //
        // Coverage is checked against `Filters` itself, not against a count written here. The
        // previous version of this test ended in `assert_eq!(cases.len(), 18)`, which is a
        // statement about this list and not about the struct — so `all_records`, `turn_of` and
        // `turn_seq` arrived on `main`, went unranked, and this test passed. A field added to
        // `Filters` now fails the set comparison at the end, naming itself.
        let cases: Vec<(&str, Filters)> = vec![
            (
                "turn_of",
                filters(|f| {
                    f.turn_of = Some("/p/abc.jsonl".into());
                    f.turn_seq = Some(12);
                }),
            ),
            ("tool_input", filters(|f| f.tool_input = vec!["k=v".into()])),
            (
                "tool_output",
                filters(|f| f.tool_output = vec!["boom".into()]),
            ),
            ("program", filters(|f| f.program = vec!["cargo".into()])),
            ("min_thinking", filters(|f| f.min_thinking = Some(1))),
            ("lang", filters(|f| f.lang = vec!["rust".into()])),
            (
                "agent_type",
                filters(|f| f.agent_type = Some("Explore".into())),
            ),
            ("model", filters(|f| f.model = Some("claude-opus-5".into()))),
            ("branch", filters(|f| f.branch = Some("main".into()))),
            ("tool", filters(|f| f.tool = vec!["Bash".into()])),
            ("session", filters(|f| f.session = Some("b20208d8".into()))),
            ("sidechains_only", filters(|f| f.sidechains_only = true)),
            ("errors_only", filters(|f| f.errors_only = true)),
            ("no_sidechains", filters(|f| f.no_sidechains = true)),
            ("kind", filters(|f| f.kind = Some("message".into()))),
            ("role", filters(|f| f.role = Some("assistant".into()))),
            ("since", filters(|f| f.since = Some("7d".into()))),
            ("until", filters(|f| f.until = Some("now".into()))),
            ("project", filters(|f| f.project = Some("/p".into()))),
        ];
        let mut covered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (name, f) in &cases {
            let fields = crate::search::testkit::filter_fields_set(f);
            assert!(
                fields.contains(*name),
                "the case for `{name}` does not set it: {fields:?}"
            );
            assert_eq!(narrowest(f).expect("one filter is set").filter, *name);
            covered.extend(fields);
        }
        covered.extend(NOT_RANKED.iter().map(|s| (*s).to_string()));
        assert_eq!(
            covered,
            crate::search::testkit::filter_field_names(),
            "every field of `Filters` is either ranked here or listed in NOT_RANKED with a \
             reason at its place in the order"
        );

        // And the full set ranks as the narrowest of all of them.
        let mut all = Filters::default();
        for (_, f) in &cases {
            merge(&mut all, f);
        }
        // Every filter at once contains both contradictory pairs, and a pair that provably
        // matches nothing outranks a filter that is merely narrow: no width of `tool_input`
        // can rescue a request that also asks for sidechains and excludes them.
        let ranked = narrowest(&all).expect("set");
        assert_eq!(ranked.filter, "no_sidechains");
        assert_eq!(ranked.contradicts, Some("sidechains_only"));

        // With the flags settled, the turn address wins: it names one turn of one file, which
        // no phrase over the whole corpus can be narrower than.
        all.no_sidechains = false;
        assert_eq!(narrowest(&all).expect("set").filter, "turn_of");

        // And with the address gone, the phrase filter — `kind`/`role` are legal here, so the
        // statically provable case does not fire either.
        all.turn_of = None;
        all.turn_seq = None;
        assert_eq!(narrowest(&all).expect("set").filter, "tool_input");
    }

    #[test]
    fn a_turn_address_outranks_every_other_filter_and_the_retry_drops_both_halves() {
        // `turn_of` + `turn_seq` name one turn of one file — ten to fifty documents — so nothing
        // that selects a set can be narrower, and a zero beside one is almost always "that turn
        // does not say this" rather than "that filter is wrong".
        let f = filters(|f| {
            f.turn_of = Some("/home/u/.claude/projects/p/abc.jsonl".into());
            f.turn_seq = Some(12);
            f.tool_input = vec!["command=cargo".into()];
            f.project = Some("/home/u/code".into());
        });
        let ranked = narrowest(&f).expect("a filter is set");
        assert_eq!(ranked.filter, "turn_of");
        assert_eq!(ranked.action, RetryAction::Drop);
        // The one thing a model holding a turn reference most needs told, said where it will be
        // read: the tool that returns the turn is `get_turn`, not this filter.
        assert!(ranked.why.contains("get_turn"), "{}", ranked.why);

        let corpus = corpus();
        let c = ctx(Some("cargo"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        // Both halves are echoed: an address is what the caller sent and what they must be able
        // to compare the answer against.
        assert!(
            none.message
                .contains("turn_of=/home/u/.claude/projects/p/abc.jsonl"),
            "{}",
            none.message
        );
        assert!(none.message.contains("turn_seq=12"), "{}", none.message);
        // And the retry drops both. A retry that kept `turn_seq` would be an ordinal with no
        // path — refused by every front end, which is a suggestion nobody can send.
        assert_eq!(
            none.retry,
            json!({
                "query": "cargo",
                "tool_input": ["command=cargo"],
                "project": "/home/u/code",
            }),
            "{}",
            none.message
        );
    }

    #[test]
    fn all_records_is_echoed_as_applied_but_is_never_blamed_for_the_zero() {
        // It is the only filter that widens: it brings the apparatus records back into scope, so
        // it can turn a zero into a hit and never the other way round. Blaming it would tell the
        // caller to drop the one thing holding the search open.
        let f = filters(|f| f.all_records = true);
        assert!(
            narrowest(&f).is_none(),
            "a filter that only widens cannot be the narrowest cause of a zero"
        );

        // Echoed all the same, and that is not decoration: `retry_body` rebuilds the retry from
        // the echo, so a filter missing there is a filter the retry silently drops — here, a
        // suggested call that searches *less* of the index than the one it is answering.
        let f = filters(|f| {
            f.all_records = true;
            f.program = vec!["Cargo".into()];
        });
        assert_eq!(narrowest(&f).expect("program is set").filter, "program");
        let corpus = corpus();
        let c = ctx(Some("cargo"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert_eq!(
            none.retry,
            json!({
                "query": "cargo",
                "program": ["cargo"],
                "all_records": true,
            }),
            "{}",
            none.message
        );
    }

    fn merge(into: &mut Filters, from: &Filters) {
        if from.project.is_some() {
            into.project = from.project.clone();
        }
        into.tool.extend(from.tool.iter().cloned());
        into.tool_input.extend(from.tool_input.iter().cloned());
        into.tool_output.extend(from.tool_output.iter().cloned());
        into.lang.extend(from.lang.iter().cloned());
        into.program.extend(from.program.iter().cloned());
        into.min_thinking = into.min_thinking.or(from.min_thinking);
        into.branch = into.branch.clone().or_else(|| from.branch.clone());
        into.model = into.model.clone().or_else(|| from.model.clone());
        into.role = into.role.clone().or_else(|| from.role.clone());
        into.kind = into.kind.clone().or_else(|| from.kind.clone());
        into.session = into.session.clone().or_else(|| from.session.clone());
        into.agent_type = into.agent_type.clone().or_else(|| from.agent_type.clone());
        into.since = into.since.clone().or_else(|| from.since.clone());
        into.until = into.until.clone().or_else(|| from.until.clone());
        into.all_records |= from.all_records;
        into.turn_of = into.turn_of.clone().or_else(|| from.turn_of.clone());
        into.turn_seq = into.turn_seq.or(from.turn_seq);
        into.errors_only |= from.errors_only;
        into.no_sidechains |= from.no_sidechains;
        into.sidechains_only |= from.sidechains_only;
    }

    #[test]
    fn no_filters_at_all_blames_the_query_and_suggests_one_term() {
        let f = Filters::default();
        let corpus = corpus();
        let c = ctx(Some("cargo build release"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert_eq!(none.narrowest_filter, None);
        assert_eq!(none.action, RetryAction::Rephrase);
        assert!(none.message.starts_with(
            "0 results for cargo build release across 190233 documents in 412 indexed sessions, \
             no filters applied."
        ));
        assert!(none.message.contains("requires every one of 3 terms"));
        assert!(none.message.contains("A leading `-` is negation."));
        assert_eq!(none.retry, json!({ "query": "release" }));
    }

    #[test]
    fn a_query_carrying_a_flag_retries_the_quoted_query_never_the_negated_term() {
        // The message explains that a leading `-` negates a term and says to quote the query.
        // The retry is the field a client sends without reading the prose, so handing back
        // `--release` — the term the message just warned about — is worse than no retry at all:
        // it matches every document that does not say "release" and warns about nothing.
        let f = Filters::default();
        let corpus = corpus();
        let c = ctx(Some("cargo build --release"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert_eq!(
            none.retry,
            json!({ "query": "\"cargo build --release\"" }),
            "{}",
            none.message
        );
        assert!(
            none.message.contains(
                "Retry with the whole query quoted, so the flag is text and not a \
                           negation"
            ),
            "{}",
            none.message
        );

        // A query that already carries quotes cannot be quoted again without nesting them, so
        // it falls back to a term — and the term is never the negated one.
        let c = ctx(Some("\"cargo build\" --release"), &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert_eq!(none.retry, json!({ "query": "build" }), "{}", none.message);
    }

    #[test]
    fn an_empty_index_says_so_rather_than_blaming_a_query_nobody_sent() {
        let f = Filters::default();
        let corpus = Corpus::default();
        let c = ctx(None, &f, &corpus);
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert!(
            none.message.contains("this index is empty"),
            "{}",
            none.message
        );
        assert_eq!(none.retry, json!({}));
    }

    #[test]
    fn a_non_empty_answer_carries_the_echo_but_never_a_retry() {
        // The other half of the contract: a suggestion beside a page of hits trains a caller to
        // stop reading the envelope, which is precisely when the zero-hit one stops working.
        let f = filters(|f| f.tool = vec!["Bash".into()]);
        let corpus = corpus();
        let c = ctx(Some("cargo"), &f, &corpus);
        let envelope = build(&c, 7, vec!["a warning".into()]);
        assert!(envelope.no_results.is_none());
        assert_eq!(envelope.applied_filters.len(), 1);
        assert_eq!(envelope.warnings, vec!["a warning".to_string()]);
    }

    #[test]
    fn the_retry_carries_tool_specific_arguments_through() {
        // `aggregate` without `field` is not a request. A retry that dropped it would be
        // unsendable, which is the same as no retry at all.
        let f = filters(|f| f.program = vec!["cargo".into()]);
        let corpus = corpus();
        let mut c = ctx(None, &f, &corpus);
        c.tool = "aggregate";
        c.extra = vec![("field", json!("tool_input.file_path"))];
        let none = build(&c, 0, Vec::new())
            .no_results
            .expect("zero carries a suggestion");
        assert_eq!(none.retry, json!({ "field": "tool_input.file_path" }));
    }
}
