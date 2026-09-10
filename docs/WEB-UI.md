# `http-api` and `web-ui` — design note and wire contract

Two optional, **default-off** cargo features:

| feature   | what it adds                                                              |
| --------- | ------------------------------------------------------------------------- |
| `http-api`| `session-search serve` — a JSON API over the read operations the CLI has    |
| `web-ui`  | the static single-page frontend, embedded in the binary, served by the same |

`web-ui` implies `http-api`. Neither is in `default`, so an ordinary
`cargo build` produces the same binary it always did, with no `axum`, no `tokio`,
and no `serve` subcommand in `--help`.

```
session-search serve [--host ADDR] [--port PORT] [--cors ORIGIN]... [--refresh-secs N]
                     [--no-refresh]
```

Default bind is `127.0.0.1:7777`. **A transcript index is a verbatim record of everything
you and the model typed, secrets included.** The server has no authentication, so binding
anything but a loopback address publishes it to the network; that is allowed (it is your
machine) and it is said loudly, once, at startup.

## Layout

```
src/api/mod.rs     server bootstrap, router, handlers, CORS, static assets
src/api/dto.rs     wire types: query-string decoding, Search UI envelope, JSON shaping
web/index.html     the shell — ids below are the contract with app.js
web/styles.css     design tokens and every class listed under "CSS contract"
web/dom.js         shared DOM primitives (no imports of its own)
web/markdown.js    a deliberately small, escape-first markdown subset
web/tools.js       one renderer per known tool, registry + fallbacks
web/app.js         state, the API client, facets, results, expansion, routing
```

The `web/` files are `include_str!`'d into the binary at compile time, so a release build
is still one file with nothing to install beside it.

## Why "Search UI compatible"

[Elastic Search UI](https://github.com/elastic/search-ui) is the obvious thing someone
points at a search index they did not write. Its connector interface is one function —
`onSearch(requestState, queryConfig) -> responseState` — so being compatible with it means
accepting a `RequestState`-shaped body and returning a `ResponseState`-shaped envelope.
Doing that costs us nothing we would not have built anyway, and it means a third-party
frontend is a twenty-line connector:

```js
const connector = {
  onSearch: (state, queryConfig) =>
    fetch("http://127.0.0.1:7777/api/search", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ ...state, ...queryConfig }),
    }).then((r) => r.json()),
  onAutocomplete: async () => ({}),
  onResultClick: () => {},
};
```

The bundled UI uses the same endpoint, so the compatible path is the *only* path — there is
no second, private API to drift away from it.

## Endpoints

Every response is `application/json`. Every failure is

```json
{ "error": { "status": 400, "message": "unknown filter field \"toolname\"; ..." } }
```

with the matching HTTP status. `400` means the request was wrong and says how; `500` means
the index or the disk was, and the message is the `anyhow` chain.

### `POST /api/search` — Search UI `RequestState` in, `ResponseState` out

Request body (every key optional):

```jsonc
{
  "searchTerm": "memmap SIGBUS",       // free text: words, "phrases", AND/OR/NOT, field:value
  "current": 1,                        // 1-based page number
  "resultsPerPage": 20,
  "filters": [                         // Search UI form...
    { "field": "tool_name", "values": ["Bash", "Read"], "type": "any" },
    { "field": "timestamp", "values": [{ "from": "2026-01-01", "to": "now" }] }
  ],
  "facets": { "tool_name": { "type": "value", "size": 15 } },
  "sortList": [{ "field": "timestamp", "direction": "desc" }],
  "includeThinking": false,            // native extras, camelCase like the rest
  "snippetChars": 240
}
```

`filters` also accepts the **native** object form — exactly `search::Filters` as the CLI
flags spell it — because that is what the bundled UI already has in hand:

```jsonc
{ "filters": { "tool": ["Bash"], "project": "~/code", "errors_only": true } }
```

Filter field names in the Search UI form map onto `Filters` like this. Anything else is a
`400` naming the field, never a silent no-op:

| `field`                    | maps to                | notes                                    |
| -------------------------- | ---------------------- | ---------------------------------------- |
| `tool_name` / `tool`       | `tool`                 | repeatable, ORed                         |
| `tool_output`              | `tool_output`          | repeatable, ANDed phrases                |
| `tool_input.<path>`        | `tool_input`           | `values` become `<path>=<value>`         |
| `code_lang` / `lang`       | `lang`                 | repeatable, ORed                         |
| `bash_cmd.program` / `program` | `program`          | repeatable, ORed                         |
| `project`                  | `project`              | prefix match; one value only             |
| `model` `role` `kind`      | same                   | one value only                           |
| `agent_type` `session_id`  | `agent_type` `session` | one value only                           |
| `git_branch` / `branch`    | `branch`               | one value only                           |
| `timestamp`                | `since` / `until`      | range value `{from, to}`                 |
| `thinking_tokens`          | `min_thinking`         | range value `{from}`                     |
| `is_error`                 | `errors_only`          | `[true]`                                 |
| `is_sidechain`             | `sidechains_only` / `no_sidechains` | `[true]` / `[false]`        |

A single-valued field given more than one value is a `400`, not a silent "first one wins".

`sortList` (or `sortField`/`sortDirection`) accepts `timestamp` only, and maps to
`SortBy::Newest` / `SortBy::Oldest`; `""`/`_score`/`relevance` is the default. Any other
field is a `400` — the index has no other sortable ordering to offer.

Response:

```jsonc
{
  "results": [
    {
      "id": { "raw": "9f2c…:-:a41b:118" },
      // `body` is the body as a reader saw it. `text` and `code` are the indexed halves of it,
      // one entry per block, and cannot be reassembled into it — the split drops link
      // destinations and keeps no record of where a fence sat. Render `body`.
      "body": { "raw": "…the memmap panic…" },
      "text": { "raw": ["…"], "snippet": "…the <em>memmap</em> panic…" },
      "code": { "raw": ["…"] },
      "tool_name": { "raw": "Bash" },
      "tool_input": { "raw": { "command": "cargo test" } },
      "timestamp": { "raw": "2026-09-09T19:07:19.248Z" },
      // …one { "raw": … } per stored field…
      "_meta": {
        "id": "9f2c…:-:a41b:118",
        "score": 12.4,
        // text | code | tool_output | thinking — which body it was cut from. Four different
        // claims: what the turn said, a snippet it quoted, what a command printed, what the
        // model reasoned privately. The snippet is attached to that field above, too.
        "snippetField": "text",
        "doc": { /* the whole document, `raw` line excluded — see ApiDoc */ }
      }
    }
  ],
  "totalResults": 431,
  "totalPages": 22,
  "pagingStart": 1,
  "pagingEnd": 20,
  "current": 1,                        // the page and size actually used, echoed back: a
  "resultsPerPage": 20,                // request that said neither still has to say which
  "requestId": "",
  "resultSearchTerm": "memmap SIGBUS",
  "wasSearched": true,
  "facets": {
    "tool_name": [{
      "field": "tool_name",
      "type": "value",
      "data": [{ "value": "Bash", "count": 212 }],
      // this index's own honesty fields, which Search UI ignores and our UI shows:
      "meta": { "matchingDocs": 431, "docsWithValue": 388, "otherDocs": 12,
                "distinct": 41, "searchShaped": false, "hiddenValues": 26 }
    }]
  },
  "info": { "elapsedMs": 7, "sort": "relevance", "limit": 20, "offset": 0, "warnings": [] }
}
```

`_meta.doc` is the field the bundled UI actually renders; the flattened `{raw}` fields exist
so a stock Search UI `Result` component works without knowing anything about us.

`info.warnings` carries what the request asked for that this server ignored — an unknown
top-level key in the body, say. It is never used to paper over a bad filter or a bad sort;
those are errors.

The `snippet` sits on whichever field it was cut from, which is what `_meta.snippetField`
names — so a hit matched in a command's output carries `tool_output.snippet`, not
`text.snippet`. A stock Search UI template reading one fixed field should read `_meta` instead.

`snippet` is **HTML**: the text is escaped first and the matched spans are then wrapped in
`<em>`, which is the convention Elastic's own snippets follow (and what every Search UI
template expects to `dangerouslySetInnerHTML`). The unescaped, `**`-marked form the CLI
prints is in `_meta.doc` only by way of the plain body text. Which spans those are comes from
`Hit::snippet_marks`, recorded where the markers are written: a `**` in the body — every turn
that read a markdown file has some — is otherwise indistinguishable from the highlighter's,
and emphasis on a word the caller did not search for is a wrong answer given confidently.

### `GET /api/search` — the same envelope, for `curl`

Repeatable keys are repeated, not comma-joined (`?tool=Bash&tool=Read`).

```
q, page, size, offset, sort, facets (comma-separated), facet_top, snippet_chars,
include_thinking, project, tool, tool_input, tool_output, lang, program, min_thinking, branch,
model, role, kind, session, agent_type, since, until, errors_only, no_sidechains, sidechains_only
```

An unrecognised parameter is a `400` listing what is accepted. A typo'd filter that
silently widened the search would be worse than an error.

### `GET /api/facets/{field}`

`field` is any fast field (`tool_name`, `project`, `model`, `git_branch`, `role`, `kind`,
`agent_type`, `entrypoint`) or any JSON path such as `tool_input.file_path`. Accepts the
same query parameters as `GET /api/search` plus `top`. Returns one `FacetResult` with the
honesty fields spelled out:

```jsonc
{ "field": "tool_input.command", "values": [{ "value": "cargo test", "count": 4 }],
  "matchingDocs": 8123, "docsWithValue": 2210, "otherDocs": 2106, "distinct": 1980,
  "searchShaped": true, "hiddenValues": 1960 }
```

### `GET /api/sessions`

`?limit=&project=&session=&branch=&model=&agent_type=&since=&until=&no_sidechains=&sidechains_only=`
— the subset of the filters `session-search sessions` honours. Returns

```jsonc
{ "sessions": [ { "session_id": "…", "agent_id": null, "agent_type": null, "title": "…",
                  "slug": "…", "project": "/home/u/p", "git_branch": "main",
                  "source_path": "/…/9f2c….jsonl", "first_ts_ms": 0, "last_ts_ms": 0,
                  "messages": 214, "tool_calls": 96, "first_prompt": "…",
                  "description": null,
                  "key": "9f2c…" } ],   // key = session_id[:agent_id], the display id
  "total": 12,
  "warnings": [] }
```

`total` counts the sessions that matched, not the ones `?limit=` returned. `warnings` says what
the listing could not honour: `sessions.json` holds one row per transcript and no per-message
field, so `?model=` is accepted (it is in the parameter list) and then ignored — and an ignored
filter that quietly returns the unfiltered listing is indistinguishable from a filter that
matched everything.

### `GET /api/sessions/{session_id}` and `GET /api/sessions/{session_id}/around`

Both take `?agent=`, `?source_path=` and `?include_raw=`. The first takes `?limit=`
(default 200) and returns the session in `seq` order; the second takes `?seq=` (required),
`?before=` and `?after=` (default 3) and returns just that window.

`session_id` and `agent` accept an unambiguous **prefix**, exactly as `show` does; an
ambiguous one is a `400` listing the candidates.

```jsonc
{ "session_id": "9f2c…", "agent_id": null, "source_path": "/…/9f2c….jsonl",
  "docs": [ /* ApiDoc */ ], "truncated": false }
```

### `ApiDoc`

`parse::Doc` serialized, with two changes: the `raw` JSONL line is **omitted** unless
`?include_raw=1` (it is the single biggest field and a window of 40 docs does not want 40
copies of it), and `timestamp` is added as an RFC3339 string beside the `timestamp_ms` the
struct already carries. Every other key is the `Doc` field name, unchanged, so
`docs/DESIGN.md` remains the reference for what they mean.

### `GET /api/health`, `GET /api/stats`, `POST /api/reindex`

```jsonc
// health — cheap, no index open
{ "ok": true, "version": "0.1.0", "index_dir": "/home/u/.local/share/session-search",
  "web_ui": true }
// stats — what `session-search stats` prints
{ "docs": 148233, "sessions": 512, "files": 512, "roots": ["/home/u/.claude/projects"],
  "thinking_indexed": true, "index_dir": "…", "size_bytes": 419430400 }
// reindex — incremental, the same one `--refresh-secs` runs
{ "started": true, "stats": { "files_scanned": 512, "docs_added": 88, … } }
```

`POST /api/reindex` holds a lock for the duration; a second concurrent call gets `409` with
`{"error":{"status":409,"message":"a reindex is already running"}}` rather than two writers
racing for the same `IndexWriter`.

## CORS

Off unless `--cors <ORIGIN>` is given (repeatable, `*` allowed). When on, the response
carries `access-control-allow-origin` for a matching `Origin` and `OPTIONS` preflight is
answered with the methods and headers the API uses. With it off, a browser on another
origin cannot read the response — which is the correct default for an unauthenticated
server holding your transcripts.

## Frontend contract

### `web/dom.js` (already written; do not change its exports)

```js
export function el(tag, props, ...children)   // props: {class, text, html, dataset, on:{click}, …}
export function frag(...children)
export function escapeHtml(s)
export function clampText(s, max)
export function fmtTime(ms)                   // "9 Sep 2026, 19:07"
export function relTime(ms)                   // "3 days ago"
export function basename(path) / dirname(path)
export function langFor(path)                 // extension -> a token for .ss-code[data-lang]
export function copyText(s)                   // clipboard, best effort
export function iconFor(name)                 // inline <svg>, see ICONS
export function debounce(fn, ms)
```

### `web/markdown.js`

```js
export function renderMarkdown(text)  // -> DocumentFragment. NEVER returns raw input as HTML.
```
Escape first, then apply the subset: fenced code (` ``` `, with the info string becoming
`data-lang`), inline code, bold, italic, links (`http`/`https`/`mailto` only), headings,
blockquotes, bullet and numbered lists, horizontal rules. Anything it does not understand
stays literal text. It is small on purpose: this renders text a model wrote, and a
markdown bug that turns into an injection is the worst possible outcome here.

### `web/tools.js`

```js
export function toolMeta(doc)        // -> {label, icon, accent}  never throws
export function renderToolCall(doc)  // -> HTMLElement            never throws
export function renderToolResult(doc)// -> HTMLElement | null     never throws
```

A registry keyed by tool name. **Every renderer is wrapped**: it may only produce a node
when the shape it expects is actually there, and returning `null` (or throwing) falls back
to the generic parameter table. That is the whole point — a transcript is full of tools
this build has never heard of, and MCP tools nobody has heard of, and none of them may
produce a broken card.

Known tools to cover: `Bash` (`$ command`, description, terminal-styled output),
`Read` (path + line range, output as numbered code), `Write` (path + content),
`Edit` / `MultiEdit` (old/new as a red/green diff), `Glob` / `Grep` (pattern + hits),
`Task`/`Agent` (subagent type, prompt, link to the sidechain transcript),
`TodoWrite` (the checklist, with status), `WebFetch` / `WebSearch` (url/query),
`NotebookEdit`, and `mcp__server__tool` (server and tool split into two chips, then the
generic table). Unknown tool, missing key, wrong type, `tool_input` that is not even an
object: generic table, no console noise beyond one `console.debug`.

### `web/index.html` ids (the contract with `app.js`)

`#q` (search input), `#sort`, `#refresh`, `#stats`, `#facets`, `#chips`, `#results`,
`#summary`, `#pager`, `#drawer`, `#drawer-body`, `#drawer-title`, `#drawer-close`,
`#empty`, `#error`, `#theme`.

### CSS contract

Tokens on `:root`, redefined under `[data-theme="dark"]` and
`@media (prefers-color-scheme: dark)`:
`--bg --panel --panel-2 --ink --ink-dim --ink-faint --line --accent --accent-dim
--ok --warn --err --add --del --mark --radius --mono --sans`.

Classes the renderers emit, which `styles.css` must style:
`.ss-card .ss-card-head .ss-card-body .ss-meta .ss-badge .ss-badge-role .ss-badge-tool
.ss-chip .ss-chip-x .ss-path .ss-path-dir .ss-path-base .ss-snippet .ss-code .ss-term
.ss-term-cmd .ss-out .ss-kv .ss-kv-k .ss-kv-v .ss-json .ss-diff .ss-diff-add .ss-diff-del
.ss-todo .ss-todo-done .ss-todo-active .ss-more .ss-thread .ss-thread-turn .ss-focus
.ss-facet .ss-facet-head .ss-facet-row .ss-count .ss-hint .ss-err .ss-spinner .ss-empty`

Dark and light both first-class; no external fonts, no CDN, no build step. The page must
work at 400px wide.

## The `dto.rs` / `mod.rs` seam

`dto.rs` is pure data — it does not import `axum`, and it reports failure as a `String` that
`mod.rs` turns into a `400`. That keeps every wire-shape decision testable without a socket,
and keeps `mod.rs` down to routing, blocking-pool hops and status codes.

```rust
// --- query strings ---------------------------------------------------------------------
/// A decoded query string with repeats preserved (`?tool=Bash&tool=Read`).
pub struct Params(Vec<(String, String)>);
impl Params {
    pub fn parse(raw: Option<&str>) -> Result<Params, String>;
    pub fn first(&self, key: &str) -> Option<&str>;
    pub fn all(&self, key: &str) -> Vec<String>;
    /// Absent -> false; present-and-empty, `1`, `true`, `yes`, `on` -> true; `0`,
    /// `false`, `no`, `off` -> false; anything else is an error naming the key.
    pub fn flag(&self, key: &str) -> Result<bool, String>;
    pub fn number<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, String>;
    /// A key outside `allowed` is an error listing `allowed`, never a silent no-op.
    pub fn reject_unknown(&self, allowed: &[&str]) -> Result<(), String>;
}

// --- search ----------------------------------------------------------------------------
/// Elastic Search UI `RequestState` + `queryConfig`, plus this index's own knobs. Unknown
/// keys are kept in `extra` and surface as `info.warnings`, never as a silent drop.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchBody { /* … + #[serde(flatten)] pub extra: serde_json::Map<String, Value> */ }

impl SearchBody {
    pub fn from_params(p: &Params) -> Result<SearchBody, String>;
    pub fn prepare(self) -> Result<PreparedSearch, String>;
}

pub struct PreparedSearch {
    pub request: crate::search::SearchRequest,
    /// 1-based page and its size, echoed back as `current` / `resultsPerPage`.
    pub page: usize,
    pub size: usize,
    pub warnings: Vec<String>,
}

/// The full `ResponseState` envelope, ready to serialize.
pub fn search_ui_response(prepared: &PreparedSearch, resp: &crate::search::SearchResponse)
    -> serde_json::Value;

/// Query-string keys `GET /api/search` accepts. `GET /api/facets/{field}` accepts these
/// plus `top`.
pub const SEARCH_PARAMS: &[&str];
/// Query-string keys `GET /api/sessions` accepts.
pub const SESSION_LIST_PARAMS: &[&str];

// --- documents and other shapes ---------------------------------------------------------
pub fn api_doc(doc: &crate::parse::Doc, include_raw: bool) -> serde_json::Value;
pub fn facet_json(f: &crate::search::FacetResult) -> serde_json::Value;
pub fn session_json(info: &crate::parse::SessionInfo) -> serde_json::Value;
/// `**marked**` plain text -> HTML-escaped text with `<em>` around the marked spans. The
/// ranges are `Hit::snippet_marks`: transcript bodies contain `**` of their own, so the
/// marked string alone cannot say which markers the highlighter wrote.
pub fn highlight_html(snippet: &str, marks: &[std::ops::Range<usize>]) -> String;
/// The `Filters` a session listing honours, from a query string.
pub fn session_filters(p: &Params) -> Result<(crate::search::Filters, usize), String>;
```

`mod.rs` owns:

```rust
pub struct ServeOptions { pub host: String, pub port: u16, pub cors: Vec<String>, pub refresh_secs: u64 }
/// Blocks until the server stops. Builds its own Tokio runtime, so `cli.rs` stays sync.
pub fn serve(index_dir: &std::path::Path, opts: ServeOptions, out: &mut impl std::io::Write)
    -> anyhow::Result<()>;
```

## Known limitations

**A reader is rebuilt per request.** `search::search`, `search::facets`, `context::session` and
`context::around` take `&Index` and call `index.reader()` themselves — a shape inherited from
the CLI, where a process did it once and exited. Under the server that re-parses `meta.json`
and reopens every segment on every request. It is not what makes an outside commit visible
(`ReloadPolicy::OnCommitWithDelay` already does that on a long-lived reader), so the fix is to
hold one `IndexReader` in `AppState` and pass a `&Searcher` down — `docs_by_seq` already takes
one. That is a change to the pinned core signatures for the sake of a local tool's request
rate, so it is recorded rather than smuggled in alongside the server.

**No authentication, and no plan for any.** The bind address is the whole security model. That
is why the default is loopback and why a non-loopback `--host` warns.

**Sorting is relevance or time, nothing else.** There is no ordering by session, project or
tool, because none of them has a meaning a reader could predict. `sortList` naming any field
but `timestamp` is a `400` rather than a silently ignored request.
