//! Rendering a [`Report`] as markdown that survives being pasted into a pull request.
//!
//! Three rules, all of them about the diff rather than the look:
//!
//! * **Stable ordering.** Classes come out in `Class`'s declaration order (a `BTreeMap` over a
//!   derived `Ord`), and the per-query appendix follows the fixture's own order. Nothing is
//!   sorted by a score, because a metric that moves would then also move rows and every line of
//!   the diff would light up.
//! * **Fixed width, fixed precision.** Every number is three decimals and every column is padded
//!   to a constant width, so a run that changed one class changes one line.
//! * **No timing, ever.** `SearchResponse::elapsed_ms` is wall-clock; putting it anywhere near
//!   this table would make the committed snapshot fail on a slow runner and teach everyone to
//!   re-accept snapshots without reading them.

use crate::metrics::{ClassMetrics, NDCG_K, Report};

/// Column widths. Wide enough for `aggregation`, for `queries`, and for the longest fixture
/// id — a column that a long id overflows stops being a column, and the whole point of these
/// tables is that a diff of two of them lines up.
const CLASS_W: usize = 11;
const NUM_W: usize = 9;
const QUERY_W: usize = 32;

/// One of the three metrics, picked out of a [`ClassMetrics`] so the diff table can iterate
/// over them instead of repeating three near-identical blocks.
type MetricOf = fn(&ClassMetrics) -> f64;

/// `0.000`-style, so a column of them lines up and a diff shows only what moved.
///
/// Negative zero is normalised because it is reachable: `f64`'s `Sum` folds from `-0.0`, so a
/// configuration that returns an empty ranked list scores an nDCG of `-0.0` and the table prints
/// `-0.000`. A minus sign in front of a metric reads as a signed quantity, and none of these
/// three is one.
fn f3(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v:.3}")
}

impl Report {
    /// The per-class table: the artifact a pull request pastes.
    pub fn table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("## Retrieval eval — {}\n\n", self.label));
        out.push_str(&header(self.k));
        for (class, m) in &self.by_class {
            out.push_str(&row(class.as_str(), m));
        }
        out.push_str(&row("overall", &self.overall));

        let unscored = self.overall.queries - self.overall.scored;
        out.push_str(&format!(
            "\n{unscored} of {} queries are aggregation-shaped: recorded, never scored. \
             Top-k is the wrong answer shape for them, and scoring one as a ranking would book \
             a modelling mistake as a retrieval miss.\n",
            self.overall.queries
        ));
        out.push_str(&self.appendix());
        out
    }

    /// Every query, in fixture order, with what it retrieved.
    fn appendix(&self) -> String {
        let mut out = String::from("\n### Per query\n\n");
        out.push_str(&format!(
            "| {:<QUERY_W$} | {:<CLASS_W$} | {:>4} | {:>4} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n",
            "query",
            "class",
            "hits",
            "rel",
            format!("recall@{}", self.k),
            "MRR",
            format!("nDCG@{NDCG_K}"),
        ));
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            "-".repeat(QUERY_W),
            "-".repeat(CLASS_W),
            "-".repeat(4),
            "-".repeat(4),
            "-".repeat(NUM_W),
            "-".repeat(NUM_W),
            "-".repeat(NUM_W),
        ));
        for r in &self.rows {
            let (recall, mrr, ndcg) = if r.scored {
                (f3(r.recall), f3(r.mrr), f3(r.ndcg))
            } else {
                // Spelled out rather than left blank: a blank cell reads as zero.
                ("wrong shape".into(), "—".into(), "—".into())
            };
            out.push_str(&format!(
                "| {:<QUERY_W$} | {:<CLASS_W$} | {:>4} | {:>4} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n",
                r.id,
                r.class.as_str(),
                r.retrieved,
                r.relevant,
                recall,
                mrr,
                ndcg,
            ));
        }

        let notes: Vec<&crate::metrics::QueryRow> =
            self.rows.iter().filter(|r| r.note.is_some()).collect();
        if !notes.is_empty() {
            out.push_str("\n### Notes\n\n");
            for r in notes {
                out.push_str(&format!(
                    "- `{}` — {}\n",
                    r.id,
                    r.note.as_deref().unwrap_or_default()
                ));
            }
        }
        out
    }

    /// Two configurations side by side: the shape a before/after claim has to be made in.
    ///
    /// `before` and `after` must have been evaluated from the same fixture at the same `k`,
    /// which is asserted rather than trusted — a diff of two different cutoffs would read as a
    /// retrieval change.
    pub fn diff(before: &Report, after: &Report) -> String {
        assert_eq!(
            before.k, after.k,
            "a before/after table needs the same cutoff on both sides"
        );
        assert_eq!(
            before.rows.len(),
            after.rows.len(),
            "a before/after table needs the same fixture on both sides"
        );

        let mut out = String::new();
        out.push_str(&format!("## {} → {}\n\n", before.label, after.label));
        out.push_str(&format!(
            "| {:<CLASS_W$} | {:<9} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n\
             | {} | {} | {} | {} | {} |\n",
            "class",
            "metric",
            "before",
            "after",
            "delta",
            "-".repeat(CLASS_W),
            "-".repeat(9),
            "-".repeat(NUM_W),
            "-".repeat(NUM_W),
            "-".repeat(NUM_W),
        ));

        let metrics: [(&str, MetricOf); 3] = [
            ("recall", |m| m.recall),
            ("MRR", |m| m.mrr),
            ("nDCG", |m| m.ndcg),
        ];
        for (class, after_m) in &after.by_class {
            let before_m = before
                .by_class
                .get(class)
                .copied()
                .unwrap_or_else(ClassMetrics::default);
            // A class with nothing scored in it has no before and no after. Printing three
            // rows of `0.000 -> 0.000` for the aggregation class would read as a collapse.
            if after_m.scored == 0 && before_m.scored == 0 {
                continue;
            }
            for (name, get) in metrics {
                out.push_str(&delta_row(
                    class.as_str(),
                    name,
                    get(&before_m),
                    get(after_m),
                ));
            }
        }
        for (name, get) in metrics {
            out.push_str(&delta_row(
                "overall",
                name,
                get(&before.overall),
                get(&after.overall),
            ));
        }
        out
    }
}

/// The class table's header and rule, shared by [`Report::table`] and [`class_table_only`].
fn header(k: usize) -> String {
    format!(
        "| {:<CLASS_W$} | {:>7} | {:>6} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n\
         | {} | {} | {} | {} | {} | {} |\n",
        "class",
        "queries",
        "scored",
        format!("recall@{k}"),
        "MRR",
        format!("nDCG@{NDCG_K}"),
        "-".repeat(CLASS_W),
        "-".repeat(7),
        "-".repeat(6),
        "-".repeat(NUM_W),
        "-".repeat(NUM_W),
        "-".repeat(NUM_W),
    )
}

/// One class row. A class with nothing scored in it prints em dashes rather than zeroes:
/// `0.000` and "this was deliberately not measured" are different claims, and a reader
/// skimming a column of numbers will not tell them apart.
fn row(label: &str, m: &ClassMetrics) -> String {
    let (recall, mrr, ndcg) = if m.scored == 0 {
        (
            "\u{2014}".to_string(),
            "\u{2014}".to_string(),
            "\u{2014}".to_string(),
        )
    } else {
        (f3(m.recall), f3(m.mrr), f3(m.ndcg))
    };
    format!(
        "| {:<CLASS_W$} | {:>7} | {:>6} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n",
        label, m.queries, m.scored, recall, mrr, ndcg,
    )
}

fn delta_row(class: &str, metric: &str, before: f64, after: f64) -> String {
    // A signed zero and a `-0.000` would both be noise in a diff; normalise to one spelling.
    let d = after - before;
    let d = if d.abs() < 5e-4 { 0.0 } else { d };
    format!(
        "| {:<CLASS_W$} | {:<9} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n",
        class,
        metric,
        f3(before),
        f3(after),
        format!("{}{}", if d > 0.0 { "+" } else { "" }, f3(d)),
    )
}

/// A markdown listing of the class table without the appendix, for the ablation file where the
/// per-query rows of two arms would be more noise than signal.
pub fn class_table_only(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!("## Retrieval eval — {}\n\n", report.label));
    out.push_str(&header(report.k));
    for (class, m) in &report.by_class {
        out.push_str(&row(class.as_str(), m));
    }
    out.push_str(&row("overall", &report.overall));
    out
}

/// Every query's ranked list, with the grade the fixture gave each hit.
///
/// Written to `target/eval/hits.md` rather than into the report table, for two reasons: it is
/// long, and it is the one part of the output that *should* change freely. Its readers are
/// whoever is adding a query to the fixture and needs to see what comes back before deciding
/// what deserves a grade, and whoever is looking at a row whose recall moved and wants to know
/// which document left the top ten.
pub fn hits_listing(report: &Report) -> String {
    let mut out = format!("# Ranked hits — {}\n\n", report.label);
    for r in &report.rows {
        out.push_str(&format!(
            "## `{}` ({}{})\n\n",
            r.id,
            r.class.as_str(),
            if r.scored { "" } else { ", not scored" }
        ));
        if r.hits.is_empty() {
            out.push_str("_nothing retrieved_\n\n");
            continue;
        }
        for (rank, (doc, grade)) in r.hits.iter().enumerate() {
            out.push_str(&format!("{:>2}. `{doc}` — grade {grade}\n", rank + 1));
        }
        out.push('\n');
    }
    out
}
