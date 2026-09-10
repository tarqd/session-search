//! `aggregate` — terms aggregation over one field.
//!
//! # What to build on
//!
//! * [`crate::search::facets`], which takes the field name and a [`crate::search::SearchRequest`]
//!   carrying the query and filters. It validates the field up front, so an unknown name or a
//!   JSON subpath under a non-JSON field is already a clear error rather than an empty result —
//!   pass that error through as-is. Set `facet_top` from `top`, and set `limit` to `top.max(1)`
//!   the way `cli.rs` does: the collector needs a non-zero limit even though no hits are read.
//!   `top` itself must be at least 1 by the time it gets there — see [`buckets_asked_for`].
//! * [`crate::search::FacetResult`] travels to the caller **whole**. Issue #28 is explicit:
//!   *"Must return `matching_docs`, `docs_with_value`, `other_docs` and `distinct`, not a bare
//!   bucket list: a model handed only buckets will sum them and report a wrong total with total
//!   confidence."* Do not flatten it, do not drop `distinct` because it is approximate, and do
//!   not compute a total from the buckets anywhere.
//! * [`crate::search::FacetResult::is_search_shaped`] and
//!   [`crate::search::FacetResult::hidden_values`] fill the two derived fields. They are already
//!   written and tuned; call them rather than re-deriving the thresholds.
//!
//! # The zero here is a different zero
//!
//! `matching_docs == 0` means the query and filters matched nothing — the ordinary zero, and the
//! envelope handles it. `matching_docs > 0` with no buckets means something else entirely: the
//! documents exist and none of them carries a value for this field, which is a fact about the
//! field, not about the filters. Feed the envelope `facet.matching_docs`, not
//! `facet.values.len()`, or a `tool_input.file_path` aggregation over a thousand Bash calls will
//! come back suggesting the caller drop a filter that was working perfectly.

use std::time::Instant;

use serde_json::json;
use tantivy::schema::{Schema, Type};

use crate::mcp::State;
use crate::mcp::envelope::{self, Context};
use crate::mcp::types::{AggregateRequest, AggregateResponse};
use crate::search::{self, FacetResult, SearchRequest, SortBy};

/// Count one field's values across the matching set.
///
/// Errors are for requests that could not be answered at all — a date that will not parse, a
/// field this index cannot count. "Nothing matched" is not one of them: it is a successful
/// response whose envelope carries the zero, the applied filters and the retry.
pub fn run(state: &State, req: AggregateRequest) -> anyhow::Result<AggregateResponse> {
    let started = Instant::now();

    // First, before the index is touched, so a bad date is `invalid_params` naming `since` or
    // `until` rather than an `anyhow` chain surfacing from inside the search.
    let time_range = envelope::resolve_time_range(&req.filters)?;

    // Second, and still before the searcher opens: an unknown field costs nothing to catch here
    // and is expensive to catch late, where the only report available is an opaque one.
    //
    // The empty spelling gets its own sentence ahead of that. The schema declares `field`
    // required, so a validating client cannot reach here — but a client that sends
    // `{"field": ""}` explicitly satisfies the schema and would otherwise be told
    // `unknown field ""`, which reads as a claim about a field rather than as "you left it out".
    if req.field.trim().is_empty() {
        return Err(crate::mcp::caller_error(
            "`field` is required — name the field whose values you want counted, such as \
             `tool_name`, `project` or `session_id`, or a JSON path such as \
             `tool_input.file_path`",
        ));
    }
    facetable(&state.index.schema(), &req.field)?;

    // Third: how many buckets were asked for, which for `top: 0` is a question with no honest
    // answer. See `buckets_asked_for`.
    let top = buckets_asked_for(req.top)?;

    let request = SearchRequest {
        query: req.query.clone(),
        filters: req.filters.clone(),
        // No hit is ever read out of this — `facets` collects counts, not documents — but the
        // top-hits collector still has to be built, and a zero limit builds a collector that
        // refuses to collect. `cli.rs` passes `top.max(1)` for the same reason; the two callers
        // agreeing is what keeps `--top 0` from meaning different things on the two surfaces.
        limit: top,
        offset: 0,
        // The single field goes to `facets` as an argument, not in here: `SearchRequest::facets`
        // is what `search()` reads, and `facets()` ignores it.
        facets: Vec::new(),
        // The same number the limit above is built from, and it has to be: `search::facets`
        // feeds `facet_top` to the collector, which floors it at 1, *and* to the take that
        // builds the buckets. Two different numbers there means documents collected into a
        // bucket that is never reported and never counted in `other_docs` either.
        facet_top: top,
        // Nothing renders a snippet on this path, and asking for one would cost a stored-field
        // fetch and a highlighter pass per hit for text no caller will ever see.
        snippet_chars: 0,
        include_thinking: req.include_thinking,
        // Ordering decides which documents come back, and none do. Left at the default rather
        // than exposed, so `aggregate` cannot be handed a `sort` that silently means nothing.
        sort: SortBy::Relevance,
        similar_to: None,
        // Turn grouping collapses *hits*, never counts. Switching it on here would change
        // nothing in the buckets and cost a collapse pass per hit.
        group_by_turn: false,
    };

    let facet = search::facets(&state.index, &state.fields, &req.field, &request)?;

    // `matching_docs`, never `values.len()`. See the module note: they answer different
    // questions and only one of them is what "did anything match" means.
    let total = usize::try_from(facet.matching_docs).unwrap_or(usize::MAX);

    let ctx = Context {
        tool: "aggregate",
        query: req.query.as_deref(),
        filters: &req.filters,
        time_range,
        // Without this the retry object is `{...filters}` with no field in it, which is not a
        // call anybody can send: `field` is required and has no default.
        extra: vec![("field", json!(req.field))],
        corpus: &state.corpus,
    };
    let envelope = envelope::build(&ctx, total, warnings(&facet));

    Ok(AggregateResponse {
        // Whole. Every count on it is load-bearing and the response has no second copy of any
        // of them to disagree with.
        search_shaped: facet.is_search_shaped(),
        hidden_values: facet.hidden_values(),
        facet,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        envelope,
    })
}

/// How many buckets the request asked for, or the refusal for a request that asked for none.
///
/// `top: 0` is rejected rather than clamped, and the reason is that it cannot be answered
/// honestly here. [`crate::search::facets`] passes one number to two places: the aggregation
/// collector, which floors it at 1, and the take that builds the bucket list. `other_docs` is the
/// collector's `sum_other_doc_count` — everything *outside* the buckets it collected — so with a
/// take of 0 the top bucket's documents were collected, dropped from `values`, and excluded from
/// `other_docs` as well. `aggregate {field: "role", top: 0}` returned
/// `{values: [], other_docs: 4, docs_with_value: 9}`: five documents in no bucket and in no
/// remainder, against a `top` whose own description promises that "whatever falls outside them is
/// counted in `other_docs`, so a small `top` is honest rather than lossy".
///
/// Clamping 0 to 1 would restore that arithmetic, and it is the other reasonable fix. It is not
/// the one taken because it answers a different question than the one asked, silently: the caller
/// sees one bucket where it asked for none, with nothing in the response to say a floor was
/// applied. Nor is `top: 0` the counts-only request it looks like — `matching_docs`,
/// `docs_with_value` and `distinct` all come back with `top: 1` unchanged, since only the bucket
/// list is truncated by `top`. So there is no question it is the right spelling of, and the
/// refusal names the one that is.
///
/// This is not [`crate::mcp::types::SearchTurnsRequest`]'s `limit: 0`, which `search_turns`
/// answers with a warning: there the count is a complete answer produced by the same pass, and
/// nothing is silently lost by returning no hits.
fn buckets_asked_for(top: usize) -> Result<usize, crate::mcp::CallerError> {
    if top == 0 {
        return Err(crate::mcp::CallerError(
            "`top` is 0, so no bucket could be returned and the documents in the top bucket \
             would be counted in neither `values` nor `other_docs` — a total that silently loses \
             them. Send `top: 1` or more. If the buckets are not what you want, note that \
             `matching_docs`, `docs_with_value` and `distinct` are unaffected by `top`: `top: 1` \
             answers 'how many documents carry a value for this field' exactly as `top: 0` would \
             have."
                .to_string(),
        ));
    }
    Ok(top)
}

/// What the counts are true about but do not say.
///
/// `search::facets` returns no warnings of its own — the three `SearchResponse` warnings are
/// raised inside `search()` and none of them is reachable from a counting pass — so this is the
/// whole of the list rather than a filter over one.
///
/// The single entry covers the outcome most easily misread as an empty answer: documents
/// matched, and not one of them carries this field. The buckets are then empty for a reason that
/// has nothing to do with the filters, and the envelope stays silent because the total is not
/// zero — correct, and quiet enough that a reader can take `values: []` for "nothing matched"
/// and drop a filter that was working. Saying it costs one sentence.
fn warnings(facet: &FacetResult) -> Vec<String> {
    if facet.matching_docs > 0 && facet.docs_with_value == 0 {
        return vec![format!(
            "{} documents matched, and none of them carries a value for `{}` — the empty bucket \
             list is a fact about the field, not about the filters. Widening the filters will \
             not fill it; a different field, or `search_turns`, might.",
            facet.matching_docs, facet.field
        )];
    }
    Vec::new()
}

/// Can this field be counted at all, and if not, what could have been?
///
/// `search::facets` checks the same three things — the field exists, it is fast, and a subpath
/// is only asked of a JSON field — and reports each as a bare `anyhow` naming the field and
/// nothing else. That is enough for a person, who can open the schema. It is not enough for a
/// model, whose next move is another tool call: "unknown facet field" ends the thread, while the
/// same sentence carrying the countable names turns it into the call it should have made. The
/// list is the reason this exists at all; without it the pre-check would only be changing the
/// error's class.
///
/// The names come out of the live schema rather than a literal list, so a field added or renamed
/// in `schema.rs` cannot leave a stale menu behind here. Three kinds of fast field are held back
/// from it, each because naming it would send the caller somewhere useless:
///
/// * the JSON roots (`tool_input`, `bash_cmd`) — a terms aggregation on the root of a JSON field
///   buckets nothing at all, and returns zero values beside a non-zero `docs_with_value`, which
///   reads as a bug. They appear in the sentence about subpaths instead, which is the form that
///   works;
/// * the date field — every document carries a distinct instant, so counting them returns one
///   bucket per document. Time is a filter here, `since`/`until`, not a facet;
/// * the facet field (`project_facet`) — a hierarchical duplicate of `project` whose values come
///   back with their path separators in them. `project` answers the same question readably.
///
/// Subpaths are not enumerable and are not enumerated: `tool_input.<anything a tool actually
/// wrote>` is countable without being declared anywhere, which is the property that makes
/// "which files failed to read" answerable at all. The message says so rather than implying the
/// list is exhaustive.
///
/// # Why this is a [`crate::mcp::CallerError`]
///
/// [`crate::search::facets`] refuses the same three cases, but as a bare `anyhow`, and
/// `crate::mcp::from_anyhow` turns anything it cannot classify into an `internal_error` — a
/// protocol-level failure that most clients render as "the tool broke". A misspelled field name
/// is not the tool breaking, and the difference is not cosmetic: an `internal_error` tells the
/// caller to stop, an `invalid_params` tells it to send a different field.
fn facetable(schema: &Schema, name: &str) -> Result<(), crate::mcp::CallerError> {
    let base = name.split('.').next().unwrap_or(name);
    let Ok(field) = schema.get_field(base) else {
        return Err(crate::mcp::CallerError(format!(
            "unknown field {name:?}. {}",
            what_is_countable(schema)
        )));
    };
    let entry = schema.get_field_entry(field);
    if !entry.field_type().is_fast() {
        return Err(crate::mcp::CallerError(format!(
            "field {name:?} is indexed but not a fast field, so its values cannot be counted; \
             search it with `search_turns` instead. {}",
            what_is_countable(schema)
        )));
    }
    if base != name && !entry.field_type().is_json() {
        return Err(crate::mcp::CallerError(format!(
            "field {name:?} asks for a subpath of {base:?}, but {base:?} is not a JSON field, so \
             it has no subpaths. {}",
            what_is_countable(schema)
        )));
    }
    Ok(())
}

/// The sentence every rejection ends with: what this index would have accepted.
fn what_is_countable(schema: &Schema) -> String {
    let mut fields: Vec<&str> = Vec::new();
    let mut json_roots: Vec<&str> = Vec::new();
    for (_, entry) in schema.fields() {
        if !entry.field_type().is_fast() {
            continue;
        }
        match entry.field_type().value_type() {
            Type::Json => json_roots.push(entry.name()),
            Type::Date | Type::Facet => {}
            _ => fields.push(entry.name()),
        }
    }
    let subpaths = json_roots
        .iter()
        .map(|root| format!("`{root}.<subpath>`"))
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        "Countable here: {}. Also any JSON subpath — {} — such as `tool_input.file_path` or \
         `bash_cmd.program`; subpaths are dynamic, so any parameter a tool actually wrote is \
         countable without being declared anywhere.",
        fields
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", "),
        subpaths
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexOptions;
    use crate::mcp::types::Corpus;
    use crate::search::Filters;
    use std::path::{Path, PathBuf};

    /// A real on-disk index over `tests/fixtures/eval/eval-facets.jsonl`.
    ///
    /// On disk rather than in RAM, and through `index::run` rather than a hand-built document,
    /// because the properties under test are properties of the indexing: `code_lang` is
    /// multi-valued only because the fence extractor writes it twice, and
    /// `tool_input.file_path` is countable only because the JSON field was indexed with fast
    /// subpaths. A synthetic document would pin this file's arithmetic against itself.
    ///
    /// This fixture over `real_main_slice.jsonl`: it is the one that carries a message with two
    /// fenced languages, which is the multi-valued case, and its `tool_input.file_path` values
    /// include a read that failed — the question the dynamic-subpath path exists to answer.
    struct Fixture {
        _tmp: tempfile::TempDir,
        state: State,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("claude/projects");
        // Discovery reads the session id off the filename, so the copy keeps the fixture's own
        // name; the directory is the project the transcript says it ran in.
        let project = root.join("-home-user-code-other-tool");
        std::fs::create_dir_all(&project).unwrap();
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval/eval-facets.jsonl");
        std::fs::copy(&source, project.join("eval-facets.jsonl")).unwrap();

        let index_dir = tmp.path().join("index");
        let roots: Vec<PathBuf> = vec![root];
        let stats = crate::index::run(&index_dir, &roots, &IndexOptions::default()).unwrap();
        // Guards every assertion below: a fixture that stopped parsing would make each of them
        // pass against an empty index and prove nothing.
        assert!(stats.docs_added > 5, "fixture indexed {stats:?}");

        let (index, fields) = crate::index::open_or_create(&index_dir).unwrap();
        Fixture {
            _tmp: tmp,
            state: State {
                index,
                fields,
                index_dir,
                corpus: Corpus::default(),
                refresh_secs: 0,
                last_refresh: std::sync::Mutex::new(None),
            },
        }
    }

    fn request(field: &str, mutate: impl FnOnce(&mut AggregateRequest)) -> AggregateRequest {
        let mut req = AggregateRequest {
            field: field.to_string(),
            ..AggregateRequest::default()
        };
        mutate(&mut req);
        req
    }

    #[test]
    fn every_count_survives_to_the_response_and_the_buckets_do_not_sum_to_the_total() {
        let fx = fixture();
        // `top` below the number of distinct values, so all four counts are distinct numbers and
        // a response that dropped one cannot pass by coincidence.
        let out = run(&fx.state, request("tool_name", |r| r.top = 2)).unwrap();

        assert_eq!(out.facet.field, "tool_name");
        assert_eq!(out.facet.values.len(), 2, "{:?}", out.facet.values);
        assert_eq!(out.facet.matching_docs, 13);
        assert_eq!(out.facet.docs_with_value, 9);
        assert_eq!(out.facet.other_docs, 3);
        assert_eq!(out.facet.distinct, Some(4));

        // The failure issue #28 names: the buckets are a truncated view and summing them is
        // wrong by more than rounding. 4 + 2 is not 13, and only `matching_docs` is the total.
        let summed: u64 = out.facet.values.iter().map(|v| v.count).sum();
        assert!(summed < out.facet.matching_docs, "{summed}");
        assert!(out.envelope.no_results.is_none());
    }

    #[test]
    fn asking_for_zero_buckets_is_refused_rather_than_answered_with_a_lossy_total() {
        let fx = fixture();
        // `top: 0` used to reach `search::facets` as one number read two ways: the collector
        // floored it at 1 and collected a top bucket, the bucket list took 0 of it, and
        // `other_docs` — `sum_other_doc_count`, everything outside what the collector collected —
        // excluded it too. `aggregate {field: "role", top: 0}` came back
        // `{values: [], other_docs: 4, docs_with_value: 9}`: five documents in no bucket and in
        // no remainder, under a `top` whose description promises the opposite.
        let err = run(&fx.state, request("role", |r| r.top = 0)).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            err.downcast_ref::<crate::mcp::CallerError>().is_some(),
            "asking for no buckets is a malformed question, not a broken server: {message}"
        );
        assert!(message.contains("other_docs"), "{message}");
        assert!(message.contains("top: 1"), "{message}");

        // And the answer `top: 0` looked like it was asking for is `top: 1`, which loses nothing:
        // one bucket, and every document either in it or in `other_docs`.
        let out = run(&fx.state, request("role", |r| r.top = 1)).unwrap();
        assert_eq!(out.facet.values.len(), 1, "{:?}", out.facet.values);
        let bucketed: u64 = out.facet.values.iter().map(|v| v.count).sum();
        assert_eq!(
            bucketed + out.facet.other_docs,
            out.facet.docs_with_value,
            "every document carrying a value is in a bucket or in the remainder: {:?}",
            out.facet
        );
    }

    #[test]
    fn the_bucket_list_and_the_remainder_are_cut_by_the_same_number() {
        let fx = fixture();
        // The invariant the `top: 0` defect broke, stated over every `top` a caller can send:
        // `search::facets` hands one number to the collector and to the bucket take, so the two
        // must be the same number or documents fall out of both `values` and `other_docs`.
        // `role` is single-valued, which is what makes the sum a document count.
        for top in 1..=5 {
            let out = run(&fx.state, request("role", |r| r.top = top)).unwrap();
            let bucketed: u64 = out.facet.values.iter().map(|v| v.count).sum();
            assert_eq!(
                bucketed + out.facet.other_docs,
                out.facet.docs_with_value,
                "top={top} lost documents from every count: {:?}",
                out.facet
            );
        }
    }

    #[test]
    fn the_derived_hints_are_the_types_own_judgement_and_never_a_second_threshold() {
        let fx = fixture();
        let out = run(&fx.state, request("tool_name", |r| r.top = 2)).unwrap();
        // Pinned as delegation rather than as values: a copy of the 0.8 ratio or of
        // `distinct - shown` in this file would be a second definition, and the two would drift
        // the first time either is tuned.
        assert_eq!(out.search_shaped, out.facet.is_search_shaped());
        assert_eq!(out.hidden_values, out.facet.hidden_values());
        assert_eq!(out.hidden_values, Some(2));
    }

    #[test]
    fn an_unknown_field_is_refused_with_a_message_naming_what_this_index_can_count() {
        let fx = fixture();
        let err = run(&fx.state, request("toolname", |_| {})).unwrap_err();
        let message = format!("{err:#}");

        // The class matters as much as the text: `internal_error` tells a caller to stop,
        // `invalid_params` tells it to send a different field, and this type is what carries
        // that distinction to `from_anyhow`.
        assert!(
            err.downcast_ref::<crate::mcp::CallerError>().is_some(),
            "{message}"
        );
        assert!(message.contains("toolname"), "{message}");
        // Refused, not answered with an empty bucket list — the outcome that would read as
        // "this index has no tool names in it".
        assert!(message.contains("tool_name"), "{message}");
        assert!(message.contains("project"), "{message}");
        assert!(message.contains("code_lang"), "{message}");
        // And the half of the accepted set that no list could enumerate.
        assert!(message.contains("tool_input.<subpath>"), "{message}");
        assert!(message.contains("dynamic"), "{message}");
        // The held-back fast fields: naming them would send the caller somewhere useless.
        assert!(!message.contains("`timestamp`"), "{message}");
        assert!(!message.contains("project_facet"), "{message}");
    }

    #[test]
    fn a_field_that_is_stored_but_not_fast_says_to_search_it_instead() {
        let fx = fixture();
        let err = run(&fx.state, request("body", |_| {})).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            err.downcast_ref::<crate::mcp::CallerError>().is_some(),
            "{message}"
        );
        assert!(message.contains("search_turns"), "{message}");
    }

    #[test]
    fn a_subpath_of_a_field_that_holds_no_json_is_refused_before_the_searcher_opens() {
        let fx = fixture();
        let err = run(&fx.state, request("tool_name.file_path", |_| {})).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            err.downcast_ref::<crate::mcp::CallerError>().is_some(),
            "{message}"
        );
        assert!(message.contains("not a JSON field"), "{message}");
    }

    #[test]
    fn a_tool_input_subpath_is_counted_without_being_declared_anywhere() {
        let fx = fixture();
        // `file_path` appears in no schema and in no allowlist. It is countable because the
        // Read calls in the transcript wrote it, which is the whole point of the dynamic half.
        let out = run(&fx.state, request("tool_input.file_path", |_| {})).unwrap();

        assert_eq!(out.facet.docs_with_value, 3);
        assert_eq!(out.facet.values.len(), 3, "{:?}", out.facet.values);
        assert!(
            out.facet
                .values
                .iter()
                .any(|v| v.value.ends_with("logs/missing.txt")),
            "{:?}",
            out.facet.values
        );
        assert!(out.envelope.no_results.is_none());
    }

    #[test]
    fn a_multi_valued_field_buckets_more_values_than_there_are_documents() {
        let fx = fixture();
        // One answer quotes a bash fence and a python fence, so it is one document and two
        // values. Narrowed to that document so the arithmetic is unambiguous: two buckets over
        // one matching document.
        let out = run(
            &fx.state,
            request("code_lang", |r| r.filters.lang = vec!["bash".into()]),
        )
        .unwrap();

        assert_eq!(out.facet.matching_docs, 1);
        let summed: u64 = out.facet.values.iter().map(|v| v.count).sum();
        assert_eq!(summed, 2, "{:?}", out.facet.values);
        assert!(summed > out.facet.matching_docs);
        // The count that stays a count of documents, which is why the response carries both.
        assert_eq!(out.facet.docs_with_value, 1);
    }

    #[test]
    fn matching_documents_that_carry_no_value_is_an_answer_and_not_a_retry() {
        let fx = fixture();
        // Four messages matched; a message has no tool input, so no bucket has anything in it.
        // The regression this pins: feeding `values.len()` to the envelope, which would answer
        // a correct "none of these documents carry that field" with "drop your `kind` filter".
        let out = run(
            &fx.state,
            request("tool_input.file_path", |r| {
                r.filters.kind = Some("message".into())
            }),
        )
        .unwrap();

        assert_eq!(out.facet.matching_docs, 4);
        assert!(out.facet.values.is_empty(), "{:?}", out.facet.values);
        assert!(out.envelope.no_results.is_none(), "{:?}", out.envelope);
        // Silent otherwise: the total is not zero, so nothing else in the response says why the
        // buckets are empty.
        assert!(
            out.envelope
                .warnings
                .iter()
                .any(|w| w.contains("none of them carries a value")),
            "{:?}",
            out.envelope.warnings
        );
    }

    #[test]
    fn zero_matching_documents_gets_the_envelopes_retry_carrying_the_field_back() {
        let fx = fixture();
        let out = run(
            &fx.state,
            request("tool_name", |r| r.filters.tool = vec!["NoSuchTool".into()]),
        )
        .unwrap();

        assert_eq!(out.facet.matching_docs, 0);
        let no_results = out.envelope.no_results.expect("a zero never travels alone");
        assert_eq!(no_results.narrowest_filter.as_deref(), Some("tool"));
        // The retry has to be sendable, and `field` is required with no default: without it the
        // suggestion is an `aggregate` call that cannot be made.
        assert_eq!(no_results.retry["field"], json!("tool_name"));
    }

    #[test]
    fn a_date_that_will_not_parse_is_the_callers_mistake_and_never_reaches_the_index() {
        let fx = fixture();
        let err = run(
            &fx.state,
            request("tool_name", |r| {
                r.filters.since = Some("last tuesdayish".into())
            }),
        )
        .unwrap_err();
        // `FilterError` unchanged, which is what `from_anyhow` downcasts to `invalid_params`,
        // and it names the plain field rather than a CLI flag that does not exist here.
        let filter_error = err
            .downcast_ref::<crate::sessions::FilterError>()
            .expect("a bad date is a filter error");
        assert_eq!(filter_error.field, "since");
    }

    #[test]
    fn the_applied_filters_are_echoed_beside_a_non_empty_answer_too() {
        let fx = fixture();
        let out = run(
            &fx.state,
            request("tool_name", |r| {
                r.query = Some("log".into());
                r.filters = Filters {
                    kind: Some("tool_call".into()),
                    ..Filters::default()
                };
            }),
        )
        .unwrap();
        assert!(
            out.envelope
                .applied_filters
                .iter()
                .any(|f| f.name == "kind"),
            "{:?}",
            out.envelope.applied_filters
        );
    }
}
