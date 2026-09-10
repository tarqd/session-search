# session-search

**Full-text search and faceted analytics over your Claude Code session transcripts.**

Every Claude Code session is written to disk as JSONL under `~/.claude/projects/` — every prompt
you typed, every command the agent ran, every file it read, every tool argument, every error,
across every project and every subagent. It is the most complete record of your own engineering
work that exists on your machine, and until now it was write-only: a pile of append-only logs with
no index, no query language, and filenames that are lossy hashes of the directory you were sitting
in. `session-search` turns that pile into a local [Tantivy](https://github.com/quickwit-oss/tantivy)
index you can actually interrogate — full-text ranked search across prompts, assistant replies,
tool calls and tool output, narrowed by project, branch, model, tool, date or error status, and —
the part that makes it more than `grep` — **faceted aggregation over tool *parameters***, so you
can ask "which files have I edited most this month" or "what is every `cargo` command I have run"
and get counted answers rather than a scroll of matches.

---

## Install

Requires a Rust 2024-edition toolchain (built and tested on `rustc 1.94.1`).

```bash
git clone <this repo> && cd session-search
cargo build --release
# binary at ./target/release/session-search
```

That is the whole tool. Two extra features — a browser UI and the HTTP API under it — are
**off by default** and have to be asked for, because they bind a port and serve your transcripts
verbatim:

```bash
cargo build --release --features web-ui     # UI + API
cargo build --release --features http-api   # API only
```

See [The web UI and the HTTP API](#the-web-ui-and-the-http-api).

Or install it onto your `PATH`:

```bash
cargo install --path .
```

```
  Installing session-search v0.1.0 (/home/user/session-search)
    Finished `release` profile [optimized] target(s) in 0.85s
  Installing /root/.cargo/bin/session-search
   Installed package `session-search v0.1.0 (/home/user/session-search)` (executable `session-search`)
```

No configuration, no daemon, no network access — a default build has no socket in it at all. It
reads `~/.claude/projects/` (or `$CLAUDE_CONFIG_DIR/projects`) and writes one index directory.

---

## Quick start

> Every block below is real output from running the command on this machine, whose only indexed
> corpus is this project's own development history: one Claude Code session plus ten subagent
> sidechains, in a single project directory. That session was still running while the README was
> written, so document counts creep upward from one example to the next — 638 at the first index,
> 646 a few minutes later, as the session that is writing this README adds turns to itself. On a
> machine with real history you would see many sessions and many projects.

### 1. Index

```bash
session-search index
```

```
  files scanned          11
  files updated          11
  files reset             0
  documents added       638
  documents deleted       0
  sessions               11
  parse errors            0
  elapsed            114 ms
```

Run it again and it does nothing, quickly — the indexer is incremental and watermarked per file:

```
  files scanned        11
  files updated         0
  files reset           0
  documents added       0
  documents deleted     0
  sessions             11
  parse errors          0
  elapsed            7 ms
```

You rarely need to run `index` by hand: `search`, `facets`, `show` and `sessions` refresh the
index themselves before they read it, unless you pass `--no-refresh`.

### 2. What have I even been working on?

```bash
session-search sessions --limit 6
```

```
b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a02e0e345842f6efc · workflow-subagent
  2026-09-09 20:43 · 8 msg · 16 tools · /home/user/session-search · claude/rust-mcp-session…
  harden:docs

b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a74ce379e5996c1a2 · workflow-subagent
  2026-09-09 20:42 · 9 msg · 66 tools · /home/user/session-search · claude/rust-mcp-session…
  harden:search-review

b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent ab8a797247298645b · workflow-subagent
  2026-09-09 20:41 · 10 msg · 41 tools · /home/user/session-search · claude/rust-mcp-sessio…
  harden:parser-review

b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent af773b62c083b8923 · workflow-subagent
  2026-09-09 20:28 · 16 msg · 88 tools · /home/user/session-search · claude/rust-mcp-sessio…
  integrate

b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent aa775065fe6de6940 · workflow-subagent
  2026-09-09 20:16 · 14 msg · 55 tools · /home/user/session-search · claude/rust-mcp-sessio…
  build:cli

b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a2cce0b9f6d21fbd9 · workflow-subagent
  2026-09-09 20:01 · 12 msg · 30 tools · /home/user/session-search · claude/rust-mcp-sessio…
  build:search
```

Subagents get their own row, keyed on `(session id, agent id)`, with the task they were given as
the caption — the shape of a delegated workflow is legible at a glance.

### 3. "Why did we decide *that*?"

Search is ranked full text over prompts, replies, tool calls and tool output. Hits are grouped by
session, and the matched terms are marked in the snippet.

```bash
session-search search "SIGBUS OR memmap" --limit 2
```

```
2 of 18 hits · 2 ms

▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a02e0e345842f6efc · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 20:43

   1. 20:43:11  assistant Bash command=export COLUMNS=92 && ./target/release/sess…  #25  15.85
      Bash export COLUMNS=92 && ./target/release/session-search search "**SIGBUS** OR **memmap**"
      --limit 3 2>&1 search demo 1

▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a2b7a39a6e61bb751 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:58

   2. 19:58:15  assistant Write file_path=/home/user/session-search/src/index.rs  …  #27  2.39
      every delete and add, and there is exactly one `commit()`. //! * Transcripts are read
      through `BufReader`, never memory-mapped: they are appended to //! while we read, and
      a truncation under a mapping raises an uncatchable `**SIGBUS**
```

The top hit is the shell command that first ran *this very example* (with `--limit 3`) a few
minutes earlier, indexed out of the session that is writing this README. The corpus includes the
session you are sitting in.

The query is a real query language, not a substring match: `"quoted phrases"`, `AND`/`OR`/`NOT`
and `field:value` all work. A message is split before it is indexed: its prose is analyzed as
English, so `compiling` finds `compiled`, while its code blocks, its inline spans and every tool
result go through an analyzer that never stems. Both halves split identifiers, so a name is
findable by any of its parts and by any of its spellings wherever it was written — `create`,
`openOrCreate` and `OpenOrCreate` all find `open_or_create`, in a fence or in a sentence — and
`"open_or_create"` in quotes is still an exact phrase. A phrase never runs across a code block
that was lifted out from between two paragraphs. Markdown headings are indexed once more on
their own and count double, so a hit in a section title outranks the same word in a paragraph.
There is no fuzzy operator — Tantivy 0.26 reads `~` as phrase slop,
not edit distance, so `widget~1` is not a near-miss search. A query that fails to parse is
retried leniently and the discarded parts are reported on stderr, so a typo'd field name does
not look like an empty corpus.

### 4. Narrow by anything the transcript recorded

Filters AND together and apply to every read command.

```bash
session-search search "rmcp" --tool Bash --limit 3
```

```
3 of 22 hits · 2 ms

▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a856aaeeb8367435e · Explore  (3 hits)
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:12

   1. 19:12:06  assistant Bash command=cd /tmp/csr/rmcp-3.2.0 && sed -n '1,120p' C…  #26  5.56
      Bash cd /tmp/csr/**rmcp**-3.2.0 && sed -n '1,120p' CHANGELOG.md Read **rmcp** changelog for
      API churn # Changelog All notable changes to this project will be documented in this
      file. The format is based on [Keep a Changelog](https

   2. 19:12:01  assistant Bash command=cd /tmp/csr && curl -sSL -o rmcp.crate "htt…  #23  5.54
      Bash cd /tmp/csr && curl -sSL -o **rmcp**.crate
      "https://static.crates.io/crates/**rmcp**/**rmcp**-3.2.0.crate" && tar xzf **rmcp**.crate && ls
      **rmcp**-3.2.0 && echo "=== tests ===" && ls **rmcp**-3.2.0/tests 2>/dev/null | head -30
      Download and extract **rmcp** 3.2.0

   3. 19:12:05  assistant Bash command=cd /tmp/csr/rmcp-3.2.0 && sed -n '1,140p' R…  #25  5.48
      Bash cd /tmp/csr/**rmcp**-3.2.0 && sed -n '1,140p' README.md Read version-pinned **rmcp**
      README <style> .rustdoc-hidden { display: none; } </style> <div
      class="rustdoc-hidden"> # **rmcp** [![Crates.io](https://img.shields.io/crates/v/**rmcp**.svg
```

Filter on a *tool parameter* with `--tool-input key=value` — here, every time anything read or
wrote `DESIGN.md`, with no free-text query at all:

```bash
session-search search "" --tool-input file_path=DESIGN.md --limit 2
```

```
2 of 8 hits · 2 ms

▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a2b7a39a6e61bb751 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:50

   1. 19:50:56  assistant Read file_path=/home/user/session-search/docs/DESIGN.md  #7  13.38
      Read /home/user/session-search/docs/DESIGN.md 1 # session-search — design contract 2 3
      **This document is the contract between modules.** Agents working in parallel own disjoint
      4 files and must code against the signatures here rather than …

▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a5de89fd39222e50f · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:30

   2. 19:30:30  assistant Read file_path=/home/user/session-search/docs/DESIGN.md  #7  13.38
      Read /home/user/session-search/docs/DESIGN.md 1 # session-search — design contract 2 3
      **This document is the contract between modules.** Agents working in parallel own disjoint
      4 files and must code against the signatures here rather than …
```

Filter on what a tool **returned** with `--tool-output`, a phrase over the result text —
"which commands printed a passing test summary?", with no free-text query at all:

```bash
session-search search "" --tool-output "test result: ok" --limit 1
```

```
1 of 10 hits · 6 ms

▌ 246d26f9-45fd-5656-aaa1-1768c41a6448  (1 hit)
▌ /home/user/session-search · claude/tool-outputs-indexing-9l87sj · 2026-09-09 23:38

   1. 23:38:26  assistant Bash command=cargo test --all-features 2>&1 | grep -E "test resu…  #63  6.48
      **test** **result**: **ok**. 176 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out
      **test** **result**: **ok**. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The call and its result are separate fields, so you can ask about either one alone:

| query | matches |
| --- | --- |
| `Compiling` | 14 — every body field, the default |
| `text:Compiling` | 10 — the tool name and its input only |
| `tool_output:Compiling` | 5 — what the tool actually printed |

A bare query still spans the pair, so nothing that used to match stops matching.

A bare query also spans one thing the transcript never said. Every document is indexed with a
short **context header** beside its body — the session's title and opening prompt, the project,
the branch, and the prompt that opened the document's own turn — so a message that reads "yes"
and a `cargo build --release` that says nothing about what it was building are findable by what
they were *for*. The header is scaffolding, not content: it is never stored, so it is never
shown, never highlighted in a snippet, and never appears in `--json`. Under the default
relevance order it is weighted well below the body, so a document that genuinely discusses your
term outranks the ones that merely happened in the same session. The header does widen what
*matches*, though: the total, the facet counts and a `--sort newest` page all cover every
document of a turn or session about your term, not only the ones that mention it. Qualify the
query (`text:tokenizer`) to ask about bodies alone.

### 5. Read the conversation around a hit

`--context N` pulls the surrounding turns in next to each hit:

```bash
session-search search "expand_dots" --limit 1 --context 2
```

```
1 of 24 hits · 1 ms

▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a2cce0b9f6d21fbd9 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:54

   1. 19:54:36  assistant Bash command=T=$(echo ~/.cargo/registry/src/*/tantivy-0.…  #23  7.74
      path, 501: json_options.is_**expand**_**dots**_enabled(), 505:
      convert_to_fast_value_and_append_to_json_term(&term, phrase, false) 533: json_path:
      &str, 544: if field_type.value_type() != Type
      #21    assistant Bash       Bash T=$(echo ~/.cargo/registry/src/*/tantivy-0.26.2) ech…
      #22    assistant Bash       Bash T=$(echo ~/.cargo/registry/src/*/tantivy-0.26.2); se…
      #24    assistant Bash       Bash T=$(echo ~/.cargo/registry/src/*/tantivy-0.26.2); se…
      #25    assistant Bash       Bash T=$(echo ~/.cargo/registry/src/*/tantivy-0.26.2); gr…
```

The four indented `#NN` lines under the snippet are the surrounding turns — two before, two
after — each collapsed to one line.

`--context turn` asks for a different window: the hit's whole enclosing turn, from the prompt
that opened it to the last thing that came back before the next one. A fixed `N` is the wrong
shape twice over — on a hit inside a forty-call turn it shows three neighbouring `Bash` calls
and never the prompt that explains them, and on a short turn it drags in the turns either side:

```bash
session-search search "transcript" --limit 1 --context turn --sidechains-only
```

```
1 of 2 hits · 6 ms

▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a10845c5ff9c7d4ec · Explore  (1 hit)
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:09

   1. 19:09:19  assistant  #6  2.51
      I'll start by exploring the directory structure and the **transcript** store
      turn #0 · 11 docs
      #0     user                 Read-only investigation. Goal: exhaustively characterize the on-d…
      #1     attachment           environment <system-reminder> # Environment You have been invoked…
      #2     attachment           model <system-reminder> You are powered by the model named Opus 5…
      #3     attachment           session_context <system-reminder> As you answer the user's questi…
      #4     attachment           date <system-reminder> Today's date is 2026-09-09. </system-remin…
      #5     attachment           remote_session_change <system-reminder> Attribution for git commi…
      #7     assistant Bash       Bash ls -la /home/PLACEHOLDER/.claude/ 2>&1 | head -50; echo "===…
      #8     assistant Bash       Bash ls -la /home/PLACEHOLDER/.claude/sessions/ /home/PLACEHOLDER…
      #9     assistant Bash       Bash ls -la /usr/lib/node_modules/ /opt 2>&1 | head -40; echo "==…
      #10    attachment           plan_mode <system-reminder> Plan mode is active. The user indicat…
```

`show` has the same window under `--turn`, which snaps `--around` to the turn instead of
counting documents out with `--before`/`--after`:

```bash
session-search show b20208d8 --agent a10845 --around 7 --turn --limit 3
```

```
▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a10845c5ff9c7d4ec · Explore
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:09
▌ turn #0 · 3 of 11 docs

#0     19:09:19  user
      Read-only investigation. Goal: exhaustively characterize the on-disk Claude Code session…
…
```

**A turn is not a bounded thing, so the window is capped.** One prompt can spawn hundreds of
tool calls over an hour, and a subagent transcript is a *single* turn — its `user` records are
synthesised by the parent, so there is no human prompt inside it to end one. `search --context
turn` stops at 200 documents per hit and `show --turn` at its own `--limit`, and both say so:
`turn #0 · 3 of 11 docs` is three shown out of eleven. In `--json` the same fact rides on the
hit as `context_turn` (`{turn_seq, shown, docs_in_turn, truncated}`), and on `show --turn` as
`turn`.

Or replay a whole session — an id prefix is enough, as long as it is unambiguous:

```bash
session-search show b20208d8 --limit 1
```

```
▌ b20208d8-fbdb-5918-ba69-d203de6ed6dc
▌ /home/user/session-search · claude/rust-mcp-session-indexing-67tza2 · 2026-09-09 19:07

#0     19:07:15  user     
      Create a rust MCP server that indexes your claude code sessions and allows agents to
      search transcripts
      
      Should support facets like project / directory, tools and their parameters, basically
      anything that’s useful
      
      Start with a functioning cli for search / index and then we will add an MCP command
      later
      
      Delegate to subagents in a dynamic workflow
```

Raise `--limit` to replay more of it; `show <ID> --around <SEQ|UUID>` prints a window instead.

### Images are described, not indexed

Transcripts carry images inline, in the same fields prose lives in: a pasted screenshot is
~300 KB of base64 on the line next to the sentence about it, a `Read` of a PNG comes back as
base64, and a `Bash` command can put image bytes straight on `stdout`. None of it is text — it
matches no query anyone would type, it dilutes the term statistics that rank the documents that
*are* text, and it costs its own size again in the index.

So the payload never reaches the index and a description of it does:

```
#0     01:20:33  user
      [image/jpeg 230 KiB]
      Attached a picture so it’d be in the session
```

The media type and size stay searchable (`session-search search 'image/jpeg'`), the file path
stays where it always was — on the tool call, in `tool_input.file_path` — and the bytes are
gone. On one session with a single pasted photo the index went from 720 KB to 420 KB.

---

## Facets: aggregation over tool parameters

This is the feature that justifies building an index instead of writing a `jq` one-liner.

Every tool call's `input` object is indexed into a single Tantivy JSON field that is
simultaneously **searchable and fast (columnar)**. That means you can aggregate on
`tool_input.<anything>` — a subpath that was **never declared in the schema** and that this tool
has never heard of. Any parameter of any tool, including MCP tools you install tomorrow, becomes
a countable dimension for free.

Start with the shape of the work:

```bash
session-search facets tool_name
```

```
tool_name  13 values · 756 of 978 matching docs have a value
  Bash              624  ████████████████████████████████████████
  Edit               75  █████
  Read               24  ██
  Write              10  █
  WebFetch            8  █
  ToolSearch          5  █
  AskUserQuestion     2  █
  StructuredOutput    2  █
  Agent               2  █
  Skill               1  █
  TaskList            1  █
  ExitPlanMode        1  █
  Workflow            1  █
```

Now descend into a *parameter*. Which files does this project actually churn?

```bash
session-search facets tool_input.file_path --top 10
```

```
tool_input.file_path  showing 10 of ~19 values · 109 of 978 matching docs have a value
  /home/user/session-search/src/parse.rs            27  ████████████████████████████████████████
  /home/user/session-search/src/index.rs            21  ███████████████████████████████
  /home/user/session-search/src/search.rs           12  ██████████████████
  /home/user/session-search/docs/DESIGN.md           9  █████████████
  /home/user/session-search/src/format.rs            8  ████████████
  /home/user/session-search/src/cli.rs               7  ██████████
  /home/user/session-search/docs/TRANSCRIPT-FORMA…   6  █████████
  /home/user/session-search/src/context.rs           4  ██████
  /root/.claude/plans/wild-spinning-puppy.md         2  ███
  /home/user/session-search/README.md                2  ███
```

`file_path` is a `Read`/`Write`/`Edit` parameter. `command` is a `Bash` parameter. Same query,
different subpath — and it composes with the ordinary filters:

```bash
session-search facets tool_input.command --tool Bash --top 8
```

```
tool_input.command  showing 8 of ~622 values · 624 of 624 matching docs have a value
  cargo check --all-targets 2>&1 | tail -20         2  ████████████████████████████████████████
  cargo check --all-targets 2>&1 | tail -40         2  ████████████████████████████████████████
  cargo build --release 2>&1 | tail -5              2  ████████████████████████████████████████
  bash /tmp/claude-0/-home-user-session-search/b2…  2  ████████████████████████████████████████
  cargo build --lib 2>&1 | grep -E "^(error|warni…  2  ████████████████████████████████████████
  for c in index search facets show sessions stat…  1  ████████████████████
  S=/tmp/claude-0/-home-user-session-search/b2020…  1  ████████████████████
  S=/tmp/claude-0/-home-user-session-search/b2020…  1  ████████████████████
  note: ~622 distinct values across 624 docs — this field is search-shaped, not facet-shaped.
        try:  search 'tool_input.command:"<text>"'
```

That `note:` line is the tool telling you it is the wrong instrument. Shell commands are
near-unique strings — 622 distinct values across 624 calls — so bucketing them returns a *list*,
not a distribution. Compare `file_path` above, where paths genuinely repeat and the counts mean
something. When a field is a long tail like this, search it instead:

```bash
session-search search 'tool_input.command:"cargo test"'
```

`tool_input` is full-text indexed, so every parameter is searchable whether or not it is worth
faceting. No "executable name" is pattern-matched out of a command line, because `FOO=bar cmd`,
`cd x && cargo build`, subshells and quoting all defeat that, and a wrong bucket is worse than no
bucket. `Bash` calls get an actual shell parse into a separate field instead:
[Bash commands, parsed](#bash-commands-parsed).

Numeric parameters work exactly the same way — here, the timeouts the agent picked for its Bash
calls:

```bash
session-search facets tool_input.timeout
```

```
tool_input.timeout  5 values · 210 of 978 matching docs have a value
  600000  189  ████████████████████████████████████████
  300000   18  ████
  420000    1  █
  540000    1  █
  180000    1  █
```

Facets also take a `--query`, which restricts the counted set to matching documents — "when the
work was about tantivy, what was I doing?":

```bash
session-search facets tool_name --query "tantivy"
```

```
tool_name  10 values · 195 of 236 matching docs have a value
  Bash              157  ████████████████████████████████████████
  Read               16  ████
  Edit                7  ██
  Write               5  █
  WebFetch            4  █
  AskUserQuestion     2  █
  StructuredOutput    1  █
  ExitPlanMode        1  █
  Workflow            1  █
  Agent               1  █
```

The declared fast fields — `tool_name`, `project`, `model`, `git_branch`, `role`, `kind`,
`agent_type`, `entrypoint` — work as facet fields too:

```bash
session-search facets agent_type
```

```
agent_type  2 values · 895 of 978 matching docs have a value
  workflow-subagent  789  ████████████████████████████████████████
  Explore            106  █████
```

```bash
session-search facets kind
```

```
kind  2 values · 978 of 978 matching docs have a value
  tool_call  756  ████████████████████████████████████████
  message    222  ████████████
```

And `search --facets a,b` returns hits and aggregations in one pass, so a single call answers
"show me the top matches *and* the distribution behind them".

> **Colons.** `tool_input` is a JSON field in the default search fields, so `word:value` is a
> lookup on that JSON subpath — which is why `command:cargo` works as shorthand for
> `tool_input.command:cargo`. A colon followed by whitespace or `/` is treated as ordinary
> punctuation instead, so `https://github.com` and `note: this` search as text. If an
> unqualified `word:value` returns nothing, the tool says so on stderr rather than letting a
> misread colon look like an empty corpus; quote the term to force a literal search.

> One asymmetry worth knowing: **facets key on the raw, untokenized value; search matches words
> inside it.** So `facets tool_input.command` shows whole command lines, while
> `search --tool-input command=cargo` matches any command *containing* `cargo`. That is
> deliberate — it is what makes both the counting and the searching useful.

> The facet header reads `showing N of ~D values · V of M matching docs have a value`. `M` is the
> size of the match set, `V` how many of those carry the field at all, and `~D` an estimate of the
> distinct values (HyperLogLog, hence the tilde). Summing the printed rows is **not** the total —
> on a long-tail field the visible rows can be a fraction of a percent of the matches. The JSON
> carries the same four numbers as `matching_docs`, `docs_with_value`, `other_docs` and `distinct`.

---

## Bash commands, parsed

`tool_input.command` is one long string, which is exactly why faceting it hands back a list
instead of a distribution. So every `Bash` tool call carries a second field, `bash_cmd`, holding
the command after it has been through a real shell parser
([`brush-parser`](https://crates.io/crates/brush-parser), wrapped in `src/bash.rs`). The blocks
in this section were captured later than the ones above, against a fresh index of this machine, so
the session ids and the totals differ:

```bash
session-search search 'bash_cmd.args:"--release"' --limit 1 --json |
  jq '.hits[0] | {command: .tool_input.command, bash_cmd}'
```

```json
{
  "command": "cargo build --release 2>&1 | tail -5",
  "bash_cmd": {
    "args": [
      "build",
      "--release",
      "-5"
    ],
    "program": [
      "cargo",
      "tail"
    ]
  }
}
```

`program` is the `argv[0]` of every *simple command* in the script; `args` is every suffix word of
every one of those commands, flags included, flattened in the same order. That one line is two
commands, so it contributes two programs, and the arguments of both land in `args`.

"Every simple command" is meant literally: both sides of a pipeline, every link of an `&&` or
`||` chain, the bodies of `if`, `while`, `until`, `for` and `case`, subshells and brace groups,
function bodies, coprocesses and process substitutions. One tool call that makes a directory,
writes a file through a heredoc, patches it with `sed` and runs the tests reports all of it:

```bash
session-search search 'bash_cmd.args:"--nocapture"' --program mkdir --limit 1 --json |
  jq -c '.hits[0].bash_cmd.program'
```

```json
["mkdir","cat","cd","sed","cargo","tail"]
```

Words are the raw text of the script with one layer of matching outer quotes removed, and nothing
else is done to them. `"*.snap*"` is indexed as `*.snap*`, and `'"command":"[^"]*"'` keeps its
inner double quotes. Variables are **not** expanded and a `$(…)` or backtick substitution is left
as opaque text, because a transcript records what was typed, not what the shell made of it at the
time. The examples in this section were themselves run through a `B=./target/release/session-search`
shorthand, and the index has them under the program `$B`, exactly as written:

```bash
session-search search 'bash_cmd.program:"$B"' --limit 0
```

```
0 of 4 hits · 1 ms
```

Three things are deliberately not args: assignment prefixes (`FOO=bar cmd` gives the program `cmd`
and no argument), redirect operators and their targets, and heredoc bodies. The call above is
`cat > …/src/bash.rs <<'RSEOF'` followed by the file body, and its `args` contain neither the
redirect target nor a word of the heredoc. A command the shell grammar rejects, an unterminated
quote for instance, gets no `bash_cmd` at all rather than a guess. There is no heuristic fallback, so
every value you see came out of a script that really parsed.

### Counting programs

```bash
session-search facets bash_cmd.program --top 8
```

```
bash_cmd.program  showing 8 of ~24 values · 120 of 207 matching docs have a value
  grep     42  ████████████████████████████████████████
  echo     37  ███████████████████████████████████
  sed      34  ████████████████████████████████
  cargo    28  ███████████████████████████
  python3  27  ██████████████████████████
  head     26  █████████████████████████
  tail     25  ████████████████████████
  cat      14  █████████████
```

Unlike whole command lines, program names repeat, so this is a distribution rather than a listing:
the same question asked of `tool_input.command` is the long tail in the section above.

Two numbers in that header need care. The bucket counts tally *values*, not documents:
`bash_cmd.program` is multi-valued, so a script that runs six programs contributes six, and the
rows above sum well past the 120 documents that carry the field. And the right-hand number is the
whole match set, every indexed document whether it is a Bash call or not, because nothing narrowed this facet.
Add `--program` or `-t Bash` to make that denominator Bash calls.

### Filtering

`--program NAME` keeps documents in which any simple command ran that program. Repeat the flag for
OR, and it ANDs with every other filter, exactly like `--tool` or `--since`.

```bash
session-search search --program cargo --limit 2
```

```
2 of 28 hits · 1 ms

▌ d0fa9bec-abe1-51fb-aed6-b510f75ea5dd  agent a1b37fc38b2bdf0f7 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/search-retrieval-improvements-g5z0mb · 2026-09-09 23:46

   1. 23:46:20  assistant Bash command=cargo build --release 2>&1 | tail -5  descr…  #12  3.05
      Bash cargo build --release 2>&1 | tail -5 Build release binary Compiling
      tracing-subscriber v0.3.23 Compiling clap v4.6.6 Compiling owo-colors v4.4.0 Compiling
      session-search v0.1.0 (/home/user/session-search) Finished `release` profile [op…

▌ d0fa9bec-abe1-51fb-aed6-b510f75ea5dd  agent ac29c4862275df7d6 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/search-retrieval-improvements-g5z0mb · 2026-09-09 23:30

   2. 23:30:22  assistant Bash command=mkdir -p /tmp/claude-0/-home-user-session-s…  #16  3.05
      Bash mkdir -p
      /tmp/claude-0/-home-user-session-search/d0fa9bec-abe1-51fb-aed6-b510f75ea5dd/scratchp…
      && cat > /home/user/session-search/src/bash.rs <<'RSEOF' //! probe #[cfg(test)] mod
      probe { #[test] fn dump() { let cases = [ "cd X && car…
```

That is the question a substring filter cannot answer honestly. `--limit 0` prints the totals and
no hits, so the two are easy to compare:

```bash
session-search search --tool-input command=cargo --limit 0
session-search search --program cargo --limit 0
```

```
0 of 55 hits · 2 ms
0 of 28 hits · 1 ms
```

The 55 are calls whose command *text* contains `cargo` anywhere: a `grep` or a `sed` over a path
under `~/.cargo/registry`, a heredoc quoting an example command line. The 28 ran it.

The filter narrows a facet too, so "what am I passing cargo" is one call:

```bash
session-search facets bash_cmd.args --program cargo --top 8
```

```
bash_cmd.args  showing 8 of ~98 values · 28 of 28 matching docs have a value
  test           19  ████████████████████████████████████████
  --lib          14  █████████████████████████████
  --             12  █████████████████████████
  -               8  █████████████████
  fmt             8  █████████████████
  --all-targets   5  ███████████
  -5              5  ███████████
  warnings        5  ███████████
```

With one caveat worth stating plainly: the filter selects *documents*, and `args` is flattened
across the whole script, so this counts every argument of every command in the scripts that ran
cargo, not only the arguments cargo itself was given. `-5` is the `| tail -5` on the end of those
lines and `-` is a `python3 -` earlier in them, while `--` and `warnings` really are cargo's, out
of `cargo clippy --all-targets -- -D warnings`. `bash_cmd` is a per-document summary, not a
per-command index.

### Exact, not tokenized

`bash_cmd` is indexed with the `raw` tokenizer, unlike `tool_input`. Values match whole and
case-sensitively, which is what makes a bare flag a searchable thing at all:

```bash
session-search search 'bash_cmd.args:"--release"'
```

```
1 of 1 hit · 1 ms

▌ d0fa9bec-abe1-51fb-aed6-b510f75ea5dd  agent a1b37fc38b2bdf0f7 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/search-retrieval-improvements-g5z0mb · 2026-09-09 23:46

   1. 23:46:20  assistant Bash command=cargo build --release 2>&1 | tail -5  descr…  #12  7.55
      Bash cargo build --release 2>&1 | tail -5 Build release binary Compiling
      tracing-subscriber v0.3.23 Compiling clap v4.6.6 Compiling owo-colors v4.4.0 Compiling
      session-search v0.1.0 (/home/user/session-search) Finished `release` profile [op…
```

Quote the value. Unquoted, the query parser reads the leading `-` of `--release` as a `NOT`, the
query is a syntax error, and you land in the lenient retry with a warning on stderr.

`bash_cmd.args:release` is a *different* query, and on this corpus it returns a different single
hit: a `grep -ln "release" tests/fixtures/*.jsonl`, whose argument really is the bare word
`release` once the quotes came off. `bash_cmd.program:Cargo` and `bash_cmd.program:CARGO` return
nothing at all. That is the mirror image of the `tool_input` asymmetry noted above, and it is the
point: `tool_input` is for finding text, `bash_cmd` is for counting facts.

### One rebuild on upgrade

`bash_cmd` changed the shape of a document, so `state.json`'s `version` went from 2 to 3; the
markdown split took it to 4, and `turn_seq` with the context header to 5. The first run of a
build whose version differs sees the mismatch, throws the watermarks away and reindexes every
transcript from byte zero. Nothing is asked of you and `index --full` is not needed; it costs
one full pass, 76 ms for the 193 documents on this machine.

---

## CLI reference

Generated from `--help`, from a build with all features on.

```
Search Claude Code session transcripts

Usage: session-search [OPTIONS] <COMMAND>

Commands:
  index     Index (or re-index) transcripts
  search    Full-text search
  facets    Count values of a fast field or any `tool_input.<path>`
  show      Print a session, or the turns around one hit
  sessions  List indexed sessions, most recent first
  stats     Index statistics, read from the index directory without touching it
  serve     Serve the index over HTTP (and, with the `web-ui` feature, the browser UI)
  help      Print this message or the help of the given subcommand(s)

Options:
      --index <DIR>  Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                     SESSION_SEARCH_INDEX=]
  -v, --verbose...   Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                     trace)
      --no-color     Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty stdout
  -h, --help         Print help
  -V, --version      Print version
```

`serve` is the only line above that a default build does not have: it comes with the `http-api`
feature, and a binary built without it has six commands, not seven. Everything else on this page
is in every build. See [The web UI and the HTTP API](#the-web-ui-and-the-http-api) for what
`serve` does and how to build it in.

### Filters

Accepted by `search`, `facets` and `sessions`:

```
  -p, --project <PATH>          Project path; matches by prefix, so `-p ~/code` catches
                                subdirectories
  -t, --tool <NAME>             Tool name; repeatable
      --tool-input <KEY=VALUE>  Tool parameter filter as `key=value`, e.g. `--tool-input
                                command=cargo`; repeatable
      --tool-output <TEXT>      Phrase the tool's *output* must contain, e.g. `--tool-output "No
                                such file"`; repeatable, and ANDed
      --lang <LANG>             Fenced-code language, as written in the info string (`rust`,
                                `bash`); repeatable
      --min-thinking <N>        Only turns where the model spent at least N thinking tokens. Works
                                even where the thinking text itself was stripped before it reached
                                disk, which is the case for remote and web sessions
      --program <NAME>          Program run by a Bash command — any simple command in the script,
                                e.g. `--program cargo`; repeatable, OR
      --branch <BRANCH>
      --model <MODEL>
      --role <ROLE>
      --kind <KIND>             `message` or `tool_call`
      --session <SESSION_ID>
      --agent-type <TYPE>
      --since <WHEN>            RFC3339, `YYYY-MM-DD`, or a relative span such as `7d`
      --until <WHEN>
      --errors-only
      --no-sidechains
      --sidechains-only
```

`--project` and `--session` match by prefix; the rest are exact.

`sessions` filters `sessions.json` rather than the index, so the per-message filters do not apply
there and are ignored with a warning on stderr:

```
$ session-search sessions --tool Bash --kind message --limit 1
WARN not applicable to `sessions` (session metadata has no per-message fields); ignored filters=--tool, --kind
b20208d8-fbdb-5918-ba69-d203de6ed6dc  wild-spinning-puppy  agent a02e0e345842f6efc · workflow-subagent
  2026-09-09 20:43 · 8 msg · 18 tools · /home/user/session-search · claude/rust-mcp-session…
  harden:docs
```

The ignored set is `--tool`, `--tool-input`, `--tool-output`, `--program`, `--lang`, `--model`,
`--role`, `--kind`, `--errors-only`.

### `index`

```
Index (or re-index) transcripts

Usage: session-search index [OPTIONS]

Options:
      --full                Ignore the watermarks and rebuild every file from scratch
      --index <DIR>         Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                            SESSION_SEARCH_INDEX=]
      --root <DIR>          Transcript root; repeatable. Defaults to `$CLAUDE_CONFIG_DIR/projects`
  -v, --verbose...          Raise the log level on stderr; repeatable (`-v` info, `-vv` debug,
                            `-vvv` trace)
      --jobs <N>            Parser threads. Defaults to the rayon pool size
      --no-color            Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty
                            stdout
      --include-thinking    Also index assistant thinking blocks. This is an index-time choice:
                            switching it on later needs `index --full`, since the watermarks say
                            nothing changed
      --no-spilled-results  Do not follow `Full output saved to: <path>` pointers into
                            `tool-results/`; index only the "output too large" stub the transcript
                            carries inline
  -h, --help                Print help
```

### `search`

```
Full-text search

Usage: session-search search [OPTIONS] [QUERY]

Arguments:
  [QUERY]  Query string: words, "phrases", AND/OR/NOT, `field:value`

Options:
      --facets <FIELD>    Comma-separated facet fields, e.g.
                          `tool_name,code_lang,tool_input.file_path`
      --index <DIR>       Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                          SESSION_SEARCH_INDEX=]
      --context <N|turn>  Also show N documents either side of each hit, or `turn` for the hit's
                          whole enclosing turn — the prompt that opened it, what was tried, and what
                          came back [default: 0]
  -v, --verbose...        Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                          trace)
      --limit <N>         [default: 20]
      --no-color          Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty
                          stdout
      --offset <N>        [default: 0]
      --json              One JSON object on stdout instead of the human rendering
      --no-refresh        Skip the incremental index refresh that normally runs first
      --include-thinking  Search assistant thinking blocks too
      --sort <ORDER>      Hit order. Relevance is meaningless without a query, so a filter-only
                          search is worth ordering by time [default: relevance] [possible values:
                          relevance, newest, oldest]
  -h, --help              Print help
```

Plus the filter block above.

#### `--sort relevance|newest|oldest`

Relevance is the default and is the right answer whenever you typed a query. It is worth nothing
at all when you did not. A filter-only browse — every `Read` on this branch, say — hands every
matching document the identical score, and what comes back is whatever order the index happened to
be in:

```bash
session-search search "" -t Read --limit 3            # --sort relevance, the default
```

```
3 of 20 hits · 7 ms

▌ aa7b6a5b-8ebc-5e23-b445-77c0f55a464a  agent a21eddf59f0012ffe · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/index-search-web-ui-ukm1ho · 2026-09-10 01:45

   1. 01:45:10  assistant Read file_path=/home/user/session-search/docs/WEB-UI.md  #7  2.45
      Read /home/user/session-search/docs/WEB-UI.md

▌ aa7b6a5b-8ebc-5e23-b445-77c0f55a464a  agent a804502e59f2148a1 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/index-search-web-ui-ukm1ho · 2026-09-10 01:58

   2. 01:58:12  assistant Read file_path=/home/user/session-search/docs/DESIGN.md  #8  2.45
      Read /home/user/session-search/docs/DESIGN.md

▌ aa7b6a5b-8ebc-5e23-b445-77c0f55a464a  agent ac197025e9eb0a521 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/index-search-web-ui-ukm1ho · 2026-09-10 01:30

   3. 01:30:30  assistant Read file_path=/home/user/session-search/docs/WEB-UI.md  #7  2.45
      Read /home/user/session-search/docs/WEB-UI.md
```

Three identical `2.45`s, and a first hit from 01:45 sitting above one from 01:58. `--sort newest`
answers the question that was actually being asked:

```bash
session-search search "" -t Read --sort newest --limit 3
```

```
3 of 20 hits · 7 ms

▌ aa7b6a5b-8ebc-5e23-b445-77c0f55a464a  agent a804502e59f2148a1 · workflow-subagent  (2 hits)
▌ /home/user/session-search · claude/index-search-web-ui-ukm1ho · 2026-09-10 01:58

   1. 01:58:12  assistant Read file_path=/home/user/session-search/docs/DESIGN.md  #8  0.00
      Read /home/user/session-search/docs/DESIGN.md

   2. 01:58:12  assistant Read file_path=/home/user/session-search/docs/WEB-UI.md  #7  0.00
      Read /home/user/session-search/docs/WEB-UI.md

▌ aa7b6a5b-8ebc-5e23-b445-77c0f55a464a  agent a102c9c49bc9ca257 · workflow-subagent  (1 hit)
▌ /home/user/session-search · claude/index-search-web-ui-ukm1ho · 2026-09-10 01:52

   3. 01:52:53  assistant Read file_path=/home/user/session-search/docs/DESIGN.md  #8  0.00
      Read /home/user/session-search/docs/DESIGN.md
```

The score column reads `0.00` under a time order, deliberately: the hits were ranked by a
timestamp, and printing a relevance number that had no part in choosing them would be a lie you
could not check. `--sort oldest` is the same thing pointed the other way, which is how you find
the *first* time you touched something.

The one thing a time order cannot do is order what has no time. Every document built from a
Claude Code transcript carries a timestamp, but a record that ever reaches disk without one has
nothing to be sorted by — it still matches, and still counts towards the total, but where it lands
in a `newest` page means nothing. `--sort relevance` is unaffected.

### `facets`

```
Count values of a fast field or any `tool_input.<path>`

Usage: session-search facets [OPTIONS] <FIELD>

Arguments:
  <FIELD>  `tool_name`, `code_lang`, `project`, `model`, `git_branch`, `role`, `kind`,
           `agent_type`, `entrypoint`, or a JSON path such as `tool_input.file_path` or
           `bash_cmd.program`

Options:
      --index <DIR>    Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                       SESSION_SEARCH_INDEX=]
      --query <QUERY>  Restrict the counted set to documents matching this query
      --top <N>        [default: 20]
  -v, --verbose...     Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                       trace)
      --json
      --no-color       Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty stdout
      --no-refresh
  -h, --help           Print help
```

Plus the filter block above.

### `show`

```
Print a session, or the turns around one hit

Usage: session-search show [OPTIONS] <SESSION_ID>

Arguments:
  <SESSION_ID>

Options:
      --agent <AGENT_ID>   Subagent id, for a sidechain transcript
      --index <DIR>        Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                           SESSION_SEARCH_INDEX=]
      --around <UUID|SEQ>  A doc uuid or a `seq` number; prints a window instead of the whole
                           session
  -v, --verbose...         Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                           trace)
      --no-color           Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty
                           stdout
      --turn               Snap the `--around` window to the enclosing turn instead of counting
                           documents with `--before`/`--after`. Capped by `--limit`, and what the
                           cap left out is reported
      --before <N>         [default: 3]
      --after <N>          [default: 3]
      --limit <N>          [default: 200]
      --json
      --no-refresh
  -h, --help               Print help
```

### `sessions`

```
List indexed sessions, most recent first

Usage: session-search sessions [OPTIONS]

Options:
      --index <DIR>  Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                     SESSION_SEARCH_INDEX=]
      --limit <N>    [default: 50]
      --json
  -v, --verbose...   Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                     trace)
      --no-color     Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty stdout
      --no-refresh
  -h, --help         Print help
```

Plus the filter block above.

### `stats`

```
Index statistics, read from the index directory without touching it

Usage: session-search stats [OPTIONS]

Options:
      --index <DIR>  Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                     SESSION_SEARCH_INDEX=]
      --json
  -v, --verbose...   Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                     trace)
      --no-color     Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty stdout
  -h, --help         Print help
```

`stats` deliberately has no `--no-refresh`: it never opens or refreshes the index at all, it just
reads the state file.

### `--json`

Every read command takes `--json` and emits exactly one JSON object on stdout, machine-first
(logs go to stderr, broken pipes exit cleanly). This is the surface the MCP server will reuse.

```bash
session-search facets tool_input.file_path --top 3 --json
```

```json
{"count":3,"field":"tool_input.file_path","total":16,"values":[{"count":8,"value":"/home/user/session-search/docs/DESIGN.md"},{"count":6,"value":"/home/user/session-search/docs/TRANSCRIPT-FORMAT.md"},{"count":2,"value":"/home/user/session-search/src/index.rs"}]}
```

A search hit carries the full document — every field, the parsed `tool_input` object, and the
marked-up snippet (`raw`, the original JSONL line, is stored but withheld from the payload).
`seq` is the document's ordinal within its transcript file; `turn_seq` is the `seq` of the
document that opened its conversational **turn** — the human prompt the whole exchange answers —
so every message, tool call and result of one turn shares it, and the prompt itself is the
document whose `seq == turn_seq`. The hit below comes from a subagent transcript, whose `user`
records are written by the parent rather than typed by a person, so the whole file is one turn
and `turn_seq` is 0:

```bash
session-search search "aggregation" -t Bash --limit 1 --json --facets tool_name
```

```json
{
  "count": 1,
  "elapsed_ms": 3,
  "facets": {
    "tool_name": [ { "count": 27, "value": "Bash" } ]
  },
  "hits": [
    {
      "agent_id": "a856aaeeb8367435e",
      "agent_type": "Explore",
      "doc_id": "b20208d8-fbdb-5918-ba69-d203de6ed6dc:a856aaeeb8367435e:d3d31e48:47",
      "entrypoint": "remote_mobile",
      "git_branch": "claude/rust-mcp-session-indexing-67tza2",
      "is_error": false,
      "is_meta": false,
      "is_sidechain": true,
      "kind": "tool_call",
      "model": "claude-opus-5",
      "parent_uuid": "9bf35bd0-6cb7-40a7-afec-803cf58e7160",
      "permission_mode": null,
      "project": "/home/user/session-search",
      "role": "assistant",
      "score": 5.602014541625977,
      "seq": 47,
      "session_id": "b20208d8-fbdb-5918-ba69-d203de6ed6dc",
      "slug": "wild-spinning-puppy",
      "snippet": "Check tantivy **aggregation** module availability\n192:pub mod **aggregation**;…",
      "source_path": "/root/.claude/projects/-home-user-session-search/…/agent-a856aaeeb8367435e.jsonl",
      "text": "Bash\ncd /tmp/csr && grep -n \"pub mod aggregation\\|mod aggregation\"…",
      "thinking": null,
      "timestamp": "2026-09-09T19:13:50.791+00:00",
      "timestamp_ms": 1788981230791,
      "tool_input": {
        "command": "cd /tmp/csr && grep -n \"pub mod aggregation\\|mod aggregation\" …",
        "description": "Check tantivy aggregation module availability"
      },
      "tool_name": "Bash",
      "tool_output": "192:pub mod aggregation;\n193:pub mod collector;…",
      "tool_use_id": "toolu_01QtnP3F5Y8o8sPGoePwD6Ug",
      "turn_seq": 0,
      "uuid": "cfbf460d-4921-4c09-b9f2-ff341c7141d6",
      "version": "2.1.266"
    }
  ],
  "total": 27
}
```

(Pretty-printed here; the real output is a single line. `snippet`, `source_path`, `text`,
`tool_output` and `tool_input.command` are truncated with `…` for width — they are complete in
the actual output. This capture, and the search results above it, predate both `bash_cmd` and
the prose/code split described under [Documents](#documents): a `Bash` hit now also carries the
parsed command described in [Bash commands, parsed](#bash-commands-parsed), a hit object also
carries `body`, `code`, `headings` and `code_lang`, `text` is a list of prose blocks rather than
a string, and a snippet is highlighted out of one of those fields rather than out of their
concatenation.)

---

## The web UI and the HTTP API

A terminal is the wrong shape for "show me everything that touched `parse.rs` on this branch, then
let me read what happened either side of the one that failed". That question wants a facet rail you
can click and a hit you can open into the turns around it. So there is a browser UI, and a JSON API
underneath it.

**Both are optional cargo features, and neither is in `default`.** That is a safety decision rather
than a packaging one. This index is a verbatim record of everything you and the model typed — the
key you pasted into a prompt, the `.env` a tool read back, the customer name in a stack trace — and
the server has no authentication of any kind. A default build binds no port, links neither `axum`
nor `tokio`, and does not have `serve` in `--help` at all; you have to ask for it, at build time,
by name.

```bash
cargo build --release --features web-ui     # the UI and the API under it
cargo build --release --features http-api   # the JSON API alone
```

`web-ui` implies `http-api`. The frontend is `include_str!`'d into the binary rather than read from
a directory at run time, so a release build is still one file you can copy anywhere — and the UI
being served cannot drift out of step with the server answering its requests, which produces bugs
that look like API bugs and are not.

### `session-search serve`

```bash
session-search serve
```

```
session-search serving on http://127.0.0.1:7777
  index    /root/.local/share/session-search
  ui       http://127.0.0.1:7777/
  cors     off
```

```
Serve the index over HTTP (and, with the `web-ui` feature, the browser UI)

Usage: session-search serve [OPTIONS]

Options:
      --host <ADDR>       Interface to bind. Anything but a loopback address publishes every
                          transcript this index holds to the network, unauthenticated; the server
                          says so loudly when asked [default: 127.0.0.1]
      --index <DIR>       Index directory. Defaults to `$XDG_DATA_HOME/session-search` [env:
                          SESSION_SEARCH_INDEX=]
      --port <PORT>       [default: 7777]
  -v, --verbose...        Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                          trace)
      --cors <ORIGIN>     Allow browser requests from this origin (`*` for any). Repeatable. Off by
                          default: the bundled UI is same-origin, and only a separately hosted
                          frontend needs this
      --no-color          Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty
                          stdout
      --refresh-secs <N>  Re-index every N seconds while the server runs. 0 (the default) never does
                          [default: 0]
      --no-refresh        Skip the incremental index refresh that normally runs before the port
                          opens
  -h, --help              Print help
```

The default bind is loopback. Binding anything else is allowed — it is your machine — and it is
said once, loudly, on the way up, because "I'll just put it on `0.0.0.0` so I can read it from the
laptop" is a decision worth making on purpose rather than by omission:

```bash
session-search serve --host 0.0.0.0
```

```
2026-09-10T02:08:27.807993Z  WARN bound to a non-loopback address: this server has no authentication and the index holds every prompt, command and tool output verbatim address=0.0.0.0:7777
session-search serving on http://0.0.0.0:7777
  index    /root/.local/share/session-search
  ui       http://0.0.0.0:7777/
  cors     off

  WARNING: 0.0.0.0 is not a loopback address. This server has no authentication, and
           the index is a verbatim record of everything you and the model typed,
           secrets included. Anyone who can reach this port can read all of it.
```

`--refresh-secs N` re-indexes every N seconds for as long as the server runs, which is what you
want when the session you are searching is still being written; it is off by default, and
`POST /api/reindex` does the same thing on demand. `--cors ORIGIN` is only for a frontend you host
somewhere else — the bundled UI is same-origin and needs nothing.

### What the UI does

`http://127.0.0.1:7777/` is one page, no build step and no CDN, dark and light both first-class,
usable at 400px wide.

- **Search and facets side by side.** Type in the box (searches settle 180ms after you stop) and
  the rail on the left counts tool, project, model, role, kind, agent type and git branch over the
  set you are looking at. Clicking a value adds a filter; active filters become chips above the
  results, and a chip is how you take one off again. Each facet carries the same honesty line the
  CLI prints — how many of the matching documents actually carry a value for that field, and how
  many values are not shown — so fifteen visible rows never read as the whole story. (At 400px the
  rail becomes a band above the results rather than a column beside them; nothing is hidden.)
- **Order by relevance, newest or oldest**, the same three the CLI has, for the same reason.
- **Expand a hit into the conversation.** Every result opens in place into the turns around it —
  three either side to start, ten more per press — so you can read the prompt that led to a command
  and the output that came back without losing your result list.
- **Open the whole session.** "open session" on any card slides in a drawer holding that whole
  transcript in `seq` order, subagent sidechains included.
- **Per-tool rendering.** `Bash` is a terminal block, `Read` is numbered source, `Edit` is a
  red/green diff, `TodoWrite` is a checklist, `Task` links to the sidechain it spawned, an
  `mcp__server__tool` splits into a server chip and a tool chip. A tool this build has never heard
  of — and there will be some — falls back to a parameter table rather than a broken card. Any
  document can be flipped to the raw JSONL line it came from.
- **The URL is the search.** Query, filters, sort and page all live in it, so the back button works
  and a search you want to keep is a link you can paste.

The one thing to know about the rendering: the model's prose goes through a deliberately small
markdown subset that escapes first and only then applies formatting, and the only string on the
page handed over as HTML is the server's own snippet. Everything the transcript contains is treated
as text, because a transcript is full of text about HTML.

### The API

Every response is `application/json`; every failure is `{"error":{"status":…,"message":…}}` with
the matching status. Health first, since it is the one call that does not open the index:

```bash
curl -s http://127.0.0.1:7777/api/health
```

```json
{"index_dir":"/root/.local/share/session-search","ok":true,"version":"0.1.0","web_ui":true}
```

`GET /api/search` is the whole search surface as a query string — the same parameter names the CLI
flags use, repeated rather than comma-joined for the repeatable ones (`?tool=Bash&tool=Read`).
Trimmed with `jq` here because a real hit carries the entire document:

```bash
curl -s 'http://127.0.0.1:7777/api/search?q=HyperLogLog&tool=Read&size=1' \
  | jq '{totalResults, sort: .info.sort, elapsedMs: .info.elapsedMs,
         hit: (.results[0] | {id: .id.raw, snippetField: ._meta.snippetField,
                              snippet: .tool_output.snippet})}'
```

```json
{
  "totalResults": 2,
  "sort": "relevance",
  "elapsedMs": 6,
  "hit": {
    "id": "aa7b6a5b-8ebc-5e23-b445-77c0f55a464a:-:700e6900:15",
    "snippetField": "tool_output",
    "snippet": "value fell outside the returned buckets (`sum_other_doc_count`).\n140\t    pub other_docs: u64,\n141\t    /// Approximate count of distinct values (<em>HyperLogLog</em>), over the matching set.\n142\t    pub distinct: Option&lt;u64&gt;,\n143\t}\n144\t\n145\timpl"
  }
}
```

Two things in that snippet are worth pointing at. It is **HTML**: escaped first and marked
afterwards, which is why the match is `<em>HyperLogLog</em>` and the Rust in the same line is
`Option&lt;u64&gt;`. That is the convention Elastic's own snippets follow and what every Search UI
template expects to render, and it is the one string on the page a renderer is allowed to trust —
the `**…**` marking the CLI prints stays on the CLI. Second, it hangs off `tool_output` rather than
`text`, because that is the field it was cut from, and `_meta.snippetField` says so. One query
spans a message body, a tool's output and the model's thinking; a UI that labelled all three "text"
would be telling you something untrue about what matched.

`GET /api/facets/{field}` is the aggregation endpoint, and it returns the honesty numbers spelled
out rather than leaving you to sum the buckets:

```bash
curl -s 'http://127.0.0.1:7777/api/facets/tool_input.file_path?top=3' \
  | jq '{field, matchingDocs, docsWithValue, otherDocs, distinct, hiddenValues,
         values: [.values[] | "\(.count)  \(.value)"]}'
```

```json
{
  "field": "tool_input.file_path",
  "matchingDocs": 362,
  "docsWithValue": 25,
  "otherDocs": 8,
  "distinct": 10,
  "hiddenValues": 7,
  "values": [
    "6  /home/user/session-search/docs/WEB-UI.md",
    "6  /home/user/session-search/docs/DESIGN.md",
    "5  /home/user/session-search/src/api/dto.rs"
  ]
}
```

362 documents matched, 25 of them carry a `file_path` at all, and the three rows shown are three
of ten distinct values — summing them answers nothing. The CLI prints the same four numbers; the
API just names them.

A parameter the endpoint does not know is a `400` that names what it does know, never a silently
ignored one. A typo that quietly widened your search to the whole corpus would be worse than an
error, because you would believe the answer:

```bash
curl -s 'http://127.0.0.1:7777/api/search?toool=Bash'
```

```json
{"error":{"message":"unknown query parameter \"toool\"; this endpoint accepts: q, page, size, offset, sort, facets, facet_top, snippet_chars, include_thinking, project, tool, tool_input, tool_output, min_thinking, branch, model, role, kind, session, agent_type, since, until, errors_only, no_sidechains, sidechains_only","status":400}}
```

The rest: `GET /api/sessions` and `GET /api/sessions/{id}` (plus `/around?seq=`) for listing and
replaying transcripts — an unambiguous id prefix is enough, exactly as `show` accepts one —
`GET /api/stats`, and `POST /api/reindex`.

### Search UI compatible, and that is the only path

`POST /api/search` takes an Elastic [Search UI](https://github.com/elastic/search-ui)
`RequestState` and returns a `ResponseState`. That interface is one function, so pointing a stock
Search UI frontend at this index is a connector of about twenty lines:

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

Which is exactly the request the bundled UI makes on every keystroke — there is deliberately no
second, private endpoint for it. A side door would drift from the documented one and nobody would
notice until somebody else's connector broke.

```bash
curl -s http://127.0.0.1:7777/api/search -H 'content-type: application/json' \
  -d '{"searchTerm":"","filters":[{"field":"tool_name","values":["Write"],"type":"any"}],
       "sortList":[{"field":"timestamp","direction":"desc"}],"resultsPerPage":2}' \
  | jq '{totalResults, totalPages, current, sort: .info.sort, ids: [.results[]._meta.id]}'
```

```json
{
  "totalResults": 2,
  "totalPages": 1,
  "current": 1,
  "sort": "newest",
  "ids": [
    "aa7b6a5b-8ebc-5e23-b445-77c0f55a464a:a102c9c49bc9ca257:061f7aee:34",
    "aa7b6a5b-8ebc-5e23-b445-77c0f55a464a:ac197025e9eb0a521:69089df8:21"
  ]
}
```

`filters` also accepts the native object form the CLI flags spell, since that is what the bundled
UI already has in hand:

```jsonc
{ "filters": { "tool": ["Bash"], "project": "~/code", "errors_only": true } }
```

Either way, a filter field the mapping does not cover is a `400` that names it, and a field that
takes one value given two is a `400` rather than a silent "first one wins". `sortList` accepts
`timestamp` and nothing else, because the index has no other ordering to offer — asking for one it
does not have should not quietly get you relevance instead.

Endpoint by endpoint, the request and response shapes, the full filter-field mapping and the
frontend's DOM and CSS contracts are in [`docs/WEB-UI.md`](docs/WEB-UI.md).

---

## How the index works

### Where it lives

`$XDG_DATA_HOME/session-search`, falling back to `~/.local/share/session-search`. Override with
`--index DIR` or `$SESSION_SEARCH_INDEX`. The Tantivy directory's size fluctuates as segments
merge and old ones are reclaimed.

```
~/.local/share/session-search/
  tantivy/        the Tantivy index    (3.7 MB for 646 documents)
  state.json      per-file watermarks  (2.9 KB)
  sessions.json   session metadata     (13 KB)
```

### Documents

One document per message, one per tool call. That granularity is what makes hits precise and
facets meaningful — a hit points at the exact turn, not at a 400-line session.

A document's body is not one indexed field. A message is markdown, so it is split before
indexing: its prose goes to `text` (analyzed as English, stemmed — one value per block, so no
phrase runs across a block that was removed from between two others), its fenced blocks and
inline spans to `code` (never stemmed), its headings to `headings` (prose, and worth double), and
each fence's language to `code_lang`, which is a facet like `tool_name`. A tool call is not
markdown and is never parsed as one: its name and input strings are `text`, and the file content
of an `Edit` or `Write` is `code`. The body as it was written is kept whole beside them in
`body`, stored and never indexed: that is what `show`, `--context` and `--json` print, so a
message reads exactly as it was written rather than as prose-then-code.

A tool call also holds the **result** that answered it, in a field of its own: `tool_output`,
joined to the call by `tool_use_id` across the records they were written in. It is neither half
of the split — a result is diagnostics and program output, not markdown — so it is analyzed as
code and capped independently of the body. Every one of these fields is searched by a bare
query, and `tool_output:"No such file"` asks about what a tool *returned* rather than what it
was asked to do.

One more field is indexed and stored nowhere: `context_text`, a capped header naming the
session, the project, the branch and the turn a document belongs to. It exists because a
document is a fragment of a conversation and a fragment does not carry its own subject — see
"Contextual BM25" in `docs/DESIGN.md`. Because it is not stored it can never be read back, so
`show`, `--context` and `--json` print exactly what the transcript held.

The index for this project's own development history:

```bash
session-search facets role
```

```
role  4 values · 978 of 978 matching docs have a value
  assistant   860  ████████████████████████████████████████
  attachment   95  ████
  user         18  █
  system        5  █
```

### Incremental

`state.json` records `{size, mtime_ms, byte_offset, docs}` for every transcript file:

```json
{
  "version": 5,
  "files": {
    "/root/.claude/projects/-home-user-session-search/b20208d8-….jsonl": {
      "size": 1029179,
      "mtime_ms": 1788982251799,
      "byte_offset": 1029179,
      "docs": 62
    }
  }
}
```

- Same size **and** mtime → the file is skipped entirely.
- Grew → seek to `byte_offset` and parse only the tail.
- Shrank, or mtime went backwards → delete that file's documents and reparse it whole.

Because sessions are live and appended to *while you read them*, the parser stops at the last
**complete** line and leaves a partial trailing line unconsumed for the next run. Transcripts are
read through a `BufReader`, never `mmap` — a truncation under a mapping raises an uncatchable
`SIGBUS`.

Parsing runs across files with `rayon`; a single writer consumes the results and there is exactly
one `commit()` per run.

A malformed line is counted as a `parse error`, never a failure — the format is a moving target
and the parser's governing rule is *tolerate everything, require nothing*
(see [`docs/TRANSCRIPT-FORMAT.md`](docs/TRANSCRIPT-FORMAT.md)).

### Rebuilding

```bash
session-search index --full
```

```
  files scanned          11
  files updated          11
  files reset            11
  documents added       640
  documents deleted     639
  sessions               11
  parse errors            0
  elapsed            119 ms
```

You need `--full` after changing an index-time decision — most importantly `--include-thinking`,
because the watermarks correctly report that nothing on disk has changed.

To start completely over, delete the index directory. Nothing in it is precious; it is derived
entirely from `~/.claude/projects/`.

### Session metadata is not in Tantivy

A session's title arrives in a `summary` sidecar record that can be appended long after the
messages it titles. Keeping session metadata in `sessions.json` means a late title update is a
cheap JSON rewrite instead of a document rebuild.

The one thing that bargain costs: the context header above is built when a document is written,
so a title that only arrives afterwards is in `sessions.json` immediately and in that document's
header at the next `--full`. The session's opening prompt is in the header from the first parse,
which is the half that does the work.

---

## What's next: the MCP server

The CLI is stage one. Stage two is an `mcp` subcommand that serves the same operations to agents
over stdio, so Claude Code can search its own history mid-task: one `#[tool]` per subcommand,
taking and returning the *same* structs the CLI already derives. `search.rs` was written with that
in mind — `Filters` derives `clap::Args` and `serde::Deserialize` side by side over plain
`Option<String>`/`Vec<String>` fields, `SearchRequest` is pure serde data, and `--json` already
emits the exact payloads the tools will return.

The [`http-api` feature](#the-web-ui-and-the-http-api) is the first instalment of that plan, and
it worked: `serve` decodes a request into the same `SearchRequest` the CLI builds, hands it to the
same `search::search`, and renders the same `SearchResponse`. So the MCP server is a third front
end over one struct rather than a third implementation of search — and the wire shapes the HTTP
API had to pin down (a hit's document, a facet's honesty numbers, a sort that is not relevance)
are the ones the tools will return.

Design note: [`docs/MCP.md`](docs/MCP.md). The HTTP one is
[`docs/WEB-UI.md`](docs/WEB-UI.md).

---

## Limitations

Honest ones, in rough order of how likely they are to bite you.

**Indexing a session while it is live used to over-count. It no longer does, and that is
measured rather than asserted.** An incremental run stops wherever the writer happened to be —
almost always between a `tool_use` and the `tool_result` that answers it, sometimes inside one
API message's block records. Both cases used to re-emit work the previous run had already
indexed: replaying one real 147-line transcript one line at a time produced **88 documents where
a single pass produced 49**, and reported 78 tool calls where there were 39.

The fix is a small per-file *carry* in `state.json` (`parse::ParseCarry`): the ids — and the
documents — a run left unfinished, plus a fingerprint of the last line it consumed. A later tail
uses it to *complete* the waiting document (same `doc_id`, same `seq`, the result added to its
`code`)
instead of inventing a second half-empty one, and to recognise a `message.id` it has already
counted. `src/index.rs` carries the test:

```
$ cargo test --release -- --ignored --nocapture replaying_a_real_transcript
replaying .../subagents/workflows/wf_c5a41e2f-a3e/agent-a08066b1dd9ff7583.jsonl
  589 lines: incremental live=207 one-shot=207
  .../sess-replay.jsonl: msgs 35 vs 35, tools 161 vs 161
test index::tests::replaying_a_real_transcript_line_by_line_matches_a_single_pass ... ok
```

The assertion is not just the document count: it compares every document body, so a live index
and a rebuilt one are byte-identical.

**The one case the watermark still cannot see** is a transcript rewritten in place, to the same
byte length, within the same millisecond as the run that indexed it. `size` + `mtime_ms` is the
skip test, and that rewrite is indistinguishable from no change at all. Any rewrite a real editor
or the CLI performs lands in a later millisecond and is detected by the line fingerprint, which
forces a full reparse of the file.

**A very large unanswered tool call loses its result until the next rebuild.** The carry holds
the waiting document so a late result can complete it, but it refuses to hold one over 128 KiB —
`state.json` is rewritten on every run and must not grow to contain a copy of an enormous `Write`
payload. Such a call is still indexed exactly once, with its name and input; only the result text
waits for `index --full`.

**`stats` reuses the indexer's stat block.** It prints `files updated 0`, `documents deleted 0`
and `elapsed 0 ms` — meaningless for a static read — and labels the index's document count as
`documents added`.

**Thinking is indexed but not searched by default.** `index` stores thinking blocks unless you
pass `--no-thinking`; `search --include-thinking` opts a query into them. So the common case needs
no rebuild — only opting *out* and back in does.

Remote and web sessions strip thinking text before it reaches disk: the block survives with its
`signature` intact but `"thinking": ""`, so on those machines there is nothing to index. Empty
blocks are skipped rather than indexed as blank documents — in a corpus with 453 stripped blocks,
the index holds none of them. What *does* survive is the cost: `thinking_tokens` is indexed as a
fast field, so `--min-thinking N` and `facets thinking_tokens` still find the turns where the
model stopped to reason, even when you cannot read what it reasoned about.

**Changing what is indexed needs `index --full`.** The watermarks say a file is unchanged, so
they will not re-read it. This applies to `--no-thinking`, `--no-spilled-results`, and to any
release that changes how a document body is built — for instance the one that split a tool
call's result out of `text` into its own `tool_output` field, or the one that split a message's
markdown across `text`, `code` and `headings`. Changing an analyzer forces the rebuild by
itself: a field's tokenizer name is part of the schema, so the index is discarded and refilled
on the next run.

**Snippet markers can collide with the text.** Matches are wrapped in `**…**`; if the indexed text
already contains `**` (this tool's own Markdown output, for instance) you will see `****term****`.
Colour output makes it unambiguous; `--no-color` does not.

**Context windows assume dense `seq`.** `show --around` and `search --context N` walk `seq` numbers
within a file. If a re-index ever leaves a hole, the window comes back short rather than erroring.
`turn_seq` inherits that assumption — a turn is a contiguous range of `seq` in one file, not a
list of the documents in it — and it inherits one more: a transcript that opens mid-conversation
after a `resetSessionFile()`, and a subagent transcript whose prompts were written by the parent,
have no human prompt to bound a turn with, so their leading documents all share turn zero.
A turn window (`--context turn`, `show --turn`) does not care: it asks for a `turn_seq` value
rather than a range, so a hole costs it the one document and nothing else.

**Spilled tool results are read from a path found in transcript text.** Oversized tool output is
written to `tool-results/<id>.txt` and the transcript carries a `Full output saved to: <path>`
pointer, which the indexer follows by default (`index --no-spilled-results` opts out). That is
content-directed file I/O — fine for your own `~/.claude` tree, worth remembering if you ever
index transcripts from elsewhere.

**Bodies are capped, and the cut is silent.** Each indexed body field of a document is truncated
at `index --max-text-bytes N`, 1 MiB by default, at a UTF-8 boundary with no marker. The default
is far above anything a transcript carries — Claude Code bounds tool output before it reaches
disk, and on a real corpus the largest inline result measured 18.7 KB — so in practice nothing is
cut. Lower it if you want a `cat` of a minified bundle kept out of the term dictionary. Note that
`raw` and `tool_input` are stored uncapped regardless, so a large input is always searchable
through `tool_input.<key>` even when `text` did not copy all of it.

**The server, if you build it, trusts whoever can reach the port.** `session-search serve` has no
authentication, no accounts and no rate limit, and it is not going to grow them: an index of your
own transcripts on your own loopback interface does not need a login, and anything that does need
one needs more than a flag. That is why the features are off by default, why the bind is loopback
by default, why `--cors` is off by default, and why a non-loopback `--host` prints a warning
rather than a shrug. The one thing it does serialise is writing: `POST /api/reindex` holds a lock
and a second concurrent call gets a `409` instead of two writers racing for the same `IndexWriter`.

**Everything is local and single-user.** No daemon, no watch mode, no incremental commit while a
session is in flight; the index is refreshed at query time. There is no cross-machine sync, and
the ranking tuning amounts to one field boost on markdown headings. Language handling is English
only: prose is stemmed by an English stemmer, and the `code` analyzer described above splits
identifiers and stems nothing.
`serve --refresh-secs N` is the closest thing to a watch mode, and it is a timer rather
than a file watcher — it re-runs the ordinary incremental index every N seconds.

---

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features
cargo build --all-targets                  # default features: the binary people install
cargo test --all-features
```

```
running 415 tests                                            # unittests src/lib.rs
test result: ok. 411 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 4.31s

running 5 tests                                              # tests/eval — the retrieval eval
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.52s
```

The compiler is pinned: [`rust-toolchain.toml`](rust-toolchain.toml) names the version and the
components those commands need. rustup reads it before its own default, so the first `cargo`
command run in this directory installs that toolchain if the machine does not already have it,
and every command after it — yours and CI's alike — is the same compiler and the same clippy. A
lint the runner would fail on therefore cannot hide behind an older toolchain at your end.
Moving to a newer Rust is its own one-line pull request: change `channel`, run the four commands,
and fix whatever the newer clippy has learned to see.

The four ignored tests all need this machine's own `~/.claude/projects` and are the ones worth
running by hand after any change to the parser or the indexer — in particular the live-replay
convergence test, which is the only thing that proves incremental indexing agrees with a rebuild
on real data:

```bash
cargo test --release -- --ignored --nocapture
```

### The retrieval eval harness

`tests/eval/` scores search against a checked-in corpus and a graded query fixture, so a change to
the analyzers, the schema or the query builder can be argued with a number instead of an anecdote.
It runs as part of `cargo test`; on its own:

```bash
cargo test --test eval                    # assert: floors, invariants, committed baseline
cargo test --test eval -- --nocapture     # ...and print the tables
```

Either way it writes four artifacts under `target/eval/` (gitignored):

| file | what it is |
| --- | --- |
| `report.md` | the per-class table and per-query appendix — the thing a pull request pastes |
| `ablation.md` | the same fixture scored with and without the `context_text` header |
| `hits.md` | every query's ranked hits with the grade each one was given |
| `corpus.md` | every document in the corpus, its reference and an excerpt |

The committed baseline is an `insta` snapshot at
`tests/eval/snapshots/eval__baseline_metrics_match_the_committed_table.snap`, so the snapshot diff
*is* the before/after table: change something, run the tests, and read what moved. Re-baseline
with `cargo insta review` (or `INSTA_UPDATE=always cargo test --test eval`) once you believe the
new numbers — and quote them in the pull request, because the snapshot is the record of what the
project thinks retrieval does.

Adding a query means adding a row to `tests/fixtures/eval_queries.json`: a class, the query
string, any filters, and graded relevance keyed by document reference
(`"{session_id}:{agent_id|-}:{seq}"` — read `target/eval/corpus.md` for the current ones). The
harness refuses to run if a reference does not resolve, because a stale reference scores as a
retrieval miss and the two are indistinguishable in a results table.

Design and limitations — including what a 65-document synthetic corpus cannot tell you — are in
[`docs/DESIGN.md`](docs/DESIGN.md) under **Retrieval evaluation**.

### Continuous integration

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs exactly the four commands above, on
the toolchain `rust-toolchain.toml` names, on every push to `main` and every pull request, as a
single Linux job — `fmt`, then `clippy` with `-D warnings`, then `build --all-targets`, then
`test`. The ignored four are not among them: a runner has no `~/.claude/projects`, so they stay a
by-hand check.

The crate resolves `~/.claude` through `$HOME`, which Windows does not set, so Windows is neither
built nor tested. macOS is: `cargo test` runs there weekly (Mondays 07:00 UTC) and on demand via
**Actions → CI → Run workflow**, rather than on every pull request, because macOS runners bill at
ten times the Linux rate on a private repository. It runs `--all-features` too: the server is the
one part of this crate that binds a socket and spawns a runtime, which is exactly the sort of
thing that works on Linux and not on macOS.

[`.github/dependabot.yml`](.github/dependabot.yml) proposes dependency bumps weekly and action
bumps monthly. `Cargo.lock` is committed and every CI command passes `--locked`, so a dependency
moves only in a pull request that has run the whole suite first.

### Cutting a release

[`.github/workflows/release.yml`](.github/workflows/release.yml) is driven by an annotated tag:

```bash
# Cargo.toml must already say 0.1.0 — the workflow refuses a tag that disagrees with it.
git tag -a v0.1.0 -m 'v0.1.0'
git push origin v0.1.0
```

It re-runs `fmt`, `clippy` and `test`, builds `x86_64-unknown-linux-gnu` and
`aarch64-apple-darwin`, checks that each binary starts (`--version`), and publishes a GitHub
Release carrying one `.tar.gz` per target plus a `SHA256SUMS` covering both. Each archive unpacks
into its own directory holding the binary, the README and the LICENSE.

The Linux binary is built on `ubuntu-22.04` rather than the newest image so that it needs only
glibc 2.35 and runs on distributions older than the runner.

Released binaries are **default-feature** builds, so they have no `serve` and no UI in them. The
release job type-checks both feature sets (its `clippy` step passes `--all-features`) but ships the
binary everyone gets from `cargo install`; wanting the web UI still means building it yourself.

**Actions → Release → Run workflow** does everything except publish: the archives land on the
workflow run as artifacts, which is the way to check a packaging change without spending a tag.

Three documents are worth reading before changing anything:

- [`docs/DESIGN.md`](docs/DESIGN.md) — the module contract: pinned type signatures, the Tantivy
  schema and the verified 0.26 API facts, the CLI surface.
- [`docs/TRANSCRIPT-FORMAT.md`](docs/TRANSCRIPT-FORMAT.md) — the input format, reverse-engineered
  from Claude Code v2.1.266: record families, the sidechain layout, the DAG, compaction, and the
  file-level hazards a reader must survive.
- [`docs/WEB-UI.md`](docs/WEB-UI.md) — the wire contract for `http-api` and `web-ui`: every
  endpoint, the Search UI envelope, the filter-field mapping, and the DOM and CSS contracts the
  `web/` modules hold each other to.

## License

MIT. See [`LICENSE`](LICENSE).
