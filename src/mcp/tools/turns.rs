//! `search_turns` — full-text search returned as turn skeletons.
//!
//! # What to build on
//!
//! * [`crate::search::search`] with a [`crate::search::SearchRequest`] whose `group_by_turn` is
//!   **`true`**. That is not a knob here: this tool's unit of answer is a turn, `limit`/`offset`
//!   count turns under grouping, and [`crate::search::Hit::collapsed`] is where the "and N more
//!   documents of this turn matched" number comes from. Leave `facets` empty — `aggregate` is
//!   the tool for that — and leave `similar_to` `None`.
//! * [`crate::context::turn_window`] for each hit's documents, then
//!   [`crate::format::turn_skeleton`] with [`crate::format::SKELETON_BUDGET`] to render the
//!   skeleton. Do not re-derive the skeleton: `format.rs` holds the only definition of that
//!   envelope and of the `dropped` count beside it.
//! * [`crate::format::doc_json`] is *not* wanted here. A skeleton replaces the documents; a hit
//!   carrying both undoes the only thing this tool is for.
//!
//! # The two caps, both reported
//!
//! A turn-shaped answer has two independent caps and a response that reports one of them is
//! worse than one that reports neither, because it reads as complete. The *document* cap is
//! `TurnWindow::total` versus `docs.len()`; the *byte* cap is `Skeleton::dropped`. Both land in
//! [`TurnSkeleton`] and [`TurnHit`].
//!
//! # Cost
//!
//! One `turn_window` per hit is one extra searcher pass per returned turn. That is the price of
//! a skeleton and it is the right trade — a turn's full context averages 5,651 bytes against a
//! skeleton's 555, and this tool exists to not send the 5,651. Fetch the window once per hit and
//! reuse it; do not call `context::turn` and `context::turn_window` separately for the same turn.

use crate::mcp::State;
use crate::mcp::types::{SearchTurnsRequest, SearchTurnsResponse};

/// Search, group by turn, and return one skeleton per turn.
///
/// Errors are for requests that could not be answered at all. "Nothing matched" is not one: it
/// is a successful response whose envelope carries the zero, the applied filters and the retry.
pub fn run(_state: &State, _req: SearchTurnsRequest) -> anyhow::Result<SearchTurnsResponse> {
    anyhow::bail!("unimplemented")
}
