# session-search as an MCP server — design note

Status: **not built yet.** This is the plan of record for the follow-up, distilled from the
"MCP readiness" section of [`DESIGN.md`](DESIGN.md) and from what the CLI already does. Read
`DESIGN.md` first for the pinned types; this note only covers the server layer on top of them.
The CLI those tools mirror is documented in the [README](../README.md).

## Goal

`session-search mcp` serves the existing read operations to an agent over stdio, so Claude Code
can search its own history mid-task — "have I hit this error before?", "what was the command that
worked?", "which files did I touch in that session?" — without shelling out and parsing text.

The CLI is the prototype of the MCP surface, not a separate product. Every `--json` payload the
CLI emits today is the payload the corresponding tool returns.

## Dependency

```toml
[features]
mcp = ["dep:rmcp", "dep:schemars", "dep:tokio"]

[dependencies]
rmcp = { version = "3.2", default-features = false, optional = true,
         features = ["server", "macros", "transport-io", "schemars"] }
```

Behind an `mcp` cargo feature so the plain CLI keeps its current dependency footprint and build
time. `default-features = false` matters: rmcp's defaults pull in client and HTTP transports that
a stdio server does not need.

## Shape: one tool per subcommand

```
search    -> SearchResponse       full-text search, optional facets
facets    -> Vec<FacetCount>      count a fast field or any tool_input.<path>
show      -> Vec<Doc>             a session, or a window around one hit
sessions  -> Vec<SessionInfo>     list indexed sessions
stats     -> IndexStats           index statistics
```

Each is an `#[rmcp::tool]` method taking `Parameters<T>` and returning `Json<U>`, where `T` is the
*same* struct clap derives its arguments into and `U` is the same struct `--json` serializes.
There is no second set of request or response types, and no second copy of the argument
documentation — the doc comments on `Filters` are already the argument help text, and become the
JSON Schema descriptions the agent reads.

`index` is deliberately not exposed as a tool for now. The read tools already refresh the index
before they read it (the `--no-refresh` path is the exception, not the rule), so an agent never
needs to ask for indexing; and a tool that writes to a shared directory is a different risk
conversation from a tool that reads.

## The shared `Filters` struct

`search::Filters` is the hinge of the whole design. One struct, two front ends and one schema:

```rust
#[derive(Debug, Clone, Default, clap::Args, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Filters {
    /// Project path; matches by prefix, so `-p ~/code` catches subdirectories.
    #[arg(short = 'p', long, value_name = "PATH")]
    pub project: Option<String>,
    /// Tool name; repeatable.
    #[arg(short = 't', long, value_name = "NAME")]
    pub tool: Vec<String>,
    …
}
```

- `clap::Args` gives the CLI its flags.
- `serde::Deserialize` + `#[serde(default)]` gives the MCP server a request body where every
  field is optional — an agent that sends `{"query": "SIGBUS"}` gets sane defaults for the rest.
- `schemars::JsonSchema` (feature-gated) gives the agent the tool's input schema.

The rule that keeps this working: **no clap-specific types in the data.** Every field is a
`String`, `Option<String>`, `Vec<String>` or `bool` — no `clap::ValueEnum`, no `PathBuf`, no
newtypes. `SearchRequest` holds `Filters` plus paging and facet options and derives serde only;
clap builds it in `cli.rs`.

The cost of that rule is that the enum-ish filters are not enum-typed anywhere: `kind` and `role`
become plain term queries, so `kind: "toolcall"` silently matches nothing instead of erroring.
That is tolerable at a shell prompt, where you see the zero and retry. It is worse for an agent,
which will read "0 results" as a fact about the corpus. The MCP schema should therefore constrain
those two fields with a schemars `enum` even though clap does not — the one place the front ends
should legitimately differ.

## Two recorded traps

**1. `schemars` is `^1.0`, not `0.8`.** rmcp's README says `schemars = "0.8"` and it is wrong for
3.2 — the `#[tool]` macro expands to `schemars` 1.x APIs. Pinning 0.8 produces a wall of trait
errors that look like a macro bug and are not. Depend on `schemars = "1"`.

**2. The stdio transport owns stdout.** The MCP framing *is* stdout: one JSON-RPC message per
line, nothing else. Therefore, in the `mcp` code path:

- `tracing` must write to **stderr** (the CLI already does this; do not regress it).
- Colour must be off — no ANSI escapes anywhere near stdout.
- Nothing may `println!`. The human renderers in `format.rs` take a `&mut impl Write` rather than
  writing to stdout directly, which is what makes them harmless here: the MCP path simply never
  calls them.

A single stray `println!` in a library module turns into a protocol parse error on the client
side with no useful diagnostic, so this is worth a test that runs the server and asserts stdout
contains only well-formed JSON-RPC.

## Open questions

- **Response size.** A `Doc` carries the full indexed `text`; a `show` over a long session can be
  megabytes. The tools likely need a byte budget and a truncation marker, the way the CLI's
  `max_text_bytes` caps indexing. `raw` (the original JSONL line) must stay out of tool responses
  entirely — the CLI already withholds it from `--json` hits.
- **Index location.** `--index` / `$SESSION_SEARCH_INDEX` are process-level; an MCP server started
  by an agent host inherits whatever environment the host gives it. Probably a startup flag,
  reported back in the server's `instructions`.
- **Concurrency.** The index is opened per invocation today. A long-lived server should hold one
  `Index` and take a fresh `Searcher` per call, and needs a policy for how often it refreshes
  rather than refreshing on every request.
- **Read-only posture.** Serving `index` later means writing to a directory on behalf of a model.
  If it happens, it should be an explicit opt-in flag on the server, not a default tool.
