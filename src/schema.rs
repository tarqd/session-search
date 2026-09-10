//! The Tantivy schema and the JSON encoding of a [`Doc`].
//!
//! The field table is pinned in `docs/DESIGN.md`. Two things in it are load-bearing:
//!
//! * `tool_input` is a JSON field that is **indexed and fast**, with dots expanded, which is
//!   what makes `tool_input.command:cargo` filtering *and* a terms aggregation over parameter
//!   keys that were never declared in the schema both work.
//! * a message's body is spread over three *indexed* fields rather than one. `text` and
//!   `headings` hold its prose and are analyzed as English; `code` holds its snippets and is
//!   analyzed as code; `code_lang` holds the fence languages as facet values. `parse.rs` does
//!   the splitting and `markdown.rs` decides what goes where. Beside them, `body` keeps the
//!   body as it was written, stored and never indexed, because that is what gets rendered.

use serde_json::{Map, Value, json};
use tantivy::schema::{
    FAST, INDEXED, IndexRecordOption, JsonObjectOptions, STORED, STRING, Schema, SchemaBuilder,
    TextFieldIndexing, TextOptions,
};

use crate::parse::{Doc, DocKind};
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
    pub thinking: tantivy::schema::Field,
    pub timestamp: tantivy::schema::Field,
    pub seq: tantivy::schema::Field,
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

pub fn build_schema() -> (Schema, Fields) {
    let mut sb: SchemaBuilder = Schema::builder();

    // Identity / join keys: exact-match only.
    let doc_id = sb.add_text_field("doc_id", STRING | STORED);
    let source_path = sb.add_text_field("source_path", STRING | STORED);
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
    // `thinking` stays on `code`: it is prose and snippets interleaved with no marker
    // separating them, so there is no split to make and the analyzer that keeps identifiers
    // intact is the one that loses least.
    let thinking = sb.add_text_field("thinking", full_text_options(CODE_ANALYZER));

    let timestamp = sb.add_date_field("timestamp", INDEXED | STORED | FAST);
    let seq = sb.add_u64_field("seq", INDEXED | STORED | FAST);
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
        body,
        text,
        code,
        headings,
        code_lang,
        thinking,
        timestamp,
        seq,
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

/// Encode a [`Doc`] for `TantivyDocument::parse_json`. Absent values are omitted rather than
/// written as `null` — Tantivy would reject a null for a typed field.
pub fn doc_to_json(doc: &Doc, include_thinking: bool) -> Value {
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
    if include_thinking && let Some(t) = doc.thinking.as_deref().filter(|s| !s.is_empty()) {
        o.insert("thinking".to_string(), json!(t));
    }
    if let Some(ts) = doc.timestamp_ms.and_then(rfc3339) {
        o.insert("timestamp".to_string(), json!(ts));
    }
    o.insert("seq".to_string(), json!(doc.seq));
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
    use tantivy::TantivyDocument;
    use tantivy::schema::Value as _;

    fn sample() -> Doc {
        Doc {
            doc_id: "sess:-:7".into(),
            kind: DocKind::ToolCall,
            source_path: "/tmp/sess.jsonl".into(),
            seq: 7,
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
            thinking: Some("hmm".into()),
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
            "body",
            "text",
            "code",
            "headings",
            "code_lang",
            "thinking",
            "timestamp",
            "seq",
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
    fn doc_json_round_trips_through_parse_json() {
        let (schema, f) = build_schema();
        let value = doc_to_json(&sample(), true);
        let doc = TantivyDocument::parse_json(&schema, &value.to_string())
            .expect("tantivy must accept our json");

        let first = |field| doc.get_first(field).unwrap().as_str().unwrap().to_string();
        assert_eq!(first(f.doc_id), "sess:-:7");
        assert_eq!(first(f.kind), "tool_call");
        assert_eq!(first(f.tool_name), "Bash");
        assert_eq!(doc.get_first(f.seq).unwrap().as_u64(), Some(7));
        assert_eq!(doc.get_first(f.is_error).unwrap().as_u64(), Some(1));
        assert!(doc.get_first(f.timestamp).unwrap().as_datetime().is_some());
        assert!(doc.get_first(f.project_facet).unwrap().as_facet().is_some());
        assert!(doc.get_first(f.tool_input).is_some());
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
        let value = doc_to_json(&d, false);
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
        let value = doc_to_json(&d, false);
        assert!(value.get("text").is_none());
        assert!(value.get("code").is_none());
        assert!(value.get("headings").is_none());
        assert!(value.get("code_lang").is_none());
        TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();
    }

    #[test]
    fn thinking_is_omitted_unless_opted_in() {
        let (schema, f) = build_schema();
        let value = doc_to_json(&sample(), false);
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
        d.model = None;
        let value = doc_to_json(&d, true);
        assert!(value.get("timestamp").is_none());
        assert!(value.get("project_facet").is_none());
        assert!(value.get("tool_input").is_none());
        TantivyDocument::parse_json(&schema, &value.to_string()).unwrap();
    }

    #[test]
    fn non_object_tool_input_is_wrapped() {
        let (schema, _) = build_schema();
        let mut d = sample();
        d.tool_input = Some(json!("a bare string"));
        let value = doc_to_json(&d, false);
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
            let json = doc_to_json(doc, false).to_string();
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

    #[test]
    fn facet_paths_always_start_with_a_slash() {
        assert_eq!(facet_path("/a/b").as_deref(), Some("/a/b"));
        assert_eq!(facet_path("C:relative").as_deref(), Some("/C:relative"));
        assert_eq!(facet_path("/a/b/").as_deref(), Some("/a/b"));
        assert!(facet_path("/").is_none());
        assert!(facet_path("").is_none());
    }
}
