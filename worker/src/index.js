import wasmModule from "./runeql_worker.wasm";
import snapshotGz from "./catalog.rql.gz";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

const CORS_HEADERS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, Accept",
  "Access-Control-Expose-Headers": "X-RuneQL-Rev, X-Eval-Us, X-RuneQL-Edge-Cache",
};

let wasmExports = null;
let initError = null;
let initPromise = null;

async function decompressGzip(arrayBuffer) {
  const stream = new Blob([arrayBuffer])
    .stream()
    .pipeThrough(new DecompressionStream("gzip"));
  const buf = await new Response(stream).arrayBuffer();
  return new Uint8Array(buf);
}

async function ensureInitialized() {
  if (wasmExports) return wasmExports;
  if (initError) throw initError;
  if (initPromise) return initPromise;

  initPromise = (async () => {
    const instance = new WebAssembly.Instance(wasmModule, {});
    const exp = instance.exports;

    // Decompress gzipped RQL1 snapshot via native Workers DecompressionStream (zero JS/WASM CPU cost!)
    // and hand raw bytes to wasm_init, freeing the input buffer immediately after indexing.
    let rawBytes = await decompressGzip(snapshotGz);
    const inLen = rawBytes.byteLength;
    const inPtr = exp.wasm_alloc(inLen);
    if (!inPtr) {
      throw new Error(`wasm_alloc(${inLen}) returned null during init`);
    }
    try {
      new Uint8Array(exp.memory.buffer, inPtr, inLen).set(rawBytes);
      rawBytes = null; // allow V8 GC to reclaim JS buffer before index allocation
      const rc = exp.wasm_init(inPtr, inLen);
      if (rc !== 0) {
        throw new Error(`wasm_init failed with code ${rc}`);
      }
    } finally {
      exp.wasm_free(inPtr, inLen);
    }

    wasmExports = exp;
    return exp;
  })().catch((err) => {
    initError = err;
    initPromise = null;
    throw err;
  });

  return initPromise;
}

function invokeWasm(exp, reqObj) {
  const reqBytes = encoder.encode(JSON.stringify(reqObj));
  const reqLen = reqBytes.byteLength;
  const reqPtr = exp.wasm_alloc(reqLen);
  if (!reqPtr) {
    throw new Error("wasm_alloc failed for request");
  }

  let packed = 0n;
  try {
    new Uint8Array(exp.memory.buffer, reqPtr, reqLen).set(reqBytes);
    packed = exp.wasm_handle(reqPtr, reqLen);
  } finally {
    exp.wasm_free(reqPtr, reqLen);
  }

  const outPtr = Number(packed >> 32n);
  const outLen = Number(packed & 0xffffffffn);
  if (!outPtr || outLen < 4) {
    throw new Error("wasm_handle returned empty response");
  }

  try {
    const mem = new Uint8Array(exp.memory.buffer, outPtr, outLen);
    const view = new DataView(exp.memory.buffer, outPtr, 4);
    const headerLen = view.getUint32(0, true);
    const headerJson = decoder.decode(mem.subarray(4, 4 + headerLen));
    const header = JSON.parse(headerJson);
    const body = mem.slice(4 + headerLen);
    return { header, body };
  } finally {
    exp.wasm_free(outPtr, outLen);
  }
}

export default {
  async fetch(request, _env, ctx) {
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS_HEADERS });
    }

    const url = new URL(request.url);
    const isCacheableGet =
      request.method === "GET" &&
      (url.pathname.startsWith("/v1/") || url.pathname === "/");

    // Check Cloudflare's per-PoP edge cache first: cache hits return in <1ms with ZERO WASM execution!
    let cache = null;
    let cacheKey = null;
    if (isCacheableGet && typeof caches !== "undefined" && caches.default) {
      cache = caches.default;
      const accept = request.headers.get("Accept") || "";
      const keyUrl = new URL(url.toString());
      if (accept.includes("text/x-runeql-packed") && !keyUrl.searchParams.has("format")) {
        keyUrl.searchParams.set("_accept", "packed");
      }
      cacheKey = new Request(keyUrl.toString(), { method: "GET" });
      const cached = await cache.match(cacheKey);
      if (cached) {
        const hitHeaders = new Headers(cached.headers);
        hitHeaders.set("X-RuneQL-Edge-Cache", "HIT");
        for (const [k, v] of Object.entries(CORS_HEADERS)) {
          hitHeaders.set(k, v);
        }
        return new Response(cached.body, {
          status: cached.status,
          headers: hitHeaders,
        });
      }
    }

    let exp;
    try {
      exp = await ensureInitialized();
    } catch (err) {
      return new Response(
        JSON.stringify({ error: "init_failed", message: String(err) }),
        {
          status: 503,
          headers: { "Content-Type": "application/json; charset=utf-8", ...CORS_HEADERS },
        }
      );
    }

    const query = {};
    for (const [k, v] of url.searchParams.entries()) {
      query[k] = v;
    }

    let bodyText = "";
    if (request.method === "POST") {
      bodyText = await request.text();
      if (bodyText.length > 8192) {
        return new Response(
          JSON.stringify({ error: "payload_too_large", message: "Request body exceeds 8 KB" }),
          {
            status: 413,
            headers: { "Content-Type": "application/json; charset=utf-8", ...CORS_HEADERS },
          }
        );
      }
    }

    const t0 = Date.now();
    const { header, body } = invokeWasm(exp, {
      method: request.method,
      path: decodeURIComponent(url.pathname),
      query,
      accept: request.headers.get("Accept") || "",
      body: bodyText,
    });
    const evalMs = Math.max(0, Date.now() - t0);

    const headers = new Headers(CORS_HEADERS);
    headers.set("Content-Type", header.contentType);
    if (header.revision) {
      headers.set("X-RuneQL-Rev", header.revision);
    }
    headers.set("X-Eval-Us", String(evalMs * 1000));
    headers.set("X-RuneQL-Edge-Cache", isCacheableGet ? "MISS" : "BYPASS");

    if (header.status === 200 && isCacheableGet) {
      if (header.immutableCache) {
        headers.set("Cache-Control", "public, max-age=31536000, immutable");
      } else if (url.pathname === "/v1/meta") {
        headers.set("Cache-Control", "public, max-age=300, stale-while-revalidate=3600");
      } else {
        headers.set("Cache-Control", "public, max-age=300, stale-while-revalidate=86400");
      }
    } else {
      headers.set("Cache-Control", "no-store");
    }

    const response = new Response(body, { status: header.status, headers });

    if (cache && cacheKey && header.status === 200) {
      ctx.waitUntil(cache.put(cacheKey, response.clone()));
    }

    return response;
  },
};
