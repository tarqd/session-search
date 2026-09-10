//! The Tantivy schema and the JSON encoding of a [`Doc`].
//!
//! The field table is pinned in `docs/DESIGN.md`. Three things in it are load-bearing:
//!
//! * `tool_input` is a JSON field that is **indexed and fast**, with dots expanded, which is
//!   what makes `tool_input.command:cargo` filtering *and* a terms aggregation over parameter
//!   keys that were never declared in the schema both work.
//! * `bash_cmd` is the same trick with a different tokenizer: it holds the parsed shape of a
//!   Bash command (`{"program": [...], "args": [...]}`) and is tokenized `raw`, because its
//!   contents are exact-match facts — `--release` has to stay `--release`.
//! * a message's body is spread over three *indexed* fields rather than one. `text` and
//!   `headings` hold its prose and are analyzed as English; `code` holds its snippets and is
//!   analyzed as code; `code_lang` holds the fence languages as facet values. `parse.rs` does
//!   the splitting and `markdown.rs` decides what goes where. Beside them, `body` keeps the
//!   body as it was written, stored and never indexed, because that is what gets rendered.
//!   A tool call's *result* is none of those: it lives in `tool_output`, analyzed as code.
//! * `context_text` is the one field that is indexed and **not stored**. It holds a header
//!   describing what the document was *for* — the session's title and opening prompt, the
//!   project, the branch, the prompt that opened the document's own turn — so a fragment like
//!   `yes` or `Bash cargo build` is retrievable by the words a person would actually type.
//!   It is scaffolding rather than content: not stored means it can never be read back, never
//!   rendered as a snippet and never reach `--json`. See [`context_header`].

use serde_json::{Map, Value, json};
use tantivy::schema::{
    FAST, INDEXED, IndexRecordOption, JsonObjectOptions, STORED, STRING, Schema, SchemaBuilder,
    TextFieldIndexing, TextOptions,
};

use crate::parse::{Doc, DocKind, SessionInfo, truncate_words};
use crate::tokenizer::{CODE_ANALYZER, PROSE_ANALYZER};

/// One handle per schema field. Cheap to clone.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub doc_id: tantivy::schema::Field,
    pub source_path: tantivy::schema::Field,
    pub uuid: tantivy::schema::Field,
    pub parent_uuid: tantivy::schema::Field,
    pub tool_use_id: tantivy::schema::Field,

    pub session_id: tantivy::schema::Field,
    pub agent_id: tantivy::schema::Field,
    pub agent_type: tantivy::schema::Field,
    pub project: tantivy::schema::Field,
    pub git_branch: tantivy::schema::Field,
    pub role: tantivy::schema::Field,
    pub kind: tantivy::schema::Field,
    pub model: tantivy::schema::Field,
    pub tool_name: tantivy::schema::Field,
    pub entrypoint: tantivy::schema::Field,
    pub permission_mode: tantivy::schema::Field,
    pub version: tantivy::schema::Field,
    pub slug: tantivy::schema::Field,

    pub project_facet: tantivy::schema::Field,
    pub tool_input: tantivy::schema::Field,
    pub bash_cmd: tantivy::schema::Field,
    /// Stored only: the body as a reader saw it, which is what every renderer prints.
    pub body: tantivy::schema::Field,
    /// Multi-valued: one value per prose block of the message.
    pub text: tantivy::schema::Field,
    /// Multi-valued: one value per code block or inline span of the message.
    pub code: tantivy::schema::Field,
    /// Multi-valued: one value per markdown heading.
    pub headings: tantivy::schema::Field,
    /// Multi-valued: the info-string language of each fenced block.
    pub code_lang: tantivy::schema::Field,
    /// What a tool returned, indexed apart from what it was asked to do.
    pub tool_output: tantivy::schema::Field,
    pub thinking: tantivy::schema::Field,
    pub thinking_tokens: tantivy::schema::Field,
    pub timestamp: tantivy::schema::Field,
    pub seq: tantivy::schema::Field,
    /// The `seq` of the doc that opened this doc's turn. Fast, so turn grouping and a
    /// pre-filtered scan read it columnar rather than out of the stored payload.
    pub turn_seq: tantivy::schema::Field,
    /// Indexed, never stored: the context header of [`context_header`].
    pub context_text: tantivy::schema::Field,
    pub is_error: tantivy::schema::Field,
    pub is_sidechain: tantivy::schema::Field,
    pub is_meta: tantivy::schema::Field,
    pub raw: tantivy::schema::Field,
}

/// Options for a full-text field: `TEXT | STORED` spelled out, with `analyzer` in place of
/// `default`.
///
/// `WithFreqsAndPositions` is not optional for either analyzer. The `code` one emits an
/// identifier's parts at the *same* position as the whole, which is what keeps phrases working
/// — and what makes a query word that expands into several terms a positional query; and
/// `SnippetGenerator` needs positions on any field it highlights.
fn full_text_options(analyzer: &str) -> TextOptions {
    TextOptions::default().set_stored().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(analyzer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// Options for a field that is indexed and **not stored**, with `analyzer` in place of
/// `default`.
///
/// The missing `set_stored()` is the whole design of `context_text`: a header is retrieval
/// scaffolding assembled at index time, not something the transcript said. Leaving it out of
/// the stored payload is what makes it impossible to leak — `search::doc_from_stored` cannot
/// read it back, no `SnippetGenerator` can highlight it, and `--json` cannot print it — rather
/// than a rule three separate call sites have to remember.
fn indexed_only_options(analyzer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(analyzer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// Options for the `tool_input` JSON field — copied verbatim from the verified-facts section
/// of `docs/DESIGN.md`. `set_fast(Some("raw"))` is required for aggregations;
/// `set_expand_dots_enabled()` is what makes `tool_input.command:x` parse.
fn tool_input_options() -> JsonObjectOptions {
    JsonObjectOptions::default()
        .set_stored()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CODE_ANALYZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_fast(Some("raw"))
        .set_expand_dots_enabled()
}

/// Options for the `bash_cmd` JSON field. Same shape as [`tool_input_options`] with two
/// deliberate differences: the tokenizer is `raw` and positions are not kept.
///
/// `bash_cmd` holds words the shell grammar already split for us, and every one of them is an
/// exact-match fact: `--release` must stay `--release` rather than becoming `release`, `Cargo`
/// must not match `cargo`, and `bash_cmd.program:cargo` must mean "ran cargo", not "mentions
/// cargo". The `default` tokenizer would strip the leading dashes, lowercase the value and
/// split on punctuation, which loses all three. Nothing here is prose, so there are no phrases
/// to search and `IndexRecordOption::Basic` is enough.
fn bash_cmd_options() -> JsonObjectOptions {
    JsonObjectOptions::default()
        .set_stored()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::Basic),
        )
        .set_fast(Some("raw"))
        .set_expand_dots_enabled()
}

pub fn build_schema() -> (Schema, Fields) {
    let mut sb: SchemaBuilder = Schema::builder();

    // Identity / join keys: exact-match only.
    let doc_id = sb.add_text_field("doc_id", STRING | STORED);
    // `FAST` as well as stored: `search::count_turns` groups by `(source_path, turn_seq)` and
    // reads the path per matching document. It is dictionary-encoded and there is one distinct
    // value per transcript file, so the column is a few ordinals wide however long the paths.
    let source_path = sb.add_text_field("source_path", STRING | STORED | FAST);
    let uuid = sb.add_text_field("uuid", STRING | STORED);
    let parent_uuid = sb.add_text_field("parent_uuid", STRING | STORED);
    let tool_use_id = sb.add_text_field("tool_use_id", STRING | STORED);

    // Filterable + facetable dimensions.
    let session_id = sb.add_text_field("session_id", STRING | STORED | FAST);
    let agent_id = sb.add_text_field("agent_id", STRING | STORED | FAST);
    let agent_type = sb.add_text_field("agent_type", STRING | STORED | FAST);
    let project = sb.add_text_field("project", STRING | STORED | FAST);
    let git_branch = sb.add_text_field("git_branch", STRING | STORED | FAST);
    let role = sb.add_text_field("role", STRING | STORED | FAST);
    let kind = sb.add_text_field("kind", STRING | STORED | FAST);
    let model = sb.add_text_field("model", STRING | STORED | FAST);
    let tool_name = sb.add_text_field("tool_name", STRING | STORED | FAST);
    let entrypoint = sb.add_text_field("entrypoint", STRING | STORED | FAST);
    let permission_mode = sb.add_text_field("permission_mode", STRING | STORED | FAST);
    let version = sb.add_text_field("version", STRING | STORED | FAST);
    let slug = sb.add_text_field("slug", STRING | STORED | FAST);

    // Hierarchical project path, e.g. `/home/user/session-search`.
    let project_facet = sb.add_facet_field("project_facet", STORED);
    let tool_input = sb.add_json_field("tool_input", tool_input_options());
    let bash_cmd = sb.add_json_field("bash_cmd", bash_cmd_options());

    // What a reader saw, kept whole and never indexed. The retrieval fields below cannot be
    // reassembled into it — the split drops link destinations, lifts fenced blocks out of the
    // order they were written in and repeats every inline span — and `show`, `--context` and
    // `--json` all have to print the message the transcript actually holds.
    let body = sb.add_text_field("body", STORED);
    // Prose gets prose analysis; code gets code analysis. `parse.rs` splits a markdown message
    // between the two, and `markdown.rs` says which piece goes where. `text` carries one value
    // per prose block: Tantivy separates the values of a field by a position gap, which is what
    // stops a phrase matching across a code block that was lifted out from between them.
    let text = sb.add_text_field("text", full_text_options(PROSE_ANALYZER));
    let headings = sb.add_text_field("headings", full_text_options(PROSE_ANALYZER));
    let code = sb.add_text_field("code", full_text_options(CODE_ANALYZER));
    // The fence language is a facet value, never a word inside a sentence: one term, verbatim,
    // and FAST so a terms aggregation can count it beside `tool_name`.
    let code_lang = sb.add_text_field("code_lang", STRING | STORED | FAST);
    // What a tool returned, indexed apart from what it was asked to do. Always indexed — a
    // tool result is the record of what actually happened, and `--include-thinking` has no
    // equivalent here. It takes the `code` analyzer, not `prose`: a result is diagnostics,
    // paths and program output, never a markdown message, so there is no split to make and
    // stemming `Serializes` into `serial` would only lose the identifiers in it.
    let tool_output = sb.add_text_field("tool_output", full_text_options(CODE_ANALYZER));
    // `thinking` stays on `code` for the same reason: it is prose and snippets interleaved
    // with no marker separating them, so there is no split to make and the analyzer that keeps
    // identifiers intact is the one that loses least.
    let thinking = sb.add_text_field("thinking", full_text_options(CODE_ANALYZER));
    // Fast so it can be faceted and range-filtered; this is the only measure of reasoning that
    // survives on machines where the thinking text is stripped.
    let thinking_tokens = sb.add_u64_field("thinking_tokens", FAST | STORED | INDEXED);

    let timestamp = sb.add_date_field("timestamp", INDEXED | STORED | FAST);
    let seq = sb.add_u64_field("seq", INDEXED | STORED | FAST);
    // Mirrors `seq`: INDEXED so `context::turn` can term-query it, FAST so grouping by turn is
    // a columnar read, STORED so `doc_from_stored` hands it back on every hit.
    let turn_seq = sb.add_u64_field("turn_seq", INDEXED | STORED | FAST);
    // The contextual-BM25 header. `prose`, because a header is English — a title, a prompt, a
    // branch name — so it wants the stemmer, and the shared code base still splits the
    // identifiers inside it. Indexed only: see [`indexed_only_options`].
    let context_text = sb.add_text_field("context_text", indexed_only_options(PROSE_ANALYZER));
    // STORED as well as indexed: `search::doc_from_stored` reads these back out of the stored
    // payload, so without it every `Hit`, every `show` document and every `--json` response
    // would report `false` — contradicting the very filter that selected them.
    let is_error = sb.add_u64_field("is_error", INDEXED | STORED | FAST);
    let is_sidechain = sb.add_u64_field("is_sidechain", INDEXED | STORED | FAST);
    let is_meta = sb.add_u64_field("is_meta", INDEXED | STORED | FAST);

    let raw = sb.add_text_field("raw", STORED);

    let schema = sb.build();
    let fields = Fields {
        doc_id,
        source_path,
        uuid,
        parent_uuid,
        tool_use_id,
        session_id,
        agent_id,
        agent_type,
        project,
        git_branch,
        role,
        kind,
        model,
        tool_name,
        entrypoint,
        permission_mode,
        version,
        slug,
        project_facet,
        tool_input,
        bash_cmd,
        body,
        text,
        code,
        headings,
        code_lang,
        tool_output,
        thinking,
        thinking_tokens,
        timestamp,
        seq,
        turn_seq,
        context_text,
        is_error,
        is_sidechain,
        is_meta,
        raw,
    };
    (schema, fields)
}

impl DocKind {
    /// The value stored in the `kind` field, and the one `--kind` filters on.
    pub fn as_str(self) -> &'static str {
        match self {
            DocKind::Message => "message",
            DocKind::ToolCall => "tool_call",
        }
    }
}

/// A facet path is required to start with `/` — `Facet::from` panics otherwise.
fn facet_path(project: &str) -> Option<String> {
    let trimmed = project.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    })
}

/// Milliseconds since the epoch as the RFC3339 string Tantivy's JSON document parser wants
/// for a DATE field.
fn rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339())
}

/// Bytes of the header one prose piece — a title, an opening prompt — may spend.
///
/// The cap is the point of the whole field. A header rides on *every* document, including a
/// one-line tool call, and BM25 divides a term's contribution by the document's length against
/// the corpus average: an uncapped opening prompt would out-mass the body it was prepended to
/// and turn a precise hit into a diluted one.
const CONTEXT_PROSE_BYTES: usize = 100;

/// Bytes for a header piece that is a name rather than a sentence — the project basename, the
/// branch. Neither is prose and neither is long; the cap is only there so a pathological value
/// cannot spend the whole budget.
const CONTEXT_NAME_BYTES: usize = 40;

/// Hard ceiling on the assembled header. The per-piece budgets above already sum below it
/// (3 × 100 + 2 × 40 + four separators = 396), so this is a backstop rather than a knife: it
/// exists so that adding a piece later cannot silently unbound the field, and so the
/// most document-specific piece — the turn's own prompt, composed last — is never the one a
/// long session title crowds out.
const CONTEXT_HEADER_BYTES: usize = 400;

/// What separates two header pieces. Not a term under either analyzer, so it costs the
/// document nothing but keeps the pieces from running two words together.
const CONTEXT_SEP: &str = " · ";

/// The searchable word of a project path. `project` already holds the whole path as an
/// exact-match field and `-p` matches it by prefix; what a header wants is the one word
/// somebody would type, not the `/home/user/` above it.
fn basename(path: &str) -> Option<&str> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
}

/// One header piece as it would be indexed: whitespace collapsed, then capped on a word
/// boundary. Whitespace is not a term — a prompt wrapped over ten lines would otherwise spend
/// most of its budget on the gaps between its words.
fn piece_of(raw: &str, budget: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_words(&collapsed, budget)
}

/// `raw`, unless it would index as the same piece the document's own body does.
fn unless_own<'a>(raw: Option<&'a str>, own_words: &str) -> Option<&'a str> {
    raw.filter(|r| piece_of(r, CONTEXT_PROSE_BYTES) != own_words)
}

/// Append one header piece, collapsed, capped and deduplicated.
fn push_piece(pieces: &mut Vec<String>, raw: Option<&str>, budget: usize) {
    let Some(raw) = raw else { return };
    let piece = piece_of(raw, budget);
    // A session whose title *is* its opening prompt, or whose first turn is the one being
    // indexed, would otherwise index the same sentence two and three times over and hand it a
    // term frequency it did not earn.
    if piece.is_empty() || pieces.contains(&piece) {
        return;
    }
    pieces.push(piece);
}

/// The contextual-BM25 header for one document: what the fragment was *for*, in the words
/// someone would search for it by.
///
/// Our documents are fragments torn out of a conversation. A tool call indexes a tool name and
/// the strings of its input (`Bash cargo build --release`) with no trace of what it was in aid
/// of; half of all user messages are `yes`, `do that`, `still broken`. Neither is retrievable
/// by anything a person would type. Prepending the chunk's own context before indexing is the
/// lexical half of contextual retrieval, and it needs no embeddings — it is an index-time text
/// change to the inverted index we already have.
///
/// Composed cheapest first, and from two different places by necessity:
///
/// * the session's title and opening prompt come from `session`, the row `index.rs` merges into
///   `sessions.json`. A tail parse cannot know either — the `summary` record and the first
///   prompt are behind its byte offset — so the indexer supplies them from the merged row;
/// * `project` and `git_branch` are on the document already;
/// * the turn's opening prompt rides on the document as [`Doc::turn_prompt`], because only the
///   parser can know it and only [`crate::parse::ParseCarry`] can carry it across a boundary.
///
/// A prompt piece that is this document's own text is not context and is left out: the
/// document that *is* its turn's opening prompt, or the session's, would otherwise index its
/// own words a second time, inflating their frequency in the one document where they need no
/// help. The turn's opener is recognised by `seq == turn_seq`; the session's is recognised by
/// its text, because it is not reliably the first document of its file — a compaction summary
/// or a `system` record can precede it, and a tail parse numbers from `seq_base` — and in a
/// subagent file it never passed `is_human_turn()` at all.
pub fn context_header(doc: &Doc, session: Option<&SessionInfo>) -> Option<String> {
    let own_words = piece_of(&doc.body, CONTEXT_PROSE_BYTES);
    let not_own = |raw| unless_own(raw, &own_words);

    let mut pieces: Vec<String> = Vec::new();
    push_piece(
        &mut pieces,
        session.and_then(|s| s.title.as_deref()),
        CONTEXT_PROSE_BYTES,
    );
    push_piece(
        &mut pieces,
        not_own(session.and_then(|s| s.first_prompt.as_deref())),
        CONTEXT_PROSE_BYTES,
    );
    push_piece(
        &mut pieces,
        doc.project.as_deref().and_then(basename),
        CONTEXT_NAME_BYTES,
    );
    push_piece(&mut pieces, doc.git_branch.as_deref(), CONTEXT_NAME_BYTES);
    // The opener is known by number as well as by text: `seq == turn_seq` is exact for the
    // document a human prompt was emitted as, and the text check covers the session's opening
    // prompt and a prompt whose record emitted something else first.
    if doc.seq != doc.turn_seq {
        push_piece(
            &mut pieces,
            not_own(doc.turn_prompt.as_deref()),
            CONTEXT_PROSE_BYTES,
        );
    }
    if pieces.is_empty() {
        return None;
    }
    Some(truncate_words(
        &pieces.join(CONTEXT_SEP),
        CONTEXT_HEADER_BYTES,
    ))
}

/// Encode a [`Doc`] for `TantivyDocument::parse_json`. Absent values are omitted rather than
/// written as `null` — Tantivy would reject a null for a typed field.
///
/// `session` is the merged `sessions.json` row for this document's transcript, and is what the
/// `context_text` header's session half is built from; `None` composes the header from the
/// document alone. It is a parameter rather than something read off the `Doc` because a tail
/// parse never sees the title or the opening prompt, and the indexer's merged row does.
pub fn doc_to_json(doc: &Doc, session: Option<&SessionInfo>, include_thinking: bool) -> Value {
    let mut o = Map::new();
    let put_str = |o: &mut Map<String, Value>, key: &str, v: Option<&str>| {
        if let Some(v) = v.filter(|s| !s.is_empty()) {
            o.insert(key.to_string(), json!(v));
        }
    };

    put_str(&mut o, "doc_id", Some(&doc.doc_id));
    put_str(&mut o, "source_path", Some(&doc.source_path));
    put_str(&mut o, "uuid", doc.uuid.as_deref());
    put_str(&mut o, "parent_uuid", doc.parent_uuid.as_deref());
    put_str(&mut o, "tool_use_id", doc.tool_use_id.as_deref());
    put_str(&mut o, "session_id", Some(&doc.session_id));
    put_str(&mut o, "agent_id", doc.agent_id.as_deref());
    put_str(&mut o, "agent_type", doc.agent_type.as_deref());
    put_str(&mut o, "project", doc.project.as_deref());
    put_str(&mut o, "git_branch", doc.git_branch.as_deref());
    put_str(&mut o, "role", Some(&doc.role));
    put_str(&mut o, "kind", Some(doc.kind.as_str()));
    put_str(&mut o, "model", doc.model.as_deref());
    put_str(&mut o, "tool_name", doc.tool_name.as_deref());
    put_str(&mut o, "entrypoint", doc.entrypoint.as_deref());
    put_str(&mut o, "permission_mode", doc.permission_mode.as_deref());
    put_str(&mut o, "version", doc.version.as_deref());
    put_str(&mut o, "slug", doc.slug.as_deref());
    put_str(&mut o, "body", Some(&doc.body));
    // Indexed, never stored: the schema entry has no `set_stored()`, so this reaches the
    // inverted index and nothing else.
    put_str(
        &mut o,
        "context_text",
        context_header(doc, session).as_deref(),
    );
    put_str(&mut o, "tool_output", doc.tool_output.as_deref());
    put_str(&mut o, "raw", Some(&doc.raw));

    // Multi-valued fields: Tantivy adds one value per array element, and an empty array is the
    // same as an absent key, so no emptiness check is needed beyond dropping blank entries.
    let put_list = |o: &mut Map<String, Value>, key: &str, values: &[String]| {
        let kept: Vec<&String> = values.iter().filter(|s| !s.trim().is_empty()).collect();
        if !kept.is_empty() {
            o.insert(key.to_string(), json!(kept));
        }
    };
    put_list(&mut o, "text", &doc.text);
    put_list(&mut o, "code", &doc.code);
    put_list(&mut o, "headings", &doc.headings);
    put_list(&mut o, "code_lang", &doc.code_langs);

    if let Some(facet) = doc.project.as_deref().and_then(facet_path) {
        o.insert("project_facet".to_string(), json!(facet));
    }
    if let Some(input) = &doc.tool_input {
        // The JSON field wants an object; anything else is wrapped so it stays searchable.
        let value = if input.is_object() {
            input.clone()
        } else {
            json!({ "value": input })
        };
        o.insert("tool_input".to_string(), value);
    }
    if let Some(n) = doc.thinking_tokens {
        // Not gated on include_thinking: this is metadata about the turn, not thinking text,
        // and it is the only thing left when the text was stripped before it reached disk.
        o.insert("thinking_tokens".to_string(), json!(n));
    }
    // Only ever `Some` for a Bash call whose command parsed; an absent value must stay absent
    // rather than becoming an empty object, so `bash_cmd.program:*` means "a Bash script ran".
    if let Some(bash_cmd) = &doc.bash_cmd {
        o.insert("bash_cmd".to_string(), bash_cmd.clone());
    }
    if include_thinking && let Some(t) = doc.thinking.as_deref().filter(|s| !s.is_empty()) {
        o.insert("thinking".to_string(), json!(t));
    }
    if let Some(ts) = doc.timestamp_ms.and_then(rfc3339) {
        o.insert("timestamp".to_string(), json!(ts));
    }
    o.insert("seq".to_string(), json!(doc.seq));
    o.insert("turn_seq".to_string(), json!(doc.turn_seq));
    o.insert("is_error".to_string(), json!(u64::from(doc.is_error)));
    o.insert(
        "is_sidechain".to_string(),
        json!(u64::from(doc.is_sidechain)),
    );
    o.insert("is_meta".to_string(), json!(u64::from(doc.is_meta)));

    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tantivy::TantivyDocument;
    use tantivy::schema::Value as _;

    fn sample() -> Doc {
        Doc {
            doc_id: "sess:-:7".into(),
            kind: DocKind::ToolCall,
            source_path: "/tmp/sess.jsonl".into(),
            seq: 7,
            turn_seq: 5,
            turn_prompt: Some("fix the tokenizer".into()),
            session_id: "sess".into(),
            agent_id: None,
            agent_type: None,
            uuid: Some("u-1".into()),
            parent_uuid: Some("u-0".into()),
            timestamp_ms: Some(1_788_980_839_248),
            project: Some("/home/user/session-search".into()),
            git_branch: Some("main".into()),
            role: "assistant".into(),
            model: Some("claude-opus-5".into()),
            tool_name: Some("Bash".into()),
            tool_use_id: Some("toolu_1".into()),
            tool_input: Some(json!({"command": "cargo build", "timeout": 600000})),
            bash_cmd: Some(json!({"program": ["cargo"], "args": ["build"]})),
            is_error: true,
            is_sidechain: false,
            is_meta: false,
            entrypoint: Some("remote_mobile".into()),
            permission_mode: Some("default".into()),
            version: Some("2.1.266".into()),
            slug: Some("wild-spinning-puppy".into()),
            body: "Bash\ncargo build\nCompiling tantivy v0.26.2\nFinished dev".into(),
            text: vec!["Bash\ncargo build".into()],
            code: vec!["Compiling tantivy v0.26.2".into(), "Finished dev".into()],
            headings: Vec::new(),
            code_langs: Vec::new(),
            tool_output: Some("Finished dev profile".into()),
            thinking: Some("hmm".into()),
            thinking_tokens: None,
            raw: "{}".into(),
        }
    }

    #[test]
    fn schema_has_every_pinned_field() {
        let (schema, _) = build_schema();
        for name in [
            "doc_id",
            "source_path",
            "uuid",
            "parent_uuid",
            "tool_use_id",
            "session_id",
            "agent_id",
            "agent_type",
            "project",
            "git_branch",
            "role",
            "kind",
            "model",
            "tool_name",
            "entrypoint",
            "permission_mode",
            "version",
            "slug",
            "project_facet",
            "tool_input",
            "bash_cmd",
            "body",
            "text",
            "code",
            "headings",
            "code_lang",
            "tool_output",
            "thinking",
            "timestamp",
            "seq",
            "turn_seq",
            "context_text",
            "is_error",
            "is_sidechain",
            "is_meta",
            "raw",
        ] {
            assert!(schema.get_field(name).is_ok(), "missing field {name}");
        }
    }

    #[test]
    fn tool_input_is_indexed_fast_and_dot_expanded() {
        let (schema, f) = build_schema();
        let entry = schema.get_field_entry(f.tool_input);
        let tantivy::schema::FieldType::JsonObject(opts) = entry.field_type() else {
            panic!("tool_input must be a JSON field");
        };
        assert!(opts.is_stored());
        assert!(opts.is_expand_dots_enabled());
        assert!(opts.get_fast_field_tokenizer_name() == Some("raw"));
        assert!(opts.get_text_indexing_options().is_some());
    }

    #[test]
    fn bash_cmd_is_a_raw_tokenized_fast_json_field() {
        let (schema, f) = build_schema();
        let entry = schema.get_field_entry(f.bash_cmd);
        let tantivy::schema::FieldType::JsonObject(opts) = entry.field_type() else {
            panic!("bash_cmd must be a JSON field");
        };
        assert!(opts.is_stored());
        assert!(opts.is_expand_dots_enabled());
        assert_eq!(opts.get_fast_field_tokenizer_name(), Some("raw"));
        let indexing = opts
            .get_text_indexing_options()
            .expect("bash_cmd must be indexed");
        assert_eq!(indexing.tokenizer(), "raw", "exact-match facts, not prose");
        assert_eq!(indexing.index_option(), IndexRecordOption::Basic);
    }

    #[test]
    fn doc_json_round_trips_through_parse_json() {
        let (schema, f) = build_schema();
        let value = doc_to_json(&sample(), None, true);
        let doc = TantivyDocument::parse_json(&schema, &value.to_string())
            .expect("tantivy must accept our json");

        let first = |field| doc.get_first(field).unwrap().as_str().unwrap().to_string();
        assert_eq!(first(f.doc_id), "sess:-:7");
        assert_eq!(first(f.kind), "tool_call");
        assert_eq!(first(f.tool_name), "Bash");
        assert_eq!(doc.get_first(f.seq).unwrap().as_u64(), Some(7));
        assert_eq!(doc.get_first(f.turn_seq).unwrap().as_u64(), Some(5));
        assert_eq!(doc.get_first(f.is_error).unwrap().as_u64(), Some(1));
        assert!(doc.get_first(f.timestamp).unwrap().as_datetime().is_some());
        assert!(doc.get_first(f.project_facet).unwrap().as_facet().is_some());
        assert!(doc.get_first(f.tool_input).is_some());
        assert!(doc.get_first(f.bash_cmd).is_some());
        assert!(doc.get_first(f.thinking).is_some());
        // The body is stored whole, beside the fields the split produced from it.
        assert_eq!(
            first(f.body),
            "Bash\ncargo build\nCompiling tantivy v0.26.2\nFinished dev"
        );
        // `text`, `code`, `headings` and `code_lang` are multi-valued: a JSON array becomes one
        // value per element, not one value holding a rendered array.
        assert_eq!(doc.get_all(f.code).count(), 2);
        assert_eq!(
            doc.get_first(f.code).unwrap().as_str(),
            Some("Compiling tantivy v0.26.2")
        );
    }

    #[test]
    fn the_multi_valued_fields_round_trip_every_entry() {
        let (schema, f) = build_schema();
        let mut d = sample();
        d.kind = DocKind::Message;
        d.text = vec!["First para".into(), "Second para".into(), "  ".into()];
        d.code = vec!["let a = 1;".into(), "let b = 2;".into(), "  ".into()];
        d.headings = vec!["First".into(), "Second".into()];
        d.code_langs = vec!["rust".into(), "bash".into()];
        let value = doc_to_json(&d, None, false);
        let doc = TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();

        // A blank entry is dropped rather than stored as an empty value.
        let text: Vec<&str> = doc.get_all(f.text).filter_map(|v| v.as_str()).collect();
        assert_eq!(text, ["First para", "Second para"]);
        let code: Vec<&str> = doc.get_all(f.code).filter_map(|v| v.as_str()).collect();
        assert_eq!(code, ["let a = 1;", "let b = 2;"]);
        let heads: Vec<&str> = doc.get_all(f.headings).filter_map(|v| v.as_str()).collect();
        assert_eq!(heads, ["First", "Second"]);
        let langs: Vec<&str> = doc
            .get_all(f.code_lang)
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(langs, ["rust", "bash"]);

        // Empty vectors are omitted entirely, the way every other absent value is.
        d.text.clear();
        d.code.clear();
        d.headings.clear();
        d.code_langs.clear();
        let value = doc_to_json(&d, None, false);
        assert!(value.get("text").is_none());
        assert!(value.get("code").is_none());
        assert!(value.get("headings").is_none());
        assert!(value.get("code_lang").is_none());
        TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();
    }

    #[test]
    fn thinking_is_omitted_unless_opted_in() {
        let (schema, f) = build_schema();
        let value = doc_to_json(&sample(), None, false);
        assert!(value.get("thinking").is_none());
        let doc = TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();
        assert!(doc.get_first(f.thinking).is_none());
    }

    #[test]
    fn absent_values_are_omitted_not_null() {
        let (schema, _) = build_schema();
        let mut d = sample();
        d.timestamp_ms = None;
        d.project = None;
        d.tool_input = None;
        d.bash_cmd = None;
        d.model = None;
        let value = doc_to_json(&d, None, true);
        assert!(value.get("timestamp").is_none());
        assert!(value.get("project_facet").is_none());
        assert!(value.get("tool_input").is_none());
        assert!(value.get("bash_cmd").is_none());
        TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();
    }

    #[test]
    fn non_object_tool_input_is_wrapped() {
        let (schema, _) = build_schema();
        let mut d = sample();
        d.tool_input = Some(json!("a bare string"));
        let value = doc_to_json(&d, None, false);
        assert_eq!(value["tool_input"]["value"], json!("a bare string"));
        TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();
    }

    /// End-to-end proof that the pinned schema delivers what `docs/DESIGN.md` promises:
    /// dynamic-subpath filtering *and* a terms aggregation over a parameter key that was
    /// never declared in the schema.
    #[test]
    fn tool_input_supports_subpath_queries_and_aggregations() {
        use tantivy::aggregation::AggregationCollector;
        use tantivy::aggregation::agg_req::Aggregations;
        use tantivy::collector::TopDocs;
        use tantivy::query::{AllQuery, QueryParser};

        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/assistant_split_blocks.jsonl");
        let out =
            crate::parse::parse_whole(&fixture, &crate::parse::ParseOptions::default()).unwrap();

        let (schema, f) = build_schema();
        let index = crate::tokenizer::create_in_ram(schema.clone());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for doc in &out.docs {
            let json = doc_to_json(doc, None, false).to_string();
            writer
                .add_document(TantivyDocument::parse_json(&schema, &json).unwrap())
                .unwrap();
        }
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();

        // `TopDocs::with_limit(n)` alone is not a Collector in 0.26 — the ordering matters.
        let qp = QueryParser::for_index(&index, vec![f.text, f.tool_input]);
        for query in [
            "tool_input.command:cargo",
            "tool_input.file_path:index.rs",
            r#"tool_input.command:"cargo build""#,
        ] {
            let parsed = qp.parse_query(query).unwrap();
            let hits = searcher
                .search(&parsed, &TopDocs::with_limit(10).order_by_score())
                .unwrap();
            assert!(!hits.is_empty(), "no hits for {query}");
        }

        // Aggregation on a subpath never named in the schema, keyed on the raw fast value.
        let aggs: Aggregations = serde_json::from_value(json!({
            "f": { "terms": { "field": "tool_input.command", "size": 10 } }
        }))
        .unwrap();
        let collector = AggregationCollector::from_aggs(aggs, Default::default());
        let res = searcher.search(&AllQuery, &collector).unwrap();
        let buckets = serde_json::to_value(res).unwrap();
        let keys: Vec<String> = buckets["f"]["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["key"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(keys, vec!["cargo build".to_string()]);
    }

    /// The `bash_cmd` twin of the test above, and the reason it uses a different tokenizer:
    /// the values are exact-match facts. Proven here, once, so nothing downstream has to guess:
    ///
    /// * `bash_cmd.program:cargo` parses through `QueryParser` and hits;
    /// * a terms aggregation on `bash_cmd.program` — a subpath under a JSON field — buckets;
    /// * `bash_cmd.args:"--release"` hits *exactly*, dashes and all;
    /// * `bash_cmd.program:Cargo` does **not** hit `cargo`, i.e. the `raw` tokenizer really is
    ///   applied to a JSON subpath by the query parser as well as by the indexer.
    #[test]
    fn bash_cmd_is_queryable_aggregatable_and_exact() {
        use tantivy::aggregation::AggregationCollector;
        use tantivy::aggregation::agg_req::Aggregations;
        use tantivy::collector::{Count, TopDocs};
        use tantivy::query::{AllQuery, QueryParser};

        let commands = [
            "cargo build --release",
            "cd /tmp/x && cargo test -- --nocapture",
            "git log --oneline -5 | head -20",
        ];
        let (schema, f) = build_schema();
        // Through `tokenizer::create_in_ram`, not `Index::create_in_ram`: `text` is analyzed
        // with `prose` now, and an unregistered analyzer fails at write time, not at open.
        let index = crate::tokenizer::create_in_ram(schema.clone());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for (i, command) in commands.iter().enumerate() {
            let mut doc = sample();
            doc.doc_id = format!("sess:-:{i}");
            doc.seq = i as u64;
            doc.tool_input = Some(json!({ "command": command }));
            doc.bash_cmd = crate::bash::extract(command).map(|c| c.to_json());
            assert!(doc.bash_cmd.is_some(), "{command:?} should parse");
            let json = doc_to_json(&doc, None, false).to_string();
            writer
                .add_document(TantivyDocument::parse_json(&schema, &json).unwrap())
                .unwrap();
        }
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();

        let qp = QueryParser::for_index(&index, vec![f.text, f.tool_input]);
        let count = |query: &str| -> usize {
            let parsed = qp
                .parse_query(query)
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            searcher.search(&parsed, &Count).unwrap()
        };

        assert_eq!(count("bash_cmd.program:cargo"), 2, "two scripts run cargo");
        assert_eq!(count("bash_cmd.program:cd"), 1);
        assert_eq!(count("bash_cmd.program:head"), 1, "inside a pipeline");
        // Quoted, because a bare leading `-` is negation in the query grammar.
        assert_eq!(count(r#"bash_cmd.args:"--release""#), 1);
        assert_eq!(count(r#"bash_cmd.args:"--oneline""#), 1);
        assert_eq!(count(r#"bash_cmd.args:"-5""#), 1);

        // The `raw` tokenizer, end to end: no lowercasing, no splitting on punctuation, and no
        // stripping of leading dashes. If any of these ever hit, `--program` has started lying.
        assert_eq!(
            count("bash_cmd.program:Cargo"),
            0,
            "raw means case-sensitive"
        );
        assert_eq!(
            count("bash_cmd.args:release"),
            0,
            "the dashes are part of the term"
        );
        assert_eq!(count("bash_cmd.args:oneline"), 0);
        assert_eq!(count(r#"bash_cmd.args:"/tmp/x""#), 1, "a path is one term");
        assert_eq!(count("bash_cmd.args:tmp"), 0, "...not three");

        // ...and the query really did reach documents, not just parse.
        let parsed = qp.parse_query("bash_cmd.program:cargo").unwrap();
        let hits = searcher
            .search(&parsed, &TopDocs::with_limit(10).order_by_score())
            .unwrap();
        assert_eq!(hits.len(), 2);

        let aggs: Aggregations = serde_json::from_value(json!({
            "p": { "terms": { "field": "bash_cmd.program", "size": 10 } },
            "a": { "terms": { "field": "bash_cmd.args", "size": 10 } }
        }))
        .unwrap();
        let collector = AggregationCollector::from_aggs(aggs, Default::default());
        let res = searcher.search(&AllQuery, &collector).unwrap();
        let buckets = serde_json::to_value(res).unwrap();
        let counted = |key: &str| -> BTreeMap<String, u64> {
            buckets[key]["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| {
                    (
                        b["key"].as_str().unwrap().to_string(),
                        b["doc_count"].as_u64().unwrap(),
                    )
                })
                .collect()
        };
        let programs = counted("p");
        assert_eq!(programs.get("cargo"), Some(&2));
        assert_eq!(programs.get("cd"), Some(&1));
        assert_eq!(programs.get("git"), Some(&1));
        assert_eq!(programs.get("head"), Some(&1));
        let args = counted("a");
        assert_eq!(args.get("--release"), Some(&1), "{args:?}");
        assert_eq!(args.get("/tmp/x"), Some(&1));
    }

    #[test]
    fn facet_paths_always_start_with_a_slash() {
        assert_eq!(facet_path("/a/b").as_deref(), Some("/a/b"));
        assert_eq!(facet_path("C:relative").as_deref(), Some("/C:relative"));
        assert_eq!(facet_path("/a/b/").as_deref(), Some("/a/b"));
        assert!(facet_path("/").is_none());
        assert!(facet_path("").is_none());
    }

    fn session() -> SessionInfo {
        SessionInfo {
            session_id: "sess".into(),
            title: Some("Tuning the markdown tokenizer".into()),
            first_prompt: Some("make search find things by what they were for".into()),
            source_path: "/tmp/sess.jsonl".into(),
            ..SessionInfo::default()
        }
    }

    /// The two halves of the `context_text` decision, in one assertion each: it is analyzed as
    /// English prose, and it is **not stored**. Not stored is what makes "never rendered, never
    /// returned, never in `--json`" a property of the schema rather than a rule three call
    /// sites have to keep remembering.
    #[test]
    fn context_text_is_prose_analyzed_and_never_stored() {
        let (schema, f) = build_schema();
        let entry = schema.get_field_entry(f.context_text);
        assert_eq!(entry.name(), "context_text");
        assert!(
            !entry.is_stored(),
            "a header is retrieval scaffolding, not something the transcript said"
        );
        let tantivy::schema::FieldType::Str(opts) = entry.field_type() else {
            panic!("context_text must be a text field");
        };
        let indexing = opts
            .get_indexing_options()
            .expect("context_text must be indexed");
        assert_eq!(indexing.tokenizer(), PROSE_ANALYZER);
        assert_eq!(
            indexing.index_option(),
            IndexRecordOption::WithFreqsAndPositions,
            "the prose analyzer emits an identifier's parts at one position, so phrases need them"
        );
    }

    /// What the header is made of, and where each piece comes from: the session row supplies
    /// the title and the opening prompt a tail parse cannot see, the document supplies the
    /// project, the branch and its own turn's prompt.
    #[test]
    fn the_header_says_what_the_fragment_was_for() {
        let mut d = sample();
        d.seq = 7;
        d.turn_seq = 5;
        let header = context_header(&d, Some(&session())).expect("a header");
        for word in [
            "Tuning the markdown tokenizer",
            "make search find things",
            "session-search",
            "main",
            "fix the tokenizer",
        ] {
            assert!(header.contains(word), "{word:?} missing from {header:?}");
        }
        // The project's *basename*: the whole path is already an exact-match field, and the
        // directories above it are not words anybody searches by.
        assert!(!header.contains("/home/user"), "{header}");

        // With no session row there is still a header — the document knows where it was and
        // what its turn asked.
        let alone = context_header(&d, None).expect("a header from the doc alone");
        assert!(alone.contains("session-search") && alone.contains("fix the tokenizer"));
        assert!(!alone.contains("Tuning"));
    }

    /// The document that *is* its turn's opening prompt would otherwise index its own words a
    /// second time, doubling their frequency in the one document that needs no help finding
    /// them.
    #[test]
    fn the_opening_prompt_of_a_turn_is_not_repeated_in_its_own_header() {
        let mut d = sample();
        d.seq = 5;
        d.turn_seq = 5;
        let header = context_header(&d, None).expect("a header");
        assert!(!header.contains("fix the tokenizer"), "{header}");
        assert!(header.contains("session-search"), "{header}");
    }

    /// A piece repeated across pieces is dropped rather than indexed twice: a session whose
    /// title *is* its opening prompt would otherwise hand those words a term frequency they
    /// did not earn in every document of the session.
    #[test]
    fn a_header_never_repeats_the_same_piece() {
        let mut d = sample();
        d.turn_prompt = Some("one and only prompt".into());
        let info = SessionInfo {
            title: Some("one and only prompt".into()),
            first_prompt: Some("one and only prompt".into()),
            ..SessionInfo::default()
        };
        let header = context_header(&d, Some(&info)).expect("a header");
        assert_eq!(header.matches("one and only prompt").count(), 1, "{header}");
    }

    /// The cap is the point of the field. A header rides on *every* document, and BM25 divides
    /// a term's contribution by the document's length: an uncapped opening prompt prepended to
    /// a one-line tool call would out-mass the body it was meant to describe.
    #[test]
    fn the_header_is_capped_and_cut_on_a_word_boundary() {
        // Five oversized pieces, each with its own leading word so that none of them is
        // deduplicated away and every one really is cut by its own budget.
        let long = "tokenizer ".repeat(200);
        let mut d = sample();
        d.seq = 7;
        d.turn_seq = 5;
        d.turn_prompt = Some(format!("turnprompt {long}"));
        d.git_branch = Some(format!("branch-{long}"));
        let info = SessionInfo {
            title: Some(format!("title {long}")),
            first_prompt: Some(format!("first {long}")),
            ..SessionInfo::default()
        };
        let header = context_header(&d, Some(&info)).expect("a header");
        assert!(
            header.len() <= CONTEXT_HEADER_BYTES,
            "{} bytes: {header}",
            header.len()
        );
        // Word boundaries, so no piece ends in a fragment like `tokeni` that matches nothing
        // anybody would type — and each piece spent its whole budget short of one word, so the
        // per-piece caps really were what stood between these inputs and an unbounded field.
        let pieces: Vec<&str> = header.split(CONTEXT_SEP).collect();
        assert_eq!(pieces.len(), 5, "{header}");
        let word = "tokenizer ".len();
        for (i, piece) in pieces.iter().enumerate() {
            let budget = match i {
                2 => "session-search".len(),
                3 => CONTEXT_NAME_BYTES,
                _ => CONTEXT_PROSE_BYTES,
            };
            assert!(
                piece.ends_with("tokenizer") || piece.ends_with("session-search"),
                "cut mid-word: {piece:?}"
            );
            assert!(
                piece.len() <= budget && piece.len() + word > budget,
                "piece {i} is {} bytes against a budget of {budget}: {piece:?}",
                piece.len()
            );
        }
        assert!(pieces[0].starts_with("title "));
        assert!(pieces[1].starts_with("first "));
        assert!(pieces[3].starts_with("branch-"));
        // The per-piece budgets are what keep the total under the ceiling, so the last piece —
        // the document's own turn prompt, the most specific thing in the header — survives a
        // session whose every other piece is oversized.
        assert!(pieces[4].starts_with("turnprompt "), "{header}");
    }

    /// The session's opening prompt is not context for the document that *is* that prompt,
    /// wherever it sits in the file: a compaction summary or a `system` record can precede it,
    /// so `seq == 0` says nothing, and in a subagent file it never opened a turn at all.
    #[test]
    fn the_session_opening_prompt_is_not_repeated_in_its_own_header() {
        let mut d = sample();
        d.kind = DocKind::Message;
        d.role = "user".into();
        d.seq = 3;
        d.turn_seq = 3;
        d.body = "Please characterize   the transcript\nformat".into();
        d.turn_prompt = None;
        let info = SessionInfo {
            title: Some("Transcript format study".into()),
            first_prompt: Some("Please characterize the transcript format".into()),
            ..SessionInfo::default()
        };
        let header = context_header(&d, Some(&info)).expect("a header");
        assert!(!header.contains("characterize"), "{header}");
        assert!(header.contains("Transcript format study"), "{header}");

        // The same prompt is context for every other document of the session.
        let mut other = sample();
        other.seq = 4;
        other.turn_seq = 3;
        let header = context_header(&other, Some(&info)).expect("a header");
        assert!(header.contains("characterize the transcript"), "{header}");
    }

    /// Multi-byte text is cut where the chars allow, at every budget: a header is assembled
    /// from whatever the transcript held, and a slice through a char would panic.
    #[test]
    fn a_header_never_splits_a_utf8_char() {
        let mut d = sample();
        d.git_branch = Some("ünicode-brânch-ünicode-brânch-ünicode-brânch-ünicode".into());
        d.turn_prompt = Some("héllo wörld ".repeat(40));
        let info = SessionInfo {
            title: Some("é".repeat(400)),
            ..SessionInfo::default()
        };
        let header = context_header(&d, Some(&info)).expect("a header");
        assert!(header.len() <= CONTEXT_HEADER_BYTES);
        assert!(header.is_char_boundary(header.len()));
    }

    /// Indexed and nowhere else. The encoder emits it; the JSON the CLI prints does not.
    #[test]
    fn the_header_is_indexed_and_absent_from_the_json_output() {
        let mut d = sample();
        d.seq = 7;
        d.turn_seq = 5;
        let value = doc_to_json(&d, Some(&session()), false);
        assert!(
            value["context_text"]
                .as_str()
                .is_some_and(|h| h.contains("tokenizer")),
            "{value:?}"
        );
        let (schema, _) = build_schema();
        TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();
        assert!(
            crate::format::doc_json(&d).get("context_text").is_none(),
            "scaffolding must not reach --json"
        );
    }

    /// A document with nothing to say about itself gets no header at all, rather than an empty
    /// value: an absent field is one fewer term position and one fewer thing to explain.
    #[test]
    fn a_document_with_no_context_gets_no_header() {
        let mut d = sample();
        d.project = None;
        d.git_branch = None;
        d.turn_prompt = None;
        assert!(context_header(&d, None).is_none());
        assert!(doc_to_json(&d, None, false).get("context_text").is_none());
    }
}
