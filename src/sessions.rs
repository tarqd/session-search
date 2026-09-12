//! Filtering the session list — the one matcher every front end uses.
//!
//! A session listing does not come from Tantivy. It comes from `sessions.json` (see
//! [`crate::index::load_sessions`]), one row per transcript file, and a [`SessionInfo`] carries
//! none of the per-message fields the index does. So every front end that offers `sessions` has
//! to answer two questions of its own: which of the [`Filters`] this row satisfies, and which of
//! them could never have been asked here at all.
//!
//! Both answers used to be written once per front end — `cli.rs` and `api/mod.rs` each held a
//! private `SessionMatcher`, the second with a comment defending the copy — and the two had
//! already drifted on the second question: one warned about `--program` and `--lang` and said
//! nothing about `--min-thinking`, the other the reverse. That is the failure mode this module
//! exists to end: a filter silently dropped on one surface and reported on another is invisible
//! to a reader of either one, and a third front end would have made it a three-way disagreement.
//!
//! What stays with each front end is only how it *reports*: the CLI logs a `tracing::warn!`, the
//! HTTP API returns the sentences in the response body, the MCP tool carries them in its
//! envelope's `warnings`, and each names the search surface its own caller can reach (see
//! [`SearchSurface`]). The list itself is [`unanswerable_filters`], here, once.

use crate::parse::SessionInfo;
use crate::search::{self, Edge, Filters, when_ms};

/// The subset of [`Filters`] that `sessions.json` can answer, pre-resolved once.
///
/// Pre-resolved because the dates are the expensive and the fallible part: `--since 7d` is
/// relative to *now*, and resolving it inside [`Self::matches`] would both re-parse it per row
/// and let `now` move underneath a listing, so the first row and the last would be measured
/// against different windows. Parsing once in [`Self::new`] also makes an unreadable date fail
/// the command instead of quietly matching nothing.
#[derive(Debug, Clone)]
pub struct SessionMatcher {
    project: Option<String>,
    branch: Option<String>,
    session: Option<String>,
    agent_type: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    no_sidechains: bool,
    sidechains_only: bool,
}

/// A `--since` / `--until` value that could not be read.
///
/// `field` is the plain name (`"since"`), never a flag: this matcher serves a CLI that spells it
/// `--since`, an HTTP API that spells it `since=`, and whatever comes next. Baking either
/// spelling in here would put a flag that does not exist into an HTTP 400 — a caller told to fix
/// something they cannot type. Each front end renders `field` in its own dialect;
/// `api::tests::a_malformed_date_is_a_400_wherever_it_arrives` pins that the HTTP side never
/// says `--since`.
#[derive(Debug, thiserror::Error)]
#[error("parsing {field}: {source:#}")]
pub struct FilterError {
    pub field: &'static str,
    #[source]
    pub source: anyhow::Error,
}

impl SessionMatcher {
    pub fn new(f: &Filters) -> Result<SessionMatcher, FilterError> {
        // One `now` for both ends and every row, for the reason in the type's doc comment.
        let now = chrono::Utc::now();
        let at = |raw: &Option<String>, field: &'static str, edge: Edge| {
            set(raw)
                .map(|s| when_ms(s, now, edge))
                .transpose()
                .map_err(|source| FilterError { field, source })
        };
        Ok(SessionMatcher {
            project: set(&f.project).map(search::expand_tilde),
            branch: set(&f.branch).map(str::to_string),
            session: set(&f.session).map(str::to_string),
            agent_type: set(&f.agent_type).map(str::to_string),
            since_ms: at(&f.since, "since", Edge::Lower)?,
            until_ms: at(&f.until, "until", Edge::Upper)?,
            no_sidechains: f.no_sidechains,
            sidechains_only: f.sidechains_only,
        })
    }

    pub fn matches(&self, info: &SessionInfo) -> bool {
        // Path-aware, exactly as the index-side filter is: `-p /home/user/alpha` must not drag
        // in the sibling `/home/user/alpha-beta`.
        if let Some(prefix) = &self.project
            && !info
                .project
                .as_deref()
                .is_some_and(|p| search::path_has_prefix(p, prefix))
        {
            return false;
        }
        if let Some(branch) = &self.branch
            && info.git_branch.as_deref() != Some(branch.as_str())
        {
            return false;
        }
        // A session id is long enough that a prefix is a convenience, not an ambiguity.
        if let Some(session) = &self.session
            && !info.session_id.starts_with(session.as_str())
        {
            return false;
        }
        if let Some(kind) = &self.agent_type
            && info.agent_type.as_deref() != Some(kind.as_str())
        {
            return false;
        }
        if self.no_sidechains && info.agent_id.is_some() {
            return false;
        }
        if self.sidechains_only && info.agent_id.is_none() {
            return false;
        }
        // A session overlaps the window if it ended after `since` and started before `until`.
        if let Some(since) = self.since_ms
            && info
                .last_ts_ms
                .or(info.first_ts_ms)
                .is_some_and(|t| t < since)
        {
            return false;
        }
        if let Some(until) = self.until_ms
            && info
                .first_ts_ms
                .or(info.last_ts_ms)
                .is_some_and(|t| t > until)
        {
            return false;
        }
        true
    }
}

/// A filter that arrived as `Some("")` — or as whitespace — is the same as absent.
///
/// Clients that fill every optional field with an empty string are common, and every other
/// reader of [`Filters`] already defends against the habit: `search::non_empty` on the index
/// side, `envelope::opt` in the filter echo and the ranking, `envelope::resolve_time_range` on
/// the dates, `mcp::tools::drill::given` on the addresses. This matcher did not, and the
/// disagreement was visible from outside: `search_sessions {since: ""}` failed with
/// `-32602 parsing since: cannot read ""`, while `search_turns {query: "indexer", since: ""}`
/// answered normally — the same value, an error on one tool and ignored on the other.
///
/// The string filters need it as much as the dates, and for a quieter reason: an empty `branch`
/// or `agent_type` matches no row at all, while `envelope::opt` drops it from the echo, so the
/// zero comes back with no filter named and nothing to retry without.
fn set(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The filters that arrived and that a session listing cannot answer, in [`Filters`] declaration
/// order.
///
/// Twelve of them, and the list is the *complement* of what [`SessionMatcher`] reads rather than
/// a remembered subset — that is the whole point of it living beside the matcher. A
/// [`SessionInfo`] is a per-transcript row: it holds a project, a branch, a session id, an agent
/// type and two timestamps, and nothing about any individual message. So `tool`, `tool_input`,
/// `tool_output`, `lang`, `min_thinking`, `program`, `model`, `role`, `kind` and `errors_only`
/// have no column to be compared against, at any cost — they are not slow here, they are
/// unanswerable.
///
/// `turn_of` and `turn_seq` are here for a sharper version of the same reason, and `turn_of` is
/// the one that has to be argued: a row *does* carry a `source_path`, so half the address looks
/// answerable. It is not. The pair names a turn, and the coarsest thing this listing can return
/// is a whole session — matching the file alone would answer "the session that turn is in",
/// which is a different question, and would report a narrowing that the ordinal never got. Half
/// an answer to an address is indistinguishable from a whole one in the result.
///
/// `all_records` is the one filter that belongs to neither list. It is not answerable and not
/// unanswerable: it widens the *document* scope, and a session listing has no document scope to
/// widen. Every row of `sessions.json` is returned either way, so nothing was refused and
/// nothing was dropped. Reporting it as ignored would be a warning about a loss that did not
/// happen, which teaches a caller to skim past the warnings that describe real ones.
///
/// Silently dropping one is the outcome this refuses. A listing filtered by eleven of twelve
/// filters looks exactly like a listing filtered by twelve, and the caller reads "no session
/// used that model" out of a result that means "that question cannot be asked here".
///
/// Names are the plain field names, not flags: `cli.rs` renders `--tool-input` and the HTTP API
/// renders `tool_input`, and the spelling is the front end's business (see [`FilterError`]).
pub fn unanswerable_filters(f: &Filters) -> Vec<&'static str> {
    [
        ("tool", !f.tool.is_empty()),
        ("tool_input", !f.tool_input.is_empty()),
        ("tool_output", !f.tool_output.is_empty()),
        ("lang", !f.lang.is_empty()),
        ("min_thinking", f.min_thinking.is_some()),
        ("program", !f.program.is_empty()),
        ("model", f.model.is_some()),
        ("role", f.role.is_some()),
        ("kind", f.kind.is_some()),
        ("turn_of", f.turn_of.is_some()),
        ("turn_seq", f.turn_seq.is_some()),
        ("errors_only", f.errors_only),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

/// The front end asking, and with it the only word of [`unanswerable_filter_notes`] that is not
/// shared: what its own caller can call the search operation.
///
/// The note has to end somewhere a reader can go, and every front end reaches search by a
/// different name. `/api/search` is a URL an MCP client cannot request and a shell cannot run;
/// `search_turns` is a tool name nothing outside an MCP session can invoke. A note that names
/// the wrong one is worse than a note with no pointer at all: it reads as an answer, so the
/// caller stops looking, and what they were told to do next does not exist for them.
///
/// The pointer is a parameter rather than the list being forked, and that is the point of this
/// module. A second sentence for MCP would have arrived with a second copy of the ten filters
/// beside it, and the two would have drifted exactly as `cli.rs` and `api/mod.rs` did before the
/// list was extracted — one surface warning about `--program`, the other about `--min-thinking`,
/// neither reader able to see the disagreement. One list, one reasoning, three names for the
/// door out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSurface {
    /// `GET /api/sessions`, whose caller can request `GET /api/search`. The original spelling,
    /// and part of that endpoint's response text since it first returned warnings.
    HttpApi,
    /// The `search_sessions` MCP tool, whose caller can call the `search_turns` tool. It has no
    /// stderr and no way to make an HTTP request; the tool name is the only address it has.
    McpTool,
    /// `session-search sessions`, whose reader can run `session-search search` in the same
    /// shell.
    Cli,
}

impl SearchSurface {
    /// The search operation, spelled the way *this* front end's caller invokes it.
    pub fn search_pointer(self) -> &'static str {
        match self {
            SearchSurface::HttpApi => "/api/search",
            SearchSurface::McpTool => "the `search_turns` tool",
            SearchSurface::Cli => "`session-search search`",
        }
    }
}

/// [`unanswerable_filters`] as sentences a caller can be handed verbatim.
///
/// For any front end that answers in data rather than on stderr — the HTTP API's
/// `warnings` array, and an agent reading a tool result, which has no stderr at all. Each names
/// the filter, says why the listing could not apply it, and points at the surface that can, in
/// the dialect [`SearchSurface`] picks.
pub fn unanswerable_filter_notes(f: &Filters, surface: SearchSurface) -> Vec<String> {
    let pointer = surface.search_pointer();
    unanswerable_filters(f)
        .into_iter()
        .map(|name| {
            format!(
                "`{name}` was ignored: a session listing reads sessions.json, which records one \
                 row per transcript and carries no per-message fields. Use {pointer} for it."
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters_with(mutate: impl FnOnce(&mut Filters)) -> Filters {
        let mut f = Filters::default();
        mutate(&mut f);
        f
    }

    /// Everything a `Filters` can carry, set at once.
    ///
    /// `every_filter_is_answered_named_unanswerable_or_deliberately_neither` pins that this is
    /// literally everything, against the struct rather than against a count — so a field added
    /// to `Filters` cannot slip past the tests below by simply not being mentioned in them,
    /// which is exactly how `all_records`, `turn_of` and `turn_seq` arrived unclassified.
    fn every_filter_set() -> Filters {
        filters_with(|f| {
            f.project = Some("/home/user".into());
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
            f.session = Some("b20208d8".into());
            f.agent_type = Some("Explore".into());
            f.since = Some("7d".into());
            f.until = Some("now".into());
            f.all_records = true;
            f.turn_of = Some("/home/user/.claude/projects/p/abc.jsonl".into());
            f.turn_seq = Some(12);
            f.errors_only = true;
            f.no_sidechains = true;
            f.sidechains_only = true;
        })
    }

    /// The filters a `SessionInfo` row can actually be compared against — what
    /// [`SessionMatcher::matches`] reads, named here so the three-way split below is exhaustive.
    const ANSWERED_BY_MATCHER: &[&str] = &[
        "project",
        "branch",
        "session",
        "agent_type",
        "since",
        "until",
        "no_sidechains",
        "sidechains_only",
    ];

    /// The filter that is neither answered nor unanswerable, and the reason it is neither.
    ///
    /// `all_records` widens the *document* scope, and a session listing has no document scope:
    /// every row of `sessions.json` is returned with or without it, so nothing was refused and
    /// nothing was dropped. Calling it ignored would warn about a loss that did not happen.
    const INERT_HERE: &[&str] = &["all_records"];

    #[test]
    fn every_filter_is_answered_named_unanswerable_or_deliberately_neither() {
        // The three buckets must partition `Filters` exactly. The two tests below check each
        // bucket behaves; this one checks that no field is in none of them — the failure that
        // has no symptom, because a filter nobody classified is a filter nobody warns about.
        let all = every_filter_set();
        assert_eq!(
            crate::search::testkit::filter_fields_set(&all),
            crate::search::testkit::filter_field_names(),
            "the fixture must set every field of `Filters`, or the split below is not exhaustive"
        );

        let mut covered: std::collections::BTreeSet<String> = unanswerable_filters(&all)
            .into_iter()
            .map(str::to_string)
            .collect();
        for name in ANSWERED_BY_MATCHER.iter().chain(INERT_HERE) {
            assert!(
                covered.insert((*name).to_string()),
                "`{name}` is both answered here and reported as unanswerable"
            );
        }
        assert_eq!(
            covered,
            crate::search::testkit::filter_field_names(),
            "every field of `Filters` is answered by the matcher, named unanswerable, or listed \
             in INERT_HERE with a reason"
        );
    }

    #[test]
    fn the_unanswerable_filter_list_is_the_same_on_both_front_ends() {
        // Everything a `Filters` can carry, set at once: the answer is the complement of what
        // `SessionMatcher` reads, so this is the list both front ends must report.
        let f = filters_with(|f| {
            f.project = Some("/home/user".into());
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
            f.session = Some("b20208d8".into());
            f.agent_type = Some("Explore".into());
            f.since = Some("7d".into());
            f.until = Some("now".into());
            f.all_records = true;
            f.turn_of = Some("/home/user/.claude/projects/p/abc.jsonl".into());
            f.turn_seq = Some(12);
            f.errors_only = true;
            f.no_sidechains = true;
        });
        let expected = [
            "tool",
            "tool_input",
            "tool_output",
            "lang",
            "min_thinking",
            "program",
            "model",
            "role",
            "kind",
            // Both halves of the address, never one: a row carries a `source_path`, so matching
            // on `turn_of` alone would answer "the session that turn is in" and report a
            // narrowing the ordinal never got. `all_records` is absent — see `INERT_HERE`.
            "turn_of",
            "turn_seq",
            "errors_only",
        ];
        assert_eq!(unanswerable_filters(&f), expected);

        // The prose the HTTP API returns is the same list, one sentence each — so the surface
        // that answers in data and the surface that answers on stderr cannot report different
        // sets. `cli::tests::the_sessions_command_names_every_filter_it_cannot_apply_as_a_flag`
        // pins the other rendering.
        let notes = unanswerable_filter_notes(&f, SearchSurface::HttpApi);
        assert_eq!(notes.len(), expected.len());
        for (note, name) in notes.iter().zip(expected) {
            assert!(note.starts_with(&format!("`{name}` was ignored")), "{note}");
        }
    }

    #[test]
    fn every_front_end_points_the_reader_at_a_surface_it_can_actually_reach() {
        // One note, three front ends. Everything up to the pointer must be byte-identical —
        // that is the shared list and the shared reason — and the pointer itself must be
        // something this particular caller can invoke. An MCP client cannot issue
        // `GET /api/search`, and a shell cannot either; a note that names it there sends the
        // reader somewhere that does not exist for them, which reads as an answer and stops
        // them looking.
        let f = filters_with(|f| f.model = Some("claude-opus-5".into()));
        let ending = |surface| {
            let notes = unanswerable_filter_notes(&f, surface);
            assert_eq!(notes.len(), 1, "one unanswerable filter, one note");
            let note = notes[0].clone();
            let (shared, pointer) = note
                .split_once("Use ")
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .expect("every note ends by naming a surface that can answer it");
            (shared, pointer)
        };
        let (http_shared, http) = ending(SearchSurface::HttpApi);
        let (mcp_shared, mcp) = ending(SearchSurface::McpTool);
        let (cli_shared, cli) = ending(SearchSurface::Cli);
        assert_eq!(http_shared, mcp_shared);
        assert_eq!(http_shared, cli_shared);

        assert_eq!(http, "/api/search for it.");
        assert_eq!(mcp, "the `search_turns` tool for it.");
        assert_eq!(cli, "`session-search search` for it.");

        // The two ways to get this wrong, stated as the reader would hit them: an HTTP route
        // handed to a caller with no HTTP, and a tool name handed to a caller with no tools.
        for (surface, unreachable) in [
            (SearchSurface::McpTool, "/api/"),
            (SearchSurface::Cli, "/api/"),
            (SearchSurface::HttpApi, "search_turns"),
        ] {
            let note = &unanswerable_filter_notes(&f, surface)[0];
            assert!(
                !note.contains(unreachable),
                "{surface:?} must not name {unreachable}: {note}"
            );
        }
    }

    #[test]
    fn a_filter_the_matcher_reads_is_never_called_unanswerable() {
        // The other half of the same contract: everything `SessionMatcher::matches` consults
        // must be absent from the list, or a working filter would be reported as ignored.
        for f in [
            filters_with(|f| f.project = Some("/home/user".into())),
            filters_with(|f| f.branch = Some("main".into())),
            filters_with(|f| f.session = Some("b20208d8".into())),
            filters_with(|f| f.agent_type = Some("Explore".into())),
            filters_with(|f| f.since = Some("7d".into())),
            filters_with(|f| f.until = Some("now".into())),
            filters_with(|f| f.no_sidechains = true),
            filters_with(|f| f.sidechains_only = true),
        ] {
            assert!(
                unanswerable_filters(&f).is_empty(),
                "answerable filters must not be reported as ignored"
            );
        }
        assert!(unanswerable_filters(&Filters::default()).is_empty());
    }

    #[test]
    fn a_bare_day_covers_the_whole_day_at_both_ends_of_a_session_window() {
        // The `Edge` decision, seen through the matcher that depends on it: a session that ran
        // late on 2026-09-09 is inside `--until 2026-09-09`, which an upper bound of midnight
        // would have dropped without saying so.
        let late = SessionInfo {
            session_id: "s1".into(),
            first_ts_ms: Some(1_788_980_839_000), // 2026-09-09T19:07:19Z
            last_ts_ms: Some(1_788_980_899_000),
            ..SessionInfo::default()
        };
        let matcher = SessionMatcher::new(&filters_with(|f| {
            f.since = Some("2026-09-09".into());
            f.until = Some("2026-09-09".into());
        }))
        .expect("both dates parse");
        assert!(matcher.matches(&late));
    }

    #[test]
    fn a_filter_sent_as_an_empty_string_is_absent_rather_than_unmatchable() {
        // Clients that fill every optional field with `""` are common, and the two front ends
        // used to disagree about what that means: `search_sessions {since: ""}` came back
        // `-32602 parsing since: cannot read ""`, while `search_turns {query: "indexer",
        // since: ""}` succeeded — `envelope::resolve_time_range` filters whitespace-only values
        // and this constructor did not.
        let blank = filters_with(|f| {
            f.since = Some("  ".into());
            f.until = Some(String::new());
            f.project = Some(String::new());
            f.branch = Some(String::new());
            f.session = Some(String::new());
            f.agent_type = Some("  ".into());
        });
        let matcher = SessionMatcher::new(&blank)
            .expect("an empty string is a field nobody filled in, not an unreadable date");

        // And absent means absent: an empty `branch` must not exclude every row that has one,
        // which is a zero with no filter in the echo to explain it.
        let row = SessionInfo {
            session_id: "s1".into(),
            project: Some("/home/user/session-search".into()),
            git_branch: Some("main".into()),
            first_ts_ms: Some(1_788_980_839_000),
            last_ts_ms: Some(1_788_980_899_000),
            ..SessionInfo::default()
        };
        assert!(matcher.matches(&row));
        // The same fields with real values still filter, so nothing was traded away for this.
        let real = SessionMatcher::new(&filters_with(|f| f.branch = Some("other".into())))
            .expect("a branch name parses");
        assert!(!real.matches(&row));
    }

    #[test]
    fn an_unreadable_date_names_the_field_without_naming_a_flag() {
        let err = SessionMatcher::new(&filters_with(|f| f.since = Some("yesterday-ish".into())))
            .expect_err("an unreadable date must fail the listing, not match nothing");
        assert_eq!(err.field, "since");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("since"), "{rendered}");
        assert!(
            !rendered.contains("--since"),
            "the flag spelling belongs to the CLI, not to the shared matcher: {rendered}"
        );
    }
}
