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

**`MoreLikeThisQuery` (0.26.2), read in full before `--similar-to` was written.** Five facts,
each of which changes the design rather than decorating it:

- **`with_document(addr)` collects from STORED fields, and from every one of them.** It calls
  `searcher.doc(addr)` and walks `get_sorted_field_values()`; a field that is indexed and not
  stored (`context_text`) is invisible to it, and a field that is stored and not indexed (`body`,
  `raw`) is fetched and then dropped by an `is_indexed()` guard. For this schema that puts
  `session_id`, `doc_id`, `source_path`, `seq`, `turn_seq`, `timestamp` and the flags into the
  term pool — and a `session_id` term's document frequency is exactly the size of the source
  session, which is squarely inside the "interesting" band. `with_document` would therefore
  return the rest of the source session and call it similarity. **Use `with_document_fields`,
  even for a single-document source.**
- **`with_document_fields` checks only `is_indexed()`**, so it *can* target `context_text`.
  Excluding the header has to be a written decision, not an accident of the storage flags.
- **JSON fields hit a `_ => {}` arm** in `add_term_frequencies`: `tool_input` and `bash_cmd`
  contribute nothing to similarity, silently. Do not offer `--similar-in tool_input`.
- **`MoreLikeThisQuery` implements only `weight`.** It inherits the no-op default
  `Query::query_terms`, so `SnippetGenerator::create` — and `search::snippet_generator`, which
  is driven the same way — sees zero terms and every hit degrades to an unhighlighted
  head-of-body excerpt. No error, no warning.
- **`MoreLikeThisQuery::weight` errors under `EnableScoring::Disabled`** ("MoreLikeThisQuery
  requires to enable scoring."), and `BooleanQuery`/`BoostQuery` forward `enable_scoring`
  verbatim, so wrapping does not rescue it. `Searcher::search` passes `Disabled` whenever
  `collector.requires_scoring()` is false — which is `TopDocs::order_by_fast_field`,
  `docs_with_value`'s bare `Count`, and all of `facets()`.
- **Its term selection is not reproducible, and this is the one that cannot be worked around
  from outside.** `create_score_term` accumulates term frequencies in a
  `std::collections::HashMap<Term, usize>` and picks the best `max_query_terms` with a
  `BinaryHeap` whose `Ord` compares **only the score**. `score = tf * idf`, so two terms with the
  same term frequency and the same document frequency tie exactly — the common case for a
  turn-shaped seed, where nearly every token occurs once — and which of the tied terms survives
  is then decided by `HashMap` iteration order. `RandomState` reseeds per instance, so the
  surviving set differs between two calls *in one process against one unchanged index*. The
  visible symptoms: `--similar-to` returned a different hit set and a different `total` on every
  invocation; `--limit`/`--offset` paged over a different query each time, repeating and skipping
  documents; and because `search()` re-derives a weight for the hit pass, the `Count`, the
  aggregation and `docs_with_value`, a single `--facets` command could print `62 of 58 matching
  docs have a value`.

Two more, smaller: an empty `field_to_values` slice is an `Err` whose message blames missing
stored fields (a lie for any caller using `with_document_fields`), while a non-empty source whose
terms are all filtered out yields a zero-clause `BooleanQuery` that matches nothing with no error
at all — two different outcomes needing two different guards. And `max_query_terms` is off by
one (`if score_terms.len() > limit`), so 32 admits 33 clauses. `MoreLikeThis` itself — the struct
with the tuning fields — is **not** exported; only `MoreLikeThisQuery` and its builder are, so
there is no way to inspect or reuse the `BooleanQuery` it builds.

**So `MoreLikeThisQuery` is not used.** `search::similar_terms` and `search::similar_query`
rebuild the same shape — tokenize the seed per field with that field's own analyzer, keep the
terms inside the document-frequency band and the word-length bounds, score them `tf * idf`, and
OR the best `SIMILAR_MAX_QUERY_TERMS` of them into a `BooleanQuery` of `BoostQuery(TermQuery)`
normalised by the best term's score. The selection is ordered by `(score, field, token)`, which
is total, so the same seed against the same index yields the same query every time; the cap is
exact rather than off by one; and because the result is a concrete `BooleanQuery` rather than a
query type that recomputes itself inside `weight()`, every pass of one request sees the same
clauses. Rebuilding it also settles the two edges above it: a `BooleanQuery` of `TermQuery`s
reports its own `query_terms` and skips scoring when a collector asks it to, so no scoring
adapter is needed.

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
  sessions.rs    filter sessions.json (not the index)             [Build B]
  format.rs      human + JSON rendering                           [Build C]
  cli.rs         clap definitions                                 [Build C]
  main.rs        wiring only                                      [Build C]
  api/mod.rs     server bootstrap, router, handlers, CORS         [feature http-api]
  api/dto.rs     wire shapes: query strings, Search UI envelope   [feature http-api]
  api/assets.rs  the `web/` files, compiled into the binary       [feature web-ui]
web/             the browser UI; no build step, no CDN            [feature web-ui]
  index.html  styles.css  dom.js  markdown.js  tools.js  app.js
tests/
  fixtures/*.jsonl        redacted real slices + hand-written record shapes
  fixtures/eval/          the retrieval eval corpus: six synthetic transcripts
  fixtures/eval_queries.json   the graded query fixture
  eval/                   the retrieval eval harness [test target `eval`]
    main.rs  corpus.rs  fixture.rs  metrics.rs  report.rs  similar.rs
docs/
  TRANSCRIPT-FORMAT.md   DESIGN.md   WEB-UI.md   MCP.md
  EVAL.md                the recorded eval numbers, spliced from target/eval/
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
    /// `skip_serializing_if`: never stored, so a `Doc` read back from the index has nothing
    /// here, and a consumer that serializes the struct whole (the HTTP API) must not report
    /// `null` on every document as if the turn had no prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
// Grew two fields with the web UI; see "Optional front ends" below for both and for why.
pub struct Hit { pub doc: Doc, pub score: f32, pub snippet: String,
                 pub snippet_field: SnippetSource, pub snippet_marks: Vec<Range<usize>> }
pub struct SearchResponse {
    pub hits: Vec<Hit>, pub total: usize,
    pub facets: BTreeMap<String, FacetResult>, pub elapsed_ms: u128,
    /// What the search noticed that the numbers cannot say. See "Warnings travel with the
    /// answer" below. `#[serde(default)]`, as `grouped` is.
    pub warnings: Vec<String>,
}

/// Which end of a `--since`/`--until` range a `When` is being resolved for, and that
/// resolution. `pub(crate)`, beside `parse_when`/`When`/`DAY_MS`.
///
/// The index side never needs it: `date_range` hands Tantivy a `Bound`, so a bare day is
/// `Included(midnight) .. Excluded(midnight + DAY_MS)` and no single instant stands for the
/// day. Everything that filters `sessions.json` instead compares two `i64`s and needs one
/// number, so the day collapses to an edge — midnight below, `+ DAY_MS - 1` above, both
/// inclusive. One definition, because a second front end that re-derived it is how
/// `--until 2026-09-09` starts meaning two different days on two surfaces.
pub(crate) enum Edge { Lower, Upper }
pub(crate) fn when_ms(raw: &str, now: chrono::DateTime<chrono::Utc>, edge: Edge)
    -> anyhow::Result<i64>;
/// `~` / `~/rest` against `$HOME`. `pub(crate)`: the index-side `project` filter and the
/// `sessions.json` one are the same typed value down two code paths, and a project that
/// matches under `search` and not under `sessions` is a difference nobody looks for.
pub(crate) fn expand_tilde(path: &str) -> String;
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

**Warnings travel with the answer.** A zero-hit response must never read as an authoritative
"no". Three outcomes of this index are indistinguishable from an empty corpus at the call site,
and all three used to reach only `tracing`:

| when | what it means |
| --- | --- |
| `total == 0` and the query holds a `word:value` whose root is not a schema field | the term became a `tool_input.word` subpath lookup, which cannot fail to parse and matches nothing. Far more often a misread colon than an empty corpus |
| `total == 0` and `--similar-to` was given | every term of the seed fell outside the similarity tuning, so `MoreLikeThisQuery` built a `BooleanQuery` with no clauses. No error is raised anywhere |
| `--group-by-turn`, a page short of `--limit`, and documents still matching past the collapse window | the matched documents cluster into fewer turns than `GROUP_FANOUT × (limit + offset)` reached |

Each is now **both** logged and pushed onto `SearchResponse::warnings`. The sentence is a
`WARN_*` constant in `search.rs` shared verbatim by the `tracing::warn!` and the push, so the two
texts cannot drift; the structured fields stay on the log line only, since a caller already holds
the query it sent. `Vec<String>` rather than a typed enum because every consumer renders these as
prose — a variant would be flattened to a sentence at the only place it is read.

The HTTP side joins them into the channel it already has: `dto::search_ui_response` chains
`SearchResponse::warnings` after `PreparedSearch::warnings` into the single `info.warnings`
array. Request-side warnings (an unknown body key, a widened facet size) come first, answer-side
warnings after. A second key beside it would be a channel every client has to be told about
separately, and the newer one is the one they would miss. `--json` on the CLI does **not** carry
them, exactly as it does not carry `grouped`: `format::search_json` hand-builds its object, and
the CLI's warnings already reach the terminal on stderr.

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
/// One turn's documents and the size of the turn they were cut from. `total` is a `Count` over
/// the same query, because `docs_by_seq` clamps `limit` against the index and a short window
/// therefore means either "the turn ends here" or "the cap bit" — only the count separates them,
/// and a capped window that says nothing reads as the whole turn.
pub struct TurnWindow { pub turn_seq: u64, pub docs: Vec<Doc>, pub total: usize }
/// [`turn`] with that count. Every renderer wants this one; `turn` is the pinned shape.
pub fn turn_window(index: &tantivy::Index, f: &Fields, source_path: &str, turn_seq: u64,
                   limit: usize) -> anyhow::Result<TurnWindow>;
```

`sessions.rs`:

A session listing does not come from Tantivy. It comes from `sessions.json` — one row per
transcript file — so every front end offering `sessions` answers two questions of its own: which
`Filters` this row satisfies, and which of them could never have been asked here. Both answers
used to be written **once per front end**, and the two had already drifted on the second: `cli.rs`
warned about `program` and `lang` and said nothing about `min_thinking`; `api/mod.rs` reported
`min_thinking` and said nothing about `program` or `lang`. Neither was right, both were plausible,
and neither reader could see the other. A third front end would have made it a three-way
disagreement, so it is one module.

```rust
/// The subset of `Filters` that `sessions.json` can answer, pre-resolved once — the dates are
/// the fallible part, and resolving them per row would also let `now` move underneath a
/// listing, measuring the first row and the last against different windows.
pub struct SessionMatcher { /* private: project, branch, session, agent_type,
                              since_ms, until_ms, no_sidechains, sidechains_only */ }
impl SessionMatcher {
    pub fn new(f: &Filters) -> Result<SessionMatcher, FilterError>;
    pub fn matches(&self, info: &SessionInfo) -> bool;
}
/// An unreadable `--since`/`--until`. `field` is the plain name (`"since"`), never a flag: the
/// CLI spells it `--since` and HTTP spells it `since=`, and a shared matcher that baked in
/// either would put a flag nobody can type into an HTTP 400. Each front end renders `field` in
/// its own dialect — pinned by `api::tests::a_malformed_date_is_a_400_wherever_it_arrives`.
pub struct FilterError { pub field: &'static str, pub source: anyhow::Error }
/// The filters that arrived and that a session listing cannot answer, in `Filters` declaration
/// order. The *complement* of what `SessionMatcher` reads, which is why it lives beside it.
pub fn unanswerable_filters(f: &Filters) -> Vec<&'static str>;
/// The same list as sentences, for a front end that answers in data rather than on stderr.
pub fn unanswerable_filter_notes(f: &Filters) -> Vec<String>;
```

**The reconciled list is ten**, and it is the complement of the eight a `SessionInfo` can be
compared against (`project`, `branch`, `session`, `agent_type`, `since`, `until`,
`no_sidechains`, `sidechains_only`):

```
tool  tool_input  tool_output  lang  min_thinking  program  model  role  kind  errors_only
```

None of them is *slow* against `sessions.json`; there is no column to compare them to at any
cost. Silently dropping one is what this refuses: a listing filtered by nine of ten looks exactly
like a listing filtered by ten, and the caller reads "no session used that model" out of a result
that means "that question cannot be asked here". Pinned by
`sessions::tests::the_unanswerable_filter_list_is_the_same_on_both_front_ends`.

What stays with each front end is only the **rendering**. `cli.rs` maps the names to flags
(`--tool-input`) and `tracing::warn!`s them, because its answer is a table a human is looking at.
`api/mod.rs` returns `unanswerable_filter_notes` in the response body, because that surface has
no stderr a caller can read — and neither will the next one.

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
   `source_path`, and the opening prompt is the doc whose `seq == turn_seq`. A prompt that
   indexes nothing — an empty message, or one whose only block carries no text — opens its turn
   at the *next* doc instead: a number pointing at a doc that was never emitted would leave a
   hole in the range, and contiguity is what a window scoped to a turn walks.
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
`tool_output`. A `Reset` is handed no carry at all (`index::Job::file_context`), so a rewind or a
`resetSessionFile()` cannot leave a tail continuing a turn out of bytes that no longer exist.

### Turn skeletons

Two problems, one fix, and the fix is a rendering rather than a schema change.

**The top-k budget.** A message-level query for "why did the build fail" matches the prompt, the
assistant text, the tool call and the result — four hits describing one moment, so `--limit 10`
spends 40% of itself there. `search --group-by-turn` collapses hits sharing `(source_path,
turn_seq)` and keeps the best-scoring member as the anchor. The path is half the key on purpose:
`turn_seq` is a per-file ordinal, so two transcripts sharing a session id (§9's
`resetSessionFile()`) both hold a turn #0 and collapsing on the number alone would merge two
conversations into one hit.

**The token budget.** Tool outputs are the overwhelming majority of the bytes in this corpus and
close to none of the intent. A *skeleton* — the prompt, the prose, and one line per call — is a
few hundred bytes for a turn that may carry 200 KB of logs. `search --context skeleton` and
`show --turn --skeleton` render one.

The nice part is that nothing had to be stored. The split that makes a skeleton free was made
for query semantics: a tool call already keeps the **call** (its name plus the input's own
strings) in `text` and the **result** in `tool_output`, so *the skeleton is the `text` side of a
turn's documents*. Measured on the two real captures and the eval corpus, a turn's JSON context
costs 5651 bytes on average and its skeleton 555 — 9.8% overall, and the worst turn in the
corpus goes from 19541 bytes to 1642 ([`EVAL.md` §5](EVAL.md#5-turn-skeletons--issue-25)).

**Derived at query time, never materialised.** A stored `turn_skeleton` field on the leading
document would save the fetch and cost the thing `build_replacements` exists to protect: a late
`tool_result` produces a *replacement* document, and the stored skeleton of the turn's opener
would have to be regenerated byte for byte to match, from a document the replacement path does
not hold. A turn is 10–50 documents and `turn_seq` is a fast field, so deriving is a term query.
Only profiling should change this, and then the replacement path has to be solved deliberately.

**The one exception: errors.** A failed call's *first output line* rides in the skeleton, and it
is the only place output text earns a slot. `format::doc_body` already leads a failed call with
its result for the same reason — the error is the answer and the command that failed is only
context, which is the whole of `--errors-only`. The first line, not the frames under it: what
names a failure is `error[E0433]: failed to resolve`, and `show` is where the stack lives.

**What each mode reports, and what it refuses to imply.**

- `Hit::collapsed` is the number of *other* matching documents in the anchor's turn, counted
  exactly — one intersection of the live query with the same `(source_path, turn_seq)` lookup a
  turn window walks, per hit on the page. Counting members as they streamed past would have
  reported the fetch window instead of the match, so `+12 in turn` would mean "+12 that happened
  to fit" and would shrink as `--limit` grew.
- **`offset` counts turns** under `--group-by-turn`, because a page of turns cannot be skipped
  by a number of documents. The collector's own offset is therefore not used, and it is asked
  for `GROUP_FANOUT × (limit + offset)` documents instead — enough to see that many distinct
  turns. A page that comes up short of `--limit` while documents are still matching says so on
  stderr rather than looking like the end of the results.
- **`total` keeps counting documents**, grouped or not, and `SearchResponse::grouped` is what
  tells a caller the two now answer different questions. The distinct turns behind a match set
  are a second pass over all of it for a number nobody pages by. The human rendering says
  `5 turns · 32 matching docs` rather than `5 of 32 hits`, so the two are never divided.
- The skeleton **replaces** the context documents in `--json` rather than joining them; sending
  both would undo the only thing it is for. The anchor document itself is untouched — a hit is
  still the full document, with its `body` and its `tool_output` — because that shape is a
  pinned contract and this issue is about what a hit's *context* costs.
- Both caps are reported. `context_turn` says what the document cap left out, and the skeleton's
  own `dropped` says what the byte cap did.

**Thinking is never in a skeleton**, at any budget. It is opt-in everywhere else in this crate,
and this is the one rendering built to be pasted somewhere else.

**Not on the HTTP surface.** `SearchBody` is Elastic Search UI's `RequestState`, whose every
paging control divides `total` by `resultsPerPage`; a facade that paged in turns while reporting
a document total would be wrong in the one place a user can see it. The decision is pinned by
`dto::tests::group_by_turn_is_not_a_search_parameter`, next to the same decision for
`--similar-to`.

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

**What it does to scoring and matching.** Three effects, all deliberate.

- *The matched set widens.* `context_text` is a default query field, so a bare query matches
  every document whose header carries the term, not only the documents whose bodies do — that
  is what makes `yes` retrievable at all. It follows that `total`, paging, `facets --query` and
  the `--facets` counts all describe the widened set: `facets tool_name --query tokenizer`
  answers "which tools ran in turns and sessions *about* tokenizers", which is a different and
  usually more useful question than the one the body-only query asked. It also follows that
  under `--sort newest|oldest`, where no score is computed, the discount below has nothing to
  act on and a header-only match is as good as a body match: a time-ordered page of a query
  that names a session's title *is* that session, in time order. A field-qualified query
  (`text:tokenizer`) still asks about bodies only.

- *Fieldnorms.* A near-constant prefix on every document shifts `avgdl` and so changes BM25
  length normalisation corpus-wide. Capping the header is what bounds the shift. The eval
  harness below now measures the header's effect on retrieval (see **Retrieval evaluation**) and
  finds no class worse off for it, but its corpus is 65 documents: that is enough to show what
  the header retrieves and far too small to say anything about a corpus-wide `avgdl` shift. No
  claim about fieldnorms is made from those numbers.
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
pub struct OutputOpts {
    pub json: bool, pub color: bool, pub context: usize, pub width: usize,
    /// `SimilarSource::label()` for a `--similar-to` search: one header line naming the turn the
    /// hits are similar to. Human rendering only — the `--json` envelope does not echo it.
    pub similar_to: Option<String>,
}
/// The turn a window snapped to, and the size of that turn. A turn window is capped, so the
/// rendering needs both numbers to say "turn #12 · 200 of 347 docs" instead of showing a
/// truncated turn as if it were whole.
pub struct TurnSpan { pub turn_seq: u64, pub total: usize }
/// One hit's pre-fetched surroundings. `--context N` fills `docs` alone; `--context turn` fills
/// `turn` as well, since documents by themselves cannot say whether any were left out.
/// `skeleton` is a rendering choice over the same documents — see "Turn skeletons".
pub struct HitContext { pub docs: Vec<Doc>, pub turn: Option<TurnSpan>, pub skeleton: bool }
impl HitContext {
    pub fn around(docs: Vec<Doc>) -> Self;                          // turn: None
    pub fn turn(docs: Vec<Doc>, turn_seq: u64, total: usize) -> Self;
}
pub fn search_results(w: &mut impl Write, r: &SearchResponse, o: &OutputOpts) -> anyhow::Result<()>;
/// `--context N|turn`: `SearchResponse` carries no surrounding turns and `format.rs` holds no
/// index handle, so `cli.rs` fetches one `context::around` (or `context::turn_window`) window
/// per hit and passes them in here. `search_results` is the `context: &[]` case of this.
/// A turn window adds a `context_turn` object — `{turn_seq, shown, docs_in_turn, truncated}` —
/// to the hit's JSON, and a `turn #N · ...` line to the human rendering.
pub fn search_results_ctx(w: &mut impl Write, r: &SearchResponse, context: &[HitContext],
                          o: &OutputOpts) -> anyhow::Result<()>;
pub fn facet_list(w: &mut impl Write, r: &FacetResult, o: &OutputOpts) -> anyhow::Result<()>;
pub fn session_view(w: &mut impl Write, docs: &[Doc], o: &OutputOpts) -> anyhow::Result<()>;
/// `session_view` for `show --around ... --turn`: the same rendering, plus the line under the
/// session header saying which turn it is and what the cap left out (`"turn"` in the JSON).
pub fn turn_view(w: &mut impl Write, docs: &[Doc], span: TurnSpan, o: &OutputOpts)
    -> anyhow::Result<()>;
pub fn session_list(w: &mut impl Write, s: &[SessionInfo], o: &OutputOpts) -> anyhow::Result<()>;
pub fn stats(w: &mut impl Write, s: &IndexStats, o: &OutputOpts) -> anyhow::Result<()>;

// -- the skeleton envelope, public because it is a wire shape ---------------------------------
/// `{lines, dropped, bytes}`, and the only definition of it. A front end that builds a
/// skeleton-shaped hit for itself gets the two keys that are not `lines` wrong: `bytes` is
/// `Skeleton::bytes()` — the rendered size with newlines counted, not `lines.join("\n").len()`
/// — and `dropped` is what the byte cap left out. A caller that omits `dropped` publishes a
/// truncated turn as a whole one, the single claim a skeleton must never make.
pub fn skeleton_json(skeleton: &Skeleton) -> Value;
/// `{turn_seq, shown, docs_in_turn, truncated}` — the *document* cap, where `skeleton_json`'s
/// `dropped` is the *byte* cap. Public alongside it because a turn-shaped answer reports both
/// or it misreports one. `TurnSpan::truncated` stays private: this is the only shape it is
/// read in.
pub fn turn_json(span: TurnSpan, shown: usize) -> Value;
```

`TurnSpan`, `HitContext`, `Skeleton`, `turn_skeleton`, `SKELETON_BUDGET` and `doc_json` were
already public and needed no change; `skeleton_json` and `turn_json` are the two that were not,
and together with those they are everything an out-of-module caller needs to assemble a
skeleton-shaped hit without reimplementing an envelope.

Anything that prints a document's body prints its stored `body`, with `tool_output` beside it:
the split is for retrieval and cannot be undone, so rendering from `text` + `code` would print
every inline span twice and move a fenced block to the end of the message. The one reordering is
the rule above — a failed call previews its error first. `doc_json` carries `body`, `text`,
`code`, `headings`, `code_lang` and `tool_output`.

### Similarity (`--similar-to`)

"Find me more turns like this one" is a different question from "find me turns matching these
words", and the difference is not stylistic: a person who has found one good answer usually
cannot say *which* of the words in front of them were the ones that mattered. Tantivy answers it
with `MoreLikeThisQuery` — re-tokenize a source document, keep the terms that discriminate, OR
them into a `BooleanQuery` of `TermQuery`s weighted by `tf * idf`. Everything below is the gap
between that sentence and a feature. The five API facts it is built on are in **Verified Tantivy
facts** and are not repeated here.

**Where it sits: one more `Occur::Must` clause in `build_query`.** That is the entire integration.
Nothing downstream learns that a clause came from a document rather than from a word, so the
collectors, `Count`, the aggregations, `docs_with_value`, `--context turn` and paging all compose
for free — and `--similar-to X "regression" -p ~/code --since 30d --errors-only` is one
`BooleanQuery`, which is what makes "more like this, but only in this project, only last month"
a command rather than a feature request.

**How a reference resolves.** `search::resolve_doc` takes four shapes, disambiguated by arity
before any index access, so a coordinate can never be mistaken for a uuid:

| shape | example | resolved by |
| --- | --- | --- |
| `SESSION:SEQ` | `b20208d8:41` | session prefix + `MustNot(Exists(agent_id))` + `seq` term |
| `SESSION:AGENT:SEQ` | `b20208d8:a108:41` | as above with an agent prefix; `-` means the main file |
| a record uuid or `tool_use_id` | `018f2c…` | exact term on `uuid` / `tool_use_id` / `doc_id` |
| a `doc_id` | `s1:-:7b841ded:2` | the same term query; `doc_id` is four colon-separated parts |

Anything that is not a coordinate goes through the id path, which tries **exact before prefix** —
mirroring `cli::resolve_id`, so an id that is also the prefix of a longer one resolves to itself.
Two candidates are fetched, never one: "the first of several" and "the only one" are the same
result to a collector, and picking the first silently is how a similarity search ends up seeded
from a document nobody named. An ambiguous prefix names its candidates and stops. This resolves
index-side rather than through `sessions.json`, which is why it covers uuids and `doc_id`s and not
just session ids — `cli::resolve_id` is still what expands a session id for `show`'s first
argument.

`show --around` was rerouted through the same resolver. Two commands disagreeing about what
`abc123` means would be worse than either behaviour alone; `--around` gains the prefix and
coordinate spellings it did not have (it previously required an id in full, and answered anything
else with `no document with uuid ... in session ...` — an error, never a wrong window); and a
linear scan of up to 50 000 documents is retired in favour of a term query.

The session named on the command line is an **input** to that resolution, not a check applied
after it (`search::DocScope`). This matters because a reference can be unique inside the named
transcript and ambiguous outside it — §9's `resetSessionFile()`/`relocated` case indexes one
transcript under two project keys, so the same record uuid legitimately appears twice, and
`source_path_for` returns `None` in exactly that case, leaving the caller no narrower spelling to
use. Resolving index-wide first would refuse a question that has one answer. Two candidates that
are the same `seq` of the same session are collapsed for the same reason: they are one record read
from two files. A reference that resolves only *outside* the named session is still refused with
both sides named, because "no such uuid" would be a lie about a document that plainly exists.

**The source is the whole turn.** A default, not a flag. A turn is the unit a person remembers —
the prompt, the calls it made, the answer — and each of those alone is a fragment: a tool call on
its own is a path and an exit code. `context::turn_window` reads it, capped at
`SIMILAR_SOURCE_DOCS = 200` for the same reason `cli::TURN_WINDOW_LIMIT` exists (a sidechain file
is one turn by rule 3, so an uncapped seed is a whole subagent transcript), and at
`SIMILAR_SOURCE_BYTES = 256 KiB` per field so one `cat` of a large file cannot make term
extraction the slowest part of a query.

A consequence worth writing down, because it contradicts the usual description of MoreLikeThis:
**the source does not reliably rank first when it is included.** That property holds for a
single-document seed — the document matches every clause it generated — and fails for a
turn-shaped one, because no single document of the turn carries the whole union of its terms and
BM25 length normalisation then lets a short document elsewhere outrank all of them. The eval
harness measures a case of exactly that. It is also why the source turn is removed by an explicit
`MustNot` rather than by trusting the ranking to put it somewhere predictable.

**What seeds it.** `--similar-in` selects among `text` (the default), `code`, `tool_output` and
`thinking`; `thinking` is gated on `--include-thinking` like everywhere else. `text` alone is what
means *about the same thing*; `code` and `tool_output` mean *built out of the same identifiers*
and *failed the same way*. `tool_input` and `bash_cmd` are not offered at all — they are JSON
fields, which Tantivy's term extraction skips in silence, so the flag would do nothing.

**`context_text` is excluded, deliberately.** `with_document_fields` could target it — it only
checks `is_indexed()` — and that is exactly why the exclusion is a decision rather than a
side-effect of the field not being stored. The header is near-identical for every document of a
session by construction (that is what makes it work for retrieval), so its terms would pull the
whole source session back and call it similarity: the one false positive this feature exists to
avoid. The interaction runs the other way too, and `SIMILAR_MAX_DOC_FREQUENCY_RATIO` is what
handles it: the header makes session-wide words *common*, and a term that common is dropped.

**The tuning, and why each number.** "Left at the default" is not an answer for any of these.

| parameter | value | why |
| --- | --- | --- |
| `min_doc_frequency` | `3` | Below this a term is a path, a uuid fragment, a blob id or a typo: it can match one or two documents while its idf dominates the score. 3 not Tantivy's 5, because a document here is one message, so a real shared term can live in a handful. Not 1, because `MAX_TOKEN_BYTES = 255` deliberately keeps whole sha256s in the dictionary. |
| `max_doc_frequency` | `max(N / 4, 50)` | Derived from `searcher.num_docs()` per query, never frozen: Tantivy's bound is an absolute count, so a literal would change meaning as an index grows. Neither analyzer has a stop-word filter, so `the` / `and` / `is` are real terms above 90%; ambient technical words (`test`, `file`, `error`) sit at 10–30%. A quarter cuts the first and keeps the second. The floor of 50 stops a brand-new 80-document index from discarding every ordinary word and returning the empty query. |
| `min_term_frequency` | `1` | Not Tantivy's 2. The seed is a turn of short documents, and requiring a repeat throws away the single mention of the identifier that is the whole reason the turn is memorable. `tf` remains the multiplier in `tf * idf`, so a term said five times still outranks one said once. |
| `max_query_terms` | `32` | Every clause is a posting-list walk, so latency is linear in it. Lucene ships 25 and Tantivy copies it; a turn-shaped seed spans more subjects than one document, so the budget is wider. Above ~50 the tail scores are within noise. (Tantivy's cutoff is `>`, so the real cap is 33.) |
| `min_word_length` | `3` | Bytes on the **analyzed** token. `WordTokenizer` splits on non-word characters, so `-f` arrives as `f`, `2>&1` as `2` and `1`, `&&` as nothing: every flag and redirection becomes a one- or two-character token with an enormous document frequency. Not 4 — that would drop `bug`, `cli`, `sql`, `api`. |
| `max_word_length` | `32` | `MAX_TOKEN_BYTES` is 255 so a sha256 stays findable by pasting it back. Right for an explicit query, wrong here: two documents sharing a blob id share one build, not a topic. 32 is above every hand-written identifier and below every hash and dash-stripped uuid. |
| `boost_factor` | `1.0` | `create_query` normalizes each clause by the best term's score, so this is uniform across the similarity clause and only means anything *relative to a co-occurring free-text `Must`*. 1.0 lets an explicit query outvote the similarity, which is right: typed words are evidence, a seed turn is an inference. |
| `stop_words` | 20 entries | See below. |

The stop-word list is written in **post-analyzer form** — lowercased and English-stemmed — because
`is_noise_word` runs on the token the analyzer emitted, so `assistant` would be a silent no-op
where `assist` works. `tool_use` is `toolus`, because the whole-identifier token drops the
underscore before the stemmer sees it. A unit test feeds every entry back through
`tokenizer::prose_analyzer()` and requires it to come out unchanged.

It is short on purpose and stops where `max_doc_frequency` starts. It holds only what a
corpus-relative bound cannot reliably catch: the tool names `parse.rs` writes into the `text` copy
of every tool call by construction, the role words, and the harness vocabulary. A similarity that
rides on "both of these are Bash calls" is precisely the false positive the feature must not make,
and the frequency of those words is a fact about how a corpus was assembled rather than about any
topic in it. Everything else — English function words included — is left to the frequency bound,
which adapts as an index grows and cannot go stale. The cost is real: `read`, `write`, `edit` and
`task` are ordinary English words too, and a turn genuinely *about* writing a file loses them as
evidence.

**The source document, and what happens to it.** The source *turn* is removed by
`(Occur::MustNot, context::turn_query(...))` — the whole turn, not just the referenced document,
because the turn's siblings share its vocabulary by construction and the first page would
otherwise be the four things already on screen. That is what `show --turn` is for.
`--include-source` puts it back, for debugging and so an eval can check the construction. The
exclusion is *announced*, not silent: the human rendering prints
`similar to <doc_id> · turn #<n> · <k> docs` above the count, so a `--limit 10` that returns nine
neighbours of a four-document turn is legible rather than a discrepancy. The `MustNot` sets
`minimum_number_should_match` to 0 on the outer boolean, which is correct because the similarity
clause is a `Must`.

**Two empty outcomes, two different answers.** An empty `field_to_values` slice makes Tantivy
return "Cannot create more like this query on empty field values. The document may not have stored
fields" — a message that would be an outright lie here, since none of this reads a stored field —
so `resolve_similar` refuses first, with an error naming the turn, the fields it looked in and the
flag that widens them. A *non-empty* seed whose terms all fall outside the tuning is a zero-clause
`BooleanQuery` that matches nothing and reports no error at all; that one cannot be caught up
front, so `search()` logs a warning when a similarity search returns `total == 0` saying which
knobs decide it. Neither case can panic and neither can silently match the whole corpus.

**Highlighting.** `MoreLikeThisQuery` reports no `query_terms`, so without a second source of
terms every similarity hit would fall through to an unhighlighted head-of-body excerpt — a search
tool that quietly stopped saying why anything matched. `snippet_generator` takes the seed text for
the field and derives terms from it directly, applying the *same* filters the query applied — word
lengths, the document-frequency band, the stop words — because a highlight is a claim about why a
document came back, and marking `Bash` when `bash` is on the stop-word list would answer that
question confidently and wrongly. The one filter not applied is `max_query_terms`, whose choice of
"best 32" depends on a `tf * idf` ordering there is no reason to recompute; the surplus terms are
ones the hit genuinely contains and the `1 / (1 + doc_freq)` weighting sorts them last anyway.

**Sorting.** `--similar-to --sort newest` means "documents similar to this one, most recent
first", which is a legitimate request, and it works because the similarity clause is an ordinary
`BooleanQuery` of `TermQuery`s: those skip scoring when the collector disables it, where a
`MoreLikeThisQuery` would have returned an error instead of an answer. The scores a time order
discards were being zeroed anyway — `search()` replaces every hit's score with `0.0` under one.

**The HTTP API deliberately does not carry it.** Three reasons. `dto::SearchBody` exists to be
Elastic Search UI's `RequestState`, and Search UI has no notion of "documents like this one", so a
`similar_to` key would sit outside the envelope that module exists to satisfy. Resolving a
reference has its own ambiguity error, which wants its own status shape and, honestly, its own
route — `GET /api/similar/{ref}` returning the same hit envelope — rather than a smuggled search
parameter. And the bundled UI has no affordance to trigger it, so it would be dead,
unauthenticated surface on a local port that already serves every secret in every transcript.
`Params::reject_unknown(SEARCH_PARAMS)` already turns `?similar_to=…` into a 400 listing what the
endpoint does take, which is the right answer for free; `similar_to_is_not_a_search_parameter`
pins it so the decision cannot decay into an oversight. Revisit when the eval numbers justify a
route.

**`STATE_VERSION` does not move.** Nothing here changes what is written to the index.

### Retrieval evaluation

`tests/eval/` is a test target, not a module of `src/`: an eval harness is not production code,
and everything it needs — `build_schema`, `tokenizer::register`, `parse_whole`, `doc_to_json`,
`search`, `facets` — is already public. Run it with `cargo test --test eval`; add `-- --nocapture`
to see the tables. Every run also writes `target/eval/report.md`, `target/eval/ablation.md`,
`target/eval/similar.md`, `target/eval/facets.md`, `target/eval/hits.md`,
`target/eval/similar-hits.md` and `target/eval/corpus.md`, which are the artifacts a pull request
pastes.

**[`EVAL.md`](EVAL.md) is the record; this section is the design.** The tables below are a summary
kept here because a design document that describes a harness and never says what it found is half
a document. The full output — every table verbatim, at a named commit, with the reproduction
command and the reading of each number — lives in `EVAL.md`, which is regenerated by splicing
`target/eval/*.md` in rather than by retyping. When a number here and a number there disagree,
`EVAL.md` is right.

**What it is for.** Ranking changes here are argued from first principles — an analyzer that
splits identifiers, a heading boost, a discounted context header — and until this existed there
was no way to tell an improvement from a regression except by trying a query and liking the
answer. The harness turns "this should help" into a number per query class, and, more
importantly, turns "this quietly stopped working" into a failing build.

**The five classes**, which are the issue's and not this file's invention:

| class | what it asks | example |
| --- | --- | --- |
| identifier | half a name, a path, a flag, a hash | `snippet` → `SnippetGenerator`, `iserror` → `is_error` |
| boundary | a word that is prose in one place and code in another | `tokenizer` in a sentence and in a ```rust fence |
| paraphrase | the words a person uses, not the ones the transcript used | `build fails` against a session whose bodies never say either word |
| filtered | a query plus `--project` / `--lang` / `--program` / `--since` / `--sidechains-only` | `release` with `--program cargo` |
| aggregation | a question whose answer is a distribution | "what errors did we see" |

The aggregation class is **recorded and never scored**. "What errors did we see" has a facet
table for an answer and no defensible top-k, so scoring one as a ranking would book a modelling
mistake as a retrieval miss and push whoever reads the table toward tuning the ranker to fix it.
Those rows are checked against `search::facets` instead — the expected buckets must come back
non-empty — and the class table prints em dashes for them rather than `0.000`, because a zero and
"deliberately not measured" are different claims that look identical in a column of numbers.

**The corpus is synthetic and checked in**: six transcripts under `tests/fixtures/eval/`, 65
documents, three projects, one of them a real subagent file at `subagents/agent-<id>.jsonl`. They
go through `parse_whole` and the real schema rather than being hand-built `Doc` values, because
the boundary class only exists if `markdown::split` is the thing that decided where a word landed
— and the same argument covers `code_langs` (which `--lang` filters on), `bash_cmd` (`--program`),
`turn_seq` / `turn_prompt`, and `SessionInfo::title` / `first_prompt`, which is what makes the
`context_text` header real rather than staged. Timestamps are frozen constants and every date
filter in the fixture is absolute: `date_range()` resolves `7d` against `chrono::Utc::now()`, so a
relative filter over a frozen corpus silently matches a shrinking set every day until the row
means nothing. A test that starts failing in a week is not deterministic, and neither is one that
starts passing vacuously.

**A document is referenced as `"{session_id}:{agent_id|-}:{seq}"`** — `Doc::doc_id` minus its
`file_tag`. `file_tag` is `fnv1a` of the *absolute* source path, so a fixture naming raw
`doc_id`s would pass on the machine that wrote it and fail in CI and in every other checkout. All
three components of the reference are STORED, so the run closure derives it straight off a `Hit`
with no side map.

**The fixture** is `tests/fixtures/eval_queries.json`: 39 queries, at least six per class, each
with an id, a class, a shape, the query string, a `filters` object that deserializes directly into
`search::Filters` (so a row is exactly as expressive as the CLI and cannot drift from it), graded
relevance 0-3, a provenance and a note. Provenance is honest and mandatory: every row here says
`synthesised`, because the corpus is. Nothing in this repository was drawn from a real query log,
and a row written against one of the redacted real slices in `tests/fixtures/` would say `real`.

`Fixture::validate` refuses to run at all unless every graded reference resolves to a document in
the corpus. That is the failure mode the whole harness is written around: renumber the corpus and,
without the check, every reference goes stale, every query scores zero, and the result is a tidy
table of `0.000` that reads exactly like a ranking regression. The floors are the other half —
every scored query must retrieve at least one relevant document, and each class must clear a
per-class recall / MRR / nDCG floor set comfortably below where main sits today.

**Metrics: recall@10, MRR and nDCG@10, per class.** Recall is the number the floors are written
against, because it is the one a ranking change cannot flatter. nDCG uses gain `2^g - 1` and
discount `log2(i + 2)` against the ideal ordering of that query's graded set. The core takes a
closure — `&dyn Fn(&EvalQuery) -> anyhow::Result<Vec<String>>`, query in, ranked references out —
and that is load-bearing rather than stylistic: the harness already scores three configurations
against the same corpus, the same fixture and the same arithmetic, and issue #26's
`MoreLikeThis` will be a fourth. A metric implementation that called `search()` itself could
measure exactly one configuration forever.

**Comparing two configurations** is `Report::diff(&before, &after)`, which asserts the two were
evaluated from the same fixture at the same cutoff and then prints class / metric / before /
after / Δ. Tables are built to be diffed and not admired: classes in a fixed order, queries in
fixture order, three decimals, fixed column widths, and never a timing — `SearchResponse::elapsed_ms`
is wall-clock and would make a committed snapshot fail on a slow runner.

**The `context_text` ablation.** `Corpus::index` takes a `Variant`, and the off-switch is one
line: `doc_to_json` returns a `serde_json::Value` and `context_text` is one top-level key of it,
so the ablated arm removes that key before `TantivyDocument::parse_json`. The two indexes are then
byte-identical in every other field — same schema, same analyzers, same single segment, same
`project`, `git_branch` and timestamps — so the filtered class stays comparable across arms and
only the `context_text` posting lists differ. Two alternatives look equivalent and are not, and
are named here so nobody re-tries them:

- `doc_to_json(doc, None, ..)` drops the title and the first prompt but still composes a header
  from the project basename, the branch and the turn prompt. That measures the `sessions.json`
  half of the feature, not the feature. It is kept as a deliberate third arm, labelled as such.
- clearing `doc.project` / `doc.git_branch` / `doc.turn_prompt` on a cloned `Doc` empties the
  `project` and `git_branch` *schema* fields too, which breaks `--project` and `--branch` on the
  ablated arm and stops the two arms being comparable at all.

**The recall ceiling, which is the first thing to know about every table below.** A scored query
is *at the ceiling* when it matched fewer documents than the cutoff and every document it matched
is graded relevant: `found == relevant == the whole match set`, so recall is `1.000` by arithmetic
and no ranking could have produced anything else. 17 of the 33 scored queries are in that state —
7 of the 8 identifier rows, 7 of the 10 filtered rows — which is why the harness prints a
`ceiling` column beside the metrics and daggers those rows in the per-query appendix. The column
is not decoration and not a metric: it is the difference between "identifier retrieval is perfect"
and "the identifier fixture graded whatever the query matched", and only one of those two is what
`recall@10 = 1.000` in that class means. `metrics::at_recall_ceiling` is the definition.

The same reasoning applies to MRR, which is `1.000` in every scored class. The top hit is graded
relevant on all 33 queries; a metric pinned at its maximum can register a collapse and nothing
else. Keep it for that and never argue *for* a change from it.

**nDCG@10 has the most headroom, and it is the one to reason from — but it is not free of the
same problem.** A row whose run returned exactly its graded set with every grade equal has an nDCG
of `1.000` under any permutation: DCG and IDCG are then the same sum, so no reordering the ranker
could produce would move it. `metrics::at_ndcg_ceiling` is the definition and the `pinned` column
beside `nDCG@10` is the count. Eight of the 33 scored rows are in that state, and **five of them
are in the filtered class** — so `filtered nDCG@10 = 0.981` is a mean over ten rows of which half
have exactly one reachable value, and a delta on that class is spread over the five movable rows
rather than over all ten. Read the filtered nDCG with that in mind; the other three classes have
at most two pinned rows each.

**The numbers on main today**, at k = 10 over 33 scored queries (`target/eval/report.md`, and
[`EVAL.md` §1](EVAL.md#1-baseline--issue-21) for the reading):

| class | queries | ceiling | recall@10 | MRR | nDCG@10 | pinned |
| --- | --- | --- | --- | --- | --- | --- |
| identifier | 8 | 7 | 1.000 | 1.000 | 0.928 | 2 |
| boundary | 8 | 2 | 0.913 | 1.000 | 0.774 | 1 |
| paraphrase | 7 | 1 | 0.844 | 1.000 | 0.782 | 0 |
| filtered | 10 | 7 | 1.000 | 1.000 | 0.981 | 5 |
| aggregation | 6 | 0 | — | — | — | 0 |
| overall | 39 | 17 | 0.946 | 1.000 | 0.876 | 8 |

and the header ablation, which is issue #23's outstanding acceptance box
([`EVAL.md` §2](EVAL.md#2-context-header-onoff--issue-23)):

| class | ceiling off → on | recall@10 off | recall@10 on | Δ | nDCG@10 off | nDCG@10 on | Δ |
| --- | --- | --- | --- | --- | --- | --- | --- |
| identifier | 8 → 7 | 1.000 | 1.000 | 0.000 | 0.928 | 0.928 | 0.000 |
| boundary | 1 → 2 | 0.875 | 0.913 | +0.039 | 0.767 | 0.774 | +0.007 |
| paraphrase | 0 → 1 | 0.206 | 0.844 | **+0.638** | 0.255 | 0.782 | **+0.526** |
| filtered | 9 → 7 | 1.000 | 1.000 | 0.000 | 0.977 | 0.981 | +0.004 |
| overall | 18 → 17 | 0.801 | 0.946 | +0.145 | 0.761 | 0.876 | +0.114 |

**The identifier class did not regress, and the ceiling column says how strong that claim is.**
Its nDCG is `0.927802467` on both arms — identical to nine decimal places, not merely to the three
the table prints — so `CONTEXT_BOOST = 0.3` did not reorder a single graded identifier document.
Its *recall* column, though, was at 8-of-8 ceiling with the header off and could not have fallen
without a graded document leaving the matched set entirely, so "identifier recall 1.000 → 1.000"
is a true statement about a number with one reachable value. The supportable claim is the nDCG
one. The same reading applies to filtered.

**The paraphrase result is the only one in this table that carries on its own weight**, and it
carries because of its shape rather than its size: eleven of the thirty-three scored queries moved
at all, seven of them paraphrase rows, and `para-build-fails` went from retrieving *nothing* to
0.750 recall. That is a mechanism demonstrated — documents unreachable by any word a person would
type became reachable — and not an effect size estimated. Boundary's `+0.007` nDCG and filtered's
`+0.004` are one and two queries respectively on classes of eight and ten, with no repeated trials
and no variance estimate anywhere in this harness; they are consistent with the header being
neutral for those classes and must not be cited as gains.

The third arm — header without the session row — lands at paraphrase recall 0.388, so on this
corpus roughly two thirds of the header's benefit comes from `sessions.json`'s title and first
prompt and one third from the per-document pieces the parser already had. No class regresses,
which is what the harness asserts rather than merely printing.

**The MoreLikeThis arm (issue #26)**, which is the fourth configuration the closure-shaped metric
core was built for, and the one that needs its protocol stated before its numbers are read.

A query fixture cannot score a document-seeded search as it stands, so `tests/eval/similar.rs`
defines a bridge and writes it down rather than hiding it. Every ranked row gets a **seed**: the
document that row graded highest, ties broken by reference order so the choice is deterministic
and does not depend on what the comparison arm returned. The similarity arm then discards the
query string entirely, searches by `--similar-to <seed>`, and keeps the row's filters. The graded
set is **narrowed** to what that arm could possibly return — the seed and every other document of
the seed's turn are dropped, because the shipped default excludes the source turn — and a row
whose graded set was *only* the seed's turn is dropped from the arm and counted. Both arms are
scored over that same narrowed fixture **and search the same narrowed candidate set**: the text
arm has the seed's turn removed from its hits before the cut at k, exactly as the similarity arm
has it removed by a `MustNot`. Without that second half the text arm spent top-k slots on
documents the protocol had already deleted from the graded set while the similarity arm
structurally could not, which understated the text column and flattered the comparison.

27 of the 39 rows survive that narrowing. At k = 10 (`target/eval/similar.md`):

| class | scored | recall@10 text | recall@10 MLT | Δ | nDCG@10 text | nDCG@10 MLT |
| --- | --- | --- | --- | --- | --- | --- |
| identifier | 6 | 1.000 | 0.361 | −0.639 | 0.903 | 0.210 |
| boundary | 7 | 0.914 | 0.570 | −0.344 | 0.793 | 0.526 |
| paraphrase | 6 | 1.000 | 0.589 | −0.411 | 0.865 | 0.334 |
| filtered | 8 | 1.000 | 0.875 | −0.125 | 1.000 | 0.643 |
| overall | 27 | 0.978 | 0.618 | −0.360 | 0.895 | 0.448 |

**Read that as a baseline, not as a verdict, and read only the recall column.** Three of the four
classes are asking this arm a question it is not for. `identifier` and `boundary` are analyzer
tests over a typed query, and there is no typed query here at all: what they measure is "having
found one document about `open_or_create`, are the others nearby", which is a coincidence rather
than a design goal, and the −0.639 is exactly what one should expect. `filtered` transfers
cleanly, and its 0.875 is the property that actually matters — the filters still AND on top of
the similarity clause. `paraphrase` is the fairest of the four, because it is the class where the
query words are *not* the transcript's words, which is the situation similarity exists for; MLT
loses 0.411 recall there against a text query that has the `context_text` header working for it.
MRR and nDCG are printed for symmetry and mean little: a similarity search has no notion of "the
answer" that belongs at rank 1.

The number to beat, for anything that proposes to rerank these results, is **paraphrase recall
0.589 and overall recall 0.618 at k = 10** on this fixture and this protocol — and any proposal
that changes the protocol has to say so, because the protocol is doing at least as much work as
the ranker.

One row returns nothing at all (`filtered-lang-python`: a `--lang python` filter ANDed with a
similarity clause whose seed shares no surviving term with either python fence), which is the
zero-clause outcome the `total == 0` warning exists for, arriving in the report as an honest 0.000
rather than as an error.

**What a 65-document synthetic corpus cannot tell you**, stated plainly because the numbers above
will be quoted:

- **Nothing about corpus-wide fieldnorm effects.** BM25 length normalisation is relative to
  `avgdl` over the whole index; at this size `avgdl` is dominated by whichever six transcripts are
  checked in. The header's effect on scoring *across a real corpus of thousands of sessions* is
  not measured here and cannot be.
- **Nothing about IDF at scale.** A term's document frequency in 65 documents is not its document
  frequency in 65,000, and the `CONTEXT_BOOST = 0.3` discount was chosen against the former.
- **Little about ranking, as opposed to matching.** MRR is 1.000 in every class, which is not a
  triumph: with a corpus this small and queries this specific, the top hit is almost always
  relevant. MRR is kept because it will stop being 1.000 the moment something breaks, not because
  its current value says anything. Recall is half-saturated for the same reason — see the ceiling
  paragraph above; 17 of 33 scored queries have a recall no ranker could have changed.
- **Nothing about the two aggregation rows with a one-bucket answer.** `agg-which-projects-errored`
  returns a single bucket over nine matched documents and `agg-which-languages-were-quoted` a
  single bucket over twenty, so both pass their "the named buckets come back non-empty" assertion
  without exercising the claim they were written to make — the `code_lang` row's whole point is
  that one document holding a rust fence and a bash fence is two bucket increments, and the corpus
  never produces that document. `target/eval/facets.md` prints the bucket tables so the gap is
  visible rather than inferred. Closing it means adding transcripts, which moves every number
  above.
- **Nothing about precision.** Only graded documents count, and a query that returns ten answers
  where three are graded is scored the same as one that returns three. The identifier row for
  `src/index.rs` notes exactly such a case: a path is three ANDed terms rather than a phrase, so
  `src/lib.rs` beside the word `index` also matches.
- **Only what was planted.** Every mechanism the corpus exercises is one someone deliberately put
  there. A retrieval failure mode nobody thought of is not in it, which is the standing argument
  for adding rows written against real slices as they are redacted.
- **Almost nothing about the similarity tuning.** `SIMILAR_MAX_DOC_FREQUENCY_FLOOR = 50` means
  that on 65 documents *nothing* is ever cut for being too common, so the upper document-frequency
  bound — the parameter that does the most work on a real index, and the one that answers the
  `context_text` interaction — is entirely inert in these numbers. `min_doc_frequency = 3` is
  correspondingly harsh at this size. Both are corpus-relative by design and both need a real
  index to be judged.

### Optional features: `http-api` and `web-ui`

Two cargo features, neither in `default`: `http-api` adds `session-search serve`, a JSON API over
the read operations this file already pins, and `web-ui` (which implies it) adds the static
frontend, `include_str!`'d into the binary. They are opt-in **at build time** because the server
binds a port and hands out a verbatim record of everything that was typed, secrets included; a
default build links no `axum`, no `tokio`, and has no `serve` in `--help`.

The wire contract — endpoints, the Elastic Search UI request/response envelope, the filter-field
mapping, the `dto.rs`/`mod.rs` seam, and the frontend's DOM and CSS contracts — is
[`WEB-UI.md`](WEB-UI.md), and is not repeated here. What the features add to the types pinned
above is only this, in `search.rs`, shared by both front ends:

```rust
/// Hit order. Relevance is meaningless for a filter-only browse — every hit scores the same —
/// which is exactly when time order earns its keep.
pub enum SortBy { Relevance /* default */, Newest, Oldest }

/// Which stored body a `Hit`'s snippet was cut from. One query spans `text`, `code`,
/// `tool_output` and `thinking`, and the four read as different claims — what a turn said, a
/// snippet it quoted, what a command printed, what the model reasoned privately. A renderer
/// that labels them all the same way tells the reader something untrue about what matched.
/// `Text` also covers the no-highlight fallback's `body`, which is the same claim.
pub enum SnippetSource { Text, Code, ToolOutput, Thinking }

pub struct SearchRequest { /* … as above … */ pub sort: SortBy }
pub struct Hit { pub doc: Doc, pub score: f32, pub snippet: String,
                 pub snippet_field: SnippetSource,
                 /// Byte ranges into `snippet` naming what each marker pair wraps.
                 pub snippet_marks: Vec<std::ops::Range<usize>> }

/// The marker either side of a matched span in `Hit::snippet`, for a consumer that renders the
/// snippet as-is — the terminal one in `format.rs`.
///
/// A consumer that *re-marks* the snippet reads `Hit::snippet_marks` instead and must not split
/// on this: transcript bodies contain `**` of their own, splitting cannot tell those from the
/// highlighter's, and the result is emphasis on words the query never matched.
pub const HIGHLIGHT: &str = "**";
```

Under `Newest`/`Oldest` every hit carries `score: 0.0`: the collector orders by the timestamp fast
field and a timestamp is not a relevance score, so reporting one would be a lie the caller cannot
check. `SortBy::Relevance` is the default everywhere, so nothing that predates this sees a change.

A document with no timestamp is still returned under a time order — Tantivy sorts on `Option<T>`
and puts `None` last in both directions — so `total` keeps matching what paging can reach. Every
transcript record carries a timestamp anyway; it is pinned by a test because "the count says 40
and you can only page to 37" is the kind of quiet arithmetic lie this index does not tell.


## CLI surface

```
session-search index [--full] [--root DIR]... [--index DIR] [--jobs N] [--include-thinking]
                     [--no-spilled-results]
session-search search <QUERY> [FILTERS] [--facets f1,f2] [--context N|turn|skeleton]
                              [--group-by-turn]
                              [--limit N] [--offset N] [--json] [--no-refresh]
                              [--include-thinking] [--sort relevance|newest|oldest]
                              [--similar-to REF] [--similar-in text,code,tool_output,thinking]
                              [--include-source]
session-search facets <FIELD> [--query Q] [FILTERS] [--top N] [--json] [--no-refresh]
session-search show <SESSION_ID> [--agent AGENT_ID] [--around REF|SEQ] [--turn] [--skeleton]
                                 [--before N] [--after N] [--limit N] [--json] [--no-refresh]
session-search sessions [FILTERS] [--limit N] [--json] [--no-refresh]
session-search stats [--json]
session-search serve [--host ADDR] [--port PORT] [--cors ORIGIN]... [--refresh-secs N]
                     [--no-refresh]                                   [feature http-api]

FILTERS: -p/--project P  -t/--tool T  --tool-input k=v  --tool-output TEXT  --program NAME
         --lang LANG  --branch B  --model M
         --role R  --kind message|tool_call  --session S  --agent-type A
         --since D  --until D  --errors-only  --no-sidechains  --sidechains-only
```

Global: `--index DIR` (`$SESSION_SEARCH_INDEX`), `-v/--verbose`, `--no-color` (`$NO_COLOR`).

**Turn-shaped windows.** `search --context turn` and `show --around ... --turn` snap the window
to the hit's enclosing turn instead of counting documents outwards. A fixed `N` is the wrong
shape for this data twice over: inside a forty-call turn it shows neighbouring `Bash` calls and
never the prompt that explains them, and on a short turn it drags in the turns either side.
`--turn` is meaningless without `--around` and clap requires it. Both are capped by document
count — `cli::TURN_WINDOW_LIMIT` for `search`, `show`'s own `--limit` — because a turn is
unbounded (rule 3 makes a whole sidechain transcript one turn) and `TopDocs` preallocates
whatever it is handed. What the cap leaves out is always reported, never dropped silently:
`turn #12 · 200 of 347 docs` in the human rendering, `context_turn` / `turn` in the JSON.

`search --context skeleton` and `show --turn --skeleton` fetch that same window and render it as
one line per document — call signatures with no output, except the first line of a failed one —
and `search --group-by-turn` collapses hits that share a turn. See **Turn skeletons** for both,
including what `--group-by-turn` does to `offset` and to `total`.

**Document references.** `search --similar-to` and `show --around` take the same four shapes —
`SESSION:SEQ`, `SESSION:AGENT:SEQ` (`-` for the main transcript), a record uuid or `tool_use_id`,
or a `doc_id` — each by unambiguous prefix, resolved by one function so the two commands cannot
disagree about what `abc123` means. An ambiguous prefix names its candidates and stops; it never
picks the first. See **Similarity** for the grammar table and for what `--similar-to` does with
what it resolves.

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

The foundation for a third front end is in place and it is not new code, it is code that stopped
being written twice: `sessions.rs` holds the one `SessionMatcher` and the one unanswerable-filter
list, `search::Edge`/`when_ms` hold the one date-edge resolution, `format::skeleton_json` /
`turn_json` hold the one skeleton envelope, and `SearchResponse::warnings` carries what used to
reach stderr alone. An MCP tool has no stderr a caller reads at all, which makes that last one
the load-bearing part: `tracing` on a stdio transport goes nowhere the agent can see.

Two traps recorded now: rmcp's README says `schemars = "0.8"` and is **wrong** (it is `^1.0`);
and stdio transport owns stdout, so `tracing` must write to **stderr** and color must be off.

## Conventions

- Errors: `thiserror` enums in library modules, `anyhow` at the CLI boundary. A malformed line
  is a counted `ParseError`, never a hard failure of the run.
- Logging: `tracing`, subscriber writes to **stderr**, `-v` raises the level.
- Rust 2024 edition. Keep `cargo clippy` clean; run `cargo fmt`.
- Tests: unit tests beside the code; fixtures in `tests/fixtures/`; `insta` for snapshots with
  UUIDs, absolute paths and timestamps redacted.
