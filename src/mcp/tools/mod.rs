//! One module per tool body, one owner each.
//!
//! The `#[tool]` methods in [`crate::mcp`] hold the schemas and the prose; these hold the work.
//! The split is deliberate and it is a file-ownership boundary as much as a code one: four
//! people can fill these in at once without touching the router, the descriptions, or each
//! other's files.
//!
//! What every `run` may assume, because [`crate::mcp::Server::blocking`] has already done it:
//!
//! * it is running off the async runtime, on a blocking thread, so tantivy work is fine here;
//! * the index has been refreshed if a refresh was due;
//! * `&State` is shared with concurrent calls — read it, never mutate it.
//!
//! What every `run` owes its caller, in the order the work has to happen:
//!
//! 1. **resolve the time window first**, with [`crate::mcp::envelope::resolve_time_range`], and
//!    return its `FilterError` unchanged — [`crate::mcp::from_anyhow`] turns it into
//!    `invalid_params` naming the field. Doing this before the index is touched is what makes a
//!    bad date a clear caller error instead of an `anyhow` chain from inside `search()`, and it
//!    is the only way the resolved range can be echoed at all;
//! 2. **build the envelope with [`crate::mcp::envelope::build`]**, always, and never write a
//!    zero-hit explanation by hand. There is one ranking and it lives in one place;
//! 3. **carry every warning through.** `SearchResponse::warnings` for the index-backed tools,
//!    `sessions::unanswerable_filter_notes` for `search_sessions`. A warning that only reaches
//!    `tracing` reaches nobody here: there is no stderr the caller is reading.

pub mod aggregate;
pub mod drill;
pub mod sessions;
pub mod turns;
