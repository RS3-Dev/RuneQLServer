# RuneQL Remote Query Server (OpenRS2 RS3 Prototype)

An immutable, in-memory Rust query server for RuneQL that builds its item catalog directly from the latest live English RuneScape 3 cache on [OpenRS2](https://archive.openrs2.org/).

## How it builds from OpenRS2 (without downloading 25 GB)

Instead of downloading an entire ~25 GB RS3 `disk.zip`, the server queries `https://archive.openrs2.org/caches.json` and fetches **only three lightweight pieces** (~2 MB compressed total), caching them under `.cache/openrs2/`:

1. **Archive 255 / Group 19** — Archive 19 reference table.
2. **Archive 67 / Groups 36 & 37** — Jagex's live `config.Obj` (`item_id -> configName`) and `config.Param` (`param_id -> paramName`) tables.
3. **Archive 19 / Groups `0..N`** — All ~64,000 RS3 item definitions (names, values, members, GE tradeable, equip slots, widget actions, and opcode 249 `params` including `NONBANKABLE`, `QUICKCHAT_DONT_LIST`, `WEAR_REQUIRES_STAT*`, `COMBATV2_*`, `INVENT_*`, `DEGRADEABLE_ITEM`, `DIANGO_*`, `GE_CATEGORY`, `FOOD_*`, `POTION_*`).

Subsequent startups load from `.cache/openrs2/` in a few hundred milliseconds with no network traffic.

By default, if `../resources/catalog` is present, RuneQL's curated attributes are merged as an overlay while keeping live OpenRS2 names, eligibility, values, and newly added items up to date. Pass `--no-overlay` to run 100% pure OpenRS2 cache-derived classification.

## Quick start

```powershell
cd server

# Build and start local HTTP server (auto-selects latest live EN RS3 cache)
cargo run --release

# Run 100% pure OpenRS2 mode without RuneQL's curated attribute overlay
cargo run --release -- --no-overlay

# Run a one-off CLI query (including sort:<field>[:ascending|descending|asc|desc])
cargo run --release -- query 'category:"melee-equipment" sort:tier:descending'
cargo run --release -- query "category:potions AND dose:[4 TO 6] sort:dose:descending"

# Pin a specific OpenRS2 cache ID
cargo run --release -- --cache-id 2767 build
```

### Sorting syntax

Queries support top-level `sort:<field>[:<ascending|descending|asc|desc>]` directives (spaces around `:` are allowed, and multiple keys can be repeated or comma-separated):

```text
gePrice:{2500 TO *} sort:gePrice:descending
category:"melee-equipment" sort:tier:descending,gePrice:descending
sort:stackValue:descending
```

- Missing values always sort last (`NULLS LAST`), followed by RuneQL's deterministic tie-breaker.
- `/v1/eval` parses and returns the `sort` spec while stripping `sort:` from the canonical filter cache key so changing client-side sort order reuses the same cached result set.

Then open **http://127.0.0.1:8080** in your browser for the interactive workbench, or hit the endpoints directly:

- `GET /health`
- `GET /v1/meta`
- `GET /v1/eval?q=category:potions&format=packed` (`Accept: text/x-runeql-packed` or `application/json`)
- `POST /v1/eval` (JSON body `{ "q": "...", "scores": true, "complement": true }`)
- `GET /v1/search?q=augmentable:true+AND+tier:[90+TO+*]&limit=25`
- `GET /v1/find?q=abyssal+whip&limit=20`
- `GET /v1/values?field=category&q=pot`
- `GET /v1/items/ABYSSAL_WHIP`
