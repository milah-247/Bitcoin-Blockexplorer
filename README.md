# Bitcoin Block Explorer

A Bitcoin block explorer: a Rust JSON API (axum) plus a dependency-free web frontend,
served together by one binary. It runs against **Bitcoin mainnet through a hosted,
rate-limited RPC provider**. You do not need your own node; a local Bitcoin Core node
(regtest/signet/testnet/mainnet) also works.

```
cp .env.example .env        # put your RPC key in .env (git-ignored)
cargo run --release         # API + frontend on http://127.0.0.1:3000
```

---

## Contents

1. [Architecture](#architecture)
2. [What the RPC provider supports](#what-the-rpc-provider-supports)
3. [Setup for mainnet](#setup-for-mainnet)
4. [Configuration](#configuration)
5. [API](#api)
6. [The address index](#the-address-index)
7. [Frontend](#frontend)
8. [Tests](#tests)
9. [Known limitations](#known-limitations)

---

## Architecture

```
 Browser ──HTTP──►  axum server (one binary, one port)
                    │
                    ├─ /             ServeDir(frontend/)  index.html, app.js, style.css
                    │
                    └─ /api/*        TraceLayer (path, status, latency)
                                     CORS (optional)  ·  request timeout  ·  JSON errors
                                         │
                                     handlers (blocks, tx, address, search, health)
                                         │
                                     chain.rs: cached data access
                                       ├─ moka cache ─── hit ──► response
                                       ├─ SQLite index (blocks, txids, addresses in range)
                                       └─ rpc.rs JSON-RPC client
                                            semaphore · token bucket · provider
                                            rate-limit headers · retry/backoff · batching
                                                 │ HTTPS + X-API-Key
                                                 ▼
                                   Hosted Bitcoin Core 28.1 (mainnet)
                                                 ▲
                    indexer.rs (background task) │ getblock verbosity 0, pipelined
                       └── writes ──► data/index-bitcoin.sqlite
```

| Module | Role |
|---|---|
| `rpc.rs` | Async JSON-RPC client (reqwest/rustls). Basic, cookie or API-key header auth. Concurrency limit, client-side rate limit, honours `X-RateLimit-*`/`Retry-After`, retries 429/5xx/connection errors with exponential backoff, JSON-RPC batches. |
| `cache.rs` | moka cache with a TTL per entry (long / short / tip). Concurrent requests for the same key share one RPC call. |
| `chain.rs` | Cached lookups: tip, height→hash, block summaries, block fees, transactions with prevouts. |
| `index.rs`, `indexer.rs` | SQLite address index and its background sync with reorg handling. |
| `check.rs` | `--check`: probes what the endpoint supports. |
| `handlers/` | HTTP handlers; `router.rs` wires routes and middleware. |
| `frontend/` | Plain HTML/CSS/JS, hash routing, no build step, no CDNs. |

## What the RPC provider supports

Run `cargo run --release -- --check` against any endpoint. Results for
`https://bitrpc.thebuidl.xyz/bitcoin` (Oct 2026):

```
capability                       | ok?  | detail
---------------------------------+------+---------------------------------------------------------
chain                            | yes  | "main", Bitcoin Core 28.1, not pruned
getblock verbosity 1             | yes  | ~0.3 MB, ~0.7 s
getblock verbosity 3             | yes  | ~13 MB, ~25 s per mainnet block
getblock verbosity 0             | yes  | ~3 MB hex, ~4-5 s
txindex (getrawtransaction)      | NO   | old txs need their block hash
getrawtransaction + blockhash    | yes  | verbosity 2 gives prevouts + fee, ~0.5 s
getblockstats                    | NO   | Method "getblockstats" is not permitted
getblockheader                   | NO   | not permitted
scantxoutset                     | NO   | not permitted
gettxout, getrawmempool, getchaintips, estimatesmartfee, getindexinfo | NO
getmempoolinfo                   | yes  |
JSON-RPC batch (100 calls)       | yes  | one batch counts as ONE request against the rate limit
provider rate limit              |      | 100 requests per minute per key
```

These results drive the design:

| Constraint | What the explorer does instead |
|---|---|
| No `getblockheader` | Block summaries come from `getblock` v1 (or the index) and are cached forever by hash. Lists fetch all missing blocks in **one batch**. |
| No `getblockstats` | Total fees = coinbase outputs − block subsidy (one ~1 KB call, cached). |
| No txindex | `?block=` hint on `/api/tx/:txid`. The frontend always passes it. The index resolves txids in its range; mempool txs always work. |
| `getblock` v3 is 13 MB | Tx detail uses `getrawtransaction(txid, 2, blockhash)`: prevouts and fee for a single tx in about 0.5 s. |
| No `scantxoutset` / address index | Own SQLite address index (below). |
| 100 req/min | Client rate limit (default 90/min), batching, caching, pausing when the provider reports `remaining: 0`. |

## Setup for mainnet

Requirements: Rust 1.80+ (stable). No system SQLite or OpenSSL needed (both are bundled/rustls).

1. **Credentials.** Copy `.env.example` to `.env` and set `RPC_API_KEY`. `.env` is in
   `.gitignore`; never commit it. Environment variables override `.env`.
   ```
   NETWORK=mainnet
   RPC_URL=https://bitrpc.thebuidl.xyz/bitcoin
   RPC_API_KEY=<your key>
   RPC_RATE_LIMIT_PER_MIN=90
   START_HEIGHT=tip-144
   ```
2. **Check the endpoint**: `cargo run --release -- --check` (about 25 requests).
3. **Run**: `cargo run --release`, then open <http://127.0.0.1:3000>.
4. The address index starts filling in the background. Progress appears in the logs,
   the page footer and `/api/health`.

Local Bitcoin Core instead: unset `RPC_API_KEY` and set `RPC_URL`, `RPC_USER`/`RPC_PASS`
or `RPC_COOKIE`. To ignore `.env` (for example for regtest), use `ENV_FILE=none`:

```
ENV_FILE=none NETWORK=regtest RPC_USER=user RPC_PASS=pass cargo run
```

## Configuration

All settings come from environment variables or `.env`.

| Variable | Default | Notes |
|---|---|---|
| `NETWORK` | `regtest` | `mainnet`, `testnet`, `signet`, `regtest` |
| `RPC_URL` | `http://127.0.0.1:<port>` | Provider: `https://bitrpc.thebuidl.xyz/bitcoin` |
| `RPC_API_KEY` | none | API key auth; sent as a header, never logged |
| `RPC_API_KEY_HEADER` | `X-API-Key` | |
| `RPC_USER` / `RPC_PASS` | none | Basic auth (local node) |
| `RPC_COOKIE` | none | Path to Core's `.cookie` file |
| `RPC_RATE_LIMIT_PER_MIN` | `0` (off) | Client-side cap; use `90` for the provider |
| `RPC_MAX_CONCURRENCY` | `4` | Max RPC requests in flight |
| `RPC_TIMEOUT_SECS` | `30` | Per RPC call (block downloads use up to 90–120 s) |
| `RPC_MAX_RETRIES` | `3` | For 429, 5xx, connection errors, Core warm-up |
| `BIND` | `127.0.0.1:3000` | |
| `REQUEST_TIMEOUT_SECS` | `120` | Whole API request; returns 504 after this |
| `CORS_ORIGINS` | unset | Comma list or `*`. Not needed for the bundled frontend |
| `FRONTEND_DIR` | `frontend` | Static files served at `/` |
| `CACHE_MAX_MB` | `256` | Approximate memory for cached responses |
| `CACHE_TTL_LONG_SECS` | `86400` | Block content; blocks/txs ≥ 6 deep |
| `CACHE_TTL_SHORT_SECS` | `15` | Near-tip blocks, mempool txs |
| `CACHE_TTL_TIP_SECS` | `5` | Chain tip |
| `INDEX_ENABLED` | `true` | `false` makes `/api/address` use `scantxoutset` (local nodes) |
| `INDEX_DB_PATH` | `data/index-<network>.sqlite` | |
| `START_HEIGHT` | `tip-144` (`0` on regtest) | A height or `tip-N`, fixed at first start |
| `INDEX_CONCURRENCY` | `2` | Block downloads in flight |
| `INDEX_POLL_SECS` | `15` | How often to look for new blocks once synced |
| `INDEX_MAX_REORG` | `100` | Stop (with an error) on deeper reorgs |
| `ENV_FILE` | `.env` | Alternative dotenv file; `none` to skip |
| `RUST_LOG` | `info` | e.g. `info,block_explorer=debug` |

## API

All responses are JSON. Errors share one shape:

```json
{ "error": { "code": "not_found", "message": "block not found" } }
```

| HTTP | `code` | When |
|---|---|---|
| 400 | `invalid_input` | Bad height/hash/txid/address, wrong network, bad query string |
| 404 | `not_found` | Unknown block/tx/endpoint, height above tip |
| 501 | `not_supported` | The provider forbids the RPC this feature needs, or the index is disabled |
| 502 | `upstream_error` | Node unreachable, RPC error, bad credentials |
| 503 | `node_busy` / `rate_limited` | Scan in progress, node warming up, index still building, provider rate limit |
| 504 | `upstream_timeout` | RPC or whole request timed out |

Page sizes are clamped: blocks ≤ 50, block txs ≤ 100, address txs ≤ 100.
Existing response fields are unchanged from the MVP; new fields were only added.

### `GET /api/health`
```json
{ "status": "ok", "version": "0.1.0", "network": "bitcoin", "uptime_secs": 16,
  "rpc": { "ok": true, "chain": "main", "tip": 969467 },
  "rpc_rate_limit": { "limit": 100, "remaining": 86, "reset_secs": 46 },
  "cache": { "entries": 28, "approx_bytes": 2335774 },
  "index": { "start_height": 969323, "indexed_height": 969342, "node_tip": 969467,
             "synced": false, "last_error": null } }
```
Returns 503 with `"status": "degraded"` when the node is unreachable.

### `GET /api/tip`
```json
{ "height": 969467, "hash": "00000000000000000000bd92…dfdf8", "chain": "main" }
```

### `GET /api/blocks?start=<height>&limit=20`
Newest first. `next_start` is the `start` for the next page (`null` at genesis).
```json
{ "tip": 969467, "next_start": 969457,
  "blocks": [ { "height": 969467, "hash": "0000…dfdf8", "time": 1790873561, "tx_count": 2993,
                "previous_hash": "0000…35ed", "size": 1738872, "weight": 3993783 } ] }
```

### `GET /api/block/:height_or_hash`
```json
{ "hash": "0000…4e3c", "height": 969457, "time": 1790866802, "median_time": 1790863974,
  "size": 1704369, "stripped_size": 763137, "weight": 3993780, "tx_count": 2960,
  "total_fees_sat": 4442396, "total_fees_source": "coinbase_minus_subsidy", "subsidy_sat": 312500000,
  "confirmations": 11, "merkle_root": "d77b…9536", "difficulty": 132757073449487.5,
  "version": 536911872, "bits": "17021ec5", "nonce": 4106511320,
  "previous_hash": "0000…4b06", "next_hash": "0000…cc18d" }
```

### `GET /api/block/:id/txs?start=0&limit=25`
```json
{ "block_hash": "0000…4e3c", "block_height": 969457, "total": 2960, "start": 0, "limit": 3,
  "next_start": 3, "txids": ["1e9e…4362", "d98b…3235", "17cf…f788"] }
```

### `GET /api/tx/:txid[?block=<height|hash>]`
The `block` hint is required for confirmed transactions outside the address index
range (the provider has no txindex). Mempool transactions never need it.
```json
{ "txid": "6651…d26e",
  "status": { "confirmed": true, "confirmations": 11, "block_hash": "0000…4e3c",
              "block_height": 969457, "block_time": 1790866802 },
  "size": 215, "vsize": 134, "weight": 536, "version": 2, "locktime": 0,
  "is_coinbase": false, "fee_sat": 9900, "fee_rate_sat_vb": 73.88,
  "inputs":  [ { "txid": "59b4…1580", "vout": 0, "address": "3NSy…orjG",
                 "script_type": "scripthash", "value_sat": 200000000 } ],
  "outputs": [ { "n": 0, "address": "3EUD…iQEu", "script_type": "scripthash", "value_sat": 199990100 } ] }
```
Coinbase inputs are `{ "coinbase": true }`. `fee_sat`/`fee_rate_sat_vb` are `null` when unknown.

### `GET /api/address/:addr`
From the address index (or `scantxoutset` when `INDEX_ENABLED=false`).
```json
{ "address": "bc1q…", "balance_sat": 0, "utxo_count": 0, "unspents": [],
  "unspents_truncated": false, "scanned_at_height": 969344,
  "tx_count": 2, "received_sat": 1200000, "sent_sat": 1200000, "source": "index",
  "index": { "start_height": 969323, "indexed_height": 969344, "node_tip": 969467,
             "synced": false, "complete_history": false },
  "note": "Partial history: only blocks 969323–969344 are indexed. Coins this address received before block 969323 are not counted, …" }
```
`unspents` lists at most 100 UTXOs (`{txid, vout, height, amount_sat}`); `utxo_count` is the full count.

### `GET /api/address/:addr/txs?start=0&limit=25`
Newest first, within the indexed range.
```json
{ "address": "bc1q…", "start": 0, "limit": 25, "next_start": null,
  "txs": [ { "txid": "a11e…cb2e", "block_height": 969330, "block_hash": "0000…e382",
             "block_time": 1790799589, "received_sat": 0, "sent_sat": 2494200, "net_sat": -2494200 } ],
  "index": { … }, "note": "…" }
```

### `GET /api/search?q=<height|blockhash|txid|address>`
```json
{ "type": "block", "value": 840000, "path": "/api/block/840000" }
```
`type` is `block`, `tx` or `address`. A 64-hex query is tried as a block hash first, then
as a txid (mempool, or within the index range).

```bash
curl localhost:3000/api/tip
curl "localhost:3000/api/blocks?limit=5"
curl localhost:3000/api/block/840000
curl "localhost:3000/api/tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d?block=57043"
curl "localhost:3000/api/search?q=840000"
```

### Caching and the "whole block" trade-off

| Data | Cached for | Why |
|---|---|---|
| Block content by hash (summary, txids, fees) | long | Content behind a hash never changes |
| Height → hash, tx placement | long once ≥ 6 deep, else 15 s | Can change in a reorg near the tip |
| Mempool txs | 15 s | They confirm or disappear |
| Tip | 5 s | |
| Confirmations, `next_hash` | not cached | Recomputed from the tip on each request |

The MVP loaded a confirmed tx by fetching the **whole block at verbosity 3** to get
prevouts. On mainnet that is ~13 MB of JSON and ~25 s per request. It now asks for the
single transaction at verbosity 2 with its block hash (~0.5 s, a few KB), which includes
prevouts and fee on Core ≥ 25. The cost is that the block hash must be known, which
without a txindex is why the `?block=` hint and the index's txid table exist. Block
pages use verbosity 1 (~0.3 MB) for header fields and txids, or the index (no RPC).

## The address index

The provider forbids `scantxoutset` and has no address index or txindex, so the
explorer builds its own index in SQLite (`data/index-<network>.sqlite`).

**How it works**
- A background task fetches each block as raw hex (`getblock` verbosity 0, ~3 MB),
  decodes it locally, and writes it in one SQLite transaction. `INDEX_CONCURRENCY`
  downloads run ahead of the writer.
- Tables: `blocks` (header, sizes, fees), `txs` (txid → height/position), `scripts`
  (exact scriptPubKey, deduplicated), `outputs` (tx, vout, script, value) and `spends`
  (which tx spent an indexed output). Balance = unspent indexed outputs of the address;
  history = txs that pay to it or spend from it.
- **Start height.** Indexing all of mainnet over a remote, rate-limited RPC would take
  weeks, so it starts at `START_HEIGHT` (default `tip-144`, one day). The value is fixed
  at first start; delete the database to change it.
- **Checkpoint/resume.** The highest stored block is the checkpoint, so a restart
  continues where it stopped.
- **Reorgs.** Before each round it checks that its top block is still the node's block
  at that height. If not, it deletes that height (every row carries a height) and steps
  back until it is on the best chain again, up to `INDEX_MAX_REORG` blocks. Tested on
  regtest with `invalidateblock`.
- **Cost on mainnet:** about 13 blocks/min, ~1.8 MB of disk per block.
  `tip-144` ≈ 260 MB and ~11 min; `tip-1008` (a week) ≈ 1.8 GB and ~80 min.
  One request per block, well inside the rate limit.
- **Why verbosity 0 and not 3:** v3 is 4–5× larger and slower. The only extra it gives
  is the address of pre-`START_HEIGHT` coins spent inside the range; their balance still
  could not be known, so it is not worth it.

**Coverage is always explicit.** `/api/address/*` responses include
`index.start_height`, `index.indexed_height`, `index.synced` and
`index.complete_history`, plus a human-readable `note`; the frontend shows them in a
notice on the address page.

The index also serves block pages, fees and txid lookups for its range without RPC calls.

## Frontend

Served by the same server at `/`, from `frontend/` (no build step, no external CDNs):

- **Home:** search (height, block hash, txid or address via `/api/search`), chain tip,
  latest blocks with "Load more".
- **Block:** header details, fees, previous/next links, paginated transaction list.
- **Transaction:** status and confirmations, fee, fee rate (sat/vB), inputs and outputs
  with clickable addresses and amounts.
- **Address:** balance, received/sent, history, UTXOs, and an index coverage notice.

Hash routing (`#/block/…`, `#/tx/…?block=…`, `#/address/…`). Loading states, error
messages per status (400/404/501/502/503/504) with retry, copy buttons on hashes,
BTC + sat amounts, relative + absolute times, light/dark following the system setting,
and a mobile layout.

```
cargo run --release
# open http://127.0.0.1:3000
```

## Tests

```bash
cargo test                       # unit tests + HTTP tests that need no node

# Same checks as smoke_test.sh plus the index, against a local regtest node
# (txindex=1, wallet support):
TEST_RPC_USER=user TEST_RPC_PASS=pass cargo test -- --ignored regtest_smoke

# Read-only mainnet checks via RPC_URL / RPC_API_KEY (env or .env), ~10 requests:
cargo test -- --ignored mainnet_smoke
```

`smoke_test.sh` still works against a running regtest server:

```bash
bitcoind -regtest -daemon
ENV_FILE=none NETWORK=regtest RPC_USER=user RPC_PASS=pass cargo run &
./smoke_test.sh            # 23 checks
```

Example `bitcoin.conf` for regtest:
```
regtest=1
server=1
txindex=1
rpcuser=user
rpcpassword=pass
fallbackfee=0.0001
```

## Known limitations

- **No txindex at the provider.** A confirmed transaction outside the index range can
  only be opened together with its block. That works from block pages, address history
  and `?block=`. Searching for an old txid by itself returns 404 with an explanation.
- **Partial address history.** Only `START_HEIGHT`..tip is indexed. Coins received
  before then are not in the balance, and their later spends are not attributed. The
  API and UI say so.
- **No mempool in address views.** Unconfirmed transactions are not indexed; they are
  visible by txid.
- **Mempool tx inputs.** Core does not return prevouts for mempool txs. They are filled
  from the index or from parent txs in the mempool; `getmempoolentry` is the fee
  fallback. Otherwise the input amount and fee show as unknown.
- **Block fees** are coinbase outputs minus subsidy. A miner that claims less than
  allowed makes this under-report (rare).
- **`median_time`** is `null` for indexed blocks whose 10 predecessors are not indexed.
- **Cold block lists** fetch missing blocks with one batched `getblock` v1 (about 0.3 MB
  each), so the first load of an unindexed page of 15 blocks takes several seconds.
  Later loads come from the cache or index.
- **Amounts** from Core's JSON are BTC floats, rounded to satoshis (exact for all valid
  amounts); index amounts are integer satoshis.
- **Single process.** Cache and index live in one process; scaling out needs a shared
  cache and a single indexer writer.
