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
//! 2. **refuse half a turn address**, with [`check_turn_address`], before the index is touched
//!    for the same reason. `turn_of` and `turn_seq` are one filter written in two fields and
//!    the index side reads them as a pair, so half of one narrows nothing at all;
//! 3. **build the envelope with [`crate::mcp::envelope::build`]**, always, and never write a
//!    zero-hit explanation by hand. There is one ranking and it lives in one place;
//! 4. **carry every warning through.** `SearchResponse::warnings` for the index-backed tools,
//!    `sessions::unanswerable_filter_notes` for `search_sessions`. A warning that only reaches
//!    `tracing` reaches nobody here: there is no stderr the caller is reading.

pub mod aggregate;
pub mod drill;
pub mod sessions;
pub mod turns;

use crate::mcp::caller_error;
use crate::search::Filters;

/// The turn address, refused unless it arrived whole.
///
/// `turn_of` and `turn_seq` are one filter written in two fields, and the other two front ends
/// each say so before the index is touched: `clap` pairs them with `requires`, and
/// `api::dto::SearchBody::prepare` rejects half of one. Nothing said it here.
/// `search::build_query` reads the pair with a single `if let (Some(path), Some(seq))`, so half
/// an address arriving over MCP was dropped in silence and the search ran unnarrowed — an answer
/// *wider* than the one asked for, which is worse than a zero because it looks like a result.
/// Now that [`crate::mcp::envelope::applied_filters`] echoes the pair, it would also have been
/// reported back as a filter that was applied.
///
/// Refused rather than warned, and for the reason `dto` gives: unlike a contradiction, there is
/// no reading of half an address that the caller could have meant.
pub fn check_turn_address(f: &Filters) -> anyhow::Result<()> {
    // Trimmed, not merely present: a client that fills every optional field with `""` sends a
    // `turn_of` the index side reads as absent (`search::non_empty`), so counting it as half an
    // address here would accept exactly the request this refuses.
    let path = f
        .turn_of
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if path.is_some() == f.turn_seq.is_some() {
        return Ok(());
    }
    Err(caller_error(
        "turn_of and turn_seq go together: turn_seq is a per-file ordinal, so without the path \
         it names that turn in every transcript. Send both, exactly as a search_turns hit \
         returned them — or call get_turn with the same pair to read the turn instead of \
         searching inside it",
    ))
}
