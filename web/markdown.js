// A deliberately small markdown subset, for text a language model wrote: a Task prompt, an
// assistant turn, the report a subagent handed back.
//
// Two rules, and everything else here follows from them.
//
// 1. Nodes are built, never assembled. `innerHTML` does not appear in this file and no string
//    ever travels from the input into markup: every piece of the input reaches the page as a
//    text node, and every element and attribute exists because a line of code below decided it
//    should. Escaping is what you do when you have already lost this argument — transcripts are
//    full of pasted HTML, `<img onerror=...>` included, and an escaping bug in a renderer that
//    concatenated strings would run that markup inside the reader's page.
// 2. Anything the subset does not understand stays literal. An unbalanced backtick, a nested
//    emphasis this parser cannot see, a `javascript:` link — all of them render as the
//    characters that were actually in the transcript. Silently dropping a line the parser could
//    not classify would make the transcript a lie about itself.
//
// Supported: fenced code with an info string, inline code, bold, italic, links
// (`http`/`https`/`mailto` only), ATX headings, blockquotes, bullet and numbered lists,
// horizontal rules. Nothing else — no tables, no images, no raw HTML.

import { el, frag } from "./dom.js";

const SAFE_SCHEMES = new Set(["http:", "https:", "mailto:"]);

/**
 * The absolute, allow-listed URL behind `raw`, or `null` if there is not one.
 *
 * Exported because `tools.js` has the same question to answer about a `WebFetch` url, and two
 * copies of "which schemes may become an href" is exactly the kind of pair that drifts apart.
 */
export function safeHref(raw) {
  try {
    const url = new URL(String(raw ?? "").trim());
    return SAFE_SCHEMES.has(url.protocol) ? url.href : null;
  } catch (_) {
    // A relative or malformed URL throws here, and throwing is the answer we want: without an
    // absolute URL carrying a scheme we recognise there is nothing safe to put in an href.
    return null;
  }
}

// Blockquotes, list items and emphasis all recurse. A model can emit "> > > > …" for a hundred
// levels, and a transcript records whatever it emitted, so the recursion needs a floor that is
// not the stack; past it the remaining text renders literally rather than not at all.
const MAX_DEPTH = 6;

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*(.*)$/;
const HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const RULE = /^ {0,3}(?:-{3,}|\*{3,}|_{3,})\s*$/;
const QUOTE = /^ {0,3}>\s?(.*)$/;
const BULLET = /^(\s*)([-*+])(\s+)(.*)$/;
const ORDERED = /^(\s*)(\d{1,9})([.)])(\s+)(.*)$/;
// The info string becomes `data-lang`, so it may only be a language token; the rest of the
// line (`bash title="x"` and friends) is dropped rather than pushed into an attribute.
const NOT_LANG = /[^A-Za-z0-9_+.#-]/g;

/** Markdown text -> a `DocumentFragment`. Never returns, or accepts, HTML. */
export function renderMarkdown(text) {
  return blocks(String(text ?? "").split(/\r?\n/), 0);
}

function blocks(lines, depth) {
  const out = frag();
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      i += 1;
      continue;
    }

    const fence = FENCE.exec(line);
    if (fence) {
      const [node, next] = fencedCode(lines, i, fence);
      out.appendChild(node);
      i = next;
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      out.appendChild(el("h" + heading[1].length, {}, inline(heading[2], depth)));
      i += 1;
      continue;
    }

    // Before the bullet check: `* * *` is a rule, and also a well-formed bullet list.
    if (RULE.test(line)) {
      out.appendChild(el("hr", {}));
      i += 1;
      continue;
    }

    if (QUOTE.test(line)) {
      const [node, next] = quote(lines, i, depth);
      out.appendChild(node);
      i = next;
      continue;
    }

    if (BULLET.test(line) || ORDERED.test(line)) {
      const [node, next] = list(lines, i, depth);
      if (node) {
        out.appendChild(node);
        i = next;
        continue;
      }
    }

    const [node, next] = paragraph(lines, i, depth);
    out.appendChild(node);
    i = next;
  }
  return out;
}

function startsBlock(line) {
  return (
    FENCE.test(line) ||
    HEADING.test(line) ||
    RULE.test(line) ||
    QUOTE.test(line) ||
    BULLET.test(line) ||
    ORDERED.test(line)
  );
}

function fencedCode(lines, start, opener) {
  const marker = opener[1];
  const lang = (opener[2].trim().split(/\s+/)[0] || "").replace(NOT_LANG, "");
  const close = new RegExp("^ {0,3}" + (marker[0] === "~" ? "~" : "`") + "{" + marker.length + ",}\\s*$");
  const buf = [];
  let i = start + 1;
  while (i < lines.length) {
    if (close.test(lines[i])) {
      i += 1;
      break;
    }
    buf.push(lines[i]);
    i += 1;
  }
  // An unclosed fence runs to the end of the input, which is both what CommonMark says and the
  // only reading that keeps every remaining line visible; treating it as literal text instead
  // would re-interpret the code as markdown, and code is full of `*` and `_`.
  const pre = el(
    "pre",
    { class: "ss-code", dataset: lang ? { lang } : null },
    el("code", { text: buf.join("\n") }),
  );
  return [pre, i];
}

function quote(lines, start, depth) {
  const buf = [];
  let i = start;
  while (i < lines.length) {
    const m = QUOTE.exec(lines[i]);
    if (m) {
      buf.push(m[1]);
      i += 1;
      continue;
    }
    // A quoted paragraph may run onto lines that forgot their `>`; a blank line or a new block
    // ends the quote.
    if (!lines[i].trim() || startsBlock(lines[i])) break;
    buf.push(lines[i]);
    i += 1;
  }
  const inner = depth < MAX_DEPTH ? blocks(buf, depth + 1) : el("p", { text: buf.join("\n") });
  return [el("blockquote", {}, inner), i];
}

function list(lines, start, depth) {
  const firstBullet = BULLET.exec(lines[start]);
  const firstOrdered = firstBullet ? null : ORDERED.exec(lines[start]);
  const head = firstBullet || firstOrdered;
  if (!head) return [null, start];
  const ordered = Boolean(firstOrdered);
  const indent = head[1].length;
  const startNum = ordered ? Number(firstOrdered[2]) : 1;

  const items = [];
  let i = start;
  while (i < lines.length) {
    const bullet = BULLET.exec(lines[i]);
    const numbered = bullet ? null : ORDERED.exec(lines[i]);
    const m = bullet || numbered;
    // A different marker kind, or a different indent, is a different list; ending this one
    // keeps `1. a` under `- a` from becoming a sibling of it.
    if (!m || m[1].length !== indent || Boolean(numbered) !== ordered) break;
    const marker = bullet ? bullet[2] + bullet[3] : numbered[2] + numbered[3] + numbered[4];
    const contentIndent = indent + marker.length;
    const buf = [bullet ? bullet[4] : numbered[5]];
    i += 1;
    while (i < lines.length) {
      const line = lines[i];
      if (!line.trim()) {
        // A blank line only continues the item if something is still indented under it;
        // otherwise the list ends here and the outer loop decides what follows.
        const next = lines[i + 1] || "";
        if (next.trim() && leading(next) >= contentIndent) {
          buf.push("");
          i += 1;
          continue;
        }
        break;
      }
      if (leading(line) >= contentIndent) {
        buf.push(line.slice(contentIndent));
        i += 1;
        continue;
      }
      if (startsBlock(line)) break;
      buf.push(line.trim());
      i += 1;
    }
    items.push(buf);
  }

  const children = items.map((buf) => {
    const li = el("li", {});
    if (depth >= MAX_DEPTH) {
      li.textContent = buf.join("\n");
      return li;
    }
    const inner = blocks(buf, depth + 1);
    // A one-paragraph item is a line of text, not a paragraph inside a bullet; unwrapping it
    // keeps a tight list from being spaced like a loose one.
    if (inner.childNodes.length === 1 && inner.firstChild.nodeName === "P") {
      li.append(...inner.firstChild.childNodes);
    } else {
      li.appendChild(inner);
    }
    return li;
  });
  const props = ordered && Number.isFinite(startNum) && startNum !== 1 ? { start: startNum } : {};
  return [el(ordered ? "ol" : "ul", props, children), i];
}

function leading(line) {
  const m = /^\s*/.exec(line);
  return m ? m[0].length : 0;
}

function paragraph(lines, start, depth) {
  const buf = [];
  let i = start;
  // The first line already failed every block test above, so this always consumes at least one
  // line and the caller's loop always advances.
  do {
    buf.push(lines[i]);
    i += 1;
  } while (i < lines.length && lines[i].trim() && !startsBlock(lines[i]));

  const p = el("p", {});
  buf.forEach((line, idx) => {
    // A single newline inside a paragraph is a line break, not a space. Models lay text out
    // with them deliberately, and reflowing an aligned list of options into one run of prose
    // loses information the writer put there.
    if (idx) p.appendChild(el("br", {}));
    p.appendChild(inline(line, depth));
  });
  return [p, i];
}

// One pass, one alternation, in precedence order: an escape, then a code span (whose contents
// are literal by definition), then a link, then bold, then italic. Named groups say which arm
// matched without counting parentheses.
const INLINE_SOURCE = [
  "(?<esc>\\\\[\\\\`*_{}\\[\\]()#+\\-.!>~])",
  "(?<ticks>`+)(?<code>[\\s\\S]*?)\\k<ticks>",
  "\\[(?<ltext>[^\\]]*)\\]\\((?<lhref>[^\\s()]*)\\)",
  "<(?<auto>[^<>\\s]+)>",
  "(?<sd>\\*\\*|__)(?<strong>[\\s\\S]+?)\\k<sd>",
  "(?<ed>[*_])(?<em>[^\\s][\\s\\S]*?)\\k<ed>",
].join("|");

function inline(text, depth) {
  const src = String(text);
  const out = frag();
  if (depth >= MAX_DEPTH) {
    out.appendChild(document.createTextNode(src));
    return out;
  }
  // A fresh regex per call, because this function recurses through emphasis and a link's text.
  // A shared global one carries `lastIndex`: the inner scan would rewind it, and the outer loop
  // would then re-match text it had already consumed — forever, allocating a node each time.
  const re = new RegExp(INLINE_SOURCE, "g");
  let last = 0;
  let m;
  while ((m = re.exec(src)) !== null) {
    if (m[0].length === 0) {
      // A zero-length match would spin the same way; nothing in the alternation can produce
      // one today, and this keeps that true if someone adds an arm that can.
      re.lastIndex += 1;
      continue;
    }
    if (m.index > last) out.appendChild(document.createTextNode(src.slice(last, m.index)));
    const node = inlineNode(m, src, depth);
    // `null` means "recognised the shape, refused it" — a `javascript:` href, emphasis inside
    // a snake_case identifier. The matched characters go back as the text they were.
    out.appendChild(node || document.createTextNode(m[0]));
    last = m.index + m[0].length;
  }
  if (last < src.length) out.appendChild(document.createTextNode(src.slice(last)));
  return out;
}

function inlineNode(m, src, depth) {
  const g = m.groups;
  if (g.esc !== undefined) return document.createTextNode(m[0].slice(1));
  if (g.code !== undefined) {
    // ``a `b` c`` — one leading and trailing space is the delimiter's, not the code's.
    return el("code", { class: "ss-code", text: g.code.replace(/^ ([\s\S]*) $/, "$1") });
  }
  if (g.ltext !== undefined) {
    const href = safeHref(g.lhref);
    return href ? link(href, inline(g.ltext, depth + 1)) : null;
  }
  if (g.auto !== undefined) {
    const href = safeHref(g.auto);
    return href ? link(href, document.createTextNode(g.auto)) : null;
  }
  if (g.strong !== undefined) {
    return intraword(g.sd, m, src) ? null : el("strong", {}, inline(g.strong, depth + 1));
  }
  if (g.em !== undefined) {
    return intraword(g.ed, m, src) ? null : el("em", {}, inline(g.em, depth + 1));
  }
  return null;
}

function link(href, child) {
  // `noopener` because the new tab must not get a handle on this one; `noreferrer` because the
  // referrer would leak that someone is reading a local transcript index.
  return el("a", { href, rel: "noopener noreferrer", target: "_blank" }, child);
}

function intraword(delim, m, src) {
  // `snake_case_name` is an identifier, not emphasis, and transcripts are mostly identifiers.
  // Asterisks have no such problem: nothing writes `a*b*c` and means multiplication twice.
  if (!delim.startsWith("_")) return false;
  const before = src[m.index - 1] || "";
  const after = src[m.index + m[0].length] || "";
  return /\w/.test(before) || /\w/.test(after);
}
