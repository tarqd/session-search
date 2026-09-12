//! `SearchRequest` -> `SearchResponse`, plus facet aggregation.
//!
//! `Filters` and `SearchRequest` are plain data deliberately: the same structs back the CLI
//! (via `clap::Args`) and, later, the MCP tool parameters (via `serde`).
//!
//! Query shape, in the order the pieces are assembled:
//!
//! * the free-text query goes through `QueryParser` over `text`, `code` and `headings`
//!   (+ `thinking` when opted in, + `tool_input`), so phrases, booleans and `field:value` all
//!   work. `headings` is boosted 2.0: a section title is what the section is *about*, so a
//!   term in one is a better answer than the same term in the middle of a paragraph;
//! * every filter is ANDed on top as a term, prefix (regex) or range query;
//! * facets are a terms aggregation collected in the *same* searcher pass as the hits.
//!
//! There is no fuzzy operator: in Tantivy 0.26 `~` is phrase slop, and `set_field_fuzzy` is
//! deliberately not wired up, so nothing here should advertise `term~1`. A query that will not
//! parse falls back to `parse_query_lenient`, and the errors that fallback discards are logged
//! rather than swallowed — a typo'd field name must not look like an empty corpus.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::{Bound, Range};
use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use tantivy::aggregation::AggregationCollector;
use tantivy::aggregation::agg_req::Aggregations;
use tantivy::collector::{Collector, Count, SegmentCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ExistsQuery, Occur, Query, QueryParser, RangeQuery,
    RegexQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption, OwnedValue, Schema, Term, Value as _};
use tantivy::snippet::{Snippet, SnippetGenerator, collapse_overlapped_ranges};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream};
use tantivy::{DateTime, Score, Searcher, TantivyDocument};

use crate::parse::{Doc, DocKind};
use crate::schema::Fields;
use crate::sessions::FilterError;

/// Wraps the matched span inside a snippet. Plain text on purpose: the snippet travels through
/// JSON output and MCP responses as well as the terminal, so HTML would be wrong everywhere.
///
/// Public because every consumer that re-marks a snippet — the terminal renderer in `format.rs`,
/// the HTML one in `api` — has to agree with this on what a mark looks like. It is the same
/// string either side of the span, so a consumer splits on it rather than matching a pair.
pub const HIGHLIGHT: &str = "**";
const HL_PREFIX: &str = HIGHLIGHT;
const HL_SUFFIX: &str = HIGHLIGHT;

/// The schema name of the date field. `TopDocs::order_by_fast_field` takes a *name*, not the
/// `Field` handle the rest of this module passes around.
const TIMESTAMP_FIELD: &str = "timestamp";

/// How much more a term in a markdown heading is worth than the same term in a paragraph.
const HEADING_BOOST: Score = 2.0;

/// Weight of a match in the `context_text` header. See `build_query`.
const CONTEXT_BOOST: Score = 0.3;

/// Documents fetched per requested turn when [`SearchRequest::group_by_turn`] is on.
///
/// A turn is 10–50 documents, but only the ones that *matched* compete for its slot, and the
/// worst case — one turn owning the whole page — is the one this has to survive. Eight is
/// generous for a message-level query and cheap regardless: `collector_limit` clamps the ask to
/// the size of the index, and a document that never anchors a hit costs one stored-field read
/// and no snippet work at all.
const GROUP_FANOUT: usize = 8;

#[derive(Debug, Clone, Default, clap::Args, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Filters {
    /// Project path; matches by prefix on a path boundary, so `-p ~/code` catches subdirectories
    /// but not `~/code-other`. `~` is expanded.
    #[arg(short = 'p', long, value_name = "PATH")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Project directory. Prefix match, aware of path boundaries: `/home/u/code` matches \
`/home/u/code` and `/home/u/code/sub`, and never the sibling `/home/u/code-other`. A trailing \
slash is ignored. Give an absolute path — this is the `cwd` each record recorded, never the \
mangled project-directory name, and a `~` is expanded against the server's HOME, which is not \
necessarily yours. This is the filter to drop last on a retry: dropping it does not widen the \
question, it answers a different one.")
    )]
    pub project: Option<String>,
    /// Tool name; repeatable, OR. Exact and case-sensitive — `Bash`, not `bash`.
    #[arg(short = 't', long, value_name = "NAME")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Tool names, ORed together. Exact term match and case-sensitive as the transcript spells them: \
`Bash`, `Read`, `Edit`, `Write`, `Grep`, `Glob`, `WebFetch`, `Task`. `bash` and `BASH` match \
nothing and do not error. If you are unsure what this corpus contains, call `aggregate` on \
`tool_name` first — that is one call and it returns the exact vocabulary.")
    )]
    pub tool: Vec<String>,
    /// Tool parameter filter as `key=value`, e.g. `--tool-input command=cargo`; repeatable, ANDed.
    #[arg(long = "tool-input", value_name = "KEY=VALUE")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Filters on one parameter a tool was called with, written `key=value`; repeatable and ANDed. \
The key is any parameter name the tool actually wrote — subpaths are dynamic, so \
`file_path=/src/main.rs` works without `file_path` being declared anywhere in the schema, and so \
does `timeout=600000` or `pattern=TODO`. The value is matched both as text terms and as the \
whole raw value, so paths, quoted phrases and numbers all match the way they were indexed. \
This is the narrowest filter in the set and it has two independent ways to be wrong: the key may \
never appear on any tool, and the value may not be spelled the way it was recorded — both are \
silent zeroes. To learn either, `aggregate` on `tool_input.<key>` and read the buckets. Use the \
free-text query instead when you want 'this string appeared somewhere in the call', since a \
`tool_input` match is an exact one on a single named parameter.")
    )]
    pub tool_input: Vec<String>,
    /// Phrase the tool's *output* must contain, e.g. `--tool-output "No such file"`; repeatable,
    /// and ANDed.
    #[arg(long = "tool-output", value_name = "TEXT")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
An exact phrase the tool's OUTPUT must contain — what came back, not what the tool was asked to \
do. Repeatable and ANDed. This is a phrase query over the code analyzer: the words must appear \
adjacent and in order, and nothing is stemmed, so `compiling` does not find `compile` here. One \
extra or missing word ends the match set. Case, unlike the words, does not matter: the analyzer \
folds it, so `ENOENT` and `enoent` are the same phrase — this is the one filter here that is not \
case-sensitive, where `tool`, `program` and `branch` all are. Use it for the literal text of an \
error you have already seen (`error[E0433]`, `No such file or directory`); use the free-text \
query when you only half-remember the wording, since a bare query already searches tool output.")
    )]
    pub tool_output: Vec<String>,
    /// Fenced-code language, as written in the info string (`rust`, `bash`); repeatable, OR.
    /// Lowercased before matching, so `Rust` and `rust` are the same fence — `rs` is not.
    #[arg(long, value_name = "LANG")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Fenced-code languages, ORed. Matched against the info string of a markdown code fence exactly \
as written, after lowercasing — `Rust` and `rust` are the same filter, `rs` is a different one \
and matches nothing if the corpus writes rust. Only messages that contain a fenced block \
carry any value at all, so this filter excludes every tool call and every unfenced message. \
Multi-valued: one message with a rust fence and a bash fence carries both.")
    )]
    pub lang: Vec<String>,
    /// Only turns where the model spent at least N thinking tokens. Works even where the
    /// thinking text itself was stripped before it reached disk, which is the case for remote
    /// and web sessions.
    #[arg(long, value_name = "N")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Minimum thinking tokens: an inclusive lower bound on the token count the model spent reasoning \
before answering. Use it to find the moments a session stopped and thought hard, which is a good \
proxy for where the difficult decisions were made. It survives where the reasoning itself does \
not: remote and web sessions strip the thinking text before it reaches disk but keep the count. \
Note this is close to a presence filter — the count is attached to exactly one document per API \
message, so even `min_thinking: 1` cuts the corpus to a small minority, and combining it with \
another narrow filter usually returns zero. The thinking TEXT is only searchable if the index \
was built to include it.")
    )]
    pub min_thinking: Option<u64>,
    /// The program that ran; exact match, case-sensitive. Any simple command in a Bash script,
    /// e.g. `--program cargo`; repeatable, OR.
    #[arg(long, value_name = "NAME")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
The program that ran; exact match, case-sensitive. Matched against `bash_cmd.program`, which is \
indexed with the raw tokenizer, so the value is compared whole and byte for byte: `cargo` is a \
hit, and `Cargo`, `cargo build` and `/usr/bin/cargo` are silent zeroes. Repeatable, ORed. \
Use this instead of putting the program's name in the free-text query. `query: \"cargo\"` also \
matches `Cargo.toml`, the flag `--cargo-flag`, a directory named `cargo` in some path, and every \
sentence that merely mentions cargo; this filter matches only commands that actually invoked it. \
Two limits worth knowing: the field exists only where the shell grammar parsed the command, so a \
command it rejected is invisible here; and it is multi-valued — one pipeline contributes every \
simple command in it, so `grep` matches `ls | grep foo`.")
    )]
    pub program: Vec<String>,
    /// Git branch; exact match on the whole name, so it must be spelled in full.
    #[arg(long, value_name = "BRANCH")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Git branch, exact term match on the whole recorded name. Not a prefix: `claude/` does not match \
`claude/rust-mcp-session-indexing`, and `main` does not match `main-2` either. Slashes are safe — \
the field is stored as one term, so a branch name is not split. A name spelled short or wrong is \
a silent zero; `aggregate` on `git_branch` lists what exists.")
    )]
    pub branch: Option<String>,
    /// Model id; exact match on the full id as recorded, e.g. `claude-opus-4-1-20250805`.
    #[arg(long, value_name = "MODEL")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Model id, exact term match on the full string the transcript recorded — `claude-opus-4-1-\
20250805`, not `opus` and not `claude-opus`. A family name matches nothing. Only assistant \
records carry a model, so this filter also excludes every user prompt, attachment and system \
record. `aggregate` on `model` gives the exact ids in this corpus.")
    )]
    pub model: Option<String>,
    /// One of `user`, `assistant`, `system`, `attachment`. Exact; any other value matches nothing.
    #[arg(long, value_name = "ROLE")]
    #[cfg_attr(feature = "mcp", schemars(
        extend("enum" = ["user", "assistant", "system", "attachment", null]),
        description = "\
Who produced the document. Exactly four legal values:
  `user`       — a typed human prompt. In a subagent transcript these are synthesised by the \
parent, not typed by a person.
  `assistant`  — model output. This INCLUDES tool calls: a tool call's role is `assistant`, so \
`role` cannot isolate them — use `kind` for that.
  `system`     — a system record, including compaction summaries and meta turns.
  `attachment` — injected context: system-reminders, environment blocks, file contents pulled in \
around a prompt.
Any other value — `human`, `tool`, `User`, `ai` — matches nothing and does not error. A zero-hit \
result with `role` set almost always means the value, not the corpus."))]
    pub role: Option<String>,
    /// `message` or `tool_call`. Exact; any other value matches nothing.
    #[arg(long, value_name = "KIND")]
    #[cfg_attr(feature = "mcp", schemars(
        extend("enum" = ["message", "tool_call", null]),
        description = "\
What the document is. Exactly two legal values:
  `message`   — a prompt, an assistant reply, a system record or an attachment.
  `tool_call` — one tool invocation joined with the result that answered it; the call's name and \
inputs are its text, the result is its output.
`toolcall`, `tool`, `ToolCall`, `tool_use` and `tool_result` all match nothing and do not error. \
This filter is weak when right — the two values split the corpus roughly in half — and total when \
wrong, which is why a zero with `kind` set is far more likely a typo than a fact about the \
corpus."))]
    pub kind: Option<String>,
    /// Session id; matches by prefix, so the first block of a uuid is enough.
    #[arg(long, value_name = "SESSION_ID")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Session id, matched by PREFIX — the first block of a uuid (`b20208d8`) is enough, which is what \
anyone pasting an id has to hand. Note two things about session identity: a subagent transcript \
records its PARENT's session id, so filtering by session includes that session's sidechains \
(separate them with `sidechains_only` / `no_sidechains`); and two files can carry the same \
session id after a reset or relocation, which is why a turn is addressed by `source_path` plus \
`turn_seq` and not by session id plus a number.")
    )]
    pub session: Option<String>,
    /// Subagent type, e.g. `Explore`; exact match. Only sidechain documents have one.
    #[arg(long = "agent-type", value_name = "TYPE")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Subagent type, exact and case-sensitive: `Explore`, `workflow-subagent`, and whatever else this \
machine has run. It comes from the attribution on a sidechain's assistant records, so setting it \
implies sidechains — every document with an `agent_type` is inside a subagent transcript, and \
combining this with `no_sidechains` is a guaranteed zero. `aggregate` on `agent_type` lists the \
values.")
    )]
    pub agent_type: Option<String>,
    /// Lower bound, inclusive. RFC3339, `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS]` (UTC), `now`, or a
    /// relative span back from now: `90s`, `30m`, `24h`, `7d`, `2w`.
    #[arg(long, value_name = "WHEN")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Start of the time window, inclusive. Accepted forms:
  RFC3339 with an offset      — `2026-09-03T14:00:00Z`, `2026-09-03T10:00:00-04:00`
  a bare day                  — `2026-09-03`, taken from that day's midnight UTC
  a local-looking timestamp   — `2026-09-03T14:00` or `2026-09-03T14:00:00`, assumed UTC
  the literal `now`
  a span counted back from now — `90s`, `30m`, `24h`, `7d`, `2w`
A bare `YYYY-MM-DD` is inclusive at BOTH ends of the range: `since 2026-09-03 until 2026-09-05` \
covers all three days, up to the last instant of the 5th, rather than stopping at its midnight. \
Anything else is a parse error, not a silent zero — this is one of the few filters that tells you \
when it is wrong. Relative spans are resolved against the server's clock at the moment of the \
call, so the response echoes the resolved absolute range back; quote that range when you report \
a result, never the span you sent.")
    )]
    pub since: Option<String>,
    /// Upper bound. Same grammar as `since`; a bare `YYYY-MM-DD` covers that whole day.
    #[arg(long, value_name = "WHEN")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
End of the time window. Same grammar as `since`: RFC3339, `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS]` \
(assumed UTC), `now`, or a span back from now (`90s`, `30m`, `24h`, `7d`, `2w`). A bare day is \
inclusive — `until 2026-09-05` covers the whole of the 5th, not just its first instant, because \
an upper bound that stopped at midnight would silently drop a day's work. An explicit timestamp \
is an inclusive instant. The response echoes the resolved absolute range; report that, not the \
relative span you sent.")
    )]
    pub until: Option<String>,
    /// Search the whole index, including the records nobody typed and the model did not
    /// write: attachments (system reminders, environment blocks, pasted file contents),
    /// `system` records, and meta turns such as compaction summaries.
    ///
    /// Excluded by default because of what they do to a result list rather than what they
    /// cost to store: measured on this repo's own history they are a fifth of all documents
    /// and a twentieth of the text, and they are dense with the vocabulary every search uses
    /// — paths, tool names, the word "session" — so a plain query returns page after page of
    /// the same boilerplate reminder. They stay indexed, and this brings them back.
    ///
    /// An explicit `--role` overrides the default on its own: asking for `--role attachment`
    /// and getting nothing would be a filter that silently contradicts itself.
    #[arg(long = "all-records")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Search the whole index, including the records nobody typed and the model did not write: \
`attachment` documents (system reminders, environment blocks, the contents of pasted files), \
`system` records, and meta turns such as compaction summaries. All three are excluded by \
default. They are a fifth of the documents here and a twentieth of the text, and they are dense \
with the vocabulary every search uses — paths, tool names, the word `session` — so a plain query \
that kept them would return page after page of the same boilerplate reminder instead of the \
conversation. They stay indexed; this brings them back. Two things follow. An explicit `role` \
already overrides the default on its own, so `role: attachment` returns attachments with or \
without this — a filter that asked for them and was handed none would be contradicting itself. \
And this is the one filter here that cannot cause a zero: it only ever widens, so it is never \
the reason a search came back empty, and it is never wrong to add on a retry. Whether it would \
change an answer is not a guess — `search_turns` returns `hidden`, the count of documents this \
query matched and the default scope refused. `hidden: 0` and `hidden: 340` are the difference \
between a search that found nothing and a search that was not allowed to look, so a non-zero \
`hidden` beside a thin result is the signal to ask again with this on.")
    )]
    pub all_records: bool,
    /// One turn, by the file it is in and its `turn_seq`: everything that happened under one
    /// human prompt, and nothing else.
    ///
    /// Both halves or neither. `turn_seq` is a per-file ordinal and two transcripts can share
    /// a `session_id` (§9), so an ordinal on its own would silently match that position in
    /// every file — the same reason [`crate::context::turn`] takes a path where [`around`]
    /// takes an `Option`.
    #[arg(long = "turn-of", value_name = "SOURCE_PATH", requires = "turn_seq")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Half of one turn address: the transcript file, spelled exactly as the `source_path` a \
`search_turns` hit came back with. `turn_of` and `turn_seq` are two halves of a single filter \
and neither does anything alone — sending one without the other is refused rather than quietly \
ignored, because half an address is not a narrower search but a wider one. Check which tool you \
want before using it: if you are holding a turn from a `search_turns` hit and mean to READ it, \
that is `get_turn`, which takes this same `source_path` and `turn_seq` and returns the turn's \
documents in full. This filter does the other thing — it restricts a *search* to that one turn, \
answering `which documents inside this turn match this query` and nothing else. Use it to count \
inside a turn with `aggregate`, or to find where a phrase sits in a long one; use `get_turn` to \
read it. A zero here means the turn does not contain your query terms, not that the address is \
wrong.")
    )]
    pub turn_of: Option<String>,
    /// The turn's ordinal within `--turn-of`'s file. Both halves or neither; see `--turn-of`.
    #[arg(long = "turn-seq", value_name = "N", requires = "turn_of")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
The other half of a turn address: the turn's ordinal inside `turn_of`'s file, exactly as the \
`turn_seq` a `search_turns` hit came back with. It is a PER-FILE ordinal and never a global id — \
turn 12 exists in every transcript at least twelve turns long, and two transcripts can even \
carry the same `session_id` — so on its own it would match that position in every file, and it \
is refused without `turn_of` rather than doing that silently. Pass the pair back exactly as it \
was returned: do not compute it, do not take one half from one hit and the other half from \
another, and do not use it as a page number. As with `turn_of`, reading a turn you have already \
found is `get_turn`, which takes this same pair; this filter is for searching or counting \
*within* the turn.")
    )]
    pub turn_seq: Option<u64>,
    /// Only failed tool calls.
    #[arg(long)]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Keep only documents flagged as a failed tool call. The flag is the transcript's own: the tool \
host said the call failed, an `Error…` / `InputValidationError…` payload came back, the call was \
interrupted, or permission was denied. It is NOT a text scan — a command that printed the word \
`error` and exited 0 is not flagged, and for that you want a `tool_output` phrase instead. \
Failures are a small minority of the corpus, so this narrows hard; it is the natural pairing with \
`aggregate` for 'what errors did we see' and 'which files could we not read'.")
    )]
    pub errors_only: bool,
    /// Exclude subagent transcripts. Mutually exclusive with `sidechains_only`.
    #[arg(long, conflicts_with = "sidechains_only")]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Exclude subagent (sidechain) transcripts, keeping only the main conversation. Use it when you \
want what the top-level session did rather than what its delegated agents did. Cheap to drop on a \
retry: sidechains are a minority of documents, so it rarely explains a zero on its own — unless \
it was combined with `agent_type` or `sidechains_only`, which contradict it outright.")
    )]
    pub no_sidechains: bool,
    /// Only subagent transcripts. Mutually exclusive with `no_sidechains`.
    #[arg(long)]
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Keep only subagent (sidechain) documents — the transcripts of delegated agents, which live in \
their own files and record the parent's session id. Worth knowing before you drill in: a \
subagent's `user` records are synthesised by the parent and never open a new turn, so an entire \
sidechain transcript is ONE turn. A `get_turn` on a hit from here returns that whole transcript \
and will hit the document cap; prefer reading the skeleton and pulling single outputs with \
`get_output`.")
    )]
    pub sidechains_only: bool,
}

/// What orders the hits.
///
/// Relevance is the default and the only order that means anything for a text query. The two
/// time orders exist for the case a text query does not cover: browsing a filter on its own
/// ("every Bash call in this project"), where BM25 scores every document identically and the
/// resulting order is whatever the segments happened to hold.
///
/// A document with no timestamp is **not** dropped: Tantivy sorts on `Option<T>` and puts
/// `None` last in both directions, so it stays reachable by paging and `total` keeps matching
/// what paging can actually reach. Every record a transcript writes carries a timestamp
/// anyway, so this is a corner — but "the count says 40 and you can only page to 37" is
/// exactly the kind of quiet arithmetic lie this codebase is at pains to avoid, so it is
/// pinned by a test rather than left to be rediscovered.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum SortBy {
    #[default]
    Relevance,
    Newest,
    Oldest,
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
    pub sort: SortBy,
    /// The seed of a "find similar" search, already resolved. `None` is an ordinary search.
    ///
    /// Resolved rather than a raw reference string on purpose: expanding `abc123` into a
    /// document is an index lookup with its own ambiguity error, and a request struct that
    /// carried the string would have to fail that lookup from inside `search()` — where the
    /// caller has no way to tell "your reference was ambiguous" from "your query found
    /// nothing". `cli::run` resolves it with [`resolve_similar`] before building the request.
    pub similar_to: Option<SimilarSource>,
    /// Collapse hits that share a turn into one, keeping the best-scoring member as the anchor.
    ///
    /// Off by default, because it changes what a hit *is*. A message-level query for "why did
    /// the build fail" matches the prompt, the assistant text, the tool call and the result —
    /// four hits describing one moment, and 40% of a `--limit 10` spent on it. Collapsing on
    /// `(source_path, turn_seq)` dedupes that structurally, which is what a diversity rerank
    /// exists to patch after the fact.
    ///
    /// Two things change with it on, each documented where it lands: [`Hit::collapsed`] says
    /// how many other documents of the turn matched, and `offset` counts **turns** rather than
    /// documents — a page of turns cannot be skipped by a number of documents.
    pub group_by_turn: bool,
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
            sort: SortBy::Relevance,
            similar_to: None,
            group_by_turn: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
pub struct FacetCount {
    /// The value itself, exactly as it was indexed.
    pub value: String,
    /// Documents carrying this value. On a multi-valued field one document can be counted in
    /// several buckets, so these do not sum to a document count. See [`FacetResult`].
    pub count: u64,
}

/// A terms aggregation plus the counts needed to read it honestly.
///
/// The bucket list alone is misleading: summing the returned buckets answers "how many documents
/// are in the rows I am showing you", which reads as "how many documents matched" and on
/// `tool_input.command` differs by 50x. Never sum the buckets. `matching_docs` is the total,
/// `docs_with_value` is the coverage, `other_docs` says the buckets are truncated, and `distinct`
/// says whether this field is a distribution at all.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
pub struct FacetResult {
    /// The field these buckets count, echoed back.
    pub field: String,
    /// The top buckets, most frequent first. Never a total — see the counts below.
    pub values: Vec<FacetCount>,
    /// Documents matching the query and filters. **Not** the sum of `values`.
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
How many documents the query and filters matched, in total. This is the denominator for every \
bucket count and it is the ONLY number here you may quote as a total. It is not the sum of the \
returned buckets and will not equal it: buckets stop at the top `n`, documents without a value \
for this field are counted here and appear in no bucket at all, and on a multi-valued field one \
document is counted once here and several times across the buckets. If you find yourself adding \
bucket counts together, the number you wanted was this one.")
    )]
    pub matching_docs: u64,
    /// Of those, the ones that actually carry a value for this field. Counted with an
    /// `ExistsQuery`, not summed from the buckets: a multi-valued field like `code_lang` or
    /// `bash_cmd.program` buckets a document once per value, so the sum counts values and
    /// can exceed `matching_docs`. Documents, not values: one answer with a rust fence and a
    /// bash fence counts once here and twice in `values`.
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
How many of the `matching_docs` carry any value for this field at all — counted directly, not \
summed from the buckets. The gap between this and `matching_docs` is real and often large: a \
`tool_input.file_path` aggregation over a set that includes prompts and Bash calls has a value on \
only the small share of documents that were file reads. Report coverage as \
`docs_with_value of matching_docs`. On a multi-valued field this is DOCUMENTS, not values: one \
answer holding a rust fence and a bash fence counts once here and twice in the buckets.")
    )]
    pub docs_with_value: u64,
    /// Values that fell outside the returned buckets (`sum_other_doc_count`) — one document
    /// per value, except on a multi-valued field, where one document can contribute several.
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
How much of the matching set sits in values that did not make the top `n` buckets. Greater than \
zero means the buckets you were given are a truncated view, and any statement of the form 'the \
only values are…' or 'X accounts for all of them' is false. Say 'the top N are…' and name this \
number. One document per value here, except on a multi-valued field, where one document can \
contribute to several.")
    )]
    pub other_docs: u64,
    /// Approximate count of distinct values (HyperLogLog), over the matching set.
    #[cfg_attr(
        feature = "mcp",
        schemars(description = "\
Roughly how many distinct values exist across the whole matching set, not just the buckets \
returned. Approximate — it is a HyperLogLog estimate, so quote it as 'about N distinct values' \
and never subtract it from anything. Its real job is to tell you what shape of field you are \
looking at: when it approaches `docs_with_value`, the values barely repeat and what you have is a \
sample of a long tail rather than a distribution — whole shell commands are the standard case — \
and the question wants full-text search instead of an aggregation.")
    )]
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

/// Which stored body a [`Hit`]'s snippet was cut from.
///
/// A search spans `text`, `code`, `tool_output` and `thinking` at once and can attribute a hit
/// to any of them, and they read as completely different claims — what a turn said, a snippet
/// it quoted, what a command printed, what the model reasoned privately. A caller that renders
/// them the same way (or labels the snippet with the wrong field, which is what a single opaque
/// string invites) tells the reader something untrue about what matched.
///
/// `Text` also covers the no-highlight fallback's `Doc::body`, which is the same claim — the
/// turn's own words — rendered from the stored body rather than the indexed halves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SnippetSource {
    Text,
    Code,
    ToolOutput,
    Thinking,
}

impl SnippetSource {
    /// The schema field name, which is also the JSON key the API reports it under.
    pub fn as_str(self) -> &'static str {
        match self {
            SnippetSource::Text => "text",
            SnippetSource::Code => "code",
            SnippetSource::ToolOutput => "tool_output",
            SnippetSource::Thinking => "thinking",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub doc: Doc,
    pub score: f32,
    pub snippet: String,
    /// The body [`Hit::snippet`] came from. Not always the field the query matched: with
    /// nothing to highlight the snippet falls back to the head of whichever body the document
    /// has, and this reports that one.
    pub snippet_field: SnippetSource,
    /// Byte ranges into [`Hit::snippet`] covering the matched text *inside* each pair of
    /// [`HIGHLIGHT`] markers this module wrote.
    ///
    /// A consumer that re-marks the snippet cannot recover these by splitting on `**`: bodies
    /// contain `**` of their own (any turn that read a markdown file carries some), and a
    /// splitter cannot tell those from ours. It mis-pairs them and emphasises words the query
    /// never matched — a search tool reporting the wrong answer with total confidence. These
    /// ranges are recorded where the truth is, at the point the markers are written.
    #[serde(default)]
    pub snippet_marks: Vec<Range<usize>>,
    /// Other documents of this hit's turn that matched the same query and were folded into it
    /// by [`SearchRequest::group_by_turn`]. `0` whenever grouping is off.
    ///
    /// Counted against the whole matched set rather than against the fetched page: this is the
    /// number of hits the anchor stands in for, and a count taken from the page would shrink
    /// as the page filled up — the one reading under which "+3 more" is a lie.
    #[serde(default)]
    pub collapsed: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
    /// Documents matching the query and filters. **Not** the number of hits a grouped search
    /// could return: grouping collapses the page, not the match set, and the distinct turns
    /// behind that set are a second pass over all of it for a number nobody pages by.
    /// [`SearchResponse::grouped`] is what tells a caller the two now mean different things.
    pub total: usize,
    pub facets: BTreeMap<String, FacetResult>,
    pub elapsed_ms: u128,
    /// True when [`SearchRequest::group_by_turn`] collapsed the hits: every hit is one turn,
    /// and `total` is still a document count.
    #[serde(default)]
    pub grouped: bool,
    /// What the search noticed about this request that the numbers above cannot say.
    ///
    /// Three outcomes of this index look exactly like an empty corpus from the outside: a
    /// `word:value` term whose root is not a schema field (read as a `tool_input` JSON subpath,
    /// which cannot fail to parse and simply matches nothing), a similarity seed whose every
    /// term fell outside the tuning, and a grouped page that came back short because the
    /// collapse window never reached `limit` distinct turns. Each was logged at WARN and
    /// nothing else, which reaches whoever is reading stderr — never the caller who drew the
    /// wrong conclusion. A zero-hit answer must not read as an authoritative "no".
    ///
    /// `Vec<String>` rather than a typed enum because these are prose advice, not a condition a
    /// caller branches on: every consumer — `info.warnings` on the HTTP envelope, an agent
    /// reading a tool result — renders them as text, so a typed variant would be flattened to a
    /// sentence at the only place it is used. The sentences themselves are the `WARN_*`
    /// constants below, shared verbatim with the `tracing` call that logs them so the two
    /// cannot drift.
    ///
    /// `#[serde(default)]` for the same reason [`SearchResponse::grouped`] carries it: a
    /// response deserialized from an older writer has no such key, and a missing warning list
    /// means "none", not a parse failure.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Documents this query matched and the default scope refused: attachments, `system`
    /// records and meta turns. Zero when `--all-records` or an explicit `--role` is in force,
    /// because nothing was refused.
    ///
    /// Counted rather than inferred. A result list quietly a fifth shorter than the corpus can
    /// support is the kind of omission a reader discovers by not finding something, and
    /// "hidden: 0" and "hidden: 340" are the difference between a search that found nothing
    /// and a search that was not allowed to look.
    #[serde(default)]
    pub hidden: usize,
}

/// A grouped page that ran out of collapse window before it ran out of turns.
///
/// Kept as a constant, not written inline, because it is both logged and returned: the caller
/// that has to act on it (raise `limit`, narrow the query) is not the one reading stderr, and
/// two spellings of the same advice is exactly the drift this crate keeps pinning down.
const WARN_GROUPED_PAGE_SHORT: &str = "grouped page is short: the matched documents cluster into fewer turns than the collapse \
     window reached. Raise --limit, or narrow the query.";

/// Zero hits from a query carrying an unqualified `word:value`. See [`has_unqualified_field_term`].
const WARN_UNQUALIFIED_FIELD_TERM: &str = "no matches: a `word:value` term here was read as a tool_input JSON subpath. \
     If you meant it as text, quote it.";

/// Zero hits from a `--similar-to` seed. See the call site for which knobs decide it.
const WARN_EMPTY_SIMILARITY_SEED: &str = "no matches: every term of the seed turn may have fallen outside the similarity \
     tuning (too rare, too common, too short or too long). Widen the seed with \
     --similar-in text,code,tool_output, or check the filters.";

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
    let scope = scope_of(&req.filters);
    let query = build_scoped_query(index, f, req, scope)?;

    // One extra `Count` over the complement, and only when there is a complement to count.
    // The alternative — subtracting from a second, unscoped total — would have to run the same
    // query twice anyway and would report a difference rather than a set.
    let hidden = if scope == Scope::Conversation {
        let refused = build_scoped_query(index, f, req, Scope::Apparatus)?;
        searcher.search(&refused, &Count)?
    } else {
        0
    };

    // `TopDocs` preallocates a heap of `limit + offset` entries **per segment**, so unclamped
    // user-supplied numbers abort the process — or overflow the addition — before a single
    // document is read. No request can return, or skip past, more documents than the index
    // holds, so that is the ceiling for both.
    // Grouping pages in *turns*, so the document offset the collector applies would skip the
    // wrong thing; the skip moves into the collapse loop below and the collector is asked for a
    // window wide enough to hold `offset + limit` distinct turns. See `GROUP_FANOUT`.
    let (wanted, doc_offset) = if req.group_by_turn {
        (
            req.limit
                .saturating_add(req.offset)
                .saturating_mul(GROUP_FANOUT),
            0,
        )
    } else {
        (req.limit, req.offset.min(searcher.num_docs() as usize))
    };
    let top = TopDocs::with_limit(collector_limit(&searcher, wanted)).and_offset(doc_offset);

    // Facet fields are validated up front so a typo is a clear error rather than a
    // Tantivy-internal one, and so the aggregation rides along in the same pass as the hits.
    let facet_fields: Vec<&str> = req.facets.iter().map(String::as_str).collect();
    for name in &facet_fields {
        validate_agg_field(&schema, name)?;
    }

    // Four arms rather than two: the aggregation has to ride in the *same* searcher pass as
    // the hits, and the two orderings are different collector types, so neither choice can be
    // hoisted out of the other.
    let (top_hits, total, agg) = match req.sort {
        SortBy::Relevance => {
            let top = top.order_by_score();
            if facet_fields.is_empty() {
                let (hits, total) = searcher.search(&query, &(top, Count))?;
                (hits, total, None)
            } else {
                let collector = agg_collector(&facet_fields, req.facet_top);
                let (hits, total, agg) = searcher.search(&query, &(top, Count, collector))?;
                (hits, total, Some(agg))
            }
        }
        SortBy::Newest | SortBy::Oldest => {
            let order = if req.sort == SortBy::Newest {
                tantivy::Order::Desc
            } else {
                tantivy::Order::Asc
            };
            let top = top.order_by_fast_field::<DateTime>(TIMESTAMP_FIELD, order);
            let (hits, total, agg) = if facet_fields.is_empty() {
                let (hits, total) = searcher.search(&query, &(top, Count))?;
                (hits, total, None)
            } else {
                let collector = agg_collector(&facet_fields, req.facet_top);
                let (hits, total, agg) = searcher.search(&query, &(top, Count, collector))?;
                (hits, total, Some(agg))
            };
            // A timestamp is not a relevance score and must not be reported as one; every hit
            // in a time-ordered page scores the same, which is exactly what it means.
            let hits = hits.into_iter().map(|(_, addr)| (0.0, addr)).collect();
            (hits, total, agg)
        }
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

    // One snippet generator per response, per field: each only keeps the query's terms for
    // its own field, and each tokenizes with that field's own analyzer.
    let raw_query = non_empty(req.query.as_deref()).unwrap_or_default();
    let snippet_chars = req.snippet_chars.max(32);
    // The seed text of a similarity search, per field: the *other* source of highlight terms.
    // See `snippet_generator`. Empty for every ordinary search, which is exactly the old
    // behaviour.
    let seed = |field: SimilarField| -> String {
        req.similar_to
            .as_ref()
            .map(|source| source.text_for(field))
            .unwrap_or_default()
    };
    let snippets = snippet_generator(
        &searcher,
        &*query,
        f.text,
        raw_query,
        &seed(SimilarField::Text),
        snippet_chars,
    )
    .ok();
    // A tool call keeps its output in `code`, and a message its snippets, so a hit that landed
    // there has nothing to highlight in `text` — which is most tool-call hits.
    let code_snippets = snippet_generator(
        &searcher,
        &*query,
        f.code,
        raw_query,
        &seed(SimilarField::Code),
        snippet_chars,
    )
    .ok();
    // A doc matched *through* `thinking` stores its body in that field and leaves `text`
    // empty, so without a third generator the one thing `--include-thinking` is paid for is
    // the one thing never shown.
    let thinking_snippets = req
        .include_thinking
        .then(|| {
            snippet_generator(
                &searcher,
                &*query,
                f.thinking,
                raw_query,
                &seed(SimilarField::Thinking),
                snippet_chars,
            )
            .ok()
        })
        .flatten();
    // Likewise for a tool call matched through its result: the call side is a tool name and a
    // command line, and highlighting that instead of the output the query actually hit shows
    // the caller the one part of the document they did not ask about.
    let output_snippets = snippet_generator(
        &searcher,
        &*query,
        f.tool_output,
        raw_query,
        &seed(SimilarField::ToolOutput),
        snippet_chars,
    )
    .ok();

    // `TopDocs::with_limit(0)` panics, so the collector always asks for at least one doc;
    // an explicit `--limit 0` still means "no hits, just totals and facets".
    let top_hits = if req.limit == 0 { Vec::new() } else { top_hits };

    let mut hits = Vec::with_capacity(top_hits.len().min(req.limit));
    // The turns already anchored, in the order the collector handed them over: with grouping on
    // the first document of a turn to arrive is its best-scoring one (or, under `--sort`, its
    // earliest or latest), and every later one is folded into it.
    let mut anchored: BTreeSet<(String, u64)> = BTreeSet::new();
    let mut skipped = 0usize;
    for (score, address) in top_hits {
        let stored: TantivyDocument = searcher.doc(address)?;
        let doc = doc_from_stored(f, &stored);
        if req.group_by_turn {
            // `turn_seq` is a per-file ordinal, so the path is half the key: two transcripts
            // sharing a session id (§9's `resetSessionFile()`) both have a turn #0.
            if !anchored.insert((doc.source_path.clone(), doc.turn_seq)) {
                continue;
            }
            if skipped < req.offset {
                skipped += 1;
                continue;
            }
            if hits.len() >= req.limit {
                break;
            }
        }
        // Prose first, then code, then the tool result, then thinking: the prose is what a
        // reader recognises, and a highlight anywhere beats a head-of-body excerpt with no
        // highlight at all. Each carries the field it came from: the four read as different
        // claims, and a snippet that does not say which it is invites the reader to take the
        // model's private reasoning for something it said out loud.
        let highlight = |g: &Option<SnippetGenerator>, from: SnippetSource| {
            g.as_ref()
                .map(|g| render_snippet(&g.snippet_from_doc(&stored)))
                .filter(|(text, _)| !text.trim().is_empty())
                .map(|(text, marks)| (text, marks, from))
        };
        let (snippet, snippet_marks, snippet_field) = highlight(&snippets, SnippetSource::Text)
            .or_else(|| highlight(&code_snippets, SnippetSource::Code))
            .or_else(|| highlight(&output_snippets, SnippetSource::ToolOutput))
            .or_else(|| highlight(&thinking_snippets, SnippetSource::Thinking))
            .unwrap_or_else(|| {
                // Nothing highlighted: fall back to whichever body this document actually has.
                // A head-of-body excerpt marks nothing, so it carries no ranges — and an empty
                // `snippet_marks` is exactly "nothing matched here", not "the marks were lost".
                let (body, from) = fallback_body(&doc);
                (excerpt(&body, req.snippet_chars), Vec::new(), from)
            });
        hits.push(Hit {
            doc,
            score,
            snippet,
            snippet_field,
            snippet_marks,
            collapsed: 0,
        });
    }

    // Counted after the page is chosen, and only for the anchors on it: one intersection of the
    // query with a two-term turn lookup per hit, driven by the turn's own posting list, which is
    // tens of documents. The alternative — counting members as they stream past — reports the
    // fetch window instead of the match, and `GROUP_FANOUT` would silently become part of the
    // contract.
    if req.group_by_turn {
        for hit in &mut hits {
            hit.collapsed = collapsed_count(&searcher, f, &*query, &hit.doc)?;
        }
    }

    // Every warning below is both logged and carried on the response: the structured fields are
    // for whoever is reading stderr, the sentence is for the caller who would otherwise read a
    // zero-hit page as an authoritative "no". `"{}"` over the shared constant is what keeps the
    // two texts one text.
    let mut warnings: Vec<String> = Vec::new();

    // A turn the fetch window never reached cannot anchor a hit, and with grouping on that is
    // the one way a page comes back short of `--limit` while documents are still matching.
    if req.group_by_turn
        && hits.len() < req.limit
        && anchored.len() < req.limit.saturating_add(req.offset)
        && total > collector_limit(&searcher, wanted)
    {
        tracing::warn!(
            turns = anchored.len(),
            docs_scanned = collector_limit(&searcher, wanted),
            matching_docs = total,
            "{}",
            WARN_GROUPED_PAGE_SHORT
        );
        warnings.push(WARN_GROUPED_PAGE_SHORT.to_string());
    }

    // An unqualified `word:value` is a JSON-subpath lookup, so it cannot fail to parse — it
    // just finds nothing when that subpath does not exist. Zero hits from a query shaped like
    // that is far more often a misread colon than an empty corpus, so say so rather than
    // letting it look like an authoritative "no".
    if total == 0
        && let Some(text) = non_empty(req.query.as_deref())
        && has_unqualified_field_term(&schema, text)
    {
        tracing::warn!(query = %text, "{}", WARN_UNQUALIFIED_FIELD_TERM);
        warnings.push(WARN_UNQUALIFIED_FIELD_TERM.to_string());
    }

    // The second silent-nothing outcome of `MoreLikeThisQuery` (the first is refused in
    // `resolve_similar`): a seed whose every term fell outside the tuning yields a
    // `BooleanQuery` with zero clauses, which matches nothing and reports no error at all. From
    // the outside that is indistinguishable from "nothing in this corpus is similar", so say
    // which knobs decide it rather than letting an over-tuned query look like an answer.
    if total == 0
        && let Some(source) = &req.similar_to
    {
        tracing::warn!(
            doc = %source.doc_id,
            turn = source.turn_seq,
            "{}",
            WARN_EMPTY_SIMILARITY_SEED
        );
        warnings.push(WARN_EMPTY_SIMILARITY_SEED.to_string());
    }

    Ok(SearchResponse {
        hits,
        total,
        facets,
        elapsed_ms: started.elapsed().as_millis(),
        grouped: req.group_by_turn,
        warnings,
        hidden,
    })
}

/// How many distinct turns the query matched.
///
/// Main deliberately did not compute this — "a second pass over all of it for a number nobody
/// pages by". Something pages by it now: the HTTP envelope reports turns as `totalResults`
/// when grouping is on, and every page control in a Search UI response divides that by
/// `resultsPerPage`. A page count derived from a document total while paging in turns is wrong
/// in the one place a user can see it, so the number has to be real.
///
/// Paid only when grouping is on, and only once per response.
pub fn count_turns(
    index: &tantivy::Index,
    f: &Fields,
    req: &SearchRequest,
) -> anyhow::Result<usize> {
    let searcher = index.reader()?.searcher();
    let query = build_query(index, f, req)?;
    let turns = searcher.search(&query, &DistinctTurns)?;
    Ok(turns.len())
}

/// The group key is `(source_path, turn_seq)`, exactly as [`crate::context::turn_query`] reads
/// it: `turn_seq` is a per-file ordinal, and two transcripts can share a `session_id` (§9), so
/// the path is half the key.
type TurnKey = (String, u64);

struct DistinctTurns;

struct DistinctTurnsSegment {
    /// `None` when the segment holds no values for the field, which no index this build writes
    /// can produce. Every document then falls into one key rather than the query failing: a
    /// stale segment is a reason to reindex, not a reason to error.
    paths: Option<tantivy::columnar::StrColumn>,
    turns: tantivy::columnar::Column<u64>,
    /// Keyed by *term ordinal*, which is per-segment and only meaningful until `harvest`.
    /// Resolving each document's path as it is collected would do a dictionary lookup per
    /// matching document instead of one per distinct file.
    seen: HashSet<(u64, u64)>,
}

impl Collector for DistinctTurns {
    type Fruit = HashSet<TurnKey>;
    type Child = DistinctTurnsSegment;

    fn for_segment(
        &self,
        _segment_ord: u32,
        reader: &tantivy::SegmentReader,
    ) -> tantivy::Result<DistinctTurnsSegment> {
        Ok(DistinctTurnsSegment {
            paths: reader.fast_fields().str("source_path")?,
            turns: reader.fast_fields().u64("turn_seq")?,
            seen: HashSet::new(),
        })
    }

    /// Counting, not ranking.
    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Self::Fruit>) -> tantivy::Result<Self::Fruit> {
        let mut merged = HashSet::new();
        for fruit in fruits {
            merged.extend(fruit);
        }
        Ok(merged)
    }
}

/// Sentinel for a document with no `source_path` value. Real ordinals never collide with it,
/// and it keeps such documents in one key rather than silently out of the count.
const MISSING_PATH_ORD: u64 = u64::MAX;

impl SegmentCollector for DistinctTurnsSegment {
    type Fruit = HashSet<TurnKey>;

    fn collect(&mut self, doc: tantivy::DocId, _score: Score) {
        let path = self
            .paths
            .as_ref()
            .and_then(|column| column.term_ords(doc).next())
            .unwrap_or(MISSING_PATH_ORD);
        self.seen.insert((path, self.turns.first(doc).unwrap_or(0)));
    }

    /// Ordinals are per-segment, so they are resolved here — before `merge_fruits` ever sees
    /// them. Merging on raw ordinals would count one turn twice, or two as one, on any index
    /// with more than one segment.
    fn harvest(self) -> Self::Fruit {
        let mut out = HashSet::with_capacity(self.seen.len());
        let mut buf = Vec::new();
        for (path_ord, turn_seq) in self.seen {
            let path = match (&self.paths, path_ord) {
                (_, MISSING_PATH_ORD) => String::new(),
                (Some(column), ord) => {
                    buf.clear();
                    match column.ord_to_bytes(ord, &mut buf) {
                        Ok(true) => String::from_utf8_lossy(&buf).into_owned(),
                        _ => String::new(),
                    }
                }
                (None, _) => String::new(),
            };
            out.insert((path, turn_seq));
        }
        out
    }
}

/// How many *other* documents of this document's turn matched `query`.
///
/// Exact, and deliberately not derived from the hits already in hand: the fetch window is a
/// tuning constant, and a count taken from it would make "+12 more in this turn" mean "+12 that
/// happened to fit", which shrinks as the page grows. `context::turn_query` is the same
/// `(source_path, turn_seq)` intersection a turn window walks, so the two can never disagree
/// about what a turn is.
fn collapsed_count(
    searcher: &Searcher,
    f: &Fields,
    query: &dyn Query,
    doc: &Doc,
) -> anyhow::Result<u64> {
    let in_turn = crate::context::turn_query(f, &doc.source_path, doc.turn_seq);
    let scoped = BooleanQuery::intersection(vec![query.box_clone(), Box::new(in_turn)]);
    let matched = searcher.search(&scoped, &Count)?;
    // The anchor itself is one of them.
    Ok((matched as u64).saturating_sub(1))
}

/// A snippet generator for `field`, driven by the *whole* terms of the query — plus, on a
/// similarity search, the whole terms of the seed text.
///
/// `SnippetGenerator::create` gives every term of the parsed query equal standing and scores a
/// fragment by summing the hits in it, while the `code` analyzer turns one query word into an
/// identifier *and each of its parts*. A paragraph that merely repeats the parts — `user` here,
/// `email` there — therefore outscores the one line that actually holds `userEmail`, and the
/// snippet shows everything except the reason the document matched.
///
/// Keeping only the whole forms fixes it. Every matching document contains them: the parts share
/// the whole's position, so the phrase query the parser builds demands the whole form too.
///
/// `similar` is the second source of terms, and it exists because `MoreLikeThisQuery` reports
/// none. It implements only `weight` and inherits the no-op `Query::query_terms`, so on a
/// `--similar-to` search the loop below finds nothing, `search_fragments` produces no
/// candidates, `render_snippet` returns `""` and every hit silently falls through to a
/// head-of-body excerpt with an empty `snippet_marks`. No error, no warning — just a search
/// tool that stopped saying why anything matched. So the terms are taken from where the truth
/// is: the seed turn's own text for this field, tokenized with this field's analyzer.
///
/// Every filter the similarity query applies is applied here too — the word-length bounds, the
/// document-frequency band and the stop words — because a highlight is a claim about *why* a
/// document came back, and marking `Bash` in a tool call when `bash` is on the stop-word list
/// would be a confident, wrong answer to that question. The one filter that is not applied is
/// `SIMILAR_MAX_QUERY_TERMS`: it keeps the best 32 of the survivors, and which 32 depends on a
/// `tf * idf` ordering this function has no reason to recompute. Keeping a few terms the query
/// dropped costs nothing — they are terms the hit genuinely contains, and the
/// `1 / (1 + doc_freq)` weighting already sorts them to the bottom of the fragment score.
///
/// Pass `""` for an ordinary search, which is exactly the old behaviour.
fn snippet_generator(
    searcher: &Searcher,
    query: &dyn Query,
    field: Field,
    raw_query: &str,
    similar: &str,
    max_num_chars: usize,
) -> anyhow::Result<SnippetGenerator> {
    let tokenizer = searcher.index().tokenizer_for_field(field)?;
    let parts = part_terms(&mut tokenizer.clone(), raw_query);

    let mut terms: BTreeSet<&Term> = BTreeSet::new();
    query.query_terms(&mut |term, _| {
        if term.field() == field {
            terms.insert(term);
        }
    });

    let mut terms_text: BTreeMap<String, Score> = BTreeMap::new();
    let mut weigh = |text: &str, term: &Term| -> anyhow::Result<()> {
        // Same weighting as `SnippetGenerator::create`: a rare term is worth more than a
        // common one, and a term the corpus does not hold at all is worth nothing.
        let doc_freq = searcher.doc_freq(term)?;
        if doc_freq > 0 {
            terms_text.insert(text.to_string(), 1.0 / (1.0 + doc_freq as Score));
        }
        Ok(())
    };

    for term in terms {
        let value = term.value();
        let Some(text) = value.as_str() else {
            continue;
        };
        if parts.contains(text) {
            continue;
        }
        weigh(text, term)?;
    }

    if !similar.is_empty() {
        // The seed text goes through the same whole-versus-part filter as a query does, for the
        // same reason: highlighting `user` and `email` across a paragraph instead of the one
        // `userEmail` that made the document similar shows everything but the answer.
        let seed_parts = part_terms(&mut tokenizer.clone(), similar);
        let mut seeds: BTreeSet<String> = BTreeSet::new();
        tokenizer
            .clone()
            .token_stream(similar)
            .process(&mut |token: &Token| {
                seeds.insert(token.text.clone());
            });
        let doc_frequency = similar_doc_frequency_band(searcher.num_docs());
        for text in seeds {
            if seed_parts.contains(&text) || !is_similarity_term(&text) {
                continue;
            }
            let term = Term::from_field_text(field, &text);
            if !doc_frequency.contains(&searcher.doc_freq(&term)?) {
                continue;
            }
            weigh(&text, &term)?;
        }
    }

    Ok(SnippetGenerator::new(
        terms_text,
        tokenizer,
        field,
        max_num_chars,
    ))
}

/// The terms `analyzer` emits as a *part* of some word of `raw_query` and never as a word in
/// its own right.
///
/// The `code` analyzer emits the whole identifier first at each position and its parts after,
/// so everything past the first token of a position is a part. A part that is also somebody's
/// whole (`create` in `create open_or_create`) is not dropped.
fn part_terms(analyzer: &mut TextAnalyzer, raw_query: &str) -> BTreeSet<String> {
    let mut wholes: BTreeSet<String> = BTreeSet::new();
    let mut parts: BTreeSet<String> = BTreeSet::new();
    let mut seen = None;
    analyzer
        .token_stream(raw_query)
        .process(&mut |token: &Token| {
            if seen == Some(token.position) {
                parts.insert(token.text.clone());
            } else {
                seen = Some(token.position);
                wholes.insert(token.text.clone());
            }
        });
    parts.retain(|part| !wholes.contains(part));
    parts
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
    build_scoped_query(index, f, req, scope_of(&req.filters))
}

fn build_scoped_query(
    index: &tantivy::Index,
    f: &Fields,
    req: &SearchRequest,
    scope: Scope,
) -> anyhow::Result<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    if let Some(text) = non_empty(req.query.as_deref()) {
        // `tool_output` is a default field, not an opt-in one: before it existed the result
        // text lived in `text`, so leaving it out would make a bare query stop matching things
        // it has always matched. `code` and `headings` are default for the same reason —
        // the split moved a message's snippets and titles out of `text`.
        // `context_text` joins them for the reason it exists: a document torn out of a
        // conversation is not retrievable by what it was *for* unless the header is searched
        // by the same bare query as the body.
        let mut default_fields = vec![f.text, f.code, f.headings, f.tool_output, f.context_text];
        if req.include_thinking {
            default_fields.push(f.thinking);
        }
        default_fields.push(f.tool_input);
        let mut qp = QueryParser::for_index(index, default_fields);
        // A heading names the subject of everything under it, so a hit in one outranks the
        // same word buried in a paragraph. 2.0 is enough to reorder two otherwise comparable
        // documents without letting one heading beat a document that matches repeatedly.
        qp.set_field_boost(f.headings, HEADING_BOOST);
        // Below 1.0, and deliberately well below: the header is a claim about what a document
        // was for, the body is what it says. A near-constant prefix repeated over every
        // document of a session would otherwise let scaffolding outrank the one document that
        // genuinely discusses the term — and every document in that session carries it, so
        // without the discount the header decides the ordering of the whole session at once.
        qp.set_field_boost(f.context_text, CONTEXT_BOOST);
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

    // The similarity clause is one more `Occur::Must`, sitting beside the free-text query and
    // every filter — which is the whole reason "more like this, but only in this project, only
    // last month" composes. Nothing downstream learns that one clause came from a document
    // rather than from a word: the collectors, the `Count`, the aggregations, `docs_with_value`
    // and paging all see an ordinary `BooleanQuery`.
    if let Some(source) = &req.similar_to {
        // A searcher rather than a captured count: the term selection needs `doc_freq` per
        // candidate term as well as the corpus size, and both have to be read against the index
        // the query is about to run on. One reader open, on a path that already opens one.
        let searcher = index.reader()?.searcher();
        clauses.push((Occur::Must, similar_query(&searcher, f, source)?));

        if !source.include_source {
            // The seed turn is excluded, and its *whole turn* rather than just the referenced
            // document — because the seed turn is the thing the caller is already looking at.
            // Its siblings share its vocabulary by construction, so without this the first page
            // is "the tool call you just read and the four around it". That is what `show
            // --turn` is for. `--include-source` puts it back, for debugging and for an eval
            // that wants to see where the source lands.
            //
            // Note this is an explicit `MustNot` and not a reliance on the ranking. The usual
            // description of MoreLikeThis — "the source ranks first by construction" — holds
            // for a *single-document* seed, which matches every clause it generated, and fails
            // for a turn-shaped one: no single document of the turn carries the whole union of
            // its terms, and BM25 length normalisation then lets a short document elsewhere
            // outrank all of them. `docs/DESIGN.md` records a measured case.
            //
            // A `MustNot` sets `minimum_number_should_match` to 0 on the outer boolean, which
            // is correct here because the similarity clause is a `Must`: something still has to
            // match positively.
            clauses.push((
                Occur::MustNot,
                Box::new(crate::context::turn_query(
                    f,
                    &source.source_path,
                    source.turn_seq,
                )),
            ));
        }
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
    // `code_lang` is a STRING field holding the info word verbatim, so this is an exact match
    // on the same lowercased token `markdown::split` stored.
    if let Some(q) = any_of(f.code_lang, &lowercased(&flt.lang)) {
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
    ] {
        if let Some(value) = non_empty(value) {
            clauses.push((Occur::Must, term_query(field, value)));
        }
    }

    // Everything in the transcript that is not the conversation, out of the way unless it was
    // asked for. `MustNot` rather than a positive `role` clause so a document with no `role`
    // at all — which the index has never produced, but the field is `Option<String>` — cannot
    // be dropped by a filter nobody set.
    match scope {
        Scope::All => {}
        Scope::Conversation => {
            for excluded in NON_CONVERSATIONAL_ROLES {
                clauses.push((Occur::MustNot, term_query(f.role, excluded)));
            }
            clauses.push((Occur::MustNot, flag_query(f.is_meta, 1)));
        }
        Scope::Apparatus => {
            let mut any: Vec<(Occur, Box<dyn Query>)> = NON_CONVERSATIONAL_ROLES
                .iter()
                .map(|role| (Occur::Should, term_query(f.role, role)))
                .collect();
            any.push((Occur::Should, flag_query(f.is_meta, 1)));
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(any))));
        }
    }

    // `clap` enforces the pair on the command line; over HTTP `dto` refuses a half of it, so
    // by here either both are set or neither is.
    if let (Some(path), Some(turn_seq)) = (non_empty(flt.turn_of.as_deref()), flt.turn_seq) {
        clauses.push((
            Occur::Must,
            Box::new(crate::context::turn_query(f, path, turn_seq)),
        ));
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

    // A boolean query made only of `MustNot` matches nothing in Tantivy — there is no positive
    // set for the negatives to subtract from. The default scope is exactly that shape on a
    // filter-only browse with no query, so it needs a universe to exclude out of; without this
    // line "everything except the apparatus" silently becomes "nothing".
    if !clauses.is_empty() && clauses.iter().all(|(occur, _)| *occur == Occur::MustNot) {
        clauses.push((Occur::Must, Box::new(AllQuery)));
    }

    Ok(match clauses.len() {
        0 => Box::new(AllQuery),
        1 => clauses.pop().expect("checked len").1,
        _ => Box::new(BooleanQuery::new(clauses)),
    })
}

/// Roles that are apparatus rather than conversation. `attachment` is the harness injecting
/// context — system reminders, environment blocks, the contents of a pasted file — and
/// `system` is the harness reporting on itself. Neither was typed and neither was generated.
pub const NON_CONVERSATIONAL_ROLES: &[&str] = &["attachment", "system"];

/// Which half of the index a query is asking about.
///
/// The same clauses build both: [`Scope::Conversation`] pushes the apparatus away with
/// `MustNot`, and [`Scope::Apparatus`] requires exactly what the other one refused, so "how
/// many did that hide" is a `Count` over the complement rather than a second opinion assembled
/// somewhere else and free to disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Conversation,
    Apparatus,
    All,
}

/// The scope a request implies. An explicit `--role` wins outright: asking for
/// `--role attachment` and being handed nothing would be a filter contradicting itself.
pub(crate) fn scope_of(flt: &Filters) -> Scope {
    if flt.all_records || non_empty(flt.role.as_deref()).is_some() {
        Scope::All
    } else {
        Scope::Conversation
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// `--lang Rust` and `--lang rust` mean the same fence: the stored term is lowercase.
fn lowercased(values: &[String]) -> Vec<String> {
    values.iter().map(|v| v.trim().to_lowercase()).collect()
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

/// `~` / `~/rest` against `$HOME`, for a `project` filter typed by a human.
///
/// `pub(crate)` because the shell is not the only front door: the CLI expands it on the way
/// into the index-side filter, and `sessions.json` is filtered by the same `Filters::project`
/// through a different code path (`sessions::SessionMatcher`). A project that matches under
/// `session-search search` and not under `session-search sessions` is a difference nobody would
/// think to look for, so there is one expansion rather than one per surface.
pub(crate) fn expand_tilde(path: &str) -> String {
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

/// A filter value this builder could not read, typed rather than left as a bare `anyhow`.
///
/// [`FilterError`] is the crate's one "the caller sent something unusable" type, and being that
/// type is what the answer depends on: `mcp::from_anyhow` downcasts it into an `invalid_params`
/// and renders everything it cannot classify as an `internal_error`. Those two say opposite
/// things — an `internal_error` tells a model the tool broke and to stop, an `invalid_params`
/// tells it to send a different value — and only the second is true of a misspelled filter. A
/// second error type would be a second arm at that boundary for somebody to forget, and the
/// symptom is silent: the sentence still arrives, labelled as the server's fault.
///
/// `field` is the plain name (`tool_input`), never `--tool-input`. This builder is shared by a
/// CLI that spells it with dashes, an HTTP API that spells it `tool_input=` and an MCP server
/// where no flag exists at all, so baking one spelling in here puts a flag that cannot be typed
/// into two of the three answers. Each front end re-spells `field` in its own dialect, exactly
/// as `cli::session_matcher` re-spells [`FilterError`]'s `since`.
fn filter_error(field: &'static str, message: String) -> anyhow::Error {
    anyhow::Error::new(FilterError {
        field,
        source: anyhow::Error::msg(message),
    })
}

/// `--tool-output TEXT` as a phrase query over what the tool returned. Quoted, so an operator
/// or a stray colon in the text is matched literally rather than reinterpreted as grammar.
fn tool_output_query(index: &tantivy::Index, phrase: &str) -> anyhow::Result<Box<dyn Query>> {
    let phrase = phrase.trim();
    if phrase.is_empty() {
        return Err(filter_error(
            "tool_output",
            "expects a non-empty value".into(),
        ));
    }
    let escaped = phrase.replace('\\', r"\\").replace('"', r#"\""#);
    let qp = QueryParser::for_index(index, Vec::new());
    qp.parse_query(&format!("tool_output:\"{escaped}\""))
        .with_context(|| format!("building a tool-output filter from {phrase:?}"))
}

/// `--tool-input command=cargo` -> a query over the JSON subpath `tool_input.command`.
///
/// Routed through `QueryParser` on purpose: it emits both the tokenized text terms *and* the
/// typed fast-value term, so `path=/tmp/x.rs`, `command="cargo build"` and `timeout=600000`
/// all match the way they were indexed.
fn tool_input_query(index: &tantivy::Index, spec: &str) -> anyhow::Result<Box<dyn Query>> {
    let unreadable = |message: String| filter_error("tool_input", message);
    let (key, value) = spec
        .split_once('=')
        .ok_or_else(|| unreadable(format!("expects KEY=VALUE, got {spec:?}")))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(unreadable(format!("expects a non-empty key, got {spec:?}")));
    }
    if key.contains([' ', '"', ':']) {
        return Err(unreadable(format!(
            "key {key:?} contains a character the query grammar reserves"
        )));
    }
    if value.trim().is_empty() {
        // `tool_input.k:""` parses cleanly and matches nothing, which is indistinguishable
        // from "this value does not occur". Say what actually went wrong instead.
        return Err(unreadable(format!(
            "expects a non-empty value, got {spec:?}"
        )));
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

/// Which end of a `--since` / `--until` range a [`When`] is being resolved for.
///
/// Only a bare `YYYY-MM-DD` needs it — `When::Day` names a whole day, and which instant of that
/// day is meant depends entirely on the end it sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edge {
    Lower,
    Upper,
}

/// [`parse_when`] resolved to a single instant at the requested end of the range.
///
/// The index side never needs this: `date_range` hands Tantivy a `Bound`, so a whole day is
/// `Included(midnight) .. Excluded(midnight + DAY_MS)` and no instant has to stand for the day.
/// Everything that filters `sessions.json` instead of the index — `session-search sessions`,
/// `GET /api/sessions`, and any front end after them — compares two `i64` timestamps and needs
/// one number, so the day has to collapse to an edge: midnight as a lower bound, the last
/// millisecond of the day (`+ DAY_MS - 1`) as an upper one. Both ends are inclusive, which is
/// what makes `--since 2026-09-09 --until 2026-09-09` mean that day rather than nothing.
///
/// It lives here, beside [`parse_when`] and [`DAY_MS`], because a second front end that
/// re-derived the edge is how `--until 2026-09-09` starts meaning "up to midnight" on one
/// surface and "up to 23:59:59.999" on another — a filter silently dropping a day of results
/// with nothing on screen to say so.
pub(crate) fn when_ms(
    raw: &str,
    now: chrono::DateTime<chrono::Utc>,
    edge: Edge,
) -> anyhow::Result<i64> {
    Ok(match parse_when(raw, now)? {
        When::Instant(ms) => ms,
        When::Day(ms) => match edge {
            Edge::Lower => ms,
            Edge::Upper => ms + DAY_MS - 1,
        },
    })
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
/// *value*, so on a multi-valued field like `code_lang` (one entry per fence) or
/// `bash_cmd.program` (one entry per simple command in the script) summing the buckets counts
/// values, and can sail past `matching_docs`.
fn docs_with_value(searcher: &Searcher, query: &dyn Query, field: &str) -> tantivy::Result<u64> {
    // `json_subpaths` matters when the facet *is* a JSON field rather than one of its paths:
    // `tool_input` and `bash_cmd` carry values only inside their subpaths.
    let exists = BooleanQuery::new(vec![
        (Occur::Must, query.box_clone()),
        (
            Occur::Must,
            Box::new(ExistsQuery::new(field.to_string(), true)) as Box<dyn Query>,
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

/// The matched spans wrapped in `**`, taken from the stored `text`, plus the byte ranges of
/// what each pair wraps. HTML escaping (what `Snippet::to_html` does) would corrupt the code
/// and paths these transcripts are full of.
///
/// The ranges are returned rather than left to be re-derived because `**` occurs in the bodies
/// themselves, so the marked string alone no longer says which markers are ours. See
/// [`Hit::snippet_marks`].
fn render_snippet(snippet: &Snippet) -> (String, Vec<Range<usize>>) {
    let fragment = snippet.fragment();
    let ranges = collapse_overlapped_ranges(snippet.highlighted());
    if ranges.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut out = String::with_capacity(fragment.len() + ranges.len() * 4);
    let mut marks = Vec::with_capacity(ranges.len());
    let mut cursor = 0;
    for range in ranges {
        if range.start < cursor || range.end > fragment.len() {
            continue;
        }
        out.push_str(&fragment[cursor..range.start]);
        out.push_str(HL_PREFIX);
        let from = out.len();
        out.push_str(&fragment[range.clone()]);
        marks.push(from..out.len());
        out.push_str(HL_SUFFIX);
        cursor = range.end;
    }
    out.push_str(&fragment[cursor..]);
    (out, marks)
}

/// What to excerpt when nothing highlighted: the prose if there is any, else the code, else
/// the thinking.
///
/// A tool call whose input was empty, and an orphaned tool result, both carry their whole body
/// in `code`; showing a blank line for them would hide the document the search just returned.
fn fallback_body(doc: &Doc) -> (String, SnippetSource) {
    let output = doc.tool_output.as_deref().filter(|s| !s.trim().is_empty());
    // A failed call leads with its result. `--errors-only` carries no free-text query and so
    // lands here every time; the error is the answer, and the command that failed is only
    // context — on a real corpus it sat 1,000–2,400 characters into the body, past a heredoc.
    if doc.is_error
        && let Some(output) = output
    {
        return (output.to_string(), SnippetSource::ToolOutput);
    }
    if !doc.body.trim().is_empty() {
        return (doc.body.clone(), SnippetSource::Text);
    }
    if !doc.text.is_empty() {
        return (doc.text.join("\n"), SnippetSource::Text);
    }
    if !doc.code.is_empty() {
        return (doc.code.join("\n"), SnippetSource::Code);
    }
    if let Some(output) = output {
        return (output.to_string(), SnippetSource::ToolOutput);
    }
    (
        doc.thinking.clone().unwrap_or_default(),
        SnippetSource::Thinking,
    )
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
// find similar
// ---------------------------------------------------------------------------

//  "Find me more turns like this one" is a different question from "find me turns matching
//  these words". The shape of the answer is Tantivy's `MoreLikeThis`: tokenize a source
//  document's own text, keep the terms that discriminate, and OR them into a `BooleanQuery` of
//  `TermQuery`s weighted by `tf * idf`. What this module does *not* do is call
//  `MoreLikeThisQuery` — [`similar_terms`] and [`similar_query`] rebuild that shape here. The
//  vendored 0.26 API has five sharp edges, and the last of them is not tunable from outside.
//
//  1. **`with_document` reads STORED fields, and every one of them.** It calls
//     `searcher.doc(addr)` and walks the stored payload, so for this schema the term pool would
//     include `session_id`, `doc_id`, `source_path`, `seq`, `turn_seq`, `timestamp` and the
//     flags. A `session_id` term's document frequency is exactly the size of the source
//     session — squarely inside the band `min_doc_frequency`/`max_doc_frequency` calls
//     interesting — so `with_document` would hand back the rest of the source session and call
//     it similarity. Naming the fields explicitly is also the only way to seed from a *turn*
//     rather than from one document: `with_document` takes a single `DocAddress` and does not
//     accumulate.
//  2. **`context_text` must be excluded deliberately, not by accident.** It is indexed and not
//     stored, so `with_document` could never see it — but a field-driven seed *can* target it.
//     It must not: the header is near-identical for every document of a session by
//     construction, so its terms would drag the whole source session back, which is the one
//     false positive this feature exists to avoid.
//  3. **JSON fields contribute nothing, silently.** `add_term_frequencies` matches
//     `FieldType::Json` under a `_ => {}` arm, so `tool_input` and `bash_cmd` can never drive
//     similarity through that API. [`SimilarField`] therefore offers only the four text
//     bodies, and nothing in the CLI promises `--similar-in tool_input`.
//  4. **`MoreLikeThisQuery` is opaque to the rest of the query machinery.** It implements only
//     `weight`; it inherits the no-op `query_terms`, so [`snippet_generator`] would find no
//     terms and every hit would fall back to a head-of-body excerpt with no highlight (see the
//     `similar` argument there), and its `weight` *errors* when scoring is disabled — which
//     `--sort newest`, [`docs_with_value`] and every arm of [`facets`] all pass.
//  5. **Its term selection is not reproducible.** `create_score_term` ranks candidate terms
//     with a heap whose `Ord` compares only the score, over a `std::collections::HashMap` whose
//     `RandomState` reseeds per instance — so once a seed offers more candidates than
//     `max_query_terms`, *which* of the equally-scored ones survive is decided by hash order,
//     and differs between two calls in one process against one unchanged index. This is the
//     edge that cannot be worked around from outside, because the cap is the thing that has to
//     be applied deterministically. [`similar_terms`] therefore does the selection itself, and
//     [`similar_query`] returns a concrete `BooleanQuery`. That also settles 4: a
//     `BooleanQuery` of `TermQuery`s reports its own terms and skips scoring when asked to.
//
//  Two empty outcomes, which are different and only one of which is an error. A seed with no
//  indexed text at all is refused up front by [`resolve_similar`], with a message naming
//  `--similar-in` rather than Tantivy's own (which blames missing stored fields, and nothing
//  here reads a stored field). A *non-empty* source whose terms are all filtered out by the
//  tuning yields a zero-clause `BooleanQuery` that matches nothing with no error at all; that
//  one is caught after the fact, by the `total == 0` warning in [`search`].

/// A body that can seed a similarity search.
///
/// Only the four text fields, and deliberately not `tool_input` / `bash_cmd`: those are JSON
/// fields, and Tantivy's term extraction ignores JSON fields without saying so, so a
/// `--similar-in tool_input` would be a flag that silently did nothing. `context_text` is
/// absent for the opposite reason — it *would* work, and it is exactly wrong (see the section
/// comment above).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum SimilarField {
    /// The prose half of a message. The default, and on its own the one that means "about the
    /// same thing" rather than "built from the same files".
    Text,
    /// The fenced blocks and inline spans. Analyzed by `code`, so it matches on identifiers.
    Code,
    /// What the tools printed. Useful for "another run that failed like this one".
    ToolOutput,
    /// The model's private reasoning. Behind `--include-thinking`, like everywhere else.
    Thinking,
}

impl SimilarField {
    pub fn as_str(self) -> &'static str {
        match self {
            SimilarField::Text => "text",
            SimilarField::Code => "code",
            SimilarField::ToolOutput => "tool_output",
            SimilarField::Thinking => "thinking",
        }
    }

    fn field(self, f: &Fields) -> Field {
        match self {
            SimilarField::Text => f.text,
            SimilarField::Code => f.code,
            SimilarField::ToolOutput => f.tool_output,
            SimilarField::Thinking => f.thinking,
        }
    }

    /// The values one document holds for this field, in the order they were indexed.
    fn values_of(self, doc: &Doc) -> Vec<String> {
        match self {
            SimilarField::Text => doc.text.clone(),
            SimilarField::Code => doc.code.clone(),
            SimilarField::ToolOutput => doc.tool_output.clone().into_iter().collect(),
            SimilarField::Thinking => doc.thinking.clone().into_iter().collect(),
        }
    }
}

/// The resolved seed of a similarity search: which turn it is, and the text that turn holds.
///
/// Plain data with no `Field` handles and no `DocAddress` in it, for the same reason [`Filters`]
/// is plain data: it is resolved once — by the CLI, by an MCP tool later, by the eval harness —
/// and handed to [`search`] to be turned into a query. Carrying the *values* rather than an
/// address is not an optimisation, it is what lets one turn's several documents seed a single
/// query; `MoreLikeThisQuery::with_document` accepts exactly one address and cannot be called
/// twice to accumulate.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimilarSource {
    /// The document the reference resolved to. Reported so a resolved prefix is visible.
    pub doc_id: String,
    /// The file that `turn_seq` numbers within. `seq` and `turn_seq` are per-*file* ordinals
    /// and one session id can name two files (§9), so a turn is only identified by the pair.
    pub source_path: String,
    pub turn_seq: u64,
    /// Documents in the source turn, before [`SIMILAR_SOURCE_DOCS`] capped the read.
    pub turn_docs: usize,
    /// Put the source turn back in the results. Off by default — see [`build_query`].
    pub include_source: bool,
    /// The seed text, one entry per selected field. Empty vectors are dropped at resolution
    /// time, so a non-empty `SimilarSource` always has something to tokenize.
    pub values: Vec<(SimilarField, Vec<String>)>,
}

impl SimilarSource {
    /// One line naming what a reference resolved to, for the human header and for `-v`.
    pub fn label(&self) -> String {
        format!(
            "similar to {} · turn #{} · {} doc{}",
            self.doc_id,
            self.turn_seq,
            self.turn_docs,
            if self.turn_docs == 1 { "" } else { "s" }
        )
    }

    /// The seed text for one field, joined — what [`snippet_generator`] highlights from.
    fn text_for(&self, field: SimilarField) -> String {
        self.values
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, values)| values.join("\n"))
            .unwrap_or_default()
    }
}

/// Documents of the source turn read to seed the query.
///
/// Same shape and same reason as `cli::TURN_WINDOW_LIMIT`: a turn is not a bounded thing — one
/// prompt can open a turn of hundreds of tool calls, and a sidechain file is a single turn by
/// rule 3 of the "Turns" section — so without a cap the seed of a similarity search is a whole
/// subagent transcript.
pub const SIMILAR_SOURCE_DOCS: usize = 200;

/// Seed text per field, in bytes. A turn that `cat`ted a large file would otherwise be
/// re-tokenized in full on every similarity search, and the terms past the first few hundred
/// kilobytes cannot change the outcome — `SIMILAR_MAX_QUERY_TERMS` keeps only 32 of them.
pub const SIMILAR_SOURCE_BYTES: usize = 256 * 1024;

/// A term in fewer than this many documents of the whole corpus is a path, a uuid fragment, a
/// blob id or a typo. It can match one or two documents at most, and its idf makes it dominate
/// the score while it does so.
///
/// 3 rather than Tantivy's default of 5: a document here is one message or one tool call, not
/// an article, so a genuinely shared technical term can legitimately live in only a handful of
/// them. Not 1, because `tokenizer::MAX_TOKEN_BYTES` deliberately lets whole sha256s into the
/// term dictionary — so that pasting one back finds it — and every similarity search seeded
/// from a tool call would otherwise be decided by blob ids.
const SIMILAR_MIN_DOC_FREQUENCY: u64 = 3;

/// A term in more than `num_docs / SIMILAR_MAX_DOC_FREQUENCY_RATIO` documents carries no
/// discrimination and is dropped.
///
/// One quarter specifically because **neither analyzer has a stop-word filter**
/// (`tokenizer::prose_analyzer`, `tokenizer::code_analyzer`): `the`, `and` and `is` are real
/// terms with real posting lists, and they sit above 90% of any transcript corpus, while the
/// ambient technical vocabulary (`test`, `file`, `error`, `run`) sits in the 10-30% band. A
/// quarter cuts the first group and keeps the discriminating half of the second.
const SIMILAR_MAX_DOC_FREQUENCY_RATIO: u64 = 4;

/// Floor under the computed `max_doc_frequency`. On an index of 80 documents a quarter is 20,
/// which discards every ordinary word and leaves the empty query that matches nothing.
const SIMILAR_MAX_DOC_FREQUENCY_FLOOR: u64 = 50;

/// How often a term must occur *in the source turn* to be considered.
///
/// 1, not Tantivy's default of 2. The source is a turn made of short documents, and demanding a
/// repeat throws away the single mention of the identifier that is the whole reason the turn is
/// memorable. Term frequency stays the multiplier in `tf * idf`, so a term said five times
/// still outranks one said once: this widens the candidate pool without flattening the ranking,
/// and the document-frequency bounds and `SIMILAR_MAX_QUERY_TERMS` bound the clause count
/// anyway.
const SIMILAR_MIN_TERM_FREQUENCY: usize = 1;

/// Clauses in the generated query. Every one is a posting-list walk, so latency is linear in
/// this number.
///
/// Lucene ships 25 and Tantivy copies it; the seed here is a whole turn (a prompt, the calls it
/// made and the answer) rather than one document, so the budget has to span more subjects.
/// Above ~50 the tail scores sit within noise of each other and buy only latency. Note that
/// Tantivy's cutoff is `if score_terms.len() > limit`, so the real cap is 33.
const SIMILAR_MAX_QUERY_TERMS: usize = 32;

/// Shortest analyzed token that can seed similarity, in **bytes on the analyzed token** —
/// lowercased, and stemmed on a `prose` field.
///
/// `WordTokenizer` splits on non-word characters, so `-f` arrives as `f`, `2>&1` as `2` and
/// `1`, and `&&` as nothing at all: every flag and redirection in a shell transcript becomes a
/// one- or two-character token with an enormous document frequency. 3 removes those and every
/// bare digit while keeping `git`, `api`, `run`. Not 4 — that would drop `bug`, `cli`, `sql`,
/// `pdf`, and three-letter technical nouns are the densest signal a transcript has.
const SIMILAR_MIN_WORD_LENGTH: usize = 3;

/// Longest analyzed token that can seed similarity, in bytes.
///
/// `tokenizer::MAX_TOKEN_BYTES` is 255 precisely so a sha256 survives into the term dictionary
/// and stays findable by pasting it back. That is right for an explicit query and wrong here:
/// two documents sharing a 64-hex blob id share one build, not a topic. 32 sits above every
/// identifier a person writes and below every hash, base64 chunk and dash-stripped uuid.
const SIMILAR_MAX_WORD_LENGTH: usize = 32;

/// Multiplier on the similarity clause as a whole.
///
/// `create_query` normalizes every clause by the best term's score, so this factor is uniform
/// across the clause and only means anything *relative to a co-occurring free-text `Must`*.
/// 1.0 lets an explicit query outvote the similarity when both are given, which is the right
/// default: the words a person typed are evidence about what they want, and the seed turn is an
/// inference. Lowering it is the one knob to reach for if the eval says otherwise.
const SIMILAR_BOOST: Score = 1.0;

/// Structural words that say "these two documents are the same *kind* of record", never "these
/// two documents are about the same thing".
///
/// Written in **post-analyzer form**, because [`is_similarity_term`] tests the token the
/// analyzer emitted rather than the word a person would write — so a surface spelling here
/// would be a list that looks right and does nothing.
///
/// There are two post-analyzer forms, not one, and that is why some concepts appear twice.
/// `--similar-in` can seed from `text` (the `prose` analyzer, which stems) or from `code`,
/// `tool_output` and `thinking` (the `code` analyzer, which does not). `assistant` stems to
/// `assist` on the first and stays `assistant` on the second; `tool_use` loses its underscore
/// to the whole-identifier token and then stems, giving `toolus` and `tooluse`. An entry
/// covering only one of the two silently stops filtering the moment a caller writes
/// `--similar-in tool_output`. `SIMILAR_STRUCTURAL_WORDS` is the surface list this one is
/// derived from, and `the_similarity_stop_words_are_spelled_as_both_analyzers_emit_them` pins
/// the derivation in both directions.
///
/// The list is short on purpose, and stops where [`SIMILAR_MAX_DOC_FREQUENCY_RATIO`] starts.
/// These are the words `parse.rs` writes into the `text` copy of every tool call by
/// construction (the tool's own name) plus the role and harness vocabulary; a similarity that
/// rides on "both of these are Bash calls" is precisely the false positive this feature must
/// not make, and their frequency is a fact about how the corpus was assembled rather than about
/// any topic in it. Everything else — English function words included — is left to the
/// corpus-relative frequency bound, which adapts as an index grows and cannot go stale.
///
/// The cost is real and worth stating: `read`, `write`, `edit` and `task` are ordinary English
/// words too, and a turn that is genuinely *about* writing to a file loses them as evidence.
/// On this corpus the structural sense outnumbers the topical one by an order of magnitude.
const SIMILAR_STOP_WORDS: [&str; 24] = [
    // Tool names, as they appear at the head of every tool call's `text`.
    "bash",
    "read",
    "write",
    "edit",
    "multiedit",
    "grep",
    "glob",
    "task",
    "todowrit",  // TodoWrite, stemmed  (prose)
    "todowrite", // TodoWrite, unstemmed (code)
    "webfetch",
    "websearch",
    "notebookedit",
    // Roles.
    "assist",    // assistant, stemmed  (prose)
    "assistant", //            unstemmed (code)
    "user",
    "human",
    // Harness vocabulary, which every transcript carries and no transcript is about.
    "system",
    "remind",   // reminder, stemmed  (prose)
    "reminder", //           unstemmed (code)
    "sidechain",
    // `tool_use` and `tool_result` lose their underscore to the whole-identifier token before
    // the stemmer sees them, so the two analyzers differ only where the stem does.
    "toolus",  // tool_use, stemmed  (prose)
    "tooluse", //           unstemmed (code)
    "toolresult",
];

/// The structural vocabulary [`SIMILAR_STOP_WORDS`] is derived from, written the way a person
/// or a transcript writes it.
///
/// Kept beside the derived list so the two can be checked against each other: the test runs
/// each of these through *both* analyzers and requires the whole-identifier token each one
/// emits to be in the derived list. Adding a tool name here and forgetting its stemmed twin is
/// then a failing test rather than a filter that quietly covers half the fields.
///
/// Test-only: nothing at runtime reads it, because the derived list is what
/// [`is_similarity_term`] consults. It is the *input* to the check, kept in the source so the
/// check has something to check against.
#[cfg(test)]
const SIMILAR_STRUCTURAL_WORDS: [&str; 20] = [
    "Bash",
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "Grep",
    "Glob",
    "Task",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "assistant",
    "user",
    "human",
    "system",
    "reminder",
    "sidechain",
    "tool_use",
    "tool_result",
];

/// Could this analyzed token seed a similarity query? The word-length bounds and the stop-word
/// list of Tantivy's own `MoreLikeThis::is_noise_word`, reimplemented because that type is not
/// exported.
///
/// Kept beside the constants it applies rather than inside [`similar_query`], because its second
/// caller is [`snippet_generator`]: the highlight has to answer "why did this come back" with
/// the same vocabulary the query asked the question in.
fn is_similarity_term(token: &str) -> bool {
    (SIMILAR_MIN_WORD_LENGTH..=SIMILAR_MAX_WORD_LENGTH).contains(&token.len())
        && !SIMILAR_STOP_WORDS.contains(&token)
}

/// The document-frequency band a term has to sit inside, for an index of `num_docs` documents.
/// The upper bound is derived rather than frozen — see [`SIMILAR_MAX_DOC_FREQUENCY_RATIO`].
fn similar_doc_frequency_band(num_docs: u64) -> std::ops::RangeInclusive<u64> {
    SIMILAR_MIN_DOC_FREQUENCY
        ..=(num_docs / SIMILAR_MAX_DOC_FREQUENCY_RATIO).max(SIMILAR_MAX_DOC_FREQUENCY_FLOOR)
}

/// The reference grammar, quoted verbatim in every error this module raises about one.
const SIMILAR_REFERENCE_GRAMMAR: &str = "a reference is SESSION:SEQ, SESSION:AGENT:SEQ, a record uuid, or a doc_id — \
     any of them by unambiguous prefix";

/// The largest `index <= at` that `s` can be split on. `str::floor_char_boundary` is unstable.
///
/// Truncating a seed value mid-codepoint would panic, and the byte the budget lands on is
/// arbitrary — it is a cap on tokenizer work, not a promise about where the text ends.
fn floor_char_boundary(s: &str, at: usize) -> usize {
    if at >= s.len() {
        return s.len();
    }
    let mut at = at;
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Resolve a document reference to the turn it belongs to, and read that turn's text.
///
/// The reference grammar, disambiguated before any index access:
///
/// * `SESSION:SEQ` — two colon-separated parts whose second parses as a number. The main
///   transcript of that session; the session part may be a prefix.
/// * `SESSION:AGENT:SEQ` — three parts, the last a number, with `-` for the main transcript.
///   This is the shape `Doc::doc_id` has once its `file_tag` is removed, and it is what a
///   person copies out of a document listing.
/// * anything else — an id: `doc_id`, `uuid` or `tool_use_id`, matched exactly first and then
///   by prefix, so the leading block a user pastes works the way `resolve_id` already lets a
///   session prefix work.
///
/// An ambiguous prefix is an error naming what it matched, never a silent pick — a "find
/// similar" that quietly seeded from the wrong document would produce a plausible, wrong answer
/// with no way to notice.
///
/// The source is the whole **turn**, not the referenced document. That is a deliberate default
/// rather than a flag: a turn is the unit a person remembers ("the time we chased the fieldnorm
/// bug"), and its documents are a prompt, the calls it made and the answer — each of which on
/// its own is a fragment. A single tool call as a seed is mostly a file path and a diff, and
/// the neighbours that explain it are exactly what a similarity search should be matching on.
pub fn resolve_similar(
    index: &tantivy::Index,
    f: &Fields,
    spec: &str,
    fields: &[SimilarField],
    include_source: bool,
) -> anyhow::Result<SimilarSource> {
    let doc = resolve_doc(index, f, spec)?;
    let window = crate::context::turn_window(
        index,
        f,
        &doc.source_path,
        doc.turn_seq,
        SIMILAR_SOURCE_DOCS,
    )?;

    // Sorted and deduplicated: `--similar-in text,text,code` is one selection, and a stable
    // order keeps the resulting `SimilarSource` (which `--json` consumers and the eval harness
    // both read) independent of the order the flags happened to be typed in.
    let selected: BTreeSet<SimilarField> = fields.iter().copied().collect();

    let mut values: Vec<(SimilarField, Vec<String>)> = Vec::new();
    for field in selected.iter().copied() {
        let mut collected: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        'field: for doc in &window.docs {
            for mut value in field.values_of(doc) {
                if value.trim().is_empty() {
                    continue;
                }
                // Truncate to the remaining budget rather than skipping the whole value. A
                // test-before-add on an untruncated value bounds nothing when the very first
                // value is the large one: `ParseOptions::max_text_bytes` admits a 1 MiB tool
                // result, which is precisely the `cat` of a large file
                // [`SIMILAR_SOURCE_BYTES`] exists to keep out of the tokenizer.
                let room = SIMILAR_SOURCE_BYTES - bytes;
                if value.len() > room {
                    value.truncate(floor_char_boundary(&value, room));
                }
                bytes += value.len();
                collected.push(value);
                if bytes >= SIMILAR_SOURCE_BYTES {
                    break 'field;
                }
            }
        }
        if !collected.is_empty() {
            values.push((field, collected));
        }
    }

    if values.is_empty() {
        // Never let Tantivy's own message out: it says "the document may not have stored
        // fields", and nothing here reads a stored field.
        let names: Vec<&str> = selected.iter().map(|f| f.as_str()).collect();
        bail!(
            "the turn at {spec:?} (turn #{} of {}) has no indexed text in {}; \
             try --similar-in text,code,tool_output",
            doc.turn_seq,
            doc.source_path,
            names.join(",")
        );
    }

    let source = SimilarSource {
        doc_id: doc.doc_id,
        source_path: doc.source_path,
        turn_seq: doc.turn_seq,
        turn_docs: window.total,
        include_source,
        values,
    };
    tracing::info!(
        reference = %spec,
        doc = %source.doc_id,
        turn = source.turn_seq,
        docs = source.turn_docs,
        "similarity source resolved"
    );
    Ok(source)
}

/// The part of the index a reference is allowed to resolve inside.
///
/// `--similar-to` takes a reference with no other context, so it resolves [`Anywhere`]. `show
/// --around` always names a transcript first, and resolving index-wide there is wrong twice
/// over: a reference that lands in another session would window a transcript the caller never
/// asked for, and — the case that made this a scope rather than a post-check — a reference that
/// is perfectly unique *inside* the named session would be refused as ambiguous because some
/// other session happens to hold a document with the same uuid. That is not hypothetical: §9's
/// `resetSessionFile()`/`relocated` case indexes one transcript under two project keys, so the
/// same record uuid legitimately appears twice, and [`crate::cli::source_path_for`] returns
/// `None` in exactly that case — leaving the caller no spelling of the command that avoids it.
///
/// [`Anywhere`]: DocScope::Anywhere
#[derive(Debug, Clone, Copy, Default)]
pub enum DocScope<'a> {
    /// The whole index.
    #[default]
    Anywhere,
    /// One transcript: a session, the agent slot inside it (`None` is the main file, which is
    /// an exact requirement and not "any"), and the file itself when the session names exactly
    /// one.
    Transcript {
        session_id: &'a str,
        agent_id: Option<&'a str>,
        source_path: Option<&'a str>,
    },
}

/// The `Must` clauses that confine a resolution to [`DocScope`].
///
/// The session and agent parts are exact rather than prefix matches, unlike [`coordinate_query`]:
/// a caller passing a scope has already resolved these (`cli::resolve_id` expands a session
/// prefix before `show` gets this far), so a prefix here would only widen what the caller
/// narrowed.
fn scope_clauses(f: &Fields, scope: DocScope<'_>) -> Vec<(Occur, Box<dyn Query>)> {
    let DocScope::Transcript {
        session_id,
        agent_id,
        source_path,
    } = scope
    else {
        return Vec::new();
    };
    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
        vec![(Occur::Must, term_query(f.session_id, session_id))];
    match agent_id {
        Some(agent) => clauses.push((Occur::Must, term_query(f.agent_id, agent))),
        // Absent, not "any": a subagent file numbers its own `seq` from zero, so the main
        // transcript's anchor must not be allowed to land in one.
        None => clauses.push((
            Occur::MustNot,
            Box::new(ExistsQuery::new("agent_id".to_string(), false)),
        )),
    }
    if let Some(path) = source_path {
        clauses.push((Occur::Must, term_query(f.source_path, path)));
    }
    clauses
}

/// `inner`, confined to `scope`.
fn scoped(f: &Fields, inner: Box<dyn Query>, scope: DocScope<'_>) -> Box<dyn Query> {
    let mut clauses = scope_clauses(f, scope);
    if clauses.is_empty() {
        return inner;
    }
    clauses.push((Occur::Must, inner));
    Box::new(BooleanQuery::new(clauses))
}

/// How many candidates a scoped resolution looks at before it gives up on telling them apart.
/// Small on purpose: past a couple, the answer is "this reference is ambiguous" and the only
/// question is which two to name.
const RESOLVE_CANDIDATE_LIMIT: usize = 8;

/// The one document a reference names. See [`resolve_similar`] for the grammar.
///
/// Public because `show --around` takes the same references, and because two commands
/// disagreeing about what `abc123` means would be worse than either behaviour on its own.
pub fn resolve_doc(index: &tantivy::Index, f: &Fields, spec: &str) -> anyhow::Result<Doc> {
    match resolve_doc_in(index, f, spec, DocScope::Anywhere)? {
        Some(doc) => Ok(doc),
        None => bail!("no document matches {spec:?}; {SIMILAR_REFERENCE_GRAMMAR}"),
    }
}

/// [`resolve_doc`], confined to `scope`, with "nothing here" as an answer rather than an error.
///
/// `Ok(None)` means the reference names nothing *inside the scope* — which is a different fact
/// from "names nothing", and the caller is expected to say so: `cli::resolve_seq` re-resolves
/// index-wide purely to report which session the reference actually lives in, because "no such
/// uuid" would be a lie about a document that plainly exists.
///
/// Ambiguity inside the scope is still an error. The one exception is the duplicate the scope
/// exists for: when every candidate is the same `seq` of the same transcript, they are one
/// logical record that §9 indexed from two files, and there is nothing to choose between them —
/// so the file that sorts first is taken, deterministically, rather than refusing a question
/// that has only one answer.
pub fn resolve_doc_in(
    index: &tantivy::Index,
    f: &Fields,
    spec: &str,
    scope: DocScope<'_>,
) -> anyhow::Result<Option<Doc>> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("a document reference cannot be empty; {SIMILAR_REFERENCE_GRAMMAR}");
    }
    let searcher = index.reader()?.searcher();
    let parts: Vec<&str> = spec.split(':').collect();

    // The coordinate shapes are decided by their arity and by the last part parsing as a
    // number, with no index access at all — so `s1:7` can never be mistaken for the prefix of
    // some uuid that happens to contain a colon.
    let coordinates = match parts.as_slice() {
        [session, seq] => seq.parse::<u64>().ok().map(|seq| (*session, None, seq)),
        [session, agent, seq] => seq
            .parse::<u64>()
            .ok()
            .map(|seq| (*session, Some(*agent), seq)),
        _ => None,
    };

    if let Some((session, agent, seq)) = coordinates {
        let query = scoped(f, coordinate_query(f, session, agent, seq)?, scope);
        return match one_of(&searcher, f, &*query, spec)? {
            Some(doc) => Ok(Some(doc)),
            // Distinguishable from a malformed reference: the coordinates parsed, they just
            // name nothing. Only the unscoped caller can call that final, so a scoped one gets
            // `None` and decides for itself.
            None if matches!(scope, DocScope::Anywhere) => {
                bail!("no document at {spec:?}; {SIMILAR_REFERENCE_GRAMMAR}")
            }
            None => Ok(None),
        };
    }

    // Exact before prefix, mirroring `cli::resolve_id`: an id that is itself also the prefix of
    // a longer one resolves to itself rather than reporting an ambiguity nobody has.
    let exact = BooleanQuery::new(
        [f.doc_id, f.uuid, f.tool_use_id]
            .into_iter()
            .map(|field| (Occur::Should, term_query(field, spec)))
            .collect(),
    );
    if let Some(doc) = one_of(&searcher, f, &*scoped(f, Box::new(exact), scope), spec)? {
        return Ok(Some(doc));
    }

    let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for field in [f.doc_id, f.uuid, f.tool_use_id] {
        shoulds.push((Occur::Should, prefix_query(field, spec)?));
    }
    let prefix = scoped(f, Box::new(BooleanQuery::new(shoulds)), scope);
    one_of(&searcher, f, &*prefix, spec)
}

/// `SESSION:SEQ` / `SESSION:AGENT:SEQ` as a query. The session and agent parts match by prefix
/// for the same reason `--session` does: a session id is a 36-character uuid and what a person
/// pastes is its leading block.
fn coordinate_query(
    f: &Fields,
    session: &str,
    agent: Option<&str>,
    seq: u64,
) -> anyhow::Result<Box<dyn Query>> {
    if session.trim().is_empty() {
        bail!(
            "a document reference needs a session id before the colon; {SIMILAR_REFERENCE_GRAMMAR}"
        );
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> =
        vec![(Occur::Must, prefix_query(f.session_id, session)?)];
    // `-` is how `doc_id` spells "the main transcript", and an absent agent means the same
    // thing. Both become the `MustNot(Exists)` that `session_clauses` uses, because a subagent
    // file numbers its own `seq` from zero and would otherwise collide with the parent.
    match agent.map(str::trim).filter(|a| !a.is_empty() && *a != "-") {
        Some(agent) => clauses.push((Occur::Must, prefix_query(f.agent_id, agent)?)),
        None => clauses.push((
            Occur::MustNot,
            Box::new(ExistsQuery::new("agent_id".to_string(), false)),
        )),
    }
    clauses.push((Occur::Must, flag_query(f.seq, seq)));
    Ok(Box::new(BooleanQuery::new(clauses)))
}

/// Exactly one document, or an error naming the alternatives.
///
/// More than one hit is fetched, never exactly one: "the first of several" and "the only one"
/// are the same result to a collector, and picking the first silently is how a similarity
/// search ends up seeded from a document the caller never named. The order is by the `seq` fast
/// field, then by `doc_id`, so the two candidates an error names are the same two on every run.
///
/// The single exception is §9's duplicate: one transcript indexed under two project keys — the
/// `resetSessionFile()`/`relocated` case — so the *same record* is read out of two files and
/// carries identical record uuids in both. There is no question to ask there: the documents say
/// the same thing, and the only difference is which file they were read from. So the first by
/// `doc_id` is taken. Anything else — two different `seq`s, two different transcripts — is a
/// real ambiguity and stays an error.
///
/// The `uuid` is what makes the exception an exception, and it is not decoration. `seq` is a
/// per-file ordinal and §9 lets two *different* transcripts share a session id, so `seq` +
/// session + agent is satisfied by turn 0 of one conversation and turn 0 of another. Matching on
/// those three alone declared that pair one record and returned whichever file sorted first — a
/// document from a transcript the caller never named, with no error and no warning, which reads
/// exactly like a correct answer. An absent uuid is no evidence either way, so it is not
/// accepted as agreement.
fn one_of(
    searcher: &Searcher,
    f: &Fields,
    query: &dyn Query,
    spec: &str,
) -> anyhow::Result<Option<Doc>> {
    let mut docs = docs_by_seq(searcher, f, query, RESOLVE_CANDIDATE_LIMIT)?;
    docs.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.doc_id.cmp(&b.doc_id)));
    let same_record = |a: &Doc, b: &Doc| {
        a.seq == b.seq
            && a.session_id == b.session_id
            && a.agent_id == b.agent_id
            && a.uuid.is_some()
            && a.uuid == b.uuid
    };
    match docs.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
        [first, rest @ ..] if rest.iter().all(|d| same_record(first, d)) => Ok(Some(first.clone())),
        [a, b, ..] => bail!(
            "ambiguous document reference {spec:?} matches at least 2: {}, {}",
            a.doc_id,
            b.doc_id
        ),
    }
}

/// The terms the seed turn contributes to the query, best first and in a **total** order.
///
/// This is a reimplementation of `MoreLikeThis::retrieve_terms_from_doc_fields` +
/// `create_score_term`, and it exists for one reason: Tantivy's version is not reproducible.
/// `create_score_term` accumulates term frequencies in a `std::collections::HashMap<Term,
/// usize>` and then selects the best `max_query_terms` of them with a `BinaryHeap` whose `Ord`
/// (`more_like_this.rs`) compares **only the score**. Two terms with the same term frequency
/// and the same document frequency therefore tie *exactly* — which is the common case, not the
/// corner case, for a turn-shaped seed where nearly every token occurs once — and which of the
/// tied terms survives the cut is decided by `HashMap` iteration order. `RandomState` reseeds
/// per map instance, so the surviving set differs on every call **within one process, against
/// one unchanged index**.
///
/// That is not a ranking nicety. `search` runs the query more than once per request (the hit
/// pass, then [`docs_with_value`] and every arm of [`facets`], each re-deriving a weight), so a
/// re-randomising term set also made `docs_with_value` disagree with the `Count` beside it and
/// print `62 of 58 matching docs have a value`. And `--limit`/`--offset` paged over a different
/// query each time, repeating and skipping documents.
///
/// So the selection happens here, where the tie-break can be made total: descending by
/// `tf * idf`, then ascending by the term's own serialized bytes. The band and the noise-word
/// rules are the same ones Tantivy would have applied — this module already reimplements the
/// second as [`is_similarity_term`] for the highlighter — and the `idf` is Tantivy's own BM25
/// formula, copied because `tantivy::query::bm25::idf` is `pub(crate)`.
///
/// One deliberate difference from Tantivy: the cap is exact. `create_score_term`'s cutoff is
/// `if score_terms.len() > limit`, so its real ceiling is `max_query_terms + 1`; here
/// [`SIMILAR_MAX_QUERY_TERMS`] means what it says.
fn similar_terms(
    searcher: &Searcher,
    f: &Fields,
    source: &SimilarSource,
) -> anyhow::Result<Vec<(Term, Score)>> {
    let num_docs = searcher.num_docs();
    // Derived from the corpus size on every query, and deliberately not a frozen literal: the
    // bound is an absolute document count, so a constant would quietly change meaning as an
    // index grows from a thousand documents to a million.
    let doc_frequency = similar_doc_frequency_band(num_docs);

    // `BTreeMap`, not `HashMap`: this map's iteration order is an input to the selection below,
    // and that is exactly what went wrong upstream.
    let mut frequencies: BTreeMap<(Field, String), usize> = BTreeMap::new();
    for (field, values) in &source.values {
        let field = field.field(f);
        // Each field is tokenized with *its own* analyzer, which is what makes the token match
        // the term dictionary: `text` is stemmed prose, `code`/`tool_output`/`thinking` are not.
        let mut analyzer = searcher.index().tokenizer_for_field(field)?;
        for value in values {
            analyzer.token_stream(value).process(&mut |token: &Token| {
                if is_similarity_term(&token.text) {
                    *frequencies.entry((field, token.text.clone())).or_insert(0) += 1;
                }
            });
        }
    }

    // Scored with the map key kept alongside, because that key is the tie-break below.
    let mut scored: Vec<((Field, String), Score)> = Vec::new();
    for ((field, text), term_frequency) in frequencies {
        if term_frequency < SIMILAR_MIN_TERM_FREQUENCY {
            continue;
        }
        let doc_freq = searcher.doc_freq(&Term::from_field_text(field, &text))?;
        // The band's floor is above zero, so a term the corpus does not hold is excluded here
        // too — an idf against `doc_freq == 0` would be meaningless as well as unbounded.
        if !doc_frequency.contains(&doc_freq) {
            continue;
        }
        let score = term_frequency as Score * similar_idf(doc_freq, num_docs);
        scored.push(((field, text), score));
    }

    // The total order. `partial_cmp` cannot see a NaN here — `similar_idf` is a `ln` of
    // something strictly greater than 1 — but a comparator that returned `Equal` on one would
    // reintroduce exactly the ambiguity this function exists to remove, so the tie-break on
    // `(field, token)` runs on every comparison rather than only on the ones that reach it.
    scored.sort_by(
        |((a_field, a_text), a_score), ((b_field, b_text), b_score)| {
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a_field.field_id().cmp(&b_field.field_id()))
                .then_with(|| a_text.cmp(b_text))
        },
    );
    scored.truncate(SIMILAR_MAX_QUERY_TERMS);
    Ok(scored
        .into_iter()
        .map(|((field, text), score)| (Term::from_field_text(field, &text), score))
        .collect())
}

/// Tantivy's BM25 `idf`, copied because `tantivy::query::bm25::idf` is `pub(crate)`.
///
/// Kept identical on purpose: the similarity clause's boosts are relative to each other and to
/// the BM25 scores of a co-occurring free-text `Must`, so a different curve here would silently
/// re-weight [`SIMILAR_BOOST`].
fn similar_idf(doc_freq: u64, num_docs: u64) -> Score {
    let x = (num_docs.saturating_sub(doc_freq) as Score + 0.5) / (doc_freq as Score + 0.5);
    (1.0 + x).ln()
}

/// The similarity clause: the seed turn's best terms, ORed together and weighted by `tf * idf`.
///
/// The clause is a concrete [`BooleanQuery`] of [`BoostQuery`]-wrapped [`TermQuery`]s — the
/// same shape `MoreLikeThis::create_query` builds, including the normalization of every boost
/// by the best term's score, so `SIMILAR_BOOST` keeps meaning "the similarity clause as a
/// whole, relative to a co-occurring free-text query".
///
/// Building it concretely rather than handing a `MoreLikeThisQuery` downstream is what makes a
/// request internally consistent. A `MoreLikeThisQuery` re-derives its own clauses inside
/// `weight()`, and `search` calls `weight()` several times per request; a plain `BooleanQuery`
/// of term queries is fixed the moment it is built, so the hit pass, the `Count`, the
/// aggregation and [`docs_with_value`] are all answering about the same query. It also removes
/// the need for the scoring-adapter this function used to return: `MoreLikeThisQuery::weight`
/// errors outright under `EnableScoring::Disabled` (which `--sort newest`, `docs_with_value`
/// and every facet collector pass), while `TermQuery` and `BoostQuery` simply skip scoring.
///
/// An empty seed yields a zero-clause `BooleanQuery`, which matches nothing without erroring —
/// caught after the fact by the `total == 0` warning in [`search`].
fn similar_query(
    searcher: &Searcher,
    f: &Fields,
    source: &SimilarSource,
) -> anyhow::Result<Box<dyn Query>> {
    let terms = similar_terms(searcher, f, source)?;
    let best = terms.first().map_or(1.0, |(_, score)| *score);
    let clauses: Vec<(Occur, Box<dyn Query>)> = terms
        .into_iter()
        .map(|(term, score)| {
            let term_query: Box<dyn Query> =
                Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            let boosted: Box<dyn Query> =
                Box::new(BoostQuery::new(term_query, score * SIMILAR_BOOST / best));
            (Occur::Should, boosted)
        })
        .collect();
    Ok(Box::new(BooleanQuery::from(clauses)))
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
    // The four multi-valued fields: `get_first` would silently keep one code block of five.
    let list = |field: Field| -> Vec<String> {
        stored
            .get_all(field)
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect()
    };

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
        kind,
        source_path: s(f.source_path).unwrap_or_default(),
        seq: u(f.seq).unwrap_or(0),
        turn_seq: u(f.turn_seq).unwrap_or(0),
        // Never read back: `context_text` is indexed and not stored, so there is nothing in
        // the payload to read. That is deliberate — a header is scaffolding the indexer
        // assembled, not something the transcript said, and a `Doc` handed to a renderer or to
        // `--json` must only carry what was actually written.
        turn_prompt: None,
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
        body: s(f.body).unwrap_or_default(),
        text: list(f.text),
        code: list(f.code),
        headings: list(f.headings),
        code_langs: list(f.code_lang),
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
    use std::collections::BTreeSet;
    use tantivy::Index;

    /// Every field of [`Filters`], by the name it travels under.
    ///
    /// Read off the struct rather than typed out, and that is the whole point of it. Three
    /// separate lists elsewhere in this crate enumerate the filters by hand — the drop order in
    /// `mcp::envelope::narrowest`, the echo in `mcp::envelope::applied_filters`, and
    /// `sessions::unanswerable_filters` — and each had a test that asserted its own length
    /// against a number written in the same commit. When `all_records`, `turn_of` and `turn_seq`
    /// were added to `Filters`, all three tests went on passing: a hand-written list cannot
    /// notice a field nobody told it about. Any test that compares its coverage against this
    /// fails instead, naming the field that was forgotten.
    pub fn filter_field_names() -> BTreeSet<String> {
        filter_object(&Filters::default())
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// The fields this `Filters` actually sets: everything that differs from the default.
    ///
    /// Lets a test say "these cases cover every field" without repeating, per case, which field
    /// it was meant to be about — the repetition being the thing that goes stale.
    pub fn filter_fields_set(f: &Filters) -> BTreeSet<String> {
        let default = filter_object(&Filters::default());
        filter_object(f)
            .into_iter()
            .filter(|(name, value)| default.get(name) != Some(value))
            .map(|(name, _)| name)
            .collect()
    }

    fn filter_object(f: &Filters) -> serde_json::Map<String, serde_json::Value> {
        match serde_json::to_value(f).expect("Filters holds only JSON-native values") {
            serde_json::Value::Object(map) => map,
            other => panic!("Filters must serialize as an object, not {other}"),
        }
    }

    pub fn blank_doc(seq: u64) -> Doc {
        Doc {
            doc_id: format!("s1:-:{seq}"),
            kind: DocKind::Message,
            source_path: "/tmp/s1.jsonl".into(),
            seq,
            turn_seq: 0,
            turn_prompt: None,
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
            body: String::new(),
            text: Vec::new(),
            code: Vec::new(),
            headings: Vec::new(),
            code_langs: Vec::new(),
            tool_output: None,
            thinking: None,
            thinking_tokens: None,
            raw: "{}".into(),
        }
    }

    /// A RAM index built straight from hand-made [`Doc`]s — no dependency on the indexer.
    pub fn index_docs(docs: &[Doc]) -> (Index, Fields) {
        index_in_segments(&[docs])
    }

    /// The same, with each slice committed separately so the index really has that many
    /// segments.
    ///
    /// Anything that reads a *fast field* per document has to merge across segments, and the
    /// merge is where the mistakes live: term ordinals are per-segment, so a collector that
    /// keys on them and forgets to resolve before merging silently files two different values
    /// under one key. A single-segment index cannot fail that way, so it cannot test it.
    pub fn index_in_segments(batches: &[&[Doc]]) -> (Index, Fields) {
        let (schema, fields) = build_schema();
        let index = crate::tokenizer::create_in_ram(schema.clone());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for batch in batches {
            for doc in *batch {
                let json = doc_to_json(doc, None, true).to_string();
                writer
                    .add_document(TantivyDocument::parse_json(&schema, &json).unwrap())
                    .unwrap();
            }
            writer.commit().unwrap();
        }
        (index, fields)
    }

    /// Eight docs covering every dimension the filters touch.
    pub fn corpus() -> Vec<Doc> {
        let mut docs = Vec::new();

        let mut d = blank_doc(0);
        d.text = vec!["please make the tantivy schema faster".into()];
        docs.push(d);

        // Tool calls are shaped the way `parse::tool_call_doc` shapes them: the name and the
        // input strings in `text`, the `Edit`/`Write` payloads in `code`, the result in
        // `tool_output`.
        let mut d = blank_doc(1);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.model = Some("claude-opus-5".into());
        d.tool_name = Some("Bash".into());
        d.tool_use_id = Some("toolu_1".into());
        d.tool_input = Some(json!({"command": "cargo build --release", "timeout": 600000}));
        d.text = vec!["Bash\ncargo build --release".into()];
        d.tool_output = Some("Compiling tantivy\nFinished dev profile".into());
        d.bash_cmd = crate::bash::extract("cargo build --release").map(|c| c.to_json());
        docs.push(d);

        let mut d = blank_doc(2);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.model = Some("claude-opus-5".into());
        d.tool_name = Some("Read".into());
        d.tool_input = Some(json!({"file_path": "/home/user/session-search/src/index.rs"}));
        d.text = vec!["Read\n/home/user/session-search/src/index.rs".into()];
        d.tool_output = Some("pub fn open_or_create(index_dir: &Path)".into());
        docs.push(d);

        let mut d = blank_doc(3);
        d.kind = DocKind::ToolCall;
        d.role = "assistant".into();
        d.tool_name = Some("Bash".into());
        d.tool_input = Some(json!({"command": "cargo test"}));
        d.bash_cmd = crate::bash::extract("cargo test").map(|c| c.to_json());
        d.text = vec!["Bash\ncargo test".into()];
        d.tool_output = Some("error: test failed".into());
        d.is_error = true;
        docs.push(d);

        let mut d = blank_doc(4);
        d.text = vec!["a sidechain turn about tantivy".into()];
        d.is_sidechain = true;
        d.agent_id = Some("a10845c5ff9c7d4ec".into());
        d.agent_type = Some("Explore".into());
        d.session_id = "s1".into();
        d.doc_id = "s1:a10845c5ff9c7d4ec:0".into();
        d.source_path = "/tmp/s1/subagents/agent-a10845c5ff9c7d4ec.jsonl".into();
        d.seq = 0;
        docs.push(d);

        let mut d = blank_doc(5);
        d.text = vec!["an older turn in another project".into()];
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
        d.text = vec!["nested project below the search root".into()];
        d.project = Some("/home/user/session-search/sub/dir".into());
        docs.push(d);

        let mut d = blank_doc(7);
        d.role = "assistant".into();
        d.text = vec!["the visible answer".into()];
        d.thinking = Some("a private deliberation about parsnips".into());
        docs.push(d);

        docs
    }

    pub fn texts(r: &SearchResponse) -> Vec<String> {
        r.hits.iter().map(|h| h.doc.text.join("\n")).collect()
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

    /// A result list a fifth shorter than the corpus can support is the kind of omission a
    /// reader only discovers by failing to find something, so the default scope has to be both
    /// narrow *and* loud about being narrow.
    #[test]
    fn the_default_scope_is_the_conversation_and_says_what_it_refused() {
        let mut docs = Vec::new();
        for (seq, role, meta) in [
            (0u64, "user", false),
            (1, "assistant", false),
            (2, "attachment", false),
            (3, "system", false),
            (4, "user", true),
        ] {
            let mut d = blank_doc(seq);
            d.role = role.into();
            d.is_meta = meta;
            d.text = vec!["the standing request".into()];
            docs.push(d);
        }
        let (index, f) = index_docs(&docs);

        let r = search(&index, &f, &req("standing")).unwrap();
        assert_eq!(r.total, 2, "user and assistant only: {:?}", texts(&r));
        assert_eq!(r.hidden, 3, "attachment, system, and the meta turn");

        let r = search(&index, &f, &req_all("standing")).unwrap();
        assert_eq!(r.total, 5);
        assert_eq!(r.hidden, 0, "nothing was refused, so nothing is reported");

        // An explicit `--role attachment` that returned nothing would be a filter
        // contradicting itself, so the scope steps aside for it.
        let by_role = SearchRequest {
            filters: Filters {
                role: Some("attachment".into()),
                ..Filters::default()
            },
            ..req("standing")
        };
        let r = search(&index, &f, &by_role).unwrap();
        assert_eq!(r.total, 1, "{:?}", texts(&r));
        assert_eq!(r.hidden, 0);
    }

    /// The scope narrows the *result set*, never the index. Everything it hides stays
    /// findable, which is the whole reason it is a search default rather than an index-time
    /// decision — "which session had that env var in its reminder" has to stay answerable.
    #[test]
    fn what_the_scope_hides_is_still_indexed() {
        let mut hidden = blank_doc(0);
        hidden.role = "attachment".into();
        hidden.text = vec!["CLAUDE_CONFIG_DIR=/srv/claude".into()];
        let (index, f) = index_docs(&[hidden]);

        assert_eq!(
            search(&index, &f, &req("CLAUDE_CONFIG_DIR")).unwrap().total,
            0
        );
        assert_eq!(
            search(&index, &f, &req_all("CLAUDE_CONFIG_DIR"))
                .unwrap()
                .total,
            1
        );
    }

    /// A filter-only browse builds a query of nothing but `MustNot`, which matches nothing in
    /// Tantivy — there is no positive set for the negatives to subtract from. The scope needs
    /// a universe to exclude out of, or "everything except the apparatus" becomes "nothing".
    #[test]
    fn the_default_scope_on_a_filter_only_browse_still_returns_the_conversation() {
        let mut docs = vec![blank_doc(0)];
        docs[0].text = vec!["something".into()];
        let mut noise = blank_doc(1);
        noise.role = "attachment".into();
        docs.push(noise);
        let (index, f) = index_docs(&docs);

        let r = search(&index, &f, &req("")).unwrap();
        assert_eq!(r.total, 1, "not zero: {:?}", texts(&r));
        assert_eq!(r.hidden, 1);
    }

    /// The number the HTTP pager divides by when grouping is on. Term ordinals are
    /// per-segment, so the distinct count has to resolve them before merging — otherwise two
    /// files' turn #0 count as one, or one file's counts twice.
    #[test]
    fn counting_turns_is_exact_across_the_segment_boundary() {
        let turn = |seq: u64, turn_seq: u64, path: &str| {
            let mut d = blank_doc(seq);
            d.turn_seq = turn_seq;
            d.source_path = path.into();
            d.text = vec!["memmap".into()];
            d
        };
        // Two files, and one turn of each straddles the commit.
        let first = vec![turn(0, 0, "/a.jsonl"), turn(1, 0, "/b.jsonl")];
        let second = vec![turn(2, 0, "/b.jsonl"), turn(3, 5, "/a.jsonl")];
        let (index, f) = index_in_segments(&[&first, &second]);

        assert_eq!(
            count_turns(&index, &f, &req("memmap")).unwrap(),
            3,
            "a#0, b#0, a#5 — the two files' turn 0 are different turns"
        );
        assert_eq!(search(&index, &f, &req("memmap")).unwrap().total, 4);
    }

    /// The same request over the whole index rather than the conversation.
    ///
    /// For the tests that are about something else — the analyzer, the schema round trip — and
    /// that use an `attachment` or `system` document because those are documents like any
    /// other to the thing under test. Scoping is tested where scoping lives.
    fn req_all(query: &str) -> SearchRequest {
        SearchRequest {
            filters: Filters {
                all_records: true,
                ..Filters::default()
            },
            ..req(query)
        }
    }

    /// The `code` analyzer's whole point: a part of an identifier finds the identifier, and
    /// the snake/camel/pascal spellings of one name are interchangeable.
    ///
    /// The corpus is deliberately minimal — each doc's body *is* the identifier — so a hit
    /// can only come from the analyzer and not from some other word in the sentence. The body
    /// is in `code`, which is the field the `code` analyzer indexes; `text` keeps a copy so
    /// the assertions can name the document they mean.
    fn identifier_docs() -> Vec<Doc> {
        let names = [
            "open_or_create",
            "OpenOrCreate",
            "SnippetGenerator",
            "parseTs2Ms",
            HEX64,
        ];
        names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let mut d = blank_doc(i as u64);
                d.text = vec![(*name).to_string()];
                d.code = vec![(*name).to_string()];
                d
            })
            .collect()
    }

    /// A sha256, the shape of every commit and content hash in a real transcript.
    const HEX64: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[test]
    fn a_part_of_an_identifier_finds_the_whole_identifier() {
        let (index, f) = index_docs(&identifier_docs());

        // The doc's text is *only* the identifier, so a hit can come from nothing but the
        // analyzer. The `default` tokenizer would index `SnippetGenerator` as one opaque
        // word, and `open_or_create` as three words sharing no term with `OpenOrCreate`.
        assert_eq!(texts(&search(&index, &f, &req("create")).unwrap()).len(), 2);
        assert!(
            texts(&search(&index, &f, &req("create")).unwrap())
                .contains(&"open_or_create".to_string())
        );
        assert_eq!(
            texts(&search(&index, &f, &req("generator")).unwrap()),
            vec!["SnippetGenerator"]
        );
        assert_eq!(
            texts(&search(&index, &f, &req("parse")).unwrap()),
            vec!["parseTs2Ms"]
        );
    }

    #[test]
    fn the_spellings_of_one_name_find_each_other() {
        let (index, f) = index_docs(&identifier_docs());
        for query in ["OpenOrCreate", "open_or_create", "openOrCreate"] {
            let hits = texts(&search(&index, &f, &req(query)).unwrap());
            assert!(
                hits.contains(&"open_or_create".to_string())
                    && hits.contains(&"OpenOrCreate".to_string()),
                "{query:?} found {hits:?}"
            );
        }
        // The whole, run together, is a term of its own.
        assert_eq!(
            texts(&search(&index, &f, &req("snippetgenerator")).unwrap()),
            vec!["SnippetGenerator"]
        );
    }

    /// The parts share the whole's position, so a phrase over an identifier is still a phrase.
    #[test]
    fn a_phrase_still_matches_across_and_inside_identifiers() {
        let mut docs = identifier_docs();
        let mut d = blank_doc(90);
        d.text = vec!["pub fn open_or_create(index_dir: &Path)".into()];
        docs.push(d);
        let mut d = blank_doc(91);
        // The same words, in the wrong order: a phrase must not match this.
        d.text = vec!["create or open".into()];
        docs.push(d);
        let (index, f) = index_docs(&docs);

        let hits = texts(&search(&index, &f, &req(r#""open_or_create""#)).unwrap());
        assert!(hits.contains(&"open_or_create".to_string()), "{hits:?}");
        assert!(
            hits.contains(&"pub fn open_or_create(index_dir: &Path)".to_string()),
            "{hits:?}"
        );
        assert!(!hits.contains(&"create or open".to_string()), "{hits:?}");

        // A phrase spanning an identifier and the words around it.
        let hits = texts(&search(&index, &f, &req(r#""fn open_or_create""#)).unwrap());
        assert_eq!(hits, vec!["pub fn open_or_create(index_dir: &Path)"]);
    }

    /// `RemoveLongFilter`'s stock 40-byte limit silently dropped every hash. At 255 they index,
    /// and pasting one back finds exactly the document it came from.
    #[test]
    fn a_sha256_is_findable_by_pasting_it_back() {
        let (index, f) = index_docs(&identifier_docs());
        let r = search(&index, &f, &req(HEX64)).unwrap();
        assert_eq!(texts(&r), vec![HEX64]);
        assert_eq!(r.total, 1);
    }

    /// `tool_input` is a JSON field in the default search fields, so any `word:value` parses
    /// cleanly as a JSON-subpath lookup — which made a pasted URL match nothing and say nothing.
    /// The corpus this was found on holds `https://github.com` 86 times; the query returned 0.
    #[test]
    fn a_colon_in_ordinary_text_is_not_a_field_lookup() {
        let mut docs = corpus();
        let mut d = blank_doc(90);
        d.text = vec!["see https://github.com/tarqd/session-search for the source".into()];
        docs.push(d);
        let mut d = blank_doc(91);
        d.text = vec!["note: this one is prose, not a field".into()];
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
            d.text = vec![format!("cargo test --test case_{i}")];
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
            d.text = vec!["tool call".into()];
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
        d.text = Vec::new();
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
        assert!(
            r.hits[0]
                .doc
                .text
                .join("\n")
                .starts_with("Bash\ncargo build --release")
        );
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

    /// A `tool_input` / `tool_output` value this builder cannot read is the caller's mistake,
    /// and it has to arrive *typed* — the sentence alone is not the fix.
    ///
    /// [`crate::mcp::from_anyhow`] downcasts [`FilterError`] into an `invalid_params` and turns
    /// everything else into an `internal_error`. Without the type these read to a model as "the
    /// tool broke", when the remedy is to send a different value; the wording would still look
    /// right in the terminal, so nothing but the downcast catches it. The field name is pinned
    /// alongside because the same builder answers an HTTP API and an MCP server, neither of
    /// which has a `--tool-input` for a caller to correct.
    #[test]
    fn an_unreadable_tool_filter_is_a_typed_caller_mistake_naming_the_plain_field() {
        let (index, f) = index_docs(&corpus());
        let input = |spec: &str| Filters {
            tool_input: vec![spec.into()],
            ..Filters::default()
        };
        let output = |phrase: &str| Filters {
            tool_output: vec![phrase.into()],
            ..Filters::default()
        };

        for (filters, field, needle) in [
            (input("command"), "tool_input", "KEY=VALUE"),
            (input("=cargo"), "tool_input", "non-empty key"),
            (
                input("a b=cargo"),
                "tool_input",
                "the query grammar reserves",
            ),
            (input("command="), "tool_input", "non-empty value"),
            (output(""), "tool_output", "non-empty value"),
        ] {
            let request = SearchRequest {
                filters,
                ..SearchRequest::default()
            };
            let err = search(&index, &f, &request).unwrap_err();
            let rendered = format!("{err:#}");
            let typed = err
                .downcast_ref::<FilterError>()
                .unwrap_or_else(|| panic!("{rendered} must classify as a caller mistake"));
            assert_eq!(typed.field, field, "{rendered}");
            assert!(rendered.contains(needle), "{rendered}");
            // The plain field, never a flag: two of the three front ends have no `--` to offer.
            assert!(!rendered.contains("--"), "{rendered}");
        }
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
        other.text = vec!["a turn in a different session".into()];
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
        assert!(r.hits[0].doc.text.join("\n").contains("older turn"));

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

    /// Hoisted here from `cli.rs` when `Edge`/`when_ms` were: two front ends had their own
    /// resolution of a bare day to an instant, and this is the one place the answer is decided.
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

        // No free-text query -> a head-of-body excerpt rather than an empty snippet.
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

        // The `code` analyzer highlights through an identifier too: a query for one part
        // marks that part, and a query for the whole name marks the whole name, because the
        // offsets the analyzer hands back point into the *stored* text. Both of these land in
        // `tool_output` — the tool call's result, which is analyzed as code for exactly this
        // reason — which is why that generator is not optional.
        // (A fragment ends at its last token, which is why the closing paren is in neither.)
        let r = search(&index, &f, &req("create")).unwrap();
        assert_eq!(
            r.hits[0].snippet,
            "pub fn open_or_**create**(index_dir: &Path"
        );
        let r = search(&index, &f, &req("OpenOrCreate")).unwrap();
        assert_eq!(
            r.hits[0].snippet,
            "pub fn **open_or_create**(index_dir: &Path"
        );
    }

    /// The snippet must show the identifier that matched, not a paragraph that happens to
    /// repeat its parts. `userEmail` expands to `useremail` + `user` + `email`, and the
    /// text ahead of it says `user` and `email` five times each.
    ///
    /// The body is in `code`, the field the `code` analyzer indexes — this is a property of
    /// that analyzer's snippets, and `text` is now analyzed as prose.
    #[test]
    fn the_snippet_shows_the_identifier_and_not_a_crowd_of_its_parts() {
        let mut d = blank_doc(1);
        d.code = vec![
            "The user asked for an email. Later the same user sent another email about \
             the user and the email. The user then wrote a third email, and the email \
             that the user sent after that was also about the user and the email they \
             had discussed. Deep at the end of the record sits the field userEmail."
                .into(),
        ];
        let (index, f) = index_docs(&[d]);

        for query in ["userEmail", "user_email"] {
            let r = search(&index, &f, &req(query)).unwrap();
            assert_eq!(r.total, 1, "{query:?}");
            assert!(
                r.hits[0].snippet.contains("**userEmail**"),
                "{query:?} -> {:?}",
                r.hits[0].snippet
            );
        }

        // A part asked for on its own is still a part, and still highlights every occurrence.
        let r = search(&index, &f, &req("email")).unwrap();
        assert!(
            r.hits[0].snippet.contains("an **email**"),
            "{:?}",
            r.hits[0].snippet
        );
    }

    /// A trailing plural on an acronym is part of the acronym: `IDs` is one word, not `I` + `Ds`.
    #[test]
    fn a_pluralised_acronym_stays_one_word() {
        let names = ["getIDs", "userIDs", "parseURLs", "HTTPServerError"];
        let docs: Vec<Doc> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let mut d = blank_doc(i as u64);
                d.text = vec![(*name).to_string()];
                d.code = vec![(*name).to_string()];
                d
            })
            .collect();
        let (index, f) = index_docs(&docs);

        let mut hits = texts(&search(&index, &f, &req("ids")).unwrap());
        hits.sort();
        assert_eq!(hits, vec!["getIDs", "userIDs"]);
        assert_eq!(
            texts(&search(&index, &f, &req("urls")).unwrap()),
            vec!["parseURLs"]
        );
        // The rule that makes that work must not stop an acronym meeting a real word.
        assert_eq!(
            texts(&search(&index, &f, &req("server")).unwrap()),
            vec!["HTTPServerError"]
        );
    }

    // -- the prose / code split ---------------------------------------------

    /// A document built the way `parse.rs` builds one: its markdown split across the fields.
    fn markdown_doc(seq: u64, body: &str) -> Doc {
        let parts = crate::markdown::split(body);
        let mut d = blank_doc(seq);
        d.role = "assistant".into();
        d.body = body.to_string();
        d.text = parts.text;
        d.code = parts.code;
        d.headings = parts.headings;
        d.code_langs = parts.code_langs;
        d
    }

    /// Lifting the code out of a message must not make the prose on either side of it
    /// adjacent: the words either side of a fence were never next to each other, and a phrase
    /// query that says they were is a false positive nothing in the transcript supports.
    #[test]
    fn a_phrase_does_not_match_across_a_block_the_split_removed() {
        let docs = vec![
            markdown_doc(
                0,
                "Call it before the writer exists.\n\n```rust\nlet x = 1;\n```\n\nThen re-run the tests.",
            ),
            markdown_doc(1, "# Title one\n\nAlpha ends here."),
            markdown_doc(2, "the writer exists and nothing else does"),
        ];
        let (index, f) = index_docs(&docs);

        assert_eq!(
            search(&index, &f, &req(r#""exists then""#)).unwrap().total,
            0
        );
        assert_eq!(
            search(&index, &f, &req(r#""the writer exists then re-run""#))
                .unwrap()
                .total,
            0
        );
        assert_eq!(search(&index, &f, &req(r#""one alpha""#)).unwrap().total, 0);
        // ...while a phrase *inside* one block still matches, in both documents holding it,
        // which is what the gap must not cost.
        assert_eq!(
            search(&index, &f, &req(r#""the writer exists""#))
                .unwrap()
                .total,
            2
        );
    }

    /// Only fenced blocks and inline spans reach `code`. Everything else a transcript carries
    /// — an unfenced identifier in a sentence, an attachment's file contents, a `system`
    /// notice, a tool call's name and input — is indexed as prose, so prose has to keep
    /// finding an identifier by its parts or those documents lose identifier search entirely.
    #[test]
    fn an_unfenced_identifier_is_still_found_by_one_of_its_parts() {
        let mut turn = markdown_doc(0, "It called openOrCreate on a closed SnippetGenerator.");
        turn.role = "user".into();
        let mut attachment = blank_doc(1);
        attachment.role = "attachment".into();
        attachment.text = vec!["pub fn open_or_create(dir: &Path) -> Result<Index>".into()];
        let mut system = blank_doc(2);
        system.role = "system".into();
        system.text = vec!["hook ran: parseTs2Ms failed".into()];
        let (index, f) = index_docs(&[turn, attachment, system]);

        for (query, role) in [
            ("snippet", "user"),
            ("generator", "user"),
            ("open_or_create", "user"),
            ("openOrCreate", "attachment"),
            ("create", "attachment"),
            ("parse", "system"),
        ] {
            let hits = search(&index, &f, &req_all(query)).unwrap().hits;
            assert!(
                hits.iter().any(|h| h.doc.role == role),
                "{query:?} did not reach the {role} document"
            );
        }
    }

    /// The whole point of the split: `text` is stemmed English and `code` is not.
    ///
    /// One analyzer cannot do both. Stemming a snippet turns `compiled` into `compil` and
    /// matches a search for `compiling`, which is wrong for code and right for prose; not
    /// stemming leaves `compiling` unable to find `compiled`, which is the reverse.
    #[test]
    fn a_prose_word_is_stemmed_and_the_same_word_in_code_is_not() {
        let prose = markdown_doc(0, "The crate compiled cleanly on the second attempt.");
        let code = markdown_doc(
            1,
            "It came out of this line:\n\n```rust\nlet compiled = 1;\n```",
        );
        let (index, f) = index_docs(&[prose, code]);

        // `compiling` reaches the prose `compiled` through the stemmer...
        let hits = texts(&search(&index, &f, &req("compiling")).unwrap());
        assert_eq!(
            hits,
            vec!["The crate compiled cleanly on the second attempt."]
        );

        // ...and does not reach the identical word inside the fence, which is indexed verbatim.
        let r = search(&index, &f, &req("compiling")).unwrap();
        assert_eq!(r.total, 1, "the fenced `compiled` must not stem");

        // Spelled exactly, it finds both: the prose through the stem, the code through itself.
        assert_eq!(search(&index, &f, &req("compiled")).unwrap().total, 2);
    }

    /// An identifier inside a fenced block is indexed by the `code` analyzer, so a part of it
    /// finds it — which is exactly what the prose analyzer on `text` could not do.
    #[test]
    fn an_identifier_in_a_fenced_block_is_found_by_one_of_its_parts() {
        let d = markdown_doc(
            0,
            "Here is the function that opens it.\n\n```rust\npub fn open_or_create(dir: &Path) {}\n```",
        );
        assert_eq!(d.code_langs, ["rust"]);
        assert!(
            !d.text.join("\n").contains("open_or_create"),
            "{:?}",
            d.text
        );
        let (index, f) = index_docs(&[d]);

        for query in ["create", "OpenOrCreate", "open_or_create", "openorcreate"] {
            assert_eq!(
                search(&index, &f, &req(query)).unwrap().total,
                1,
                "{query:?} did not reach the fenced block"
            );
        }
        // The prose around it is still prose: it is the one being stemmed, not the snippet.
        assert_eq!(search(&index, &f, &req("opening")).unwrap().total, 1);
    }

    /// `code_lang` is a fast field, so it counts as a facet exactly like `tool_name`.
    #[test]
    fn code_langs_are_counted_as_a_facet() {
        let docs = vec![
            markdown_doc(0, "one\n\n```rust\nlet a = 1;\n```"),
            markdown_doc(1, "two\n\n```rust\nlet b = 2;\n```"),
            markdown_doc(2, "three\n\n```python\nb = 2\n```"),
            // Two fences, two languages, one document — a multi-valued field.
            markdown_doc(3, "four\n\n```bash\nls\n```\n\n```rust\nlet c = 3;\n```"),
            // No fence at all: this one carries no value and must not be counted.
            markdown_doc(4, "five, in prose, with an `inline span`"),
        ];
        let (index, f) = index_docs(&docs);

        let r = facets(&index, &f, "code_lang", &SearchRequest::default()).unwrap();
        let counts: BTreeMap<&str, u64> = r
            .values
            .iter()
            .map(|c| (c.value.as_str(), c.count))
            .collect();
        assert_eq!(counts.get("rust"), Some(&3));
        assert_eq!(counts.get("python"), Some(&1));
        assert_eq!(counts.get("bash"), Some(&1));
        assert_eq!(r.matching_docs, 5, "all five documents matched");
        // Four documents hold five values between them, and `docs_with_value` counts
        // documents: the fifth document has no fence at all, and the one with two fences is
        // one document however many buckets it lands in.
        assert_eq!(r.docs_with_value, 4, "four documents, five values");
        assert_eq!(
            r.values.iter().map(|v| v.count).sum::<u64>(),
            5,
            "the buckets count values"
        );

        // And the same field filters, case-insensitively, the way `--tool` does.
        let mut only_rust = SearchRequest::default();
        only_rust.filters.lang = vec!["RUST".into()];
        assert_eq!(search(&index, &f, &only_rust).unwrap().total, 3);
        let mut two = SearchRequest::default();
        two.filters.lang = vec!["python".into(), "bash".into()];
        assert_eq!(search(&index, &f, &two).unwrap().total, 2);
    }

    /// A heading names what the section under it is about, so a term in one is a better answer
    /// than the same term in the middle of a paragraph.
    ///
    /// The two documents differ in exactly one character — the `#` that makes the first line a
    /// heading — so their `text` is identical and the boost is the only thing separating them.
    #[test]
    fn a_term_in_a_heading_outranks_the_same_term_in_a_paragraph() {
        let tail = "\n\nthe surrounding paragraph is word for word the same in both.";
        let with_heading = markdown_doc(0, &format!("# Retry policy{tail}"));
        let without = markdown_doc(1, &format!("Retry policy{tail}"));
        assert_eq!(with_heading.text, without.text, "only the heading differs");
        assert_eq!(with_heading.headings, ["Retry policy"]);
        assert!(without.headings.is_empty());

        let (index, f) = index_docs(&[with_heading, without]);
        let r = search(&index, &f, &req("retry")).unwrap();
        assert_eq!(r.total, 2);
        assert_eq!(r.hits[0].doc.seq, 0, "the heading hit ranks first");
        assert!(
            r.hits[0].score > r.hits[1].score,
            "{} vs {}",
            r.hits[0].score,
            r.hits[1].score
        );
    }

    /// A tool call keeps its output in `tool_output`, so both the search and the snippet have
    /// to reach a field that neither `text` nor `code` holds.
    #[test]
    fn a_tool_calls_output_is_searchable_and_snippets_from_tool_output() {
        let (index, f) = index_docs(&corpus());
        let r = search(&index, &f, &req("Compiling")).unwrap();

        assert_eq!(r.total, 1);
        let hit = &r.hits[0];
        assert_eq!(hit.doc.kind, DocKind::ToolCall);
        assert_eq!(hit.doc.tool_name.as_deref(), Some("Bash"));
        assert!(
            hit.doc.text.join("\n").contains("cargo build"),
            "the input is still text: {:?}",
            hit.doc.text
        );
        assert!(
            !hit.doc.text.join("\n").contains("Compiling"),
            "the output is not: {:?}",
            hit.doc.text
        );
        assert!(
            !hit.doc.code.join("\n").contains("Compiling"),
            "nor is it code: {:?}",
            hit.doc.code
        );
        assert!(
            hit.doc
                .tool_output
                .as_deref()
                .is_some_and(|o| o.contains("Compiling")),
            "{:?}",
            hit.doc.tool_output
        );
        assert!(
            hit.snippet.contains("**Compiling**"),
            "the snippet comes from `tool_output`: {:?}",
            hit.snippet
        );
    }

    /// A hash is indexed whole and never in pieces, so a single hex character finds nothing.
    #[test]
    fn a_hash_contributes_no_single_character_terms() {
        let (index, f) = index_docs(&identifier_docs());
        assert_eq!(
            texts(&search(&index, &f, &req(HEX64)).unwrap()),
            vec![HEX64]
        );
        for junk in ["f", "d", "0", "86"] {
            assert_eq!(
                search(&index, &f, &req(junk)).unwrap().total,
                0,
                "{junk:?} matched something"
            );
        }
    }

    /// `snippet_field` is what the UI labels the excerpt with — "matched in what the tool
    /// printed", "matched in the model's thinking". A label that is always `text` is a
    /// misattributed quote, and it looks exactly like a correct one.
    #[test]
    fn a_snippet_reports_the_body_it_was_actually_cut_from() {
        let mut printed = blank_doc(0);
        printed.kind = DocKind::ToolCall;
        printed.role = "assistant".into();
        printed.tool_name = Some("Bash".into());
        printed.body = "Bash\ncargo test".into();
        printed.text = vec!["Bash".into(), "cargo test".into()];
        printed.tool_output = Some("error: linker `cc` not found — quernstone".into());

        let mut thought = blank_doc(1);
        thought.role = "assistant".into();
        thought.body = "the visible answer".into();
        thought.text = vec!["the visible answer".into()];
        thought.thinking = Some("a private deliberation about parsnips".into());

        let mut failed = blank_doc(2);
        failed.kind = DocKind::ToolCall;
        failed.role = "assistant".into();
        failed.tool_name = Some("Cargo".into());
        failed.body = "Cargo\ncargo build".into();
        failed.text = vec!["Cargo".into(), "cargo build".into()];
        failed.tool_output = Some("error: could not compile session-search".into());
        failed.is_error = true;

        let (index, f) = index_docs(&[printed, thought, failed]);

        // The term is in `tool_output` only; the call side is a tool name and a command line.
        let r = search(&index, &f, &req("quernstone")).unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].snippet_field, SnippetSource::ToolOutput);
        let hit = &r.hits[0];
        let marked: Vec<&str> = hit
            .snippet_marks
            .iter()
            .map(|m| &hit.snippet[m.clone()])
            .collect();
        assert_eq!(
            marked,
            vec!["quernstone"],
            "the marks name the matched span and nothing else: {:?}",
            hit.snippet
        );

        let r0 = SearchRequest {
            include_thinking: true,
            ..req("parsnips")
        };
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].snippet_field, SnippetSource::Thinking);

        // `--errors-only` carries no free-text query, so it always lands in the fallback. A
        // failed call leads with its result, and the label has to follow it there.
        let mut r0 = SearchRequest::default();
        r0.filters.errors_only = true;
        let r = search(&index, &f, &r0).unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].snippet_field, SnippetSource::ToolOutput);
        assert!(r.hits[0].snippet.starts_with("error: could not compile"));
        assert!(
            r.hits[0].snippet_marks.is_empty(),
            "an excerpt marks nothing"
        );
    }

    /// A sort silently running backwards produces plausible output, so only the order itself
    /// can catch it.
    #[test]
    fn a_time_sort_orders_by_timestamp_in_the_direction_it_names() {
        // Deliberately not in `seq` order: a collector that ignored `sort` and returned docs
        // in insertion order would pass against an already-sorted corpus.
        // Whole seconds apart: the `timestamp` fast field stores seconds, so docs that differ
        // only in milliseconds tie and come back in insertion order — which would make this
        // test pass without the sort doing anything.
        let docs: Vec<Doc> = [(0u64, 300i64), (1, 100), (2, 500), (3, 200), (4, 400)]
            .into_iter()
            .map(|(seq, seconds)| {
                let mut d = blank_doc(seq);
                d.body = "tantivy ordering probe".into();
                d.text = vec!["tantivy ordering probe".into()];
                d.timestamp_ms = Some(1_700_000_000_000 + seconds * 1000);
                d
            })
            .collect();
        let (index, f) = index_docs(&docs);

        let ids = |sort, offset, facets: &[&str]| -> Vec<String> {
            let r0 = SearchRequest {
                sort,
                offset,
                facets: facets.iter().map(|s| (*s).to_string()).collect(),
                ..req("probe")
            };
            search(&index, &f, &r0)
                .unwrap()
                .hits
                .iter()
                .map(|h| h.doc.doc_id.clone())
                .collect()
        };

        let newest = ["s1:-:2", "s1:-:4", "s1:-:0", "s1:-:3", "s1:-:1"];
        let oldest: Vec<&str> = newest.iter().rev().copied().collect();
        assert_eq!(ids(SortBy::Newest, 0, &[]), newest);
        assert_eq!(ids(SortBy::Oldest, 0, &[]), oldest);

        // Faceting builds a second collector tuple, and paging goes through `and_offset`;
        // either could be wired to a differently-ordered `TopDocs` without the plain case
        // noticing.
        assert_eq!(ids(SortBy::Newest, 0, &["tool_name"]), newest);
        assert_eq!(ids(SortBy::Newest, 2, &[]), newest[2..]);
        assert_eq!(ids(SortBy::Oldest, 2, &["tool_name"]), oldest[2..]);
    }

    /// A document with no timestamp has no value in the fast field at all. Tantivy sorts on
    /// `Option<T>` and orders `None` last either way, so such a document is still returned —
    /// which is what keeps `total` honest, since `Count` has no idea the sort exists.
    #[test]
    fn a_time_sort_still_returns_a_document_that_has_no_timestamp() {
        let mut docs: Vec<Doc> = [(0u64, 100i64), (1, 200)]
            .into_iter()
            .map(|(seq, seconds)| {
                let mut d = blank_doc(seq);
                d.body = "undated probe".into();
                d.text = vec!["undated probe".into()];
                d.timestamp_ms = Some(1_700_000_000_000 + seconds * 1000);
                d
            })
            .collect();
        let mut undated = blank_doc(2);
        undated.body = "undated probe".into();
        undated.text = vec!["undated probe".into()];
        undated.timestamp_ms = None;
        docs.push(undated);
        let (index, f) = index_docs(&docs);

        for sort in [SortBy::Newest, SortBy::Oldest] {
            let r = search(
                &index,
                &f,
                &SearchRequest {
                    sort,
                    ..req("undated")
                },
            )
            .unwrap();
            let ids: Vec<&str> = r.hits.iter().map(|h| h.doc.doc_id.as_str()).collect();
            assert_eq!(
                r.hits.len(),
                r.total,
                "{sort:?}: every counted match must be reachable, got {ids:?}"
            );
            assert_eq!(
                ids.last(),
                Some(&"s1:-:2"),
                "{sort:?}: the undated document sorts last, not first and not away"
            );
        }
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
        assert_eq!(r.hits[0].doc.text, ["the visible answer"]);
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
        // The round trip, not the scope: `docs[3]` is deliberately meta, which the default
        // scope refuses and this test is not about.
        r0.filters.all_records = true;
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
        docs[7].text = Vec::new();
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
        let out =
            crate::parse::parse_whole(&fixture, &crate::parse::ParseOptions::default()).unwrap();
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
                .any(|d| d.text.join("\n").contains("unterminated") && d.bash_cmd.is_none())
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
        assert!(
            r.hits[0]
                .doc
                .text
                .join("\n")
                .starts_with("Bash\ncargo test"),
            "{:?}",
            r.hits[0].doc.text
        );

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
        let out =
            crate::parse::parse_whole(&fixture, &crate::parse::ParseOptions::default()).unwrap();
        assert!(out.docs.len() > 5, "fixture should yield real docs");
        let (index, f) = index_docs(&out.docs);

        // Free text reaches the tool calls, because `parse.rs` folds the tool input into `text`.
        let r = search(&index, &f, &req("git")).unwrap();
        assert!(r.total > 0);
        assert!(r.hits.iter().any(|h| h.snippet.contains("**")));

        // The project comes from the record `cwd`, and matches by prefix. On a real slice the
        // default scope is doing visible work, so the two halves are asserted together: what
        // came back plus what was refused is the whole file, and neither number is guessed.
        let mut r0 = SearchRequest::default();
        r0.filters.project = Some("/home/user".into());
        let scoped = search(&index, &f, &r0).unwrap();
        assert!(scoped.hidden > 0, "this fixture carries apparatus records");
        assert_eq!(scoped.total + scoped.hidden, out.docs.len());

        r0.filters.all_records = true;
        let whole = search(&index, &f, &r0).unwrap();
        assert_eq!(whole.total, out.docs.len());
        assert_eq!(whole.hidden, 0, "nothing refused, nothing reported");

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

    /// A RAM index whose documents carry the `context_text` header composed against one session
    /// row — what `index.rs` does with the merged `sessions.json` entry.
    fn index_with_session(
        docs: &[Doc],
        session: &crate::parse::SessionInfo,
    ) -> (tantivy::Index, Fields) {
        let (schema, fields) = crate::schema::build_schema();
        let index = crate::tokenizer::create_in_ram(schema.clone());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for doc in docs {
            let json = crate::schema::doc_to_json(doc, Some(session), true).to_string();
            writer
                .add_document(TantivyDocument::parse_json(&schema, &json).unwrap())
                .unwrap();
        }
        writer.commit().unwrap();
        (index, fields)
    }

    /// One turn: a prompt, the answer to it, and a tool call inside it. `turn_prompt` is what
    /// `parse.rs` puts on every document of a turn.
    fn one_turn() -> Vec<Doc> {
        let prompt = "fix the tokenizer";
        let mut opener = blank_doc(0);
        opener.text = vec![prompt.into()];
        opener.body = prompt.into();
        opener.turn_prompt = Some(prompt.into());

        let mut agreement = blank_doc(1);
        agreement.text = vec!["yes".into()];
        agreement.body = "yes".into();
        agreement.turn_prompt = Some(prompt.into());

        let mut call = blank_doc(2);
        call.kind = DocKind::ToolCall;
        call.role = "assistant".into();
        call.tool_name = Some("Bash".into());
        call.tool_input = Some(json!({"command": "cargo build --release"}));
        call.text = vec!["Bash\ncargo build --release".into()];
        call.body = "Bash\ncargo build --release".into();
        call.turn_prompt = Some(prompt.into());

        vec![opener, agreement, call]
    }

    /// The defect contextual BM25 exists to fix. Half of all user messages are "yes", "do
    /// that", "still broken", and a tool call is a program name and a command line; neither is
    /// retrievable by anything a person would type. With the turn's prompt in the header they
    /// are — and the prompt itself is still one hit, not two.
    #[test]
    fn a_bare_yes_is_found_by_what_its_turn_asked() {
        let (index, f) = index_with_session(&one_turn(), &crate::parse::SessionInfo::default());
        let r = search(&index, &f, &req("tokenizer")).unwrap();
        let seqs: Vec<u64> = r.hits.iter().map(|h| h.doc.seq).collect();
        // The whole turn: the prompt through its body, the bare `yes` and the tool call
        // through the header the prompt gave them.
        assert_eq!(r.total, 3, "{seqs:?}");
        assert!(seqs.contains(&1), "the bare `yes` is reachable: {seqs:?}");
        assert!(seqs.contains(&2), "so is the tool call: {seqs:?}");
        assert_eq!(
            seqs.iter().filter(|s| **s == 0).count(),
            1,
            "the prompt matches its body and is not doubled by a header repeating it"
        );
    }

    /// The session half of the header, which only the indexer's merged `sessions.json` row
    /// knows: a `Bash cargo build --release` document says nothing about what it was for, and
    /// is found anyway by a word out of the session's title.
    #[test]
    fn a_tool_call_is_found_by_the_words_of_its_session() {
        let session = crate::parse::SessionInfo {
            title: Some("Tuning the markdown analyzer".into()),
            first_prompt: Some("make retrieval find fragments by their context".into()),
            ..crate::parse::SessionInfo::default()
        };
        let (index, f) = index_with_session(&one_turn(), &session);
        for query in ["analyzer", "retrieval", "session-search", "main"] {
            let r = search(&index, &f, &req(query)).unwrap();
            assert!(
                r.hits
                    .iter()
                    .any(|h| h.doc.tool_name.as_deref() == Some("Bash")),
                "{query:?} did not reach the tool call"
            );
        }
    }

    /// The header is scaffolding, so it may decide *whether* a document is retrieved and never
    /// what the reader is shown. It cannot be a snippet — it is not stored — but the fallback
    /// has to produce something honest rather than a blank line, and the `Doc` handed back must
    /// carry none of it.
    #[test]
    fn a_hit_matched_only_through_its_header_still_shows_its_own_body() {
        let session = crate::parse::SessionInfo {
            title: Some("Tuning the markdown analyzer".into()),
            ..crate::parse::SessionInfo::default()
        };
        let (index, f) = index_with_session(&one_turn(), &session);
        let r = search(&index, &f, &req("analyzer")).unwrap();
        let hit = r
            .hits
            .iter()
            .find(|h| h.doc.seq == 1)
            .expect("the bare `yes` matched through its header");
        assert_eq!(hit.snippet, "yes", "the snippet comes from the body");
        assert!(!hit.snippet.contains("analyzer"), "{}", hit.snippet);
        assert!(
            !hit.snippet.contains("**"),
            "nothing to highlight in a body"
        );
        // Nothing of the header survives into what the caller reads back.
        assert!(hit.doc.turn_prompt.is_none());
        assert!(!hit.doc.body.contains("analyzer"));
        assert!(!hit.doc.text.join(" ").contains("analyzer"));
    }

    /// The discount, pinned by a corpus where it decides the order.
    ///
    /// Within one session the IDF collapse already makes a shared header cheap — every document
    /// carries it, so the term is common — and a body match wins at any boost. The case that
    /// needs the discount is the opposite one: a *small* session whose title names the term,
    /// beside larger sessions where the term is written in a document's body. Across the
    /// corpus the header term is then the rarer one, and at a boost of 1.0 BM25 ranks the two
    /// documents that merely happened in a session *about* tokenizers above the two that
    /// discuss one. The body wins below roughly 0.8; 0.3 leaves a margin, and this corpus
    /// fails the assertion at 1.0.
    #[test]
    fn a_body_term_outranks_the_same_term_in_a_header() {
        let doc = |seq: u64, session: &str, text: &str| {
            let mut d = blank_doc(seq);
            d.doc_id = format!("{session}:-:{seq}");
            d.session_id = session.into();
            d.source_path = format!("/tmp/{session}.jsonl");
            d.project = None;
            d.git_branch = None;
            d.text = vec![text.into()];
            d.body = text.into();
            d
        };
        let titled = |title: &str| crate::parse::SessionInfo {
            title: Some(title.into()),
            ..crate::parse::SessionInfo::default()
        };
        // Two documents in a session titled by the term, with bodies that never say it.
        let about = titled("tokenizer");
        // Six documents in two other sessions, two of which say the term in their bodies.
        let facets = titled("adding facets to the cli");
        let windows = titled("snapping context windows to turns");
        let corpus = [
            (
                doc(0, "a", "yes please do exactly that and nothing more"),
                &about,
            ),
            (
                doc(1, "a", "the parts of a name share one position now"),
                &about,
            ),
            (
                doc(
                    0,
                    "b",
                    "the tokenizer splits identifiers at a case boundary",
                ),
                &facets,
            ),
            (
                doc(1, "b", "facets count the values of one fast field"),
                &facets,
            ),
            (
                doc(2, "b", "a bucket is a value and not a document"),
                &facets,
            ),
            (
                doc(0, "c", "the tokenizer keeps a hash whole and alone"),
                &windows,
            ),
            (
                doc(1, "c", "a window snaps to the prompt that opened it"),
                &windows,
            ),
            (
                doc(2, "c", "the cap keeps the head of a long turn"),
                &windows,
            ),
        ];

        let (schema, f) = crate::schema::build_schema();
        let index = crate::tokenizer::create_in_ram(schema.clone());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for (doc, session) in &corpus {
            let json = crate::schema::doc_to_json(doc, Some(session), true).to_string();
            writer
                .add_document(TantivyDocument::parse_json(&schema, &json).unwrap())
                .unwrap();
        }
        writer.commit().unwrap();

        let r = search(&index, &f, &req("tokenizer")).unwrap();
        let scored: Vec<(String, Score)> = r
            .hits
            .iter()
            .map(|h| (h.doc.doc_id.clone(), h.score))
            .collect();
        assert_eq!(
            r.total, 4,
            "two bodies and one two-document header: {scored:?}"
        );

        let score_of = |id: &str| scored.iter().find(|(d, _)| d == id).unwrap().1;
        let body_floor = score_of("b:-:0").min(score_of("c:-:0"));
        let header_ceiling = score_of("a:-:0").max(score_of("a:-:1"));
        assert!(
            body_floor > header_ceiling,
            "scaffolding must not outrank content: {scored:?}"
        );
        let ids: Vec<&str> = scored.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(&ids[..2], ["b:-:0", "c:-:0"], "{scored:?}");
    }

    // -----------------------------------------------------------------------
    // find similar
    // -----------------------------------------------------------------------

    /// Twelve documents in six two-document turns, split between two topics that share **no**
    /// vocabulary at all.
    ///
    /// The disjoint vocabulary is what makes the assertions mean something on a corpus this
    /// small. `SIMILAR_MAX_DOC_FREQUENCY_FLOOR` is 50, so on twelve documents nothing is ever
    /// cut for being too common — a filler word shared by both topics would become a query term
    /// and every document would match, leaving "topic B is absent" untestable. Every content
    /// word here occurs in six documents, comfortably over `SIMILAR_MIN_DOC_FREQUENCY`, and no
    /// word occurs in both halves.
    fn similar_docs() -> Vec<Doc> {
        const SCORING: &str = "fieldnorm normalisation lets bm25 discount lengthy documents";
        const PARSING: &str = "recursive descent parsing turns grammar into syntax nodes";

        (0..12u64)
            .map(|seq| {
                let mut d = blank_doc(seq);
                // Two documents per turn, so excluding "the source turn" and excluding "the
                // source document" are observably different outcomes.
                d.turn_seq = seq - seq % 2;
                // One document carries a real 36-character uuid so a *prefix* of one can be
                // tested; `uuid-{seq}` prefixes each other and would only ever be ambiguous.
                d.uuid = Some(if seq == 3 {
                    "b20208d8-fbdb-5918-ba69-d203de6ed6dc".to_string()
                } else {
                    format!("uuid-{seq}")
                });
                d.doc_id = format!("s1:-:5e0f1a2b:{seq}");
                d.text = vec![if seq < 6 {
                    format!("{SCORING} {seq}")
                } else {
                    format!("{PARSING} {seq}")
                }];
                d
            })
            .collect()
    }

    fn similar_req(source: SimilarSource) -> SearchRequest {
        SearchRequest {
            similar_to: Some(source),
            ..SearchRequest::default()
        }
    }

    fn seeded(index: &tantivy::Index, f: &Fields, spec: &str) -> SimilarSource {
        resolve_similar(index, f, spec, &[SimilarField::Text], false).unwrap()
    }

    fn refs(r: &SearchResponse) -> Vec<u64> {
        r.hits.iter().map(|h| h.doc.seq).collect()
    }

    /// `MoreLikeThisQuery` returning nothing is indistinguishable from "nothing here is
    /// similar", and it reports no error at all. The advice about which knobs decide it has to
    /// travel with the empty answer.
    #[test]
    fn an_empty_similarity_result_names_the_knobs_on_the_response() {
        let (index, f) = index_docs(&similar_docs());
        let mut request = similar_req(seeded(&index, &f, "s1:3"));
        // Nothing is indexed under this project, so the similarity query cannot answer at all.
        request.filters.project = Some("/nowhere".into());
        let empty = search(&index, &f, &request).unwrap();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.warnings, vec![WARN_EMPTY_SIMILARITY_SEED.to_string()]);
    }

    /// Every spelling of a reference names the same document, and so does an unambiguous prefix
    /// of each. This is the acceptance box: `--similar-to` must resolve a reference the way
    /// `show` resolves an id.
    #[test]
    fn every_spelling_of_a_document_reference_resolves_to_one_document() {
        let (index, f) = index_docs(&similar_docs());
        for spec in [
            "s1:3",                                 // SESSION:SEQ
            "s1:-:3",                               // SESSION:AGENT:SEQ, `-` for the main file
            "s1:-:5e0f1a2b:3",                      // the doc_id itself
            "b20208d8-fbdb-5918-ba69-d203de6ed6dc", // a record uuid
            "b20208d8",                             // the leading block of one, as pasted
        ] {
            let doc = resolve_doc(&index, &f, spec).unwrap_or_else(|e| panic!("{spec:?}: {e:#}"));
            assert_eq!(doc.seq, 3, "{spec:?} resolved to {}", doc.doc_id);
        }
        // A session prefix works in the coordinate shape too, exactly as `--session` does.
        assert_eq!(resolve_doc(&index, &f, "s:3").unwrap().seq, 3);
    }

    /// An ambiguous prefix is a question, never a silent pick. A "find similar" seeded from a
    /// document the caller never named produces a plausible, entirely wrong answer.
    #[test]
    fn an_ambiguous_document_reference_names_its_candidates() {
        let mut docs = similar_docs();
        docs[4].uuid = Some("dup-alpha".into());
        docs[5].uuid = Some("dup-beta".into());
        let (index, f) = index_docs(&docs);

        let err = format!("{:#}", resolve_doc(&index, &f, "dup").unwrap_err());
        assert!(err.contains("ambiguous document reference"), "{err}");
        assert!(err.contains("at least 2"), "{err}");
        assert!(
            err.contains(&docs[4].doc_id) && err.contains(&docs[5].doc_id),
            "{err}"
        );

        // The exact-before-prefix rule: one of them spelled in full is not ambiguous.
        assert_eq!(resolve_doc(&index, &f, "dup-alpha").unwrap().seq, 4);
    }

    /// Two documents at the same `SESSION:SEQ` that are not the same record stay an ambiguity.
    ///
    /// §9's `resetSessionFile()` lets two *different* transcripts carry one session id, and
    /// `seq` restarts at 0 in every file — so `s1:0` names turn 0 of one conversation and turn 0
    /// of another, and they agree on session, agent and `seq`. Without the `uuid` half of
    /// [`one_of`]'s same-record test this passes silently and wrongly: the pair is declared one
    /// logical record, whichever file sorts first by `doc_id` is returned, and the caller is
    /// handed a document out of a transcript it never named with no error and no warning. That
    /// is the hazard `get_turn` warns a model about for a bare turn number, arriving through the
    /// resolver instead.
    #[test]
    fn two_transcripts_sharing_a_session_id_stay_an_ambiguous_reference() {
        let docs: Vec<Doc> = [
            ("aaaaaaaa", "/tmp/a/s1.jsonl", "record-in-a"),
            ("bbbbbbbb", "/tmp/b/s1.jsonl", "record-in-b"),
        ]
        .into_iter()
        .map(|(tag, path, uuid)| Doc {
            doc_id: format!("s1:-:{tag}:0"),
            source_path: path.into(),
            // Different records: two files that merely share a session id are two conversations.
            uuid: Some(uuid.into()),
            body: format!("{tag} turn 0"),
            ..blank_doc(0)
        })
        .collect();
        let (index, f) = index_docs(&docs);

        let err = format!("{:#}", resolve_doc(&index, &f, "s1:0").unwrap_err());
        assert!(err.contains("ambiguous document reference"), "{err}");
        assert!(
            err.contains("s1:-:aaaaaaaa:0") && err.contains("s1:-:bbbbbbbb:0"),
            "both candidates are named so the caller can pick one: {err}"
        );

        // The documents themselves were never ambiguous — only the coordinate was.
        assert_eq!(
            resolve_doc(&index, &f, "record-in-b").unwrap().doc_id,
            "s1:-:bbbbbbbb:0"
        );
    }

    /// §9's `relocated` duplicate is the one ambiguity with a single answer, and it survives the
    /// test above: one transcript indexed under two project keys is the *same record* read from
    /// two files, so the two candidates carry identical record uuids.
    ///
    /// The pair matters. Tightening [`one_of`] until this case errors too would refuse a
    /// reference the caller has no other spelling for — `cli::source_path_for` returns `None` in
    /// exactly this case, so there is no `--session`-plus-file form to fall back to, and
    /// `--similar-to <uuid>` on a relocated transcript would stop working entirely.
    #[test]
    fn one_record_read_from_two_files_is_still_one_record() {
        let docs: Vec<Doc> = [
            ("aaaaaaaa", "/tmp/a/s1.jsonl"),
            ("bbbbbbbb", "/tmp/b/s1.jsonl"),
        ]
        .into_iter()
        .map(|(tag, path)| Doc {
            doc_id: format!("s1:-:{tag}:0"),
            source_path: path.into(),
            // The same record uuid in both files — that is what makes it one transcript.
            uuid: Some("relocated-record".into()),
            ..blank_doc(0)
        })
        .collect();
        let (index, f) = index_docs(&docs);

        assert_eq!(resolve_doc(&index, &f, "s1:0").unwrap().seq, 0);
        assert_eq!(
            resolve_doc(&index, &f, "relocated-record").unwrap().doc_id,
            "s1:-:aaaaaaaa:0",
            "the first by `doc_id`, so two runs name the same file"
        );
    }

    /// An unknown reference is an error naming the grammar, not an empty result set. The two
    /// look identical on a terminal and only one of them is an answer.
    #[test]
    fn an_unknown_document_reference_is_an_error_not_an_empty_page() {
        let (index, f) = index_docs(&similar_docs());
        let err = format!("{:#}", resolve_doc(&index, &f, "nope-nothing").unwrap_err());
        assert!(err.contains("no document matches"), "{err}");
        assert!(err.contains("SESSION:SEQ"), "{err}");

        let err = format!("{:#}", resolve_doc(&index, &f, "s1:999").unwrap_err());
        assert!(err.contains("no document at"), "{err}");
    }

    /// A seed wider than [`SIMILAR_MAX_QUERY_TERMS`] returns the *same* ranked list every
    /// time, and the same `total`.
    ///
    /// This is the regression test for the reason [`similar_terms`] exists. Tantivy's own
    /// selection breaks ties on `tf * idf` by `HashMap` iteration order, so a seed with more
    /// equally-scored candidates than the cap admits produced a different hit set on every
    /// call against one unchanged index — different results for the same command, a `total`
    /// that moved while nothing else did, and paging that repeated and skipped documents
    /// because page 2 was cut from a different query than page 1.
    ///
    /// The corpus is built to make every candidate tie *exactly*: sixty distinct words, each
    /// said once by the seed (so `tf` is 1 for all of them) and each appearing in exactly three
    /// filler documents (so `doc_freq`, and therefore `idf`, is identical too). Under the old
    /// implementation ten runs of this returned five different totals and ten different hit
    /// lists.
    #[test]
    fn a_seed_wider_than_the_term_cap_still_searches_reproducibly() {
        let words: Vec<String> = (0..60).map(|i| format!("scoringword{i:03}")).collect();

        let mut docs = Vec::new();
        let mut seed = blank_doc(0);
        seed.turn_seq = 0;
        seed.uuid = Some("seedref".into());
        seed.doc_id = "s1:-:5e0f1a2b:0".into();
        seed.text = vec![words.join(" ")];
        docs.push(seed);

        let mut seq = 1u64;
        for word in &words {
            for _ in 0..SIMILAR_MIN_DOC_FREQUENCY {
                let mut d = blank_doc(seq);
                d.turn_seq = seq;
                d.uuid = Some(format!("fill-{seq}"));
                d.doc_id = format!("s1:-:5e0f1a2b:{seq}");
                d.text = vec![format!("{word} in a filler document")];
                docs.push(d);
                seq += 1;
            }
        }
        let (index, f) = index_docs(&docs);

        assert!(
            similar_terms(
                &index.reader().unwrap().searcher(),
                &f,
                &seeded(&index, &f, "seedref"),
            )
            .unwrap()
            .len()
                == SIMILAR_MAX_QUERY_TERMS,
            "the seed has to overflow the cap or this proves nothing"
        );

        let page = |offset: usize| {
            let source = seeded(&index, &f, "seedref");
            search(
                &index,
                &f,
                &SearchRequest {
                    limit: 10,
                    offset,
                    ..similar_req(source)
                },
            )
            .unwrap()
        };

        let first = page(0);
        for _ in 0..9 {
            let again = page(0);
            assert_eq!(
                refs(&again),
                refs(&first),
                "the same command against one unchanged index has to return one answer"
            );
            assert_eq!(again.total, first.total, "and one total");
        }

        // Paging is the same query cut twice, so the two pages have to be disjoint.
        let second = page(10);
        let overlap: Vec<u64> = refs(&second)
            .into_iter()
            .filter(|seq| refs(&first).contains(seq))
            .collect();
        assert!(
            overlap.is_empty(),
            "page 2 repeated documents from page 1: {overlap:?}"
        );
    }

    /// `--similar-to --facets <field>` reports arithmetic that adds up.
    ///
    /// `docs_with_value` is by construction a subset of the matching set, so it can never
    /// exceed `matching_docs` — but it is counted by a *second* `searcher.search` over
    /// `query.box_clone()`, so it only holds if the clone is the same query. Against a
    /// `MoreLikeThisQuery`, whose `weight()` re-derived its own clauses on every call, a single
    /// command could print `62 of 58 matching docs have a value`. Wide seed for the same reason
    /// as the test above: the divergence only appears past the term cap.
    #[test]
    fn a_similarity_facet_counts_a_subset_of_the_documents_it_matched() {
        let words: Vec<String> = (0..60).map(|i| format!("facetword{i:03}")).collect();

        let mut docs = Vec::new();
        let mut seed = blank_doc(0);
        seed.turn_seq = 0;
        seed.uuid = Some("seedref".into());
        seed.doc_id = "s1:-:5e0f1a2b:0".into();
        seed.text = vec![words.join(" ")];
        docs.push(seed);

        let mut seq = 1u64;
        for word in &words {
            for _ in 0..SIMILAR_MIN_DOC_FREQUENCY {
                let mut d = blank_doc(seq);
                d.turn_seq = seq;
                d.uuid = Some(format!("fill-{seq}"));
                d.doc_id = format!("s1:-:5e0f1a2b:{seq}");
                d.text = vec![format!("{word} in a filler document")];
                // Every document has a `role`, so `docs_with_value` should equal `matching_docs`.
                d.role = "user".into();
                docs.push(d);
                seq += 1;
            }
        }
        let (index, f) = index_docs(&docs);

        for _ in 0..5 {
            let req = similar_req(seeded(&index, &f, "seedref"));
            let hits = search(&index, &f, &req).unwrap();
            let facet = facets(&index, &f, "role", &req).unwrap();
            assert_eq!(
                facet.matching_docs, hits.total as u64,
                "the facet pass and the hit pass have to agree on what matched"
            );
            assert_eq!(
                facet.docs_with_value, facet.matching_docs,
                "every document here has a role, so the subset is the whole set"
            );
        }
    }

    /// One oversized value cannot blow the per-field seed budget.
    ///
    /// The budget is a bound on how much text gets tokenized twice per query (once to pick the
    /// terms, once by the highlighter, which also does a `doc_freq` per distinct token). A
    /// `cat` of a large file arrives as a *single* `tool_output` value, so a check that only
    /// runs between values bounds nothing at all: it has to truncate.
    #[test]
    fn one_huge_value_is_truncated_to_the_seed_budget() {
        let mut seed = blank_doc(0);
        seed.kind = DocKind::ToolCall;
        seed.turn_seq = 0;
        seed.uuid = Some("seedref".into());
        seed.doc_id = "s1:-:5e0f1a2b:0".into();
        // One value, four times the budget, and multibyte so a naive truncate would panic.
        seed.tool_output = Some("π cat of a large file ".repeat(50_000));
        assert!(seed.tool_output.as_ref().unwrap().len() > 4 * SIMILAR_SOURCE_BYTES);

        let (index, f) = index_docs(&[seed]);
        let source =
            resolve_similar(&index, &f, "seedref", &[SimilarField::ToolOutput], false).unwrap();

        let seeded: usize = source
            .values
            .iter()
            .flat_map(|(_, values)| values.iter())
            .map(String::len)
            .sum();
        assert!(
            seeded <= SIMILAR_SOURCE_BYTES,
            "{seeded} bytes of seed against a {SIMILAR_SOURCE_BYTES} byte budget"
        );
    }

    /// The point of the feature: the other turns of the same topic come back, and the other
    /// topic does not.
    #[test]
    fn similarity_finds_the_topic_and_not_the_corpus() {
        let (index, f) = index_docs(&similar_docs());
        let response = search(&index, &f, &similar_req(seeded(&index, &f, "s1:0"))).unwrap();
        assert_eq!(
            refs(&response),
            vec![2, 3, 4, 5],
            "the two remaining turns of the scoring topic, and nothing from the parsing one"
        );
    }

    /// The source turn is excluded by default and restored by `--include-source`.
    ///
    /// It ranks first *on this corpus*, where the seed turn is two short documents that between
    /// them carry every generated clause. That is a fact about this fixture and not a general
    /// property — see `docs/DESIGN.md` on why a turn-shaped seed does not reliably rank first —
    /// so the exclusion is an explicit `MustNot` rather than a bet on the ordering.
    #[test]
    fn the_source_turn_is_excluded_unless_it_is_asked_for() {
        let (index, f) = index_docs(&similar_docs());

        let default = search(&index, &f, &similar_req(seeded(&index, &f, "s1:0"))).unwrap();
        assert!(
            !refs(&default).contains(&0) && !refs(&default).contains(&1),
            "the seed at seq 0 and its turn sibling at seq 1 are both out: {:?}",
            refs(&default)
        );

        let source = resolve_similar(&index, &f, "s1:0", &[SimilarField::Text], true).unwrap();
        let included = search(&index, &f, &similar_req(source)).unwrap();
        assert_eq!(
            refs(&included).first(),
            Some(&0),
            "with --include-source the seed is back, and it ranks first: {:?}",
            refs(&included)
        );
        assert_eq!(
            included.total,
            default.total + 2,
            "the whole turn came back"
        );
    }

    /// `--similar-to X -p /elsewhere --sort newest --facets tool_name` has to be one query.
    ///
    /// The two sorted arms and the facet arm are why the similarity clause is a concrete
    /// `BooleanQuery` of `TermQuery`s: all three run collectors that disable scoring, and a
    /// `MoreLikeThisQuery` answers that with `Err("MoreLikeThisQuery requires to enable
    /// scoring.")`. Against that query type these were not worse results, they were errors.
    #[test]
    fn similarity_composes_with_filters_sorting_and_facets() {
        let (index, f) = index_docs(&similar_docs());
        let source = seeded(&index, &f, "s1:0");

        let filtered = SearchRequest {
            filters: Filters {
                project: Some("/home/user/other-project".into()),
                ..Filters::default()
            },
            ..similar_req(source.clone())
        };
        assert_eq!(
            search(&index, &f, &filtered).unwrap().total,
            0,
            "a project filter still ANDs on top of the similarity clause"
        );
        assert!(
            search(&index, &f, &similar_req(source.clone()))
                .unwrap()
                .total
                > 0
        );

        for sort in [SortBy::Newest, SortBy::Oldest] {
            let request = SearchRequest {
                sort,
                ..similar_req(source.clone())
            };
            let response = search(&index, &f, &request)
                .unwrap_or_else(|e| panic!("--similar-to --sort {sort:?} failed: {e:#}"));
            assert_eq!(response.total, 4, "{sort:?}");
        }
        assert_eq!(
            refs(
                &search(
                    &index,
                    &f,
                    &SearchRequest {
                        sort: SortBy::Oldest,
                        ..similar_req(source.clone())
                    }
                )
                .unwrap()
            ),
            vec![2, 3, 4, 5]
        );

        // Both faceting entry points: the one that rides along with the hits, and `facets()`,
        // which collects with `&(Count, collector)` and no `TopDocs` at all.
        let with_facets = SearchRequest {
            facets: vec!["role".to_string()],
            ..similar_req(source.clone())
        };
        assert!(!search(&index, &f, &with_facets).unwrap().facets.is_empty());
        let facet = facets(&index, &f, "role", &similar_req(source)).unwrap();
        assert_eq!(facet.matching_docs, 4);
        assert_eq!(facet.docs_with_value, 4);
    }

    /// A turn with nothing indexed in the selected fields is our error, naming the flag that
    /// fixes it — never Tantivy's, which blames stored fields this code does not read.
    #[test]
    fn a_seed_with_no_indexed_text_is_refused_with_our_own_message() {
        let mut docs = similar_docs();
        // A turn of its own, carrying a rendered body and nothing the schema indexes as text.
        let mut empty = blank_doc(12);
        empty.turn_seq = 12;
        empty.uuid = Some("uuid-12".into());
        empty.doc_id = "s1:-:5e0f1a2b:12".into();
        empty.body = "ok".into();
        docs.push(empty);
        let (index, f) = index_docs(&docs);

        let err = format!(
            "{:#}",
            resolve_similar(&index, &f, "uuid-12", &[SimilarField::Text], false).unwrap_err()
        );
        assert!(err.contains("has no indexed text"), "{err}");
        assert!(err.contains("--similar-in"), "{err}");
        assert!(
            !err.contains("stored fields"),
            "Tantivy's own message leaked: {err}"
        );
    }

    /// A similarity hit is highlighted from the seed turn's terms.
    ///
    /// `MoreLikeThisQuery` reports no `query_terms`, so without the `similar` argument to
    /// `snippet_generator` every hit here would carry an unmarked head-of-body excerpt — no
    /// error, no warning, just a search tool that stopped saying why anything matched.
    #[test]
    fn a_similarity_hit_still_says_why_it_matched() {
        let (index, f) = index_docs(&similar_docs());
        let response = search(&index, &f, &similar_req(seeded(&index, &f, "s1:0"))).unwrap();
        let hit = response.hits.first().expect("a hit");
        assert!(
            !hit.snippet_marks.is_empty(),
            "no highlight on {}: {:?}",
            hit.doc.doc_id,
            hit.snippet
        );
        assert!(hit.snippet.contains(HIGHLIGHT), "{:?}", hit.snippet);
        assert_eq!(hit.snippet_field, SnippetSource::Text);
    }

    /// Every stop word is spelled the way an analyzer emits it, for **both** analyzers.
    ///
    /// [`is_similarity_term`] tests the token *after* the analyzer has run, so an entry written
    /// in surface form (`assistant`, `reminder`) is a silent no-op: the list looks right and
    /// does nothing. The trap this test exists for is that there are two analyzers and they
    /// disagree. `--similar-in text` stems (`assistant` -> `assist`) and `--similar-in
    /// tool_output` does not (`assistant` -> `assistant`), so a list holding only the stemmed
    /// form filters one of the four selectable fields and silently passes the other three —
    /// which is exactly the "these two documents are the same kind of record" vocabulary the
    /// list exists to remove, on the fields where tool-call text actually lives.
    ///
    /// So the derivation is checked rather than the spelling: every structural word, through
    /// every analyzer, has to land on an entry that is in the list.
    #[test]
    fn the_similarity_stop_words_are_spelled_as_both_analyzers_emit_them() {
        // The whole-identifier token is the first one emitted: `SplitIdentifiers` puts the
        // parts after it at the same position. That token is what the list has to match; the
        // parts are ordinary words and are deliberately left to the frequency bound.
        fn whole(mut analyzer: TextAnalyzer, word: &str) -> String {
            let mut first = None;
            analyzer.token_stream(word).process(&mut |token: &Token| {
                if first.is_none() {
                    first = Some(token.text.clone());
                }
            });
            first.unwrap_or_else(|| panic!("{word:?} produced no tokens at all"))
        }
        let prose = crate::tokenizer::prose_analyzer;
        let code = crate::tokenizer::code_analyzer;

        for word in SIMILAR_STRUCTURAL_WORDS {
            for (name, emitted) in [
                ("prose", whole(prose(), word)),
                ("code", whole(code(), word)),
            ] {
                assert!(
                    SIMILAR_STOP_WORDS.contains(&emitted.as_str()),
                    "the {name} analyzer turns the structural word {word:?} into {emitted:?}, \
                     which is not in SIMILAR_STOP_WORDS — so a similarity seeded from a field \
                     using that analyzer would ride on it"
                );
                assert!(
                    !is_similarity_term(&emitted),
                    "{emitted:?} still passes is_similarity_term"
                );
            }
        }

        // No dead weight the other way: every entry has to be something an analyzer really
        // emits, or it is a line that can never match a token.
        for entry in SIMILAR_STOP_WORDS {
            let reachable = SIMILAR_STRUCTURAL_WORDS
                .iter()
                .any(|word| whole(prose(), word) == entry || whole(code(), word) == entry);
            assert!(
                reachable,
                "{entry:?} is in SIMILAR_STOP_WORDS but no structural word analyzes to it"
            );
        }

        // The trap, stated as an example: the surface spellings do not survive the stemmer, and
        // the two analyzers therefore need two different entries for one word.
        assert_eq!(whole(prose(), "assistant"), "assist");
        assert_eq!(whole(code(), "assistant"), "assistant");
        assert_eq!(whole(prose(), "tool_use"), "toolus");
        assert_eq!(whole(code(), "tool_use"), "tooluse");
    }

    // -----------------------------------------------------------------------
    // grouping by turn
    // -----------------------------------------------------------------------

    /// Three turns of the shape a debugging question really has: a prompt, the answer, and the
    /// calls in between — every document of a turn carrying the word the query will ask for, so
    /// an ungrouped search returns the turn several times over.
    fn turns_about(word: &str, sizes: &[usize]) -> Vec<Doc> {
        let mut docs = Vec::new();
        let mut seq = 0u64;
        for (turn, size) in sizes.iter().enumerate() {
            let turn_seq = seq;
            for member in 0..*size {
                let mut d = blank_doc(seq);
                d.turn_seq = turn_seq;
                // Descending, so the best-scoring document of a turn is never its first: an
                // anchor picked by position rather than by score would pass a test that only
                // ever scored them equally.
                let repeats = size - member;
                d.text = vec![format!("turn{turn} {}", vec![word; repeats].join(" "))];
                d.role = if member == 0 { "user" } else { "assistant" }.into();
                docs.push(d);
                seq += 1;
            }
        }
        docs
    }

    #[test]
    fn grouping_collapses_a_turn_to_its_best_document_and_says_how_many_it_stood_in_for() {
        let docs = turns_about("memmap", &[4, 3, 2]);
        let (index, f) = index_docs(&docs);

        let plain = search(&index, &f, &req("memmap")).unwrap();
        assert_eq!(plain.hits.len(), 9, "every document of every turn is a hit");
        assert!(!plain.grouped);
        assert!(plain.hits.iter().all(|h| h.collapsed == 0));

        let grouped = search(
            &index,
            &f,
            &SearchRequest {
                group_by_turn: true,
                ..req("memmap")
            },
        )
        .unwrap();
        assert!(grouped.grouped);
        assert_eq!(grouped.hits.len(), 3, "one hit per turn");

        // `total` still counts documents. That is the point of reporting `grouped` beside it:
        // the two numbers answer different questions and neither is the other's page count.
        assert_eq!(grouped.total, 9);

        let turns: Vec<u64> = grouped.hits.iter().map(|h| h.doc.turn_seq).collect();
        assert_eq!(turns, vec![0, 4, 7]);
        // The anchor is the best-scoring member, which `turns_about` made the *first* document
        // of each turn — the one that repeats the term most.
        assert_eq!(
            grouped.hits.iter().map(|h| h.doc.seq).collect::<Vec<_>>(),
            vec![0, 4, 7]
        );
        // Four documents matched in the first turn, three in the second, two in the third; the
        // anchor is one of them, so it stands in for the rest.
        assert_eq!(
            grouped.hits.iter().map(|h| h.collapsed).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    /// The count is of the *matched* set, not of the page. A turn whose members are spread
    /// across a fetch window still reports all of them, and a `--limit 1` reports the same
    /// number a `--limit 10` does.
    #[test]
    fn the_collapsed_count_does_not_depend_on_the_page_size() {
        let docs = turns_about("memmap", &[6, 2]);
        let (index, f) = index_docs(&docs);
        let collapsed = |limit: usize| {
            search(
                &index,
                &f,
                &SearchRequest {
                    group_by_turn: true,
                    limit,
                    ..req("memmap")
                },
            )
            .unwrap()
            .hits[0]
                .collapsed
        };
        assert_eq!(collapsed(1), 5);
        assert_eq!(collapsed(10), 5);
    }

    /// Only the documents that *matched* collapse. A turn is not a bucket of everything that
    /// happened in it — `collapsed` is "hits this one is standing in for", so a document of the
    /// same turn that the query never touched is not counted.
    #[test]
    fn only_matching_documents_collapse() {
        let mut docs = turns_about("memmap", &[3]);
        let mut unrelated = blank_doc(9);
        unrelated.turn_seq = 0;
        unrelated.text = vec!["turn0 something else entirely".into()];
        docs.push(unrelated);
        let (index, f) = index_docs(&docs);

        let grouped = search(
            &index,
            &f,
            &SearchRequest {
                group_by_turn: true,
                ..req("memmap")
            },
        )
        .unwrap();
        assert_eq!(grouped.hits.len(), 1);
        assert_eq!(grouped.hits[0].collapsed, 2, "three matched, one anchors");
        assert_eq!(grouped.total, 3);
    }

    /// `turn_seq` is a per-file ordinal, so the path is half the key. Two transcripts sharing a
    /// session id (§9's `resetSessionFile()`) both hold a turn #0, and collapsing on the number
    /// alone would merge two conversations into one hit.
    #[test]
    fn two_files_sharing_a_turn_number_are_two_turns() {
        let mut docs = turns_about("memmap", &[2]);
        let relocated: Vec<Doc> = docs
            .iter()
            .map(|d| Doc {
                source_path: "/tmp/s1-relocated.jsonl".into(),
                doc_id: format!("{}:relocated", d.doc_id),
                ..d.clone()
            })
            .collect();
        docs.extend(relocated);
        let (index, f) = index_docs(&docs);

        let grouped = search(
            &index,
            &f,
            &SearchRequest {
                group_by_turn: true,
                ..req("memmap")
            },
        )
        .unwrap();
        assert_eq!(grouped.hits.len(), 2, "one hit per file, not one overall");
        assert_eq!(
            grouped.hits.iter().map(|h| h.collapsed).collect::<Vec<_>>(),
            vec![1, 1],
            "neither file's count leaks into the other"
        );
    }

    /// The zero-hit outcomes this index can produce that look exactly like an empty corpus.
    /// Logging them reaches whoever is watching stderr; the caller who drew the wrong conclusion
    /// reads the response, so the response is where the sentence has to be.
    #[test]
    fn a_zero_hit_page_carries_its_warning_to_the_caller_not_only_to_stderr() {
        let (index, f) = index_docs(&identifier_docs());

        // `zzz:` is no field of this schema, so the term became a `tool_input.zzz` subpath
        // lookup that cannot fail and cannot match. That is a misread colon, not an empty index.
        let misread = search(&index, &f, &req("zzz:qqq")).unwrap();
        assert_eq!(misread.total, 0);
        assert_eq!(
            misread.warnings,
            vec![WARN_UNQUALIFIED_FIELD_TERM.to_string()],
            "the carried sentence is the logged one, verbatim"
        );

        // A real field that simply matched nothing is an honest zero: no warning to give.
        let honest = search(&index, &f, &req("tool_name:NoSuchTool")).unwrap();
        assert_eq!(honest.total, 0);
        assert!(honest.warnings.is_empty(), "{:?}", honest.warnings);

        // And a page that found something says nothing at all.
        let found = search(&index, &f, &req("create")).unwrap();
        assert!(found.total > 0 && found.warnings.is_empty());
    }

    /// `GROUP_FANOUT * (limit + offset)` documents were scanned and they all belonged to one
    /// turn, so the page is short of `--limit` while documents are still matching. Silence here
    /// reads as the end of the results.
    #[test]
    fn a_grouped_page_that_came_up_short_says_so_on_the_response() {
        // One turn of 20 matching documents: at `limit` 2 the collapse window reaches 16 of
        // them and still finds only one turn to anchor.
        let (index, f) = index_docs(&turns_about("memmap", &[20]));
        let short = search(
            &index,
            &f,
            &SearchRequest {
                group_by_turn: true,
                limit: 2,
                ..req("memmap")
            },
        )
        .unwrap();
        assert_eq!(short.hits.len(), 1);
        assert_eq!(short.total, 20, "`total` still counts documents");
        assert_eq!(short.warnings, vec![WARN_GROUPED_PAGE_SHORT.to_string()]);

        // A window wide enough to see the whole match set has nothing to report.
        let whole = search(
            &index,
            &f,
            &SearchRequest {
                group_by_turn: true,
                limit: 3,
                ..req("memmap")
            },
        )
        .unwrap();
        assert!(whole.warnings.is_empty(), "{:?}", whole.warnings);
    }

    /// Paging a grouped search pages turns. An offset in documents would skip *into* the first
    /// turn and hand back the same turn again under a different anchor.
    #[test]
    fn a_grouped_offset_skips_turns_not_documents() {
        let docs = turns_about("memmap", &[4, 3, 2]);
        let (index, f) = index_docs(&docs);
        let page = |limit: usize, offset: usize| {
            search(
                &index,
                &f,
                &SearchRequest {
                    group_by_turn: true,
                    limit,
                    offset,
                    ..req("memmap")
                },
            )
            .unwrap()
            .hits
            .iter()
            .map(|h| h.doc.turn_seq)
            .collect::<Vec<_>>()
        };
        assert_eq!(page(2, 0), vec![0, 4]);
        assert_eq!(page(2, 1), vec![4, 7]);
        assert_eq!(page(2, 2), vec![7]);
        assert_eq!(page(2, 3), Vec::<u64>::new());
    }

    /// A time-ordered grouped search has no scores to pick an anchor by, so the anchor is the
    /// first document of the turn the ordering reaches — the newest one under `--sort newest`.
    /// The turn is still collapsed, and still reports what it collapsed.
    #[test]
    fn grouping_survives_a_time_ordered_search() {
        let docs = turns_about("memmap", &[3, 2]);
        let (index, f) = index_docs(&docs);
        let grouped = search(
            &index,
            &f,
            &SearchRequest {
                group_by_turn: true,
                sort: SortBy::Newest,
                ..req("memmap")
            },
        )
        .unwrap();
        assert_eq!(
            grouped.hits.iter().map(|h| h.doc.seq).collect::<Vec<_>>(),
            vec![4, 2],
            "the newest document of each turn anchors it, newest turn first"
        );
        assert_eq!(
            grouped.hits.iter().map(|h| h.collapsed).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// Grouping composes with the filters rather than sitting in front of them: `collapsed`
    /// counts the documents that survived the whole query, not the documents in the turn.
    #[test]
    fn the_collapsed_count_respects_the_filters() {
        let mut docs = turns_about("memmap", &[4]);
        for d in docs.iter_mut().skip(2) {
            d.role = "assistant".into();
            d.kind = DocKind::ToolCall;
            d.tool_name = Some("Bash".into());
        }
        let (index, f) = index_docs(&docs);

        let grouped = search(
            &index,
            &f,
            &SearchRequest {
                group_by_turn: true,
                filters: Filters {
                    tool: vec!["Bash".into()],
                    ..Filters::default()
                },
                ..req("memmap")
            },
        )
        .unwrap();
        assert_eq!(grouped.hits.len(), 1);
        assert_eq!(
            grouped.hits[0].collapsed, 1,
            "two Bash calls matched, so the anchor stands in for one"
        );
    }
}
