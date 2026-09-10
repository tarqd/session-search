//! The retrieval evaluation harness (issue #21).
//!
//! `cargo test --test eval` asserts; `cargo test --test eval -- --nocapture` prints the tables.
//! Every run also writes `target/eval/report.md`, `target/eval/ablation.md`,
//! `target/eval/similar.md`, `target/eval/facets.md`, `target/eval/corpus.md`,
//! `target/eval/hits.md` and `target/eval/similar-hits.md`, which are the artifacts a pull
//! request pastes. [`docs/EVAL.md`](../../docs/EVAL.md) is the committed record: it splices
//! those files in verbatim at a named commit, so an issue can cite a number without retyping
//! it, and it is what should be regenerated when any of these numbers move.
//!
//! **This target measures. It never changes ranking.** A finding about `search.rs` belongs in an
//! issue, not in a patch smuggled in beside a number that improves because of it.
//!
//! What it is made of:
//!
//! * [`corpus`] — six synthetic transcripts, parsed by the real parser and indexed through the
//!   real schema, in two variants: with and without the `context_text` header.
//! * [`fixture`] — `tests/fixtures/eval_queries.json`: 39 queries across the five classes, with
//!   graded relevance and per-row provenance.
//! * [`metrics`] — recall@k, MRR and nDCG@10 over a closure, so a *different retrieval
//!   configuration* is scored by the same arithmetic.
//! * [`similar`] — issue #26's `MoreLikeThisQuery` as a fourth configuration, with the protocol
//!   that makes a query fixture answerable by a document-seeded search written out in full.
//! * [`report`] — markdown tables built to be diffed rather than admired.
//!
//! The floors below are the point of the whole thing. A query set that returns nothing scores
//! `0.000` on every metric, and a table of zeroes reads exactly like a table of results. So the
//! numbers a broken corpus would produce are asserted against instead: every scored query must
//! retrieve at least one relevant document, and every class must clear a floor. A tokenizer
//! change that guts recall breaks this build; it does not quietly print a worse number.

mod corpus;
mod fixture;
mod metrics;
mod report;
mod similar;

use std::collections::BTreeSet;
use std::path::PathBuf;

use corpus::{Corpus, Variant, doc_ref};
use fixture::{Class, EvalQuery, Fixture};
use metrics::{Report, evaluate};
use session_search::schema::Fields;
use session_search::search::{SearchRequest, facets, search};
use tantivy::Index;

/// The retrieval cutoff every table in this harness is reported at. Ten is what a person sees
/// on one screen of `session-search search`, and it is the default `--limit`'s order of
/// magnitude; nDCG is fixed at 10 independently (`metrics::NDCG_K`) so the two cannot drift.
const K: usize = 10;

/// Per-class floors for the shipped configuration, checked before the snapshot is compared.
///
/// These are not targets and not a leaderboard: they are set comfortably below what main scores
/// today, so ordinary ranking movement does not trip them and a collapse does. Raise one only
/// with the number that justifies it.
const FLOOR_RECALL: f64 = 0.70;
const FLOOR_MRR: f64 = 0.75;
const FLOOR_NDCG: f64 = 0.60;

/// The `SearchRequest` a fixture row means. Everything not named by the row is left at its
/// default, so the harness measures the search a person gets and not a tuned one.
fn request_from(query: &EvalQuery) -> SearchRequest {
    SearchRequest {
        query: Some(query.query.clone()),
        filters: query.filters.clone(),
        limit: K,
        include_thinking: query.include_thinking,
        ..SearchRequest::default()
    }
}

/// The run closure for a plain `search()` over one index: query in, ranked document references
/// out. This is the body issue #26 will supply a `MoreLikeThis` sibling for.
fn search_run<'a>(
    index: &'a Index,
    fields: &'a Fields,
) -> impl Fn(&EvalQuery) -> anyhow::Result<Vec<String>> + 'a {
    move |query: &EvalQuery| {
        let response = search(index, fields, &request_from(query))?;
        Ok(response.hits.iter().map(|hit| doc_ref(&hit.doc)).collect())
    }
}

/// Fixture, corpus and the validation that ties them together. Every test starts here, because
/// a stale document reference has to be an error before it can become a zero.
fn loaded() -> anyhow::Result<(Fixture, Corpus, BTreeSet<String>)> {
    let corpus = Corpus::load()?;
    let refs = corpus.refs();
    let fixture = Fixture::load()?;
    fixture.validate(&refs)?;
    Ok((fixture, corpus, refs))
}

fn artifact_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/eval")
}

/// Write one report artifact and print it, so `--nocapture` and the file agree.
fn publish(name: &str, contents: &str) -> anyhow::Result<()> {
    let dir = artifact_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(name), contents)?;
    println!("{contents}");
    Ok(())
}

/// Every reference the fixture grades resolves, every class is populated, every row is the shape
/// it claims. Split out from the metric tests so a fixture mistake reports itself as a fixture
/// mistake rather than as a retrieval result.
#[test]
fn the_fixture_and_the_corpus_agree() -> anyhow::Result<()> {
    let (fixture, corpus, refs) = loaded()?;
    assert!(
        corpus.docs().count() >= 60,
        "the corpus shrank to {} documents; the fixture is graded against a larger one",
        corpus.docs().count()
    );
    assert_eq!(
        refs.len(),
        corpus.docs().count(),
        "two documents share a reference, so a graded hit cannot be attributed"
    );
    // Every class the issue names, with the aggregation rows carrying their explanation.
    for class in Class::all() {
        assert!(
            fixture.queries.iter().any(|q| q.class == class),
            "class {} is empty",
            class.as_str()
        );
    }
    publish("corpus.md", &corpus.reference_table())?;
    Ok(())
}

/// The committed baseline: the shipped configuration, scored per class, snapshotted so that any
/// movement shows up as a diff someone has to look at.
#[test]
fn baseline_metrics_match_the_committed_table() -> anyhow::Result<()> {
    let (fixture, corpus, _) = loaded()?;
    let (index, fields) = corpus.index(Variant::WithContext)?;
    let run = search_run(&index, &fields);
    let report = evaluate(&fixture, Variant::WithContext.label(), &run, K)?;

    // The loud floor. A corpus that stopped parsing, an analyzer change that emptied the
    // dictionary and a fixture whose references went stale all produce the same tidy zeroes,
    // and none of them is a result.
    for row in report.rows.iter().filter(|r| r.scored) {
        assert!(
            row.found > 0,
            "query {:?} retrieved none of its {} relevant documents. Either retrieval broke or \
             the fixture is wrong; both are failures, neither is a score.",
            row.id,
            row.relevant
        );
    }
    for (class, m) in &report.by_class {
        if *class == Class::Aggregation {
            continue;
        }
        assert!(
            m.recall >= FLOOR_RECALL && m.mrr >= FLOOR_MRR && m.ndcg >= FLOOR_NDCG,
            "class {} fell through the floor: recall {:.3}, MRR {:.3}, nDCG {:.3}",
            class.as_str(),
            m.recall,
            m.mrr,
            m.ndcg
        );
    }

    let table = report.table();
    publish("report.md", &table)?;
    // Not snapshotted and not asserted on: the ranked lists are the authoring aid for whoever
    // grades the next query, and pinning them would turn every ranking nudge into a snapshot
    // conflict in a file nobody reads for its numbers.
    std::fs::write(
        artifact_dir().join("hits.md"),
        report::hits_listing(&report),
    )?;
    insta::assert_snapshot!(table);
    Ok(())
}

/// Issue #23's outstanding acceptance box: the same corpus, the same fixture and the same
/// arithmetic over an index built without the `context_text` header.
///
/// Two claims are asserted rather than left to the reader of the table: the paraphrase class —
/// the one the header exists for — strictly improves, and no class is worse off for it.
#[test]
fn the_context_header_is_what_answers_a_paraphrase() -> anyhow::Result<()> {
    let (fixture, corpus, _) = loaded()?;

    let (with_index, with_fields) = corpus.index(Variant::WithContext)?;
    let with_run = search_run(&with_index, &with_fields);
    let with = evaluate(&fixture, Variant::WithContext.label(), &with_run, K)?;

    let (without_index, without_fields) = corpus.index(Variant::WithoutContext)?;
    let without_run = search_run(&without_index, &without_fields);
    let without = evaluate(&fixture, Variant::WithoutContext.label(), &without_run, K)?;

    // The third arm: the per-document half of the header (project basename, branch, turn
    // prompt) with no session row behind it. Reported, not asserted on — it answers "how much
    // of the win is sessions.json" and nothing in the product depends on the answer.
    let (row_index, row_fields) = corpus.index(Variant::WithoutSessionRow)?;
    let row_run = search_run(&row_index, &row_fields);
    let without_session_row = evaluate(&fixture, Variant::WithoutSessionRow.label(), &row_run, K)?;

    let paraphrase_before = without.by_class[&Class::Paraphrase];
    let paraphrase_after = with.by_class[&Class::Paraphrase];
    assert!(
        paraphrase_after.recall > paraphrase_before.recall,
        "the paraphrase class is the one the header exists for, and it did not improve: \
         {:.3} -> {:.3}",
        paraphrase_before.recall,
        paraphrase_after.recall
    );
    for (class, after) in &with.by_class {
        let before = without.by_class[class];
        assert!(
            after.recall >= before.recall - 1e-9,
            "class {} lost recall to the context header: {:.3} -> {:.3}. The header is meant to \
             widen the matched set, never to push an answer out of the top {K}.",
            class.as_str(),
            before.recall,
            after.recall
        );
    }

    // The two class tables come first and the diff second, deliberately. The diff is the
    // claim — "the header bought this much" — and the class tables are what makes it readable:
    // they carry the `ceiling` count per class, which is the difference between "identifier did
    // not regress" and "seven of identifier's eight rows could not have moved in either
    // direction". A before/after pasted without them invites the first reading.
    let mut out = String::from("# The `context_text` header — issue #23\n\n");
    out.push_str(&report::class_table_only(&without));
    out.push('\n');
    out.push_str(&report::class_table_only(&with));
    out.push('\n');
    out.push_str(&Report::diff(&without, &with));
    out.push('\n');
    out.push_str(&report::query_delta_table(&without, &with));
    out.push_str("\n\n");
    out.push_str(&Report::diff(&without, &without_session_row));
    out.push_str(
        "\nThe second table is the third arm: `doc_to_json(doc, None, ..)`, which drops the \
         session title and the opening prompt but still composes a header out of the project \
         basename, the branch and the turn prompt. It measures the `sessions.json` half of the \
         feature, not the feature.\n",
    );
    out.push('\n');
    out.push_str(&report::class_table_only(&without_session_row));
    publish("ablation.md", &out)?;
    insta::assert_snapshot!(Report::diff(&without, &with));
    Ok(())
}

/// The analyzer invariants the issue names, as hard assertions over the indexed corpus rather
/// than as an average. A metric can absorb one of these breaking; an assertion cannot.
///
/// **Every assertion here has to be one identifier splitting can fail.** That is not automatic,
/// and getting it wrong is the quiet way this test becomes decoration. A transcript *about* the
/// tokenizer discusses `snippet` and `is_error` in prose, so a document that spells the query
/// verbatim answers it whether or not `SplitIdentifiers` ever ran — asserting against one of
/// those proves the corpus contains the word, not that the analyzer split anything. Worse,
/// `context_text` copies a session's first prompt onto *every* document of that session, so on
/// the `WithContext` index a word in the opening question is reachable from every document in
/// the file by a route that has nothing to do with splitting.
///
/// So the targets below are chosen against both leaks: each one is a document that does not
/// spell the query, reached from a session whose header does not spell it either. The check
/// that this is still true is mechanical — delete `.filter(SplitIdentifiers)` from both
/// analyzers in `src/tokenizer.rs` and every `splitting_only` assertion here must fail.
#[test]
fn analyzer_invariants_hold_over_the_indexed_corpus() -> anyhow::Result<()> {
    let corpus = Corpus::load()?;
    let (index, fields) = corpus.index(Variant::WithContext)?;
    // The same corpus without the header, for the assertions whose target is only reachable
    // through `context_text` on the shipped index. Scoring is irrelevant here — this test asks
    // what is retrievable, not in what order — so the ablated arm is a legitimate place to ask.
    let (bare_index, bare_fields) = corpus.index(Variant::WithoutContext)?;

    let hits_in = |index: &Index, fields: &Fields, query: &str| -> anyhow::Result<Vec<String>> {
        let request = SearchRequest {
            query: Some(query.to_string()),
            limit: 20,
            ..SearchRequest::default()
        };
        Ok(search(index, fields, &request)?
            .hits
            .iter()
            .map(|hit| doc_ref(&hit.doc))
            .collect())
    };
    let hits = |query: &str| hits_in(&index, &fields, query);
    let contains = |query: &str, doc: &str| -> anyhow::Result<()> {
        let found = hits(query)?;
        assert!(
            found.iter().any(|r| r == doc),
            "{query:?} did not retrieve {doc:?}; it retrieved {found:?}"
        );
        Ok(())
    };

    // The four invariants issue #21 names, each aimed at a document that has to be *split* into
    // reach. `splitting_only` marks them: remove the filter and these are the ones that go red.

    // splitting_only: `eval-facets:-:2` is the `grep -rn is_error src` tool call, in a session
    // about error facets whose header never says "iserror". Only the underscore-stripped whole
    // form puts it in reach of this query. (`eval-tokenizer:-:11` — asserted below — spells
    // "iserror" in prose, so it answers this query either way and proves nothing on its own.)
    contains("iserror", "eval-facets:-:2")?;
    // Not splitting_only — the tool call spells `is_error` — but the pair is the invariant:
    // both spellings have to reach the same document.
    contains("is_error", "eval-facets:-:2")?;

    // splitting_only: the ```rust fence holding `SnippetGenerator::create`, which spells neither
    // "snippet" nor "generator". Asserted on the ablated index because on the shipped one the
    // session's opening question ("Why does searching for snippet never find ...") rides along
    // in `context_text` on every document of the file, so the `WithContext` arm cannot tell
    // splitting from the header.
    let bare = hits_in(&bare_index, &bare_fields, "snippet")?;
    assert!(
        bare.iter().any(|r| r == "eval-tokenizer:-:4"),
        "\"snippet\" did not reach the SnippetGenerator fence without the header; it retrieved \
         {bare:?}"
    );

    // splitting_only: same document, the other half of the name.
    contains("generator", "eval-tokenizer:-:1")?;

    // And the plain-prose spellings, which a user does type and which must keep working — but
    // which the assertions above are what actually pin.
    contains("snippet", "eval-tokenizer:-:1")?;
    contains("iserror", "eval-tokenizer:-:11")?;
    contains("is_error", "eval-tokenizer:-:11")?;
    // splitting_only: `eval-tokenizer:-:2` names `open_or_create` only inside the tool result.
    contains("OpenOrCreate", "eval-tokenizer:-:2")?;
    contains("open_or_create", "eval-tokenizer:-:2")?;
    contains("openOrCreate", "eval-tokenizer:-:2")?;
    assert_eq!(
        hits("OpenOrCreate")?,
        hits("open_or_create")?,
        "the two spellings must produce the same term set, so they must retrieve the same list"
    );

    // A hash is one term. Pasting it back finds its document; no fragment of it finds anything,
    // because the fragments were never indexed.
    let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    contains(sha, "eval-tokenizer:-:7")?;
    for fragment in ["9f86d081", "884c7d65", "b0f00a08"] {
        assert!(
            hits(fragment)?.is_empty(),
            "{fragment:?} is a fragment of a hash the analyzer keeps whole, and it retrieved \
             something"
        );
    }
    // The first chunk of a uuid *is* a term, because the uuid was written with separators.
    contains("b20208d8", "b20208d8-fbdb-5918-ba69-d203de6ed6dc:-:7")?;

    // A path splits at its punctuation, so each component is searchable.
    contains("src/index.rs", "b20208d8-fbdb-5918-ba69-d203de6ed6dc:-:2")?;
    // A flag has to be quoted: unquoted, the leading `-` is the parser's negation operator.
    contains(
        "\"cargo build --release\"",
        "b20208d8-fbdb-5918-ba69-d203de6ed6dc:-:5",
    )?;

    // The prose half stems and the code half does not — the two facts that make the boundary
    // class a class.
    contains("compiled", "eval-notes:-:5")?;
    contains(
        "writer_with_num_threads",
        "b20208d8-fbdb-5918-ba69-d203de6ed6dc:-:3",
    )?;
    Ok(())
}

/// The aggregation-shaped rows, checked against the API that actually answers them.
///
/// Every one of these queries returns a ranked list too, and that list is the wrong answer: the
/// question is about a distribution. The rows are recorded in the report as `wrong shape` and
/// contribute to none of the three metrics; here they are checked against `facets()` instead.
#[test]
fn aggregation_shaped_queries_are_a_facet_not_a_ranking() -> anyhow::Result<()> {
    let (fixture, corpus, _) = loaded()?;
    let (index, fields) = corpus.index(Variant::WithContext)?;

    // What this class reports *instead of* a retrieval score. The three metric columns are em
    // dashes for these rows on purpose, and an em dash is not an answer — so the answer gets
    // written out here, in the same run, as the artifact issue #21's "per class" box needs for
    // the one class that has no per-class score.
    let mut table = String::from(
        "# Aggregation-shaped queries — what they report instead of a ranking\n\n\
         These six rows are recorded and never scored. Recall, MRR and nDCG are undefined for \
         them, not zero: the answer to \"what errors did we see\" is a distribution, and any \
         top-k over it is an arbitrary sample whose contents change with whatever `k` the \
         caller happened to pass. What is checked instead is `search::facets` — every bucket \
         the fixture names must come back non-empty — and what is reported is the bucket table \
         itself.\n\n\
         `docs matched` is the query's whole matching set, which is the denominator a \
         distribution is read against; `buckets returned` is how many distinct values the facet \
         found, which is the number a ranked list can never express, because one document can \
         increment several buckets and ten documents can be one.\n\n\
         | query | facet field | docs matched | buckets returned | expected buckets, with the counts that came back |\n\
         | --- | --- | --- | --- | --- |\n",
    );

    let mut checked = 0;
    for query in fixture.facet_queries() {
        let field = query
            .facet_field
            .as_deref()
            .expect("validate() requires a facet_field on a facet row");
        let result = facets(&index, &fields, field, &request_from(query))?;
        let expected: Vec<String> = query
            .expect_facet_values
            .iter()
            .map(|value| {
                let count = result
                    .values
                    .iter()
                    .find(|v| v.value == *value)
                    .map_or(0, |v| v.count);
                format!("`{value}` {count}")
            })
            .collect();
        table.push_str(&format!(
            "| `{}` | `{field}` | {} | {} | {} |\n",
            query.id,
            result.matching_docs,
            result.values.len(),
            expected.join(", "),
        ));
        assert!(
            result.matching_docs > 0,
            "facet query {:?} matched no documents at all, so its buckets say nothing",
            query.id
        );
        for expected in &query.expect_facet_values {
            let bucket = result
                .values
                .iter()
                .find(|v| v.value == *expected)
                .unwrap_or_else(|| {
                    panic!(
                        "facet query {:?} on {field:?} has no bucket for {expected:?}; it \
                         returned {:?}",
                        query.id, result.values
                    )
                });
            assert!(
                bucket.count > 0,
                "facet query {:?} returned an empty bucket for {expected:?}",
                query.id
            );
        }
        checked += 1;
    }
    assert!(
        checked >= 6,
        "only {checked} aggregation-shaped rows were checked"
    );
    publish("facets.md", &table)?;
    Ok(())
}

/// Issue #26's `MoreLikeThisQuery`, scored so that the next proposal for a neural reranker has
/// a number to beat rather than an intuition to argue with.
///
/// Read [`similar`]'s module documentation before reading the table this writes: the fixture
/// asks "which documents match these words" and this arm answers "which documents look like
/// this one", so the protocol that bridges the two — seed, discarded query string, kept
/// filters, narrowed graded set — is what the numbers are actually about.
///
/// Two assertions and no floor. A floor would be a claim that a particular similarity quality
/// is required, which nothing in the product depends on yet. What is asserted is that the
/// configuration *works*: it answers every query without erroring, it finds something relevant
/// overall, and with `--include-source` the seed's turn is **retrievable** — which is the
/// property whose absence would mean the query is not being built from the seed at all.
///
/// Retrievable, not first. See the comment on that assertion: "the source ranks first" is a
/// property of a single-document seed and is measurably false for a turn-shaped one.
#[test]
fn more_like_this_is_scored_as_the_similarity_baseline() -> anyhow::Result<()> {
    let (fixture, corpus, _) = loaded()?;
    let (index, fields) = corpus.index(Variant::WithContext)?;
    let placements = similar::Placements::of(&corpus);
    let narrowed = similar::narrow(&fixture, &placements);

    let text_run = similar::text_run(&index, &fields, &placements, &narrowed.seeds, K);
    let text = evaluate(
        &narrowed.fixture,
        "text query, seed's turn ungraded",
        &text_run,
        K,
    )?;

    let run = similar::similar_run(&index, &fields, &placements, &narrowed.seeds, K, false);
    let mlt = evaluate(
        &narrowed.fixture,
        "more like this, seeded from the top-graded document",
        &run,
        K,
    )?;

    assert!(
        mlt.overall.recall > 0.0,
        "the similarity arm retrieved nothing relevant on any query. An over-tuned \
         MoreLikeThis returns an empty BooleanQuery that matches nothing without erroring, so \
         this is what that failure looks like from the outside."
    );

    // The construction check: with `--include-source`, the seed's own turn is retrievable.
    //
    // Deliberately *not* "the seed ranks first". That is true of `MoreLikeThis` seeded from one
    // document — it matches every clause it generated, and Tantivy's own test asserts it — and
    // it is **false** here, measurably, because the seed is a whole turn. The query is built
    // from the union of the turn's documents, no single one of them carries all of it, and BM25
    // length normalisation then lets a short document elsewhere that concentrates the surviving
    // terms outrank every document of the turn the terms came from. `ident-source-path` is that
    // case on this corpus. It is a fact about turn-shaped seeds worth knowing, and it is the
    // reason the source turn is excluded by *filtering* rather than by trusting the ranking to
    // put it somewhere predictable.
    let with_source = similar::similar_run(&index, &fields, &placements, &narrowed.seeds, K, true);
    for query in &narrowed.fixture.queries {
        let hits = with_source(query)?;
        let seed = &narrowed.seeds[&query.id];
        let turn = placements.turn_of(seed);
        assert!(
            hits.iter().any(|hit| turn.contains(hit)),
            "query {:?} with --include-source retrieved none of its own seed turn {turn:?}; it \
             retrieved {hits:?}. The seed matches the terms the seed produced, so this means \
             the query was not built from the seed.",
            query.id
        );
    }

    let mut out = String::from("# More like this — issue #26\n\n");
    out.push_str(&similar::preamble(&narrowed, fixture.queries.len()));
    out.push('\n');
    out.push_str(&mlt.table());
    out.push_str("\n\n");
    out.push_str(&Report::diff(&text, &mlt));
    if !narrowed.dropped.is_empty() {
        out.push_str(&format!(
            "\nDropped from this arm: {}.\n",
            narrowed.dropped.join(", ")
        ));
    }
    publish("similar.md", &out)?;
    std::fs::write(
        artifact_dir().join("similar-hits.md"),
        report::hits_listing(&mlt),
    )?;
    insta::assert_snapshot!(report::class_table_only(&mlt));
    Ok(())
}
