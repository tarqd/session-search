//! Slicing a captured tool output down to something worth reading.
//!
//! A tool output is unbounded in practice: a build log is routinely 200 KB and a spilled result
//! is larger. The caller on the other end of `get_output` is a language model that pays for every
//! byte and is usually looking for about six specific lines. So this module answers one question —
//! *which lines, and how do we say what we left out* — and answers it with pure functions over
//! `&str`: no I/O, no index, no MCP types, so the CLI can reuse it verbatim.
//!
//! The rule that governs every decision below, from issue #28: **a partial answer must never read
//! as a complete one.** Two mechanisms enforce it, and both are mandatory:
//!
//! * every gap in the returned text carries a [`gap_marker`] saying how many lines are missing.
//!   Silently joining two ends of a log fabricates adjacency the output never had — a reader who
//!   sees `Compiling foo` immediately above `error: aborting` will conclude the two are related.
//!   Markers sit **between** kept lines only, never at the edges: omitting the start or end of a
//!   log invents nothing, whereas a marker above the first line of every `tail` would be noise
//!   that teaches a reader to skip the markers that do carry meaning;
//! * [`Slice::report`] travels with the text and states the original's size, so a caller can tell
//!   "the log has no errors" from "the 40 lines I was shown have no errors". A renderer that drops
//!   the report re-creates exactly the failure this module exists to prevent.
//!
//! Order of operations, which is also the order of the fields in [`SliceRequest`]:
//!
//! 1. **`grep`** — a real [`regex::Regex`], see [`SliceRequest::grep`] — selects candidate lines,
//!    each widened by [`SliceRequest::context`] lines either side, overlapping windows merged;
//! 2. **`head`/`tail`** cut that candidate list down — *the candidates*, not the original lines,
//!    so `grep` + `head: 20` means "the first 20 matching lines", which is what a caller asking
//!    both actually wants;
//! 3. **`max_bytes`** is a hard cap applied last, once the lines are chosen. It has to be last:
//!    a cap applied first would spend the budget on the first 16 KB of a log and then run `grep`
//!    over the part that happened to fit, reporting no matches for a file full of them.
//!
//! ## Where the fallible step lives
//!
//! Compiling a pattern is the module's only failure mode, and it is deliberately *not* inside the
//! slicing call. [`SlicePlan::new`] compiles a [`SliceRequest`] and can fail with
//! [`InvalidPattern`]; [`SlicePlan::slice`] is then total and returns a [`Slice`], never a
//! `Result`. Two things follow, and both are the point:
//!
//! * a `get_output` tool layer validates parameters and answers `invalid_params` — with the regex
//!   syntax error inside it, since a model that sent `[unclosed` can only fix what it is shown —
//!   **before it reads a 200 KB output out of the index**. A `slice(text, req) -> Result` would
//!   invert that, doing the expensive fetch first and failing on the pattern afterwards;
//! * `get_turn` slices *several* outputs against one request. A plan compiles the pattern once and
//!   is reused across all of them, instead of recompiling per output and discovering a malformed
//!   pattern on the third one after two have already been rendered.
//!
//! [`SliceRequest`] itself stays plain, `String`-typed data so it remains exactly what `clap`
//! derives into and what `serde` deserializes from a tool call; a compiled `Regex` in that struct
//! could be neither.

use std::borrow::Cow;

use regex::Regex;

/// Default byte budget when a caller does not name one, in bytes of returned text.
///
/// Roughly 4,000 tokens: about 2% of a 200K context window, and enough for ~200 lines of a build
/// log. It is deliberately an order of magnitude above `format.rs`'s 1,600-byte skeleton budget,
/// because reaching `get_output` at all means the caller already read a skeleton and chose *this*
/// output — and an order of magnitude below the 200 KB logs that motivate the module, so a model
/// can pull three or four outputs in one turn without spending its window.
///
/// Note that [`SliceRequest::max_bytes`] of `None` means this number, not "unbounded". There is
/// no way to ask for unbounded output, on purpose: an unbounded default is the exact failure this
/// module exists to prevent, and a caller that genuinely wants everything can pass `usize::MAX`
/// and say so.
pub const DEFAULT_MAX_BYTES: usize = 16_000;

/// Appended (or prepended — see [`SliceRequest::max_bytes`]) when the byte cap bit.
///
/// It carries no counts on purpose. The notice is emitted *inside* the budget, so its length has
/// to be known before we know how many lines fit; a notice whose text depended on the answer
/// would be circular. The counts live in [`SliceReport`], which cannot be truncated.
pub const TRUNCATION_NOTICE: &str = "[... truncated: byte budget reached ...]";

/// How a gap in the returned text is spelled.
///
/// Brackets and the count, both load-bearing: the brackets keep it from reading as a line of the
/// log, and the count is what lets a reader judge whether the gap could plausibly contain the
/// answer. `[... 3 lines omitted ...]` invites a follow-up call; `...` does not.
#[must_use]
pub fn gap_marker(lines: usize) -> String {
    format!(
        "[... {lines} line{} omitted ...]",
        if lines == 1 { "" } else { "s" }
    )
}

/// A [`SliceRequest::grep`] that is not a valid regular expression.
///
/// Carries the pattern *and* the underlying [`regex::Error`], because the two answer different
/// halves of the caller's question and a tool layer has to pass both through into its
/// `invalid_params` response: the pattern says which argument was wrong, and the syntax error —
/// whose `Display` points at the offending character — is the only part a model can act on. A bare
/// "invalid pattern" leaves it to guess, and a pattern silently matching nothing would be worse
/// still: it reads as "the log contains no errors".
#[derive(Debug, thiserror::Error)]
#[error("invalid `grep` pattern `{pattern}`: {source}")]
pub struct InvalidPattern {
    pub pattern: String,
    #[source]
    pub source: regex::Error,
}

/// What to keep. All fields are optional or zero-valued, so `SliceRequest::default()` is the
/// identity slice — everything, capped at [`DEFAULT_MAX_BYTES`].
///
/// Plain data with public fields, matching `search.rs`'s `Filters`: the same struct is meant to be
/// what `clap` derives into and what `serde` deserializes from an MCP tool call, so it must not
/// grow constructors — or field types, such as a compiled [`Regex`] — that either of those cannot
/// express. Compile it with [`SlicePlan::new`] before slicing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SliceRequest {
    /// Keep the first N lines. Applied to the `grep` candidates when both are given.
    pub head: Option<usize>,
    /// Keep the last N lines. Applied to the `grep` candidates when both are given.
    pub tail: Option<usize>,
    /// Keep only lines matching this **regular expression**, in the `regex` crate's syntax —
    /// character classes, repetition, alternation, groups and inline flags such as `(?i)` all
    /// work. An invalid pattern is [`InvalidPattern`] from [`SlicePlan::new`], never a silent
    /// zero-match.
    ///
    /// The pattern is applied **one line at a time**, which has two consequences worth stating in
    /// any tool description built on this: `^` and `$` anchor to the start and end of a *line*
    /// without needing the `(?m)` flag, and a pattern cannot match across a line break.
    ///
    /// Unanchored and case-sensitive, as `grep(1)` is: a log's `ERROR` and a path's `error` are
    /// different things, and a caller that wants both can say `(?i)error`.
    pub grep: Option<String>,
    /// Lines either side of each `grep` match. Zero by default, as in `grep(1)`; overlapping
    /// windows merge rather than repeating the lines they share.
    ///
    /// Ignored without `grep`, because "context" around nothing has no meaning.
    pub context: usize,
    /// Hard cap on the bytes of [`Slice::text`], markers and notice included. `None` means
    /// [`DEFAULT_MAX_BYTES`].
    ///
    /// The cap is on the *returned string*, not on the content within it. A cap that excluded its
    /// own markers would hand a caller with a 16 KB budget 16 KB plus change, which makes the
    /// budget useless for the one thing budgets are for — bounding the next request.
    ///
    /// The cut keeps the **beginning** of the selection, except for a tail-only request (`tail`
    /// set, `head` unset), where it keeps the **end**: a caller that asked for the last 100 lines
    /// and got the first 20 of them got the opposite of what it asked for. The notice moves to the
    /// front in that case, so the marker sits where the missing bytes are.
    pub max_bytes: Option<usize>,
}

/// A validated [`SliceRequest`] with its pattern compiled — the thing that actually slices.
///
/// Built once and reused for every output a request applies to; see the module docs for why the
/// compile step lives here rather than inside [`SlicePlan::slice`].
#[derive(Debug, Clone)]
pub struct SlicePlan {
    head: Option<usize>,
    tail: Option<usize>,
    grep: Option<Regex>,
    context: usize,
    max_bytes: usize,
}

/// A slice and the truth about what it left out. Both halves are the answer; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    /// The selected lines, joined with `\n`, with [`gap_marker`] lines where the original had
    /// more. There is no trailing newline, whether or not the original had one — the original's
    /// exact size is in [`SliceReport::total_bytes`], so nothing is lost by normalising here.
    pub text: String,
    pub report: SliceReport,
}

/// What was there, what came back, and what was dropped on the way.
///
/// Every count except [`Self::returned_lines`] describes the **original** output, so that a caller
/// can size a follow-up request without fetching anything again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceReport {
    /// Bytes of the original output, before any slicing.
    pub total_bytes: usize,
    /// Lines of the original output, before any slicing. A trailing newline does not add an empty
    /// final line, so `"a\n"` is one line — a log that ends properly is not one line longer than
    /// the same log that does not.
    pub total_lines: usize,
    /// How many original lines matched `grep`, or `None` when no `grep` was given. `Some(0)` and
    /// `None` are different answers and must render differently: the first says the pattern found
    /// nothing, the second says nothing was asked.
    pub matched_lines: Option<usize>,
    /// Lines of the original present in [`Slice::text`]. Marker lines are not counted — they are
    /// this module's words, not the output's.
    pub returned_lines: usize,
    /// `total_lines - returned_lines`.
    pub dropped_lines: usize,
    /// How many [`gap_marker`] lines were emitted.
    pub gaps: usize,
    /// Whether the byte cap bit, i.e. whether lines chosen by `head`/`tail`/`grep` were then
    /// dropped (or one was shortened) to fit [`SliceRequest::max_bytes`].
    pub truncated: bool,
    /// Whether the cap forced a cut inside a line rather than between two. Only happens when not
    /// even one selected line fits the budget. Worth its own flag rather than folding into
    /// [`Self::truncated`]: a caller can safely parse whole lines of JSON or a whole file path
    /// out of a slice that is merely short, and cannot out of one that ends mid-token.
    pub cut_mid_line: bool,
}

impl SliceReport {
    /// Whether [`Slice::text`] is the entire original output.
    ///
    /// The one question a caller must be able to ask before summarising a slice as if it were the
    /// whole log.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.dropped_lines == 0 && !self.truncated && !self.cut_mid_line
    }

    /// A one-line rendering for a reader who will not be shown the struct.
    ///
    /// Deliberately says `complete` out loud in the good case rather than staying silent: a caller
    /// that only ever sees text when something was dropped learns to treat the absence of a notice
    /// as noise, and then misses it on the call that mattered.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut s = format!(
            "showing {} of {} lines ({} bytes of original output)",
            self.returned_lines, self.total_lines, self.total_bytes
        );
        if let Some(matched) = self.matched_lines {
            s.push_str(&format!("; {matched} matched the pattern"));
        }
        if self.dropped_lines > 0 {
            s.push_str(&format!("; {} lines omitted", self.dropped_lines));
        }
        if self.truncated {
            s.push_str("; TRUNCATED at the byte budget");
        }
        if self.cut_mid_line {
            s.push_str(" (cut mid-line)");
        }
        if self.is_complete() {
            s.push_str("; this is the complete output");
        }
        s
    }
}

/// One line of the answer: either a line of the original, or something this module wrote.
struct OutLine<'a> {
    text: Cow<'a, str>,
    /// Index into the original's lines, or `None` for a marker.
    origin: Option<usize>,
}

impl SlicePlan {
    /// Validate a request, compiling [`SliceRequest::grep`].
    ///
    /// The module's only fallible operation. See the module docs for why it is separated from
    /// [`Self::slice`] rather than folded into it.
    pub fn new(req: &SliceRequest) -> Result<SlicePlan, InvalidPattern> {
        let grep = match &req.grep {
            Some(p) => Some(Regex::new(p).map_err(|source| InvalidPattern {
                pattern: p.clone(),
                source,
            })?),
            None => None,
        };
        Ok(SlicePlan {
            head: req.head,
            tail: req.tail,
            grep,
            context: req.context,
            max_bytes: req.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        })
    }

    /// Slice `text`. Total — once the pattern compiled there is nothing left to fail: the input is
    /// already `&str`, so encoding cannot fail either, and the report is the channel for
    /// everything a caller needs to know about what was dropped.
    ///
    /// See the module docs for the order the options apply in and why it is that order.
    #[must_use]
    pub fn slice(&self, text: &str) -> Slice {
        let lines = split_lines(text);
        let total_lines = lines.len();

        // 1. `grep` picks candidates, widened by `context` and merged where the windows overlap.
        let (candidates, matched_lines) = match &self.grep {
            Some(re) => {
                let matched: Vec<usize> = (0..total_lines)
                    .filter(|&i| re.is_match(strip_cr(lines[i])))
                    .collect();
                let mut selected: Vec<usize> = Vec::new();
                for &m in &matched {
                    let lo = m.saturating_sub(self.context);
                    let hi = (m + self.context).min(total_lines.saturating_sub(1));
                    // `selected` is built in increasing order, so a window that overlaps the
                    // previous one simply continues past its end; nothing is ever pushed twice.
                    let start = match selected.last() {
                        Some(&last) => lo.max(last + 1),
                        None => lo,
                    };
                    selected.extend(start..=hi);
                }
                (selected, Some(matched.len()))
            }
            None => ((0..total_lines).collect::<Vec<usize>>(), None),
        };

        // 2. `head`/`tail` cut the candidate list.
        let n = candidates.len();
        let kept: Vec<usize> = match (self.head, self.tail) {
            (None, None) => candidates,
            (Some(h), None) => candidates[..h.min(n)].to_vec(),
            (None, Some(t)) => candidates[n - t.min(n)..].to_vec(),
            (Some(h), Some(t)) => {
                let (h, t) = (h.min(n), t.min(n));
                // Touching or overlapping ranges cover everything, so there is no gap to mark. A
                // marker here would read as `[... 0 lines omitted ...]`, which is a lie about
                // there being a boundary at all.
                if h + t >= n {
                    candidates
                } else {
                    let mut v = candidates[..h].to_vec();
                    v.extend_from_slice(&candidates[n - t..]);
                    v
                }
            }
        };

        // 3. Render, marking every discontinuity in the *original* line numbers.
        let mut rendered: Vec<OutLine<'_>> = Vec::with_capacity(kept.len());
        let mut prev: Option<usize> = None;
        for &i in &kept {
            if let Some(p) = prev
                && i > p + 1
            {
                rendered.push(OutLine {
                    text: Cow::Owned(gap_marker(i - p - 1)),
                    origin: None,
                });
            }
            rendered.push(OutLine {
                text: Cow::Borrowed(lines[i]),
                origin: Some(i),
            });
            prev = Some(i);
        }

        // 4. The byte cap, last.
        let keep_end = self.head.is_none() && self.tail.is_some();
        let (out, truncated, cut_mid_line) = apply_budget(rendered, self.max_bytes, keep_end);

        let returned_lines = out.iter().filter(|l| l.origin.is_some()).count();
        let gaps = out
            .iter()
            .filter(|l| l.origin.is_none() && l.text != TRUNCATION_NOTICE)
            .count();

        let joined: Vec<&str> = out.iter().map(|l| l.text.as_ref()).collect();
        Slice {
            text: joined.join("\n"),
            report: SliceReport {
                total_bytes: text.len(),
                total_lines,
                matched_lines,
                returned_lines,
                dropped_lines: total_lines - returned_lines,
                gaps,
                truncated,
                cut_mid_line,
            },
        }
    }
}

/// Split into lines without inventing one.
///
/// `"a\n"` is one line, not two: a trailing newline terminates the last line rather than starting
/// an empty one, and counting it would make every well-formed log report one line more than it
/// has. `""` is zero lines — an empty output has no content, and reporting `1` would let a caller
/// believe it received something.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n').collect()
}

/// Drop a single trailing `\r` for matching purposes.
///
/// CRLF input is common (Windows toolchains, some CI runners) and a `\r` sitting between the last
/// character and the end of the line would make every `$` anchor fail on it. The `\r` stays in the
/// returned text — the slice is meant to be byte-faithful to the lines it kept — it is only
/// invisible to the matcher.
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// The largest `index <= at` that `s` can be split on. `str::floor_char_boundary` is unstable.
fn floor_char_boundary(s: &str, at: usize) -> usize {
    if at >= s.len() {
        return s.len();
    }
    let mut at = at;
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest `index >= at` that `s` can be split on.
fn ceil_char_boundary(s: &str, at: usize) -> usize {
    if at >= s.len() {
        return s.len();
    }
    let mut at = at;
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Enforce the hard cap, returning the surviving lines plus whether it bit and whether it had to
/// cut inside a line.
///
/// The rules, in priority order, each of which exists to keep a truncated answer from reading as a
/// whole one:
///
/// * the cap counts the joined result, separators, markers and notice included — see
///   [`SliceRequest::max_bytes`];
/// * lines are dropped whole. A cut lands inside a line only when not even one line fits, since
///   that is the only case where a line boundary is not available;
/// * a marker is never emitted partially. Half of `[... 40 lines omitted ...]` is worse than none;
/// * a cut inside a line lands on a UTF-8 character boundary, so the result is still `&str`-shaped
///   and a multi-byte character is never split into mojibake.
fn apply_budget<'a>(
    lines: Vec<OutLine<'a>>,
    max_bytes: usize,
    keep_end: bool,
) -> (Vec<OutLine<'a>>, bool, bool) {
    let total: usize =
        lines.iter().map(|l| l.text.len()).sum::<usize>() + lines.len().saturating_sub(1);
    if total <= max_bytes {
        return (lines, false, false);
    }

    // Reserve room for the notice, which is emitted inside the budget. When even the notice does
    // not fit we spend everything on content: the report still tells the truth.
    let reserve = if TRUNCATION_NOTICE.len() < max_bytes {
        TRUNCATION_NOTICE.len() + 1
    } else if TRUNCATION_NOTICE.len() == max_bytes {
        TRUNCATION_NOTICE.len()
    } else {
        0
    };
    let content_budget = max_bytes.saturating_sub(reserve);

    let mut kept: Vec<OutLine<'a>> = Vec::new();
    let mut used = 0usize;
    let mut cut_mid_line = false;
    let order: Vec<usize> = if keep_end {
        (0..lines.len()).rev().collect()
    } else {
        (0..lines.len()).collect()
    };
    for idx in order {
        let line = &lines[idx];
        let cost = line.text.len() + usize::from(used > 0);
        if used + cost <= content_budget {
            used += cost;
            kept.push(OutLine {
                text: line.text.clone(),
                origin: line.origin,
            });
            continue;
        }
        if used == 0 && line.origin.is_some() && content_budget > 0 {
            // Nothing fits whole, so this is the one case where a line boundary is unavailable.
            let s = line.text.as_ref();
            let piece = if keep_end {
                &s[ceil_char_boundary(s, s.len() - content_budget)..]
            } else {
                &s[..floor_char_boundary(s, content_budget)]
            };
            if !piece.is_empty() {
                kept.push(OutLine {
                    text: Cow::Owned(piece.to_string()),
                    origin: line.origin,
                });
                cut_mid_line = true;
            }
        }
        break;
    }
    if keep_end {
        kept.reverse();
    }

    if reserve > 0 {
        let notice = OutLine {
            text: Cow::Borrowed(TRUNCATION_NOTICE),
            origin: None,
        };
        // The notice sits where the missing bytes are: at the front when we kept the end.
        if keep_end {
            kept.insert(0, notice);
        } else {
            kept.push(notice);
        }
    }
    (kept, true, cut_mid_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A numbered log, one line per number: `L1` .. `Ln`, no trailing newline unless asked.
    fn log(n: usize) -> String {
        (1..=n)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Compile and slice. Every test that is not about the malformed-pattern path goes through
    /// here, so a pattern that stopped compiling shows up as a failure in that test rather than
    /// as a wrong answer everywhere.
    fn run(text: &str, r: &SliceRequest) -> Slice {
        SlicePlan::new(r).expect("pattern compiles").slice(text)
    }

    fn req() -> SliceRequest {
        SliceRequest {
            // Big enough that the byte cap never bites in a test that is not about the byte cap.
            max_bytes: Some(usize::MAX),
            ..SliceRequest::default()
        }
    }

    #[test]
    fn an_empty_output_reports_nothing_rather_than_pretending_to_be_a_slice() {
        let s = run("", &req());
        assert_eq!(s.text, "");
        assert_eq!(s.report.total_lines, 0);
        assert_eq!(s.report.total_bytes, 0);
        assert_eq!(s.report.returned_lines, 0);
        assert!(s.report.is_complete());
    }

    /// Without this, `split('\n')` on `"a\n"` yields a phantom empty final line, and every count
    /// this module reports about a well-formed log is one too high.
    #[test]
    fn a_trailing_newline_does_not_invent_a_final_empty_line() {
        assert_eq!(run("a\nb\n", &req()).report.total_lines, 2);
        assert_eq!(run("a\nb", &req()).report.total_lines, 2);
        // A genuinely empty final line is still a line.
        assert_eq!(run("a\nb\n\n", &req()).report.total_lines, 3);
    }

    /// The common shape for a captured stream that was cut off. Losing the last line here would
    /// lose the one line a caller reading a crashed process most wants.
    #[test]
    fn output_without_a_trailing_newline_keeps_its_last_line() {
        let s = run("a\nb\nlast", &req());
        assert!(s.text.ends_with("last"), "{}", s.text);
        assert_eq!(s.report.returned_lines, 3);
    }

    #[test]
    fn head_returns_the_first_n_lines_and_says_what_it_dropped() {
        let s = run(
            &log(10),
            &SliceRequest {
                head: Some(3),
                ..req()
            },
        );
        assert_eq!(s.text, "L1\nL2\nL3");
        assert_eq!(s.report.returned_lines, 3);
        assert_eq!(s.report.dropped_lines, 7);
        assert!(!s.report.is_complete());
    }

    #[test]
    fn tail_returns_the_last_n_lines_and_says_what_it_dropped() {
        let s = run(
            &log(10),
            &SliceRequest {
                tail: Some(2),
                ..req()
            },
        );
        assert_eq!(s.text, "L9\nL10");
        assert_eq!(s.report.returned_lines, 2);
        assert_eq!(s.report.dropped_lines, 8);
    }

    /// The single most important test in the file. Joining the two ends with nothing between them
    /// would pass every count assertion above while telling the reader that `L2` is followed by
    /// `L9` — fabricated adjacency in a log is how a reader concludes one line caused the next.
    #[test]
    fn head_and_tail_together_never_pretend_the_gap_was_not_there() {
        let s = run(
            &log(10),
            &SliceRequest {
                head: Some(2),
                tail: Some(2),
                ..req()
            },
        );
        assert_eq!(s.text, "L1\nL2\n[... 6 lines omitted ...]\nL9\nL10");
        assert_eq!(s.report.returned_lines, 4);
        assert_eq!(s.report.dropped_lines, 6);
        assert_eq!(s.report.gaps, 1);
    }

    /// `head: 5` + `tail: 5` over ten lines covers everything. A marker here would read as
    /// `[... 0 lines omitted ...]` — a boundary announced where none exists, which teaches a
    /// reader to distrust the markers that do mean something.
    #[test]
    fn head_and_tail_that_touch_emit_no_marker_spanning_nothing() {
        let s = run(
            &log(10),
            &SliceRequest {
                head: Some(5),
                tail: Some(5),
                ..req()
            },
        );
        assert_eq!(s.text, log(10));
        assert_eq!(s.report.gaps, 0);
        assert!(s.report.is_complete());
    }

    /// Overlapping ranges must not emit the shared lines twice either — a duplicated `error:`
    /// line reads as two failures.
    #[test]
    fn head_and_tail_that_overlap_emit_the_whole_output_once() {
        let s = run(
            &log(10),
            &SliceRequest {
                head: Some(8),
                tail: Some(8),
                ..req()
            },
        );
        assert_eq!(s.text, log(10));
        assert_eq!(s.report.returned_lines, 10);
        assert_eq!(s.report.gaps, 0);
        assert!(s.report.is_complete());
    }

    #[test]
    fn head_or_tail_larger_than_the_output_is_the_whole_output_and_reads_as_complete() {
        for r in [
            SliceRequest {
                head: Some(99),
                ..req()
            },
            SliceRequest {
                tail: Some(99),
                ..req()
            },
        ] {
            let s = run(&log(3), &r);
            assert_eq!(s.text, log(3));
            assert!(s.report.is_complete(), "{:?}", s.report);
        }
    }

    #[test]
    fn grep_returns_only_matching_lines_with_their_context() {
        let text = "alpha\nbeta\nERROR here\ngamma\ndelta";
        let s = run(
            text,
            &SliceRequest {
                grep: Some("ERROR".into()),
                context: 1,
                ..req()
            },
        );
        // No marker at either edge: an edge omission fabricates nothing, and the report already
        // carries the count. See the module docs.
        assert_eq!(s.text, "beta\nERROR here\ngamma");
        assert_eq!(s.report.matched_lines, Some(1));
        assert_eq!(s.report.returned_lines, 3);
    }

    /// Two matches three lines apart with `context: 2` share a line. Emitting each window
    /// independently would print that line twice and mark a gap of zero between them.
    #[test]
    fn overlapping_context_windows_merge_instead_of_repeating_lines() {
        let text = "1\nhit\n3\n4\nhit\n6";
        let s = run(
            text,
            &SliceRequest {
                grep: Some("hit".into()),
                context: 2,
                ..req()
            },
        );
        assert_eq!(s.text, text);
        assert_eq!(s.report.matched_lines, Some(2));
        assert_eq!(s.report.returned_lines, 6);
        assert_eq!(s.report.gaps, 0);
    }

    #[test]
    fn non_adjacent_grep_groups_are_separated_by_the_same_gap_marker() {
        let text = "hit\na\nb\nc\nd\nhit";
        let s = run(
            text,
            &SliceRequest {
                grep: Some("hit".into()),
                ..req()
            },
        );
        assert_eq!(s.text, "hit\n[... 4 lines omitted ...]\nhit");
        assert_eq!(s.report.gaps, 1);
    }

    /// `Some(0)` and `None` must not collapse: "the pattern found nothing" and "no pattern was
    /// given" lead a caller to opposite next moves, and an empty `text` alone cannot tell them
    /// apart from an empty log.
    #[test]
    fn a_grep_that_matches_nothing_says_zero_matched_not_empty_output() {
        let s = run(
            &log(50),
            &SliceRequest {
                grep: Some("nowhere".into()),
                ..req()
            },
        );
        assert_eq!(s.text, "");
        assert_eq!(s.report.matched_lines, Some(0));
        assert_eq!(s.report.total_lines, 50);
        assert_eq!(s.report.dropped_lines, 50);
        assert!(!s.report.is_complete());
        assert!(s.report.summary().contains("0 matched"));
        assert_eq!(run(&log(50), &req()).report.matched_lines, None);
    }

    #[test]
    fn a_grep_that_matches_everything_is_still_reported_as_complete() {
        let s = run(
            &log(10),
            &SliceRequest {
                grep: Some("L".into()),
                ..req()
            },
        );
        assert_eq!(s.text, log(10));
        assert_eq!(s.report.matched_lines, Some(10));
        assert!(s.report.is_complete());
    }

    /// The whole reason `regex` is a direct dependency. Under the literal-substring matcher this
    /// module used to carry, every pattern below except the bare alternation matched nothing —
    /// and a caller sending `.*error.*` to a log full of errors was told, truthfully and
    /// uselessly, that zero lines matched.
    #[test]
    fn grep_is_a_real_regex_and_not_a_substring_match() {
        let text = "warn: a\nerror: b\nplain\ncode 42 here";
        let hits = |p: &str| {
            run(
                text,
                &SliceRequest {
                    grep: Some(p.into()),
                    ..req()
                },
            )
            .text
        };
        assert_eq!(hits(".*error.*"), "error: b");
        assert_eq!(hits("[0-9]+"), "code 42 here");
        assert_eq!(hits(r"\bcode\s+\d+"), "code 42 here");
        assert_eq!(hits("warn|error"), "warn: a\nerror: b");
        // Inline flags work, which is how a caller asks for the case-insensitive match that the
        // default deliberately is not.
        assert_eq!(hits("(?i)WARN"), "warn: a");
    }

    /// Lines are matched one at a time, so `^`/`$` are line anchors without the `(?m)` flag a
    /// caller would otherwise have to know to pass — and a pattern cannot reach across a line
    /// break to match text that was never on one line.
    #[test]
    fn anchors_bind_to_a_line_without_needing_the_multiline_flag() {
        let text = "warn: a\nerror: b\nplain";
        let hits = |p: &str| {
            run(
                text,
                &SliceRequest {
                    grep: Some(p.into()),
                    ..req()
                },
            )
            .text
        };
        assert_eq!(hits("^error"), "error: b");
        assert_eq!(hits("^plain$"), "plain");
        assert_eq!(hits("a\nerror"), "");
    }

    /// The failure a model actually produces. Matching a malformed pattern literally, or as
    /// nothing, reports "0 lines matched" for a log full of errors — an answer that is wrong and
    /// looks right. It has to be an error, and the error has to carry the regex crate's own
    /// message, since a caller that sent `[unclosed` can only fix what it is shown.
    #[test]
    fn a_malformed_pattern_is_a_usable_syntax_error_not_a_silent_zero() {
        let bad = SliceRequest {
            grep: Some("[unclosed".into()),
            ..req()
        };
        let err = SlicePlan::new(&bad).expect_err("a malformed pattern must not compile");
        let rendered = err.to_string();
        assert!(rendered.contains("[unclosed"), "{rendered}");
        // The regex crate's diagnostic, not a bare "invalid pattern".
        assert!(
            rendered.contains("unclosed character class"),
            "the underlying syntax error must survive into the message: {rendered}"
        );
        assert_eq!(err.pattern, "[unclosed");
        assert!(std::error::Error::source(&err).is_some());
    }

    /// Pins the reason the compile step is a separate call: a tool layer can answer
    /// `invalid_params` before it reads a 200 KB output out of the index, and one plan slices
    /// every output of a turn without recompiling.
    #[test]
    fn a_pattern_is_validated_before_any_output_is_read() {
        let r = SliceRequest {
            grep: Some("err".into()),
            ..req()
        };
        // No text is involved in validation at all.
        let plan = SlicePlan::new(&r).expect("valid");
        assert_eq!(plan.slice("err one").report.matched_lines, Some(1));
        assert_eq!(plan.slice("nothing here").report.matched_lines, Some(0));
    }

    /// A CRLF log ends every line with `\r`, so an unstripped `$` anchor matches nothing in the
    /// whole file — and reports it as an honest zero.
    #[test]
    fn carriage_returns_do_not_break_the_end_of_line_anchor() {
        let s = run(
            "alpha\r\nbeta\r\n",
            &SliceRequest {
                grep: Some("beta$".into()),
                ..req()
            },
        );
        assert_eq!(s.report.matched_lines, Some(1));
        assert_eq!(s.text, "beta\r");
    }

    /// The `\r` is invisible to the matcher but must survive into the output: the slice is meant
    /// to be byte-faithful to the lines it kept, so a caller diffing it against the original does
    /// not see phantom changes.
    #[test]
    fn crlf_input_comes_back_with_its_carriage_returns_intact() {
        let s = run("a\r\nb\r\nc\r\n", &req());
        assert_eq!(s.text, "a\r\nb\r\nc\r");
        assert_eq!(s.report.total_lines, 3);
    }

    /// If the cap ran first, `grep` would search only the first 30 bytes and report no matches for
    /// a log that plainly contains one.
    #[test]
    fn the_byte_cap_applies_after_the_lines_were_chosen_not_before() {
        let mut text = "filler line\n".repeat(500);
        text.push_str("NEEDLE at the very end");
        let s = run(
            &text,
            &SliceRequest {
                grep: Some("NEEDLE".into()),
                max_bytes: Some(60),
                ..SliceRequest::default()
            },
        );
        assert_eq!(s.report.matched_lines, Some(1));
        assert!(s.text.contains("NEEDLE"), "{}", s.text);
    }

    /// A cut at a raw byte index inside `é` (two bytes) would panic on the slice, or with a
    /// lenient implementation produce a lone continuation byte that is not valid UTF-8.
    #[test]
    fn a_cut_never_lands_inside_a_character() {
        // One line, all multi-byte, so the cut has to happen mid-line and lands between chars.
        let text = "éééééééééé";
        for budget in 0..=text.len() {
            let s = run(
                text,
                &SliceRequest {
                    max_bytes: Some(budget),
                    ..SliceRequest::default()
                },
            );
            assert!(s.text.len() <= budget, "budget {budget}: {:?}", s.text);
            // Reaching here at all is half the test: a cut at a raw byte index inside `é` panics
            // in `slice`. The rest is that what came back is whole characters of the original and
            // not a truncated one rendered as something else.
            let content: String = s.text.replace(TRUNCATION_NOTICE, "");
            assert!(
                content.chars().all(|c| c == 'é'),
                "budget {budget} produced {content:?}"
            );
            assert_eq!(content.len() % 'é'.len_utf8(), 0, "budget {budget}");
        }
    }

    #[test]
    fn a_single_line_longer_than_the_budget_is_cut_mid_line_and_says_so() {
        let text = "x".repeat(1000);
        let s = run(
            &text,
            &SliceRequest {
                max_bytes: Some(100),
                ..SliceRequest::default()
            },
        );
        assert!(s.text.len() <= 100);
        assert!(s.report.truncated);
        assert!(s.report.cut_mid_line);
        assert!(!s.report.is_complete());
        assert!(s.report.summary().contains("cut mid-line"));
    }

    /// A zero budget is a legitimate request (a caller counting bytes before deciding), and must
    /// not panic, underflow, or hand back a notice that busts the budget it was asked to respect.
    #[test]
    fn a_max_bytes_of_zero_returns_nothing_and_admits_it() {
        let s = run(
            &log(10),
            &SliceRequest {
                max_bytes: Some(0),
                ..SliceRequest::default()
            },
        );
        assert_eq!(s.text, "");
        assert!(s.report.truncated);
        assert_eq!(s.report.returned_lines, 0);
        assert_eq!(s.report.total_lines, 10);
        assert!(!s.report.is_complete());
    }

    /// The markers and the notice are this module's own bytes, and a cap that excluded them would
    /// hand a caller with a 200-byte budget more than 200 bytes — which makes the budget useless
    /// for the one thing it is for: bounding the next request.
    #[test]
    fn the_returned_text_never_exceeds_the_byte_cap_including_its_markers() {
        let text = log(200);
        for budget in [0, 1, 5, 39, 40, 41, 60, 100, 250, 1000, 100_000] {
            for r in [
                SliceRequest {
                    max_bytes: Some(budget),
                    ..SliceRequest::default()
                },
                SliceRequest {
                    head: Some(20),
                    tail: Some(20),
                    max_bytes: Some(budget),
                    ..SliceRequest::default()
                },
                SliceRequest {
                    tail: Some(30),
                    max_bytes: Some(budget),
                    ..SliceRequest::default()
                },
                SliceRequest {
                    grep: Some("L1".into()),
                    context: 2,
                    max_bytes: Some(budget),
                    ..SliceRequest::default()
                },
            ] {
                let s = run(&text, &r);
                assert!(
                    s.text.len() <= budget,
                    "budget {budget} exceeded ({}) by {r:?}",
                    s.text.len()
                );
                // A half-written marker is worse than no marker.
                assert!(
                    !s.text.contains("[... ") || s.text.contains(" ...]"),
                    "partial marker at budget {budget}: {:?}",
                    s.text
                );
            }
        }
    }

    /// A caller that asked for the last 100 lines and got the first 20 of them got the opposite of
    /// what it asked for, and would read a build log's opening banner as its failure.
    #[test]
    fn a_tail_only_request_keeps_the_end_when_the_budget_bites() {
        let s = run(
            &log(200),
            &SliceRequest {
                tail: Some(100),
                max_bytes: Some(80),
                ..SliceRequest::default()
            },
        );
        assert!(s.text.contains("L200"), "{}", s.text);
        assert!(!s.text.contains("L101"), "{}", s.text);
        assert!(s.report.truncated);
        // The notice sits where the missing bytes are.
        assert!(s.text.starts_with(TRUNCATION_NOTICE), "{}", s.text);
    }

    /// `head` counting original lines instead of matches would return the first 2 lines of the
    /// file — which do not match at all — for a request that plainly asked for matches.
    #[test]
    fn grep_then_head_slices_the_matches_not_the_original_lines() {
        let text = "skip\nskip\nhit a\nskip\nhit b\nskip\nhit c";
        let s = run(
            text,
            &SliceRequest {
                grep: Some("hit".into()),
                head: Some(2),
                ..req()
            },
        );
        assert_eq!(s.text, "hit a\n[... 1 line omitted ...]\nhit b");
        assert_eq!(s.report.matched_lines, Some(3));
        assert_eq!(s.report.returned_lines, 2);
    }

    /// `is_complete` is what a caller checks before summarising a slice as if it were the log, so
    /// every way of losing a byte has to clear it.
    #[test]
    fn a_complete_slice_is_the_only_one_that_reports_itself_complete() {
        let full = run(&log(5), &req());
        assert!(full.report.is_complete());
        assert!(full.report.summary().contains("complete output"));

        for r in [
            SliceRequest {
                head: Some(2),
                ..req()
            },
            SliceRequest {
                grep: Some("L1$".into()),
                ..req()
            },
            SliceRequest {
                max_bytes: Some(4),
                ..SliceRequest::default()
            },
        ] {
            let s = run(&log(5), &r);
            assert!(!s.report.is_complete(), "{r:?} => {:?}", s.report);
            assert!(!s.report.summary().contains("complete output"));
        }
    }

    /// The counts are for sizing a follow-up request, so they describe the original. Reporting the
    /// slice's own size as `total_bytes` would tell a caller a 200 KB log is 400 bytes, and it
    /// would stop asking.
    #[test]
    fn the_report_counts_the_original_not_what_survived() {
        let text = log(100);
        let s = run(
            &text,
            &SliceRequest {
                head: Some(1),
                ..req()
            },
        );
        assert_eq!(s.report.total_bytes, text.len());
        assert_eq!(s.report.total_lines, 100);
        assert_eq!(s.report.returned_lines, 1);
        assert_eq!(s.report.dropped_lines, 99);
        assert!(s.text.len() < s.report.total_bytes);
    }

    /// `None` means [`DEFAULT_MAX_BYTES`], not unbounded — the whole point of the module is that
    /// nobody gets a 200 KB answer by forgetting to ask for a cap.
    #[test]
    fn an_unspecified_budget_is_the_default_not_unbounded() {
        let text = "y".repeat(DEFAULT_MAX_BYTES * 2);
        let s = run(&text, &SliceRequest::default());
        assert!(s.text.len() <= DEFAULT_MAX_BYTES);
        assert!(s.report.truncated);
    }
}
