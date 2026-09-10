//! Full-text search over coding-agent session transcripts.
//!
//! Module boundaries are pinned in `docs/DESIGN.md`. Each supported agent's on-disk format
//! lives behind [`agent::Agent`] under `agents/`; the Claude Code format is described in
//! `docs/TRANSCRIPT-FORMAT.md`, and the plan for the others in `docs/MULTI-AGENT.md`.

pub mod agent;
pub mod agents;
pub mod bash;
pub mod cli;
pub mod context;
pub mod doc;
pub mod format;
pub mod index;
pub mod schema;
pub mod search;

pub use agent::{Agent, Root, SessionFile};
pub use bash::{BashCmd, extract};
pub use doc::{
    Doc, DocKind, FileContext, ParseCarry, ParseError, ParseOptions, ParseOutput, SessionInfo,
};
pub use index::{IndexOptions, IndexStats};
pub use schema::{Fields, build_schema, doc_to_json};
pub use search::{FacetCount, Filters, Hit, SearchRequest, SearchResponse};
