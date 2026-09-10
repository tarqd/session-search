//! `search_sessions` — the session listing.
//!
//! # What to build on
//!
//! * [`crate::index::load_sessions`] via [`crate::mcp::State::sessions`]. This tool does **not**
//!   touch the tantivy index: a session listing comes from `sessions.json`, one row per
//!   transcript file, and that is the entire reason ten of the eighteen filters cannot be
//!   answered here.
//! * [`crate::sessions::SessionMatcher::new`] and `matches`. One matcher, shared with the CLI and
//!   the HTTP API — do not write a third. Its `new` returns a
//!   [`crate::sessions::FilterError`] for an unreadable date; return it unchanged and
//!   [`crate::mcp::from_anyhow`] turns it into `invalid_params` naming the field, never a flag.
//! * [`crate::sessions::unanswerable_filter_notes`] for the warnings. Every one of those
//!   sentences must reach the response: a listing filtered by nine of ten filters looks exactly
//!   like a listing filtered by ten, and a model reads "no session used that model" out of a
//!   result that means "that question cannot be asked here". `tracing` is not a channel here.
//!
//!   The sentence's closing pointer is a parameter, [`crate::sessions::SearchSurface`], and this
//!   tool passes `McpTool` so it reads "Use the `search_turns` tool for it". It used to say
//!   `/api/search` unconditionally — an HTTP route an MCP client cannot request, which reads as
//!   an answer and stops the caller looking. The list and the reasoning stay shared: a second
//!   sentence for MCP would have arrived with a second copy of the ten filters beside it.
//!
//!   Those same ten are cleared before the envelope is built — see [`applied_filters`]. The
//!   envelope echoes and ranks what was *applied*, and a filter that could never have been
//!   applied is not a cause a caller can act on.
//!
//! # Ordering and counts
//!
//! Most recent `last_ts_ms` first, ties broken by session id and then agent id, exactly as
//! `cli.rs` orders it — a listing whose order changes between front ends is a listing nobody can
//! page. `total` is how many matched *before* `limit` cut the list; `returned` is how many are
//! here. Truncating without reporting the first number is how "I only worked on three things
//! last week" gets said out loud.

use crate::mcp::State;
use crate::mcp::envelope::{self, Context};
use crate::mcp::types::{SearchSessionsRequest, SearchSessionsResponse, SessionRow};
use crate::parse::SessionInfo;
use crate::search::Filters;
use crate::sessions::{
    SearchSurface, SessionMatcher, unanswerable_filter_notes, unanswerable_filters,
};

/// Filter, sort and page the session list.
///
/// The work is in the order the contract demands and for the reasons it gives: the window is
/// resolved before `sessions.json` is opened, so an unreadable `since` is `invalid_params`
/// naming the field rather than an I/O-shaped failure; the notes for the filters this listing
/// cannot answer are collected from the request alone, so they travel whether or not anything
/// matched; and the answer always leaves through [`envelope::build`].
///
/// Errors are for requests that could not be answered at all. "Nothing matched" is not one: it
/// is a successful response whose envelope carries the zero, the applied filters and the retry.
pub fn run(state: &State, req: SearchSessionsRequest) -> anyhow::Result<SearchSessionsResponse> {
    // 1. The window, before any file is read. `?` keeps the `FilterError` concrete all the way
    //    to `from_anyhow`, which downcasts it into `invalid_params` naming `since` or `until`.
    let time_range = envelope::resolve_time_range(&req.filters)?;

    // 2. What this listing was asked and cannot answer, taken from the request rather than from
    //    the result: a filter that could never have been applied is worth saying on a page of
    //    fifty rows exactly as much as on a zero, and the caller has no stderr to hear it on.
    let warnings = unanswerable_filter_notes(&req.filters, SearchSurface::McpTool);

    // 3. The rows. `SessionMatcher` resolves the same two dates a second time against its own
    //    `now`, microseconds after `resolve_time_range` did — irrelevant at the resolution of a
    //    session's last activity, and the alternative is a second constructor on a type three
    //    front ends share.
    let matcher = SessionMatcher::new(&req.filters)?;
    let mut matched: Vec<SessionInfo> = state
        .sessions()?
        .into_values()
        .filter(|info| matcher.matches(info))
        .collect();

    // `total` counts everything that matched, not what fits under `limit`: a listing that
    // reports its own page size as the total tells the caller there is nothing more, and "I
    // only worked on three things last week" is how that gets said out loud.
    let total = matched.len();

    // Most recent first; the ids break ties so the listing is deterministic. Byte-identical to
    // what `session-search sessions` and `GET /api/sessions` do, because a listing whose order
    // changes between front ends is a listing nobody can page. `None` sorts last under the
    // reversed comparison, which puts sessions with no timestamp at the end where they belong.
    matched.sort_by(|a, b| {
        b.last_ts_ms
            .cmp(&a.last_ts_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    matched.truncate(req.limit);

    let sessions: Vec<SessionRow> = matched.into_iter().map(row).collect();
    // 4. The filters the envelope is allowed to reason about: the ones this listing actually
    //    applied. Everything the warnings above call ignored is cleared first, for the reason in
    //    `applied`'s own doc comment — an envelope over the raw request blames filters that
    //    narrowed nothing.
    let applied = applied_filters(&req.filters);
    let ctx = Context {
        tool: "search_sessions",
        // This tool takes no free-text query: `sessions.json` has nothing to search. A zero here
        // is always a filter's doing, so the envelope's query-shaped advice never applies.
        query: None,
        filters: &applied,
        time_range,
        extra: Vec::new(),
        corpus: &state.corpus,
    };
    Ok(SearchSessionsResponse {
        returned: sessions.len(),
        sessions,
        total,
        envelope: envelope::build(&ctx, total, warnings),
    })
}

/// The request's filters with the unanswerable ones cleared — what the listing actually applied.
///
/// [`envelope::build`] echoes these and ranks them, and both readings are of *applied* filters:
/// the echo is a report of what narrowed the answer, and `narrowest` names the likeliest cause of
/// a zero. Handing it the whole request lets it blame a filter this same response has already
/// called ignored, which is the one thing a caller cannot act on. It happened: `search_sessions
/// {session: "zzzzzz", tool_input: ["command=cargo"], model: "claude-opus-5"}` came back with
/// `warnings: ["`tool_input` was ignored…", "`model` was ignored…"]` beside
/// `narrowest_filter: "tool_input"` and a retry that dropped the ignored `tool_input`, kept the
/// ignored `model`, and left `session` — the filter that actually caused the zero — in place.
/// Following that retry returns zero again, and the caller has learned nothing.
///
/// The list of what to clear is [`unanswerable_filters`], the same list the warnings are built
/// from, so the two cannot disagree about which filters this listing can answer. A name that
/// arrives here with no arm to clear it is that drift, and
/// `no_filter_the_listing_ignored_can_be_blamed_for_the_zero_it_did_not_cause` is where it fails.
fn applied_filters(f: &Filters) -> Filters {
    let mut applied = f.clone();
    for name in unanswerable_filters(f) {
        match name {
            "tool" => applied.tool.clear(),
            "tool_input" => applied.tool_input.clear(),
            "tool_output" => applied.tool_output.clear(),
            "lang" => applied.lang.clear(),
            "min_thinking" => applied.min_thinking = None,
            "program" => applied.program.clear(),
            "model" => applied.model = None,
            "role" => applied.role = None,
            "kind" => applied.kind = None,
            "errors_only" => applied.errors_only = false,
            other => debug_assert!(
                false,
                "`{other}` is reported as ignored but is not cleared here: the envelope would \
                 blame a filter this listing never applied"
            ),
        }
    }
    applied
}

/// One `sessions.json` row on the wire.
///
/// The two timestamp strings are derived here rather than stored: `SessionInfo` keeps only epoch
/// milliseconds, and a caller reading a listing should not have to convert them to say when
/// something happened. They are spelled the way `TimeRange` spells its bounds — seconds, `Z` —
/// so a row's `last_timestamp` and the envelope's resolved window can be compared by eye.
///
/// `description` and `slug` are dropped: the first is a subagent's `.meta.json` blurb and the
/// second a filename fragment, and neither is something to filter, address or quote.
fn row(info: SessionInfo) -> SessionRow {
    SessionRow {
        session_id: info.session_id,
        agent_id: info.agent_id,
        agent_type: info.agent_type,
        title: info.title,
        first_prompt: info.first_prompt,
        project: info.project,
        git_branch: info.git_branch,
        source_path: info.source_path,
        first_timestamp: info.first_ts_ms.and_then(rfc3339),
        last_timestamp: info.last_ts_ms.and_then(rfc3339),
        first_ts_ms: info.first_ts_ms,
        last_ts_ms: info.last_ts_ms,
        messages: info.messages,
        tool_calls: info.tool_calls,
    }
}

fn rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::*;
    use crate::mcp::types::{Corpus, RetryAction};
    use crate::search::Filters;

    /// A `State` over a `sessions.json` written by hand.
    ///
    /// The index is created in RAM and never read: `search_sessions` answers out of
    /// `sessions.json`, and an in-memory index makes that structural rather than a claim — a
    /// body that reached for tantivy would find nothing here.
    fn state_over(rows: &[SessionInfo]) -> (tempfile::TempDir, State) {
        let tmp = tempfile::tempdir().expect("a temp dir");
        write_sessions(tmp.path(), rows);
        let (schema, fields) = crate::schema::build_schema();
        let state = State {
            index: tantivy::Index::create_in_ram(schema),
            fields,
            index_dir: tmp.path().to_path_buf(),
            corpus: Corpus {
                sessions: rows.len(),
                docs: 190_233,
                newest_ts: Some("2026-09-10T11:20:14Z".into()),
            },
            refresh_secs: 0,
            last_refresh: Mutex::new(None),
        };
        (tmp, state)
    }

    /// `sessions.json` is keyed by absolute transcript path, because a subagent shares its
    /// parent's session id and two sidechains would otherwise collapse onto one row.
    fn write_sessions(index_dir: &Path, rows: &[SessionInfo]) {
        let map: BTreeMap<String, SessionInfo> = rows
            .iter()
            .map(|info| (info.source_path.clone(), info.clone()))
            .collect();
        std::fs::write(
            index_dir.join("sessions.json"),
            serde_json::to_vec(&map).expect("SessionInfo holds only JSON-native values"),
        )
        .expect("writing the fixture listing");
    }

    fn session(id: &str, last_ts_ms: i64) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            source_path: format!("/transcripts/{id}.jsonl"),
            project: Some("/home/user/session-search".into()),
            first_ts_ms: Some(last_ts_ms - 60_000),
            last_ts_ms: Some(last_ts_ms),
            messages: 4,
            tool_calls: 2,
            ..SessionInfo::default()
        }
    }

    fn sidechain(id: &str, agent_id: &str, last_ts_ms: i64) -> SessionInfo {
        SessionInfo {
            agent_id: Some(agent_id.into()),
            agent_type: Some("Explore".into()),
            source_path: format!("/transcripts/{id}.{agent_id}.jsonl"),
            ..session(id, last_ts_ms)
        }
    }

    fn request(limit: usize, mutate: impl FnOnce(&mut Filters)) -> SearchSessionsRequest {
        let mut filters = Filters::default();
        mutate(&mut filters);
        SearchSessionsRequest {
            filters,
            limit,
            ..SearchSessionsRequest::default()
        }
    }

    fn ids(res: &SearchSessionsResponse) -> Vec<(&str, Option<&str>)> {
        res.sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.agent_id.as_deref()))
            .collect()
    }

    #[test]
    fn the_newest_session_is_first_and_both_tie_breaks_survive() {
        // Two ties, deliberately: `bbb` and `aaa` share an instant, and `aaa`'s two rows share
        // an instant *and* a session id — a sidechain records its parent's id, so the agent id
        // is the only thing left to order them by. Without both tie-breaks the listing comes
        // back in `BTreeMap` order on one run and in another order on the next, and a caller
        // paging it sees rows twice or not at all.
        let (_tmp, state) = state_over(&[
            session("bbb", 1_700_000_000_000),
            sidechain("aaa", "agent-2", 1_700_000_000_000),
            session("ccc", 1_700_000_900_000),
            sidechain("aaa", "agent-1", 1_700_000_000_000),
            session("aaa", 1_700_000_000_000),
        ]);
        let res = run(&state, request(50, |_| {})).expect("a listing with no filters");
        assert_eq!(
            ids(&res),
            [
                ("ccc", None),
                ("aaa", None),
                ("aaa", Some("agent-1")),
                ("aaa", Some("agent-2")),
                ("bbb", None),
            ],
            "last activity descending, then session id, then agent id"
        );
    }

    #[test]
    fn total_counts_what_matched_and_returned_counts_what_fits_under_the_limit() {
        // The pair exists so a truncated page cannot be read as a complete one. Reporting five
        // as two is how "I only worked on two things last week" gets said out loud.
        let rows: Vec<SessionInfo> = (0..5)
            .map(|i| session(&format!("s{i}"), 1_700_000_000_000 + i * 1_000))
            .collect();
        let (_tmp, state) = state_over(&rows);
        let res = run(&state, request(2, |_| {})).expect("a listing with no filters");
        assert_eq!(res.total, 5);
        assert_eq!(res.returned, 2);
        assert_eq!(res.sessions.len(), 2);
        assert_eq!(ids(&res), [("s4", None), ("s3", None)]);
        assert!(
            res.envelope.no_results.is_none(),
            "a page of hits must never carry a retry: `no_results` is `Some` exactly when \
             `total` is 0"
        );
    }

    #[test]
    fn a_filter_the_listing_cannot_answer_arrives_as_a_warning_instead_of_narrowing_nothing() {
        // `model` is a per-message fact and `sessions.json` has no column for it. The listing is
        // therefore identical with and without it — which is exactly why the note has to travel:
        // a caller who sent `model` and got these rows back would otherwise read them as "these
        // are the sessions that used that model".
        let (_tmp, state) = state_over(&[
            session("aaa", 1_700_000_000_000),
            session("bbb", 1_700_000_900_000),
        ]);
        let unfiltered = run(&state, request(50, |_| {})).expect("a listing with no filters");
        let filtered = run(
            &state,
            request(50, |f| {
                f.model = Some("claude-opus-5".into());
                f.errors_only = true;
            }),
        )
        .expect("an unanswerable filter is not an error");

        assert_eq!(filtered.total, unfiltered.total, "nothing was narrowed");
        assert_eq!(ids(&filtered), ids(&unfiltered));
        assert_eq!(
            filtered.envelope.warnings.len(),
            2,
            "one note per unanswerable filter: {:?}",
            filtered.envelope.warnings
        );
        assert!(filtered.envelope.warnings[0].starts_with("`model` was ignored"));
        assert!(filtered.envelope.warnings[1].starts_with("`errors_only` was ignored"));
        // The pointer has to be one an MCP caller can act on; `/api/search` is not.
        for warning in &filtered.envelope.warnings {
            assert!(
                warning.contains("Use the `search_turns` tool for it."),
                "{warning}"
            );
        }
        assert!(unfiltered.envelope.warnings.is_empty());
    }

    #[test]
    fn no_filter_the_listing_ignored_can_be_blamed_for_the_zero_it_did_not_cause() {
        // One envelope used to hold both halves of a contradiction: warnings saying `tool_input`
        // and `model` were ignored, and `narrowest_filter: "tool_input"` with "it is the
        // likeliest cause" — of a zero it could not have caused, since it narrowed nothing. The
        // retry that came with it dropped the ignored `tool_input`, kept the ignored `model`, and
        // kept `session`, the filter that actually caused the zero. Sending it returns zero
        // again.
        let (_tmp, state) = state_over(&[session("aaa", 1_700_000_000_000)]);
        let res = run(
            &state,
            request(50, |f| {
                f.session = Some("zzzzzz".into());
                f.tool_input = vec!["command=cargo".into()];
                f.model = Some("claude-opus-5".into());
            }),
        )
        .expect("a zero is a successful answer");
        assert_eq!(res.total, 0);
        assert_eq!(
            res.envelope.warnings.len(),
            2,
            "{:?}",
            res.envelope.warnings
        );

        let echoed: Vec<&str> = res
            .envelope
            .applied_filters
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            echoed,
            ["session"],
            "only filters the listing applied are `applied_filters`"
        );

        let no_results = res.envelope.no_results.expect("a zero is never bare");
        assert_eq!(
            no_results.narrowest_filter.as_deref(),
            Some("session"),
            "the blame has to land on the one filter that actually cut the listing"
        );
        assert!(
            !no_results.message.contains("tool_input"),
            "{}",
            no_results.message
        );
        // The retry is the call to send next, so it must not carry a filter that was ignored —
        // and it must not carry the one being blamed either.
        assert_eq!(
            no_results.retry,
            serde_json::json!({}),
            "dropping the only applied filter leaves a listing of everything, which is sendable"
        );
    }

    #[test]
    fn every_filter_this_listing_cannot_apply_is_cleared_before_the_envelope_sees_it() {
        // The drift guard for `applied_filters`: the ten names come from
        // `sessions::unanswerable_filters`, and a filter added to that list without an arm to
        // clear it would go back to being blamed for zeroes it did not cause. Everything a
        // `Filters` can carry is set here, so the two lists are compared in full.
        let (_tmp, state) = state_over(&[session("aaa", 1_700_000_000_000)]);
        let req = request(50, |f| {
            f.project = Some("/home/user/session-search".into());
            f.tool = vec!["Bash".into()];
            f.tool_input = vec!["command=cargo".into()];
            f.tool_output = vec!["No such file".into()];
            f.lang = vec!["rust".into()];
            f.min_thinking = Some(500);
            f.program = vec!["cargo".into()];
            f.branch = Some("main".into());
            f.model = Some("claude-opus-5".into());
            f.role = Some("assistant".into());
            f.kind = Some("tool_call".into());
            f.session = Some("aaa".into());
            f.agent_type = Some("Explore".into());
            f.since = Some("2020-01-01".into());
            f.until = Some("2030-01-01".into());
            f.errors_only = true;
            f.no_sidechains = true;
        });
        let ignored = crate::sessions::unanswerable_filters(&req.filters);
        assert_eq!(ignored.len(), 10, "{ignored:?}");
        let res = run(&state, req).expect("unanswerable filters are not errors");

        for name in ignored {
            assert!(
                !res.envelope.applied_filters.iter().any(|a| a.name == name),
                "`{name}` was reported as ignored and echoed as applied: {:?}",
                res.envelope.applied_filters
            );
        }
        // And the answerable ones are all still there: clearing must not cost a filter that works.
        let echoed: Vec<&str> = res
            .envelope
            .applied_filters
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            echoed,
            [
                "project",
                "branch",
                "session",
                "agent_type",
                "since",
                "until",
                "no_sidechains"
            ]
        );
    }

    #[test]
    fn a_zero_carries_the_filters_the_resolved_window_and_a_retry_to_send() {
        // Issue #28's failure in miniature: `{sessions: [], total: 0}` alone reads as "nothing
        // happened", and a project scope that matches no row looks exactly like an idle month.
        let (_tmp, state) = state_over(&[session("aaa", 1_700_000_000_000)]);
        let res = run(
            &state,
            request(50, |f| {
                f.project = Some("/home/user/nowhere".into());
                f.since = Some("2026-09-01".into());
            }),
        )
        .expect("a zero is a successful answer");
        assert_eq!(res.total, 0);
        assert_eq!(res.returned, 0);

        let applied: Vec<&str> = res
            .envelope
            .applied_filters
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(applied, ["project", "since"]);
        assert_eq!(
            res.envelope.time_range.since.as_deref(),
            Some("2026-09-01T00:00:00Z"),
            "the absolute window is the one to quote, not the string that was sent"
        );

        let no_results = res.envelope.no_results.expect("a zero is never bare");
        // `since` is blamed before `project`, and the order is the envelope's, not this tool's:
        // widening the window asks the same question over a longer period, while dropping the
        // project scope asks a different question of other repositories. The scope filter goes
        // last for that reason, so it is the one thing the retry keeps.
        assert_eq!(no_results.narrowest_filter.as_deref(), Some("since"));
        assert_eq!(no_results.action, RetryAction::Drop);
        assert!(
            no_results.message.contains("0 results"),
            "{}",
            no_results.message
        );
        assert!(
            no_results
                .message
                .contains("Time range resolved to 2026-09-01T00:00:00Z"),
            "the zero has to name the window it looked in: {}",
            no_results.message
        );
        assert_eq!(
            no_results.retry,
            serde_json::json!({ "project": "/home/user/nowhere" }),
            "the retry must be sendable as-is, with the blamed filter gone and the rest kept"
        );
    }

    #[test]
    fn an_unreadable_date_is_the_callers_mistake_and_stops_before_anything_is_read() {
        // Resolved first, and from the request alone: pointing `index_dir` at nothing means a
        // body that read `sessions.json` before parsing the date would fail with an I/O error
        // instead of the one thing the caller can fix.
        let state = State {
            index: tantivy::Index::create_in_ram(crate::schema::build_schema().0),
            fields: crate::schema::build_schema().1,
            index_dir: PathBuf::from("/nonexistent/index"),
            corpus: Corpus::default(),
            refresh_secs: 0,
            last_refresh: Mutex::new(None),
        };
        let err = run(
            &state,
            request(50, |f| f.until = Some("half past ten".into())),
        )
        .expect_err("an unreadable date must fail the call, not match nothing");
        let filter = err
            .downcast_ref::<crate::sessions::FilterError>()
            .expect("`from_anyhow` downcasts this into `invalid_params`");
        assert_eq!(filter.field, "until");
    }

    /// The session id inside `tests/fixtures/real_main_slice.jsonl`. Discovery takes the id from
    /// the *filename*, so the copy has to be named after it or nothing resolves.
    const FIXTURE_SESSION: &str = "b20208d8-fbdb-5918-ba69-d203de6ed6dc";

    #[test]
    fn a_real_index_lists_the_transcript_it_was_built_from() {
        // Everything above writes `sessions.json` by hand. This one runs the indexer, so the row
        // shape, the timestamps and the counts are the ones a real transcript produces — and a
        // change to what `load_sessions` stores cannot pass here by agreeing with a fixture that
        // was written to match it.
        let tmp = tempfile::tempdir().expect("a temp dir");
        let root = tmp.path().join("claude/projects");
        let project = root.join("-home-user-session-search");
        std::fs::create_dir_all(&project).expect("the transcript root");
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real_main_slice.jsonl");
        std::fs::copy(&fixture, project.join(format!("{FIXTURE_SESSION}.jsonl")))
            .expect("copying the fixture");

        let index_dir = tmp.path().join("index");
        let stats = crate::index::run(
            &index_dir,
            std::slice::from_ref(&root),
            &crate::index::IndexOptions::default(),
        )
        .expect("indexing the fixture");
        // Guards the assertions below: if the fixture stopped parsing they would pass against
        // an empty listing and prove nothing.
        assert!(stats.docs_added > 5, "fixture indexed {stats:?}");

        let (index, fields) = crate::index::open_or_create(&index_dir).expect("the index opens");
        let state = State {
            index,
            fields,
            index_dir,
            corpus: Corpus::default(),
            refresh_secs: 0,
            last_refresh: Mutex::new(None),
        };

        let res = run(&state, request(50, |_| {})).expect("a listing over the real index");
        assert_eq!(res.total, res.returned, "the fixture fits under the limit");
        let row = res
            .sessions
            .iter()
            .find(|s| s.session_id == FIXTURE_SESSION)
            .expect("the indexed session is listed");
        assert!(
            row.source_path
                .ends_with(&format!("{FIXTURE_SESSION}.jsonl"))
        );
        assert!(row.messages > 0, "a real transcript has messages: {row:?}");
        assert_eq!(
            row.last_timestamp.is_some(),
            row.last_ts_ms.is_some(),
            "the RFC3339 spelling is derived from the milliseconds, never separately"
        );

        // The same index, asked for a session that is not in it: still an envelope, not a bare
        // zero, and the retry drops the filter that caused it.
        let empty = run(&state, request(50, |f| f.session = Some("00000000".into())))
            .expect("a zero is a successful answer");
        assert_eq!(empty.total, 0);
        assert!(empty.sessions.is_empty());
        let no_results = empty.envelope.no_results.expect("a zero is never bare");
        assert_eq!(no_results.narrowest_filter.as_deref(), Some("session"));
    }
}
