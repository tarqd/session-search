//! Expand a hit to the turns around it, or reconstruct a whole session.
//!
//! Both entry points are targeted lookups, never a scan: a term query on `session_id` (plus
//! `agent_id`, or its *absence* for a main transcript, plus `source_path` when the caller knows
//! which file it means) narrows to one file's worth of docs, a range query on the `seq` fast
//! field narrows to the window, and `TopDocs` orders by `seq`.

use tantivy::query::{BooleanQuery, Occur, Query, RangeQuery};
use tantivy::schema::Term;

use crate::doc::Doc;
use crate::schema::Fields;
use crate::search::{docs_by_seq, session_clauses};

/// The `before`/`after` docs surrounding `seq` within one session (or subagent).
///
/// The returned window includes the doc at `seq` itself when it exists, and is ordered by
/// `seq` ascending. `seq` is a dense per-file ordinal, so the window is exactly
/// `[seq - before, seq + after]`. `source_path` narrows to the one file that numbering belongs
/// to; pass `None` to accept whichever file(s) carry the session id.
// Six of the eight arguments are the coordinates of a window in a specific transcript, and the
// signature is pinned in `docs/DESIGN.md`; bundling them into a struct would move the same
// fields behind one more name without making a call site clearer.
#[allow(clippy::too_many_arguments)]
pub fn around(
    index: &tantivy::Index,
    f: &Fields,
    session_id: &str,
    agent_id: Option<&str>,
    source_path: Option<&str>,
    seq: u64,
    before: usize,
    after: usize,
) -> anyhow::Result<Vec<Doc>> {
    let low = seq.saturating_sub(before as u64);
    let high = seq.saturating_add(after as u64);

    let mut clauses = session_clauses(f, session_id, agent_id, source_path);
    clauses.push((
        Occur::Must,
        Box::new(RangeQuery::new(
            std::ops::Bound::Included(Term::from_field_u64(f.seq, low)),
            std::ops::Bound::Included(Term::from_field_u64(f.seq, high)),
        )) as Box<dyn Query>,
    ));

    // `high - low + 1` is the exact size of the window; +1 keeps the arithmetic honest when
    // before/after are 0. `docs_by_seq` clamps it against the index, so a `--after 4000000000`
    // asks for a big window rather than aborting the process.
    let window = (high - low).saturating_add(1) as usize;
    let searcher = index.reader()?.searcher();
    docs_by_seq(&searcher, f, &BooleanQuery::new(clauses), window)
}

/// Every doc of a session (or subagent) in `seq` order, capped at `limit`.
pub fn session(
    index: &tantivy::Index,
    f: &Fields,
    session_id: &str,
    agent_id: Option<&str>,
    source_path: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<Doc>> {
    let clauses = session_clauses(f, session_id, agent_id, source_path);
    let searcher = index.reader()?.searcher();
    docs_by_seq(&searcher, f, &BooleanQuery::new(clauses), limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::DocKind;
    use crate::search::testkit::{blank_doc, index_docs};

    /// A main transcript of 10 docs, plus a 3-doc subagent file that numbers its own `seq`
    /// from zero, plus an unrelated session — the three ways a lookup can go wrong.
    fn corpus() -> Vec<Doc> {
        let mut docs = Vec::new();
        for seq in 0..10u64 {
            let mut d = blank_doc(seq);
            d.text = format!("main turn {seq}");
            if seq % 3 == 0 {
                d.kind = DocKind::ToolCall;
                d.tool_name = Some("Bash".into());
            }
            docs.push(d);
        }
        for seq in 0..3u64 {
            let mut d = blank_doc(seq);
            d.doc_id = format!("s1:a10845c5ff9c7d4ec:{seq}");
            d.source_path = "/tmp/s1/subagents/agent-a10845c5ff9c7d4ec.jsonl".into();
            d.agent_id = Some("a10845c5ff9c7d4ec".into());
            d.agent_type = Some("Explore".into());
            d.is_sidechain = true;
            d.text = format!("agent turn {seq}");
            docs.push(d);
        }
        for seq in 0..4u64 {
            let mut d = blank_doc(seq);
            d.doc_id = format!("s2:-:{seq}");
            d.session_id = "s2".into();
            d.source_path = "/tmp/s2.jsonl".into();
            d.text = format!("other session turn {seq}");
            docs.push(d);
        }
        docs
    }

    fn texts(docs: &[Doc]) -> Vec<String> {
        docs.iter().map(|d| d.text.clone()).collect()
    }

    #[test]
    fn around_returns_the_right_neighbours_in_seq_order() {
        let (index, f) = index_docs(&corpus());
        let got = around(&index, &f, "s1", None, None, 5, 2, 2).unwrap();
        assert_eq!(
            texts(&got),
            vec![
                "main turn 3",
                "main turn 4",
                "main turn 5",
                "main turn 6",
                "main turn 7"
            ]
        );
        assert!(got.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[test]
    fn around_is_asymmetric_when_asked_to_be() {
        let (index, f) = index_docs(&corpus());
        let got = around(&index, &f, "s1", None, None, 5, 0, 3).unwrap();
        assert_eq!(
            texts(&got),
            vec!["main turn 5", "main turn 6", "main turn 7", "main turn 8"]
        );

        let got = around(&index, &f, "s1", None, None, 5, 3, 0).unwrap();
        assert_eq!(
            texts(&got),
            vec!["main turn 2", "main turn 3", "main turn 4", "main turn 5"]
        );

        let got = around(&index, &f, "s1", None, None, 5, 0, 0).unwrap();
        assert_eq!(texts(&got), vec!["main turn 5"]);
    }

    #[test]
    fn around_clamps_at_both_ends_of_the_file() {
        let (index, f) = index_docs(&corpus());
        let got = around(&index, &f, "s1", None, None, 0, 5, 1).unwrap();
        assert_eq!(texts(&got), vec!["main turn 0", "main turn 1"]);

        let got = around(&index, &f, "s1", None, None, 9, 1, 5).unwrap();
        assert_eq!(texts(&got), vec!["main turn 8", "main turn 9"]);
    }

    #[test]
    fn a_subagent_never_bleeds_into_the_main_transcript() {
        let (index, f) = index_docs(&corpus());
        // Both files number seq from 0 under the same session_id.
        let main = around(&index, &f, "s1", None, None, 1, 1, 1).unwrap();
        assert_eq!(
            texts(&main),
            vec!["main turn 0", "main turn 1", "main turn 2"]
        );
        assert!(main.iter().all(|d| d.agent_id.is_none()));

        let agent = around(&index, &f, "s1", Some("a10845c5ff9c7d4ec"), None, 1, 1, 1).unwrap();
        assert_eq!(
            texts(&agent),
            vec!["agent turn 0", "agent turn 1", "agent turn 2"]
        );
        assert!(agent.iter().all(|d| d.agent_id.is_some()));
    }

    #[test]
    fn another_session_is_never_included() {
        let (index, f) = index_docs(&corpus());
        let got = around(&index, &f, "s2", None, None, 1, 5, 5).unwrap();
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|d| d.session_id == "s2"));
    }

    #[test]
    fn session_returns_everything_in_order_up_to_the_limit() {
        let (index, f) = index_docs(&corpus());
        let all = session(&index, &f, "s1", None, None, 100).unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(all.first().unwrap().text, "main turn 0");
        assert_eq!(all.last().unwrap().text, "main turn 9");
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));

        // The cap takes the *first* docs, not an arbitrary slice.
        let head = session(&index, &f, "s1", None, None, 3).unwrap();
        assert_eq!(
            texts(&head),
            vec!["main turn 0", "main turn 1", "main turn 2"]
        );

        let agent = session(&index, &f, "s1", Some("a10845c5ff9c7d4ec"), None, 100).unwrap();
        assert_eq!(agent.len(), 3);
    }

    #[test]
    fn an_unknown_session_is_empty_not_an_error() {
        let (index, f) = index_docs(&corpus());
        assert!(
            session(&index, &f, "nope", None, None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            around(&index, &f, "nope", None, None, 0, 5, 5)
                .unwrap()
                .is_empty()
        );
        assert!(
            session(&index, &f, "s1", Some("no-such-agent"), None, 10)
                .unwrap()
                .is_empty()
        );
    }

    /// A real transcript slice: the window `show --around` produces must be contiguous and in
    /// conversational order, whatever mix of docs `parse.rs` emitted for those records.
    #[test]
    fn around_over_a_real_transcript_slice() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/real_main_slice.jsonl");
        let out = crate::agents::claude::parse::parse_whole(
            &fixture,
            &crate::doc::ParseOptions::default(),
        )
        .unwrap();
        let (index, f) = index_docs(&out.docs);
        let session_id = out.docs[0].session_id.clone();

        let all = session(&index, &f, &session_id, None, None, 1000).unwrap();
        assert_eq!(all.len(), out.docs.len());
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));

        let middle = all[all.len() / 2].seq;
        let window = around(&index, &f, &session_id, None, None, middle, 2, 2).unwrap();
        assert_eq!(window.len(), 5);
        assert_eq!(window[2].seq, middle);
        assert!(window.windows(2).all(|w| w[1].seq == w[0].seq + 1));
    }

    #[test]
    fn stored_fields_survive_the_round_trip() {
        let mut source = corpus();
        source[3].is_error = true;
        source[3].is_meta = true;
        let (index, f) = index_docs(&source);
        let got = around(&index, &f, "s1", None, None, 3, 0, 0).unwrap();
        let doc = &got[0];
        assert_eq!(doc.kind, DocKind::ToolCall);
        assert_eq!(doc.tool_name.as_deref(), Some("Bash"));
        assert_eq!(doc.doc_id, "s1:-:3");
        assert_eq!(doc.source_path, "/tmp/s1.jsonl");
        assert!(doc.timestamp_ms.is_some());
        assert_eq!(doc.project.as_deref(), Some("/home/user/session-search"));
        // The three flags are read out of the stored payload, so they have to be stored;
        // reporting them as `false` would contradict the filters that select on them.
        assert!(doc.is_error);
        assert!(doc.is_meta);
        assert!(!doc.is_sidechain);
        let agent = around(&index, &f, "s1", Some("a10845c5ff9c7d4ec"), None, 0, 0, 0).unwrap();
        assert!(agent[0].is_sidechain);
    }

    /// `seq` numbers documents within one *file*. Two transcripts can share a `sessionId`
    /// (TRANSCRIPT-FORMAT §9), and a window scoped only by session id then interleaves them
    /// and silently drops the neighbours it was asked for.
    #[test]
    fn a_window_is_scoped_to_one_transcript_file() {
        let mut docs = corpus();
        // A second file, same session id, its own seq numbering.
        for seq in 0..5u64 {
            let mut d = blank_doc(seq);
            d.doc_id = format!("s1:-:relocated:{seq}");
            d.source_path = "/tmp/relocated/s1.jsonl".into();
            d.project = Some("/home/user/elsewhere".into());
            d.text = format!("relocated turn {seq}");
            docs.push(d);
        }
        let (index, f) = index_docs(&docs);

        let window = around(&index, &f, "s1", None, Some("/tmp/s1.jsonl"), 2, 1, 1).unwrap();
        assert_eq!(
            texts(&window),
            vec!["main turn 1", "main turn 2", "main turn 3"]
        );

        let other = around(
            &index,
            &f,
            "s1",
            None,
            Some("/tmp/relocated/s1.jsonl"),
            2,
            1,
            1,
        )
        .unwrap();
        assert_eq!(
            texts(&other),
            vec!["relocated turn 1", "relocated turn 2", "relocated turn 3"]
        );

        // Whole-session views are scoped the same way.
        let one_file = session(&index, &f, "s1", None, Some("/tmp/s1.jsonl"), 100).unwrap();
        assert_eq!(one_file.len(), 10);
        assert!(one_file.iter().all(|d| d.source_path == "/tmp/s1.jsonl"));
    }
}
