use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::config::QueryLimits;
use crate::engine::index::CatalogSnapshot;
use crate::engine::{CatalogItem, QueryError};
use crate::http::packed::{
    PACKED_CONTENT_TYPE, encode_eval, encode_find, encode_item, encode_search, encode_values,
};

pub struct AppState {
    pub snapshot: Arc<CatalogSnapshot>,
    pub limits: QueryLimits,
    pub started_at: Instant,
    pub requests_served: AtomicU64,
}

#[derive(Debug, Deserialize)]
pub struct EvalQueryParams {
    pub q: String,
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(default = "default_true")]
    pub eligible: bool,
    #[serde(default = "default_true")]
    pub scores: bool,
    #[serde(default = "default_true")]
    pub complement: bool,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EvalPostBody {
    pub q: String,
    #[serde(default = "default_true")]
    pub eligible: bool,
    #[serde(default = "default_true")]
    pub scores: bool,
    #[serde(default = "default_true")]
    pub complement: bool,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQueryParams {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_true")]
    pub eligible: bool,
    #[serde(default = "default_true")]
    pub fields: bool,
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindQueryParams {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_true")]
    pub eligible: bool,
    #[serde(default = "default_true")]
    pub fields: bool,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ValuesQueryParams {
    pub field: String,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ItemQueryParams {
    #[serde(default)]
    pub format: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_limit() -> usize {
    40
}

fn wants_packed(headers: &HeaderMap, format_param: Option<&str>) -> bool {
    if let Some(fmt) = format_param {
        return fmt.eq_ignore_ascii_case("packed") || fmt.eq_ignore_ascii_case("tsv");
    }
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/x-runeql-packed"))
}

fn with_meta_headers(
    mut resp: Response,
    revision: &str,
    eval_us: u128,
    immutable_cache: bool,
) -> Response {
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(revision) {
        h.insert("x-runeql-rev", v);
    }
    if let Ok(v) = HeaderValue::from_str(&eval_us.to_string()) {
        h.insert("x-eval-us", v);
    }
    if immutable_cache {
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    } else {
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=60"),
        );
    }
    resp
}

fn error_response(err: QueryError) -> Response {
    let (status, code) = match &err {
        QueryError::InvalidSyntax(_) => (StatusCode::BAD_REQUEST, "invalid_syntax"),
        QueryError::LimitExceeded(_) => (StatusCode::BAD_REQUEST, "limit_exceeded"),
        QueryError::BudgetExceeded(_) => (StatusCode::UNPROCESSABLE_ENTITY, "budget_exceeded"),
        QueryError::ResultTooBroad { .. } => (StatusCode::PAYLOAD_TOO_LARGE, "result_too_broad"),
    };
    #[derive(Serialize)]
    struct ErrBody<'a> {
        error: &'a str,
        message: String,
    }
    (
        status,
        Json(ErrBody {
            error: code,
            message: err.to_string(),
        }),
    )
        .into_response()
}

pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Health<'a> {
        status: &'static str,
        revision: &'a str,
        openrs2_cache_id: u32,
        build: &'a str,
        total_items: usize,
        eligible_items: usize,
        uptime_secs: u64,
        requests_served: u64,
    }
    Json(Health {
        status: "ok",
        revision: &state.snapshot.revision,
        openrs2_cache_id: state.snapshot.source.openrs2_cache_id,
        build: &state.snapshot.source.build,
        total_items: state.snapshot.source.total_items,
        eligible_items: state.snapshot.source.eligible_items,
        uptime_secs: state.started_at.elapsed().as_secs(),
        requests_served: state.requests_served.load(Ordering::Relaxed),
    })
    .into_response()
}

pub async fn meta(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Meta<'a> {
        api_version: u32,
        language_version: u32,
        lucene_dialect: &'static str,
        revision: &'a str,
        source: &'a crate::engine::CatalogSourceInfo,
        limits: QueryLimits,
        categories: &'a [crate::engine::CategoryMeta],
        fields: &'a [crate::engine::FieldMeta],
    }
    let resp = Json(Meta {
        api_version: 3,
        language_version: 4,
        lucene_dialect: "classic-10.3",
        revision: &state.snapshot.revision,
        source: &state.snapshot.source,
        limits: state.limits,
        categories: &state.snapshot.categories,
        fields: &state.snapshot.field_defs,
    })
    .into_response();
    with_meta_headers(resp, &state.snapshot.revision, 0, false)
}

pub async fn eval_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<EvalQueryParams>,
) -> Response {
    let start = Instant::now();
    let pinned = params.rev.as_deref() == Some(state.snapshot.revision.as_str());
    match state.snapshot.eval_set(
        &params.q,
        params.eligible,
        params.scores,
        params.complement,
        &state.limits,
    ) {
        Ok(res) => {
            let elapsed_us = start.elapsed().as_micros();
            let resp = if wants_packed(&headers, params.format.as_deref()) {
                (
                    [(header::CONTENT_TYPE, PACKED_CONTENT_TYPE)],
                    encode_eval(&res, params.scores),
                )
                    .into_response()
            } else {
                Json(res).into_response()
            };
            with_meta_headers(resp, &state.snapshot.revision, elapsed_us, pinned)
        }
        Err(err) => error_response(err),
    }
}

pub async fn eval_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<EvalPostBody>,
) -> Response {
    let start = Instant::now();
    match state.snapshot.eval_set(
        &body.q,
        body.eligible,
        body.scores,
        body.complement,
        &state.limits,
    ) {
        Ok(res) => {
            let elapsed_us = start.elapsed().as_micros();
            let resp = if wants_packed(&headers, body.format.as_deref()) {
                (
                    [(header::CONTENT_TYPE, PACKED_CONTENT_TYPE)],
                    encode_eval(&res, body.scores),
                )
                    .into_response()
            } else {
                Json(res).into_response()
            };
            with_meta_headers(resp, &state.snapshot.revision, elapsed_us, false)
        }
        Err(err) => error_response(err),
    }
}

pub async fn search_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<SearchQueryParams>,
) -> Response {
    let start = Instant::now();
    let pinned = params.rev.as_deref() == Some(state.snapshot.revision.as_str());
    match state.snapshot.search(
        &params.q,
        params.limit,
        params.offset,
        params.eligible,
        &state.limits,
    ) {
        Ok((hits, total, canonical, sort)) => {
            let elapsed_us = start.elapsed().as_micros();
            let resp = if wants_packed(&headers, params.format.as_deref()) {
                (
                    [(header::CONTENT_TYPE, PACKED_CONTENT_TYPE)],
                    encode_search(
                        &state.snapshot.revision,
                        &hits,
                        total,
                        params.offset,
                        params.fields,
                    ),
                )
                    .into_response()
            } else {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct SearchItemJson<'a> {
                    id: u32,
                    config_name: &'a str,
                    display_name: &'a str,
                    normalized_name: &'a str,
                    bankable: bool,
                    deleted: bool,
                    score: f64,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    fields: Option<&'a std::collections::BTreeMap<String, crate::engine::FieldValue>>,
                }
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct SearchResp<'a> {
                    revision: &'a str,
                    canonical_query: String,
                    sort: Vec<crate::engine::SortRule>,
                    total: usize,
                    offset: usize,
                    eval_us: u128,
                    results: Vec<SearchItemJson<'a>>,
                }
                let results = hits
                    .into_iter()
                    .map(|h| SearchItemJson {
                        id: h.item.id,
                        config_name: &h.item.config_name,
                        display_name: &h.item.display_name,
                        normalized_name: &h.item.normalized_name,
                        bankable: h.item.bankable,
                        deleted: h.item.deleted,
                        score: h.score,
                        fields: params.fields.then_some(&h.item.fields),
                    })
                    .collect();
                Json(SearchResp {
                    revision: &state.snapshot.revision,
                    canonical_query: canonical,
                    sort,
                    total,
                    offset: params.offset,
                    eval_us: elapsed_us,
                    results,
                })
                .into_response()
            };
            with_meta_headers(resp, &state.snapshot.revision, elapsed_us, pinned)
        }
        Err(err) => error_response(err),
    }
}

pub async fn find_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<FindQueryParams>,
) -> Response {
    let start = Instant::now();
    let limit = params.limit.clamp(1, state.limits.max_search_limit);
    let (items, total) = state.snapshot.find(&params.q, limit, params.eligible);
    let elapsed_us = start.elapsed().as_micros();

    let resp = if wants_packed(&headers, params.format.as_deref()) {
        (
            [(header::CONTENT_TYPE, PACKED_CONTENT_TYPE)],
            encode_find(&state.snapshot.revision, &items, total, params.fields),
        )
            .into_response()
    } else {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct FindItem<'a> {
            id: u32,
            config_name: &'a str,
            display_name: &'a str,
            normalized_name: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            fields: Option<&'a std::collections::BTreeMap<String, crate::engine::FieldValue>>,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct FindResp<'a> {
            revision: &'a str,
            total: usize,
            eval_us: u128,
            results: Vec<FindItem<'a>>,
        }
        let results = items
            .into_iter()
            .map(|it| FindItem {
                id: it.id,
                config_name: &it.config_name,
                display_name: &it.display_name,
                normalized_name: &it.normalized_name,
                fields: params.fields.then_some(&it.fields),
            })
            .collect();
        Json(FindResp {
            revision: &state.snapshot.revision,
            total,
            eval_us: elapsed_us,
            results,
        })
        .into_response()
    };
    with_meta_headers(resp, &state.snapshot.revision, elapsed_us, false)
}

pub async fn values_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<ValuesQueryParams>,
) -> Response {
    let start = Instant::now();
    let limit = params.limit.map(|l| l.clamp(1, 500));
    match state
        .snapshot
        .values(&params.field, params.q.as_deref(), limit)
    {
        Some((rows, total)) => {
            let elapsed_us = start.elapsed().as_micros();
            let resp = if wants_packed(&headers, params.format.as_deref()) {
                (
                    [(header::CONTENT_TYPE, PACKED_CONTENT_TYPE)],
                    encode_values(&state.snapshot.revision, &rows, total),
                )
                    .into_response()
            } else {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct ValuesResp<'a> {
                    revision: &'a str,
                    field: String,
                    total: usize,
                    values: Vec<&'a crate::engine::FieldValueSuggestion>,
                }
                Json(ValuesResp {
                    revision: &state.snapshot.revision,
                    field: params.field,
                    total,
                    values: rows,
                })
                .into_response()
            };
            with_meta_headers(resp, &state.snapshot.revision, elapsed_us, false)
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "unknown_field",
                "message": format!("Unknown catalog field: {}", params.field)
            })),
        )
            .into_response(),
    }
}

pub async fn item_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(config_name): Path<String>,
    Query(params): Query<ItemQueryParams>,
) -> Response {
    let start = Instant::now();
    let Some(item): Option<&CatalogItem> = state.snapshot.get(&config_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "message": format!("Item not found: {config_name}")
            })),
        )
            .into_response();
    };
    let elapsed_us = start.elapsed().as_micros();
    let resp = if wants_packed(&headers, params.format.as_deref()) {
        (
            [(header::CONTENT_TYPE, PACKED_CONTENT_TYPE)],
            encode_item(&state.snapshot.revision, item),
        )
            .into_response()
    } else {
        Json(item).into_response()
    };
    with_meta_headers(resp, &state.snapshot.revision, elapsed_us, false)
}

/// Lightweight local query workbench served at `/` for interactive testing.
pub async fn workbench() -> Html<&'static str> {
    Html(WORKBENCH_HTML)
}

const WORKBENCH_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>RuneQL OpenRS2 Server — Local Workbench</title>
<style>
  :root { color-scheme: dark; --bg: #0f1117; --card: #171a23; --border: #282d3d; --fg: #e6e9f2; --muted: #8b93a8; --accent: #6aa6ff; }
  * { box-sizing: border-box; }
  body { margin: 0; font-family: system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--fg); padding: 24px; max-width: 1040px; margin-inline: auto; }
  h1 { margin: 0 0 6px; font-size: 20px; }
  .sub { color: var(--muted); font-size: 13px; margin-bottom: 18px; }
  .bar { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 12px; }
  input, select, button { background: var(--card); color: var(--fg); border: 1px solid var(--border); border-radius: 6px; padding: 9px 12px; font-size: 14px; }
  input[type="text"] { flex: 1; min-width: 260px; font-family: ui-monospace, SFMono-Regular, monospace; }
  button { cursor: pointer; background: #22304d; border-color: #355084; font-weight: 600; }
  button:hover { background: #2b3d63; }
  .chips { display: flex; gap: 6px; flex-wrap: wrap; margin-bottom: 16px; }
  .chip { font-size: 12px; padding: 4px 9px; border-radius: 99px; background: var(--card); border: 1px solid var(--border); color: var(--accent); cursor: pointer; font-family: ui-monospace, monospace; }
  .meta { font-size: 12px; color: var(--muted); margin-bottom: 10px; display: flex; gap: 16px; flex-wrap: wrap; }
  pre { background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 14px; overflow: auto; max-height: 68vh; font-size: 12.5px; line-height: 1.45; }
</style>
</head>
<body>
  <h1>RuneQL Remote Query Server (OpenRS2 RS3)</h1>
  <div id="source" class="sub">Loading /v1/meta...</div>
  <div class="bar">
    <select id="endpoint">
      <option value="search">/v1/search (paginated hits)</option>
      <option value="eval">/v1/eval (configName set for Bank Tags)</option>
      <option value="find">/v1/find (ranked name picker)</option>
    </select>
    <input id="q" type="text" value="category:potions AND dose:[4 TO 6]" placeholder="Enter RuneQL query..." />
    <select id="format">
      <option value="json">JSON</option>
      <option value="packed">text/x-runeql-packed (Lua wire format)</option>
    </select>
    <button id="run">Run</button>
  </div>
  <div class="chips">
    <span class="chip">category:"melee-equipment" sort:tier:descending</span>
    <span class="chip">baseValue:[2500 TO *] sort:baseValue:descending</span>
    <span class="chip">category:potions AND dose:[4 TO 6] sort:dose:descending</span>
    <span class="chip">category:food sort:healAmount:descending</span>
    <span class="chip">augmentable:true AND tier:[90 TO *] sort:tier:descending</span>
    <span class="chip">combatStyle:necromancy AND slot:"main-hand"</span>
    <span class="chip">"abyssal whip" OR shark~1</span>
  </div>
  <div id="stats" class="meta"></div>
  <pre id="out">Run a query to inspect results.</pre>
<script>
const $ = id => document.getElementById(id);
fetch('/v1/meta').then(r => r.json()).then(m => {
  $('source').textContent = `OpenRS2 Cache #${m.source.openrs2CacheId} (RS3 build ${m.source.build}) • ${m.source.totalItems.toLocaleString()} items (${m.source.eligibleItems.toLocaleString()} bankable & active) • Overlay: ${m.source.overlayApplied ? m.source.overlayMatched.toLocaleString() + ' matched' : 'off'} • Rev ${m.revision.slice(0,12)}`;
});
async function runQuery() {
  const ep = $('endpoint').value;
  const q = $('q').value;
  const fmt = $('format').value;
  const url = `/v1/${ep}?q=${encodeURIComponent(q)}&format=${fmt}${ep !== 'eval' ? '&limit=25' : ''}`;
  const t0 = performance.now();
  const res = await fetch(url);
  const dt = (performance.now() - t0).toFixed(1);
  const evalUs = res.headers.get('x-eval-us') || '?';
  const text = await res.text();
  $('stats').textContent = `HTTP ${res.status} • Server eval: ${evalUs} µs • Round-trip: ${dt} ms • Payload: ${text.length.toLocaleString()} bytes`;
  if (fmt === 'json') {
    try { $('out').textContent = JSON.stringify(JSON.parse(text), null, 2); }
    catch { $('out').textContent = text; }
  } else {
    $('out').textContent = text;
  }
}
$('run').onclick = runQuery;
$('q').addEventListener('keydown', e => { if (e.key === 'Enter') runQuery(); });
document.querySelectorAll('.chip').forEach(el => {
  el.onclick = () => { $('q').value = el.textContent; runQuery(); };
});
runQuery();
</script>
</body>
</html>
"#;
