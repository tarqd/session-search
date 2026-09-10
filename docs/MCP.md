# session-search as an MCP server — design note

Status: **built.** `session-search mcp` serves five read-only tools over stdio, behind the
default-off `mcp` cargo feature. Covered by tests: the tool bodies, the slicer and the zero-hit
ranking as unit tests (`mcp/envelope.rs`, `mcp/tools/*`, `slice.rs`), and `tests/mcp_stdio.rs`,
which spawns the real binary, speaks `initialize` + `tools/list` to it, and asserts stdout parses
as JSON-RPC, the server introduces itself by its own name rather than its transport library's,
every tool has an object-rooted input schema with a description on every property and no required
argument but `aggregate`'s `field`, both closed vocabularies carry their `enum`, and neither stream carries an escape
sequence. **Not** covered: no test drives the schemas with a model on the eval query set,
so the one acceptance item of #28 that cannot be asserted from inside the process — do these
descriptions actually route a model to the right tool — is still open. `docs/EVAL.md` has no MCP
section.

This note is the argument: why this shape, and what breaks under the alternative. The pinned types
and signatures live in [`DESIGN.md`](DESIGN.md#mcp-surface-mcp-feature-mcp--pinned) under **MCP
surface**, which is the contract; this is the reasoning behind it. The CLI these tools share their
structs with is documented in the [README](../README.md).

## Goal

`session-search mcp` serves the read operations to an agent over stdio, so Claude Code can search
its own history mid-task — "have I hit this error before?", "what was the command that worked?",
"which files did I touch in that session?" — without shelling out and parsing text.

The division of labour is the reason the natural-language work is *not* in the engine: on this
path **the caller is already an LLM**, so parsing a question and synthesising an answer are free.
Doing either inside a Rust binary would cost the offline-first property and ship a model in a CLI
that has to start in milliseconds. What the server owes the caller is not understanding; it is a
surface where the wrong call is hard to make and a wrong result is hard to mistake for a fact.

## Shape: five tools, one per query class

The first version of this note said "one tool per subcommand" — `search`, `facets`, `show`,
`sessions`, `stats` mapped one-to-one. That was the wrong axis, and it is the main thing this
rewrite replaces. Subcommands are a CLI's factoring: they exist because a shell user types a verb
and then reads what comes back, correcting as they go. A model does not correct as it goes. It
picks one tool from a list of descriptions, sends one call, and treats what returns as the answer.

Issue #28 puts the argument as a table. Three canonical questions, three different *shapes*, and
mapping them onto one `search` tool guarantees the model reaches for the wrong one:

| Question | Actual shape | Tool |
| --- | --- | --- |
| "what was I working on last week" | temporal filter + aggregation over sessions | `search_sessions` |
| "why did the build fail" | retrieval + reading in context | `search_turns` |
| "what errors did we see" | aggregation — a table, not a hit list | `aggregate` |

Two of those were already answerable before any of this was written; they just were not *phrased*
that way. The gap was routing and answer shape, not ranking.

What breaks under one `search` tool is specific, not hypothetical. Ask it "what errors did we
see" and it returns ten ranked passages. Ten is not a fact about the distribution — seven
failures spread over four tools, ranked by BM25, is an arbitrary sample that changes with every
`limit` the model happens to pass. The model then counts what it was given and reports a total
with total confidence. Ask it "what was I working on last week" and it returns twenty documents
from three sessions, and the model has to *infer* the list — a step it will perform, and perform
plausibly, whether or not the inference holds. Neither answer is flagged as the wrong shape,
because nothing in a hit list can flag it.

So the tools are named for the answer's shape, and the server's `instructions` open with a
routing rule that says so out loud: *route by the shape of the answer, not by the words in the
question.*

Five rather than three because retrieval is two steps, not one:

```
search_turns    SearchTurnsRequest    -> SearchTurnsResponse    which moment (skeletons)
get_turn        GetTurnRequest        -> GetTurnResponse        that turn, in full
get_output      GetOutputRequest      -> GetOutputResponse      that one call's output, sliced
search_sessions SearchSessionsRequest -> SearchSessionsResponse which sessions
aggregate       AggregateRequest      -> AggregateResponse      which values, counted
```

`get_turn` and `get_output` are separate tools because they fail differently. `get_turn` returns
many documents and *truncates* each one to a byte budget; `get_output` returns one document and
*slices* it — head, tail, regex grep. Folding them together would give a caller one knob that
means "cut every document a bit" and no way to say "give me the six lines of this 200 KB log that
mention the failing test". That is the difference between "show me this turn" and "show me this
log", and a model that has only the first will fetch the turn repeatedly with a larger budget.

Two CLI subcommands have no tool at all. `show` is subsumed: `get_turn` takes `before`/`after` and
reads a session forwards or backwards from a turn you have already located. `stats` is not a tool
because its whole content — session count, document count, newest indexed transcript — is a fact
about the corpus that the model needs *before* it decides what to call, not after. It is
substituted into `instructions` at startup, where it orients the first call instead of costing
one.

## Skeleton-first retrieval, and what it saves

`search_turns` returns one result per conversational turn, and each turn as a **skeleton**: the
human prompt, the assistant prose, and one line per tool call giving its signature and how it
ended (`-> ok`, `-> no result`, or `-> error:` and the first line of what failed). Tool output is
not there. That is the point.

The measurement is in [`EVAL.md` §5](EVAL.md#5-turn-skeletons--issue-25): across the eval corpus a
turn's context costs **5,651 bytes** on average and its skeleton **555** — 9.8% overall, and the
worst turn goes from 19,541 bytes to 1,642. Tool output is most of the bytes in a transcript and
close to none of the intent, so a page of twenty skeletons is roughly eleven kilobytes and buys
the same routing decision that twenty full turns would buy for a hundred and thirteen.

The three-step protocol falls out of that: read twenty skeletons, decide which single turn answers
the question, and pay full price only there. `get_turn` for that turn's documents, `get_output`
for the one call whose output you need, sliced. The defaults encode the shape — `search_turns`
returns 20 turns, `get_turn` 60 documents at 4 KB of tool result each, `get_output` a 16 KB budget
after head/tail/grep have chosen the lines.

Skeletons were not built for this server; `--group-by-turn` and `--context skeleton` existed on
the CLI first, and nothing had to be stored to make them cheap — a skeleton is the `text` side of
a `tool_call` document with the `tool_output` side left out. The MCP surface is where the saving
actually matters, because the CLI's reader can skim past output they did not want and a context
window cannot.

## The schemas are the deliverable

There is more leverage in the eighteen filter descriptions than in the code that reads them. A
filter a model guesses at is a silent zero, and a silent zero is reported to a user as a fact
about their history.

`search::Filters` is the hinge: one struct, three front ends, one schema. `clap::Args` gives the
CLI its flags; `serde::Deserialize` with `#[serde(default)]` gives the HTTP API and the MCP server
a request body where every field is optional; `schemars::JsonSchema`, feature-gated, gives the
model the input schema. The rule that keeps this working is **no clap-specific types in the
data**: every field is a `String`, `Option<String>`, `Vec<String>`, `Option<u64>` or `bool` — no
`ValueEnum`, no `PathBuf`, no newtypes. Request structs `#[serde(flatten)]` the whole struct
rather than nesting it, so the descriptions land at the top level of the input schema where a
model reads them rather than one level down behind a `$ref`, and so the retry body the envelope
hands back is a flat object the caller can send verbatim.

The clap view and the schemars view of that one struct legitimately differ in exactly two places,
and both differences run the same direction — the model is told what the shell user would have
discovered by retrying:

1. **Length.** The `///` doc comment stays: it is the `--help` line, and `--help` is read at a
   terminal by someone who can try again. The `schemars(description = …)` override replaces it in
   the JSON Schema with what a model cannot see from outside — that `program` is compared byte for
   byte against a raw-tokenized field, so `Cargo`, `cargo build` and `/usr/bin/cargo` are silent
   zeroes where `cargo` is a hit; that `tool_input` subpaths are dynamic, so
   `tool_input.file_path` works without being declared anywhere; that a tool call's role is
   `assistant`, so `role` cannot isolate one and `kind` is the only route.
2. **Enums.** `kind` and `role` carry `schemars(extend("enum" = [...]))` — including `null`,
   because schemars emits `"type": ["string","null"]` for an `Option<String>` and a strict
   validator ANDs `type` with `enum`. clap has no enum here, because `Filters` may hold no clap
   types. This is the one constraint worth paying for twice: `kind: "toolcall"` matches nothing
   and does not error, and a shell user sees the zero and retries where a model reports it.

Everything else a description can do, it does in prose: it names the tokenizer, says whether a
match is exact or prefix or phrase, says which documents a filter silently excludes as a
side effect (`model` excludes every user prompt; `lang` excludes every tool call), and — where the
vocabulary is discoverable — names the `aggregate` call that lists it. A description that ends
"`aggregate` on `git_branch` lists what exists" turns a dead end into a next call.

## Never a bare empty answer

The documented failure mode of LLM-driven structured querying is that reliability holds for simple
queries and degrades sharply as complexity rises — and every degradation here looks identical from
the outside. A misspelled `kind`, a case-wrong `program`, a branch name spelled short, and a
genuinely empty corpus all return the same zero. A model handed `{turns: []}` will tell the user
nothing happened last week.

So every response — not only the empty ones — carries an `Envelope`: the filters actually applied,
the time window resolved to **absolute instants** (a relative `7d` means a different window on
every call, so echoing the span back is not a report of what was searched), the engine's warnings,
and, exactly when the count is zero, a `no_results` with one ranked retry. `no_results` is `Some`
if and only if `total == 0`; an envelope that suggested a retry beside a page of hits would train
a caller to skip it, and then it would be skipped on the call that mattered.

The interesting part is the ranking, because nothing else in the codebase ranks filters and the
order cannot be read off the request. It follows from **how each filter is matched**, which is
precisely what the caller cannot see:

0. an address (`turn_of` + `turn_seq`) — an intersection naming one turn of one file. Every other
   filter selects a set; this one selects a place, so nothing below it can be narrower;
1. phrase over analyzed text (`tool_input`, `tool_output`) — adjacent, in order, unstemmed;
2. exact term over an **open** vocabulary (`program`, `branch`, `model`, `tool`, `agent_type`,
   `lang`) — byte equality against a value reproduced from memory, so a silent zero whenever it
   was misremembered;
3. prefix or range (`session`, `project`, `min_thinking`, `since`/`until`) — width is the caller's
   choice, not the field's;
4. exact term over a **closed** vocabulary, and the flags — wide when right, total when wrong.

Two overrides on plain narrowness. **Silent-zero traps are promoted:** `kind` and `role` have
statically known legal sets, so a value outside one is *provably* the cause — it is blamed first,
and the retry substitutes a legal value rather than dropping the filter. **Scope filters are held
back:** `project` and the time window drop last, because dropping them does not widen the
question, it answers a different one. A hit from another repository is not a better answer than
zero; it is a wrong answer that reads like a right one.

One filter is outside the ranking altogether. `all_records` only ever widens — it brings the
apparatus records (attachments, `system` records, meta turns) back into a scope that excludes them
by default — so it cannot be the cause of a zero, and naming it would tell a caller to drop the
one thing holding the search open. It is still echoed as applied, because the retry is rebuilt
from that echo and a retry that dropped it would search *less* than the call it is answering. What
the scope did to a result is reported as a number instead: `search_turns` returns `hidden`, the
count of documents the query matched and the default scope refused, which is the difference
between a search that found nothing and a search that was not allowed to look.

Three rules govern what the retry *does* with that diagnosis, because the message is prose a model
may skim and the retry is an object it will send unread — where the two disagree, the retry wins in
practice. **A contradiction drops the exclusion, not the intent:** `agent_type` lives only on
sidechain documents and `no_sidechains` deletes every one of them, so the flag goes and the value
stays. **A mis-spelled exact-match value is corrected, not deleted:** `program: ["Cargo"]` retries
as `["cargo"]`, because dropping the filter answers a much wider question — everything mentioning
the query — and reads as an answer to the one that was asked. **A negated query term is never
handed back:** a query carrying a `-flag` retries as the whole query quoted, which is the repair
the message itself recommends.

The ranking is a total function of the request and never probes the index — a ranking that ran a
second query for every zero would still be a guess, and would cost a second pass to make it. The
corrections above keep that promise: each candidate is derived from the value the caller sent
(case-folded, cut at its first word, or respelled against the capitalised tool vocabulary), never
from a lookup, and a correction that is itself wrong costs one extra call — the next pass finds
nothing left to repair and drops the filter. It lives in one module with four callers, for the
same reason `sessions.rs` was extracted: four copies would disagree about which filter is
narrowest, and the disagreement would be invisible from any one of them.

An argument no tool defines is the one caller mistake a description cannot cover, so it is caught
rather than described. `#[serde(deny_unknown_fields)]` is incompatible with the `#[serde(flatten)]`
every request uses for `Filters`, so each request carries a second flattened field — a map — that
takes whatever the named fields did not, and the router turns each leftover key into a warning
naming the closest argument that does exist (`tool_name` → `tool`, `sinceX` → `since`). Before it,
a misspelled filter came back as the whole corpus with `applied_filters: []` and nothing to
suggest the filter had never been applied.

The other half of the same argument is `SearchResponse::warnings`. Three outcomes of this index
look exactly like an empty corpus from outside — a `word:value` term whose root is not a schema
field (read as a `tool_input` JSON subpath, which cannot fail to parse and simply matches
nothing), a similarity seed whose every term fell outside the tuning, and a grouped page that came
back short because the collapse window ran out before `limit` distinct turns did. Each was logged
at WARN and nothing else. Over stdio there is no stderr the caller is reading, so a warning that
only reaches `tracing` reaches nobody; they now travel on the response and land in the envelope.
`search_sessions` carries the parallel list — the twelve filters `sessions.json` cannot answer,
the ten per-message ones plus both halves of the turn address — through the same channel, because
a listing filtered by eleven of twelve filters looks exactly like a listing filtered by twelve.

## Errors: the caller's mistake, or the server's

`ErrorData` is a JSON-RPC protocol error, which clients render opaquely and models often cannot
act on. It is used **only** for requests that could not be started: a malformed date, a reference
naming nothing, two addresses at once, half a turn address, a field this index cannot count, a
`grep` that is not a regex. Everything a tool can answer, including "nothing matched", comes back as a normal result
whose envelope explains itself.

That distinction has to be made in exactly one place. Integrating the four tool bodies turned up
the bug that proves it: `from_anyhow` downcast only `FilterError`, so every other caller mistake
arrived as `internal_error` — telling the caller the tool had broken when the remedy was to send
something different. Two tool modules had independently invented a type for it. There is now one,
`mcp::CallerError`, classified at the boundary alongside `FilterError`. A second type would be a
second match arm somebody forgets, and the symptom is silent: the message still arrives, just
labelled as the server's fault.

## Recorded traps

Five facts about this stack that were found by driving it rather than by reading its
documentation. They are recorded because not one of them produces a diagnostic that names the
cause — two of them produce no diagnostic at all.

**1. `schemars` is `^1.0`, not `0.8`.** rmcp's README says `schemars = "0.8"` and it is wrong for
3.2 — the `#[tool]` macro expands to schemars 1.x APIs. Pinning 0.8 produces a wall of trait
errors that look like a macro bug and are not.

**2. `serverInfo` defaults to `{"name":"rmcp","version":"3.2.0"}`.** `ServerInfo::new` calls
`Implementation::from_build_env()`, whose `env!` expands *inside the rmcp crate*. Nothing warns.
`.with_server_info(Implementation::new("session-search", env!("CARGO_PKG_VERSION")))` is not
optional: this string is what a host shows a user when it asks whether to trust these tools.

**3. An input schema whose root is not `type: "object"` panics at router construction** — at
startup, not at build time. Every `Parameters<T>` must be a struct. A tool that takes a bare
string compiles fine and takes the whole server down the first time it is run.

**4. `Json<Vec<T>>` puts a bare array in `structuredContent`,** which the spec says must be an
object; strict clients reject it. Every response here is a named struct with the collection as one
field — which is also why `search_turns` returns `{turns, total_documents, returned, …}` rather
than a list.

**5. The stdout deadlock.** The MCP framing *is* stdout: one JSON-RPC message per line, nothing
else. `cli::run` wrapped `stdout().lock()` in a `BufWriter` for the whole of `dispatch`, and
rmcp's transport writes from its own threads. A `std::io::Stdout` lock is reentrant within a
thread and blocking across threads, so the transport's first response parked forever — no panic,
no error, no log line, just a server that never answered. `Command::owns_stdout` now splits the
two paths: a command that frames stdout itself is handed `io::sink()` for the channel every other
command writes through, rather than a second handle to the stream it is framing.

That last one is why `tests/mcp_stdio.rs` drives the built binary instead of unit-testing the
handler. Both halves of the bug lived outside `Server` — one in argument dispatch, one in the
transport — and a test that constructs the server directly reaches neither. The companion test
`mcp::tests::stdout_belongs_to_the_transport` scans the module tree's own source (with comments
stripped, since these docs discuss the failure by name) for `println!`, `print!` and
`io::stdout`, because a running server only proves the branches it happened to take and this
failure lives in the branch nobody exercised.

## Decisions that were open questions

The previous version of this note ended in four open questions. All four are now decided.

**Response size — skeleton first, and a byte budget on the one thing that is unbounded.** A `Doc`
carries the full indexed text and a `show` over a long session can be megabytes. The answer is not
one cap; it is that the *default* retrieval path never fetches text at all (skeletons), and the
two drill-down tools each carry the budget appropriate to their failure: `get_turn` truncates per
document (4 KB) and reports how many documents it cut, `get_output` slices with head/tail/grep and
then caps (16 KB) and reports total bytes, total lines, matched lines, returned lines and whether
the cut landed mid-line. A partial answer must never read as a whole one, which is why every one
of those counts is on the wire and why gaps are marked rather than silently joined.

One rmcp detail sharpens this: `Json<T>` puts the payload on the wire **twice** —
`structuredContent` and a text block carrying the same JSON. Every byte budget here is worth
double what it looks like. And `raw`, the original JSONL line, stays out of tool responses
entirely; `format::doc_json` withholds it, and the MCP `Document` schema says so.

**Index location — resolved before dispatch, reported in `instructions`.** An MCP server started
by an agent host inherits whatever environment the host gives it, so "which index am I actually
serving" is a real question with a non-obvious answer. It is the existing global `--index` /
`$SESSION_SEARCH_INDEX`, resolved by `cli::run` exactly as for every other command — no
`mcp`-specific flag, because a second way to say the same thing is a second thing to get wrong —
and the resolved directory is substituted into the server's `instructions` beside the corpus
counts, where the model can see it. The `mcp` subcommand's own flags are only the two about
refresh.

**Concurrency — one `Index` per process, one reader per call.** The server opens one
`tantivy::Index` and one `Fields` at startup and holds them for the life of the process. Each call
takes its own reader, because `search::search`, `search::facets` and the `context::*` entry points
call `index.reader()?.searcher()` internally. Hoisting an `IndexReader` would buy one avoided
`reader()` call per request and cost a change to every one of those pinned signatures — the
signatures three front ends now share — plus a reader that has to be told when to reload. That is
out of scope, and this is the decision: **do not hoist it.**

Refreshing is a different question, because it *writes*. The CLI re-indexes before every read
command, which is right for a process that lives 200 ms and wrong for one that lives a day: a tool
call would pay for an index scan, and two concurrent calls would contend for the writer lock. So:
once at startup before the transport opens (unless `--no-refresh`); then at most once every
`--refresh-secs`, default 300, `0` to disable, checked at the start of a tool call and guarded by
a mutex so two calls never run two indexers; and a refresh failure is logged to stderr and the
call proceeds against the index as it stands, because an unreadable transcript root must not stop
the server answering from what it has. Because each call opens a fresh reader, a refresh — by this
process or by a `session-search index` running beside it — is visible to the very next call with
nothing to arrange.

**Read-only posture — `index` is still not a tool, and every tool says so.** The read path already
refreshes on its own schedule, so a model never needs to ask for indexing; and a tool that writes
to a shared directory on a model's behalf is a different risk conversation from a tool that reads.
All five tools are annotated `read_only_hint = true, open_world_hint = false`, and the
`instructions` say it in the first paragraph: nothing writes to a transcript, and nothing writes
to the index. If serving `index` is ever wanted, it should be an explicit opt-in flag on the
server, never a default tool.

## Still open

- **The routing evidence is a one-off, not a harness.** The schemas *have* now been driven by a
  model working from the `instructions` string and the `tools/list` payload alone, over the 39
  queries in `tests/fixtures/eval_queries.json` and the three canonical questions: all 42 routed
  to the right tool on the first attempt and all 42 were answerable from the response. That
  exercise is what produced most of the corrections in the descriptions, and every deliberate
  mistake it made — `kind: "toolcall"`, `program: "Cargo"`, a bare `turn_seq`, a malformed
  `grep` — came back with the repair rather than a bare zero.

  What does not exist is a way to *re-run* it. The result is a paragraph in a commit message, not
  a number `EVAL.md` tracks, so a description edited next month is unmeasured again. Routing is
  also the cheap half: what a fixed query set cannot tell you is whether a model asks the right
  question in the first place. `EVAL.md` has no MCP section.
- **No resources and no prompts.** Tools only. A session transcript is an obvious MCP *resource*
  and is not exposed as one; nothing yet needs it, and a resource is a URI scheme to keep stable.
- **The corpus counts in `instructions` are read once at startup** and not updated by a refresh.
  They are an orientation, not an answer, and re-deriving them would cost a `sessions.json` parse
  per request — but a long-lived server's counts do drift from its index.
