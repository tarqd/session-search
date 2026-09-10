//! Claude Code: `~/.claude/projects/**/*.jsonl`, per `docs/TRANSCRIPT-FORMAT.md`.
//!
//! `model.rs` is the raw serde view of one record, `discovery.rs` walks the project tree and
//! `parse.rs` turns records into [`crate::doc::Doc`]s. This file is only the adapter glue.

pub mod discovery;
pub mod model;
pub mod parse;

use std::path::{Path, PathBuf};

use crate::agent::{Agent, SessionFile};
use crate::doc::{FileContext, ParseOptions, ParseOutput};

/// Registry id. Stored in every doc, matched by `--agent`.
pub const ID: &str = "claude-code";

pub struct ClaudeCode;

impl Agent for ClaudeCode {
    fn id(&self) -> &'static str {
        ID
    }

    fn default_roots(&self) -> anyhow::Result<Vec<PathBuf>> {
        Ok(vec![discovery::default_root()?])
    }

    fn discover(&self, roots: &[PathBuf]) -> anyhow::Result<Vec<SessionFile>> {
        discovery::discover(roots)
    }

    fn parse(
        &self,
        path: &Path,
        from_offset: u64,
        seq_base: u64,
        opts: &ParseOptions,
        ctx: &FileContext,
    ) -> anyhow::Result<(ParseOutput, u64)> {
        parse::parse_file(path, from_offset, seq_base, opts, ctx)
    }
}
