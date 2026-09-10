//! Tolerant serde types for Claude Code transcript records.
//!
//! The governing rule from `docs/TRANSCRIPT-FORMAT.md` is *tolerate everything, require
//! nothing*. Concretely, in this module:
//!
//! * every record enum has a `#[serde(other)] Unknown` fallback,
//! * every struct is `#[serde(default)]` so missing fields are never an error,
//! * every struct keeps unknown keys in a `#[serde(flatten)] extra` map,
//! * scalar fields go through the `flex_*` helpers so a string where a bool was expected
//!   (or vice versa) degrades instead of failing,
//! * [`parse_line`] can never return a record error for syntactically valid JSON: a record
//!   whose *shape* is unrecognised becomes [`Record::Unknown`], and the raw
//!   [`serde_json::Value`] is handed back alongside so callers can still mine it.

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

/// Unknown keys retained verbatim from a record.
pub type Extra = Map<String, Value>;

// ---------------------------------------------------------------------------
// lenient scalar helpers
// ---------------------------------------------------------------------------

fn value_as_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn value_as_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => matches!(s.as_str(), "true" | "1" | "yes"),
        _ => false,
    }
}

fn value_as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        Value::String(s) => s.parse().ok(),
        Value::Bool(b) => Some(u64::from(*b)),
        _ => None,
    }
}

/// Accept a string, number or bool as `Option<String>`; anything else becomes `None`.
pub fn flex_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(value_as_string(&Value::deserialize(d)?))
}

/// Accept anything as a flag; only truthy JSON yields `true`.
pub fn flex_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(value_as_bool(&Value::deserialize(d)?))
}

/// Accept a number or numeric string as `Option<u64>`.
pub fn flex_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(value_as_u64(&Value::deserialize(d)?))
}

/// Accept any JSON as a `Vec<T>`: non-arrays become empty, unparseable elements are dropped.
pub fn flex_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: for<'a> Deserialize<'a>,
{
    let v = Value::deserialize(d)?;
    let Value::Array(items) = v else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|item| T::deserialize(item).ok())
        .collect())
}

// ---------------------------------------------------------------------------
// records
// ---------------------------------------------------------------------------

/// One JSONL line. `Unknown` covers every record type we do not model explicitly —
/// including ones that do not exist yet.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Record {
    #[serde(rename = "user")]
    User(UserRecord),
    #[serde(rename = "assistant")]
    Assistant(AssistantRecord),
    #[serde(rename = "attachment")]
    Attachment(AttachmentRecord),
    #[serde(rename = "system")]
    System(SystemRecord),
    #[serde(rename = "summary")]
    Summary(SummaryRecord),
    #[serde(rename = "last-prompt")]
    LastPrompt(LastPromptRecord),
    #[serde(rename = "queue-operation")]
    QueueOperation(QueueOperationRecord),
    #[serde(other)]
    Unknown,
}

/// Fields present on every DAG record (`user`, `assistant`, `attachment`, `system`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Common {
    #[serde(deserialize_with = "flex_string")]
    pub uuid: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub parent_uuid: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub timestamp: Option<String>,
    #[serde(alias = "session_id", deserialize_with = "flex_string")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub cwd: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub git_branch: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub version: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub user_type: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub entrypoint: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub slug: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub agent_id: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub permission_mode: Option<String>,
    #[serde(deserialize_with = "flex_bool")]
    pub is_sidechain: bool,
    #[serde(alias = "is_meta", deserialize_with = "flex_bool")]
    pub is_meta: bool,
}

/// `origin` on a user record: `{"kind":"human"}` or `{"kind":"peer",...}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Origin {
    #[serde(deserialize_with = "flex_string")]
    pub kind: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UserRecord {
    #[serde(flatten)]
    pub common: Common,
    pub message: Option<UserMessage>,
    /// `String | Object | absent` — the rich non-API result payload.
    pub tool_use_result: Option<Value>,
    pub origin: Option<Origin>,
    #[serde(deserialize_with = "flex_string")]
    pub prompt_id: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub source_tool_assistant_uuid: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub tool_denial_kind: Option<String>,
    #[serde(deserialize_with = "flex_bool")]
    pub is_compact_summary: bool,
    #[serde(deserialize_with = "flex_bool")]
    pub is_visible_in_transcript_only: bool,
    #[serde(deserialize_with = "flex_bool")]
    pub turn_companion: bool,
    #[serde(deserialize_with = "flex_bool")]
    pub is_virtual: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl UserRecord {
    /// The CLI's own predicate for "this is a real human turn".
    pub fn is_human_turn(&self) -> bool {
        self.origin.as_ref().and_then(|o| o.kind.as_deref()) == Some("human")
            && self.tool_use_result.is_none()
            && !self.is_compact_summary
            && !self.common.is_meta
            && !self.is_visible_in_transcript_only
            && !self.turn_companion
    }
}

/// A `user` message is exactly `{role, content}`; `content` is `String | Array<block>`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserMessage {
    #[serde(deserialize_with = "flex_string")]
    pub role: Option<String>,
    pub content: MessageContent,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AssistantRecord {
    #[serde(flatten)]
    pub common: Common,
    pub message: Option<AssistantMessage>,
    #[serde(deserialize_with = "flex_string")]
    pub request_id: Option<String>,
    #[serde(deserialize_with = "flex_u64")]
    pub api_block_index: Option<u64>,
    #[serde(deserialize_with = "flex_string")]
    pub attribution_agent: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub attribution_skill: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub effort: Option<String>,
    #[serde(deserialize_with = "flex_bool")]
    pub is_api_error_message: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AssistantMessage {
    /// Shared by every sibling block record of one API message — the dedupe key.
    #[serde(deserialize_with = "flex_string")]
    pub id: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub model: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub role: Option<String>,
    pub content: MessageContent,
    /// Duplicated verbatim on every sibling block — dedupe by [`AssistantMessage::id`].
    pub usage: Option<Usage>,
    #[serde(deserialize_with = "flex_string")]
    pub stop_reason: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    #[serde(deserialize_with = "flex_u64")]
    pub input_tokens: Option<u64>,
    #[serde(deserialize_with = "flex_u64")]
    pub output_tokens: Option<u64>,
    #[serde(deserialize_with = "flex_u64")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(deserialize_with = "flex_u64")]
    pub cache_read_input_tokens: Option<u64>,
    /// Survives even when the thinking text itself is stripped, which is what makes it the
    /// only handle on reasoning in remote and web transcripts.
    pub output_tokens_details: Option<OutputTokenDetails>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Note: the API `usage` block is snake_case, unlike the transcript envelope around it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OutputTokenDetails {
    #[serde(deserialize_with = "flex_u64")]
    pub thinking_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AttachmentRecord {
    #[serde(flatten)]
    pub common: Common,
    pub attachment: Option<Attachment>,
    /// The rendered element key is `content`, not `text`.
    #[serde(deserialize_with = "flex_vec")]
    pub rendered: Vec<Rendered>,
    #[serde(deserialize_with = "flex_vec")]
    pub rendered_in_human_turn: Vec<Rendered>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The attachment payload. The subtype universe is open, so everything below `type` is
/// kept as raw JSON and mined by `parse.rs`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Attachment {
    #[serde(rename = "type", deserialize_with = "flex_string")]
    pub kind: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Rendered {
    #[serde(deserialize_with = "flex_string")]
    pub content: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SystemRecord {
    #[serde(flatten)]
    pub common: Common,
    #[serde(deserialize_with = "flex_string")]
    pub subtype: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub content: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub level: Option<String>,
    pub compact_metadata: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SummaryRecord {
    #[serde(deserialize_with = "flex_string")]
    pub summary: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub leaf_uuid: Option<String>,
    #[serde(alias = "session_id", deserialize_with = "flex_string")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LastPromptRecord {
    #[serde(deserialize_with = "flex_string")]
    pub last_prompt: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub leaf_uuid: Option<String>,
    #[serde(alias = "session_id", deserialize_with = "flex_string")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QueueOperationRecord {
    #[serde(deserialize_with = "flex_string")]
    pub operation: Option<String>,
    /// Duplicates the `user` record's prompt — do not double-index.
    #[serde(deserialize_with = "flex_string")]
    pub content: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub timestamp: Option<String>,
    #[serde(alias = "session_id", deserialize_with = "flex_string")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

// ---------------------------------------------------------------------------
// message content
// ---------------------------------------------------------------------------

/// `String | Array<ContentBlock>` — and anything else, kept as raw JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
    Other(Value),
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Other(Value::Null)
    }
}

impl MessageContent {
    pub fn is_empty(&self) -> bool {
        match self {
            MessageContent::Text(s) => s.is_empty(),
            MessageContent::Blocks(b) => b.is_empty(),
            MessageContent::Other(v) => v.is_null(),
        }
    }

    /// The blocks of this content, or an empty slice for the string / other forms.
    pub fn blocks(&self) -> &[ContentBlock] {
        match self {
            MessageContent::Blocks(b) => b,
            _ => &[],
        }
    }

    /// Plain text carried directly by this content (the `String` form only).
    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// API content blocks. Types that exist in the CLI bundle but not in any sample
/// (`redacted_thinking`, `search_result`, ...) land in `Unknown`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text(TextBlock),
    /// A pasted screenshot, or a tool that answered with an image. Modelled rather than left
    /// to `Unknown` so the bytes can be *replaced* by a description of them
    /// (`crate::media`) instead of silently dropped along with the fact an image was there.
    #[serde(rename = "image")]
    Image(MediaBlock),
    /// The same, for a PDF or other document attached to a turn.
    #[serde(rename = "document")]
    Document(MediaBlock),
    #[serde(rename = "thinking")]
    Thinking(ThinkingBlock),
    #[serde(rename = "tool_use")]
    ToolUse(ToolUseBlock),
    #[serde(rename = "mcp_tool_use")]
    McpToolUse(ToolUseBlock),
    #[serde(rename = "server_tool_use")]
    ServerToolUse(ToolUseBlock),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultBlock),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TextBlock {
    #[serde(deserialize_with = "flex_string")]
    pub text: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// An `image` or `document` block. The payload itself is never modelled: `source` holds it in
/// the API form (`{type:"base64",media_type,data}`, or a `url` / `file_id` locator), `extra`
/// holds the flattened MCP form (`{data,mimeType}`), and `crate::media` reads a description off
/// whichever one turned up.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MediaBlock {
    pub source: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThinkingBlock {
    #[serde(deserialize_with = "flex_string")]
    pub thinking: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub signature: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolUseBlock {
    #[serde(deserialize_with = "flex_string")]
    pub id: Option<String>,
    #[serde(deserialize_with = "flex_string")]
    pub name: Option<String>,
    pub input: Option<Value>,
    /// Only emitted form is `{"type":"direct"}`; absent on older records.
    pub caller: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolResultBlock {
    #[serde(deserialize_with = "flex_string")]
    pub tool_use_id: Option<String>,
    #[serde(deserialize_with = "flex_bool")]
    pub is_error: bool,
    /// `String | Array<{type:"text"|"image",...}>`.
    pub content: MessageContent,
    #[serde(flatten)]
    pub extra: Extra,
}

// ---------------------------------------------------------------------------
// line parsing
// ---------------------------------------------------------------------------

/// One parsed JSONL line: the typed view plus the raw JSON it came from.
#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub record: Record,
    pub value: Value,
}

impl ParsedLine {
    /// `type` as written on the wire, even for [`Record::Unknown`].
    pub fn type_name(&self) -> Option<&str> {
        self.value.get("type").and_then(Value::as_str)
    }

    /// A string field straight off the raw JSON — for sidecar types we do not model.
    pub fn raw_str(&self, key: &str) -> Option<&str> {
        self.value.get(key).and_then(Value::as_str)
    }
}

/// Strip the leading NUL bytes the CLI's own reader skips (preallocated-file padding),
/// then trailing whitespace / CR — and trailing NULs, which are the same padding seen from
/// the other end and are *not* ASCII whitespace, so they would otherwise reach `serde_json`
/// and cost the whole record a "trailing characters" error.
pub fn clean_line(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|b| *b != 0).unwrap_or(line.len());
    let mut end = line.len();
    while end > start && (line[end - 1] == 0 || (line[end - 1] as char).is_ascii_whitespace()) {
        end -= 1;
    }
    &line[start..end]
}

/// Parse one cleaned line. Fails only on invalid JSON — a valid JSON object whose shape is
/// unknown yields [`Record::Unknown`] rather than an error.
pub fn parse_line(line: &[u8]) -> Result<ParsedLine, serde_json::Error> {
    let value: Value = serde_json::from_slice(line)?;
    let record = Record::deserialize(&value).unwrap_or(Record::Unknown);
    Ok(ParsedLine { record, value })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ParsedLine {
        parse_line(s.as_bytes()).expect("valid json must parse")
    }

    #[test]
    fn clean_line_strips_leading_nuls_and_trailing_ws() {
        assert_eq!(clean_line(b"\0\0{\"a\":1}\r\n"), b"{\"a\":1}");
        assert_eq!(clean_line(b"\0\0\0"), b"");
        assert_eq!(clean_line(b""), b"");
    }

    /// Regression: preallocated-file padding at the *end* of a line is the same NUL padding as
    /// at the start, and must not cost the record a "trailing characters" parse error.
    #[test]
    fn clean_line_strips_trailing_nul_padding() {
        assert_eq!(clean_line(b"{\"a\":1}\0\0\0\n"), b"{\"a\":1}");
        assert_eq!(clean_line(b"\0{\"a\":1}\0\r\n\0"), b"{\"a\":1}");
        assert!(parse_line(clean_line(b"{\"type\":\"user\"}\0\0\n")).is_ok());
    }

    #[test]
    fn garbage_is_a_json_error_never_a_panic() {
        assert!(parse_line(b"not json at all").is_err());
        assert!(parse_line(b"{\"unterminated\": ").is_err());
        assert!(parse_line(b"").is_err());
    }

    #[test]
    fn unknown_record_types_deserialize() {
        for line in [
            r#"{"type":"worktree-state","payload":{"a":1}}"#,
            r#"{"type":"a-type-invented-next-year","x":[1,2,3]}"#,
            r#"{"no_type_at_all":true}"#,
            r#"{}"#,
        ] {
            assert!(matches!(parse(line).record, Record::Unknown), "{line}");
        }
    }

    #[test]
    fn non_object_json_still_parses_to_unknown() {
        for line in ["[]", "3", "\"hello\"", "null", "true"] {
            assert!(matches!(parse(line).record, Record::Unknown), "{line}");
        }
    }

    #[test]
    fn records_without_uuid_are_fine() {
        let p = parse(r#"{"type":"summary","summary":"A title","leafUuid":"leaf-1"}"#);
        let Record::Summary(s) = p.record else {
            panic!("expected summary");
        };
        assert_eq!(s.summary.as_deref(), Some("A title"));
        assert_eq!(s.leaf_uuid.as_deref(), Some("leaf-1"));

        let p = parse(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#);
        let Record::User(u) = p.record else {
            panic!("expected user");
        };
        assert!(u.common.uuid.is_none());
        assert_eq!(u.message.unwrap().content.as_text(), Some("hi"));
    }

    #[test]
    fn wrong_types_degrade_instead_of_failing() {
        // isSidechain as a string, uuid as a number, timestamp null, message missing.
        let p = parse(r#"{"type":"user","isSidechain":"true","uuid":12,"timestamp":null}"#);
        let Record::User(u) = p.record else {
            panic!("expected user");
        };
        assert!(u.common.is_sidechain);
        assert_eq!(u.common.uuid.as_deref(), Some("12"));
        assert!(u.common.timestamp.is_none());
        assert!(u.message.is_none());
    }

    #[test]
    fn user_content_is_string_or_array() {
        let s = parse(r#"{"type":"user","message":{"role":"user","content":"plain"}}"#);
        let Record::User(u) = s.record else { panic!() };
        assert_eq!(u.message.unwrap().content.as_text(), Some("plain"));

        let a = parse(
            r#"{"type":"user","message":{"role":"user","content":[
                 {"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}]}}"#,
        );
        let Record::User(u) = a.record else { panic!() };
        let blocks = u.message.unwrap().content;
        match &blocks.blocks()[0] {
            ContentBlock::ToolResult(r) => {
                assert!(r.is_error);
                assert_eq!(r.tool_use_id.as_deref(), Some("t1"));
                assert_eq!(r.content.as_text(), Some("boom"));
            }
            other => panic!("expected tool_result, got {other:?}"),
        }

        // an array of things that are not blocks at all
        let weird = parse(r#"{"type":"user","message":{"role":"user","content":[1,2,3]}}"#);
        let Record::User(u) = weird.record else {
            panic!()
        };
        assert!(matches!(
            u.message.unwrap().content,
            MessageContent::Other(_)
        ));
    }

    #[test]
    fn tool_result_content_is_string_or_block_array() {
        let p = parse(
            r#"{"type":"user","message":{"role":"user","content":[
                 {"type":"tool_result","tool_use_id":"t2","content":[
                    {"type":"text","text":"one"},{"type":"tool_reference","tool_name":"Bash"}]}]}}"#,
        );
        let Record::User(u) = p.record else { panic!() };
        let content = u.message.unwrap().content;
        let ContentBlock::ToolResult(r) = &content.blocks()[0] else {
            panic!("expected tool_result")
        };
        assert_eq!(r.content.blocks().len(), 2);
        assert!(matches!(r.content.blocks()[1], ContentBlock::Unknown));
    }

    #[test]
    fn tool_use_result_is_string_object_or_absent() {
        let s = parse(r#"{"type":"user","toolUseResult":"Error: nope"}"#);
        let Record::User(u) = s.record else { panic!() };
        assert_eq!(u.tool_use_result.unwrap().as_str(), Some("Error: nope"));

        let o = parse(r#"{"type":"user","toolUseResult":{"stdout":"ok","stderr":""}}"#);
        let Record::User(u) = o.record else { panic!() };
        assert!(u.tool_use_result.unwrap().is_object());

        let a = parse(r#"{"type":"user"}"#);
        let Record::User(u) = a.record else { panic!() };
        assert!(u.tool_use_result.is_none());
    }

    #[test]
    fn unknown_content_block_types_are_tolerated() {
        let p = parse(
            r#"{"type":"assistant","message":{"id":"msg_1","model":"m","content":[
                 {"type":"redacted_thinking","data":"xxx"}]}}"#,
        );
        let Record::Assistant(a) = p.record else {
            panic!()
        };
        assert!(matches!(
            a.message.unwrap().content.blocks()[0],
            ContentBlock::Unknown
        ));
    }

    #[test]
    fn unknown_keys_are_retained_in_extra() {
        let p = parse(
            r#"{"type":"assistant","uuid":"u1","brandNewField":{"deep":1},
                 "message":{"id":"m1","content":[],"futureKey":7}}"#,
        );
        let Record::Assistant(a) = p.record else {
            panic!()
        };
        assert!(a.extra.contains_key("brandNewField"));
        assert!(!a.extra.contains_key("type"));
        assert!(!a.extra.contains_key("uuid"));
        assert!(a.message.unwrap().extra.contains_key("futureKey"));
    }

    #[test]
    fn human_turn_predicate_matches_the_cli() {
        let human = parse(
            r#"{"type":"user","origin":{"kind":"human"},
                 "message":{"role":"user","content":"do the thing"}}"#,
        );
        let Record::User(u) = human.record else {
            panic!()
        };
        assert!(u.is_human_turn());

        let compact = parse(r#"{"type":"user","origin":{"kind":"human"},"isCompactSummary":true}"#);
        let Record::User(u) = compact.record else {
            panic!()
        };
        assert!(!u.is_human_turn());

        let tool = parse(r#"{"type":"user","origin":{"kind":"human"},"toolUseResult":"x"}"#);
        let Record::User(u) = tool.record else {
            panic!()
        };
        assert!(!u.is_human_turn());
    }

    #[test]
    fn system_and_sidecar_records_parse() {
        let p = parse(
            r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted",
                 "level":"info","uuid":"s1","session_id":"sess"}"#,
        );
        let Record::System(s) = p.record else {
            panic!()
        };
        assert_eq!(s.subtype.as_deref(), Some("compact_boundary"));
        assert_eq!(s.common.session_id.as_deref(), Some("sess"));

        let p = parse(r#"{"type":"queue-operation","operation":"enqueue","content":"prompt"}"#);
        let Record::QueueOperation(q) = p.record else {
            panic!()
        };
        assert_eq!(q.content.as_deref(), Some("prompt"));

        let p = parse(r#"{"type":"last-prompt","lastPrompt":"p","leafUuid":"l"}"#);
        let Record::LastPrompt(l) = p.record else {
            panic!()
        };
        assert_eq!(l.last_prompt.as_deref(), Some("p"));
    }

    #[test]
    fn attachment_rendered_key_is_content() {
        let p = parse(
            r#"{"type":"attachment","attachment":{"type":"date","date":"2026-09-09"},
                 "rendered":[{"content":"today"}],"renderedInHumanTurn":"not-an-array"}"#,
        );
        let Record::Attachment(a) = p.record else {
            panic!()
        };
        assert_eq!(a.attachment.as_ref().unwrap().kind.as_deref(), Some("date"));
        assert_eq!(a.rendered[0].content.as_deref(), Some("today"));
        assert!(a.rendered_in_human_turn.is_empty());
    }

    #[test]
    fn thinking_tokens_parse_from_a_real_usage_block() {
        let line = r#"{"type":"assistant","uuid":"a","message":{"id":"m","role":"assistant",
          "usage":{"input_tokens":2,"output_tokens":623,
            "output_tokens_details":{"thinking_tokens":300},
            "cache_creation_input_tokens":22034}}}"#;
        let parsed = parse_line(line.as_bytes()).expect("parses");
        let Record::Assistant(a) = parsed.record else {
            panic!("assistant")
        };
        let usage = a.message.unwrap().usage.unwrap();
        assert_eq!(
            usage.output_tokens_details.unwrap().thinking_tokens,
            Some(300),
            "the usage block is snake_case; a camelCase rename would silently read None"
        );
    }

    #[test]
    fn usage_leaves_are_all_optional() {
        let p = parse(
            r#"{"type":"assistant","message":{"id":"m","usage":
                 {"input_tokens":2,"cache_creation_input_tokens":null,"cache_read_input_tokens":null}}}"#,
        );
        let Record::Assistant(a) = p.record else {
            panic!()
        };
        let usage = a.message.unwrap().usage.unwrap();
        assert_eq!(usage.input_tokens, Some(2));
        assert!(usage.cache_read_input_tokens.is_none());
    }
}
