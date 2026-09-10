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
//   `text`, `renderMarkdown` or the renderers in `tools.js`.

import {
  basename,
  clampText,
  copyText,
  debounce,
  dirname,
  el,
  fmtBytes,
  fmtCount,
  fmtTime,
  frag,
  iconFor,
  relTime,
} from "./dom.js";
import { renderMarkdown } from "./markdown.js";
import { renderToolCall, renderToolResult, toolMeta } from "./tools.js";

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

// ─── Element handles ──────────────────────────────────────────────────────────────────────

const byId = (id) => document.getElementById(id);

const qEl = byId("q");
const sortEl = byId("sort");
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
  includeThinking: false,
  filters: {
    tool: [],
    tool_input: [],
    project: "",
    model: "",
    role: "",
    kind: "",
    agent_type: "",
    branch: "",
    since: "",
    until: "",
    errors_only: false,
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
  if (state.size !== 20) p.set("size", String(state.size));
  for (const tool of state.filters.tool) p.append("tool", tool);
  for (const ti of state.filters.tool_input) p.append("tool_input", ti);
  for (const key of ["project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    if (state.filters[key]) p.set(key, state.filters[key]);
  }
  if (state.filters.errors_only) p.set("errors_only", "1");
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
  state.includeThinking = p.get("include_thinking") === "1";
  state.filters.tool = p.getAll("tool").filter(Boolean);
  state.filters.tool_input = p.getAll("tool_input").filter(Boolean);
  for (const key of ["project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    state.filters[key] = p.get(key) || "";
  }
  state.filters.errors_only = p.get("errors_only") === "1";
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
  for (const key of ["project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    if (f[key]) filters[key] = f[key];
  }
  if (f.errors_only) filters.errors_only = true;
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
  const elapsed = data.info && Number.isFinite(data.info.elapsedMs) ? `${data.info.elapsedMs} ms` : "";
  if (total === 0) {
    summaryEl.replaceChildren();
    return;
  }
  // The total is the whole matching set, not the page: `pagingStart`/`pagingEnd` say which
  // slice is on screen, and reporting only the slice would understate the corpus by 20x.
  summaryEl.replaceChildren(
    el("strong", { text: fmtCount(total) }),
    ` ${total === 1 ? "document" : "documents"} matched`,
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
  for (const key of ["project", "model", "role", "kind", "agent_type", "branch", "since", "until"]) {
    state.filters[key] = "";
  }
  state.filters.errors_only = false;
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

function renderResults(data) {
  const results = Array.isArray(data.results) ? data.results : [];
  if (!results.length) {
    resultsEl.replaceChildren();
    showEmpty();
    return;
  }
  emptyEl.hidden = true;
  resultsEl.replaceChildren(...results.map((result, i) => renderCard(result, i)));
}

/**
 * One hit.
 *
 * The rendered body is `_meta.doc` — a whole `ApiDoc` — rather than the flattened `{raw}`
 * fields beside it. Those exist so a stock Search UI result template works against this server
 * without knowing anything about transcripts; this UI knows about transcripts.
 */
function renderCard(result, index) {
  const doc = (result._meta && result._meta.doc) || {};
  const meta = result._meta || {};

  const threadHost = el("div");
  const rawHost = el("div");
  let windowSize = 0;

  const expandBtn = el("button", {
    class: "ss-more",
    type: "button",
    text: "expand",
    on: { click: () => expand() },
  });
  const collapseBtn = el("button", {
    class: "ss-more",
    type: "button",
    text: "collapse",
    hidden: true,
    on: {
      click: () => {
        windowSize = 0;
        threadHost.replaceChildren();
        collapseBtn.hidden = true;
        expandBtn.textContent = "expand";
      },
    },
  });

  async function expand() {
    if (!Number.isFinite(Number(doc.seq)) || !doc.session_id) {
      // `seq` is what a window is centred on; without it there is nothing to expand around,
      // and letting the request go would spend a round trip to be told the same thing.
      threadHost.replaceChildren(
        el("p", {
          class: "ss-hint",
          text: "this hit carries no session and seq, so there is no position in a transcript to expand around",
        }),
      );
      return;
    }
    // The first press opens a small window and each further press widens it, which is how you
    // read outwards from a hit without reloading the whole session into the page.
    windowSize = windowSize === 0 ? WINDOW_START : windowSize + WINDOW_STEP;
    expandBtn.disabled = true;
    expandBtn.textContent = "loading…";
    threadHost.replaceChildren(el("div", { class: "ss-loading" }, el("span", { class: "ss-spinner" }), "loading turns…"));
    try {
      const data = await fetchAround(doc, windowSize, windowSize, false);
      const docs = Array.isArray(data.docs) ? data.docs : [];
      threadHost.replaceChildren(
        el(
          "div",
          { class: "ss-thread" },
          ...docs.map((turn) =>
            renderTurn(turn, { focused: turn.doc_id === doc.doc_id, card: false }),
          ),
        ),
      );
      collapseBtn.hidden = false;
      expandBtn.textContent = `wider (±${windowSize + WINDOW_STEP})`;
    } catch (err) {
      if (err === SUPERSEDED) return;
      windowSize = Math.max(0, windowSize - WINDOW_STEP);
      threadHost.replaceChildren(el("p", { class: "ss-err", text: failureText(err) }));
      expandBtn.textContent = windowSize ? `wider (±${windowSize + WINDOW_STEP})` : "expand";
    } finally {
      expandBtn.disabled = false;
      // Whatever happened above, the button must stop claiming to be loading — a control stuck
      // on "loading…" reads as a hung request rather than as one that already came back.
      if (expandBtn.textContent === "loading…") {
        expandBtn.textContent = windowSize ? `wider (±${windowSize + WINDOW_STEP})` : "expand";
      }
    }
  }

  const card = el(
    "article",
    {
      class: "ss-card",
      tabindex: "-1",
      dataset: { index: String(index), error: doc.is_error ? "1" : null },
      on: { click: () => setSelected(index, false) },
    },
    el("div", { class: "ss-card-head" }, ...badges(doc)),
    el(
      "div",
      { class: "ss-card-body" },
      metaLine(doc),
      snippetBlock(result, meta, doc),
      el(
        "div",
        { class: "ss-meta" },
        expandBtn,
        collapseBtn,
        el("button", {
          class: "ss-more",
          type: "button",
          text: "open session",
          on: { click: () => openDrawer(doc) },
        }),
        rawButton(doc, rawHost),
      ),
      rawHost,
      threadHost,
    ),
  );
  card.__expand = expand;
  return card;
}

function badges(doc) {
  const out = [];
  if (doc.role) {
    out.push(el("span", { class: "ss-badge ss-badge-role", dataset: { role: doc.role }, text: doc.role }));
  }
  if (doc.tool_name) {
    const tool = toolMeta(doc);
    out.push(
      el(
        "span",
        {
          class: "ss-badge ss-badge-tool",
          dataset: { accent: tool.accent },
          title: doc.tool_name,
        },
        iconFor(tool.icon),
        el("span", { text: clampText(tool.label, 22) }),
      ),
    );
  }
  if (doc.is_error) out.push(el("span", { class: "ss-badge is-error", text: "error" }));
  if (doc.is_sidechain) out.push(el("span", { class: "ss-badge", text: "sidechain" }));
  if (doc.agent_type) out.push(el("span", { class: "ss-badge", text: clampText(doc.agent_type, 18) }));
  return out;
}

function metaLine(doc) {
  const parts = [];
  if (doc.project) {
    parts.push(
      el(
        "span",
        { class: "ss-path", title: doc.project },
        el("span", { class: "ss-path-dir", text: dirname(doc.project) }),
        el("span", { class: "ss-path-base", text: basename(doc.project) }),
      ),
    );
  }
  if (doc.git_branch) parts.push(el("span", { text: doc.git_branch }));
  if (doc.model) parts.push(el("span", { text: clampText(doc.model, 28) }));
  if (Number.isFinite(doc.timestamp_ms)) {
    parts.push(el("span", { title: fmtTime(doc.timestamp_ms), text: relTime(doc.timestamp_ms) }));
  }
  parts.push(el("span", { title: doc.doc_id || "", text: sessionKey(doc) }));
  return el("div", { class: "ss-meta" }, ...parts);
}

/**
 * The excerpt, labelled with the body it was cut from.
 *
 * `_meta.snippetField` says whether the words matched in the turn's text, in what a tool
 * printed, or in what the model was thinking. Those are three different claims about what
 * happened, and a card that showed thinking as if it were assistant text — or a command's
 * output as if the model had said it — would be misattributing a quote.
 */
function snippetBlock(result, meta, doc) {
  const field = meta.snippetField || "text";
  const entry = result[field];
  const html = entry && typeof entry.snippet === "string" ? entry.snippet : null;

  const label =
    field === "tool_output"
      ? { text: "matched in what the tool printed", accent: "warn" }
      : field === "thinking"
        ? { text: "matched in the model's thinking", accent: "accent" }
        : null;

  const nodes = [];
  if (label) {
    nodes.push(el("span", { class: "ss-badge ss-badge-tool", dataset: { accent: label.accent }, text: label.text }));
  }
  if (html !== null) {
    // The one place `html` is used. `dto::highlight_html` escapes the text and then wraps the
    // matched spans in `<em>`; anything else on this page goes through `text`.
    nodes.push(el("p", { class: "ss-snippet", html }));
  } else {
    // A hit with no snippet is normal for a filter-only browse, where there are no query terms
    // to mark. Showing the head of the body is better than showing an empty card.
    const body = doc.kind === "tool_call" ? doc.tool_output || doc.text : doc.text;
    nodes.push(el("p", { class: "ss-snippet", text: clampText(body || "(no indexed text)", 400) }));
  }
  return frag(...nodes);
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

/** One turn, as a nested thread entry or as a card in the drawer. */
function renderTurn(doc, { focused = false, card = false } = {}) {
  const node = el(card ? "article" : "div", {
    class: [card ? "ss-card" : "ss-thread-turn", focused ? "ss-focus" : null],
    dataset: { docId: doc.doc_id || "", error: doc.is_error ? "1" : null },
  });
  const head = el("div", { class: card ? "ss-card-head" : "ss-meta" }, ...badges(doc));
  const body = el("div", { class: "ss-card-body" }, metaLine(doc), ...turnContent(doc));
  node.append(head, body);
  return node;
}

function turnContent(doc) {
  const out = [];
  let rendered = false;
  if (doc.kind === "tool_call") {
    out.push(renderToolCall(doc));
    const result = renderToolResult(doc);
    if (result) out.push(result);
    rendered = true;
  } else if (doc.text && doc.text.trim()) {
    out.push(el("div", {}, renderMarkdown(doc.text)));
    rendered = true;
  }

  if (doc.thinking && doc.thinking.trim()) {
    rendered = true;
    // Behind a disclosure and never open: thinking is the model's scratch work, it is long, and
    // presenting it inline alongside the turn invites reading it as something that was said.
    out.push(
      el(
        "details",
        {},
        el("summary", { text: `thinking (${fmtCount(doc.thinking.length)} characters)` }),
        el("div", {}, renderMarkdown(doc.thinking)),
      ),
    );
  }

  const host = el("div");
  out.push(el("div", { class: "ss-meta" }, rawButton(doc, host)), host);

  if (!rendered) {
    // A record this build indexed but has no view for. Saying so points at the affordance that
    // always works, rather than leaving a turn that looks empty because it contained nothing.
    out.unshift(
      el("p", {
        class: "ss-hint",
        text: "no text here that this view knows how to render — the raw JSONL line is the record itself",
      }),
    );
  }
  return out;
}

/**
 * The graceful-degradation floor.
 *
 * Every renderer above can decline: an unknown MCP tool, a `tool_input` that is a number, a
 * record shape this build predates. The original JSONL line is the one representation that is
 * always right, so it is one press away from every turn rather than a thing you go to the
 * terminal for.
 */
function rawButton(doc, host) {
  let shown = false;
  const button = el("button", {
    class: "ss-more",
    type: "button",
    text: "raw JSON",
    on: {
      click: async () => {
        if (shown) {
          host.replaceChildren();
          shown = false;
          button.textContent = "raw JSON";
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
          button.textContent = "hide raw";
        } catch (err) {
          if (err !== SUPERSEDED) host.replaceChildren(el("p", { class: "ss-err", text: failureText(err) }));
        } finally {
          button.disabled = false;
        }
      },
    },
  });
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
    const nodes = docs.map((turn) => renderTurn(turn, { card: true, focused: turn.doc_id === doc.doc_id }));
    if (data.truncated) {
      nodes.push(
        el("p", {
          class: "ss-hint",
          text: `the first ${fmtCount(docs.length)} turns; this session continues past them`,
        }),
      );
    }
    drawerBodyEl.replaceChildren(...nodes);
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

function showDrawer() {
  drawerEl.hidden = false;
  if (typeof drawerEl.showModal === "function" && !drawerEl.open) drawerEl.showModal();
}

function closeDrawer() {
  if (drawerEl.open) drawerEl.close();
  drawerEl.hidden = true;
  drawerBodyEl.replaceChildren();
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

  const panel = el(
    "section",
    { class: "ss-facet" },
    el("div", { class: "ss-facet-head" }, el("span", { text: "filters" })),
    el("div", {}, ...rows),
  );
  facetsEl.parentNode.insertBefore(panel, facetsEl);

  return { toolInput, since, until, errors, sidechain, thinking };
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
  controls.since.value = state.filters.since;
  controls.until.value = state.filters.until;
  controls.errors.checked = state.filters.errors_only;
  controls.sidechain.value = state.filters.sidechain;
  controls.thinking.checked = state.includeThinking;
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
  // The drawer is modal: every other shortcut here acts on the result list behind it.
  if (drawerEl.open || !drawerEl.hidden) return;

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
