//! The normalized document model every agent adapter produces and everything downstream
//! consumes.
//!
//! Nothing in this module knows which agent wrote a transcript. [`Doc`] is one searchable unit
//! (a message or a tool call), [`SessionInfo`] is the per-file summary kept in
//! `sessions.json`, and [`ParseOutput`]/[`ParseCarry`] are the contract between an adapter's
//! parser and the incremental indexer. The Claude Code-specific fields on [`Doc`] (`uuid`,
//! `parent_uuid`, `entrypoint`, …) are all optional so another adapter can leave them empty
//! without a schema change.

use serde_json::Value;

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
    /// Which agent wrote the transcript this came from: a registry id such as `"claude-code"`
    /// (see [`crate::agent`]). Not to be confused with `agent_id`, which names a *subagent*
    /// within one session. `#[serde(default)]` so a carried document from an older
    /// `state.json` still loads.
    #[serde(default)]
    pub agent: String,
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
    pub is_error: bool,
    pub is_sidechain: bool,
    /// Compaction summaries, meta turns — excluded from "human prompt".
    pub is_meta: bool,
    pub entrypoint: Option<String>,
    pub permission_mode: Option<String>,
    pub version: Option<String>,
    pub slug: Option<String>,
    /// The indexed body.
    pub text: String,
    /// Stored; indexed only with `include_thinking`.
    pub thinking: Option<String>,
    /// The original JSONL line.
    pub raw: String,
}

/// `#[serde(default)]` so a `sessions.json` written by an older build — one missing a field
/// added since — still loads instead of failing the whole file.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SessionInfo {
    pub session_id: String,
    /// Registry id of the agent that wrote this transcript (`"claude-code"`, …).
    pub agent: String,
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
pub(crate) const CARRY_CAP: usize = 512;

/// How many unanswered tool calls one file carries. In practice this is 0, 1, or the width of
/// one batch of parallel calls.
pub(crate) const PENDING_CAP: usize = 32;

/// A carried document larger than this is remembered by id only: `state.json` is rewritten on
/// every run, so it must not grow to hold a copy of an enormous `Write` payload.
pub(crate) const PENDING_DOC_CAP: usize = 128 * 1024;

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
    /// `(start offset, hash)` of the last complete line consumed.
    pub tail_line: Option<TailLine>,
    /// Whatever else *this agent's* parser needs across a tail boundary, opaque to the
    /// indexer. Claude Code needs nothing beyond the fields above; a format that carries
    /// session-level state in standalone records (pi's `model_change`, for one) keeps the
    /// latest value here so a tail parse can stamp it onto the docs it emits.
    pub agent_state: Value,
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

#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// Cap on the indexed body of one doc.
    pub max_text_bytes: usize,
    /// Follow `<persisted-output>` pointers into `tool-results/<id>.txt`.
    pub load_spilled_results: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            max_text_bytes: 32 * 1024,
            load_spilled_results: false,
        }
    }
}

/// Everything about a doc that comes from the *content block*, before the record-level
/// metadata is folded in. The builder an agent adapter emits docs through.
pub struct PartialDoc {
    pub kind: DocKind,
    pub role: String,
    pub text: String,
    pub thinking: Option<String>,
    pub model: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: Option<Value>,
    pub is_error: bool,
    pub is_meta: bool,
}

impl PartialDoc {
    pub fn message(role: &str, text: String) -> PartialDoc {
        PartialDoc {
            kind: DocKind::Message,
            role: role.to_string(),
            text,
            thinking: None,
            model: None,
            tool_name: None,
            tool_use_id: None,
            tool_input: None,
            is_error: false,
            is_meta: false,
        }
    }

    pub fn tool_call(
        tool_name: Option<String>,
        tool_use_id: Option<String>,
        tool_input: Option<Value>,
        text: String,
        is_error: bool,
    ) -> PartialDoc {
        PartialDoc {
            kind: DocKind::ToolCall,
            role: "assistant".to_string(),
            text,
            thinking: None,
            model: None,
            tool_name,
            tool_use_id,
            tool_input,
            is_error,
            is_meta: false,
        }
    }

    pub fn role(mut self, role: &str) -> Self {
        self.role = role.to_string();
        self
    }
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }
    pub fn thinking(mut self, thinking: Option<String>) -> Self {
        self.thinking = thinking;
        self
    }
    pub fn meta(mut self, is_meta: bool) -> Self {
        self.is_meta = is_meta;
        self
    }
    pub fn error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }
}

/// A copy of a document small enough to sit in `state.json` until its result arrives.
pub fn carryable(doc: &Doc) -> Option<Box<Doc>> {
    (doc.text.len() + doc.raw.len() <= PENDING_DOC_CAP).then(|| Box::new(doc.clone()))
}
