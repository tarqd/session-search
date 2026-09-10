# Claude Code transcript format

Reverse-engineered from Claude Code **v2.1.266** — a live session on disk plus the shipped
CLI bundle (`/opt/claude-code/bin/claude`, a Bun single-file executable whose JS is greppable).

This is the contract the parser is written against. **The format is visibly still moving**, so
the governing rule is: *tolerate everything, require nothing*.

## 1. On-disk layout

```
~/.claude/projects/<projectKey>/<sessionId>.jsonl          <- main transcript
~/.claude/projects/<projectKey>/<sessionId>/               <- per-session sidecar dir
      ccr-tip.json                                         <- {"eventId","updatedAt"}
      tool-results/<id>.txt                                <- spilled oversized tool output
      subagents/agent-<agentId>.jsonl                      <- sidechain transcript
      subagents/agent-<agentId>.meta.json
      subagents/<nested>/agent-<agentId>.jsonl             <- nesting depth > 1 exists
```

Root override: `$CLAUDE_CONFIG_DIR` else `~/.claude`.

### `<projectKey>` is lossy — never decode it

The CLI computes it as `cwd.replace(/[^a-zA-Z0-9]/g, "-")`, and past a length cap it becomes
`<truncated>-<base36 hash>`. So `/a/b-c` and `/a-b/c` produce the same key, and long paths are
irreversible.

**Derive the project from the `cwd` field present on every DAG record.** `~/.claude.json`'s
`projects` map is keyed by the real absolute path and is a useful cross-check.

### `agent-<id>.meta.json`

```json
{"agentType":"Explore","description":"Characterize transcript format",
 "toolUseId":"toolu_012BPPXJ74jR9q59kKPGDu7E","spawnDepth":1,
 "requestShape":"foreground","requestNonInteractive":true}
```

## 2. File-level hazards

Append-only JSONL, one object per line. The reader **must** handle:

- **Leading NUL bytes on a line.** The CLI's own reader skips `charCodeAt(n)===0` before
  `JSON.parse` (preallocated-file padding). Strip leading `\0` before parsing.
- **A partially-written final line** — sessions are live. Never fail the file for it.
- **Lines with no `uuid` at all** (see §3) — roughly 20% of lines.
- **Dangling `parentUuid`.** The CLI emits a `tengu_transcript_phantom_parent` telemetry
  event, so this happens in the wild. Resolve leniently; fall back to file order.
- **Duplicated sidecar records after resume.** On resume the CLI re-appends the whole metadata
  block. Dedupe last-wins, keyed by `leafUuid` for `summary` and `sessionId` otherwise.
- **Files that vanish.** `~/.claude/.last-cleanup` shows the CLI prunes old transcripts.

## 3. Two disjoint families of record

**DAG records** — `user`, `assistant`, `attachment`, `system`. Always carry `uuid`,
`parentUuid`, `timestamp`, `sessionId`, `cwd`, `gitBranch`, `version`, `userType`,
`entrypoint`, `isSidechain`.

**Sidecar / state records** — no `uuid`, no `parentUuid`; only `type` + `sessionId` + payload.
The complete set the writer emits:

`last-prompt`, `custom-title`, `ai-title`, `tag`, `relocated`, `agent-name`, `agent-color`,
`agent-setting`, `mode`, `permission-mode`, `isolation-latch`, `atis-latch`, `summary`,
`ended`, `queue-operation`, `file-history-snapshot`, `file-history-delta`,
`attribution-snapshot`, `content-replacement`, `worktree-state`, `cost-state`,
`bridge-session`, `artifact-comment-monitor`, `artifact-autoreact-ledger`.

A parser that assumes every line has a `uuid` crashes on the first real transcript.

### Useful sidecar records

```json
{"type":"summary","summary":"...","leafUuid":"..."}                 <- SESSION TITLE
{"type":"last-prompt","lastPrompt":"...","leafUuid":"...","sessionId":"..."}
{"type":"queue-operation","operation":"enqueue","timestamp":"...","sessionId":"...","content":"<raw prompt>"}
{"type":"atis-latch","atis":"","sessionId":"..."}
```

`summary` keyed by `leafUuid` is how the CLI titles sessions in its resume list — take it.
`queue-operation`/`enqueue` `content` duplicates the `user` record; dedupe, don't double-index.
`last-prompt` may also carry `explicit:true`, `rewound:true`.

## 4. `assistant` records

**One API content block per record.** Blocks of one API message share `message.id` and
`requestId`, ordered by `apiBlockIndex` (0-based, and itself *optional* — the transcript
loader strips it on merged records, which then carry `apiBlockIndices` plural).
`message.content` is a 1-element array.

**`message.usage` is duplicated verbatim on every sibling block record.** Dedupe token
accounting by `message.id` or you multiply-count.

```json
{"parentUuid":"d5deacb4-...","isSidechain":false,
 "message":{"model":"claude-opus-5","id":"msg_011CetPa97...","type":"message","role":"assistant",
   "content":[{"type":"thinking","thinking":"...","signature":"CAIS0QgKpgEIERgC..."}],
   "container":null,"stop_reason":"tool_use","stop_sequence":null,"stop_details":null,
   "usage":{"input_tokens":2,"cache_creation_input_tokens":22034,"cache_read_input_tokens":39062,
     "output_tokens":623,"output_tokens_details":{"thinking_tokens":300},
     "server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},
     "service_tier":"standard",
     "cache_creation":{"ephemeral_1h_input_tokens":22034,"ephemeral_5m_input_tokens":0},
     "inference_geo":"not_available","iterations":[{...}],"speed":"standard"},
   "diagnostics":null,"context_management":null},
 "apiBlockIndex":0,"requestId":"req_011CetPa8byV...","type":"assistant",
 "uuid":"7b77e49b-...","timestamp":"2026-09-09T19:07:19.248Z","effort":"high",
 "userType":"external","entrypoint":"remote_mobile","cwd":"/home/user/session-search",
 "sessionId":"b20208d8-...","version":"2.1.266","gitBranch":"claude/...","slug":"wild-spinning-puppy"}
```

`slug` (human-ish session nickname) appears on only *some* records. Do not require it.

**Content block types seen:** `thinking` `{type,thinking,signature}`, `text` `{type,text}`,
`tool_use` `{type,id,name,input,caller}`, `image`
`{type,source:{type:"base64",media_type,data}}` — a pasted screenshot, ~300 KB of base64 on the
line. `source.type` is also `"url"` (`{url}`) and `"file"` (`{file_id}`); MCP flattens the same
block to `{type:"image",data,mimeType}`. `document` carries a PDF the same way.
**Exist in the bundle, not in the sample — must be tolerated:** `redacted_thinking`,
`server_tool_use`, `web_search_tool_result`, `mcp_tool_use`, `search_result`.

`caller`: the only emitted form is `{"type":"direct"}`, absent on older records. Keep it
`Option<Value>`.

**Optional `assistant` fields:** `slug`, `effort`, `agentId`, `attributionAgent`,
`apiBlockIndices`, `batchToolUses` (`[{id,name}]`), `wireToolInputs`, `wireIngestContext`,
`serverClassifierRequest`, `advisorModel`, `collapseSources`, `ephemeral`,
`isApiErrorMessage`, `isMeta`, `is_meta`, `local_command_source`, `attributionSkill`,
`attributionPlugin`, `attributionMcpServer`, `attributionMcpTool`, `parent_tool_use_id`.

## 5. `user` records

`message` is exactly `{role, content}` — no `id`, `model`, or `usage`.

**`message.content` is `String` for human prompts and `Array<ContentBlock>` for tool-result
turns.** Model as an untagged enum.

### Human prompt

```json
{"parentUuid":null,"isSidechain":false,"promptId":"e03de8ba-...","type":"user",
 "message":{"role":"user","content":"Create a rust MCP server that indexes..."},
 "uuid":"e9af1588-...","timestamp":"2026-09-09T19:07:15.164Z","permissionMode":"default",
 "origin":{"kind":"human"},"promptSource":"sdk","userType":"external",
 "entrypoint":"remote_mobile","cwd":"/home/user/session-search","sessionId":"b20208d8-...",
 "version":"2.1.266","gitBranch":"claude/..."}
```

**The CLI's own predicate for "this is a real human turn"** (several call sites):

```
type === "user"
  && origin?.kind === "human"
  && toolUseResult === undefined
  && isCompactSummary !== true
  && isMeta !== true
  && isVisibleInTranscriptOnly !== true
  && turnCompanion !== true
  && verifiedSlackHumanTurn !== true
```

Also counted as human input: `attachment` records with `attachment.type === "queued_command"`
and `attachment.origin?.kind === "human"`.

`origin.kind` is at least `"human"` | `"peer"` (peer carries `hopChain`, `fromSession`,
`fromMode`, `body`).

### Synthetic tool-result turn

`sourceToolAssistantUUID` = uuid of the *assistant block record* that emitted the `tool_use`.
`message.content` is an array of:

```json
{"type":"tool_result","tool_use_id":"toolu_017o44...","is_error":false,"content":"total 24\n..."}
```

`tool_result.content` is `String | Array<{type:"text"|"image",...}>`.

### `toolUseResult` — `String | Object | absent`

The rich, non-API result payload. **It is a bare string on every failure path**: `"Error: ..."`,
`"InputValidationError: JSON parse failed (N bytes)"`, `"Error: No such tool available: X"`,
`"Conversation ended by model"`, `"Streaming fallback - tool execution discarded"`.
Model as `serde_json::Value` and branch.

Object shapes by tool:

```
Bash:       {stdout, stderr, interrupted, isImage, noOutputExpected}
            isImage:true means stdout IS the image bytes; stderr beside it is still text
Read (bin): {type:"image"|"pdf"|"audio"|"video", file:{base64, type, originalSize, dimensions?}}
            pdf adds file.filePath and pages:[{base64, mediaType}]
Bash (bg):  {..., backgroundTaskId, backgroundCwdHint}
Read:       {type:"text", file:{filePath,content,numLines,startLine,totalLines,truncatedByTokenCap?}, artifactRead?}
Edit:       {filePath, oldString, newString, originalFile, structuredPatch, userModified, replaceAll,
             staleRecovered?, memdirStamped?, gitDiff?}
Write:      {filePath, content, structuredPatch, type:"create"|"update", originalFile}
Glob:       {filenames, durationMs, numFiles, truncated}
Grep:       {mode?, numFiles, filenames, content?, numLines?, numMatches?, totalFiles?,
             truncated, totalMatches, countIsComplete, durationMs}
TodoWrite:  {oldTodos, newTodos}
Agent/Task: {content, totalDurationMs, totalTokens, totalToolUseCount, usage, toolStats}
```

MCP tools add a top-level `mcpMeta` on the user record.

**Optional `user` fields** (authoritative superset, from the record factory in the bundle):
`content, isMeta, ephemeral, turnCompanion, usageLimitNote, replacesSpan,
isVisibleInTranscriptOnly, isVirtual, isCompactSummary, summarizeMetadata, toolUseResult,
classifierMetaLines, serverClassifierContext, hostClassifierContext, toolDenialKind,
userFeedback, mcpMeta, toolEndsTurn, uuid, timestamp, imagePasteIds,
sourceToolAssistantUUID, permissionMode, origin, promptSource, promptId,
interruptedMessageId, interruptedByShutdown`.

`toolDenialKind` literals: `"interrupted"`, `"cancelled"`, `"permission-rule"`, `"user-rejected"`.

## 6. `attachment` records

Shape: `{type:"attachment", attachment:{type:<subtype>, ...}, rendered?:[{content}],
renderedInHumanTurn?:[{content}]}` plus the DAG fields. **The rendered element key is
`content`, not `text`.**

| subtype | payload | verdict |
| --- | --- | --- |
| `environment` | `snapshot:{workingDirectory,isWorktree,isGitRepo,platform,shell,osVersion,scratchpadDirectory,...}` | **metadata — index** |
| `model` | `identity:{modelId,marketingName,knowledgeCutoff}`, `text` | **metadata — index** |
| `remote_session_change` | `url`, `commit`, `pr`, `sendUserFileHint` | metadata (session URL) |
| `session_context` | `context:{userEmail}` free-form map | metadata |
| `date` | `date:"2026-09-09"` | metadata |
| `plan_mode` | `isSubAgent`, `planExists`, `planFilePath`, `reminderType` | metadata |
| `queued_command` | `commandMode`, `prompt`, `timestamp`, `origin` | **index — real user text** |
| `prompt_snapshot` | `systemPrompt` (~31 KB), `tools?` (41×`{name,description,schema}`), `cliPrefix?` | **skip text**; `tools[].name` is a good facet |
| `skill_listing` | `content` (17.6 KB), `names[]`, `skillCount` | skip text, facet `names` |
| `deferred_tools_delta` | `addedNames[]`, `removedNames[]`, `wireHiddenNames[]`, `failedMcpServers`, ... | facet only |
| `agent_listing_delta` | `addedTypes[]`, `removedTypes[]`, ... | facet only |
| `mcp_instructions_delta` | `addedNames[]`, `addedBlocks[]` | facet only |
| `total_tokens_reminder` | `text` | **noise** (13 of 20 attachments) |
| `task_reminder` | `content` (JSON string), `itemCount` | low value |

**Also worth indexing as text:** `file`, `edited_text_file`, `instructions`, `nested_memory`,
`relevant_memories`, `cowork_memory_context`, `agent_mention`, `peer_mention`,
`teammate_mailbox`, `hook_*` outputs, `structured_output`.

Full subtype universe (67, from the bundle — treat as open):
`agent_mention, async_hook_response, async_hook_response_batch, auto_mode, auto_mode_exit,
batching_reminder, cowork_memory_context, date, date_change, deferred_tools_delta,
deferred_tools_record, dir_sync_notice, edited_text_file, environment, file, fork_briefing,
goal_status, hook_additional_context, hook_blocking_error, hook_cancelled, hook_deferred_tool,
hook_error_during_execution, hook_non_blocking_error, hook_permission_decision,
hook_stopped_continuation, hook_success, hook_system_message, instructions, invoked_skills,
language, max_turns_reached, model, nested_memory, output_style_instructions, peer_mention,
plan_mode, plan_mode_exit, plan_mode_reentry, poll_events, prompt_snapshot, queued_command,
read_truncation_notice, relevant_memories, remote_session_change, sandbox_instructions,
secondary_reminder, session_context, silent_turn_reminder, skill_listing, structured_output,
task_reminder, task_status, team_context, teammate_mailbox, teammate_shutdown_batch,
thinking_stripped, todo_reminder, tool_host_result_lines, tool_hosts_notice,
tool_search_usage_reminder, ultra_effort_enter, ultra_effort_exit,
workflow_size_guideline_change`

## 7. The DAG — `parentUuid` is write order, not API structure

Every DAG record's parent is the immediately preceding DAG record in the file. From a real
session:

```
17 assistant 7b77e49b <- d5deacb4 (attachment)   apiBlockIndex 0  thinking
18 assistant 7621989d <- 7b77e49b                apiBlockIndex 1  text
19 assistant 4fee9186 <- 7621989d                apiBlockIndex 2  tool_use Bash
20 user      7deb6cb8 <- 4fee9186                tool_result
21 assistant 51338fa8 <- 7deb6cb8                apiBlockIndex 3  tool_use Bash  <-- parent is a USER record
22 user      12652cc9 <- 51338fa8                tool_result
```

Line 21 belongs to the **same API message** as 17–19, yet its parent is a tool_result.

1. To reconstruct API messages: group by `(message.id, requestId)`, order by `apiBlockIndex`.
   **Never** by `parentUuid`.
2. To reconstruct conversational order: file order, which agrees with `parentUuid`.

Attachments are interleaved *into* the chain, not side-attached. The first human `user`
record has `parentUuid: null`.

## 8. Sidechains / subagents

- Sidechain records live in **a separate file**, `subagents/agent-<agentId>.jsonl`. The main
  transcript holds only the `Agent` `tool_use` and its later `tool_result`.
- Every record: `isSidechain: true`, `agentId: "a10845c5ff9c7d4ec"` (17 hex chars, not a UUID).
- `assistant` records add `attributionAgent: "Explore"` (the subagent type).
- **`sessionId` in a sidechain file is the *parent* session's id.** Key on `(sessionId, agentId)`.
- The first record is a `user` with `parentUuid: null`, `isSidechain: true`, and a `promptId`
  **equal to the parent session's `promptId`** — a join back to the originating human turn.
  The stronger join is `.meta.json`'s `toolUseId` → the parent's `tool_use.id`.
- Other attribution fields the writer may add: `attributionAgent`, `attributionSkill`,
  `attributionPlugin`, `attributionMcpServer`, `attributionMcpTool`.
- Tolerate `isSidechain: true` records appearing inline in `<sessionId>.jsonl` (older layout).

## 9. Resume, fork, compaction

- **Resume appends to the same file** and re-appends the whole sidecar metadata block, so
  those records repeat. Dedupe last-wins on `leafUuid` (for `summary`) else `sessionId`.
- `resetSessionFile()` exists — a session can start a *new* file mid-life, so multiple files
  may share a lineage.
- `leafUuid` is the head-of-conversation pointer.
- **Fork:** expect a `forkSession` / `forkSessionId` field, often absent. A verbatim string in
  the bundle: *"forkSession follows across the compaction break. Distinct from the session-file
  chain parent (which is the post-compact summary)."*
- **Compaction** emits two records:
  1. `{"type":"system","subtype":"compact_boundary","content":"Conversation compacted",
     "level":"info","uuid":...,"timestamp":...,"logicalParentUuid":...?,
     "compactMetadata":{trigger,preTokens,postTokens?,cumulativeDroppedTokens?,userContext?,
     messagesSummarized?,preservedSegment?,preservedMessages?,preCompactDiscoveredTools?}}`
  2. a `user` record with `isCompactSummary: true` **and** `isVisibleInTranscriptOnly: true`
     carrying the summary text.

  Exclude both from human-prompt detection and from message counts.

## 10. `system` records

Absent from remote sessions, present in TTY ones:
`{type:"system", subtype, content?, level:"info"|"notice"|"warning"|"error", isMeta, uuid,
timestamp, session_id?}` plus subtype-specific fields.

Persisted subtypes to expect: `compact_boundary`, `local_command` (+`contextUsage`),
`file_snapshot` (+`snapshotFiles`), `agents_killed`, `informational`,
`api_error` (+`error,retryInMs,retryAttempt,maxRetries,source`), `model_refusal_fallback`,
`task_notification`, `task_started`, `task_progress`, `task_updated`, `hook_started`,
`hook_progress`, `hook_response`, `vcs_state_changed`, `code_change_published`,
`post_turn_summary`, `turn_duration`, `stop_hook_summary`. (130+ subtype literals exist in the
bundle; most are stream events that never hit disk. Index defensively; do not enumerate.)

## 11. Other directories — and why they are not useful

- `~/.claude/sessions/<pid>.json` — **live-process registry, not history.** Keyed by PID,
  deleted on exit. Carries `pid, sessionId, cwd, startedAt, version, kind, entrypoint, name`.
  Useful only for "is this session live" and a derived `name`.
- `~/.claude/session-env/<sessionId>/` — empty in practice.
- `~/.claude/backups/` — only `.claude.json.backup.<epoch>` with an `oauthAccount` blob.
- `~/.claude/plans/<slug>.md` — plan-mode artifacts, joinable via `slug` or
  `attachment.plan_mode.planFilePath`.
- `~/.claude.json` — global; `projects` keyed by real absolute cwd, values carry
  `allowedTools`, `mcpServers`, etc. Good for projectKey → real path and MCP facets.

## 12. Record `type` values — the full known set

Sample: `user`, `assistant`, `attachment`, `queue-operation`, `atis-latch`, `last-prompt`.

Bundle-only (must be tolerated): `system`, `summary`, `custom-title`, `ai-title`, `tag`,
`relocated`, `agent-name`, `agent-color`, `agent-setting`, `mode`, `permission-mode`,
`isolation-latch`, `ended`, `file-history-snapshot`, `file-history-delta`,
`attribution-snapshot`, `content-replacement`, `worktree-state`, `cost-state`,
`bridge-session`, `artifact-comment-monitor`, `artifact-autoreact-ledger`,
`conversation_reset`, `stream_event`.

## 13. Parser rules that follow

1. `#[serde(tag = "type")]` with a `#[serde(other)] Unknown` variant; `#[serde(default)]` on
   every field; `#[serde(flatten)] extra: Map<String, Value>` to retain unknown keys.
2. Keep the raw line for every record.
3. Discover sessions by globbing `projects/*/*.jsonl` **and**
   `projects/*/*/subagents/**/agent-*.jsonl`.
4. Derive the project from the record `cwd`, never from the directory name.
5. Dedupe usage on `message.id`; dedupe sidecar records last-wins.
6. `message.usage` leaves are all optional — `cache_creation_input_tokens` and
   `cache_read_input_tokens` are explicitly nullable in the CLI's own zod schema.
