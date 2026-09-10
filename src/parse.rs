//! Transcript records -> [`Doc`]s.
//!
//! Everything hostile about the on-disk format is handled here:
//!
//! * leading NUL bytes on a line (preallocated-file padding),
//! * a partially-written final line — the returned offset stops at the end of the last
//!   *complete* line so the next incremental run re-reads the partial one,
//! * ~20% of lines that carry no `uuid` at all (sidecar/state records),
//! * assistant records split one content block per record, sharing a `message.id`,
//! * `user` `message.content` as `String | Array`, `toolUseResult` as `String | Object | absent`,
//! * dangling `parentUuid` (kept verbatim; conversational order comes from file order),
//! * sidecar records re-appended wholesale on resume (deduped last-wins).
//!
//! A `tool_use` block and the `tool_result` that answers it are joined by `tool_use_id` into a
//! single [`DocKind::ToolCall`] document carrying name, input, result text and error flag. The
//! result goes into its own [`Doc::tool_output`] field rather than being concatenated onto
//! `text`, so `tool_output:"..."` can ask about what a tool *returned* and not what it was
//! asked to do.
//!
//! What is left of a document's body is **split by kind**, because prose and code want
//! opposite analysis (`schema.rs`, `tokenizer.rs`):
//!
//! * a `user` or `assistant` message is markdown, so [`crate::markdown::split`] routes its
//!   prose to `text`, its blocks and spans to `code` and its headings to `headings`;
//! * a tool call is not markdown and is never parsed as such — a `Bash` script full of `#`
//!   and `*` is not a heading and a bullet list. Its name and input strings are `text`, the
//!   file content of an `Edit`/`Write` is `code`, and its result is `tool_output`;
//! * attachments and `system` records stay whole in `text`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::media;
use crate::model::{
    AssistantRecord, Attachment, AttachmentRecord, ContentBlock, MediaBlock, MessageContent,
    ParsedLine, Record, SystemRecord, ToolUseBlock, UserRecord, clean_line, parse_line,
};

// ---------------------------------------------------------------------------
// pinned public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DocKind {
    Message,
    ToolCall,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Doc {
    /// `"<session_id>:<agent_id|->:<file_tag>:<seq>"` — unique, and the delete key for a
    /// single doc. `file_tag` is a short digest of `source_path`: `seq` restarts at 0 in every
    /// file, and two transcripts can legitimately share a `session_id`
    /// (`resetSessionFile()`, a `relocated` sidecar — TRANSCRIPT-FORMAT §9), so without it
    /// the id is not unique.
    pub doc_id: String,
    pub kind: DocKind,
    /// Absolute; the delete-by-term key when a whole file is re-indexed.
    pub source_path: String,
    /// Monotonic ordinal within the file, continuing from `seq_base`.
    pub seq: u64,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub timestamp_ms: Option<i64>,
    /// From the record `cwd`, NEVER the lossy project directory name.
    pub project: Option<String>,
    pub git_branch: Option<String>,
    /// `"user" | "assistant" | "system" | "attachment"`.
    pub role: String,
    pub model: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: Option<Value>,
    /// Structured view of a Bash `tool_input.command` — `{"program": [...], "args": [...]}`
    /// as produced by [`crate::bash::extract`]. `None` for every other tool, and for a
    /// command the shell grammar rejects.
    ///
    /// `#[serde(default)]` because a `Doc` also travels inside [`ParseCarry`] in
    /// `state.json`: state written before this field existed must still load.
    #[serde(default)]
    pub bash_cmd: Option<Value>,
    pub is_error: bool,
    pub is_sidechain: bool,
    /// Compaction summaries, meta turns — excluded from "human prompt".
    pub is_meta: bool,
    pub entrypoint: Option<String>,
    pub permission_mode: Option<String>,
    pub version: Option<String>,
    pub slug: Option<String>,
    /// The body as a reader saw it: a message's original markdown, or a tool call's name and
    /// input strings. Stored, never indexed — it is what `show`, `--context` and `--json`
    /// render, beside `tool_output`.
    ///
    /// The retrieval fields below cannot be reassembled into it: the split drops link
    /// destinations, repeats every inline span in both halves, and keeps no record of where a
    /// fence sat among the paragraphs it was written between.
    #[serde(default)]
    pub body: String,
    /// The indexed prose, **one entry per block**: for a message the markdown minus its code
    /// blocks, for a tool call the tool name and its input strings. Analyzed as English.
    ///
    /// One entry per block, not one joined string, so that Tantivy's position gap stands where
    /// a code block was lifted out and no phrase can match across it.
    #[serde(default)]
    pub text: Vec<String>,
    /// The code half of the body: one entry per code block or inline span of a message, and
    /// the `Edit`/`Write` file-content payloads of a tool call. Analyzed as code. A tool
    /// *result* is not here — it has its own field. `#[serde(default)]` on this and its
    /// neighbours so a `state.json` carried over from a build without them still loads — and
    /// `STATE_VERSION` is bumped when a field changes shape, which `text` just did.
    #[serde(default)]
    pub code: Vec<String>,
    /// A message's markdown headings, one entry each. Also present in `text`.
    #[serde(default)]
    pub headings: Vec<String>,
    /// The info-string language of each fenced block, deduped.
    #[serde(default)]
    pub code_langs: Vec<String>,
    /// The joined `tool_result` text, indexed and stored in its own right. `None` on a
    /// message, and on a tool call whose result has not been read yet.
    #[serde(default)]
    pub tool_output: Option<String>,
    /// Stored; indexed only with `include_thinking`.
    pub thinking: Option<String>,
    /// `usage.output_tokens_details.thinking_tokens`, attached to exactly ONE document per API
    /// message so sums and facets are not multiplied by the block count. Remote and web
    /// sessions strip the thinking *text* but keep this, so it is the only surviving measure of
    /// where a session stopped to reason.
    pub thinking_tokens: Option<u64>,
    /// The original JSONL line.
    pub raw: String,
}

/// `#[serde(default)]` so a `sessions.json` written by an older build — one missing a field
/// added since — still loads instead of failing the whole file.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SessionInfo {
    pub session_id: String,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    /// Subagent `.meta.json` description; filled by the indexer, not by the parser.
    pub description: Option<String>,
    /// From a `summary` record, keyed by `leafUuid`.
    pub title: Option<String>,
    pub slug: Option<String>,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    pub source_path: String,
    pub first_ts_ms: Option<i64>,
    pub last_ts_ms: Option<i64>,
    /// Conversational turns: human prompts, assistant API messages (counted once per
    /// `message.id`, however many block records it was split across) and `system` records.
    /// Compaction records and attachments are **excluded** (TRANSCRIPT-FORMAT §9).
    pub messages: u64,
    pub tool_calls: u64,
    pub first_prompt: Option<String>,
}

/// A malformed line. Counted, never fatal.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParseError {
    pub path: String,
    /// 1-based line number *within this parse run*.
    pub line: u64,
    /// Absolute byte offset of the start of the line.
    pub byte_offset: u64,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{} (byte {}): {}",
            self.path, self.line, self.byte_offset, self.message
        )
    }
}

#[derive(Debug)]
pub struct ParseOutput {
    pub docs: Vec<Doc>,
    /// Documents that **replace** ones already in the index: same `doc_id`, same `seq`, now
    /// carrying the tool result that arrived after they were written. The consumer must delete
    /// each one's `doc_id` before adding it, and must **not** count them towards the file's
    /// `docs` watermark — their `seq` numbers were handed out by an earlier run.
    pub replacements: Vec<Doc>,
    pub session: SessionInfo,
    pub errors: Vec<ParseError>,
    /// What the *next* incremental parse of this same file has to be told. See [`ParseCarry`].
    pub carry: ParseCarry,
}

/// How many message ids one file's [`ParseCarry`] keeps. Cross-boundary memory only has to
/// reach back far enough to cover one interleaving of records; the cap stops a pathological
/// transcript from growing `state.json` without bound.
const CARRY_CAP: usize = 512;

/// How many unanswered tool calls one file carries. In practice this is 0, 1, or the width of
/// one batch of parallel calls.
const PENDING_CAP: usize = 32;

/// A carried document larger than this is remembered by id only: `state.json` is rewritten on
/// every run, so it must not grow to hold a copy of an enormous `Write` payload.
const PENDING_DOC_CAP: usize = 128 * 1024;

/// State that has to survive from one incremental parse of a file to the next.
///
/// A tail parse sees only the bytes appended since the last run, and two things in this format
/// straddle that boundary:
///
/// * a `tool_use` block and the `tool_result` that answers it are usually written in
///   consecutive records, so the boundary lands between them almost every time the indexer
///   runs against a live transcript. Without [`ParseCarry::pending_tool_uses`] the tail sees an
///   *orphan* result and emits a second, half-empty document for a tool call that is already
///   indexed — doubling every tool call in a live session.
/// * one API message is split across sibling block records sharing a `message.id` (§4), and
///   those records are *not* contiguous (§7), so a boundary inside one message would count it
///   twice without [`ParseCarry::counted_message_ids`].
///
/// [`ParseCarry::tail_line`] is unrelated to parsing: it fingerprints the last complete line
/// consumed so the indexer can tell "the file grew" from "the file was rewritten in place".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ParseCarry {
    /// Tool calls this file has already emitted a document for that no `tool_result` has
    /// answered yet.
    pub pending_tool_uses: Vec<PendingToolCall>,
    /// API `message.id`s already counted towards [`SessionInfo::messages`].
    pub counted_message_ids: Vec<String>,
    /// API `message.id`s whose thinking cost has already been charged to a document. Separate
    /// from `counted_message_ids` because the record carrying `thinking_tokens` is usually the
    /// `tool_use` one, not whichever block record emitted first.
    pub charged_message_ids: Vec<String>,
    /// `(start offset, hash)` of the last complete line consumed.
    pub tail_line: Option<TailLine>,
}

/// One tool call whose document is in the index but whose result has not been read yet.
///
/// Carrying the document itself — not just the id — is what lets a later tail *complete* it:
/// the replacement keeps the original `doc_id` and `seq`, so the result text lands on the same
/// document a whole-file parse would have produced, rather than being lost or duplicated.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PendingToolCall {
    pub tool_use_id: String,
    /// The document as it was indexed, minus the result. `None` when it was too large to carry
    /// (see [`PENDING_DOC_CAP`]): the duplicate is still suppressed, but a late result for it
    /// updates nothing until the next `index --full`.
    pub doc: Option<Box<Doc>>,
}

/// A fingerprint of the last complete line a parse consumed, so a later run can check that the
/// bytes before its watermark are still the bytes it read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TailLine {
    pub start: u64,
    pub hash: u64,
}

/// Per-file inputs that are not the bytes of the file itself.
#[derive(Debug, Clone, Default)]
pub struct FileContext {
    /// `agent-<id>.meta.json`'s `agentType`. TRANSCRIPT-FORMAT §1 makes this the canonical
    /// source for a subagent's type; `attributionAgent` on an assistant record is a bonus that
    /// only some writers emit, so a parser that relies on it alone leaves `agent_type` empty
    /// on exactly the transcripts the sidecar exists for.
    pub agent_type: Option<String>,
    /// What the previous incremental parse of this same file left behind.
    pub carry: ParseCarry,
}

/// FNV-1a, 64-bit. Deterministic across builds (unlike `DefaultHasher`), which matters because
/// these digests are persisted in `state.json` and baked into `doc_id`.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The `file_tag` component of a [`Doc::doc_id`]: eight hex digits of the source path.
pub fn file_tag(source_path: &str) -> String {
    format!("{:08x}", fnv1a(source_path.as_bytes()) as u32)
}

/// The `text` copy of a tool call's input never needs the full budget: every input is already
/// indexed whole and uncapped in `tool_input`, which is one of the default query fields, so this
/// copy exists for matching `Bash cargo build` as prose and for previews — not for coverage.
/// Bounding it also keeps a tool-call document small enough to stay carryable across an
/// incremental boundary (see [`PENDING_DOC_CAP`]), which raising [`ParseOptions::max_text_bytes`]
/// would otherwise quietly stop happening.
const INPUT_LEAVES_CAP: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// Cap on the indexed body of one doc. `tool_output` is capped separately, at the same
    /// number: a tool call's input and its result are two fields now, not one shared budget.
    ///
    /// The default is deliberately far above what a transcript actually carries. Claude Code
    /// bounds tool output before it reaches disk — anything oversized is spilled to
    /// `tool-results/<id>.txt` and referenced by a stub — so on a real corpus the largest
    /// inline result measured 18.7 KB and *nothing* reached the old 32 KiB cap. The one place
    /// that cap did bite was the spill path, which exists to go and fetch precisely the outputs
    /// too big to inline: a 54.5 KB spill lost 40% of itself on the way in. A cap this size
    /// still bounds a `cat` of something enormous, without cutting any output a person would
    /// call reasonable.
    pub max_text_bytes: usize,
    /// Follow `<persisted-output>` pointers into `tool-results/<id>.txt`.
    pub load_spilled_results: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            max_text_bytes: 1024 * 1024,
            load_spilled_results: false,
        }
    }
}

/// Prefix of the first human prompt kept on [`SessionInfo`].
const FIRST_PROMPT_CHARS: usize = 500;

/// Attachment subtypes whose text is pure noise or pure boilerplate. Everything not listed
/// is indexed — the subtype universe is open, so the default must be "keep".
const SKIPPED_ATTACHMENTS: &[&str] = &[
    "agent_listing_delta",
    "batching_reminder",
    "command_permissions",
    "deferred_tools_delta",
    "deferred_tools_record",
    "mcp_instructions_delta",
    "prompt_snapshot",
    "secondary_reminder",
    "silent_turn_reminder",
    "skill_listing",
    "task_reminder",
    "todo_reminder",
    "tool_search_usage_reminder",
    "total_tokens_reminder",
];

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

/// Parse a transcript. `from_offset` supports incremental tailing; `seq_base` continues
/// numbering; `ctx` carries the per-file facts a tail parse cannot see for itself. Returns the
/// byte offset of the end of the last COMPLETE line — a partial trailing line is left
/// unconsumed so the next run re-reads it.
pub fn parse_file(
    path: &Path,
    from_offset: u64,
    seq_base: u64,
    opts: &ParseOptions,
    ctx: &FileContext,
) -> anyhow::Result<(ParseOutput, u64)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if from_offset >= len {
        let parser = Parser::new(path, opts, ctx);
        return Ok((
            ParseOutput {
                docs: Vec::new(),
                replacements: Vec::new(),
                session: parser.session,
                errors: Vec::new(),
                // Nothing was read, so nothing the previous run recorded has been superseded.
                carry: ctx.carry.clone(),
            },
            from_offset.min(len),
        ));
    }
    file.seek(SeekFrom::Start(from_offset))?;
    let reader = BufReader::new(file);

    let mut parser = Parser::new(path, opts, ctx);
    let consumed = parser.ingest(reader, from_offset)?;
    let output = parser.finish(seq_base);
    Ok((output, consumed))
}

/// One-shot parse of a whole file with no carried state — the ground truth an incremental run
/// has to converge on, and what every caller that is not the incremental indexer wants.
pub fn parse_whole(path: &Path, opts: &ParseOptions) -> anyhow::Result<ParseOutput> {
    Ok(parse_file(path, 0, 0, opts, &FileContext::default())?.0)
}

// ---------------------------------------------------------------------------
// the parser
// ---------------------------------------------------------------------------

/// A `tool_result` waiting to be folded into the `tool_use` that asked for it.
#[derive(Debug, Default, Clone)]
struct ToolOutcome {
    text: String,
    is_error: bool,
}

struct Line {
    parsed: ParsedLine,
    raw: String,
}

struct Parser<'a> {
    path: PathBuf,
    opts: &'a ParseOptions,
    session: SessionInfo,
    /// `Some(..)` only for `subagents/**/agent-<id>.jsonl`. This — never the progressively
    /// absorbed `session.agent_id` — is what makes a document a sidechain by virtue of the
    /// file it came from: TRANSCRIPT-FORMAT §8 requires tolerating `isSidechain: true` records
    /// inline in a main transcript, and one of those must not relabel the rest of the file.
    path_agent_id: Option<String>,
    /// `file_tag(source_path)`, precomputed for every `doc_id`.
    file_tag: String,
    lines: Vec<Line>,
    errors: Vec<ParseError>,
    /// `tool_use_id` -> joined result.
    outcomes: HashMap<String, ToolOutcome>,
    /// `tool_use_id`s an assistant record actually emitted, in issue order.
    issued_tool_uses: Vec<String>,
    /// Membership index over `issued_tool_uses`.
    issued_index: HashSet<String>,
    /// Tool calls an *earlier* parse of this file already emitted a document for, keyed by
    /// `tool_use_id`. A `tool_result` for one of these completes that document instead of
    /// looking like an orphan and inventing a second one.
    carried_pending: BTreeMap<String, PendingToolCall>,
    /// uuid of an assistant record -> the `tool_use` ids it emitted, for the failure paths
    /// that carry no `tool_result` block (§5) but do name `sourceToolAssistantUUID`.
    tool_uses_by_uuid: HashMap<String, Vec<String>>,
    /// `leafUuid` -> summary text, last-wins across resume duplicates.
    summaries: BTreeMap<String, String>,
    /// Order of arrival of the summary keys, so "last summary in the file" is recoverable.
    summary_order: Vec<String>,
    /// Deduped sidecar payloads: (type, leafUuid else sessionId) -> value, last-wins.
    sidecars: BTreeMap<(String, String), Value>,
    /// The newest `last-prompt` sidecar seen, last-wins in *file* order.
    last_prompt: Option<String>,
    /// API `message.id`s already counted, seeded from the carry so a tail boundary inside one
    /// message does not count it twice.
    counted_message_ids: HashSet<String>,
    charged_message_ids: HashSet<String>,
    charged_order: Vec<String>,
    /// The same ids in arrival order, so the carry can keep the most recent ones.
    counted_order: Vec<String>,
    last_uuid: Option<String>,
    /// Fingerprint of the last complete line consumed, for the indexer's rewrite detection.
    tail_line: Option<TailLine>,
    /// First user text of any kind, used when no record passes the human-turn predicate
    /// (subagent transcripts have no `origin` on their opening turn).
    fallback_first_prompt: Option<String>,
}

impl<'a> Parser<'a> {
    fn new(path: &Path, opts: &'a ParseOptions, ctx: &FileContext) -> Parser<'a> {
        let ids = ids_from_path(path);
        let source_path = path.display().to_string();
        Parser {
            path: path.to_path_buf(),
            opts,
            file_tag: file_tag(&source_path),
            session: SessionInfo {
                session_id: ids.session_id,
                agent_id: ids.agent_id.clone(),
                agent_type: ctx.agent_type.clone(),
                source_path,
                ..SessionInfo::default()
            },
            path_agent_id: ids.agent_id,
            lines: Vec::new(),
            errors: Vec::new(),
            outcomes: HashMap::new(),
            issued_tool_uses: Vec::new(),
            issued_index: HashSet::new(),
            carried_pending: ctx
                .carry
                .pending_tool_uses
                .iter()
                .map(|p| (p.tool_use_id.clone(), p.clone()))
                .collect(),
            tool_uses_by_uuid: HashMap::new(),
            summaries: BTreeMap::new(),
            summary_order: Vec::new(),
            sidecars: BTreeMap::new(),
            last_prompt: None,
            counted_message_ids: ctx.carry.counted_message_ids.iter().cloned().collect(),
            charged_message_ids: ctx.carry.charged_message_ids.iter().cloned().collect(),
            charged_order: ctx.carry.charged_message_ids.clone(),
            counted_order: ctx.carry.counted_message_ids.clone(),
            last_uuid: None,
            tail_line: ctx.carry.tail_line,
            fallback_first_prompt: None,
        }
    }

    /// Read complete lines, parse them, and gather the cross-record state the second pass
    /// needs. Returns the offset just past the last complete line.
    fn ingest<R: BufRead>(&mut self, mut reader: R, from_offset: u64) -> anyhow::Result<u64> {
        let mut offset = from_offset;
        let mut consumed = from_offset;
        let mut lineno = 0u64;
        let mut buf = Vec::new();

        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            let line_start = offset;
            offset += n as u64;
            if !buf.ends_with(b"\n") {
                // Partially written final line — leave it for the next run.
                tracing::debug!(path = %self.path.display(), "trailing partial line left unconsumed");
                break;
            }
            consumed = offset;
            lineno += 1;
            // Fingerprint every complete line; the last one to survive the loop is the one the
            // watermark points just past, and is what proves next run that the bytes before
            // the watermark were not rewritten underneath us.
            self.tail_line = Some(TailLine {
                start: line_start,
                hash: fnv1a(&buf),
            });

            let cleaned = clean_line(&buf);
            if cleaned.is_empty() {
                continue;
            }
            match parse_line(cleaned) {
                Ok(parsed) => {
                    let raw = String::from_utf8_lossy(cleaned).into_owned();
                    self.observe(&parsed);
                    self.lines.push(Line { parsed, raw });
                }
                Err(err) => self.errors.push(ParseError {
                    path: self.path.display().to_string(),
                    line: lineno,
                    byte_offset: line_start,
                    message: err.to_string(),
                }),
            }
        }
        Ok(consumed)
    }

    /// First pass: collect the state that the emitting pass needs to look *forwards* for.
    fn observe(&mut self, line: &ParsedLine) {
        match &line.record {
            Record::Assistant(a) => {
                let mut issued: Vec<String> = Vec::new();
                if let Some(msg) = &a.message {
                    for block in msg.content.blocks() {
                        if let Some(tu) = tool_use_block(block)
                            && let Some(id) = &tu.id
                            && self.issued_index.insert(id.clone())
                        {
                            self.issued_tool_uses.push(id.clone());
                            issued.push(id.clone());
                        }
                    }
                }
                if !issued.is_empty()
                    && let Some(uuid) = a.common.uuid.as_deref()
                {
                    self.tool_uses_by_uuid.insert(uuid.to_string(), issued);
                }
            }
            Record::User(u) => self.observe_results(u),
            Record::Summary(s) => {
                let key = s
                    .leaf_uuid
                    .clone()
                    .or_else(|| s.session_id.clone())
                    .unwrap_or_default();
                if let Some(text) = s.summary.clone().filter(|t| !t.is_empty()) {
                    if self.summaries.insert(key.clone(), text).is_none() {
                        self.summary_order.push(key.clone());
                    } else {
                        // resume duplicate: last wins, and it moves to the end
                        self.summary_order.retain(|k| k != &key);
                        self.summary_order.push(key.clone());
                    }
                }
                self.record_sidecar(
                    "summary",
                    s.leaf_uuid.as_deref(),
                    s.session_id.as_deref(),
                    line,
                );
            }
            Record::LastPrompt(l) => {
                // Last one in the file wins — `sidecars` is a BTreeMap, so reading it back
                // would pick the lexicographically smallest `leafUuid` instead.
                if let Some(text) = l.last_prompt.clone().filter(|t| !t.trim().is_empty()) {
                    self.last_prompt = Some(text);
                }
                self.record_sidecar(
                    "last-prompt",
                    l.leaf_uuid.as_deref(),
                    l.session_id.as_deref(),
                    line,
                );
            }
            Record::QueueOperation(q) => {
                self.record_sidecar("queue-operation", None, q.session_id.as_deref(), line)
            }
            Record::Unknown => {
                let ty = line.type_name().unwrap_or("unknown").to_string();
                let leaf = line.raw_str("leafUuid").map(str::to_string);
                let sess = line.raw_str("sessionId").map(str::to_string);
                self.record_sidecar(&ty, leaf.as_deref(), sess.as_deref(), line);
            }
            _ => {}
        }
    }

    /// Dedupe last-wins, keyed by `leafUuid` for `summary` and `sessionId` otherwise.
    fn record_sidecar(
        &mut self,
        ty: &str,
        leaf: Option<&str>,
        session: Option<&str>,
        line: &ParsedLine,
    ) {
        if line.value.get("uuid").is_some() {
            return; // a DAG record, not a sidecar
        }
        let key = leaf.or(session).unwrap_or("").to_string();
        self.sidecars
            .insert((ty.to_string(), key), line.value.clone());
    }

    fn observe_results(&mut self, u: &UserRecord) {
        let rich = u.tool_use_result.as_ref();
        let rich_text = rich.map(|v| self.result_text(v)).unwrap_or_default();
        // When the rich payload is bytes rather than text, its description *is* the result:
        // the `tool_result` block beside it is the same bytes in the API's own wrapping, and
        // when `Bash` sets `isImage` that wrapping is a bare string of them, which reads as
        // ordinary output and would otherwise be indexed as one.
        let rich_media = rich
            .and_then(Value::as_object)
            .is_some_and(|o| media::describe_payload(o).is_some());
        let rich_error = rich.is_some_and(is_error_payload) || u.tool_denial_kind.is_some();

        let mut matched = false;
        if let Some(msg) = &u.message {
            for block in msg.content.blocks() {
                let ContentBlock::ToolResult(tr) = block else {
                    continue;
                };
                let Some(id) = tr.tool_use_id.clone() else {
                    continue;
                };
                let mut text = if rich_media {
                    rich_text.clone()
                } else {
                    content_text(&tr.content)
                };
                if text.trim().is_empty() {
                    text = rich_text.clone();
                }
                let text = self.maybe_load_spilled(text);
                matched = true;
                self.outcomes.insert(
                    id,
                    ToolOutcome {
                        text,
                        is_error: tr.is_error || rich_error,
                    },
                );
            }
        }
        if matched || rich.is_none() {
            return;
        }
        // TRANSCRIPT-FORMAT §5: `toolUseResult` "is a bare string on every failure path", and
        // those turns routinely carry a plain-string `message.content` with no `tool_result`
        // block at all. Dropping them here is what made `--errors-only` miss most failures.
        let Some(id) = self.implied_tool_use_id(u) else {
            return;
        };
        let text = if rich_text.trim().is_empty() {
            u.message
                .as_ref()
                .map(|m| content_text(&m.content))
                .unwrap_or_default()
        } else {
            rich_text
        };
        let text = self.maybe_load_spilled(text);
        self.outcomes.insert(
            id,
            ToolOutcome {
                text,
                is_error: rich_error,
            },
        );
    }

    /// Which `tool_use` a result turn answers when it names no `tool_use_id`: the assistant
    /// record it points at with `sourceToolAssistantUUID`, else the most recent tool call this
    /// file has issued and nothing has answered.
    fn implied_tool_use_id(&self, u: &UserRecord) -> Option<String> {
        if let Some(uuid) = u.source_tool_assistant_uuid.as_deref()
            && let Some(ids) = self.tool_uses_by_uuid.get(uuid)
            && let Some(id) = ids.iter().find(|id| !self.outcomes.contains_key(*id))
        {
            return Some(id.clone());
        }
        self.issued_tool_uses
            .iter()
            .rev()
            .find(|id| !self.outcomes.contains_key(*id))
            .cloned()
    }

    /// `<persisted-output> … Full output saved to: <path>` — pull the real thing back in.
    fn maybe_load_spilled(&self, text: String) -> String {
        if !self.opts.load_spilled_results {
            return text;
        }
        let Some(rest) = text.split("Full output saved to:").nth(1) else {
            return text;
        };
        let path = rest.trim_start().lines().next().unwrap_or("").trim();
        if path.is_empty() {
            return text;
        }
        match std::fs::read_to_string(path) {
            // The spill is the whole oversized output; the same cap as any other body applies.
            Ok(spilled) => truncate(&format!("{text}\n{spilled}"), self.opts.max_text_bytes),
            Err(err) => {
                tracing::debug!(%err, path, "spilled tool result unreadable");
                text
            }
        }
    }

    /// Best-effort plain text from a `toolUseResult` payload of any shape.
    fn result_text(&self, v: &Value) -> String {
        let cap = self.opts.max_text_bytes;
        if let Some(s) = v.as_str() {
            return truncate(&media::scrub(s), cap);
        }
        let Some(obj) = v.as_object() else {
            return truncate(&media::scrub(&v.to_string()), cap);
        };
        // A result that is bytes rather than text — a `Read` of an image, a rendered PDF, a
        // `Bash` command whose stdout is image data — indexes what it was, not what it held.
        if let Some(desc) = media::describe_payload(obj) {
            return truncate(&desc, cap);
        }
        let mut parts: Vec<String> = Vec::new();
        let push = |parts: &mut Vec<String>, s: &str| {
            if !s.trim().is_empty() {
                parts.push(s.to_string());
            }
        };
        for key in ["stdout", "stderr", "content", "text"] {
            if let Some(s) = obj.get(key).and_then(Value::as_str) {
                push(&mut parts, s);
            }
        }
        if let Some(file) = obj.get("file").and_then(Value::as_object) {
            if let Some(p) = file.get("filePath").and_then(Value::as_str) {
                push(&mut parts, p);
            }
            if let Some(c) = file.get("content").and_then(Value::as_str) {
                push(&mut parts, c);
            }
        }
        for key in ["filePath", "oldString", "newString"] {
            if let Some(s) = obj.get(key).and_then(Value::as_str) {
                push(&mut parts, s);
            }
        }
        if let Some(names) = obj.get("filenames").and_then(Value::as_array) {
            let joined = names
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n");
            push(&mut parts, &joined);
        }
        if parts.is_empty() {
            // Unknown tool shape: keep the JSON, capped. Still searchable — minus any blob in
            // it, which never was.
            return truncate(&media::scrub(&v.to_string()), cap);
        }
        truncate(&parts.join("\n"), cap)
    }

    /// Second pass: emit docs in file order.
    fn finish(mut self, seq_base: u64) -> ParseOutput {
        let mut docs: Vec<Doc> = Vec::new();
        let mut seq = seq_base;
        let lines = std::mem::take(&mut self.lines);

        for line in &lines {
            self.absorb_metadata(&line.parsed);
            let mut emitted: Vec<PartialDoc> = Vec::new();
            match &line.parsed.record {
                Record::User(u) => self.user_docs(u, &mut emitted),
                Record::Assistant(a) => self.assistant_docs(a, &mut emitted),
                Record::Attachment(a) => self.attachment_docs(a, &mut emitted),
                Record::System(s) => self.system_docs(s, &mut emitted),
                // Sidecars carry no conversational text of their own: `summary` becomes the
                // session title and `queue-operation` duplicates the user record verbatim.
                _ => {}
            }
            for partial in emitted {
                let doc = self.build_doc(partial, &line.parsed, &line.raw, seq);
                seq += 1;
                docs.push(doc);
            }
        }

        self.session.title = self.resolve_title();
        if self.session.first_prompt.is_none() {
            self.session.first_prompt = self
                .fallback_first_prompt
                .clone()
                .or_else(|| self.sidecar_prompt());
        }
        let replacements = self.build_replacements();
        let carry = self.build_carry(&docs);
        ParseOutput {
            docs,
            replacements,
            session: self.session,
            errors: self.errors,
            carry,
        }
    }

    /// Complete the documents a previous run left waiting for a result. The replacement keeps
    /// the original `doc_id` and `seq` and fills in the result exactly as `tool_call_doc`
    /// would have, so the outcome is byte-identical to what a whole-file parse produces.
    fn build_replacements(&self) -> Vec<Doc> {
        let mut out = Vec::new();
        for pending in self.carried_pending.values() {
            let (Some(outcome), Some(doc)) = (
                self.outcomes.get(&pending.tool_use_id),
                pending.doc.as_deref(),
            ) else {
                continue;
            };
            let mut doc = doc.clone();
            // `text`, `code` and `body` are all the call side, which the result cannot
            // change, so completing a document only fills in `tool_output`. Byte-identity with
            // a whole-file parse is what it always was; it no longer depends on rebuilding the
            // body, because there is no longer an ordering inside it to get wrong.
            doc.tool_output =
                Some(media::scrub(&truncate(&outcome.text, self.opts.max_text_bytes)).into_owned())
                    .filter(|s| !s.is_empty());
            doc.is_error = outcome.is_error;
            out.push(doc);
        }
        out
    }

    /// What the next tail parse of this file needs to know about the bytes this one consumed.
    fn build_carry(&self, docs: &[Doc]) -> ParseCarry {
        let emitted: HashMap<&str, &Doc> = docs
            .iter()
            .filter(|d| d.kind == DocKind::ToolCall)
            .filter_map(|d| d.tool_use_id.as_deref().map(|id| (id, d)))
            .collect();

        // Everything issued — this run, or carried in from an earlier one — that no result has
        // answered yet is still liable to be answered across the next boundary.
        let mut pending: Vec<PendingToolCall> = self
            .carried_pending
            .values()
            .filter(|p| {
                !self.outcomes.contains_key(&p.tool_use_id)
                    && !self.issued_index.contains(&p.tool_use_id)
            })
            .cloned()
            .collect();
        for id in &self.issued_tool_uses {
            if self.outcomes.contains_key(id) {
                continue;
            }
            pending.push(PendingToolCall {
                tool_use_id: id.clone(),
                doc: emitted.get(id.as_str()).and_then(|d| carryable(d)),
            });
        }
        if pending.len() > PENDING_CAP {
            pending.drain(..pending.len() - PENDING_CAP);
        }

        ParseCarry {
            pending_tool_uses: pending,
            counted_message_ids: tail_of(self.counted_order.clone(), CARRY_CAP),
            charged_message_ids: tail_of(self.charged_order.clone(), CARRY_CAP),
            tail_line: self.tail_line,
        }
    }

    /// Claim a message's thinking cost. Returns false if it was already charged, including in
    /// an earlier incremental run.
    fn charge_message(&mut self, id: &str) -> bool {
        if self.charged_message_ids.insert(id.to_string()) {
            self.charged_order.push(id.to_string());
            true
        } else {
            false
        }
    }

    /// Count one API message, claiming its `message.id` so its sibling block records — which
    /// are not contiguous (§7) and may land on the other side of an incremental boundary —
    /// cannot count it again. Claiming happens *here*, at the point of emission, and not when
    /// the record is first seen: a message whose first block record emits nothing (an empty
    /// `thinking` block, or a `tool_use`) would otherwise burn the id and never be counted.
    fn count_message(&mut self, id: Option<&str>) {
        self.session.messages += 1;
        if let Some(id) = id
            && self.counted_message_ids.insert(id.to_string())
        {
            self.counted_order.push(id.to_string());
        }
    }

    /// The title is the `summary` pointing at the newest record we have seen, else the last
    /// summary in the file.
    fn resolve_title(&self) -> Option<String> {
        if let Some(uuid) = &self.last_uuid
            && let Some(text) = self.summaries.get(uuid)
        {
            return Some(text.clone());
        }
        self.summary_order
            .last()
            .and_then(|k| self.summaries.get(k))
            .cloned()
    }

    fn absorb_metadata(&mut self, line: &ParsedLine) {
        let common = match &line.record {
            Record::User(u) => Some(&u.common),
            Record::Assistant(a) => Some(&a.common),
            Record::Attachment(a) => Some(&a.common),
            Record::System(s) => Some(&s.common),
            _ => None,
        };
        let Some(common) = common else { return };

        if self.session.session_id.is_empty()
            && let Some(id) = common.session_id.as_deref().filter(|s| !s.is_empty())
        {
            self.session.session_id = id.to_string();
        }
        // NB: `agentId` is deliberately NOT absorbed from records. A subagent file takes its
        // id from its path; a main transcript that happens to hold one inline sidechain record
        // (§8, older layout) must not acquire that agent's identity for the whole session.
        if self.session.project.is_none()
            && let Some(cwd) = common.cwd.as_deref().filter(|s| !s.is_empty())
        {
            self.session.project = Some(cwd.to_string());
        }
        if let Some(b) = common.git_branch.as_deref().filter(|s| !s.is_empty()) {
            self.session.git_branch = Some(b.to_string());
        }
        if self.session.slug.is_none()
            && let Some(s) = common.slug.as_deref().filter(|s| !s.is_empty())
        {
            self.session.slug = Some(s.to_string());
        }
        // Same reasoning as `agentId`: `attributionAgent` names the agent that wrote *this*
        // record, so it only describes the session when the session is a sidechain one.
        if let Record::Assistant(a) = &line.record
            && self.session.agent_type.is_none()
            && (common.is_sidechain || self.path_agent_id.is_some())
            && let Some(t) = a.attribution_agent.as_deref().filter(|s| !s.is_empty())
        {
            self.session.agent_type = Some(t.to_string());
        }
        if let Some(ms) = common.timestamp.as_deref().and_then(parse_ts_ms) {
            self.session.first_ts_ms = Some(self.session.first_ts_ms.map_or(ms, |f| f.min(ms)));
            self.session.last_ts_ms = Some(self.session.last_ts_ms.map_or(ms, |l| l.max(ms)));
        }
        if let Some(uuid) = common.uuid.as_deref().filter(|s| !s.is_empty()) {
            self.last_uuid = Some(uuid.to_string());
        }
    }

    // -- per record-type emission -------------------------------------------

    fn user_docs(&mut self, u: &UserRecord, out: &mut Vec<PartialDoc>) {
        let is_meta = u.common.is_meta
            || u.is_compact_summary
            || u.is_visible_in_transcript_only
            || u.turn_companion;
        let Some(msg) = &u.message else { return };

        match &msg.content {
            MessageContent::Text(s) if !s.trim().is_empty() => {
                self.note_first_prompt(u, s);
                // §9: a compaction summary is not a turn. `is_meta` also covers the
                // transcript-only and companion turns, which are not turns either.
                if !is_meta {
                    self.session.messages += 1;
                }
                out.push(PartialDoc::markdown("user", s, self.opts.max_text_bytes).meta(is_meta));
            }
            MessageContent::Blocks(blocks) => {
                let mut texts: Vec<String> = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text(t) => {
                            if let Some(s) = t.text.as_deref().filter(|s| !s.trim().is_empty()) {
                                texts.push(s.to_string());
                            }
                        }
                        // A pasted screenshot is part of the turn even though none of it is
                        // text: the description stands in for it, so the message is counted,
                        // its prose is indexed beside it, and "what was that image" is a
                        // query that can hit.
                        ContentBlock::Image(m) => texts.push(media_text(m, "image")),
                        ContentBlock::Document(m) => texts.push(media_text(m, "document")),
                        ContentBlock::ToolResult(tr) => {
                            // Orphaned result: the `tool_use` never appeared in this file, so
                            // nothing else will carry it.
                            let id = tr.tool_use_id.clone().unwrap_or_default();
                            // `carried_tool_uses` is the whole point of `ParseCarry`: on a
                            // tail parse the matching `tool_use` lives in the already-consumed
                            // prefix, so without it this reads as an orphan and emits a second
                            // half-empty document for a tool call that is already indexed.
                            if id.is_empty()
                                || self.issued_index.contains(&id)
                                || self.carried_pending.contains_key(&id)
                            {
                                continue;
                            }
                            let outcome = self.outcomes.get(&id).cloned().unwrap_or_default();
                            self.session.tool_calls += 1;
                            // An orphan has no call side at all — no name, no input — so its
                            // whole body is the result, and `text`, `code` and `body` stay
                            // empty.
                            out.push(
                                PartialDoc::tool_call(
                                    None,
                                    Some(id),
                                    None,
                                    String::new(),
                                    Vec::new(),
                                    outcome.is_error,
                                )
                                .output(truncate(&outcome.text, self.opts.max_text_bytes))
                                .role("user")
                                .meta(is_meta),
                            );
                        }
                        _ => {}
                    }
                }
                if !texts.is_empty() {
                    let joined = texts.join("\n");
                    self.note_first_prompt(u, &joined);
                    if !is_meta {
                        self.session.messages += 1;
                    }
                    out.push(
                        PartialDoc::markdown("user", &joined, self.opts.max_text_bytes)
                            .meta(is_meta),
                    );
                }
            }
            MessageContent::Other(v) if !v.is_null() => {
                if !is_meta {
                    self.session.messages += 1;
                }
                out.push(
                    PartialDoc::message("user", truncate(&v.to_string(), self.opts.max_text_bytes))
                        .meta(is_meta),
                );
            }
            _ => {}
        }
    }

    fn note_first_prompt(&mut self, u: &UserRecord, text: &str) {
        let text = &*media::scrub(text);
        if self.session.first_prompt.is_none() && u.is_human_turn() {
            self.session.first_prompt = Some(truncate_chars(text, FIRST_PROMPT_CHARS));
        }
        if self.fallback_first_prompt.is_none() {
            self.fallback_first_prompt = Some(truncate_chars(text, FIRST_PROMPT_CHARS));
        }
    }

    /// `last-prompt` sidecars, deduped last-wins, as the final fallback for the opening
    /// prompt of a file whose user records carry no text at all.
    fn sidecar_prompt(&self) -> Option<String> {
        self.last_prompt
            .as_deref()
            .map(|s| truncate_chars(s, FIRST_PROMPT_CHARS))
    }

    fn assistant_docs(&mut self, a: &AssistantRecord, out: &mut Vec<PartialDoc>) {
        let Some(msg) = &a.message else { return };
        let model = msg.model.clone();
        // Blocks of one API message are split one per record and share a `message.id`, so the
        // message is counted by whichever of its block records is the first to *emit* — not by
        // whichever comes first in the file.
        let id = msg.id.clone();
        let mut counted = id
            .as_deref()
            .is_some_and(|id| self.counted_message_ids.contains(id));
        let first_emitted = out.len();

        for block in msg.content.blocks() {
            match block {
                ContentBlock::Text(t) => {
                    let Some(s) = t.text.as_deref().filter(|s| !s.trim().is_empty()) else {
                        continue;
                    };
                    if !counted {
                        self.count_message(id.as_deref());
                        counted = true;
                    }
                    out.push(
                        PartialDoc::markdown("assistant", s, self.opts.max_text_bytes)
                            .model(model.clone())
                            .error(a.is_api_error_message),
                    );
                }
                ContentBlock::Thinking(t) => {
                    let Some(s) = t.thinking.as_deref().filter(|s| !s.trim().is_empty()) else {
                        continue;
                    };
                    if !counted {
                        self.count_message(id.as_deref());
                        counted = true;
                    }
                    // Thinking is stored in its own field, never in `text`.
                    out.push(
                        PartialDoc::message("assistant", String::new())
                            .model(model.clone())
                            .thinking(Some(truncate(s, self.opts.max_text_bytes))),
                    );
                }
                ContentBlock::ToolUse(tu)
                | ContentBlock::McpToolUse(tu)
                | ContentBlock::ServerToolUse(tu) => {
                    self.session.tool_calls += 1;
                    out.push(self.tool_call_doc(tu).model(model.clone()));
                }
                _ => {}
            }
        }

        // `thinking_tokens` is a per-message total that appears on *some* of the message's
        // block records — usually the `tool_use` one, not the first to emit — and is repeated
        // on 1 to 4 of them with the same value. So charge it on a record that actually carries
        // it, once per message, or the sum is multiplied by however many records repeated it.
        if let Some(tokens) = msg
            .usage
            .as_ref()
            .and_then(|u| u.output_tokens_details.as_ref())
            .and_then(|d| d.thinking_tokens)
            .filter(|n| *n > 0)
            && let Some(doc) = out.get_mut(first_emitted)
            && id.as_deref().is_none_or(|id| self.charge_message(id))
        {
            doc.thinking_tokens = Some(tokens);
        }
    }

    /// The indexed body of a tool-call document: the tool name and the input's own strings, so
    /// `Bash cargo build` matches on `text` as well as through `tool_input.command`.
    ///
    /// The result is NOT here — it is a field of its own, `tool_output`. That is what retired
    /// the ordering this function used to carry: a failed call's body used to lead with the
    /// error, because a preview of `text` otherwise showed a heredoc and never the one line
    /// saying what broke. With two fields the same guarantee is a rendering decision instead
    /// (`format::doc_body`, and the snippet fallback in `search`), which reaches the previews
    /// that motivated it without reordering bytes anyone might later search.
    /// A tool call, with each of its three parts in the field that suits it.
    ///
    /// `text` keeps the tool name and the input's own strings, so `Bash cargo build` matches
    /// on `text` as well as through `tool_input.command`. The `Edit`/`Write` payloads
    /// (`old_string`, `new_string`, `content`) go to `code` — those are file contents, and the
    /// prose analyzer would stem every identifier in them. The **result** goes to
    /// `tool_output`, which is neither: it is what the tool returned, not what it was asked
    /// to do.
    ///
    /// The input copy does not scale with the cap: `text` and `code` share a quarter of the
    /// budget between them, and never more than `INPUT_LEAVES_CAP`, because `tool_input`
    /// already carries a `Write` payload whole. `tool_output` is capped separately at the full
    /// `max_text_bytes`, since it is a field of its own rather than a share of one body.
    fn tool_call_doc(&self, tu: &ToolUseBlock) -> PartialDoc {
        let outcome = tu
            .id
            .as_ref()
            .and_then(|id| self.outcomes.get(id))
            .cloned()
            .unwrap_or_default();

        let cap = self.opts.max_text_bytes;
        let input_budget = (cap / 4).min(INPUT_LEAVES_CAP);
        let mut body = String::new();
        let mut code: Vec<String> = Vec::new();
        if let Some(name) = &tu.name {
            body.push_str(name);
            body.push('\n');
        }
        if let Some(input) = &tu.input {
            let (words, contents) = input_leaves(input);
            // Both halves of the copy share the one `input_budget`. Routing the file contents
            // to `code` instead of into the same string must not double what the cap allows.
            let words = truncate(&words.join("\n"), input_budget);
            let contents_budget = input_budget.saturating_sub(words.len());
            body.push_str(&words);
            body.push('\n');
            code.extend(entry(&contents.join("\n"), contents_budget));
        }

        // `bash_cmd` is a Bash-only structured view of the command line, so `--program cargo`
        // finds every script that ran `cargo` anywhere — inside a pipeline, an `&&` chain or a
        // loop body — which searching the raw command text cannot do without also matching
        // `--cargo-flag` or a path segment.
        let bash_cmd = (tu.name.as_deref() == Some("Bash"))
            .then(|| {
                tu.input
                    .as_ref()
                    .and_then(|input| input.get("command"))
                    .and_then(Value::as_str)
                    .and_then(crate::bash::extract)
                    .map(|cmd| cmd.to_json())
            })
            .flatten();

        PartialDoc::tool_call(
            tu.name.clone(),
            tu.id.clone(),
            // `tool_input` is indexed, not just stored, so a blob in it would become a term.
            tu.input.as_ref().map(media::redacted),
            body,
            code,
            outcome.is_error,
        )
        .output(truncate(&outcome.text, self.opts.max_text_bytes))
        .bash_cmd(bash_cmd)
    }

    fn attachment_docs(&mut self, a: &AttachmentRecord, out: &mut Vec<PartialDoc>) {
        let empty = Attachment::default();
        let att = a.attachment.as_ref().unwrap_or(&empty);
        let kind = att.kind.as_deref().unwrap_or("");
        if SKIPPED_ATTACHMENTS.contains(&kind) {
            return;
        }
        let text = attachment_text(a, att, self.opts.max_text_bytes);
        if text.trim().is_empty() {
            return;
        }
        let body = if kind.is_empty() {
            text
        } else {
            format!("{kind}\n{text}")
        };
        // Attachments are indexed (they carry real user text, environment snapshots, …) but
        // they are not conversational turns, so they do not move the message count.
        out.push(
            PartialDoc::message("attachment", truncate(&body, self.opts.max_text_bytes))
                .meta(a.common.is_meta),
        );
    }

    fn system_docs(&mut self, s: &SystemRecord, out: &mut Vec<PartialDoc>) {
        let subtype = s.subtype.as_deref().unwrap_or("");
        let content = s.content.as_deref().unwrap_or("");
        if subtype.is_empty() && content.trim().is_empty() {
            return;
        }
        let body = if content.trim().is_empty() {
            subtype.to_string()
        } else {
            format!("{subtype}\n{content}")
        };
        // §9: the `compact_boundary` half of a compaction is excluded from message counts.
        let is_meta = s.common.is_meta || subtype == "compact_boundary";
        if !is_meta {
            self.session.messages += 1;
        }
        out.push(
            PartialDoc::message("system", truncate(&body, self.opts.max_text_bytes)).meta(is_meta),
        );
    }

    fn build_doc(&self, p: PartialDoc, line: &ParsedLine, raw: &str, seq: u64) -> Doc {
        let common = match &line.record {
            Record::User(u) => Some(&u.common),
            Record::Assistant(a) => Some(&a.common),
            Record::Attachment(a) => Some(&a.common),
            Record::System(s) => Some(&s.common),
            _ => None,
        };
        let session_id = common
            .and_then(|c| c.session_id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.session.session_id.clone());
        // The record's own `agentId` wins; the file-level fallback is the *path*, never the
        // progressively absorbed session id, so an inline sidechain record (§8) cannot lend
        // its identity to the ordinary turns that follow it in a main transcript.
        let agent_id = common
            .and_then(|c| c.agent_id.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| self.path_agent_id.clone());
        let is_sidechain = common.is_some_and(|c| c.is_sidechain) || self.path_agent_id.is_some();
        let agent_type = match &line.record {
            Record::Assistant(a) => a.attribution_agent.clone(),
            _ => None,
        }
        .or_else(|| {
            is_sidechain
                .then(|| self.session.agent_type.clone())
                .flatten()
        });

        Doc {
            doc_id: format!(
                "{}:{}:{}:{}",
                session_id,
                agent_id.as_deref().unwrap_or("-"),
                self.file_tag,
                seq
            ),
            kind: p.kind,
            source_path: self.session.source_path.clone(),
            seq,
            session_id,
            agent_id,
            agent_type,
            uuid: common.and_then(|c| c.uuid.clone()),
            // Kept verbatim even when it dangles; conversational order is file order.
            parent_uuid: common.and_then(|c| c.parent_uuid.clone()),
            timestamp_ms: common
                .and_then(|c| c.timestamp.as_deref())
                .and_then(parse_ts_ms),
            project: common
                .and_then(|c| c.cwd.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| self.session.project.clone()),
            git_branch: common.and_then(|c| c.git_branch.clone()),
            role: p.role,
            model: p.model,
            tool_name: p.tool_name,
            tool_use_id: p.tool_use_id,
            tool_input: p.tool_input,
            bash_cmd: p.bash_cmd,
            is_error: p.is_error,
            is_sidechain,
            is_meta: p.is_meta || common.is_some_and(|c| c.is_meta),
            entrypoint: common.and_then(|c| c.entrypoint.clone()),
            permission_mode: common.and_then(|c| c.permission_mode.clone()),
            version: common.and_then(|c| c.version.clone()),
            slug: common
                .and_then(|c| c.slug.clone())
                .or_else(|| self.session.slug.clone()),
            // Every indexed body passes through here, so this is where "no payload is ever a
            // term" is *guaranteed* rather than argued shape by shape. The descriptions the
            // parser built above survive it untouched; anything a future transcript smuggles
            // past them does not — and that has to hold for every half of the split, not just
            // the one the payload happened to land in.
            body: media::scrub(&p.body).into_owned(),
            text: scrub_all(p.text),
            code: scrub_all(p.code),
            headings: scrub_all(p.headings),
            code_langs: p.code_langs,
            tool_output: p.tool_output.map(|t| media::scrub(&t).into_owned()),
            thinking: p.thinking.map(|t| media::scrub(&t).into_owned()),
            thinking_tokens: p.thinking_tokens,
            // `raw` is stored, never indexed and never returned (`format`, `docs/MCP.md`), so
            // a 300 KiB pasted photo here is pure weight: it inflates the index by the size of
            // the transcript's images and can push a pending tool call past `PENDING_DOC_CAP`,
            // losing the carry. The elision is lexical, so everything that is not an encoded
            // payload survives byte for byte.
            raw: media::scrub(raw).into_owned(),
        }
    }
}

/// Everything about a doc that comes from the *content block*, before the record-level
/// metadata is folded in.
struct PartialDoc {
    kind: DocKind,
    role: String,
    body: String,
    text: Vec<String>,
    code: Vec<String>,
    headings: Vec<String>,
    code_langs: Vec<String>,
    tool_output: Option<String>,
    thinking: Option<String>,
    thinking_tokens: Option<u64>,
    model: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    tool_input: Option<Value>,
    bash_cmd: Option<Value>,
    is_error: bool,
    is_meta: bool,
}

impl PartialDoc {
    /// A message body indexed verbatim: no markdown split. For the record types whose text is
    /// not markdown — a JSON dump, an attachment, a `system` notice.
    fn message(role: &str, text: String) -> PartialDoc {
        PartialDoc {
            kind: DocKind::Message,
            role: role.to_string(),
            body: text.clone(),
            text: entry(&text, usize::MAX).into_iter().collect(),
            code: Vec::new(),
            headings: Vec::new(),
            code_langs: Vec::new(),
            tool_output: None,
            thinking: None,
            thinking_tokens: None,
            model: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            bash_cmd: None,
            is_error: false,
            is_meta: false,
        }
    }

    /// A markdown message: prose to `text`, blocks and spans to `code`, headings to both.
    ///
    /// The body is capped **before** the split rather than after, so the same 32KB of a giant
    /// message is considered as was considered before the split existed — the pieces divide
    /// that budget instead of each getting one. Truncating markdown can cut a fence in half;
    /// `markdown::split` treats an unclosed fence as a closed one.
    fn markdown(role: &str, body: &str, cap: usize) -> PartialDoc {
        let body = truncate(body, cap);
        let parts = crate::markdown::split(&body);
        PartialDoc {
            kind: DocKind::Message,
            role: role.to_string(),
            body,
            text: parts.text,
            code: parts.code,
            headings: parts.headings,
            code_langs: parts.code_langs,
            tool_output: None,
            thinking: None,
            thinking_tokens: None,
            model: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            bash_cmd: None,
            is_error: false,
            is_meta: false,
        }
    }

    /// `text` is the call side — the tool name and the input's own strings. The result goes in
    /// `tool_output` via [`PartialDoc::output`], never concatenated onto `text`.
    fn tool_call(
        tool_name: Option<String>,
        tool_use_id: Option<String>,
        tool_input: Option<Value>,
        text: String,
        code: Vec<String>,
        is_error: bool,
    ) -> PartialDoc {
        PartialDoc {
            kind: DocKind::ToolCall,
            role: "assistant".to_string(),
            body: rendered_body(&text, &code),
            text: entry(&text, usize::MAX).into_iter().collect(),
            code,
            headings: Vec::new(),
            code_langs: Vec::new(),
            tool_output: None,
            thinking: None,
            thinking_tokens: None,
            model: None,
            tool_name,
            tool_use_id,
            tool_input,
            bash_cmd: None,
            is_error,
            is_meta: false,
        }
    }

    fn role(mut self, role: &str) -> Self {
        self.role = role.to_string();
        self
    }
    fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }
    fn thinking(mut self, thinking: Option<String>) -> Self {
        self.thinking = thinking;
        self
    }
    /// Empty output is `None`, not `Some("")`: "the tool returned nothing" and "the result has
    /// not arrived yet" both render as absent, and neither should occupy a posting list.
    fn output(mut self, output: String) -> Self {
        self.tool_output = Some(output).filter(|s| !s.is_empty());
        self
    }
    fn bash_cmd(mut self, bash_cmd: Option<Value>) -> Self {
        self.bash_cmd = bash_cmd;
        self
    }
    fn meta(mut self, is_meta: bool) -> Self {
        self.is_meta = is_meta;
        self
    }
    fn error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

struct FileIds {
    session_id: String,
    agent_id: Option<String>,
}

/// Session / agent ids implied by the path. Records override these when they disagree; this
/// is only the fallback for a file whose records carry neither.
fn ids_from_path(path: &Path) -> FileIds {
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_suffix(".jsonl"))
        .unwrap_or("");
    if let Some(agent) = stem.strip_prefix("agent-") {
        let parts: Vec<&str> = path.iter().filter_map(|c| c.to_str()).collect::<Vec<_>>();
        let session = parts
            .iter()
            .position(|p| *p == "subagents")
            .and_then(|i| i.checked_sub(1))
            .map(|i| parts[i].to_string())
            .unwrap_or_default();
        FileIds {
            session_id: session,
            agent_id: Some(agent.to_string()),
        }
    } else {
        FileIds {
            session_id: stem.to_string(),
            agent_id: None,
        }
    }
}

/// A copy of a document small enough to sit in `state.json` until its result arrives.
fn carryable(doc: &Doc) -> Option<Box<Doc>> {
    let size = doc.body.len() + doc.raw.len() + doc.tool_output.as_deref().map_or(0, str::len);
    (size <= PENDING_DOC_CAP).then(|| Box::new(doc.clone()))
}

/// The last `cap` elements, in order. Used to bound what one file's [`ParseCarry`] persists.
fn tail_of(mut items: Vec<String>, cap: usize) -> Vec<String> {
    if items.len() > cap {
        items.drain(..items.len() - cap);
    }
    items
}

fn tool_use_block(block: &ContentBlock) -> Option<&ToolUseBlock> {
    match block {
        ContentBlock::ToolUse(t) | ContentBlock::McpToolUse(t) | ContentBlock::ServerToolUse(t) => {
            Some(t)
        }
        _ => None,
    }
}

/// Plain text of a `String | Array<block>` content.
fn content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => media::scrub(s).into_owned(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => t.text.clone(),
                ContentBlock::Thinking(t) => t.thinking.clone(),
                ContentBlock::Image(m) => Some(media_text(m, "image")),
                ContentBlock::Document(m) => Some(media_text(m, "document")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        MessageContent::Other(v) if v.is_null() => String::new(),
        // An unmodelled content shape is kept as JSON so it stays searchable; blobs inside it
        // are not text and never were.
        MessageContent::Other(v) => media::scrub(&v.to_string()).into_owned(),
    }
}

/// The one line an `image` / `document` block contributes to an indexed body: what it was, how
/// big, and where it came from — never the bytes. See `crate::media`.
fn media_text(m: &MediaBlock, kind: &str) -> String {
    media::describe_block(kind, m.source.as_ref(), &m.extra)
}

fn attachment_text(rec: &AttachmentRecord, att: &Attachment, cap: usize) -> String {
    // `queued_command` is real user text; its `rendered` form is a system notice wrapper.
    if att.kind.as_deref() == Some("queued_command")
        && let Some(prompt) = att.extra.get("prompt").and_then(Value::as_str)
    {
        return truncate(prompt, cap);
    }
    let rendered: Vec<&str> = rec
        .rendered
        .iter()
        .chain(rec.rendered_in_human_turn.iter())
        .filter_map(|r| r.content.as_deref())
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !rendered.is_empty() {
        return truncate(&rendered.join("\n"), cap);
    }
    let mut leaves = Vec::new();
    for (_, v) in att.extra.iter() {
        collect_strings(v, 0, &mut leaves);
    }
    truncate(&leaves.join("\n"), cap)
}

/// String leaves of a JSON value, depth-limited so a pathological payload cannot blow up.
fn collect_strings(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 4 || out.len() > 256 {
        return;
    }
    match v {
        // Wherever a leaf turns out to be an encoded payload — an MCP tool's `data`, an
        // attachment nobody modelled — the description goes in and the bytes do not.
        Value::String(s) if !s.is_empty() => out.push(media::scrub(s).into_owned()),
        Value::Array(items) => items
            .iter()
            .for_each(|i| collect_strings(i, depth + 1, out)),
        Value::Object(map) => map
            .iter()
            .for_each(|(_, i)| collect_strings(i, depth + 1, out)),
        _ => {}
    }
}

/// Tool-input keys whose value is a chunk of a file rather than a parameter someone typed.
const FILE_CONTENT_KEYS: &[&str] = &["old_string", "new_string", "content"];

/// String leaves of a tool input, split into the ones that name what the call *does* and the
/// ones that are file content.
///
/// The check is by key at any depth, so a `MultiEdit` — whose `edits` is an array of objects
/// each holding `old_string` and `new_string` — is split the same way a single `Edit` is, and
/// once a key marks a subtree as content everything under it is content.
fn input_leaves(v: &Value) -> (Vec<String>, Vec<String>) {
    fn walk(
        v: &Value,
        depth: usize,
        is_content: bool,
        words: &mut Vec<String>,
        code: &mut Vec<String>,
    ) {
        if depth > 4 || words.len() + code.len() > 256 {
            return;
        }
        match v {
            Value::String(s) if !s.is_empty() => {
                if is_content {
                    code.push(s.clone());
                } else {
                    words.push(s.clone());
                }
            }
            Value::Array(items) => items
                .iter()
                .for_each(|i| walk(i, depth + 1, is_content, words, code)),
            Value::Object(map) => map.iter().for_each(|(key, i)| {
                let content = is_content || FILE_CONTENT_KEYS.contains(&key.as_str());
                walk(i, depth + 1, content, words, code)
            }),
            _ => {}
        }
    }
    let (mut words, mut code) = (Vec::new(), Vec::new());
    walk(v, 0, false, &mut words, &mut code);
    (words, code)
}

/// A tool call as a reader sees it: its name and input strings, then each `code` entry — the
/// file contents it carried — on a line of its own. Its *result* is not here: that lives in
/// `tool_output`, and the renderers print it beside the body rather than inside it.
///
/// A tool call has no source markdown to keep, so its `body` is assembled from the same two
/// halves that are indexed, in the order they were built.
fn rendered_body(text: &str, code: &[String]) -> String {
    let mut out = text.trim_end().to_string();
    for block in code {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(block);
    }
    out
}

/// Every entry of a multi-valued field, with any encoded payload elided ([`media::scrub`]).
///
/// The guarantee "no payload is ever a term" has to hold per entry: the split routes a base64
/// blob to whichever half it was written in, so scrubbing only the joined body would leave the
/// other half holding it.
fn scrub_all(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|v| media::scrub(&v).into_owned())
        .collect()
}

/// `text` truncated to `max` bytes as a one-element vector, or nothing when it is blank.
///
/// The multi-valued fields hold real content or no value at all: a blank entry would cost a
/// stored string and a position gap for nothing.
fn entry(text: &str, max: usize) -> Option<String> {
    let kept = truncate(text, max);
    (!kept.trim().is_empty()).then_some(kept)
}

/// `toolUseResult` is a bare string on every failure path.
fn is_error_payload(v: &Value) -> bool {
    match v {
        Value::String(s) => {
            let s = s.trim_start();
            s.starts_with("Error")
                || s.starts_with("InputValidationError")
                || s.starts_with("Conversation ended by model")
                || s.starts_with("Streaming fallback")
        }
        Value::Object(o) => o
            .get("interrupted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

fn parse_ts_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Truncate to at most `max` bytes, on a char boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn parse(name: &str) -> (ParseOutput, u64) {
        parse_file(
            &fixture(name),
            0,
            0,
            &ParseOptions::default(),
            &FileContext::default(),
        )
        .expect("fixture must parse")
    }

    /// Parse an ad-hoc transcript body written into a temporary directory.
    fn parse_body(dir: &Path, name: &str, body: &str) -> ParseOutput {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        parse_whole(&path, &ParseOptions::default()).expect("body must parse")
    }

    fn texts(out: &ParseOutput) -> Vec<String> {
        out.docs.iter().map(text_of).collect()
    }

    /// The prose half of a document as one string. `text` is one entry per block, so the
    /// assertions that only care *that* a sentence was indexed join them back up.
    fn text_of(d: &Doc) -> String {
        d.text.join("\n")
    }

    /// The `code` half of a document as one string: a message's blocks and spans, and the
    /// `Edit`/`Write` file contents of a tool call.
    fn code_of(d: &Doc) -> String {
        d.code.join("\n")
    }

    /// Everything a document indexes, for assertions that do not care which of the fields the
    /// split and the call/result divide happened to put it in.
    fn body_of(d: &Doc) -> String {
        format!(
            "{}\n{}\n{}",
            d.text.join("\n"),
            d.code.join("\n"),
            d.tool_output.as_deref().unwrap_or("")
        )
    }

    /// The reason `crate::media` exists: a phone photo pasted into a prompt is ~300 KB of
    /// base64 on the same line as the sentence about it. The sentence is the document; the
    /// bytes are not, and the description is what stands in for them.
    #[test]
    fn a_pasted_image_indexes_a_description_beside_the_prose_never_the_bytes() {
        let (out, _) = parse("images.jsonl");
        let prompt = out
            .docs
            .iter()
            .find(|d| d.role == "user" && d.kind == DocKind::Message)
            .expect("the human turn is a document");

        // One prose block: the description and the sentence are one paragraph, and a soft
        // break inside a block does not start a new entry.
        assert_eq!(
            text_of(prompt),
            "[image/jpeg 22 KiB]\nAttached a picture so it'd be in the session"
        );
        assert_eq!(
            out.session.messages, 1,
            "an image-bearing turn is still one turn"
        );
        assert_eq!(
            out.session.first_prompt.as_deref(),
            Some("[image/jpeg 22 KiB]\nAttached a picture so it'd be in the session"),
        );
    }

    /// `Read` of a PNG: the path is on the call and the payload is on the result. Keeping the
    /// first and dropping the second is the whole of "paths are fine, bytes are not".
    #[test]
    fn an_image_result_keeps_the_path_from_the_call_and_describes_the_payload() {
        let (out, _) = parse("images.jsonl");
        let read = out
            .docs
            .iter()
            .find(|d| d.tool_name.as_deref() == Some("Read"))
            .expect("the Read call is a document");

        assert!(
            text_of(read).contains("/home/user/shots/joel-watch.png"),
            "the path is the searchable fact: {:?}",
            read.text
        );
        assert_eq!(read.tool_output.as_deref(), Some("[image/png 15 KiB]"));
    }

    /// `Bash` flags image bytes in `stdout` rather than moving them; the warning printed beside
    /// them on `stderr` is ordinary output and has to survive.
    #[test]
    fn a_bash_result_flagged_as_an_image_keeps_its_stderr() {
        let (out, _) = parse("images.jsonl");
        let bash = out
            .docs
            .iter()
            .find(|d| d.tool_name.as_deref() == Some("Bash"))
            .expect("the Bash call is a document");

        assert_eq!(
            bash.tool_output.as_deref(),
            Some("[image 9 KiB]\nimport: warning: no display")
        );
    }

    /// The backstop, over every shape in the fixture at once — including the two nobody
    /// modelled, an MCP tool's flattened `data` block and its `{renders:[{b64}]}` result.
    /// Whatever a future CLI does with images, none of it becomes a term.
    #[test]
    fn no_field_of_any_document_carries_an_encoded_payload() {
        let (out, _) = parse("images.jsonl");
        assert_eq!(out.docs.len(), 4, "one turn and three tool calls");

        // The blob the fixture is built from: `crate::media`'s placeholder is the only thing
        // that may survive of it, in any field, indexed or merely stored.
        let smell = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        for doc in &out.docs {
            // Every field, including each entry of the multi-valued ones: the split routes a
            // blob to whichever half it was written in, so checking a joined string would let
            // the other half through.
            for (field, values) in [
                ("body", vec![doc.body.clone()]),
                ("text", doc.text.clone()),
                ("code", doc.code.clone()),
                ("headings", doc.headings.clone()),
                ("tool_output", doc.tool_output.clone().into_iter().collect()),
                ("thinking", doc.thinking.clone().into_iter().collect()),
                (
                    "tool_input",
                    doc.tool_input
                        .as_ref()
                        .map(Value::to_string)
                        .into_iter()
                        .collect(),
                ),
                ("raw", vec![doc.raw.clone()]),
            ] {
                for value in values {
                    assert!(
                        !value.contains(smell),
                        "{} of {} kept a payload: {}...",
                        field,
                        doc.doc_id,
                        &value[..120.min(value.len())]
                    );
                }
            }
        }
    }

    /// `raw` is stored, never indexed and never returned, so eliding a blob there is free —
    /// but only if the elision is surgical. Every other byte of the line is the record.
    #[test]
    fn scrubbing_raw_leaves_the_rest_of_the_line_intact() {
        let (out, _) = parse("images.jsonl");
        let prompt = out
            .docs
            .iter()
            .find(|d| d.role == "user" && d.kind == DocKind::Message)
            .unwrap();

        let raw: Value = serde_json::from_str(&prompt.raw).expect("still a JSON line");
        assert_eq!(
            raw["message"]["content"][0]["source"]["media_type"],
            "image/jpeg"
        );
        assert_eq!(
            raw["message"]["content"][0]["source"]["data"],
            "[base64 22 KiB]"
        );
        assert_eq!(
            raw["message"]["content"][1]["text"],
            "Attached a picture so it'd be in the session"
        );
        assert!(
            prompt.raw.len() < 2_000,
            "the line went from 30 KB of base64 to nothing: {}",
            prompt.raw.len()
        );
    }

    /// `thinking_tokens` is a per-message total that appears on only *some* of the message's
    /// block records — in real transcripts usually the `tool_use` one, not the first to emit —
    /// and is repeated with the same value on up to four of them. Charging per record would
    /// multiply the total; keying off "the record that emitted first" would miss it entirely,
    /// which is how a first attempt at this recovered 20,154 of a real 185,472 tokens.
    #[test]
    fn thinking_tokens_are_charged_once_on_a_record_that_carries_them() {
        let block = |idx: u32, uuid: &str, parent: &str, content: &str, usage: &str| {
            format!(
                r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","timestamp":"2026-09-09T19:07:19.248Z","sessionId":"sess-1","cwd":"/home/user/proj","gitBranch":"main","version":"2.1.266","isSidechain":false,"apiBlockIndex":{idx},"requestId":"req1","message":{{"role":"assistant","id":"msg1","model":"claude-opus-5","content":[{content}],"usage":{{"output_tokens":623{usage}}}}}}}"#
            )
        };
        let carries = r#","output_tokens_details":{"thinking_tokens":300}"#;
        let body = [
            // Emits first, and does NOT carry the count. (On a stripped transcript this block
            // emits nothing at all, since the thinking text is empty.)
            block(
                0,
                "a1",
                "u0",
                r#"{"type":"text","text":"here is the plan"}"#,
                "",
            ),
            // Carries it.
            block(
                1,
                "a2",
                "a1",
                r#"{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}"#,
                carries,
            ),
            // Repeats the same value; must not be charged twice.
            block(2, "a3", "a2", r#"{"type":"text","text":"done"}"#, carries),
        ]
        .join("\n")
            + "\n";

        let tmp = tempfile::tempdir().unwrap();
        let out = parse_body(tmp.path(), "sess-1.jsonl", &body);

        let charged: Vec<u64> = out.docs.iter().filter_map(|d| d.thinking_tokens).collect();
        assert_eq!(charged, [300], "one message, one charge, on the carrier");
    }

    /// The cap exists to bound a pathological `cat`, not to cut real output. The old 32 KiB
    /// ceiling was below the size of the spilled results the indexer deliberately goes and
    /// fetches — the README's own example is 54.5 KB — so the one path that reaches for large
    /// output was also the one guaranteed to lose it.
    #[test]
    fn a_result_larger_than_the_old_cap_survives_whole() {
        let dir = tempfile::tempdir().unwrap();
        let big = "quokkatron ".repeat(6_000); // 66 KB: past 32 KiB, a plausible build log
        let body = format!(
            concat!(
                r#"{{"type":"assistant","uuid":"a1","sessionId":"s","cwd":"/p","message":{{"role":"assistant","id":"m1","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"cargo build"}}}}]}}}}"#,
                "\n",
                r#"{{"type":"user","uuid":"u1","sessionId":"s","cwd":"/p","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}}}}"#,
                "\n",
            ),
            big = big
        );
        let out = parse_body(dir.path(), "big.jsonl", &body);
        let call = out
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("t1"))
            .expect("a tool call");
        assert_eq!(
            call.tool_output.as_deref().map(str::len),
            Some(big.len()),
            "the result is indexed whole, not clipped at the old 32 KiB"
        );
    }

    /// Raising `max_text_bytes` must not drag the `text` copy of the input up with it: the
    /// input is already indexed whole in `tool_input`, so a huge `Write` payload would be
    /// duplicated for nothing — and would push the document past `PENDING_DOC_CAP`, silently
    /// costing it the ability to be completed across an incremental boundary.
    #[test]
    fn the_input_copy_stays_bounded_however_high_the_cap_goes() {
        let dir = tempfile::tempdir().unwrap();
        let payload = "z".repeat(300_000);
        let body = format!(
            concat!(
                r#"{{"type":"assistant","uuid":"a1","sessionId":"s","cwd":"/p","message":{{"role":"assistant","id":"m1","content":[{{"type":"tool_use","id":"t1","name":"Write","input":{{"content":"{payload}"}}}}]}}}}"#,
                "\n",
            ),
            payload = payload
        );
        std::fs::write(dir.path().join("w.jsonl"), &body).unwrap();
        let opts = ParseOptions {
            max_text_bytes: 8 * 1024 * 1024,
            ..ParseOptions::default()
        };
        let out = parse_whole(&dir.path().join("w.jsonl"), &opts).unwrap();
        let call = out
            .docs
            .iter()
            .find(|d| d.kind == DocKind::ToolCall)
            .unwrap();
        assert!(
            text_of(call).len() + code_of(call).len() <= INPUT_LEAVES_CAP + 16,
            "input copy grew to {} bytes",
            text_of(call).len() + code_of(call).len()
        );
        // ...and nothing was actually lost: `tool_input` still carries the whole payload.
        assert_eq!(
            call.tool_input.as_ref().unwrap()["content"]
                .as_str()
                .unwrap()
                .len(),
            payload.len()
        );
    }

    /// A failed call's error must be reachable without digging past the command that failed.
    /// `--errors-only` otherwise retrieves exactly the right documents and then shows the
    /// command: on a real corpus the error text sat 1,000-2,400 characters in, past a heredoc,
    /// so every preview was the thing that ran rather than the reason it broke.
    ///
    /// The body used to be reordered to fix that. Now the two are separate fields, so the
    /// error is at offset 0 of its own — and `format::a_failed_tool_call_previews_its_error`
    /// pins the rendering half, which is what the reorder was really protecting.
    #[test]
    fn a_failed_tool_call_keeps_its_error_out_of_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let long_input = "x".repeat(600);
        let body = format!(
            concat!(
                r#"{{"type":"assistant","uuid":"a1","sessionId":"s","cwd":"/p","message":{{"role":"assistant","id":"m1","content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"cat > f <<'PY'\n{long_input}"}}}}]}}}}"#,
                "\n",
                r#"{{"type":"user","uuid":"u1","sessionId":"s","cwd":"/p","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"InputValidationError: JSON parse failed"}}]}},"toolUseResult":"InputValidationError: JSON parse failed"}}"#,
                "\n",
            ),
            long_input = long_input
        );
        let out = parse_body(dir.path(), "e.jsonl", &body);

        let doc = out
            .docs
            .iter()
            .find(|d| d.kind == DocKind::ToolCall)
            .expect("a tool call");
        assert!(doc.is_error);
        let output = doc.tool_output.as_deref().expect("error indexed");
        assert!(
            output.starts_with("InputValidationError"),
            "the error is the whole of its field, not buried in one: {output:?}"
        );
        assert!(
            text_of(doc).contains("cat > f"),
            "the command is still indexed, on the call side"
        );
        assert!(
            !text_of(doc).contains("InputValidationError"),
            "and the error is not also duplicated into it"
        );
    }

    /// The reorder is only safe if the incremental path rebuilds the body rather than appending
    /// to it — a failed call needs its result inserted *before* the input, which no append can
    /// do. This is the byte-identity invariant, for the error case specifically.
    #[test]
    fn a_failed_call_completed_across_a_boundary_matches_a_whole_file_parse() {
        let dir = tempfile::tempdir().unwrap();
        let head = concat!(
            r#"{"type":"assistant","uuid":"a1","sessionId":"s","cwd":"/p","message":{"role":"assistant","id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"set -e\nbroken"}}]}}"#,
            "\n",
        );
        let tail = concat!(
            r#"{"type":"user","uuid":"u1","sessionId":"s","cwd":"/p","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Error: exit 1"}]},"toolUseResult":"Error: exit 1"}"#,
            "\n",
        );
        let path = dir.path().join("split-err.jsonl");

        // Split parse: the tool_use in one run, its failure in the next.
        std::fs::write(&path, head).unwrap();
        let (first, offset) = parse_file(
            &path,
            0,
            0,
            &ParseOptions::default(),
            &FileContext::default(),
        )
        .unwrap();
        std::fs::write(&path, format!("{head}{tail}")).unwrap();
        let ctx = FileContext {
            carry: first.carry.clone(),
            ..FileContext::default()
        };
        let (second, _) = parse_file(
            &path,
            offset,
            first.docs.len() as u64,
            &ParseOptions::default(),
            &ctx,
        )
        .unwrap();

        let completed = second
            .replacements
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("t1"))
            .expect("the pending document is completed, not duplicated");

        // Whole-file parse of the same bytes.
        let whole = parse_body(dir.path(), "whole-err.jsonl", &format!("{head}{tail}"));
        let expected = whole
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("t1"))
            .expect("a tool call");

        assert_eq!(completed.text, expected.text, "body must be byte-identical");
        assert_eq!(
            completed.tool_output, expected.tool_output,
            "and so must the result"
        );
        assert_eq!(completed.is_error, expected.is_error);
        assert_eq!(
            completed.tool_output.as_deref(),
            Some("Error: exit 1"),
            "the error is the result field, whichever side of the boundary it arrived on"
        );
    }

    // -- file-level hazards -------------------------------------------------

    #[test]
    fn leading_nul_bytes_are_stripped() {
        let (out, _) = parse("nul_and_partial.jsonl");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(
            texts(&out).iter().any(|t| t == "padded line answer"),
            "the NUL-padded line must be parsed: {:?}",
            texts(&out)
        );
    }

    #[test]
    fn a_partial_final_line_is_left_unconsumed() {
        let path = fixture("nul_and_partial.jsonl");
        let len = std::fs::metadata(&path).unwrap().len();
        let (out, offset) = parse("nul_and_partial.jsonl");
        assert!(offset < len, "offset {offset} must stop before EOF {len}");
        assert!(out.errors.is_empty(), "a partial line is not an error");
        assert!(!texts(&out).iter().any(|t| t.contains("truncated pro")));

        // Once the writer finishes the line, resuming from the offset picks it up.
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live.jsonl");
        std::fs::copy(&path, &live).unwrap();
        let (out, offset2) = parse_file(
            &live,
            0,
            0,
            &ParseOptions::default(),
            &FileContext::default(),
        )
        .unwrap();
        let seq_base = out.docs.len() as u64;
        std::fs::write(
            &live,
            [
                std::fs::read(&path).unwrap(),
                b"mpt\"},\"cwd\":\"/home/user/session-search\"}\n".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        let (tail, offset3) = parse_file(
            &live,
            offset2,
            seq_base,
            &ParseOptions::default(),
            &FileContext::default(),
        )
        .unwrap();
        assert_eq!(tail.docs.len(), 1);
        assert_eq!(tail.docs[0].text, ["truncated prompt"]);
        assert_eq!(tail.docs[0].seq, seq_base);
        assert_eq!(offset3, std::fs::metadata(&live).unwrap().len());
    }

    #[test]
    fn a_complete_file_is_fully_consumed() {
        let path = fixture("assistant_split_blocks.jsonl");
        let (_, offset) = parse("assistant_split_blocks.jsonl");
        assert_eq!(offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn malformed_lines_are_counted_not_fatal() {
        let (out, _) = parse("garbage.jsonl");
        // Two syntactically broken lines; `3` and `[1,2,3]` are valid JSON and become Unknown.
        assert_eq!(out.errors.len(), 2, "{:?}", out.errors);
        assert_eq!(texts(&out), vec!["good line", "still fine"]);
        assert!(out.errors[0].message.contains("expected"));
        assert!(out.errors[0].byte_offset > 0);
    }

    #[test]
    fn sidecar_records_without_uuid_do_not_crash() {
        let (out, _) = parse("sidecars_no_uuid.jsonl");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        // The queue-operation duplicates the user prompt; it must not be indexed twice.
        assert_eq!(
            texts(&out).iter().filter(|t| **t == "first prompt").count(),
            1
        );
    }

    #[test]
    fn duplicated_sidecars_after_resume_are_last_wins() {
        let (out, _) = parse("sidecars_no_uuid.jsonl");
        assert_eq!(
            out.session.title.as_deref(),
            Some("Fresh title after the resume")
        );
        assert_eq!(out.session.first_prompt.as_deref(), Some("first prompt"));
    }

    #[test]
    fn dangling_parent_uuids_are_kept_verbatim() {
        let (out, _) = parse("dangling_parent.jsonl");
        assert!(out.errors.is_empty());
        assert_eq!(out.docs.len(), 2);
        assert_eq!(
            out.docs[0].parent_uuid.as_deref(),
            Some("uuid-that-was-never-written")
        );
        assert_eq!(out.docs[1].parent_uuid.as_deref(), Some("another-phantom"));
    }

    // -- record semantics ---------------------------------------------------

    #[test]
    fn assistant_blocks_share_a_message_id_and_are_counted_once() {
        let (out, _) = parse("assistant_split_blocks.jsonl");
        assert!(out.errors.is_empty());
        // one human turn + one API message (three block records) = 2 messages
        assert_eq!(out.session.messages, 2);
        assert_eq!(out.session.tool_calls, 2);
        assert_eq!(out.docs.len(), 5);
        assert_eq!(
            out.docs.iter().map(|d| d.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert!(
            out.docs
                .iter()
                .all(|d| d.doc_id.starts_with("sess-fixture:-:"))
        );
    }

    #[test]
    fn thinking_lands_in_its_own_field_never_in_text() {
        let (out, _) = parse("assistant_split_blocks.jsonl");
        let thinking = out
            .docs
            .iter()
            .find(|d| d.thinking.is_some())
            .expect("a thinking doc");
        assert!(thinking.text.is_empty());
        assert!(
            thinking
                .thinking
                .as_deref()
                .unwrap()
                .contains("secret reasoning")
        );
        assert!(
            !out.docs
                .iter()
                .any(|d| text_of(d).contains("secret reasoning")),
            "thinking must never leak into the indexed text"
        );
    }

    #[test]
    fn tool_use_is_joined_to_its_tool_result() {
        let (out, _) = parse("assistant_split_blocks.jsonl");
        let bash = out
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("toolu_1"))
            .unwrap();
        assert_eq!(bash.kind, DocKind::ToolCall);
        assert_eq!(bash.tool_name.as_deref(), Some("Bash"));
        assert_eq!(bash.tool_input.as_ref().unwrap()["command"], "cargo build");
        assert!(
            text_of(bash).contains("cargo build"),
            "input is indexed as text"
        );
        assert!(
            bash.tool_output
                .as_deref()
                .unwrap()
                .contains("Compiling session-search"),
            "result is indexed, in its own field"
        );
        assert!(
            !text_of(bash).contains("Compiling session-search"),
            "and not also smuggled into `text`"
        );
        assert!(
            !code_of(bash).contains("Compiling session-search"),
            "...nor into `code`: {:?}",
            bash.code
        );
        assert!(!bash.is_error);

        let read = out
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("toolu_2"))
            .unwrap();
        assert!(read.is_error);
        assert!(
            read.tool_output
                .as_deref()
                .unwrap()
                .contains("Error: file not found")
        );
        assert_eq!(
            read.tool_input.as_ref().unwrap()["file_path"],
            "/home/user/session-search/src/index.rs"
        );
    }

    // -- the markdown split -------------------------------------------------

    /// A message is markdown, and its two halves go to the two fields that suit them.
    #[test]
    fn a_markdown_message_is_split_into_prose_code_and_headings() {
        let dir = tempfile::tempdir().unwrap();
        let body = "## The fix\n\nThe indexer had already compiled it, so `open_or_create`                     reused the schema:\n\n```rust\npub fn open_or_create(dir: &Path) {}\n```";
        let line = serde_json::json!({
            "type": "assistant", "uuid": "a1", "sessionId": "s", "cwd": "/p",
            "message": {"role": "assistant", "id": "m1", "model": "mm",
                        "content": [{"type": "text", "text": body}]},
        });
        let out = parse_body(dir.path(), "md.jsonl", &format!("{line}\n"));
        let doc = &out.docs[0];

        assert_eq!(doc.headings, ["The fix"]);
        assert_eq!(doc.code_langs, ["rust"]);
        // The fence is one entry, the inline span another; both stay out of the prose.
        assert_eq!(doc.code.len(), 2, "{:?}", doc.code);
        assert!(doc.code.iter().any(|c| c.contains("pub fn open_or_create")));
        assert!(doc.code.contains(&"open_or_create".to_string()));
        assert!(!text_of(doc).contains("pub fn"), "{:?}", doc.text);
        // The prose keeps the sentence — and the heading, which is prose as well as a heading.
        assert!(doc.text.first().unwrap() == "The fix", "{:?}", doc.text);
        assert!(
            text_of(doc).contains("had already compiled it"),
            "{:?}",
            doc.text
        );
        assert!(doc.raw.contains("```rust"), "the raw line is untouched");
    }

    /// The same for a user turn, and a turn with no markdown in it at all is unchanged.
    #[test]
    fn a_user_turn_is_split_too_and_plain_prose_survives_intact() {
        let dir = tempfile::tempdir().unwrap();
        let line = |text: &str| {
            serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "s", "cwd": "/p",
                "message": {"role": "user", "content": text},
            })
            .to_string()
        };
        let out = parse_body(
            dir.path(),
            "u.jsonl",
            &format!("{}\n", line("run this:\n\n```bash\ncargo test\n```")),
        );
        assert_eq!(out.docs[0].text, ["run this:"]);
        assert_eq!(out.docs[0].code, ["cargo test"]);
        assert_eq!(out.docs[0].code_langs, ["bash"]);

        let plain = "please make the tantivy schema faster";
        let out = parse_body(dir.path(), "p.jsonl", &format!("{}\n", line(plain)));
        assert_eq!(out.docs[0].text, [plain]);
        assert!(out.docs[0].code.is_empty());
    }

    /// A tool call is not markdown and must never be parsed as one: a `Bash` script is full of
    /// `#`, `*` and `>` that mean nothing of the sort.
    #[test]
    fn a_tool_call_is_not_parsed_as_markdown_and_its_output_is_its_own_field() {
        let dir = tempfile::tempdir().unwrap();
        let script = "# rebuild\n*.rs > /tmp/list";
        let body = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "s", "cwd": "/p",
                "message": {"role": "assistant", "id": "m1", "model": "mm", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash",
                     "input": {"command": script, "description": "rebuild the list"}}]},
            }),
            serde_json::json!({
                "type": "user", "uuid": "u1", "sessionId": "s", "cwd": "/p",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "# 3 files"}]},
            }),
        );
        let out = parse_body(dir.path(), "bash.jsonl", &body);
        let call = out
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("t1"))
            .unwrap();

        assert!(call.headings.is_empty(), "`# rebuild` is not a heading");
        assert!(call.code_langs.is_empty());
        // The name and the input strings are the text, verbatim, exactly as before.
        assert!(text_of(call).starts_with("Bash\n"), "{:?}", call.text);
        assert!(text_of(call).contains("# rebuild"), "{:?}", call.text);
        assert!(
            text_of(call).contains("*.rs > /tmp/list"),
            "{:?}",
            call.text
        );
        // The output is `tool_output`, and only that: `# 3 files` is not a heading either.
        assert_eq!(call.tool_output.as_deref(), Some("# 3 files"));
        assert!(call.code.is_empty(), "{:?}", call.code);
    }

    /// An `Edit` payload is file content, not a parameter someone typed, so it is code.
    #[test]
    fn edit_and_write_payloads_are_code_and_the_rest_of_the_input_is_text() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant", "uuid": "a1", "sessionId": "s", "cwd": "/p",
                "message": {"role": "assistant", "id": "m1", "model": "mm", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Edit", "input": {
                        "file_path": "/home/user/session-search/src/index.rs",
                        "old_string": "fn openOrCreate(dir: &Path)",
                        "new_string": "fn open_or_create(dir: &Path)"}},
                    {"type": "tool_use", "id": "t2", "name": "MultiEdit", "input": {
                        "file_path": "/tmp/a.rs",
                        "edits": [{"old_string": "let mut x", "new_string": "let x"}]}}]},
            }),
        );
        let out = parse_body(dir.path(), "edit.jsonl", &body);
        let call = |id: &str| {
            out.docs
                .iter()
                .find(|d| d.tool_use_id.as_deref() == Some(id))
                .unwrap()
        };

        let edit = call("t1");
        assert!(text_of(edit).contains("/home/user/session-search/src/index.rs"));
        assert!(!text_of(edit).contains("openOrCreate"), "{:?}", edit.text);
        // Leaf order follows the JSON object's key order, which `serde_json` sorts.
        assert_eq!(
            edit.code,
            ["fn open_or_create(dir: &Path)\nfn openOrCreate(dir: &Path)"]
        );
        // A `MultiEdit` nests its payloads one level deeper; the key still decides.
        let multi = call("t2");
        assert!(text_of(multi).contains("/tmp/a.rs"));
        assert_eq!(multi.code, ["let x\nlet mut x"]);
    }

    /// The three parts of a tool call must not bleed into one another: a word that occurs only
    /// in the input must not be findable through `tool_output`, and vice versa.
    #[test]
    fn the_call_and_its_result_stay_in_separate_fields() {
        let (out, _) = parse("assistant_split_blocks.jsonl");
        let bash = out
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("toolu_1"))
            .unwrap();
        let output = bash.tool_output.as_deref().unwrap();
        assert!(
            text_of(bash).contains("Bash"),
            "the name is on the call side"
        );
        assert!(!output.contains("cargo build"), "input leaked into output");
        assert!(
            !text_of(bash).contains("Compiling"),
            "output leaked into text"
        );
        assert!(
            !code_of(bash).contains("Compiling"),
            "output leaked into code"
        );

        // A message carries no output at all — not even an empty string, which would sit in
        // the index as a term nobody can search for.
        assert!(
            out.docs
                .iter()
                .filter(|d| d.kind == DocKind::Message)
                .all(|d| d.tool_output.is_none())
        );
    }

    /// `bash_cmd` is filled for `Bash` and only for `Bash`, from `tool_input.command`.
    #[test]
    fn bash_tool_calls_carry_a_parsed_command() {
        let (out, _) = parse("bash_commands.jsonl");
        let by_id = |id: &str| {
            out.docs
                .iter()
                .find(|d| d.tool_use_id.as_deref() == Some(id))
                .unwrap()
        };

        // Every simple command in the script contributes its program, pipeline included.
        assert_eq!(
            by_id("toolu_1").bash_cmd,
            Some(serde_json::json!({
                "program": ["cargo", "tail"],
                "args": ["build", "--release", "-20"],
            })),
        );
        // `&&` chain; the redirect target `2>&1` above and here contribute no args.
        assert_eq!(
            by_id("toolu_2").bash_cmd,
            Some(serde_json::json!({
                "program": ["cd", "git"],
                "args": ["/tmp/x", "status", "--short"],
            })),
        );
        // Another tool's input is never parsed as a shell command...
        assert_eq!(by_id("toolu_3").tool_name.as_deref(), Some("Read"));
        assert_eq!(by_id("toolu_3").bash_cmd, None);
        // ...and a command the grammar rejects gets nothing rather than a guess.
        assert_eq!(by_id("toolu_4").tool_name.as_deref(), Some("Bash"));
        assert_eq!(by_id("toolu_4").bash_cmd, None);
        // Messages are not tool calls.
        assert!(
            out.docs
                .iter()
                .all(|d| d.kind == DocKind::ToolCall || d.bash_cmd.is_none())
        );
    }

    #[test]
    fn user_content_string_and_array_both_index() {
        let (out, _) = parse("union_types.jsonl");
        assert!(out.errors.is_empty());
        let t = texts(&out);
        assert!(t.iter().any(|t| t == "a string prompt"));
        assert!(t.iter().any(|t| t == "an array prompt"));
        assert!(
            t.iter().any(|s| s.contains("weird")),
            "non-union content is still kept"
        );
    }

    #[test]
    fn tool_use_result_string_marks_an_error() {
        let (out, _) = parse("union_types.jsonl");
        let ls = out
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("toolu_s"))
            .unwrap();
        assert!(
            ls.is_error,
            "a bare-string toolUseResult starting `Error:` is a failure"
        );
        assert!(ls.tool_output.as_deref().unwrap().contains("total 24"));
    }

    #[test]
    fn tool_result_content_array_is_flattened() {
        let (out, _) = parse("union_types.jsonl");
        let agent = out
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("toolu_b"))
            .unwrap();
        assert!(
            agent
                .tool_output
                .as_deref()
                .unwrap()
                .contains("agent said hello")
        );
        assert!(!agent.is_error);
    }

    #[test]
    fn noise_attachments_are_skipped_and_useful_ones_kept() {
        let (out, _) = parse("attachments.jsonl");
        assert!(out.errors.is_empty());
        let t = texts(&out);
        assert!(!t.iter().any(|s| s.contains("tokens left")), "noise: {t:?}");
        assert!(
            !t.iter().any(|s| s.contains("prompt_snapshot")),
            "boilerplate: {t:?}"
        );
        assert!(
            t.iter().any(|s| s.contains("please index the transcripts")),
            "queued_command is real user text: {t:?}"
        );
        assert!(t.iter().any(|s| s.contains("Primary working directory")));
        assert!(
            out.docs
                .iter()
                .all(|d| d.role == "attachment" || d.role == "system")
        );
    }

    #[test]
    fn compact_boundary_is_meta() {
        let (out, _) = parse("attachments.jsonl");
        let sys = out.docs.iter().find(|d| d.role == "system").unwrap();
        assert!(sys.is_meta);
        assert!(text_of(sys).contains("compact_boundary"));
    }

    #[test]
    fn text_is_capped_at_max_text_bytes() {
        let opts = ParseOptions {
            max_text_bytes: 16,
            load_spilled_results: false,
        };
        let out = parse_whole(&fixture("union_types.jsonl"), &opts).unwrap();
        assert!(
            out.docs
                .iter()
                .all(|d| d.text.iter().all(|t| t.len() <= 16)
                    && d.code.iter().all(|c| c.len() <= 16)),
            "cap not applied"
        );
    }

    #[test]
    fn a_spilled_tool_result_is_pulled_back_in_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("bjetw9qfe.txt");
        std::fs::write(&spill, "the whole oversized output, phrase: quokkatron\n").unwrap();

        let transcript = dir.path().join("s.jsonl");
        let stub = format!(
            "<persisted-output>\nOutput too large (54.5KB). Full output saved to: {}\n",
            spill.display()
        );
        let lines = [
            serde_json::json!({
                "type": "assistant", "uuid": "u1", "parentUuid": null,
                "timestamp": "2026-09-09T19:00:00.000Z", "sessionId": "sess-spill",
                "cwd": "/home/user/session-search",
                "message": {"role": "assistant", "id": "msg_1", "model": "claude-opus-5",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                                 "input": {"command": "ls"}}]},
            }),
            serde_json::json!({
                "type": "user", "uuid": "u2", "parentUuid": "u1",
                "timestamp": "2026-09-09T19:00:01.000Z", "sessionId": "sess-spill",
                "cwd": "/home/user/session-search",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": stub}]},
            }),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&transcript, body).unwrap();

        let off = ParseOptions {
            load_spilled_results: false,
            ..ParseOptions::default()
        };
        let out = parse_whole(&transcript, &off).unwrap();
        assert!(
            !out.docs.iter().any(|d| body_of(d).contains("quokkatron")),
            "the spill must stay out unless asked for"
        );

        let on = ParseOptions {
            load_spilled_results: true,
            ..ParseOptions::default()
        };
        let out = parse_whole(&transcript, &on).unwrap();
        assert!(
            out.docs.iter().any(|d| body_of(d).contains("quokkatron")),
            "spill not folded in: {:?}",
            out.docs.iter().map(body_of).collect::<Vec<_>>()
        );

        // And it obeys the same body cap as anything else — `tool_output` has its own budget,
        // so the cap is checked per field rather than on the sum of all of them, while the
        // two halves of the split still share one.
        let capped = ParseOptions {
            load_spilled_results: true,
            max_text_bytes: 24,
        };
        let out = parse_whole(&transcript, &capped).unwrap();
        assert!(
            out.docs
                .iter()
                .all(|d| text_of(d).len() + code_of(d).len() <= 24),
            "{:?}",
            out.docs
                .iter()
                .map(|d| (&d.text, &d.code))
                .collect::<Vec<_>>()
        );
        assert!(
            out.docs
                .iter()
                .all(|d| d.tool_output.as_deref().map_or(0, str::len) <= 24)
        );
    }

    // -- session metadata ---------------------------------------------------

    #[test]
    fn project_comes_from_cwd_not_the_directory_name() {
        let (out, _) = parse("assistant_split_blocks.jsonl");
        assert_eq!(
            out.session.project.as_deref(),
            Some("/home/user/session-search")
        );
        assert!(
            out.docs
                .iter()
                .all(|d| d.project.as_deref() == Some("/home/user/session-search"))
        );
        assert_eq!(out.session.git_branch.as_deref(), Some("main"));
        assert!(out.session.first_ts_ms.unwrap() <= out.session.last_ts_ms.unwrap());
    }

    #[test]
    fn seq_base_continues_numbering() {
        let (out, _) = parse_file(
            &fixture("dangling_parent.jsonl"),
            0,
            100,
            &ParseOptions::default(),
            &FileContext::default(),
        )
        .unwrap();
        assert_eq!(
            out.docs.iter().map(|d| d.seq).collect::<Vec<_>>(),
            vec![100, 101]
        );
        assert!(out.docs[0].doc_id.starts_with("sess-fixture:-:"));
        assert!(out.docs[0].doc_id.ends_with(":100"));
    }

    // -- real, redacted transcripts ----------------------------------------

    #[test]
    fn real_main_transcript_slice_parses_cleanly() {
        let (out, offset) = parse("real_main_slice.jsonl");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(
            offset,
            std::fs::metadata(fixture("real_main_slice.jsonl"))
                .unwrap()
                .len()
        );
        assert!(out.docs.len() > 5, "{} docs", out.docs.len());
        assert_eq!(
            out.session.project.as_deref(),
            Some("/home/user/session-search")
        );
        assert!(out.session.first_prompt.is_some());
        assert!(out.docs.iter().any(|d| d.kind == DocKind::ToolCall));
        // NB: in transcripts written by this CLI build the `thinking` string is persisted
        // empty (only the signature survives), so a thinking doc is not expected here — the
        // synthetic fixture covers the populated case.
        assert!(
            out.docs
                .iter()
                .any(|d| d.role == "assistant" && !d.text.is_empty())
        );
        assert!(out.docs.iter().all(|d| !d.is_sidechain));
        // `slug` appears on only some records and shows up later in the file, so it is not
        // required here — `version` and `entrypoint` are on every DAG record.
        assert!(
            out.docs
                .iter()
                .all(|d| d.version.as_deref() == Some("2.1.266"))
        );
        assert!(out.docs.iter().any(|d| d.entrypoint.is_some()));
    }

    #[test]
    fn first_prompt_falls_back_when_no_record_is_a_human_turn() {
        // A subagent's opening turn carries no `origin`, so the CLI predicate rejects it.
        let (out, _) = parse("real_sidechain_slice.jsonl");
        assert!(
            out.session.first_prompt.is_some(),
            "every session needs an opening prompt for the session list"
        );
    }

    #[test]
    fn real_sidechain_slice_carries_agent_identity() {
        let (out, _) = parse("real_sidechain_slice.jsonl");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(!out.docs.is_empty());
        // sessionId inside a sidechain file is the PARENT session's id.
        assert!(out.docs.iter().all(|d| d.is_sidechain));
        assert!(out.docs.iter().all(|d| d.agent_id.is_some()));
        assert_eq!(out.session.agent_type.as_deref(), Some("Explore"));
    }

    #[test]
    fn sidechain_ids_fall_back_to_the_path() {
        let ids = ids_from_path(Path::new(
            "/r/projects/-p/sess-9/subagents/workflows/wf/agent-abc123.jsonl",
        ));
        assert_eq!(ids.session_id, "sess-9");
        assert_eq!(ids.agent_id.as_deref(), Some("abc123"));

        let ids = ids_from_path(Path::new("/r/projects/-p/sess-9.jsonl"));
        assert_eq!(ids.session_id, "sess-9");
        assert!(ids.agent_id.is_none());
    }

    // -- regressions --------------------------------------------------------

    /// An API message is counted by whichever block record first *emits*. Every `thinking`
    /// block this machine's remote sessions write is `"thinking": ""`, so a message whose
    /// block 0 is one of those used to burn its `message.id` and never be counted at all.
    #[test]
    fn a_message_is_counted_even_when_its_first_block_record_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let record = |i: usize, block: &str| {
            format!(
                r#"{{"type":"assistant","uuid":"a{i}","sessionId":"s","cwd":"/p","apiBlockIndex":{i},"message":{{"role":"assistant","id":"msg_1","model":"mm","content":[{block}]}}}}"#
            ) + "\n"
        };

        let empty_thinking_first =
            record(0, r#"{"type":"thinking","thinking":"","signature":"g"}"#)
                + &record(1, r#"{"type":"text","text":"the visible answer"}"#);
        let out = parse_body(dir.path(), "thinking-first.jsonl", &empty_thinking_first);
        assert_eq!(out.session.messages, 1, "{:?}", texts(&out));

        let tool_use_first = record(
            0,
            r#"{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}"#,
        ) + &record(1, r#"{"type":"text","text":"and here is what i found"}"#);
        let out = parse_body(dir.path(), "tool-first.jsonl", &tool_use_first);
        assert_eq!(out.session.messages, 1, "{:?}", texts(&out));
        assert_eq!(out.session.tool_calls, 1);

        // …and a message really is counted only once across its sibling records.
        let two_texts = record(0, r#"{"type":"text","text":"part one"}"#)
            + &record(1, r#"{"type":"text","text":"part two"}"#);
        let out = parse_body(dir.path(), "two-texts.jsonl", &two_texts);
        assert_eq!(out.session.messages, 1);
        assert_eq!(out.docs.len(), 2);
    }

    /// TRANSCRIPT-FORMAT §8: `isSidechain: true` records can appear inline in a main
    /// transcript. One of them must not relabel everything that follows it.
    #[test]
    fn an_inline_sidechain_record_does_not_relabel_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"user","uuid":"u1","sessionId":"sess-main","cwd":"/p","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"main turn one"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","sessionId":"sess-main","cwd":"/p","isSidechain":true,"agentId":"aDEADBEEF","attributionAgent":"Explore","message":{"role":"assistant","id":"m1","model":"mm","content":[{"type":"text","text":"sidechain says hi"}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","sessionId":"sess-main","cwd":"/p","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"main turn two"}}"#,
            "\n",
        );
        let out = parse_body(dir.path(), "sess-main.jsonl", body);

        let inline = out
            .docs
            .iter()
            .find(|d| text_of(d).contains("sidechain says hi"))
            .unwrap();
        assert!(inline.is_sidechain, "the record itself said so");
        assert_eq!(inline.agent_id.as_deref(), Some("aDEADBEEF"));

        for text in ["main turn one", "main turn two"] {
            let doc = out.docs.iter().find(|d| d.text == [text]).unwrap();
            assert!(!doc.is_sidechain, "{text}: {doc:?}");
            assert!(doc.agent_id.is_none(), "{text}: {doc:?}");
            assert!(doc.agent_type.is_none(), "{text}: {doc:?}");
            assert!(doc.doc_id.starts_with("sess-main:-:"), "{}", doc.doc_id);
        }
        // …and the session itself is still the main one, not the stray agent's.
        assert!(out.session.agent_id.is_none());
    }

    /// §9: "Exclude both from human-prompt detection **and from message counts**."
    #[test]
    fn compaction_records_are_indexed_but_not_counted_as_messages() {
        let dir = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"user","uuid":"u0","sessionId":"s","cwd":"/p","origin":{"kind":"human"},"message":{"role":"user","content":"real human turn"}}"#,
            "\n",
            r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","level":"info","uuid":"sys1","timestamp":"2026-09-09T19:00:00.000Z","sessionId":"s","cwd":"/p","compactMetadata":{"trigger":"auto"}}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","sessionId":"s","cwd":"/p","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"origin":{"kind":"human"},"message":{"role":"user","content":"THE SUMMARY OF EVERYTHING"}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","sessionId":"s","cwd":"/p","origin":{"kind":"human"},"message":{"role":"user","content":"second human turn"}}"#,
            "\n",
        );
        let out = parse_body(dir.path(), "compaction.jsonl", body);

        assert_eq!(out.docs.len(), 4, "all four are still searchable");
        assert_eq!(out.session.messages, 2, "only the two human turns count");
        assert!(
            out.docs
                .iter()
                .find(|d| text_of(d).contains("THE SUMMARY"))
                .unwrap()
                .is_meta
        );
        assert!(
            out.docs
                .iter()
                .find(|d| d.role == "system")
                .unwrap()
                .is_meta
        );
        assert_eq!(out.session.first_prompt.as_deref(), Some("real human turn"));
    }

    /// §5: `toolUseResult` "is a bare string on every failure path", and those turns often have
    /// no `tool_result` block to hang the error flag on.
    #[test]
    fn a_failure_with_no_tool_result_block_still_marks_the_call_failed() {
        let dir = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"assistant","uuid":"a1","sessionId":"s","cwd":"/p","message":{"role":"assistant","id":"m1","model":"mm","content":[{"type":"tool_use","id":"t1","name":"Nope","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","sessionId":"s","cwd":"/p","sourceToolAssistantUUID":"a1","message":{"role":"user","content":"No such tool available: Nope"},"toolUseResult":"Error: No such tool available: Nope"}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a2","sessionId":"s","cwd":"/p","message":{"role":"assistant","id":"m2","model":"mm","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"sleep 100"}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","sessionId":"s","cwd":"/p","toolDenialKind":"interrupted","toolUseResult":"Conversation ended by model","message":{"role":"user","content":[]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a3","sessionId":"s","cwd":"/p","message":{"role":"assistant","id":"m3","model":"mm","content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"true"}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"u3","sessionId":"s","cwd":"/p","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t3","content":"fine"}]},"toolUseResult":{"stdout":"fine","interrupted":false}}"#,
            "\n",
        );
        let out = parse_body(dir.path(), "bare-errors.jsonl", body);
        let call = |id: &str| {
            out.docs
                .iter()
                .find(|d| d.tool_use_id.as_deref() == Some(id))
                .unwrap_or_else(|| panic!("no document for {id}"))
        };

        assert!(
            call("t1").is_error,
            "bare `Error:` string: {:?}",
            call("t1")
        );
        assert!(
            call("t1")
                .tool_output
                .as_deref()
                .unwrap()
                .contains("No such tool available")
        );
        assert!(call("t2").is_error, "toolDenialKind: {:?}", call("t2"));
        assert!(!call("t3").is_error, "a success must stay a success");
        // And nothing was invented: three tool calls in, three documents out.
        assert_eq!(
            out.docs
                .iter()
                .filter(|d| d.kind == DocKind::ToolCall)
                .count(),
            3
        );
    }

    /// Two transcripts can legitimately carry the same `sessionId` (§9). `seq` restarts in
    /// each, so without the file tag their documents would share a `doc_id`.
    #[test]
    fn doc_ids_are_unique_across_two_files_sharing_a_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let line = |uuid: &str, text: &str| {
            format!(
                r#"{{"type":"user","uuid":"{uuid}","sessionId":"sess-1","cwd":"/p","message":{{"role":"user","content":"{text}"}}}}"#
            ) + "\n"
        };
        let a = parse_body(dir.path(), "one/sess-1.jsonl", &line("u1", "file one"));
        let b = parse_body(dir.path(), "two/sess-1.jsonl", &line("u2", "file two"));
        assert_ne!(a.docs[0].doc_id, b.docs[0].doc_id, "{}", a.docs[0].doc_id);
        assert!(a.docs[0].doc_id.starts_with("sess-1:-:"));
        assert!(a.docs[0].doc_id.ends_with(":0"));
    }

    /// The canonical source for a subagent's type is `agent-<id>.meta.json` (§1), not the
    /// `attributionAgent` that only some writers put on assistant records.
    #[test]
    fn the_agent_type_from_the_meta_json_reaches_every_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess-1/subagents/agent-abc123.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","uuid":"s1","sessionId":"sess-1","cwd":"/p","isSidechain":true,"message":{"role":"user","content":"explore"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let ctx = FileContext {
            agent_type: Some("Explore".to_string()),
            ..FileContext::default()
        };
        let (out, _) = parse_file(&path, 0, 0, &ParseOptions::default(), &ctx).unwrap();
        assert_eq!(out.session.agent_type.as_deref(), Some("Explore"));
        assert!(
            out.docs
                .iter()
                .all(|d| d.agent_type.as_deref() == Some("Explore")),
            "{:?}",
            out.docs.iter().map(|d| &d.agent_type).collect::<Vec<_>>()
        );
        assert!(out.docs.iter().all(|d| d.is_sidechain));
    }

    /// Sidecars dedupe last-wins *in file order* (§2) — not by the lexicographic order of the
    /// `leafUuid` a `BTreeMap` happens to iterate in.
    #[test]
    fn the_newest_last_prompt_sidecar_wins() {
        let dir = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"last-prompt","lastPrompt":"the older prompt","leafUuid":"aaa","sessionId":"s"}"#,
            "\n",
            r#"{"type":"last-prompt","lastPrompt":"the newest prompt","leafUuid":"zzz","sessionId":"s"}"#,
            "\n",
            r#"{"type":"last-prompt","lastPrompt":"actually this one","leafUuid":"bbb","sessionId":"s"}"#,
            "\n",
        );
        let out = parse_body(dir.path(), "prompts.jsonl", body);
        assert_eq!(
            out.session.first_prompt.as_deref(),
            Some("actually this one")
        );
    }

    /// The carry is what makes a tail parse agree with a whole-file one. Checked here at the
    /// parser level; `index.rs` proves the same property end to end.
    #[test]
    fn the_carry_suppresses_a_duplicate_for_a_tool_use_answered_across_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let head = concat!(
            r#"{"type":"assistant","uuid":"a1","sessionId":"s","cwd":"/p","message":{"role":"assistant","id":"m1","model":"mm","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build"}}]}}"#,
            "\n",
        );
        let tail = concat!(
            r#"{"type":"user","uuid":"u1","sessionId":"s","cwd":"/p","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Finished"}]},"toolUseResult":{"stdout":"Finished"}}"#,
            "\n",
        );
        let path = dir.path().join("split.jsonl");
        std::fs::write(&path, head).unwrap();
        let (first, offset) = parse_file(
            &path,
            0,
            0,
            &ParseOptions::default(),
            &FileContext::default(),
        )
        .unwrap();
        assert_eq!(first.docs.len(), 1);
        assert_eq!(
            first
                .carry
                .pending_tool_uses
                .iter()
                .map(|p| p.tool_use_id.as_str())
                .collect::<Vec<_>>(),
            ["t1"]
        );
        assert!(
            first.carry.pending_tool_uses[0].doc.is_some(),
            "the document is carried, so the late result can complete it"
        );

        std::fs::write(&path, format!("{head}{tail}")).unwrap();
        // Through serde, because that is how `index.rs` hands a carry to the next run: it
        // lives in `state.json` between processes, so a `Doc` field that does not survive the
        // round trip is silently dropped from the completed document.
        let carried: ParseCarry =
            serde_json::from_str(&serde_json::to_string(&first.carry).unwrap()).unwrap();
        let ctx = FileContext {
            carry: carried,
            ..FileContext::default()
        };
        let (second, _) = parse_file(
            &path,
            offset,
            first.docs.len() as u64,
            &ParseOptions::default(),
            &ctx,
        )
        .unwrap();
        assert!(
            second.docs.is_empty(),
            "the result belongs to a document that is already indexed: {:?}",
            texts(&second)
        );
        assert_eq!(second.session.tool_calls, 0, "the call was counted already");
        assert!(
            second.carry.pending_tool_uses.is_empty(),
            "answered, so no longer pending"
        );

        // …and the document it completes is byte-identical to the whole-file parse.
        assert_eq!(second.replacements.len(), 1);
        let whole = parse_whole(&path, &ParseOptions::default()).unwrap();
        let expected = whole
            .docs
            .iter()
            .find(|d| d.tool_use_id.as_deref() == Some("t1"))
            .unwrap();
        assert_eq!(second.replacements[0].doc_id, expected.doc_id);
        assert_eq!(second.replacements[0].seq, expected.seq);
        assert_eq!(second.replacements[0].text, expected.text);
        assert_eq!(second.replacements[0].code, expected.code);
        assert_eq!(second.replacements[0].tool_output, expected.tool_output);
        assert!(
            second.replacements[0]
                .tool_output
                .as_deref()
                .unwrap()
                .contains("Finished")
        );
        // Including `bash_cmd`: it is computed when the `tool_use` is read, so a completion
        // that rebuilt the document instead of carrying it would lose the field entirely.
        assert_eq!(
            second.replacements[0].bash_cmd,
            Some(serde_json::json!({ "program": ["cargo"], "args": ["build"] })),
        );
        assert_eq!(second.replacements[0].bash_cmd, expected.bash_cmd);

        // Without the carry the same tail invents a second half-document instead.
        let (naive, _) = parse_file(
            &path,
            offset,
            1,
            &ParseOptions::default(),
            &FileContext::default(),
        )
        .unwrap();
        assert_eq!(naive.docs.len(), 1, "the defect this carry exists to stop");
        assert!(naive.replacements.is_empty());
    }

    // -- helpers ------------------------------------------------------------

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("héllo", 3), "hé");
        assert_eq!(truncate("abc", 99), "abc");
        assert_eq!(truncate_chars("héllo", 2), "hé");
    }

    #[test]
    fn timestamps_parse_to_millis() {
        assert_eq!(
            parse_ts_ms("2026-09-09T19:07:19.248Z"),
            Some(1_788_980_839_248)
        );
        assert_eq!(parse_ts_ms("not a date"), None);
    }

    /// Sanity check against every transcript on this machine. Ignored by default because it
    /// depends on the developer's own `~/.claude`. Run with:
    /// `cargo test -- --ignored --nocapture real_transcripts_on_this_machine`
    #[test]
    #[ignore]
    fn real_transcripts_on_this_machine() {
        let root = crate::discovery::default_root().expect("a transcript root");
        let files = crate::discovery::discover(std::slice::from_ref(&root)).expect("discovery");
        println!("root: {}", root.display());
        println!("files: {}", files.len());
        let (mut docs, mut errors, mut tools, mut thinking) = (0usize, 0usize, 0usize, 0usize);
        for f in &files {
            let (out, offset) = parse_file(
                &f.path,
                0,
                0,
                &ParseOptions::default(),
                &FileContext::default(),
            )
            .expect("parse");
            docs += out.docs.len();
            errors += out.errors.len();
            tools += out
                .docs
                .iter()
                .filter(|d| d.kind == DocKind::ToolCall)
                .count();
            thinking += out.docs.iter().filter(|d| d.thinking.is_some()).count();
            println!(
                "  {:60} docs={:5} tool_calls={:4} errors={} consumed={}/{} title={:?}",
                f.path.file_name().unwrap().to_string_lossy(),
                out.docs.len(),
                out.docs
                    .iter()
                    .filter(|d| d.kind == DocKind::ToolCall)
                    .count(),
                out.errors.len(),
                offset,
                f.size,
                out.session.title.as_deref().unwrap_or("-"),
            );
            for e in out.errors.iter().take(3) {
                println!("    ERROR {e}");
            }
        }
        println!("TOTAL docs={docs} tool_calls={tools} thinking={thinking} parse_errors={errors}");
        assert_eq!(errors, 0, "real transcripts must parse without errors");
    }
}
