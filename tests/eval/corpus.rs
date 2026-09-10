//! The evaluation corpus: synthetic transcripts under `tests/fixtures/eval/`, run through the
//! real parser and indexed through the real schema.
//!
//! **Why transcripts and not hand-built `Doc`s.** The harness has to measure the machinery that
//! decides *where* a word lands, not a staged version of it. A term that is English prose in one
//! session and an identifier inside a fenced block in another only ends up in `text` versus
//! `code` because `markdown::split` put it there; writing those strings into `Doc::text` and
//! `Doc::code` by hand would fake the exact mechanism the prose/code boundary class exists to
//! test. The same argument covers `code_langs` (fence info strings, which `--lang` filters on),
//! `bash_cmd` (`bash::extract`, which `--program` filters on), `turn_seq` / `turn_prompt`, and
//! `SessionInfo::title` / `first_prompt` — which is what makes the `context_text` header real
//! rather than staged. `Doc` also derives no `Default`, so every hand-built document would be a
//! thirty-five field literal.
//!
//! **Why this rebuilds index construction instead of calling `search::testkit`.** `testkit` and
//! `tokenizer::create_in_ram` are both `#[cfg(test)] pub(crate)`. An integration test links the
//! library compiled *without* `cfg(test)`, so neither is reachable from here, and widening them
//! would put test scaffolding into the shipped crate. Everything needed is already public:
//! [`build_schema`], [`tokenizer::register`], [`parse_whole`] and [`doc_to_json`]. The dozen
//! lines that costs are lines the harness wants to own anyway, because it needs the context
//! on/off switch [`Variant`] that `testkit` has no reason to carry.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use session_search::parse::{Doc, ParseOptions, SessionInfo, parse_whole};
use session_search::schema::{Fields, build_schema, doc_to_json};
use session_search::tokenizer;
use tantivy::{Index, TantivyDocument};

/// Which header the corpus is indexed with. The harness scores the same queries over both and
/// reports the difference, which is how issue #23's before/after box gets closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// What `index.rs` writes today: `context_text` composed from the merged session row.
    WithContext,
    /// The same documents with the `context_text` key removed from the JSON handed to Tantivy.
    ///
    /// This is the *only* honest off-switch. Two alternatives look equivalent and are not:
    ///
    /// * `doc_to_json(doc, None, true)` drops the title and the first prompt but still composes
    ///   a header out of the project basename, the git branch and the turn prompt — that
    ///   measures the `sessions.json` half of the feature, not the feature. It is available
    ///   here as [`Variant::SessionRowOnly`], labelled as what it is.
    /// * clearing `doc.project` / `doc.git_branch` / `doc.turn_prompt` on a cloned `Doc` also
    ///   empties the `project` and `git_branch` *schema* fields, which breaks `--project` and
    ///   `--branch` on the ablated arm and stops the two arms being comparable at all.
    ///
    /// Removing the key leaves the two indexes byte-identical in every other field — same
    /// schema, same analyzers, same single segment, same timestamps and filters — so only the
    /// `context_text` posting lists differ. Fieldnorms are per field, so the BM25 length
    /// normalisation of `text`, `code` and the rest is untouched by the removal.
    WithoutContext,
    /// `doc_to_json(doc, None, ..)`: no session row, but the per-document pieces of the header
    /// (project basename, branch, turn prompt) still compose one. The third arm, and the one
    /// that answers "how much of the win comes from `sessions.json`".
    SessionRowOnly,
}

impl Variant {
    /// The label used in report tables and diff headings.
    pub fn label(self) -> &'static str {
        match self {
            Variant::WithContext => "context_text on",
            Variant::WithoutContext => "context_text off",
            Variant::SessionRowOnly => "context_text without the session row",
        }
    }
}

/// One parsed transcript: its documents and the session row `context_header` composes from.
pub struct Transcript {
    pub session: SessionInfo,
    pub docs: Vec<Doc>,
}

/// The corpus, parsed once. Cheap enough to build per test (six files, ~70 documents).
pub struct Corpus {
    pub transcripts: Vec<Transcript>,
}

/// `tests/fixtures/eval/`.
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval")
}

/// A document's stable reference: `"{session_id}:{agent_id|-}:{seq}"`.
///
/// This is `Doc::doc_id` minus its `file_tag`, and the omission is the point. `file_tag` is
/// `fnv1a` of the *absolute* source path, so a fixture naming raw `doc_id`s would pass on the
/// machine that wrote it and fail in CI and in every other checkout. Every component here is
/// STORED, so `search::doc_from_stored` hands all three back on a `Hit` and the run closure
/// derives the same string with no side map — which also keeps the fixture valid if the corpus
/// ever stops coming from `parse_whole`.
pub fn doc_ref(doc: &Doc) -> String {
    format!(
        "{}:{}:{}",
        doc.session_id,
        doc.agent_id.as_deref().unwrap_or("-"),
        doc.seq
    )
}

impl Corpus {
    /// Parse every `*.jsonl` under `tests/fixtures/eval/`, in sorted path order.
    pub fn load() -> anyhow::Result<Corpus> {
        let mut paths = Vec::new();
        collect_jsonl(&fixture_dir(), &mut paths)?;
        paths.sort();
        anyhow::ensure!(
            !paths.is_empty(),
            "no eval fixtures under {:?}",
            fixture_dir()
        );

        let opts = ParseOptions::default();
        let mut transcripts = Vec::new();
        for path in paths {
            let out = parse_whole(&path, &opts)?;
            anyhow::ensure!(
                out.errors.is_empty(),
                "fixture {path:?} did not parse cleanly: {:?}",
                out.errors
            );
            anyhow::ensure!(
                !out.docs.is_empty(),
                "fixture {path:?} produced no documents"
            );
            transcripts.push(Transcript {
                session: out.session,
                docs: out.docs,
            });
        }
        Ok(Corpus { transcripts })
    }

    /// Every document, in corpus order.
    pub fn docs(&self) -> impl Iterator<Item = &Doc> {
        self.transcripts.iter().flat_map(|t| t.docs.iter())
    }

    /// Every document reference the fixture is allowed to name.
    pub fn refs(&self) -> BTreeSet<String> {
        self.docs().map(doc_ref).collect()
    }

    /// Build a single-segment RAM index of the whole corpus.
    ///
    /// One writer thread and exactly one commit, deliberately: `TopDocs` breaks a BM25 tie on
    /// the document address, so with more than one segment the order of equally-scored hits
    /// depends on how a merge happened to land, and a snapshot of a table holding tied scores
    /// would flap between runs. The single segment is asserted rather than assumed.
    pub fn index(&self, variant: Variant) -> anyhow::Result<(Index, Fields)> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema.clone());
        // Both analyzers, by name, on every index the harness builds — the writer and the
        // `QueryParser` both look them up and a missing registration is an error deep in a
        // stack rather than at open time.
        tokenizer::register(&index);
        let mut writer = index.writer_with_num_threads(1, 15_000_000)?;
        for transcript in &self.transcripts {
            for doc in &transcript.docs {
                let session = match variant {
                    // The ablated arm composes the header and then drops it, rather than
                    // skipping the composition: every *other* field then comes off exactly the
                    // same code path on both arms, which is what makes them comparable.
                    Variant::WithContext | Variant::WithoutContext => Some(&transcript.session),
                    Variant::SessionRowOnly => None,
                };
                let mut json = doc_to_json(doc, session, true);
                if variant == Variant::WithoutContext {
                    // The ablation, in one line. Do not "simplify" this into rebuilding the
                    // `Doc` with its context-bearing fields cleared: that empties the `project`
                    // and `git_branch` schema fields too and breaks the filters that the
                    // filtered class measures on both arms.
                    json.as_object_mut()
                        .expect("doc_to_json returns a JSON object")
                        .remove("context_text");
                }
                writer.add_document(TantivyDocument::parse_json(&schema, &json.to_string())?)?;
            }
        }
        writer.commit()?;

        let searcher = index.reader()?.searcher();
        anyhow::ensure!(
            searcher.segment_readers().len() == 1,
            "the corpus must be one segment so tie order cannot depend on a merge, got {}",
            searcher.segment_readers().len()
        );
        Ok((index, fields))
    }

    /// A markdown listing of every document reference and what is in it, written beside the
    /// report so that whoever adds a query to the fixture can find the reference to grade
    /// without reading the JSONL.
    pub fn reference_table(&self) -> String {
        let mut out = String::from("# Eval corpus reference\n\n");
        for transcript in &self.transcripts {
            let session = &transcript.session;
            out.push_str(&format!(
                "## `{}`\n\n- title: {}\n- first prompt: {}\n- project: {}\n- branch: {}\n\
                 - documents: {}\n\n| ref | kind | role | tool | excerpt |\n\
                 | --- | --- | --- | --- | --- |\n",
                session.source_path.rsplit('/').next().unwrap_or_default(),
                quoted(session.title.as_deref()),
                quoted(session.first_prompt.as_deref()),
                quoted(session.project.as_deref()),
                quoted(session.git_branch.as_deref()),
                transcript.docs.len(),
            ));
            for doc in &transcript.docs {
                let body = if doc.body.trim().is_empty() {
                    doc.tool_output.as_deref().unwrap_or_default()
                } else {
                    &doc.body
                };
                out.push_str(&format!(
                    "| `{}` | {} | {} | {} | {} |\n",
                    doc_ref(doc),
                    doc.kind.as_str(),
                    doc.role,
                    doc.tool_name.as_deref().unwrap_or("—"),
                    excerpt(body, 90),
                ));
            }
            out.push('\n');
        }
        out
    }
}

fn quoted(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("`{}`", v.replace('|', "\\|")),
        None => "—".to_string(),
    }
}

/// One line of a body, collapsed and capped, safe to put in a markdown table cell.
fn excerpt(text: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = flat.replace('|', "\\|").replace('`', "'");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Recursive, sorted-at-the-caller walk. `walkdir` would do, but the corpus is six files in two
/// directories and std keeps the harness's dependency surface at zero.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_jsonl(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}
