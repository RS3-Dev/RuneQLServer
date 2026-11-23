# RuneQL Server & Edge Worker (`runeql.rs3.dev`)

Immutable, in-memory [RuneQL](https://github.com/RS3-Dev/RuneQL) query engine built directly from live English RuneScape 3 caches on [OpenRS2](https://archive.openrs2.org/), deployed globally on **Cloudflare Workers (WASM)** at **`https://runeql.rs3.dev`**.

## Repository Layout

| Path | Description |
| --- | --- |
| `server/` | Native Rust OpenRS2 cache downloader/decoder, catalog synthesizer, inverted-index query engine, local HTTP server (`127.0.0.1:8787`), and compact `RQL1` snapshot exporter. |
| `worker/` | Cloudflare Workers edge package (`wrangler.toml`, JS fetch/edge-cache entrypoint, raw `wasm32-unknown-unknown` wrapper crate, and `deploy.ps1`). |
| `overlay/` | Curated RuneQL attribute overlay shards (`attributes_00..31.lua`) automatically merged on top of live OpenRS2 cache definitions. |

## Architecture

- **Memory-optimized inverted index:** Hybrid `PostingList` (`Sparse(Vec<u32>)` promoted to dense `BitSet` at 1,024 docs), sorted token vectors, and compact `RQL1` interned binary snapshots keep the entire 44,696-item RS3 catalog inside **~57.6 MB** WASM linear memory (well under Cloudflare Workers' 128 MB isolate limit) and **~1.68 MiB** total bundle size (`523 KB` `.wasm` + `1.12 MB` gzipped `catalog.rql.gz`).
- **Per-PoP edge caching:** `GET /v1/*` responses cache at Cloudflare's edge (`caches.default`), with `Cache-Control: public, max-age=31536000, immutable` when `?rev=<current>` is pinned.
- **Zero-inventory privacy:** Only the query string is sent to the server; player bank contents, quantities, GE prices, and sorting stay 100% client-side in the RuneQL RS3 plugin.

## Endpoints (`https://runeql.rs3.dev`)

- `GET /` — Interactive query workbench
- `GET /health` — Server status, OpenRS2 cache ID, RS3 build, item counts, revision
- `GET /v1/meta` — API version, limits, categories, field metadata
- `GET /v1/eval?q=...&format=packed` & `POST /v1/eval` — Matching `configName` set (with automatic complement encoding for broad sets)
- `GET /v1/search?q=...&limit=40&offset=0` — Paginated ranked hits (supports `sort:<field>[:asc|desc]`)
- `GET /v1/find?q=...&limit=40&fields=false` — Ranked display-name picker
- `GET /v1/values?field=...` — Field value autocomplete suggestions
- `GET /v1/items/{configName}` — Single item metadata lookup

## Deploying / Refreshing After Weekly RS3 Updates

### Prerequisites
- Rust toolchain with `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`)
- Node.js + authenticated Wrangler CLI (`npx wrangler login`)

### One-command snapshot refresh & deploy (PowerShell)
```powershell
.\worker\deploy.ps1
```
This script:
1. Builds `server/` in release mode and fetches the latest English RS3 cache from OpenRS2 (cached under `server/.cache/openrs2/`).
2. Merges `overlay/` and exports `worker/src/catalog.rql.gz` (`RQL1` binary format).
3. Compiles `worker/wasm/` to `worker/src/runeql_worker.wasm`.
4. Deploys to Cloudflare Workers (`https://runeql.rs3.dev`) via `npx wrangler deploy`.

Useful flags:
- `.\worker\deploy.ps1 -DryRun` — build everything and dry-run Wrangler without deploying
- `.\worker\deploy.ps1 -SkipWasm` — refresh OpenRS2 snapshot only and redeploy
- `.\worker\deploy.ps1 -SkipExport` — rebuild WASM code only and redeploy

## Running Locally

```powershell
cd server
cargo test
cargo run --release
# Local workbench & API at http://127.0.0.1:8787
```

See [server/README.md](server/README.md) for CLI options (`--cache-id`, `--no-overlay`, `--bind`, `query`, `build`, `export-snapshot`).
