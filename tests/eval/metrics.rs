//! The metric core: graded relevance in, per-class retrieval metrics out.
//!
//! Everything here is driven by one closure, and that is the load-bearing design decision.
//! A metric implementation that reached for `search::search` directly would measure exactly one
//! retrieval configuration forever; the harness has to score at least three
//! (`context_text` on, `context_text` off, and — once issue #26 lands — a `MoreLikeThis` query
//! seeded from a document) against the same corpus, the same fixture and the same arithmetic, or
//! the numbers cannot be compared with each other. So [`evaluate`] takes a
//! [`Run`]: query in, ranked document references out, nothing else.
//!
//! The three metrics, and why these three:
//!
//! * **recall@k** — did the answer come back at all? It is the only one of the three that a
//!   ranking change cannot flatter, which is why the harness's floor assertions are written
//!   against it. Denominator is every document graded 1 or better, so a query with ten relevant
//!   documents and a limit of ten can reach 1.000 and no query can reach it by accident.
//! * **MRR** — how far down was the first useful answer? A search tool is used by reading from
//!   the top, so the rank of the first hit that is any good is the number a person feels.
//! * **nDCG@10** — the graded metric. Grades run 0-3 in the fixture (3 = this document *is* the
//!   answer, 1 = a reader would keep it), gain is `2^g - 1` and the discount `log2(i + 2)`,
//!   scored against the ideal ordering of that query's graded set. It is the only one of the
//!   three that can tell "the right document, ranked first" from "the right document, ranked
//!   ninth behind eight near-misses".
//!
//! Aggregation-shaped queries are **not** scored by any of them, and that is a finding rather
//! than an omission: "what errors did we see" has a facet table for an answer and no defensible
//! top-k, so scoring it as a ranking would book a modelling mistake as a retrieval miss and push
//! whoever reads the table toward tuning the ranker to fix it. Those rows are counted, listed
//! with the fixture's note explaining why, and checked separately against the facet API.

use std::collections::BTreeMap;

use crate::fixture::{Class, EvalQuery, Fixture, Shape};

/// Rank cutoff for nDCG. Fixed at 10 independently of the recall cutoff: nDCG is reported as
/// `nDCG@10` in the table and in every write-up of it, so it must not silently follow `k`.
pub const NDCG_K: usize = 10;

/// A retrieval configuration under test: a query in, ranked document references out.
///
/// `&dyn Fn` rather than `&mut dyn FnMut` because every configuration the harness scores is a
/// read-only search over an already-built index, and a shared closure can be handed to two
/// evaluations in the same test without a borrow dance. The `Result` is not decoration: a
/// configuration that errors on one query must fail the run loudly, not score it as zero.
pub type Run<'a> = &'a dyn Fn(&EvalQuery) -> anyhow::Result<Vec<String>>;

/// One query's outcome. `scored` is false for the aggregation-shaped rows.
#[derive(Debug, Clone)]
pub struct QueryRow {
    pub id: String,
    pub class: Class,
    pub scored: bool,
    /// Documents the run closure returned, capped by the caller's `k`.
    pub retrieved: usize,
    /// Documents graded 1 or better in the fixture.
    pub relevant: usize,
    /// Of those, how many came back.
    pub found: usize,
    /// This query's recall@k was arithmetically forced to `1.000` by the fixture rather than
    /// earned by the ranker. See [`at_recall_ceiling`] for exactly what that means.
    pub ceiling: bool,
    /// The same fact about nDCG: this query's nDCG@[`NDCG_K`] could not have been anything but
    /// `1.000`. See [`at_ndcg_ceiling`].
    pub ndcg_pinned: bool,
    pub recall: f64,
    pub mrr: f64,
    pub ndcg: f64,
    /// The fixture's note, carried through verbatim so the appendix explains itself.
    pub note: Option<String>,
    /// The ranked list itself, each entry flagged with the grade the fixture gave it. Not in
    /// any table — it is written to `target/eval/hits.md`, which is what someone grading a new
    /// query reads instead of guessing.
    pub hits: Vec<(String, u8)>,
}

/// Means over the scored queries of one class, plus the counts they were taken over.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClassMetrics {
    /// Every query of the class, scored or not.
    pub queries: usize,
    /// The ones the three metrics were averaged over.
    pub scored: usize,
    /// Of those, how many could not have scored anything but `recall = 1.000`. A class whose
    /// `ceiling` equals its `scored` has a recall column that measures the fixture.
    pub ceiling: usize,
    /// Of those, how many could not have scored anything but `nDCG = 1.000`. Same failure mode
    /// as `ceiling`, on the metric the write-ups tell readers to reason from.
    pub ndcg_pinned: usize,
    pub recall: f64,
    pub mrr: f64,
    pub ndcg: f64,
}

/// One evaluation of one configuration over the whole fixture.
#[derive(Debug, Clone)]
pub struct Report {
    /// What was being scored, e.g. `context_text on`. Appears in the table heading.
    pub label: String,
    pub k: usize,
    pub rows: Vec<QueryRow>,
    pub by_class: BTreeMap<Class, ClassMetrics>,
    pub overall: ClassMetrics,
}

/// Score `run` over every query in `fixture`, cutting each ranked list at `k`.
pub fn evaluate(fixture: &Fixture, label: &str, run: Run<'_>, k: usize) -> anyhow::Result<Report> {
    // The cut below is what `ndcg` sees, and `ndcg` divides by an ideal that always runs to
    // `NDCG_K`. With `k < NDCG_K` the actual DCG would be computed over `k` documents and the
    // ideal over up to ten, and the quotient would still be printed under a column headed
    // `nDCG@10` — systematically understated, and mislabelled, which is the drift `NDCG_K`'s
    // own documentation promises does not happen. Loud rather than latent.
    assert!(
        k >= NDCG_K,
        "evaluate(k = {k}) is below NDCG_K = {NDCG_K}: nDCG would be computed over {k} \
         documents, divided by an ideal over {NDCG_K}, and printed as nDCG@{NDCG_K}"
    );
    let mut rows = Vec::with_capacity(fixture.queries.len());
    for query in &fixture.queries {
        // The closure runs for aggregation-shaped rows too. What it returns is recorded and
        // deliberately not scored: the point of the class is that a ranking *does* come back
        // and is the wrong answer shape, which is only visible if the run happens.
        let hits: Vec<String> = run(query)?.into_iter().take(k).collect();
        rows.push(score(query, &hits, k));
    }

    let mut by_class: BTreeMap<Class, Vec<&QueryRow>> = BTreeMap::new();
    for row in &rows {
        by_class.entry(row.class).or_default().push(row);
    }
    let by_class: BTreeMap<Class, ClassMetrics> =
        by_class.into_iter().map(|(c, rs)| (c, mean(&rs))).collect();
    let overall = mean(&rows.iter().collect::<Vec<_>>());

    Ok(Report {
        label: label.to_string(),
        k,
        rows,
        by_class,
        overall,
    })
}

/// The three metrics for one query against one ranked list.
fn score(query: &EvalQuery, hits: &[String], k: usize) -> QueryRow {
    let relevant: Vec<&String> = query
        .relevant
        .iter()
        .filter(|(_, grade)| **grade >= 1)
        .map(|(id, _)| id)
        .collect();

    let scored = query.shape == Shape::Ranked;
    let found = hits.iter().filter(|hit| relevant.contains(hit)).count();

    // A ranked query with no relevant document is rejected by `Fixture::validate`, so the
    // guard here is arithmetic hygiene rather than a live case.
    let recall = if relevant.is_empty() {
        0.0
    } else {
        found as f64 / relevant.len() as f64
    };
    let mrr = hits
        .iter()
        .position(|hit| relevant.contains(&hit))
        .map_or(0.0, |i| 1.0 / (i + 1) as f64);

    QueryRow {
        id: query.id.clone(),
        class: query.class,
        scored,
        retrieved: hits.len(),
        relevant: relevant.len(),
        found,
        ceiling: scored && at_recall_ceiling(hits.len(), found, relevant.len(), k),
        ndcg_pinned: scored && at_ndcg_ceiling(query, hits, found, relevant.len()),
        recall: if scored { recall } else { 0.0 },
        mrr: if scored { mrr } else { 0.0 },
        ndcg: if scored { ndcg(query, hits) } else { 0.0 },
        note: query.note.clone(),
        hits: hits
            .iter()
            .map(|hit| (hit.clone(), query.relevant.get(hit).copied().unwrap_or(0)))
            .collect(),
    }
}

/// Was this query's `recall = 1.000` a result, or arithmetic?
///
/// This is the harness auditing its own fixture, and it exists because of the failure mode
/// issue #21 was opened about. A class that prints `recall@10  1.000` reads as "retrieval is
/// perfect here". On a corpus of 65 documents it very often means something much weaker: the
/// query matched fewer documents than the cutoff, and the fixture author graded *every single
/// one of them* as relevant. Recall is then `found / relevant` where `found == relevant ==
/// the entire match set`, and no ranker could have scored it differently. The number is a
/// restatement of "the matcher matched", and it cannot fall except by a document dropping out
/// of the matched set entirely — which means it also cannot detect a reordering regression.
///
/// The three conditions, all required:
///
/// * `retrieved < k` — the list was not truncated, so what came back *is* the whole match set.
///   A query that filled the cutoff may have had relevant documents pushed off the end, and its
///   `1.000` is then a real statement about ranking.
/// * `found == retrieved` — nothing ungraded came back, so the graded set covers the match set.
/// * `found == relevant` — and nothing graded was missed, so the graded set is *exactly* the
///   match set. Without this, a query like `boundary-parse-verb-and-function` (eight retrieved,
///   all graded, nine graded in total) would be counted as a ceiling when its recall is 0.889
///   precisely because one graded document is unreachable. That row is measuring something.
///
/// Reported as a count per class rather than asserted on. A ceiling row is not a bug in the
/// harness and not always a bug in the fixture — `ident-sha256-pasted-whole` matches exactly
/// one document because a pasted hash *should* match exactly one document, and grading that one
/// document is the only thing to do. It is a bug in the *reading* of the table, and the count is
/// what stops someone quoting `identifier recall 1.000` as evidence that identifier retrieval is
/// solved.
fn at_recall_ceiling(retrieved: usize, found: usize, relevant: usize, k: usize) -> bool {
    retrieved < k && found == retrieved && found == relevant && relevant > 0
}

/// Was this query's nDCG arithmetically pinned at `1.000` by the fixture?
///
/// The same failure mode [`at_recall_ceiling`] exists for, on the metric the write-ups then tell
/// readers to reason from — and it is the more dangerous of the two, because nDCG is the column
/// that is supposed to still move when recall has saturated.
///
/// It is pinned when the run returned exactly the graded set (`found == retrieved == relevant`)
/// **and every returned grade is the same**. DCG and IDCG are then the same multiset of gains
/// against the same discounts, so they are equal under *any* permutation: no reordering the
/// ranker could produce would change the number. A one-document row with a single grade is the
/// commonest case; two documents both graded 3 is the same thing.
///
/// The equal-grades condition is what makes this narrower than the recall ceiling: a row that
/// retrieved exactly its graded set with grades 3 and 1 is *not* pinned, because putting the 1
/// first would cost it.
///
/// Counted per class rather than asserted on, for the same reason as the recall ceiling: it is
/// a fact about the fixture that a reader of the table needs, not a bug to fix.
fn at_ndcg_ceiling(query: &EvalQuery, hits: &[String], found: usize, relevant: usize) -> bool {
    if relevant == 0 || found != relevant || found != hits.len() {
        return false;
    }
    let mut grades = hits
        .iter()
        .map(|hit| query.relevant.get(hit).copied().unwrap_or(0));
    let Some(first) = grades.next() else {
        return false;
    };
    grades.all(|g| g == first)
}

/// nDCG@[`NDCG_K`]: `sum((2^g - 1) / log2(i + 2))` over the returned order, divided by the same
/// sum over the ideal order of the query's graded set.
///
/// The ideal is the fixture's grades sorted descending and truncated to the same cutoff, so a
/// query whose relevant set is larger than the cutoff is not penalised for the documents that
/// could not have fitted.
fn ndcg(query: &EvalQuery, hits: &[String]) -> f64 {
    let gain = |grade: u8| 2f64.powi(i32::from(grade)) - 1.0;
    let discounted = |i: usize, g: u8| gain(g) / ((i + 2) as f64).log2();

    let actual: f64 = hits
        .iter()
        .take(NDCG_K)
        .enumerate()
        .map(|(i, hit)| discounted(i, query.relevant.get(hit).copied().unwrap_or(0)))
        .sum();

    let mut ideal_grades: Vec<u8> = query
        .relevant
        .values()
        .copied()
        .filter(|g| *g > 0)
        .collect();
    ideal_grades.sort_unstable_by(|a, b| b.cmp(a));
    let ideal: f64 = ideal_grades
        .into_iter()
        .take(NDCG_K)
        .enumerate()
        .map(|(i, g)| discounted(i, g))
        .sum();

    if ideal == 0.0 { 0.0 } else { actual / ideal }
}

/// Mean of each metric over the *scored* rows, with the unscored ones still counted in
/// `queries`. Averaging a class of six where two were skipped over a denominator of six would
/// quietly halve it.
fn mean(rows: &[&QueryRow]) -> ClassMetrics {
    let scored: Vec<&&QueryRow> = rows.iter().filter(|r| r.scored).collect();
    let n = scored.len();
    let avg = |f: fn(&QueryRow) -> f64| -> f64 {
        if n == 0 {
            0.0
        } else {
            scored.iter().map(|r| f(r)).sum::<f64>() / n as f64
        }
    };
    ClassMetrics {
        queries: rows.len(),
        scored: n,
        ceiling: scored.iter().filter(|r| r.ceiling).count(),
        ndcg_pinned: scored.iter().filter(|r| r.ndcg_pinned).count(),
        recall: avg(|r| r.recall),
        mrr: avg(|r| r.mrr),
        ndcg: avg(|r| r.ndcg),
    }
}
