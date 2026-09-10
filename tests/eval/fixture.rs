//! The query fixture: `tests/fixtures/eval_queries.json`, and the validation that makes a
//! broken corpus fail loudly instead of quietly scoring zero.
//!
//! The failure mode this whole file exists to prevent: someone renumbers the corpus, every
//! document reference in the fixture stops resolving, every query retrieves nothing, and the
//! harness reports `recall 0.000` in a tidy table that a reader takes for a regression in the
//! ranker. So [`Fixture::validate`] refuses to run at all unless every reference names a
//! document that is actually in the index, every class is populated, and every row is the shape
//! its `shape` field claims.
//!
//! `filters` deserializes straight into [`Filters`], the same struct `clap` fills from the
//! command line, so a fixture row is exactly as expressive as the CLI and cannot drift from it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use session_search::search::Filters;

/// The five query classes from issue #21. Declaration order is table order: `Ord` is derived,
/// and `Report::by_class` is a `BTreeMap`, so the rows of every table this harness prints come
/// out in this sequence no matter what order the fixture happens to list its queries in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// A name, a path, a flag or a hash, typed the way a person half-remembers it. These pin
    /// the tokenizer invariants: `snippet` finds `SnippetGenerator`, `iserror` finds `is_error`,
    /// `OpenOrCreate` finds `open_or_create`, and a pasted sha256 finds the document it came
    /// from.
    Identifier,
    /// A word that is English prose in one place and code in another. The two halves of a
    /// message are analyzed differently on purpose, and this class is what says whether one
    /// query still reaches both.
    Boundary,
    /// The words a person would use, which are not the words the transcript used. This is the
    /// class `context_text` exists for.
    Paraphrase,
    /// A query plus structured filters — project, language, program, date window, sidechain.
    /// Measures the filters and the text query together, because that is how they are used.
    Filtered,
    /// A question whose honest answer is a distribution, not a list. Recorded, never scored.
    Aggregation,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Identifier => "identifier",
            Class::Boundary => "boundary",
            Class::Paraphrase => "paraphrase",
            Class::Filtered => "filtered",
            Class::Aggregation => "aggregation",
        }
    }

    /// Every class, in table order. Used by the fixture's own coverage check.
    pub fn all() -> [Class; 5] {
        [
            Class::Identifier,
            Class::Boundary,
            Class::Paraphrase,
            Class::Filtered,
            Class::Aggregation,
        ]
    }
}

/// What kind of answer the query has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// A ranked list of documents, scored by recall / MRR / nDCG.
    #[default]
    Ranked,
    /// A facet table. Checked against `search::facets`, never scored as a ranking.
    Facet,
}

/// Where a query came from. Honesty about this is a requirement of the issue, not a nicety: a
/// synthetic corpus can only tell you about the mechanisms you planted in it, and a reader has
/// to be able to see which rows are that and which came off a real transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Written against the synthetic corpus, modelled on the shape of a real query.
    Synthesised,
    /// Written against one of the redacted real-transcript slices in `tests/fixtures/`.
    Real,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EvalQuery {
    /// Stable kebab slug. It is the row key of the per-query appendix, so renaming one shows up
    /// as a moved row in a diff — which is the point.
    pub id: String,
    pub class: Class,
    #[serde(default)]
    pub shape: Shape,
    /// The query string, exactly as a person would type it after `session-search search`.
    pub query: String,
    /// Deserialized straight into the CLI's own filter struct.
    #[serde(default)]
    pub filters: Filters,
    #[serde(default)]
    pub include_thinking: bool,
    /// Document reference -> grade. 3 = this document *is* the answer, 2 = a good answer,
    /// 1 = a reader would keep it, absent = not relevant. Grade >= 1 is the binary relevant set
    /// recall and MRR are taken over; the grades themselves are what nDCG needs.
    #[serde(default)]
    pub relevant: BTreeMap<String, u8>,
    /// Facet rows only: the field to aggregate.
    #[serde(default)]
    pub facet_field: Option<String>,
    /// Facet rows only: buckets that must come back with at least one document.
    #[serde(default)]
    pub expect_facet_values: Vec<String>,
    pub provenance: Provenance,
    /// Free text. **Mandatory on every aggregation-shaped row**, where it has to say why top-k
    /// is the wrong shape, and used on any row whose behaviour would otherwise surprise a
    /// reader of the appendix.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FixtureProvenance {
    pub corpus: String,
    pub authored_by: String,
    /// Real-transcript slices any `provenance: "real"` query is written against.
    pub real_slices: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Fixture {
    pub schema_version: u32,
    pub provenance: FixtureProvenance,
    pub queries: Vec<EvalQuery>,
}

/// The lowest number of queries per class the issue asks for.
const MIN_PER_CLASS: usize = 6;
/// The lowest total. The issue asks for 30-50.
const MIN_QUERIES: usize = 30;
/// The highest grade the nDCG gain function is calibrated for.
const MAX_GRADE: u8 = 3;

impl Fixture {
    pub fn path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval_queries.json")
    }

    pub fn load() -> anyhow::Result<Fixture> {
        let path = Fixture::path();
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading the query fixture {path:?}: {e}"))?;
        let fixture: Fixture = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing the query fixture {path:?}: {e}"))?;
        Ok(fixture)
    }

    /// Every check that has to pass before a single number is believable.
    ///
    /// `known` is the set of document references the corpus actually produced. A reference that
    /// is not in it is a hard error and not a zero, because the two are indistinguishable in a
    /// results table and only one of them is a retrieval result.
    pub fn validate(&self, known: &BTreeSet<String>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version == 1,
            "unknown fixture schema_version {}",
            self.schema_version
        );
        anyhow::ensure!(
            self.queries.len() >= MIN_QUERIES,
            "the fixture holds {} queries; issue #21 asks for at least {MIN_QUERIES}",
            self.queries.len()
        );

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for query in &self.queries {
            anyhow::ensure!(!query.id.trim().is_empty(), "a query has an empty id");
            anyhow::ensure!(
                seen.insert(&query.id),
                "duplicate query id {:?}; ids are the appendix's row keys",
                query.id
            );
            anyhow::ensure!(
                !query.query.trim().is_empty(),
                "query {:?} has an empty query string",
                query.id
            );

            for (doc_ref, grade) in &query.relevant {
                anyhow::ensure!(
                    known.contains(doc_ref),
                    "query {:?} grades {doc_ref:?}, which is not in the corpus. \
                     A stale reference scores as a miss and is indistinguishable from a \
                     ranking regression, so it is refused here instead. Re-read \
                     target/eval/corpus.md for the current references.",
                    query.id
                );
                anyhow::ensure!(
                    *grade <= MAX_GRADE,
                    "query {:?} grades {doc_ref:?} at {grade}; grades run 0-{MAX_GRADE}",
                    query.id
                );
            }

            match query.shape {
                Shape::Ranked => {
                    anyhow::ensure!(
                        query.relevant.values().any(|g| *g >= 1),
                        "ranked query {:?} has no relevant document, so its recall can only \
                         ever be zero",
                        query.id
                    );
                    anyhow::ensure!(
                        query.facet_field.is_none() && query.expect_facet_values.is_empty(),
                        "ranked query {:?} carries facet expectations",
                        query.id
                    );
                }
                Shape::Facet => {
                    anyhow::ensure!(
                        query.class == Class::Aggregation,
                        "query {:?} is facet-shaped but not in the aggregation class",
                        query.id
                    );
                    anyhow::ensure!(
                        query.facet_field.as_deref().is_some_and(|f| !f.is_empty()),
                        "facet query {:?} names no facet_field",
                        query.id
                    );
                    anyhow::ensure!(
                        !query.expect_facet_values.is_empty(),
                        "facet query {:?} expects no buckets, so it asserts nothing",
                        query.id
                    );
                    anyhow::ensure!(
                        query.relevant.is_empty(),
                        "facet query {:?} grades documents; a facet answer is a distribution \
                         and grading it invites someone to score it as a ranking",
                        query.id
                    );
                    anyhow::ensure!(
                        query.note.as_deref().is_some_and(|n| n.len() > 20),
                        "aggregation-shaped query {:?} needs a note saying why top-k is the \
                         wrong shape for it",
                        query.id
                    );
                }
            }
        }

        for class in Class::all() {
            let n = self.queries.iter().filter(|q| q.class == class).count();
            anyhow::ensure!(
                n >= MIN_PER_CLASS,
                "class {} has {n} queries; issue #21 asks for at least {MIN_PER_CLASS} in each \
                 of the five",
                class.as_str()
            );
        }
        Ok(())
    }

    /// The facet-shaped rows, which are checked against `search::facets` rather than scored.
    pub fn facet_queries(&self) -> impl Iterator<Item = &EvalQuery> {
        self.queries.iter().filter(|q| q.shape == Shape::Facet)
    }
}
