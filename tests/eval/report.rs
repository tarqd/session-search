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
        out.push_str(&ceiling_footnote(&self.overall));
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
                // The dagger is the whole reason the column is worth reading: without it, a row
                // whose recall could not have been anything but `1.000` looks exactly like a
                // row that earned it.
                let recall = if r.ceiling {
                    format!("{}\u{2020}", f3(r.recall))
                } else {
                    f3(r.recall)
                };
                (recall, f3(r.mrr), f3(r.ndcg))
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

        // The diff table has no room for a `ceiling` column — it is already five columns of
        // three-decimal numbers — but it is the table that gets pasted into an issue as a
        // before/after claim, so the count follows it as a line of prose. A `+0.000` on a class
        // where every scored query is at the ceiling is not "the change was neutral here"; it
        // is "this class could not have moved", and the two read identically without this.
        if before.overall.ceiling > 0 || after.overall.ceiling > 0 {
            out.push_str(&format!(
                "\nRecall ceiling: {} of {} scored queries before and {} of {} after had their \
                 recall forced to 1.000 by the fixture — the query matched fewer documents than \
                 the cutoff and every one of them is graded relevant. A delta of 0.000 on a \
                 class made mostly of those rows means the class could not have moved, which is \
                 a different claim from the change being neutral.\n",
                before.overall.ceiling,
                before.overall.scored,
                after.overall.ceiling,
                after.overall.scored,
            ));
        }
        out
    }
}

/// The class table's header and rule, shared by [`Report::table`] and [`class_table_only`].
fn header(k: usize) -> String {
    format!(
        "| {:<CLASS_W$} | {:>7} | {:>6} | {:>7} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n\
         | {} | {} | {} | {} | {} | {} | {} |\n",
        "class",
        "queries",
        "scored",
        "ceiling",
        format!("recall@{k}"),
        "MRR",
        format!("nDCG@{NDCG_K}"),
        "-".repeat(CLASS_W),
        "-".repeat(7),
        "-".repeat(6),
        "-".repeat(7),
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
        "| {:<CLASS_W$} | {:>7} | {:>6} | {:>7} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n",
        label, m.queries, m.scored, m.ceiling, recall, mrr, ndcg,
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
    out.push_str(&ceiling_footnote(&report.overall));
    out
}

/// The sentence that has to travel with every table carrying a `ceiling` column.
///
/// Written into the artifact rather than left to whoever pastes it, because the tables in this
/// harness exist precisely to be pasted into issues and pull requests, and a caveat that lives
/// only in a design document is a caveat that will be separated from its number on the first
/// copy. If the count is zero the sentence is omitted: a footnote about a phenomenon that did
/// not occur is noise, and its absence is itself the signal that the recall column is honest.
fn ceiling_footnote(overall: &ClassMetrics) -> String {
    if overall.ceiling == 0 {
        return String::new();
    }
    format!(
        "\n`ceiling` counts scored queries whose recall was forced to 1.000 by the fixture \
         rather than earned by the ranker: the query matched fewer documents than the cutoff \
         and every one of them is graded relevant, so `found == relevant == the whole match \
         set` and no ranking could have scored it differently. {} of {} scored queries are in \
         that state, marked \u{2020} in the per-query table. Their recall cannot rise, cannot \
         fall except by a document leaving the matched set entirely, and says nothing about \
         ordering. A class whose `ceiling` equals its `scored` has a recall column that measures \
         the corpus, not retrieval.\n",
        overall.ceiling, overall.scored,
    )
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

/// Every scored query whose recall or nDCG moved between two configurations, with both values.
///
/// The class tables are means, and a mean over seven queries hides the shape of what happened.
/// `paraphrase recall 0.206 -> 0.844` could be seven rows each gaining a little or two rows going
/// from nothing to everything, and those are different findings: the first is a tuning result and
/// the second is "documents that were unreachable became reachable". Only this table can tell
/// them apart, and it exists so that whoever writes the second sentence into an issue is reading
/// it off the harness rather than reconstructing it.
///
/// Rows that did not move are omitted rather than printed as zeroes. A before/after listing where
/// most lines are `0.000 → 0.000` trains the eye to skip it, and the class table already carries
/// the fact that the rest of the fixture held still. The count of what was left out is printed
/// instead, so "nothing else moved" is a statement in the artifact and not an inference from a
/// table that might simply have been truncated.
pub fn query_delta_table(before: &Report, after: &Report) -> String {
    assert_eq!(
        before.rows.len(),
        after.rows.len(),
        "a per-query before/after listing needs the same fixture on both sides"
    );

    let mut out = format!(
        "### Per query, where it moved — {} → {}\n\n\
         | {:<QUERY_W$} | {:<CLASS_W$} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n\
         | {} | {} | {} | {} | {} | {} |\n",
        before.label,
        after.label,
        "query",
        "class",
        "recall b",
        "recall a",
        "nDCG b",
        "nDCG a",
        "-".repeat(QUERY_W),
        "-".repeat(CLASS_W),
        "-".repeat(NUM_W),
        "-".repeat(NUM_W),
        "-".repeat(NUM_W),
        "-".repeat(NUM_W),
    );

    let mut held = 0;
    for (b, a) in before.rows.iter().zip(after.rows.iter()) {
        assert_eq!(b.id, a.id, "the two arms disagree about fixture order");
        if !a.scored {
            continue;
        }
        // 5e-4 is the width of the printed column: a movement smaller than that would render as
        // two identical cells, and a row of two identical cells in a table of movements is a
        // reader's bug report rather than a finding.
        if (a.recall - b.recall).abs() < 5e-4 && (a.ndcg - b.ndcg).abs() < 5e-4 {
            held += 1;
            continue;
        }
        out.push_str(&format!(
            "| {:<QUERY_W$} | {:<CLASS_W$} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} |\n",
            a.id,
            a.class.as_str(),
            f3(b.recall),
            f3(a.recall),
            f3(b.ndcg),
            f3(a.ndcg),
        ));
    }
    out.push_str(&format!(
        "\n{held} further scored queries were unchanged on both recall and nDCG to three \
         decimals and are omitted.\n"
    ));
    out
}
