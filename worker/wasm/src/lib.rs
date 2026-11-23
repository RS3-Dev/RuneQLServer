use std::alloc::{Layout, alloc, dealloc};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};

mod config {
    #[derive(Debug, Clone, Copy, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct QueryLimits {
        pub max_query_bytes: usize,
        pub max_clauses: usize,
        pub max_nesting_depth: usize,
        pub max_expensive_clauses: usize,
        pub max_regex_length: usize,
        pub max_regex_depth: usize,
        pub max_regex_repeat: usize,
        pub max_regex_steps: usize,
        pub max_eval_steps: usize,
        pub max_eval_ms: u64,
        pub max_eval_results: usize,
        pub max_search_limit: usize,
    }

    impl Default for QueryLimits {
        fn default() -> Self {
            Self {
                max_query_bytes: 4096,
                max_clauses: 128,
                max_nesting_depth: 8,
                max_expensive_clauses: 6,
                max_regex_length: 128,
                max_regex_depth: 8,
                max_regex_repeat: 64,
                max_regex_steps: 25_000,
                max_eval_steps: 500_000,
                max_eval_ms: 50,
                max_eval_results: 0,
                max_search_limit: 200,
            }
        }
    }
}

#[path = "../../../server/src/engine/mod.rs"]
mod engine;

#[path = "../../../server/src/http/packed.rs"]
mod packed;

use crate::config::QueryLimits;
use crate::engine::index::CatalogSnapshot;
use crate::engine::snapshot_codec::decode_snapshot;
use crate::engine::{CatalogItem, FieldValue, QueryError};
use crate::packed::{
    PACKED_CONTENT_TYPE, encode_eval, encode_find, encode_item, encode_search, encode_values,
};

struct WorkerState {
    snapshot: CatalogSnapshot,
    limits: QueryLimits,
    requests_served: AtomicU64,
}

static STATE: OnceLock<WorkerState> = OnceLock::new();

#[unsafe(no_mangle)]
pub extern "C" fn wasm_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let layout = match Layout::from_size_align(len, 1) {
        Ok(l) => l,
        Err(_) => return std::ptr::null_mut(),
    };
    unsafe { alloc(layout) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    if let Ok(layout) = Layout::from_size_align(len, 1) {
        unsafe { dealloc(ptr, layout) };
    }
}

/// Initializes the global immutable `CatalogSnapshot` from raw or gzipped `RQL1` snapshot bytes.
/// Returns `0` on success, negative error code on failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_init(ptr: *const u8, len: usize) -> i32 {
    if STATE.get().is_some() {
        return 0;
    }
    if ptr.is_null() || len < 4 {
        return -1;
    }
    let input = unsafe { std::slice::from_raw_parts(ptr, len) };

    let decoded = if input.len() >= 2 && input[0] == 0x1F && input[1] == 0x8B {
        let mut decoder = GzDecoder::new(input);
        let mut raw = Vec::with_capacity(input.len() * 4);
        if decoder.read_to_end(&mut raw).is_err() {
            return -2;
        }
        decode_snapshot(&raw)
    } else {
        decode_snapshot(input)
    };

    let Ok(payload) = decoded else {
        return -3;
    };

    let snapshot = CatalogSnapshot::build_presorted(
        payload.items,
        payload.revision,
        payload.source,
        payload.categories,
        payload.field_defs,
    );

    let _ = STATE.set(WorkerState {
        snapshot,
        limits: QueryLimits::default(),
        requests_served: AtomicU64::new(0),
    });
    0
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WasmRequest {
    method: String,
    path: String,
    #[serde(default)]
    query: BTreeMap<String, String>,
    #[serde(default)]
    accept: String,
    #[serde(default)]
    body: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResponseHeader {
    status: u16,
    content_type: String,
    revision: String,
    immutable_cache: bool,
}

#[derive(Deserialize)]
struct EvalPostBody {
    q: String,
    #[serde(default = "default_true")]
    eligible: bool,
    #[serde(default = "default_true")]
    scores: bool,
    #[serde(default = "default_true")]
    complement: bool,
    #[serde(default)]
    format: Option<String>,
}

fn default_true() -> bool {
    true
}

fn query_bool(query: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    match query.get(key).map(|s| s.as_str()) {
        Some("true" | "1" | "yes" | "TRUE") => true,
        Some("false" | "0" | "no" | "FALSE") => false,
        _ => default,
    }
}

fn query_usize(query: &BTreeMap<String, String>, key: &str) -> Option<usize> {
    query.get(key).and_then(|s| s.parse::<usize>().ok())
}

fn wants_packed(accept: &str, format_param: Option<&str>) -> bool {
    if let Some(fmt) = format_param {
        return fmt.eq_ignore_ascii_case("packed") || fmt.eq_ignore_ascii_case("tsv");
    }
    accept.contains("text/x-runeql-packed")
}

fn build_wire_response(
    status: u16,
    content_type: &str,
    revision: &str,
    immutable_cache: bool,
    body: &[u8],
) -> u64 {
    let hdr = ResponseHeader {
        status,
        content_type: content_type.to_string(),
        revision: revision.to_string(),
        immutable_cache,
    };
    let hdr_bytes = serde_json::to_vec(&hdr).unwrap_or_default();
    let total_len = 4 + hdr_bytes.len() + body.len();
    let out_ptr = wasm_alloc(total_len);
    if out_ptr.is_null() {
        return 0;
    }
    unsafe {
        let slice = std::slice::from_raw_parts_mut(out_ptr, total_len);
        slice[..4].copy_from_slice(&(hdr_bytes.len() as u32).to_le_bytes());
        slice[4..4 + hdr_bytes.len()].copy_from_slice(&hdr_bytes);
        slice[4 + hdr_bytes.len()..].copy_from_slice(body);
    }
    ((out_ptr as u64) << 32) | (total_len as u64)
}

fn json_error(status: u16, code: &str, message: &str, rev: &str) -> u64 {
    let payload = serde_json::json!({
        "error": code,
        "message": message,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    build_wire_response(status, "application/json; charset=utf-8", rev, false, &bytes)
}

fn query_error_response(err: QueryError, rev: &str) -> u64 {
    let (status, code) = match &err {
        QueryError::InvalidSyntax(_) => (400, "invalid_syntax"),
        QueryError::LimitExceeded(_) => (400, "limit_exceeded"),
        QueryError::BudgetExceeded(_) => (422, "budget_exceeded"),
        QueryError::ResultTooBroad { .. } => (413, "result_too_broad"),
    };
    json_error(status, code, &err.to_string(), rev)
}

/// Handles a single HTTP request encoded as JSON `{ method, path, query, accept, body }`.
/// Returns `((out_ptr as u64) << 32) | (out_len as u64)` where output is `[u32 header_len][header JSON][raw body]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_handle(ptr: *const u8, len: usize) -> u64 {
    let Some(state) = STATE.get() else {
        return json_error(503, "not_initialized", "RuneQL snapshot not initialized", "");
    };
    let rev = state.snapshot.revision.as_str();

    if ptr.is_null() || len == 0 {
        return json_error(400, "bad_request", "Empty request", rev);
    }
    let req_bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(req) = serde_json::from_slice::<WasmRequest>(req_bytes) else {
        return json_error(400, "bad_request", "Malformed worker request envelope", rev);
    };

    let req_count = state.requests_served.fetch_add(1, Ordering::Relaxed) + 1;
    let path = req.path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };

    match (req.method.as_str(), path) {
        ("GET", "/") => build_wire_response(
            200,
            "text/html; charset=utf-8",
            rev,
            false,
            WORKBENCH_HTML.as_bytes(),
        ),
        ("GET", "/health") => {
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Health<'a> {
                status: &'static str,
                runtime: &'static str,
                revision: &'a str,
                openrs2_cache_id: u32,
                build: &'a str,
                total_items: usize,
                eligible_items: usize,
                requests_served: u64,
            }
            let body = serde_json::to_vec(&Health {
                status: "ok",
                runtime: "cloudflare-workers-wasm",
                revision: rev,
                openrs2_cache_id: state.snapshot.source.openrs2_cache_id,
                build: &state.snapshot.source.build,
                total_items: state.snapshot.source.total_items,
                eligible_items: state.snapshot.source.eligible_items,
                requests_served: req_count,
            })
            .unwrap_or_default();
            build_wire_response(200, "application/json; charset=utf-8", rev, false, &body)
        }
        ("GET", "/v1/meta") => {
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Meta<'a> {
                api_version: u32,
                language_version: u32,
                lucene_dialect: &'static str,
                revision: &'a str,
                source: &'a engine::CatalogSourceInfo,
                limits: QueryLimits,
                categories: &'a [engine::CategoryMeta],
                fields: &'a [engine::FieldMeta],
            }
            let body = serde_json::to_vec(&Meta {
                api_version: 3,
                language_version: 4,
                lucene_dialect: "classic-10.3",
                revision: rev,
                source: &state.snapshot.source,
                limits: state.limits,
                categories: &state.snapshot.categories,
                fields: &state.snapshot.field_defs,
            })
            .unwrap_or_default();
            build_wire_response(200, "application/json; charset=utf-8", rev, false, &body)
        }
        ("GET", "/v1/eval") => {
            let Some(q) = req.query.get("q") else {
                return json_error(400, "missing_param", "Missing required query parameter 'q'", rev);
            };
            let eligible = query_bool(&req.query, "eligible", true);
            let scores = query_bool(&req.query, "scores", true);
            let complement = query_bool(&req.query, "complement", true);
            let pinned = req.query.get("rev").map(|s| s.as_str()) == Some(rev);
            let fmt = req.query.get("format").map(|s| s.as_str());

            match state
                .snapshot
                .eval_set(q, eligible, scores, complement, &state.limits)
            {
                Ok(res) => {
                    if wants_packed(&req.accept, fmt) {
                        let body = encode_eval(&res, scores);
                        build_wire_response(200, PACKED_CONTENT_TYPE, rev, pinned, body.as_bytes())
                    } else {
                        let body = serde_json::to_vec(&res).unwrap_or_default();
                        build_wire_response(
                            200,
                            "application/json; charset=utf-8",
                            rev,
                            pinned,
                            &body,
                        )
                    }
                }
                Err(err) => query_error_response(err, rev),
            }
        }
        ("POST", "/v1/eval") => {
            let Ok(post) = serde_json::from_str::<EvalPostBody>(&req.body) else {
                return json_error(400, "invalid_body", "Expected JSON body with field 'q'", rev);
            };
            match state.snapshot.eval_set(
                &post.q,
                post.eligible,
                post.scores,
                post.complement,
                &state.limits,
            ) {
                Ok(res) => {
                    if wants_packed(&req.accept, post.format.as_deref()) {
                        let body = encode_eval(&res, post.scores);
                        build_wire_response(200, PACKED_CONTENT_TYPE, rev, false, body.as_bytes())
                    } else {
                        let body = serde_json::to_vec(&res).unwrap_or_default();
                        build_wire_response(
                            200,
                            "application/json; charset=utf-8",
                            rev,
                            false,
                            &body,
                        )
                    }
                }
                Err(err) => query_error_response(err, rev),
            }
        }
        ("GET", "/v1/search") => {
            let Some(q) = req.query.get("q") else {
                return json_error(400, "missing_param", "Missing required query parameter 'q'", rev);
            };
            let limit = query_usize(&req.query, "limit").unwrap_or(40);
            let offset = query_usize(&req.query, "offset").unwrap_or(0);
            let eligible = query_bool(&req.query, "eligible", true);
            let include_fields = query_bool(&req.query, "fields", true);
            let pinned = req.query.get("rev").map(|s| s.as_str()) == Some(rev);
            let fmt = req.query.get("format").map(|s| s.as_str());

            match state
                .snapshot
                .search(q, limit, offset, eligible, &state.limits)
            {
                Ok((hits, total, canonical, sort)) => {
                    if wants_packed(&req.accept, fmt) {
                        let body = encode_search(rev, &hits, total, offset, include_fields);
                        build_wire_response(200, PACKED_CONTENT_TYPE, rev, pinned, body.as_bytes())
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
                            fields: Option<&'a BTreeMap<String, FieldValue>>,
                        }
                        #[derive(Serialize)]
                        #[serde(rename_all = "camelCase")]
                        struct SearchResp<'a> {
                            revision: &'a str,
                            canonical_query: String,
                            sort: Vec<engine::SortRule>,
                            total: usize,
                            offset: usize,
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
                                fields: include_fields.then_some(&h.item.fields),
                            })
                            .collect();
                        let body = serde_json::to_vec(&SearchResp {
                            revision: rev,
                            canonical_query: canonical,
                            sort,
                            total,
                            offset,
                            results,
                        })
                        .unwrap_or_default();
                        build_wire_response(
                            200,
                            "application/json; charset=utf-8",
                            rev,
                            pinned,
                            &body,
                        )
                    }
                }
                Err(err) => query_error_response(err, rev),
            }
        }
        ("GET", "/v1/find") => {
            let Some(q) = req.query.get("q") else {
                return json_error(400, "missing_param", "Missing required query parameter 'q'", rev);
            };
            let limit = query_usize(&req.query, "limit")
                .unwrap_or(40)
                .clamp(1, state.limits.max_search_limit);
            let eligible = query_bool(&req.query, "eligible", true);
            let include_fields = query_bool(&req.query, "fields", true);
            let fmt = req.query.get("format").map(|s| s.as_str());

            let (items, total) = state.snapshot.find(q, limit, eligible);
            if wants_packed(&req.accept, fmt) {
                let body = encode_find(rev, &items, total, include_fields);
                build_wire_response(200, PACKED_CONTENT_TYPE, rev, false, body.as_bytes())
            } else {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct FindItem<'a> {
                    id: u32,
                    config_name: &'a str,
                    display_name: &'a str,
                    normalized_name: &'a str,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    fields: Option<&'a BTreeMap<String, FieldValue>>,
                }
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct FindResp<'a> {
                    revision: &'a str,
                    total: usize,
                    results: Vec<FindItem<'a>>,
                }
                let results = items
                    .into_iter()
                    .map(|it| FindItem {
                        id: it.id,
                        config_name: &it.config_name,
                        display_name: &it.display_name,
                        normalized_name: &it.normalized_name,
                        fields: include_fields.then_some(&it.fields),
                    })
                    .collect();
                let body = serde_json::to_vec(&FindResp {
                    revision: rev,
                    total,
                    results,
                })
                .unwrap_or_default();
                build_wire_response(200, "application/json; charset=utf-8", rev, false, &body)
            }
        }
        ("GET", "/v1/values") => {
            let Some(field) = req.query.get("field") else {
                return json_error(
                    400,
                    "missing_param",
                    "Missing required query parameter 'field'",
                    rev,
                );
            };
            let filter_q = req.query.get("q").map(|s| s.as_str());
            let limit = query_usize(&req.query, "limit").map(|l| l.clamp(1, 500));
            let fmt = req.query.get("format").map(|s| s.as_str());

            match state.snapshot.values(field, filter_q, limit) {
                Some((rows, total)) => {
                    if wants_packed(&req.accept, fmt) {
                        let body = encode_values(rev, &rows, total);
                        build_wire_response(200, PACKED_CONTENT_TYPE, rev, false, body.as_bytes())
                    } else {
                        #[derive(Serialize)]
                        #[serde(rename_all = "camelCase")]
                        struct ValuesResp<'a> {
                            revision: &'a str,
                            field: &'a str,
                            total: usize,
                            values: Vec<&'a engine::FieldValueSuggestion>,
                        }
                        let body = serde_json::to_vec(&ValuesResp {
                            revision: rev,
                            field,
                            total,
                            values: rows,
                        })
                        .unwrap_or_default();
                        build_wire_response(
                            200,
                            "application/json; charset=utf-8",
                            rev,
                            false,
                            &body,
                        )
                    }
                }
                None => json_error(
                    404,
                    "unknown_field",
                    &format!("Unknown catalog field: {field}"),
                    rev,
                ),
            }
        }
        ("GET", p) if p.starts_with("/v1/items/") => {
            let config_name = &p["/v1/items/".len()..];
            let fmt = req.query.get("format").map(|s| s.as_str());
            let Some(item): Option<&CatalogItem> = state.snapshot.get(config_name) else {
                return json_error(404, "not_found", &format!("Item not found: {config_name}"), rev);
            };
            if wants_packed(&req.accept, fmt) {
                let body = encode_item(rev, item);
                build_wire_response(200, PACKED_CONTENT_TYPE, rev, false, body.as_bytes())
            } else {
                let body = serde_json::to_vec(item).unwrap_or_default();
                build_wire_response(200, "application/json; charset=utf-8", rev, false, &body)
            }
        }
        _ => json_error(404, "not_found", &format!("Route not found: {} {}", req.method, path), rev),
    }
}

const WORKBENCH_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>RuneQL Edge Query Server — runeql.rs3.dev</title>
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
  <h1>RuneQL Edge Query Server (Cloudflare Workers + OpenRS2 RS3)</h1>
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
  const cache = res.headers.get('x-runeql-edge-cache') || 'BYPASS';
  const text = await res.text();
  $('stats').textContent = `HTTP ${res.status} • Edge cache: ${cache} • Edge eval: ${evalUs} µs • Round-trip: ${dt} ms • Payload: ${text.length.toLocaleString()} bytes`;
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
