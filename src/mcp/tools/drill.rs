//! `get_turn` and `get_output` — the two halves of the drill-down.
//!
//! They share a file because they share the hard part: resolving what the caller is pointing at.
//! Both accept a reference the model is holding rather than one it constructed, and both have to
//! tell "you pointed at nothing" apart from "what you pointed at is empty".
//!
//! # `get_turn`
//!
//! * The address is the pair `(source_path, turn_seq)`, or a single `doc_id`. Exactly one of the
//!   two forms must arrive: a bare `turn_seq` is not an address — it is an ordinal within one
//!   file, and two transcripts can carry the same session id — so reject it with
//!   [`crate::mcp::invalid_params`] rather than guessing a file.
//! * `doc_id` resolves through [`crate::search::resolve_doc`], which takes a `doc_id`, a record
//!   uuid, a `tool_use_id`, or `SESSION:SEQ` / `SESSION:AGENT:SEQ`, each by unambiguous prefix,
//!   and reports ambiguity as an error naming the candidates. Take `source_path` and `turn_seq`
//!   off the document it returns.
//! * The documents come from [`crate::context::turn_window`], which returns the head of the turn
//!   plus the turn's true size. `before`/`after` mean *neighbouring turns in the same file*, not
//!   neighbouring documents: walk `turn_seq` outwards and call `turn_window` per turn. Turns are
//!   not densely numbered — `turn_seq` is the `seq` of the record that opened the turn — so
//!   "the previous turn" is a lookup, not `turn_seq - 1`. [`crate::context::around`] over `seq`
//!   is the cheapest way to find the neighbouring turn numbers.
//! * Render each document with [`crate::format::doc_json`], which is the pinned document shape
//!   and already withholds `raw`. Then cut `tool_output` to `max_doc_bytes` and mark it, and
//!   count how many documents you cut into `docs_with_truncated_output`. That cut is this tool's
//!   own: `get_turn` truncates, `get_output` slices, and the difference is the whole reason both
//!   exist.
//!
//! # `get_output`
//!
//! * The address is `doc_id` or `tool_use_id`; both go through [`crate::search::resolve_doc`],
//!   which already searches the `tool_use_id` field. Exactly one must arrive.
//! * The slicing is [`crate::slice::slice`], with a [`crate::slice::SliceRequest`] built from the
//!   request's `head`/`tail`/`grep`/`context`/`max_bytes`. Do not re-implement any of it: that
//!   module owns the gap markers, the byte cap and the report, and a second implementation is a
//!   second set of numbers to disagree about. Map [`crate::slice::SliceReport`] onto the response
//!   field for field.
//! * **Three states, not two.** `Doc::tool_output` being `None` means no result ever reached the
//!   index — interrupted, denied, never answered — and `Some("")` means the call ran and printed
//!   nothing. They are different facts and
//!   [`crate::mcp::types::OutputState`] is where they are told apart. An empty string for both is
//!   the failure this field exists to prevent.
//! * A document that is not a tool call at all is a caller mistake, not an empty output: say so
//!   with [`crate::mcp::invalid_params`], naming what the reference actually resolved to.

use crate::mcp::State;
use crate::mcp::types::{GetOutputRequest, GetOutputResponse, GetTurnRequest, GetTurnResponse};

/// One turn's documents in full, plus any neighbouring turns asked for.
pub fn run_turn(_state: &State, _req: GetTurnRequest) -> anyhow::Result<GetTurnResponse> {
    anyhow::bail!("unimplemented")
}

/// One tool call's output, sliced.
pub fn run_output(_state: &State, _req: GetOutputRequest) -> anyhow::Result<GetOutputResponse> {
    anyhow::bail!("unimplemented")
}
