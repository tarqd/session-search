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

No configuration, no daemon, no network access. It reads `~/.claude/projects/` (or
`$CLAUDE_CONFIG_DIR/projects`) and writes one index directory.

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
faceting. Nothing derives an "executable name" facet from a command, because doing that honestly
would mean parsing shell grammar — `FOO=bar cmd`, `cd x && cargo build`, subshells, quoting — and
a wrong bucket is worse than no bucket.

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

## CLI reference

Generated from `--help`.

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

### Filters

Accepted by `search`, `facets` and `sessions`:

```
  -p, --project <PATH>          Project path; matches by prefix, so `-p ~/code` catches
                                subdirectories
  -t, --tool <NAME>             Tool name; repeatable
      --tool-input <KEY=VALUE>  Tool parameter filter as `key=value`, e.g. `--tool-input
                                command=cargo`; repeatable
      --lang <LANG>             Fenced-code language, as written in the info string (`rust`,
                                `bash`); repeatable
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

The ignored set is `--tool`, `--tool-input`, `--lang`, `--model`, `--role`, `--kind`,
`--errors-only`.

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
      --context <N>       Also show N turns either side of each hit [default: 0]
  -v, --verbose...        Raise the log level on stderr; repeatable (`-v` info, `-vv` debug, `-vvv`
                          trace)
      --limit <N>         [default: 20]
      --no-color          Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty
                          stdout
      --offset <N>        [default: 0]
      --json              One JSON object on stdout instead of the human rendering
      --no-refresh        Skip the incremental index refresh that normally runs first
      --include-thinking  Search assistant thinking blocks too
  -h, --help              Print help
```

Plus the filter block above.

### `facets`

```
Count values of a fast field or any `tool_input.<path>`

Usage: session-search facets [OPTIONS] <FIELD>

Arguments:
  <FIELD>  `tool_name`, `code_lang`, `project`, `model`, `git_branch`, `role`, `kind`,
           `agent_type`, `entrypoint`, or a JSON path such as `tool_input.file_path`

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
      --before <N>         [default: 3]
      --no-color           Never colourise. Also honoured: a non-empty `$NO_COLOR`, and a non-tty
                           stdout
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
marked-up snippet (`raw`, the original JSONL line, is stored but withheld from the payload):

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
      "tool_use_id": "toolu_01QtnP3F5Y8o8sPGoePwD6Ug",
      "uuid": "cfbf460d-4921-4c09-b9f2-ff341c7141d6",
      "version": "2.1.266"
    }
  ],
  "total": 27
}
```

(Pretty-printed here; the real output is a single line. `snippet`, `source_path`, `text` and
`tool_input.command` are truncated with `…` for width — they are complete in the actual output.
This capture, and the search results above it, predate the prose/code split described under
[Documents](#documents): a hit object now also carries `body`, `code`, `headings` and
`code_lang`, `text` is a list of prose blocks rather than a string, a tool call's output is in
`code` rather than at the end of `text`, and a snippet is highlighted out of one of those fields
rather than out of their concatenation.)

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
markdown and is never parsed as one: its name and input strings are `text`, while its **output**
— and the file content of an `Edit` or `Write` — is `code`. The body as it was written is kept
whole beside them in `body`, stored and never indexed: that is what `show`, `--context` and
`--json` print, so a message reads exactly as it was written rather than as prose-then-code.

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
  "version": 1,
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

---

## What's next: the MCP server

The CLI is stage one. Stage two is an `mcp` subcommand that serves the same operations to agents
over stdio, so Claude Code can search its own history mid-task: one `#[tool]` per subcommand,
taking and returning the *same* structs the CLI already derives. `search.rs` was written with that
in mind — `Filters` derives `clap::Args` and `serde::Deserialize` side by side over plain
`Option<String>`/`Vec<String>` fields, `SearchRequest` is pure serde data, and `--json` already
emits the exact payloads the tools will return.

Design note: [`docs/MCP.md`](docs/MCP.md).

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

**Thinking is off by default and index-time.** `--include-thinking` on `index` decides whether
thinking blocks are searchable at all; `--include-thinking` on `search` only opts the query into
them. Flipping the index-time one requires `index --full`. Note also that remote sessions strip
thinking text before it reaches disk, so on some machines every thinking block is empty and there
is nothing to index either way.

**Snippet markers can collide with the text.** Matches are wrapped in `**…**`; if the indexed text
already contains `**` (this tool's own Markdown output, for instance) you will see `****term****`.
Colour output makes it unambiguous; `--no-color` does not.

**Context windows assume dense `seq`.** `show --around` and `search --context` walk `seq` numbers
within a file. If a re-index ever leaves a hole, the window comes back short rather than erroring.

**Spilled tool results are read from a path found in transcript text.** Oversized tool output is
written to `tool-results/<id>.txt` and the transcript carries a `Full output saved to: <path>`
pointer, which the indexer follows by default (`index --no-spilled-results` opts out). That is
content-directed file I/O — fine for your own `~/.claude` tree, worth remembering if you ever
index transcripts from elsewhere.

**Everything is local and single-user.** No daemon, no watch mode, no incremental commit while a
session is in flight; the index is refreshed at query time. There is no cross-machine sync, and
the ranking tuning amounts to one field boost on markdown headings. Language handling is English
only: prose is stemmed by an English stemmer, and the `code` analyzer described above splits
identifiers and stems nothing.

---

## Development

```bash
cargo build --all-targets
cargo clippy --all-targets
cargo fmt --check
cargo test
```

```
running 228 tests
test result: ok. 224 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 2.4s
```

The four ignored tests all need this machine's own `~/.claude/projects` and are the ones worth
running by hand after any change to the parser or the indexer — in particular the live-replay
convergence test, which is the only thing that proves incremental indexing agrees with a rebuild
on real data:

```bash
cargo test --release -- --ignored --nocapture
```

Two documents are worth reading before changing anything:

- [`docs/DESIGN.md`](docs/DESIGN.md) — the module contract: pinned type signatures, the Tantivy
  schema and the verified 0.26 API facts, the CLI surface.
- [`docs/TRANSCRIPT-FORMAT.md`](docs/TRANSCRIPT-FORMAT.md) — the input format, reverse-engineered
  from Claude Code v2.1.266: record families, the sidechain layout, the DAG, compaction, and the
  file-level hazards a reader must survive.

## License

MIT. See [`LICENSE`](LICENSE).
