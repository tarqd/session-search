# session-search — design contract

**This document is the contract between modules.** Agents working in parallel own disjoint
files and must code against the signatures here rather than inventing their own. If a
signature genuinely needs to change, change it here in the same commit and say so.

Read `docs/TRANSCRIPT-FORMAT.md` first — it is the input format spec.

## Goal

Index Claude Code session transcripts into a local Tantivy index; search them with full-text
ranking plus facets over project/directory, tool, **tool parameters**, model, branch, date and
errors. A CLI now; an `mcp` subcommand serving the same operations over stdio next.

## Confirmed product decisions

- **One doc per message, one doc per tool call.** Hits are precise and facetable; the CLI
  expands a hit to surrounding turns on demand.
- **Indexed text:** user prompts, assistant text, tool inputs, tool results (size-capped).
  A tool call's result lives in its own `tool_output` field rather than being concatenated
  onto `text`, so `tool_output:"No such file"` asks about what a tool *returned* and
  `text:...` about what it was asked to do. Both are default query fields, so a bare query
  still spans the pair.
  Assistant **thinking is stored but not indexed** — `--include-thinking` opts in, default off.
- **A message body is split by kind before it is indexed.** Its prose goes to `text`, its
  fenced blocks and inline spans to `code`, its headings to `headings` — because prose and code
  want opposite analysis. The body it was written as is kept whole in `body`, stored and never
  indexed, because the split cannot be undone and `body` is what every renderer prints. See
  "The markdown split" below. A tool call's *result* is in neither half: it is `tool_output`.
- **Bash commands are indexed structurally as well as textually.** Every `Bash` tool-call
  document carries `bash_cmd` — `{"program": [...], "args": [...]}`, the `argv[0]` of *every*
  simple command in the script (pipelines, `&&` chains, subshells, loop and function bodies)
  plus every suffix word — parsed with `brush-parser` (`src/bash.rs`). It is tokenized **raw**,
  unlike `tool_input`: these are exact-match facts, so `--release` stays `--release` rather
  than being split into `release`, and matching is case-sensitive (`Cargo` is not `cargo`).
  That is what lets `--program cargo` mean "ran cargo" instead of "the command text mentions
  cargo somewhere", which also matches `--cargo-flag` or a path segment. A command the shell
  grammar rejects gets no `bash_cmd` at all; there is no heuristic fallback.
- **Incremental one-shot indexing** keyed on (size, mtime, byte offset) per file.
  The read commands (`search`, `facets`, `show`, `sessions`) auto-refresh unless `--no-refresh`;
  `stats` never opens the index at all. `--include-thinking` is an *index-time* choice — turning
  it on later needs `index --full`, because the watermarks say nothing changed.

## Verified Tantivy facts (0.26.2) — do not re-litigate these

These were proven with a working probe before the design was fixed. The API differs from
older tutorials; follow this, not your memory.

**The key decision: `tool_input` is one JSON field that is indexed *and* fast.** That is what
makes both dynamic-subpath filtering and query-time faceting work for parameter keys never
declared in the schema.

```rust
let json_opts = JsonObjectOptions::default()
    .set_stored()
    .set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("code")    // <- the analyzer below, not tantivy's `default`
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
    .set_fast(Some("raw"))        // <- required for aggregations
    .set_expand_dots_enabled();   // <- makes `tool_input.command:x` parse
sb.add_json_field("tool_input", json_opts);
```

Proven to work against that field:

| Operation | Result |
| --- | --- |
| `tool_input.command:cargo` | 2 hits |
| `tool_input.file_path:index.rs` | 2 hits |
| `tool_input.command:"cargo build"` (phrase) | 1 hit |
| terms agg on `tool_input.command` | `"git status"=1, "cargo build --release"=1, "cargo test"=1` |
| terms agg on `tool_input.file_path` | `".../src/index.rs"=2, ".../src/main.rs"=1` |

Aggregation on a subpath **never named in the schema** works. Note aggregations key on the raw
(untokenized) fast value, while filtering goes through the tokenized index — that asymmetry is
intended: facets show whole commands/paths, search matches words inside them.

**API gotchas, all hit for real:**

- `TopDocs::with_limit(n)` alone does **not** implement `Collector` in 0.26. You must chain an
  ordering: `TopDocs::with_limit(n).order_by_score()` (or `order_by_fast_field`, etc.).
- Build documents with `TantivyDocument::parse_json(&schema, &json_string)?`. The `doc!` macro
  will not accept a `serde_json::Map` for a JSON field.
- `to_json` needs `use tantivy::Document;` in scope (trait method).
- Aggregations:
  ```rust
  let aggs: Aggregations = serde_json::from_value(json!({
      "f": { "terms": { "field": field, "size": top } }
  }))?;
  let collector = AggregationCollector::from_aggs(aggs, Default::default());
  let res = searcher.search(&query, &collector)?;   // res serializes to {"f":{"buckets":[{key,doc_count}]}}
  ```
- `tantivy` needs no extra cargo features for aggregations — they are built in.

## The two analyzers (`tokenizer.rs`)

Two registered analyzers, one per kind of text:

| analyzer | fields | pipeline |
| --- | --- | --- |
| `code` | `code`, `tool_output`, `thinking`, `tool_input` | `WordTokenizer -> SplitIdentifiers -> LowerCaser -> RemoveLongFilter(255)` |
| `prose` | `text`, `headings`, `context_text` | `WordTokenizer -> SplitIdentifiers -> LowerCaser -> Stemmer(English) -> RemoveLongFilter(255)` |

`prose` is `code` plus an English stemmer, and the stemmer is the whole difference: `compiling`
finds `compiled`. Stemming is exactly what must never touch a snippet — it turns `Serializes`
into `serial` and `parses` into `pars`, terms nobody types — which is why the two halves of a
message are indexed in separate fields rather than under one compromise analyzer.

What `prose` does **not** drop is identifier splitting. Only fenced blocks and inline spans are
routed to `code`; a sentence naming `SnippetGenerator` without backticks stays in `text`, as do
attachments, `system` records and a tool call's name and input — none of which is markdown to
split at all. Tokenizing those the stock way would index `SnippetGenerator` as one opaque word
and leave `snippet` unable to find it, which is the hole the `code` analyzer exists to close.
`context_text` takes `prose` for the reason its
contents are: a title, an opening prompt and a branch name are English, and the shared `code`
base still splits the identifiers inside them. A plain English word still emits once, stemmed. `thinking` keeps `code`: it is
prose and snippets interleaved with no marker between them, so there is no split to make, and
the analyzer that keeps identifiers intact loses least. `tool_output` keeps `code` for the same
reason — a result is diagnostics, paths and program output, never a markdown message. And
`tool_input` keeps `code` for the reason it always had it: its values are commands and paths.

The rest of this section is the `code` analyzer.

Transcripts are mostly identifiers, paths and shell, and the stock `default` analyzer answers
badly for them: `snippet` misses `SnippetGenerator`, which it indexes as one opaque word;
`OpenOrCreate` and `open_or_create` share no term, so neither spelling finds the other; and every
sha256 is discarded by the 40-byte `RemoveLongFilter`.

```
WordTokenizer  ->  SplitIdentifiers  ->  LowerCaser  ->  RemoveLongFilter::limit(255)
```

`SplitIdentifiers` emits, for each token, **the whole identifier and each of its parts at the
same position** — a synonym expansion, so the parts consume no position and phrases still work:

```
open_or_create   -> openorcreate, open, or, create        (all at position p)
SnippetGenerator -> snippetgenerator, snippet, generator  (all at position p)
parseTs2Ms       -> parsets2ms, parse, ts, 2, ms          (all at position p)
cargo            -> cargo                                 (a plain word emits once)
```

Parts break on `_`, on a case boundary (`parseTs`, and `HTTPServer` -> `HTTP`, `Server`), and on
a letter/digit boundary. An acronym only ends where a real word begins, so a lone trailing `s`
stays with it (`getIDs` -> `get`, `IDs`) and a search for `ids` finds it. Four consequences are
load-bearing:

- **The whole form is the separator-free lowercasing, not the verbatim one.** `QueryParser` turns
  a query word that yields several tokens into a `PhraseQuery` over `(position, term)` pairs, so
  terms sharing a position must *all* be present at that position in the document. A query
  therefore matches an identifier only when both sides produce the same set of terms. Collapsing
  `open_or_create` and `OpenOrCreate` to the same `openorcreate` + parts is exactly what makes
  each spelling find the other; keeping the verbatim form would break that in one direction.
- **The base tokenizer is hand-written, not `SimpleTokenizer` and not a regex.**
  `SimpleTokenizer` splits on `_` before any filter can see it, which would leave an
  underscore-aware filter dead and make `OpenOrCreate` unable to find `open_or_create`. A `\w+`
  `RegexTokenizer` draws the right boundaries but runs the regex engine once per token and
  clones the compiled regex per field value — measured at roughly fourteen times the scan cost
  of `SimpleTokenizer` over real transcript text, and the dominant part of the analyzer's bill.
  `WordTokenizer` scans `char_indices` for runs of letters, digits and `_` instead, at
  `SimpleTokenizer` speed, so `src/index.rs` still yields `src`, `index`, `rs`.
- **The 255-byte length limit is deliberate, and hashes are never split.** The stock 40 silently
  drops every hash and long generated identifier; at 255 a pasted sha256 finds the document it
  came from. But a hash is not a name: a run of eight or more hex digits mixing letters and
  digits, or any token that shatters into a crowd of one- and two-character fragments, is
  emitted whole and alone. Splitting a sha256 into 41 fragments would put single characters in
  the dictionary (`f` matching every hash in the corpus) and inflate the BM25 field length of
  every document holding one, since Tantivy counts tokens rather than positions.

A field's tokenizer *name* is part of the schema, so **changing this analyzer forces a full
reindex**: `index::open_or_create` sees `SchemaError`, discards the index together with
`state.json` and `sessions.json`, and the next run refills it. Changing the analyzer's *behaviour*
without changing its name does **not** trip that check — bump the registered name too, or the
old terms stay on disk.

Snippets: one `SnippetGenerator` per response for `text`, one for `code`, one for `tool_output`
and, under `--include-thinking`, one for `thinking`. The first of those with something to
highlight wins — prose, then code, then the tool result, then thinking — and a hit with nothing
highlighted anywhere falls back to the head of its body. The extra generators are not optional:
a tool call keeps its result in `tool_output` and its file-content payloads in `code`, so most
tool-call hits have nothing in `text` to mark.

The no-snippet fallback has one rule of its own: **a failed call leads with its error.**
`--errors-only` carries no free-text query and so lands there every time, and it otherwise
showed the command that failed rather than the reason it broke — on a real corpus the error sat
1,000–2,400 characters into the body, past a heredoc. This is a rendering rule
(`format::doc_body` and `search::fallback_body`), not a storage one: with the result in a field
of its own there is no ordering inside a body to get wrong, and nothing is reordered underneath
a query.

Each generator is built from the **whole** terms of the query only (`search::snippet_generator`).
`SnippetGenerator::create` weighs every term of the parsed query alike and scores a fragment by
summing its hits, so for a query word that expands into an identifier plus its parts, a paragraph
repeating the parts (`user` here, `email` there) outscores the one line holding `userEmail` and
the snippet shows everything except the reason the document matched. Dropping the parts is safe:
they share the whole's position, so every matching document contains the whole form too.

`tokenizer::register(&index)` registers **both** analyzers and must run on every path that opens
or creates an `Index`, before any document is added and before any query is parsed: the writer
and `QueryParser` both look an analyzer up by name, and a missing registration fails there rather
than at open time.

## The markdown split (`markdown.rs`)

`markdown::split(&str) -> MarkdownParts { text, code, headings, code_langs }` walks
pulldown-cmark's event stream once (tables + strikethrough on, everything else off, no regex)
and routes each event:

| markdown | lands in |
| --- | --- |
| paragraph, list item, table cell, blockquote, link text, image alt, raw HTML | `text` |
| fenced or indented code block | one `code` entry, its info word in `code_langs` |
| inline `` `span` `` | one `code` entry **and** stays in `text` |
| heading | one `headings` entry **and** an entry of `text` |

`text` is **one entry per prose block**, never one joined string. Lifting a fence out of the
middle of a message would otherwise close the gap it left, making the sentence before it
adjacent to the sentence after it, and a phrase query would match across text that was never
adjacent. Tantivy separates the values of a multi-valued field by a position gap, which is
exactly the gap a removed block should leave behind. Three routings are deliberate:

- **A heading is also prose.** Routing headings only to their own field would take the section
  title out of the sentence flow a `text` query searches. Headings are short, and the boost on
  `headings` is what makes the ranking difference, not exclusivity.
- **Raw HTML is text, not dropped.** A turn that opens with `<system-reminder>` is one HTML block
  to a markdown parser; dropping HTML would silently unindex the whole of it.
- **Link destinations are dropped.** A URL is not prose, and `tool_input` already carries the
  paths anyone searches for.

Malformed input needs no special case: pulldown-cmark is total over `&str`, closes every tag it
opened at EOF, and never fails. An unclosed fence is a closed one — which also makes it safe to
cap a body *before* splitting it.

`parse.rs` applies it to `user` and `assistant` **message** docs only:

- a **tool call is never parsed as markdown** — a `Bash` script is full of `#`, `*` and `>` that
  mean nothing of the sort. Its name and input strings stay in `text` as they always were; the
  `old_string` / `new_string` / `content` leaves of an `Edit`/`Write`/`MultiEdit` go to `code`,
  because they are file contents (the key decides, at any depth); and its **result** goes to
  `tool_output`, which is neither half of the split;
- attachments, `system` records and a non-text `user` payload stay whole in `text` — one value,
  unsplit. They are not markdown, and the `prose` analyzer splits identifiers, so a rendered
  file, a diagnostic or a hook's output is still searchable by the parts of the names in it;
- `max_text_bytes` bounds one document's body: a message is truncated *before* the split, and a
  tool call's input copy spends one budget across `text` then `code` — a quarter of the cap and
  never more than `INPUT_LEAVES_CAP`, so the copy does not scale with the cap and a `Write`
  payload `tool_input` already holds whole is not duplicated at length.
  `tool_output` is capped separately at the same number, because it is a field of its own rather
  than a share of one body. `ParseOutput::replacements` only fills in `tool_output`, so a
  completed tail parse stays byte-identical to a whole-file one without rebuilding anything;
- every doc also carries `body`: the message's markdown as it was written (after the cap), or a
  tool call's name, input strings and file-content payloads in the order they were built. It is
  what `show`, `--context` and `--json` print, beside `tool_output`. The indexed halves cannot
  be reassembled into it — the split drops link destinations, repeats every inline span in both
  halves, and knows nothing of where a fence sat relative to the paragraphs around it.

## Layout

```
src/
  lib.rs         re-exports; `pub mod` declarations only
  model.rs       raw serde types for transcript records          [Foundation]
  media.rs       describe binary payloads, never index them       [Foundation]
  discovery.rs   locate roots, enumerate transcripts + sidechains [Foundation]
  parse.rs       records -> Vec<Doc>                              [Foundation]
  markdown.rs    markdown body -> prose / code / headings          [Foundation]
  tokenizer.rs   the `code` + `prose` analyzers, and registration  [Foundation]
  schema.rs      Tantivy schema + Fields handle                   [Foundation]
  index.rs       incremental indexer, watermarks, sessions.json   [Build A]
  search.rs      SearchRequest -> SearchResponse, facets          [Build B]
  context.rs     expand a hit / reconstruct a session             [Build B]
  format.rs      human + JSON rendering                           [Build C]
  cli.rs         clap definitions                                 [Build C]
  main.rs        wiring only                                      [Build C]
tests/
  fixtures/*.jsonl
docs/
  TRANSCRIPT-FORMAT.md   DESIGN.md
```

## Index directory

Default `$XDG_DATA_HOME/session-search` else `~/.local/share/session-search`,
overridable with `--index` / `$SESSION_SEARCH_INDEX`.

```
<index>/tantivy/        the Tantivy index
<index>/state.json      { "version": 5, "roots": [...], "thinking_indexed": bool,
                          "files": { "<abs path>": {size, mtime_ms, byte_offset, docs, carry} } }
<index>/sessions.json   { "<abs transcript path>": SessionInfo }
```

`sessions.json` exists because a session's title arrives in a `summary` sidecar record that may
be appended long after the messages it titles. Keeping session metadata out of Tantivy means a
late title update is a cheap JSON rewrite instead of a doc rebuild.

It is keyed by **transcript path, one entry per file** — not by session id. A session can start
a new file mid-life (`resetSessionFile()`) or move project directories (`relocated`), so two
files may share a `sessionId` (TRANSCRIPT-FORMAT §9); they are two transcripts with their own
counts, project and opening prompt, and merging them loses all three. The display key
`"<session_id>[:<agent_id>]"` is derived at render time.

`carry` is `parse::ParseCarry` — see `parse.rs` below.

## Core types (pinned)

`model.rs` — tolerant; see the parser rules in the format doc. Every struct gets
`#[serde(default)]` fields plus `#[serde(flatten)] extra: serde_json::Map<String, Value>`, and
every enum a `#[serde(other)] Unknown` variant. Nothing here may fail to deserialize on
unknown input.

`parse.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DocKind { Message, ToolCall }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Doc {
    pub doc_id: String,          // "<session_id>:<agent_id|->:<file_tag>:<seq>" — unique,
                                 // delete key. `file_tag` is 8 hex digits of a digest of
                                 // `source_path`: `seq` restarts at 0 in every file and two
                                 // transcripts can share a `session_id`, so without it the
                                 // id is not unique.
    pub kind: DocKind,
    pub source_path: String,     // absolute; delete-by-term key for re-index
    pub seq: u64,                // monotonic doc ordinal within the file, in record order.
                                 // NOT the line number: one record can yield several docs
                                 // (or none), and `index.rs` continues numbering with
                                 // `seq_base` = the previously recorded `docs` count, so the
                                 // two only agree if they are both per-doc.
    /// The conversational turn this doc belongs to: the `seq` of the first doc emitted from
    /// the record that opened it. See "Turns" below. `#[serde(default)]`: travels in the carry.
    #[serde(default)]
    pub turn_seq: u64,
    /// The text of the human prompt that opened this doc's turn, capped at `TURN_PROMPT_BYTES`
    /// (240) on a word boundary. Only the parser can know it and only the carry can move it
    /// across an incremental boundary; `schema::context_header` prepends it to `context_text`.
    /// See "Contextual BM25" below. `#[serde(default)]`: travels in the carry.
    #[serde(default)]
    pub turn_prompt: Option<String>,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub project: Option<String>, // from record `cwd`, NEVER the directory name
    pub git_branch: Option<String>,
    pub role: String,            // "user" | "assistant" | "system" | "attachment"
    pub model: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_output: Option<String>, // the joined tool_result, indexed in its own right
    /// `bash::extract(tool_input.command).to_json()` for a `Bash` call whose command parses;
    /// `None` for every other tool and for a command the shell grammar rejects.
    /// `#[serde(default)]`: a `Doc` also travels inside `ParseCarry` in `state.json`.
    #[serde(default)]
    pub bash_cmd: Option<serde_json::Value>,
    pub is_error: bool,
    pub is_sidechain: bool,
    pub is_meta: bool,           // compaction summaries, meta turns — excluded from "human prompt"
    pub entrypoint: Option<String>,
    pub permission_mode: Option<String>,
    pub version: Option<String>,
    pub slug: Option<String>,
    pub body: String,            // the body as written: what every renderer prints. Stored only
    pub text: Vec<String>,       // the prose half, ONE ENTRY PER BLOCK: a message's markdown
                                 // minus its code blocks, or a tool call's name + input strings
    pub code: Vec<String>,       // the code half: one entry per code block or inline span of a
                                 // message, or a tool call's file-content inputs. NOT its
                                 // result — that is `tool_output`, above
    pub headings: Vec<String>,   // a message's markdown headings (also present in `text`)
    pub code_langs: Vec<String>, // fence info words, deduped
    pub thinking: Option<String>,// stored, indexed only with include_thinking
    pub raw: String,             // the JSONL line, minus any base64 payload (`media::scrub`)
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub description: Option<String>, // subagent .meta.json description
    pub title: Option<String>,       // from a `summary` record, keyed by leafUuid
    pub slug: Option<String>,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    pub source_path: String,
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
    pub messages: u64,           // conversational turns: human prompts, assistant API
                                 // messages (once per `message.id`), `system` records.
                                 // Compaction records and attachments are excluded (§9).
    pub tool_calls: u64,
    pub first_prompt: Option<String>,
}

pub struct ParseOutput {
    pub docs: Vec<Doc>,
    pub replacements: Vec<Doc>,  // documents that REPLACE ones already in the index: same
                                 // `doc_id`, same `seq`, now carrying a tool result that
                                 // arrived after they were written. The consumer deletes each
                                 // `doc_id` before adding it, and must NOT count them towards
                                 // the file's `docs` watermark.
    pub session: SessionInfo, pub errors: Vec<ParseError>,
    pub carry: ParseCarry,       // what the NEXT tail parse of this file must be told
}

/// A malformed line. Counted, never fatal.
pub struct ParseError { pub path: String, pub line: u64, pub byte_offset: u64, pub message: String }

/// State that has to survive from one incremental parse of a file to the next, because a tail
/// parse sees only the bytes appended since the last run and two things straddle that boundary:
///
/// * a `tool_use` and the `tool_result` answering it are consecutive records, so on a live
///   transcript the boundary lands between them almost every time. Without
///   `pending_tool_uses` the tail reads the result as an *orphan* and emits a second,
///   half-empty document for a tool call that is already indexed — doubling every tool call.
///   Each entry carries the document itself, not just the id, so the later tail can COMPLETE
///   it (as a `ParseOutput::replacements` entry) instead of losing the result text.
/// * blocks of one API message share a `message.id` and are NOT contiguous (§7), so a boundary
///   inside one message would count it twice without `counted_message_ids`.
///
/// `tail_line` is not about parsing: it fingerprints the last complete line consumed so the
/// indexer can tell "the file grew" from "the file was rewritten in place".
/// Both id lists are bounded (last 512).
pub struct ParseCarry {
    pub pending_tool_uses: Vec<PendingToolCall>,   // { tool_use_id, doc: Option<Box<Doc>> };
                                         // `doc` is None when it was too large to carry, and
                                         // then a late result only suppresses the duplicate.
    pub counted_message_ids: Vec<String>,
    pub tail_line: Option<TailLine>,     // { start: u64, hash: u64 }
    pub open_turn_seq: Option<u64>,      // the turn open at the last consumed byte, so a tail
                                         // continues numbering instead of restarting it at
                                         // its own `seq_base`. `None` (an older state.json)
                                         // falls back to `seq_base`.
    pub open_turn_prompt: Option<String>,// that same turn's opening prompt — `Doc::turn_prompt`
                                         // for the docs the next tail emits. Carried for the
                                         // same reason: the record it came from is behind the
                                         // byte offset, and a tail that guessed would give the
                                         // same document a different `context_text`.
}

/// Per-file inputs that are not the bytes of the file itself.
pub struct FileContext {
    pub agent_type: Option<String>,  // agent-<id>.meta.json `agentType` — the canonical source
                                     // per §1, and the only one for subagents whose records
                                     // carry no `attributionAgent`. Seeds `Doc::agent_type`.
    pub carry: ParseCarry,
}

/// Parse a transcript. `from_offset` supports incremental tailing; `seq_base` continues
/// numbering; `ctx` carries the per-file facts a tail parse cannot see for itself. Returns the
/// byte offset of the end of the last COMPLETE line — a partial trailing line must be left
/// unconsumed so the next run re-reads it.
pub fn parse_file(path: &Path, from_offset: u64, seq_base: u64, opts: &ParseOptions,
                  ctx: &FileContext) -> anyhow::Result<(ParseOutput, u64)>;

/// One-shot parse of a whole file with no carried state — the ground truth an incremental run
/// must converge on, and what every caller that is not the indexer wants.
pub fn parse_whole(path: &Path, opts: &ParseOptions) -> anyhow::Result<ParseOutput>;

/// 8 hex digits of a digest of an absolute `source_path`; the `file_tag` in every `doc_id`.
pub fn file_tag(source_path: &str) -> String;

pub struct ParseOptions { pub max_text_bytes: usize, pub load_spilled_results: bool }
```

`discovery.rs`:

```rust
pub struct TranscriptFile {
    pub path: PathBuf,
    pub session_id: String,        // from the filename / parent dir
    pub agent_id: Option<String>,  // Some(..) for subagents/agent-<id>.jsonl
    pub meta: Option<AgentMeta>,   // agent-<id>.meta.json
    pub size: u64,
    pub mtime_ms: i64,
}
pub fn default_root() -> anyhow::Result<PathBuf>;              // $CLAUDE_CONFIG_DIR else ~/.claude, + /projects
pub fn discover(roots: &[PathBuf]) -> anyhow::Result<Vec<TranscriptFile>>;
```

`schema.rs`:

```rust
pub struct Fields { /* one pub Field per name below */ }
pub fn build_schema() -> (tantivy::schema::Schema, Fields);
/// `session` is the merged `sessions.json` row for this doc's transcript; it is what the
/// `context_text` header's session half is built from. `None` composes from the doc alone.
pub fn doc_to_json(doc: &Doc, session: Option<&SessionInfo>, include_thinking: bool)
    -> serde_json::Value;                       // feed to parse_json
/// The `context_text` header for one document. See "Contextual BM25" below.
pub fn context_header(doc: &Doc, session: Option<&SessionInfo>) -> Option<String>;
```

Field names and options:

| field | type / options |
| --- | --- |
| `doc_id`, `source_path`, `uuid`, `parent_uuid`, `tool_use_id` | `STRING \| STORED` |
| `session_id`, `agent_id`, `agent_type`, `project`, `git_branch`, `role`, `kind`, `model`, `tool_name`, `entrypoint`, `permission_mode`, `version`, `slug` | `STRING \| STORED \| FAST` |
| `project_facet` | `FacetOptions` — hierarchical, `/home/user/session-search` |
| `tool_input` | JSON, indexed with the `code` tokenizer + `set_fast(Some("raw"))` + `set_expand_dots_enabled()` + stored |
| `bash_cmd` | JSON, stored + indexed with tokenizer `"raw"` / `IndexRecordOption::Basic` + `set_fast(Some("raw"))` + `set_expand_dots_enabled()`. `{"program": [..], "args": [..]}` from `bash::extract`; emitted only when `Some`. The `raw` tokenizer is the point: exact-match facts, so `--release` stays `--release` and matching is case-sensitive |
| `body` | `STORED` only — the body as it was written, which is what gets rendered |
| `text` | stored, indexed with the `prose` tokenizer, `WithFreqsAndPositions` (`TEXT \| STORED` but for the tokenizer); **multi-valued** — one value per prose block, so a position gap stands where a code block was lifted out |
| `headings` | same as `text`; **multi-valued** — one value per markdown heading |
| `code` | same options, but the `code` tokenizer; **multi-valued** — one value per code block or inline span of a message, and the `Edit`/`Write` file contents of a tool call |
| `code_lang` | `STRING \| STORED \| FAST`; **multi-valued** — the info word of each fence, lowercased |
| `tool_output` | same options as `code`, single-valued — what the tool returned, analyzed as code |
| `thinking` | stored, indexed with the `code` tokenizer (only populated when `include_thinking`) |
| `thinking_tokens` | `U64 \| FAST \| STORED \| INDEXED` — per-message reasoning cost |
| `timestamp` | date field, `INDEXED \| STORED \| FAST` (tantivy 0.26 has no `DATE` flag const; `add_date_field` takes the numeric flags) |
| `seq` | `U64 \| STORED \| FAST \| INDEXED` |
| `turn_seq` | `U64 \| STORED \| FAST \| INDEXED` — mirrors `seq`. INDEXED so `context::turn` can term-query it, FAST so grouping by turn is a columnar read, STORED so every hit reports it |
| `context_text` | indexed with the `prose` tokenizer, `WithFreqsAndPositions`, **not stored**. A compact context header — session title / opening prompt, project, branch, the turn's opening prompt — prepended at index time so a fragment is findable by what it was *for* (contextual BM25). A default query field; **never a snippet source**; absent from `doc_from_stored` and from `--json`. See "Contextual BM25" below |
| `is_error`, `is_sidechain`, `is_meta` | `U64 \| FAST \| INDEXED \| STORED` (0/1) — STORED because `search::doc_from_stored` reads them back out of the stored payload |
| `raw` | `STORED` only |

### Images are described, never indexed

A pasted screenshot, a `Read` of a PNG and a `Bash` command with `isImage` all put base64 in
the same fields prose lives in — ~300 KB per phone photo. None of it is text: it matches no
query, it dilutes term statistics, and it costs its own size again in the index. `media.rs`
replaces every payload with a placeholder describing it — `[image/jpeg 230 KiB]` — which
tokenizes into terms someone would actually search, while the path stays where it always was,
on the tool call. Two layers, because the transcript format is open:

1. `describe_block` / `describe_payload` recognise today's shapes and render the placeholder
   from the metadata beside the bytes.
2. `scrub` / `redacted` recognise a base64 blob *by looking at it* — an unbroken run of ≥512
   `[A-Za-z0-9+/=]` carrying both cases and a digit — wherever it turns up. `parse::build_doc`
   runs it over `body`, every entry of `text`, `code` and `headings`, and over `tool_output`
   and `thinking`, so the guarantee holds at one point rather than shape by shape — per entry,
   because the split routes a blob to whichever half it was written in. The earlier passes
   exist so the byte budget is spent on text.

`raw` is scrubbed too. It is stored, never indexed and never returned, so a payload there is
pure weight — it inflates the index by the size of the transcript's images and can push a
pending tool call past `PENDING_DOC_CAP`. The elision is lexical and a base64 run cannot span
a quote or a brace, so the line stays valid JSON and every other byte of it survives.

`index.rs`:

```rust
pub struct IndexOptions {
    pub full: bool, pub jobs: Option<usize>,
    pub include_thinking: bool,   // default TRUE; `index --no-thinking` opts out
    pub load_spilled_results: bool,  // default TRUE — follow `Full output saved to: <path>`
                                     // into `tool-results/<id>.txt`, else an oversized tool
                                     // result is only its "output too large" stub. CLI:
                                     // `index --no-spilled-results` opts out.
    pub max_text_bytes: usize,   // per body field, 1 MiB default; caps the spill too.
                                 // CLI: `index --max-text-bytes N`
    pub heap_bytes: usize,       // default 200 MB
}
pub struct IndexStats {
    pub files_scanned: usize, pub files_updated: usize, pub files_reset: usize,
    pub docs_added: u64, pub docs_deleted: u64, pub sessions: usize,
    pub parse_errors: u64, pub elapsed_ms: u128,
}
pub fn open_or_create(index_dir: &Path) -> anyhow::Result<(tantivy::Index, Fields)>;
pub fn run(index_dir: &Path, roots: &[PathBuf], opts: &IndexOptions) -> anyhow::Result<IndexStats>;
pub fn load_sessions(index_dir: &Path) -> anyhow::Result<BTreeMap<String, SessionInfo>>;
```

An index is **bound to its corpus**: `state.json` records the roots it was built from, and
`run()` refuses a different set unless `--full` re-points it. Auto-refresh follows the recorded
roots, never the default one — resolving the default here silently merged a second corpus into an
index built over a snapshot, and every count then described the union.

Thinking is indexed by **default** (`index --no-thinking` opts out) and the choice is recorded, so
a query-time `--include-thinking` against an index built without it warns instead of silently
matching nothing.

`usage.output_tokens_details.thinking_tokens` is indexed as a fast field, because it survives on
transcripts whose thinking *text* was stripped before it reached disk. It is a per-message total
that appears on only some of the message's block records — usually the `tool_use` one, not the
first to emit — repeated with the same value on up to four of them, so it is charged once per
`message.id` on a record that actually carries it (`ParseCarry::charged_message_ids` keeps that
true across an incremental boundary). Filter with `--min-thinking N`.

A tool-call document is three fields: `text` is `name` plus the input's own strings, `code` is
its `Edit`/`Write` file-content payloads, and `tool_output` is the result. A document finished
across an incremental boundary must be byte-identical to one a whole-file parse produced —
which is now straightforward, since `text` and `code` are both the call side and the arriving
result only fills in `tool_output`.

**A failed call previews its result first.** `--errors-only` otherwise retrieves exactly the
right documents and shows the command that failed rather than the reason it broke — on a real
corpus the error sat 1,000–2,400 characters into the body, past a heredoc. This is a rendering
rule (`format::doc_body`, and the no-snippet fallback in `search`), not a storage one: with the
result in a field of its own there is no ordering inside a body to get wrong, and nothing is
reordered underneath a query.

Incremental rules:
- Watermark per file: `{size, mtime_ms, byte_offset, docs, carry}`.
- Unchanged `size` **and** `mtime_ms` → skip entirely.
- Grew → seek to `byte_offset`, parse the tail only, `seq_base` = recorded `docs`, and hand the
  parser the recorded `carry` so the tail converges on what a whole-file parse would produce.
- Shrank, or mtime went backwards, or `--full` → `delete_term(source_path)` and reparse whole.
- **Before trusting a tail, re-check `carry.tail_line`**: seek to its `start`, re-read up to
  `byte_offset`, and compare the hash. A rewind, `resetSessionFile()` or a restored fork (§9)
  rewrites bytes the watermark already covers and can leave the file the same size or larger,
  which `size`/`mtime` cannot distinguish from an append; a mismatch means everything recorded
  describes bytes that no longer exist, so the file becomes a `Reset`. An absent fingerprint
  (a `state.json` written by an older build) is treated as intact.
- `ParseOutput::replacements` are `delete_term(doc_id)`-ed and re-added, and excluded from the
  `docs` count. This is what makes live indexing converge on the one-shot result *byte for
  byte*, not merely in document count.
- A file that produced no documents and no session metadata gets no `sessions.json` row.
- One `commit()` per run. Parse with `rayon` across files; a **single** writer consumes.
- Never `memmap2` — these files are appended live and truncation raises an uncatchable SIGBUS.

`search.rs`:

```rust
// Derive clap::Args + Deserialize + (later) JsonSchema on this — one struct, both front ends.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub project: Option<String>, pub tool: Vec<String>, pub tool_input: Vec<String>, // "key=value"
    pub tool_output: Vec<String>,   // phrases the result must contain, ANDed
    pub program: Vec<String>,    // any simple command's argv[0] in a Bash script; repeatable, OR
    pub lang: Vec<String>,       // fence languages, matched against `code_lang` (--lang)
    pub branch: Option<String>, pub model: Option<String>, pub role: Option<String>,
    pub kind: Option<String>, pub session: Option<String>, pub agent_type: Option<String>,
    pub since: Option<String>, pub until: Option<String>,   // RFC3339 or YYYY-MM-DD or "7d"
    pub errors_only: bool, pub no_sidechains: bool, pub sidechains_only: bool,
}
pub struct SearchRequest {
    pub query: Option<String>, pub filters: Filters,
    pub limit: usize, pub offset: usize,
    pub facets: Vec<String>,     // "tool_name", "code_lang", "project", or any JSON path
                                 // e.g. "tool_input.file_path", "bash_cmd.program"
    pub facet_top: usize, pub snippet_chars: usize, pub include_thinking: bool,
}
pub struct FacetCount { pub value: String, pub count: u64 }
/// Buckets plus the counts needed to read them honestly. Summing `values` answers "how many
/// docs are in the rows shown", which reads as "how many matched" and is wrong by 50x on a
/// high-cardinality field like `tool_input.command`.
pub struct FacetResult {
    pub field: String, pub values: Vec<FacetCount>,
    pub matching_docs: u64,      // docs matching query+filters; NOT the sum of `values`
    pub docs_with_value: u64,    // of those, the DOCUMENTS carrying a value for this field —
                                 // a second Count over `query AND ExistsQuery(field)`, because
                                 // summing the buckets counts values, and `code_lang` and
                                 // `bash_cmd.program` are multi-valued: one answer with a
                                 // rust fence and a bash fence
                                 // is one document in two buckets
    pub other_docs: u64,         // sum_other_doc_count: values outside the returned buckets
    pub distinct: Option<u64>,   // approximate distinct values (cardinality agg)
}
impl FacetResult {
    /// Values barely repeat, so the bucket list is a sample of a long tail. Drives a hint
    /// steering the caller to full-text search; `tool_input` is indexed for exactly that.
    pub fn is_search_shaped(&self) -> bool;
    pub fn hidden_values(&self) -> Option<u64>;
}
pub struct Hit { pub doc: Doc, pub score: f32, pub snippet: String }
pub struct SearchResponse {
    pub hits: Vec<Hit>, pub total: usize,
    pub facets: BTreeMap<String, FacetResult>, pub elapsed_ms: u128,
}
/// The inverse of `schema::doc_to_json` — the one way to read a `Doc` back out of the index.
pub fn doc_from_stored(f: &Fields, stored: &tantivy::TantivyDocument) -> Doc;
pub fn search(index: &tantivy::Index, f: &Fields, req: &SearchRequest) -> anyhow::Result<SearchResponse>;
pub fn facets(index: &tantivy::Index, f: &Fields, field: &str, req: &SearchRequest)
    -> anyhow::Result<FacetResult>;
```

Query semantics: the free-text query goes through `QueryParser` over `text`, `code`, `headings`,
`tool_output` and `context_text` (+ `thinking` when opted in, + `tool_input`), so phrases,
booleans and `field:value` all work. `tool_output` is a *default* field, not an opt-in one: the result text
used to live in `text`, and leaving it out would make a bare query stop matching what it always
matched; `code` and `headings` are default for the mirror-image reason, since the split moved a
message's snippets and titles out of `text`. `QueryParser` applies each field's own analyzer to
the query, so one word is stemmed against the prose fields and split into identifier parts
against the code ones — and a word that expands into several terms becomes a positional query,
which is why every full-text field is indexed `WithFreqsAndPositions`. `headings` carries
`set_field_boost(2.0)`: a section title says what the section is about, so a term in one is a
better answer than the same term in the middle of a paragraph. `context_text` carries
`set_field_boost(0.3)`, for the mirror-image reason — see "Contextual BM25". **There is no fuzzy operator**:
`~` is phrase slop in Tantivy 0.26 and `set_field_fuzzy` is deliberately not wired
up, so do not advertise `term~1`. A query that fails to parse falls back to
`parse_query_lenient`, and the discarded errors are logged at WARN — a typo'd field name must
not look like an empty corpus. Filters are ANDed on top as term/range queries. `--tool-input
k=v` becomes a term query on `tool_input.k` for `v`; `--tool-output TEXT` becomes a phrase
query on `tool_output`, repeatable and ANDed; an empty key *or value* is an error, not a
silent zero. `--program NAME` is the `--tool-input` construction over `bash_cmd.program`, ORed
across repeats and ANDed with everything else; because `bash_cmd` is tokenized `raw` the value
matches whole and case-sensitively, and empty values are skipped rather than rejected.
`project` matches by prefix **on a path boundary**, so `-p ~/code` catches
subdirectories but `-p ~/code` does not catch `~/code-scratch`; `--session` is a bare character
prefix, so the leading block of a uuid is enough (`show` resolves an unambiguous id prefix the
same way, and errors when it is ambiguous). `--limit 0` returns totals and facets with no hits,
and the human rendering still prints the total. Every collector limit (`--limit`, `--offset`,
`--context`, `show --before/--after`) is clamped against the number of documents in the index:
`TopDocs` preallocates whatever it is handed, so an unclamped number aborts the process.

`context.rs`:

```rust
// `source_path` scopes the lookup to one transcript file. `seq` is a per-FILE ordinal and two
// files can share a `session_id` (§9), so a window scoped only by session id interleaves them
// and silently drops the neighbours it was asked for. `None` means "whichever file(s)".
pub fn around(index: &tantivy::Index, f: &Fields, session_id: &str, agent_id: Option<&str>,
              source_path: Option<&str>, seq: u64, before: usize, after: usize)
    -> anyhow::Result<Vec<Doc>>;
pub fn session(index: &tantivy::Index, f: &Fields, session_id: &str, agent_id: Option<&str>,
               source_path: Option<&str>, limit: usize) -> anyhow::Result<Vec<Doc>>;
/// Every document of one turn, in `seq` order, capped at `limit` docs. `source_path` is
/// required, not optional: `turn_seq` is a per-FILE ordinal like `seq`, and two files can share
/// a `session_id` (§9), so scoping by path is what keeps two transcripts from interleaving. A
/// `TermQuery` on `source_path` ANDed with a term query on the `turn_seq` fast field.
pub fn turn(index: &tantivy::Index, f: &Fields, source_path: &str, turn_seq: u64, limit: usize)
    -> anyhow::Result<Vec<Doc>>;
```

### Turns

A **turn** is the span from one human prompt to the next: the prompt, the assistant text answering
it, and every tool call and result in between. It is the smallest unit that carries its own
referent — half of all user messages are "yes", "do that", "still broken" — and it is what
answers "why did the build fail" when the answer spans four documents. One doc per message / per
tool call stays; the turn is an identifier layered on top, not a replacement.

The transcript format already defines the boundary: conversational order is file order, and the
boundary is exactly a `user` record that `UserRecord::is_human_turn()` accepts (tool results,
compaction summaries, meta turns and `isVisibleInTranscriptOnly` records are `user` records too,
and the predicate excludes every one of them). Three rules pin `turn_seq`:

1. **Definition.** `turn_seq` is the `seq` of the first doc emitted from the record that opened
   the turn. Every doc in the turn carries it, so a turn is a contiguous `seq` range within one
   `source_path`, and the opening prompt is the doc whose `seq == turn_seq`.
2. **A file that opens mid-conversation** (`resetSessionFile()`, a `relocated` sidecar — §9) has
   assistant records before any human prompt. Those docs get `turn_seq == seq_base` — a synthetic
   opening turn — so grouping stays total and no consumer handles an `Option`.
3. **A sidechain is one turn.** A subagent's `user` records are synthesised by the parent, so
   `origin.kind == "human"` never fires and the whole file shares `turn_seq == seq_base`. That is
   arguably right — a subagent invocation *is* one turn of its parent — but any window that snaps
   to a turn has to cap, because "the turn" here is the whole transcript.

`ParseCarry::open_turn_seq` is the non-obvious part. A tail parse sees only the appended bytes,
and on a live transcript a turn straddles that boundary nearly every time; without the carry every
tail would restart numbering at its own `seq_base` and the incremental result would stop matching
`parse_whole`, which the rules above require byte for byte. Replacements keep their original
`turn_seq` for free — they are rebuilt from the carried `Doc`, and a late result only fills in
`tool_output`.

### Contextual BM25

Our documents are fragments torn out of a conversation, and a fragment is not retrievable by
what it was *for*. A tool call indexes a tool name and the strings of its input — `Bash cargo
build --release` — with no trace of what it was in aid of; half of all user messages are "yes",
"do that", "still broken". Neither is reachable by anything a person would type. Anthropic's
Contextual Retrieval measures the fix: prepending chunk-specific context before indexing cut
top-20 retrieval failure from 5.7% to 3.7% on the embedding side, and the BM25 half of the same
technique took the combination to 2.9%. The BM25 half needs no embeddings — it is an index-time
text change to the lexical index we already have.

`context_text` is that header, and every consequence follows from two words in its schema row:
**indexed, not stored**. Not stored is why `search::doc_from_stored` cannot read it back, why no
`SnippetGenerator` can highlight it, and why `format::doc_json` cannot print it — a property of
the schema rather than a rule three call sites have to remember. It is *not* a change to `text`:
a message's markdown is split into `text` / `code` / `headings` with `body` holding the original,
and scaffolding injected into `text` would corrupt that split and leak into every snippet.

**What is in it, cheapest first.** The session's title, then its first prompt, then the project
path's *basename* (the whole path is already an exact-match field, and `/home/user/` is not a
word anyone searches by), then the git branch, then the opening human prompt of the document's
own turn. Pieces are joined with `" · "`, whitespace inside each is collapsed, and a piece equal
to one already in the header is dropped — a session whose title *is* its opening prompt would
otherwise hand those words a term frequency they did not earn, in every document it holds. The
document that *is* its turn's opening prompt (`seq == turn_seq`) gets no turn piece at all: it
would be indexing its own words a second time.

**Where each piece is composed, and why it has to be in two places.**

- The **per-turn** piece is known only to the parser, and on a tail parse the opening record is
  almost always on the far side of the byte offset. So the parser tracks it beside
  `Parser::current_turn` — set where `is_human_turn()` fires, cleared there too so a text-less
  human turn cannot leave the previous prompt describing it — carries it in
  `ParseCarry::open_turn_prompt`, and puts it on every doc of the turn as `Doc::turn_prompt`.
  A `Doc` field rather than a side channel, so a completed tool call rebuilt from the carried
  document keeps its header byte-identical for free.
- The **per-session** pieces are *not* reliably known to a tail parse — the `summary` record and
  the first prompt are behind the offset — but `index.rs` already maintains the merged
  `SessionInfo` per path. So `consume()` merges the session row **before** it writes the
  documents and hands that row to `doc_to_json`. The trade-off this leaves: a title that arrives
  after a doc was indexed is not in that doc's header until the next `index --full`. That is the
  bargain `sessions.json` was built on — a late title is a cheap JSON rewrite, not a doc rebuild
  — and the `first_prompt` fallback is in the header from the very first parse, which is the
  half that matters.
- `project` and `git_branch` are already per-doc and need no carry.

**The cap.** A long first prompt would dominate a short tool call's fieldnorm, so every piece
has a budget — 100 bytes for each prose piece, 40 for a name — and the assembled header a hard
ceiling of 400. The budgets sum below the ceiling on purpose (3 × 100 + 2 × 40 + separators =
396): the ceiling is a backstop against a piece added later, not a knife, so the most
document-specific piece — the turn's own prompt, composed last — is never the one a long session
title crowds out. Every cut is on a word boundary as well as a char one
(`parse::truncate_words`): a header is fed to the `prose` analyzer, so a cut through the middle
of a word invents a term nobody types and still charges the document for it.

**What it does to scoring.** Two effects, both deliberate.

- *Fieldnorms.* A near-constant prefix on every document shifts `avgdl` and so changes BM25
  length normalisation corpus-wide. Capping the header is what bounds the shift; the eval
  harness that would quantify it does not exist yet, so there are no before/after numbers here
  and none are claimed.
- *IDF collapse.* Every document in a session shares a header, driving those terms toward 100%
  document frequency *within* the session. `context_text` therefore carries
  `set_field_boost(0.3)` — well below 1.0, because the header is a claim about what a document
  was for and the body is what it says. Without the discount, scaffolding shared by a whole
  session would reorder that entire session at once. With it, a document holding the term in its
  body outranks the ones carrying it only in their header, which is asserted rather than argued.

**Snippets.** A `context_text` match must never become the rendered snippet. It cannot — the
field is not stored, so `snippet_from_doc` has nothing to read and `search()` builds no generator
for it — but a hit matched *only* through its header still has to show something honest, and it
falls through to the excerpt of its own body.

`format.rs`:

```rust
pub struct OutputOpts { pub json: bool, pub color: bool, pub context: usize, pub width: usize }
pub fn search_results(w: &mut impl Write, r: &SearchResponse, o: &OutputOpts) -> anyhow::Result<()>;
/// `--context N`: `SearchResponse` carries no surrounding turns and `format.rs` holds no index
/// handle, so `cli.rs` fetches one `context::around` window per hit and passes them in here.
/// `search_results` is the `context: &[]` case of this.
pub fn search_results_ctx(w: &mut impl Write, r: &SearchResponse, context: &[Vec<Doc>],
                          o: &OutputOpts) -> anyhow::Result<()>;
pub fn facet_list(w: &mut impl Write, r: &FacetResult, o: &OutputOpts) -> anyhow::Result<()>;
pub fn session_view(w: &mut impl Write, docs: &[Doc], o: &OutputOpts) -> anyhow::Result<()>;
pub fn session_list(w: &mut impl Write, s: &[SessionInfo], o: &OutputOpts) -> anyhow::Result<()>;
pub fn stats(w: &mut impl Write, s: &IndexStats, o: &OutputOpts) -> anyhow::Result<()>;
```

Anything that prints a document's body prints its stored `body`, with `tool_output` beside it:
the split is for retrieval and cannot be undone, so rendering from `text` + `code` would print
every inline span twice and move a fenced block to the end of the message. The one reordering is
the rule above — a failed call previews its error first. `doc_json` carries `body`, `text`,
`code`, `headings`, `code_lang` and `tool_output`.

## CLI surface

```
session-search index [--full] [--root DIR]... [--index DIR] [--jobs N] [--include-thinking]
                     [--no-spilled-results]
session-search search <QUERY> [FILTERS] [--facets f1,f2] [--context N]
                              [--limit N] [--offset N] [--json] [--no-refresh]
                              [--include-thinking]
session-search facets <FIELD> [--query Q] [FILTERS] [--top N] [--json] [--no-refresh]
session-search show <SESSION_ID> [--agent AGENT_ID] [--around UUID|SEQ]
                                 [--before N] [--after N] [--limit N] [--json] [--no-refresh]
session-search sessions [FILTERS] [--limit N] [--json] [--no-refresh]
session-search stats [--json]

FILTERS: -p/--project P  -t/--tool T  --tool-input k=v  --tool-output TEXT  --program NAME
         --lang LANG  --branch B  --model M
         --role R  --kind message|tool_call  --session S  --agent-type A
         --since D  --until D  --errors-only  --no-sidechains  --sidechains-only
```

Global: `--index DIR` (`$SESSION_SEARCH_INDEX`), `-v/--verbose`, `--no-color` (`$NO_COLOR`).

**`FIELD` for `facets`** is any fast field name (`tool_name`, `project`, `model`,
`git_branch`, `role`, `kind`, `agent_type`, `entrypoint`, `code_lang`) **or any JSON path** such
as `tool_input.file_path`, `tool_input.command`, `tool_input.pattern`, `bash_cmd.program`,
`bash_cmd.args`. `code_lang` and the `bash_cmd` paths are multi-valued: a script that runs four
programs lands in four buckets, as does an answer with four fence languages, so the bucket
counts total *values*, not documents — they can run above the
match set (many programs per Bash call) or far below it (most matched documents are not Bash
calls at all). `docs_with_value` is a document count either way, so it never exceeds the match.

## MCP readiness (next step — do not build now)

`rmcp 3.2`, `default-features = false`, `features = ["server","macros","transport-io","schemars"]`,
behind an `mcp` cargo feature. One `#[tool]` per subcommand, taking `Parameters<T>` and
returning `Json<T>`, where `T` is the *same* struct clap derives into. Therefore: keep
`Filters`/`SearchRequest` plain data, no clap types leaking into `search.rs`, and derive
`serde::Deserialize` on them from the start.

Two traps recorded now: rmcp's README says `schemars = "0.8"` and is **wrong** (it is `^1.0`);
and stdio transport owns stdout, so `tracing` must write to **stderr** and color must be off.

## Conventions

- Errors: `thiserror` enums in library modules, `anyhow` at the CLI boundary. A malformed line
  is a counted `ParseError`, never a hard failure of the run.
- Logging: `tracing`, subscriber writes to **stderr**, `-v` raises the level.
- Rust 2024 edition. Keep `cargo clippy` clean; run `cargo fmt`.
- Tests: unit tests beside the code; fixtures in `tests/fixtures/`; `insta` for snapshots with
  UUIDs, absolute paths and timestamps redacted.
