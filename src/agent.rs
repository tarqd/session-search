//! The seam between "an agent's on-disk session format" and everything else.
//!
//! Every module past this one — the schema, the indexer, search, context, rendering — speaks
//! only [`crate::doc::Doc`] and [`crate::doc::SessionInfo`]. What an agent contributes is an
//! implementation of [`Agent`]: where its transcripts live, how to enumerate them, and how to
//! turn one file (or the bytes appended to it since last time) into documents.
//!
//! Adapters are registered in [`all`]; a transcript root is always paired with the adapter
//! that reads it ([`Root`]). Adding a new agent means a new module under `src/agents/`, one
//! entry in [`all`], and nothing else.
//!
//! Vocabulary trap: `agent` (this module, `Doc::agent`, `--agent`) is *which program wrote
//! the transcript*: `claude-code`, `codex`, `pi`. `agent_id` / `agent_type` on a doc or a
//! session are Claude Code's names for a **subagent** within a session and predate this
//! seam; they keep their meaning.

use std::path::{Path, PathBuf};

use crate::doc::{FileContext, ParseOptions, ParseOutput};

/// One transcript file an adapter found, plus what the path alone says about it. The
/// indexer keys watermarks by `path` and hands the rest to the parser as [`FileContext`].
#[derive(Debug, Clone)]
pub struct SessionFile {
    /// [`Agent::id`] of the adapter that found this file and will parse it.
    pub agent: &'static str,
    pub path: PathBuf,
    /// From the filename / parent dir. Records may disagree; records win downstream.
    pub session_id: String,
    /// `Some(..)` for a subagent transcript.
    pub agent_id: Option<String>,
    /// Subagent type, when a sidecar beside the file names one (Claude Code's
    /// `agent-<id>.meta.json`).
    pub agent_type: Option<String>,
    /// Subagent description, from the same sidecar.
    pub description: Option<String>,
    pub size: u64,
    pub mtime_ms: i64,
}

impl SessionFile {
    /// `"<session_id>"` or `"<session_id>:<agent_id>"`.
    pub fn key(&self) -> String {
        match &self.agent_id {
            Some(a) => format!("{}:{}", self.session_id, a),
            None => self.session_id.clone(),
        }
    }
}

/// A transcript root and the adapter that reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub agent: &'static str,
    pub path: PathBuf,
}

impl Root {
    /// Parse a `--root` argument: `[AGENT=]DIR`. A bare `DIR` is a Claude Code root, which
    /// is what `--root` always meant before there was more than one agent.
    pub fn parse(spec: &str) -> anyhow::Result<Root> {
        let (agent, path) = match spec.split_once('=') {
            // `C:\...` on Windows also contains `=`-free drive letters, but a `=` is never
            // part of a sane transcript path, so the split is unambiguous.
            Some((agent, path)) if !agent.is_empty() && !agent.contains('/') => {
                let adapter = by_id(agent.trim()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown agent {:?} in --root {spec:?}; known: {}",
                        agent.trim(),
                        ids().join(", ")
                    )
                })?;
                (adapter.id(), PathBuf::from(path))
            }
            _ => (DEFAULT_AGENT, PathBuf::from(spec)),
        };
        if path.as_os_str().is_empty() {
            anyhow::bail!("empty path in --root {spec:?}");
        }
        Ok(Root { agent, path })
    }
}

/// One agent's session format.
///
/// Parsing is byte-offset based on purpose: every format supported so far is one append-only
/// JSONL file per session, and the incremental indexer's whole model (`state.json`
/// watermarks, tail fingerprints) is built on that. A source that is not a growing file — a
/// SQLite store, a compressed archive — should return `from_offset == 0` parses only and let
/// the indexer treat every change as a reset; generalising the watermark to an opaque cursor
/// is the documented next step (`docs/MULTI-AGENT.md`), not something to fake here.
pub trait Agent: Send + Sync {
    /// Stable registry id; the value stored in `Doc::agent` and matched by `--agent`.
    /// Lower-case, `[a-z0-9-]`, never renamed once released.
    fn id(&self) -> &'static str;

    /// Where this agent keeps transcripts when the user passes no `--root`. Roots that do not
    /// exist are fine — discovery skips them — but an *unknowable* root (no `$HOME`) is an
    /// error, so the CLI can say so instead of silently indexing nothing.
    fn default_roots(&self) -> anyhow::Result<Vec<PathBuf>>;

    /// Enumerate every transcript under `roots`. Must set [`SessionFile::agent`] to
    /// [`Agent::id`].
    fn discover(&self, roots: &[PathBuf]) -> anyhow::Result<Vec<SessionFile>>;

    /// Parse `path` from `from_offset`, numbering docs from `seq_base`, and return the
    /// output plus the offset just past the last *complete* record consumed. Every doc must
    /// carry `agent == self.id()` and a dense per-file `seq`; see `docs/DESIGN.md` for the
    /// rest of the contract, which is unchanged from the single-agent days.
    fn parse(
        &self,
        path: &Path,
        from_offset: u64,
        seq_base: u64,
        opts: &ParseOptions,
        ctx: &FileContext,
    ) -> anyhow::Result<(ParseOutput, u64)>;

    /// One-shot parse of a whole file with no carried state.
    fn parse_whole(&self, path: &Path, opts: &ParseOptions) -> anyhow::Result<ParseOutput> {
        Ok(self.parse(path, 0, 0, opts, &FileContext::default())?.0)
    }
}

/// The adapter a bare `--root DIR` means, and the one the tests assume.
pub const DEFAULT_AGENT: &str = crate::agents::claude::ID;

static REGISTRY: [&dyn Agent; 1] = [&crate::agents::claude::ClaudeCode];

/// Every registered adapter, in a stable order.
pub fn all() -> &'static [&'static dyn Agent] {
    &REGISTRY
}

pub fn by_id(id: &str) -> Option<&'static dyn Agent> {
    all().iter().copied().find(|a| a.id() == id)
}

pub fn ids() -> Vec<&'static str> {
    all().iter().map(|a| a.id()).collect()
}

/// Every adapter's default roots, each paired with its adapter.
pub fn default_roots() -> anyhow::Result<Vec<Root>> {
    let mut roots = Vec::new();
    for agent in all() {
        for path in agent.default_roots()? {
            roots.push(Root {
                agent: agent.id(),
                path,
            });
        }
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_root_is_a_claude_code_root() {
        let root = Root::parse("/a/projects").unwrap();
        assert_eq!(root.agent, "claude-code");
        assert_eq!(root.path, PathBuf::from("/a/projects"));
    }

    #[test]
    fn a_prefixed_root_names_its_agent() {
        let root = Root::parse("claude-code=/a/projects").unwrap();
        assert_eq!(root.agent, "claude-code");
        assert_eq!(root.path, PathBuf::from("/a/projects"));
    }

    #[test]
    fn an_unknown_agent_is_an_error_that_lists_the_known_ones() {
        let err = Root::parse("cursor=/a").unwrap_err().to_string();
        assert!(err.contains("unknown agent \"cursor\""), "{err}");
        assert!(err.contains("claude-code"), "{err}");
        assert!(Root::parse("").is_err());
        assert!(Root::parse("claude-code=").is_err());
    }

    #[test]
    fn a_path_containing_an_equals_sign_is_still_a_path() {
        // A directory component with `=` in it must not be mistaken for an agent prefix.
        let root = Root::parse("/tmp/a=b/projects").unwrap();
        assert_eq!(root.path, PathBuf::from("/tmp/a=b/projects"));
    }

    #[test]
    fn the_registry_is_consistent() {
        assert_eq!(ids(), vec!["claude-code"]);
        assert_eq!(by_id("claude-code").unwrap().id(), "claude-code");
        assert!(by_id("nope").is_none());
        assert_eq!(DEFAULT_AGENT, "claude-code");
    }
}
