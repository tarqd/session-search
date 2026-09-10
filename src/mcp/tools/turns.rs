//! `search_turns` — full-text search answered as turn skeletons.
//!
//! The retrieval tool of this surface: the one a caller reaches for when the answer is a passage
//! somebody wrote or a command somebody ran, rather than a distribution (`aggregate`) or a list
//! of conversations (`search_sessions`). It is also the only tool here whose unit of answer is a
//! *turn* rather than a document, and every decision below follows from that.
//!
//! # Grouping is not a knob
//!
//! [`crate::search::SearchRequest::group_by_turn`] is `true` on every call this module makes.
//! A message-level query for "why did the build fail" matches the prompt, the assistant text,
//! the tool call and its result — four hits describing one moment, and a fifth of a twenty-hit
//! page spent on it. Exposing the flag would let a caller ask for that page by accident and get
//! back a screen of the same turn under four names, which is exactly what a caller reading tool
//! results has no way to notice.
//!
//! Grouping changes two things a caller has to be told about, and both are carried through:
//! `limit` and `offset` count **turns**, so `total_documents` — the size of the match set — is
//! larger than `returned` whenever a turn matched in more than one document, and
//! [`crate::search::Hit::collapsed`] is how many *other* documents of the same turn matched.
//!
//! # The skeleton replaces the documents
//!
//! A hit carries [`crate::format::turn_skeleton`]'s rendering of its whole turn and none of that
//! turn's text. No [`crate::format::doc_json`], not even for the anchor document: the anchor is
//! where `body` and `tool_output` live, so emitting it would put back precisely the bytes the
//! skeleton exists to avoid, and the response would be *larger* than an ungrouped search while
//! claiming to be the economical one. `docs/DESIGN.md` ("Turn skeletons") pins the replacement;
//! the measurement behind it is that a turn's full context averages 5,651 bytes and its skeleton
//! 555.
//!
//! Reading the turn in full is `get_turn`, and reading one call's output in full is
//! `get_output`. Every hit carries the `(source_path, turn_seq)` pair that addresses both.
//!
//! # Both caps, always
//!
//! A turn-shaped answer has two independent ways of being incomplete, and a response that
//! reports one of them is worse than one that reports neither, because it reads as complete:
//!
//! * the **document** cap — [`crate::context::TurnWindow::total`] against `docs.len()`, which
//!   bites when a turn is longer than [`SKELETON_WINDOW_DOCS`];
//! * the **byte** cap — [`crate::format::Skeleton::dropped`], which bites when the rendered
//!   lines exceed [`crate::format::SKELETON_BUDGET`].
//!
//! Both have a typed home on [`TurnSkeleton`]: the byte cap in `dropped`, the document cap in
//! the `shown` / `docs_in_turn` / `truncated` triple — the same shape `get_turn` reports it in
//! and the one [`crate::format::turn_json`] writes for the CLI, so a reader who has seen one
//! knows how to read the other.
//!
//! # Cost
//!
//! One [`crate::context::turn_window`] per hit is one extra searcher pass per returned turn.
//! That is the price of a skeleton and it is the right trade — the page it replaces would carry
//! ten times its bytes. The window is fetched once and reused for both caps and the rendering;
//! `context::turn` would be a second pass over the same query for a strictly smaller answer.

use crate::mcp::State;
use crate::mcp::envelope::{self, Context};
use crate::mcp::types::{SearchTurnsRequest, SearchTurnsResponse, TurnHit, TurnRef, TurnSkeleton};
use crate::parse::Doc;
use crate::search::{SearchRequest, SearchResponse};
use crate::{context, format, search};

/// Documents of one turn a skeleton may be built from, per hit.
///
/// A turn is not a bounded thing: one prompt can open hundreds of tool calls, and a sidechain
/// transcript is a single turn covering an entire subagent run. `TopDocs` preallocates whatever
/// it is handed, so an uncapped window is a process abort rather than a long scroll — and a page
/// of twenty hits would pay for it twenty times over. Matching `cli.rs`'s `--context turn` cap
/// rather than inventing a second number: the two fetch the same window for the same reason.
///
/// It is deliberately far above the point where [`crate::format::SKELETON_BUDGET`] starts
/// dropping lines, because the two caps answer different questions. The byte cap decides how
/// much of the turn is *rendered* and is the one that normally bites; this one exists so that a
/// pathological turn cannot make the fetch itself expensive. Lowering it to "about where the
/// budget runs out" would make the document cap bite on ordinary turns, where it says nothing
/// the byte cap has not already said better.
const SKELETON_WINDOW_DOCS: usize = 200;

/// Search, group by turn, and return one skeleton per turn.
///
/// Errors are for requests that could not be answered at all. "Nothing matched" is not one: it
/// is a successful response whose envelope carries the zero, the applied filters and the retry.
pub fn run(state: &State, req: SearchTurnsRequest) -> anyhow::Result<SearchTurnsResponse> {
    // First, and before the index is touched. A `since` of `last tuesdayy` is then an
    // `invalid_params` naming `since`, rather than an `anyhow` surfacing from inside `search()`
    // where a caller cannot tell a bad date from an empty corpus — and the absolute window the
    // envelope echoes back does not exist until somebody has resolved it.
    let time_range = envelope::resolve_time_range(&req.filters)?;
    // Second, and still before the index is opened: half a turn address narrows nothing, so a
    // request carrying one would search the whole corpus and report the half back as applied.
    super::check_turn_address(&req.filters)?;

    let request = SearchRequest {
        query: req.query.clone(),
        filters: req.filters.clone(),
        limit: req.limit,
        offset: req.offset,
        // Counting values is `aggregate`'s job and it rides in the same searcher pass there.
        // Asking for facets here would pay for an aggregation nothing in this response has a
        // field to carry.
        facets: Vec::new(),
        // Unread while `facets` is empty, and given a value anyway rather than left to a
        // default nobody can see: `search()` only consults it once per requested facet field.
        facet_top: 0,
        snippet_chars: req.snippet_chars,
        include_thinking: req.include_thinking,
        sort: req.sort,
        // "Find similar" is a CLI affordance that resolves a reference to a seed document before
        // building the request; there is no MCP tool that takes one.
        similar_to: None,
        // The whole shape of this tool. See the module docs.
        group_by_turn: true,
        // Every field spelled out rather than finished with `..Default::default()`, so that
        // adding one to `SearchRequest` is a compile error somebody has to decide about instead
        // of a default this tool silently inherits. `src/api/dto.rs` does the same, for the same
        // reason, and both are load-bearing: `group_by_turn` itself defaults to `false`.
    };
    let SearchResponse {
        hits,
        total,
        elapsed_ms,
        mut warnings,
        hidden,
        ..
    } = search::search(&state.index, &state.fields, &request)?;

    let mut turns = Vec::with_capacity(hits.len());
    for hit in hits {
        // Built before the document is consumed, and per hit: a turn whose window cannot be read
        // degrades to a skeleton-less hit rather than taking the page down with it. The
        // remaining nineteen turns are still the answer to the question that was asked.
        let skeleton = skeleton_for(state, &hit.doc, &mut warnings);
        let doc = hit.doc;
        turns.push(TurnHit {
            turn: TurnRef {
                source_path: doc.source_path,
                turn_seq: doc.turn_seq,
            },
            doc_id: doc.doc_id,
            session_id: doc.session_id,
            agent_id: doc.agent_id,
            agent_type: doc.agent_type,
            project: doc.project,
            git_branch: doc.git_branch,
            timestamp: doc.timestamp_ms.and_then(rfc3339),
            score: hit.score,
            snippet: hit.snippet,
            snippet_field: hit.snippet_field,
            collapsed: hit.collapsed,
            skeleton,
        });
    }

    if turns.is_empty() && total > 0 {
        warnings.push(empty_page_warning(&req, total));
    }

    let ctx = Context {
        tool: "search_turns",
        query: req.query.as_deref(),
        filters: &req.filters,
        time_range,
        extra: Vec::new(),
        corpus: &state.corpus,
    };
    // The *document* count, not `turns.len()`, because that is what `no_results` is about: the
    // retry it builds drops the narrowest filter, and that is advice about an empty match set. A
    // page that came back empty because `offset` ran past the last turn matched plenty, and
    // telling that caller to widen a search which already worked sends them the wrong way — so
    // that case is a warning instead, above.
    let envelope = envelope::build(&ctx, total, warnings);
    Ok(SearchTurnsResponse {
        returned: turns.len(),
        turns,
        total_documents: total,
        // Carried through rather than recomputed. `search()` counts the refused set with one
        // `Count` over the complement of the scope it applied, so a second opinion assembled
        // here could disagree with the answer it is attached to — and the number's whole job is
        // to be trusted when the page above it looks thin.
        hidden,
        // `u128` on the way in because `Instant::elapsed` is; nothing this side of a hung index
        // reaches the saturation, and a wrapping cast would report an hour-long search as fast.
        elapsed_ms: u64::try_from(elapsed_ms).unwrap_or(u64::MAX),
        envelope,
    })
}

/// One hit's turn, fetched once and rendered as a skeleton, with both caps accounted for.
///
/// A failure here is per-hit and never fatal. `cli::context_windows` sets the precedent: a
/// context lookup that fails leaves that one hit without its window and logs it. The difference
/// is that a log reaches nobody over stdio, so the sentence goes into `warnings` as well —
/// otherwise an empty `lines` reads as "this turn had nothing in it", which is a claim about the
/// transcript rather than about the lookup.
fn skeleton_for(state: &State, doc: &Doc, warnings: &mut Vec<String>) -> TurnSkeleton {
    let window = match context::turn_window(
        &state.index,
        &state.fields,
        &doc.source_path,
        doc.turn_seq,
        SKELETON_WINDOW_DOCS,
    ) {
        Ok(window) => window,
        Err(err) => {
            tracing::warn!(
                doc = %doc.doc_id,
                error = %format!("{err:#}"),
                "turn window lookup failed; returning the hit without its skeleton"
            );
            warnings.push(format!(
                "the turn holding {} could not be read, so that hit has no skeleton — an empty \
                 skeleton here means the lookup failed, not that the turn was empty: {err:#}",
                doc.doc_id
            ));
            return TurnSkeleton {
                lines: Vec::new(),
                dropped: 0,
                bytes: 0,
                shown: 0,
                docs_in_turn: 0,
                truncated: false,
            };
        }
    };
    let rendered = format::turn_skeleton(&window.docs, format::SKELETON_BUDGET);
    TurnSkeleton {
        // `Skeleton::bytes()` — the rendered size with newlines counted — not
        // `lines.join("\n").len()`, which is short by one per line and is the number a
        // reimplementation gets wrong.
        bytes: rendered.bytes(),
        lines: rendered.lines,
        dropped: rendered.dropped,
        shown: window.docs.len(),
        docs_in_turn: window.total,
        truncated: window.total > window.docs.len(),
    }
}

/// An empty page over a non-empty match set — the one zero the envelope must not explain.
///
/// Two ways to get here, and they need different advice, but they share the hazard: `turns: []`
/// with no `no_results` beside it is the bare empty list issue #28 exists to abolish, and it
/// would be the only answer this tool can give that explains nothing.
fn empty_page_warning(req: &SearchTurnsRequest, total: usize) -> String {
    if req.limit == 0 {
        return format!(
            "limit is 0, so no turns were returned. {total} documents matched — this is a count, \
             not an answer; send the same request with a limit to read them."
        );
    }
    format!(
        "no turns on this page: {total} documents matched, but offset {} is past the last turn \
         they group into. Lower `offset`; the match set itself is not empty.",
        req.offset
    )
}

/// A stored timestamp as RFC3339, matching what `format::doc_json` writes.
///
/// The same instant is quoted by `get_turn`'s documents and by a `search_turns` hit, and a
/// caller comparing the two to decide whether they hold the same moment must not be reading two
/// renderings of one number.
fn rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types::RetryAction;
    use serde_json::Value;
    use std::path::Path as FsPath;
    use std::sync::Arc;

    /// The session id inside `tests/fixtures/real_main_slice.jsonl`. Discovery takes the id from
    /// the *filename*, so a copy has to be named after it or nothing resolves.
    const SESSION: &str = "b20208d8-fbdb-5918-ba69-d203de6ed6dc";
    /// A second transcript, for the tests that need more than one turn to page over.
    const SECOND: &str = "c30308d8-fbdb-5918-ba69-d203de6ed6dc";

    /// A real, on-disk index built from the fixture, and the `State` a tool body reads.
    ///
    /// On disk rather than in RAM because `State` carries the index *directory*: the corpus
    /// counts in a zero-hit message come from `sessions.json`, which an in-memory index does not
    /// have, and a zero that could not name the corpus is half the answer this tool owes.
    struct Fixture {
        _tmp: tempfile::TempDir,
        state: Arc<State>,
    }

    fn fixture(sessions: &[&str], extra_calls: usize) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("claude/projects");
        let project = root.join("-home-user-session-search");
        std::fs::create_dir_all(&project).unwrap();
        let base = std::fs::read_to_string(
            FsPath::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_main_slice.jsonl"),
        )
        .unwrap();
        let transcript = lengthened(&base, extra_calls);
        for session in sessions {
            std::fs::write(project.join(format!("{session}.jsonl")), &transcript).unwrap();
        }

        let index_dir = tmp.path().join("index");
        let stats = crate::index::run(
            &index_dir,
            std::slice::from_ref(&root),
            &crate::index::IndexOptions::default(),
        )
        .unwrap();
        // Guards every assertion below: if the fixture stopped parsing, a search over an empty
        // index would return zero and the zero-hit tests would pass for the wrong reason.
        assert!(stats.docs_added > 5, "fixture indexed {stats:?}");

        let server = crate::mcp::Server::new(&index_dir).unwrap();
        Fixture {
            _tmp: tmp,
            state: server.state,
        }
    }

    /// The fixture with `extra` more tool calls appended to its single turn.
    ///
    /// Real transcript records, cloned from the two the fixture already holds and re-keyed, so
    /// the parser sees the shapes it sees in production. A turn long enough to hit
    /// [`SKELETON_WINDOW_DOCS`] cannot be checked in as a fixture — it is hundreds of records of
    /// nothing — and it is the only way to exercise the document cap at all, since a `tool_result`
    /// record does not open a turn and every appended pair therefore lands in the same one.
    fn lengthened(base: &str, extra: usize) -> String {
        if extra == 0 {
            return base.to_string();
        }
        let records: Vec<Value> = base
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let with_block = |kind: &str| {
            records
                .iter()
                .find(|r| {
                    r["message"]["content"]
                        .as_array()
                        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == kind))
                })
                .expect("the fixture holds one of these")
                .clone()
        };
        let call = with_block("tool_use");
        let result = with_block("tool_result");

        let mut out = base.trim_end().to_string();
        for i in 0..extra {
            let id = format!("toolu_synthetic_{i}");
            let mut call = call.clone();
            call["uuid"] = Value::String(format!("synthetic-call-{i}"));
            for block in call["message"]["content"].as_array_mut().unwrap() {
                if block["type"] == "tool_use" {
                    block["id"] = Value::String(id.clone());
                    block["input"]["command"] = Value::String(format!("echo synthetic {i}"));
                }
            }
            let mut result = result.clone();
            result["uuid"] = Value::String(format!("synthetic-result-{i}"));
            for block in result["message"]["content"].as_array_mut().unwrap() {
                if block["type"] == "tool_result" {
                    block["tool_use_id"] = Value::String(id.clone());
                    block["content"] = Value::String(format!("synthetic output {i}"));
                }
            }
            out.push('\n');
            out.push_str(&serde_json::to_string(&call).unwrap());
            out.push('\n');
            out.push_str(&serde_json::to_string(&result).unwrap());
        }
        out.push('\n');
        out
    }

    fn request(query: &str) -> SearchTurnsRequest {
        SearchTurnsRequest {
            query: Some(query.to_string()),
            ..SearchTurnsRequest::default()
        }
    }

    /// A hit's whole point: the shape of the turn, at a few hundred bytes, and none of the text
    /// the anchor document holds. Emitting `doc_json` beside it — for the anchor alone, even —
    /// puts `body` and `tool_output` back on the wire and makes the skeleton pure overhead.
    #[test]
    fn a_hit_returns_its_turns_skeleton_and_never_the_turns_documents() {
        let fx = fixture(&[SESSION], 0);
        let out = run(&fx.state, request("claude")).unwrap();
        assert_eq!(out.returned, out.turns.len());
        assert!(out.returned > 0, "the fixture matches `claude`");

        let hit = &out.turns[0];
        assert!(!hit.skeleton.lines.is_empty(), "{:?}", hit.skeleton);
        assert_eq!(
            hit.skeleton.bytes,
            hit.skeleton
                .lines
                .iter()
                .map(|l| l.len() + 1)
                .sum::<usize>(),
            "`bytes` is the rendered size, newlines counted"
        );

        let json = serde_json::to_value(hit).unwrap();
        let object = json.as_object().unwrap();
        for key in [
            "body",
            "tool_output",
            "text",
            "code",
            "raw",
            "docs",
            "context",
        ] {
            assert!(
                !object.contains_key(key),
                "a hit carried `{key}`: {}",
                serde_json::to_string_pretty(&json).unwrap()
            );
        }
        // The address to drill with, in the shape `get_turn` accepts.
        assert!(object.contains_key("source_path") && object.contains_key("turn_seq"));
        // And nothing about a page of hits is an occasion for a retry suggestion.
        assert!(out.envelope.no_results.is_none());
    }

    /// The number that separates "the corpus has nothing" from "the scope refused to look".
    ///
    /// A default-scoped search drops attachments, `system` records and meta turns, which on this
    /// fixture is most of the file. Without `hidden` on the response, a caller reading a short
    /// page has no way to tell that a fifth of the index was never consulted — and reports the
    /// absence as a fact about the work rather than about the query.
    #[test]
    fn a_search_reports_how_many_documents_the_default_scope_refused() {
        let fx = fixture(&[SESSION], 0);
        let scoped = run(&fx.state, request("claude")).unwrap();
        assert!(
            scoped.hidden > 0,
            "the fixture's attachment records match `claude`, so some were refused: hidden={}",
            scoped.hidden
        );

        let mut widened = request("claude");
        widened.filters.all_records = true;
        let whole = run(&fx.state, widened).unwrap();
        assert_eq!(
            whole.hidden, 0,
            "nothing was refused, so nothing is reported"
        );
        assert_eq!(
            whole.total_documents,
            scoped.total_documents + scoped.hidden,
            "`hidden` counts exactly what the default scope held back"
        );
    }

    /// Half a turn address narrows nothing, so a request carrying one must not be answered.
    ///
    /// `clap` pairs the two with `requires` and `api::dto` refuses half of one; nothing said it
    /// over MCP, where `search::build_query` reads the pair together and silently ignores a lone
    /// `turn_seq`. The answer that came back was the whole corpus — wider than the one asked
    /// for, and indistinguishable from a search that was narrowed.
    #[test]
    fn half_a_turn_address_is_refused_rather_than_answered_over_the_whole_corpus() {
        let fx = fixture(&[SESSION], 0);
        let whole = run(&fx.state, request("claude")).unwrap();
        let address = whole.turns[0].turn.clone();

        for half in [
            (Some(address.source_path.clone()), None),
            (None, Some(address.turn_seq)),
        ] {
            let mut req = request("claude");
            (req.filters.turn_of, req.filters.turn_seq) = half;
            let err = run(&fx.state, req).expect_err("half an address is not a request");
            assert!(
                err.downcast_ref::<crate::mcp::CallerError>().is_some(),
                "a malformed address is the caller's mistake, not the server's: {err:#}"
            );
            let rendered = format!("{err:#}");
            assert!(
                rendered.contains("turn_of") && rendered.contains("turn_seq"),
                "{rendered}"
            );
        }

        // And the whole pair is a request: it narrows to that one turn rather than being
        // dropped, which is what makes the refusal above a repair and not just a refusal.
        let mut addressed = request("claude");
        addressed.filters.turn_of = Some(address.source_path.clone());
        addressed.filters.turn_seq = Some(address.turn_seq);
        let one = run(&fx.state, addressed).unwrap();
        assert!(one.total_documents > 0, "the turn the hit came from");
        assert!(
            one.total_documents <= whole.total_documents,
            "an address cannot match more than the search it narrows: {} vs {}",
            one.total_documents,
            whole.total_documents
        );
        assert_eq!(one.returned, 1, "one turn, addressed");
        assert_eq!(one.turns[0].turn.turn_seq, address.turn_seq);
    }

    /// Two caps, two channels, and neither one implies the other: the byte budget stops the
    /// rendering mid-turn, the window cap means documents were never fetched to render. A
    /// response reporting one of them reads as complete about the other.
    #[test]
    fn both_caps_are_reported_the_skeletons_byte_cap_and_the_windows_document_cap() {
        let fx = fixture(&[SESSION], 300);
        let out = run(&fx.state, request("claude")).unwrap();
        let hit = &out.turns[0];

        assert!(
            hit.skeleton.dropped > 0,
            "300 calls do not fit in {} bytes: {:?}",
            format::SKELETON_BUDGET,
            hit.skeleton
        );
        // The document cap is a typed triple rather than prose, so a caller reads it the same
        // way here as on `get_turn`, and never has to compare two numbers to notice.
        assert!(hit.skeleton.truncated, "{:?}", hit.skeleton);
        assert_eq!(hit.skeleton.shown, SKELETON_WINDOW_DOCS);
        assert!(
            hit.skeleton.docs_in_turn > hit.skeleton.shown,
            "{:?}",
            hit.skeleton
        );
        // The address to reread the rest with is on the hit itself, so a caller holding twenty
        // of these knows which turn each one refers to.
        assert!(hit.turn.source_path.contains(SESSION), "{:?}", hit.turn);
    }

    /// A skeleton that fits reports itself as whole on both channels. Without this, `truncated`
    /// could be hardcoded true and every assertion above would still pass.
    #[test]
    fn a_turn_that_fits_is_not_reported_as_truncated_on_either_channel() {
        let fx = fixture(&[SESSION], 0);
        let out = run(&fx.state, request("claude")).unwrap();
        let hit = &out.turns[0];

        assert!(!hit.skeleton.truncated, "{:?}", hit.skeleton);
        assert_eq!(hit.skeleton.shown, hit.skeleton.docs_in_turn);
        assert_eq!(hit.skeleton.dropped, 0, "{:?}", hit.skeleton);
    }

    /// Grouping changes what a page *is*. A turn that matched in eleven documents is one turn
    /// here, so a `limit` of 1 returns one turn and `total_documents` still counts eleven —
    /// paging by the document count would skip whole conversations.
    #[test]
    fn limit_and_offset_page_over_turns_not_over_matching_documents() {
        let fx = fixture(&[SESSION, SECOND], 0);

        let all = run(&fx.state, request("claude")).unwrap();
        assert_eq!(all.returned, 2, "one turn per transcript");
        assert!(
            all.total_documents > all.returned,
            "{} documents across {} turns",
            all.total_documents,
            all.returned
        );
        assert!(
            all.turns.iter().any(|t| t.collapsed > 0),
            "a turn matching in several documents reports the others it stands in for"
        );

        let first = run(
            &fx.state,
            SearchTurnsRequest {
                limit: 1,
                ..request("claude")
            },
        )
        .unwrap();
        let second = run(
            &fx.state,
            SearchTurnsRequest {
                limit: 1,
                offset: 1,
                ..request("claude")
            },
        )
        .unwrap();
        assert_eq!((first.returned, second.returned), (1, 1));
        assert_ne!(
            first.turns[0].turn.source_path, second.turns[0].turn.source_path,
            "offset 1 skipped a turn, not a document"
        );
        assert_eq!(
            (first.total_documents, second.total_documents),
            (all.total_documents, all.total_documents),
            "`total_documents` is the match set and does not move with the page"
        );
    }

    /// The failure issue #28 names: a model handed `{turns: []}` reports that nothing happened
    /// last week. Every zero carries what was applied, what the window resolved to, and a body
    /// to send back.
    #[test]
    fn a_zero_hit_search_carries_the_applied_filters_the_resolved_range_and_a_retry() {
        let fx = fixture(&[SESSION], 0);
        let out = run(
            &fx.state,
            SearchTurnsRequest {
                filters: crate::search::Filters {
                    program: vec!["definitely-not-a-program".into()],
                    since: Some("30d".into()),
                    ..crate::search::Filters::default()
                },
                ..request("claude")
            },
        )
        .unwrap();
        assert_eq!((out.returned, out.total_documents), (0, 0));

        let no_results = out.envelope.no_results.expect("a zero explains itself");
        assert_eq!(no_results.narrowest_filter.as_deref(), Some("program"));
        assert_eq!(no_results.action, RetryAction::Drop);
        let retry = no_results.retry.as_object().unwrap();
        assert!(!retry.contains_key("program"), "the retry drops it");
        assert_eq!(retry["since"], "30d", "and keeps the rest as sent");

        let names: Vec<&str> = out
            .envelope
            .applied_filters
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["program", "since"]);
        assert!(
            out.envelope.time_range.since.is_some() && out.envelope.time_range.since_ms.is_some(),
            "a relative span is a different window on every call, so the absolute one is echoed"
        );
    }

    /// `search()` moved these onto the response precisely because `tracing` reaches whoever is
    /// reading stderr, and over a stdio transport that is nobody. A tool that dropped them would
    /// hand back a zero that reads as an authoritative "no".
    #[test]
    fn a_search_warning_reaches_the_envelope_because_stderr_reaches_nobody() {
        let fx = fixture(&[SESSION], 0);
        // `zzz:` is no field of this schema, so the term became a `tool_input.zzz` subpath
        // lookup that cannot fail and cannot match: a misread colon, not an empty index.
        let out = run(&fx.state, request("zzz:qqq")).unwrap();
        assert_eq!(out.total_documents, 0);
        assert!(
            out.envelope
                .warnings
                .iter()
                .any(|w| w.contains("tool_input JSON subpath")),
            "{:?}",
            out.envelope.warnings
        );
    }

    /// Resolved before the index is touched, so the caller is told which field they mistyped
    /// rather than being handed an `anyhow` chain from inside a query builder — or, worse, a
    /// zero they read as an empty corpus.
    #[test]
    fn a_malformed_since_fails_as_a_filter_error_before_the_index_is_touched() {
        let fx = fixture(&[SESSION], 0);
        let err = run(
            &fx.state,
            SearchTurnsRequest {
                filters: crate::search::Filters {
                    since: Some("last tuesdayy".into()),
                    ..crate::search::Filters::default()
                },
                ..request("claude")
            },
        )
        .expect_err("an unreadable date is the caller's mistake");
        let filter = err
            .downcast_ref::<crate::sessions::FilterError>()
            .expect("stays a FilterError so `from_anyhow` can name the field");
        assert_eq!(filter.field, "since");
    }

    /// An empty page over a non-empty match set is not a zero-hit answer, and the retry
    /// machinery would give it exactly the wrong advice — widen a search that already worked.
    #[test]
    fn an_empty_page_past_the_last_turn_is_a_warning_not_a_zero_hit_retry() {
        let fx = fixture(&[SESSION], 0);
        let out = run(
            &fx.state,
            SearchTurnsRequest {
                offset: 99,
                ..request("claude")
            },
        )
        .unwrap();
        assert_eq!(out.returned, 0);
        assert!(out.total_documents > 0);
        assert!(
            out.envelope.no_results.is_none(),
            "the match set is not empty, so nothing here should be dropped"
        );
        assert!(
            out.envelope
                .warnings
                .iter()
                .any(|w| w.contains("offset 99")),
            "{:?}",
            out.envelope.warnings
        );
    }
}
