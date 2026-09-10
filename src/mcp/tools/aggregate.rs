//! `aggregate` — terms aggregation over one field.
//!
//! # What to build on
//!
//! * [`crate::search::facets`], which takes the field name and a [`crate::search::SearchRequest`]
//!   carrying the query and filters. It validates the field up front, so an unknown name or a
//!   JSON subpath under a non-JSON field is already a clear error rather than an empty result —
//!   pass that error through as-is. Set `facet_top` from `top`, and set `limit` to `top.max(1)`
//!   the way `cli.rs` does: the collector needs a non-zero limit even though no hits are read.
//! * [`crate::search::FacetResult`] travels to the caller **whole**. Issue #28 is explicit:
//!   *"Must return `matching_docs`, `docs_with_value`, `other_docs` and `distinct`, not a bare
//!   bucket list: a model handed only buckets will sum them and report a wrong total with total
//!   confidence."* Do not flatten it, do not drop `distinct` because it is approximate, and do
//!   not compute a total from the buckets anywhere.
//! * [`crate::search::FacetResult::is_search_shaped`] and
//!   [`crate::search::FacetResult::hidden_values`] fill the two derived fields. They are already
//!   written and tuned; call them rather than re-deriving the thresholds.
//!
//! # The zero here is a different zero
//!
//! `matching_docs == 0` means the query and filters matched nothing — the ordinary zero, and the
//! envelope handles it. `matching_docs > 0` with no buckets means something else entirely: the
//! documents exist and none of them carries a value for this field, which is a fact about the
//! field, not about the filters. Feed the envelope `facet.matching_docs`, not
//! `facet.values.len()`, or a `tool_input.file_path` aggregation over a thousand Bash calls will
//! come back suggesting the caller drop a filter that was working perfectly.

use crate::mcp::State;
use crate::mcp::types::{AggregateRequest, AggregateResponse};

/// Count one field's values across the matching set.
pub fn run(_state: &State, _req: AggregateRequest) -> anyhow::Result<AggregateResponse> {
    anyhow::bail!("unimplemented")
}
