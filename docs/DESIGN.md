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
  Assistant **thinking is stored but not indexed** — `--include-thinking` opts in, default off.
- **Incremental one-shot indexing** keyed on (size, mtime, byte offset) per file.
  `search` auto-refreshes unless `--no-refresh`.

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
            .set_tokenizer("default")
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

## Layout

```
src/
  lib.rs         re-exports; `pub mod` declarations only
  model.rs       raw serde types for transcript records          [Foundation]
  discovery.rs   locate roots, enumerate transcripts + sidechains [Foundation]
  parse.rs       records -> Vec<Doc>                              [Foundation]
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
<index>/state.json      { "files": { "<abs path>": {size, mtime_ms, byte_offset, docs} }, "version": 1 }
<index>/sessions.json   { "<session_id>[:<agent_id>]": SessionInfo }
```

`sessions.json` exists because a session's title arrives in a `summary` sidecar record that may
be appended long after the messages it titles. Keeping session metadata out of Tantivy means a
late title update is a cheap JSON rewrite instead of a doc rebuild.

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
    pub doc_id: String,          // "<session_id>:<agent_id|->:<seq>" — unique, delete key
    pub kind: DocKind,
    pub source_path: String,     // absolute; delete-by-term key for re-index
    pub seq: u64,                // 0-based record order within the file
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
    pub is_error: bool,
    pub is_sidechain: bool,
    pub is_meta: bool,           // compaction summaries, meta turns — excluded from "human prompt"
    pub entrypoint: Option<String>,
    pub permission_mode: Option<String>,
    pub version: Option<String>,
    pub slug: Option<String>,
    pub text: String,            // the indexed body
    pub thinking: Option<String>,// stored, indexed only with include_thinking
    pub raw: String,             // original JSONL line
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
    pub messages: u64,
    pub tool_calls: u64,
    pub first_prompt: Option<String>,
}

pub struct ParseOutput { pub docs: Vec<Doc>, pub session: SessionInfo, pub errors: Vec<ParseError> }

/// Parse a transcript. `from_offset` supports incremental tailing; `seq_base` continues
/// numbering. Returns the byte offset of the end of the last COMPLETE line — a partial
/// trailing line must be left unconsumed so the next run re-reads it.
pub fn parse_file(path: &Path, from_offset: u64, seq_base: u64, opts: &ParseOptions)
    -> anyhow::Result<(ParseOutput, u64)>;

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
pub fn doc_to_json(doc: &Doc, include_thinking: bool) -> serde_json::Value; // feed to parse_json
```

Field names and options:

| field | type / options |
| --- | --- |
| `doc_id`, `source_path`, `uuid`, `parent_uuid`, `tool_use_id` | `STRING \| STORED` |
| `session_id`, `agent_id`, `agent_type`, `project`, `git_branch`, `role`, `kind`, `model`, `tool_name`, `entrypoint`, `permission_mode`, `version`, `slug` | `STRING \| STORED \| FAST` |
| `project_facet` | `FacetOptions` — hierarchical, `/home/user/session-search` |
| `tool_input` | JSON, indexed + `set_fast(Some("raw"))` + `set_expand_dots_enabled()` + stored |
| `text` | `TEXT \| STORED` |
| `thinking` | `TEXT \| STORED` (only populated when `include_thinking`) |
| `timestamp` | `DATE \| STORED \| FAST` (`INDEXED` too) |
| `seq` | `U64 \| STORED \| FAST \| INDEXED` |
| `is_error`, `is_sidechain`, `is_meta` | `U64 \| FAST \| INDEXED` (0/1) |
| `raw` | `STORED` only |

`index.rs`:

```rust
pub struct IndexOptions {
    pub full: bool, pub jobs: Option<usize>, pub include_thinking: bool,
    pub max_text_bytes: usize,   // default 32 * 1024
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

Incremental rules:
- Watermark per file: `{size, mtime_ms, byte_offset, docs}`.
- Unchanged `size` **and** `mtime_ms` → skip entirely.
- Grew → seek to `byte_offset`, parse the tail only, `seq_base` = recorded `docs`.
- Shrank, or mtime went backwards, or `--full` → `delete_term(source_path)` and reparse whole.
- One `commit()` per run. Parse with `rayon` across files; a **single** writer consumes.
- Never `memmap2` — these files are appended live and truncation raises an uncatchable SIGBUS.

`search.rs`:

```rust
// Derive clap::Args + Deserialize + (later) JsonSchema on this — one struct, both front ends.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub project: Option<String>, pub tool: Vec<String>, pub tool_input: Vec<String>, // "key=value"
    pub branch: Option<String>, pub model: Option<String>, pub role: Option<String>,
    pub kind: Option<String>, pub session: Option<String>, pub agent_type: Option<String>,
    pub since: Option<String>, pub until: Option<String>,   // RFC3339 or YYYY-MM-DD or "7d"
    pub errors_only: bool, pub no_sidechains: bool, pub sidechains_only: bool,
}
pub struct SearchRequest {
    pub query: Option<String>, pub filters: Filters,
    pub limit: usize, pub offset: usize,
    pub facets: Vec<String>,     // "tool_name", "project", or any JSON path e.g. "tool_input.file_path"
    pub facet_top: usize, pub snippet_chars: usize, pub include_thinking: bool,
}
pub struct FacetCount { pub value: String, pub count: u64 }
pub struct Hit { pub doc: Doc, pub score: f32, pub snippet: String }
pub struct SearchResponse {
    pub hits: Vec<Hit>, pub total: usize,
    pub facets: BTreeMap<String, Vec<FacetCount>>, pub elapsed_ms: u128,
}
pub fn search(index: &tantivy::Index, f: &Fields, req: &SearchRequest) -> anyhow::Result<SearchResponse>;
pub fn facets(index: &tantivy::Index, f: &Fields, field: &str, req: &SearchRequest)
    -> anyhow::Result<Vec<FacetCount>>;
```

Query semantics: the free-text query goes through `QueryParser` over `text` (+ `thinking` when
opted in, + `tool_input`), so phrases, booleans, `field:value` and fuzzy all work. Filters are
ANDed on top as term/range queries. `--tool-input k=v` becomes a term query on
`tool_input.k` for `v`. `project` matches by prefix so `-p ~/code` catches subdirectories.

`context.rs`:

```rust
pub fn around(index: &tantivy::Index, f: &Fields, session_id: &str, agent_id: Option<&str>,
              seq: u64, before: usize, after: usize) -> anyhow::Result<Vec<Doc>>;
pub fn session(index: &tantivy::Index, f: &Fields, session_id: &str, agent_id: Option<&str>,
               limit: usize) -> anyhow::Result<Vec<Doc>>;
```

`format.rs`:

```rust
pub struct OutputOpts { pub json: bool, pub color: bool, pub context: usize, pub width: usize }
pub fn search_results(w: &mut impl Write, r: &SearchResponse, o: &OutputOpts) -> anyhow::Result<()>;
pub fn facet_list(w: &mut impl Write, field: &str, c: &[FacetCount], o: &OutputOpts) -> anyhow::Result<()>;
pub fn session_view(w: &mut impl Write, docs: &[Doc], o: &OutputOpts) -> anyhow::Result<()>;
pub fn session_list(w: &mut impl Write, s: &[SessionInfo], o: &OutputOpts) -> anyhow::Result<()>;
pub fn stats(w: &mut impl Write, s: &IndexStats, o: &OutputOpts) -> anyhow::Result<()>;
```

## CLI surface

```
session-search index [--full] [--root DIR]... [--index DIR] [--jobs N] [--include-thinking]
session-search search <QUERY> [FILTERS] [--facets f1,f2] [--context N]
                              [--limit N] [--offset N] [--json] [--no-refresh]
session-search facets <FIELD> [--query Q] [FILTERS] [--top N] [--json]
session-search show <SESSION_ID> [--agent AGENT_ID] [--around UUID|SEQ]
                                 [--before N] [--after N] [--limit N] [--json]
session-search sessions [FILTERS] [--limit N] [--json]
session-search stats [--json]

FILTERS: -p/--project P  -t/--tool T  --tool-input k=v  --branch B  --model M
         --role R  --kind message|tool_call  --session S  --agent-type A
         --since D  --until D  --errors-only  --no-sidechains  --sidechains-only
```

Global: `--index DIR` (`$SESSION_SEARCH_INDEX`), `-v/--verbose`, `--no-color` (`$NO_COLOR`).

**`FIELD` for `facets`** is any fast field name (`tool_name`, `project`, `model`,
`git_branch`, `role`, `kind`, `agent_type`, `entrypoint`) **or any JSON path** such as
`tool_input.file_path`, `tool_input.command`, `tool_input.pattern`.

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
