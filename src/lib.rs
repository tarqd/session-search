//! Full-text search over Claude Code session transcripts.
//!
//! Module boundaries are pinned in `docs/DESIGN.md`; the input format is described in
//! `docs/TRANSCRIPT-FORMAT.md`.

#[cfg(feature = "http-api")]
pub mod api;
pub mod cli;
pub mod context;
pub mod discovery;
pub mod format;
pub mod index;
pub mod model;
pub mod parse;
pub mod schema;
pub mod search;

pub use discovery::{AgentMeta, TranscriptFile, default_root, discover};
pub use index::{IndexOptions, IndexStats};
pub use parse::{
    Doc, DocKind, FileContext, ParseCarry, ParseError, ParseOptions, ParseOutput, SessionInfo,
    parse_file, parse_whole,
};
pub use schema::{Fields, build_schema, doc_to_json};
pub use search::{FacetCount, Filters, Hit, SearchRequest, SearchResponse, SnippetSource, SortBy};
