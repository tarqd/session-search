// The application: state, the API client, facets, results, expansion, the drawer, routing.
//
// Three rules shape everything below.
//
// * **One endpoint.** Every search goes through `POST /api/search` with a Search UI request
//   body — the same call a third-party connector would make. There is deliberately no private
//   side door, because a second path would drift from the documented one and nobody would
//   notice until an outside connector broke.
// * **Nothing fails quietly.** A fetch that never lands, a 400 that names a bad filter, an
//   empty index and an empty result set are four different situations and each one says which
//   it is. A caller's mistake that looked like "no matches" would send them hunting through
//   their transcripts for something that was never searched for.
// * **One string is HTML.** `text.snippet` (or `tool_output.snippet`, or `thinking.snippet`)
//   arrives escaped-then-marked-up from the server and is the only value handed to `el`'s
//   `html` prop. Everything else — text a model wrote, a path, a tool parameter — goes through
//   `text`, `renderMarkdown` or the renderers in `tools.js`. The block holding a server
//   snippet carries `data-marked`, which is how `highlight.js` knows to leave it alone.

import {
  basename,
  clampText,
  copyText,
  debounce,
  dirname,
  el,
  fmtBytes,
  fmtClock,
  fmtCount,
  fmtTime,
  frag,
  iconFor,
  relTime,
} from "./dom.js";
import { countMarks, isEmptyPlan, markMatches, matchRanges, parseQuery } from "./highlight.js";
import { renderMarkdown } from "./markdown.js";
import { renderToolCall, renderToolResult, toolMeta, toolSummary } from "./tools.js";

// ─── Constants ────────────────────────────────────────────────────────────────────────────

/**
 * The facets asked for on every search, and the filter each one toggles.
 *
 * `multi` is not decoration: `Filters::tool` is a list that ORs, and every other field here is
 * a single `Option<String>` that a second value would be a 400 for. A rail that let you click
 * two projects and then dropped one silently would be lying about what was searched.
 */
const FACETS = [
  { field: "tool_name", filter: "tool", label: "tool", multi: true },
  { field: "project", filter: "project", label: "project", multi: false },
  { field: "model", filter: "model", label: "model", multi: false },
  { field: "role", filter: "role", label: "role", multi: false },
  { field: "kind", filter: "kind", label: "kind", multi: false },
  { field: "agent_type", filter: "agent_type", label: "agent type", multi: false },
  { field: "git_branch", filter: "branch", label: "git branch", multi: false },
];

/** How many values each facet asks for. Beyond this the honesty line says how many are hidden. */
const FACET_TOP = 15;

/** Typing pause before a search leaves. Long enough not to search every keystroke of "cargo". */
const TYPE_DEBOUNCE_MS = 180;

/** The first expansion window, and how much each further press adds on either side. */
const WINDOW_START = 3;
const WINDOW_STEP = 10;

/** Turns the drawer asks for. `truncated` in the response says when a session ran past it. */
const SESSION_LIMIT = 400;

const SORTS = ["relevance", "newest", "oldest"];

/** The result units. `turn` is one card per request; `none` is one per document. */
const GROUPS = ["turn", "none"];

// ─── Element handles ──────────────────────────────────────────────────────────────────────

const byId = (id) => document.getElementById(id);

const qEl = byId("q");
const sortEl = byId("sort");
const groupEl = byId("group");
const refreshEl = byId("refresh");
const statsEl = byId("stats");
const facetsEl = byId("facets");
const chipsEl = byId("chips");
const resultsEl = byId("results");
const summaryEl = byId("summary");
const pagerEl = byId("pager");
const drawerEl = byId("drawer");
const drawerBodyEl = byId("drawer-body");
const drawerTitleEl = byId("drawer-title");
const drawerCloseEl = byId("drawer-close");
const drawerNavEl = byId("drawer-nav");
const emptyEl = byId("empty");
const errorEl = byId("error");
const themeEl = byId("theme");
const loadingEl = byId("loading");

/** Warnings, caveats and the "relevance means nothing here" note, above the result list. */
const notesEl = el("div", { class: "ss-notes" });
resultsEl.parentNode.insertBefore(notesEl, resultsEl);

// ─── State ────────────────────────────────────────────────────────────────────────────────

/**
 * Everything the address bar round-trips. `filters` mirrors `search::Filters` by name so the
 * request body is this object with the empty keys dropped — one shape, so a filter cannot be
 * spelled one way in the UI and another on the wire.
 *
 * `sidechain` is the exception: three mutually exclusive states that the API spells as two
 * booleans, because `no_sidechains` and `sidechains_only` together are a contradiction the CLI
 * refuses outright.
 */
const state = {
  q: "",
  page: 1,
  size: 20,
  sort: "relevance",
  /// The result unit. Grouped by default: a transcript answers a question many times over,
  /// and a flat list makes the reader do the grouping by eye from the session ids.
  group: "turn",
  includeThinking: false,
  filters: {
    tool: [],
    tool_input: [],
    // One turn, as the pair the index keys turns by. `turn_seq` is a per-file ordinal and two
    // transcripts can share a session id, so the path travels with it or neither does — the
    // server refuses half of the pair rather than answering a wider search than was asked.
    turn_of: "",
    turn_seq: "",
    session: "",
    project: "",
    model: "",
    role: "",
    kind: "",
    agent_type: "",
    branch: "",
    since: "",
    until: "",
    errors_only: false,
    // The default scope is the conversation: attachments, `system` records and meta turns are
    // in the index but out of the way. `renderNotes` says how many that hid, every time.
    all_records: false,
    sidechain: "any", // any | exclude | only
  },
};

/** `/api/stats`, fetched once at boot: the difference between "no matches" and "no index". */
let indexStats = null;

/** Which result card `j`/`k` are on. -1 is "none", which is where Escape puts it. */
let selected = -1;

/** Facet sections the reader collapsed. Kept out of the URL: it is a view preference, not a query. */
const collapsedFacets = new Set();

// ─── URL state ────────────────────────────────────────────────────────────────────────────
//
// A search that cannot be pasted into a chat window is half a tool. Typing calls
// `history.replaceState` so the back button does not walk back through every keystroke;
// anything that reads as a navigation — a facet click, a page, a sort — pushes.

function stateToParams() {
  const p = new URLSearchParams();
  if (state.q) p.set("q", state.q);
  if (state.page > 1) p.set("page", String(state.page));
  if (state.sort !== "relevance") p.set("sort", state.sort);
  if (state.group !== "turn") p.set("group", state.group);
  if (state.size !== 20) p.set("size", String(state.size));
  for (const tool of state.filters.tool) p.append("tool", tool);
  for (const ti of state.filters.tool_input) p.append("tool_input", ti);
  for (const key of ["turn_of", "turn_seq", "session", "project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    if (state.filters[key]) p.set(key, state.filters[key]);
  }
  if (state.filters.errors_only) p.set("errors_only", "1");
  if (state.filters.all_records) p.set("all_records", "1");
  if (state.filters.sidechain !== "any") p.set("sidechain", state.filters.sidechain);
  if (state.includeThinking) p.set("include_thinking", "1");
  return p;
}

function applyParams(p) {
  state.q = p.get("q") || "";
  state.page = Math.max(1, Number.parseInt(p.get("page") || "1", 10) || 1);
  state.size = Math.max(1, Number.parseInt(p.get("size") || "20", 10) || 20);
  // An unknown `sort=` in a hand-edited URL falls back rather than travelling to the server,
  // which would answer the whole search with a 400 about a control the reader cannot see.
  const sort = p.get("sort") || "relevance";
  state.sort = SORTS.includes(sort) ? sort : "relevance";
  const group = p.get("group") || "turn";
  state.group = GROUPS.includes(group) ? group : "turn";
  state.includeThinking = p.get("include_thinking") === "1";
  state.filters.tool = p.getAll("tool").filter(Boolean);
  state.filters.tool_input = p.getAll("tool_input").filter(Boolean);
  for (const key of ["turn_of", "turn_seq", "session", "project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    state.filters[key] = p.get(key) || "";
  }
  state.filters.errors_only = p.get("errors_only") === "1";
  state.filters.all_records = p.get("all_records") === "1";
  const side = p.get("sidechain") || "any";
  state.filters.sidechain = ["any", "exclude", "only"].includes(side) ? side : "any";
}

function writeUrl(push) {
  const query = stateToParams().toString();
  const url = query ? `${location.pathname}?${query}` : location.pathname;
  if (push) history.pushState(null, "", url);
  else history.replaceState(null, "", url);
}

// ─── The API client ───────────────────────────────────────────────────────────────────────

/**
 * A failure with a message worth showing. `status` is null when the request never reached the
 * server at all, which is a different thing to explain than a 400 and gets a different line.
 */
class ApiFailure extends Error {
  constructor(message, status) {
    super(message);
    this.name = "ApiFailure";
    this.status = status ?? null;
  }
}

/** Thrown when a request was superseded. Callers drop it; it is not a failure to report. */
const SUPERSEDED = Symbol("superseded");

async function api(path, opts) {
  let res;
  try {
    res = await fetch(path, opts);
  } catch (err) {
    if (err && err.name === "AbortError") throw SUPERSEDED;
    // `fetch` rejects with a deliberately vague TypeError for every network-layer failure, so
    // the useful half of this message is ours: the server this page came from has stopped
    // answering, which on a localhost tool almost always means it was Ctrl-C'd.
    throw new ApiFailure(
      `cannot reach ${location.origin} — the \`session-search serve\` process this page came from is not answering (${err && err.message ? err.message : "network error"}).`,
      null,
    );
  }

  const body = await res.text();
  let data = null;
  try {
    data = body ? JSON.parse(body) : null;
  } catch (_) {
    data = null;
  }

  if (!res.ok) {
    // Every failure the server raises is `{"error":{status,message}}` and the message is
    // written to be read — it names the field that was wrong and what would have been
    // accepted. Replacing it with "request failed" throws away the only useful thing here.
    const message =
      data && data.error && typeof data.error.message === "string"
        ? data.error.message
        : `the server answered ${res.status} ${res.statusText || ""}`.trim();
    throw new ApiFailure(message, res.status);
  }
  if (data === null) {
    throw new ApiFailure(
      `${path} answered ${res.status} with something that is not JSON; is another server on this port?`,
      res.status,
    );
  }
  return data;
}

/** The Search UI request body for the current state. */
function searchBody() {
  const f = state.filters;
  const filters = {};
  if (f.tool.length) filters.tool = f.tool;
  if (f.tool_input.length) filters.tool_input = f.tool_input;
  for (const key of ["turn_of", "turn_seq", "session", "project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    if (f[key]) filters[key] = f[key];
  }
  if (f.errors_only) filters.errors_only = true;
  if (f.all_records) filters.all_records = true;
  if (f.sidechain === "exclude") filters.no_sidechains = true;
  if (f.sidechain === "only") filters.sidechains_only = true;

  const facets = {};
  for (const facet of FACETS) facets[facet.field] = { type: "value", size: FACET_TOP };

  return {
    searchTerm: state.q,
    current: state.page,
    resultsPerPage: state.size,
    filters,
    facets,
    // An empty list is how this request says "relevance"; `dto.rs` reads `Some([])` as the
    // default order, and omitting the key entirely would mean the same thing less explicitly.
    sortList:
      state.sort === "relevance"
        ? []
        : [{ field: "timestamp", direction: state.sort === "newest" ? "desc" : "asc" }],
    includeThinking: state.includeThinking,
    // Omitted rather than sent as false: absent is the flat shape a stock Search UI
    // connector expects, and saying so explicitly would only be a longer way to agree.
    ...(state.group === "turn" ? { groupByTurn: true } : {}),
  };
}

// ─── Searching ────────────────────────────────────────────────────────────────────────────

let searchGeneration = 0;
let inFlight = null;

/**
 * Run the current state as a search.
 *
 * Search-as-you-type means several requests can be open at once, and they do not come back in
 * the order they left: a broad "c" can outrun the narrow "cargo test" typed after it and
 * repaint the list with results for a query nobody is looking at any more. The generation
 * counter is the fix — a response whose generation is stale is dropped without touching the
 * DOM — and the `AbortController` is the courtesy, freeing the connection early.
 */
async function runSearch() {
  const generation = ++searchGeneration;
  if (inFlight) inFlight.abort();
  const controller = new AbortController();
  inFlight = controller;

  setLoading(true);
  try {
    const data = await api("/api/search", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(searchBody()),
      signal: controller.signal,
    });
    if (generation !== searchGeneration) return;
    clearError();
    render(data);
  } catch (err) {
    if (err === SUPERSEDED || generation !== searchGeneration) return;
    showFailure(err);
  } finally {
    if (generation === searchGeneration) {
      inFlight = null;
      setLoading(false);
    }
  }
}

/** Re-run from page 1. Every control except the pager lands here: a filter change invalidates
 *  the page number, and staying on page 7 of a result set that now has two pages shows nothing. */
function search({ push = false, resetPage = true } = {}) {
  if (resetPage) state.page = 1;
  writeUrl(push);
  selected = -1;
  runSearch();
}

const searchSoon = debounce(() => search({ push: false }), TYPE_DEBOUNCE_MS);

// ─── Rendering: the whole response ────────────────────────────────────────────────────────

function render(data) {
  // Before anything is drawn: every turn rendered from here on — in a card now, in a thread
  // or a drawer minutes later — marks the words of the search that produced them.
  markPlan = parseQuery(state.q);
  renderNotes(data);
  renderChips();
  renderFacets(data.facets || {});
  renderResults(data);
  renderPager(data);
  renderSummary(data);
}

function renderSummary(data) {
  const total = Number(data.totalResults) || 0;
  const from = Number(data.pagingStart) || 0;
  const to = Number(data.pagingEnd) || 0;
  const info = data.info || {};
  const elapsed = Number.isFinite(info.elapsedMs) ? `${info.elapsedMs} ms` : "";
  if (total === 0) {
    summaryEl.replaceChildren();
    return;
  }
  // `totalResults` counts whatever the server grouped by, and the two totals are different
  // questions: "how much of my work touched this" is requests, "how many times" is documents.
  // Naming both is the only way neither gets mistaken for the other.
  const grouped = info.grouped === true;
  const docs = Number(info.totalDocuments);
  summaryEl.replaceChildren(
    el("strong", { text: fmtCount(total) }),
    grouped
      ? ` ${total === 1 ? "turn" : "turns"} matched`
      : ` ${total === 1 ? "document" : "documents"} matched`,
    grouped && Number.isFinite(docs)
      ? ` · ${fmtCount(docs)} ${docs === 1 ? "document" : "documents"} in them`
      : "",
    // The total is the whole matching set, not the page: `pagingStart`/`pagingEnd` say which
    // slice is on screen, and reporting only the slice would understate the corpus by 20x.
    from ? ` · showing ${fmtCount(from)}–${fmtCount(to)}` : "",
    elapsed ? ` · ${elapsed}` : "",
  );
}

/**
 * Warnings from the server, plus the one caveat only this side knows about.
 *
 * `info.warnings` is what the request asked for and this server ignored — an unknown key in the
 * body, a `sortField` that lost to a `sortList`. It is never used for a bad filter (that is a
 * 400), so anything arriving here is worth one quiet line rather than an error block.
 */
function renderNotes(data) {
  const notes = [];

  for (const warning of (data.info && data.info.warnings) || []) {
    notes.push(el("p", { class: "ss-hint", text: `the server ignored part of this request: ${warning}` }));
  }

  // BM25 scores every document in a filter-only browse identically, so "relevance" there is
  // whatever order the segments happened to hold — stable, meaningless, and easily mistaken
  // for a ranking. Say so, and offer the order that does mean something.
  if (state.sort === "relevance" && !state.q.trim() && hasActiveFilters()) {
    notes.push(
      el(
        "p",
        { class: "ss-hint" },
        "There are no words to rank, so these are in no particular order — relevance only means something with a query. ",
        el("button", {
          class: "ss-more",
          type: "button",
          text: "sort by newest",
          on: {
            click: () => {
              state.sort = "newest";
              sortEl.value = "newest";
              search({ push: true });
            },
          },
        }),
      ),
    );
  }

  // What the default scope refused. Never inferred from the result list — the server counts
  // it — because a list a fifth shorter than the corpus can support is exactly the kind of
  // omission a reader discovers by failing to find something.
  const hidden = data.info && Number(data.info.hidden);
  if (Number.isFinite(hidden) && hidden > 0) {
    notes.push(
      el(
        "p",
        { class: "ss-hint" },
        `${fmtCount(hidden)} more ${hidden === 1 ? "document" : "documents"} matched and are not shown: attachments, `,
        el("code", { text: "system" }),
        " records and meta turns — the transcript's apparatus rather than its conversation. ",
        el("button", {
          class: "ss-more",
          type: "button",
          text: "include them",
          on: {
            click: () => {
              state.filters.all_records = true;
              syncControls();
              search({ push: true });
            },
          },
        }),
      ),
    );
  }

  if (state.includeThinking && indexStats && indexStats.thinking_indexed === false) {
    notes.push(
      el(
        "p",
        { class: "ss-hint" },
        "This index was built without thinking text, so including it matches nothing extra. ",
        el("code", { text: "session-search index --full" }),
        " rebuilds with it.",
      ),
    );
  }

  notesEl.replaceChildren(...notes);
}

// ─── Rendering: active filter chips ───────────────────────────────────────────────────────

function hasActiveFilters() {
  return activeFilters().length > 0;
}

/** One entry per thing narrowing the search, each knowing how to take itself back off. */
function activeFilters() {
  const f = state.filters;
  const chips = [];
  for (const tool of f.tool) {
    chips.push({ label: `tool: ${tool}`, remove: () => (f.tool = f.tool.filter((t) => t !== tool)) });
  }
  for (const pair of f.tool_input) {
    chips.push({
      label: `tool-input: ${pair}`,
      remove: () => (f.tool_input = f.tool_input.filter((p) => p !== pair)),
    });
  }
  // One chip for the pair: half of it is not a narrower search but a wider one, so they come
  // off together.
  if (f.turn_of && f.turn_seq) {
    chips.push({
      label: "one turn",
      remove: () => {
        f.turn_of = "";
        f.turn_seq = "";
      },
    });
  } else if (f.session) {
    chips.push({ label: `session: ${clampText(f.session, 12)}`, remove: () => (f.session = "") });
  }
  for (const [key, label] of [
    ["project", "project"],
    ["model", "model"],
    ["role", "role"],
    ["kind", "kind"],
    ["agent_type", "agent type"],
    ["branch", "branch"],
    ["since", "since"],
    ["until", "until"],
  ]) {
    if (f[key]) chips.push({ label: `${label}: ${f[key]}`, remove: () => (f[key] = "") });
  }
  if (f.errors_only) chips.push({ label: "errors only", remove: () => (f.errors_only = false) });
  if (f.all_records) {
    chips.push({ label: "including attachments", remove: () => (f.all_records = false) });
  }
  if (f.sidechain === "exclude") chips.push({ label: "no sidechains", remove: () => (f.sidechain = "any") });
  if (f.sidechain === "only") chips.push({ label: "sidechains only", remove: () => (f.sidechain = "any") });
  if (state.includeThinking) {
    chips.push({ label: "including thinking", remove: () => (state.includeThinking = false) });
  }
  return chips;
}

function renderChips() {
  const chips = activeFilters().map((chip) =>
    el(
      "span",
      { class: "ss-chip", title: chip.label },
      el("span", { text: chip.label }),
      el("button", {
        class: "ss-chip-x",
        type: "button",
        "aria-label": `Remove filter ${chip.label}`,
        on: {
          click: () => {
            chip.remove();
            syncControls();
            search({ push: true });
          },
        },
      }),
    ),
  );
  if (chips.length > 1) {
    chips.push(
      el("button", {
        class: "ss-more",
        type: "button",
        text: "clear all",
        on: {
          click: () => {
            clearFilters();
            syncControls();
            search({ push: true });
          },
        },
      }),
    );
  }
  chipsEl.replaceChildren(...chips);
}

function clearFilters() {
  state.filters.tool = [];
  state.filters.tool_input = [];
  for (const key of ["turn_of", "turn_seq", "session", "project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    state.filters[key] = "";
  }
  state.filters.errors_only = false;
  state.filters.all_records = false;
  state.filters.sidechain = "any";
  state.includeThinking = false;
}

// ─── Rendering: facets ────────────────────────────────────────────────────────────────────

function isFacetActive(facet, value) {
  return facet.multi ? state.filters[facet.filter].includes(value) : state.filters[facet.filter] === value;
}

function toggleFacet(facet, value) {
  if (facet.multi) {
    const list = state.filters[facet.filter];
    state.filters[facet.filter] = list.includes(value) ? list.filter((v) => v !== value) : [...list, value];
  } else {
    // Single-valued: clicking a second value replaces the first rather than adding it, and the
    // chip row shows the replacement immediately. A UI that let both look selected would send
    // one of them and drop the other.
    state.filters[facet.filter] = state.filters[facet.filter] === value ? "" : value;
  }
  search({ push: true });
}

function renderFacets(facets) {
  const sections = [];
  for (const facet of FACETS) {
    const envelope = (facets[facet.field] || [])[0];
    if (!envelope) continue;
    const rows = Array.isArray(envelope.data) ? envelope.data : [];
    const meta = envelope.meta || {};
    if (!rows.length) continue;

    const collapsed = collapsedFacets.has(facet.field);
    const body = el(
      "div",
      { hidden: collapsed },
      ...rows.map((row) =>
        el(
          "button",
          {
            class: "ss-facet-row",
            type: "button",
            "aria-pressed": isFacetActive(facet, row.value) ? "true" : "false",
            title: row.value,
            on: { click: () => toggleFacet(facet, String(row.value)) },
          },
          el("span", { text: displayValue(facet.field, row.value) }),
          el("span", { class: "ss-count", text: fmtCount(Number(row.count) || 0) }),
        ),
      ),
      ...facetHints(facet, meta, rows.length),
    );

    sections.push(
      el(
        "section",
        { class: "ss-facet" },
        el(
          "button",
          {
            class: "ss-facet-head",
            type: "button",
            "aria-expanded": collapsed ? "false" : "true",
            on: {
              click: () => {
                if (collapsedFacets.has(facet.field)) collapsedFacets.delete(facet.field);
                else collapsedFacets.add(facet.field);
                // Only the rows go: hiding the whole section would take the header with it and
                // leave no way to open it again.
                body.hidden = collapsedFacets.has(facet.field);
              },
            },
          },
          el("span", { text: facet.label }),
          el("span", { class: "ss-count", text: collapsed ? "+" : "−" }),
        ),
        body,
      ),
    );
  }

  facetsEl.replaceChildren(...sections);
}

/**
 * The honesty line under a facet.
 *
 * Summing the rows above answers "how many documents are in the buckets shown", which reads as
 * "how many matched" and is wrong by a factor of fifty on a field like `tool_input.command`.
 * `FacetResult` already worked out both caveats — how much of the distribution is off-screen,
 * and whether the field is a long tail that wants full-text search instead — so these lines
 * report its judgement rather than recomputing a second opinion that could disagree with it.
 */
function facetHints(facet, meta, shown) {
  const hints = [];
  const hidden = Number(meta.hiddenValues);
  const distinct = Number(meta.distinct);
  const matching = Number(meta.matchingDocs);
  const withValue = Number(meta.docsWithValue);

  if (meta.searchShaped) {
    hints.push(
      el(
        "p",
        { class: "ss-hint" },
        "Values here barely repeat",
        Number.isFinite(distinct) ? ` (about ${fmtCount(distinct)} distinct)` : "",
        ", so this list is a sample of a long tail rather than a distribution. Search it instead: ",
        el("code", { text: `${facet.field}:something` }),
        " in the query box.",
      ),
    );
  }

  if (Number.isFinite(hidden) && hidden > 0) {
    hints.push(
      el("p", {
        class: "ss-hint",
        text: `Showing the top ${fmtCount(shown)} of ${Number.isFinite(distinct) ? fmtCount(distinct) : "many"} values; ${fmtCount(hidden)} are not listed.`,
      }),
    );
  }

  if (Number.isFinite(matching) && Number.isFinite(withValue) && withValue < matching) {
    hints.push(
      el("p", {
        class: "ss-hint",
        text: `${fmtCount(withValue)} of ${fmtCount(matching)} matching documents carry this field, so these counts do not add up to the total.`,
      }),
    );
  }

  if (!facet.multi && shown > 1) {
    hints.push(el("p", { class: "ss-hint", text: "One value at a time; picking another replaces it." }));
  }

  return hints;
}

/** Facet values are whole paths and whole model ids; the row is one line wide. */
function displayValue(field, value) {
  const text = String(value ?? "");
  if (field === "project") return clampText(text.replace(/^\/home\/[^/]+/, "~"), 34);
  return clampText(text, 34);
}

// ─── Rendering: results ───────────────────────────────────────────────────────────────────
//
// A transcript is a conversation, so it is drawn as one: a column of turns down a rail, the
// person's words in a bubble, the model's as prose, and each tool call as a single collapsed
// line that says what it did. The alternative — every parameter of every call laid out as a
// table under every hit — is the same information and unreadable at the length a real session
// runs to, because nothing in it is smaller than anything else.
//
// What the search adds to that reading is emphasis. `_meta.snippetField` says which body the
// words were found in, and `highlight.js` marks them again in every turn drawn afterwards, so
// a session opened from a hit shows where the query is throughout rather than in one line.

/** What the current query marks. Rebuilt per search, so anything drawn later marks the same
 *  words — a thread widened twice, a drawer opened three clicks after the search. */
let markPlan = parseQuery("");

/** Mark `node` in place and hand it back, so it can be used inline in an `el(...)` call. */
function highlight(node) {
  markMatches(node, markPlan);
  return node;
}

/** Body text rendered in full inside a result card; past it the card shows the excerpt. */
const CARD_BODY_MAX = 1200;
/** And for a `system` or `attachment` record, which is routinely a whole environment block
 *  and is never what a card is being scanned for. */
const CARD_NOTE_MAX = 500;
/** And inside a thread or the drawer, where the reader asked for the turn itself. */
const TURN_BODY_MAX = 20000;
/** Text scanned when counting matches for a collapsed row. A `tool_output` reaches 1 MiB and
 *  the drawer asks this of four hundred turns, so the count is of a prefix and says so. */
const SCAN_MAX = 200000;

const SPEAKERS = {
  user: { who: "You", icon: "user" },
  assistant: { who: "Claude", icon: "spark" },
  system: { who: "System", icon: "gear" },
  attachment: { who: "Attachment", icon: "file" },
};

function renderResults(data) {
  const results = Array.isArray(data.results) ? data.results : [];
  if (!results.length) {
    resultsEl.replaceChildren();
    showEmpty();
    return;
  }
  emptyEl.hidden = true;
  // One card shape serves both units. The server collapses a turn to its best-scoring
  // document and says how many it stood in for, so a grouped card is a flat card that also
  // knows what it is standing in front of — there is no second `_meta` shape to read wrong.
  resultsEl.replaceChildren(...results.map((result, i) => renderCard(result, i)));
}

/**
 * One hit: the turn it matched, drawn as it was said, under a line of where and when.
 *
 * The rendered turn is `_meta.doc` — a whole `ApiDoc` — rather than the flattened `{raw}`
 * fields beside it. Those exist so a stock Search UI result template works against this server
 * without knowing anything about transcripts; this UI knows about transcripts.
 */
function renderCard(result, index) {
  const meta = result._meta || {};
  const doc = meta.doc || {};
  const excerpt = excerptOf(result, meta);
  const turn = turnOf(doc, meta);
  // Other documents of this turn that matched and were folded into this one. Zero unless the
  // search is grouped, and counted server-side against the whole matched set — so it is what
  // the anchor stands in for, not what happened to fit on the page.
  const collapsed = Number(meta.collapsed) || 0;

  // The turn as the card shows it. Once the gap above or below it is opened the loaded run
  // contains this same turn, marked as the focused one, and showing both says it twice.
  const hitNode = renderTurn(doc, { excerpt, bodyMax: noteLike(doc) ? CARD_NOTE_MAX : CARD_BODY_MAX });

  const before = el("div");
  const after = el("div");

  const conv = el(
    "div",
    { class: "ss-conv ss-request" },
    // What was being asked. Suppressed when the search is already scoped to one turn: every
    // card would then repeat the same instruction, which is the chip's job and not twenty
    // cards'.
    state.filters.turn_of ? null : openerTurn(turn),
    turn.hiddenBefore > 0
      ? turnGap(doc, {
          host: before,
          direction: "before",
          count: turn.hiddenBefore,
          label:
            turn.hiddenBefore === 1
              ? "1 turn between the request and this"
              : `${fmtCount(turn.hiddenBefore)} turns between the request and this`,
        })
      : null,
    before,
    hitNode,
    after,
    turnGap(doc, { host: after, direction: "after", label: "what happened next" }),
  );

  const card = el(
    "article",
    {
      class: "ss-card",
      tabindex: "-1",
      dataset: { index: String(index), error: doc.is_error ? "1" : null },
      on: { click: () => setSelected(index, false) },
    },
    contextLine(doc, collapsed ? collapsed + 1 : null),
    conv,
    el(
      "div",
      { class: "ss-card-actions" },
      // The gap between what the card shows and what the turn matched. Only ever set on a
      // grouped search, where this card is standing in for the rest.
      collapsed > 0 && turn.scopeable
        ? el("button", {
            class: "ss-more",
            type: "button",
            title: "Every matching document in this turn, one per row",
            text: `${fmtCount(collapsed)} more ${collapsed === 1 ? "match" : "matches"} in this turn`,
            on: { click: () => scopeToTurn(doc, { flat: true }) },
          })
        : null,
      turn.scopeable && !collapsed
        ? el("button", {
            class: "ss-more",
            type: "button",
            title: "Every document under this one turn",
            text: "only this turn",
            on: { click: () => scopeToTurn(doc, { flat: false }) },
          })
        : null,
      el("button", {
        class: "ss-more",
        type: "button",
        text: "open session",
        on: { click: () => openDrawer(doc) },
      }),
    ),
  );
  // `j`/`k` open the gap between the request and the hit, which is the one that has a count
  // and is the reason the card is shaped this way.
  card.__expand = () => {
    const gap = card.querySelector(".ss-gap[data-direction='before']") || card.querySelector(".ss-gap");
    if (gap) gap.click();
  };
  return card;
}

/**
 * Narrow to one turn.
 *
 * Both halves of the key or neither: `turn_seq` is a per-file ordinal, so a path-less one is
 * not a narrower search but a wider one, and the server refuses it rather than answering it.
 * `flat` turns grouping off with it — a grouped view of one turn is one card, which is the
 * card the reader is already looking at.
 */
function scopeToTurn(doc, { flat }) {
  state.filters.turn_of = doc.source_path || "";
  state.filters.turn_seq = String(doc.turn_seq);
  if (flat) state.group = "none";
  syncControls();
  search({ push: true });
}

/**
 * What the hit says about the turn it happened under.
 *
 * `turn_seq` is on every document and the turn's opening prompt is resolved once per page by
 * the server, so all of this is answerable from the search response — a card costs no extra
 * request to say what was being asked when the thing it found happened.
 */
function turnOf(doc, meta) {
  const turnSeq = Number(doc.turn_seq);
  const at = Number(doc.seq);
  const opener = (meta && meta.turnOpener) || null;
  if (!Number.isFinite(turnSeq) || !Number.isFinite(at)) {
    return { opener: null, hiddenBefore: 0, scopeable: false, opening: false };
  }
  return {
    opener,
    // The hit *is* the turn's opening prompt — there is nothing above it to draw. A file that
    // begins mid-conversation gets a synthetic turn with no opener at all, and then
    // `turnOpener` is null and this is false: both are "nothing to put above the hit".
    opening: turnSeq === at,
    hiddenBefore: Math.max(0, at - turnSeq - 1),
    scopeable: Boolean(doc.source_path) && Number.isFinite(turnSeq),
  };
}

/**
 * The human turn a hit happened under.
 *
 * The whole document, not a copy of its opening: the server resolves the opener for every turn
 * on the page in one query, so what arrives here is a real turn with its own timestamp and its
 * own raw line — and nothing has to warn the reader that they are looking at the first two
 * hundred characters of an instruction.
 */
function openerTurn(turn) {
  if (turn.opening || !turn.opener) return null;
  const node = renderTurn(turn.opener, { bodyMax: CARD_BODY_MAX });
  node.classList.add("ss-turn-request");
  const who = node.querySelector(".ss-who");
  if (who) who.textContent = "You asked";
  return node;
}

/**
 * A run of turns nobody has asked for yet, as one line saying how many.
 *
 * The gap above a hit has an exact count, because `turn_seq` says where the turn began.
 * The one below does not — nothing on the hit says how far the request runs — so it loads a
 * window and stops at the boundary it finds, which is either the next request or the end of
 * the transcript. Saying which is the point: "nothing follows" and "the next request starts
 * here" are different facts about the session and a reader is entitled to both.
 */
function turnGap(doc, { host, direction, count = null, label }) {
  const at = direction === "before";
  let loaded = 0;

  const button = el("button", {
    class: "ss-gap",
    type: "button",
    dataset: { direction },
    "aria-expanded": "false",
    on: { click: () => load() },
  });
  const caption = el("span", { class: "ss-gap-label", text: label });
  button.append(el("span", { class: "ss-gap-rule", "aria-hidden": "true" }), caption);

  async function load() {
    if (!Number.isFinite(Number(doc.seq)) || !doc.session_id) {
      caption.textContent = "this hit carries no session and seq to read around";
      button.disabled = true;
      return;
    }
    // Before: exactly the run between the request and the hit, which is known. After: a
    // window, widened by another one on each press until a boundary answers the question.
    const want = at ? count : loaded + WINDOW_STEP;
    button.disabled = true;
    caption.textContent = "loading…";
    try {
      const data = await fetchAround(doc, at ? want : 0, at ? 0 : want, false);
      const all = Array.isArray(data.docs) ? data.docs : [];
      let turns = all.filter((d) =>
        at ? Number(d.seq) < Number(doc.seq) : Number(d.seq) > Number(doc.seq),
      );
      // Below the hit, the run belongs to this request only until the next one begins.
      let boundary = null;
      if (!at) {
        const next = turns.findIndex((d) => Number(d.turn_seq) !== Number(doc.turn_seq));
        if (next !== -1) {
          boundary = turns[next];
          turns = turns.slice(0, next);
        }
      }
      host.replaceChildren(...turns.map((turn) => renderTurn(turn)));
      loaded = turns.length;
      button.setAttribute("aria-expanded", "true");

      if (at) {
        button.remove();
        return;
      }
      if (boundary) {
        caption.textContent = "the next request starts here";
        button.disabled = true;
      } else if (turns.length < want) {
        caption.textContent = turns.length
          ? "the end of this transcript"
          : "nothing follows this in the transcript";
        button.disabled = true;
      } else {
        caption.textContent = "further";
        button.disabled = false;
      }
    } catch (err) {
      if (err === SUPERSEDED) return;
      caption.textContent = failureText(err);
      button.disabled = false;
    }
  }

  return button;
}

/**
 * The excerpt the server cut, and which body it came from.
 *
 * `_meta.snippetField` is four different claims about what happened — what the turn said, a
 * snippet it quoted, what a command printed, what the model was thinking privately — and each
 * one is shown in the place that says so, never as if the model had written it.
 */
function excerptOf(result, meta) {
  const field = meta.snippetField || "text";
  const entry = result[field];
  const html = entry && typeof entry.snippet === "string" ? entry.snippet : null;
  return html === null ? null : { field, html };
}

/** The server's excerpt as a block. `data-marked` keeps `highlight.js` off it: the `<em>`s in
 *  it are the spans the index actually matched, and a second opinion can only disagree. */
function excerptNode(excerpt) {
  const label =
    excerpt.field === "tool_output"
      ? "matched in what the tool printed"
      : excerpt.field === "thinking"
        ? "matched in the model's thinking"
        : excerpt.field === "code"
          ? "matched in code"
          : null;
  return el(
    "div",
    { class: "ss-excerpt", dataset: { marked: "", field: excerpt.field } },
    label ? el("span", { class: "ss-excerpt-label", text: label }) : null,
    el("p", { class: "ss-snippet", html: excerpt.html }),
  );
}

/** Where and when, above a hit. Quiet on purpose: it is the filing, not the conversation. */
function contextLine(doc, matched = null) {
  const parts = [];
  if (doc.project) {
    parts.push(
      el(
        "span",
        { class: "ss-path", title: doc.project },
        el("span", { class: "ss-path-dir", text: dirname(String(doc.project).replace(/^\/home\/[^/]+/, "~")) }),
        el("span", { class: "ss-path-base", text: basename(doc.project) }),
      ),
    );
  }
  if (doc.git_branch) parts.push(el("span", { text: doc.git_branch }));
  if (Number.isFinite(doc.timestamp_ms)) {
    parts.push(el("span", { title: fmtTime(doc.timestamp_ms), text: relTime(doc.timestamp_ms) }));
  }
  parts.push(el("span", { class: "ss-count", title: doc.doc_id || "", text: sessionKey(doc) }));
  if (doc.is_sidechain) parts.push(el("span", { class: "ss-badge", text: "sidechain" }));
  if (doc.agent_type) parts.push(el("span", { class: "ss-badge", text: clampText(doc.agent_type, 18) }));
  if (doc.is_meta) parts.push(el("span", { class: "ss-badge", text: "meta" }));
  if (matched !== null) {
    parts.push(
      el("span", {
        class: "ss-hits ss-ctx-hits",
        text: matched === 1 ? "1 match" : `${fmtCount(matched)} matches`,
      }),
    );
  }
  return el("div", { class: "ss-card-ctx" }, ...parts);
}

/** A record that is part of the transcript rather than part of the conversation. */
function noteLike(doc) {
  return doc.role === "system" || doc.role === "attachment";
}

function sessionKey(doc) {
  const id = String(doc.session_id || "");
  const short = id.length > 8 ? id.slice(0, 8) : id;
  return doc.agent_id ? `${short}:${String(doc.agent_id).slice(0, 6)}` : short;
}

// ─── Expansion and turn rendering ─────────────────────────────────────────────────────────

/**
 * The window around one hit.
 *
 * `source_path` travels with the request, and it is not optional politeness: `seq` is a
 * per-file ordinal, and a `resetSessionFile()` or a relocated project leaves two transcripts
 * sharing one `session_id`. Scoped by session alone the two files interleave and the window
 * quietly returns neighbours from the wrong conversation.
 */
function fetchAround(doc, before, after, includeRaw) {
  const p = new URLSearchParams({ seq: String(doc.seq), before: String(before), after: String(after) });
  if (doc.agent_id) p.set("agent", doc.agent_id);
  if (doc.source_path) p.set("source_path", doc.source_path);
  if (includeRaw) p.set("include_raw", "1");
  return api(`/api/sessions/${encodeURIComponent(doc.session_id || "")}/around?${p.toString()}`);
}

/**
 * One turn of the conversation.
 *
 * Three shapes, because three things happened: a person typed, the model answered, or a tool
 * ran. A tool call collapses to its one line — `Bash  cargo test --locked` — and opens on
 * demand, which is the only way a session of six hundred turns stays a thing you can scroll.
 */
function renderTurn(doc, { focused = false, excerpt = null, bodyMax = TURN_BODY_MAX } = {}) {
  const role = typeof doc.role === "string" ? doc.role : "";
  const isTool = doc.kind === "tool_call";
  const speaker = SPEAKERS[role] || SPEAKERS.system;

  const main = el("div", { class: "ss-turn-main" });
  if (!isTool) main.append(turnHead(doc, speaker));

  let drew = false;
  if (isTool) {
    main.append(renderActivity(doc, { excerpt, open: focused && excerpt === null }));
    drew = true;
  } else {
    const body = messageBody(doc, excerpt, bodyMax);
    if (body) {
      main.append(body);
      drew = true;
    }
  }

  const thinking = thinkingBlock(doc, excerpt, bodyMax);
  if (thinking) {
    main.append(thinking);
    drew = true;
  }

  if (!drew) {
    // A record this build indexed but has no view for. Saying so points at the affordance that
    // always works, rather than leaving a turn that looks empty because it contained nothing.
    main.append(
      el("p", {
        class: "ss-hint",
        text: "no text here that this view knows how to render — the raw JSONL line is the record itself",
      }),
    );
  }

  // The raw line lives in the gutter under the avatar rather than in a row of its own: a
  // control on every one of four hundred turns costs a row of empty space on every one of
  // them, and empty space between a turn and the next is exactly what a conversation is not.
  const rawHost = el("div");
  main.append(rawHost);

  return el(
    "div",
    {
      class: ["ss-turn", focused ? "ss-focus" : null],
      dataset: {
        role: role || "system",
        kind: isTool ? "tool_call" : "message",
        docId: doc.doc_id || "",
        error: doc.is_error ? "1" : null,
      },
    },
    el(
      "div",
      { class: "ss-turn-rail" },
      avatarNode(doc, speaker, isTool),
      el("div", { class: "ss-turn-tools" }, rawButton(doc, rawHost, { icon: true })),
    ),
    main,
  );
}

function avatarNode(doc, speaker, isTool) {
  if (isTool) {
    const meta = toolMeta(doc);
    return el("span", { class: "ss-avatar ss-avatar-tool", dataset: { accent: meta.accent } }, iconFor(meta.icon));
  }
  return el("span", { class: "ss-avatar", dataset: { role: doc.role || "system" } }, iconFor(speaker.icon));
}

function turnHead(doc, speaker) {
  const parts = [el("span", { class: "ss-who", text: speaker.who })];
  if (doc.role === "assistant" && doc.model) {
    parts.push(el("span", { class: "ss-turn-model", title: doc.model, text: clampText(doc.model, 24) }));
  }
  if (Number.isFinite(doc.timestamp_ms)) {
    parts.push(el("span", { class: "ss-when", title: fmtTime(doc.timestamp_ms), text: fmtClock(doc.timestamp_ms) }));
  }
  return el("div", { class: "ss-turn-head" }, ...parts);
}

/**
 * What was said.
 *
 * `body` is the body as a reader saw it; `text` and `code` are the indexed halves of it, split
 * for retrieval and not reassemblable into it (the split drops link destinations and loses
 * where a fence sat). So the reader always gets `body` — unless it is longer than this view
 * asked for, in which case the server's excerpt is shown instead and says that it is one.
 */
function messageBody(doc, excerpt, max) {
  const body = typeof doc.body === "string" ? doc.body : "";
  const own = excerpt && excerpt.field !== "thinking" ? excerpt : null;
  const wrap = (node) => el("div", { class: doc.role === "user" ? "ss-bubble" : "ss-prose" }, node);
  if (!body.trim()) return own ? wrap(excerptNode(own)) : null;
  return wrap(longText(body, own, max));
}

/**
 * Text that may be longer than this view wants to draw.
 *
 * Under the cap it is rendered whole and marked here, and the server's excerpt is not shown at
 * all — it is a window onto text that is already entirely on screen, and printing both says
 * the same thing twice. Over the cap the excerpt wins, because it is centred on the match and
 * the first `max` characters of a turn whose match is nine thousand characters in are not.
 */
function longText(text, excerpt, max) {
  if (text.length <= max) return highlight(el("div", { class: "ss-md" }, renderMarkdown(text)));

  const host = el("div", { class: "ss-md" });
  const showAll = el("button", {
    class: "ss-more",
    type: "button",
    text: `show all ${fmtCount(text.length)} characters`,
    on: {
      click: () => {
        host.replaceChildren(highlight(el("div", {}, renderMarkdown(text))));
        showAll.remove();
      },
    },
  });
  host.append(excerpt ? excerptNode(excerpt) : highlight(el("div", {}, renderMarkdown(clampText(text, max)))));
  return frag(host, showAll);
}

/**
 * The model's scratch work, behind a disclosure.
 *
 * Never open unless the match is in it: thinking is long, it is not something that was said,
 * and presenting it inline beside the turn invites reading it as if it were.
 */
function thinkingBlock(doc, excerpt, max) {
  const text = typeof doc.thinking === "string" ? doc.thinking : "";
  const matched = excerpt && excerpt.field === "thinking" ? excerpt : null;
  if (!text.trim()) return matched ? el("div", { class: "ss-think-body" }, excerptNode(matched)) : null;

  const host = el("div", { class: "ss-think-body" });
  let built = false;
  const build = () => {
    if (built) return;
    built = true;
    host.append(longText(text, matched, max));
  };

  const hits = countIn(text);
  const details = el(
    "details",
    { class: "ss-think", on: { toggle: () => details.open && build() } },
    el(
      "summary",
      {},
      iconFor("brain"),
      el("span", { text: `thought for ${fmtCount(text.length)} characters` }),
      hits ? el("span", { class: "ss-hits", text: hitLabel(hits) }) : null,
    ),
    host,
  );
  if (matched) {
    // The words were found in here, so opening it is the answer to "found where?".
    build();
    details.open = true;
  }
  return details;
}

/**
 * A tool call as one line, opening onto the call and its result.
 *
 * Built lazily: a `MultiEdit` is a stack of diffs and the drawer draws four hundred turns, so
 * a row that is never opened must cost a row. The match count is not lazy — it is counted from
 * the text rather than from the DOM, precisely so a collapsed row can say what is inside it.
 */
function renderActivity(doc, { excerpt = null, open = false } = {}) {
  const meta = toolMeta(doc);
  const summary = toolSummary(doc);
  const hits = countIn(doc.body) + countIn(doc.tool_output);

  const bodyHost = el("div", { class: "ss-act-body", hidden: true });
  let built = false;
  const build = () => {
    if (built) return;
    built = true;
    bodyHost.append(renderToolCall(doc));
    const result = renderToolResult(doc);
    if (result) bodyHost.append(result);
    highlight(bodyHost);
  };

  const chevron = iconFor("chevron");
  chevron.classList.add("ss-act-chevron");

  // The row's own argument is marked like everything else. `markMatches` skips buttons when it
  // walks a subtree — chrome is not transcript — so the span is handed to it directly.
  const arg = summary ? el("span", { class: "ss-act-arg", title: summary, text: summary }) : null;
  const argMarks = arg ? markMatches(arg, markPlan) : 0;

  const head = el(
    "button",
    {
      class: "ss-act",
      type: "button",
      "aria-expanded": "false",
      dataset: { accent: meta.accent, error: doc.is_error ? "1" : null },
      on: { click: () => setOpen(bodyHost.hidden) },
    },
    el("span", { class: "ss-act-tool", text: clampText(meta.label, 24) }),
    arg,
    doc.is_error ? el("span", { class: "ss-act-flag", text: "error" }) : null,
    hits ? el("span", { class: "ss-hits", text: hitLabel(hits) }) : null,
    chevron,
  );

  // An excerpt cut from the call itself repeats the row above it word for word — the body of
  // a tool call *is* its name and its parameters — so it is only worth the space when the row
  // is not already showing the match. Output and thinking are never on the row, so they are.
  const shows = excerpt && (excerpt.field === "tool_output" || excerpt.field === "thinking" || argMarks === 0);
  const preview = shows ? excerptNode(excerpt) : null;

  const setOpen = (on) => {
    if (on) build();
    bodyHost.hidden = !on;
    // The excerpt is the stand-in for the body; showing both says the same thing twice, and
    // the one that is actually in the transcript should win once it is on screen.
    if (preview) preview.hidden = on;
    head.setAttribute("aria-expanded", on ? "true" : "false");
  };

  const node = el("div", { class: "ss-act-wrap" }, head, preview, bodyHost);
  if (open) setOpen(true);
  return node;
}

/** How many marks `text` would get. Counted from the text, so a collapsed row can say it. */
function countIn(text) {
  if (typeof text !== "string" || !text || isEmptyPlan(markPlan)) return 0;
  return matchRanges(text.slice(0, SCAN_MAX), markPlan).length;
}

function hitLabel(n) {
  return n === 1 ? "1 match" : `${fmtCount(n)} matches`;
}

/**
 * The graceful-degradation floor.
 *
 * Every renderer above can decline: an unknown MCP tool, a `tool_input` that is a number, a
 * record shape this build predates. The original JSONL line is the one representation that is
 * always right, so it is one press away from every turn rather than a thing you go to the
 * terminal for.
 */
function rawButton(doc, host, { icon = false } = {}) {
  let shown = false;
  const label = (open) => (open ? "hide raw" : "raw JSON");
  const setLabel = (open) => {
    // The icon form has no room for a caption, so the state it would have carried moves to
    // the tooltip and to `aria-pressed`, which is what a screen reader reads either way.
    if (icon) button.setAttribute("aria-pressed", open ? "true" : "false");
    else button.textContent = label(open);
  };
  const button = el("button", {
    class: icon ? "ss-btn ss-btn-icon ss-raw" : "ss-more",
    type: "button",
    title: "The raw JSONL line for this turn",
    "aria-label": "Show the raw JSONL line",
    "aria-pressed": icon ? "false" : null,
    text: icon ? null : "raw JSON",
    on: {
      click: async () => {
        if (shown) {
          host.replaceChildren();
          shown = false;
          setLabel(false);
          return;
        }
        if (!Number.isFinite(Number(doc.seq)) || !doc.session_id) {
          host.replaceChildren(el("p", { class: "ss-hint", text: "this document carries no session and seq to fetch its raw line by" }));
          return;
        }
        button.disabled = true;
        try {
          // A zero-width window: the raw line is the biggest field there is, and asking for it
          // across a whole expanded window would fetch forty copies of what one press wanted.
          const data = await fetchAround(doc, 0, 0, true);
          const match = (data.docs || []).find((d) => d.doc_id === doc.doc_id) || (data.docs || [])[0];
          host.replaceChildren(rawBlock(match));
          shown = true;
          setLabel(true);
        } catch (err) {
          if (err !== SUPERSEDED) host.replaceChildren(el("p", { class: "ss-err", text: failureText(err) }));
        } finally {
          button.disabled = false;
        }
      },
    },
  });
  if (icon) button.appendChild(iconFor("braces"));
  return button;
}

function rawBlock(doc) {
  if (!doc || typeof doc.raw !== "string") {
    return el("p", { class: "ss-hint", text: "the server returned no raw line for this document" });
  }
  let text = doc.raw;
  try {
    text = JSON.stringify(JSON.parse(doc.raw), null, 2);
  } catch (_) {
    // A line that does not parse is exactly the case this button exists for; show it verbatim.
  }
  return frag(
    el("pre", { class: "ss-json", text }),
    el("button", {
      class: "ss-more",
      type: "button",
      text: "copy",
      on: { click: (ev) => copyText(doc.raw).then(() => (ev.currentTarget.textContent = "copied")) },
    }),
  );
}

// ─── The session drawer ───────────────────────────────────────────────────────────────────

async function openDrawer(doc) {
  drawerTitleEl.textContent = `${sessionKey(doc)}${doc.project ? ` · ${doc.project}` : ""}`;
  drawerBodyEl.replaceChildren(el("div", { class: "ss-loading" }, el("span", { class: "ss-spinner" }), "loading the session…"));
  showDrawer();

  const p = new URLSearchParams({ limit: String(SESSION_LIMIT) });
  if (doc.agent_id) p.set("agent", doc.agent_id);
  if (doc.source_path) p.set("source_path", doc.source_path);

  try {
    const data = await api(`/api/sessions/${encodeURIComponent(doc.session_id || "")}?${p.toString()}`);
    const docs = Array.isArray(data.docs) ? data.docs : [];
    const conv = el(
      "div",
      { class: "ss-conv" },
      ...docs.map((turn) => renderTurn(turn, { focused: turn.doc_id === doc.doc_id })),
    );
    const nodes = [conv];
    if (data.truncated) {
      nodes.push(
        el("p", {
          class: "ss-hint",
          text: `the first ${fmtCount(docs.length)} turns; this session continues past them`,
        }),
      );
    }
    drawerBodyEl.replaceChildren(...nodes);
    drawerNavEl.replaceChildren(matchNav(drawerBodyEl));
    // The matched turn is the reason the drawer was opened; a session view that lands at turn
    // one leaves the reader to find it again by hand in four hundred turns.
    const focus = drawerBodyEl.querySelector(".ss-focus");
    if (focus) focus.scrollIntoView({ block: "center" });
  } catch (err) {
    if (err !== SUPERSEDED) drawerBodyEl.replaceChildren(el("p", { class: "ss-err", text: failureText(err) }));
  }
}

/**
 * The Task card's "sidechain transcript" button, which `tools.js` renders and only this file
 * can act on — it is the one place that knows the route.
 *
 * Delegated rather than bound per button: results are replaced wholesale on every search and
 * again on every expansion, and a button whose listener was missed is worse than no button —
 * it hovers like a chip, looks live, and does nothing.
 */
function bindSidechainLinks(host) {
  host.addEventListener("click", (ev) => {
    const button = ev.target.closest && ev.target.closest("button[data-agent]");
    if (!button || !host.contains(button)) return;
    // `tools.js` renders the button only when it has both ids, and the drawer resolves the
    // subagent's own `source_path` from them.
    openDrawer({ session_id: button.dataset.session, agent_id: button.dataset.agent });
  });
}

bindSidechainLinks(resultsEl);
bindSidechainLinks(drawerBodyEl);

/**
 * Step through the marks in a session.
 *
 * The count is taken at each press rather than cached, because it moves: a collapsed tool row
 * says how many matches are inside it, and opening one puts those marks into this pool. A
 * number frozen at render time would disagree with the row right next to it.
 */
function matchNav(host) {
  if (isEmptyPlan(markPlan)) return frag();

  let at = -1;
  const label = el("span", { class: "ss-count" });

  const marks = () => Array.from(host.querySelectorAll("mark.ss-hit"));

  const step = (dir) => {
    const all = marks();
    if (!all.length) {
      label.textContent = "no marks on screen";
      return;
    }
    at = (at + dir + all.length) % all.length;
    for (const mark of all) mark.classList.remove("ss-hit-on");
    all[at].classList.add("ss-hit-on");
    all[at].scrollIntoView({ block: "center" });
    label.textContent = `${at + 1} / ${all.length}`;
  };

  const button = (text, dir, title) =>
    el("button", { class: "ss-btn ss-btn-icon", type: "button", title, "aria-label": title, text, on: { click: () => step(dir) } });

  const count = countMarks(host);
  label.textContent = count ? hitLabel(count) : "no marks on screen";
  host.__step = step;
  return frag(label, button("\u2191", -1, "Previous match"), button("\u2193", 1, "Next match"));
}

function showDrawer() {
  drawerEl.hidden = false;
  if (typeof drawerEl.showModal === "function" && !drawerEl.open) drawerEl.showModal();
}

function closeDrawer() {
  if (drawerEl.open) drawerEl.close();
  drawerEl.hidden = true;
  drawerBodyEl.replaceChildren();
  drawerNavEl.replaceChildren();
}

// `<dialog>` closes itself on Escape and on the backdrop, and the markup ships `hidden` as well
// as closed — so the attribute has to come back on whichever way it was closed, or the stylesheet
// keeps showing it over the results.
drawerEl.addEventListener("close", () => {
  drawerEl.hidden = true;
});
drawerCloseEl.addEventListener("click", closeDrawer);

// ─── Pagination ───────────────────────────────────────────────────────────────────────────

function renderPager(data) {
  const totalPages = Number(data.totalPages) || 0;
  const current = Number(data.current) || state.page;
  if (totalPages <= 1) {
    pagerEl.replaceChildren();
    return;
  }

  const go = (page) => {
    state.page = page;
    writeUrl(true);
    selected = -1;
    runSearch();
    resultsEl.scrollIntoView({ block: "start" });
  };

  const buttons = [
    el("button", {
      type: "button",
      text: "prev",
      disabled: current <= 1,
      on: { click: () => go(current - 1) },
    }),
  ];

  // A window of pages around the current one: a corpus of 148k documents is 7,000 pages, and
  // rendering all of them would be a longer list than the results.
  const first = Math.max(1, current - 2);
  const last = Math.min(totalPages, first + 4);
  if (first > 1) buttons.push(el("button", { type: "button", text: "1", on: { click: () => go(1) } }));
  if (first > 2) buttons.push(el("span", { class: "ss-count", text: "…" }));
  for (let page = first; page <= last; page += 1) {
    buttons.push(
      el("button", {
        type: "button",
        text: String(page),
        "aria-current": page === current ? "page" : null,
        on: { click: () => go(page) },
      }),
    );
  }
  if (last < totalPages - 1) buttons.push(el("span", { class: "ss-count", text: "…" }));
  if (last < totalPages) {
    buttons.push(el("button", { type: "button", text: String(totalPages), on: { click: () => go(totalPages) } }));
  }
  buttons.push(
    el("button", {
      type: "button",
      text: "next",
      disabled: current >= totalPages,
      on: { click: () => go(current + 1) },
    }),
  );

  pagerEl.replaceChildren(...buttons);
}

// ─── States: loading, error, empty ────────────────────────────────────────────────────────

function setLoading(on) {
  if (loadingEl) loadingEl.hidden = !on;
}

function clearError() {
  errorEl.hidden = true;
  errorEl.replaceChildren();
}

function failureText(err) {
  if (err instanceof ApiFailure) return err.status ? `${err.status} — ${err.message}` : err.message;
  return err && err.message ? err.message : String(err);
}

/**
 * A failed search.
 *
 * The results are cleared rather than left standing: a stale list beside a fresh error reads as
 * the answer to the query that just failed, which is the one thing this must never imply.
 */
function showError(err) {
  const status = err instanceof ApiFailure ? err.status : null;
  errorEl.replaceChildren(
    el("strong", { text: status ? `${status} ` : "failed: " }),
    el("span", { text: err instanceof ApiFailure ? err.message : failureText(err) }),
  );
  errorEl.hidden = false;
}

function showFailure(err) {
  showError(err);
  resultsEl.replaceChildren();
  pagerEl.replaceChildren();
  summaryEl.replaceChildren();
  notesEl.replaceChildren();
  emptyEl.hidden = true;
}

/**
 * Nothing matched — but there are two very different reasons for that, and they need different
 * next actions. An index that was never built answers every query with zero, and telling that
 * reader to loosen their filters would send them refining a search against no documents at all.
 */
function showEmpty() {
  emptyEl.hidden = false;
  if (!indexStats) {
    // `/api/stats` has not answered yet, and the two explanations below differ only by what it
    // says. Claiming "0 documents are indexed" on an unanswered stats call would invent the
    // more alarming of the two.
    emptyEl.replaceChildren(el("p", { class: "ss-empty-title", text: "No documents matched." }));
    loadStats().then(() => {
      if (!emptyEl.hidden) emptyBody();
    });
    return;
  }
  emptyBody();
}

function emptyBody() {
  const empty = indexStats && Number(indexStats.docs) === 0;
  if (empty) {
    emptyEl.replaceChildren(
      el("p", { class: "ss-empty-title", text: "This index has no documents in it." }),
      el(
        "p",
        { class: "ss-empty-body" },
        "Nothing has been indexed yet, so every query answers zero. Run ",
        el("code", { text: "session-search index" }),
        " on the machine serving this page, or press ",
        el("strong", { text: "reindex" }),
        " above.",
      ),
    );
  } else if (hasActiveFilters()) {
    emptyEl.replaceChildren(
      el("p", { class: "ss-empty-title", text: "No documents matched." }),
      el(
        "p",
        { class: "ss-empty-body" },
        `Every filter in the rail is ANDed on top of the query, so ${activeFilters().length === 1 ? "the chip" : "one of the chips"} above can be the reason rather than the words. There is no fuzzy operator — `,
        el("code", { text: "term~1" }),
        " is phrase slop, not a typo forgiver.",
      ),
    );
  } else {
    emptyEl.replaceChildren(
      el("p", { class: "ss-empty-title", text: "No documents matched." }),
      el(
        "p",
        { class: "ss-empty-body" },
        "The query language is words, ",
        el("code", { text: '"phrases"' }),
        ", ",
        el("code", { text: "AND/OR/NOT" }),
        " and ",
        el("code", { text: "field:value" }),
        `. ${fmtCount(indexStats ? Number(indexStats.docs) || 0 : 0)} documents are indexed, so there is a corpus to search — these words are just not in it.`,
      ),
    );
  }
}

// ─── The filter panel ─────────────────────────────────────────────────────────────────────
//
// Built once, at boot, and never rebuilt: `#facets` is replaced wholesale on every response,
// and an `<input>` replaced under a reader's fingers loses the caret mid-word. So these
// controls live beside that container rather than inside it.

function buildFilterPanel() {
  const rows = [];

  const toolInput = el("input", {
    type: "text",
    placeholder: "command=cargo",
    "aria-label": "Filter by a tool parameter, as key=value",
    on: {
      keydown: (ev) => {
        if (ev.key !== "Enter") return;
        const value = toolInput.value.trim();
        if (!value) return;
        // Sent as typed, including a missing `=`: `search.rs` refuses an empty key or value with
        // a message that names the form, and inventing a second, differently-worded rejection
        // here would give the same mistake two answers.
        if (!state.filters.tool_input.includes(value)) state.filters.tool_input.push(value);
        toolInput.value = "";
        search({ push: true });
      },
    },
  });
  rows.push(field("tool input", toolInput, "a tool parameter as key=value; Enter adds it"));

  const since = dateField("since", "since");
  const until = dateField("until", "until");
  rows.push(field("since", since, "RFC3339, YYYY-MM-DD, or a span like 7d"));
  rows.push(field("until", until, null));

  const errors = el("input", {
    type: "checkbox",
    on: {
      change: () => {
        state.filters.errors_only = errors.checked;
        search({ push: true });
      },
    },
  });
  rows.push(checkbox("errors only", errors));

  const sidechain = el(
    "select",
    {
      "aria-label": "Sidechains",
      on: {
        change: () => {
          state.filters.sidechain = sidechain.value;
          search({ push: true });
        },
      },
    },
    el("option", { value: "any", text: "include" }),
    el("option", { value: "exclude", text: "exclude" }),
    el("option", { value: "only", text: "only" }),
  );
  rows.push(field("sidechains", sidechain, "subagent transcripts run beside the main one"));

  const thinking = el("input", {
    type: "checkbox",
    on: {
      change: () => {
        state.includeThinking = thinking.checked;
        search({ push: true });
      },
    },
  });
  rows.push(checkbox("search thinking", thinking));

  const allRecords = el("input", {
    type: "checkbox",
    on: {
      change: () => {
        state.filters.all_records = allRecords.checked;
        search({ push: true });
      },
    },
  });
  rows.push(checkbox("attachments and system", allRecords));

  const panel = el(
    "section",
    { class: "ss-facet" },
    el("div", { class: "ss-facet-head" }, el("span", { text: "filters" })),
    el("div", {}, ...rows),
  );
  facetsEl.parentNode.insertBefore(panel, facetsEl);

  return { toolInput, since, until, errors, sidechain, thinking, allRecords };
}

function dateField(key, label) {
  // Not `type="date"`: the API also accepts a relative span like `7d`, and a native date picker
  // would make the most useful spelling of "this week" untypeable.
  const input = el("input", {
    type: "text",
    placeholder: "2026-01-01 or 7d",
    "aria-label": label,
    on: {
      change: () => {
        state.filters[key] = input.value.trim();
        search({ push: true });
      },
    },
  });
  return input;
}

/**
 * A labelled control in the filter panel.
 *
 * Deliberately not `.ss-facet-row`: that row is a value and a count, and it ellipsis-clips its
 * first child so a long path degrades gracefully. Put a text input in it and the input wins the
 * width, leaving the label clipped to "to…" — the panel stops saying what its own fields are.
 * A form row stacks instead, so the label is always whole and the control always full width.
 */
function field(label, control, hint) {
  return frag(
    el("label", { class: "ss-field-row" }, el("span", { text: label }), control),
    hint ? el("p", { class: "ss-hint", text: hint }) : null,
  );
}

/** A checkbox is the one control narrow enough to sit beside its label. */
function checkbox(label, control) {
  return el(
    "label",
    { class: "ss-field-row ss-field-inline" },
    el("span", { text: label }),
    control,
  );
}

const controls = buildFilterPanel();

/** Push state back into the controls, after a URL restore or a chip removal. */
function syncControls() {
  qEl.value = state.q;
  sortEl.value = state.sort;
  groupEl.value = state.group;
  controls.since.value = state.filters.since;
  controls.until.value = state.filters.until;
  controls.errors.checked = state.filters.errors_only;
  controls.sidechain.value = state.filters.sidechain;
  controls.thinking.checked = state.includeThinking;
  controls.allRecords.checked = state.filters.all_records;
}

// ─── Stats and reindex ────────────────────────────────────────────────────────────────────

async function loadStats() {
  try {
    indexStats = await api("/api/stats");
    renderStats();
  } catch (err) {
    // A stats failure must not look like a search failure: the search box still works, and the
    // only thing lost is the line that says how big the corpus is.
    statsEl.replaceChildren(el("span", { text: `index stats unavailable: ${failureText(err)}` }));
  }
}

function renderStats() {
  if (!indexStats) return;
  const bits = [
    `${fmtCount(Number(indexStats.docs) || 0)} documents`,
    `${fmtCount(Number(indexStats.sessions) || 0)} sessions`,
    `${fmtCount(Number(indexStats.files) || 0)} transcripts`,
  ];
  if (Number.isFinite(indexStats.size_bytes)) bits.push(fmtBytes(indexStats.size_bytes));
  bits.push(indexStats.thinking_indexed ? "thinking indexed" : "thinking not indexed");
  statsEl.replaceChildren(el("span", { title: indexStats.index_dir || "", text: bits.join(" · ") }));
}

refreshEl.addEventListener("click", async () => {
  refreshEl.disabled = true;
  const label = refreshEl.querySelector(".ss-btn-label");
  const original = label ? label.textContent : "";
  if (label) label.textContent = "indexing…";
  clearError();
  try {
    await api("/api/reindex", { method: "POST" });
    await loadStats();
    // The corpus moved under the current result set, so the page on screen may now be stale by
    // exactly the documents the reader pressed this button to find.
    runSearch();
  } catch (err) {
    // 409 is the expected answer when `--refresh-secs` is already mid-run, not a bug: one
    // `IndexWriter` at a time is the rule, and the server's message says so.
    showFailure(err);
  } finally {
    refreshEl.disabled = false;
    if (label) label.textContent = original;
  }
});

// ─── Selection and keyboard ───────────────────────────────────────────────────────────────

function cards() {
  return Array.from(resultsEl.children);
}

/**
 * Mark one card as the keyboard's current row.
 *
 * `.ss-focus` is the stylesheet's "this is the one" and there is no second selection style, so
 * it does double duty here. An invisible selection would make `j`/`k` look broken.
 */
function setSelected(index, scroll = true) {
  const list = cards();
  if (!list.length) {
    selected = -1;
    return;
  }
  selected = Math.max(0, Math.min(list.length - 1, index));
  list.forEach((card, i) => card.classList.toggle("ss-focus", i === selected));
  if (scroll) list[selected].scrollIntoView({ block: "nearest" });
}

function clearSelection() {
  selected = -1;
  for (const card of cards()) card.classList.remove("ss-focus");
}

/** A shortcut that fires while someone is typing a query is a bug, not a shortcut. */
function isTyping(target) {
  if (!target) return false;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable === true;
}

document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") {
    if (drawerEl.open || !drawerEl.hidden) closeDrawer();
    else if (isTyping(ev.target)) ev.target.blur();
    else clearSelection();
    return;
  }

  if (ev.metaKey || ev.ctrlKey || ev.altKey) return;

  // The drawer is modal, and inside it the only thing to move between is the marks: `n` and
  // `N` are the same pair the two steppers in its header press.
  if (drawerEl.open || !drawerEl.hidden) {
    if ((ev.key === "n" || ev.key === "N") && !isTyping(ev.target) && typeof drawerBodyEl.__step === "function") {
      ev.preventDefault();
      drawerBodyEl.__step(ev.key === "n" ? 1 : -1);
    }
    return;
  }

  if (ev.key === "/" && !isTyping(ev.target)) {
    ev.preventDefault();
    qEl.focus();
    qEl.select();
    return;
  }

  if (isTyping(ev.target)) return;

  if (ev.key === "j") {
    ev.preventDefault();
    setSelected(selected + 1);
  } else if (ev.key === "k") {
    ev.preventDefault();
    setSelected(selected <= 0 ? 0 : selected - 1);
  } else if (ev.key === "Enter" && selected >= 0) {
    const card = cards()[selected];
    if (card && typeof card.__expand === "function") {
      ev.preventDefault();
      card.__expand();
    }
  }
});

// ─── Theme ────────────────────────────────────────────────────────────────────────────────
//
// The inline script in `index.html` applies the stored choice before first paint; this only
// has to flip and store it. Absent means "follow the OS", so the first press has to decide
// which way to go from what is actually on screen rather than from an unset key.

themeEl.addEventListener("click", () => {
  const explicit = document.documentElement.dataset.theme;
  const dark = explicit ? explicit === "dark" : window.matchMedia("(prefers-color-scheme: dark)").matches;
  const next = dark ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try {
    localStorage.setItem("ss-theme", next);
  } catch (_) {
    // Storage denied (private mode, blocked site data). The toggle still works for this page
    // view, which is more useful than refusing to switch at all.
  }
});

// ─── Wiring and boot ──────────────────────────────────────────────────────────────────────

qEl.addEventListener("input", () => {
  state.q = qEl.value;
  searchSoon();
});

qEl.addEventListener("keydown", (ev) => {
  if (ev.key !== "Enter") return;
  // Enter means "now", so it jumps the debounce and counts as a navigation worth a history
  // entry — the back button should return to the search before this one, not to a keystroke.
  // Cancelling first matters: the keystroke that preceded Enter has a call already scheduled,
  // and letting it land repeats the identical request and overwrites the pushed history entry.
  ev.preventDefault();
  searchSoon.cancel();
  state.q = qEl.value;
  search({ push: true });
});

sortEl.addEventListener("change", () => {
  state.sort = SORTS.includes(sortEl.value) ? sortEl.value : "relevance";
  search({ push: true });
});

groupEl.addEventListener("change", () => {
  state.group = GROUPS.includes(groupEl.value) ? groupEl.value : "turn";
  search({ push: true });
});

window.addEventListener("popstate", () => {
  applyParams(new URLSearchParams(location.search));
  syncControls();
  selected = -1;
  runSearch();
});

applyParams(new URLSearchParams(location.search));
syncControls();
loadStats();
runSearch();
