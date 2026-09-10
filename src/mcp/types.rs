//! Every request and response the five tools speak, and the envelope they all carry.
//!
//! Two rules govern this module, both of them wire-format facts rather than taste:
//!
//! 1. **No bare collection is ever a response.** `Json<Vec<T>>` serialises `structuredContent`
//!    as an array, which the MCP spec says must be an object; strict clients reject it. Every
//!    response here is a named struct with the collection as one field.
//! 2. **Every response carries an [`Envelope`].** Issue #28: "a zero-hit response must carry the
//!    filters actually applied, the zero, and a retry suggestion with the narrowest filter
//!    dropped. A model that receives `{hits: []}` will tell the user nothing happened last
//!    week." The envelope is that, plus the engine's own warnings, on every answer — not only
//!    the empty ones, because "which filters did you actually apply" is a question a non-empty
//!    answer raises just as often.
//!
//! Request structs flatten [`Filters`] rather than nesting it: the retry bodies the envelope
//! hands back are flat objects (`{"query":"SIGBUS","project":"…","since":"7d"}`), and a caller
//! that has to re-nest them will get it wrong. Flattening is also what makes the 18 filter
//! descriptions land at the top level of the input schema, where a model reads them.
//!
//! Both halves of every payload are counted twice on the wire: rmcp's `Json<T>` writes the value
//! into `structuredContent` *and* a text block carrying the same JSON. Byte budgets here are
//! therefore worth double what they look like.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::search::{FacetResult, Filters, SnippetSource, SortBy};

// ---------------------------------------------------------------------------
// the shared envelope
// ---------------------------------------------------------------------------

/// One filter, as the server actually applied it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppliedFilter {
    /// The filter's name, spelled as the request spells it (`tool_input`, never `--tool-input`).
    pub name: String,
    /// The value as it arrived. A repeatable filter arrives as an array.
    pub value: serde_json::Value,
    /// For `since` and `until` only: the same value resolved against the server's clock, as
    /// RFC3339. A relative span means a different window on every call, so the span alone is
    /// not a report of what was searched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

/// The time window a request actually searched, in absolute instants.
///
/// Resolved in the tool layer, before the index is touched, for two reasons that both matter:
/// an unreadable date becomes `invalid_params` up front instead of an `anyhow` surfacing from
/// deep inside `search()`, and the echo is only possible at all if somebody resolved it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TimeRange {
    /// Inclusive lower bound, RFC3339. `null` when `since` was not set — the window is open.
    pub since: Option<String>,
    /// Inclusive lower bound in epoch milliseconds, for arithmetic.
    pub since_ms: Option<i64>,
    /// Inclusive upper bound, RFC3339. `null` when `until` was not set.
    ///
    /// A bare `YYYY-MM-DD` resolves to the last millisecond of that day here, matching what the
    /// index-side range query does with the same value.
    pub until: Option<String>,
    /// Inclusive upper bound in epoch milliseconds.
    pub until_ms: Option<i64>,
}

/// What to do about a zero-hit answer. See `envelope::build` for the ranking behind it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NoResults {
    /// The whole thing as prose, ready to act on. This is the field to read first.
    pub message: String,
    /// The filter the ranking blames, or `null` when no filter was set and the query itself
    /// matched nothing.
    pub narrowest_filter: Option<String>,
    /// That filter's value, as it arrived.
    pub narrowest_value: Option<serde_json::Value>,
    /// Why this filter and not another — the mechanism that makes it the narrowest, in one
    /// sentence.
    pub why: Option<String>,
    /// What to do with it: `drop` to widen, or `fix` when the value is provably outside a
    /// closed vocabulary and the legal set is known.
    pub action: RetryAction,
    /// The legal values, when `action` is `fix`. Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legal_values: Vec<String>,
    /// A ready-to-send arguments object for the same tool, with the narrowest filter dropped
    /// (or corrected). Send it as-is before rewriting the query.
    pub retry: serde_json::Value,
}

/// Whether the retry drops the narrowest filter or corrects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetryAction {
    /// Widen: send the retry without that filter.
    Drop,
    /// The value is outside a closed vocabulary and could never have matched. The retry carries
    /// a legal value in its place.
    Fix,
    /// No filter was set. The query itself matched nothing, and the retry narrows it to its
    /// most distinctive term.
    Rephrase,
}

/// What the server did with a request, beside answering it.
///
/// Present on every response, empty parts and all. A caller that only ever sees an envelope when
/// something went wrong learns to skip it, and then misses it on the call that mattered.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    /// Every filter that was set, in `Filters` declaration order, with `since`/`until` carrying
    /// their resolved instants. Filters this tool cannot answer are listed here too — see
    /// `warnings`, which names them.
    pub applied_filters: Vec<AppliedFilter>,
    /// The resolved absolute time window. Quote this, never the relative span you sent.
    pub time_range: TimeRange,
    /// What the search noticed that the counts cannot say: a `word:value` term read as a JSON
    /// subpath, a similarity seed that fell outside the tuning, a grouped page that came up
    /// short, a filter a session listing cannot answer. Prose, meant to be read.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Present exactly when the answer is empty. Never a bare zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_results: Option<NoResults>,
}

/// The corpus, as the server reports it in `instructions` and in a no-filter zero.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    pub sessions: usize,
    pub docs: u64,
    /// The newest transcript timestamp, RFC3339, or `None` when nothing is indexed.
    pub newest_ts: Option<String>,
}

// ---------------------------------------------------------------------------
// documents on the wire
// ---------------------------------------------------------------------------

/// One document, in the shape `format::doc_json` pins.
///
/// A newtype over `serde_json::Value` rather than a struct of 30 fields, because `doc_json` is
/// already the one definition of that shape and a second copy here would drift from it — and
/// because `raw`, the original JSONL line, is withheld there. `schemars` would give a bare
/// `Value` the schema `true`; this says `object` and describes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocValue(pub serde_json::Value);

impl JsonSchema for DocValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Document".into()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        concat!(module_path!(), "::DocValue").into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "description": "One indexed document: `doc_id`, `kind` (`message` or `tool_call`), \
                `seq`, `turn_seq`, `session_id`, `agent_id`, `agent_type`, `uuid`, `timestamp`, \
                `project`, `git_branch`, `role`, `model`, `tool_name`, `tool_use_id`, \
                `tool_input`, `bash_cmd`, `is_error`, `is_sidechain`, `body`, `tool_output` and \
                the indexed halves `text`/`code`. The raw transcript line is never included."
        })
    }
}

/// A turn reduced to its shape: one line per document, no tool output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnSkeleton {
    /// One line per document, in transcript order: the prompt, the prose, and each tool call's
    /// signature ending in `-> ok`, `-> no result`, or `-> error:` and the first line of what
    /// failed.
    pub lines: Vec<String>,
    /// Documents the byte budget left out. Greater than zero means this skeleton stops in the
    /// middle of its turn.
    pub dropped: usize,
    /// Rendered size of `lines`, newlines included.
    pub bytes: usize,
    /// Documents of the turn this skeleton was rendered from.
    ///
    /// Fewer than `docs_in_turn` means the *document* cap bit before the byte budget was ever
    /// reached, so `dropped` describes only the part that was fetched. The two caps are
    /// independent and a skeleton that reported one of them would read as complete.
    pub shown: usize,
    /// Documents the turn holds in full. A sidechain is a single turn covering an entire
    /// subagent transcript, so this is routinely far larger than `shown`.
    pub docs_in_turn: usize,
    /// `docs_in_turn > shown`, said out loud so nobody has to compare two numbers to notice.
    pub truncated: bool,
}

/// The pair that addresses one turn. Pass it back exactly as it was returned.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnRef {
    /// Absolute path of the transcript file the turn lives in. Half of the address, and not
    /// optional: `turn_seq` is an ordinal within one file.
    pub source_path: String,
    /// The turn's ordinal within `source_path`.
    pub turn_seq: u64,
}

// ---------------------------------------------------------------------------
// search_turns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SearchTurnsRequest {
    /// Free text. Bare words are ANDed, `"quoted phrases"` are exact, `AND`/`OR`/`NOT` and
    /// `field:value` work, and a leading `-` negates — so quote anything containing a flag.
    /// Searches the prose, the code, the tool output, the markdown headings, the tool inputs
    /// and a per-document context header carrying the session title and the turn's opening
    /// prompt. Omit it to browse a filter on its own, and set `sort` when you do: relevance is
    /// meaningless without a query.
    pub query: Option<String>,
    #[serde(flatten)]
    pub filters: Filters,
    /// How many turns to return. Turns, not documents — hits sharing a turn are collapsed into
    /// one. Twenty skeletons is roughly eleven kilobytes; that is the budget to think in.
    pub limit: usize,
    /// How many turns to skip, for paging. Turns, not documents — and the response counts
    /// documents, so `total_documents` is not the bound to page against: hits collapse into
    /// turns and there are always fewer turns than that. The end of the results is `returned`
    /// coming back smaller than `limit`. Page past the end and you get an ordinary empty
    /// answer with no zero-hit explanation attached, because nothing about the query was
    /// wrong — check `offset` before rewriting anything.
    pub offset: usize,
    /// Hit order. `relevance` is the default and the only one that means anything with a query;
    /// `newest` and `oldest` are for browsing a filter on its own, where every document scores
    /// the same and the resulting order would otherwise be whatever the segments happened to
    /// hold.
    pub sort: SortBy,
    /// Also search assistant thinking blocks. Off by default, and it only works if the index
    /// was built with thinking included.
    pub include_thinking: bool,
    /// Characters of snippet around the match. The skeleton is the body of the answer; this is
    /// the one highlighted fragment beside it.
    pub snippet_chars: usize,
}

impl Default for SearchTurnsRequest {
    fn default() -> Self {
        SearchTurnsRequest {
            query: None,
            filters: Filters::default(),
            limit: DEFAULT_TURN_LIMIT,
            offset: 0,
            sort: SortBy::default(),
            include_thinking: false,
            snippet_chars: DEFAULT_SNIPPET_CHARS,
        }
    }
}

/// Turns to return when the caller does not say. Twenty skeletons at ~555 bytes each.
pub const DEFAULT_TURN_LIMIT: usize = 20;
/// Snippet width, matching the CLI's.
pub const DEFAULT_SNIPPET_CHARS: usize = 240;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchTurnsResponse {
    /// The ranked turns, best first.
    pub turns: Vec<TurnHit>,
    /// Documents matching the query and filters. **Not** the number of turns: grouping
    /// collapses the page, not the match set, so this is larger than `turns.len()` whenever a
    /// turn matched in more than one document.
    pub total_documents: usize,
    /// How many turns this page actually carries.
    pub returned: usize,
    pub elapsed_ms: u64,
    pub envelope: Envelope,
}

/// One turn, as a skeleton and the address to drill into it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnHit {
    /// The address of this turn. Pass it to `get_turn` verbatim.
    #[serde(flatten)]
    pub turn: TurnRef,
    /// The best-scoring document of the turn — the anchor the snippet was cut from, and a
    /// `doc_id` `get_turn` also accepts.
    pub doc_id: String,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    /// The anchor document's timestamp, RFC3339.
    pub timestamp: Option<String>,
    pub score: f32,
    /// The matched fragment, with `**` around the matched terms.
    pub snippet: String,
    /// Which stored body the snippet was cut from. Not always the field the query matched:
    /// with nothing to highlight it falls back to the head of whichever body the document has.
    /// The four read as different claims — what the turn said, a snippet it quoted, what a
    /// command printed, what the model reasoned privately.
    pub snippet_field: SnippetSource,
    /// Other documents of this turn that matched the same query. `0` means the anchor was the
    /// only one.
    pub collapsed: u64,
    /// The turn's shape. Tool output is not here; that is the point.
    pub skeleton: TurnSkeleton,
}

// ---------------------------------------------------------------------------
// get_turn
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct GetTurnRequest {
    /// The transcript file, from a `search_turns` result. Required with `turn_seq`.
    pub source_path: Option<String>,
    /// The turn's ordinal within `source_path`. Required with `source_path`.
    pub turn_seq: Option<u64>,
    /// A document reference instead of the pair: a `doc_id`, a record uuid, a `tool_use_id`, or
    /// `SESSION:SEQ` / `SESSION:AGENT:SEQ` — any by unambiguous prefix. Resolves to that
    /// document's turn. Give this or the pair, not both.
    pub doc_id: Option<String>,
    /// Also return this many turns before the one addressed, in the same file. Reading a
    /// session backwards.
    pub before: usize,
    /// Also return this many turns after the one addressed, in the same file.
    pub after: usize,
    /// Cap on documents returned per turn. One prompt can open a turn of hundreds of tool
    /// calls, and a sidechain transcript is a single turn covering an entire subagent run, so
    /// this bites on real transcripts — `docs_in_turn` says when it did.
    pub max_docs: usize,
    /// Per-document byte budget for tool results. A result longer than this is cut and marked;
    /// `get_output` is the way to read one in full.
    pub max_doc_bytes: usize,
}

impl Default for GetTurnRequest {
    fn default() -> Self {
        GetTurnRequest {
            source_path: None,
            turn_seq: None,
            doc_id: None,
            before: 0,
            after: 0,
            max_docs: DEFAULT_TURN_DOCS,
            max_doc_bytes: DEFAULT_DOC_BYTES,
        }
    }
}

/// Documents of one turn returned when the caller does not say.
pub const DEFAULT_TURN_DOCS: usize = 60;
/// Per-document tool-result budget inside `get_turn`, in bytes.
pub const DEFAULT_DOC_BYTES: usize = 4_000;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetTurnResponse {
    /// The addressed turn, plus any neighbours `before`/`after` asked for, in `turn_seq` order.
    pub turns: Vec<TurnDocuments>,
    pub envelope: Envelope,
}

/// One turn in full: its documents, and what the two caps left out.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnDocuments {
    #[serde(flatten)]
    pub turn: TurnRef,
    pub session_id: String,
    pub agent_id: Option<String>,
    /// The documents, in `seq` order.
    pub docs: Vec<DocValue>,
    /// How many are here.
    pub shown: usize,
    /// How many the turn holds in full. Greater than `shown` means the document cap bit and
    /// this is the head of the turn, not the turn.
    pub docs_in_turn: usize,
    /// `docs_in_turn > shown`, said out loud so nobody has to compare two numbers to notice.
    pub truncated: bool,
    /// How many documents had a tool result cut to `max_doc_bytes`. Each such document carries
    /// its own marker; this is the count, so a caller can tell one truncated result from forty.
    pub docs_with_truncated_output: usize,
}

// ---------------------------------------------------------------------------
// get_output
// ---------------------------------------------------------------------------

/// `Default` is the identity slice: no address, no slicing, `max_bytes` left to
/// [`crate::slice::DEFAULT_MAX_BYTES`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct GetOutputRequest {
    /// The call to read, as a `doc_id` from a `get_turn` document, a record uuid, or a
    /// `SESSION:SEQ` reference — any by unambiguous prefix. Give this or `tool_use_id`.
    /// It must address a tool call, and a `search_turns` result does not: the `doc_id` on a
    /// result addresses the turn's opening document, usually the prompt, and the skeleton
    /// lines beneath it carry no id of their own. So the route from a search hit is
    /// `get_turn` first — the documents it returns are the ones with a `tool_use_id` — and
    /// this tool second. Addressing a message rather than a call is an error naming the
    /// document you actually hit, not an empty output.
    pub doc_id: Option<String>,
    /// The call to read, by the `tool_use_id` the transcript gave it — the field of that name
    /// on a `get_turn` document, spelled as the host wrote it (`toolu_…`), not a name you can
    /// derive from a session id. Give this or `doc_id`.
    pub tool_use_id: Option<String>,
    /// Number of lines to take from the start of the output. Applied before `max_bytes`.
    #[schemars(description = "\
Take the first N lines. This is the right default for a failure: the line that names it — \
`error[E0433]: failed to resolve` — is nearly always at the top and the frames beneath it are \
context. Combines with `tail`; the two ends are returned with an explicit marker for the gap \
between them, never silently joined.")]
    pub head: Option<usize>,
    /// Number of lines to take from the end of the output. Applied before `max_bytes`.
    #[schemars(description = "\
Take the last N lines. This is the right default for a build or a test run: the summary, the \
count of failures and the exit status are at the bottom, under thousands of lines of progress. \
Combines with `head`.")]
    pub tail: Option<usize>,
    /// Return only lines matching this regex, with `context` lines either side.
    #[schemars(description = "\
A regular expression, in the `regex` crate's syntax; only matching lines are returned, each with \
`context` lines either side. Use it when you know what you are looking for in a large output and \
neither end will have it — a specific test name, a file path, a status code. Matching is applied \
one line at a time, so `^` and `$` anchor to a line without the multiline flag and no pattern can \
match across a line break. Case-sensitive like grep(1); write `(?i)` for otherwise. A pattern \
that does not compile is rejected with the syntax error, before any output is read — it never \
degrades to zero matches, because a caller told only \"0 matched\" would report a log full of \
errors as clean. Applied before `max_bytes`, so a grep that matches thousands of lines can still \
be truncated; the response says how many lines matched and how many were returned, so a capped \
grep never reads as a complete one.")]
    pub grep: Option<String>,
    /// Lines of context either side of each `grep` match. Ignored without `grep`.
    pub context: usize,
    /// Byte budget for the returned slice. The slice is cut to fit and the response reports what
    /// was dropped.
    #[schemars(description = "\
Hard cap on the bytes returned, applied last, after `head`/`tail`/`grep` have chosen the lines. \
Exists because tool outputs are unbounded in practice — a 200 KB build log is ordinary and \
spilled results can be larger — and an unbudgeted fetch spends a context window on progress \
bars. When the cap bites, the slice is cut at a line boundary and the response says how many \
bytes and lines were dropped. Prefer narrowing with `head`, `tail` or `grep` over raising this: \
a bigger budget returns more of the same noise, a better slice returns the answer.")]
    pub max_bytes: Option<usize>,
}

/// Whether there is an output at all, and why not when there is not.
///
/// Three states, not two. A call that was interrupted and a call that printed nothing are
/// different facts about what happened, and an empty string reports them identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    /// The result is indexed and `output` carries it (sliced).
    Present,
    /// The call completed and its result was empty.
    Empty,
    /// No result ever reached the index for this call: it was interrupted, denied, or never
    /// answered. `output` is empty and means nothing.
    NoResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetOutputResponse {
    /// The document this output came from.
    pub doc_id: String,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    /// The transcript's own failure flag for this call — not a text scan of the output.
    pub is_error: bool,
    /// The address of the turn this call sits in, for reading around it.
    #[serde(flatten)]
    pub turn: TurnRef,
    /// Whether there is an output at all. Read this before reading `output`.
    pub state: OutputState,
    /// The slice, with `[... N lines omitted ...]` markers where lines were skipped. Two ends
    /// of a log are never silently joined.
    pub output: String,
    /// Bytes of the original output, before slicing.
    pub total_bytes: usize,
    /// Lines of the original output, before slicing.
    pub total_lines: usize,
    /// Original lines that matched `grep`, or `null` when no `grep` was given. `0` and `null`
    /// are different answers: the first says the pattern found nothing, the second says nothing
    /// was asked.
    pub matched_lines: Option<usize>,
    /// Lines of the original present in `output`. Marker lines are not counted.
    pub returned_lines: usize,
    /// `total_lines - returned_lines`.
    pub dropped_lines: usize,
    /// Whether the byte cap bit.
    pub truncated: bool,
    /// Whether the cap cut inside a line rather than between two. A slice that ends mid-token
    /// cannot be parsed as whole lines of JSON or a whole file path.
    pub cut_mid_line: bool,
    /// True only when `output` is the entire original. The one question to ask before
    /// summarising a slice as if it were the whole log.
    pub complete: bool,
    pub envelope: Envelope,
}

// ---------------------------------------------------------------------------
// search_sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SearchSessionsRequest {
    #[serde(flatten)]
    pub filters: Filters,
    /// How many sessions to return, most recent last-activity first.
    pub limit: usize,
}

impl Default for SearchSessionsRequest {
    fn default() -> Self {
        SearchSessionsRequest {
            filters: Filters::default(),
            limit: DEFAULT_SESSION_LIMIT,
        }
    }
}

/// Sessions returned when the caller does not say.
pub const DEFAULT_SESSION_LIMIT: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchSessionsResponse {
    /// The matching sessions, most recently active first.
    pub sessions: Vec<SessionRow>,
    /// How many matched before `limit` cut the list.
    pub total: usize,
    /// How many are here.
    pub returned: usize,
    pub envelope: Envelope,
}

/// One session, as `sessions.json` records it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionRow {
    /// Pass this to `search_turns` as the `session` filter to read what was said.
    pub session_id: String,
    /// Set on a subagent transcript, which records its PARENT's `session_id`.
    pub agent_id: Option<String>,
    /// The subagent's type, on a sidechain.
    pub agent_type: Option<String>,
    /// The session's summary title, when one was written.
    pub title: Option<String>,
    /// The first human prompt of the session, truncated.
    pub first_prompt: Option<String>,
    /// The `cwd` the session ran in.
    pub project: Option<String>,
    pub git_branch: Option<String>,
    /// The transcript file. Half of a turn address, so a `get_turn` can be aimed here.
    pub source_path: String,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
    /// Conversational turns: human prompts, assistant API messages and system records.
    /// Compaction records and attachments are excluded.
    pub messages: u64,
    pub tool_calls: u64,
}

// ---------------------------------------------------------------------------
// aggregate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AggregateRequest {
    /// Field to count. A declared fast field, or any JSON path such as `tool_input.file_path` or
    /// `bash_cmd.program`.
    #[schemars(description = "\
The field whose values are counted. Required — there is no default worth having, and omitting \
it is rejected as `unknown field \"\"` rather than counting something arbitrary. Two kinds are \
accepted.

Declared fast fields — the ones the schema names:
  `tool_name`   which tools were used
  `project`     which repositories the work happened in
  `model`       which model answered
  `git_branch`  which branches
  `role`        user / assistant / system / attachment
  `kind`        message / tool_call
  `agent_type`  which subagent types ran
  `entrypoint`  how the session was started
  `code_lang`   which languages were quoted in fences
Also aggregatable, and useful: `session_id` (which sessions did X — a set-membership question a \
ranked list answers only by accident), `agent_id`, `slug`, `version`, `permission_mode`, and the \
numeric fields `is_error`, `is_sidechain`, `is_meta`, `seq`, `turn_seq`, `thinking_tokens`. A \
numeric field buckets as its decimal spelling, so a boolean one comes back as the values `\"0\"` \
and `\"1\"`, never `false` and `true`.

JSON paths — anything under `tool_input`, plus `bash_cmd.program` and `bash_cmd.args`. \
Subpaths are DYNAMIC: `tool_input.file_path` works without being declared anywhere, and so does \
`tool_input.pattern`, `tool_input.command`, `tool_input.timeout` or any other parameter a tool \
actually wrote. This is what answers 'which files were we unable to read' — a question with no \
ranking of documents behind it at all, only a list of distinct values and their counts. \
`bash_cmd.program` answers 'what did we actually run' without needing a tool of its own.

Three of these are MULTI-VALUED — `code_lang`, `bash_cmd.program` and `bash_cmd.args` — because \
one message can hold several fences and one shell one-liner can invoke several programs. On \
those, a single document lands in several buckets, so the bucket counts total VALUES, not \
documents, and can exceed `matching_docs`. Read `docs_with_value` for the document count.

An unknown field name, or a JSON subpath under a field that is not JSON, is a hard error rather \
than an empty result, and the error lists the whole countable vocabulary — one wrong guess buys \
you the right answer.")]
    pub field: String,
    /// Restrict the counted set to documents matching this free-text query. Same grammar as
    /// `search_turns`. Omit it to count across everything the filters keep.
    pub query: Option<String>,
    #[serde(flatten)]
    pub filters: Filters,
    /// How many buckets to return. What falls outside them is still counted: `other_docs` for
    /// the documents behind the truncated values, `hidden_values` for roughly how many distinct
    /// values you were not shown — so a small `top` is honest rather than lossy. Read
    /// `other_docs: 0` narrowly: it means no value was truncated away, NOT that every matching
    /// document is in a bucket. Documents carrying no value for this field at all are in
    /// neither, and `matching_docs` minus `docs_with_value` is how many.
    pub top: usize,
    /// Also search assistant thinking blocks when applying `query`.
    pub include_thinking: bool,
}

impl Default for AggregateRequest {
    fn default() -> Self {
        AggregateRequest {
            field: String::new(),
            query: None,
            filters: Filters::default(),
            top: DEFAULT_FACET_TOP,
            include_thinking: false,
        }
    }
}

/// Buckets returned when the caller does not say.
pub const DEFAULT_FACET_TOP: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AggregateResponse {
    /// The buckets and the four counts that make them readable.
    pub facet: FacetResult,
    /// True when the values barely repeat — `distinct` approaches `docs_with_value` — so what
    /// you have is a sample of a long tail rather than a distribution. Whole shell commands are
    /// the standard case; that field wants `search_turns` instead.
    pub search_shaped: bool,
    /// Roughly how many distinct values did not fit in `facet.values`. `null` when everything
    /// fit.
    pub hidden_values: Option<u64>,
    pub elapsed_ms: u64,
    pub envelope: Envelope,
}
