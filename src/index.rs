//! Incremental indexer, per-file watermarks and `sessions.json`.
//!
//! The whole module exists to make one property true: **re-running `index` over unchanged
//! transcripts must add zero documents**, and appending one record to a live transcript must
//! add exactly the documents that record yields.
//!
//! How that is achieved, per `docs/DESIGN.md`:
//!
//! * A watermark `{size, mtime_ms, byte_offset, docs, carry}` per file in
//!   `<index>/state.json`. Unchanged `size` **and** `mtime_ms` skips the file without opening it.
//! * A grown file is tailed from `byte_offset` with `seq_base` = the recorded `docs` count, so
//!   `Doc::seq` (and therefore `doc_id`) keeps counting where the last run stopped — and with
//!   the recorded `carry`, which is what makes a tail agree with a whole-file parse across the
//!   boundaries this format puts in the way (a `tool_use` and its `tool_result`; the block
//!   records of one API message). Before a tail is trusted, the fingerprint of the last line it
//!   consumed is re-checked, because a rewind or `resetSessionFile()` can rewrite the file in
//!   place and leave it the same size or larger.
//! * A shrunk file, an mtime that went backwards, `--full`, or a *missing* watermark all mean
//!   "reparse the whole thing", and every such file is first cleared with
//!   `delete_term(source_path)`. Making a missing watermark a reset is what keeps a lost or
//!   corrupt `state.json` from duplicating documents: the delete runs before the re-add.
//! * Files are parsed in parallel with rayon, but a **single** consumer — the thread that owns
//!   the `IndexWriter` — performs every delete and add, and there is exactly one `commit()`.
//! * Transcripts are read through `BufReader`, never memory-mapped: they are appended to
//!   while we read, and a truncation under a mapping raises an uncatchable `SIGBUS`.
//!
//! `<index>/sessions.json` is merged rather than clobbered, because a session's title arrives
//! in a `summary` sidecar record that may be appended long after the messages it titles. It is
//! keyed by transcript path, one row per file: a session can start a new file mid-life and two
//! files may share a `sessionId` (§9), and merging those loses both their counts.
//!
//! Both JSON files are written to a temporary file in the same directory and renamed, so a
//! crash mid-write cannot leave a half-written watermark behind.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tantivy::schema::Schema;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use crate::discovery::{TranscriptFile, discover};
use crate::parse::{FileContext, ParseCarry, ParseOptions, ParseOutput, SessionInfo, parse_file};
use crate::schema::{Fields, build_schema, doc_to_json};

const TANTIVY_SUBDIR: &str = "tantivy";
const STATE_FILE: &str = "state.json";
const SESSIONS_FILE: &str = "sessions.json";
/// Bumped whenever the *shape* of an indexed document changes, because the watermarks in
/// `state.json` otherwise say "nothing changed" and no existing index would ever gain the new
/// field. A mismatch makes [`load_state`] start from an empty state, which reindexes every
/// file from byte zero.
///
/// 1 -> 2: `roots` and `thinking_indexed` joined the state file.
/// 2 -> 3: the `bash_cmd` field. Existing indexes are fully reindexed on the next run.
/// 3 -> 4: the markdown split. A carried tool-call document now holds `text` as a list of
///         blocks with `body`, `code`, `headings` and `code_lang` beside it, so a state file
///         written by an older version cannot be read back into one; and the schema gained
///         those fields with two new analyzers besides.
/// 4 -> 5: `turn_seq` on every document and `open_turn_seq` in the carry. Without the bump an
///         index built by an older binary would report `turn_seq = 0` for every document,
///         which is worse than a rebuild.
const STATE_VERSION: u32 = 5;

/// Tantivy refuses a per-thread arena below this (`MEMORY_BUDGET_NUM_BYTES_MIN`).
const MIN_HEAP_BYTES: usize = 15_000_000;

/// How many parsed files may sit in flight between the rayon parsers and the single writer.
/// Bounded so a fast parser cannot pull an entire `~/.claude/projects` into memory.
const CHANNEL_DEPTH: usize = 8;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub full: bool,
    pub jobs: Option<usize>,
    pub include_thinking: bool,
    /// Follow `Full output saved to: <path>` pointers into `tool-results/<id>.txt`, so an
    /// oversized tool result is searchable rather than just its "output too large" stub.
    pub load_spilled_results: bool,
    /// Cap on each indexed body field of one document. See [`crate::parse::ParseOptions`] for
    /// why the default is where it is.
    pub max_text_bytes: usize,
    pub heap_bytes: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            full: false,
            jobs: None,
            include_thinking: true,
            load_spilled_results: true,
            max_text_bytes: 1024 * 1024,
            heap_bytes: 200 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct IndexStats {
    pub files_scanned: usize,
    pub files_updated: usize,
    pub files_reset: usize,
    pub docs_added: u64,
    pub docs_deleted: u64,
    pub sessions: usize,
    pub parse_errors: u64,
    pub elapsed_ms: u128,
}

// ---------------------------------------------------------------------------
// watermarks
// ---------------------------------------------------------------------------

/// One file's watermark. `byte_offset` is the end of the last *complete* line consumed, which
/// is not always `size`: a live transcript can have a partially written final line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct FileState {
    size: u64,
    mtime_ms: i64,
    byte_offset: u64,
    /// Number of documents this file has contributed so far — the next run's `seq_base`.
    docs: u64,
    /// What the parser has to be told to make the next tail parse agree with a whole-file
    /// one, plus the fingerprint of the last line consumed. See [`ParseCarry`].
    carry: ParseCarry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct State {
    version: u32,
    /// The transcript roots this index was built from, canonicalized.
    ///
    /// An index is bound to its corpus. Without this, an auto-refresh resolves the *default*
    /// root and silently merges a second corpus into the index — which is exactly how an index
    /// built over a 12-file snapshot came to report 32 files and answer questions about
    /// sessions that were not in the snapshot at all.
    roots: Vec<String>,
    /// Whether thinking text was indexed. A query-time `--include-thinking` against an index
    /// built without it silently matches nothing, so the query side warns instead.
    thinking_indexed: bool,
    files: BTreeMap<String, FileState>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: STATE_VERSION,
            roots: Vec::new(),
            thinking_indexed: false,
            files: BTreeMap::new(),
        }
    }
}

/// What this run must do with one discovered file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Byte-for-byte identical to the last run: never opened.
    Skip,
    /// Clear every document carrying this `source_path`, then parse from byte 0.
    Reset,
    /// Parse only the bytes appended since the last run.
    Tail { from: u64, seq_base: u64 },
}

struct Job {
    /// `source_path` as it is stored in Tantivy, and the key in both `state.json` and
    /// `sessions.json`.
    key: String,
    file: TranscriptFile,
    prev: Option<FileState>,
    plan: Plan,
}

impl Job {
    /// What the parser cannot work out from the bytes of this file alone: the subagent type
    /// from the `.meta.json` beside it, and — for a tail — what the last parse left behind.
    fn file_context(&self) -> FileContext {
        FileContext {
            agent_type: self
                .file
                .meta
                .as_ref()
                .and_then(|m| m.agent_type.clone())
                .filter(|t| !t.is_empty()),
            carry: match (self.plan, &self.prev) {
                (Plan::Tail { .. }, Some(prev)) => prev.carry.clone(),
                // A reset re-reads the file from byte zero, so nothing carries over.
                _ => ParseCarry::default(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// Open `<index_dir>/tantivy`, creating it with the pinned schema if absent.
///
/// If an index exists whose schema differs from the current one it cannot be read, so it is
/// discarded and rebuilt — together with `state.json` and `sessions.json`, which would
/// otherwise claim documents that no longer exist. A field's tokenizer *name* is part of its
/// schema, so changing the `code` analyzer's name — or moving a field onto it — triggers that
/// rebuild by itself.
///
/// The returned index always has the `code` analyzer registered.
pub fn open_or_create(index_dir: &Path) -> Result<(Index, Fields)> {
    let dir = index_dir.join(TANTIVY_SUBDIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating index directory {}", dir.display()))?;

    let (schema, fields) = build_schema();
    let mmap = tantivy::directory::MmapDirectory::open(&dir)
        .with_context(|| format!("opening index directory {}", dir.display()))?;

    let index = match Index::open_or_create(mmap, schema.clone()) {
        Ok(index) => index,
        Err(tantivy::TantivyError::SchemaError(message)) => {
            tracing::warn!(
                %message,
                dir = %dir.display(),
                "index schema changed; discarding and rebuilding the index"
            );
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing stale index at {}", dir.display()))?;
            std::fs::create_dir_all(&dir)?;
            // The watermarks describe documents that no longer exist.
            let _ = std::fs::remove_file(index_dir.join(STATE_FILE));
            let _ = std::fs::remove_file(index_dir.join(SESSIONS_FILE));
            Index::create_in_dir(&dir, schema)
                .with_context(|| format!("creating a fresh tantivy index at {}", dir.display()))?
        }
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("opening tantivy index at {}", dir.display()));
        }
    };
    // Before any writer or `QueryParser` exists: both look the `code` analyzer up by name.
    crate::tokenizer::register(&index);
    Ok((index, fields))
}

/// One incremental pass over every transcript under `roots`.
pub fn run(index_dir: &Path, roots: &[PathBuf], opts: &IndexOptions) -> Result<IndexStats> {
    let started = Instant::now();
    let (index, fields) = open_or_create(index_dir)?;
    let schema = index.schema();

    // Canonical roots keep `source_path` — the delete key — stable no matter how the caller
    // spelled the path.
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()))
        .collect();
    let mut state = load_state(index_dir);

    // An index belongs to one corpus. Pointing a second corpus at it would merge the two with
    // no way to tell them apart afterwards, and every count the tool reports would silently
    // describe a union nobody asked for.
    let requested: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    if !state.roots.is_empty() && state.roots != requested {
        if opts.full {
            // A full rebuild drops every document anyway, so re-pointing is well defined.
            tracing::info!(from = ?state.roots, to = ?requested, "rebuilding index against new roots");
        } else {
            anyhow::bail!(
                "this index was built from {} but you asked to index {}.\n\
                 Merging two corpora into one index would make every count describe their union.\n\
                 Use a different --index for the new corpus, or `index --full` to rebuild this \
                 one against it.",
                state.roots.join(", "),
                requested.join(", "),
            );
        }
    }

    let files = discover(&roots)?;
    let mut sessions = match load_sessions(index_dir) {
        Ok(sessions) => sessions,
        Err(err) => {
            // Session metadata for untouched files would be lost forever, so pay for a full
            // rebuild instead: every file becomes a reset and re-contributes its session.
            tracing::warn!(error = %err, "sessions.json unreadable; reindexing everything");
            state.files.clear();
            BTreeMap::new()
        }
    };

    let mut stats = IndexStats {
        files_scanned: files.len(),
        ..IndexStats::default()
    };

    let jobs: Vec<Job> = files
        .into_iter()
        .filter_map(|file| {
            let key = file.path.display().to_string();
            let prev = state.files.get(&key).cloned();
            let mut plan = plan_for(&file, prev.as_ref(), opts.full);
            // A file can be rewritten in place and still be larger than it was — a rewind,
            // `resetSessionFile()`, a restored fork (§9). Size and mtime cannot tell that from
            // an append, so the last line we actually read is re-checked before trusting the
            // watermark; if it is gone, everything we recorded describes bytes that no longer
            // exist and the file has to be reparsed.
            if let (Plan::Tail { .. }, Some(prev)) = (plan, prev.as_ref())
                && !tail_intact(&file.path, prev)
            {
                tracing::debug!(path = %key, "rewritten in place; reparsing from zero");
                plan = Plan::Reset;
            }
            if plan == Plan::Skip {
                tracing::debug!(path = %key, "unchanged, skipping");
                return None;
            }
            Some(Job {
                key,
                file,
                prev,
                plan,
            })
        })
        .collect();

    let parse_opts = ParseOptions {
        max_text_bytes: opts.max_text_bytes,
        load_spilled_results: opts.load_spilled_results,
    };
    let pool = match opts.jobs {
        Some(n) if n > 0 => Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build()
                .context("building the transcript parse thread pool")?,
        ),
        _ => None,
    };

    let mut writer: IndexWriter<TantivyDocument> = index
        .writer::<TantivyDocument>(opts.heap_bytes.max(MIN_HEAP_BYTES))
        .context("opening the tantivy index writer")?;

    // Rayon parses; this thread — the only one holding the writer — consumes.
    let mut fatal: Option<anyhow::Error> = None;
    let mut replaced: BTreeSet<String> = BTreeSet::new();
    {
        type Parsed = (usize, Result<(ParseOutput, u64)>);
        let (tx, rx) = sync_channel::<Parsed>(CHANNEL_DEPTH);
        let jobs_ref = &jobs;
        let parse_opts_ref = &parse_opts;

        std::thread::scope(|scope| {
            scope.spawn(move || {
                let work = || {
                    jobs_ref
                        .par_iter()
                        .enumerate()
                        .for_each_with(tx, |tx, (i, job)| {
                            let (from, seq_base) = match job.plan {
                                Plan::Tail { from, seq_base } => (from, seq_base),
                                _ => (0, 0),
                            };
                            let ctx = job.file_context();
                            let parsed =
                                parse_file(&job.file.path, from, seq_base, parse_opts_ref, &ctx)
                                    .with_context(|| {
                                        format!("parsing {}", job.file.path.display())
                                    });
                            // A closed receiver only happens if the consumer already died.
                            let _ = tx.send((i, parsed));
                        });
                };
                match &pool {
                    Some(pool) => pool.install(work),
                    None => work(),
                }
            });

            // Drain to completion even after a fatal error, or the bounded channel would
            // wedge the parser threads and `scope` would never join.
            for (i, parsed) in rx {
                if fatal.is_some() {
                    continue;
                }
                let job = &jobs[i];
                let (out, consumed) = match parsed {
                    Ok(v) => v,
                    Err(err) => {
                        // A transcript that vanished or became unreadable mid-run keeps its
                        // old watermark and is retried next time.
                        tracing::warn!(path = %job.key, error = %err, "skipping transcript");
                        continue;
                    }
                };
                if let Err(err) = consume(
                    &mut writer,
                    &schema,
                    &fields,
                    opts,
                    job,
                    out,
                    consumed,
                    &mut state,
                    &mut sessions,
                    &mut replaced,
                    &mut stats,
                ) {
                    fatal = Some(err);
                }
            }
        });
    }

    if let Some(err) = fatal {
        // Dropping the writer without committing rolls the whole run back.
        return Err(err);
    }

    prune_vanished(&mut writer, &fields, &mut state, &mut sessions, &mut stats);

    writer.commit().context("committing the tantivy index")?;

    // Bind the index to the corpus it now holds, so a later auto-refresh cannot wander off to
    // the default root and merge a second one in.
    state.roots = requested;
    state.thinking_indexed = opts.include_thinking;

    // Written after the commit: the watermark must never claim documents the index does not
    // yet hold. A crash in this window costs a re-parse, never a lost document.
    write_json_atomic(&index_dir.join(SESSIONS_FILE), &sessions)?;
    write_json_atomic(&index_dir.join(STATE_FILE), &state)?;

    stats.sessions = sessions.len();
    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

/// Read `<index_dir>/sessions.json`, keyed by **absolute transcript path**.
///
/// One entry per file, not per session id: TRANSCRIPT-FORMAT §9 lets a session start a new
/// file mid-life (`resetSessionFile()`) or move project directories (`relocated`), and two
/// files sharing a `sessionId` are two transcripts with their own counts, project and opening
/// prompt. The display key (`"<session_id>[:<agent_id>]"`) is derived at render time.
///
/// A missing file is an empty map, not an error — nothing has been indexed yet.
pub fn load_sessions(index_dir: &Path) -> Result<BTreeMap<String, SessionInfo>> {
    let path = index_dir.join(SESSIONS_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("reading {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

// ---------------------------------------------------------------------------
// the writer side
// ---------------------------------------------------------------------------

/// Apply one parsed file to the index, the watermarks and the session map.
#[allow(clippy::too_many_arguments)]
fn consume(
    writer: &mut IndexWriter<TantivyDocument>,
    schema: &Schema,
    fields: &Fields,
    opts: &IndexOptions,
    job: &Job,
    out: ParseOutput,
    consumed: u64,
    state: &mut State,
    sessions: &mut BTreeMap<String, SessionInfo>,
    replaced: &mut BTreeSet<String>,
    stats: &mut IndexStats,
) -> Result<()> {
    stats.files_updated += 1;
    stats.parse_errors += out.errors.len() as u64;
    for err in &out.errors {
        tracing::debug!("{err}");
    }

    let is_reset = job.plan == Plan::Reset;
    if is_reset {
        // Deletes apply to documents added before this point, so delete-then-add inside one
        // commit is the supported "replace" pattern.
        writer.delete_term(Term::from_field_text(fields.source_path, &job.key));
        if let Some(prev) = &job.prev {
            stats.files_reset += 1;
            stats.docs_deleted += prev.docs;
        }
    }

    // A replacement completes a document an earlier run wrote before its tool result existed.
    // Delete first: within one commit, a delete applies to everything indexed before it, so
    // delete-then-add is the supported "replace" pattern.
    for doc in &out.replacements {
        writer.delete_term(Term::from_field_text(fields.doc_id, &doc.doc_id));
        stats.docs_deleted += 1;
    }
    for doc in out.docs.iter().chain(out.replacements.iter()) {
        let json = doc_to_json(doc, opts.include_thinking).to_string();
        match TantivyDocument::parse_json(schema, &json) {
            Ok(td) => {
                writer
                    .add_document(td)
                    .with_context(|| format!("indexing document {}", doc.doc_id))?;
                stats.docs_added += 1;
            }
            // One unrepresentable document must not sink the run; `seq` still advances so the
            // watermark stays consistent with what the parser numbered.
            Err(err) => tracing::warn!(
                doc_id = %doc.doc_id,
                error = %err,
                "document rejected by the schema; skipping"
            ),
        }
    }

    let seq_base = match job.plan {
        Plan::Tail { seq_base, .. } => seq_base,
        _ => 0,
    };
    state.files.insert(
        job.key.clone(),
        FileState {
            // `size`/`mtime_ms` are the values observed *before* reading, so a file that grew
            // while we read it still looks changed next run and gets tailed rather than
            // skipped. `consumed` can exceed the stat when that happens.
            size: job.file.size.max(consumed),
            mtime_ms: job.file.mtime_ms,
            byte_offset: consumed,
            // Replacements reuse `seq` numbers an earlier run already handed out, so they do
            // not advance the watermark.
            docs: seq_base + out.docs.len() as u64,
            carry: out.carry,
        },
    );

    let info = session_info(job, out.session);
    // A zero-byte transcript, or one holding nothing but sidecar records, has no session to
    // report. Inserting it anyway puts an empty row in `sessions` and inflates `stats`.
    let key = job.key.clone();
    if out.docs.is_empty()
        && out.replacements.is_empty()
        && is_blank(&info)
        && !sessions.contains_key(&key)
    {
        return Ok(());
    }
    // The first reset of a key this run replaces its entry outright — a full reparse already
    // recounted everything. Every other outcome merges.
    if is_reset && replaced.insert(key.clone()) {
        sessions.insert(key, info);
    } else {
        match sessions.entry(key) {
            Entry::Occupied(mut e) => merge_session(e.get_mut(), info),
            Entry::Vacant(e) => {
                e.insert(info);
            }
        }
    }
    Ok(())
}

/// Nothing worth listing: no counts, no title, no project, no timestamps.
fn is_blank(info: &SessionInfo) -> bool {
    info.messages == 0
        && info.tool_calls == 0
        && info.title.is_none()
        && info.description.is_none()
        && info.project.is_none()
        && info.first_ts_ms.is_none()
        && info.first_prompt.is_none()
}

/// Is the last line the previous run consumed still exactly where — and what — it was?
///
/// `size`/`mtime` only ever say "this file changed"; they cannot distinguish an append from a
/// rewrite that happens to end up the same size or larger, and a rewrite tailed from a stale
/// offset keeps documents for turns that no longer exist and never indexes their replacements.
/// A missing fingerprint (a `state.json` written before this existed) is treated as intact, so
/// upgrading does not force a full reindex.
fn tail_intact(path: &Path, prev: &FileState) -> bool {
    let Some(tail) = prev.carry.tail_line else {
        return true;
    };
    if tail.start >= prev.byte_offset {
        return false;
    }
    let len = (prev.byte_offset - tail.start) as usize;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    if file.seek(std::io::SeekFrom::Start(tail.start)).is_err() {
        return false;
    }
    let mut buf = vec![0u8; len];
    match file.read_exact(&mut buf) {
        Ok(()) => crate::parse::fnv1a(&buf) == tail.hash,
        Err(_) => false,
    }
}

/// Drop watermarks (and documents, and session entries) for transcripts that no longer exist.
/// Keyed on "the path is gone from disk", not "the path was not discovered", so indexing a
/// single `--root` never evicts the rest of the index.
fn prune_vanished(
    writer: &mut IndexWriter<TantivyDocument>,
    fields: &Fields,
    state: &mut State,
    sessions: &mut BTreeMap<String, SessionInfo>,
    stats: &mut IndexStats,
) {
    let gone: Vec<String> = state
        .files
        .keys()
        .filter(|key| !Path::new(key).exists())
        .cloned()
        .collect();
    for key in gone {
        if let Some(prev) = state.files.remove(&key) {
            tracing::debug!(path = %key, "transcript vanished; dropping its documents");
            writer.delete_term(Term::from_field_text(fields.source_path, &key));
            stats.docs_deleted += prev.docs;
        }
        sessions.remove(&key);
    }
}

fn plan_for(file: &TranscriptFile, prev: Option<&FileState>, full: bool) -> Plan {
    let Some(prev) = prev else {
        // No watermark: reparse from zero, and delete first in case the index still holds
        // documents from a run whose state.json was lost.
        return Plan::Reset;
    };
    if full {
        return Plan::Reset;
    }
    if file.size == prev.size && file.mtime_ms == prev.mtime_ms {
        return Plan::Skip;
    }
    // Truncated, rewritten in place, or restored from a backup: nothing we recorded can be
    // trusted to still describe the same bytes.
    if file.size < prev.size || file.size < prev.byte_offset || file.mtime_ms < prev.mtime_ms {
        return Plan::Reset;
    }
    Plan::Tail {
        from: prev.byte_offset,
        seq_base: prev.docs,
    }
}

// ---------------------------------------------------------------------------
// session bookkeeping
// ---------------------------------------------------------------------------

/// Fill in what only the indexer knows: the ids the filename carries, and the subagent
/// `.meta.json` that lives beside the transcript rather than inside it.
fn session_info(job: &Job, mut info: SessionInfo) -> SessionInfo {
    if info.session_id.is_empty() {
        info.session_id = job.file.session_id.clone();
    }
    if info.agent_id.is_none() {
        info.agent_id = job.file.agent_id.clone();
    }
    if let Some(meta) = &job.file.meta {
        if info.agent_type.is_none() {
            info.agent_type = meta.agent_type.clone();
        }
        if info.description.is_none() {
            info.description = meta.description.clone();
        }
    }
    if info.source_path.is_empty() {
        info.source_path = job.key.clone();
    }
    info
}

/// Merge a tail's contribution into what is already known. Newer facts win over older ones;
/// an absent newer fact never erases an older one, because a tail parse only sees the records
/// it read. Counts accumulate; the first prompt, being the *first*, does not move.
fn merge_session(dst: &mut SessionInfo, src: SessionInfo) {
    if dst.session_id.is_empty() {
        dst.session_id = src.session_id;
    }
    if src.agent_id.is_some() {
        dst.agent_id = src.agent_id;
    }
    if src.agent_type.is_some() {
        dst.agent_type = src.agent_type;
    }
    if src.description.is_some() {
        dst.description = src.description;
    }
    if src.title.is_some() {
        dst.title = src.title;
    }
    if src.slug.is_some() {
        dst.slug = src.slug;
    }
    if src.project.is_some() {
        dst.project = src.project;
    }
    if src.git_branch.is_some() {
        dst.git_branch = src.git_branch;
    }
    if !src.source_path.is_empty() {
        dst.source_path = src.source_path;
    }
    dst.first_ts_ms = match (dst.first_ts_ms, src.first_ts_ms) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    dst.last_ts_ms = match (dst.last_ts_ms, src.last_ts_ms) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    dst.messages += src.messages;
    dst.tool_calls += src.tool_calls;
    if dst.first_prompt.is_none() {
        dst.first_prompt = src.first_prompt;
    }
}

// ---------------------------------------------------------------------------
// state files
// ---------------------------------------------------------------------------

/// What an existing index is bound to. Lets the query side refresh the *right* corpus, warn
/// when `--include-thinking` cannot possibly match, and report its own scope.
#[derive(Debug, Clone, Default)]
pub struct IndexMeta {
    pub roots: Vec<PathBuf>,
    pub thinking_indexed: bool,
    pub files: usize,
}

/// Read an index's binding without opening the Tantivy index. An index that does not exist yet
/// reports empty roots, which callers read as "not bound to anything".
pub fn meta(index_dir: &Path) -> IndexMeta {
    let state = load_state(index_dir);
    IndexMeta {
        roots: state.roots.iter().map(PathBuf::from).collect(),
        thinking_indexed: state.thinking_indexed,
        files: state.files.len(),
    }
}

/// A missing, unreadable, malformed or wrong-version `state.json` all mean the same thing:
/// index everything again. That is safe because a file with no watermark is a `Reset`, and a
/// reset deletes by `source_path` before it adds.
fn load_state(index_dir: &Path) -> State {
    let path = index_dir.join(STATE_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return State::default(),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "cannot read state; reindexing");
            return State::default();
        }
    };
    match serde_json::from_slice::<State>(&bytes) {
        Ok(state) if state.version == STATE_VERSION => state,
        Ok(state) => {
            tracing::warn!(
                found = state.version,
                expected = STATE_VERSION,
                "state.json version mismatch; reindexing"
            );
            State::default()
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "corrupt state; reindexing");
            State::default()
        }
    }
}

/// Write to a temporary file in the same directory, flush it to disk, then rename over the
/// target. `rename` within a directory is atomic, so a crash leaves either the old file or
/// the new one — never a truncated watermark.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating state directory {}", dir.display()))?;

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    let tmp = dir.join(format!(".{}.{}.tmp", name, std::process::id()));

    let bytes = serde_json::to_vec_pretty(value).context("serialising index state")?;
    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {}", path.display()))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::Count;
    use tantivy::query::AllQuery;

    /// Documents currently visible in the committed index.
    fn live_docs(index_dir: &Path) -> u64 {
        let (index, _) = open_or_create(index_dir).unwrap();
        index.reader().unwrap().searcher().num_docs()
    }

    /// Documents the parser would produce for the whole file — the ground truth every
    /// incremental assertion is measured against.
    fn docs_in(path: &Path) -> u64 {
        let out = crate::parse::parse_whole(path, &ParseOptions::default()).unwrap();
        out.docs.len() as u64
    }

    fn user_line(uuid: &str, parent: Option<&str>, text: &str) -> String {
        let parent = match parent {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent},"timestamp":"2026-09-09T19:07:15.164Z","sessionId":"sess-1","cwd":"/home/user/proj","gitBranch":"main","version":"2.1.266","isSidechain":false,"entrypoint":"cli","origin":{{"kind":"human"}},"message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn assistant_line(uuid: &str, parent: &str, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","timestamp":"2026-09-09T19:07:19.248Z","sessionId":"sess-1","cwd":"/home/user/proj","gitBranch":"main","version":"2.1.266","isSidechain":false,"message":{{"role":"assistant","id":"msg_{uuid}","model":"claude-opus-5","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    fn tool_use_line(uuid: &str, parent: &str, tool_id: &str, command: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","timestamp":"2026-09-09T19:07:20.000Z","sessionId":"sess-1","cwd":"/home/user/proj","gitBranch":"main","isSidechain":false,"message":{{"role":"assistant","id":"msg_{uuid}","model":"claude-opus-5","content":[{{"type":"tool_use","id":"{tool_id}","name":"Bash","input":{{"command":"{command}"}}}}]}}}}"#
        )
    }

    fn tool_result_line(uuid: &str, parent: &str, tool_id: &str, output: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":"{parent}","timestamp":"2026-09-09T19:07:21.000Z","sessionId":"sess-1","cwd":"/home/user/proj","gitBranch":"main","isSidechain":false,"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_id}","is_error":false,"content":"{output}"}}]}},"toolUseResult":{{"stdout":"{output}","stderr":"","interrupted":false}}}}"#
        )
    }

    /// A three-turn transcript: a prompt, an answer, and a Bash call with its result.
    fn base_transcript() -> String {
        [
            user_line("u1", None, "index my transcripts"),
            assistant_line("a1", "u1", "on it"),
            tool_use_line("a2", "a1", "toolu_1", "cargo build"),
            tool_result_line("u2", "a2", "toolu_1", "Finished dev profile"),
        ]
        .join("\n")
            + "\n"
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        index_dir: PathBuf,
        root: PathBuf,
        transcript: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture::with_body(&base_transcript())
        }

        fn with_body(body: &str) -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("claude/projects");
            let project = root.join("-home-user-proj");
            std::fs::create_dir_all(&project).unwrap();
            let transcript = project.join("sess-1.jsonl");
            std::fs::write(&transcript, body).unwrap();
            Fixture {
                index_dir: tmp.path().join("index"),
                root,
                transcript,
                _tmp: tmp,
            }
        }

        fn index(&self) -> IndexStats {
            self.index_with(&IndexOptions::default())
        }

        fn index_with(&self, opts: &IndexOptions) -> IndexStats {
            run(&self.index_dir, std::slice::from_ref(&self.root), opts).unwrap()
        }

        fn append(&self, line: &str) {
            let mut body = std::fs::read_to_string(&self.transcript).unwrap();
            body.push_str(line);
            body.push('\n');
            std::fs::write(&self.transcript, body).unwrap();
        }

        fn live(&self) -> u64 {
            live_docs(&self.index_dir)
        }

        fn expected(&self) -> u64 {
            docs_in(&self.transcript)
        }

        fn state(&self) -> State {
            load_state(&self.index_dir)
        }

        fn sessions(&self) -> BTreeMap<String, SessionInfo> {
            load_sessions(&self.index_dir).unwrap()
        }
    }

    /// The bug this guards, found by an A/B eval whose corpus it silently corrupted: an index
    /// built over one corpus was refreshed against the *default* root, merging a second corpus
    /// in. Counts then described the union — 32 files where the corpus had 12, "28 subagent
    /// sessions" where there were 11 — with nothing in the output admitting it.
    #[test]
    fn an_index_refuses_a_second_corpus() {
        let a = Fixture::new();
        a.index();

        // A different root, same index directory.
        let other = tempfile::tempdir().unwrap();
        let root_b = other.path().join("claude/projects/-home-user-other");
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(root_b.join("sess-2.jsonl"), base_transcript()).unwrap();
        let root_b = other.path().join("claude/projects");

        let err = run(
            &a.index_dir,
            std::slice::from_ref(&root_b),
            &IndexOptions::default(),
        )
        .expect_err("indexing a second corpus into one index must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("built from"), "{msg}");
        assert!(msg.contains("--index"), "unhelpful message: {msg}");

        // The index is untouched by the refusal.
        assert_eq!(meta(&a.index_dir).roots.len(), 1);

        // --full re-points it, because a rebuild drops everything anyway.
        run(
            &a.index_dir,
            std::slice::from_ref(&root_b),
            &IndexOptions {
                full: true,
                ..IndexOptions::default()
            },
        )
        .expect("--full may re-point an index at a new corpus");
        assert_eq!(
            meta(&a.index_dir).roots,
            [std::fs::canonicalize(&root_b).unwrap()]
        );
    }

    /// Re-indexing the same corpus, spelled differently, is not a second corpus.
    #[test]
    fn re_indexing_the_same_root_is_not_a_conflict() {
        let fx = Fixture::new();
        fx.index();
        let before = fx.live();
        // A trailing slash and a `.` component spell the same directory.
        let noisy = fx.root.join(".");
        run(
            &fx.index_dir,
            std::slice::from_ref(&noisy),
            &IndexOptions::default(),
        )
        .expect("the same root spelled differently must not conflict");
        assert_eq!(fx.live(), before, "no documents added on a no-op re-run");
    }

    /// Thinking is indexed by default now, and the index records the choice so the query side
    /// can tell a caller why `--include-thinking` is matching nothing.
    #[test]
    fn thinking_is_indexed_by_default_and_the_choice_is_recorded() {
        assert!(
            IndexOptions::default().include_thinking,
            "thinking should be indexed unless opted out"
        );

        let fx = Fixture::new();
        fx.index();
        assert!(meta(&fx.index_dir).thinking_indexed);

        let opted_out = Fixture::new();
        opted_out.index_with(&IndexOptions {
            include_thinking: false,
            ..IndexOptions::default()
        });
        assert!(!meta(&opted_out.index_dir).thinking_indexed);
    }

    #[test]
    fn fixture_transcript_actually_produces_documents() {
        // Guards every other test in this module: if the fixture stopped parsing, the
        // "zero added" assertions would pass vacuously.
        let fx = Fixture::new();
        assert!(fx.expected() >= 3, "fixture yielded {}", fx.expected());
    }

    #[test]
    fn first_run_indexes_everything() {
        let fx = Fixture::new();
        let stats = fx.index();
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_reset, 0, "a first-time file is not a reset");
        assert_eq!(stats.docs_added, fx.expected());
        assert_eq!(stats.docs_deleted, 0);
        assert_eq!(stats.parse_errors, 0);
        assert_eq!(stats.sessions, 1);
        assert_eq!(fx.live(), fx.expected());
    }

    #[test]
    fn second_run_adds_nothing() {
        let fx = Fixture::new();
        let first = fx.index();
        let second = fx.index();

        assert_eq!(second.docs_added, 0, "re-indexing must add no documents");
        assert_eq!(second.docs_deleted, 0);
        assert_eq!(second.files_scanned, 1);
        assert_eq!(second.files_updated, 0, "the file must not even be opened");
        assert_eq!(second.files_reset, 0);
        assert_eq!(fx.live(), first.docs_added);
    }

    #[test]
    fn appending_one_record_adds_exactly_its_documents() {
        let fx = Fixture::new();
        let before = fx.index().docs_added;

        fx.append(&assistant_line("a3", "u2", "done"));
        let after_total = fx.expected();
        let stats = fx.index();

        assert_eq!(stats.files_updated, 1);
        assert_eq!(stats.files_reset, 0, "an append is a tail, not a reset");
        assert_eq!(stats.docs_added, after_total - before);
        assert_eq!(stats.docs_deleted, 0);
        assert_eq!(fx.live(), after_total);
    }

    #[test]
    fn appended_documents_continue_the_seq_numbering() {
        let fx = Fixture::new();
        let before = fx.index().docs_added;
        fx.append(&assistant_line("a3", "u2", "done"));
        fx.index();

        let key = fx.transcript.display().to_string();
        let watermark = fx.state().files[&key].clone();
        assert_eq!(watermark.docs, fx.expected());
        assert!(watermark.docs > before);
        assert_eq!(watermark.byte_offset, watermark.size);

        // Unique doc_ids are what make the delete key work; a restarted `seq` would collide.
        let out = crate::parse::parse_whole(&fx.transcript, &ParseOptions::default()).unwrap();
        let ids: BTreeSet<&str> = out.docs.iter().map(|d| d.doc_id.as_str()).collect();
        assert_eq!(ids.len(), out.docs.len());
    }

    #[test]
    fn truncation_forces_a_full_reparse() {
        let fx = Fixture::new();
        let before = fx.index().docs_added;

        // A shorter file at the same path: the recorded byte offset now points past the end.
        let shorter = user_line("u9", None, "started over") + "\n";
        std::fs::write(&fx.transcript, &shorter).unwrap();
        let expected = fx.expected();
        let stats = fx.index();

        assert_eq!(stats.files_reset, 1);
        assert_eq!(stats.docs_deleted, before);
        assert_eq!(stats.docs_added, expected);
        assert_eq!(fx.live(), expected, "stale documents must be gone");
    }

    #[test]
    fn mtime_going_backwards_forces_a_full_reparse() {
        let fx = Fixture::new();
        let expected = fx.index().docs_added;

        // Simulate a restored-from-backup file by dating the watermark into the future.
        let mut state = fx.state();
        let key = fx.transcript.display().to_string();
        state.files.get_mut(&key).unwrap().mtime_ms = i64::MAX;
        write_json_atomic(&fx.index_dir.join(STATE_FILE), &state).unwrap();

        let stats = fx.index();
        assert_eq!(stats.files_reset, 1);
        assert_eq!(stats.docs_added, expected);
        assert_eq!(fx.live(), expected, "the reparse must not duplicate");
    }

    #[test]
    fn full_reindex_replaces_rather_than_duplicates() {
        let fx = Fixture::new();
        let expected = fx.index().docs_added;

        let opts = IndexOptions {
            full: true,
            ..IndexOptions::default()
        };
        let stats = fx.index_with(&opts);
        assert_eq!(stats.files_reset, 1);
        assert_eq!(stats.docs_added, expected);
        assert_eq!(stats.docs_deleted, expected);
        assert_eq!(fx.live(), expected, "--full must not double the index");
    }

    #[test]
    fn a_lost_state_file_does_not_duplicate_documents() {
        let fx = Fixture::new();
        let expected = fx.index().docs_added;

        std::fs::remove_file(fx.index_dir.join(STATE_FILE)).unwrap();
        let stats = fx.index();

        assert_eq!(stats.docs_added, expected);
        assert_eq!(fx.live(), expected, "delete-by-source_path must have run");
    }

    #[test]
    fn a_corrupt_state_file_is_recovered_from() {
        let fx = Fixture::new();
        let expected = fx.index().docs_added;
        std::fs::write(fx.index_dir.join(STATE_FILE), "{ not json").unwrap();

        let stats = fx.index();
        assert_eq!(stats.docs_added, expected);
        assert_eq!(fx.live(), expected);
    }

    #[test]
    fn a_vanished_transcript_is_pruned() {
        let fx = Fixture::new();
        let expected = fx.index().docs_added;
        std::fs::remove_file(&fx.transcript).unwrap();

        let stats = fx.index();
        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.docs_deleted, expected);
        assert_eq!(fx.live(), 0);
        assert!(fx.state().files.is_empty());
        assert!(fx.sessions().is_empty());
    }

    #[test]
    fn a_partial_trailing_line_is_left_for_the_next_run() {
        let mut body = base_transcript();
        body.push_str(r#"{"type":"user","uuid":"u3","message":{"role":"user","content":"half"#);
        let fx = Fixture::with_body(&body);

        let first = fx.index();
        let key = fx.transcript.display().to_string();
        let watermark = fx.state().files[&key].clone();
        assert!(
            watermark.byte_offset < watermark.size,
            "the partial line must stay unconsumed"
        );

        // Completing the line indexes it once, and only it.
        fx.append("\"}}");
        let stats = fx.index();
        assert!(stats.docs_added >= 1);
        assert_eq!(fx.live(), first.docs_added + stats.docs_added);
        assert_eq!(fx.live(), fx.expected());
        assert_eq!(stats.parse_errors, 0);
    }

    #[test]
    fn a_late_summary_titles_the_session_without_losing_counts() {
        let fx = Fixture::new();
        fx.index();
        let before = fx.sessions();
        // `sessions.json` is keyed by transcript path: one entry per file, so two transcripts
        // that share a `sessionId` cannot merge into one row.
        let key = fx.transcript.display().to_string();
        let key = key.as_str();
        assert!(before[key].title.is_none());
        assert_eq!(
            before[key].first_prompt.as_deref(),
            Some("index my transcripts")
        );
        let messages_before = before[key].messages;
        assert!(messages_before > 0);

        // The title arrives long after the messages it titles.
        fx.append(r#"{"type":"summary","summary":"Indexing transcripts","leafUuid":"u2"}"#);
        fx.append(&assistant_line("a4", "u2", "and one more turn"));
        fx.index();

        let after = fx.sessions();
        assert_eq!(after[key].title.as_deref(), Some("Indexing transcripts"));
        assert!(
            after[key].messages > messages_before,
            "tail counts must accumulate, not replace"
        );
        assert_eq!(
            after[key].first_prompt.as_deref(),
            Some("index my transcripts"),
            "the first prompt must not move"
        );
        assert_eq!(after[key].project.as_deref(), Some("/home/user/proj"));
        assert_eq!(after[key].git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn subagent_transcripts_are_keyed_separately_and_carry_their_meta() {
        let fx = Fixture::new();
        let subagents = fx.transcript.parent().unwrap().join("sess-1/subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-a1b2.jsonl"),
            user_line("s1", None, "explore the format") + "\n",
        )
        .unwrap();
        std::fs::write(
            subagents.join("agent-a1b2.meta.json"),
            r#"{"agentType":"Explore","description":"Characterize transcript format"}"#,
        )
        .unwrap();

        let stats = fx.index();
        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.sessions, 2);

        let sessions = fx.sessions();
        let agent = &sessions[&subagents.join("agent-a1b2.jsonl").display().to_string()];
        assert_eq!(agent.agent_id.as_deref(), Some("a1b2"));
        assert_eq!(agent.agent_type.as_deref(), Some("Explore"));
        assert_eq!(
            agent.description.as_deref(),
            Some("Characterize transcript format")
        );

        // And re-indexing still adds nothing, with two files in play.
        assert_eq!(fx.index().docs_added, 0);
    }

    #[test]
    fn every_document_carries_its_source_path_delete_key() {
        let fx = Fixture::new();
        fx.index();
        let (index, fields) = open_or_create(&fx.index_dir).unwrap();
        let searcher = index.reader().unwrap().searcher();
        let term = Term::from_field_text(fields.source_path, &fx.transcript.display().to_string());
        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let matched = searcher.search(&query, &Count).unwrap() as u64;
        assert_eq!(matched, searcher.search(&AllQuery, &Count).unwrap() as u64);
        assert_eq!(matched, fx.expected());
    }

    #[test]
    fn a_single_job_still_indexes_everything() {
        let fx = Fixture::new();
        let opts = IndexOptions {
            jobs: Some(1),
            ..IndexOptions::default()
        };
        assert_eq!(fx.index_with(&opts).docs_added, fx.expected());
        assert_eq!(fx.index_with(&opts).docs_added, 0);
    }

    #[test]
    fn an_empty_root_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let stats = run(
            &tmp.path().join("index"),
            &[tmp.path().join("missing")],
            &IndexOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.docs_added, 0);
        assert_eq!(stats.sessions, 0);
    }

    /// The exact schema a build before the custom analyzers pinned: identical to today's but
    /// for the tokenizer name on every full-text field. Round-tripping through JSON keeps the
    /// two in step, so this test proves the *tokenizer* is what makes the old index unreadable
    /// and not some unrelated drift.
    fn schema_with_the_previous_tokenizer() -> tantivy::schema::Schema {
        let (schema, _) = build_schema();
        let mut json = serde_json::to_value(&schema).unwrap();
        for field in json.as_array_mut().unwrap() {
            if let Some(tokenizer) = field.pointer_mut("/options/indexing/tokenizer")
                && (*tokenizer == crate::tokenizer::CODE_ANALYZER
                    || *tokenizer == crate::tokenizer::PROSE_ANALYZER)
            {
                *tokenizer = serde_json::json!("default");
            }
        }
        serde_json::from_value(json).unwrap()
    }

    /// A field's tokenizer name is part of the schema, so naming an analyzer on `text`,
    /// `code`, `headings`, `tool_output`, `thinking` and `tool_input` makes every index an
    /// older build wrote unreadable. The recovery has to be automatic — nobody is going to be told to delete a
    /// directory — so `open_or_create` must notice, discard it, and hand back a clean index
    /// that the next `run` refills.
    #[test]
    fn an_index_built_with_the_previous_tokenizer_is_discarded_and_reindexed() {
        let fx = Fixture::new();
        let expected = fx.expected();
        let old = schema_with_the_previous_tokenizer();
        assert_ne!(old, build_schema().0, "the old schema must actually differ");

        // Plant exactly what the previous build left on disk: an index, its watermarks, and
        // the session titles that describe documents about to disappear.
        let dir = fx.index_dir.join(TANTIVY_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let stale = Index::create_in_dir(&dir, old.clone()).unwrap();
        let mut writer = stale.writer_with_num_threads(1, 15_000_000).unwrap();
        let json = r#"{"doc_id":"stale","source_path":"/gone.jsonl","session_id":"gone","role":"user","kind":"message","text":"open_or_create","seq":0}"#;
        writer
            .add_document(tantivy::TantivyDocument::parse_json(&old, json).unwrap())
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(stale);
        std::fs::write(fx.index_dir.join(STATE_FILE), "{}").unwrap();
        std::fs::write(fx.index_dir.join(SESSIONS_FILE), "{}").unwrap();

        // Opening is not an error, and what comes back is today's schema with nothing in it.
        let (index, _) = open_or_create(&fx.index_dir).unwrap();
        assert_eq!(index.schema(), build_schema().0);
        assert_eq!(
            index.reader().unwrap().searcher().num_docs(),
            0,
            "the documents indexed under the old tokenizer are gone"
        );
        assert!(
            index
                .tokenizers()
                .get(crate::tokenizer::CODE_ANALYZER)
                .is_some()
        );
        assert!(
            !fx.index_dir.join(STATE_FILE).exists(),
            "stale watermarks would suppress the reindex"
        );
        assert!(!fx.index_dir.join(SESSIONS_FILE).exists());
        drop(index);

        // And the next ordinary run reindexes the transcripts from scratch, then settles.
        assert_eq!(fx.index().docs_added, expected);
        assert_eq!(fx.live(), expected);
        assert_eq!(fx.index().docs_added, 0);
    }

    #[test]
    fn open_or_create_is_idempotent_and_creates_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested/index");
        let (first, _) = open_or_create(&dir).unwrap();
        drop(first);
        let (second, fields) = open_or_create(&dir).unwrap();
        assert!(second.schema().get_field("tool_input").is_ok());
        assert_eq!(fields.source_path, build_schema().1.source_path);
    }

    #[test]
    fn load_sessions_on_a_fresh_directory_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_sessions(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn state_writes_are_atomic_and_leave_no_temporary_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let mut state = State::default();
        state.files.insert(
            "/a/b.jsonl".to_string(),
            FileState {
                size: 10,
                mtime_ms: 20,
                byte_offset: 8,
                docs: 3,
                carry: ParseCarry::default(),
            },
        );
        write_json_atomic(&path, &state).unwrap();
        write_json_atomic(&path, &state).unwrap();

        let round_tripped: State = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(round_tripped.version, STATE_VERSION);
        assert_eq!(round_tripped.files["/a/b.jsonl"].docs, 3);

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    // -- regressions --------------------------------------------------------

    /// The property the whole module claims: indexing a live transcript one append at a time
    /// must land on exactly the document set a single pass over the finished file produces.
    ///
    /// The boundary between a `tool_use` and the `tool_result` that answers it is where this
    /// used to fail, and on a live session that is where the boundary almost always falls.
    #[test]
    fn a_tail_boundary_between_a_tool_use_and_its_result_adds_no_duplicate() {
        let head = [
            user_line("u1", None, "build it"),
            tool_use_line("a1", "u1", "toolu_1", "cargo build"),
        ]
        .join("\n")
            + "\n";
        let fx = Fixture::with_body(&head);
        let first = fx.index();
        assert_eq!(first.docs_added, 2);

        fx.append(&tool_result_line("u2", "a1", "toolu_1", "Finished"));
        let second = fx.index();

        // The result completes a document that is already indexed: one replaced, none added.
        assert_eq!(second.docs_added, 1);
        assert_eq!(second.docs_deleted, 1);
        assert_eq!(fx.live(), fx.expected(), "live == one-shot");
        let sessions = fx.sessions();
        let info = &sessions[&fx.transcript.display().to_string()];
        assert_eq!(info.tool_calls, 1, "one call, not two");

        // …and the surviving document is the whole one: name, input *and* result.
        let (index, fields) = open_or_create(&fx.index_dir).unwrap();
        let mut request = crate::search::SearchRequest {
            limit: 10,
            ..crate::search::SearchRequest::default()
        };
        request.filters.kind = Some("tool_call".into());
        let found = crate::search::search(&index, &fields, &request).unwrap();
        assert_eq!(found.total, 1, "no half-documents");
        let doc = &found.hits[0].doc;
        assert_eq!(doc.tool_name.as_deref(), Some("Bash"));
        assert_eq!(doc.tool_input.as_ref().unwrap()["command"], "cargo build");
        assert!(
            doc.text.iter().any(|t| t.contains("cargo build")),
            "{:?}",
            doc.text
        );
        assert!(
            doc.tool_output
                .as_deref()
                .is_some_and(|o| o.contains("Finished")),
            "the result is carried as tool_output: {:?}",
            doc.tool_output
        );

        // A third run with nothing new must still add nothing.
        assert_eq!(fx.index().docs_added, 0);
    }

    /// The mirror image: a boundary *inside* one API message must not count it twice.
    #[test]
    fn a_tail_boundary_inside_one_api_message_counts_it_once() {
        let block = |uuid: &str, index: usize, text: &str| {
            format!(
                r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":null,"timestamp":"2026-09-09T19:07:19.248Z","sessionId":"sess-1","cwd":"/home/user/proj","gitBranch":"main","isSidechain":false,"apiBlockIndex":{index},"message":{{"role":"assistant","id":"msg_SAME","model":"claude-opus-5","content":[{{"type":"text","text":"{text}"}}]}}}}"#
            )
        };
        let fx = Fixture::with_body(&(block("a1", 0, "part one") + "\n"));
        fx.index();
        fx.append(&block("a2", 1, "part two"));
        fx.index();

        let key = fx.transcript.display().to_string();
        let incremental = fx.sessions()[&key].messages;
        let one_shot = crate::parse::parse_whole(&fx.transcript, &ParseOptions::default())
            .unwrap()
            .session
            .messages;
        assert_eq!(incremental, one_shot);
        assert_eq!(incremental, 1);
    }

    /// A rewind, a `resetSessionFile()` or a restored fork rewrites bytes the watermark
    /// already covers. `size`/`mtime` cannot see that, so the last consumed line is
    /// fingerprinted and re-checked: if it is gone, the file is reparsed from zero.
    #[test]
    fn an_in_place_rewrite_is_reparsed_however_the_size_moved() {
        for (name, replacement) in [
            // A rewrite that leaves the file longer is the one `size`/`mtime` cannot see at
            // all; the same-size case is the one that used to be an absorbing state.
            ("grows", "CCCCCCCCCCCCCCCCCCCCCCCCCCCC"),
            ("same size", "CCCC"),
            ("shrinks", "CC"),
        ] {
            let fx = Fixture::with_body(
                &(user_line("u1", None, "AAAA")
                    + "\n"
                    + &user_line("u2", Some("u1"), "BBBB")
                    + "\n"),
            );
            fx.index();
            // `mtime_ms` has millisecond resolution and a same-size rewrite within the same
            // millisecond is indistinguishable from no change at all — that is what the
            // `size` + `mtime` watermark means. Anything a real editor or the CLI does lands
            // in a later millisecond.
            std::thread::sleep(std::time::Duration::from_millis(15));
            std::fs::write(
                &fx.transcript,
                user_line("u1", None, "AAAA")
                    + "\n"
                    + &user_line("u3", Some("u1"), replacement)
                    + "\n",
            )
            .unwrap();

            let stats = fx.index();
            assert_eq!(stats.files_reset, 1, "{name}: must not be tailed");
            assert_eq!(fx.live(), fx.expected(), "{name}");
            let texts = indexed_texts(&fx.index_dir);
            assert!(!texts.iter().any(|t| t == "BBBB"), "{name}: {texts:?}");
            assert!(texts.iter().any(|t| t == replacement), "{name}: {texts:?}");
        }
    }

    /// Two transcripts that share a `sessionId` (§9: `resetSessionFile()`, `relocated`) are two
    /// transcripts, with their own counts, project and opening prompt.
    #[test]
    fn two_files_sharing_a_session_id_stay_two_sessions() {
        let fx = Fixture::new();
        let other_project = fx.root.join("-home-user-other").join("sess-1.jsonl");
        std::fs::create_dir_all(other_project.parent().unwrap()).unwrap();
        std::fs::write(
            &other_project,
            user_line("z1", None, "the same id elsewhere") + "\n",
        )
        .unwrap();

        let stats = fx.index();
        assert_eq!(stats.sessions, 2, "one row per transcript");
        let sessions = fx.sessions();
        assert_eq!(
            sessions[&other_project.display().to_string()]
                .first_prompt
                .as_deref(),
            Some("the same id elsewhere")
        );
        assert_eq!(
            sessions[&fx.transcript.display().to_string()]
                .first_prompt
                .as_deref(),
            Some("index my transcripts")
        );

        // Deleting one must not evict the other's metadata.
        std::fs::remove_file(&other_project).unwrap();
        fx.index();
        let sessions = fx.sessions();
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key(&fx.transcript.display().to_string()));
    }

    /// A zero-byte transcript, or one holding only sidecar records, has nothing to list.
    #[test]
    fn a_transcript_with_no_documents_gets_no_session_row() {
        let fx = Fixture::with_body("");
        let stats = fx.index();
        assert_eq!(stats.docs_added, 0);
        assert_eq!(stats.sessions, 0, "no blank row");
        assert!(fx.sessions().is_empty());

        // A sidecar-only file does carry something worth listing — its title.
        let fx = Fixture::with_body(
            "{\"type\":\"summary\",\"summary\":\"Only a title\",\"leafUuid\":\"leaf-1\"}\n",
        );
        fx.index();
        let sessions = fx.sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[&fx.transcript.display().to_string()]
                .title
                .as_deref(),
            Some("Only a title")
        );
    }

    #[test]
    fn tail_intact_detects_a_rewritten_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let state = |offset: u64, start: u64, hash: u64| FileState {
            size: 8,
            mtime_ms: 1,
            byte_offset: offset,
            docs: 2,
            carry: ParseCarry {
                tail_line: Some(crate::parse::TailLine { start, hash }),
                ..ParseCarry::default()
            },
        };
        let two = crate::parse::fnv1a(b"two\n");
        assert!(tail_intact(&path, &state(8, 4, two)));
        assert!(!tail_intact(&path, &state(8, 4, two ^ 1)), "wrong hash");
        assert!(!tail_intact(&path, &state(4, 4, two)), "start past offset");
        assert!(
            !tail_intact(&tmp.path().join("gone.jsonl"), &state(8, 4, two)),
            "unreadable"
        );
        // A watermark written before fingerprints existed is trusted, not reindexed.
        assert!(tail_intact(
            &path,
            &FileState {
                size: 8,
                mtime_ms: 1,
                byte_offset: 8,
                docs: 2,
                carry: ParseCarry::default(),
            }
        ));
    }

    /// Every stored body currently in the index. `body` rather than `text`, which is one
    /// value per prose block and would report only the first of them.
    fn indexed_texts(index_dir: &Path) -> Vec<String> {
        use tantivy::schema::Value as _;
        let (index, fields) = open_or_create(index_dir).unwrap();
        let searcher = index.reader().unwrap().searcher();
        let found = searcher
            .search(
                &AllQuery,
                &tantivy::collector::TopDocs::with_limit(1_000).order_by_score(),
            )
            .unwrap();
        found
            .into_iter()
            .map(|(_, address)| {
                let doc: TantivyDocument = searcher.doc(address).unwrap();
                doc.get_first(fields.body)
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn plan_for_covers_every_watermark_transition() {
        let file = |size: u64, mtime_ms: i64| TranscriptFile {
            path: PathBuf::from("/p/sess.jsonl"),
            session_id: "sess".into(),
            agent_id: None,
            meta: None,
            size,
            mtime_ms,
        };
        let prev = FileState {
            size: 100,
            mtime_ms: 500,
            byte_offset: 90,
            docs: 7,
            carry: ParseCarry::default(),
        };

        assert_eq!(plan_for(&file(100, 500), Some(&prev), false), Plan::Skip);
        assert_eq!(
            plan_for(&file(150, 600), Some(&prev), false),
            Plan::Tail {
                from: 90,
                seq_base: 7
            }
        );
        assert_eq!(plan_for(&file(50, 600), Some(&prev), false), Plan::Reset);
        assert_eq!(plan_for(&file(150, 400), Some(&prev), false), Plan::Reset);
        assert_eq!(plan_for(&file(100, 500), Some(&prev), true), Plan::Reset);
        assert_eq!(plan_for(&file(100, 500), None, false), Plan::Reset);

        // Same size, newer mtime: rewritten in place, but nothing was lost — tail from the
        // watermark, which re-reads only the bytes we never consumed.
        assert_eq!(
            plan_for(&file(100, 700), Some(&prev), false),
            Plan::Tail {
                from: 90,
                seq_base: 7
            }
        );
    }

    #[test]
    fn merge_session_keeps_the_older_first_prompt_and_the_newer_title() {
        let mut dst = SessionInfo {
            session_id: "s".into(),
            title: None,
            first_prompt: Some("first".into()),
            first_ts_ms: Some(10),
            last_ts_ms: Some(20),
            messages: 2,
            tool_calls: 1,
            project: Some("/p".into()),
            ..SessionInfo::default()
        };
        merge_session(
            &mut dst,
            SessionInfo {
                session_id: "s".into(),
                title: Some("late title".into()),
                first_prompt: Some("second".into()),
                first_ts_ms: Some(30),
                last_ts_ms: Some(40),
                messages: 3,
                tool_calls: 2,
                project: None,
                ..SessionInfo::default()
            },
        );

        assert_eq!(dst.title.as_deref(), Some("late title"));
        assert_eq!(dst.first_prompt.as_deref(), Some("first"));
        assert_eq!(dst.first_ts_ms, Some(10));
        assert_eq!(dst.last_ts_ms, Some(40));
        assert_eq!(dst.messages, 5);
        assert_eq!(dst.tool_calls, 3);
        assert_eq!(dst.project.as_deref(), Some("/p"), "None must not erase");
    }

    /// The headline incrementality claim, measured against a real transcript rather than a
    /// fixture: replay it one line at a time, indexing after every append, and require the
    /// result to equal a single pass over the finished file. Ignored by default because it
    /// needs this machine's `~/.claude/projects`. Run with
    /// `cargo test --release -- --ignored --nocapture replaying_a_real_transcript`.
    #[test]
    #[ignore = "requires real transcripts on this machine"]
    fn replaying_a_real_transcript_line_by_line_matches_a_single_pass() {
        let Ok(root) = crate::discovery::default_root() else {
            return;
        };
        let Some(source) = discover(std::slice::from_ref(&root))
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.size > 0)
            .max_by_key(|f| f.size)
        else {
            return;
        };
        println!("replaying {}", source.path.display());

        let tmp = tempfile::tempdir().unwrap();
        let live_root = tmp.path().join("projects");
        let target = live_root.join("-p/sess-replay.jsonl");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let body = std::fs::read(&source.path).unwrap();
        let lines: Vec<&[u8]> = body.split_inclusive(|b| *b == b'\n').collect();

        let live_dir = tmp.path().join("live");
        let mut accumulated: Vec<u8> = Vec::new();
        for line in &lines {
            accumulated.extend_from_slice(line);
            std::fs::write(&target, &accumulated).unwrap();
            // Millisecond mtime resolution: without this a same-size-and-millisecond write is
            // indistinguishable from no change, which is the documented watermark rule.
            std::thread::sleep(std::time::Duration::from_millis(2));
            run(
                &live_dir,
                std::slice::from_ref(&live_root),
                &IndexOptions::default(),
            )
            .unwrap();
        }

        let once_dir = tmp.path().join("once");
        let once = run(
            &once_dir,
            std::slice::from_ref(&live_root),
            &IndexOptions::default(),
        )
        .unwrap();
        println!(
            "  {} lines: incremental live={} one-shot={}",
            lines.len(),
            live_docs(&live_dir),
            once.docs_added
        );

        let incremental = load_sessions(&live_dir).unwrap();
        let one_shot = load_sessions(&once_dir).unwrap();
        for (key, live) in &incremental {
            let whole = &one_shot[key];
            println!(
                "  {key}: msgs {} vs {}, tools {} vs {}",
                live.messages, whole.messages, live.tool_calls, whole.tool_calls
            );
            assert_eq!(live.messages, whole.messages, "message count for {key}");
            assert_eq!(live.tool_calls, whole.tool_calls, "tool count for {key}");
        }
        assert_eq!(
            live_docs(&live_dir),
            once.docs_added,
            "live indexing must converge on the one-shot result"
        );
        // Not just the same number of documents — the same documents.
        let by_id = |dir: &Path| -> BTreeMap<String, (String, String, String)> {
            indexed_bodies_by_id(dir)
        };
        assert_eq!(
            by_id(&live_dir),
            by_id(&once_dir),
            "live indexing must produce the same document bodies"
        );
    }

    /// Every document in an index, as `doc_id -> (body, tool_output, bash_cmd)`.
    ///
    /// Read back through [`crate::search::doc_from_stored`], the one reader of the stored
    /// payload, so the comparison covers the structured `bash_cmd` too: a tool call completed
    /// across an incremental boundary is rebuilt from a carried document, and losing its
    /// `bash_cmd` there would leave `--program` blind to exactly the live sessions people
    /// search most. `body` stands in for the split halves: it is the whole a renderer prints,
    /// and the split is a pure function of it.
    fn indexed_bodies_by_id(index_dir: &Path) -> BTreeMap<String, (String, String, String)> {
        let (index, fields) = open_or_create(index_dir).unwrap();
        let searcher = index.reader().unwrap().searcher();
        let limit = (searcher.num_docs() as usize).max(1);
        let found = searcher
            .search(
                &AllQuery,
                &tantivy::collector::TopDocs::with_limit(limit).order_by_score(),
            )
            .unwrap();
        found
            .into_iter()
            .map(|(_, address)| {
                let stored: TantivyDocument = searcher.doc(address).unwrap();
                let doc = crate::search::doc_from_stored(&fields, &stored);
                let bash_cmd = doc
                    .bash_cmd
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                (
                    doc.doc_id,
                    (doc.body, doc.tool_output.unwrap_or_default(), bash_cmd),
                )
            })
            .collect()
    }

    /// Not part of the gate — it needs this machine's `~/.claude/projects`. Run with
    /// `cargo test -- --ignored --nocapture real_transcripts_index_incrementally`.
    #[test]
    #[ignore = "requires real transcripts on this machine"]
    fn real_transcripts_index_incrementally() {
        let Ok(root) = crate::discovery::default_root() else {
            return;
        };
        if !root.is_dir() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let index_dir = tmp.path().join("index");
        let roots = [root.clone()];

        let first = run(&index_dir, &roots, &IndexOptions::default()).unwrap();
        println!(
            "root {}\n  scanned={} updated={} docs={} sessions={} errors={} in {}ms",
            root.display(),
            first.files_scanned,
            first.files_updated,
            first.docs_added,
            first.sessions,
            first.parse_errors,
            first.elapsed_ms
        );
        assert!(first.docs_added > 0, "no documents came out of real data");
        assert_eq!(live_docs(&index_dir), first.docs_added);

        let second = run(&index_dir, &roots, &IndexOptions::default()).unwrap();
        println!(
            "  re-run: updated={} added={} deleted={} in {}ms",
            second.files_updated, second.docs_added, second.docs_deleted, second.elapsed_ms
        );
        // A live transcript may legitimately have grown between the two runs; what must never
        // happen is a re-add of documents that are already there.
        assert!(second.docs_added <= first.docs_added / 4);
        assert_eq!(second.files_reset, 0, "nothing should need a full reparse");

        let full = run(
            &index_dir,
            &roots,
            &IndexOptions {
                full: true,
                ..IndexOptions::default()
            },
        )
        .unwrap();
        println!(
            "  --full: reset={} added={}",
            full.files_reset, full.docs_added
        );
        assert_eq!(
            live_docs(&index_dir),
            full.docs_added,
            "--full must replace, not duplicate"
        );

        for (key, info) in load_sessions(&index_dir).unwrap().iter().take(6) {
            println!(
                "  {key}: msgs={} tools={} title={:?} project={:?}",
                info.messages, info.tool_calls, info.title, info.project
            );
        }
    }
}
