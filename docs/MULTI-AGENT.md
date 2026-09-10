# Multi-agent support — research and plan

Status, September 2026: the seam exists (`src/agent.rs`) and Claude Code is the only adapter
behind it. This document records what was learned about each candidate agent's on-disk session
format, what each one would cost to add, and the order to do it in. Sources are cited at the
end of each section; anything marked *inferred* was not verified against source.

## The seam

Only three modules ever knew the agent was Claude Code: `model.rs`, `discovery.rs`, `parse.rs`.
They now live under `src/agents/claude/`, and the indexer reaches them through the `Agent`
trait:

```rust
pub trait Agent: Send + Sync {
    fn id(&self) -> &'static str;                                  // "claude-code"
    fn default_roots(&self) -> anyhow::Result<Vec<PathBuf>>;
    fn discover(&self, roots: &[PathBuf]) -> anyhow::Result<Vec<SessionFile>>;
    fn parse(&self, path: &Path, from_offset: u64, seq_base: u64,
             opts: &ParseOptions, ctx: &FileContext) -> anyhow::Result<(ParseOutput, u64)>;
}
```

What sits on each side of it:

| shared (`src/doc.rs`, everything downstream) | per adapter (`src/agents/<id>/`) |
| --- | --- |
| `Doc`, `DocKind`, `SessionInfo`, `ParseOutput`, `ParseError`, `ParseOptions`, `FileContext`, `ParseCarry` | raw record types, root discovery, the record → `PartialDoc` walk |
| `PartialDoc` builder (`message`, `tool_call`, `.model()`, `.meta()`, …) | error detection, message counting, tool call ↔ result joining, spill/blob following |
| `agent` schema field: `STRING \| STORED \| FAST`, so `--agent x` and `facets agent` need no further code | `default_roots()` and the `--root AGENT=DIR` id |

Facts that made this cheap, verified in code before the refactor:

- **Context expansion never walks parent links.** `context::around` is a `seq` range query and
  `context::session` is a term query. An agent with no `parentUuid` equivalent loses nothing; it
  only has to emit a dense per-file `seq`.
- **`doc_id` embeds a digest of the source path**, so two agents can never collide on ids.
- **Every Claude-shaped `Doc` field is optional** (`uuid`, `parent_uuid`, `entrypoint`, …), so
  other adapters leave them empty with no schema change.
- **Facet fields are validated against the schema dynamically**, not an allowlist: adding the
  `agent` fast field made it facetable with zero search-side code.
- **A schema change auto-rebuilds the index** (`index::open_or_create`), so adding a field costs
  one full reindex and nothing else. `state.json` is versioned (now 2) for the same reason.

What deliberately did *not* change:

- **Parsing is still byte-offset based.** Every format below except Cursor's IDE store is one
  append-only JSONL file per session, and the watermark model (`size`/`mtime`/`byte_offset` plus
  a tail-line fingerprint) is built on that. Generalising the watermark to an opaque cursor
  (`bytes{offset} | rowid{n} | digest{hash}`) is the most invasive change on this list and is
  only needed for a SQLite source; it is deferred until one is actually being added.
- **`ParseCarry` stays a shared struct** with one new opaque slot, `agent_state:
  serde_json::Value`, for whatever else an adapter needs across a tail boundary. pi is the first
  format that needs it (see below).

Two cross-agent quality questions to settle when the second adapter lands, not before:

1. **`tool_input` facet paths diverge per agent** unless adapters map their primary argument
   onto the same keys where they mean the same thing (`command`, `file_path`, `pattern`).
   Non-object arguments already survive (`schema::doc_to_json` wraps them as `{"value": …}`) but
   land under `tool_input.value`, which makes cross-agent facets lumpy.
2. **`SessionInfo::messages` is defined in Claude Code terms** (human prompts + assistant API
   messages counted once per `message.id` + system records). Each adapter must document its own
   counting rule or cross-agent session listings compare unlike things. `--errors-only` is
   likewise only as good as each adapter's `is_error` heuristic.

## Recommended order

1. ~~The seam refactor with Claude Code as the only adapter, plus the `agent` field.~~ Done.
2. **pi.** Cheapest second format, and it exercises two things the seam has not yet been
   tested against: in-place branching and carried parse state.
3. **Codex.** Same shape as Claude Code with string-encoded tool arguments and compressed cold
   files.
4. **oh-my-pi** as a variant of the pi adapter, not a separate one.
5. **Cursor**, in two halves: the CLI's JSONL transcripts fit the seam as is; the IDE's SQLite
   store needs the watermark generalisation first and is worth its own design pass.

Per-adapter cost, once the seam exists and the format is known: discovery 80–150 lines,
record types 150–300, parser 300–600, fixtures and tests around 300. The highest-value test
investment is making `index.rs`'s convergence tests (a one-shot parse must equal the sum of
incremental tails, byte for byte) generic over the adapter, so every new one inherits the
incremental-correctness gate for free.

---

## pi (badlogic/pi-mono, now under the earendil-works org)

**Verdict: the closest match to Claude Code of anything researched. 2–3 days.**

**Storage.** `~/.pi/agent/sessions/--<encoded-cwd>--/<timestamp>_<session-id>.jsonl`, one
append-only file per session. Base dir overridable with `PI_CODING_AGENT_DIR`; session root
with `PI_CODING_AGENT_SESSION_DIR` or `--session-dir`. The cwd encoding strips the leading
slash and maps `/`, `\`, `:` to `-`, wrapped in `--…--`. The file only appears on disk after
the first assistant message (entries are buffered until then), and is rewritten in place only
on a version migration. `/fork` and `/clone` write a *new* file with a `parentSession` header
pointing at the source.

**Records.** Every line has `type`; all but the header carry `id`, `parentId` (nullable) and
`timestamp`. Header: `{"type":"session","version":3,"id","timestamp","cwd","parentSession"?}`.
Entry types: `message`, `model_change {provider, modelId}`, `thinking_level_change`,
`compaction {summary, firstKeptEntryId, tokensBefore}`, `branch_summary {fromId, summary}`,
`custom`, `custom_message`, `label {targetId, label}`, `session_info {name}`. A `message`
wraps a pi-ai message: `UserMessage {role:"user", content, timestamp}`, `AssistantMessage
{role:"assistant", content:[text|thinking|toolCall], api, provider, model, usage, stopReason,
timestamp}`, `ToolResultMessage {role:"toolResult", toolCallId, toolName, content, details?,
isError, timestamp}`. `toolCall.arguments` is a real JSON object. There is an official spec
document, `packages/coding-agent/docs/session-format.md`, a `version` field with in-code
migrations (v1→v2 added the tree, v2→v3 renamed `hookMessage`→`custom`), and a changelog entry
for each bump.

**Tools.** `bash {command, timeout?}`, `read {path, offset?, limit?}`, `write {path, content}`,
`edit {path, edits}`, `grep {pattern, path?, glob?, …}`, `find {pattern, path?}`, `ls {path?}`.
Note `path`, not `file_path`. Oversized bash output is truncated with the full output written
to a temp file named in `details.fullOutputPath`, the same shape as Claude Code's spilled
`tool-results/`.

**What the adapter has to handle.**

- *Branching is in place.* The tree lives in one file; the current path is the ancestor chain
  of the last entry. Since `seq` order is write order, context expansion still shows
  chronologically adjacent turns, which is what it does for Claude Code rewinds today.
  Abandoned branches simply stay indexed. Nothing to do beyond not assuming linearity.
- *Model and thinking level are change entries, not per-message fields.* The parser tracks the
  latest `model_change` while walking the file and stamps it onto later docs. Across an
  incremental boundary that state must survive: this is what `ParseCarry::agent_state` is for.
  (Assistant messages also carry `model` directly, so the carry only matters for the docs
  between a `model_change` and the next assistant turn.)
- *No git branch anywhere.* The facet is empty for pi.
- *Subagents leave no file.* The reference subagent extension runs children with
  `--no-session` and folds the result into one `toolResult` in the parent. Nothing to link.
- *Fork lineage* via the `parentSession` header is worth a `parent_session` field on
  `SessionInfo` at some point; Codex has the same concept (`forked_from_id`).

**Prior art.** jazzyalex/agent-sessions parses this layout.

Sources: `packages/coding-agent/docs/session-format.md`, `docs/sessions.md`,
`src/core/session-manager.ts`, `src/config.ts`, `packages/ai/src/types.ts`,
`examples/extensions/subagent/`, `CHANGELOG.md` (v0.31.0 "Session Tree") in
https://github.com/badlogic/pi-mono; https://github.com/jazzyalex/agent-sessions.

---

## OpenAI Codex CLI (openai/codex, Rust implementation)

**Verdict: same shape as Claude Code with three wrinkles. 2–4 days.**

**Storage.** `$CODEX_HOME` (default `~/.codex`) `/sessions/YYYY/MM/DD/rollout-<ts>-<thread-id>.jsonl`,
one append-only file per thread. A `thread/revert` writes a new file suffixed `_<rollout-id>`.
Cold files are **zstd-compressed** in the background to `.jsonl.zst`; readers must accept both.
`archived_sessions/` holds relocated rollouts in the same format. `session_index.jsonl` is a
thread-id → metadata index and `history.jsonl` is a flat log of user prompts only
(`{session_id, ts, text}`). No SQLite store was found in the current checkout — but see the
oh-my-pi note below.

**Records.** Each line is `{"timestamp", "ordinal"?, "type", "payload"}` with `type` from an
internally tagged enum: `session_meta`, `response_item`, `event_msg`, `compacted`,
`turn_context`, `token_usage_record`, `world_state`, `inter_agent_communication`, and more.
`session_meta` is the first line: `{meta: {session_id, id, timestamp, cwd, originator,
cli_version, source, model_provider, base_instructions?, forked_from_id?, parent_thread_id?,
agent_nickname?, agent_role?, …}, git: {commit_hash, branch, repository_url}?}`.
`response_item` wraps `message`, `agent_message`, `reasoning`, `function_call {name, arguments,
call_id}`, `function_call_output {call_id, output}`, `local_shell_call`, `custom_tool_call`.
Calls and results join on `call_id`, which also threads through the `event_msg` stream
(`ExecCommandBegin/End`, `PatchApplyBegin/End`, `McpToolCallBegin/End`).

**Subagents** are first-class: each gets its own rollout file and `ThreadId`, linked back by
`parent_thread_id` / `forked_from_id` on its `session_meta`, with spawn and wait events in the
parent. Maps directly onto `agent_id` plus sidechain discovery.

**What the adapter has to handle.**

- *Tool arguments are a JSON string*, not an object ("the Responses API returns arguments as a
  string"). Parse before storing in `tool_input`. `shell` takes `command` as an argv array;
  `apply_patch` rides inside it as `["apply_patch", "<patch text>"]`; `update_plan` is a clean
  object; MCP tools are `mcp__<server>__<tool>`, the same convention as Claude Code.
- *Discovery needs `.jsonl` and `.jsonl.zst` plus `archived_sessions/`.* A compressed file
  cannot be byte-offset tailed, so it is always a whole-file reset (cheap: cold files do not
  change).
- *No parent pointers at all*, only file order and an optional `ordinal`. Fine, per the context
  finding above.
- *Heavy churn right now*: fork/revert/subagent fields, an `agent_type`→`agent_role` rename
  with a serde alias, crates reorganised (`codex-rs/rollout`, `history`, `thread-store`). An
  older single-JSON `~/.codex/sessions/*.json` format predates the rollout JSONL and is
  *plausibly* still on some disks (unverified). Same "tolerate everything, require nothing"
  posture as the Claude Code parser, leaned on harder.

Sources (commit `3ef3cec`): `codex-rs/protocol/src/{protocol,models,plan_tool}.rs`,
`codex-rs/rollout/src/{recorder,rollout_file_name,list,compression,lib,session_index}.rs`,
`codex-rs/history/src/{lib,rollout_payload}.rs`, `codex-rs/message-history/src/lib.rs`,
`codex-rs/core/src/exec.rs` in https://github.com/openai/codex; discussion
https://github.com/openai/codex/discussions/3827.

---

## oh-my-pi (can1357/oh-my-pi)

**Verdict: pi's skeleton with a much bigger surface. 1–2 days on top of the pi adapter, as a
variant flag rather than a second implementation.**

**Storage.** `~/.omp/agent/sessions/<encoded-cwd>/<timestamp>_<sessionId>.jsonl` with a
different cwd encoding from pi (`-<relative>` under home, `-tmp-<relative>` under the temp
root, `--<encoded-absolute>--` otherwise). `PI_CONFIG_DIR` / `PI_CODING_AGENT_DIR` relocate the
root; after an explicit `omp config migrate` the XDG layout (`$XDG_DATA_HOME/omp/sessions`)
applies instead. The directory scheme has already changed twice (a hashed-bucket scheme in
17.2.5–17.2.8, reverted in 17.2.9), so discovery should accept both. Large payloads and images
are externalised to a content-addressed blob store at `~/.omp/agent/blobs/<sha256>`.

**Records.** pi's tree JSONL at version 3, same `toolCall` / `toolResult` blocks, plus: a
fixed-width 256-byte `type: "title"` line *before* the header (skip it); a header with
`additionalDirectories`, `title`/`titleSource`, `previousSessionFiles`, `parentSession`; and
roughly fifteen entry types (`service_tier_change`, `reset_boundary`, `title_change`,
`ttsr_injection`, `credential_pin`, `session_init`, `mode_change`, …), most of which are state
markers to ignore. Feature data lands in namespaced `custom` entries (`tool_execution_start`,
`session_exit`, `user_todo_edit`, `vibe-session-lifecycle`), which is why the schema version
rarely bumps even though releases are near-daily.

**Subagents do get their own files**, at `<parent-file>/<childId>.jsonl` (a directory named
after the parent transcript), linked by the child's `parentSession` header and by
`vibe-session-lifecycle` entries in the parent. That maps onto `agent_id` and sidechain
discovery exactly as Claude Code's `subagents/` does.

**Known lossiness.** Cursor-backed models run native tools through an "exec channel" that never
emits `toolCall` blocks (issue #4348, closed by PR #4351): a turn with 75 tool executions can
persist as two assistant messages and a wall of `toolResult`s. Nothing an indexer can recover.

**A conflict worth checking before writing the Codex adapter.** oh-my-pi ships its own
importers for Claude Code and Codex sessions (`/resume @claude`, `/resume @codex`), and its
Codex importer reads a *SQLite* session database. The Codex research above found no SQLite
store in the current checkout. One of the two is stale.

Sources: `docs/session.md`, `docs/session-switching-and-recent-listing.md`,
`docs/porting-from-pi-mono.md`, `packages/coding-agent/src/session/*.ts`,
`packages/coding-agent/src/vibe/{runtime,lifecycle}.ts`, `packages/utils/src/dirs.ts`,
https://github.com/can1357/oh-my-pi/issues/4348.

---

## Cursor

**Verdict: materially harder, and it is two different sources. 5–10 days, at lower fidelity
than the others whatever you do.**

### `cursor-agent` CLI

`~/.cursor/projects/<sanitized-project-path>/agent-transcripts/<id>/<id>.jsonl`, a minimal
Claude-like JSONL (`{"role":"assistant","content":{"type":"tool_use","id","name","input"}}`,
`{"role":"user","content":{"type":"tool_result","tool_use_id","content"}}`) with no envelope:
no uuid, no parent, no session id, no cwd. Session metadata lives separately in
`~/.cursor/chats/<md5(project-path)>/<session-id>/store.db` (SQLite; name, model hint,
timestamps, workspace context), and `~/.cursor/acp-sessions/**/store.db` for editor-embedded
sessions. `CURSOR_DATA_PATH` / `CURSOR_STORE_ROOT` relocate both. The `--output-format
stream-json` wire format (`system` init with `cwd`, `session_id`, `model`; `tool_call` events
with `call_id` and typed payloads like `shellToolCall`, `editToolCall`) is richer than what
lands on disk and is not confirmed to be the same thing.

The transcript is documented as lossy at the source: native shell/read/edit calls are
summarised to `command`/`path` or missing, and Cursor's team has said tool outputs are
intentionally excluded (a `postToolUse` hook is the suggested workaround).

The JSONL half fits the seam as is; the adapter joins metadata from `store.db` by session id.

### IDE agent (Composer)

`globalStorage/state.vscdb` (macOS `~/Library/Application Support/Cursor/User/`, Linux
`~/.config/Cursor/User/`, Windows `%APPDATA%\Cursor\User\`), a SQLite database with two
key/value tables (`ItemTable`, `cursorDiskKV`; `key TEXT UNIQUE, value BLOB` holding JSON).
Conversations are `composerData:<composerId>` (metadata, ordered `fullConversationHeadersOnly`
index of bubble ids, `unifiedMode`, `modelConfig.modelName`, `createdAt`) plus one
`bubbleId:<composerId>:<bubbleId>` per message (`type` 1 = user, 2 = assistant, `text`,
`toolResults`, `codeBlocks`, `allThinkingBlocks`, `createdAt`). Per-workspace
`workspaceStorage/<hash>/state.vscdb` holds pointers on Cursor 3.0+ (content on ≤2.6); the
hash is opaque and resolves to a folder only via the sibling `workspace.json`. Older chats may
be protobuf blobs. No official schema; a `_v` version counter exists; the database is WAL mode,
readable concurrently if opened `mode=ro`, and reportedly grows to multi-GB with no vacuum.

This is the one source that breaks the indexer's model: blobs are updated in place, there is
no append-only file to watermark, and rows can vanish. It needs the opaque-cursor watermark
(rowid or content digest per key) and a periodic reconciliation against the full key listing.
The cheap fallback is treating the whole store as a reset on every run.

**Prior art**, the closest thing to a spec: S2thend/cursor-history (reads all four surfaces),
saharmor/cursor-view, Callum-Ward/cursaves, somogyijanos/cursor-chat-export (archived),
anasabbasdev/cursor-chat-bulk-export, mikhailsal/cursor-chronicle.

Sources: https://github.com/Callum-Ward/cursaves/blob/main/docs/how-cursor-stores-chats.md,
https://github.com/saharmor/cursor-view, https://github.com/S2thend/cursor-history,
https://cursor.com/docs/cli/reference/output-format, https://cursor.com/docs/cli/using,
https://jazzyalex.github.io/agent-sessions/blog/where-agents-store-history/,
https://forum.cursor.com/t/accessing-the-full-agent-transcript-in-cursor/157311,
https://github.com/can1357/oh-my-pi/issues/4348.
