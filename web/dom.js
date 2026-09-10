// Shared DOM primitives. Imported by app.js, tools.js and markdown.js; imports nothing
// itself, so it is the one file that can be written before the others and never has to
// change when they are.
//
// Everything here builds nodes through the DOM rather than through strings. The one
// exception is `props.html`, which every caller must have escaped already — the snippet the
// server sends is escaped-then-marked-up, and it is the only HTML this app trusts.

/** `el("div", {class: "x", text: "hi"}, child, child)`. Null/undefined children are skipped. */
export function el(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    switch (key) {
      case "class":
        node.className = Array.isArray(value) ? value.filter(Boolean).join(" ") : value;
        break;
      case "text":
        node.textContent = String(value);
        break;
      case "html":
        node.innerHTML = value; // caller-escaped; see the note above
        break;
      case "dataset":
        for (const [k, v] of Object.entries(value)) {
          if (v !== null && v !== undefined) node.dataset[k] = String(v);
        }
        break;
      case "on":
        for (const [event, fn] of Object.entries(value)) node.addEventListener(event, fn);
        break;
      case "style":
        if (typeof value === "string") node.setAttribute("style", value);
        else Object.assign(node.style, value);
        break;
      default:
        if (value === true) node.setAttribute(key, "");
        else node.setAttribute(key, String(value));
    }
  }
  append(node, children);
  return node;
}

export function frag(...children) {
  const f = document.createDocumentFragment();
  append(f, children);
  return f;
}

function append(parent, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    parent.appendChild(child instanceof Node ? child : document.createTextNode(String(child)));
  }
}

export function escapeHtml(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function clampText(s, max) {
  const text = String(s ?? "");
  return text.length <= max ? text : text.slice(0, Math.max(0, max - 1)) + "…";
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

export function fmtTime(ms) {
  if (!Number.isFinite(ms)) return "";
  const d = new Date(ms);
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getDate()} ${MONTHS[d.getMonth()]} ${d.getFullYear()}, ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function fmtClock(ms) {
  if (!Number.isFinite(ms)) return "";
  const d = new Date(ms);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

const SPANS = [
  [31536000000, "year"],
  [2592000000, "month"],
  [604800000, "week"],
  [86400000, "day"],
  [3600000, "hour"],
  [60000, "minute"],
];

export function relTime(ms) {
  if (!Number.isFinite(ms)) return "";
  const delta = Date.now() - ms;
  if (delta < 60000) return "just now";
  for (const [span, name] of SPANS) {
    if (delta >= span) {
      const n = Math.floor(delta / span);
      return `${n} ${name}${n === 1 ? "" : "s"} ago`;
    }
  }
  return "just now";
}

export function basename(path) {
  const s = String(path ?? "");
  const cut = s.lastIndexOf("/");
  return cut === -1 ? s : s.slice(cut + 1);
}

export function dirname(path) {
  const s = String(path ?? "");
  const cut = s.lastIndexOf("/");
  return cut <= 0 ? "" : s.slice(0, cut + 1);
}

const LANGS = {
  rs: "rust", ts: "ts", tsx: "ts", js: "js", jsx: "js", mjs: "js", py: "python",
  go: "go", rb: "ruby", java: "java", c: "c", h: "c", cc: "cpp", cpp: "cpp", hpp: "cpp",
  cs: "csharp", swift: "swift", kt: "kotlin", php: "php", sh: "bash", bash: "bash",
  zsh: "bash", fish: "bash", sql: "sql", html: "html", css: "css", scss: "css",
  json: "json", yaml: "yaml", yml: "yaml", toml: "toml", md: "markdown", lock: "toml",
  Dockerfile: "docker", Makefile: "make",
};

/** A token for `.ss-code[data-lang]`; purely a label, nothing here highlights syntax. */
export function langFor(path) {
  const base = basename(path);
  if (LANGS[base]) return LANGS[base];
  const dot = base.lastIndexOf(".");
  if (dot === -1) return "";
  return LANGS[base.slice(dot + 1)] || "";
}

export function copyText(text) {
  try {
    if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text);
  } catch (_) {
    /* fall through to the textarea route */
  }
  // http://127.0.0.1 is a secure context in every current browser, but a user who reached
  // this over a LAN address is not, and losing "copy" there would be a silly way to fail.
  return new Promise((resolve) => {
    const ta = el("textarea", { style: "position:fixed;opacity:0", text });
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
    } catch (_) {
      /* nothing else to try */
    }
    ta.remove();
    resolve();
  });
}

// 16x16 line icons, stroke-driven so they inherit `currentColor`.
const ICONS = {
  search: "M11 11 15 15M7 12.5A5.5 5.5 0 1 0 7 1.5a5.5 5.5 0 0 0 0 11Z",
  terminal: "M2.5 3.5 6 8l-3.5 4.5M8.5 12.5h5",
  file: "M4 1.5h5l3 3v10H4zM9 1.5v3h3",
  edit: "M2.5 11 11 2.5l2.5 2.5L5 13.5l-3 .5z",
  search_code: "M6.5 11.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9ZM10 10l3.5 3.5",
  globe: "M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13ZM1.5 8h13M8 1.5c3.5 4 3.5 9 0 13-3.5-4-3.5-9 0-13Z",
  robot: "M4 5.5h8v7H4zM8 2v3.5M6 8.5h.01M10 8.5h.01",
  check: "M3 8.5 6.5 12 13 4",
  list: "M5.5 4h8M5.5 8h8M5.5 12h8M2.5 4h.01M2.5 8h.01M2.5 12h.01",
  book: "M3 2.5h4.5A1.5 1.5 0 0 1 9 4v9.5H4.5A1.5 1.5 0 0 1 3 12zM13 2.5H9.5",
  plug: "M6 2v4M10 2v4M4 6h8v2a4 4 0 0 1-8 0zM8 12v2.5",
  chevron: "M6 3.5 10.5 8 6 12.5",
  braces: "M6.5 2.5c-1.5 0-1.5 1.7-1.5 3S4 8 3 8s2 .2 2 2.5 0 3 1.5 3M9.5 2.5c1.5 0 1.5 1.7 1.5 3S12 8 13 8s-2 .2-2 2.5 0 3-1.5 3",
  close: "M4 4l8 8M12 4l-8 8",
  copy: "M5.5 5.5h8v8h-8zM2.5 10.5v-8h8",
  link: "M6.5 9.5a3 3 0 0 0 4.2 0l2-2a3 3 0 0 0-4.2-4.2l-.8.8M9.5 6.5a3 3 0 0 0-4.2 0l-2 2a3 3 0 0 0 4.2 4.2l.8-.8",
  clock: "M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13ZM8 4.5V8l2.5 1.5",
  warn: "M8 2 15 14H1zM8 6.5v3.5M8 12h.01",
  user: "M8 8a2.75 2.75 0 1 0 0-5.5 2.75 2.75 0 0 0 0 5.5ZM2.5 14a5.5 5.5 0 0 1 11 0",
  spark: "M8 1.5 9.6 6.4 14.5 8l-4.9 1.6L8 14.5 6.4 9.6 1.5 8l4.9-1.6z",
  gear: "M8 10.2a2.2 2.2 0 1 0 0-4.4 2.2 2.2 0 0 0 0 4.4ZM8 1.5v1.6M8 12.9v1.6M1.5 8h1.6M12.9 8h1.6M3.4 3.4l1.1 1.1M11.5 11.5l1.1 1.1M12.6 3.4l-1.1 1.1M4.5 11.5l-1.1 1.1",
  brain: "M6.5 2.5a2 2 0 0 0-2 2 2 2 0 0 0-1 3.6A2 2 0 0 0 5 11.6a2 2 0 0 0 3.5.9V3.6a2 2 0 0 0-2-1.1ZM9.5 2.5a2 2 0 0 1 2 2 2 2 0 0 1 1 3.6 2 2 0 0 1-1.5 3.5",
  tool: "M10.5 2.5a3.5 3.5 0 0 0-4.6 4.4L2 10.8l1.8 1.8 3.9-3.9a3.5 3.5 0 0 0 4.4-4.6L10 6l-1.4-.6L8 4z",
};

/** An inline `<svg>`; unknown names give the generic tool glyph rather than nothing. */
export function iconFor(name) {
  const path = ICONS[name] || ICONS.tool;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("class", "ss-icon");
  svg.setAttribute("aria-hidden", "true");
  const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
  p.setAttribute("d", path);
  p.setAttribute("fill", "none");
  p.setAttribute("stroke", "currentColor");
  p.setAttribute("stroke-width", "1.5");
  p.setAttribute("stroke-linecap", "round");
  p.setAttribute("stroke-linejoin", "round");
  svg.appendChild(p);
  return svg;
}

export function debounce(fn, ms) {
  let timer = 0;
  const run = (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
  /** Drop a scheduled call. For the caller who decided to act *now* and would otherwise get
   *  the debounced one landing on top of it a moment later. */
  run.cancel = () => clearTimeout(timer);
  return run;
}

/** Bytes as a short human string. */
export function fmtBytes(n) {
  if (!Number.isFinite(n)) return "";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** `12345` -> `12,345`. */
export function fmtCount(n) {
  return Number.isFinite(n) ? n.toLocaleString() : "";
}
