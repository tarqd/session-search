//! Issue #25's measurement: what a turn costs as documents, and what it costs as a skeleton.
//!
//! The claim under test is a token-budget claim — "the difference between returning 20 results
//! in ~4k tokens and blowing the context window on the first call" — so the unit measured here
//! is the one a caller actually pays in: **the bytes of the JSON a hit's turn context adds to a
//! response**. `--context turn --json` sends the turn's documents (`format::doc_json` each);
//! `--context skeleton --json` sends the skeleton object instead. Both are rendered here by the
//! same functions the CLI renders them with, so the ratio cannot drift away from the product.
//!
//! **Why not tokens.** A token count needs a tokenizer this crate does not ship and would not
//! agree with whichever model reads the output. Bytes are exact, reproducible in CI, and divide
//! by ~4 for an English-and-JSON estimate.
//!
//! **Why the fixtures are the corpus.** The two `real_*_slice.jsonl` files are captures of a
//! real session — a subagent's investigation and the main transcript that spawned it — kept in
//! the repo precisely so a measurement like this one is reproducible by anyone who checks it
//! out. The eval fixtures join them because they are the corpus every other number in
//! `docs/EVAL.md` is quoted against, and a ratio that only held on one file would be a claim
//! about that file.

use std::path::{Path, PathBuf};

use session_search::format::{SKELETON_BUDGET, Skeleton, doc_json, turn_skeleton};
use session_search::parse::{Doc, ParseOptions, parse_whole};

use crate::corpus::fixture_dir;

/// One transcript's turns, measured.
pub struct Measured {
    pub label: String,
    pub turns: usize,
    pub docs: usize,
    /// Bytes of `--context turn --json` over every turn: the documents, as the API sends them.
    pub context_bytes: usize,
    /// Bytes of `--context skeleton --json` over the same turns.
    pub skeleton_bytes: usize,
    /// The single most expensive turn, in each unit. A mean hides the case the feature is for:
    /// one turn that read a 200 KB log.
    pub worst_context: usize,
    pub worst_skeleton: usize,
}

impl Measured {
    /// Skeleton bytes as a percentage of context bytes. Lower is the whole point.
    pub fn percent(&self) -> f64 {
        if self.context_bytes == 0 {
            return 0.0;
        }
        100.0 * self.skeleton_bytes as f64 / self.context_bytes as f64
    }
}

/// The transcripts this measurement runs over: the two real captures first, because they are the
/// ones that answer "on a real corpus", then the eval fixtures in the sorted order every other
/// table in this harness uses.
fn transcripts() -> anyhow::Result<Vec<PathBuf>> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths = vec![
        fixtures.join("real_main_slice.jsonl"),
        fixtures.join("real_sidechain_slice.jsonl"),
    ];
    let mut eval: Vec<PathBuf> = std::fs::read_dir(fixture_dir())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    eval.sort();
    paths.extend(eval);
    Ok(paths)
}

/// A transcript's documents grouped into turns, in transcript order.
///
/// `turn_seq` is contiguous within a file by construction (the Turns section of
/// `docs/DESIGN.md`), so this is a fold rather than a sort — and a fold that would notice if
/// contiguity ever broke, because a re-opened turn would come out as two rows.
fn by_turn(docs: &[Doc]) -> Vec<Vec<&Doc>> {
    let mut turns: Vec<Vec<&Doc>> = Vec::new();
    for doc in docs {
        match turns.last_mut() {
            Some(last) if last[0].turn_seq == doc.turn_seq => last.push(doc),
            _ => turns.push(vec![doc]),
        }
    }
    turns
}

/// The JSON `--context turn` adds to a hit: the turn's documents, exactly as `format::hit_json`
/// puts them under `"context"`.
fn context_json_bytes(turn: &[&Doc]) -> usize {
    let docs: Vec<serde_json::Value> = turn.iter().map(|d| doc_json(d)).collect();
    serde_json::to_string(&docs).map(|s| s.len()).unwrap_or(0)
}

/// The JSON `--context skeleton` adds instead. Rendered through the same `turn_skeleton` the
/// renderer calls, at the same budget.
fn skeleton_json_bytes(turn: &[&Doc]) -> usize {
    let owned: Vec<Doc> = turn.iter().map(|d| (*d).clone()).collect();
    let skeleton: Skeleton = turn_skeleton(&owned, SKELETON_BUDGET);
    serde_json::to_string(&serde_json::json!({
        "lines": skeleton.lines,
        "dropped": skeleton.dropped,
        "bytes": skeleton.bytes(),
    }))
    .map(|s| s.len())
    .unwrap_or(0)
}

/// `Doc::source_path` is absolute, so it is a property of the *checkout* rather than of the
/// transcript: `/home/user/session-search/...` locally and `/home/runner/work/...` on the runner
/// differ by about twenty bytes per document, all of them on the context side, and a snapshot
/// taken on one machine fails on the other. The same trap `corpus::doc_ref` documents for
/// document ids.
///
/// Replacing it with the file's own name makes the table reproducible, and errs the safe way:
/// a real absolute path *adds* to the context side and nothing to the skeleton, so what is
/// measured here understates the saving rather than inflating it.
fn without_the_checkout_path(docs: Vec<Doc>, path: &Path) -> Vec<Doc> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    docs.into_iter()
        .map(|d| Doc {
            source_path: name.clone(),
            ..d
        })
        .collect()
}

pub fn measure() -> anyhow::Result<Vec<Measured>> {
    let opts = ParseOptions::default();
    let mut out = Vec::new();
    for path in transcripts()? {
        let parsed = parse_whole(&path, &opts)?;
        anyhow::ensure!(!parsed.docs.is_empty(), "{path:?} produced no documents");
        let docs = without_the_checkout_path(parsed.docs, &path);
        let turns = by_turn(&docs);
        let mut measured = Measured {
            label: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            turns: turns.len(),
            docs: docs.len(),
            context_bytes: 0,
            skeleton_bytes: 0,
            worst_context: 0,
            worst_skeleton: 0,
        };
        for turn in &turns {
            let context = context_json_bytes(turn);
            let skeleton = skeleton_json_bytes(turn);
            measured.context_bytes += context;
            measured.skeleton_bytes += skeleton;
            // The worst turn in each unit, not the worst turn by one of them: what the cap has
            // to survive is the biggest skeleton, wherever it lands.
            measured.worst_context = measured.worst_context.max(context);
            measured.worst_skeleton = measured.worst_skeleton.max(skeleton);
        }
        out.push(measured);
    }
    Ok(out)
}

/// Column widths, per the rules in `report.rs`: fixed, so a run that moved one file moves one
/// line of the diff.
const LABEL_W: usize = 42;
const NUM_W: usize = 9;

fn cell(value: impl std::fmt::Display, width: usize) -> String {
    format!("{value:>width$}")
}

/// The table a pull request pastes.
pub fn table(rows: &[Measured]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "| {:<LABEL_W$} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} | {:>NUM_W$} | {:>7} |\n",
        "transcript", "turns", "docs", "ctx B", "skel B", "skel %"
    ));
    out.push_str(&format!(
        "|{}|{}|{}|{}|{}|{}|\n",
        "-".repeat(LABEL_W + 2),
        "-".repeat(NUM_W + 2),
        "-".repeat(NUM_W + 2),
        "-".repeat(NUM_W + 2),
        "-".repeat(NUM_W + 2),
        "-".repeat(9),
    ));
    let mut totals = Measured {
        label: "ALL".into(),
        turns: 0,
        docs: 0,
        context_bytes: 0,
        skeleton_bytes: 0,
        worst_context: 0,
        worst_skeleton: 0,
    };
    for row in rows {
        out.push_str(&format!(
            "| {:<LABEL_W$} | {} | {} | {} | {} | {:>7.1} |\n",
            row.label,
            cell(row.turns, NUM_W),
            cell(row.docs, NUM_W),
            cell(row.context_bytes, NUM_W),
            cell(row.skeleton_bytes, NUM_W),
            row.percent(),
        ));
        totals.turns += row.turns;
        totals.docs += row.docs;
        totals.context_bytes += row.context_bytes;
        totals.skeleton_bytes += row.skeleton_bytes;
        totals.worst_context = totals.worst_context.max(row.worst_context);
        totals.worst_skeleton = totals.worst_skeleton.max(row.worst_skeleton);
    }
    out.push_str(&format!(
        "| {:<LABEL_W$} | {} | {} | {} | {} | {:>7.1} |\n",
        totals.label,
        cell(totals.turns, NUM_W),
        cell(totals.docs, NUM_W),
        cell(totals.context_bytes, NUM_W),
        cell(totals.skeleton_bytes, NUM_W),
        totals.percent(),
    ));
    out.push('\n');
    // The averages a reader would otherwise compute wrongly off the totals, and the worst case
    // the budget exists for.
    out.push_str(&format!(
        "mean turn: {} B of context, {} B of skeleton\n",
        totals.context_bytes / totals.turns.max(1),
        totals.skeleton_bytes / totals.turns.max(1),
    ));
    out.push_str(&format!(
        "worst turn: {} B of context, {} B of skeleton\n",
        totals.worst_context, totals.worst_skeleton,
    ));
    out
}
