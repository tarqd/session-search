//! Wire shapes for the HTTP API: query-string decoding, the Elastic Search UI request and
//! response envelope, and the JSON rendering of documents, facets and sessions.
//!
//! This half is pure data on purpose. It never imports `axum`, and it reports every caller
//! mistake as a `String` that `mod.rs` turns into a `400`, so every wire-shape decision is
//! testable without a socket or an index.
//!
//! One rule runs through the whole module: a request this server did not honour in full must
//! never come back looking like an honest empty result. An unrecognised parameter, an unknown
//! filter field, a sort this index cannot perform, a filter it cannot express — all of those are
//! errors that name what *was* accepted. Only what costs the caller nothing (an unknown key in
//! the Search UI envelope, which that library adds to over time) degrades to `info.warnings`.

use std::ops::Range;

use serde_json::{Map, Value, json};

use crate::parse::{Doc, SessionInfo};
use crate::search::{FacetResult, Filters, HIGHLIGHT, Hit, SearchRequest, SearchResponse, SortBy};

// ---------------------------------------------------------------------------
// query strings
// ---------------------------------------------------------------------------

/// A decoded query string with repeats preserved (`?tool=Bash&tool=Read`).
///
/// A map would be the obvious type and the wrong one: `tool`, `tool_input`, `tool_output` and
/// `facets` are all repeatable, and collapsing them to one value each would drop filters the
/// caller asked for without saying so.
#[derive(Debug, Clone, Default)]
pub struct Params(Vec<(String, String)>);

impl Params {
    /// `raw` is the query string as the router hands it over, with or without its leading `?`.
    pub fn parse(raw: Option<&str>) -> Result<Params, String> {
        let raw = raw.unwrap_or("").trim_start_matches('?');
        if raw.is_empty() {
            return Ok(Params(Vec::new()));
        }
        serde_urlencoded::from_str::<Vec<(String, String)>>(raw)
            .map(Params)
            .map_err(|err| format!("cannot read the query string: {err}"))
    }

    pub fn first(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn all(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Absent -> false; present-and-empty, `1`, `true`, `yes`, `on` -> true; `0`, `false`, `no`,
    /// `off` -> false; anything else is an error naming the key.
    ///
    /// Present-and-empty is true because `?errors_only` is how a flag is written by hand, and
    /// most URL builders spell that `?errors_only=`.
    pub fn flag(&self, key: &str) -> Result<bool, String> {
        let Some(raw) = self.first(key) else {
            return Ok(false);
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(format!(
                "{key}={other:?} is not a yes/no value; use 1/true/yes/on, 0/false/no/off, \
                 or {key} on its own"
            )),
        }
    }

    /// An empty value (`?limit=`) reads as *absent*, the way an untouched form field does. A
    /// value that is present but not a number is an error: silently falling back to the default
    /// would answer a different question than the one that was asked.
    pub fn number<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, String> {
        match self.first(key).map(str::trim) {
            None | Some("") => Ok(None),
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|_| format!("{key}={raw:?} is not a number")),
        }
    }

    /// A key outside `allowed` is an error listing `allowed`, never a silent no-op.
    ///
    /// This is the difference between `?tool_name=Bash` returning nothing because no Bash call
    /// matched and returning everything because the filter was never applied.
    pub fn reject_unknown(&self, allowed: &[&str]) -> Result<(), String> {
        let mut unknown: Vec<&str> = Vec::new();
        for (key, _) in &self.0 {
            if !allowed.contains(&key.as_str()) && !unknown.contains(&key.as_str()) {
                unknown.push(key);
            }
        }
        if unknown.is_empty() {
            return Ok(());
        }
        let plural = if unknown.len() == 1 { "" } else { "s" };
        let listed = unknown
            .iter()
            .map(|k| format!("{k:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "unknown query parameter{plural} {listed}; this endpoint accepts: {}",
            allowed.join(", ")
        ))
    }
}

/// Query-string keys `GET /api/search` accepts. `GET /api/facets/{field}` accepts these plus
/// `top`.
///
/// [`SearchBody::from_params`] reads the keys it knows and ignores the rest, so the handler has
/// to call [`Params::reject_unknown`] with this list — otherwise `?tool_nmae=Bash` would widen
/// the search to the whole corpus and look like a legitimate answer.
pub const SEARCH_PARAMS: &[&str] = &[
    "q",
    "page",
    "size",
    "offset",
    "sort",
    "facets",
    "facet_top",
    "snippet_chars",
    "include_thinking",
    "project",
    "tool",
    "tool_input",
    "tool_output",
    "lang",
    "program",
    "min_thinking",
    "branch",
    "model",
    "role",
    "kind",
    "session",
    "agent_type",
    "since",
    "until",
    "errors_only",
    "no_sidechains",
    "sidechains_only",
];

/// Query-string keys `GET /api/sessions` accepts — the subset of the filters a session listing
/// can answer, since `sessions.json` carries no tool, role or kind.
pub const SESSION_LIST_PARAMS: &[&str] = &[
    "limit",
    "project",
    "session",
    "branch",
    "model",
    "agent_type",
    "since",
    "until",
    "no_sidechains",
    "sidechains_only",
];

/// Sessions shown by `GET /api/sessions` when the caller does not say. Matches `sessions
/// --limit`, so the two front doors agree.
const SESSION_LIST_LIMIT: usize = 50;

// ---------------------------------------------------------------------------
// search request
// ---------------------------------------------------------------------------

/// Elastic Search UI `RequestState` + `queryConfig`, plus this index's own knobs.
///
/// `filters` and `facets` are held as raw JSON rather than typed fields because both accept two
/// shapes, and because `serde`'s own error for a mistyped key inside them would be a type
/// complaint rather than "this index has no such filter". They are decoded in [`Self::prepare`],
/// where the message can name the field.
///
/// Unknown keys are kept in `extra` and surface as `info.warnings`, never as a silent drop.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchBody {
    pub search_term: Option<String>,
    /// 1-based page number, as Search UI counts.
    pub current: Option<usize>,
    pub results_per_page: Option<usize>,
    /// 0-based, and this index's own: `current` covers paging, this covers "the 20 after 137".
    pub offset: Option<usize>,
    pub filters: Option<Value>,
    pub facets: Option<Value>,
    pub sort_list: Option<Vec<Value>>,
    pub sort_field: Option<String>,
    pub sort_direction: Option<String>,
    pub include_thinking: Option<bool>,
    pub snippet_chars: Option<usize>,
    pub facet_top: Option<usize>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct PreparedSearch {
    pub request: SearchRequest,
    /// 1-based page and its size, echoed back as `current` / `resultsPerPage`.
    pub page: usize,
    pub size: usize,
    pub warnings: Vec<String>,
}

impl SearchBody {
    /// The `GET /api/search` spelling of the same request. Repeatable keys are repeated
    /// (`?tool=Bash&tool=Read`), never comma-joined, except `facets` which is a field list.
    ///
    /// An empty value reads as absent throughout, matching [`Params::number`]: a URL built from
    /// a form should not have to omit the boxes nobody filled in.
    pub fn from_params(p: &Params) -> Result<SearchBody, String> {
        let filters = Filters {
            project: text(p, "project"),
            tool: texts(p, "tool"),
            tool_input: texts(p, "tool_input"),
            tool_output: texts(p, "tool_output"),
            lang: texts(p, "lang"),
            program: texts(p, "program"),
            min_thinking: p.number::<u64>("min_thinking")?,
            branch: text(p, "branch"),
            model: text(p, "model"),
            role: text(p, "role"),
            kind: text(p, "kind"),
            session: text(p, "session"),
            agent_type: text(p, "agent_type"),
            since: text(p, "since"),
            until: text(p, "until"),
            errors_only: p.flag("errors_only")?,
            no_sidechains: p.flag("no_sidechains")?,
            sidechains_only: p.flag("sidechains_only")?,
        };

        // `--sort` on the CLI names the ordering; `sortList` on the wire names a field and a
        // direction. Both spellings are accepted and both end up in the same two fields, so
        // `prepare` validates them in one place and reports them in one voice.
        let (sort_field, sort_direction) = match p.first("sort").map(str::trim) {
            None | Some("") => (None, None),
            Some(raw) => match raw.to_ascii_lowercase().as_str() {
                "relevance" | "_score" => (Some("relevance".to_string()), None),
                "newest" | "-timestamp" | "timestamp:desc" => {
                    (Some("timestamp".to_string()), Some("desc".to_string()))
                }
                "oldest" | "+timestamp" | "timestamp:asc" => {
                    (Some("timestamp".to_string()), Some("asc".to_string()))
                }
                // Anything else travels verbatim so an unsortable field is refused by the same
                // check, with the same message, as `sortField` in a POST body.
                _ => (Some(raw.to_string()), None),
            },
        };

        // `?facets=a,b&facets=c` — a field list, so commas split and repeats accumulate.
        let facets: Vec<Value> = p
            .all("facets")
            .iter()
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| Value::String(v.to_string()))
            .collect();

        Ok(SearchBody {
            search_term: text(p, "q"),
            current: p.number::<usize>("page")?,
            results_per_page: p.number::<usize>("size")?,
            offset: p.number::<usize>("offset")?,
            filters: Some(
                serde_json::to_value(&filters).expect("Filters holds only JSON-native values"),
            ),
            facets: (!facets.is_empty()).then_some(Value::Array(facets)),
            sort_list: None,
            sort_field,
            sort_direction,
            include_thinking: p.flag("include_thinking")?.then_some(true),
            snippet_chars: p.number::<usize>("snippet_chars")?,
            facet_top: p.number::<usize>("facet_top")?,
            extra: Map::new(),
        })
    }

    pub fn prepare(self) -> Result<PreparedSearch, String> {
        let mut warnings: Vec<String> = Vec::new();
        for key in self.extra.keys() {
            warnings.push(format!("ignored unknown request key {key:?}"));
        }

        let filters = decode_filters(self.filters)?;
        // `clap` refuses this pair on the command line; the same request over HTTP asks for
        // "only sidechains, and no sidechains", which has exactly one honest answer.
        if filters.no_sidechains && filters.sidechains_only {
            return Err(
                "no_sidechains and sidechains_only contradict each other; set at most one".into(),
            );
        }

        // `since` and `until` are parsed here rather than left to `search::search`, where the
        // same failure arrives as an `anyhow` error and this boundary renders it as a `500`. A
        // caller typing a date hits every prefix of it on the way — `2026`, `2026-0`, `7` — and
        // a `500` claims the index or the disk broke, logs an error, and invites a retry that
        // can never succeed. The value is the caller's mistake and `/api/sessions` already
        // says so with a `400`.
        let now = chrono::Utc::now();
        for (name, raw) in [("since", &filters.since), ("until", &filters.until)] {
            let Some(raw) = raw.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
                continue;
            };
            // Named for the wire parameter: a caller of `?since=` has no `--since` flag to fix,
            // and `search::search` phrases the same failure against the CLI.
            crate::search::parse_when(raw, now).map_err(|err| format!("{name}: {err:#}"))?;
        }

        let sort = decode_sort(
            self.sort_list.as_deref(),
            self.sort_field.as_deref(),
            self.sort_direction.as_deref(),
            &mut warnings,
        )?;
        let (facets, facet_size) = decode_facets(self.facets, &mut warnings)?;

        let defaults = SearchRequest::default();
        let size = self.results_per_page.unwrap_or(defaults.limit);
        let page = match self.current {
            Some(0) => return Err("current is a 1-based page number, so 0 is not a page".into()),
            Some(page) => page,
            None => 1,
        };
        // `current` is 1-based and `offset` is 0-based; an explicit offset wins, and the page
        // number is recomputed from it so the echoed paging fields describe the page actually
        // returned rather than the one that was asked for.
        let (offset, page) = match self.offset {
            Some(offset) => {
                let from_page = page.saturating_sub(1).saturating_mul(size);
                if self.current.is_some() && offset != from_page {
                    warnings.push(format!(
                        "both current={page} and offset={offset} were given; offset wins"
                    ));
                }
                // `size` is 0 on an explicit `resultsPerPage=0`, which asks for totals and
                // facets and no hits. There is exactly one page of nothing, so it is page 1.
                let page = offset.checked_div(size).map_or(1, |page| page + 1);
                (offset, page)
            }
            None => (page.saturating_sub(1).saturating_mul(size), page),
        };

        let request = SearchRequest {
            query: self
                .search_term
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .map(str::to_string),
            filters,
            limit: size,
            offset,
            facets,
            facet_top: self
                .facet_top
                .or(facet_size)
                .unwrap_or(defaults.facet_top)
                .max(1),
            snippet_chars: self.snippet_chars.unwrap_or(defaults.snippet_chars),
            include_thinking: self.include_thinking.unwrap_or(defaults.include_thinking),
            sort,
            // Spelled out rather than left to `..defaults`, so that adding a field to
            // `SearchRequest` is a compile error here and somebody has to decide, once, whether
            // the HTTP envelope carries it. These two deliberately do not — see
            // `similar_to_is_not_a_search_parameter` and `group_by_turn_is_not_a_search_parameter`.
            similar_to: None,
            group_by_turn: false,
        };

        Ok(PreparedSearch {
            request,
            page,
            size,
            warnings,
        })
    }
}

fn text(p: &Params, key: &str) -> Option<String> {
    p.first(key)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn texts(p: &Params, key: &str) -> Vec<String> {
    p.all(key)
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// filters
// ---------------------------------------------------------------------------

/// The `Filters` keys the native object form accepts — the CLI flag names, minus their dashes.
const NATIVE_FILTER_KEYS: &[&str] = &[
    "project",
    "tool",
    "tool_input",
    "tool_output",
    "lang",
    "program",
    "min_thinking",
    "branch",
    "model",
    "role",
    "kind",
    "session",
    "agent_type",
    "since",
    "until",
    "errors_only",
    "no_sidechains",
    "sidechains_only",
];

/// The `field` values the Search UI array form accepts, for the message a typo gets.
const UI_FILTER_FIELDS: &str = "tool_name, tool, tool_output, tool_input.<path>, code_lang, lang, \
     bash_cmd.program, program, project, model, role, kind, agent_type, session_id, git_branch, \
     branch, timestamp, thinking_tokens, is_error, is_sidechain";

/// Either shape of `filters`: the Search UI array, or the native object the CLI flags spell.
fn decode_filters(value: Option<Value>) -> Result<Filters, String> {
    match value {
        None | Some(Value::Null) => Ok(Filters::default()),
        Some(Value::Array(list)) => ui_filters(&list),
        Some(Value::Object(map)) => native_filters(map),
        Some(_) => Err(
            "filters must be a Search UI array of {field, values} or the native filters object"
                .into(),
        ),
    }
}

/// `{ "tool": ["Bash"], "project": "~/code", "errors_only": true }` — `search::Filters` verbatim.
fn native_filters(map: Map<String, Value>) -> Result<Filters, String> {
    let mut normalized = Map::new();
    for (key, value) in map {
        if !NATIVE_FILTER_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "unknown filter field {key:?}; the native filters object accepts: {}",
                NATIVE_FILTER_KEYS.join(", ")
            ));
        }
        // A repeatable filter given one bare value is what everybody writes by hand, and
        // `serde` would answer it with "invalid type: string, expected a sequence".
        let value = match (key.as_str(), value) {
            ("tool" | "tool_input" | "tool_output" | "lang" | "program", v @ Value::String(_)) => {
                Value::Array(vec![v])
            }
            (_, v) => v,
        };
        normalized.insert(key, value);
    }
    serde_json::from_value(Value::Object(normalized))
        .map_err(|err| format!("cannot read the filters object: {err}"))
}

/// `[{ "field": "tool_name", "values": ["Bash"], "type": "any" }]` — the Search UI array form.
fn ui_filters(list: &[Value]) -> Result<Filters, String> {
    let mut f = Filters::default();
    for entry in list {
        let Some(entry) = entry.as_object() else {
            return Err("each filter must be an object of {field, values, type}".into());
        };
        for key in entry.keys() {
            if !matches!(key.as_str(), "field" | "values" | "type") {
                return Err(format!(
                    "unknown key {key:?} in a filter; a filter is {{field, values, type}}"
                ));
            }
        }
        let field = entry
            .get("field")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .ok_or_else(|| "every filter needs a non-empty \"field\"".to_string())?;
        let values = match entry.get("values") {
            Some(Value::Array(values)) if !values.is_empty() => values.as_slice(),
            Some(Value::Array(_)) | None => {
                return Err(format!("filter field {field:?} was given no values"));
            }
            Some(_) => {
                return Err(format!(
                    "filter field {field:?} needs \"values\" as an array"
                ));
            }
        };
        let kind = entry.get("type").and_then(Value::as_str).map(str::trim);

        match field {
            // Repeatable, ORed — so "all", which asks for a document whose single tool_name is
            // two different tools at once, cannot be honoured and is refused rather than widened.
            "tool_name" | "tool" => {
                check_type(field, kind, &["any"])?;
                for value in values {
                    f.tool.push(as_text(field, value)?);
                }
            }
            // Repeatable, ORed, like `tool`: a code block has one language and a command one
            // program, so "all" is the same unsatisfiable ask and gets the same refusal.
            "code_lang" | "lang" => {
                check_type(field, kind, &["any"])?;
                for value in values {
                    f.lang.push(as_text(field, value)?);
                }
            }
            "bash_cmd.program" | "program" => {
                check_type(field, kind, &["any"])?;
                for value in values {
                    f.program.push(as_text(field, value)?);
                }
            }
            // Repeatable, ANDed phrases.
            "tool_output" => {
                check_type(field, kind, &["all"])?;
                for value in values {
                    f.tool_output.push(as_text(field, value)?);
                }
            }
            "tool_input" => {
                check_type(field, kind, &["all"])?;
                for value in values {
                    let spec = as_text(field, value)?;
                    if !spec.contains('=') {
                        return Err(format!(
                            "filter {field:?} expects KEY=VALUE, or use the field \
                             \"tool_input.<path>\"; got {spec:?}"
                        ));
                    }
                    f.tool_input.push(spec);
                }
            }
            _ if field.starts_with("tool_input.") => {
                check_type(field, kind, &["all"])?;
                let path = field.trim_start_matches("tool_input.").trim();
                if path.is_empty() {
                    return Err("filter field \"tool_input.\" needs a path, e.g. \
                                \"tool_input.file_path\""
                        .into());
                }
                for value in values {
                    f.tool_input
                        .push(format!("{path}={}", as_text(field, value)?));
                }
            }
            "project" => set_single(&mut f.project, field, kind, values)?,
            "model" => set_single(&mut f.model, field, kind, values)?,
            "role" => set_single(&mut f.role, field, kind, values)?,
            "kind" => set_single(&mut f.kind, field, kind, values)?,
            "agent_type" => set_single(&mut f.agent_type, field, kind, values)?,
            "session_id" | "session" => set_single(&mut f.session, field, kind, values)?,
            "git_branch" | "branch" => set_single(&mut f.branch, field, kind, values)?,
            "timestamp" => {
                check_type(field, kind, &["any", "all"])?;
                let range = one_value(field, values)?;
                let range = range_object(field, range)?;
                let since = range_edge(range, "from")
                    .map(|v| date_edge(field, "from", v))
                    .transpose()?;
                let until = range_edge(range, "to")
                    .map(|v| date_edge(field, "to", v))
                    .transpose()?;
                if since.is_none() && until.is_none() {
                    return Err(format!(
                        "filter field {field:?} needs a range with \"from\" and/or \"to\""
                    ));
                }
                if f.since.is_some() || f.until.is_some() {
                    return Err(format!("filter field {field:?} was given twice"));
                }
                f.since = since;
                f.until = until;
            }
            "thinking_tokens" => {
                check_type(field, kind, &["any", "all"])?;
                let range = one_value(field, values)?;
                let range = range_object(field, range)?;
                // `Filters` has no upper bound to map an "at most N thinking tokens" onto, and
                // quietly dropping the `to` would answer a wider question than the one asked.
                if range_edge(range, "to").is_some() {
                    return Err(format!(
                        "filter field {field:?} has no upper bound; only \"from\" is supported"
                    ));
                }
                let from = range_edge(range, "from")
                    .ok_or_else(|| format!("filter field {field:?} needs a \"from\""))?;
                // A token count, not a date — the same `{from}` shape carries both, and reading
                // this one as epoch milliseconds would turn "5000 tokens" into 1970.
                let min = match from {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => s.trim().parse::<u64>().ok(),
                    _ => None,
                }
                .ok_or_else(|| {
                    format!("filter field {field:?}: from {from} is not a whole number")
                })?;
                if f.min_thinking.is_some() {
                    return Err(format!("filter field {field:?} was given twice"));
                }
                f.min_thinking = Some(min);
            }
            // This index can select errors but has no "everything that did not fail" filter, so
            // `[false]` is refused instead of being read as "no filter at all".
            "is_error" => {
                check_type(field, kind, &["any", "all"])?;
                if !as_bool(field, one_value(field, values)?)? {
                    return Err(format!(
                        "filter field {field:?} can only select errors: use values [true]"
                    ));
                }
                f.errors_only = true;
            }
            "is_sidechain" => {
                check_type(field, kind, &["any", "all"])?;
                if as_bool(field, one_value(field, values)?)? {
                    f.sidechains_only = true;
                } else {
                    f.no_sidechains = true;
                }
            }
            other => {
                return Err(format!(
                    "unknown filter field {other:?}; accepted fields are: {UI_FILTER_FIELDS}"
                ));
            }
        }
    }
    Ok(f)
}

/// Search UI's `type` says how a field's values combine. This index combines each field one
/// fixed way, so a `type` it cannot honour is refused rather than silently reinterpreted.
fn check_type(field: &str, kind: Option<&str>, allowed: &[&str]) -> Result<(), String> {
    match kind {
        None | Some("") => Ok(()),
        Some(kind) if allowed.contains(&kind) => Ok(()),
        Some(kind) => Err(format!(
            "filter field {field:?} does not support type {kind:?}; it accepts: {}",
            allowed.join(", ")
        )),
    }
}

fn set_single(
    slot: &mut Option<String>,
    field: &str,
    kind: Option<&str>,
    values: &[Value],
) -> Result<(), String> {
    check_type(field, kind, &["any", "all"])?;
    let value = as_text(field, one_value(field, values)?)?;
    // Two filters on the same single-valued field would mean the last one wins, and the caller
    // would never learn that the first was dropped.
    if slot.is_some() {
        return Err(format!("filter field {field:?} was given twice"));
    }
    *slot = Some(value);
    Ok(())
}

fn one_value<'a>(field: &str, values: &'a [Value]) -> Result<&'a Value, String> {
    match values {
        [only] => Ok(only),
        [] => Err(format!("filter field {field:?} was given no values")),
        many => Err(format!(
            "filter field {field:?} takes a single value, but {} were given",
            many.len()
        )),
    }
}

fn as_text(field: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(format!(
            "filter field {field:?} needs a non-empty text value, got {value}"
        )),
    }
}

fn as_bool(field: &str, value: &Value) -> Result<bool, String> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) if n.as_u64() == Some(1) => Ok(true),
        Value::Number(n) if n.as_u64() == Some(0) => Ok(false),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            _ => Err(format!(
                "filter field {field:?} needs true or false, got {value}"
            )),
        },
        _ => Err(format!(
            "filter field {field:?} needs true or false, got {value}"
        )),
    }
}

fn range_object<'a>(field: &str, value: &'a Value) -> Result<&'a Map<String, Value>, String> {
    let Some(range) = value.as_object() else {
        return Err(format!(
            "filter field {field:?} needs a range value such as {{\"from\": …, \"to\": …}}"
        ));
    };
    for key in range.keys() {
        // `name` is the label Search UI carries on a predefined range option; it says nothing
        // about which documents match, so accepting and ignoring it loses the caller nothing.
        if !matches!(key.as_str(), "from" | "to" | "name") {
            return Err(format!(
                "unknown key {key:?} in the {field:?} range; a range is {{from, to}}"
            ));
        }
    }
    Ok(range)
}

/// One end of a range, if the caller set it. `null` and `""` are how a Search UI range facet
/// spells an open end, so neither is a value.
fn range_edge<'a>(range: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    match range.get(key) {
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::Null) | None => None,
        Some(value) => Some(value),
    }
}

/// A date edge as `search::parse_when` will read it: a string passes through (`now`, `7d`,
/// `2026-01-01`, RFC3339), and a number is taken as epoch milliseconds — which is what Search
/// UI's own date range facets emit.
fn date_edge(field: &str, key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.trim().to_string()),
        Value::Number(n) => {
            let ms = n
                .as_i64()
                .ok_or_else(|| format!("filter field {field:?}: {key} {n} is not a timestamp"))?;
            let at = chrono::DateTime::from_timestamp_millis(ms)
                .ok_or_else(|| format!("filter field {field:?}: {key} {ms} is not a timestamp"))?;
            Ok(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        }
        other => Err(format!(
            "filter field {field:?}: {key} must be a date or epoch milliseconds, got {other}"
        )),
    }
}

// ---------------------------------------------------------------------------
// sort and facets
// ---------------------------------------------------------------------------

/// The only orderings this index can produce, spelled the way a caller may ask for them.
const SORTABLE: &str = "\"timestamp\" (or \"\" / \"_score\" / \"relevance\" for relevance)";

fn decode_sort(
    list: Option<&[Value]>,
    field: Option<&str>,
    direction: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<SortBy, String> {
    let (field, direction) = match list {
        Some([]) | None => (field, direction),
        Some([only]) => {
            if field.is_some() || direction.is_some() {
                warnings.push("both sortList and sortField were given; sortList wins".into());
            }
            let Some(entry) = only.as_object() else {
                return Err("each sortList entry is an object of {field, direction}".into());
            };
            for key in entry.keys() {
                if !matches!(key.as_str(), "field" | "direction") {
                    return Err(format!(
                        "unknown key {key:?} in sortList; an entry is {{field, direction}}"
                    ));
                }
            }
            (
                entry.get("field").and_then(Value::as_str),
                entry.get("direction").and_then(Value::as_str),
            )
        }
        // Nothing here can break a tie inside an ordering, so a second key would be accepted
        // and then never applied.
        Some(many) => {
            return Err(format!(
                "sortList takes a single key, but {} were given; this index sorts by {SORTABLE}",
                many.len()
            ));
        }
    };

    let direction = direction.map(str::trim).unwrap_or("");
    match field
        .map(str::trim)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        // Deliberately not `"score"`: the accepted spellings are the ones `SORTABLE` names, and
        // a set of synonyms wider than the error message advertises is a set nobody can discover.
        "" | "_score" | "relevance" => Ok(SortBy::Relevance),
        "timestamp" => match direction.to_ascii_lowercase().as_str() {
            // Newest-first is the default because a timestamp sort is what you reach for when
            // relevance is meaningless, and that is nearly always "what happened last".
            "" | "desc" | "descending" => Ok(SortBy::Newest),
            "asc" | "ascending" => Ok(SortBy::Oldest),
            other => Err(format!(
                "unknown sort direction {other:?}; use \"asc\" or \"desc\""
            )),
        },
        other => Err(format!(
            "cannot sort by {other:?}; this index sorts by {SORTABLE}"
        )),
    }
}

/// The facet field list, plus the largest `size` any one facet asked for.
fn decode_facets(
    value: Option<Value>,
    warnings: &mut Vec<String>,
) -> Result<(Vec<String>, Option<usize>), String> {
    let mut fields: Vec<String> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();

    match value {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => fields.extend(
            s.split(',')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_string),
        ),
        Some(Value::Array(list)) => {
            for item in list {
                match item {
                    Value::String(s) if !s.trim().is_empty() => fields.push(s.trim().to_string()),
                    other => {
                        return Err(format!("a facet field must be a name, got {other}"));
                    }
                }
            }
        }
        Some(Value::Object(map)) => {
            for (field, config) in map {
                let field = field.trim().to_string();
                if field.is_empty() {
                    return Err("a facet field name cannot be empty".into());
                }
                match &config {
                    Value::Null => {}
                    Value::Object(config) => {
                        match config.get("type").and_then(Value::as_str) {
                            None | Some("value") => {}
                            // A range facet answered with value buckets would look like an
                            // answer and be a different question.
                            Some(other) => {
                                return Err(format!(
                                    "facet {field:?} asks for type {other:?}; this index only \
                                     counts values"
                                ));
                            }
                        }
                        if let Some(size) = config.get("size") {
                            let size = size.as_u64().ok_or_else(|| {
                                format!("facet {field:?}: size {size} is not a number")
                            })?;
                            sizes.push(size as usize);
                        }
                        for key in config.keys() {
                            if !matches!(key.as_str(), "type" | "size") {
                                warnings.push(format!(
                                    "ignored unknown key {key:?} on facet {field:?}"
                                ));
                            }
                        }
                    }
                    other => {
                        return Err(format!(
                            "facet {field:?} needs a configuration object, got {other}"
                        ));
                    }
                }
                fields.push(field);
            }
        }
        Some(other) => {
            return Err(format!(
                "facets must be a field list or a {{field: {{type, size}}}} object, got {other}"
            ));
        }
    }

    // One aggregation pass serves every facet, so `SearchRequest` has a single `facet_top`.
    // Taking the largest keeps every facet at least as long as it asked for; saying so keeps
    // the caller from reading a 50-row facet as the answer to their `size: 15`.
    sizes.sort_unstable();
    sizes.dedup();
    if sizes.len() > 1 {
        let listed = sizes
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let top = sizes.last().copied().unwrap_or_default();
        warnings.push(format!(
            "facet sizes differ ({listed}); this index applies one facet_top of {top} to all"
        ));
    }
    Ok((fields, sizes.last().copied()))
}

// ---------------------------------------------------------------------------
// responses
// ---------------------------------------------------------------------------

/// The full `ResponseState` envelope, ready to serialize.
pub fn search_ui_response(prepared: &PreparedSearch, resp: &SearchResponse) -> Value {
    let req = &prepared.request;
    let results: Vec<Value> = resp.hits.iter().map(result_json).collect();

    // Ceiling division: 21 results at 20 per page is two pages, and a `totalPages` of 1 would
    // hide the last one behind a pager that thinks it is at the end.
    let total_pages = if prepared.size == 0 {
        0
    } else {
        resp.total.div_ceil(prepared.size)
    };
    // 1-based and inclusive, and 0/0 rather than 1/0 when there is nothing to number — a
    // "showing 1-0 of 0" is the shape of a bug, not of an empty result.
    let (paging_start, paging_end) = if results.is_empty() {
        (0, 0)
    } else {
        (req.offset + 1, req.offset + results.len())
    };

    let mut facets = Map::new();
    for (field, facet) in &resp.facets {
        facets.insert(field.clone(), json!([facet_envelope(facet)]));
    }

    json!({
        "results": results,
        "totalResults": resp.total,
        "totalPages": total_pages,
        "pagingStart": paging_start,
        "pagingEnd": paging_end,
        "current": prepared.page,
        "resultsPerPage": prepared.size,
        "requestId": "",
        "resultSearchTerm": req.query.clone().unwrap_or_default(),
        "wasSearched": true,
        "facets": Value::Object(facets),
        "info": {
            "elapsedMs": resp.elapsed_ms,
            "sort": match req.sort {
                SortBy::Relevance => "relevance",
                SortBy::Newest => "newest",
                SortBy::Oldest => "oldest",
            },
            "limit": req.limit,
            "offset": req.offset,
            "warnings": prepared.warnings,
        },
    })
}

/// One hit as Search UI reads it: every stored field flattened to `{ "raw": … }`, the snippet on
/// the body it was cut from, and everything this UI actually renders under `_meta`.
fn result_json(hit: &Hit) -> Value {
    let doc = api_doc(&hit.doc, false);
    let mut fields = Map::new();
    // Search UI addresses a result by an `id` field; `doc_id` is that identity, and it is
    // repeated rather than renamed so `_meta.doc` stays a plain `Doc`.
    fields.insert("id".into(), json!({ "raw": hit.doc.doc_id }));
    if let Some(doc) = doc.as_object() {
        for (key, value) in doc {
            // A field the document does not carry is not a stored field; `{"raw": null}` would
            // render as the string "null" in a stock Search UI result template.
            if value.is_null() {
                continue;
            }
            fields.insert(key.clone(), json!({ "raw": value }));
        }
    }

    let snippet_field = hit.snippet_field.as_str();
    let entry = fields
        .entry(snippet_field.to_string())
        .or_insert_with(|| json!({ "raw": "" }));
    if let Some(entry) = entry.as_object_mut() {
        entry.insert(
            "snippet".into(),
            json!(highlight_html(&hit.snippet, &hit.snippet_marks)),
        );
    }

    fields.insert(
        "_meta".into(),
        json!({
            "id": hit.doc.doc_id,
            // An f32 widened to f64 carries its binary error into JSON (12.4 becomes
            // 12.399999618530273), which reads as precision this score does not have.
            "score": (f64::from(hit.score) * 1e4).round() / 1e4,
            "snippetField": snippet_field,
            "doc": doc,
        }),
    );
    Value::Object(fields)
}

/// `parse::Doc` as the API returns it: the `raw` JSONL line dropped unless asked for, and an
/// RFC3339 `timestamp` beside the `timestamp_ms` the struct carries.
///
/// Derived from the serialized `Doc` rather than field by field on purpose: a field added to
/// `Doc` later has to appear here without anyone remembering to add it, because the alternative
/// failure — a new field that silently never reaches the UI — looks like missing data rather
/// than like a bug.
/// A [`Doc`] as the API reports it.
///
/// Serialized from the struct rather than field-by-field so that a field added to `Doc` cannot
/// silently go missing here. The cost of that is that the *spellings* are serde's, so any enum
/// in `Doc` has to carry the vocabulary the rest of the system uses — see `DocKind`, whose
/// derive default (`"ToolCall"`) once reached the wire and quietly disabled every consumer that
/// switched on `kind == "tool_call"`.
pub fn api_doc(doc: &Doc, include_raw: bool) -> Value {
    let mut value = serde_json::to_value(doc).expect("Doc holds only JSON-native values");
    if let Some(map) = value.as_object_mut() {
        if !include_raw {
            // The single biggest field, and a 40-document window does not want 40 copies of it.
            map.remove("raw");
        }
        let timestamp = doc
            .timestamp_ms
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|at| Value::String(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
            .unwrap_or(Value::Null);
        map.insert("timestamp".into(), timestamp);
    }
    value
}

/// One `FacetResult` with its honesty fields spelled out, for `GET /api/facets/{field}`.
pub fn facet_json(f: &FacetResult) -> Value {
    let mut value = Map::new();
    value.insert("field".into(), json!(f.field));
    value.insert("values".into(), facet_counts(f));
    value.append(&mut facet_meta(f));
    Value::Object(value)
}

/// The same facet inside a Search UI response, where the honesty fields live under `meta` —
/// which Search UI ignores and this UI shows.
fn facet_envelope(f: &FacetResult) -> Value {
    json!({
        "field": f.field,
        "type": "value",
        "data": facet_counts(f),
        "meta": Value::Object(facet_meta(f)),
    })
}

fn facet_counts(f: &FacetResult) -> Value {
    Value::Array(
        f.values
            .iter()
            .map(|c| json!({ "value": c.value, "count": c.count }))
            .collect(),
    )
}

/// The counts a caller needs to read the buckets without over-reading them. `searchShaped` and
/// `hiddenValues` are `FacetResult`'s own judgements, never recomputed here — two answers to
/// "is this field a long tail" that disagree would be worse than none.
fn facet_meta(f: &FacetResult) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("matchingDocs".into(), json!(f.matching_docs));
    meta.insert("docsWithValue".into(), json!(f.docs_with_value));
    meta.insert("otherDocs".into(), json!(f.other_docs));
    meta.insert("distinct".into(), json!(f.distinct));
    meta.insert("searchShaped".into(), json!(f.is_search_shaped()));
    meta.insert("hiddenValues".into(), json!(f.hidden_values()));
    meta
}

/// A session listing entry: `SessionInfo` verbatim, plus the display id the UI links by.
pub fn session_json(info: &SessionInfo) -> Value {
    let mut value = serde_json::to_value(info).expect("SessionInfo holds only JSON-native values");
    if let Some(map) = value.as_object_mut() {
        // `session_id[:agent_id]` — a subagent shares its parent's session id, so the id alone
        // is not a key and two sidechains would collapse onto one row.
        let key = match &info.agent_id {
            Some(agent) => format!("{}:{agent}", info.session_id),
            None => info.session_id.clone(),
        };
        map.insert("key".into(), Value::String(key));
    }
    value
}

/// `**marked**` plain text plus the mark ranges [`Hit::snippet_marks`] carries -> HTML-escaped
/// text with `<em>` around exactly the spans that matched.
///
/// The ranges are required rather than re-derived by splitting on `**`, because a transcript
/// body carries `**` of its own — a turn that read a markdown file, or a model writing bold
/// prose. Splitting cannot tell those from the highlighter's, so it pairs them up wrongly and
/// puts match emphasis on words nobody searched for, or gives up and leaks the raw `**` form
/// into a field this API promises is HTML. Neither failure announces itself.
///
/// The escaping happens *first*, and the tags are added to text that can no longer contain
/// any: a snippet is a verbatim slice of a transcript, so it is full of `<script>`.
pub fn highlight_html(snippet: &str, marks: &[Range<usize>]) -> String {
    let mut out = String::with_capacity(snippet.len() + marks.len() * 9);
    let mut cursor = 0;
    for mark in marks {
        // A mark that is not wrapped in a marker pair, or that runs backwards past the last
        // one, means the snippet and the ranges did not come from the same hit. Dropping it
        // emphasises nothing, which is the only harmless way to be wrong here — slicing on it
        // would panic mid-character or wrap text the query never touched.
        let Some(open) = mark.start.checked_sub(HIGHLIGHT.len()) else {
            continue;
        };
        let close = mark.end.saturating_add(HIGHLIGHT.len());
        if open < cursor
            || !snippet.is_char_boundary(open)
            || !snippet.is_char_boundary(mark.start)
            || !snippet.is_char_boundary(mark.end)
            || !snippet.is_char_boundary(close)
            || &snippet[open..mark.start] != HIGHLIGHT
            || &snippet[mark.end..close] != HIGHLIGHT
        {
            continue;
        }
        out.push_str(&escape_html(&snippet[cursor..open]));
        if mark.start != mark.end {
            out.push_str("<em>");
            out.push_str(&escape_html(&snippet[mark.start..mark.end]));
            out.push_str("</em>");
        }
        cursor = close;
    }
    out.push_str(&escape_html(&snippet[cursor..]));
    out
}

/// `"` and `'` are escaped along with the three that matter for element content, because this
/// string also ends up inside `title="…"` attributes in the templates that consume it.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The `Filters` a session listing honours, from a query string, with the row limit.
///
/// Unknown keys are refused here rather than in the handler: `sessions.json` answers only some
/// of the filters `search` does, and `?tool=Bash` quietly listing every session would be the
/// exact lie this API is written to avoid.
pub fn session_filters(p: &Params) -> Result<(Filters, usize), String> {
    p.reject_unknown(SESSION_LIST_PARAMS)?;
    let filters = Filters {
        project: text(p, "project"),
        branch: text(p, "branch"),
        model: text(p, "model"),
        session: text(p, "session"),
        agent_type: text(p, "agent_type"),
        since: text(p, "since"),
        until: text(p, "until"),
        no_sidechains: p.flag("no_sidechains")?,
        sidechains_only: p.flag("sidechains_only")?,
        ..Filters::default()
    };
    if filters.no_sidechains && filters.sidechains_only {
        return Err(
            "no_sidechains and sidechains_only contradict each other; set at most one".into(),
        );
    }
    let limit = p.number::<usize>("limit")?.unwrap_or(SESSION_LIST_LIMIT);
    Ok((filters, limit))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{Doc, DocKind};
    use crate::search::{FacetCount, SnippetSource};

    fn params(raw: &str) -> Params {
        Params::parse(Some(raw)).expect("query string parses")
    }

    fn prepared(body: Value) -> Result<PreparedSearch, String> {
        serde_json::from_value::<SearchBody>(body)
            .map_err(|err| err.to_string())?
            .prepare()
    }

    fn filters_of(body: Value) -> Result<Filters, String> {
        prepared(body).map(|p| p.request.filters)
    }

    fn doc() -> Doc {
        Doc {
            doc_id: "9f2c:-:a41b:118".into(),
            kind: DocKind::ToolCall,
            source_path: "/home/u/.claude/projects/p/9f2c.jsonl".into(),
            seq: 118,
            turn_seq: 110,
            turn_prompt: Some("make the build faster".into()),
            session_id: "9f2c".into(),
            agent_id: None,
            agent_type: None,
            uuid: Some("u-118".into()),
            parent_uuid: None,
            timestamp_ms: Some(1_757_444_839_248),
            project: Some("/home/u/p".into()),
            git_branch: Some("main".into()),
            role: "assistant".into(),
            model: Some("claude-opus-5".into()),
            tool_name: Some("Bash".into()),
            tool_use_id: Some("toolu_1".into()),
            tool_input: Some(json!({ "command": "cargo test" })),
            bash_cmd: Some(json!({ "program": ["cargo"], "args": ["test"] })),
            is_error: false,
            is_sidechain: false,
            is_meta: false,
            entrypoint: Some("cli".into()),
            permission_mode: None,
            version: Some("2.1.266".into()),
            slug: None,
            body: "Bash\ncargo test".into(),
            text: vec!["Bash".into(), "cargo test".into()],
            code: Vec::new(),
            headings: Vec::new(),
            code_langs: Vec::new(),
            tool_output: Some("test result: ok".into()),
            thinking: None,
            thinking_tokens: None,
            raw: "{\"the\":\"whole line\"}".into(),
        }
    }

    // --- params ------------------------------------------------------------

    #[test]
    fn repeated_keys_are_kept_and_ordered() {
        let p = params("tool=Bash&tool=Read&q=memmap");
        assert_eq!(p.all("tool"), vec!["Bash", "Read"]);
        assert_eq!(p.first("tool"), Some("Bash"));
        assert_eq!(p.first("q"), Some("memmap"));
        assert!(p.all("nope").is_empty());
    }

    #[test]
    fn a_flag_is_false_when_absent_and_true_when_bare() {
        let p = params("errors_only&no_sidechains=0&include_thinking=yes");
        assert!(p.flag("errors_only").unwrap(), "present-and-empty is true");
        assert!(!p.flag("no_sidechains").unwrap());
        assert!(p.flag("include_thinking").unwrap());
        assert!(!p.flag("sidechains_only").unwrap(), "absent is false");

        let err = params("errors_only=maybe").flag("errors_only").unwrap_err();
        assert!(err.contains("errors_only"), "{err}");
        assert!(err.contains("1/true/yes/on"), "{err}");
    }

    #[test]
    fn a_number_that_is_not_one_is_an_error_naming_the_key() {
        let p = params("size=20&page=&facet_top=lots");
        assert_eq!(p.number::<usize>("size").unwrap(), Some(20));
        assert_eq!(
            p.number::<usize>("page").unwrap(),
            None,
            "an empty value reads as absent"
        );
        assert_eq!(p.number::<usize>("offset").unwrap(), None);
        let err = p.number::<usize>("facet_top").unwrap_err();
        assert!(err.contains("facet_top"), "{err}");
        assert!(err.contains("lots"), "{err}");
    }

    /// A typo'd parameter that silently widened the search would look exactly like an honest
    /// answer, so the error has to name both the typo and what this endpoint takes.
    #[test]
    fn an_unknown_query_parameter_is_refused_and_lists_the_accepted_ones() {
        assert!(
            params("q=x&tool=Bash")
                .reject_unknown(SEARCH_PARAMS)
                .is_ok()
        );
        let err = params("q=x&toolname=Bash")
            .reject_unknown(SEARCH_PARAMS)
            .unwrap_err();
        assert!(err.contains("\"toolname\""), "{err}");
        assert!(
            err.contains("tool_input"),
            "must list what is accepted: {err}"
        );
    }

    /// The HTTP surface deliberately does **not** carry `--similar-to`, and this pins the
    /// decision rather than leaving it to look like an oversight.
    ///
    /// Three reasons, spelled out in `docs/DESIGN.md`: `SearchBody` exists to be Elastic Search
    /// UI's `RequestState`, which has no notion of "documents like this one"; resolving a
    /// document reference has its own ambiguity error that wants its own status shape and,
    /// honestly, its own route (`GET /api/similar/{ref}`); and the bundled UI has no affordance
    /// to trigger it, so it would be dead, unauthenticated surface on a port that already
    /// serves every secret in every transcript. `reject_unknown` gives the right answer for
    /// free — a 400 that lists what this endpoint does take.
    #[test]
    fn similar_to_is_not_a_search_parameter() {
        let err = params("q=x&similar_to=abc123")
            .reject_unknown(SEARCH_PARAMS)
            .unwrap_err();
        assert!(err.contains("\"similar_to\""), "{err}");
    }

    /// Nor does it carry `--group-by-turn`, for a reason that is specific to this envelope:
    /// grouping makes `limit`/`offset` count turns while `total` keeps counting documents, and
    /// every page control in a Search UI response — `current`, `resultsPerPage`, the pager the
    /// bundled UI draws from them — divides one by the other. A facade that paged in turns and
    /// reported a document total would be wrong in the one place a user can see it. The CLI is
    /// free of that: it prints a page and says what the page is.
    #[test]
    fn group_by_turn_is_not_a_search_parameter() {
        let err = params("q=x&group_by_turn=1")
            .reject_unknown(SEARCH_PARAMS)
            .unwrap_err();
        assert!(err.contains("\"group_by_turn\""), "{err}");
        assert!(
            !SearchBody::default()
                .prepare()
                .unwrap()
                .request
                .group_by_turn
        );
    }

    // --- GET -> body -------------------------------------------------------

    #[test]
    fn a_get_query_string_becomes_the_same_request_as_a_post_body() {
        let p = params(
            "q=memmap&tool=Bash&tool=Read&tool_input=command%3Dcargo&project=~%2Fcode\
             &errors_only&since=7d&facets=tool_name%2Cproject&facets=model&size=5&page=3\
             &sort=oldest&include_thinking=1&snippet_chars=80",
        );
        let prep = SearchBody::from_params(&p).unwrap().prepare().unwrap();

        assert_eq!(prep.request.query.as_deref(), Some("memmap"));
        assert_eq!(prep.request.filters.tool, vec!["Bash", "Read"]);
        assert_eq!(prep.request.filters.tool_input, vec!["command=cargo"]);
        assert_eq!(prep.request.filters.project.as_deref(), Some("~/code"));
        assert!(prep.request.filters.errors_only);
        assert_eq!(prep.request.filters.since.as_deref(), Some("7d"));
        assert_eq!(
            prep.request.facets,
            vec!["tool_name", "project", "model"],
            "comma-separated and repeated both accumulate"
        );
        assert_eq!((prep.size, prep.page), (5, 3));
        assert_eq!(prep.request.offset, 10);
        assert_eq!(prep.request.sort, SortBy::Oldest);
        assert!(prep.request.include_thinking);
        assert_eq!(prep.request.snippet_chars, 80);
    }

    /// A malformed date used to reach `search::search`, whose `anyhow` error the handler
    /// renders as a `500` — the status that means "the index or the disk was wrong".
    #[test]
    fn a_malformed_date_is_the_callers_mistake_and_names_the_wire_parameter() {
        let err = SearchBody::from_params(&params("since=notadate"))
            .unwrap()
            .prepare()
            .unwrap_err();
        assert!(err.starts_with("since:"), "names the wire parameter: {err}");
        assert!(
            !err.contains("--since"),
            "there is no such flag over HTTP: {err}"
        );
        assert!(err.contains("7d"), "says what is accepted: {err}");

        let err = filters_of(json!({
            "filters": [{ "field": "timestamp", "values": [{ "from": "garbage" }] }]
        }))
        .unwrap_err();
        assert!(err.starts_with("since:"), "{err}");

        let err = SearchBody::from_params(&params("until=2026-0"))
            .unwrap()
            .prepare()
            .unwrap_err();
        assert!(err.starts_with("until:"), "{err}");

        // Every spelling `--since` takes still goes through, and an empty value is "unset".
        for raw in [
            "since=7d",
            "since=now",
            "since=2026-09-10",
            "until=",
            "since=2026-09-10T13:00:00Z",
        ] {
            SearchBody::from_params(&params(raw))
                .unwrap()
                .prepare()
                .unwrap_or_else(|err| panic!("{raw:?} is a date this index accepts: {err}"));
        }
    }

    #[test]
    fn get_sort_accepts_the_cli_spelling_and_refuses_anything_else() {
        let sort = |raw: &str| {
            SearchBody::from_params(&params(raw))
                .unwrap()
                .prepare()
                .map(|p| p.request.sort)
        };
        assert_eq!(sort("sort=newest").unwrap(), SortBy::Newest);
        assert_eq!(sort("sort=oldest").unwrap(), SortBy::Oldest);
        assert_eq!(sort("sort=relevance").unwrap(), SortBy::Relevance);
        assert_eq!(sort("").unwrap(), SortBy::Relevance);
        assert_eq!(sort("sort=timestamp").unwrap(), SortBy::Newest);
        let err = sort("sort=score_desc").unwrap_err();
        assert!(err.contains("score_desc"), "{err}");
        assert!(err.contains("timestamp"), "{err}");
    }

    // --- filters -----------------------------------------------------------

    #[test]
    fn the_native_filters_object_is_search_filters_verbatim() {
        let f = filters_of(json!({
            "filters": { "tool": ["Bash"], "project": "~/code", "errors_only": true,
                         "min_thinking": 500, "tool_output": "No such file" }
        }))
        .unwrap();
        assert_eq!(f.tool, vec!["Bash"]);
        assert_eq!(f.project.as_deref(), Some("~/code"));
        assert!(f.errors_only);
        assert_eq!(f.min_thinking, Some(500));
        assert_eq!(
            f.tool_output,
            vec!["No such file"],
            "a lone value for a repeatable filter is not a type error"
        );
    }

    #[test]
    fn an_unknown_native_filter_key_is_refused_rather_than_dropped() {
        let err = filters_of(json!({ "filters": { "toolname": ["Bash"] } })).unwrap_err();
        assert!(err.contains("\"toolname\""), "{err}");
        assert!(
            err.contains("tool_output"),
            "must list the real keys: {err}"
        );
    }

    #[test]
    fn the_search_ui_array_form_maps_onto_the_same_filters() {
        let f = filters_of(json!({
            "filters": [
                { "field": "tool_name", "values": ["Bash", "Read"], "type": "any" },
                { "field": "git_branch", "values": ["main"] },
                { "field": "session_id", "values": ["9f2c"] },
                { "field": "is_error", "values": [true] },
                { "field": "is_sidechain", "values": [false] },
                { "field": "thinking_tokens", "values": [{ "from": 5000 }] }
            ]
        }))
        .unwrap();
        assert_eq!(f.tool, vec!["Bash", "Read"]);
        assert_eq!(f.branch.as_deref(), Some("main"));
        assert_eq!(f.session.as_deref(), Some("9f2c"));
        assert!(f.errors_only);
        assert!(f.no_sidechains && !f.sidechains_only);
        assert_eq!(f.min_thinking, Some(5000));
    }

    #[test]
    fn tool_input_path_filters_become_key_equals_value() {
        let f = filters_of(json!({
            "filters": [
                { "field": "tool_input.file_path", "values": ["/tmp/a.rs", "/tmp/b.rs"] },
                { "field": "tool_input.command", "values": ["cargo test"] }
            ]
        }))
        .unwrap();
        assert_eq!(
            f.tool_input,
            vec![
                "file_path=/tmp/a.rs",
                "file_path=/tmp/b.rs",
                "command=cargo test"
            ]
        );
        let err = filters_of(json!({ "filters": [{ "field": "tool_input.", "values": ["x"] }] }))
            .unwrap_err();
        assert!(err.contains("tool_input.file_path"), "{err}");
    }

    #[test]
    fn a_timestamp_range_becomes_since_and_until() {
        let f = filters_of(json!({
            "filters": [{ "field": "timestamp",
                          "values": [{ "from": "2026-01-01", "to": "now", "name": "This year" }] }]
        }))
        .unwrap();
        assert_eq!(f.since.as_deref(), Some("2026-01-01"));
        assert_eq!(f.until.as_deref(), Some("now"));

        // Search UI's own date facets emit epoch milliseconds.
        let f = filters_of(json!({
            "filters": [{ "field": "timestamp", "values": [{ "from": 1_757_444_839_248i64 }] }]
        }))
        .unwrap();
        assert_eq!(f.since.as_deref(), Some("2025-09-09T19:07:19.248Z"));
        assert_eq!(f.until, None);
    }

    #[test]
    fn a_range_this_index_cannot_express_is_an_error_not_a_wider_search() {
        let err = filters_of(json!({
            "filters": [{ "field": "thinking_tokens", "values": [{ "from": 1, "to": 99 }] }]
        }))
        .unwrap_err();
        assert!(err.contains("thinking_tokens"), "{err}");
        assert!(err.contains("upper bound"), "{err}");

        let err = filters_of(json!({
            "filters": [{ "field": "is_error", "values": [false] }]
        }))
        .unwrap_err();
        assert!(err.contains("is_error"), "{err}");

        let err = filters_of(json!({
            "filters": [{ "field": "timestamp", "values": [{ "gte": "2026-01-01" }] }]
        }))
        .unwrap_err();
        assert!(err.contains("\"gte\""), "{err}");
    }

    /// The failure this prevents: two projects would silently become one, and the answer would
    /// look like a complete listing of both.
    #[test]
    fn a_single_valued_field_given_two_values_is_an_error_naming_the_field() {
        let err = filters_of(json!({
            "filters": [{ "field": "project", "values": ["/a", "/b"] }]
        }))
        .unwrap_err();
        assert!(err.contains("\"project\""), "{err}");
        assert!(err.contains("single value"), "{err}");

        let err = filters_of(json!({
            "filters": [{ "field": "model", "values": ["a"] }, { "field": "model", "values": ["b"] }]
        }))
        .unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn an_unknown_filter_field_or_combining_type_is_an_error() {
        let err = filters_of(json!({
            "filters": [{ "field": "toolname", "values": ["Bash"] }]
        }))
        .unwrap_err();
        assert!(err.contains("\"toolname\""), "{err}");
        assert!(err.contains("tool_input.<path>"), "{err}");

        let err = filters_of(json!({
            "filters": [{ "field": "tool_name", "values": ["Bash"], "type": "all" }]
        }))
        .unwrap_err();
        assert!(err.contains("\"all\""), "{err}");

        let err = filters_of(json!({
            "filters": [{ "field": "tool_name", "values": [] }]
        }))
        .unwrap_err();
        assert!(err.contains("no values"), "{err}");
    }

    #[test]
    fn contradictory_sidechain_filters_are_refused() {
        let err = filters_of(json!({
            "filters": { "no_sidechains": true, "sidechains_only": true }
        }))
        .unwrap_err();
        assert!(err.contains("sidechains_only"), "{err}");
        let err = session_filters(&params("no_sidechains&sidechains_only")).unwrap_err();
        assert!(err.contains("contradict"), "{err}");
    }

    // --- sort and facets ---------------------------------------------------

    #[test]
    fn sort_list_accepts_timestamp_only() {
        let sort = |body: Value| prepared(body).map(|p| p.request.sort);
        assert_eq!(
            sort(json!({ "sortList": [{ "field": "timestamp", "direction": "desc" }] })).unwrap(),
            SortBy::Newest
        );
        assert_eq!(
            sort(json!({ "sortList": [{ "field": "timestamp", "direction": "asc" }] })).unwrap(),
            SortBy::Oldest
        );
        assert_eq!(
            sort(json!({ "sortList": [{ "field": "", "direction": "" }] })).unwrap(),
            SortBy::Relevance
        );
        assert_eq!(
            sort(json!({ "sortField": "_score" })).unwrap(),
            SortBy::Relevance
        );
        assert_eq!(
            sort(json!({ "sortField": "timestamp", "sortDirection": "asc" })).unwrap(),
            SortBy::Oldest
        );
        assert_eq!(sort(json!({})).unwrap(), SortBy::Relevance);

        assert_eq!(
            sort(json!({ "sortList": [{ "field": "_score", "direction": "sideways" }] })).unwrap(),
            SortBy::Relevance,
            "relevance has one order, so a direction beside it is inert rather than an error"
        );
        let err = sort(json!({ "sortList": [{ "field": "score" }] })).unwrap_err();
        assert!(
            err.contains("_score"),
            "the near-miss names the real spelling: {err}"
        );
        let err = sort(json!({ "sortField": "tool_name" })).unwrap_err();
        assert!(err.contains("tool_name"), "{err}");
        assert!(err.contains("timestamp"), "{err}");
        let err =
            sort(json!({ "sortField": "timestamp", "sortDirection": "sideways" })).unwrap_err();
        assert!(err.contains("sideways"), "{err}");
        let err =
            sort(json!({ "sortList": [{ "field": "timestamp", "order": "asc" }] })).unwrap_err();
        assert!(err.contains("\"order\""), "{err}");
    }

    #[test]
    fn the_facets_object_names_fields_and_the_largest_size_wins() {
        let prep = prepared(json!({
            "facets": { "tool_name": { "type": "value", "size": 15 },
                        "project": { "type": "value", "size": 50, "sort": "count" } }
        }))
        .unwrap();
        assert_eq!(
            prep.request.facets,
            vec!["project", "tool_name"],
            "sorted by key"
        );
        assert_eq!(prep.request.facet_top, 50);
        assert!(
            prep.warnings
                .iter()
                .any(|w| w.contains("facet sizes differ")),
            "{:?}",
            prep.warnings
        );
        assert!(
            prep.warnings.iter().any(|w| w.contains("\"sort\"")),
            "an unknown facet key is a warning, not a silent drop: {:?}",
            prep.warnings
        );

        let err = prepared(json!({ "facets": { "timestamp": { "type": "range" } } })).unwrap_err();
        assert!(err.contains("range"), "{err}");
    }

    /// An unknown top-level key costs the caller nothing, so it is the one thing that degrades
    /// to a warning rather than a 400 — Search UI adds keys to `RequestState` over time.
    #[test]
    fn an_unknown_body_key_is_a_warning_not_an_error() {
        let prep = prepared(json!({ "searchTerm": "x", "trackTotalHits": true })).unwrap();
        assert!(
            prep.warnings.iter().any(|w| w.contains("trackTotalHits")),
            "{:?}",
            prep.warnings
        );
    }

    // --- paging ------------------------------------------------------------

    #[test]
    fn current_is_one_based_and_offset_is_zero_based() {
        let prep = prepared(json!({ "current": 3, "resultsPerPage": 20 })).unwrap();
        assert_eq!(prep.request.offset, 40);
        assert_eq!((prep.page, prep.size), (3, 20));

        // An explicit offset wins, and `current` is recomputed so the two never disagree.
        let prep = prepared(json!({ "current": 3, "resultsPerPage": 20, "offset": 137 })).unwrap();
        assert_eq!(prep.request.offset, 137);
        assert_eq!(prep.page, 7);
        assert!(prep.warnings.iter().any(|w| w.contains("offset wins")));

        let err = prepared(json!({ "current": 0 })).unwrap_err();
        assert!(err.contains("1-based"), "{err}");
    }

    #[test]
    fn paging_is_right_on_the_last_short_page_and_on_an_empty_result() {
        let hits = |n: usize| SearchResponse {
            hits: (0..n)
                .map(|i| Hit {
                    doc: Doc {
                        seq: i as u64,
                        ..doc()
                    },
                    score: 1.0,
                    snippet: String::new(),
                    snippet_field: SnippetSource::Text,
                    snippet_marks: Vec::new(),
                    collapsed: 0,
                })
                .collect(),
            total: 431,
            facets: Default::default(),
            elapsed_ms: 7,
            grouped: false,
        };

        let prep = prepared(json!({ "current": 22, "resultsPerPage": 20 })).unwrap();
        let out = search_ui_response(&prep, &hits(11));
        assert_eq!(out["totalPages"], json!(22), "ceiling division, not 21");
        assert_eq!(out["pagingStart"], json!(421));
        assert_eq!(out["pagingEnd"], json!(431), "the short page ends early");
        assert_eq!(out["current"], json!(22));
        assert_eq!(out["resultsPerPage"], json!(20));

        let mut empty = hits(0);
        empty.total = 0;
        let prep = prepared(json!({ "current": 1, "resultsPerPage": 20 })).unwrap();
        let out = search_ui_response(&prep, &empty);
        assert_eq!(out["totalPages"], json!(0));
        assert_eq!(
            (&out["pagingStart"], &out["pagingEnd"]),
            (&json!(0), &json!(0)),
            "0/0, never 1/0"
        );
        assert_eq!(out["wasSearched"], json!(true));
    }

    // --- documents and the envelope ----------------------------------------

    /// The frontend switches on `kind` to decide whether a turn gets a tool rendering, and the
    /// documented values are `message` and `tool_call`. Serde's derive default spells the
    /// variant name instead, which matches nothing — and the failure is silent, because "not a
    /// tool call" is an ordinary thing for a document to be.
    #[test]
    fn api_doc_reports_the_kind_values_the_contract_documents() {
        let mut doc = doc();
        doc.kind = DocKind::ToolCall;
        assert_eq!(api_doc(&doc, false)["kind"], json!("tool_call"));
        doc.kind = DocKind::Message;
        assert_eq!(api_doc(&doc, false)["kind"], json!("message"));
    }

    #[test]
    fn api_doc_drops_raw_unless_asked_and_adds_an_rfc3339_timestamp() {
        let doc = doc();
        let slim = api_doc(&doc, false);
        assert!(slim.get("raw").is_none(), "the JSONL line is the big field");
        assert_eq!(slim["timestamp"], json!("2025-09-09T19:07:19.248Z"));
        assert_eq!(slim["timestamp_ms"], json!(1_757_444_839_248i64));
        assert_eq!(slim["tool_input"], json!({ "command": "cargo test" }));
        // Serialized from `Doc` rather than hand-listed, so even the fields nothing reads yet
        // are here — a field added to `Doc` later cannot silently vanish from the API.
        assert!(slim.get("permission_mode").is_some());
        assert!(slim.get("is_meta").is_some());

        let full = api_doc(&doc, true);
        assert_eq!(full["raw"], json!("{\"the\":\"whole line\"}"));

        let undated = api_doc(
            &Doc {
                timestamp_ms: None,
                ..doc
            },
            false,
        );
        assert_eq!(undated["timestamp"], Value::Null);
    }

    #[test]
    fn a_result_carries_flat_raw_fields_the_snippet_and_meta() {
        let hit = Hit {
            doc: doc(),
            score: 12.4,
            snippet: "the **memmap** panic".into(),
            snippet_field: SnippetSource::ToolOutput,
            snippet_marks: marks(&[(6, 12)]),
            collapsed: 0,
        };
        let resp = SearchResponse {
            hits: vec![hit],
            total: 1,
            facets: Default::default(),
            elapsed_ms: 7,
            grouped: false,
        };
        let prep = prepared(json!({ "searchTerm": "memmap" })).unwrap();
        let out = search_ui_response(&prep, &resp);
        let result = &out["results"][0];

        assert_eq!(result["id"]["raw"], json!("9f2c:-:a41b:118"));
        assert_eq!(result["tool_name"]["raw"], json!("Bash"));
        assert_eq!(
            result["tool_input"]["raw"],
            json!({ "command": "cargo test" })
        );
        assert_eq!(
            result["tool_output"]["snippet"],
            json!("the <em>memmap</em> panic"),
            "the snippet lands on the body it was cut from"
        );
        assert!(
            result["text"].get("snippet").is_none(),
            "and on no other body"
        );
        assert!(
            result.get("thinking").is_none(),
            "a field the document does not carry is not a stored field"
        );
        assert_eq!(result["_meta"]["snippetField"], json!("tool_output"));
        assert_eq!(result["_meta"]["score"], json!(12.4), "no f32 noise");
        assert_eq!(result["_meta"]["doc"]["session_id"], json!("9f2c"));
        assert!(result["_meta"]["doc"].get("raw").is_none());
        assert_eq!(out["resultSearchTerm"], json!("memmap"));
        assert_eq!(out["info"]["sort"], json!("relevance"));
        assert_eq!(out["info"]["elapsedMs"], json!(7));
        assert_eq!(out["requestId"], json!(""));
    }

    // --- facets and sessions ------------------------------------------------

    fn long_tail() -> FacetResult {
        FacetResult {
            field: "tool_input.command".into(),
            values: vec![FacetCount {
                value: "cargo test".into(),
                count: 4,
            }],
            matching_docs: 8123,
            docs_with_value: 2210,
            other_docs: 2106,
            distinct: Some(1980),
        }
    }

    #[test]
    fn a_facet_reports_the_counts_needed_to_read_it() {
        let f = long_tail();
        let flat = facet_json(&f);
        assert_eq!(flat["field"], json!("tool_input.command"));
        assert_eq!(
            flat["values"][0],
            json!({ "value": "cargo test", "count": 4 })
        );
        assert_eq!(flat["matchingDocs"], json!(8123));
        assert_eq!(flat["docsWithValue"], json!(2210));
        assert_eq!(flat["otherDocs"], json!(2106));
        assert_eq!(flat["distinct"], json!(1980));
        // Both come from `FacetResult`'s own judgement, never recomputed here.
        assert_eq!(flat["searchShaped"], json!(f.is_search_shaped()));
        assert_eq!(flat["hiddenValues"], json!(1979));
    }

    #[test]
    fn the_search_envelope_keeps_the_same_facet_honesty_under_meta() {
        let mut facets = std::collections::BTreeMap::new();
        facets.insert("tool_name".to_string(), {
            let mut f = long_tail();
            f.field = "tool_name".into();
            f.distinct = Some(1);
            f.values = vec![FacetCount {
                value: "Bash".into(),
                count: 212,
            }];
            f
        });
        let resp = SearchResponse {
            hits: Vec::new(),
            total: 431,
            facets,
            elapsed_ms: 1,
            grouped: false,
        };
        let out = search_ui_response(&prepared(json!({})).unwrap(), &resp);
        let facet = &out["facets"]["tool_name"][0];
        assert_eq!(facet["type"], json!("value"));
        assert_eq!(facet["data"][0], json!({ "value": "Bash", "count": 212 }));
        assert_eq!(facet["meta"]["matchingDocs"], json!(8123));
        assert_eq!(facet["meta"]["searchShaped"], json!(false));
        assert_eq!(
            facet["meta"]["hiddenValues"],
            Value::Null,
            "everything fit, and null is not the same claim as 0"
        );
    }

    #[test]
    fn a_session_carries_its_display_key() {
        let info = SessionInfo {
            session_id: "9f2c".into(),
            source_path: "/t/9f2c.jsonl".into(),
            messages: 214,
            tool_calls: 96,
            ..SessionInfo::default()
        };
        let out = session_json(&info);
        assert_eq!(out["key"], json!("9f2c"));
        assert_eq!(out["messages"], json!(214));
        assert_eq!(out["agent_id"], Value::Null);
        assert_eq!(out["description"], Value::Null);

        let sidechain = SessionInfo {
            agent_id: Some("a108".into()),
            ..info
        };
        assert_eq!(
            session_json(&sidechain)["key"],
            json!("9f2c:a108"),
            "a subagent shares its parent's session id"
        );
    }

    #[test]
    fn session_filters_take_the_subset_a_listing_can_answer() {
        let (f, limit) =
            session_filters(&params("project=~%2Fcode&no_sidechains&limit=3")).unwrap();
        assert_eq!(f.project.as_deref(), Some("~/code"));
        assert!(f.no_sidechains);
        assert_eq!(limit, 3);

        let (_, limit) = session_filters(&params("")).unwrap();
        assert_eq!(limit, SESSION_LIST_LIMIT);

        let err = session_filters(&params("tool=Bash")).unwrap_err();
        assert!(err.contains("\"tool\""), "{err}");
        assert!(
            err.contains("agent_type"),
            "must list what it does take: {err}"
        );
    }

    // --- highlighting -------------------------------------------------------

    /// Mark ranges from `(start, end)` pairs. Written out rather than as `&[a..b]` literals,
    /// which clippy reads as a mistyped `vec![x; n]`.
    fn marks(spans: &[(usize, usize)]) -> Vec<Range<usize>> {
        spans.iter().map(|&(start, end)| start..end).collect()
    }

    /// `"the **memmap** panic"` with the mark `search::render_snippet` would have recorded.
    fn marked(before: &str, term: &str, after: &str) -> (String, Vec<Range<usize>>) {
        let snippet = format!("{before}{HIGHLIGHT}{term}{HIGHLIGHT}{after}");
        let start = before.len() + HIGHLIGHT.len();
        (snippet, marks(&[(start, start + term.len())]))
    }

    #[test]
    fn a_snippet_is_escaped_before_it_is_marked() {
        let (snippet, marks) = marked("the ", "memmap", " panic");
        assert_eq!(
            highlight_html(&snippet, &marks),
            "the <em>memmap</em> panic"
        );
        assert_eq!(
            highlight_html("<script>alert(1)</script>", &[]),
            "&lt;script&gt;alert(1)&lt;/script&gt;",
            "a transcript body must come out inert"
        );
        let (snippet, marks) = marked("", "<img src=x onerror=\"go()\">", "");
        assert_eq!(
            highlight_html(&snippet, &marks),
            "<em>&lt;img src=x onerror=&quot;go()&quot;&gt;</em>",
            "the marked span is escaped too"
        );
        assert_eq!(highlight_html("a & b", &[]), "a &amp; b");
        assert_eq!(highlight_html("", &[]), "");
    }

    /// The bug the ranges exist to close: a body carrying its own `**` used to have those
    /// markers paired with the highlighter's, so `<em>` landed on text that never matched — or
    /// the whole snippet degraded and shipped the raw `**` form to a client expecting HTML.
    #[test]
    fn markers_in_the_body_are_literal_text_and_only_the_marked_span_is_emphasised() {
        let (snippet, marks) = marked("AAA ** BBBBBB ** CCC ", "markerprobe", " DDD");
        assert_eq!(
            highlight_html(&snippet, &marks),
            "AAA ** BBBBBB ** CCC <em>markerprobe</em> DDD",
            "the body's own markers stay literal and emphasise nothing"
        );

        // One stray marker used to make the count odd and silently drop every `<em>`.
        let (snippet, marks) = marked("EEE ** FFF ", "markerprobe", " GGG");
        assert_eq!(
            highlight_html(&snippet, &marks),
            "EEE ** FFF <em>markerprobe</em> GGG"
        );

        assert_eq!(
            highlight_html("use **bold** in <b>markdown</b>", &[]),
            "use **bold** in &lt;b&gt;markdown&lt;/b&gt;",
            "no marks means nothing matched, not that the marks were lost"
        );
    }

    /// Ranges that do not describe this snippet are a bug on our side, and the only harmless
    /// way to be wrong about a match is to claim none.
    #[test]
    fn ranges_that_do_not_fit_the_snippet_are_dropped_rather_than_sliced() {
        let text = "the **memmap** panic";
        assert!(!highlight_html(text, &marks(&[(6, 9999)])).contains("<em>"));
        assert!(
            !highlight_html(text, &marks(&[(0, 3)])).contains("<em>"),
            "no marker pair around it"
        );
        // A range landing inside a multi-byte character must not panic.
        let wide = "**héllo**";
        assert!(!highlight_html(wide, &marks(&[(3, 6)])).contains("<em>"));
        // A marked span that is itself a closing tag: escaped inside its own `<em>`, never a tag.
        let (snippet, marks) = marked("", "</em><script>x</script>", "");
        let injected = highlight_html(&snippet, &marks);
        assert!(!injected.contains("<script>"), "{injected}");
        assert!(injected.starts_with("<em>&lt;/em&gt;"), "{injected}");
    }
}
