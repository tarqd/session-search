// One renderer per tool this build knows about, and the fallbacks that make the ones it does
// not know about survivable.
//
// The shape of this file is the point. A renderer is a pure function of `tool_input` that
// returns `null` the instant the shape it wants is not there, and `attempt()` is the only
// thing that ever calls one — inside a try/catch, with the generic parameter table waiting
// underneath. So the exported three cannot throw, and a renderer cannot half-build a card and
// then fail in the middle of it.
//
// That structure is not defensive habit, it is the requirement. A transcript is a record of
// what happened, including calls that were malformed when they were made: `tool_input` may be
// null, a string, an array, a number, or an object with none of the keys the tool documents.
// Most tools in a real corpus are MCP tools this build has never heard of. If the checks lived
// in the callers instead, the first `Read` with a number where a path belongs would blank a
// card, and nothing would say which document did it.
//
// `accent` is a hook for `styles.css`, not a colour: one of
// `run read write edit find web agent plan mcp tool`. An accent the stylesheet does not know
// must still look like a tool badge, so nothing here depends on it being styled.

import { basename, clampText, dirname, el, frag, iconFor, langFor } from "./dom.js";
import { renderMarkdown, safeHref } from "./markdown.js";

const META = {
  Bash: { label: "Bash", icon: "terminal", accent: "run" },
  Read: { label: "Read", icon: "file", accent: "read" },
  Write: { label: "Write", icon: "file", accent: "write" },
  Edit: { label: "Edit", icon: "edit", accent: "edit" },
  MultiEdit: { label: "MultiEdit", icon: "edit", accent: "edit" },
  NotebookEdit: { label: "NotebookEdit", icon: "book", accent: "write" },
  Glob: { label: "Glob", icon: "search_code", accent: "find" },
  Grep: { label: "Grep", icon: "search_code", accent: "find" },
  Task: { label: "Task", icon: "robot", accent: "agent" },
  Agent: { label: "Agent", icon: "robot", accent: "agent" },
  TodoWrite: { label: "TodoWrite", icon: "check", accent: "plan" },
  WebFetch: { label: "WebFetch", icon: "globe", accent: "web" },
  WebSearch: { label: "WebSearch", icon: "search", accent: "web" },
};

const UNKNOWN = { label: "tool", icon: "tool", accent: "tool" };

/** `{label, icon, accent}` for a tool call. Never throws; an unknown tool gets its own name. */
export function toolMeta(doc) {
  try {
    const name = toolName(asDoc(doc));
    if (!name) return { ...UNKNOWN };
    const meta = lookup(META, name);
  if (meta) return { ...meta };
    const mcp = splitMcp(name);
    if (mcp) return { label: mcp.server + "/" + mcp.tool, icon: "plug", accent: "mcp" };
    return { label: name, icon: "tool", accent: "tool" };
  } catch (_) {
    return { ...UNKNOWN };
  }
}

/**
 * The one line that stands for a whole call when the call is collapsed.
 *
 * A chat transcript reads as a column of activity rows — `Bash  cargo test --locked` — and
 * the row is only worth having if that second half is the thing the reader would have looked
 * for. So this is per tool rather than "the first key of `tool_input`": a `Read` is its path,
 * a `Grep` is its pattern, a `Task` is what the subagent was asked to do. Falling back to the
 * generic order (`KEY_PARAMS`) keeps an unknown or MCP tool from rendering a blank row.
 *
 * Never throws, and returns `""` when there is genuinely nothing to say — the row then shows
 * the tool name alone, which is still the truth.
 */
export function toolSummary(doc) {
  try {
    const d = asDoc(doc);
    const o = obj(d.tool_input);
    if (!o) return typeof d.tool_input === "string" ? oneLine(d.tool_input) : "";
    const name = toolName(d);

    if (name === "TodoWrite" && Array.isArray(o.todos)) {
      const done = o.todos.filter((t) => obj(t) && str(obj(t).status) === "completed").length;
      return done + " of " + o.todos.length + " done";
    }
    if (name === "MultiEdit" && Array.isArray(o.edits)) {
      const path = firstStr(o, "file_path", "path");
      return (path === null ? "" : basename(path) + " · ") + o.edits.length + " edits";
    }
    if (name === "Task" || name === "Agent") {
      return oneLine(firstStr(o, "description", "subagent_type", "prompt") ?? "");
    }
    // A path is shown as its basename: the row is one line and the directory is the half that
    // is the same for every row in the session. The full path is in the expanded card.
    const path = firstStr(o, "file_path", "path", "notebook_path");
    if (path !== null) return basename(path) || path;
    const key = firstStr(o, "command", "pattern", "query", "url", "prompt", "description");
    if (key !== null) return oneLine(key);
    for (const k of orderedKeys(o)) {
      const v = str(o[k]);
      if (v !== null && v.trim()) return oneLine(v);
    }
    return "";
  } catch (_) {
    return "";
  }
}

/** The first non-empty line, clamped: a heredoc's opening line stands for the heredoc. */
function oneLine(text) {
  const line = String(text).split("\n").find((l) => l.trim()) || "";
  return clampText(line.trim(), 96);
}

/** The call side of a tool document: what it was asked to do. Never throws. */
export function renderToolCall(doc) {
  const d = asDoc(doc);
  const name = toolName(d);
  const known = lookup(CALL, name) || (splitMcp(name) ? mcpCall : null);
  const node = known ? attempt(known, d, name) : null;
  return node || attempt(genericCall, d, "generic") || body(hint("this call could not be rendered"));
}

/**
 * The result side, or `null` when there is nothing to show.
 *
 * A failed call leads with its error: `--errors-only` retrieves exactly the right documents,
 * and on a real corpus the reason a call broke sits a thousand characters into its output,
 * past a heredoc. The same rule `format::doc_body` follows in the terminal.
 */
export function renderToolResult(doc) {
  const d = asDoc(doc);
  const out = str(d.tool_output);
  const failed = d.is_error === true;
  if (out === null || !out.trim()) {
    // A failed call with no output text still has to say it failed; rendering nothing would
    // read as "this worked and printed nothing".
    return failed ? body(errorHead("")) : null;
  }
  const name = toolName(d);
  const known = lookup(RESULT, name);
  const node = (known && attempt(known, d, name, out)) || attempt(genericResult, d, "result", out);
  return body(failed ? errorHead(out) : null, node || el("pre", { class: "ss-out", text: out }));
}

// --- the wrapper ------------------------------------------------------------------------

/**
 * A registry entry, or null — never something inherited from `Object.prototype`.
 *
 * These registries are keyed by `tool_name`, which comes out of the transcript, and the whole
 * premise of this file is tools nobody here has heard of naming themselves whatever they like.
 * A plain `CALL[name]` with `name === "constructor"` hands back `Object` and calls it: it
 * returns a truthy non-node, the fallback never runs, and the card renders `[object Object]`.
 */
function lookup(registry, name) {
  return typeof name === "string" && Object.prototype.hasOwnProperty.call(registry, name)
    ? registry[name]
    : null;
}

function attempt(render, doc, label, extra) {
  try {
    const node = render(doc.tool_input, doc, extra);
    // A renderer's contract is a node or nothing. Anything else is a bug in the renderer, and
    // letting it through means `append` stringifies it into the card.
    if (node instanceof Node) return node;
    if (node) {
      console.debug("session-search: " + (label || "tool") + " renderer returned a non-node", node);
      return null;
    }
    // One line, at debug level: a declined render is normal (that is what the fallback is
    // for), but a card that quietly stopped matching its tool should still be findable.
    console.debug("session-search: " + (label || "tool") + " renderer declined this shape", doc.tool_input);
    return null;
  } catch (err) {
    console.debug("session-search: " + (label || "tool") + " renderer failed", err);
    return null;
  }
}

// --- shape guards -----------------------------------------------------------------------
//
// Each returns the value it recognises or `null`, so a renderer's first lines read as the
// contract it needs. `str` admits the empty string — `Write` with `content: ""` creates an
// empty file, and treating that as a missing parameter would drop a real call.

function asDoc(doc) {
  return doc && typeof doc === "object" ? doc : {};
}

function toolName(doc) {
  return typeof doc.tool_name === "string" ? doc.tool_name : "";
}

function obj(v) {
  return v && typeof v === "object" && !Array.isArray(v) ? v : null;
}

function str(v) {
  return typeof v === "string" ? v : null;
}

function nonEmpty(v) {
  return typeof v === "string" && v.trim() ? v : null;
}

function num(v) {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  // Transcripts carry numeric parameters as strings often enough to be worth accepting one.
  if (typeof v === "string" && v.trim() && Number.isFinite(Number(v))) return Number(v);
  return null;
}

function firstStr(o, ...keys) {
  for (const key of keys) {
    const s = str(o[key]);
    if (s !== null) return s;
  }
  return null;
}

function typeName(v) {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  return typeof v;
}

function splitMcp(name) {
  if (!name.startsWith("mcp__")) return null;
  const parts = name.slice(5).split("__");
  const server = parts.shift() || "";
  if (!server) return null;
  return { server, tool: parts.join("__") || name };
}

// --- shared pieces ----------------------------------------------------------------------

function body(...children) {
  return el("div", { class: "ss-card-body" }, ...children);
}

function hint(text) {
  return el("div", { class: "ss-hint", text });
}

function caption(v) {
  const s = nonEmpty(v);
  return s === null ? null : el("div", { class: "ss-meta", text: s });
}

function pathNode(path) {
  const dir = dirname(path);
  const base = basename(path);
  return el(
    "span",
    { class: "ss-path", title: path },
    dir ? el("span", { class: "ss-path-dir", text: dir }) : null,
    el("span", { class: "ss-path-base", text: base || path }),
  );
}

function chip(label, value) {
  const text = value === undefined ? String(label) : label + ": " + scalar(value);
  return el("span", { class: "ss-chip", text });
}

/** The chips a renderer names, skipping the ones this call did not set. */
function chips(o, keys) {
  const out = keys.filter((k) => o[k] !== undefined && o[k] !== null).map((k) => chip(k, o[k]));
  return out.length ? el("div", { class: "ss-meta" }, out) : null;
}

function scalar(v) {
  if (typeof v === "string") return clampText(v, 80);
  if (Array.isArray(v)) return v.length + " items";
  if (v && typeof v === "object") return Object.keys(v).length + " keys";
  return String(v);
}

/**
 * Whatever a specialised renderer did not consume, as a table.
 *
 * Without this, a parameter the tool grew after this file was written would be visible in the
 * CLI and invisible here, which is the worst way to lose it: the card looks complete.
 */
function rest(o, handled) {
  const keys = Object.keys(o).filter((k) => !handled.includes(k));
  return keys.length ? kvTable(o, keys) : null;
}

/** A `<pre>` that collapses past `maxLines`, with a button that puts the rest back. */
function collapsible(text, className, lang, maxLines) {
  const lines = String(text).split("\n");
  const props = { class: className, dataset: lang ? { lang } : null };
  if (lines.length <= maxLines) return el("pre", props, el("code", { text: String(text) }));

  const code = el("code", { text: lines.slice(0, maxLines).join("\n") });
  const pre = el("pre", props, code);
  let open = false;
  const more = el("button", {
    class: "ss-more",
    type: "button",
    text: "show all " + lines.length + " lines",
    on: {
      click: () => {
        open = !open;
        code.textContent = open ? String(text) : lines.slice(0, maxLines).join("\n");
        more.textContent = open ? "show less" : "show all " + lines.length + " lines";
        // The height cap is CSS, not content: swapping the text alone leaves the block the
        // same clipped size with a longer inner scrollbar, so the button appears to do
        // nothing. `[data-expanded]` is what lifts it.
        if (open) pre.dataset.expanded = "";
        else delete pre.dataset.expanded;
      },
    },
  });
  return frag(pre, more);
}

function jsonBlock(value) {
  let text;
  try {
    text = JSON.stringify(value, null, 2);
  } catch (_) {
    text = null;
  }
  // `undefined` from `JSON.stringify` is not an error case worth hiding: say the value exists
  // and could not be shown, rather than rendering an empty cell that reads as "no value".
  if (typeof text !== "string") return hint("a " + typeName(value) + " that cannot be shown as JSON");
  return collapsible(text, "ss-json", null, 8);
}

function valueNode(value) {
  if (typeof value === "string") {
    // No `data-lang` here: the generic table has a key and a value, never the path that would
    // say what language the value is in, and guessing from the text itself would be a coin toss.
    if (value.includes("\n") || value.length > 160) return collapsible(value, "ss-code", null, 12);
    return el("span", { text: value });
  }
  if (value === null || typeof value !== "object") return el("span", { text: String(value) });
  return jsonBlock(value);
}

// The identifying parameters, in the order `format.rs`'s `KEY_PARAMS` lists them, so a card and
// a terminal line put the same thing first. Everything else follows in the order the transcript
// recorded it.
const KEY_PARAMS = [
  "command",
  "file_path",
  "path",
  "pattern",
  "query",
  "url",
  "notebook_path",
  "prompt",
  "description",
  "subagent_type",
  "old_string",
  "content",
];

function orderedKeys(o) {
  const own = Object.keys(o);
  const lead = KEY_PARAMS.filter((k) => own.includes(k));
  return lead.concat(own.filter((k) => !lead.includes(k)));
}

function kvTable(o, keys) {
  return el(
    "dl",
    { class: "ss-kv" },
    (keys || orderedKeys(o)).map((k) =>
      frag(el("dt", { class: "ss-kv-k", text: k }), el("dd", { class: "ss-kv-v" }, valueNode(o[k]))),
    ),
  );
}

/** The fallback every other renderer falls back to. */
function genericCall(input) {
  if (input === undefined || input === null) return body(hint("this call recorded no parameters"));
  const o = obj(input);
  if (!o) {
    // Naming what was actually there is the whole job: an empty card would look like a tool
    // that takes no arguments, and this one took an argument of the wrong kind.
    return body(
      hint("tool_input is a " + typeName(input) + ", not an object of parameters; showing it as recorded"),
      valueNode(input),
    );
  }
  const keys = orderedKeys(o);
  return body(keys.length ? kvTable(o, keys) : hint("this call recorded an empty parameter object"));
}

function mcpCall(input, doc) {
  const mcp = splitMcp(toolName(doc));
  if (!mcp) return null;
  return body(
    el(
      "div",
      { class: "ss-meta" },
      el("span", { class: "ss-chip", text: "server: " + mcp.server }),
      el("span", { class: "ss-chip", text: "tool: " + mcp.tool }),
    ),
    genericCall(input),
  );
}

// --- call renderers ---------------------------------------------------------------------

function bashCall(input) {
  const o = obj(input);
  if (!o) return null;
  const command = str(o.command);
  if (command === null) return null;
  return body(
    caption(o.description),
    el(
      "pre",
      { class: "ss-term" },
      el("code", { class: "ss-term-cmd" }, el("span", { class: "ss-hint", text: "$ " }), command),
    ),
    chips(o, ["timeout", "run_in_background", "sandbox"]),
    rest(o, ["command", "description", "timeout", "run_in_background", "sandbox"]),
  );
}

function readCall(input) {
  const o = obj(input);
  if (!o) return null;
  const path = firstStr(o, "file_path", "path", "notebook_path");
  if (path === null) return null;
  const offset = num(o.offset);
  const limit = num(o.limit);
  let range = null;
  if (offset !== null && limit !== null) range = chip("lines", offset + "–" + (offset + limit - 1));
  else if (offset !== null) range = chip("from line", offset);
  else if (limit !== null) range = chip("limit", limit);
  return body(
    el("div", { class: "ss-meta" }, iconFor("file"), pathNode(path), range),
    rest(o, ["file_path", "path", "notebook_path", "offset", "limit"]),
  );
}

function writeCall(input) {
  const o = obj(input);
  if (!o) return null;
  const path = firstStr(o, "file_path", "path");
  const content = str(o.content);
  if (path === null || content === null) return null;
  return body(
    el("div", { class: "ss-meta" }, iconFor("file"), pathNode(path)),
    collapsible(content, "ss-code", langFor(path), 20),
    rest(o, ["file_path", "path", "content"]),
  );
}

function notebookCall(input) {
  const o = obj(input);
  if (!o) return null;
  const path = firstStr(o, "notebook_path", "file_path", "path");
  const source = firstStr(o, "new_source", "source", "content");
  if (path === null || source === null) return null;
  // A notebook's language lives in the cell, not the `.ipynb` extension, so `langFor` has
  // nothing to work with here.
  const lang = str(o.cell_type) === "markdown" ? "markdown" : "python";
  return body(
    el("div", { class: "ss-meta" }, iconFor("book"), pathNode(path)),
    chips(o, ["cell_id", "cell_type", "edit_mode"]),
    collapsible(source, "ss-code", lang, 20),
    rest(o, ["notebook_path", "file_path", "path", "new_source", "source", "content", "cell_id", "cell_type", "edit_mode"]),
  );
}

function editCall(input) {
  const o = obj(input);
  if (!o) return null;
  const before = str(o.old_string);
  const after = str(o.new_string);
  if (before === null && after === null) return null;
  const path = firstStr(o, "file_path", "path");
  return body(
    path === null ? null : el("div", { class: "ss-meta" }, iconFor("edit"), pathNode(path)),
    chips(o, ["replace_all"]),
    diffNode(before ?? "", after ?? ""),
    rest(o, ["file_path", "path", "old_string", "new_string", "replace_all"]),
  );
}

// `edits` is as uncapped as everything else in `tool_input`, and each entry carries a diff of
// its own, so the per-diff budget alone still multiplies. The rest are one click away.
const MAX_EDITS = 12;

function multiEditCall(input) {
  const o = obj(input);
  if (!o) return null;
  if (!Array.isArray(o.edits)) return null;
  const path = firstStr(o, "file_path", "path");
  const one = (edit, i) => {
    const head = el("div", { class: "ss-meta", text: "edit " + (i + 1) + " of " + o.edits.length });
    const e = obj(edit);
    if (!e) {
      // A malformed entry is part of the record. Skipping it would renumber the ones after it
      // and quietly disagree with the transcript about how many edits this call made.
      return frag(head, hint("this entry is a " + typeName(edit) + ", not an {old_string, new_string} object"), valueNode(edit));
    }
    return frag(head, diffNode(str(e.old_string) ?? "", str(e.new_string) ?? ""), rest(e, ["old_string", "new_string", "replace_all"]));
  };
  const drawn = el("div", {}, o.edits.slice(0, MAX_EDITS).map(one));
  const hidden = o.edits.length - MAX_EDITS;
  const more =
    hidden <= 0
      ? null
      : el("button", {
          class: "ss-more",
          type: "button",
          text: "show the remaining " + hidden + " edits",
          on: {
            click: () => {
              o.edits.slice(MAX_EDITS).forEach((edit, i) => drawn.appendChild(one(edit, MAX_EDITS + i)));
              more.remove();
            },
          },
        });
  return body(
    path === null ? null : el("div", { class: "ss-meta" }, iconFor("edit"), pathNode(path)),
    drawn,
    more,
    rest(o, ["file_path", "path", "edits"]),
  );
}

function globCall(input) {
  const o = obj(input);
  if (!o) return null;
  const pattern = str(o.pattern);
  if (pattern === null) return null;
  return body(
    el("pre", { class: "ss-code", dataset: { lang: "glob" } }, el("code", { text: pattern })),
    chips(o, ["path", "glob", "type", "head_limit"]),
    rest(o, ["pattern", "path", "glob", "type", "head_limit"]),
  );
}

const GREP_OPTS = [
  "path", "glob", "type", "output_mode", "head_limit", "multiline",
  "-i", "-n", "-A", "-B", "-C", "offset",
];

function grepCall(input) {
  const o = obj(input);
  if (!o) return null;
  const pattern = str(o.pattern);
  if (pattern === null) return null;
  return body(
    el("pre", { class: "ss-code", dataset: { lang: "regex" } }, el("code", { text: pattern })),
    chips(o, GREP_OPTS),
    rest(o, ["pattern"].concat(GREP_OPTS)),
  );
}

function taskCall(input, doc) {
  const o = obj(input);
  if (!o) return null;
  const prompt = str(o.prompt);
  const description = nonEmpty(o.description);
  const subagent = nonEmpty(o.subagent_type) || nonEmpty(doc.agent_type);
  if (prompt === null && description === null && subagent === null) return null;
  return body(
    el(
      "div",
      { class: "ss-meta" },
      subagent ? el("span", { class: "ss-badge ss-badge-tool", text: subagent }) : null,
      description ? el("span", { text: description }) : null,
      sidechainLink(doc),
    ),
    prompt === null ? null : el("div", { class: "ss-snippet" }, renderMarkdown(prompt)),
    rest(o, ["prompt", "description", "subagent_type"]),
  );
}

/**
 * The hook `app.js` binds to open the subagent's own transcript.
 *
 * A button, not a link: only `app.js` knows the route, and this file fetches nothing. The ids
 * ride along in `dataset` so the handler does not have to search for the document again.
 */
function sidechainLink(doc) {
  const agent = nonEmpty(doc.agent_id);
  const session = nonEmpty(doc.session_id);
  // Both ids or no button: the route is `/api/sessions/{session_id}?agent=`, so a button
  // missing either one renders as a live control and then cannot act.
  if (!agent || !session) return null;
  return el(
    "button",
    {
      class: "ss-chip",
      type: "button",
      dataset: { agent, session },
    },
    iconFor("robot"),
    "sidechain transcript",
  );
}

const TODO_CLASS = { completed: "ss-todo-done", in_progress: "ss-todo-active" };
const TODO_ICON = { completed: "check", in_progress: "clock" };

function todoCall(input) {
  const o = obj(input);
  if (!o || !Array.isArray(o.todos)) return null;
  const items = o.todos.map((todo) => {
    const t = obj(todo);
    if (!t) return el("li", {}, valueNode(todo));
    const status = str(t.status) ?? "";
    const text = firstStr(t, "content", "activeForm", "task", "title");
    const known = Object.prototype.hasOwnProperty.call(TODO_CLASS, status);
    return el(
      "li",
      { class: TODO_CLASS[status] || null, dataset: { status: status || "unset" } },
      iconFor(TODO_ICON[status] || "list"),
      text === null ? valueNode(todo) : el("span", { text }),
      // An unrecognised status renders as itself. Folding it into "pending" would make a
      // corpus from a future build look like it had no in-flight work.
      known || !status ? null : el("span", { class: "ss-chip", text: status }),
    );
  });
  return body(el("ul", { class: "ss-todo" }, items), rest(o, ["todos"]));
}

function urlCall(input) {
  const o = obj(input);
  if (!o) return null;
  const url = firstStr(o, "url", "query");
  if (url === null) return null;
  const href = str(o.url) === null ? null : safeHref(o.url);
  const isUrl = str(o.url) !== null;
  return body(
    el(
      "div",
      { class: "ss-meta" },
      iconFor(isUrl ? "globe" : "search"),
      href
        ? el("a", { href, rel: "noopener noreferrer", target: "_blank", text: url })
        : el("span", { text: url }),
      // Anything that is not an absolute http(s) URL stays text, and says why, so a `file://`
      // or a template placeholder is visible rather than mistaken for a dead link.
      isUrl && !href ? hint("not an http or https URL, so it is not a link") : null,
    ),
    caption(o.prompt),
    chips(o, ["allowed_domains", "blocked_domains"]),
    rest(o, ["url", "query", "prompt", "allowed_domains", "blocked_domains"]),
  );
}

const CALL = {
  Bash: bashCall,
  Read: readCall,
  Write: writeCall,
  NotebookEdit: notebookCall,
  Edit: editCall,
  MultiEdit: multiEditCall,
  Glob: globCall,
  Grep: grepCall,
  Task: taskCall,
  Agent: taskCall,
  TodoWrite: todoCall,
  WebFetch: urlCall,
  WebSearch: urlCall,
};

// --- the diff ---------------------------------------------------------------------------

// The LCS table is one cell per pair of lines. A 3,000-line `old_string` against a 3,000-line
// `new_string` is nine million cells and a frozen tab, so past this budget the two sides are
// shown whole: less useful than a diff, but still the truth and still instant.
const DIFF_CELLS = 250000;
// A long run of untouched lines is the part nobody reads; the count stands in for the middle.
const DIFF_CONTEXT = 3;
// Elements one diff may put on the page before the rest goes behind a button. `tool_input` is
// indexed uncapped (`parse.rs`), so a 20,000-line `old_string` arrives whole and one `<div>`
// per line is 40,000 elements — inside `renderTurn`, which the drawer calls for up to 400
// turns. A collapsed run costs its own handful of elements, so the budget counts what is
// appended rather than what is diffed.
const DIFF_ROWS = 400;
// Lines per side once the LCS is out of budget. That branch emits every line of both sides
// with no `same` runs to collapse, so it is the one place nothing else bounds.
const DIFF_MAX_LINES = 2000;

function diffNode(before, after) {
  const rows = diffLines(before, after);
  const out = el("div", { class: "ss-diff" });
  let at = drawDiff(out, rows, 0, DIFF_ROWS);
  if (at >= rows.length) return out;
  const more = el("button", {
    class: "ss-more",
    type: "button",
    text: "show the remaining " + (rows.length - at) + " diff lines",
    on: {
      click: () => {
        at = drawDiff(out, rows, at, Infinity);
        // As in `collapsible`: the rest of the rows exist now, but `.ss-diff` is height-capped
        // in CSS, so without this the button just lengthens a scrollbar.
        out.dataset.expanded = "";
        more.remove();
      },
    },
  });
  return frag(out, more);
}

/**
 * Append rows to `out` until `budget` elements have been added; return where it stopped.
 *
 * The budget is checked only after a changed line, which is also the only point at which no
 * run of unchanged lines is pending — so the caller can resume at the returned index without
 * the collapsed middle of a run being split across the two passes and counted twice.
 */
function drawDiff(out, rows, from, budget) {
  let run = [];
  let spent = 0;
  const put = (node) => {
    out.appendChild(node);
    spent += 1;
  };
  const flush = () => {
    if (!run.length) return;
    if (run.length <= DIFF_CONTEXT * 2 + 1) {
      run.forEach((line) => put(el("div", { text: line })));
    } else {
      run.slice(0, DIFF_CONTEXT).forEach((line) => put(el("div", { text: line })));
      put(hint("… " + (run.length - DIFF_CONTEXT * 2) + " unchanged lines"));
      run.slice(-DIFF_CONTEXT).forEach((line) => put(el("div", { text: line })));
    }
    run = [];
  };
  let i = from;
  for (; i < rows.length; i += 1) {
    const [kind, line] = rows[i];
    if (kind === "same") {
      run.push("  " + line);
      continue;
    }
    flush();
    if (kind === "note") {
      put(hint(line));
    } else {
      put(el("div", { class: kind === "add" ? "ss-diff-add" : "ss-diff-del", text: (kind === "add" ? "+ " : "- ") + line }));
    }
    if (spent >= budget) {
      i += 1;
      break;
    }
  }
  flush();
  return i;
}

function diffLines(before, after) {
  const a = String(before).split("\n");
  const b = String(after).split("\n");
  if (a.length * b.length > DIFF_CELLS) {
    // Whole, but not unbounded: each side is clamped and says how much it left out, because
    // "show them both" over a 1 MiB string is the largest thing this file can be asked to
    // build and the only branch with no `same` runs to collapse.
    const rows = [];
    for (const [lines, kind] of [[a, "del"], [b, "add"]]) {
      lines.slice(0, DIFF_MAX_LINES).forEach((l) => rows.push([kind, l]));
      if (lines.length > DIFF_MAX_LINES) {
        rows.push(["note", "… " + (lines.length - DIFF_MAX_LINES) + " more lines, too large to diff"]);
      }
    }
    return rows;
  }
  const width = b.length + 1;
  const table = new Int32Array((a.length + 1) * width);
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      table[i * width + j] =
        a[i] === b[j]
          ? table[(i + 1) * width + j + 1] + 1
          : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    }
  }
  const rows = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      rows.push(["same", a[i]]);
      i += 1;
      j += 1;
    } else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) {
      rows.push(["del", a[i]]);
      i += 1;
    } else {
      rows.push(["add", b[j]]);
      j += 1;
    }
  }
  while (i < a.length) rows.push(["del", a[i++]]);
  while (j < b.length) rows.push(["add", b[j++]]);
  return rows;
}

// --- result renderers -------------------------------------------------------------------

const MAX_RESULT_LINES = 24;
// Past this, a result is machine output that happens to contain asterisks, and turning tens of
// thousands of characters into markdown nodes costs more than the formatting is worth.
const MAX_MARKDOWN_CHARS = 20000;

function errorHead(out) {
  const first = String(out).split("\n").find((line) => line.trim()) || "";
  return el(
    "div",
    { class: "ss-err" },
    iconFor("warn"),
    el("span", { text: first ? clampText(first, 200) : "this call failed and recorded no output" }),
  );
}

function genericResult(input, doc, out) {
  return collapsible(out, "ss-out", null, MAX_RESULT_LINES);
}

function bashResult(input, doc, out) {
  return collapsible(out, ["ss-term", "ss-out"], null, MAX_RESULT_LINES);
}

// `     1→fn main() {`: `Read` numbers its output, and those numbers are what a reviewer cites.
// They belong in a gutter of their own — left in the text they get copied along with the code
// and break every paste.
const NUMBERED = /^\s*(\d+)[\t→](.*)$/;

function readResult(input, doc, out) {
  const lines = String(out).split("\n");
  const parsed = lines.map((line) => NUMBERED.exec(line));
  const hits = parsed.filter(Boolean).length;
  const filled = lines.filter((line) => line.trim()).length;
  // A `Read` of a file that itself starts every line with a number would parse too, and that
  // is fine — the gutter would still hold what the transcript put there. What must not happen
  // is a stray numbered line turning an ordinary result into a table of one.
  if (hits < 2 || hits < filled * 0.6) return null;
  const lang = langFor(firstStr(obj(input) || {}, "file_path", "path", "notebook_path") ?? "");
  const code = el("code", {});
  lines.forEach((line, idx) => {
    const m = parsed[idx];
    if (idx) code.appendChild(document.createTextNode("\n"));
    if (!m) {
      code.appendChild(document.createTextNode(line));
      return;
    }
    code.appendChild(el("span", { class: "ss-count", text: m[1] }));
    code.appendChild(document.createTextNode(m[2]));
  });
  return el("pre", { class: "ss-code", dataset: lang ? { lang } : null }, code);
}

// `src/search.rs:118:    let snippet = ...` — `Grep -n` and friends put the location first, and
// splitting it out is what makes a result list scannable.
const FILE_LINE = /^([^\s:][^:]*):(\d+):(.*)$/;

// `tool_output` reaches the frontend at up to `max_text_bytes` (1 MiB), so a routine `grep -rn`
// is tens of thousands of lines. The cap is applied to the *lines*, before any node is built:
// mapping first and slicing after spends the whole cost on rows that are thrown away.
const MAX_HIT_ROWS = 200;

function hitListResult(input, doc, out) {
  const lines = String(out).split("\n");
  if (!lines.some((line) => FILE_LINE.test(line) || looksLikePath(line))) return null;
  const rows = lines.slice(0, MAX_HIT_ROWS).map((line) => {
    const m = FILE_LINE.exec(line);
    if (m) {
      return el(
        "div",
        {},
        pathNode(m[1]),
        el("span", { class: "ss-count", text: m[2] }),
        el("span", { text: m[3] }),
      );
    }
    if (looksLikePath(line)) return el("div", {}, pathNode(line.trim()));
    // Counts, "No files found", and the rest of what these tools print stay as they were.
    return el("div", { class: "ss-hint", text: line });
  });
  return el(
    "div",
    { class: "ss-out" },
    rows,
    lines.length > MAX_HIT_ROWS ? hint("… " + (lines.length - MAX_HIT_ROWS) + " more lines") : null,
  );
}

function looksLikePath(line) {
  const s = line.trim();
  return Boolean(s) && !/\s/.test(s) && (s.startsWith("/") || s.startsWith("./") || s.includes("/"));
}

function proseResult(input, doc, out) {
  if (out.length > MAX_MARKDOWN_CHARS) return null;
  return el("div", { class: "ss-snippet" }, renderMarkdown(out));
}

const RESULT = {
  Bash: bashResult,
  Read: readResult,
  Glob: hitListResult,
  Grep: hitListResult,
  Task: proseResult,
  Agent: proseResult,
  WebFetch: proseResult,
  WebSearch: proseResult,
};
