//! `search_sessions` — the session listing.
//!
//! # What to build on
//!
//! * [`crate::index::load_sessions`] via [`crate::mcp::State::sessions`]. This tool does **not**
//!   touch the tantivy index: a session listing comes from `sessions.json`, one row per
//!   transcript file, and that is the entire reason ten of the eighteen filters cannot be
//!   answered here.
//! * [`crate::sessions::SessionMatcher::new`] and `matches`. One matcher, shared with the CLI and
//!   the HTTP API — do not write a third. Its `new` returns a
//!   [`crate::sessions::FilterError`] for an unreadable date; return it unchanged and
//!   [`crate::mcp::from_anyhow`] turns it into `invalid_params` naming the field, never a flag.
//! * [`crate::sessions::unanswerable_filter_notes`] for the warnings. Every one of those
//!   sentences must reach the response: a listing filtered by nine of ten filters looks exactly
//!   like a listing filtered by ten, and a model reads "no session used that model" out of a
//!   result that means "that question cannot be asked here". `tracing` is not a channel here.
//!
//!   One thing to fix while you are in there: those sentences currently end with "Use /api/search
//!   for it", which is an HTTP route this caller cannot reach. `sessions.rs` anticipates it —
//!   *"a front end whose search operation is not an HTTP route wants the same sentence with its
//!   own name for it; parameterise the pointer then, rather than writing a second list of filters
//!   to go with a second sentence."* Parameterise it and point at `search_turns`; do not fork the
//!   list.
//!
//! # Ordering and counts
//!
//! Most recent `last_ts_ms` first, ties broken by session id and then agent id, exactly as
//! `cli.rs` orders it — a listing whose order changes between front ends is a listing nobody can
//! page. `total` is how many matched *before* `limit` cut the list; `returned` is how many are
//! here. Truncating without reporting the first number is how "I only worked on three things
//! last week" gets said out loud.

use crate::mcp::State;
use crate::mcp::types::{SearchSessionsRequest, SearchSessionsResponse};

/// Filter, sort and page the session list.
pub fn run(_state: &State, _req: SearchSessionsRequest) -> anyhow::Result<SearchSessionsResponse> {
    anyhow::bail!("unimplemented")
}
