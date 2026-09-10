//! Issue #26's configuration: `MoreLikeThisQuery`, scored on the same corpus, the same fixture
//! and the same arithmetic as everything else in this harness.
//!
//! **The honest health warning comes first, because the numbers are meaningless without it.**
//! "Find documents similar to this one" is a different question from "find documents matching
//! these words", and this fixture asks the second. There is no way to score the first on it
//! without inventing a protocol, so one is invented here and written down rather than hidden:
//!
//! 1. Every ranked query has a **seed**: the document the fixture graded highest, ties broken
//!    by reference order so the choice is deterministic. This models the moment the feature is
//!    actually for — a person ran the search, found one good answer, and wants the rest.
//! 2. The MoreLikeThis arm drops the query string entirely and searches by
//!    `--similar-to <seed>`, keeping the row's filters (a similarity search composes with them,
//!    and the filtered class is only meaningful if it does).
//! 3. The graded set is **narrowed** to what that arm could possibly return: the seed and every
//!    other document of the seed's turn are removed, because the shipped default excludes the
//!    source turn. Counting documents the configuration is designed never to return would
//!    manufacture a miss and report it as a retrieval result.
//! 4. A query whose graded set is *only* the seed's turn is dropped from this arm and counted
//!    in the report's preamble. Its recall would be 0/0.
//!
//! Both arms — plain search and MoreLikeThis — are then scored over that same narrowed fixture,
//! so the diff table compares two answers to one question rather than two questions.
//!
//! **What each class means for this arm.** `identifier` and `boundary` measure analyzer
//! behaviour on a typed query; there is no typed query here, so those rows say only "having
//! found one document about `open_or_create`, are the others about it nearby". `filtered`
//! measures that the filters still AND on top, which is a real property of this feature and the
//! one class that transfers cleanly. `paraphrase` is the closest thing to a fair test: it is
//! the class where the query words are *not* the transcript's words, which is the situation
//! similarity exists for. `aggregation` is dropped with the rest of the unranked rows.
//!
//! What this cannot tell anybody: whether the tuning constants in `search.rs` are right for a
//! corpus of a hundred thousand documents. `SIMILAR_MAX_DOC_FREQUENCY_FLOOR` alone means that
//! on 65 documents *nothing* is ever cut for being too common, so the whole upper bound — the
//! one parameter that does the most work on a real index — is inert here.

use std::collections::{BTreeMap, BTreeSet};

use crate::corpus::{Corpus, doc_ref};
use crate::fixture::{EvalQuery, Fixture, Shape};
use session_search::schema::Fields;
use session_search::search::{SearchRequest, SimilarField, resolve_similar, search};
use tantivy::Index;

/// Where a document sits, for the turn arithmetic: its `doc_id` (which is what a reference
/// resolves against) and the `(file, turn)` pair that identifies its turn.
///
/// The pair, not `turn_seq` alone: `turn_seq` is a per-*file* ordinal and one session id can
/// name two files, so turn 3 of one transcript and turn 3 of another are different turns.
#[derive(Debug, Clone)]
struct Placement {
    doc_id: String,
    turn: (String, u64),
}

/// The corpus indexed by document reference — the key the fixture grades in.
pub struct Placements(BTreeMap<String, Placement>);

impl Placements {
    pub fn of(corpus: &Corpus) -> Placements {
        Placements(
            corpus
                .docs()
                .map(|doc| {
                    (
                        doc_ref(doc),
                        Placement {
                            doc_id: doc.doc_id.clone(),
                            turn: (doc.source_path.clone(), doc.turn_seq),
                        },
                    )
                })
                .collect(),
        )
    }

    /// Every reference in the same turn as `reference`, including it.
    ///
    /// Public because the turn, not the document, is what `--similar-to` seeds from and what it
    /// excludes — so "the source came back" is a question about this set.
    pub fn turn_of(&self, reference: &str) -> BTreeSet<String> {
        let Some(seed) = self.0.get(reference) else {
            return BTreeSet::new();
        };
        self.0
            .iter()
            .filter(|(_, p)| p.turn == seed.turn)
            .map(|(r, _)| r.clone())
            .collect()
    }
}

/// The document a query is seeded from: its highest grade, ties broken by reference order.
///
/// Deterministic by construction — `relevant` is a `BTreeMap`, so `max_by_key` over it walks
/// references in sorted order and Rust's `max_by_key` keeps the *last* maximum, which is a
/// stable choice for a stable input. A protocol that picked the seed by running the baseline
/// search first would make this arm's numbers depend on the arm it is being compared against.
fn seed_of(query: &EvalQuery) -> Option<String> {
    query
        .relevant
        .iter()
        .filter(|(_, grade)| **grade >= 1)
        .max_by_key(|(_, grade)| **grade)
        .map(|(reference, _)| reference.clone())
}

/// The fixture this arm is scored on: ranked rows only, each carrying a seed, with the seed's
/// whole turn removed from the graded set.
///
/// Returns the narrowed fixture, the seed per query id, and the ids that had to be dropped
/// because nothing was left to find.
pub struct Narrowed {
    pub fixture: Fixture,
    pub seeds: BTreeMap<String, String>,
    pub dropped: Vec<String>,
}

pub fn narrow(fixture: &Fixture, placements: &Placements) -> Narrowed {
    let mut queries = Vec::new();
    let mut seeds = BTreeMap::new();
    let mut dropped = Vec::new();

    for query in &fixture.queries {
        if query.shape != Shape::Ranked {
            continue;
        }
        let Some(seed) = seed_of(query) else {
            dropped.push(query.id.clone());
            continue;
        };
        let excluded = placements.turn_of(&seed);
        let mut narrowed = query.clone();
        narrowed
            .relevant
            .retain(|reference, grade| *grade >= 1 && !excluded.contains(reference));
        if narrowed.relevant.is_empty() {
            dropped.push(query.id.clone());
            continue;
        }
        seeds.insert(query.id.clone(), seed);
        queries.push(narrowed);
    }

    Narrowed {
        fixture: Fixture {
            schema_version: fixture.schema_version,
            provenance: fixture.provenance.clone(),
            queries,
        },
        seeds,
        dropped,
    }
}

/// The plain-search arm over the narrowed fixture: the comparison column, and the reason the
/// MoreLikeThis numbers can be read at all.
pub fn text_run<'a>(
    index: &'a Index,
    fields: &'a Fields,
    k: usize,
) -> impl Fn(&EvalQuery) -> anyhow::Result<Vec<String>> + 'a {
    move |query: &EvalQuery| {
        let request = SearchRequest {
            query: Some(query.query.clone()),
            filters: query.filters.clone(),
            limit: k,
            include_thinking: query.include_thinking,
            ..SearchRequest::default()
        };
        Ok(search(index, fields, &request)?
            .hits
            .iter()
            .map(|hit| doc_ref(&hit.doc))
            .collect())
    }
}

/// The MoreLikeThis arm: no query string at all, seeded from the row's own seed document.
///
/// `include_source` is left at the shipped default (false), matching the narrowed graded set.
/// A reference that fails to resolve, or a seed turn with no indexed text, is an error and
/// fails the whole run — which is the point of the closure returning a `Result`: a
/// configuration that cannot answer a query has not scored zero on it.
pub fn similar_run<'a>(
    index: &'a Index,
    fields: &'a Fields,
    placements: &'a Placements,
    seeds: &'a BTreeMap<String, String>,
    k: usize,
    include_source: bool,
) -> impl Fn(&EvalQuery) -> anyhow::Result<Vec<String>> + 'a {
    move |query: &EvalQuery| {
        let seed = seeds
            .get(&query.id)
            .ok_or_else(|| anyhow::anyhow!("query {:?} has no seed", query.id))?;
        let placement = placements
            .0
            .get(seed)
            .ok_or_else(|| anyhow::anyhow!("seed {seed:?} is not in the corpus"))?;
        // Seeded by `doc_id`, which is the unambiguous, fully-spelled form of a reference. The
        // prefix and coordinate spellings are exercised by the unit tests in `search.rs`; a
        // metric run is not the place to also be testing the parser.
        let source = resolve_similar(
            index,
            fields,
            &placement.doc_id,
            &[SimilarField::Text],
            include_source,
        )?;
        let request = SearchRequest {
            query: None,
            filters: query.filters.clone(),
            limit: k,
            include_thinking: query.include_thinking,
            similar_to: Some(source),
            ..SearchRequest::default()
        };
        Ok(search(index, fields, &request)?
            .hits
            .iter()
            .map(|hit| doc_ref(&hit.doc))
            .collect())
    }
}

/// The preamble that has to travel with the table wherever it is pasted.
pub fn preamble(narrowed: &Narrowed, total: usize) -> String {
    format!(
        "The MoreLikeThis arm answers a different question from the fixture's own. Each row is \
         seeded from the document that row graded highest, the query string is discarded, the \
         row's filters are kept, and the seed's whole turn is removed from the graded set \
         because `--similar-to` excludes it by default. {} of {total} fixture rows are scored \
         here: {} were dropped for having no graded document outside the seed's own turn, and \
         the aggregation-shaped rows are not ranked answers at all.\n\n\
         Read `recall` and ignore nothing else: MRR and nDCG are reported for symmetry with the \
         baseline table, but a similarity search has no notion of \"the answer\" to rank first.\n",
        narrowed.fixture.queries.len(),
        narrowed.dropped.len(),
    )
}
