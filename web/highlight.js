// Query-term highlighting, everywhere the server's snippet is not.
//
// `dto::highlight_html` marks the one excerpt the server cut, and it is authoritative for that
// excerpt. But a chat transcript is read by expanding outwards — a thread window, a whole
// session in the drawer — and none of that text has passed through the highlighter. Without
// this file the reader gets one marked line and then four hundred turns of prose in which they
// have to find the words again by eye, which is the moment a search UI stops being one.
//
// The whole difficulty is that "does this word match" is not a string question here.
// `tokenizer.rs` indexes `open_or_create` as `openorcreate` + `open` + `or` + `create`, all at
// one position, so a search for `openOrCreate` finds the snake_case spelling and a search for
// `open` finds both. A substring highlighter would mark neither, and would then look broken in
// exactly the cases the analyzer was written to handle. So the rules below are a port of that
// analyzer's splitting — deliberately a port and not an approximation, because a highlighter
// that disagrees with the index is worse than none: it says "not here" about a document the
// index returned *because* the word was here.
//
// Two places it knowingly under-marks rather than guessing:
//
// * **Stemming.** `prose_analyzer` runs an English Snowball stemmer, so `compiling` matches
//   `compiled` in the index. Porting Snowball for a highlight is not worth it, so `foldSuffix`
//   below folds a short list of regular endings and stops. A stemmed match this misses stays
//   unmarked — the safe direction. Marking a word nobody searched for is the unsafe one.
// * **Field-scoped clauses.** `tool_output:"No such file"` marks that phrase wherever it
//   appears in the turn, not only in `tool_output`: the rendered nodes carry no field labels,
//   and the alternative is not marking a phrase the reader definitely searched for.
//
// Negated clauses (`NOT foo`, `-foo`) are never marked. A matching document cannot contain
// them, so anything that looked like one would be a false mark by construction.

import { el } from "./dom.js";

/** Letters, digits and `_` make a token; everything else separates two. `is_word` in tokenizer.rs. */
function isWord(ch) {
  return ch === "_" || /[\p{L}\p{N}]/u.test(ch);
}

/** At least two lowercase letters after the first character — `word_follows`. */
function wordFollows(rest) {
  const a = rest[1];
  const b = rest[2];
  return Boolean(a && b && a.toLowerCase() === a && a.toUpperCase() !== a && b.toLowerCase() === b && b.toUpperCase() !== b);
}

const isUpper = (c) => c.toUpperCase() === c && c.toLowerCase() !== c;
const isAlpha = (c) => /\p{L}/u.test(c);
const isDigit = (c) => /\p{N}/u.test(c);

/** Is there a part boundary before `cur`, which follows `prev` and heads `rest`? — `is_boundary`. */
function isBoundary(prev, cur, rest) {
  if (!isUpper(prev) && isAlpha(prev) && isUpper(cur)) return true;
  if (isUpper(prev) && isUpper(cur) && wordFollows(rest)) return true;
  return isDigit(prev) !== isDigit(cur);
}

/** Byte — here character — ranges of the sub-parts of one token. `parts_of`. */
function partsOf(text) {
  const parts = [];
  let start = null;
  let prev = null;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if (c === "_") {
      if (start !== null) {
        parts.push([start, i]);
        start = null;
      }
      prev = c;
      continue;
    }
    const boundary = prev !== null && prev !== "_" ? isBoundary(prev, c, text.slice(i)) : false;
    if (boundary && start !== null) {
      parts.push([start, i]);
      start = i;
    }
    if (start === null) start = i;
    prev = c;
  }
  if (start !== null) parts.push([start, text.length]);
  return parts;
}

const MIN_HEX_BLOB = 8;
const MAX_IDENT_PARTS = 6;

/** `is_blob`: a sha256 or a UUID chunk, whose fragments are nobody's search. */
function isBlob(whole, parts) {
  if (
    whole.length >= MIN_HEX_BLOB &&
    /^[0-9a-fA-F]+$/.test(whole) &&
    /[0-9]/.test(whole) &&
    /[a-fA-F]/.test(whole)
  ) {
    return true;
  }
  const short = parts.filter(([from, to]) => to - from <= 2).length;
  return parts.length > MAX_IDENT_PARTS && short * 2 > parts.length;
}

/**
 * The terms one whole-word token yields, lowercased — the whole separator-free form plus each
 * part, which is the set `SplitIdentifiers` emits at that one position.
 */
function termsOf(word) {
  const parts = partsOf(word);
  if (!parts.length) return [];
  if (parts.length === 1) {
    const [from, to] = parts[0];
    return [word.slice(from, to).toLowerCase()];
  }
  const whole = parts.map(([from, to]) => word.slice(from, to)).join("").toLowerCase();
  if (isBlob(whole, parts)) return [whole];
  return [whole, ...parts.map(([from, to]) => word.slice(from, to).toLowerCase())];
}

/** `[{start, end, terms}]` for `text`, in order. The base `WordTokenizer`, plus the filter. */
export function tokenize(text) {
  const out = [];
  let i = 0;
  while (i < text.length) {
    if (!isWord(text[i])) {
      i += 1;
      continue;
    }
    let j = i;
    while (j < text.length && isWord(text[j])) j += 1;
    const word = text.slice(i, j);
    const terms = termsOf(word);
    if (terms.length) out.push({ start: i, end: j, terms: new Set(terms) });
    i = j;
  }
  return out;
}

// A handful of regular English endings, folded on both sides so `parses` and `parse` meet in
// the middle. Every plausible fold is kept rather than the first one that fits — `queries` is
// `querie`+`s` or `quer`+`ies`, and only the second is the word — and two terms match when
// their fold sets intersect. The floor keeps `bus` from becoming `bu` and matching noise;
// anything irregular simply does not fold, and goes unmarked rather than marked wrongly.
const SUFFIXES = ["ingly", "edly", "ing", "ies", "ied", "es", "ed", "ly", "s"];
const STEM_FLOOR = 4;

function foldings(term) {
  const out = [term];
  for (const suffix of SUFFIXES) {
    if (term.length - suffix.length < STEM_FLOOR || !term.endsWith(suffix)) continue;
    const stem = term.slice(0, -suffix.length);
    // `ies`/`ied` -> `y` is the one irregularity common enough to be worth a line: `queries`
    // and `query` are the same word to any reader, and to the stemmer in the index.
    out.push(suffix === "ies" || suffix === "ied" ? stem + "y" : stem);
  }
  // Snowball drops a bare final `e` too, which is the whole difference between `parse` and
  // the `pars` that `parsed` and `parses` fold to.
  if (term.length > STEM_FLOOR && term.endsWith("e")) out.push(term.slice(0, -1));
  return out;
}

/** One word of the query: the terms a document token must carry for this word to match it. */
function queryWord(raw) {
  // `term~1` is phrase slop and `term*` is a prefix; neither belongs in the text being matched.
  let text = String(raw).replace(/~\d*$/, "");
  const prefix = text.endsWith("*");
  if (prefix) text = text.slice(0, -1);
  const terms = termsOf(text.replace(/[^\p{L}\p{N}_]/gu, "_"));
  if (!terms.length) return null;
  return { terms, folded: terms.map(foldings), prefix };
}

// `-x`, `+x`, `field:x`, `field:"a b"`, `"a b"`, `x`. Parens are structure this does not need:
// what is inside them is still a clause, and dropping the bracket loses nothing here.
const CLAUSE = /([-+])?(?:([\p{L}_][\p{L}\p{N}_.]*):)?(?:"([^"]*)"|([^\s"()]+))/gu;
const OPERATORS = new Set(["AND", "OR", "NOT", "TO"]);

/**
 * A query string -> what to mark: single words, and phrases as runs of consecutive words.
 *
 * The returned plan is `{words, phrases}` and is empty when there is nothing to mark, which is
 * the filter-only browse — there are no query terms there, and marking the filter *values*
 * would put emphasis on words the reader never typed.
 */
export function parseQuery(query) {
  const words = [];
  const phrases = [];
  let negateNext = false;
  for (const m of String(query ?? "").matchAll(CLAUSE)) {
    const [, sign, , quoted, bare] = m;
    if (quoted === undefined && OPERATORS.has(bare)) {
      // `NOT` negates the clause after it; `AND`/`OR`/`TO` are structure with nothing to mark.
      negateNext = bare === "NOT";
      continue;
    }
    const negated = negateNext || sign === "-";
    negateNext = false;
    if (negated) continue;

    if (quoted !== undefined) {
      const parts = quoted.split(/\s+/).filter(Boolean).map(queryWord).filter(Boolean);
      if (parts.length === 1) words.push(parts[0]);
      else if (parts.length > 1) phrases.push(parts);
      continue;
    }
    const word = queryWord(bare);
    if (word) words.push(word);
  }
  return { words, phrases };
}

export function isEmptyPlan(plan) {
  return !plan || (!plan.words.length && !plan.phrases.length);
}

/** Does `token` carry everything `word` needs? Exact first, then the folded forms. */
function wordMatches(word, token) {
  if (word.prefix) {
    for (const term of token.terms) {
      if (term.startsWith(word.terms[0])) return true;
    }
    return false;
  }
  if (word.terms.every((term) => token.terms.has(term))) return true;
  const folded = new Set();
  for (const term of token.terms) for (const fold of foldings(term)) folded.add(fold);
  return word.folded.every((folds) => folds.some((fold) => folded.has(fold)));
}

/** `[start, end]` character ranges of `text` that the plan marks, merged and in order. */
export function matchRanges(text, plan) {
  const tokens = tokenize(text);
  if (!tokens.length) return [];
  const ranges = [];

  for (const token of tokens) {
    if (plan.words.some((word) => wordMatches(word, token))) ranges.push([token.start, token.end]);
  }

  // A phrase is consecutive tokens, which is what the index means by one too — marking its
  // words wherever they appear separately would claim a phrase match that never happened.
  for (const phrase of plan.phrases) {
    for (let i = 0; i + phrase.length <= tokens.length; i += 1) {
      let hit = true;
      for (let j = 0; j < phrase.length; j += 1) {
        if (!wordMatches(phrase[j], tokens[i + j])) {
          hit = false;
          break;
        }
      }
      if (hit) ranges.push([tokens[i].start, tokens[i + phrase.length - 1].end]);
    }
  }

  if (ranges.length < 2) return ranges;
  ranges.sort((a, b) => a[0] - b[0] || b[1] - a[1]);
  const merged = [ranges[0]];
  for (const [start, end] of ranges.slice(1)) {
    const last = merged[merged.length - 1];
    if (start <= last[1]) last[1] = Math.max(last[1], end);
    else merged.push([start, end]);
  }
  return merged;
}

/** Elements whose text is chrome rather than transcript, or is already marked by the server. */
const SKIP = new Set(["MARK", "SCRIPT", "STYLE", "BUTTON", "SELECT", "TEXTAREA", "OPTION"]);

// A whole session is up to four hundred turns and a single `tool_output` can be a megabyte. The
// tokenizing is linear and cheap; what is not cheap is the DOM, so the budget counts marks, and
// a subtree that blows through it keeps the marks it got rather than losing them all.
const MAX_MARKS = 1500;

/**
 * Wrap every match under `root` in `<mark class="ss-hit">`; return how many were added.
 *
 * Text nodes are collected before any of them is replaced: splitting a node while the walker is
 * standing on it invalidates the walk, and the symptom is a document where marking stops
 * silently partway down.
 */
export function markMatches(root, plan) {
  if (!root || isEmptyPlan(plan)) return 0;

  const nodes = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode(node) {
      if (!node.nodeValue || !node.nodeValue.trim()) return NodeFilter.FILTER_REJECT;
      for (let at = node.parentElement; at && at !== root.parentElement; at = at.parentElement) {
        // `data-marked` is the server's excerpt: it already carries `<em>` around the spans
        // this index actually matched, and a second opinion over the top of it can only
        // disagree.
        if (SKIP.has(at.tagName) || at.dataset.marked !== undefined) return NodeFilter.FILTER_REJECT;
      }
      return NodeFilter.FILTER_ACCEPT;
    },
  });
  for (let node = walker.nextNode(); node; node = walker.nextNode()) nodes.push(node);

  let marks = 0;
  for (const node of nodes) {
    if (marks >= MAX_MARKS) break;
    const text = node.nodeValue;
    const ranges = matchRanges(text, plan);
    if (!ranges.length) continue;

    const out = document.createDocumentFragment();
    let cursor = 0;
    for (const [start, end] of ranges) {
      if (marks >= MAX_MARKS) break;
      if (start > cursor) out.appendChild(document.createTextNode(text.slice(cursor, start)));
      out.appendChild(el("mark", { class: "ss-hit", text: text.slice(start, end) }));
      cursor = end;
      marks += 1;
    }
    if (cursor < text.length) out.appendChild(document.createTextNode(text.slice(cursor)));
    node.parentNode.replaceChild(out, node);
  }
  return marks;
}

/** How many marks are under `root` — for a row that wants to say so without expanding. */
export function countMarks(root) {
  return root ? root.querySelectorAll("mark.ss-hit").length : 0;
}
