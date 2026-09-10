//! `get_turn` and `get_output` — the two halves of the drill-down.
//!
//! They share a file because they share the hard part: resolving what the caller is pointing at.
//! Both accept a reference the model is holding rather than one it constructed, and both have to
//! tell "you pointed at nothing" apart from "what you pointed at is empty".
//!
//! # `get_turn`
//!
//! * The address is the pair `(source_path, turn_seq)`, or a single `doc_id`. Exactly one of the
//!   two forms must arrive: a bare `turn_seq` is not an address — it is an ordinal within one
//!   file, and two transcripts can carry the same session id — so reject it with [`crate::mcp::CallerError`]
//!   rather than guessing a file.
//! * `doc_id` resolves through [`crate::search::resolve_doc`], which takes a `doc_id`, a record
//!   uuid, a `tool_use_id`, or `SESSION:SEQ` / `SESSION:AGENT:SEQ`, each by unambiguous prefix,
//!   and reports ambiguity as an error naming the candidates. Take `source_path` and `turn_seq`
//!   off the document it returns.
//! * The documents come from [`crate::context::turn_window`], which returns the head of the turn
//!   plus the turn's true size. `before`/`after` mean *neighbouring turns in the same file*, not
//!   neighbouring documents: walk `turn_seq` outwards and call `turn_window` per turn. Turns are
//!   not densely numbered — `turn_seq` is the `seq` of the record that opened the turn — so
//!   "the previous turn" is a lookup, not `turn_seq - 1`. [`neighbour_turn`] is that lookup.
//! * Render each document with [`crate::format::doc_json`], which is the pinned document shape
//!   and already withholds `raw`. Then cut `tool_output` to `max_doc_bytes`, mark it, and count
//!   how many documents were cut into `docs_with_truncated_output`. `get_turn` truncates where
//!   `get_output` slices, and the difference is the whole reason both tools exist — but the cut
//!   itself is still [`crate::slice`]'s, one [`SlicePlan`] built from `max_doc_bytes` and reused
//!   for every document of every turn. Only the marker is this module's: see
//!   [`truncation_marker`] for why the generic notice is swapped for a counted one.
//!
//! # `get_output`
//!
//! * The address is `doc_id` or `tool_use_id`; both go through [`crate::search::resolve_doc`],
//!   which already searches the `tool_use_id` field. Exactly one must arrive.
//! * The slicing is [`crate::slice`]: a [`SliceRequest`] built from the request's
//!   `head`/`tail`/`grep`/`context`/`max_bytes`, compiled once by [`SlicePlan::new`] and then
//!   applied. Do not re-implement any of it: that module owns the gap markers, the byte cap and
//!   the report, and a second implementation is a second set of numbers to disagree about. Map
//!   [`crate::slice::SliceReport`] onto the response field for field.
//! * **Three states, not two.** `Doc::tool_output` being `None` means no result ever reached the
//!   index — interrupted, denied, never answered — and `Some("")` means the call ran and printed
//!   nothing. They are different facts and [`OutputState`] is where they are told apart. An empty
//!   string for both is the failure this field exists to prevent.
//! * A document that is not a tool call at all is a caller mistake, not an empty output: say so
//!   with [`crate::mcp::CallerError`], naming what the reference actually resolved to.
//!
//! # Compile the plan before reading the index
//!
//! [`SlicePlan::new`] is the only fallible step of slicing, so both tools compile their plan
//! *before* they resolve anything. A malformed `grep` then costs a regex parse instead of a
//! 200 KB fetch, and — more importantly — it comes back as the caller's mistake rather than as a
//! pattern that matched nothing, which would read as "the log contains no errors".

use std::ops::Bound;

use serde_json::json;
use tantivy::Order;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, RangeQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Term};

use crate::mcp::State;
use crate::mcp::envelope::{self, Context};
use crate::mcp::types::{
    DocValue, GetOutputRequest, GetOutputResponse, GetTurnRequest, GetTurnResponse, OutputState,
    TurnDocuments, TurnRef,
};
use crate::parse::{Doc, DocKind};
use crate::search::Filters;
use crate::slice::{SlicePlan, SliceReport, SliceRequest, TRUNCATION_NOTICE};
use crate::{context, format, search};

/// A mistake in the request itself: an address that is not one, a reference that names nothing or
/// names four things, a `grep` that is not a regular expression.
///
/// It exists because these two tools are the only ones whose *arguments* can be wrong in a way
/// the index cannot answer. Everything else — including "that turn holds no documents" — is a
/// normal result with an [`crate::mcp::types::Envelope`] explaining itself, per the contract in
/// [`crate::mcp::tools`].
///
/// Carried as `anyhow` so the bodies here read like the rest of the crate, and classified once at
/// the boundary by [`crate::mcp::from_anyhow`], exactly as [`crate::sessions::FilterError`] is.
fn bad(message: impl Into<String>) -> anyhow::Error {
    crate::mcp::caller_error(message)
}

/// A request field that arrived as `Some("")` is the same as absent.
///
/// A client that fills every optional field with an empty string is common enough — and
/// `""` addresses nothing — that reading it as "the caller gave a `doc_id`" would turn a missing
/// argument into "no document matches \"\"".
fn given(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// get_turn
// ---------------------------------------------------------------------------

/// One turn's documents in full, plus any neighbouring turns asked for.
pub fn run_turn(state: &State, req: GetTurnRequest) -> anyhow::Result<GetTurnResponse> {
    // 1. The time window, before the index is touched. `get_turn` takes no filters — an address
    //    is not a search — but the envelope is the same envelope, and it must echo a resolved
    //    range rather than an absent one.
    let filters = Filters::default();
    let time_range = envelope::resolve_time_range(&filters)?;

    // 2. The truncation plan, once for the whole request. There is no pattern to compile here,
    //    but there is exactly one budget, and one plan reused across every document is what keeps
    //    two documents of the same turn from being cut by two different rules.
    let plan = SlicePlan::new(&SliceRequest {
        max_bytes: Some(req.max_doc_bytes),
        ..SliceRequest::default()
    })
    .map_err(anyhow::Error::new)?;

    let anchor = address(state, &req)?;

    // 3. The neighbours, outwards from the anchor, one lookup each.
    let mut wanted = Vec::with_capacity(req.before + req.after + 1);
    let mut cursor = anchor.turn_seq;
    for _ in 0..req.before {
        match neighbour_turn(state, &anchor.source_path, cursor, Step::Back)? {
            Some(prev) => {
                wanted.push(prev);
                cursor = prev;
            }
            // The file starts here. Fewer turns than asked for is the answer, not an error.
            None => break,
        }
    }
    wanted.reverse();
    wanted.push(anchor.turn_seq);
    cursor = anchor.turn_seq;
    for _ in 0..req.after {
        match neighbour_turn(state, &anchor.source_path, cursor, Step::Forward)? {
            Some(next) => {
                wanted.push(next);
                cursor = next;
            }
            None => break,
        }
    }

    let mut turns = Vec::with_capacity(wanted.len());
    for turn_seq in wanted {
        let window = context::turn_window(
            &state.index,
            &state.fields,
            &anchor.source_path,
            turn_seq,
            req.max_docs,
        )?;
        // A turn with no documents at all is a turn that does not exist — an address the caller
        // made up, or a stale one from an index that has since been rebuilt. Dropping it here is
        // what makes `turns` empty and hands the explanation to the envelope, rather than
        // returning a hollow entry that reads as a real but silent turn.
        if window.total == 0 {
            continue;
        }
        turns.push(render_turn(&anchor.source_path, &window, &plan));
    }

    let envelope = envelope::build(
        &Context {
            tool: "get_turn",
            query: None,
            filters: &filters,
            time_range,
            extra: vec![
                ("source_path", json!(anchor.source_path)),
                ("turn_seq", json!(anchor.turn_seq)),
            ],
            corpus: &state.corpus,
        },
        turns.len(),
        Vec::new(),
    );
    Ok(GetTurnResponse { turns, envelope })
}

/// The turn the request points at, or the reason it points at nothing.
///
/// The four ways this is called wrong get four different sentences. They are not interchangeable:
/// "turn_seq alone" is a caller who dropped half an address it was handed, and "no address at
/// all" is a caller that has not read a `search_turns` result yet, and the fix differs.
fn address(state: &State, req: &GetTurnRequest) -> anyhow::Result<TurnRef> {
    let path = given(&req.source_path);
    let doc_id = given(&req.doc_id);
    match (path, req.turn_seq, doc_id) {
        (Some(path), Some(turn_seq), None) => Ok(TurnRef {
            source_path: path.to_string(),
            turn_seq,
        }),
        (None, None, Some(spec)) => {
            let doc = search::resolve_doc(&state.index, &state.fields, spec)
                .map_err(|err| bad(format!("{err:#}")))?;
            Ok(TurnRef {
                source_path: doc.source_path,
                turn_seq: doc.turn_seq,
            })
        }
        (_, _, Some(_)) => Err(bad(
            "give either the pair (source_path, turn_seq) or a doc_id, not both: they can \
             disagree, and there is no right way to pick a winner",
        )),
        (None, Some(turn_seq), None) => Err(bad(format!(
            "turn_seq={turn_seq} alone is not an address. `turn_seq` is an ordinal within one \
             transcript file, and two transcripts can carry the same session id, so a bare \
             number can name two different conversations. Send `source_path` with it — \
             `search_turns` returns the pair together — or send a `doc_id` instead"
        ))),
        (Some(path), None, None) => Err(bad(format!(
            "source_path={path} alone is not an address: it names a whole transcript, not a \
             turn. Send `turn_seq` with it, as `search_turns` returns the pair together, or send \
             a `doc_id` instead"
        ))),
        (None, None, None) => Err(bad(
            "no turn was addressed. Send the pair (source_path, turn_seq) exactly as \
             `search_turns` returned it, or a single `doc_id` — a doc_id, a record uuid, a \
             tool_use_id, or SESSION:SEQ / SESSION:AGENT:SEQ, any by unambiguous prefix",
        )),
    }
}

/// Which way [`neighbour_turn`] steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Back,
    Forward,
}

/// The `turn_seq` of the turn immediately before or after `from` in one transcript file, or
/// `None` at the file's edge.
///
/// **Turn numbers are sparse.** `turn_seq` is the `seq` of the record that opened the turn, so a
/// turn of forty documents is followed by a turn numbered forty higher, and `from - 1` is
/// normally not a turn at all — it is a document in the middle of this one. Subtracting one
/// silently returns an empty window, which renders as "that turn has no documents": the caller
/// asked to read backwards and is told the conversation began here.
///
/// So the neighbour is a lookup: every document with a smaller (or larger) `turn_seq` in this
/// file, ordered by `turn_seq` and cut to one. `turn_seq` is a fast field, so the collector hands
/// back the value itself and no stored document is fetched — one query per neighbour, whatever
/// the size of the turns in between. The alternative, walking `seq` outwards until the number
/// changes, costs a query per *document* and a sidechain turn can hold thousands.
///
/// Scoped by `source_path` alone, exactly as [`crate::context::turn_query`] is: the path names
/// one file, and one file is the numbering `turn_seq` belongs to.
fn neighbour_turn(
    state: &State,
    source_path: &str,
    from: u64,
    step: Step,
) -> anyhow::Result<Option<u64>> {
    let f = &state.fields;
    let (lo, hi) = match step {
        // Explicit numeric edges rather than `Bound::Unbounded`, so the range always carries a
        // term of the field it queries and cannot be read as a range over some other field.
        Step::Back => (
            Bound::Included(Term::from_field_u64(f.turn_seq, 0)),
            Bound::Excluded(Term::from_field_u64(f.turn_seq, from)),
        ),
        Step::Forward => (
            Bound::Excluded(Term::from_field_u64(f.turn_seq, from)),
            Bound::Included(Term::from_field_u64(f.turn_seq, u64::MAX)),
        ),
    };
    let query = BooleanQuery::new(vec![
        (
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(f.source_path, source_path),
                IndexRecordOption::Basic,
            )) as Box<dyn Query>,
        ),
        (
            Occur::Must,
            Box::new(RangeQuery::new(lo, hi)) as Box<dyn Query>,
        ),
    ]);
    let order = match step {
        Step::Back => Order::Desc,
        Step::Forward => Order::Asc,
    };
    let searcher = state.index.reader()?.searcher();
    let top = TopDocs::with_limit(1).order_by_fast_field::<u64>("turn_seq", order);
    let found = searcher.search(&query, &top)?;
    // The collector hands back the fast-field value itself, as an `Option` because a document
    // could in principle carry no `turn_seq`. Every document this indexer writes carries one, so
    // a missing value can only mean the file's edge as far as this walk is concerned.
    Ok(found.first().and_then(|&(turn_seq, _)| turn_seq))
}

/// One turn's documents, rendered, with every oversized tool result cut and marked.
fn render_turn(source_path: &str, window: &context::TurnWindow, plan: &SlicePlan) -> TurnDocuments {
    let mut docs = Vec::with_capacity(window.docs.len());
    let mut docs_with_truncated_output = 0;
    for doc in &window.docs {
        let mut value = format::doc_json(doc);
        if let Some(cut) = truncated_output(doc, plan) {
            value["tool_output"] = json!(cut);
            docs_with_truncated_output += 1;
        }
        docs.push(DocValue(value));
    }
    TurnDocuments {
        turn: TurnRef {
            source_path: source_path.to_string(),
            turn_seq: window.turn_seq,
        },
        // Read off the documents rather than carried in from the request: the turn's session is
        // whatever the transcript says it is, and for a subagent file that is not the session the
        // caller searched.
        session_id: window
            .docs
            .first()
            .map(|d| d.session_id.clone())
            .unwrap_or_default(),
        agent_id: window.docs.first().and_then(|d| d.agent_id.clone()),
        shown: docs.len(),
        docs,
        docs_in_turn: window.total,
        truncated: window.total > window.docs.len(),
        docs_with_truncated_output,
    }
}

/// The document's tool result cut to the plan's budget, or `None` when it already fits.
///
/// Outputs inside the budget are passed through untouched rather than round-tripped through
/// [`SlicePlan::slice`], which normalises line endings and drops a trailing newline: for the
/// overwhelming majority of documents `get_turn` returns exactly what the transcript recorded,
/// byte for byte, and only a result that had to be cut is rewritten at all.
fn truncated_output(doc: &Doc, plan: &SlicePlan) -> Option<String> {
    let output = doc.tool_output.as_deref()?;
    let sliced = plan.slice(output);
    if !sliced.report.truncated {
        return None;
    }
    // `slice` ends a truncated text with its own notice, which is deliberately count-free because
    // it is emitted *inside* the byte budget. Here the budget is per document and the counts are
    // exactly what a caller needs to decide whether to spend a `get_output` call, so the notice is
    // swapped for one that says what was cut and how to get the rest. Swapped, not appended: two
    // markers in a row teach a reader to skip both.
    let kept = sliced
        .text
        .strip_suffix(TRUNCATION_NOTICE)
        .map_or(sliced.text.as_str(), str::trim_end);
    Some(format!(
        "{kept}\n{}",
        truncation_marker(&doc.doc_id, &sliced.report)
    ))
}

/// What was cut from one document's tool result, and the call that returns the rest.
///
/// Bracketed like [`crate::slice::gap_marker`] so it cannot be read as a line of the output, and
/// carrying the numbers rather than the word "truncated" alone: a reader who knows 3 of 1877
/// lines are here reaches for `get_output`, and a reader who is told only that something was
/// truncated summarises what is in front of them.
fn truncation_marker(doc_id: &str, report: &SliceReport) -> String {
    let mid_line = if report.cut_mid_line {
        ", and the cut fell inside a line"
    } else {
        ""
    };
    format!(
        "[... cut to fit get_turn's per-document budget: {} of {} lines shown, {} bytes in \
         full{mid_line}. Read it whole with get_output doc_id=\"{doc_id}\" ...]",
        report.returned_lines, report.total_lines, report.total_bytes
    )
}

// ---------------------------------------------------------------------------
// get_output
// ---------------------------------------------------------------------------

/// One tool call's output, sliced.
pub fn run_output(state: &State, req: GetOutputRequest) -> anyhow::Result<GetOutputResponse> {
    let filters = Filters::default();
    let time_range = envelope::resolve_time_range(&filters)?;

    // Before the address is resolved and before a byte of output is read: see the module docs.
    // `InvalidPattern` renders as the pattern plus the regex crate's own syntax error, which
    // points at the offending character — the only part of this a model can act on.
    let plan = SlicePlan::new(&SliceRequest {
        head: req.head,
        tail: req.tail,
        grep: req.grep.clone(),
        context: req.context,
        max_bytes: req.max_bytes,
    })
    .map_err(|err| bad(format!("{err}")))?;

    let doc = resolve_call(state, &req)?;
    let output_state = classify(doc.tool_output.as_deref());
    // Both empty states are sliced from `""` rather than given a hand-built report, so that every
    // count in the response comes from one place: zero bytes, zero lines, and `matched_lines` of
    // `Some(0)` when a pattern was given — the pattern found nothing, which is not the same
    // answer as nothing having been asked.
    let slice = plan.slice(doc.tool_output.as_deref().unwrap_or_default());
    let report = &slice.report;

    let envelope = envelope::build(
        &Context {
            tool: "get_output",
            query: None,
            filters: &filters,
            time_range,
            extra: vec![("doc_id", json!(doc.doc_id))],
            corpus: &state.corpus,
        },
        // One document was addressed and one document is being answered about, whatever its
        // output turned out to be. A `no_result` state is not zero results: it is a fact about
        // the call, already carried by `state`, and a zero-hit envelope beside it would suggest
        // dropping filters that were never set.
        1,
        Vec::new(),
    );
    Ok(GetOutputResponse {
        doc_id: doc.doc_id.clone(),
        tool_use_id: doc.tool_use_id.clone(),
        tool_name: doc.tool_name.clone(),
        is_error: doc.is_error,
        turn: TurnRef {
            source_path: doc.source_path.clone(),
            turn_seq: doc.turn_seq,
        },
        state: output_state,
        output: slice.text.clone(),
        total_bytes: report.total_bytes,
        total_lines: report.total_lines,
        matched_lines: report.matched_lines,
        returned_lines: report.returned_lines,
        dropped_lines: report.dropped_lines,
        truncated: report.truncated,
        cut_mid_line: report.cut_mid_line,
        // True for both empty states as well: nothing was withheld. `state` is what says whether
        // there was anything to withhold, which is why it is documented as the field to read
        // first.
        complete: report.is_complete(),
        envelope,
    })
}

/// Which of the three output states a stored `tool_output` is.
///
/// The whole point of the enum, in one function so it can be pinned by a test: `None` is a result
/// that never reached the index — the call was interrupted, denied, or is still open — and
/// `Some("")` is a call that ran and printed nothing. Rendering both as an empty string tells a
/// caller that `cargo test` produced no output when in fact it never ran.
fn classify(output: Option<&str>) -> OutputState {
    match output {
        None => OutputState::NoResult,
        Some("") => OutputState::Empty,
        Some(_) => OutputState::Present,
    }
}

/// The tool call the request points at.
///
/// `tool_use_id` is not a second resolution path: [`crate::search::resolve_doc`] already searches
/// that field, so both arguments feed the same resolver and cannot disagree about what a
/// reference means.
fn resolve_call(state: &State, req: &GetOutputRequest) -> anyhow::Result<Doc> {
    let spec = match (given(&req.doc_id), given(&req.tool_use_id)) {
        (Some(spec), None) | (None, Some(spec)) => spec,
        (Some(_), Some(_)) => {
            return Err(bad(
                "give either doc_id or tool_use_id, not both: they can name different calls, and \
                 there is no right way to pick a winner",
            ));
        }
        (None, None) => {
            return Err(bad(
                "no tool call was addressed. Send the `doc_id` of a tool_call document from \
                 `get_turn`, or the `tool_use_id` the transcript gave the call",
            ));
        }
    };
    let doc = search::resolve_doc(&state.index, &state.fields, spec)
        .map_err(|err| bad(format!("{err:#}")))?;
    if doc.kind != DocKind::ToolCall {
        // Not an empty output: there is no call here at all. Naming what it did resolve to is the
        // difference between a caller that fixes its reference and one that concludes the tool
        // printed nothing.
        return Err(bad(format!(
            "{spec:?} resolves to doc_id={} — a {} document with role={:?} in turn {} of {}, not \
             a tool call, so it has no output. `get_turn` returns that turn's documents; the \
             tool_call documents in it are the ones with a `tool_use_id`",
            doc.doc_id,
            doc.kind.as_str(),
            doc.role,
            doc.turn_seq,
            doc.source_path
        )));
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{self, IndexOptions};
    use crate::mcp::types::Corpus;
    use std::path::Path as FsPath;
    use std::sync::Mutex;

    const MAIN: &str = "b20208d8-fbdb-5918-ba69-d203de6ed6dc";
    const TURNS: &str = "aaaaaaaa-1111-5111-8111-aaaaaaaaaaaa";
    const CUT: &str = "bbbbbbbb-2222-5222-8222-bbbbbbbbbbbb";

    struct Harness {
        _tmp: tempfile::TempDir,
        state: State,
        /// The `turns.jsonl` copy: three sparsely numbered turns in one file.
        turns_path: String,
        /// The `real_main_slice.jsonl` copy, which shares its session id with `turns_path`.
        main_path: String,
        /// The synthetic transcript: one 9 KB result and one call that was never answered.
        cut_path: String,
    }

    fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("claude/projects");
        let project = root.join("-home-user-session-search");
        std::fs::create_dir_all(&project).unwrap();
        let fixtures = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        std::fs::copy(
            fixtures.join("real_main_slice.jsonl"),
            project.join(format!("{MAIN}.jsonl")),
        )
        .unwrap();
        let turns_path = project.join(format!("{TURNS}.jsonl"));
        std::fs::copy(fixtures.join("turns.jsonl"), &turns_path).unwrap();
        let cut_path = project.join(format!("{CUT}.jsonl"));
        std::fs::write(&cut_path, cut_transcript()).unwrap();

        let index_dir = tmp.path().join("index");
        let stats = index::run(
            &index_dir,
            std::slice::from_ref(&root),
            &IndexOptions::default(),
        )
        .unwrap();
        assert!(stats.docs_added > 5, "fixture indexed {stats:?}");

        let (index, fields) = index::open_or_create(&index_dir).unwrap();
        Harness {
            _tmp: tmp,
            state: State {
                index,
                fields,
                index_dir,
                corpus: Corpus::default(),
                refresh_secs: 0,
                last_refresh: Mutex::new(None),
            },
            turns_path: turns_path.display().to_string(),
            main_path: project.join(format!("{MAIN}.jsonl")).display().to_string(),
            cut_path: cut_path.display().to_string(),
        }
    }

    fn cut_transcript() -> String {
        let long: String = (1..=200)
            .map(|n| format!("line {n}: compiling something that takes a while\n"))
            .collect();
        let rows = [
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": "build it"},
                "parentUuid": null, "isSidechain": false, "uuid": "c-u1",
                "timestamp": "2026-09-09T20:00:00.000Z", "cwd": "/home/user/session-search",
                "sessionId": CUT, "version": "2.1.266", "gitBranch": "claude/drill"
            }),
            serde_json::json!({
                "type": "assistant", "requestId": "req_c1", "apiBlockIndex": 0,
                "message": {"model": "claude-opus-5", "id": "msg_c1", "type": "message",
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_long", "name": "Bash",
                        "input": {"command": "cargo build"}}],
                    "stop_reason": "tool_use"},
                "parentUuid": "c-u1", "isSidechain": false, "uuid": "c-a1",
                "timestamp": "2026-09-09T20:00:01.000Z", "cwd": "/home/user/session-search",
                "sessionId": CUT, "version": "2.1.266", "gitBranch": "claude/drill"
            }),
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": [{"tool_use_id": "toolu_long",
                    "type": "tool_result", "content": long, "is_error": false}]},
                "toolUseResult": {"stdout": long, "stderr": "", "interrupted": false,
                    "isImage": false},
                "sourceToolAssistantUUID": "c-a1", "parentUuid": "c-a1", "isSidechain": false,
                "uuid": "c-u2", "timestamp": "2026-09-09T20:00:02.000Z",
                "cwd": "/home/user/session-search", "sessionId": CUT, "version": "2.1.266",
                "gitBranch": "claude/drill"
            }),
            // The call that was never answered: a `tool_use` with no `tool_result` anywhere in
            // the file, which is what an interrupt or a denied permission leaves behind.
            serde_json::json!({
                "type": "assistant", "requestId": "req_c2", "apiBlockIndex": 0,
                "message": {"model": "claude-opus-5", "id": "msg_c2", "type": "message",
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_interrupted", "name": "Bash",
                        "input": {"command": "cargo test"}}],
                    "stop_reason": "tool_use"},
                "parentUuid": "c-u2", "isSidechain": false, "uuid": "c-a2",
                "timestamp": "2026-09-09T20:00:03.000Z", "cwd": "/home/user/session-search",
                "sessionId": CUT, "version": "2.1.266", "gitBranch": "claude/drill"
            }),
        ];
        rows.iter()
            .map(|r| format!("{r}\n"))
            .collect::<Vec<_>>()
            .concat()
    }

    /// The turn numbers the `turns.jsonl` fixture actually produces, which is what makes the
    /// sparseness in `the_neighbour_walk_crosses_sparse_turn_numbering` a fact rather than a hope.
    const SPARSE_TURNS: [u64; 3] = [0, 5, 11];

    fn turn_seqs(response: &GetTurnResponse) -> Vec<u64> {
        response.turns.iter().map(|t| t.turn.turn_seq).collect()
    }

    #[test]
    fn a_bare_turn_seq_is_refused_rather_than_guessed_at_a_file() {
        let h = harness();
        let err = run_turn(
            &h.state,
            GetTurnRequest {
                turn_seq: Some(0),
                ..GetTurnRequest::default()
            },
        )
        .unwrap_err();
        assert!(
            err.downcast_ref::<crate::mcp::CallerError>().is_some(),
            "a missing address is the caller's mistake, not the server's: {err:#}"
        );
        let message = format!("{err:#}");
        assert!(message.contains("not an address"), "{message}");
        assert!(
            message.contains("two transcripts can carry the same session id"),
            "the refusal has to say why a number alone is ambiguous: {message}"
        );
        // And the fixture proves the ambiguity is real: turn 0 exists in two indexed files that
        // carry the same session id, so there is no file for a bare number to be resolved against.
        for path in [&h.turns_path, &h.main_path] {
            let window =
                context::turn_window(&h.state.index, &h.state.fields, path, 0, 10).unwrap();
            assert!(window.total > 0, "turn 0 should exist in {path}");
        }
    }

    #[test]
    fn an_address_given_both_ways_at_once_is_refused_rather_than_silently_preferred() {
        let h = harness();
        let err = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                turn_seq: Some(0),
                doc_id: Some("toolu_1".into()),
                ..GetTurnRequest::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not both"), "{err:#}");
    }

    #[test]
    fn the_pair_and_a_doc_id_address_the_same_turn() {
        let h = harness();
        let by_pair = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                turn_seq: Some(0),
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        // `toolu_1` is a document *inside* turn 0, not its opener: resolving a reference has to
        // land on the turn that holds it, not on a turn of its own.
        let by_doc = run_turn(
            &h.state,
            GetTurnRequest {
                doc_id: Some("toolu_1".into()),
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        assert_eq!(turn_seqs(&by_pair), vec![0]);
        assert_eq!(turn_seqs(&by_doc), turn_seqs(&by_pair));
        assert_eq!(by_doc.turns[0].shown, by_pair.turns[0].shown);
    }

    #[test]
    fn the_neighbour_walk_crosses_sparse_turn_numbering() {
        let h = harness();
        // The fact the walk has to survive: turn 11's predecessor is turn 5, and 10 is not a
        // turn at all — it is a document in the middle of turn 5. `turn_seq - 1` would return an
        // empty window and read as "the conversation starts here".
        for absent in [SPARSE_TURNS[2] - 1, SPARSE_TURNS[1] - 1] {
            let window =
                context::turn_window(&h.state.index, &h.state.fields, &h.turns_path, absent, 10)
                    .unwrap();
            assert_eq!(
                window.total, 0,
                "turn_seq {absent} is not a turn; the fixture stopped being sparse"
            );
        }

        let backwards = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                turn_seq: Some(SPARSE_TURNS[2]),
                before: 2,
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        assert_eq!(turn_seqs(&backwards), SPARSE_TURNS.to_vec());

        let forwards = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                turn_seq: Some(SPARSE_TURNS[0]),
                after: 2,
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        assert_eq!(turn_seqs(&forwards), SPARSE_TURNS.to_vec());
        // Every returned turn is a real one, with documents in it.
        for turn in &forwards.turns {
            assert!(turn.docs_in_turn > 0, "{:?}", turn.turn);
        }
    }

    #[test]
    fn the_neighbour_walk_stops_at_the_edges_of_the_file_rather_than_failing() {
        let h = harness();
        let whole = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                turn_seq: Some(SPARSE_TURNS[1]),
                before: 50,
                after: 50,
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        assert_eq!(turn_seqs(&whole), SPARSE_TURNS.to_vec());
    }

    #[test]
    fn the_neighbour_walk_never_leaves_the_file_it_started_in() {
        let h = harness();
        // Both fixtures carry the same session id — §9's case, and the reason `turn_seq` alone
        // is not an address. A walk scoped by session rather than by path would interleave them.
        let walked = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                turn_seq: Some(SPARSE_TURNS[0]),
                after: 50,
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        for turn in &walked.turns {
            assert_eq!(turn.turn.source_path, h.turns_path);
        }
    }

    #[test]
    fn a_turn_addressed_but_not_found_is_an_empty_answer_with_an_envelope() {
        let h = harness();
        let response = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                // A number in the middle of turn 5, which no turn opens on.
                turn_seq: Some(SPARSE_TURNS[1] + 1),
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        assert!(response.turns.is_empty());
        assert!(
            response.envelope.no_results.is_some(),
            "a zero must never travel alone"
        );
    }

    #[test]
    fn the_document_cap_reports_the_size_of_the_turn_it_cut() {
        let h = harness();
        let response = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.turns_path.clone()),
                turn_seq: Some(SPARSE_TURNS[1]),
                max_docs: 2,
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        let turn = &response.turns[0];
        assert_eq!(turn.shown, 2);
        assert!(turn.docs_in_turn > 2, "{turn:?}");
        assert!(turn.truncated);
        assert!(response.envelope.no_results.is_none());
    }

    #[test]
    fn a_truncated_document_says_what_was_cut_and_how_to_read_the_rest() {
        let h = harness();
        let response = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.cut_path.clone()),
                turn_seq: Some(0),
                max_doc_bytes: 200,
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        let turn = &response.turns[0];
        // Two tool calls in the turn, one of them 9 KB and one with no result at all: the count
        // is of documents actually cut, not of documents that have a `tool_output` field.
        assert_eq!(turn.docs_with_truncated_output, 1, "{turn:?}");
        let cut = turn
            .docs
            .iter()
            .find_map(|d| {
                d.0["tool_output"]
                    .as_str()
                    .filter(|o| o.contains("cut to fit"))
            })
            .expect("no document carries a truncation marker");
        assert!(cut.contains("of 200 lines shown"), "{cut}");
        assert!(cut.contains("9692 bytes in full"), "{cut}");
        assert!(cut.contains("get_output doc_id=\""), "{cut}");
        // One marker, not two: `slice`'s own count-free notice is swapped out, not appended.
        assert!(!cut.contains(TRUNCATION_NOTICE), "{cut}");
        assert_eq!(cut.matches("[... ").count(), 1, "{cut}");
        // And the doc_id in the marker is a reference `get_output` actually accepts.
        let quoted = cut.split("get_output doc_id=\"").nth(1).unwrap();
        let doc_id = quoted.split('"').next().unwrap();
        let full = run_output(
            &h.state,
            GetOutputRequest {
                doc_id: Some(doc_id.to_string()),
                ..GetOutputRequest::default()
            },
        )
        .unwrap();
        assert_eq!(full.total_bytes, 9692);
        assert_eq!(full.state, OutputState::Present);
    }

    #[test]
    fn an_output_inside_the_budget_is_returned_byte_for_byte() {
        let h = harness();
        let response = run_turn(
            &h.state,
            GetTurnRequest {
                source_path: Some(h.cut_path.clone()),
                turn_seq: Some(0),
                max_doc_bytes: 20_000,
                ..GetTurnRequest::default()
            },
        )
        .unwrap();
        let turn = &response.turns[0];
        assert_eq!(turn.docs_with_truncated_output, 0);
        let output = turn
            .docs
            .iter()
            .find_map(|d| d.0["tool_output"].as_str())
            .expect("the long call has an output");
        // The transcript's trailing newline survives, which round-tripping every output through
        // the slicer would have quietly normalised away.
        assert!(output.ends_with('\n'), "{:?}", &output[output.len() - 20..]);
        assert_eq!(output.len(), 9692);
    }

    #[test]
    fn a_missing_result_and_an_empty_result_are_different_states() {
        assert_eq!(classify(None), OutputState::NoResult);
        assert_eq!(classify(Some("")), OutputState::Empty);
        assert_eq!(classify(Some("ok")), OutputState::Present);

        // And end to end: the fixture's interrupted call reaches the caller as `no_result`, with
        // an output that is empty because there is none — never as a call that printed nothing.
        let h = harness();
        let response = run_output(
            &h.state,
            GetOutputRequest {
                tool_use_id: Some("toolu_interrupted".into()),
                ..GetOutputRequest::default()
            },
        )
        .unwrap();
        assert_eq!(response.state, OutputState::NoResult);
        assert_eq!(response.output, "");
        assert_eq!(response.total_bytes, 0);
        assert_eq!(response.total_lines, 0);
        assert_eq!(response.tool_use_id.as_deref(), Some("toolu_interrupted"));
        assert!(
            response.envelope.no_results.is_none(),
            "{:?}",
            response.envelope
        );
    }

    #[test]
    fn a_malformed_grep_is_rejected_with_its_syntax_error_before_any_output_is_read() {
        let h = harness();
        let err = run_output(
            &h.state,
            GetOutputRequest {
                // A reference that names nothing, so the only way this can fail on the pattern is
                // if the pattern was compiled before the index was touched.
                doc_id: Some("no-such-reference-anywhere".into()),
                grep: Some("error(".into()),
                ..GetOutputRequest::default()
            },
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            err.downcast_ref::<crate::mcp::CallerError>().is_some(),
            "{message}"
        );
        assert!(message.contains("invalid `grep` pattern"), "{message}");
        assert!(message.contains("regex parse error"), "{message}");
        assert!(
            !message.contains("no document matches"),
            "the address was resolved before the pattern was compiled: {message}"
        );
    }

    #[test]
    fn a_grep_that_matches_nothing_is_zero_matches_and_not_an_error() {
        let h = harness();
        let response = run_output(
            &h.state,
            GetOutputRequest {
                tool_use_id: Some("toolu_long".into()),
                grep: Some("(?i)^segmentation fault$".into()),
                ..GetOutputRequest::default()
            },
        )
        .unwrap();
        assert_eq!(response.matched_lines, Some(0));
        assert_eq!(response.returned_lines, 0);
        assert_eq!(response.total_lines, 200);
        assert!(!response.complete, "200 lines were dropped");
    }

    #[test]
    fn a_sliced_output_never_reports_itself_as_complete() {
        let h = harness();
        let sliced = run_output(
            &h.state,
            GetOutputRequest {
                tool_use_id: Some("toolu_long".into()),
                head: Some(2),
                tail: Some(1),
                ..GetOutputRequest::default()
            },
        )
        .unwrap();
        assert_eq!(sliced.returned_lines, 3);
        assert_eq!(sliced.total_lines, 200);
        assert_eq!(sliced.dropped_lines, 197);
        assert!(!sliced.complete);
        // The gap between the two ends is marked, never silently joined.
        assert!(
            sliced.output.contains("197 lines omitted"),
            "{}",
            sliced.output
        );

        let whole = run_output(
            &h.state,
            GetOutputRequest {
                tool_use_id: Some("toolu_long".into()),
                ..GetOutputRequest::default()
            },
        )
        .unwrap();
        assert!(whole.complete, "{whole:?}");
        assert_eq!(whole.matched_lines, None, "nothing was asked of a pattern");
        assert_eq!(whole.returned_lines, 200);
    }

    #[test]
    fn a_document_that_is_not_a_tool_call_is_a_bad_reference_and_not_an_empty_output() {
        let h = harness();
        let err = run_output(
            &h.state,
            GetOutputRequest {
                // The user prompt that opens the turn, addressed by its record uuid.
                doc_id: Some("c-u1".into()),
                ..GetOutputRequest::default()
            },
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            err.downcast_ref::<crate::mcp::CallerError>().is_some(),
            "{message}"
        );
        assert!(message.contains("not a tool call"), "{message}");
        assert!(message.contains("role=\"user\""), "{message}");
    }

    #[test]
    fn addressing_no_call_at_all_says_which_two_references_are_accepted() {
        let h = harness();
        let err = run_output(&h.state, GetOutputRequest::default()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("doc_id"), "{message}");
        assert!(message.contains("tool_use_id"), "{message}");
    }
}
